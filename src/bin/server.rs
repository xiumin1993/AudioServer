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
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();

    // ── v3.5：启动环境门禁（只读检测，不写注册表、不装驱动）──
    if !args.skip_env_check {
        let report = audioserver::env_check::detect();
        if !report.ready() {
            eprintln!("=== Audio Server (CLI) 启动前自检未通过 ===");
            for m in report.missing() {
                eprintln!("  缺少：{m}");
            }
            eprintln!(
                "\n  本程序不附带、也不运行任何驱动安装脚本，请自行下载后按官方说明安装：\n  \
                 VB-CABLE：https://vb-audio.com/Cable/\n  \
                 OBS Studio（自带 OBS Virtual Camera）：https://obsproject.com/download\n  \
                 Unity Capture：https://github.com/Unity-Technologies/Unity-Capture"
            );
            eprintln!("\n（确认知道自己在做什么时，可加 --skip-env-check 强行启动）");
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
