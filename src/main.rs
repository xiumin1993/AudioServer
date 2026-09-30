use eframe::egui;
use log::{LevelFilter, Log, Metadata, Record};
use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Instant;
use tokio::sync::mpsc as tokio_mpsc;

use audioserver::env_check::{self, EnvGuide, GuideAction};
use audioserver::server::{run_server, ServerCommand, ServerConfig, ServerEvent, ServerStatus};

// ── 双输出日志（stderr + 文件）──
// env_logger 只能写 stderr，这里自定义 logger 同时写文件，
// 方便排查问题时回溯完整日志。
struct DualLogger {
    level: LevelFilter,
    log_file: OnceLock<std::fs::File>,
}

thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(512));
}

impl Log for DualLogger {
    fn enabled(&self, meta: &Metadata) -> bool {
        meta.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        BUF.with(|buf| {
            let mut b = buf.borrow_mut();
            b.clear();
            let _ = write!(
                b,
                "[{} {} {}] {}\n",
                record.level(),
                record.target(),
                record.file().unwrap_or("?"),
                record.args()
            );
            // stderr
            let _ = std::io::stderr().write_all(&b);
            // 文件（lazy init，只打开一次）
            if self.log_file.get().is_none() {
                let exe_dir = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                    .unwrap_or_default();
                if let Ok(f) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(exe_dir.join("audioserver.log"))
                {
                    let _ = self.log_file.set(f);
                }
            }
            if let Some(f) = self.log_file.get() {
                let mut f_ref = f;
                let _ = f_ref.write_all(&b);
            }
        });
    }

    fn flush(&self) {}
}

fn init_logger() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(LevelFilter::Info);
    let logger = Box::new(DualLogger {
        level,
        log_file: OnceLock::new(),
    });
    log::set_boxed_logger(logger).ok();
    log::set_max_level(level);
}

// ── v2 浅色主题配色 ──
mod colors {
    use eframe::egui::Color32;

    // 背景
    pub const BG_WHITE: Color32 = Color32::from_rgb(255, 255, 255);
    pub const BG_LIGHT: Color32 = Color32::from_rgb(250, 250, 250);
    // 边框
    pub const BORDER: Color32 = Color32::from_rgb(229, 229, 229);
    // 强调色（蓝）
    pub const ACCENT: Color32 = Color32::from_rgb(37, 99, 235);
    pub const ACCENT_BG: Color32 = Color32::from_rgb(240, 245, 255);
    pub const ACCENT_BORDER: Color32 = Color32::from_rgb(208, 223, 255);

    // 文字
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(34, 34, 34);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(102, 102, 102);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(153, 153, 153);
    pub const TEXT_DISABLED: Color32 = Color32::from_rgb(187, 187, 187);

    // 状态
    pub const GREEN: Color32 = Color32::from_rgb(34, 197, 94);
    pub const RED: Color32 = Color32::from_rgb(220, 38, 38);
    pub const RED_BG: Color32 = Color32::from_rgb(254, 242, 242);
    pub const RED_BORDER: Color32 = Color32::from_rgb(254, 202, 202);
    pub const WARN_TEXT: Color32 = Color32::from_rgb(180, 83, 9);
    pub const WARN_BG: Color32 = Color32::from_rgb(255, 251, 235);
    pub const WARN_BORDER: Color32 = Color32::from_rgb(253, 230, 138);

    // Toggle
    pub const TOGGLE_ON: Color32 = Color32::from_rgb(34, 197, 94);
    pub const TOGGLE_OFF: Color32 = Color32::from_rgb(221, 221, 221);
}

fn main() -> eframe::Result<()> {
    init_logger();
    log::info!("[Main] AudioServer starting (dual logger: stderr + audioserver.log)");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 540.0])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Audio Server",
        options,
        Box::new(|cc| {
            // 中文字体兜底（必须在设 visuals / 建界面之前）
            install_cjk_fonts(&cc.egui_ctx);

            // 基于 egui 浅色主题，只覆盖需要的颜色
            let mut visuals = egui::Visuals::light();
            visuals.override_text_color = Some(colors::TEXT_PRIMARY);
            visuals.hyperlink_color = colors::ACCENT;
            visuals.faint_bg_color = colors::BG_LIGHT;
            visuals.extreme_bg_color = colors::BG_LIGHT;
            visuals.window_stroke = egui::Stroke::new(1.0_f32, colors::BORDER);
            visuals.widgets.noninteractive.bg_fill = colors::BG_WHITE;
            visuals.widgets.noninteractive.fg_stroke =
                egui::Stroke::new(1.0_f32, colors::TEXT_SECONDARY);
            visuals.widgets.inactive.bg_fill = colors::BG_WHITE;
            visuals.widgets.inactive.fg_stroke =
                egui::Stroke::new(1.0_f32, colors::TEXT_PRIMARY);
            visuals.widgets.hovered.bg_fill = colors::BG_LIGHT;
            visuals.widgets.hovered.fg_stroke =
                egui::Stroke::new(1.0_f32, colors::TEXT_PRIMARY);
            visuals.widgets.active.bg_fill = colors::ACCENT_BG;
            visuals.widgets.active.fg_stroke =
                egui::Stroke::new(1.0_f32, colors::ACCENT);
            visuals.selection.bg_fill =
                egui::Color32::from_rgba_premultiplied(37, 99, 235, 30);
            visuals.selection.stroke = egui::Stroke::new(1.0_f32, colors::ACCENT);
            cc.egui_ctx.set_visuals(visuals);

            let app = AudioServerApp::new();
            // 门禁生效时标题栏也换成"需要准备驱动"，任务栏上一眼可辨
            if let Some(g) = app.env_gate.as_ref() {
                cc.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Title(
                        g.viewport_title().to_string(),
                    ));
            }
            Ok(Box::new(app))
        }),
    )
}

// ── 中文字体兜底（v3.5）──
// 为什么必须做：egui 自带的两款字体（Proportional / Monospace）不含中日韩字形，
//              中文会被画成一排方框 □□□（环境向导页全是中文，首屏就不能看）。
// 为什么从系统字体目录读，而不是塞进 exe：
//              ① 中文字体动辄 10MB，会毁掉"绿色单文件、体积小"的目标；
//              ② 微软雅黑/黑体是微软版权字体，重新分发不合规。
// 失败策略：一个都读不到 → 保持默认字体，界面退回英文显示，绝不 panic。
//           （epaint 解析字体失败是直接 panic 的，所以交给它之前先校验文件头）
const CJK_FONT_NAME: &str = "system-cjk";

/// 候选中文字体，按"最稳的排前面"排序：
/// 单文件 TTF 解析最保险，TTC（字体集合）放后面。
const CJK_FONT_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\simhei.ttf",                 // 黑体：中文版 Windows 标配，纯 TTF
    r"C:\Windows\Fonts\Deng.ttf",                   // 等线
    r"C:\Windows\Fonts\msyh.ttc",                   // 微软雅黑：Win10/11 各语言版本都有（TTC 集合）
    r"C:\Windows\Fonts\simsun.ttc",                 // 宋体（TTC 集合）
    "/System/Library/Fonts/PingFang.ttc",           // macOS（Mac 移植期中文界面同样需要）
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", // Linux 兜底
];

/// 只校验文件头魔数：够挡住"读回来的根本不是字体"这种最坏情况。
/// TrueType 开头是版本号 0x00010000；'true'/'ttcf'/'OTTO' 是另外几种合法开头。
fn looks_like_font(b: &[u8]) -> bool {
    if b.len() < 12 {
        return false;
    }
    let h = &b[..4];
    if h == [0x00, 0x01, 0x00, 0x00] || h == b"true" || h == b"OTTO" || h == b"ttcf" {
        return true;
    }
    // 'ttcf' 之外还可能是 'oth '（OpenType 集合）；其它一律拒绝
    h == b"oth "
}

/// 把系统中文字体追加到两个字体族末尾，作为"内置字体画不出来的字"的兜底字形。
/// 注意是 push 到末尾而不是插到开头：英文/数字继续用原来的 egui 字体，观感不变。
fn install_cjk_fonts(ctx: &egui::Context) {
    for path in CJK_FONT_CANDIDATES {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if !looks_like_font(&bytes) {
            log::warn!("[Main] {path} 不像字体文件，跳过");
            continue;
        }
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert(CJK_FONT_NAME.to_owned(), egui::FontData::from_owned(bytes));
        for family in [
            egui::FontFamily::Proportional,
            egui::FontFamily::Monospace,
        ] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push(CJK_FONT_NAME.to_owned());
        }
        ctx.set_fonts(fonts);
        log::info!("[Main] CJK font installed: {path}");
        return;
    }
    log::warn!("[Main] 系统里没找到可用的中文字体，界面中文会显示成方框");
}

// ── Tab 枚举 ──
#[derive(Debug, Clone, Copy, PartialEq)]
enum AppTab {
    Connection,
    Settings,
    Log,
}

// ── v3：全局工作模式 ──
#[derive(Debug, Clone, Copy, PartialEq)]
enum AppMode {
    /// v2 原有：PC loopback 采集 → 推流到手机播放
    Speaker,
    /// v3 新增：手机上行 PCM → 注入 CABLE Input（系统侧 CABLE Output 即手机麦克风）
    Mic,
    /// v3.4 新增：手机上行 JPEG → 注入 Unity Video Capture 虚拟摄像头
    Camera,
}

/// Mic 链路统计快照（来自 ServerEvent::MicStats）
#[derive(Clone, Copy)]
struct MicStatsView {
    kbps: u32,
    total_kb: u64,
    interval_ms: u32,
}

/// v3.4：摄像头链路统计快照（来自 ServerEvent::CamStats）
#[derive(Clone, Copy)]
struct CamStatsView {
    kbps: u32,
    fps: u32,
    width: u16,
    height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum ConnectionType {
    Wifi,
    Usb,
    Bluetooth,
}

struct AudioServerApp {
    port: String,
    sample_rate: String,
    channels: String,
    buffer_size: String,
    connection_type: ConnectionType,
    server_status: ServerStatus,
    server_handle: Option<std::thread::JoinHandle<()>>,
    cmd_tx: Option<tokio_mpsc::UnboundedSender<ServerCommand>>,
    event_rx: Option<mpsc::Receiver<ServerEvent>>,
    client_count: u64,
    logs: Vec<String>,
    #[allow(dead_code)]
    device_name: String,
    active_tab: AppTab,
    start_time: Option<Instant>,
    // ── v3 麦克风模式状态 ──
    mode: AppMode,
    /// 注入引擎已打开的播放设备名（None = 未找到 CABLE Input）
    mic_engine_device: Option<String>,
    /// 手机是否正在上行（v3.1 语义 = CABLE Output 被应用占用）
    mic_live: bool,
    /// v3.1：手机待命会话已登记（mic_start standby），等待 PC 应用取用
    mic_session: bool,
    /// v3.1：手机侧手动闭麦中（静音键按下）
    mic_muted: bool,
    /// 上行来源：IP / 采样率 / 声道
    mic_source: Option<(String, u32, u16)>,
    /// 电平滚动条（左旧右新，0.0~1.0）
    mic_bars: [f32; 34],
    /// 最近一次链路统计
    mic_stats: Option<MicStatsView>,
    /// 可选：mic 上行时暂停下行播放（默认关，全双工同时进行）
    pause_speaker_live: bool,
    // ── v3.4 摄像头模式状态 ──
    /// 有应用正在观看 Unity Video Capture（vcam 引擎上报 = cam_state 推送依据）
    cam_live: bool,
    /// 手机摄像头会话已登记（cam_start），等待应用取用
    cam_session: bool,
    /// 上行来源手机 IP
    cam_source: Option<String>,
    /// 最近一次链路统计（kbps / fps / 当前分辨率）
    cam_stats: Option<CamStatsView>,
    /// 手机上报的能力 JSON 原文（"这台手机最高支持什么画质"）
    cam_caps: Option<String>,
    /// GUI 预览纹理（每秒随 CamFrame 事件刷新一张最新 JPEG 解码结果）
    cam_texture: Option<egui::TextureHandle>,
    // ── v3.5 启动环境门禁 ──
    /// Some = 必需驱动缺失，主窗口整页只显示向导，服务端线程不起
    env_gate: Option<EnvGuide>,
}

impl AudioServerApp {
    fn new() -> Self {
        let mut app = Self {
            port: "8080".to_string(),
            sample_rate: "48000".to_string(),
            channels: "2".to_string(),
            buffer_size: "1024".to_string(),
            connection_type: ConnectionType::Wifi,
            server_status: ServerStatus::Stopped,
            server_handle: None,
            cmd_tx: None,
            event_rx: None,
            client_count: 0,
            logs: Vec::new(),
            device_name: String::new(),
            active_tab: AppTab::Connection,
            start_time: None,
            mode: AppMode::Speaker,
            mic_engine_device: None,
            mic_live: false,
            mic_session: false,
            mic_muted: false,
            mic_source: None,
            mic_bars: [0.0; 34],
            mic_stats: None,
            pause_speaker_live: false,
            cam_live: false,
            cam_session: false,
            cam_source: None,
            cam_stats: None,
            cam_caps: None,
            cam_texture: None,
            env_gate: None,
        };
        // v3.5：启动先做一次【只读】环境自检（不写注册表、不装任何东西）。
        //   · 必需驱动齐 → 和以前一样，自动开启服务端
        //   · 缺驱动     → 不起服务端线程，主窗口只显示"环境准备"向导页，
        //                  用户装完点【重新检测】才会真正进入主界面
        // 开发调试（Mac 移植期 / 想在缺驱动的机器上看 UI）可用环境变量跳过：
        //   set PCSPEAKER_SKIP_ENV_CHECK=1
        let report = env_check::detect();
        let skip_gate = std::env::var("PCSPEAKER_SKIP_ENV_CHECK").is_ok();
        // 开发/演示用：PCSPEAKER_FORCE_ENV_GUIDE=1 时，第一帧就停在向导页，
        // 并且把检测结果换成"什么都没装"的样例，这样不用找一台干净电脑，
        // 也能看到缺项 + 下载按钮的完整形态（点【重新检测】会按真实结果判定，
        // 环境其实齐全的话照常进主界面）。
        let force_guide = std::env::var("PCSPEAKER_FORCE_ENV_GUIDE").is_ok();
        let report = if force_guide {
            env_check::EnvReport::demo_missing()
        } else {
            report
        };
        if (report.ready() || skip_gate) && !force_guide {
            if skip_gate && !report.ready() {
                log::warn!("[Main] PCSPEAKER_SKIP_ENV_CHECK set —— 环境未齐，仍强行进入主界面");
            }
            app.start_server();
        } else {
            log::info!("[Main] 环境门禁生效：服务端未启动，等待用户安装驱动");
            app.env_gate = Some(EnvGuide::new(report));
        }
        app
    }

    fn poll_server_events(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(rx) = &self.event_rx {
            for _ in 0..50 {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.server_status = ServerStatus::Stopped;
                        self.cmd_tx = None;
                        self.event_rx = None;
                        self.start_time = None;
                        self.add_log("Server process ended".to_string());
                        return;
                    }
                }
            }
        }
        for event in events {
            self.handle_event(event, ctx);
        }
    }

    fn handle_event(&mut self, event: ServerEvent, ctx: &egui::Context) {
        match event {
            ServerEvent::Log(msg) => self.add_log(msg),
            ServerEvent::StatusChanged(status) => {
                self.server_status = status;
                if status == ServerStatus::Running {
                    self.start_time = Some(Instant::now());
                } else {
                    self.start_time = None;
                }
            }
            ServerEvent::ClientConnected(_id) => self.client_count += 1,
            ServerEvent::ClientDisconnected(_id) => {
                self.client_count = self.client_count.saturating_sub(1);
            }
            ServerEvent::Error(msg) => {
                self.add_log(format!("[Error] {}", msg));
            }
            // ── v3 麦克风事件 ──
            ServerEvent::MicLevel(level) => {
                // 滚动电平：左移一格，右侧写入新值（×3 感知增益后截断）
                self.mic_bars.rotate_left(1);
                self.mic_bars[self.mic_bars.len() - 1] = (level * 3.0).min(1.0);
            }
            ServerEvent::MicStats { kbps, total_kb, interval_ms, .. } => {
                self.mic_stats = Some(MicStatsView { kbps, total_kb, interval_ms });
            }
            ServerEvent::MicSource { ip, sample_rate, channels } => {
                self.mic_session = true;
                self.mic_source = Some((ip, sample_rate, channels));
            }
            ServerEvent::MicStopped => {
                self.mic_live = false;
                self.mic_session = false;
                self.mic_muted = false;
                self.mic_source = None;
                self.mic_stats = None;
                self.mic_bars = [0.0; 34];
            }
            // v3.1：CABLE Output 被应用占用翻转 → 决定手机上行/待命
            ServerEvent::MicState { active } => {
                self.mic_live = active;
                if !active {
                    self.mic_bars = [0.0; 34];
                }
                self.add_log(format!(
                    "[Mic] Capture {} (phone {})",
                    if active { "ACTIVE" } else { "idle" },
                    if active { "woken" } else { "standby" }
                ));
            }
            // v3.1：手机静音键状态
            ServerEvent::MicMuted { muted } => {
                self.mic_muted = muted;
                if muted {
                    self.mic_bars = [0.0; 34];
                }
            }
            ServerEvent::MicEngine { device } => {
                self.mic_engine_device = device.clone();
                match device {
                    Some(name) => self.add_log(format!("[Mic] Engine ready → {}", name)),
                    None => self.add_log("[Mic] Engine unavailable (CABLE Input not found)".to_string()),
                }
            }
            // ── v3.4 摄像头事件 ──
            ServerEvent::CamSource { ip } => {
                self.cam_session = true;
                self.cam_source = Some(ip.clone());
                self.add_log(format!("[Cam] Phone {} ready as camera source", ip));
            }
            ServerEvent::CamStopped => {
                self.cam_live = false;
                self.cam_session = false;
                self.cam_source = None;
                self.cam_stats = None;
                self.cam_texture = None;
                self.add_log("[Cam] Camera uplink stopped".to_string());
            }
            ServerEvent::CamState { active } => {
                self.cam_live = active;
                if !active {
                    // 无人观看：手机会立刻关相机，预览同步清掉（黑位占位）
                    self.cam_texture = None;
                    self.cam_stats = None;
                }
                self.add_log(format!(
                    "[Cam] Virtual camera {}",
                    if active { "is being watched by an app" } else { "released" }
                ));
            }
            ServerEvent::CamStats { kbps, fps, width, height, .. } => {
                self.cam_stats = Some(CamStatsView { kbps, fps, width, height });
            }
            ServerEvent::CamCaps { caps, ip } => {
                self.cam_caps = Some(caps);
                self.add_log(format!("[Cam] Capabilities reported by {}", ip));
            }
            ServerEvent::CamFrame(jpeg) => {
                // 解码最新一帧 → 上传为 GUI 纹理（1fps，开销可忽略）
                if let Some(tex) = decode_jpeg_to_texture(ctx, &jpeg) {
                    self.cam_texture = Some(tex);
                }
            }
        }
    }

    /// 向服务器线程发命令（服务器未运行时忽略）
    fn send_cmd(&self, cmd: ServerCommand) {
        if let Some(tx) = &self.cmd_tx {
            tx.send(cmd).ok();
        }
    }

    /// 按当前模式向服务器同步"下行暂停"状态
    fn sync_speaker_pause(&self) {
        let pause = self.mode == AppMode::Mic && self.pause_speaker_live;
        self.send_cmd(ServerCommand::SetSpeakerPaused(pause));
    }

    fn add_log(&mut self, msg: String) {
        let timestamp = chrono_now();
        self.logs.push(format!("[{}] {}", timestamp, msg));
        if self.logs.len() > 200 {
            self.logs.drain(0..50);
        }
    }

    fn start_server(&mut self) {
        if self.server_status == ServerStatus::Running {
            return;
        }
        let config = ServerConfig {
            port: self.port.parse().unwrap_or(8080),
            sample_rate: self.sample_rate.parse().unwrap_or(48000),
            channels: self.channels.parse().unwrap_or(2),
            buffer_size: self.buffer_size.parse().unwrap_or(1024),
        };
        let (event_tx, event_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();
        self.event_rx = Some(event_rx);
        self.cmd_tx = Some(cmd_tx);
        self.client_count = 0;
        self.logs.clear();
        self.start_time = Some(Instant::now());
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(run_server(config, event_tx, cmd_rx));
        });
        self.server_handle = Some(handle);
        self.add_log("Starting server...".to_string());
    }

    fn stop_server(&mut self) {
        if let Some(tx) = &self.cmd_tx {
            tx.send(ServerCommand::Stop).ok();
        }
        self.cmd_tx = None;
        self.event_rx = None;
        self.server_handle = None;
        self.server_status = ServerStatus::Stopped;
        self.start_time = None;
        self.add_log("Stop command sent".to_string());
    }

    fn get_local_ip() -> String {
        local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "Unknown".to_string())
    }

    fn uptime_str(&self) -> String {
        match self.start_time {
            Some(start) => {
                let elapsed = start.elapsed().as_secs();
                let h = elapsed / 3600;
                let m = (elapsed % 3600) / 60;
                let s = elapsed % 60;
                format!("{:02}:{:02}:{:02}", h, m, s)
            }
            None => "00:00:00".to_string(),
        }
    }

    fn audio_summary(&self) -> String {
        format!("{}Hz · {}ch · PCM", self.sample_rate, self.channels)
    }

    // ── Header：状态 + 开关 ──
    fn show_header(&mut self, ui: &mut egui::Ui, is_running: bool) {
        ui.horizontal(|ui| {
            // 小圆点状态指示器
            let dot_size = 12.0_f32;
            let (dot_rect, _) = ui.allocate_exact_size(
                egui::vec2(dot_size, dot_size),
                egui::Sense::hover(),
            );
            if ui.is_rect_visible(dot_rect) {
                let dot_color = if is_running {
                    colors::GREEN
                } else {
                    colors::TEXT_DISABLED
                };
                ui.painter()
                    .circle_filled(dot_rect.center(), dot_size / 2.0, dot_color);
            }

            ui.add_space(10.0);

            // 状态文字（v3：随模式变化）
            ui.vertical(|ui| {
                let title = if !is_running {
                    "Server Stopped"
                } else if self.mode == AppMode::Mic {
                    if self.mic_live && self.mic_muted {
                        "Mic Muted"
                    } else if self.mic_live {
                        "Mic Live"
                    } else if self.mic_session {
                        "Mic Standby"
                    } else {
                        "Mic Idle"
                    }
                } else if self.mode == AppMode::Camera {
                    // v3.4：摄像头模式标题（LIVE = 有应用真的在看，手机相机已开）
                    if self.cam_live {
                        "Cam Live"
                    } else if self.cam_session {
                        "Cam Standby"
                    } else {
                        "Cam Idle"
                    }
                } else {
                    "Server Running"
                };
                ui.label(
                    egui::RichText::new(title)
                        .size(15.0)
                        .strong()
                        .color(colors::TEXT_PRIMARY),
                );
                let subtitle = if !is_running {
                    "Toggle to start".to_string()
                } else if self.mode == AppMode::Mic {
                    if self.mic_live {
                        format!("Phone → CABLE · Uptime {}", self.uptime_str())
                    } else if self.mic_session {
                        "Phone on standby · speaks when any app opens the mic".to_string()
                    } else {
                        "Waiting for phone connection".to_string()
                    }
                } else if self.mode == AppMode::Camera {
                    if self.cam_live {
                        format!("Phone → Unity Video Capture · Uptime {}", self.uptime_str())
                    } else if self.cam_session {
                        "Phone on standby · opens camera when any app watches it".to_string()
                    } else {
                        "Waiting for phone camera session".to_string()
                    }
                } else {
                    format!("Uptime {}", self.uptime_str())
                };
                ui.label(
                    egui::RichText::new(subtitle)
                        .size(11.0)
                        .color(colors::TEXT_MUTED),
                );
            });

            // Toggle 开关
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let toggle_w = 44.0_f32;
                let toggle_h = 24.0_f32;
                let (toggle_rect, toggle_resp) = ui.allocate_exact_size(
                    egui::vec2(toggle_w, toggle_h),
                    egui::Sense::click(),
                );
                if toggle_resp.clicked() {
                    if is_running {
                        self.stop_server();
                    } else {
                        self.start_server();
                    }
                }
                if ui.is_rect_visible(toggle_rect) {
                    let knob_r = 10.0_f32;
                    let knob_y = toggle_rect.center().y;
                    let knob_x = if is_running {
                        toggle_rect.right() - 12.0_f32
                    } else {
                        toggle_rect.left() + 12.0_f32
                    };
                    let bg_color = if is_running {
                        colors::TOGGLE_ON
                    } else {
                        colors::TOGGLE_OFF
                    };
                    ui.painter().rect_filled(
                        toggle_rect,
                        egui::Rounding::same(toggle_h / 2.0),
                        bg_color,
                    );
                    // 白色旋钮，带一点阴影感
                    ui.painter().circle_filled(
                        egui::pos2(knob_x, knob_y),
                        knob_r,
                        egui::Color32::WHITE,
                    );
                    ui.painter().circle_stroke(
                        egui::pos2(knob_x, knob_y),
                        knob_r,
                        egui::Stroke::new(
                            0.5_f32,
                            egui::Color32::from_rgba_premultiplied(0, 0, 0, 25),
                        ),
                    );
                }
                if toggle_resp.hovered() {
                    ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                }
            });
        });
    }

    // ── v3：模式切换条（Speaker / Microphone 两个胶囊按钮） ──
    fn show_mode_switch(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(34.0);
            let modes = [
                (AppMode::Speaker, "Speaker Mode"),
                (AppMode::Mic, "Microphone Mode"),
                (AppMode::Camera, "Camera Mode"),
            ];
            let gap = 4.0_f32;
            let btn_w = (ui.available_width() - gap * (modes.len() as f32 - 1.0))
                / modes.len() as f32;

            for (m, label) in &modes {
                let is_active = self.mode == *m;
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(btn_w, 30.0),
                    egui::Sense::click(),
                );
                if resp.clicked() && !is_active {
                    self.mode = *m;
                    // 切模式时同步下行暂停策略（默认全双工不暂停）
                    self.sync_speaker_pause();
                }
                if ui.is_rect_visible(rect) {
                    let (bg, border, text_color) = if is_active {
                        (colors::ACCENT_BG, colors::ACCENT, colors::ACCENT)
                    } else {
                        (colors::BG_LIGHT, colors::BORDER, colors::TEXT_MUTED)
                    };
                    ui.painter().rect(
                        rect,
                        egui::Rounding::same(6.0),
                        bg,
                        egui::Stroke::new(1.0_f32, border),
                    );
                    let galley = ui.fonts(|f| {
                        f.layout_no_wrap(
                            label.to_string(),
                            egui::FontId::proportional(if is_active { 12.5 } else { 12.0 }),
                            text_color,
                        )
                    });
                    let text_pos = egui::pos2(
                        rect.center().x - galley.size().x / 2.0,
                        rect.center().y - galley.size().y / 2.0,
                    );
                    ui.painter().galley(text_pos, galley, text_color);
                }
                if resp.hovered() {
                    ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                }
            }
        });
    }

    // ── v3：上行电平滚动条（复刻原型里的动画电平条） ──
    fn show_level_bars(&self, ui: &mut egui::Ui) {
        let bar_w = 7.0_f32;
        let gap = 3.0_f32;
        let max_h = 44.0_f32;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), max_h),
            egui::Sense::hover(),
        );
        if !ui.is_rect_visible(rect) {
            return;
        }
        // 按可用宽度决定画多少根（从右往左取最新的值）
        let n = ((rect.width() + gap) / (bar_w + gap)).floor() as usize;
        let n = n.min(self.mic_bars.len());
        let bars = &self.mic_bars[self.mic_bars.len() - n..];
        for (i, &v) in bars.iter().enumerate() {
            let x = rect.left() + i as f32 * (bar_w + gap);
            let h = (v * (max_h - 4.0)).max(3.0);
            let r = egui::Rect::from_min_size(
                egui::pos2(x, rect.bottom() - h),
                egui::vec2(bar_w, h),
            );
            let color = if v < 0.02 {
                colors::ACCENT_BORDER
            } else {
                colors::ACCENT
            };
            ui.painter().rect_filled(r, egui::Rounding::same(3.0), color);
        }
    }

    // ── v3：小圆角状态徽章（手绘：0.29 的 Label 无 fill） ──
    fn show_chip(ui: &mut egui::Ui, text: &str, fg: egui::Color32, bg: egui::Color32) {
        let galley = ui.fonts(|f| {
            f.layout_no_wrap(text.to_string(), egui::FontId::proportional(10.0), fg)
        });
        let pad_x = 8.0_f32;
        let pad_y = 3.0_f32;
        let size = egui::vec2(galley.size().x + pad_x * 2.0, galley.size().y + pad_y * 2.0);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        if ui.is_rect_visible(rect) {
            ui.painter()
                .rect_filled(rect, egui::Rounding::same(size.y / 2.0), bg);
            let pos = rect.left_center() + egui::vec2(pad_x, 0.0);
            ui.painter().galley(pos, galley, fg);
        }
    }

    // ── Tab 栏 ──
    fn show_tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(38.0);
            let tabs = [
                (AppTab::Connection, "Connection"),
                (AppTab::Settings, "Settings"),
                (AppTab::Log, "Log"),
            ];
            let total_width = ui.available_width();
            let tab_width = total_width / tabs.len() as f32;

            for (tab, label) in &tabs {
                let is_active = self.active_tab == *tab;
                let (tab_rect, tab_resp) = ui.allocate_exact_size(
                    egui::vec2(tab_width, 38.0),
                    egui::Sense::click(),
                );
                if tab_resp.clicked() {
                    self.active_tab = *tab;
                }
                if ui.is_rect_visible(tab_rect) {
                    // 选中态：底部蓝色指示线
                    if is_active {
                        let line_h = 2.0_f32;
                        let line_rect = egui::Rect::from_min_size(
                            egui::pos2(tab_rect.left(), tab_rect.bottom() - line_h),
                            egui::vec2(tab_rect.width(), line_h),
                        );
                        ui.painter()
                            .rect_filled(line_rect, egui::Rounding::ZERO, colors::ACCENT);
                    }
                    let text_color = if is_active {
                        colors::ACCENT
                    } else {
                        colors::TEXT_MUTED
                    };
                    let galley = ui.fonts(|f| {
                        f.layout_no_wrap(
                            label.to_string(),
                            egui::FontId::proportional(13.0),
                            text_color,
                        )
                    });
                    let text_pos = egui::pos2(
                        tab_rect.center().x - galley.size().x / 2.0,
                        tab_rect.center().y - galley.size().y / 2.0,
                    );
                    ui.painter().galley(text_pos, galley, text_color);
                }
                if tab_resp.hovered() {
                    ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                }
            }
        });
    }

    // ── Footer ──
    fn show_footer(&self, ui: &mut egui::Ui, is_running: bool) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(10.0)
                    .color(colors::TEXT_DISABLED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // v3：摘要随模式变化
                let summary = if !is_running {
                    "Idle".to_string()
                } else if self.mode == AppMode::Mic {
                    match &self.mic_source {
                        Some((_, sr, ch)) => format!("{}Hz · {}ch · MIC", sr, ch),
                        None => "48kHz · 1ch · MIC (idle)".to_string(),
                    }
                } else if self.mode == AppMode::Camera {
                    // v3.4：底部摘要显示当前上行画质（有统计时）
                    match &self.cam_stats {
                        Some(s) if s.width > 0 => {
                            format!("{}×{} · {}fps · CAM", s.width, s.height, s.fps)
                        }
                        _ => "CAM (idle)".to_string(),
                    }
                } else {
                    self.audio_summary()
                };
                ui.label(
                    egui::RichText::new(summary)
                        .size(10.0)
                        .color(colors::TEXT_DISABLED),
                );
                ui.add_space(6.0);
                let dot_size = 6.0_f32;
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(dot_size, dot_size),
                    egui::Sense::hover(),
                );
                if ui.is_rect_visible(rect) {
                    let dot_color = if is_running {
                        colors::GREEN
                    } else {
                        colors::TEXT_DISABLED
                    };
                    ui.painter()
                        .circle_filled(rect.center(), dot_size / 2.0, dot_color);
                }
            });
        });
    }

    // ── Connection Tab ──
    fn show_connection_tab(&mut self, ui: &mut egui::Ui) {
        // v3：Mic 模式呈现注入状态视图
        if self.mode == AppMode::Mic {
            self.show_mic_connection_tab(ui);
            return;
        }
        // v3.4：Camera 模式呈现虚拟摄像头视图（预览 + 统计 + 双开关）
        if self.mode == AppMode::Camera {
            self.show_camera_connection_tab(ui);
            return;
        }
        let local_ip = Self::get_local_ip();

        // 连接方式按钮行
        ui.horizontal(|ui| {
            ui.set_height(56.0);
            let conns = [
                (ConnectionType::Wifi, "WiFi LAN", true),
                (ConnectionType::Usb, "USB", false),
                (ConnectionType::Bluetooth, "Bluetooth", false),
            ];
            let gap = 8.0_f32;
            let btn_w = (ui.available_width() - gap * 2.0) / 3.0;

            for (ct, label, enabled) in &conns {
                let is_active = self.connection_type == *ct && *enabled;
                let (resp_rect, resp) = ui.allocate_exact_size(
                    egui::vec2(btn_w, 52.0),
                    if *enabled {
                        egui::Sense::click()
                    } else {
                        egui::Sense::hover()
                    },
                );
                if ui.is_rect_visible(resp_rect) {
                    let (bg, border, text_color) = if is_active {
                        (colors::ACCENT_BG, colors::ACCENT, colors::ACCENT)
                    } else if *enabled {
                        (colors::BG_WHITE, colors::BORDER, colors::TEXT_SECONDARY)
                    } else {
                        (colors::BG_WHITE, colors::BORDER, colors::TEXT_DISABLED)
                    };
                    ui.painter().rect(
                        resp_rect,
                        egui::Rounding::same(8.0),
                        bg,
                        egui::Stroke::new(1.0_f32, border),
                    );
                    // 标签文字居中
                    let galley = ui.fonts(|f| {
                        f.layout_no_wrap(
                            label.to_string(),
                            egui::FontId::proportional(12.0),
                            text_color,
                        )
                    });
                    let text_pos = egui::pos2(
                        resp_rect.center().x - galley.size().x / 2.0,
                        resp_rect.center().y - galley.size().y / 2.0,
                    );
                    ui.painter().galley(text_pos, galley, text_color);
                }
                if resp.clicked() && *enabled {
                    self.connection_type = *ct;
                }
                if resp.hovered() && *enabled {
                    ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                }
            }
        });

        ui.add_space(4.0);

        // 连接信息卡片
        let card_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        card_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new("CONNECTION INFO")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);

            // IP 行
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Local IP")
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Copy").size(10.0).color(colors::ACCENT),
                            )
                            .fill(colors::ACCENT_BG)
                            .stroke(egui::Stroke::new(1.0_f32, colors::ACCENT_BORDER))
                            .rounding(4.0),
                        )
                        .clicked()
                    {
                        ui.output_mut(|o| o.copied_text = local_ip.clone());
                    }
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(&local_ip)
                            .monospace()
                            .size(13.0)
                            .color(colors::ACCENT),
                    );
                });
            });

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            // WebSocket 地址
            let ws_addr = format!("ws://{}:{}/ws/audio", local_ip, self.port);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("WebSocket")
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Copy").size(10.0).color(colors::ACCENT),
                            )
                            .fill(colors::ACCENT_BG)
                            .stroke(egui::Stroke::new(1.0_f32, colors::ACCENT_BORDER))
                            .rounding(4.0),
                        )
                        .clicked()
                    {
                        ui.output_mut(|o| o.copied_text = ws_addr.clone());
                    }
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(&ws_addr)
                            .monospace()
                            .size(11.0)
                            .color(colors::ACCENT),
                    );
                });
            });
        });

        ui.add_space(4.0);

        // 客户端数量卡片
        let client_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        client_frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                let count_color = if self.client_count > 0 {
                    colors::ACCENT
                } else {
                    colors::TEXT_DISABLED
                };
                ui.label(
                    egui::RichText::new(format!("{}", self.client_count))
                        .size(28.0)
                        .strong()
                        .color(count_color),
                );
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("Connected Clients")
                            .size(12.0)
                            .color(colors::TEXT_MUTED),
                    );
                    if self.client_count == 0 {
                        ui.label(
                            egui::RichText::new("Waiting...")
                                .size(11.0)
                                .color(colors::TEXT_DISABLED),
                        );
                    }
                });
            });
        });
    }

    // ── v3：Mic 模式 Connection Tab ──
    fn show_mic_connection_tab(&mut self, ui: &mut egui::Ui) {
        let card = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        // 卡片 1：虚拟麦克风状态 + 实时电平
        card.show(ui, |ui| {
            ui.label(
                egui::RichText::new("VIRTUAL MICROPHONE")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Phone Mic (VB-CABLE)")
                        .size(14.0)
                        .strong()
                        .color(colors::TEXT_PRIMARY),
                );
                ui.add_space(8.0);
                if self.mic_engine_device.is_some() {
                    Self::show_chip(
                        ui,
                        "READY",
                        egui::Color32::from_rgb(4, 120, 87),
                        egui::Color32::from_rgb(236, 253, 245),
                    );
                } else {
                    Self::show_chip(
                        ui,
                        "DRIVER NEEDED",
                        colors::WARN_TEXT,
                        colors::WARN_BG,
                    );
                }
                // v3.1：上行状态芯片（自动感知：应用占用 → LIVE，空闲 → STANDBY）
                if self.mic_live && self.mic_muted {
                    Self::show_chip(
                        ui,
                        "MUTED",
                        egui::Color32::from_rgb(120, 113, 108),
                        egui::Color32::from_rgb(245, 245, 244),
                    );
                } else if self.mic_live {
                    Self::show_chip(
                        ui,
                        "LIVE",
                        egui::Color32::from_rgb(190, 18, 60),
                        egui::Color32::from_rgb(255, 228, 230),
                    );
                } else if self.mic_session {
                    Self::show_chip(
                        ui,
                        "STANDBY",
                        egui::Color32::from_rgb(4, 120, 87),
                        egui::Color32::from_rgb(236, 253, 245),
                    );
                }
            });
            ui.add_space(4.0);
            self.show_level_bars(ui);
            ui.add_space(4.0);
            let hint = if self.mic_engine_device.is_some() {
                "PC apps: select \"CABLE Output\" as input device — phone mic is live automatically."
            } else {
                "VB-CABLE playback device not found. Install VB-Audio Virtual Cable, then restart."
            };
            ui.label(
                egui::RichText::new(hint)
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            // 下行暂停开关按钮（默认全双工同时进行）
            let btn_label = if self.pause_speaker_live {
                "Speaker: Paused in Mic Mode"
            } else {
                "Speaker: Live (full duplex)"
            };
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(btn_label).size(11.0).color(colors::ACCENT),
                    )
                    .fill(colors::ACCENT_BG)
                    .stroke(egui::Stroke::new(1.0_f32, colors::ACCENT_BORDER))
                    .rounding(6.0),
                )
                .clicked()
            {
                self.pause_speaker_live = !self.pause_speaker_live;
                self.sync_speaker_pause();
            }
        });

        ui.add_space(8.0);

        // 卡片 2：上行来源（手机）
        let card2 = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card2.show(ui, |ui| {
            ui.label(
                egui::RichText::new("MIC SOURCE (PHONE)")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            match &self.mic_source {
                Some((ip, sr, ch)) => {
                    let (ip, sr, ch) = (ip.clone(), *sr, *ch);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("\u{1F4F1}").size(16.0));
                        ui.add_space(4.0);
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new(&ip)
                                    .size(13.0)
                                    .strong()
                                    .color(colors::TEXT_PRIMARY),
                            );
                            ui.label(
                                egui::RichText::new(format!("{}Hz · {}ch · PCM 16-bit", sr, ch))
                                    .size(11.0)
                                    .monospace()
                                    .color(colors::TEXT_MUTED),
                            );
                        });
                        // 录制红点
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(10.0, 10.0),
                                    egui::Sense::hover(),
                                );
                                if ui.is_rect_visible(rect) {
                                    let pulse =
                                        (0.6 + 0.4 * (self.uptime_f64() * 4.0).sin()) as f32;
                                    ui.painter().circle_filled(
                                        rect.center(),
                                        5.0,
                                        egui::Color32::from_rgb(220, 38, 38)
                                            .linear_multiply(pulse),
                                    );
                                }
                            },
                        );
                    });
                }
                None => {
                    ui.label(
                        egui::RichText::new("Waiting for phone to start mic uplink…")
                            .size(12.0)
                            .color(colors::TEXT_DISABLED),
                    );
                }
            }

            // 链路统计
            if let Some(stats) = self.mic_stats {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let cell = |ui: &mut egui::Ui, value: String, label: &str| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(value)
                                    .size(15.0)
                                    .strong()
                                    .monospace()
                                    .color(colors::TEXT_PRIMARY),
                            );
                            ui.label(
                                egui::RichText::new(label)
                                    .size(10.0)
                                    .color(colors::TEXT_MUTED),
                            );
                        });
                    };
                    cell(ui, format!("{}ms", stats.interval_ms), "chunk");
                    ui.add_space(8.0);
                    cell(ui, format!("{}k", stats.kbps), "kbps uplink");
                    ui.add_space(8.0);
                    cell(ui, format!("{}KB", stats.total_kb), "received");
                });
            }
        });

        ui.add_space(8.0);

        // 卡片 3：注入引擎设备信息（对应原型 Connection Info 位置）
        let card3 = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card3.show(ui, |ui| {
            ui.label(
                egui::RichText::new("INJECTION TARGET")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            let target = self
                .mic_engine_device
                .clone()
                .unwrap_or_else(|| "not opened (retrying every 3s)".to_string());
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Render device")
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(target)
                            .monospace()
                            .size(11.0)
                            .color(if self.mic_engine_device.is_some() {
                                colors::ACCENT
                            } else {
                                colors::TEXT_DISABLED
                            }),
                    );
                });
            });
        });
    }

    // ── v3.4：Camera 模式 Connection Tab（预览 + 统计 + 双入口控制）──
    fn show_camera_connection_tab(&mut self, ui: &mut egui::Ui) {
        let card = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        // 卡片 1：虚拟摄像头状态 + 双开关（请求 / 强制关闭）
        card.show(ui, |ui| {
            ui.label(
                egui::RichText::new("VIRTUAL CAMERA")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Unity Video Capture  ·  OBS Virtual Camera")
                        .size(14.0)
                        .strong()
                        .color(colors::TEXT_PRIMARY),
                );
                ui.add_space(8.0);
                // 状态芯片：LIVE=红（应用正在看，手机相机已开）/ STANDBY=绿（待命）
                if self.cam_live {
                    Self::show_chip(
                        ui,
                        "LIVE",
                        egui::Color32::from_rgb(190, 18, 60),
                        egui::Color32::from_rgb(255, 228, 230),
                    );
                } else if self.cam_session {
                    Self::show_chip(
                        ui,
                        "STANDBY",
                        egui::Color32::from_rgb(4, 120, 87),
                        egui::Color32::from_rgb(236, 253, 245),
                    );
                }
            });
            ui.add_space(4.0);
            let hint = if self.cam_live {
                "An app is watching the virtual camera — the phone camera is ON right now."
            } else if self.cam_session {
                "Phone is on standby. Pick \"OBS Virtual Camera\" (browsers/Edge) or \"Unity Video Capture\" (desktop apps) \u{2014} the phone camera starts automatically."
            } else {
                "No phone camera session. Use the request button below \u{2014} the phone asks the user to approve."
            };
            ui.label(
                egui::RichText::new(hint)
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            // 双开关：请求手机开启（软） + 强制关闭（隐私硬闸）
            ui.horizontal(|ui| {
                let req_enabled = self.client_count > 0;
                let req = egui::Button::new(
                    egui::RichText::new("\u{1F4F7} Request Phone Camera")
                        .size(11.0)
                        .color(if req_enabled { colors::ACCENT } else { colors::TEXT_DISABLED }),
                )
                .fill(colors::ACCENT_BG)
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    if req_enabled { colors::ACCENT_BORDER } else { colors::BORDER },
                ))
                .rounding(6.0);
                if ui.add_enabled(req_enabled, req).clicked() {
                    self.send_cmd(ServerCommand::CamRequest);
                    self.add_log("[Cam] Request sent to phone (awaiting user approval)".to_string());
                }
                let stop_enabled = self.cam_session;
                let stop_btn = egui::Button::new(
                    egui::RichText::new("\u{1F6D1} Force Stop Camera")
                        .size(11.0)
                        .color(if stop_enabled { colors::RED } else { colors::TEXT_DISABLED }),
                )
                .fill(colors::RED_BG)
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    if stop_enabled { colors::RED_BORDER } else { colors::BORDER },
                ))
                .rounding(6.0);
                if ui.add_enabled(stop_enabled, stop_btn).clicked() {
                    self.send_cmd(ServerCommand::CamForceStop);
                    self.add_log("[Cam] Force stop sent — phone camera revoked".to_string());
                }
            });
        });

        ui.add_space(8.0);

        // 卡片 2：实时预览 + 上行统计
        let card2 = egui::Frame::none()
            .fill(colors::BG_LIGHT)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card2.show(ui, |ui| {
            ui.label(
                egui::RichText::new("LIVE PREVIEW (1 FPS)")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            let preview_w = ui.available_width();
            match &self.cam_texture {
                Some(tex) => {
                    let size = tex.size(); // [w, h]（像素）
                    let h = preview_w * size[1] as f32 / size[0].max(1) as f32;
                    ui.add(
                        egui::Image::from_texture(tex)
                            .fit_to_exact_size(egui::vec2(preview_w, h))
                            .rounding(6.0),
                    );
                }
                None => {
                    // 占位框：与预览同宽、4:3 高，文字说明为何是黑的
                    let (_rect, _) = ui.allocate_exact_size(
                        egui::vec2(preview_w, preview_w * 3.0 / 4.0),
                        egui::Sense::hover(),
                    );
                    ui.label(
                        egui::RichText::new("camera off — waiting for an app to watch")
                            .size(11.0)
                            .color(colors::TEXT_DISABLED),
                    );
                }
            }

            // 链路统计 cells
            if let Some(stats) = self.cam_stats {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let cell = |ui: &mut egui::Ui, value: String, label: &str| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(value)
                                    .size(15.0)
                                    .strong()
                                    .monospace()
                                    .color(colors::TEXT_PRIMARY),
                            );
                            ui.label(
                                egui::RichText::new(label)
                                    .size(10.0)
                                    .color(colors::TEXT_MUTED),
                            );
                        });
                    };
                    cell(
                        ui,
                        format!("{}×{}", stats.width, stats.height),
                        "resolution",
                    );
                    ui.add_space(8.0);
                    cell(ui, format!("{}fps", stats.fps), "uplink fps");
                    ui.add_space(8.0);
                    cell(ui, format!("{}k", stats.kbps), "kbps uplink");
                });
            }
        });

        ui.add_space(8.0);

        // 卡片 3：手机来源 + 能力清单（"这台手机最高支持什么画质"）
        let card3 = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card3.show(ui, |ui| {
            ui.label(
                egui::RichText::new("CAMERA SOURCE (PHONE)")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            match &self.cam_source {
                Some(ip) => {
                    ui.label(
                        egui::RichText::new(format!("\u{1F4F1} {}", ip))
                            .size(13.0)
                            .strong()
                            .color(colors::TEXT_PRIMARY),
                    );
                }
                None => {
                    ui.label(
                        egui::RichText::new("Waiting for phone to register camera session…")
                            .size(12.0)
                            .color(colors::TEXT_DISABLED),
                    );
                }
            }
            // 能力 JSON 原文（手机上报，每镜头一档一档）—— 小字换行显示
            if let Some(caps) = self.cam_caps.clone() {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("PHONE CAPABILITIES")
                        .size(10.0)
                        .color(colors::TEXT_MUTED),
                );
                ui.label(
                    egui::RichText::new(wrap_json_brief(&caps))
                        .size(10.0)
                        .monospace()
                        .color(colors::TEXT_SECONDARY),
                );
            }
        });
    }

    /// v3.4：Camera 模式 Settings Tab（无参数可调，放使用说明与设备名）
    fn show_camera_settings_tab(&self, ui: &mut egui::Ui) {
        let card = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card.show(ui, |ui| {
            ui.label(
                egui::RichText::new("HOW TO USE")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "1. Connect the phone app to this server.\n\
                     2. Enable \"Camera Guardian\" on the phone, or press\n    \
                       \"Request Phone Camera\" in Connection tab.\n\
                     3. Pick a webcam in your app:\n    \
                       \u{2022} Browsers / Edge / Meet \u{2192} \"OBS Virtual Camera\"\n    \
                       \u{2022} Desktop apps (DingTalk etc.) \u{2192} \"Unity Video Capture\"\n\
                     4. The phone camera turns ON only while watched,\n    \
                       and turns OFF the instant the app releases it.",
                )
                .size(12.0)
                .color(colors::TEXT_SECONDARY),
            );
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new("Drivers: OBS Virtual Camera (browsers) + Unity Video Capture (desktop)")
                    .size(11.0)
                    .monospace()
                    .color(colors::TEXT_MUTED),
            );
            ui.label(
                egui::RichText::new("Placeholder: 640×480 black frame when no uplink")
                    .size(11.0)
                    .monospace()
                    .color(colors::TEXT_MUTED),
            );
        });
    }

    /// 运行秒数（浮点，供红点脉冲动画用）
    fn uptime_f64(&self) -> f64 {
        match self.start_time {
            Some(start) => start.elapsed().as_secs_f64(),
            None => 0.0,
        }
    }

    // ── Settings Tab ──
    fn show_settings_tab(&mut self, ui: &mut egui::Ui, is_running: bool) {
        // v3：Mic 模式呈现麦克风设置
        if self.mode == AppMode::Mic {
            self.show_mic_settings_tab(ui);
            return;
        }
        // v3.4：Camera 模式呈现摄像头设置（设备名提示 + 待命说明）
        if self.mode == AppMode::Camera {
            self.show_camera_settings_tab(ui);
            return;
        }
        // 运行时锁定提示
        if is_running {
            let warn_frame = egui::Frame::none()
                .fill(colors::WARN_BG)
                .stroke(egui::Stroke::new(1.0_f32, colors::WARN_BORDER))
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(12.0, 8.0));

            warn_frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("\u{26A0}").size(14.0).color(colors::WARN_TEXT));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("Stop the server to change settings.")
                            .size(12.0)
                            .color(colors::WARN_TEXT),
                    );
                });
            });
            ui.add_space(8.0);
        }

        // 音频参数卡片
        let audio_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        audio_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new("AUDIO")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.add_enabled_ui(!is_running, |ui| {
                egui::Grid::new("audio_settings")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Sample Rate")
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.sample_rate)
                                        .desired_width(80.0)
                                        .horizontal_align(egui::Align::RIGHT),
                                );
                            },
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Channels")
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.channels)
                                        .desired_width(80.0)
                                        .horizontal_align(egui::Align::RIGHT),
                                );
                            },
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Buffer Size")
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.buffer_size)
                                        .desired_width(80.0)
                                        .horizontal_align(egui::Align::RIGHT),
                                );
                            },
                        );
                        ui.end_row();
                    });
            });
        });

        ui.add_space(8.0);

        // 网络参数卡片
        let net_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        net_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new("NETWORK")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.add_enabled_ui(!is_running, |ui| {
                egui::Grid::new("net_settings")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Port")
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.port)
                                        .desired_width(80.0)
                                        .horizontal_align(egui::Align::RIGHT),
                                );
                            },
                        );
                        ui.end_row();
                    });
            });
        });
    }

    // ── v3：Mic 模式 Settings Tab ──
    fn show_mic_settings_tab(&mut self, ui: &mut egui::Ui) {
        let card = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        card.show(ui, |ui| {
            ui.label(
                egui::RichText::new("BEHAVIOR")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Pause speaker while mic live")
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let old = self.pause_speaker_live;
                    ui.checkbox(&mut self.pause_speaker_live, "");
                    if self.pause_speaker_live != old {
                        self.sync_speaker_pause();
                    }
                });
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Off (default): playback and mic run at the same time (full duplex).",
                )
                .size(11.0)
                .color(colors::TEXT_MUTED),
            );
        });

        ui.add_space(8.0);

        let card2 = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card2.show(ui, |ui| {
            ui.label(
                egui::RichText::new("UPLINK FORMAT (SET BY PHONE)")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            let (sr, ch) = match &self.mic_source {
                Some((_, sr, ch)) => (sr.to_string(), ch.to_string()),
                None => ("—".to_string(), "—".to_string()),
            };
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Sample Rate")
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(sr).monospace().size(13.0).color(colors::ACCENT),
                    );
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Channels")
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(ch).monospace().size(13.0).color(colors::ACCENT),
                    );
                });
            });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Keep phone at 48kHz mono to match the CABLE mix format.",
                )
                .size(11.0)
                .color(colors::TEXT_MUTED),
            );
        });
    }

    // ── Log Tab ──
    fn show_log_tab(&mut self, ui: &mut egui::Ui) {
        // 工具栏
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("RUNTIME LOG")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Clear").size(11.0).color(colors::RED),
                        )
                        .fill(colors::RED_BG)
                        .stroke(egui::Stroke::new(1.0_f32, colors::RED_BORDER))
                        .rounding(4.0),
                    )
                    .clicked()
                {
                    self.logs.clear();
                }
            });
        });

        ui.add_space(4.0);

        // 日志区域
        let log_frame = egui::Frame::none()
            .fill(colors::BG_LIGHT)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(6.0)
            .inner_margin(egui::Margin::same(10.0));

        let available_height = ui.available_height() - 4.0;
        log_frame.show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(available_height)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.logs.is_empty() {
                        ui.label(
                            egui::RichText::new("No logs yet")
                                .size(12.0)
                                .color(colors::TEXT_DISABLED),
                        );
                    }
                    for log in &self.logs {
                        let color = if log.contains("[Error]") {
                            colors::RED
                        } else if log.contains("[Mic]") || log.contains("[Cam]") {
                            // v3：麦克风链路日志用强调色区分（v3.4：摄像头同色）
                            colors::ACCENT
                        } else if log.contains("started")
                            || log.contains("connected")
                            || log.contains("Device")
                        {
                            colors::GREEN
                        } else {
                            colors::TEXT_SECONDARY
                        };
                        ui.label(
                            egui::RichText::new(log)
                                .monospace()
                                .size(11.0)
                                .color(color),
                        );
                    }
                });
        });
    }
}

impl eframe::App for AudioServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── v3.5 环境门禁：必需驱动没齐时，整页只显示向导 ──
        // 这里【不 poll_server_events、不画主界面、不起服务端】——
        // 用户看到的就是"程序还没开始工作"，避免连上手机才发现没设备。
        if self.env_gate.is_some() {
            // 门禁期间 1 秒兜底重绘：用户去装驱动/启动 FrameServer 时页面会自己刷新，
            // 不会出现"点了没反应"的错觉。
            ctx.request_repaint_after(std::time::Duration::from_millis(1000));
            let mut action = GuideAction::None;
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::none()
                        .fill(colors::BG_WHITE)
                        .inner_margin(egui::Margin::symmetric(18.0, 16.0)),
                )
                .show(ctx, |ui| {
                    if let Some(g) = self.env_gate.as_mut() {
                        action = g.render(ui);
                    }
                });
            match action {
                GuideAction::Ready => {
                    // 驱动装好了：撤掉门禁，此刻才真正启动服务端
                    self.env_gate = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(
                        "Audio Server".to_string(),
                    ));
                    self.start_server();
                    ctx.request_repaint();
                }
                GuideAction::Quit => {
                    // 窗口正在关闭：门禁状态【保留】，避免关窗前的最后一帧
                    // 落到主界面上（那样会在退出瞬间把服务端 UI 闪出来）。
                }
                GuideAction::None => {}
            }
            return;
        }

        self.poll_server_events(ctx);
        let is_running = self.server_status == ServerStatus::Running;

        // 顶部面板：Header
        egui::TopBottomPanel::top("header_panel")
            .frame(
                egui::Frame::none()
                    .fill(colors::BG_WHITE)
                    .inner_margin(egui::Margin::symmetric(16.0, 14.0)),
            )
            .show(ctx, |ui| {
                self.show_header(ui, is_running);
            });

        // 顶部面板：v3 模式切换条
        egui::TopBottomPanel::top("mode_panel")
            .frame(
                egui::Frame::none()
                    .fill(colors::BG_WHITE)
                    .inner_margin(egui::Margin::symmetric(16.0, 2.0)),
            )
            .show(ctx, |ui| {
                self.show_mode_switch(ui);
            });

        // 顶部面板：Tab 栏
        egui::TopBottomPanel::top("tab_panel")
            .frame(
                egui::Frame::none()
                    .fill(colors::BG_WHITE)
                    .inner_margin(egui::Margin::symmetric(0.0, 0.0)),
            )
            .show(ctx, |ui| {
                self.show_tabs(ui);
            });

        // 底部面板：Footer
        egui::TopBottomPanel::bottom("footer_panel")
            .frame(
                egui::Frame::none()
                    .fill(colors::BG_LIGHT)
                    .inner_margin(egui::Margin::symmetric(16.0, 6.0)),
            )
            .show(ctx, |ui| {
                self.show_footer(ui, is_running);
            });

        // 中央面板：Tab 内容
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(colors::BG_WHITE)
                    .inner_margin(egui::Margin::same(16.0)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                match self.active_tab {
                    AppTab::Connection => self.show_connection_tab(ui),
                    AppTab::Settings => self.show_settings_tab(ui, is_running),
                    AppTab::Log => self.show_log_tab(ui),
                }
            });

        ctx.request_repaint();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.server_status == ServerStatus::Running {
            self.stop_server();
        }
    }
}

/// v3.4：把手机上报的能力 JSON 排成人类可读的短行：
///   back  ·  1280×720@30 · 960×540@30 …
/// 本 crate 不依赖 serde_json，这里只做轻量数字扫描：
/// 每个 "facing" 分段里、"sizes" 之后的数字恰好是 width/height/maxFps 三元组
/// （手机侧 jsonEncode 紧凑无空格，键名不含数字，扫描是安全的）。
fn wrap_json_brief(caps: &str) -> String {
    let mut out = String::new();
    for seg in caps.split(r#""facing""#).skip(1) {
        // 镜头名：'facing': 后第一个引号里的字符串
        let facing = seg
            .split(':')
            .nth(1)
            .and_then(|s| s.trim_start().split('"').nth(1))
            .unwrap_or("?");
        // sizes 区段内的数字即档位
        let body = seg.split(r#""sizes""#).nth(1).unwrap_or("");
        let chars: Vec<char> = body.chars().collect();
        let mut idx = 0usize;
        let mut parts: Vec<String> = Vec::new();
        while let Some(w) = next_number(&chars, &mut idx) {
            let h = next_number(&chars, &mut idx).unwrap_or(0);
            let f = next_number(&chars, &mut idx).unwrap_or(0);
            parts.push(format!("{}×{}@{}", w, h, f));
            if parts.len() >= 8 {
                break;
            }
        }
        if !parts.is_empty() {
            out.push_str(&format!("{}  ·  {}\n", facing, parts.join("  ·  ")));
        }
    }
    if out.is_empty() {
        out = caps.to_string(); // 解析不了就原样显示，至少信息不丢
    }
    out.trim_end().to_string()
}

/// 从 *idx 起读下一个数字（跳过非数字），游标推进到该数字之后
fn next_number(chars: &[char], idx: &mut usize) -> Option<u32> {
    while *idx < chars.len() && !chars[*idx].is_ascii_digit() {
        *idx += 1;
    }
    if *idx >= chars.len() {
        return None;
    }
    let start = *idx;
    while *idx < chars.len() && chars[*idx].is_ascii_digit() {
        *idx += 1;
    }
    chars[start..*idx].iter().collect::<String>().parse().ok()
}

/// v3.4：把手机上行的一帧 JPEG 解码成 egui 纹理（GUI 预览用）。
/// jpeg_decoder 默认输出 RGB24，与 ColorImage::from_rgb 的期望一致。
/// 解码失败（坏帧/网络截断）返回 None，界面保留上一帧不闪烁。
fn decode_jpeg_to_texture(ctx: &egui::Context, jpeg: &[u8]) -> Option<egui::TextureHandle> {
    let mut decoder = jpeg_decoder::Decoder::new(jpeg);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let image =
        egui::ColorImage::from_rgb([info.width as usize, info.height as usize], &pixels);
    Some(ctx.load_texture("cam_frame", image, egui::TextureOptions::LINEAR))
}

fn chrono_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() % 86400;
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}
