// ══════════════ 这个文件是什么（初学者导读）══════════════
// 1) 它验证/解决什么问题：不是硬件探针，而是 AudioServer 的"无界面命令行版"
//    入口——和 GUI(src/main.rs) 跑同一个核心 src/server.rs::run_server
//    （WebSocket 服务 + 系统音频捕获 + 麦克风/摄像头注入引擎），适合挂服务器、
//    写脚本、或用 Node/手机直接连来测协议。它本身不验证硬件结论。
// 2) 怎么单独跑：cargo run --bin server —— 默认端口 8080；可加
//    --port 9000 / --sample-rate 48000 / --channels 2 / --buffer-size 1024；
//    --skip-env-check 跳过驱动门禁硬起；--print-default-config 只打印默认配置
//    就退出（打包脚本用）。Ctrl+C 优雅停止。手机连的就是它打印的那个 ws:// 地址。
// 3) 打印怎么读：启动成功时末尾一块 "=== Audio Server (CLI) ==="，其中
//    WebSocket: ws://<本机IP>:<端口>/ws/audio 是关键行——手机填连接地址抄这行；
//    若一启动就退出且 stderr 列了缺项清单（退出码 2），= 环境门禁没过，
//    VB-CABLE 或虚拟摄像头没装，看提示链接装驱动或加 --skip-env-check。
//    日志行带毫秒时间戳，级别 info 起（RUST_LOG 环境变量可调到 debug）。
// 4) 对应主干：src/server.rs（run_server 核心）、src/config.rs（默认配置）、
//    src/env_check.rs（启动门禁）、src/lang.rs（界面/CLI 文案语言）。
//
// 概念速查：Result<T> 是"成功值或错误"的二选一容器；main 返回 Result<()> 让 ?
// 可以把错误直接抛给运行时打印。trait≈其它语言的接口声明；#[derive(Parser)] 是
// 宏替 struct Args 自动实现 clap crate 的 Parser trait（"能从命令行解析自己"）。
//
use anyhow::Result;
use clap::Parser;
use std::sync::mpsc;
use tokio::sync::mpsc as tokio_mpsc;

use audioserver::server::{run_server, ServerCommand, ServerConfig};

/// Audio Server - headless CLI version
#[derive(Parser, Debug)]
#[command(author, version, about = "Audio Server - Capture system audio and stream via WebSocket")]
struct Args {
    /// WebSocket listen port
    #[arg(short, long, default_value = "8080")]
    port: u16,

    /// Audio sample rate
    #[arg(short, long, default_value = "48000")]
    sample_rate: u32,

    /// Audio channel count
    #[arg(short, long, default_value = "2")]
    channels: u16,

    /// Buffer size (in samples)
    #[arg(short, long, default_value = "1024")]
    buffer_size: u32,

    /// 跳过启动环境自检（缺驱动也硬起）。默认会先检查 VB-CABLE / 虚拟摄像头，
    /// 缺必需驱动时直接打印缺项并退出，不监听端口 —— 与 GUI 版行为一致。
    #[arg(long)]
    skip_env_check: bool,

    /// 把 config.json 的**完整默认内容**打印到标准输出后立即退出（不监听端口、不检查驱动）。
    /// 打包脚本用它的输出落成 `config.default.json`，这样"默认值"永远只有
    /// src/config.rs 一处定义，不会出现文档和程序对不上的情况。
    #[arg(long)]
    print_default_config: bool,
}

// #[tokio::main] 宏展开后其实是：建一个 tokio 异步运行时 + 在其上跑下面的
// async fn。async/await=单线程事件循环里"等待不占线程"的写法，WebSocket 服务
// 靠它同时伺候多个连接。-> Result<()> 允许 main 直接用 ? 提前返回错误。
#[tokio::main]
async fn main() -> Result<()> {
    // env_logger：把 log crate 的日志打到 stderr；默认级别 info，RUST_LOG=debug
    // 可放开。format_timestamp_millis=每行前缀带毫秒时间戳，方便对时序。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // CLI 版没有界面可点，语言由环境变量 / settings.txt / 系统语言决定（与 GUI 同一套优先级）
    // Args::parse()：clap 读 std::env::args()，按 struct 字段名自动生成 --port 等
    // 长选项（字段上的 /// 文档注释会变成 --help 里的说明文字）。
    let args = Args::parse();

    // "打印默认配置"要排在最前面：它只是给打包脚本用的纯输出，
    // 既不该建 %APPDATA%\PCAssistant\config.json，也不该被驱动门禁拦住。
    if args.print_default_config {
        print!("{}", audioserver::config::default_json());
        return Ok(());
    }

    // apply_startup_locale 返回 (语言码, 判定来源) 元组；let (a, b) = ... 是
    // 解构赋值，一行拆两个值。lang::t()/tf() 是翻译查表（tf 带参数插值）。
    let (locale, locale_from) = audioserver::lang::apply_startup_locale();
    log::info!("[CLI] UI language = {locale} (from {locale_from})");

    // ── v3.5：启动环境门禁（只读检测，不写注册表、不装驱动）──
    // detect() 出一张体检报告，ready() 一票否决；缺项逐行打给用户后
    // exit(2)——退出码非 0 让脚本能区分"环境没配好"和"跑到一半被 Ctrl+C"。
    if !args.skip_env_check {
        let report = audioserver::env_check::detect();
        if !report.ready() {
            use audioserver::lang;
            eprintln!("{}", lang::t("cli.check_failed"));
            for m in report.missing() {
                eprintln!("{}", lang::tf("cli.missing", &[("item", &m)]));
            }
            eprintln!("\n{}", lang::t("cli.no_install"));
            eprintln!("{}", lang::t("cli.vb_url"));
            eprintln!("{}", lang::t("cli.obs_url"));
            eprintln!("{}", lang::t("cli.unity_url"));
            eprintln!("\n{}", lang::t("cli.force_hint"));
            std::process::exit(2);
        }
    }

    // struct 字面量：一次性给出所有字段装配运行参数（字段名必须写全，防止错位）。
    let config = ServerConfig {
        port: args.port,
        sample_rate: args.sample_rate,
        channels: args.channels,
        buffer_size: args.buffer_size,
    };

    // 两条"管道"（channel=多生产者单消费者队列，线程间传值的正规姿势）：
    // · event_tx/event_rx：std 阻塞版，server 往外出事件（CLI 版没人收，
    //   _event_rx 下划线前缀=故意不消费，只为满足类型）。
    // · cmd_tx/cmd_rx：tokio 异步无界版，外部往 server 发命令（这里只有 Stop）。
    // mpsc = multi-producer single-consumer，命名即语义。
    let (event_tx, _event_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();

    // Run server in background thread
    // thread::spawn 开一条真·操作系统线程，move 闭包把 config/event_tx/cmd_rx
    // 的所有权整个搬进去；里面新建一个独立的 tokio 运行时来 block_on 异步的
    // run_server——服务器从此在这条后台线程上自己跑。
    let server_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(run_server(config, event_tx, cmd_rx));
    });

    // Print connection info
    // local_ip() 返回 Result<IpAddr>：map 成功改名成 String，or_else 失败给兜底
    // 占位文本——探测不到网卡 IP 也不该让打印崩掉。
    let local_ip = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "<your-ip>".to_string());

    println!();
    println!("=== Audio Server (CLI) ===");
    println!("WebSocket: ws://{}:{}/ws/audio", local_ip, args.port);
    println!("Press Ctrl+C to stop");
    println!();

    // Handle Ctrl+C
    // .await 挂起主任务等信号（不烧 CPU）；? 把监听失败的错误直接抛给 main 返回。
    tokio::signal::ctrl_c().await?;
    println!("\nStopping...");
    // send(...).ok()：把 Result 转 Option 丢掉错误——若 server 已经先退出，
    // 发送失败也完全无所谓，这是刻意的"尽力通知"。
    cmd_tx.send(ServerCommand::Stop).ok();

    // Wait for server thread
    // join() 等后台线程收尾（其返回的 Result 只反映线程是否 panic，忽略）。
    let _ = server_handle.join();
    println!("Server stopped");

    // Ok(())：main 的成功出口；返回类型 Result<()> 让上面任何 ? 都能直接结束函数。
    Ok(())
}
