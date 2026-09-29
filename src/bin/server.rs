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
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();

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
