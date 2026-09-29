use anyhow::Result;
#[cfg(not(windows))]
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

#[cfg(windows)]
use windows::Win32::Media::Audio::{
    eConsole, eRender, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator,
};
#[cfg(windows)]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED, STGM_READ,
};
#[cfg(windows)]
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PROPERTYKEY};
#[cfg(windows)]
use windows::core::GUID;

/// Server configuration
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub port: u16,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_size: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            sample_rate: 48000,
            channels: 2,
            buffer_size: 1024,
        }
    }
}

/// Server → GUI events
#[derive(Debug, Clone)]
pub enum ServerEvent {
    Log(String),
    StatusChanged(ServerStatus),
    ClientConnected(String),
    ClientDisconnected(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServerStatus {
    Stopped,
    Running,
}

/// GUI → Server commands
pub enum ServerCommand {
    Stop,
}

/// Client connection manager
struct ClientManager {
    clients: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>>,
    /// Actual capture format from WASAPI (sample_rate, channels)
    capture_format: Arc<Mutex<(u32, u16)>>,
    /// Bridge: audio capture thread → tokio forwarding task
    /// 使用 tokio::sync::mpsc 替代 crossbeam_channel，
    /// 这样转发任务可以用纯异步 recv().await，不再需要 spawn_blocking
    audio_bridge_tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl ClientManager {
    fn new() -> (Self, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        (
            Self {
                clients: Arc::new(Mutex::new(HashMap::new())),
                capture_format: Arc::new(Mutex::new((48000, 2))),
                audio_bridge_tx: tx,
            },
            rx,
        )
    }

    async fn add_client(&self, client_key: String) -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.clients.lock().await.insert(client_key, tx);
        rx
    }

    async fn remove_client(&self, client_key: &str) {
        self.clients.lock().await.remove(client_key);
    }

    async fn _client_count(&self) -> usize {
        self.clients.lock().await.len()
    }
}

/// Run the audio server (executed in a background thread)
pub async fn run_server(
    config: ServerConfig,
    event_tx: std::sync::mpsc::Sender<ServerEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<ServerCommand>,
) {
    info!("Starting audio server...");

    let (client_manager, mut bridge_rx) = ClientManager::new();
    let client_manager = Arc::new(client_manager);

    // Spawn a Tokio task to forward audio from capture thread to WebSocket clients
    // 【关键修复】不再使用 spawn_blocking 逐包等待，改用纯异步 recv().await。
    // 之前每个音频包都要 spawn_blocking → 阻塞线程 → recv() → 返回 → 再 spawn_blocking，
    // 这个模式依赖 Tokio 阻塞线程池的可用性，容易因线程调度延迟导致数据堆积。
    // 现在用 tokio::sync::mpsc，WASAPI 线程用 send()（同步非阻塞）写入，
    // 转发任务用 recv().await（纯异步）读取，零额外线程、零调度开销。
    let fwd_manager = client_manager.clone();
    tokio::spawn(async move {
        let mut packet_count: u64 = 0;
        while let Some(data) = bridge_rx.recv().await {
            packet_count += 1;
            if packet_count % 100 == 1 {
                info!("[Bridge] Forwarded {} packets, latest {} bytes, {} clients",
                    packet_count, data.len(),
                    fwd_manager.clients.lock().await.len());
            }

            let clients = fwd_manager.clients.lock().await;
            let mut failed = Vec::new();
            for (id, tx) in clients.iter() {
                if tx.send(data.clone()).is_err() {
                    failed.push(id.clone());
                }
            }
            drop(clients);
            if !failed.is_empty() {
                let mut clients = fwd_manager.clients.lock().await;
                for id in failed {
                    clients.remove(&id);
                }
            }
        }
        info!("[Bridge] Forwarding task exited after {} packets", packet_count);
    });

    // Start audio capture in a separate thread (WASAPI needs a non-async thread)
    let capture_config = config.clone();
    let capture_manager = client_manager.clone();
    let capture_event_tx = event_tx.clone();
    let _audio_handle = std::thread::spawn(move || {
        if let Err(e) = start_audio_capture(capture_config, capture_manager, capture_event_tx) {
            error!("Audio capture failed: {}", e);
        }
    });

    // Wait for audio capture to initialize
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Start WebSocket server
    let addr = format!("0.0.0.0:{}", config.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            event_tx.send(ServerEvent::Log(format!(
                "WebSocket server listening on port {}",
                config.port
            ))).ok();
            event_tx.send(ServerEvent::StatusChanged(ServerStatus::Running)).ok();
            l
        }
        Err(e) => {
            event_tx.send(ServerEvent::Error(format!("Failed to bind port: {}", e))).ok();
            return;
        }
    };

    // Main loop: accept connections + handle commands
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        info!("New connection: {}", addr);
                        let manager = client_manager.clone();
                        let event_tx_clone = event_tx.clone();
                        let client_key = addr.ip().to_string();
                        tokio::spawn(async move {
                            handle_connection(stream, client_key, manager, event_tx_clone.clone()).await;
                        });
                    }
                    Err(e) => {
                        error!("Accept failed: {}", e);
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(ServerCommand::Stop) | None => {
                        info!("Stopping server...");
                        break;
                    }
                }
            }
        }
    }

    event_tx.send(ServerEvent::Log("Server stopped".to_string())).ok();
    event_tx.send(ServerEvent::StatusChanged(ServerStatus::Stopped)).ok();
}

/// Start audio capture using WASAPI loopback (Windows only)
#[cfg(windows)]
fn start_audio_capture(
    _config: ServerConfig,
    client_manager: Arc<ClientManager>,
    event_tx: std::sync::mpsc::Sender<ServerEvent>,
) -> Result<()> {
    use windows::Win32::System::Com::CoTaskMemFree;

    // Initialize COM for this thread
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        anyhow::bail!("CoInitializeEx failed: {:?}", hr);
    }

    // Create device enumerator
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?
    };

    // Get default render (output) device — this is what we loopback from
    // GetDefaultAudioEndpoint(dataflow: EDataFlow, role: ERole)
    let device: IMMDevice = unsafe {
        enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?
    };

    // 通过属性存储获取设备友好名称（如 "Speakers (Realtek Audio)"）
    // PKEY_Device_FriendlyName 的 GUID 值（Windows SDK 标准定义）
    const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_values(
            0xa45c254e, 0xdf1c, 0x4efd,
            [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
        ),
        pid: 14,
    };

    let device_name = unsafe {
        match device.OpenPropertyStore(STGM_READ) {
            Ok(store) => {
                match store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) {
                    Ok(pv) => {
                        // PROPVARIANT 内存布局：
                        //   offset 0: vt (u16) — 类型标识，VT_LPWSTR = 31
                        //   offset 8: pwszVal (*mut u16) — 宽字符串指针
                        let pv_ptr = &pv as *const _ as *const u8;
                        let vt = *(pv_ptr as *const u16);
                        if vt == 31 {
                            // VT_LPWSTR: 字符串指针在 offset 8
                            let pwsz = *(pv_ptr.add(8) as *const *const u16);
                            if !pwsz.is_null() {
                                let len = (0..).take_while(|&i| *pwsz.add(i) != 0).count();
                                let slice = std::slice::from_raw_parts(pwsz, len);
                                String::from_utf16_lossy(slice)
                            } else {
                                "Unknown".to_string()
                            }
                        } else {
                            "Unknown".to_string()
                        }
                    }
                    Err(_) => "Unknown".to_string(),
                }
            }
            Err(_) => "Unknown".to_string(),
        }
    };
    info!("Using audio device: {}", device_name);
    event_tx.send(ServerEvent::Log(format!("Audio device: {}", device_name))).ok();

    // Activate IAudioClient on the device
    // Activate<T>(clsctx: CLSCTX, params: Option<*const PROPVARIANT>) -> Result<T>
    let audio_client: IAudioClient = unsafe {
        device.Activate(CLSCTX_ALL, None)?
    };

    // Get the mix format (what Windows mixes everything into)
    let mix_format_ptr = unsafe { audio_client.GetMixFormat()? };
    if mix_format_ptr.is_null() {
        anyhow::bail!("GetMixFormat returned null");
    }

    // Read the WAVEFORMATEX to get sample rate / channels / bits per sample
    let mix_format = unsafe { &*mix_format_ptr };
    let sample_rate = mix_format.nSamplesPerSec;
    let channels = mix_format.nChannels as u16;
    let bits_per_sample = mix_format.wBitsPerSample;

    info!(
        "WASAPI mix format: {}Hz, {}ch, {}-bit",
        sample_rate, channels, bits_per_sample
    );

    // Store actual capture format so handle_connection can send correct header
    {
        let mut fmt = client_manager.capture_format.blocking_lock();
        *fmt = (sample_rate, channels);
    }

    // Calculate buffer duration: 10ms worth of audio (in 100-nanosecond units)
    let buffer_duration = (sample_rate as i64) * 10_000i64; // 10ms = 10,000 * 100ns

    // Initialize with LOOPBACK flag — this is the key difference from cpal
    unsafe {
        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            buffer_duration,
            0,
            mix_format_ptr,
            None,
        )?;
    }

    // Get the capture client service
    let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService()? };

    // Start the audio stream
    unsafe { audio_client.Start()? };

    info!(
        "WASAPI loopback started: {}Hz, {}ch, {}-bit",
        sample_rate, channels, bits_per_sample
    );
    event_tx.send(ServerEvent::Log(format!(
        "Capture started: {}Hz, {}ch",
        sample_rate, channels
    )))
    .ok();

    // Use the bridge channel from ClientManager to forward audio to the tokio task
    let bridge_tx = client_manager.audio_bridge_tx.clone();

    // COM interfaces are not Send by default, but they are thread-safe
    // when used from a single thread. Wrap to allow moving into the thread.
    struct SendCaptureClient(IAudioCaptureClient);
    unsafe impl Send for SendCaptureClient {}
    impl std::ops::Deref for SendCaptureClient {
        type Target = IAudioCaptureClient;
        fn deref(&self) -> &Self::Target { &self.0 }
    }

    let send_client = SendCaptureClient(capture_client);

    // Spawn a thread to read from WASAPI and push into the bridge channel
    let mut wasapi_packet_count: u64 = 0;
    std::thread::spawn(move || {
        loop {
            let mut buffer_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;

            let get_result = unsafe {
                send_client.GetBuffer(
                    &mut buffer_ptr,
                    &mut num_frames,
                    &mut flags,
                    None,
                    None,
                )
            };

            if get_result.is_err() {
                // 【关键修复】不再静默退出，记录错误信息帮助诊断
                error!("[WASAPI] GetBuffer failed after {} packets: HRESULT = {:?}. Audio capture thread exiting.",
                    wasapi_packet_count, get_result.err());
                break;
            }

            if !buffer_ptr.is_null() && num_frames > 0 {
                let channel_count = channels as u32;
                let sample_count = num_frames * channel_count;

                let byte_vec = match bits_per_sample {
                    16 => {
                        unsafe {
                            std::slice::from_raw_parts(
                                buffer_ptr as *const u8,
                                sample_count as usize * 2,
                            )
                            .to_vec()
                        }
                    }
                    32 => {
                        let src = unsafe {
                            std::slice::from_raw_parts(
                                buffer_ptr as *const f32,
                                sample_count as usize,
                            )
                        };
                        let i16_data: Vec<i16> = src
                            .iter()
                            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                            .collect();
                        unsafe {
                            std::slice::from_raw_parts(
                                i16_data.as_ptr() as *const u8,
                                i16_data.len() * 2,
                            )
                        }
                        .to_vec()
                    }
                    _ => {
                        unsafe { send_client.ReleaseBuffer(num_frames).ok() };
                        continue;
                    }
                };

                // 使用 tokio mpsc 的 send()（同步非阻塞），替代 crossbeam 的 try_send()
                if let Err(e) = bridge_tx.send(byte_vec.clone()) {
                    error!("[WASAPI] Failed to send packet to bridge: {} (forwarding task may have exited)", e);
                    break;
                }

                wasapi_packet_count += 1;
                // Log first few packets to verify data is not all zeros
                if wasapi_packet_count <= 5 {
                    let non_zero = byte_vec.iter().filter(|&&b| b != 0).count();
                    info!("[WASAPI] Packet #{}: {} bytes, {} non-zero bytes ({}%)",
                        wasapi_packet_count, byte_vec.len(), non_zero,
                        if byte_vec.is_empty() { 0 } else { non_zero * 100 / byte_vec.len() });
                }
            }

            unsafe { send_client.ReleaseBuffer(num_frames).ok() };
        }
    });

    // Free the mix format allocated by WASAPI
    unsafe { CoTaskMemFree(Some(mix_format_ptr as *mut _)) };

    // Block this thread — the WASAPI capture thread and bridge forwarder handle everything
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Fallback for non-Windows platforms (uses cpal input stream)
#[cfg(not(windows))]
fn start_audio_capture(
    config: ServerConfig,
    client_manager: Arc<ClientManager>,
    event_tx: std::sync::mpsc::Sender<ServerEvent>,
) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("No default output device found")?;

    let device_name = device.name().unwrap_or_else(|_| "Unknown device".to_string());
    info!("Using audio device: {}", device_name);
    event_tx.send(ServerEvent::Log(format!("Audio device: {}", device_name))).ok();

    let default_config = device
        .default_output_config()
        .context("Failed to get default output config")?;

    let stream_config = cpal::StreamConfig {
        channels: config.channels.into(),
        sample_rate: cpal::SampleRate(config.sample_rate),
        buffer_size: cpal::BufferSize::Fixed(config.buffer_size),
    };

    let sample_format = default_config.sample_format();
    let bridge_tx = client_manager.audio_bridge_tx.clone();

    let err_fn = move |err| {
        error!("Audio stream error: {}", err);
    };

    let stream = match sample_format {
        cpal::SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let bytes = unsafe {
                    std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2)
                };
                bridge_tx.send(bytes.to_vec()).ok();
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let i16_data: Vec<i16> = data.iter()
                    .map(|&s| (s * 32767.0) as i16)
                    .collect();
                let bytes = unsafe {
                    std::slice::from_raw_parts(i16_data.as_ptr() as *const u8, i16_data.len() * 2)
                };
                bridge_tx.send(bytes.to_vec()).ok();
            },
            err_fn,
            None,
        ),
        _ => {
            anyhow::bail!("Unsupported sample format: {:?}", sample_format);
        }
    }?;

    stream.play()?;
    info!("Audio capture started: {}Hz, {}ch, buffer: {} samples",
          config.sample_rate, config.channels, config.buffer_size);
    event_tx.send(ServerEvent::Log(format!(
        "Capture started: {}Hz, {}ch",
        config.sample_rate, config.channels
    ))).ok();

    // Block this thread — the cpal stream and bridge forwarder handle everything
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Handle WebSocket connection
async fn handle_connection(
    stream: TcpStream,
    client_key: String,
    client_manager: Arc<ClientManager>,
    event_tx: std::sync::mpsc::Sender<ServerEvent>,
) {
    let addr = stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
    info!("New TCP connection from: {}", addr);

    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed: {}", e);
            return;
        }
    };

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    let mut audio_rx = client_manager.add_client(client_key.clone()).await;

    event_tx.send(ServerEvent::ClientConnected(client_key.clone())).ok();
    event_tx.send(ServerEvent::Log(format!("Client connected: {}", client_key))).ok();

    // Send audio format info (JSON header) — read actual capture format
    let (actual_sr, actual_ch) = {
        let fmt = client_manager.capture_format.lock().await;
        *fmt
    };
    let header = format!(
        r#"{{"type":"audio_config","sample_rate":{},"channels":{},"format":"pcm_s16le"}}"#,
        actual_sr, actual_ch
    );
    if let Err(e) = ws_sender.send(Message::Text(header)).await {
        error!("Failed to send header: {}", e);
        client_manager.remove_client(&client_key).await;
        return;
    }

    // Send audio data
    let send_task = tokio::spawn(async move {
        while let Some(data) = audio_rx.recv().await {
            if ws_sender.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // Receive client messages
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    info!("Client message: {}", text);
                }
                Ok(Message::Close(_)) => {
                    info!("Client requested close");
                    break;
                }
                Err(e) => {
                    warn!("Receive error: {}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    client_manager.remove_client(&client_key).await;
    event_tx.send(ServerEvent::ClientDisconnected(client_key)).ok();
}
