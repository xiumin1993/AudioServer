use anyhow::Result;
#[cfg(not(windows))]
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::mic_out::{self, MicQueue};
use crate::vcam::{self, FrameMailbox};
use crate::vcam_obs;

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
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
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
    // ── v3 麦克风上行事件 ──
    /// 上行音频 RMS 电平（0.0 ~ 1.0），限频约 20Hz 推送
    MicLevel(f32),
    /// 上行链路统计，每秒推送一次
    MicStats {
        kbps: u32,
        total_kb: u64,
        packets: u64,
        interval_ms: u32,
    },
    /// 手机麦克风来源已连接（收到 mic_start 头）
    MicSource {
        ip: String,
        sample_rate: u32,
        channels: u16,
    },
    /// 麦克风上行会话结束
    MicStopped,
    /// 虚拟麦克风注入引擎状态：Some(设备名) = 已向 CABLE Input 建流；None = 不可用
    MicEngine {
        device: Option<String>,
    },
    /// v3.1：CABLE Output 捕获占用状态翻转（true = 有应用正在用麦克风）
    MicState {
        active: bool,
    },
    /// v3.1：手机端手动闭麦状态（true = 用户按了静音键，服务器丢弃上行）
    MicMuted {
        muted: bool,
    },
    // ── v3.4 摄像头上行事件 ──
    /// 手机摄像头会话已登记（收到 cam_start，手机侧相机硬件可能仍未开启）
    CamSource {
        ip: String,
    },
    /// 摄像头上行会话结束
    CamStopped,
    /// 应用占用虚拟摄像头状态翻转（true = 有应用在观看 Unity Video Capture）。
    /// 同时作为 cam_state 广播给手机：手机据此开/关相机硬件（按需取景，对齐 v3.3 麦克风）
    CamState {
        active: bool,
    },
    /// 摄像头链路统计，每秒推送一次
    CamStats {
        kbps: u32,
        fps: u32,
        frames: u64,
        width: u16,
        height: u16,
    },
    /// 手机上报的相机硬件能力（JSON 原文，GUI 展示"手机最高支持什么画质"）
    CamCaps {
        caps: String,
        ip: String,
    },
    /// v3.4 GUI 预览：每秒随 CamStats 附带一张最新 JPEG 帧（仅给界面显示，
    /// 不进虚拟摄像头链路 —— 那条路走 cam_mailbox，互不干扰）
    CamFrame(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServerStatus {
    Stopped,
    Running,
}

/// GUI → Server commands
pub enum ServerCommand {
    Stop,
    /// 暂停/恢复 loopback 音频向客户端的转发（v3：Mic 模式下避免回声与带宽挤占）
    SetSpeakerPaused(bool),
    /// v3.4：GUI"请求手机开启摄像头"→ 向所有手机广播 cam_request（手机弹窗确认）
    CamRequest,
    /// v3.4：GUI"强制关闭摄像头"→ 向所有手机广播 cam_stop（守护隐私，立即生效）
    CamForceStop,
}

/// WebSocket 写半端类型别名：发送/接收两个任务通过 Arc<Mutex> 共享
/// （v3 需要接收任务在收到 mic_start 后回 mic_ack，写端不能再被发送任务独占）
type WsSink = futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>;

/// Client connection manager
struct ClientManager {
    clients: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>>,
    /// v3.1：每个已连接客户端的 WebSocket 文本写端（ip → sink），
    /// 用于向手机广播 mic_state 唤醒/休眠指令
    text_sinks: Arc<Mutex<HashMap<String, Arc<Mutex<WsSink>>>>>,
    /// v3.1：当前是否有 PC 应用在录 CABLE Output（由捕获检测线程更新）。
    /// true 时才接收/注入手机上行，待命期手机不上传数据（省电）
    mic_live: Arc<AtomicBool>,
    /// v3.1：手机侧手动闭麦标志（true = 静音键按下，上行直接丢弃 + 清队列）
    mic_muted: Arc<AtomicBool>,
    /// Actual capture format from WASAPI (sample_rate, channels)
    capture_format: Arc<Mutex<(u32, u16)>>,
    /// Bridge: audio capture thread → tokio forwarding task
    /// 使用 tokio::sync::mpsc 替代 crossbeam_channel，
    /// 这样转发任务可以用纯异步 recv().await，不再需要 spawn_blocking
    audio_bridge_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// v3：手机上行 PCM 队列，由 mic_out 注入线程消费写入 CABLE Input
    mic_queue: MicQueue,
    // ── v3.4 摄像头 ──
    /// 最新 JPEG 帧邮箱：WS 接收任务写入，vcam 引擎线程取走解码上传驱动
    cam_mailbox: FrameMailbox,
    /// v3.4 双通道：OBS 虚拟摄像头引擎的独立邮箱（同一帧推两份，互不抢消费）
    cam_mailbox_obs: FrameMailbox,
    /// 手机已登记摄像头会话（cam_start），准备接收二进制帧
    cam_session: Arc<AtomicBool>,
    /// 有应用正在观看虚拟摄像头（vcam 引擎上报），驱动 cam_state 广播
    cam_live: Arc<AtomicBool>,
    /// 双通道各自的活跃信号：cam_live = unity || obs（任一设备被占用就唤醒手机）
    cam_live_unity: Arc<AtomicBool>,
    cam_live_obs: Arc<AtomicBool>,
}

impl ClientManager {
    fn new() -> (Self, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        (
            Self {
                clients: Arc::new(Mutex::new(HashMap::new())),
                text_sinks: Arc::new(Mutex::new(HashMap::new())),
                mic_live: Arc::new(AtomicBool::new(false)),
                mic_muted: Arc::new(AtomicBool::new(false)),
                capture_format: Arc::new(Mutex::new((48000, 2))),
                audio_bridge_tx: tx,
                mic_queue: mic_out::new_queue(),
                cam_mailbox: vcam::new_mailbox(),
                cam_mailbox_obs: vcam::new_mailbox(),
                cam_session: Arc::new(AtomicBool::new(false)),
                cam_live: Arc::new(AtomicBool::new(false)),
                cam_live_unity: Arc::new(AtomicBool::new(false)),
                cam_live_obs: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    /// v3.1：向所有已连接手机广播麦克风唤醒/休眠状态。
    /// 唤醒瞬间清空上行抖动队列，丢弃任何残留旧数据，保证新会话从实时点开始。
    async fn broadcast_mic_state(&self, active: bool) {
        self.mic_live.store(active, Ordering::Relaxed);
        if active {
            if let Ok(mut q) = self.mic_queue.lock() {
                q.clear();
            }
        }
        let msg = format!(r#"{{"type":"mic_state","active":{}}}"#, active);
        let sinks = self.text_sinks.lock().await;
        for sink in sinks.values() {
            let mut s = sink.lock().await;
            let _ = s.send(Message::Text(msg.clone())).await;
        }
    }

    /// v3.4：向所有已连接手机广播"虚拟摄像头是否被应用观看"。
    /// active=true → 手机才开相机硬件上行；false → 手机立刻关相机（按需取景）。
    /// 唤醒瞬间清空帧邮箱，保证新会话从实时点开始（不会先看到旧画面）。
    async fn broadcast_cam_state(&self, active: bool) {
        self.cam_live.store(active, Ordering::Relaxed);
        if active {
            if let Ok(mut mb) = self.cam_mailbox.lock() {
                *mb = None;
            }
            if let Ok(mut mb) = self.cam_mailbox_obs.lock() {
                *mb = None;
            }
        }
        let msg = format!(r#"{{"type":"cam_state","active":{}}}"#, active);
        let sinks = self.text_sinks.lock().await;
        for sink in sinks.values() {
            let mut s = sink.lock().await;
            let _ = s.send(Message::Text(msg.clone())).await;
        }
    }

    /// v3.4：向所有手机广播一条文本指令（cam_request / cam_stop 等 GUI 发起的操作）
    async fn broadcast_cam_text(&self, msg: &str) {
        let sinks = self.text_sinks.lock().await;
        for sink in sinks.values() {
            let mut s = sink.lock().await;
            let _ = s.send(Message::Text(msg.to_string())).await;
        }
    }

    async fn add_client(&self, client_key: String) -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.clients.lock().await.insert(client_key, tx);
        rx
    }

    async fn remove_client(&self, client_key: &str) {
        self.clients.lock().await.remove(client_key);
        self.text_sinks.lock().await.remove(client_key);
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

    // v3：GUI 切到 Mic 模式时置 true，转发任务跳过 loopback 数据包（直接丢弃，不堆积）
    let speaker_paused = Arc::new(AtomicBool::new(false));

    // Spawn a Tokio task to forward audio from capture thread to WebSocket clients
    // 【关键修复】不再使用 spawn_blocking 逐包等待，改用纯异步 recv().await。
    // 之前每个音频包都要 spawn_blocking → 阻塞线程 → recv() → 返回 → 再 spawn_blocking，
    // 这个模式依赖 Tokio 阻塞线程池的可用性，容易因线程调度延迟导致数据堆积。
    // 现在用 tokio::sync::mpsc，WASAPI 线程用 send()（同步非阻塞）写入，
    // 转发任务用 recv().await（纯异步）读取，零额外线程、零调度开销。
    let fwd_manager = client_manager.clone();
    let fwd_paused = speaker_paused.clone();
    tokio::spawn(async move {
        let mut packet_count: u64 = 0;
        while let Some(data) = bridge_rx.recv().await {
            // Mic 模式：暂停下行转发
            if fwd_paused.load(Ordering::Relaxed) {
                continue;
            }
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

    // v3：启动虚拟麦克风注入引擎（把 mic_queue 里的上行 PCM 持续写入 CABLE Input）
    mic_out::spawn_mic_output(client_manager.mic_queue.clone(), event_tx.clone());

    // v3.1：启动"应用占用麦克风"检测线程。检测结果通过 std 通道进入 tokio，
    // 再 ① 转成 GUI 事件 ② 向所有已连接手机广播 mic_state 唤醒/休眠指令
    let (mon_tx, mut mon_rx) = mpsc::unbounded_channel::<bool>();
    mic_out::spawn_capture_monitor(mon_tx);
    let mon_manager = client_manager.clone();
    let mon_event_tx = event_tx.clone();
    tokio::spawn(async move {
        while let Some(active) = mon_rx.recv().await {
            mon_event_tx.send(ServerEvent::MicState { active }).ok();
            mon_manager.broadcast_mic_state(active).await;
        }
    });

    // v3.4：启动 Unity Capture 虚拟摄像头注入引擎（消费 cam_mailbox 里的 JPEG 帧）。
    // 引擎线程通过 Want 事件判断"有应用在观看摄像头"，翻转时经通道回传。
    let (cam_tx, mut cam_rx) = mpsc::unbounded_channel::<bool>();
    vcam::spawn_vcam(client_manager.cam_mailbox.clone(), cam_tx);

    // v3.4 双通道：再开一条 OBS Virtual Camera 注入线（浏览器/新框架应用只认它）。
    // OBS 协议没有 Want 事件，用隐私监控注册表判断"有应用在访问摄像头"。
    let (cam_obs_tx, mut cam_obs_rx) = mpsc::unbounded_channel::<bool>();
    vcam_obs::spawn_vcam_obs(client_manager.cam_mailbox_obs.clone(), cam_obs_tx);

    // 两路活跃信号在这里合并（OR 语义）：任一虚拟摄像头被观看 → 唤醒手机开相机；
    // 两路都释放 → 手机立刻关相机回待命。翻转时 ① 推 GUI 事件 ② 广播 cam_state
    let cam_manager = client_manager.clone();
    let cam_event_tx = event_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(v) = cam_rx.recv() => { cam_manager.cam_live_unity.store(v, Ordering::Relaxed); }
                Some(v) = cam_obs_rx.recv() => { cam_manager.cam_live_obs.store(v, Ordering::Relaxed); }
                else => break,
            }
            let now = cam_manager.cam_live_unity.load(Ordering::Relaxed)
                || cam_manager.cam_live_obs.load(Ordering::Relaxed);
            if cam_manager.cam_live.swap(now, Ordering::Relaxed) != now {
                cam_event_tx.send(ServerEvent::CamState { active: now }).ok();
                cam_manager.broadcast_cam_state(now).await;
            }
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
                        // v3.4 修复：客户端 key 必须含端口、每条 TCP 连接唯一。
                        // 之前只用 IP：手机快速重连时新旧两条连接共用同一 key，
                        // 新连接覆盖登记表 → 旧连接断开时 remove_client 把
                        // 【新连接的存活表项】一起删掉 → 手机在线却永远收不到
                        // mic_state/cam_state 唤醒广播（语音输入失效的直接原因）。
                        let client_key = format!("{}:{}", addr.ip(), addr.port());
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
                    Some(ServerCommand::SetSpeakerPaused(paused)) => {
                        speaker_paused.store(paused, Ordering::Relaxed);
                        info!("Speaker forwarding {}", if paused { "paused" } else { "resumed" });
                    }
                    // v3.4：GUI"请求手机开启摄像头"——手机弹窗确认后才开硬件
                    Some(ServerCommand::CamRequest) => {
                        info!("[Cam] Request sent to phones (user must confirm)");
                        client_manager
                            .broadcast_cam_text(r#"{"type":"cam_request"}"#)
                            .await;
                    }
                    // v3.4：GUI"强制关闭摄像头"——隐私守护开关，手机收到立即停
                    Some(ServerCommand::CamForceStop) => {
                        info!("[Cam] Force stop sent to phones");
                        client_manager
                            .broadcast_cam_text(r#"{"type":"cam_stop","forced":true}"#)
                            .await;
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

/// 非 Windows 平台下行捕获（主战场是 macOS）：用 cpal 输入流。
///
/// 背景知识：macOS 没有 Windows WASAPI loopback 那种"录声卡输出"的系统能力，
/// Mac 上的标准做法是安装免费虚拟声卡 BlackHole：
///   1) 在"音频 MIDI 设置"里建一个【多输出设备】，同时勾选 扬声器 + BlackHole 2ch；
///   2) 把系统声音输出指到该多输出设备 → 电脑发声的同时被"抄送"进 BlackHole；
///   3) 本函数在 BlackHole 的【输入侧】开捕获流，录到的就是系统声音（下行音频）。
/// 设备名可用环境变量 PCSPEAKER_CAPTURE_DEVICE 覆盖（子串匹配、大小写不敏感），
/// 默认查找 "BlackHole 16ch" —— 故意与上行注入用的 "BlackHole 2ch" 分开两个设备：
/// 若下行捕获和麦克风注入挤在同一个设备的两侧，占用检测（IsRunningSomewhere）
/// 会把"服务器自己在录"也数进去 → 手机永远以为有应用在用麦。一设备一用途最干净。
#[cfg(not(windows))]
fn start_audio_capture(
    _config: ServerConfig,
    client_manager: Arc<ClientManager>,
    event_tx: std::sync::mpsc::Sender<ServerEvent>,
) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();

    // ── 设备选择：在全部"输入设备"里按名字找（BlackHole 同时有输入/输出两侧）──
    let wanted = std::env::var("PCSPEAKER_CAPTURE_DEVICE").unwrap_or_default();
    let needle = if wanted.is_empty() {
        "blackhole 16ch".to_string()
    } else {
        wanted.to_ascii_lowercase()
    };
    let device = host
        .input_devices()
        .context("Failed to enumerate audio input devices")?
        .find(|d| {
            d.name()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains(&needle)
        })
        .with_context(|| format!(
            "Audio capture input device containing '{}' not found. \
             macOS 用户：请先安装 BlackHole 并把系统输出指到多输出设备（见 README Mac 章节）",
            needle
        ))?;

    let device_name = device.name().unwrap_or_else(|_| "Unknown device".to_string());
    info!("Using capture device: {}", device_name);
    event_tx
        .send(ServerEvent::Log(format!("Audio device: {}", device_name)))
        .ok();

    // ── 格式完全跟随设备当前实际输入配置（BlackHole 常见 48kHz / 2ch / f32）──
    let input_cfg = device
        .default_input_config()
        .context("Failed to get device default input format")?;
    let sample_rate = input_cfg.sample_rate().0;
    let channels = input_cfg.channels();
    let sample_format = input_cfg.sample_format();
    // From<SupportedStreamConfig>：缓冲大小等由 cpal 按设备默认值决定
    let stream_config = cpal::StreamConfig::from(input_cfg);

    info!(
        "Capture format (cpal): {}Hz, {}ch, {:?}",
        sample_rate, channels, sample_format
    );

    // 回填真实捕获格式 → 手机会收到正确的 audio_config 头（与 Windows 路径同逻辑）
    {
        let mut fmt = client_manager.capture_format.blocking_lock();
        *fmt = (sample_rate, channels);
    }

    let bridge_tx = client_manager.audio_bridge_tx.clone();
    let err_fn = |err| error!("[Capture] cpal stream error: {}", err);

    let stream = match sample_format {
        // s16：与桥接格式一致，零转换直接转字节
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
        )?,
        // f32：Mac 设备最常见的格式。先限幅再转 s16，防脏数据溢出爆音
        cpal::SampleFormat::F32 => {
            let bridge_tx = client_manager.audio_bridge_tx.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let i16_data: Vec<i16> = data
                        .iter()
                        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                        .collect();
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            i16_data.as_ptr() as *const u8,
                            i16_data.len() * 2,
                        )
                    };
                    bridge_tx.send(bytes.to_vec()).ok();
                },
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::F64 => {
            let bridge_tx = client_manager.audio_bridge_tx.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[f64], _: &cpal::InputCallbackInfo| {
                    let i16_data: Vec<i16> = data
                        .iter()
                        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                        .collect();
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            i16_data.as_ptr() as *const u8,
                            i16_data.len() * 2,
                        )
                    };
                    bridge_tx.send(bytes.to_vec()).ok();
                },
                err_fn,
                None,
            )?
        }
        other => anyhow::bail!("Unsupported capture sample format: {:?}", other),
    };

    stream.play()?;
    info!(
        "Audio capture started (cpal): {}Hz, {}ch",
        sample_rate, channels
    );
    event_tx
        .send(ServerEvent::Log(format!(
            "Capture started: {}Hz, {}ch",
            sample_rate, channels
        )))
        .ok();

    // 阻塞保活：`stream` 必须一直活在作用域里（drop 即停止捕获）
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

    // ── 性能优化（实时性）：关闭 Nagle 算法 ──────────────────────
    // Nagle 会把"小包等下一个小包攒一起发"，和接收端的延迟 ACK
    // 撞上时会给每一帧音频/视频凭空加上最多 ~40ms 的抖动。
    // 我们是实时流，宁可多发几个小包也不能等，所以每个连接一建立
    // 就设置 TCP_NODELAY（对上下行都生效，握手完成后 ws 沿用同一 socket）。
    if let Err(e) = stream.set_nodelay(true) {
        info!("Failed to set TCP_NODELAY: {}", e);
    }

    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed: {}", e);
            return;
        }
    };

    let (ws_sink, mut ws_receiver) = ws_stream.split();
    let ws_sink: Arc<Mutex<WsSink>> = Arc::new(Mutex::new(ws_sink));
    let mut audio_rx = client_manager.add_client(client_key.clone()).await;
    // v3.1：登记文本写端，捕获状态翻转时可向本客户端推 mic_state
    client_manager
        .text_sinks
        .lock()
        .await
        .insert(client_key.clone(), ws_sink.clone());

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
    {
        let mut sink = ws_sink.lock().await;
        if let Err(e) = sink.send(Message::Text(header)).await {
            error!("Failed to send header: {}", e);
            client_manager.remove_client(&client_key).await;
            return;
        }
    }

    // Downlink send task: loopback audio → WebSocket
    // 客户端被移出广播表时 audio_rx 自然关闭，任务退出（Mic 模式即走此路径停止下行）
    let sink_send = ws_sink.clone();
    let key_send = client_key.clone();
    let send_task = tokio::spawn(async move {
        while let Some(data) = audio_rx.recv().await {
            let mut sink = sink_send.lock().await;
            if sink.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
        info!("[{}] Downlink task exited", key_send);
    });

    // Receive task: client messages + v3 mic uplink
    let sink_recv = ws_sink.clone();
    let mgr_recv = client_manager.clone();
    let key_recv = client_key.clone();
    let event_tx_recv = event_tx.clone();
    let mic_live_recv = client_manager.mic_live.clone();
    let mic_muted_recv = client_manager.mic_muted.clone();
    // v3.4：摄像头会话/占用标志（跨任务共享）
    let cam_session_recv = client_manager.cam_session.clone();
    let cam_live_recv = client_manager.cam_live.clone();
    let recv_task = tokio::spawn(async move {
        // Mic 会话状态（未收到 mic_start 前行为与 v2 完全相同）
        // 全双工：mic 期间下行转发照常，不摘除订阅
        // v3.3：手机连接即发 mic_start 进入【待命】（麦克风硬件关闭）；
        // 本服务器检测到 PC 应用开始录 CABLE Output 时推 mic_state true，
        // 手机才开硬件上行；PC 应用停止 → 推 false → 手机立刻停录。
        let mut mic_session = false;
        let mut mic_packets: u64 = 0;
        let mut mic_bytes_window: u64 = 0;
        let mut mic_bytes_total: u64 = 0;
        let mut last_pkt: Option<Instant> = None;
        let mut interval_sum: u64 = 0;
        let mut interval_n: u64 = 0;
        let mut window_start = Instant::now();

        // v3.4：摄像头上行统计（独立窗口，每秒推一次 CamStats）
        let mut cam_packets: u64 = 0;
        let mut cam_bytes_window: u64 = 0;
        let mut cam_window_start = Instant::now();
        let mut cam_frame_dims: (u16, u16) = (0, 0);
        // GUI 预览用：暂存窗口内最新一帧，随每秒统计一起发出（1fps 足够看画面）
        let mut cam_preview_hold: Option<Vec<u8>> = None;

        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    info!("Client message: {}", text);
                    // 去掉空白后做轻量匹配，无需 serde
                    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                    if compact.contains("\"type\":\"mic_start\"") && !mic_session {
                        let sr = json_number(&compact, "sample_rate").unwrap_or(48000) as u32;
                        let ch = json_number(&compact, "channels").unwrap_or(1) as u16;
                        // v3.4.4：把上行采样率告知注入引擎（44.1k≠设备48k时启用重采样）
                        mic_out::set_uplink_rate(sr);
                        mic_session = true;
                        // v3.1 修复：新会话一律重置全局静音标志。
                        // 否则上一个客户端按过静音后标志残留 true，
                        // 新连接（甚至新重启的客户端）上行会被全部丢弃 —— 
                        // 这正是"手机显示在录音、电脑却完全无声"的元凶之一。
                        mic_muted_recv.store(false, Ordering::Relaxed);
                        // 顺带清空上一会话残留的尾音，GUI 静音角标复位
                        if let Ok(mut q) = mgr_recv.mic_queue.lock() {
                            q.clear();
                        }
                        event_tx_recv.send(ServerEvent::MicMuted { muted: false }).ok();
                        // 回执给手机（附带当前捕获状态：若应用已在用麦克风，手机立即上行）
                        let live_now = mic_live_recv.load(Ordering::Relaxed);
                        let mut sink = sink_recv.lock().await;
                        let _ = sink
                            .send(Message::Text(format!(
                                r#"{{"type":"mic_ack","active":{}}}"#,
                                live_now
                            )))
                            .await;
                        drop(sink);
                        event_tx_recv
                            .send(ServerEvent::MicSource {
                                ip: key_recv.clone(),
                                sample_rate: sr,
                                channels: ch,
                            })
                            .ok();
                        event_tx_recv
                            .send(ServerEvent::Log(format!(
                                "[Mic] Standby session: {}Hz, {}ch from {} (live={})",
                                sr, ch, key_recv, live_now
                            )))
                            .ok();
                    } else if compact.contains("\"type\":\"mic_stop\"") && mic_session {
                        // 手机端主动结束会话（断开/手动关闭）
                        mic_session = false;
                        if mic_live_recv.load(Ordering::Relaxed) {
                            event_tx_recv.send(ServerEvent::MicStopped).ok();
                        }
                        event_tx_recv
                            .send(ServerEvent::Log(format!("[Mic] Uplink stopped by {}", key_recv)))
                            .ok();
                    } else if compact.contains("\"type\":\"mic_mute\"") {
                        // v3.1：手机"静音键"。按下后服务器丢弃上行（等效闭麦），
                        // 并清空抖动队列 —— 防止关麦前的尾音还在 PC 里播放
                        // muted 是布尔字段，直接匹配序列化后的 "muted":true
                        let muted = compact.contains("\"muted\":true");
                        mic_muted_recv.store(muted, Ordering::Relaxed);
                        if muted {
                            if let Ok(mut q) = mgr_recv.mic_queue.lock() {
                                q.clear();
                            }
                        }
                        event_tx_recv.send(ServerEvent::MicMuted { muted }).ok();
                        event_tx_recv
                            .send(ServerEvent::Log(format!(
                                "[Mic] {} by {}",
                                if muted { "Muted (manual)" } else { "Unmuted" },
                                key_recv
                            )))
                            .ok();
                    // ── v3.4 摄像头指令 ──
                    } else if compact.contains("\"type\":\"cam_capabilities\"") {
                        // 手机上报相机硬件能力（每颗镜头的分辨率/帧率清单）。
                        // 服务器不解析细节，原文转给 GUI 展示"手机最高支持什么画质"
                        event_tx_recv
                            .send(ServerEvent::CamCaps {
                                caps: text.to_string(),
                                ip: key_recv.clone(),
                            })
                            .ok();
                        event_tx_recv
                            .send(ServerEvent::Log(format!(
                                "[Cam] Capabilities from {}: {}",
                                key_recv, compact
                            )))
                            .ok();
                    } else if compact.contains("\"type\":\"cam_start\"") {
                        // 手机相机硬件已打开、开始推 JPEG 帧（或待命登记，语义同 mic_start）
                        cam_session_recv.store(true, Ordering::Relaxed);
                        // 回执附带当前占用状态：若应用已在观看，手机保持推流；
                        // 若无人观看，手机可回到"硬件关闭"待命，等 cam_state 唤醒
                        let live_now = cam_live_recv.load(Ordering::Relaxed);
                        let mut sink = sink_recv.lock().await;
                        let _ = sink
                            .send(Message::Text(format!(
                                r#"{{"type":"cam_ack","active":{}}}"#,
                                live_now
                            )))
                            .await;
                        drop(sink);
                        event_tx_recv
                            .send(ServerEvent::CamSource { ip: key_recv.clone() })
                            .ok();
                        event_tx_recv
                            .send(ServerEvent::Log(format!(
                                "[Cam] Session from {} (live={})",
                                key_recv, live_now
                            )))
                            .ok();
                    } else if compact.contains("\"type\":\"cam_stop\"") {
                        // 手机端主动结束摄像头会话（或手机端确认关闭）
                        cam_session_recv.store(false, Ordering::Relaxed);
                        if let Ok(mut mb) = mgr_recv.cam_mailbox.lock() {
                            *mb = None;
                        }
                        if let Ok(mut mb) = mgr_recv.cam_mailbox_obs.lock() {
                            *mb = None;
                        }
                        event_tx_recv.send(ServerEvent::CamStopped).ok();
                        event_tx_recv
                            .send(ServerEvent::Log(format!("[Cam] Stopped by {}", key_recv)))
                            .ok();
                    }
                }
                Ok(Message::Binary(data)) => {
                    // v3.4：摄像头 JPEG 帧带 4 字节魔术头 [0x03,'C','A','M']，
                    // PCM 音频包撞头的概率约 2^-32，可安全分流两种二进制帧
                    if data.len() > 4 && data[0] == 0x03 && &data[1..4] == b"CAM" {
                        if cam_session_recv.load(Ordering::Relaxed) {
                            let jpeg = data[4..].to_vec();
                            // v3.4 双通道：同一帧分别投给两个引擎的邮箱
                            vcam::push_frame(&mgr_recv.cam_mailbox, jpeg.clone());
                            vcam_obs::push_frame(&mgr_recv.cam_mailbox_obs, jpeg);
                            cam_preview_hold = Some(data[4..].to_vec());
                            cam_packets += 1;
                            cam_bytes_window += data.len() as u64;
                            // 从 JPEG 头里读出这一帧的分辨率（用于 GUI 显示"当前画质"）
                            if let Some((w, h)) = jpeg_dims(&data[4..]) {
                                cam_frame_dims = (w, h);
                            }
                            // 每秒推一次摄像头链路统计（附带最新一帧给 GUI 预览）
                            let elapsed = cam_window_start.elapsed();
                            if elapsed.as_millis() >= 1000 {
                                let secs = elapsed.as_millis() as f64 / 1000.0;
                                let kbps = (cam_bytes_window as f64 * 8.0 / 1000.0 / secs) as u32;
                                let fps = (cam_packets as f64 / secs) as u32;
                                event_tx_recv
                                    .send(ServerEvent::CamStats {
                                        kbps,
                                        fps,
                                        frames: cam_packets,
                                        width: cam_frame_dims.0,
                                        height: cam_frame_dims.1,
                                    })
                                    .ok();
                                if let Some(frame) = cam_preview_hold.take() {
                                    event_tx_recv.send(ServerEvent::CamFrame(frame)).ok();
                                }
                                cam_packets = 0;
                                cam_bytes_window = 0;
                                cam_window_start = Instant::now();
                            }
                        }
                    }
                    // v3.3：会话已登记（mic_start）且未静音 → 一律接收注入。
                    // 硬件开关由手机按 mic_state 自己执行（待命时根本不会发数据），
                    // 服务器不做二次判断 —— 双端语义一致，链路最简单可靠。
                    // mic_live 标志同时驱动 mic_state 广播与 GUI 显示。
                    else if mic_session && !mic_muted_recv.load(Ordering::Relaxed) {
                        let level = rms_level(&data);
                        mic_out::push_uplink(&mgr_recv.mic_queue, &data);
                        mic_packets += 1;
                        mic_bytes_window += data.len() as u64;
                        mic_bytes_total += data.len() as u64;
                        if let Some(t) = last_pkt {
                            interval_sum += t.elapsed().as_millis() as u64;
                            interval_n += 1;
                        }
                        last_pkt = Some(Instant::now());
                        // 每 5 包推一次电平（手机侧 10ms/包 ≈ 20Hz 刷新）
                        if mic_packets % 5 == 0 {
                            event_tx_recv.send(ServerEvent::MicLevel(level)).ok();
                        }
                        // 每秒推一次链路统计
                        let elapsed = window_start.elapsed();
                        if elapsed.as_millis() >= 1000 {
                            let secs = elapsed.as_millis() as f64 / 1000.0;
                            let kbps = (mic_bytes_window as f64 * 8.0 / 1000.0 / secs) as u32;
                            let interval_ms =
                                if interval_n > 0 { (interval_sum / interval_n) as u32 } else { 0 };
                            event_tx_recv
                                .send(ServerEvent::MicStats {
                                    kbps,
                                    total_kb: mic_bytes_total / 1024,
                                    packets: mic_packets,
                                    interval_ms,
                                })
                                .ok();
                            mic_bytes_window = 0;
                            interval_sum = 0;
                            interval_n = 0;
                            window_start = Instant::now();
                        }
                    }
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

        if mic_session {
            // 会话结束（无论待命还是上行中），GUI 复位
            event_tx_recv.send(ServerEvent::MicStopped).ok();
            event_tx_recv
                .send(ServerEvent::Log(format!("[Mic] Uplink ended: {}", key_recv)))
                .ok();
        }
        // v3.4：摄像头会话同样随连接结束而终止，清空邮箱并复位 GUI
        if cam_session_recv.swap(false, Ordering::Relaxed) {
            if let Ok(mut mb) = mgr_recv.cam_mailbox.lock() {
                *mb = None;
            }
            if let Ok(mut mb) = mgr_recv.cam_mailbox_obs.lock() {
                *mb = None;
            }
            event_tx_recv.send(ServerEvent::CamStopped).ok();
            event_tx_recv
                .send(ServerEvent::Log(format!("[Cam] Uplink ended: {}", key_recv)))
                .ok();
        }
    });

    // 注意：不能用 select!（send_task 因 mic 摘除订阅而正常退出时会误关整条连接），
    // 必须 join 等两个任务都结束
    let _ = tokio::join!(send_task, recv_task);

    client_manager.remove_client(&client_key).await;
    event_tx.send(ServerEvent::ClientDisconnected(client_key)).ok();
}

// ── v3 麦克风上行支撑 ─────────────────────────────────────────────

/// 从 JPEG 段结构中读出图像宽高（SOF 标记 0xC0..0xCF，跳过 DHT/C8/DAC）。
/// 只扫段头不解析熵数据，开销极小，用于 GUI 显示"当前画质"。
fn jpeg_dims(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]);
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]);
            return if w > 0 && h > 0 { Some((w, h)) } else { None };
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        i += 2 + seg_len.max(2);
    }
    None
}

/// 计算 PCM s16le 数据的 RMS 电平，归一化到 0.0 ~ 1.0
fn rms_level(bytes: &[u8]) -> f32 {
    let mut acc: f64 = 0.0;
    let mut n: u64 = 0;
    for chunk in bytes.chunks_exact(2) {
        let s = i16::from_le_bytes([chunk[0], chunk[1]]) as f64;
        acc += s * s;
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        ((acc / n as f64).sqrt() / 32768.0) as f32
    }
}

/// 从压缩后的 JSON 文本中提取数字字段。
/// 仅用于我们自定义的 mic_start 协议头，字段少且格式可控，不引入 serde。
fn json_number(compact: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{}\":", key);
    let idx = compact.find(&pat)? + pat.len();
    let digits: String = compact[idx..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}
