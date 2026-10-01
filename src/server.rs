// ══ 本文件说明书（初学者先读这段）════════════════════════════════════
// 角色：PC 端服务核心 —— 一个 WebSocket 服务器 + 音频/视频引擎的总装线。
// 被谁调用：main.rs 的 egui 图形界面点"启动"后，在专用后台线程里建 tokio
//   运行时并 block_on(run_server)；无界面的命令行版 bin/server.rs 走同一入口。
// 对外暴露：ServerConfig（四个可调参数）、ServerEvent（服务器→GUI 事件）、
//   ServerCommand（GUI→服务器命令）、ServerStatus、run_server（总入口）。
// 本文件内的线程/任务（另有 mic_out/vcam/vcam_obs 引擎线程在兄弟文件）：
//   ① WASAPI 环回采集线程（std::thread，内含第二条取数线程）：录系统声音；
//   ② tokio 转发任务：音频包 → 广播进每个客户端的专属通道；
//   ③ 麦克风占用监测、摄像头活跃合并等几个小 tokio 任务；
//   ④ 每条手机连接 2 个 tokio 任务：下行发送（音频+心跳）与上行接收。
// 下行数据路径：扬声器出声 → WASAPI loopback 抓取 → 统一转 PCM s16le
//   字节 → mpsc 通道 → 转发任务 → WebSocket 二进制帧 → 手机实时播放。
// 上行数据路径：手机麦克风 PCM / 摄像头 JPEG → WebSocket 二进制帧 →
//   本文件按 4 字节魔术头分流 → mic_queue（注入 VB-CABLE）或
//   cam_mailbox（写入 UnityCapture/OBS 虚拟摄像头）。
// 跨线程三件套速记：Arc=多方共享同一数据；Mutex=同一时刻只一人改；
//   mpsc 通道=线程/任务之间传数据的管道，send 不阻塞、recv().await 零开销等待。
// ═══════════════════════════════════════════════════════════════════════
use anyhow::Result;
// anyhow::Result = 项目统一的"可能成功 Ok / 可能失败 Err"返回类型；
// 失败值沿 ? 运算符一路向上传给调用者，直到有人处理或打日志为止。
// 下面这行 #[cfg(not(windows))] 是条件编译属性：仅 macOS 分支需要 context()
// 补充错误说明，Windows 编译时整行 use anyhow::Context 被跳过（防未用警告）。
#[cfg(not(windows))]
use anyhow::Context;
// futures_util 的两个 trait：SinkExt 让写端有 .send()，StreamExt 让读端有
// .next()。trait（能力）不导入，方法就调不出来 —— Rust 特有的显式规则。
use futures_util::{SinkExt, StreamExt};
// 日志宏：info!/warn!/error! 的输出由 main.rs 的 DualLogger 同时写两处 ——
// GUI 日志窗口 + exe 同目录的 audioserver.log 文件（排障优先看文件）。
use log::{error, info, warn};
// 键值表：客户端标识 → 发送通道 / WebSocket 写端
use std::collections::HashMap;
// AtomicBool：不加锁也能跨线程安全读写的 true/false 开关（mic_live 等）。
// Ordering::Relaxed：只保证"最终大家都能看到新值"，不额外排序 —— 对
// "暂停/静音/活跃"这类状态开关足够，且是性能最好的内存序。
use std::sync::atomic::{AtomicBool, Ordering};
// Arc（原子引用计数智能指针）：多线程共同持有同一份数据，最后一个持有者
// drop 后才释放。三个字母各解决什么详见 ClientManager 上方的说明。
use std::sync::Arc;
// 单调时钟：只用来量时间间隔（统计窗口、包间隔），不受改系统时间影响。
use std::time::Instant;
// tokio 的异步 TCP：accept 等连接时任务挂起、线程去干别的，绝不空等。
use tokio::net::{TcpListener, TcpStream};
// tokio::sync::mpsc：多生产者单消费者异步通道，跨线程/跨任务传数据首选。
// tokio::sync::Mutex：异步互斥锁；.lock().await 排队等锁时不霸占工作线程。
use tokio::sync::{mpsc, Mutex};
// WebSocket 协议库（tungstenite）：Message 是协议帧的枚举 ——
// Text(JSON 文本帧=控制指令)、Binary(二进制帧=PCM 音频/JPEG 画面)、
// Ping/Pong(心跳)、Close(关闭)。
use tokio_tungstenite::tungstenite::Message;
// 一条完成握手后的 WebSocket 连接对象（可 split 成读/写两半）
use tokio_tungstenite::WebSocketStream;

// 兄弟模块：mic_out=手机上行 PCM 注入 VB-CABLE 的引擎；
// vcam=UnityCapture 虚拟摄像头引擎；vcam_obs=OBS Virtual Camera 引擎。
// MicQueue = Arc<Mutex<VecDeque<i16>>>（上行样本队列）
// FrameMailbox = Arc<Mutex<Option<Vec<u8>>>>（最新 JPEG 帧邮箱）
use crate::mic_out::{self, MicQueue};
use crate::vcam::{self, FrameMailbox};
use crate::vcam_obs;

// ── Windows FFI（外部函数接口）：直调系统 DLL 里的 COM 组件 ──────────
// windows crate 把 Windows SDK 一比一绑成 Rust；COM 调用一律要写 unsafe，
// 因为编译器无法验证跨边界指针的有效性；调用约定/句柄管理由 crate 代劳。
// WASAPI 音频组：IAudioClient=设备上的音频流控制器、IAudioCaptureClient=
// 环回取数泵、IMMDevice=一台声卡设备、IMMDeviceEnumerator=设备枚举器
// （MMDeviceEnumerator 是其 COM 实现类）；eRender=播放方向、eConsole=
// 多媒体角色、AUDCLNT_SHAREMODE_SHARED=共享模式（走系统混音器）、
// AUDCLNT_STREAMFLAGS_LOOPBACK=★环回标志：捕获"正在播放的声音"而非麦克风。
#[cfg(windows)]
use windows::Win32::Media::Audio::{
    eConsole, eRender, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator,
};
// COM 基础设施组：CoInitializeEx=给当前线程初始化 COM"套间"（线程模型
// 作用域，不初始化就调不了任何系统 COM 对象）、CoCreateInstance=按 CLSID
// 构造 COM 对象（相当于 new）、CLSCTX_ALL=允许进程内/远端实现、
// COINIT_APARTMENTTHREADED=单线程套间模型、STGM_READ=只读打开属性存储。
#[cfg(windows)]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED, STGM_READ,
};
// 设备属性存储的"键"类型：本文件只用它读设备友好名（如 "Speakers (Realtek)"）。
#[cfg(windows)]
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
#[cfg(windows)]
use windows::core::GUID;

/// Server configuration
///
/// 界面上能改的就是这四个（v3.8 起它们的初值来自 config.json）。
/// 其余运行参数（监听地址、Ping 间隔、统计间隔、设备名、虚拟摄像头分辨率…）
/// 一律从 config.json 读，见 `crate::config`。
// 补充：config.json 改动只在新建连接/重启服务时生效 —— 每个连接建立时
// 读一次内存里的配置（见 handle_connection），不会中途换节奏。
// derive(Debug, Clone)：自动实现"可打印调试"与"可整体复制"两个能力。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    // TCP/WebSocket 监听端口，默认 8080。手机连 ws://PC的IP:8080，两端必须一致；
    // 改了不改手机侧就连不上。1024 以下端口在 Windows 通常要管理员权限。
    pub port: u16,
    // 期望采样率 48000Hz = 每秒 4.8 万个音频样本（CD 级音质）。
    // ⚠ 注意：这里只是初值——真实下行格式以 WASAPI 读到的设备混音格式为准
    // （见 capture_format 回填逻辑）；强行按错误值发给手机会变速变调。
    pub sample_rate: u32,
    // 声道数：2=立体声（样本按 L R L R 交错）。改 1 省一半带宽，改 6/8 带宽翻倍。
    pub channels: u16,
    // 名义缓冲 1024 帧：48kHz 下 1024/48000 ≈ 21.3ms 声音一帧。
    // 帧越大单位时间调用越少越省 CPU，但首包延迟越高。
    pub buffer_size: u32,
}

impl Default for ServerConfig {
    // 出厂默认值（GUI 首次启动时的初值，之后由界面/config.json 覆盖）
    fn default() -> Self {
        Self {
            // 8080：非特权端口，和手机 App 默认配置成对出现
            port: 8080,
            // 带宽算式：48000Hz × 2声道 × 2字节/样本(int16) = 192000 字节/秒
            // ≈ 1536 kbps ≈ 1.5 Mbps —— 改采样率/声道数等于按比例乘除这条带宽
            sample_rate: 48000,
            channels: 2,
            buffer_size: 1024,
        }
    }
}

/// Server → GUI events
// 服务器发给界面的一条条"通知"。投递方式：std::sync::mpsc 同步通道
// （发送端 event_tx 一路传遍本文件）。GUI 主循环每帧 try_recv() 批量取走，
// 服务器侧 .send(...).ok() 主动吞掉发送失败 —— GUI 若已关闭就静默丢事件，
// 绝不因为"没人收"而阻塞或崩溃；这也是"UI 线程慢/死不影响服务"的保险丝。
#[derive(Debug, Clone)]
pub enum ServerEvent {
    // 一行日志文本 → GUI 日志区显示。
    // ⚠ 注意：ServerEvent::Log 只进 GUI 内存队列、不落 audioserver.log 文件；
    // 想事后回溯必须同时用 log::info!，本文件多处就是双写的（见 bind 成功分支）。
    Log(String),
    // 服务器 启动/停止 状态变化 → GUI 切换状态灯与按钮
    StatusChanged(ServerStatus),
    // 某客户端连上/断开（携带 "ip:端口" 键）→ GUI 更新在线数
    ClientConnected(String),
    ClientDisconnected(String),
    // 致命错误（如端口绑定失败）→ GUI 红色提示；服务器随后退出
    Error(String),
    // ── v3 麦克风上行事件 ──
    /// 上行音频 RMS 电平（0.0 ~ 1.0），限频约 20Hz 推送
    MicLevel(f32),
    /// 上行链路统计，每秒推送一次
    MicStats {
        // kbps = 窗口内字节数 × 8(位) ÷ 1000 ÷ 窗口秒数；
        // total_kb = 会话累计 KB（÷1024）；packets = 累计包数；
        // interval_ms = 相邻上行包平均间隔（手机 10ms/包 → 约等于 10）
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

// 服务器运行状态两态枚举。Copy=很小可按位复制，PartialEq=能用 == 比较，
// GUI 靠它决定按钮文案与状态灯颜色。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServerStatus {
    Stopped,
    Running,
}

/// GUI → Server commands
// 反方向通道：界面按钮 → 服务器。走 tokio::sync::mpsc（异步接收），
// run_server 主循环里 cmd_rx.recv().await 一条条消费；命令串行处理，
// 天然没有并发冲突。None（发送端被 drop = GUI 已退出）也当"停止"处理。
pub enum ServerCommand {
    // 停止服务器：主循环收到后 break，run_server 正常返回
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
// split() 把一条连接拆成"读半+写半"两半；WebSocket 同一时刻只允许一个写者，
// 所以要发文本指令的接收任务和发音频的发送任务共用一把异步锁排队写。
type WsSink = futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>;

/// Client connection manager
// 全服共享的状态中心：谁在线、往哪发、各开关的当前值。
// 为什么处处是 Arc<Mutex<T>>（三个字母各解决一个问题）：
//   Arc   —— 所有权：转发任务、采集线程、每条连接的任务都要"同时持有"这张表；
//   Mutex —— 并发写：同一瞬间只允许一个任务增删/改值，防止数据竞争；
//   <T>   —— 被保护的数据本体（HashMap 表、(u32,u16) 元组、bool 开关…）。
// 纯开关量（mic_live 等）不用 Mutex 而用 AtomicBool：读写一步完成更轻。
struct ClientManager {
    // 客户端登记表：client_key("ip:端口") → 该客户端专属音频下行通道发送端。
    // 想给某手机停播 = 从表里移除，其接收端自动关闭、下行任务自然退出。
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
    /// 采集线程初始化成功后回填真实值；连接握手发的 audio_config 头读的就是它
    capture_format: Arc<Mutex<(u32, u16)>>,
    /// Bridge: audio capture thread → tokio forwarding task
    /// 使用 tokio::sync::mpsc 替代 crossbeam_channel，
    /// 这样转发任务可以用纯异步 recv().await，不再需要 spawn_blocking
    // 为什么用 mpsc 跨线程传命令/数据：采集线程 send() 立刻返回（不阻塞），
    // tokio 任务 recv().await 零开销等待 —— 两边节奏解耦，天然缓冲。
    audio_bridge_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// v3：手机上行 PCM 队列，由 mic_out 注入线程消费写入 CABLE Input
    /// VB-CABLE：需单独安装的免费虚拟声卡，Input(播)/Output(录) 内部直连
    mic_queue: MicQueue,
    // ── v3.4 摄像头 ──
    /// 最新 JPEG 帧邮箱：WS 接收任务写入，vcam 引擎线程取走解码上传驱动
    /// 邮箱语义 = 只存最新一帧（覆盖旧帧），天然丢过期画面保实时性
    cam_mailbox: FrameMailbox,
    /// v3.4 双通道：OBS 虚拟摄像头引擎的独立邮箱（同一帧推两份，互不抢消费）
    cam_mailbox_obs: FrameMailbox,
    /// 手机已登记摄像头会话（cam_start），准备接收二进制帧
    cam_session: Arc<AtomicBool>,
    /// v3.4.5：记录哪个 client_key 拥有摄像头会话（防止重连时旧连接清理误清新连接状态）
    cam_session_owner: Arc<Mutex<Option<String>>>,
    /// 有应用正在观看虚拟摄像头（vcam 引擎上报），驱动 cam_state 广播
    cam_live: Arc<AtomicBool>,
    /// 双通道各自的活跃信号：cam_live = unity || obs（任一设备被占用就唤醒手机）
    cam_live_unity: Arc<AtomicBool>,
    cam_live_obs: Arc<AtomicBool>,
}

impl ClientManager {
    // 工厂方法：造好整张状态表，并把音频桥通道的"接收端"交还调用者
    //（发送端 self 保留，采集线程经由它投递音频包）。
    fn new() -> (Self, mpsc::UnboundedReceiver<Vec<u8>>) {
        // unbounded_channel：无界队列，send 永不失败于"队满"。
        // ⚠ 注意：风险是消费者若长时间不取，数据会无限积攒占内存；
        // 本文件消费者（转发任务/下行任务）几乎不会停，可接受。
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        (
            Self {
                clients: Arc::new(Mutex::new(HashMap::new())),
                text_sinks: Arc::new(Mutex::new(HashMap::new())),
                mic_live: Arc::new(AtomicBool::new(false)),
                mic_muted: Arc::new(AtomicBool::new(false)),
                // 占位默认 48000Hz/立体声：采集线程读到真实格式后会覆盖。
                // 300ms 内连进来的首条连接理论上可能拿到这份占位值。
                capture_format: Arc::new(Mutex::new((48000, 2))),
                audio_bridge_tx: tx,
                mic_queue: mic_out::new_queue(),
                cam_mailbox: vcam::new_mailbox(),
                cam_mailbox_obs: vcam::new_mailbox(),
                cam_session: Arc::new(AtomicBool::new(false)),
                cam_session_owner: Arc::new(Mutex::new(None)),
                cam_live: Arc::new(AtomicBool::new(false)),
                cam_live_unity: Arc::new(AtomicBool::new(false)),
                cam_live_obs: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    /// v3.1：向所有已连接手机广播麦克风唤醒/休眠状态。
    /// 唤醒瞬间清空上行抖动队列，丢弃任何残留旧数据，保证新会话从实时点开始。
    // 消息格式（Text 帧 JSON）：{"type":"mic_state","active":true/false}
    //   active=PC 端此刻有没有应用在录 CABLE Output；手机收到 true 才打开
    //   麦克风硬件上行，收到 false 立刻关闭 —— 省电 + 隐私的"按需采集"。
    // 发送方向：服务器 → 所有手机。发失败 .await 结果直接 `let _ =` 丢弃：
    //   广播是尽力而为，某台手机掉线不该连累循环。
    async fn broadcast_mic_state(&self, active: bool) {
        self.mic_live.store(active, Ordering::Relaxed);
        if active {
            // if let Ok(...)：std 同步锁可能被中毒（别的线程持锁时 panic），
            // 这里只在上锁成功时清队列，失败就算了 —— 防御式写法。
            if let Ok(mut q) = self.mic_queue.lock() {
                q.clear();
            }
        }
        // 手拼 JSON 字符串（协议简单可控，不引 serde 也能跑；
        // format! 里的 r#""# 是原始字符串，让内层双引号不用转义）
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
    // 消息格式：{"type":"cam_state","active":true/false}（Text 帧，服务器→手机）
    // active = 有没有 PC 应用正在观看虚拟摄像头。
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
        // 新连接入场：现开一条专属下行通道，发送端进登记表，
        // 接收端交给该连接的下行任务 —— 表和任务之间只靠这条通道耦合。
        let (tx, rx) = mpsc::unbounded_channel();
        self.clients.lock().await.insert(client_key, tx);
        rx
    }

    async fn remove_client(&self, client_key: &str) {
        // 离场：两张表都注销。音频通道发送端被 drop 后，下行任务的
        // audio_rx.recv() 返回 None，任务自然收尾（见 handle_connection 尾部注释）。
        self.clients.lock().await.remove(client_key);
        self.text_sinks.lock().await.remove(client_key);
    }

    // 下划线前缀 = 故意保留但暂不调用的函数（告诉编译器"没用到是有意的"）。
    async fn _client_count(&self) -> usize {
        self.clients.lock().await.len()
    }
}

/// Run the audio server (executed in a background thread)
// async fn = 返回一个"将来才会算完"的未来值；每处 .await 表示"我等这一步，
// 等的时候把线程让给别的任务"。调用方（main.rs GUI / bin/server.rs）在专用
// 线程里 block_on 驱动它 —— GUI 线程绝不阻塞的关键就在此：界面和服务器
// 分处两条线程，只通过事件/命令两条通道对话，谁慢都不拖累谁。
pub async fn run_server(
    config: ServerConfig,
    // 服务器→GUI 事件发送端：std 同步版 mpsc（send 立即返回，任何线程可用）
    event_tx: std::sync::mpsc::Sender<ServerEvent>,
    // GUI→服务器命令接收端：tokio 异步版（recv().await 挂起零 CPU）
    mut cmd_rx: mpsc::UnboundedReceiver<ServerCommand>,
) {
    info!("Starting audio server...");

    // 组装全服状态表，同时拿到音频桥通道的接收端 bridge_rx
    let (client_manager, mut bridge_rx) = ClientManager::new();
    // Arc 包一层：转发任务、采集线程、每条连接任务各自 clone() 一个指针，
    // 大家共享的是【同一张表】而不是各自复制一份数据
    let client_manager = Arc::new(client_manager);

    // v3：GUI 切到 Mic 模式时置 true，转发任务跳过 loopback 数据包（直接丢弃，不堆积）
    // 为什么用 AtomicBool 而不是锁里放 bool：GUI 线程 store、转发任务 load，
    // 单值开关原子读写即可；Relaxed 序对"最终生效"的开关语义完全够用。
    let speaker_paused = Arc::new(AtomicBool::new(false));

    // Spawn a Tokio task to forward audio from capture thread to WebSocket clients
    // 【关键修复】不再使用 spawn_blocking 逐包等待，改用纯异步 recv().await。
    // 之前每个音频包都要 spawn_blocking → 阻塞线程 → recv() → 返回 → 再 spawn_blocking，
    // 这个模式依赖 Tokio 阻塞线程池的可用性，容易因线程调度延迟导致数据堆积。
    // 现在用 tokio::sync::mpsc，WASAPI 线程用 send()（同步非阻塞）写入，
    // 转发任务用 recv().await（纯异步）读取，零额外线程、零调度开销。
    // tokio::spawn 与 std::thread 的区别：spawn 不起新操作系统线程，而是把
    // async 块注册进 tokio 线程池成为"任务"——await 挂起时线程立刻去跑别的
    // 任务；一条任务内存开销几百字节，远轻于线程的 MB 级栈。
    // clone 的 Arc 被 move 进 async 块，任务结束自动 drop（引用计数 -1）。
    let fwd_manager = client_manager.clone();
    let fwd_paused = speaker_paused.clone();
    tokio::spawn(async move {
        // 仅用于限频打日志的计数器（每 100 包报 1 条，防止刷屏）
        let mut packet_count: u64 = 0;
        // 通道没关闭就永远有下一包：recv().await 无数据时挂起本任务（零 CPU），
        // 采集线程一 send 立即被唤醒。Some(data)=拿到一包，None=发送端全销毁。
        while let Some(data) = bridge_rx.recv().await {
            // Mic 模式：暂停下行转发
            if fwd_paused.load(Ordering::Relaxed) {
                continue;
            }
            packet_count += 1;
            // %100==1 → 第 1/101/201… 包各打一条日志，"活着证明"式限频
            if packet_count % 100 == 1 {
                info!("[Bridge] Forwarded {} packets, latest {} bytes, {} clients",
                    packet_count, data.len(),
                    fwd_manager.clients.lock().await.len());
            }

            // 锁住登记表 → 给每个在线客户端的专属通道塞一份音频拷贝。
            // 一包 1024 帧@48kHz ≈ 4KB，两三台手机重复几份无所谓。
            // tx.send 是同步非阻塞入队：绝不因为某台手机慢而卡住所有下行。
            let clients = fwd_manager.clients.lock().await;
            let mut failed = Vec::new();
            for (id, tx) in clients.iter() {
                // send 失败唯一原因：该客户端接收端已被 drop（连接任务已退出）
                if tx.send(data.clone()).is_err() {
                    failed.push(id.clone());
                }
            }
            // 显式提前解锁：带着还在生效的锁去下面二次 lock() 会自锁死，
            // drop(clients) 把 Guard 的借用期剪短到此处为止
            drop(clients);
            if !failed.is_empty() {
                // 清掉"电话已断"的僵尸登记（正常断线走 remove_client，这里是兜底）
                let mut clients = fwd_manager.clients.lock().await;
                for id in failed {
                    clients.remove(&id);
                }
            }
        }
        // while 只有通道关闭（返回 None）才会退出 —— 说明转发端没人用了
        info!("[Bridge] Forwarding task exited after {} packets", packet_count);
    });

    // Start audio capture in a separate thread (WASAPI needs a non-async thread)
    // 为什么用 std::thread 而不是 tokio::spawn：start_audio_capture 内部是
    // 全同步死循环 + COM 套间（线程私有环境），塞进 tokio 工作线程会把该
    // 线程整个霸死、饿掉别的任务。真 OS 线程虽重（约 1MB 栈）但互不拖累。
    // 前缀下划线 _audio_handle：join 句柄故意不用——采集线程设计为与进程同寿命。
    let capture_config = config.clone();
    let capture_manager = client_manager.clone();
    let capture_event_tx = event_tx.clone();
    let _audio_handle = std::thread::spawn(move || {
        // if let Err(e)：只关心失败分支。捕获起不来就记一条 error 日志、
        // 线程自然结束——不 panic 不重试，麦克风/摄像头功能不受牵连。
        if let Err(e) = start_audio_capture(capture_config, capture_manager, capture_event_tx) {
            error!("Audio capture failed: {}", e);
        }
    });

    // v3：启动虚拟麦克风注入引擎（把 mic_queue 里的上行 PCM 持续写入 CABLE Input）
    // 手机还没上报采样率之前，上行默认速率取 config.json 的 mic.uplink_sample_rate
    // VB-CABLE 是什么：要单独安装的免费"虚拟声卡"驱动，播放端 CABLE Input 与
    // 录音端 CABLE Output 内部直连，像一根虚拟音频线。手机 PCM 写进 Input，
    // 任何 PC 应用把输入设备选成 Output 就"听见"手机。没装驱动时引擎上报
    // MicEngine{device:None}，GUI 显示不可用，但整个服务不崩（错误只记日志）。
    mic_out::apply_startup_rate();
    mic_out::spawn_mic_output(client_manager.mic_queue.clone(), event_tx.clone());

    // v3.1：启动"应用占用麦克风"检测线程。检测结果通过 std 通道进入 tokio，
    // 再 ① 转成 GUI 事件 ② 向所有已连接手机广播 mic_state 唤醒/休眠指令
    // 复用同一个套路：bool 通道当"同步线程 → 异步任务"的桥。
    // spawn_capture_monitor 内部是 std::thread 轮询检测，只能 send 不能 await。
    let (mon_tx, mut mon_rx) = mpsc::unbounded_channel::<bool>();
    // 发送端 move 进检测线程，接收端 mon_rx 自己留着 await
    mic_out::spawn_capture_monitor(mon_tx);
    let mon_manager = client_manager.clone();
    let mon_event_tx = event_tx.clone();
    tokio::spawn(async move {
        // 每收到一次"占用状态翻转"：先转 GUI 事件，再广播 mic_state。
        // .send(...).ok() 吞错：GUI 关了就算了，广播照做不误。
        while let Some(active) = mon_rx.recv().await {
            mon_event_tx.send(ServerEvent::MicState { active }).ok();
            mon_manager.broadcast_mic_state(active).await;
        }
    });

    // v3.4：启动 Unity Capture 虚拟摄像头注入引擎（消费 cam_mailbox 里的 JPEG 帧）。
    // 引擎线程通过 Want 事件判断"有应用在观看摄像头"，翻转时经通道回传。
    // v3.8：两路引擎各由 config.json 的一个开关控制（默认都开，因为不同应用只认
    // 其中一路：浏览器认 OBS，钉钉一类桌面软件认 Unity）。关掉一路 = 那路应用就
    // 看不到手机摄像头，这是有意的取舍，不是 bug。
    let cam_cfg = crate::config::get().camera;
    let (cam_tx, mut cam_rx) = mpsc::unbounded_channel::<bool>();
    if cam_cfg.unity_enabled {
        vcam::spawn_vcam(client_manager.cam_mailbox.clone(), cam_tx.clone());
    } else {
        info!("[Server] camera.unity_enabled = false → 不启动 Unity Capture 通道");
    }

    // v3.4 双通道：再开一条 OBS Virtual Camera 注入线（浏览器/新框架应用只认它）。
    // OBS 协议没有 Want 事件，用隐私监控注册表判断"有应用在访问摄像头"。
    let (cam_obs_tx, mut cam_obs_rx) = mpsc::unbounded_channel::<bool>();
    if cam_cfg.obs_enabled {
        vcam_obs::spawn_vcam_obs(client_manager.cam_mailbox_obs.clone(), cam_obs_tx.clone());
    } else {
        info!("[Server] camera.obs_enabled = false → 不启动 OBS Virtual Camera 通道");
    }
    // 没启动的那一路也必须留一个发送端在这里：通道一旦全关，下面 select! 的对应
    // 分支会立刻拿到 None，两路都关时整个合并循环还会直接 break。
    let _cam_tx_keepalive = (cam_tx, cam_obs_tx);

    // 两路活跃信号在这里合并（OR 语义）：任一虚拟摄像头被观看 → 唤醒手机开相机；
    // 两路都释放 → 手机立刻关相机回待命。翻转时 ① 推 GUI 事件 ② 广播 cam_state
    // tokio::select! = "同时等多个异步事件，谁先完成就跑谁的分支"（随机公平）。
    // swap(新值) 返回旧值：新旧不同才动作 —— 只在状态翻转瞬间推一次，
    // 不会每秒重复轰炸手机；cam_live 这个 AtomicBool 同时是合并结果的缓存。
    let cam_manager = client_manager.clone();
    let cam_event_tx = event_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(v) = cam_rx.recv() => { cam_manager.cam_live_unity.store(v, Ordering::Relaxed); }
                Some(v) = cam_obs_rx.recv() => { cam_manager.cam_live_obs.store(v, Ordering::Relaxed); }
                // else 分支：两个通道同时全部拿到 None（都关闭）才触发 → break。
                else => break,
            }
            let now = cam_manager.cam_live_unity.load(Ordering::Relaxed)
                || cam_manager.cam_live_obs.load(Ordering::Relaxed);
            if cam_manager.cam_live.swap(now, Ordering::Relaxed) != now {
                cam_event_tx.send(ServerEvent::CamState { active: now }).ok();
                cam_manager.broadcast_cam_state(now).await;

                // v3.4.10：应用真的打开了虚拟摄像头，但手机上还没有摄像头会话
                // → 光推 cam_state 没人接（摄像头守护没开时 provider 不在待命态），
                //   必须同时发 cam_request 让手机弹出"PC 请求使用摄像头"确认横幅，
                //   用户点同意才登记会话、开始推流。这是"入口 B"的自动触发版。
                if now && !cam_manager.cam_session.load(Ordering::Relaxed) {
                    cam_manager
                        .broadcast_cam_text(r#"{"type":"cam_request"}"#)
                        .await;
                    // 直接 info!：ServerEvent::Log 只进 GUI 事件队列，不落 audioserver.log，
                    // 出问题时没法回溯
                    info!(
                        "[Cam] App opened the virtual camera but no phone session → sent cam_request"
                    );
                    cam_event_tx
                        .send(ServerEvent::Log(
                            "[Cam] App opened the virtual camera but no phone session \
                             → sent cam_request (phone shows consent banner)"
                                .to_string(),
                        ))
                        .ok();
                }
            }
        }
    });

    // Wait for audio capture to initialize
    // 300ms 固定等待：给采集线程时间打开设备、读到真实混音格式并回填
    // capture_format（回填前握手会发占位值 48000/2）。
    // ⚠ 注意：这是"尽力而为"的时序约定而非严格同步；风险是慢设备上最早
    // 连进来的手机拿到占位格式，需重连纠正——现状极少触发，故未加握手锁等待。
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Start WebSocket server
    // 监听地址来自 config.json 的 network.bind，默认 "0.0.0.0" = 所有网卡
    // （手机要从局域网连进来，只能监听回环就连不上）。改成具体网卡地址可以把
    // 服务限制在某个网段，例如 "127.0.0.1" 只允许 USB 有线（adb forward）连进来。
    let bind = crate::config::get().network.bind;
    // 拼监听地址字符串，例："0.0.0.0:8080"（bind=网卡地址，port=端口）
    let addr = format!("{}:{}", bind, config.port);
    // match 穷举 Result 两个分支 —— Rust 错误处理第一课：失败不是异常，
    // 是一个必须当场想好两种案的值。Ok(l) 取出监听器，Err(e) 走失败分支。
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            // 绑定成功：先给 GUI 报"监听中 + 状态 Running"。
            // 手机此时应能连入；防火墙弹窗要允许，否则局域网连不上。
            event_tx.send(ServerEvent::Log(format!(
                "WebSocket server listening on {}:{}",
                bind, config.port
            ))).ok();
            event_tx.send(ServerEvent::StatusChanged(ServerStatus::Running)).ok();
            l
        }
        Err(e) => {
            // 绑定失败（最常见：10048 端口被占用 / bind 地址不存在）：
            // 通知 GUI 红色报错后直接 return —— async fn 的 return 就是整个
            // 服务器启动失败，本任务安静结束，不 panic、不重试。
            event_tx.send(ServerEvent::Error(format!("Failed to bind port: {}", e))).ok();
            return;
        }
    };

    // Main loop: accept connections + handle commands
    // 服务器心脏：一个 select! 同时等"新 TCP 连接"和"GUI 命令"两件事。
    // listener.accept().await 在无人连接时完全挂起零开销；来一条连接就
    // spawn 一条专属任务处理，主循环马上回来继续等 —— 这就是异步服务器
    // 能同时伺候多台手机的原理（对比：同步写法一条连接卡住全盘）。
    loop {
        tokio::select! {
            // 分支一：accept 返回 Result<(TCP流, 对端地址)>，match 两层解包
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
                        // 每条连接起一条独立 tokio 任务（handle_connection），
                        // stream/client_key 的所有权 move 进任务；manager/event_tx
                        // 用 clone 的 Arc/Sender，主循环继续持有原件。
                        // 任务之间互不阻塞：一台手机慢/断线不影响别人。
                        tokio::spawn(async move {
                            handle_connection(stream, client_key, manager, event_tx_clone.clone()).await;
                        });
                    }
                    Err(e) => {
                        // accept 失败通常是瞬时的（fd 耗尽等），记日志继续循环等下一个
                        error!("Accept failed: {}", e);
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                // 分支二：GUI 发来一条命令。match 的 Option 两层：
                // Some(命令)=正常指令；None=发送端全被 drop（GUI 已退出），
                // 与显式 Stop 同等对待 —— 防止界面关了服务器还僵尸运行。
                match cmd {
                    Some(ServerCommand::Stop) | None => {
                        info!("Stopping server...");
                        // break 跳出 loop → 走到函数末尾发 Stopped 事件收尾
                        break;
                    }
                    Some(ServerCommand::SetSpeakerPaused(paused)) => {
                        // 纯开关：AtomicBool 置位，转发任务下一包立即生效
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

    // 服务器退出收尾（主循环 break 后才走到这里）。
    // ⚠ 注意：这里的行为是只通知 GUI"已停止"，已 spawn 的采集线程/其余任务
    // 并不显式回收（异步任务随运行时结束消失，std 线程见 start_audio_capture
    // 尾部说明）；风险是 block_on 返回后 run_server 内线程仍驻留到进程退出。
    event_tx.send(ServerEvent::Log("Server stopped".to_string())).ok();
    event_tx.send(ServerEvent::StatusChanged(ServerStatus::Stopped)).ok();
}

/// 通过属性存储获取设备友好名称（如 "Speakers (Realtek Audio)"）
///
/// PKEY_Device_FriendlyName 的 GUID 值（Windows SDK 标准定义）
// PROPERTYKEY = {全局唯一格式 ID(16 字节 GUID) + 32 位编号}：Windows 把每台
// 设备的元数据（名字、图标…）存成"属性存储"，按这串对折键取值。
// GUID::from_values 的参数就是 SDK 头文件里的字面量，抄错一个字符就读不到名字。
#[cfg(windows)]
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_values(
        0xa45c254e, 0xdf1c, 0x4efd,
        [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
    ),
    pid: 14,
};

/// 读一台设备的友好名（与 mic_out.rs 同一手法：直接按 PROPVARIANT 内存布局取值）
///
/// 单独抽出来是因为 v3.8 起扬声器回路（loopback）也支持按名字选设备了，
/// 选设备必须先能读到名字。
#[cfg(windows)]
fn device_friendly_name(device: &IMMDevice) -> String {
    // 整段为什么必须 unsafe：下面要按字节解读 C 结构 PROPVARIANT、解裸指针，
    // 编译器无法验证内存布局对不对，安全责任由程序员按 SDK 文档担保。
    // COM 对象（store/pv）在 Rust 里离开作用域自动 Release —— 这就是 C 世界
    // "句柄/引用计数必须手动关闭"问题的 RAII 解法，忘不掉也 double-free 不了。
    unsafe {
        match device.OpenPropertyStore(STGM_READ) {
            Ok(store) => {
                match store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) {
                    Ok(pv) => {
                        // PROPVARIANT 内存布局：
                        //   offset 0: vt (u16) — 类型标识，VT_LPWSTR = 31
                        //   offset 8: pwszVal (*mut u16) — 宽字符串指针
                        // 为什么不用 API 函数解包而直接按偏移读：windows crate
                        // 该版本的 PROPVARIANT 取值助手不开放（mic_out.rs 同法）。
                        let pv_ptr = &pv as *const _ as *const u8;
                        let vt = *(pv_ptr as *const u16);
                        if vt == 31 {
                            // VT_LPWSTR: 字符串指针在 offset 8
                            // （Windows 宽字符串 = UTF-16 序列，0 结尾）
                            let pwsz = *(pv_ptr.add(8) as *const *const u16);
                            if !pwsz.is_null() {
                                // 手动数到 0 求长度（C 字符串惯例），再切片段。
                                // from_utf16_lossy：UTF-16 → UTF-8，遇到坏码位
                                // 替换为  而不是整体报错 —— 读设备名要"能读到就行"。
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
    }
}

/// 在"当前可用的播放设备"里按友好名模糊找一台（大小写不敏感、子串匹配）
///
/// 找不到（或枚举失败）就返回 None，由调用方退回系统默认播放设备——
/// 绝不因为配置里写错一个名字就让程序起不来。
#[cfg(windows)]
unsafe fn find_render_device_by_name(
    enumerator: &IMMDeviceEnumerator,
    hint: &str,
) -> Option<IMMDevice> {
    // unsafe fn：整个函数体在调用者的 unsafe 上下文里执行（COM 枚举调用），
    // 所以调用方（start_audio_capture）也要包在 unsafe {} 中。
    // EnumAudioEndpoints(eRender, ACTIVE) = "所有当前插着/启用的播放设备"清单；
    // DEVICE_STATE_ACTIVE 过滤掉禁用和掉线的（不然 Item(i) 可能取到死设备）。
    let collection = match enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
        Ok(c) => c,
        Err(e) => {
            // 枚举失败只 warn 返回 None → 上层退回默认设备：
            // 设备选择是"锦上添花"，绝不让主流程因此起不来
            warn!("[Speaker] EnumAudioEndpoints(eRender) failed: {e:?}");
            return None;
        }
    };
    // 两段 GetCount/Item 是 COM 集合的标准遍历（没有迭代器支持，只能索引）
    let count = match collection.GetCount() {
        Ok(n) => n,
        Err(e) => {
            warn!("[Speaker] device collection count failed: {e:?}");
            return None;
        }
    };
    // 模糊匹配：统一转大写后做子串查找——用户只写 "CABLE" 也能命中
    // "CABLE Output (VB-Audio Voirc..." 这类全名；找第一台算第一台。
    let needle = hint.to_uppercase();
    for i in 0..count {
        let device = match collection.Item(i) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if device_friendly_name(&device).to_uppercase().contains(&needle) {
            return Some(device);
        }
    }
    None
}

/// Start audio capture using WASAPI loopback (Windows only)
// ── 初学者扫盲：WASAPI 环回采集 ────────────────────────────────────────
// WASAPI = Windows Audio Session API，Vista 之后 Windows 音频的底层系统接口。
// loopback（环回）：普通录音录的是"麦克风→设备输入侧"；挂上 LOOPBACK 标志后
// 录的是"这台播放设备正在播放什么"——即系统混音器的最终输出（浏览器声音、
// 游戏声音、会议声音全部在内）。这就是"把 PC 声音实时推给手机"的核心机关。
// 关键 COM 对象分工：
//   IMMDeviceEnumerator —— 枚举/取系统音频设备的工厂
//   IMMDevice           —— 一台设备的句柄式对象（可 Activate 出接口）
//   IAudioClient        —— 设备上的音频流控制器（Initialize/Start/GetService）
//   IAudioCaptureClient —— 流建成后的数据泵（GetBuffer/ReleaseBuffer 成对用）
// GetMixFormat 返回的 WAVEFORMATEX（即任务里常说的"AUDCLNT 格式"结构体：
// 采样率/声道数/位深）★必须按设备真实值走：写死 48k/2ch 遇到 44.1k 或 5.1
// 设备就会变速变调、声道错位。位深几乎总是 32（float32 混音），要转成手机
// 认识的 int16：float 以 -1.0~+1.0 表示满量程，×32767 并 clamp 后即 s16。
// 缓冲区大小与延迟：请求粒度越小首包越快但 GetBuffer 调用越频繁越费 CPU；
// 本实现请求 10ms 级粒度，实际由系统按设备周期成批交付（约 1024~10240 帧）。
#[cfg(windows)]
fn start_audio_capture(
    _config: ServerConfig,
    client_manager: Arc<ClientManager>,
    event_tx: std::sync::mpsc::Sender<ServerEvent>,
) -> Result<()> {
    // 前缀下划线参数：这个平台暂不读 ServerConfig（格式跟随设备），保留签名
    // 与 macOS 版一致，便于 run_server 无 cfg 分支地调用。
    // CoTaskMemFree：释放"系统任务分配器"分配的内存专用函数（见文末用法注释）。
    use windows::Win32::System::Com::CoTaskMemFree;

    // Initialize COM for this thread
    // 每个要调 COM 的线程必须先初始化自己的"套间"（apartment，线程模型域）。
    // APARTMENTTHREADED = 本线程独占对象消息泵，符合"单线程用一套接口"惯例。
    // 返回 HRESULT（C 的 32 位状态码），is_err() 判失败；S_FALSE 表示"已初始化
    // 过"也算 OK。失败直接 bail —— anyhow::bail! = return Err(这条消息) 的速记。
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        anyhow::bail!("CoInitializeEx failed: {:?}", hr);
    }

    // Create device enumerator
    // CoCreateInstance：按 CLSID（实现类 GUID）构造 COM 单例对象；
    // ? 运算符：失败就携带 HRESULT 立刻向上传播给本函数的 Result<()> 返回值。
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?
    };

    // Get default render (output) device — this is what we loopback from
    // GetDefaultAudioEndpoint(dataflow: EDataFlow, role: ERole)
    // eRender=播放方向、eConsole=多媒体角色 —— 两个参数合起来就是
    // "用户右下角音量图标对应的那台放声音设备"。
    //
    // v3.8：config.json 的 speaker.capture_device_hint 可以指定"从哪台播放设备取回路"。
    //   · 留空（默认）→ 和以前完全一样，用系统默认播放设备；
    //   · 填了名字（如 "Realtek" / "CABLE"）→ 在可用播放设备里模糊找第一台；
    //   · 写了但没找到 → 记一条 warn 后仍退回默认播放设备（不会因为笔误起不来）。
    let hint = crate::config::get().speaker.capture_device_hint;
    let hint = hint.trim();
    // 日志里要说清楚这台设备到底是怎么来的，否则排障时会误以为 config 生效了
    let mut device_source = if hint.is_empty() { "系统默认播放设备" } else { "config 指定" };
    let device: IMMDevice = if hint.is_empty() {
        unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? }
    } else {
        match unsafe { find_render_device_by_name(&enumerator, hint) } {
            Some(d) => d,
            None => {
                warn!(
                    "[Speaker] capture_device_hint '{hint}' 没匹配到播放设备 → 改用系统默认播放设备"
                );
                device_source = "系统默认播放设备（hint 未命中）";
                unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? }
            }
        }
    };

    let device_name = device_friendly_name(&device);
    info!("Using audio device: {} ({})", device_name, device_source);
    event_tx.send(ServerEvent::Log(format!("Audio device: {}", device_name))).ok();

    // Activate IAudioClient on the device
    // Activate<T>(clsctx: CLSCTX, params: Option<*const PROPVARIANT>) -> Result<T>
    // 从"设备"激活出"音频流控制器"。? 把失败一路带给 run_server 里
    // 调用方（外层只记 error 日志）—— 典型 Rust：错误自己爬，人只在顶端处理。
    let audio_client: IAudioClient = unsafe {
        device.Activate(CLSCTX_ALL, None)?
    };

    // Get the mix format (what Windows mixes everything into)
    // 共享模式下系统把所有程序混音后统一用的格式（几乎总是 float32）。
    // 返回的是系统堆上分配的裸指针：谁分配谁收钱 —— 函数末尾必须
    // CoTaskMemFree 归还；用 Rust 的 drop/Box 接管会直接堆破坏。
    let mix_format_ptr = unsafe { audio_client.GetMixFormat()? };
    if mix_format_ptr.is_null() {
        // COM 有时用 null 而非 HRESULT 表示失败，必须防这一手
        anyhow::bail!("GetMixFormat returned null");
    }

    // Read the WAVEFORMATEX to get sample rate / channels / bits per sample
    // &*ptr：把裸指针借成只读引用再访问字段 —— unsafe 块只包住解引用本身，
    // 尽量缩小"编译器放弃监督"的范围（Rust 最佳实践）。
    let mix_format = unsafe { &*mix_format_ptr };
    let sample_rate = mix_format.nSamplesPerSec;
    let channels = mix_format.nChannels as u16;
    let bits_per_sample = mix_format.wBitsPerSample;

    info!(
        "WASAPI mix format: {}Hz, {}ch, {}-bit",
        sample_rate, channels, bits_per_sample
    );

    // Store actual capture format so handle_connection can send correct header
    // 用 {} 单独作用域包锁：花括号一关 Guard 就 drop、锁自动释放，
    // 绝不可能"带着锁跑出这段代码"。
    // blocking_lock = std 风格的阻塞等锁（本线程是普通 OS 线程、非 tokio
    // 任务，可以安心睡等）；对比异步任务里用的 .lock().await。
    {
        let mut fmt = client_manager.capture_format.blocking_lock();
        *fmt = (sample_rate, channels);
    }

    // Calculate buffer duration: 10ms worth of audio (in 100-nanosecond units)
    // WASAPI 的时间单位是 100 纳秒（REFERENCE_TIME 惯例）：1ms = 10,000 个单位，
    // 真正的 10ms 应等于 10 × 10,000 = 100,000（与采样率无关）。
    // ⚠ 注意：这里写的却是 采样率 × 10_000 —— 48000×10000 = 480,000,000 个
    // 100ns ≈ 48 秒，行尾"10ms"的说明和算式对不上。眼下无害的原因：
    // 共享模式下 Windows 直接忽略这个参数（缓冲由系统混音器决定）。
    // 风险：若日后切独占模式或依赖此值估算延迟，会拿到错 4 个数量级的数字。
    let buffer_duration = (sample_rate as i64) * 10_000i64; // 10ms = 10,000 * 100ns

    // Initialize with LOOPBACK flag — this is the key difference from cpal
    // 参数五连：共享模式 + ★环回标志（录"正在播的声音"）+ 缓冲时长（见上）
    // + 0（保留）+ 格式指针（就是刚读的 mix format，NULL 会报错）+ None（不用事件）。
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
    // GetService::<T>()：向主接口"问出"子服务——这里要的是数据泵。
    // 类型参数即想要的接口，windows crate 帮你做 QI（QueryInterface）。
    let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService()? };

    // Start the audio stream
    // Start 之后环回流就开始积累数据；不 Start 则 GetBuffer 永远给 0 帧。
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
    // clone 的只是"发送端票根"：真实通道本体活在 ClientManager/转发任务里。
    let bridge_tx = client_manager.audio_bridge_tx.clone();

    // COM interfaces are not Send by default, but they are thread-safe
    // when used from a single thread. Wrap to allow moving into the thread.
    // Send = "此类型可安全搬去别的线程"的标记 trait。COM 接口因套间规则默认
    // 不 Send；但本对象从创建到使用全程只在一条线程活动（创建线程把它 move
    // 进新线程后自己不再碰），符合"单线程独占即安全"的例外，所以用新类型
    // 包装 + unsafe impl Send 手写"我担保"。Deref 让包装能像内部接口一样
    // 直接 .GetBuffer() 调方法，省一层 .0。
    struct SendCaptureClient(IAudioCaptureClient);
    unsafe impl Send for SendCaptureClient {}
    impl std::ops::Deref for SendCaptureClient {
        type Target = IAudioCaptureClient;
        fn deref(&self) -> &Self::Target { &self.0 }
    }

    let send_client = SendCaptureClient(capture_client);

    // Spawn a thread to read from WASAPI and push into the bridge channel
    // 这是本文件真正的"数据泵"线程：唯一工作就是无限 GetBuffer。
    // 用 std::thread 而非 tokio 任务：内部全是同步 COM 调用、一次 await 没有，
    // 放进 tokio 会永久霸占一条工作线程（调度器会直接饿死别的任务）。
    let mut wasapi_packet_count: u64 = 0;
    std::thread::spawn(move || {
        loop {
            // GetBuffer 的三个输出参数由系统回填：
            //   buffer_ptr  本批数据的起始裸指针（指向 WASAPI 环形缓冲，禁写）
            //   num_frames  本次交付的帧数（0 = 缓冲区现在是空的）
            //   flags       状态位（本代码未使用）
            // 最后两个 None：不关心时间戳（实时流按到达顺序转发即可）、
            // 不关心下一个包序号 —— 可选输出参数直接不要。
            // mut + null_mut：先把"我要接三个值"的内存位置交给系统填。
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
                // HRESULT 用 {:?} 原样打出（如 AUDCLNT_E_DEVICE_INVALIDATED =
                // 设备被拔出/禁用）；break 结束本线程 = 下行静默，
                // 但服务其他部分照常 —— 音频采集挂了不等于全服该崩。
                error!("[WASAPI] GetBuffer failed after {} packets: HRESULT = {:?}. Audio capture thread exiting.",
                    wasapi_packet_count, get_result.err());
                break;
            }

            // 指针非空且有帧才处理：帧数 × 声道数 = 样本总数
            //（立体声样本交错存放：L R L R…，所以按"样本"而不是"帧"转字节）
            if !buffer_ptr.is_null() && num_frames > 0 {
                let channel_count = channels as u32;
                let sample_count = num_frames * channel_count;

                // 按位深分支：环回数据必须统一成 PCM s16le（小端 16 位整数），
                // 这是 audio_config 头里向手机承诺的唯一格式，App 端只认它。
                let byte_vec = match bits_per_sample {
                    // 16 位：设备直接交 s16，零转换；字节数 = 样本数 × 2，
                    // from_raw_parts 把 (指针, 长度) 拼成安全的 &[u8] 再 to_vec 拷贝
                    16 => {
                        unsafe {
                            std::slice::from_raw_parts(
                                buffer_ptr as *const u8,
                                sample_count as usize * 2,
                            )
                            .to_vec()
                        }
                    }
                    // 32 位：混音格式几乎总是 float32 —— 浮点样本以 -1.0~+1.0
                    // 表示满量程。为什么要转：手机解码端固定 s16，不能直接收浮点。
                    // clamp(-1.0,1.0)：截断超范围脏值防爆音；×32767（i16 最大正值）
                    // 把 [-1,1] 映射到 [-32767,32767]；as i16 截断取整。
                    // 小端：x86/ARM 手机都是小端，Rust 原生 to_vec 展平即 s16le。
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
                    // 其他位深（24bit 等罕见格式）：归还缓冲、跳过本轮，
                    // 本包静默丢弃，continue 直接进下一次 GetBuffer。
                    // ⚠ 注意：这里的行为是"永远不出声"而不是报错，风险=遇到
                    // 非 16/32 位深设备时下行完全无声且日志无任何提示。
                    _ => {
                        unsafe { send_client.ReleaseBuffer(num_frames).ok() };
                        continue;
                    }
                };

                // 使用 tokio mpsc 的 send()（同步非阻塞），替代 crossbeam 的 try_send()
                // 同步非阻塞入队：只把包塞进无界队列就返回，接收方（转发任务）
                // 是快是慢都不拖累采集节拍。Err 的唯一可能：接收端已被 drop
                // （转发任务退出）→ 采集也没意义了，记日志 break 收工。
                if let Err(e) = bridge_tx.send(byte_vec.clone()) {
                    error!("[WASAPI] Failed to send packet to bridge: {} (forwarding task may have exited)", e);
                    break;
                }

                wasapi_packet_count += 1;
                // Log first few packets to verify data is not all zeros
                // 头 5 包统计"非零字节占比"：全零 = 设备静音或环回抓错设备的
                // 快速诊断信号（正常放音时大部分字节非零）。
                if wasapi_packet_count <= 5 {
                    let non_zero = byte_vec.iter().filter(|&&b| b != 0).count();
                    info!("[WASAPI] Packet #{}: {} bytes, {} non-zero bytes ({}%)",
                        wasapi_packet_count, byte_vec.len(), non_zero,
                        if byte_vec.is_empty() { 0 } else { non_zero * 100 / byte_vec.len() });
                }
            }

            // GetBuffer / ReleaseBuffer 必须成对：归还"我读完这批了"，
            // 不 Release 下一次 GetBuffer 不会前进；num_frames=0 的空转轮次
            // 同样要调 —— 这是 WASAPI 的规矩。
            unsafe { send_client.ReleaseBuffer(num_frames).ok() };

            // ── v3.7 CPU 修复：空缓冲区必须让出 CPU ─────────────────────────
            // 这个循环原本从头到尾【一次休眠都没有】，全指望 WASAPI"有数据才给帧"。
            // 问题是缓冲区空的时候 GetBuffer 照样成功返回、只是 num_frames = 0，
            // 于是 ReleaseBuffer(0) → 立刻再转一圈 → 纯自旋，实测吃掉整整一个核心
            // （线程级采样 99.6%）。
            // 为什么平时不容易注意到：loopback 端点只在【电脑真的在出声】时才有包。
            // 所以"手机断开 + 桌面安静"两件事同时成立时，程序就一直在满核空转 ——
            // 任务管理器里 audioserver 常年 14~16%、电源计划直接判"非常高"，根因在这。
            //
            // 只在"这一轮确实没取到帧"时歇 3ms：
            //   · 有声音时 num_frames > 0，一行都不睡，下行延迟完全不变；
            //   · 静音时轮询从"极限速度"降到 ~330 次/秒，空转的 CPU 归零。
            //     3ms 远小于一个音频包（1024 帧 @48kHz ≈ 21ms），不会漏包，
            //     也不会让"突然出声"到"第一包发出"变慢到能察觉。
            if num_frames == 0 {
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
        }
    });

    // Free the mix format allocated by WASAPI
    // 系统堆上的内存要还给系统分配器（CoTaskMemFree），用 Rust 的 drop 会堆破坏。
    // 时机安全：Initialize 早已把格式拷走，取数线程不再碰这个指针，故此处释放无碍。
    unsafe { CoTaskMemFree(Some(mix_format_ptr as *mut _)) };

    // Block this thread — the WASAPI capture thread and bridge forwarder handle everything
    // 本线程到此只剩一个使命：别退出。COM 套间随线程终止会销毁还在用的接口对象，
    // 所以让它每小时醒一次再睡回去（休眠中的线程不耗 CPU）。
    // ⚠ 注意：这里的行为是永久阻塞、没有任何退出/唤醒路径，风险=收到 Stop 或
    // GUI 关闭后本线程与内层取数线程仍驻留，直到整个进程退出才回收；
    // 对桌面工具无害，但进程若被设计成"可反复启停服务"就会积累死线程。
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
// ── 扫盲：cpal = Rust 跨平台音频 I/O 库（macOS 内部调 CoreAudio）。
// Windows 不用它：cpal 没有跨平台的 loopback 能力，我们直写 WASAPI 才有环回。
// build_input_stream 是回调模型：cpal 在自己的实时音频线程里按缓冲粒度调用
// 闭包，闭包必须快速返回 —— 所以只做格式转换 + 非阻塞 send，绝不等网络。
// 采样格式三分支：I16 零转换直发；F32/F64 先 clamp 限幅再 ×32767 转 s16
//（同 WASAPI 分支的道理：手机只认 PCM s16le，浮点超限不截断会爆音）。
// 错误处理风格：本函数所有失败都经 ? 汇成 Result 传回 run_server 里那条
// std::thread（那里只 error! 记日志）——"错误只记日志不崩"贯穿全项目。
#[cfg(not(windows))]
fn start_audio_capture(
    _config: ServerConfig,
    client_manager: Arc<ClientManager>,
    event_tx: std::sync::mpsc::Sender<ServerEvent>,
) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    // host = 平台音频宿主对象（Mac 上即 CoreAudio 的入口），一切设备从它枚举。
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
    // err_fn = 流运行期错误的回调（设备被抢占/拔出等）：cpal 不会 panic，
    // 把错误交给我们处置 —— 这里只记日志，符合"局部错误不拖垮全局"策略。
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

    // s16le 小端说明：i16 按"低字节在前"排布，与手机 App / Android 音频惯例一致。
    // 带宽速算（48kHz 立体声）：48000 × 2 声道 × 2 字节 = 192000 字节/秒。
    // stream.play()? 失败（设备被占用等）经 ? 传播 → 本函数返回 Err → 上层记日志。
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
// ── 扫盲：一条手机连接的一生 ──────────────────────────────────────────
// WebSocket：在一条 TCP 上双向随时发消息的协议（开场用 HTTP 握手"升级"）。
// 帧分两类：Text = JSON 文本控制指令（mic_start/mic_ack/cam_state…），
// Binary = 音频 PCM / JPEG 画面（体积大、无格式，靠约定解析）。
// 本函数为一条连接装配：发 audio_config 头 → 起下行任务（音频+心跳）→
// 起上行任务（解析指令/分流二进制）→ 两任务共享一个写端 → 结束统一清账。
async fn handle_connection(
    stream: TcpStream,
    client_key: String,
    client_manager: Arc<ClientManager>,
    event_tx: std::sync::mpsc::Sender<ServerEvent>,
) {
    // 取对端地址纯为日志。unwrap_or_else：Result 失败时走兜底闭包。
    // ⚠ 注意：兜底值本身又是一个会 panic 的 unwrap（"unknown" 不是合法
    // SocketAddr，parse 必败）。风险=peer_addr 真失败时本任务 panic；
    // tokio 会捕获任务级 panic（进程无恙、仅该连接终止），而已建立连接取
    // 地址几乎不可能失败，故维持现状不改。
    let addr = stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
    info!("New TCP connection from: {}", addr);

    // ── 性能优化（实时性）：关闭 Nagle 算法 ──────────────────────
    // Nagle 会把"小包等下一个小包攒一起发"，和接收端的延迟 ACK
    // 撞上时会给每一帧音频/视频凭空加上最多 ~40ms 的抖动。
    // 我们是实时流，宁可多发几个小包也不能等，所以每个连接一建立
    // 就设置 TCP_NODELAY（对上下行都生效，握手完成后 ws 沿用同一 socket）。
    // 数值账：音频包 ~21ms 一个、40ms 抖动 ≈ 直接翻倍体感延迟，必须掐掉。
    if let Err(e) = stream.set_nodelay(true) {
        info!("Failed to set TCP_NODELAY: {}", e);
    }

    // 服务端握手：校验对方的 HTTP Upgrade 请求，成功后这条 TCP 改说
    // WebSocket 方言。失败（端口被扫描器乱连、App 版本不对等）记日志
    // 直接 return 断开 —— 不进入后续任何任务。
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed: {}", e);
            return;
        }
    };

    // split()：一拆为二 —— ws_sink 只管写、ws_receiver 只管读，
    // 之后读写在两个任务里并发进行（WebSocket 允许全双工）。
    // 写端再包 Arc<Mutex<>>：下行任务和上行任务都要写（后者发 mic_ack），
    // 协议规定同一时刻只能有一个写者 → 排队拿锁者才准发一帧。
    let (ws_sink, mut ws_receiver) = ws_stream.split();
    let ws_sink: Arc<Mutex<WsSink>> = Arc::new(Mutex::new(ws_sink));
    // 在 ClientManager 登记音频通道，拿回本客户端专属接收端 audio_rx
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
    // 连接建立后服务器主动发的第一条 Text 帧（服务器→手机）：
    //   {"type":"audio_config","sample_rate":48000,"channels":2,"format":"pcm_s16le"}
    //   · type        —— 消息类型，手机端按它分发处理
    //   · sample_rate / channels —— 采集线程回填的【真实】设备格式，不是配置初值
    //   · format      —— "pcm_s16le" = 小端 16 位整数的裸 PCM（无头无压缩），
    //     后续所有 Binary 帧都按此格式播放
    // 先拿锁拷贝元组再立刻释放（{} 限定作用域），绝不带着锁去 await 网络发送。
    let (actual_sr, actual_ch) = {
        let fmt = client_manager.capture_format.lock().await;
        *fmt
    };
    let header = format!(
        r#"{{"type":"audio_config","sample_rate":{},"channels":{},"format":"pcm_s16le"}}"#,
        actual_sr, actual_ch
    );
    {
        // 头都没发出去 = 连接已死/手机跑了：注销登记后 return，全剧终。
        // remove_client 会把两张表的表项清掉，不留僵尸。
        let mut sink = ws_sink.lock().await;
        if let Err(e) = sink.send(Message::Text(header)).await {
            error!("Failed to send header: {}", e);
            client_manager.remove_client(&client_key).await;
            return;
        }
    }

    // ── v3.8：补发 cam_request，收拾"手机晚于 PC 应用连上来"的时序漏洞 ──
    // 原来的 cam_request 只在"有应用开始观看虚拟摄像头"的【翻转瞬间】广播一次
    // （见 run_server 里 cam_live 的 swap 分支）。若那一刻还没有任何手机在线，
    // 这条广播就打给了空气；之后观看状态一直保持 true 不再翻转，
    // 于是永远不会再补发一次 —— 手机从头到尾收不到 cam_request，
    // 界面只能一直停在"待电脑请求"。典型触发顺序：先开 PC 上的取景程序，
    // 再启动手机 App（或中途重连）。
    // 修法：每条新连接在握手成功后自查一次 —— 此刻有人在看 + 手机会话还没登记
    // → 给【这一条】连接单独补发一次 cam_request。补的是"错过那一枪"，
    //   顺序谁先谁后都能自动接管，不需要用户去 GUI 上点按钮。
    let watching_no_session = client_manager.cam_live.load(Ordering::Relaxed)
        && !client_manager.cam_session.load(Ordering::Relaxed);
    if watching_no_session {
        {
            let mut sink = ws_sink.lock().await;
            let _ = sink
                .send(Message::Text(r#"{"type":"cam_request"}"#.to_string()))
                .await;
        }
        info!(
            "[Cam] New client {} arrived while an app is watching → resent cam_request to it",
            client_key
        );
        event_tx
            .send(ServerEvent::Log(format!(
                "[Cam] App is watching the virtual camera but no phone session → \
                 resent cam_request to {}",
                client_key
            )))
            .ok();
    }

    // v3.8：心跳 / 统计间隔从 config.json 取。每个连接建立时读一次内存里那份配置，
    // 绝不在循环里读 —— 配置改了下一次连接才生效，这是有意的（连接中途改节奏没意义）。
    // ping_ms（默认 5000ms）：发 WebSocket Ping 帧的周期，兼当断线探测器；
    // stat_ms（默认 1000ms）：向 GUI 推 Mic/Cam 统计的窗口长度。
    let run_cfg = crate::config::get();
    let ping_ms = run_cfg.network.ping_interval_ms;
    let stat_ms = run_cfg.diagnostics.stat_interval_ms;

    // Downlink send task: loopback audio → WebSocket + periodic ping for keepalive
    // 客户端被移出广播表时 audio_rx 自然关闭，任务退出（Mic 模式即走此路径停止下行）
    // 克隆三个"小抄"进任务：写端指针、客户端名（日志用）、通道所有权。
    let sink_send = ws_sink.clone();
    let key_send = client_key.clone();
    let send_task = tokio::spawn(async move {
        // v3.4.5：WiFi 稳定性 —— 定期发 Ping，检测连接是否存活（默认 5 秒，
        // 可在 config.json 的 network.ping_interval_ms 里改）
        // interval：每隔 ping_ms 准点触发一次；手机侧协议栈自动回 Pong，
        // 上行任务收到 Pong 即知链路活着。改大了断线发现慢，改小了空转费流量。
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_millis(ping_ms));
        ping_interval.tick().await; // 跳过第一次立即触发的 tick

        loop {
            // select! 同时等两件事：下行音频到达 / 心跳到点。谁先来跑谁。
            tokio::select! {
                // 音频数据下行
                data = audio_rx.recv() => {
                    match data {
                        Some(data) => {
                            // Binary 帧整包发出：一个包 ≈ 1024 帧 @48kHz ≈ 21ms
                            // 声音 ≈ 4KB（48000Hz×2声道×2字节=192000字节/秒，
                            // 每 21ms 约 4096 字节）。send 失败=对端已断，break。
                            let mut sink = sink_send.lock().await;
                            if sink.send(Message::Binary(data)).await.is_err() {
                                break;
                            }
                        }
                        None => break, // channel closed
                    }
                }
                // 定时 Ping 心跳（WiFi 连接健康检测）
                _ = ping_interval.tick() => {
                    // Ping 都发不出去 = TCP 已死；等 Pong 超时由对端协议栈兜底。
                    let mut sink = sink_send.lock().await;
                    if sink.send(Message::Ping(vec![])).await.is_err() {
                        info!("[{}] Ping failed - connection dead", key_send);
                        break;
                    }
                }
            }
        }
        // 两条退路都到这里：audio_rx 关闭（被移出广播表 = Mic 模式停下行）
        // 或发送失败（断线）。正常暂停下行时本任务静默退场，不算错误。
        info!("[{}] Downlink task exited", key_send);
    });

    // Receive task: client messages + v3 mic uplink
    // 克隆/引用共享状态进任务：写端（回 mic_ack 用）、管理器、事件通道、
    // 各 AtomicBool 的 Arc 指针 —— 全部指向 ClientManager 里的同一份数据。
    let sink_recv = ws_sink.clone();
    let mgr_recv = client_manager.clone();
    let key_recv = client_key.clone();
    let event_tx_recv = event_tx.clone();
    let mic_live_recv = client_manager.mic_live.clone();
    let mic_muted_recv = client_manager.mic_muted.clone();
    // v3.4：摄像头会话/占用标志（跨任务共享）
    let cam_session_recv = client_manager.cam_session.clone();
    let cam_session_owner_recv = client_manager.cam_session_owner.clone();
    let recv_task = tokio::spawn(async move {
        // Mic 会话状态（未收到 mic_start 前行为与 v2 完全相同）
        // 全双工：mic 期间下行转发照常，不摘除订阅
        // v3.3：手机连接即发 mic_start 进入【待命】（麦克风硬件关闭）；
        // 本服务器检测到 PC 应用开始录 CABLE Output 时推 mic_state true，
        // 手机才开硬件上行；PC 应用停止 → 推 false → 手机立刻停录。
        // 下面这组变量都是"本连接私有"的统计量，任务结束即销毁，无需加锁。
        let mut mic_session = false;
        // 以下统计量：包数 / 窗口字节数(算 kbps) / 会话累计字节(÷1024=total_kb) /
        // 上一包时刻(算间隔) / 间隔累计 ms / 参与均值的个数 / 统计窗口起点
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
        // ⚠ 注意（改动前就存在的编译警告）："value assigned to cam_preview_hold
        // is never read"——编译器指出【这行赋的初值 None】永远读不到：每帧到达
        // 都会在本分支重新赋值、并在统计窗口到期时被 take 消费。行为无害，
        // 属死写；按铁律保持原样不修。
        let mut cam_preview_hold: Option<Vec<u8>> = None;

        // ws_receiver.next().await = 从这条连接读下一帧；None/Err = 断开。
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                // ══ Text 帧：JSON 控制指令 ══
                // 方向约定：手机→服务器 = mic_start / mic_stop / mic_mute /
                //   cam_capabilities / cam_start / cam_stop；
                // 服务器→手机（见 broadcast_* 与回执）= audio_config / mic_ack /
                //   mic_state / cam_ack / cam_state / cam_request / cam_stop(广播)。
                Ok(Message::Text(text)) => {
                    info!("Client message: {}", text);
                    // 去掉空白后做轻量匹配，无需 serde
                    // compact 手法：JSON 里所有空格/换行删掉后，用字符串
                    // contains 判断"是否含某类型"——省一个 serde 依赖，协议字段
                    // 少且可控时够用；风险=字段里若出现转义引号会误配（现协议无）。
                    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                    if compact.contains("\"type\":\"mic_start\"") && !mic_session {
                        // ── mic_start（手机→服务器）：登记麦克风待命会话 ──
                        //   sample_rate：手机将上行的 PCM 采样率（缺省按 48000）
                        //   channels：上行声道数（缺省 1=单声道）
                        // json_number 取字段失败给合理默认（unwrap_or），
                        // 坏报文不至于让会话开不起来。
                        let sr = json_number(&compact, "sample_rate").unwrap_or(48000) as u32;
                        let ch = json_number(&compact, "channels").unwrap_or(1) as u16;
                        // v3.4.4：把上行采样率告知注入引擎（44.1k≠设备48k时启用重采样）
                        // 重采样比例 = 设备率 ÷ 上行率（48000/44100 ≈ 1.088 倍）；
                        // 不重采样直接把 44.1k 当 48k 播 = 音调升 ~4%，声音发尖。
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
                        // ── mic_ack（服务器→手机）：{"type":"mic_ack","active":bool}
                        //   active = 此刻 mic_live（有没有应用在录 CABLE Output），
                        //   true 则手机不用等 mic_state，马上开硬件开录。
                        let live_now = mic_live_recv.load(Ordering::Relaxed);
                        let mut sink = sink_recv.lock().await;
                        let _ = sink
                            .send(Message::Text(format!(
                                r#"{{"type":"mic_ack","active":{}}}"#,
                                live_now
                            )))
                            .await;
                        // drop(sink)：立刻手动还锁，下面还要发 GUI 事件，
                        // 别让 WebSocket 写端在长串 .send(...).ok() 期间被白占着
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
                        // ── mic_stop（手机→服务器）：结束麦克风会话 ──
                        // 手机端主动结束会话（断开/手动关闭）
                        mic_session = false;
                        if mic_live_recv.load(Ordering::Relaxed) {
                            // 只有"正在上行中"才发 MicStopped，GUI 电平表复位；
                            // 待命期结束不惊动界面（本来也没在动）
                            event_tx_recv.send(ServerEvent::MicStopped).ok();
                        }
                        event_tx_recv
                            .send(ServerEvent::Log(format!("[Mic] Uplink stopped by {}", key_recv)))
                            .ok();
                    } else if compact.contains("\"type\":\"mic_mute\"") {
                        // ── mic_mute（手机→服务器）：{"type":"mic_mute","muted":bool}
                        //   muted=true 手机按下静音键 / false 取消静音
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
                        // ── cam_capabilities（手机→服务器）：整颗相机能力清单 ──
                        // JSON 原文含每颗镜头支持的分辨率/帧率组合列表。
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
                        // ── cam_start（手机→服务器）：登记摄像头会话 ──
                        // 无参数（只有 type 字段）。登记后服务器开始接受该连接的
                        // CAM 魔术头二进制帧；此刻手机相机硬件仍是关的。
                        // 手机登记摄像头会话（待命，等 cam_state 唤醒）
                        cam_session_recv.store(true, Ordering::Relaxed);
                        // v3.4.5：记录本连接为会话所有者（防止重连时旧连接清理误清新连接状态）
                        *cam_session_owner_recv.lock().await = Some(key_recv.clone());
                        // v3.4.9：这里【不再】强制置位 cam_live。
                        // 两路引擎现在都有真实检测（Unity=Want 事件、OBS=共享内存句柄数），
                        // 由它们上报"有应用在观看"才唤醒手机开相机。
                        // cam_start 只登记会话，硬件保持关闭 —— 隐私语义：待命 = 摄像头 OFF。
                        // cam_ack 的 active 字段会被手机端当作 cam_state 直接消费
                        // （network_service.dart 里 _camStateController.add(active)），
                        // 所以必须上报【真实的观看状态】，不能写死 true：
                        // 写死会让手机一登记就打开相机硬件（= 一连上就"正在使用摄像头"）。
                        // 真实语义：登记这一刻若已有应用在看，就立刻进取景；否则保持待命。
                        let watching_now = mgr_recv.cam_live.load(Ordering::Relaxed);
                        // ── cam_ack（服务器→手机）：{"type":"cam_ack","active":bool}
                        //   active = 当前"有应用在观看虚拟摄像头"的真实值；
                        //   手机端把它当作 cam_state 直接消费，决定相机开或关。
                        let mut sink = sink_recv.lock().await;
                        let _ = sink
                            .send(Message::Text(format!(
                                r#"{{"type":"cam_ack","active":{watching_now}}}"#
                            )))
                            .await;
                        drop(sink);
                        event_tx_recv
                            .send(ServerEvent::Log(format!(
                                "[Cam] Session from {} → standby, waiting for an app to open the virtual camera",
                                key_recv,
                            )))
                            .ok();
                        info!("[Cam] Session from {key_recv} → standby (camera hardware OFF), waiting for an app to open the virtual camera");
                    } else if compact.contains("\"type\":\"cam_stop\"") {
                        // ── cam_stop 到达服务器的两个来源 ──
                        //   ① 手机主动发 Text {"type":"cam_stop"}（用户关掉页面）
                        //   ② GUI 按"强制关闭"→ 服务器广播 {"type":"cam_stop",
                        //      "forced":true} → 手机确认后自己再回一条 cam_stop
                        //      （forced 标志由手机侧消费，服务器这里不区分来源）
                        // 手机端主动结束摄像头会话（或手机端确认关闭）
                        cam_session_recv.store(false, Ordering::Relaxed);
                        // v3.4.7：重置 OBS 活跃标记（与 cam_start 时的置位对称）
                        mgr_recv.cam_live_obs.store(false, Ordering::Relaxed);
                        // v3.4.8：引擎不再上报活跃，cam_stop 直接关闭 cam_live 并广播
                        mgr_recv.cam_live.store(false, Ordering::Relaxed);
                        mgr_recv.cam_live_unity.store(false, Ordering::Relaxed);
                        // v3.4.5：清除会话所有者
                        *cam_session_owner_recv.lock().await = None;
                        if let Ok(mut mb) = mgr_recv.cam_mailbox.lock() {
                            *mb = None;
                        }
                        if let Ok(mut mb) = mgr_recv.cam_mailbox_obs.lock() {
                            *mb = None;
                        }
                        event_tx_recv.send(ServerEvent::CamStopped).ok();
                        event_tx_recv
                            .send(ServerEvent::CamState { active: false })
                            .ok();
                        mgr_recv.broadcast_cam_state(false).await;
                        event_tx_recv
                            .send(ServerEvent::Log(format!("[Cam] Stopped by {}", key_recv)))
                            .ok();
                    }
                }
                // ══ Binary 帧：上行媒体数据（麦克风 PCM / 摄像头 JPEG）══
                Ok(Message::Binary(data)) => {
                    // v3.4：摄像头 JPEG 帧带 4 字节魔术头 [0x03,'C','A','M']，
                    // PCM 音频包撞头的概率约 2^-32，可安全分流两种二进制帧
                    //（同一条连接既传麦克风又传画面，全靠这个头区分）。
                    // 布局：data[0]=0x03、data[1..4]="CAM"、data[4..]=完整 JPEG。
                    if data.len() > 4 && data[0] == 0x03 && &data[1..4] == b"CAM" {
                        // 有 CAM 会话才消费；没登记 = 帧直接丢（隐私默认关闭）
                        if cam_session_recv.load(Ordering::Relaxed) {
                            let jpeg = data[4..].to_vec();
                            // v3.4 双通道：同一帧分别投给两个引擎的邮箱
                            //（clone 一份给 OBS 路，Unity 路用原件）
                            vcam::push_frame(&mgr_recv.cam_mailbox, jpeg.clone());
                            vcam_obs::push_frame(&mgr_recv.cam_mailbox_obs, jpeg);
                            // 给 GUI 预览攒下窗口内最新一帧（到统计点 take 走）
                            cam_preview_hold = Some(data[4..].to_vec());
                            cam_packets += 1;
                            cam_bytes_window += data.len() as u64;
                            // 从 JPEG 头里读出这一帧的分辨率（用于 GUI 显示"当前画质"）
                            if let Some((w, h)) = jpeg_dims(&data[4..]) {
                                cam_frame_dims = (w, h);
                            }
                            // 按 diagnostics.stat_interval_ms（默认 1000ms）推一次摄像头链路统计
                            let elapsed = cam_window_start.elapsed();
                            if elapsed.as_millis() >= stat_ms as u128 {
                                // 单位算式（与麦克风侧同构）：
                                //   kbps = 字节 × 8(位/字节) ÷ 1000(比特→千比特) ÷ 窗口秒数
                                //   fps  = 包数 ÷ 窗口秒数（一帧=一包）
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
                    // 无魔术头的 Binary = 上行 PCM s16le 裸音频（手机 10ms/包）。
                    else if mic_session && !mic_muted_recv.load(Ordering::Relaxed) {
                        // rms_level：算本包音量给 GUI 电平表（见函数尾注释）
                        let level = rms_level(&data);
                        // push_uplink：字节解回 i16 样本压进 mic_queue，
                        // 由 mic_out 注入线程按节拍取走写 CABLE Input（VB-CABLE）
                        mic_out::push_uplink(&mgr_recv.mic_queue, &data);
                        mic_packets += 1;
                        mic_bytes_window += data.len() as u64;
                        mic_bytes_total += data.len() as u64;
                        // 记录"距上一包过了多久"：interval_ms 均值 ≈ 手机发包
                        // 节奏（正常 10ms；显著大于 10 = WiFi 卡顿掉包）
                        if let Some(t) = last_pkt {
                            interval_sum += t.elapsed().as_millis() as u64;
                            interval_n += 1;
                        }
                        last_pkt = Some(Instant::now());
                        // 每 5 包推一次电平（手机侧 10ms/包 ≈ 20Hz 刷新）
                        // 20Hz 的由来：1000ms ÷ 10ms/包 ÷ 5包/次 = 20 次/秒。
                        if mic_packets % 5 == 0 {
                            event_tx_recv.send(ServerEvent::MicLevel(level)).ok();
                        }
                        // 按 stat_interval_ms（默认 1000ms）推一次链路统计
                        let elapsed = window_start.elapsed();
                        if elapsed.as_millis() >= stat_ms as u128 {
                            // kbps = 字节×8÷1000÷秒；带宽速算：单声道 48000Hz
                            // ×2字节 = 96000 字节/秒 ≈ 768 kbps（手机 16k 上行则 ~128kbps）
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
                // Close 帧 = 对端礼貌告别（协议级断开），直接跳出读循环。
                Ok(Message::Close(_)) => {
                    info!("Client requested close");
                    break;
                }
                Ok(Message::Pong(_)) => {
                    // v3.4.5：收到 Pong —— WiFi 连接存活证明（日志级别，避免刷屏）
                    // 心跳闭环：下行任务发 Ping → 手机协议栈自动回 Pong → 到这里。
                    // 长时间没 Pong/Ping 收发失败 = 断链，发送任务会先察觉并 break。
                    info!("[{}] Pong received - connection alive", key_recv);
                }
                Ok(Message::Ping(_)) => {
                    // tungstenite 自动回复 Pong，这里只记日志
                    //（手机也可以反向发心跳，服务器这边由库代答）
                    info!("[{}] Ping received", key_recv);
                }
                Err(e) => {
                    // 读失败（网线拔了/超时/坏帧）：warn 后 break，走善后清理。
                    // 单连接错误只杀这条连接，服务器主循环照常等其他客户端。
                    warn!("Receive error: {}", e);
                    break;
                }
                // 其他帧类型（如 Frame 级元数据）静默忽略 —— match 穷举的兜底臂
                _ => {}
            }
        }

        // ══ 读循环退出 = 这条连接完了；下面做善后 ══
        if mic_session {
            // 会话结束（无论待命还是上行中），GUI 复位
            event_tx_recv.send(ServerEvent::MicStopped).ok();
            event_tx_recv
                .send(ServerEvent::Log(format!("[Mic] Uplink ended: {}", key_recv)))
                .ok();
        }
        // v3.4：摄像头会话同样随连接结束而终止，清空邮箱并复位 GUI
        // v3.4.5：只有当本连接仍是会话所有者时才清理（防止重连时旧连接清理误清新连接状态）
        // 所有权比对：Option<String> == Some(本连接key) 才动手 —— 教科书级的
        // "谁登记、谁销账"，避免两条同名连接新旧互踩。
        let is_owner = {
            let owner = cam_session_owner_recv.lock().await;
            owner.as_ref() == Some(&key_recv)
        };
        // swap(false) 返回旧值：旧值是 true 且我是主人 → 真有一个会话要我来收尾
        if is_owner && cam_session_recv.swap(false, Ordering::Relaxed) {
            *cam_session_owner_recv.lock().await = None;
            // v3.4.8：引擎不再上报，断线时直接重置活跃状态
            mgr_recv.cam_live.store(false, Ordering::Relaxed);
            mgr_recv.cam_live_obs.store(false, Ordering::Relaxed);
            mgr_recv.cam_live_unity.store(false, Ordering::Relaxed);
            if let Ok(mut mb) = mgr_recv.cam_mailbox.lock() {
                *mb = None;
            }
            if let Ok(mut mb) = mgr_recv.cam_mailbox_obs.lock() {
                *mb = None;
            }
            event_tx_recv.send(ServerEvent::CamStopped).ok();
            event_tx_recv
                .send(ServerEvent::CamState { active: false })
                .ok();
            mgr_recv.broadcast_cam_state(false).await;
            event_tx_recv
                .send(ServerEvent::Log(format!("[Cam] Uplink ended: {}", key_recv)))
                .ok();
        }
    });

    // 注意：不能用 select!（send_task 因 mic 摘除订阅而正常退出时会误关整条连接），
    // 必须 join 等两个任务都结束
    // tokio::join! = 并发等待多个 Future 全部完成（不是 select! 的"取第一个"）。
    // 返回 (结果1, 结果2)；这里不关心 JoinHandle 的 Result，`let _ =` 明确丢弃。
    // JoinHandle 本身被 drop：任务结束自然回收，不 join 也不会泄漏内存。
    let _ = tokio::join!(send_task, recv_task);

    // 两张表最后注销（remove_client 会关掉音频通道的发送端）
    // → 转发任务对它的 send 开始报 Err → 兜底清理路径也自动闭合。
    client_manager.remove_client(&client_key).await;
    event_tx.send(ServerEvent::ClientDisconnected(client_key)).ok();
}

// ── v3 麦克风上行支撑 ─────────────────────────────────────────────
// 下面三个纯函数都是"输入字节/文本 → 输出值"的无状态工具：
// 不碰锁、不发事件、不 await —— 因此可以安全地在任何任务里随时调用。

/// 从 JPEG 段结构中读出图像宽高（SOF 标记 0xC0..0xCF，跳过 DHT/C8/DAC）。
/// 只扫段头不解析熵数据，开销极小，用于 GUI 显示"当前画质"。
// JPEG 文件是一串"段"：都以 0xFF+段号 开头，带长度域的段可整段跳过。
// SOF（Start Of Frame，0xC0..0xCF 去掉 0xC4/0xC8/0xCC 三个非帧段）里
// 固定存放 高16位、宽16位（大端）：offset+5/+6=高，+7/+8=宽。
// from_be_bytes：JPEG 规定大端（网络序），显式转换与本机字节序无关。
fn jpeg_dims(data: &[u8]) -> Option<(u16, u16)> {
    // 前两个字节必须是 JPEG 魔数 FFD8，否则不是 JPEG 直接放弃
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            // 不在段标记上：逐字节向前扫（熵数据里也可能混现 FF，容错跳过）
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            // matches! 排除的三个段号：DHT(0xC4)/JPEG扩展(0xC8)/DAC(0xCC) 不是 SOF
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]);
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]);
            // 宽高任一为 0 = 畸形帧，宁可不知道也不给 GUI 显示 0x0
            return if w > 0 && h > 0 { Some((w, h)) } else { None };
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            // 这些标记本身不带长度域（SOI/TEM/RSTn），前进 2 字节继续
            i += 2;
            continue;
        }
        // 普通段：读 2 字节大端长度，整段跳过去（.max(2) 防长度 0/1 死循环）
        let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        i += 2 + seg_len.max(2);
    }
    // 扫到文件尾也没找到 SOF：坏帧或截断，返回 None 让调用方保持旧尺寸
    None
}

/// 计算 PCM s16le 数据的 RMS 电平，归一化到 0.0 ~ 1.0
// RMS=均方根：先平方平均再开方，是音量的标准物理量算法。
// 算式：level = √(Σs²/n) ÷ 32768（满量程 i16 峰值 32768 → 结果落在 0..1）。
// chunks_exact(2)：按 2 字节一组取整（丢弃末尾半样本），from_le_bytes 还原
// 小端 i16。全零输入返回 0.0（静音），不会除以零。
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
// 实现：找 "key": 出现的位置 → 从冒号后连续取 ASCII 数字 → parse。
// 局限（可接受）：只认第一个匹配、只支持非负整数；若 mic_start 将来加
// 嵌套同名键会取错值 —— 协议改动时记得复核这里。
fn json_number(compact: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{}\":", key);
    // ? 运算符第二次出场：找不到 '"key":' 时 find 返回 None，? 让整个函数
    // 提前返回 None（Option 也能用 ? —— "没有值"也是一种"失败"）。
    // + pat.len()：把"匹配起点"换算成"冒号后第一个字符的下标"
    let idx = compact.find(&pat)? + pat.len();
    let digits: String = compact[idx..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    // parse 失败（空串等）返回 Err → .ok() 摊平成 None，调用方用默认值兜底
    digits.parse().ok()
}
