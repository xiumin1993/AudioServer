use anyhow::{Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

/// PC 音频服务器 - 捕获系统音频并通过 WebSocket 流式传输
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// WebSocket 服务监听端口
    #[arg(short, long, default_value = "8080")]
    port: u16,

    /// 音频采样率
    #[arg(short, long, default_value = "48000")]
    sample_rate: u32,

    /// 音频通道数
    #[arg(short, long, default_value = "2")]
    channels: u16,

    /// 缓冲区大小（采样点数）
    #[arg(short, long, default_value = "1024")]
    buffer_size: u32,
}

/// 客户端连接管理器
struct ClientManager {
    clients: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Vec<u8>>>>>,
    next_id: AtomicU64,
}

impl ClientManager {
    fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

    async fn add_client(&self) -> (u64, mpsc::UnboundedReceiver<Vec<u8>>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        self.clients.lock().await.insert(id, tx);
        info!("客户端已连接: ID={}, 当前连接数: {}", id, self.clients.lock().await.len());
        (id, rx)
    }

    async fn remove_client(&self, id: u64) {
        self.clients.lock().await.remove(&id);
        info!("客户端已断开: ID={}, 当前连接数: {}", id, self.clients.lock().await.len());
    }

    async fn broadcast(&self, data: &[u8]) {
        let clients = self.clients.lock().await;
        let mut failed_clients = Vec::new();

        for (id, tx) in clients.iter() {
            if tx.send(data.to_vec()).is_err() {
                failed_clients.push(*id);
            }
        }

        // 清理失败的客户端
        if !failed_clients.is_empty() {
            drop(clients);
            let mut clients = self.clients.lock().await;
            for id in failed_clients {
                clients.remove(&id);
            }
        }
    }
}

/// 启动音频捕获
fn start_audio_capture(
    sample_rate: u32,
    channels: u16,
    buffer_size: u32,
    client_manager: Arc<ClientManager>,
) -> Result<()> {
    let host = cpal::default_host();

    // 尝试获取默认输出设备进行 loopback 捕获
    let device = host
        .default_output_device()
        .context("未找到默认输出设备")?;

    info!("使用音频设备: {}", device.name()?);

    // 配置音频流
    let config = StreamConfig {
        channels: channels.into(),
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Fixed(buffer_size),
    };

    let client_manager_clone = client_manager.clone();

    // 创建音频流回调
    let err_fn = move |err| {
        error!("音频流错误: {}", err);
    };

    // 根据采样格式选择数据处理方式
    let sample_format = device.default_output_config()
        .context("无法获取默认输出配置")?
        .sample_format();

    let stream = match sample_format {
        SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let bytes = unsafe {
                    std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2)
                };
                let manager = client_manager_clone.clone();
                tokio::spawn(async move {
                    manager.broadcast(bytes).await;
                });
            },
            err_fn,
            None,
        ),
        SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                // 将 f32 转换为 i16
                let i16_data: Vec<i16> = data.iter()
                    .map(|&s| (s * 32767.0) as i16)
                    .collect();
                let bytes = unsafe {
                    std::slice::from_raw_parts(i16_data.as_ptr() as *const u8, i16_data.len() * 2)
                };
                let manager = client_manager_clone.clone();
                tokio::spawn(async move {
                    manager.broadcast(bytes).await;
                });
            },
            err_fn,
            None,
        ),
        _ => {
            anyhow::bail!("不支持的采样格式: {:?}", sample_format);
        }
    }?;

    stream.play()?;
    info!("音频捕获已启动: {}Hz, {}通道, 缓冲区: {}采样点",
          sample_rate, channels, buffer_size);

    // 保持流活跃
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    });

    Ok(())
}

/// 处理 WebSocket 连接
async fn handle_connection(
    stream: TcpStream,
    client_manager: Arc<ClientManager>,
) {
    let addr = stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
    info!("新的 TCP 连接来自: {}", addr);

    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket 握手失败: {}", e);
            return;
        }
    };

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    let (client_id, mut audio_rx) = client_manager.add_client().await;

    // 发送音频格式信息（JSON 头部）
    let header = format!(
        r#"{{"type":"audio_config","sample_rate":48000,"channels":2,"format":"pcm_s16le"}}"#
    );
    if let Err(e) = ws_sender.send(Message::Text(header)).await {
        error!("发送头部失败: {}", e);
        client_manager.remove_client(client_id).await;
        return;
    }

    // 创建任务发送音频数据
    let send_task = tokio::spawn(async move {
        while let Some(data) = audio_rx.recv().await {
            if ws_sender.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // 接收客户端消息（用于心跳或控制命令）
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    info!("收到客户端消息: {}", text);
                }
                Ok(Message::Close(_)) => {
                    info!("客户端请求关闭连接");
                    break;
                }
                Err(e) => {
                    warn!("接收消息错误: {}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    // 等待任一任务完成
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    client_manager.remove_client(client_id).await;
}

/// 启动 WebSocket 服务器
async fn start_websocket_server(port: u16, client_manager: Arc<ClientManager>) -> Result<()> {
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).await?;
    info!("WebSocket 服务器已启动，监听端口: {}", port);
    info!("连接地址: ws://<你的电脑IP>:{}/ws/audio", port);

    loop {
        let (stream, addr) = listener.accept().await?;
        info!("新连接: {}", addr);

        let manager = client_manager.clone();
        tokio::spawn(async move {
            handle_connection(stream, manager).await;
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();

    info!("=== PC 音频服务器 ===");
    info!("配置: 端口={}, 采样率={}Hz, 通道数={}, 缓冲区={}",
          args.port, args.sample_rate, args.channels, args.buffer_size);

    // 创建客户端管理器
    let client_manager = Arc::new(ClientManager::new());

    // 启动音频捕获
    let capture_manager = client_manager.clone();
    std::thread::spawn(move || {
        if let Err(e) = start_audio_capture(
            args.sample_rate,
            args.channels,
            args.buffer_size,
            capture_manager,
        ) {
            error!("音频捕获失败: {}", e);
            std::process::exit(1);
        }
    });

    // 等待一小段时间让音频捕获启动
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // 启动 WebSocket 服务器
    start_websocket_server(args.port, client_manager).await
}
