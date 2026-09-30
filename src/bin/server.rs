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

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // CLI 版没有界面可点，语言由环境变量 / settings.txt / 系统语言决定（与 GUI 同一套优先级）
    let args = Args::parse();

    // "打印默认配置"要排在最前面：它只是给打包脚本用的纯输出，
    // 既不该建 %APPDATA%\PCAssistant\config.json，也不该被驱动门禁拦住。
    if args.print_default_config {
        print!("{}", audioserver::config::default_json());
        return Ok(());
    }

    let (locale, locale_from) = audioserver::lang::apply_startup_locale();
    log::info!("[CLI] UI language = {locale} (from {locale_from})");

    // ── v3.5：启动环境门禁（只读检测，不写注册表、不装驱动）──
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

    let config = ServerConfig {
        port: args.port,
        sample_rate: args.sample_rate,
        channels: args.channels,
        buffer_size: args.buffer_size,
    };

    let (event_tx, _event_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();

    // Run server in background thread
    let server_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(run_server(config, event_tx, cmd_rx));
    });

    // Print connection info
    let local_ip = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "<your-ip>".to_string());

    println!();
    println!("=== Audio Server (CLI) ===");
    println!("WebSocket: ws://{}:{}/ws/audio", local_ip, args.port);
    println!("Press Ctrl+C to stop");
    println!();

    // Handle Ctrl+C
    tokio::signal::ctrl_c().await?;
    println!("\nStopping...");
    cmd_tx.send(ServerCommand::Stop).ok();

    // Wait for server thread
    let _ = server_handle.join();
    println!("Server stopped");

    Ok(())
}
