// ── 不弹那个黑色控制台窗口 ──────────────────────────────────────────────────
// Rust 的 bin 默认编译成 Windows 的"控制台子系统"（PE 头 SUBSYSTEM=Console），
// 于是双击 exe 时系统会额外分配一个黑色控制台，把 stderr 的日志刷在上面。
// eframe 只画它自己的窗口，不会替我们隐藏控制台 —— 这就是那个黑窗口的来历，
// 它不是调试版才有的东西，正式包同样会弹。
//
// 声明成 windows 子系统后进程一开始就没有控制台。日志一条都不少：
//   · 完整落在 exe 同目录的 audioserver.log（DualLogger 本来就在写文件）；
//   · 窗口里的"日志"页照常看。
// 想要实时滚动输出：PCSPEAKER_CONSOLE=1 启动（自己开一个控制台），
// 或 PCSPEAKER_CONSOLE=attach（从 cmd/PowerShell 里启动时挂到父控制台）。
#![cfg_attr(windows, windows_subsystem = "windows")]

// ── 初学者 3 分钟总览：这个界面是怎么画出来的 ──────────────────────────────
// 整个文件是一套 egui 程序。egui 是"即时模式"（immediate mode）GUI 框架，
// 和常见的"保留模式"（Win32 控件、Qt、WPF 那一类）根本区别在于：
//   · 保留模式：控件是长期存在的对象，创建一次；数据变了调 setText() 局部刷新；
//     框架只重画"脏了"的区域。
//   · 即时模式：世界上没有"现成的控件"。每一帧都把界面从头描述一遍
//     （本文件的 eframe::App::update() 就是每帧被调用的那段描述代码），
//     egui 照着画一次就丢掉，下一帧再从头来。真正的"状态"只存在你自己的
//     struct 字段里（AudioServerApp），界面每帧都是这些字段的一张"照片"。
// 这解释了为什么本文件的代码都是"一连串 ui.label / ui.add 顺序往下写"——
// 不是在"创建控件"，而是在"每帧重新描述"。eframe 是 egui 官方的窗口 +
// 事件循环外壳；详细原理见文件末尾 `impl eframe::App` 上方那段总览。
use eframe::egui;
use log::{LevelFilter, Log, Metadata, Record};
use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Write;
// std 同步通道：服务线程 → GUI 的"回灌"管线（ServerEvent）。
// GUI 端只用 try_recv 非阻塞轮询（见 poll_server_events 的注释：界面线程绝不能等）。
use std::sync::mpsc;
// OnceLock = "只能写一次、之后反复读"的全局变量（首次用到时才初始化，线程安全）。
// 本文件用它存日志文件路径和句柄；config.rs 用的是 OnceLock<Mutex<Config>> ——
// 那种"多线程共享 + 每次读写加锁"的常规组合就是 Arc<Mutex<T>>/Mutex 家族，
// 区别只是配置全局只有一份、不需要 Arc 引用计数。
use std::sync::OnceLock;
use std::time::Instant;
// tokio 异步通道：GUI → 服务线程的"下命令"管线（ServerCommand）。
// 和上面 std 版的分工：服务端在 async 运行时里用 .await 收命令，
// 换成 std 通道会在队列空时把执行器卡死，所以异步侧必须用 tokio 的这套。
// 一进一出两条通道 = GUI 线程和后台服务之间全部的沟通方式，双方不共享可变数据。
use tokio::sync::mpsc as tokio_mpsc;

use audioserver::config;
use audioserver::env_check::{self, EnvGuide, GuideAction};
use audioserver::lang;
use audioserver::server::{run_server, ServerCommand, ServerConfig, ServerEvent, ServerStatus};

// ── 双输出日志（stderr + 文件）──
// env_logger 只能写 stderr，这里自定义 logger 同时写文件，
// 方便排查问题时回溯完整日志。
struct DualLogger {
    log_file: OnceLock<std::fs::File>,
}

thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(512));
}

/// 日志文件实际落在哪（第一次用到时定下来，之后不再变）
static LOG_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

/// 选日志文件路径。
///
/// 规则：能写就写在 exe 旁边（绿色版 / 开发时的老习惯，README 里也是这么写的），
/// 写不动就退回配置目录 %APPDATA%\PCAssistant。
/// 为什么必须有退路：安装版把 exe 放进 Program Files，普通用户权限在那里**建不了文件**，
/// 原来的代码会静默失败——出问题时用户手里没有日志，我们远程也问不到现场。
fn log_file_path() -> std::path::PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(dir) = exe_dir {
        let candidate = dir.join("audioserver.log");
        // 试探性打开一次（create+append 等价于"能不能在这儿写"），成功就定在这儿
        if OpenOptions::new()
            .create(true)
            .append(true)
            .open(&candidate)
            .is_ok()
        {
            return candidate;
        }
    }
    // 退路：配置目录（config::init() 已经保证它存在；拿不到目录就写当前目录）
    config::config_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("audioserver.log")
}

/// 给界面 / 日志用的当前日志文件路径
fn log_path() -> std::path::PathBuf {
    LOG_PATH.get_or_init(log_file_path).clone()
}

/// 当前日志级别，用原子量存着 —— 设置页里改完下一行日志就按新级别过滤，不用重启。
///
/// 为什么不用 DualLogger.level 那个字段：logger 一旦交给 `set_boxed_logger` 就再也
/// 拿不到 `&mut`（它是全局单例、被 Arc 住），而日志宏在别的线程随时会读。
/// 原子量是"改得动 + 不加锁"的最小组合；LevelFilter 的枚举顺序本身就是
/// Off < Error < Warn < Info < Debug < Trace，所以直接比大小就等价于比详细程度。
static LOG_LEVEL: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(LevelFilter::Info as usize);

/// 把一个新的日志级别同时应用到两处：我们自己的过滤（LOG_LEVEL）和 log crate 的
/// 全局 max_level（其它中间件读的是后者）。少调一处就会出现"界面显示已改成 debug，
/// 日志却还是 info"这种最难查的假象。
fn apply_log_level(level: LevelFilter) {
    // Ordering::Relaxed 是原子操作附带的"内存序"标记，初学者可以这样记：
    // 它规定这条读/写和其它数据之间要不要保持先后可见关系。日志级别只是
    // "下一行日志按新值过滤"，不依赖任何配套数据，所以用最省 Relaxed。
    // （常见档位：SeqCst 最严格通用、Acquire/Release 用于无锁结构成对同步。）
    use std::sync::atomic::Ordering::Relaxed;
    LOG_LEVEL.store(level as usize, Relaxed);
    log::set_max_level(level);
    // 这句用 info 级别写：就算刚被调成 error，改级别这个动作本身也值得留痕。
    // （error 级别下这行会被过滤掉 —— 可接受，真要排查时先调高再调低。）
    log::info!("[Config] log level = {}", level.as_str());
}

/// 配置文件里存的是字符串（"info"/"debug"…），解析不出来一律退回 info：
/// 宁可少打日志，也不要因为一个拼错的单词把日志全关掉。
fn parse_log_level(s: &str) -> LevelFilter {
    s.parse().unwrap_or(LevelFilter::Info)
}

impl Log for DualLogger {
    fn enabled(&self, meta: &Metadata) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        meta.level() as usize <= LOG_LEVEL.load(Relaxed)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // 格式化只拼一份：先 write! 进线程本地 BUF（thread_local + RefCell<Vec<u8>>，
        // 每根线程一个、预留 512 字节免去小日志的反复扩容；Vec 当字符串缓冲区用，
        // 比 String 好在可以 clear 后原地复用容量），然后同一块内存喂给两路输出。
        // log::Record 没有现成的"一行文本"，级别/目标/文件名/正文要自己拼。
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
                if let Ok(f) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_path())
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

    // log crate 要求的"强制刷盘"接口。我们两路输出都是 write_all 直接落（stderr
    // 行缓冲、文件靠 OS 页缓存），没有自己的缓冲池，所以这里合法地什么都不用做。
    fn flush(&self) {}
}

fn init_logger() {
    // 级别优先级：RUST_LOG（临时排障，改起来最快）> config.json 的 diagnostics.log_level
    // > info。RUST_LOG 认不认得出由 std::str::FromStr 决定，认不出就往下走配置。
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| Some(parse_log_level(&config::get().diagnostics.log_level)))
        .unwrap_or(LevelFilter::Info);
    // 先装 logger 再定级别：apply_log_level 自己会打一行"级别是多少"，
    // 顺序反了那行就被丢进空气里（还没有接收方）。
    let logger = Box::new(DualLogger {
        log_file: OnceLock::new(),
    });
    log::set_boxed_logger(logger).ok();
    apply_log_level(level);
}

/// 把"配置从哪来、有没有新建、有没有被夹紧"写进日志。
/// 这三件事必须留痕：用户报"我改了没生效"时，第一句要问的就是他改的是哪份文件。
fn log_config(loaded: &config::Loaded) {
    match &loaded.path {
        Some(p) if loaded.created => log::info!("[Config] created default config: {}", p.display()),
        Some(p) => log::info!("[Config] loaded {}", p.display()),
        None => log::warn!("[Config] no config dir (APPDATA missing) —— running on built-in defaults"),
    }
    if let Some(e) = &loaded.error {
        // 文件在但读不动/解析失败：按默认值跑，同时把原因摊开，
        // 不然用户只会觉得"程序把我的配置吞了"
        log::error!("[Config] invalid config file, fell back to defaults: {e}");
    }
    for w in &loaded.warnings {
        log::warn!("[Config] {w}");
    }
}

/// 只有在用户明确要求时才创建控制台：环境变量 PCSPEAKER_CONSOLE 或
/// config.json 的 diagnostics.show_console（配合文件头的 `windows_subsystem = "windows"`）。
/// 必须在 init_logger 之前调用：Rust 的标准输出句柄是"第一次用到时才缓存"，
/// 先建控制台、后写日志，stderr 才会指向新控制台；反过来就永远是个空句柄。
#[cfg(windows)]
fn maybe_attach_console(config_wants_console: bool) {
    let mode = std::env::var("PCSPEAKER_CONSOLE").ok();
    if mode.is_none() && !config_wants_console {
        return;
    }
    use windows::Win32::System::Console::{
        AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS,
    };
    unsafe {
        // attach = 从 cmd/PowerShell 启动时接回调用方的控制台；
        // 其它任何值（含"只有配置要求开"的情况）= 自己开一个新控制台窗口
        if mode.is_some_and(|m| m.eq_ignore_ascii_case("attach")) {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        } else {
            let _ = AllocConsole();
        }
    }
}

#[cfg(not(windows))]
fn maybe_attach_console(_config_wants_console: bool) {}

/// 把 panic 写进日志。没有控制台窗口之后这一步是必需的：
/// 后台线程（采集/注入/推流）万一 panic，程序不会崩给你看，只是那条线悄悄断了，
/// 现场什么痕迹都没有 —— 现在它会进 audioserver.log，界面的"日志"页也能看到。
fn install_panic_logger() {
    // 防递归：万一 panic 发生在日志本身的处理里，钩子里再调 log::error! 会无限套娃
    static IN_HOOK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    std::panic::set_hook(Box::new(|info| {
        if IN_HOOK.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        log::error!("[Panic] {info}");
        eprintln!("[Panic] {info}");
        IN_HOOK.store(false, std::sync::atomic::Ordering::Relaxed);
    }));
}

// ── v2 浅色主题配色 ──
// 想改整套观感，只该动这个模块里的常量：界面各处画笔几乎全部引用这里的
// 名字（少数状态芯片例外，是内联的 from_rgb）。换主色调 → ACCENT 三件套；
// 卡片/窗口底色 → BG_WHITE / BG_LIGHT；文字深浅层级 → TEXT_PRIMARY 到
// TEXT_DISABLED 四档；输入框可见性 → INPUT_ 开头的五个（见下方说明）。
// Color32::from_rgb(r, g, b)：三个参数各 0~255 的红绿蓝强度，
// (255,255,255)=纯白、(0,0,0)=黑、(37,99,235)=本主题的蓝（tailwind blue-600）。
// 另一套 from_rgba_premultiplied 多一个 alpha 通道（0 全透明 ~ 255 不透明）。
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

    // ── 输入控件（文本框 / 下拉框）──
    // 为什么单独一组：卡片是纯白的，而 egui 默认给输入框的底色是 extreme_bg_color
    // （浅色主题里几乎是白），边框又走 noninteractive/inactive 那套淡灰 ——
    // 结果就是"输入框和背景糊成一片，看不出哪里能改"（2026-10-01 用户反馈原话）。
    // 所以底色要压到肉眼可辨的浅灰蓝，边框给到 slate-400 这个强度。
    pub const INPUT_BG: Color32 = Color32::from_rgb(241, 245, 249);
    pub const INPUT_BG_HOVER: Color32 = Color32::from_rgb(226, 232, 240);
    pub const INPUT_BORDER: Color32 = Color32::from_rgb(148, 163, 184);
    pub const INPUT_BORDER_HOVER: Color32 = Color32::from_rgb(100, 116, 139);
    /// 服务运行中字段是禁用的：不能像 egui 默认那样淡到看不见，
    /// 用户得能读出"现在生效的是哪个值"，只是改不动。
    pub const INPUT_BG_DISABLED: Color32 = Color32::from_rgb(243, 244, 246);
}

/// 下拉框的一项：`label` 给人看，`value` 是写进 config.json 的那串字。
///
/// 为什么 value 存字符串而不是直接存数字：设置页的落盘逻辑（persist_settings）
/// 本来就按"界面上的字符串 → 解析 → 写进 Config 字段"这条路走，
/// 下拉复用同一条路就够，不用为它单开一套写入通道。
#[derive(Clone)]
struct ComboOption {
    label: String,
    value: String,
}

/// 用一串数字值生成下拉选项，label = 值 + 后缀（如 "48000 Hz"）。
fn number_options(values: &[&str], suffix: &str) -> Vec<ComboOption> {
    values
        .iter()
        .map(|v| ComboOption { label: format!("{v}{suffix}"), value: v.to_string() })
        .collect()
}

/// 列出本机音频设备的友好名，分成（播放设备, 录音设备）两摞。
///
/// 为什么要列：这三项设备选择以前只能手改 config.json 里的字符串，
/// 拼错一个字母就静默退回默认设备，排查半天。做成下拉就点不错了。
/// 用 cpal 而不是自己写 WASAPI 枚举：cpal 本来就是依赖（Mac 侧在用），
/// 同一份代码在 Mac 上也能列出 CoreAudio 设备，不用加 #[cfg] 分支。
/// 失败（无声卡、驱动异常）就返回空表 —— 下拉里只剩"自定义/系统默认"，
/// 界面照常能用，绝不因为枚举失败而让程序起不来。
fn audio_devices() -> (Vec<String>, Vec<String>) {
    // trait 不 import 进来，host.output_devices() / d.name() 就"找不到方法"
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    let mut outputs = Vec::new();
    let mut inputs = Vec::new();

    for (label, list, sink) in [
        ("output", host.output_devices(), &mut outputs),
        ("input", host.input_devices(), &mut inputs),
    ] {
        match list {
            Ok(devs) => {
                for d in devs {
                    let name = d.name().unwrap_or_default();
                    if !name.is_empty() {
                        sink.push(name);
                    }
                }
            }
            Err(e) => log::warn!("[Gui] enumerate {label} devices failed: {e}"),
        }
    }
    (outputs, inputs)
}

/// 把界面上那串 "960x720" 拆成 (宽, 高)。
/// 分隔符同时认 x / X / ×：下拉里给的是小写 x，但用户手改 config.json 时
/// 可能照着界面上"960×720"那种排版写，别为难他。
/// 任一维不是数字、或数字为 0 → None（调用方保留旧值不动）。
fn parse_resolution(text: &str) -> Option<(u32, u32)> {
    let norm = text.replace('×', "x").replace('X', "x");
    let (w, h) = norm.split_once('x')?;
    let w = w.trim().parse::<u32>().ok()?;
    let h = h.trim().parse::<u32>().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

fn main() -> eframe::Result<()> {
    // 顺序不能乱：配置要第一个读，因为"要不要开控制台"和"日志级别"都由它决定，
    // 而这两个东西一旦开始输出日志就再也改不回来了。
    let loaded = config::init();
    maybe_attach_console(loaded.config.diagnostics.show_console);
    init_logger();
    install_panic_logger();
    log::info!("[Main] AudioServer starting (dual logger: stderr + audioserver.log)");
    log_config(&loaded);
    // 日志写在哪必须在第一屏就说清楚：绿色版在 exe 旁边，装进 Program Files 会退到
    // %APPDATA%\PCAssistant。出问题时第一条要问的就是这个路径。
    log::info!("[Main] log file: {}", log_path().display());

    // i18n：先决定界面语言，再画任何一帧。
    // 优先级 = PCSPEAKER_LANG 环境变量 > config.json 的 language > 系统显示语言。
    // 放在这里（而不是 App::new 里）是因为窗口标题在 run_native 就要用到文案。
    let (locale, locale_from) = lang::apply_startup_locale();
    log::info!("[Main] UI language = {locale} (from {locale_from})");

    // NativeOptions：建窗参数。inner_size [420.0, 540.0] 单位是"逻辑像素"
    // （不是屏幕物理像素，高 DPI 屏上 egui/winit 会按缩放比例自动放大，
    // 所以 420 在 2x 屏上实际占 840 个物理像素，界面元素也同比例放大）。
    // 这个尺寸是按"竖着一列卡片"的原型调出来的；resizable(true) 允许用户拖大，
    // 但设置页卡片很长，默认高度装不下 → 那一页自己套了滚动区（见 update 注释）。
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 540.0])
            .with_resizable(true),
        ..Default::default()
    };

    // 英文 420px 宽够用，但中文标题更短、按钮更长，宽度先不随语言变（下一版再看）
    let window_title = lang::t("app.title");

    // run_native 是"点火钥匙"：从这一行起创建窗口、进入 winit 操作系统事件循环，
    // **它不返回，直到窗口关闭才带结果回来** —— main() 的执行顺序到此为止，
    // 后面的错误日志要等退出才跑。参数里的 Box::new(|cc| ...) 是 App 工厂闭包：
    // 启动时只调一次（cc = 创建上下文，带 egui_ctx），负责"一次性初始化"：
    // 装中文字体、设主题 visuals、构造 AudioServerApp::new()；
    // 之后事件循环每帧调用它的 update()。"new 一次 / update 每帧"就是
    // App trait 的分工，也是即时模式"状态在我这、界面每帧重画"的落地形态。
    let result = eframe::run_native(
        &window_title,
        options,
        Box::new(|cc| {
            // 中文字体兜底（必须在设 visuals / 建界面之前）
            // 为什么强调顺序：egui 在排版每一段文字时会按 FontDefinitions 找字形，
            // 字体表必须在第一帧画文字前就位；而 set_fonts 和 set_visuals 互不依赖，
            // 放一起纯粹是"一次把全局观感配完"的写法。
            install_cjk_fonts(&cc.egui_ctx);

            // 基于 egui 浅色主题，只覆盖需要的颜色
            // Visuals::light() 是 egui 自带的一整套浅色配色"底版"；下面全是挑着改
            // ——调色第二层入口在这里（第一层是 colors 模块的色板常量）：
            // 那层定义"有哪些颜色"，这层决定"哪个部位用哪个颜色"。
            // override_text_color = 全局默认文字色；widgets.* 按交互状态分组：
            // noninteractive（不可交互）/ inactive（可交互未动）/ hovered（悬停）/
            // active（按下/展开中）——update() 每帧按控件当前状态查这张表取色。
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

            // ── 输入控件的可见性（v3.8.1）──
            // TextEdit 的底色只认 extreme_bg_color（egui 写死的，见 widgets/text_edit/builder.rs），
            // 边框认"当前交互状态"的 bg_stroke；所以这两处一改，所有文本框一起变。
            // 下拉框（ComboBox）不一样：它用 weak_bg_fill，而那一项同时也是弹出菜单
            // 每一行的底色 —— 在这里全局改会把整个菜单染成一片灰。所以下拉框的底色
            // 由 config_dropdown 在自己的小范围 style 里改（见 Ui::scope 那段注释）。
            visuals.extreme_bg_color = colors::INPUT_BG;
            visuals.widgets.inactive.bg_stroke =
                egui::Stroke::new(1.0_f32, colors::INPUT_BORDER);
            visuals.widgets.hovered.bg_stroke =
                egui::Stroke::new(1.0_f32, colors::INPUT_BORDER_HOVER);
            // 悬停/菜单行的高亮底色：weak_bg_fill 是"可选底色"，下拉框和弹出菜单的每一行
            // 都读它。给一档比白卡片深的浅灰，鼠标划过才看得见"是哪一行"。
            visuals.widgets.hovered.weak_bg_fill = colors::INPUT_BG_HOVER;
            visuals.widgets.active.bg_stroke = egui::Stroke::new(1.5_f32, colors::ACCENT);
            // 禁用态走的是 noninteractive（add_enabled_ui(false) 会改变 sense），
            // 边框留一条可辨的浅灰，别让它消失成纯文本。
            visuals.widgets.noninteractive.bg_stroke =
                egui::Stroke::new(1.0_f32, colors::BORDER);
            // 聚焦描边：文本框获得焦点时 egui 用 selection.stroke 画框，加粗一点才看得出"我在改哪一格"
            visuals.selection.stroke = egui::Stroke::new(1.5_f32, colors::ACCENT);
            cc.egui_ctx.set_visuals(visuals);

            let app = AudioServerApp::new();
            // 门禁生效时标题栏也换成"需要准备驱动"，任务栏上一眼可辨
            // send_viewport_cmd = egui 里"和窗口本身说话"的通道（改标题、关窗口、
            // 改尺寸都走它），和画在客户区里的 ui 是两套出口，跨帧生效、不阻塞。
            if let Some(g) = app.env_gate.as_ref() {
                cc.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Title(g.viewport_title()));
            }
            Ok(Box::new(app))
        }),
    );
    if let Err(e) = &result {
        // 黑窗口没了，"双击没反应"这种失败必须自己留痕：
        // 起不来通常是显卡/驱动或窗口系统的问题，日志里要能看到原因。
        log::error!("[Main] GUI failed to start: {e:?}");
    }
    result
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
            log::warn!("[Main] {path} does not look like a font file, skipping");
            continue;
        }
        // egui 字体系统速览：FontDefinitions 是"字体族 → 字体清单"的总表，
        // FontData::from_owned(字节) 把整个字体文件读进内存登记造册；
        // families 里的 Proportional/Monospace 两条链按顺序找字形，内置字体
        // 画不出来的字（汉字）才轮到 push 在链尾的系统字体顶上——这就是"兜底"。
        // ctx.set_fonts 一次性替换全局字体表，所以必须赶在第一帧排版之前完成。
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
    log::warn!("[Main] no usable CJK font found in the system, Chinese UI text will render as boxes");
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
    // ── 与后台服务线程配合的三件套（即时模式下"线程怎么和界面说话"的答案）──
    // · server_handle：服务 OS 线程的 JoinHandle（线程身份凭据）。本程序退出时
    //   只发 Stop 命令让线程自行收尾、不 wait 等它（join 会把 GUI 卡住），所以只是拿着。
    // · cmd_tx：GUI→服务 的命令发送端（tokio 无界通道，send 不阻塞；"无界"的代价
    //   是没有背压，服务消费不过来只会排队 —— 换来的是界面永不被命令拖住）。
    // · event_rx：服务→GUI 的事件接收端（std 通道，update() 里 try_recv 轮询取走）。
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
    // ── v3.6 国际化 ──
    /// 上一次切换语言时保存失败的原文（界面显示，成功则清空）
    lang_error: Option<String>,
    // ── v3.8 配置文件 ──
    /// 配置文件路径的显示文本（在 new() 里算一次）。
    /// 为什么不每帧现取：设置页每帧都要画这一行，历史事故是"每帧读一次文件 + 打一行日志"
    /// 把 audioserver.log 灌了三万多次；这里连字符串拼接都提前做掉。
    config_path_text: Option<String>,
    /// 设置页里"改过但还没落盘"的输入框（等到输入框失去焦点那一下才写 config.json，
    /// 见 persist_settings）。(字段名, 这一项是否已改完)：字段名进日志，
    /// "改完"决定落盘时机。
    settings_dirty: Vec<(&'static str, bool)>,
    // ── v3.8.2：设置页下拉化的其余配置项（初值同样来自 config.json）──
    /// 虚拟摄像头输出分辨率，界面上是"宽x高"一整串（落盘时拆成 camera.width/height）
    cam_resolution: String,
    cam_fps: String,
    /// 两条虚拟摄像头通道各自的开关（关掉 = 不注册那个设备，见 server.rs 的 *_enabled 判断）
    cam_unity: bool,
    cam_obs: bool,
    /// 日志级别（这一项是设置页里唯一改完立刻生效的，见 apply_log_level）
    log_level: String,
    /// 启动时是否额外开一个控制台窗口刷日志（要重启才生效：控制台必须在任何日志
    /// 写出去之前建好，Rust 的 stdout 句柄是"第一次用到就缓存"的）
    show_console: bool,
    /// 上行音频落进 CABLE 时用的采样率（要和虚拟声卡的混音格式一致，不然会重采样）
    mic_uplink_rate: String,
    /// 上行抖动缓冲上限：网络抖动超过这个时长就丢帧保实时，不再往后堆延迟
    mic_max_queue_ms: String,
    // ── v3.8.2：音频设备下拉（以前只能手改 config.json 里的字符串）──
    /// 音箱模式回环捕获用的播放设备名（空 = 系统默认）
    capture_device: String,
    /// 麦克风模式上行注入用的播放设备名（默认匹配 CABLE Input）
    inject_device: String,
    /// "有没有应用正在用这根线"要监听的录音设备名
    monitor_device: String,
    /// 启动时枚举一次的全部播放设备（下拉列表的数据源）
    output_devices: Vec<String>,
    /// 启动时枚举一次的全部录音设备
    input_devices: Vec<String>,
}

impl AudioServerApp {
    fn new() -> Self {
        // 界面上那几个输入框的初值来自 config.json（不是硬编码），
        // 这样"改配置文件 → 看界面 → 点开关"三处看到的永远是同一个数。
        // config::get() 只是读内存里那份全局配置（main 开头已经 init 过），不碰磁盘。
        // 顺带把 config.json 的机制说清（详见 config.rs）：Config 及各小节 struct 都
        // derive(Serialize, Deserialize)，serde 自动生成交互代码——读=JSON 文本变结构体，
        // 写=结构体变 JSON 文本（to_string_pretty 带缩进，用户可用记事本直接改）。
        // 文件在 %APPDATA%\PCAssistant\config.json，第一次运行自动生成默认版；
        // 再老的版本用 exe 同目录 settings.txt（只有 language 一行），升级首启会被
        // 搬进 config.json 一次。任何来源的值都要过 sanitize()：越界"夹紧"回合法区间
        // （端口 0→8080、声道 5→2、缓冲区 100 万帧→8192…），每条夹紧都会以
        // [Config] warning 写进日志留痕，界面上看到的就是实际生效的那个数。
        let cfg = config::get();
        let mut app = Self {
            port: cfg.network.port.to_string(),
            sample_rate: cfg.speaker.sample_rate.to_string(),
            channels: cfg.speaker.channels.to_string(),
            buffer_size: cfg.speaker.buffer_frames.to_string(),
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
            pause_speaker_live: cfg.speaker.pause_while_mic_live,
            cam_live: false,
            cam_session: false,
            cam_source: None,
            cam_stats: None,
            cam_caps: None,
            cam_texture: None,
            env_gate: None,
            lang_error: None,
            config_path_text: config::config_path().map(|p| p.display().to_string()),
            settings_dirty: Vec::new(),
            cam_resolution: format!("{}x{}", cfg.camera.width, cfg.camera.height),
            cam_fps: cfg.camera.fps.to_string(),
            cam_unity: cfg.camera.unity_enabled,
            cam_obs: cfg.camera.obs_enabled,
            log_level: cfg.diagnostics.log_level.clone(),
            show_console: cfg.diagnostics.show_console,
            mic_uplink_rate: cfg.mic.uplink_sample_rate.to_string(),
            mic_max_queue_ms: cfg.mic.max_queue_ms.to_string(),
            capture_device: cfg.speaker.capture_device_hint.clone(),
            inject_device: cfg.mic.inject_device_hint.clone(),
            monitor_device: cfg.mic.monitor_capture_hint.clone(),
            // 只在启动时枚举一次：cpal 每次调用都要过一遍 WASAPI 的 IMMDeviceEnumerator，
            // 放在 update() 里就是每帧几十毫秒的系统调用。设备插拔了用旁边的"重新检测"。
            output_devices: Vec::new(),
            input_devices: Vec::new(),
        };
        app.refresh_audio_devices();
        // v3.5：启动先做一次【只读】环境自检（不写注册表、不装任何东西）。
        //   · 必需驱动齐 → 和以前一样，自动开启服务端
        //   · 缺驱动     → 不起服务端线程，主窗口只显示"环境准备"向导页，
        //                  用户装完点【重新检测】才会真正进入主界面
        // 开发调试（Mac 移植期 / 想在缺驱动的机器上看 UI）可跳过：
        //   环境变量 PCSPEAKER_SKIP_ENV_CHECK=1，或 config.json 里
        //   diagnostics.skip_env_check = true。两条路都留着：环境变量适合临时/脚本，
        //   配置项适合"这台机器我就想常开着看界面"。
        let report = env_check::detect();
        let skip_gate = std::env::var("PCSPEAKER_SKIP_ENV_CHECK").is_ok()
            || config::get().diagnostics.skip_env_check;
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
                log::warn!("[Main] 跳过环境门禁（PCSPEAKER_SKIP_ENV_CHECK 或 diagnostics.skip_env_check）—— 环境未齐，仍强行进入主界面");
            }
            app.start_server();
        } else {
            log::info!("[Main] 环境门禁生效：服务端未启动，等待用户安装驱动");
            app.env_gate = Some(EnvGuide::new(report));
        }
        // ── 开发/验收用的界面定位（普通用户不会设置这两个变量，行为完全不变）──
        // 为什么要有：国际化验收要拍"每种语言 × 每个模式 × 每个页签"的截图，
        // 而脚本模拟鼠标点击太脆弱。用环境变量直接指定首屏落在哪，拍出来就是哪。
        //   PCSPEAKER_DEMO_MODE = speaker | mic | camera
        //   PCSPEAKER_DEMO_TAB  = connection | settings | log
        if let Ok(m) = std::env::var("PCSPEAKER_DEMO_MODE") {
            app.mode = match m.to_lowercase().as_str() {
                "mic" => AppMode::Mic,
                "camera" => AppMode::Camera,
                _ => AppMode::Speaker,
            };
            log::info!("[Main] demo override: mode = {m}");
        }
        if let Ok(t) = std::env::var("PCSPEAKER_DEMO_TAB") {
            app.active_tab = match t.to_lowercase().as_str() {
                "settings" => AppTab::Settings,
                "log" => AppTab::Log,
                _ => AppTab::Connection,
            };
            log::info!("[Main] demo override: tab = {t}");
        }
        app
    }

    /// 取走服务端事件并落到界面状态上。
    ///
    /// 返回值 = "这一帧确实取到了东西"。v3.7 CPU 优化专用：
    /// 调用方据此决定要不要立刻再重绘一次（数据驱动重绘），
    /// 而不是像以前那样无条件满帧重绘。
    fn poll_server_events(&mut self, ctx: &egui::Context) -> bool {
        let mut events = Vec::new();
        if let Some(rx) = &self.event_rx {
            // 铁律：GUI 线程不能阻塞、不能长任务。即时模式的 update() 一帧不返回，
            // 窗口就一帧不重绘、不响应鼠标 —— Windows 会给标题挂"未响应"。
            // 所以这里用 try_recv（队列空立刻返回 Empty 就 break），绝不用会干等的 recv()。
            // 每帧最多取 50 条：日志风暴时一帧也只消化这么多，剩余的下一帧再取；
            // 只要这一帧取到过东西，update() 末尾就会 request_repaint()，不会有延迟堆积。
            for _ in 0..50 {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    // Disconnected = 发送端（服务线程）已经消失：要么服务自己退出了，
                    // 要么 run_server 返回/崩了。这里把它翻译成"停止运行"的界面状态，
                    // 并把两条通道句柄清掉（cmd_tx=None 后 send_cmd 自动变空操作）。
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.server_status = ServerStatus::Stopped;
                        self.cmd_tx = None;
                        self.event_rx = None;
                        self.start_time = None;
                        self.add_log("Server process ended".to_string());
                        // 状态变了 → 界面必须重画，返回 true
                        return true;
                    }
                }
            }
        }
        let got_any = !events.is_empty();
        for event in events {
            self.handle_event(event, ctx);
        }
        got_any
    }

    // 事件回灌界面的唯一入口：每个 ServerEvent 变体在这里翻译成"改掉某个 self 字段"，
    // 全程不画任何东西——下一帧 update() 重新描述界面时自然读到新值。
    // 即时模式完整数据闭环：后台线程 send 事件 → GUI try_recv → 改字段 → 请求重绘。
    // ctx 参数只为 CamFrame 解码上传纹理用（load_texture 要摸渲染上下文）。
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
                // 展开说：mic_bars 是定长 34 的数组，当"没有环的环形缓冲"用 ——
                // rotate_left(1) 把最老的值甩到队尾，再往最后一格写最新值，
                // 画出来就是"左旧右新"的滚动波形。34 这个数字 = 界面电平条最多
                // 画 34 根（show_level_bars 按可用宽度取尾部若干根）。
                // ×3.0 是感知增益：静音环境原始电平太小，曲线贴着底看不出动静；
                // .min(1.0) 把它截到 100%，爆表也不会画出去。
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
    // 实现细节：unbounded 通道 send 永不阻塞、只可能因对方已退出而报错；
    // .ok() 是【故意】把 Err 丢掉——服务都停了，命令没人收正是预期行为，
    // 不该为它在界面线程上做任何错误处理。
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

    // ── v3.8：设置页 ↔ config.json ─────────────────────────────────────────
    // 为什么自动写盘、界面上不放【保存】按钮：安装版的原则是"配置文件说了算，
    // 界面上改了不丢"。加按钮就要多一套文案、多一种"改了但没保存"的困惑状态。
    // 什么时候落盘：输入框【失去焦点】或按回车的那一下（见 flush_settings_if_ready），
    // 不是每敲一个字符写一次盘 —— 那是每按一键一次磁盘 IO + 一次整份序列化。
    // 落到哪：%APPDATA%\PCAssistant\config.json（见 config.rs 顶部说明）。

    /// 画一个"接到配置上的输入框"。改过的字段先记进 `dirty`，
    /// 真正写文件的时机由 flush_settings_if_ready 统一判断。
    /// dirty 里那个 bool = "这一项已经改完了"（失去焦点或按了回车），
    /// 只要有一项改完，就把这一批一起落盘。
    ///
    /// `numeric = true`：这一格只该填数字（现在只有端口用到）。
    fn config_text_edit(
        ui: &mut egui::Ui,
        value: &mut String,
        key: &'static str,
        dirty: &mut Vec<(&'static str, bool)>,
        numeric: bool,
    ) {
        // 只允许数字：egui 0.29 没有 TextEdit::numeric()（那是 0.31 才加的），
        // 所以走"事后清理"：这一帧文本真变了就把非数字字符剔掉。
        // 效果上和事前拦截一样（字母根本留不下来），代价是粘贴一整串脏字符时
        // 光标会跳到末尾 —— 端口一共五位数，可以接受。
        let edit = egui::TextEdit::singleline(value)
            .desired_width(88.0)
            // 内边距：数字贴着边框会显得"不像个框"，留 6x4 才像输入控件
            .margin(egui::Margin::symmetric(6.0, 4.0))
            // 长度也顺手限住：端口最长 5 位（65535），第六位压根敲不进去，
            // 比"让你敲满 8 位再告诉你解析失败"友好。
            // 以后要是有更多位数/别的 numeric 字段，把这个上限提成参数。
            .char_limit(if numeric { 5 } else { usize::MAX })
            .horizontal_align(egui::Align::RIGHT);
        // 为什么两种状态都要包一层 Ui::scope（看着很多余，其实是被坑出来的）：
        // TextEdit 的边框/底色不是一起画的 —— show() 先往 painter 里塞一个 Shape::Noop 占位，
        // 画完内容再 ui.painter().set(那个下标, 矩形) 把背景填回去。
        // 而 egui::Grid 会重排/搬移单元格里的 shape 列表，占位下标就失效了：
        // 结果【直接 add 的输入框永远不画框】，只剩一串数字贴在白卡片上（2026-10-01 实测：
        // 停止服务后 48000/2/1024 三行像素级纯白，一个框都没有；运行中走了 scope 反而有框）。
        // 包一层 scope 之后，占位下标落在子 ui 自己的列表里，父级怎么搬都不影响它。
        //
        // ⚠️ 取的是 `.inner` 不是 `.response`（2026-10-01 踩到）：
        // `Ui::scope` 返回 InnerResponse，`.response` 是【那层子 ui 自己】的响应
        // （egui 内部走 remember_min_rect，Sense 只有 hover），
        // changed() / lost_focus() 永远是 false —— 表现就是"端口改了死活不落盘"，
        // 而且界面上看着一切正常。真正的输入框响应是闭包的返回值，在 `.inner` 里。
        let resp = ui
            .scope(|ui| {
                // 文本框底色只认 extreme_bg_color（不随交互状态变），所以禁用态在这里换一档：
                // 底色压深 + 下面 noninteractive 的浅边框，一眼能看出"这是当前值，但现在改不动"。
                ui.visuals_mut().extreme_bg_color = if ui.is_enabled() {
                    colors::INPUT_BG
                } else {
                    colors::INPUT_BG_DISABLED
                };
                ui.add(edit)
            })
            .inner;
        if numeric && resp.changed() {
            value.retain(|c| c.is_ascii_digit());
        }
        // changed() 只在"这一帧里文本真的变了"时为真。
        // 为什么不用"整个界面还有没有焦点"来判断改完没有：按 Tab 时焦点会跳到
        // 下一个控件（还是 is_some），那样就永远不落盘了。所以问这个框自己。
        let finished = resp.lost_focus()
            || (resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if resp.changed() || finished {
            match dirty.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 |= finished,
                None => dirty.push((key, finished)),
            }
        }
    }

    /// v3.8.1：下拉框版的配置项 —— 采样率 / 声道数 / 缓冲区大小这类
    /// "合法取值就那么几个"的量，一律做成选择而不是手打（用户 2026-10-01 提的）。
    /// 手打的三个毛病：打错一位要靠夹紧才救得回来、用户不知道能填什么、
    /// 填了个没测过的值等于拿稳定性赌。下拉把"能填什么"直接摊开，选中的天然合法。
    ///
    /// 当前值不在选项里怎么办（用户自己手改过 config.json）：不藏起来 ——
    /// 选择框显示"自定义（当前值）"，并把这一项放在列表最上面，
    /// 否则用户一打开下拉就被迫改掉了他手调的值。
    fn config_dropdown(
        ui: &mut egui::Ui,
        value: &mut String,
        key: &'static str,
        options: &[ComboOption],
        dirty: &mut Vec<(&'static str, bool)>,
    ) {
        Self::dropdown_at_width(ui, value, key, options, dirty, 120.0);
    }

    /// 音频设备下拉：设备名动辄三四十个字符（"Digital Audio (S/PDIF) (High Definition
    /// Audio Device)"），120px 只够看见前几个字，所以这一种占满整行宽。
    /// `default_label` = Some(文字) 时列表第一项是"留空 = 用默认"，文字由调用方给，
    /// 因为两处"默认"的含义不一样：
    ///   · 音箱捕获留空 → 跟着系统默认播放设备走；
    ///   · 麦克风注入留空 → 回到 VB-CABLE 的固定名（mic_out 里是安全默认，
    ///     绝不允许"随便挑一台设备"，否则手机声音会直接灌进真实扬声器变成啸叫）。
    fn device_dropdown(
        ui: &mut egui::Ui,
        value: &mut String,
        key: &'static str,
        devices: &[String],
        default_label: Option<String>,
        dirty: &mut Vec<(&'static str, bool)>,
    ) {
        let mut options: Vec<ComboOption> = Vec::with_capacity(devices.len() + 1);
        let allow_default = default_label.is_some();
        if let Some(label) = default_label {
            options.push(ComboOption {
                label,
                value: String::new(),
            });
        }
        // 配置里存的其实是"名片段"：server / mic_out 匹配设备用的是不区分大小写的
        // contains（"CABLE Input" 能匹上 "CABLE Input (VB-Audio Virtual Cable)"）。
        // 枚举出来的是完整名，如果直接照搬，出厂默认值会显示成莫名其妙的
        // "自定义 CABLE Input"。所以这里把"当前片段恰好匹配到的那一台"的值写成
        // 用户现有的片段：打开下拉它显示为选中项，再点它一次配置字符串原样不动
        // （绝不做静默改写 —— 用户手打的片段可能有他的道理）。
        let needle = value.trim().to_lowercase();
        let mut matched = false;
        for d in devices {
            let hit = !needle.is_empty() && !matched && d.to_lowercase().contains(&needle);
            if hit {
                matched = true;
            }
            options.push(ComboOption {
                label: d.clone(),
                value: if hit { value.trim().to_string() } else { d.clone() },
            });
        }
        // 一台设备都没有（无声卡 / 驱动异常）：列表不能是空的，否则点开一片空白，
        // 用户以为程序坏了。给一行"未检测到"，它对应的值就是当前值 —— 选了也不改动配置。
        if devices.is_empty() && !allow_default {
            options.push(ComboOption {
                label: lang::t("settings.no_devices"),
                value: value.clone(),
            });
        }
        // 减 14 是给右边的下拉箭头留位置；再窄也保底 160px，别让名字缩成一个字
        let width = (ui.available_width() - 14.0).max(160.0);
        Self::dropdown_at_width(ui, value, key, &options, dirty, width);
    }

    fn dropdown_at_width(
        ui: &mut egui::Ui,
        value: &mut String,
        key: &'static str,
        options: &[ComboOption],
        dirty: &mut Vec<(&'static str, bool)>,
        width: f32,
    ) {
        let mut items: Vec<ComboOption> = options.to_vec();
        let mut current = items.iter().position(|o| o.value == *value);
        if current.is_none() {
            items.insert(
                0,
                ComboOption {
                    label: lang::tf("settings.custom_value", &[("value", value)]),
                    value: value.clone(),
                },
            );
            current = Some(0);
        }
        let current = current.unwrap_or(0);
        let selected_label = items[current].label.clone();
        let mut picked = current;

        // 为什么要包一层 scope，两个原因（都是踩出来的）：
        // ① ComboBox 和 TextEdit 一样用"先占位、后回填"的手法画背景，
        //    放进 Grid 会被重排掉（见 config_text_edit 上面那段注释）；
        // ② 下拉框的底色取自 widgets.*.weak_bg_fill，而那一项同时也是
        //    弹出菜单每一行的底色 —— 在全局 visuals 里改会把整张菜单染成一片灰，
        //    所以只在这一小格里改。子 ui 会克隆一份 style，改它不外溢。
        ui.scope(|ui| {
            let enabled = ui.is_enabled();
            {
                let v = ui.visuals_mut();
                if enabled {
                    v.widgets.inactive.weak_bg_fill = colors::INPUT_BG;
                    v.widgets.inactive.bg_stroke =
                        egui::Stroke::new(1.0_f32, colors::INPUT_BORDER);
                    v.widgets.hovered.weak_bg_fill = colors::INPUT_BG_HOVER;
                    v.widgets.hovered.bg_stroke =
                        egui::Stroke::new(1.0_f32, colors::INPUT_BORDER_HOVER);
                } else {
                    // 禁用态走 noninteractive：灰底 + 浅边框，值照样读得出来
                    v.widgets.noninteractive.weak_bg_fill = colors::INPUT_BG_DISABLED;
                    v.widgets.noninteractive.bg_stroke =
                        egui::Stroke::new(1.0_f32, colors::BORDER);
                }
                // 展开中 / 点下去的那一帧描一圈蓝，和文本框的聚焦态同一套语言
                v.widgets.active.weak_bg_fill = colors::BG_WHITE;
                v.widgets.active.bg_stroke = egui::Stroke::new(1.5_f32, colors::ACCENT);
                v.widgets.open.weak_bg_fill = colors::ACCENT_BG;
                v.widgets.open.bg_stroke = egui::Stroke::new(1.5_f32, colors::ACCENT);
            }
            // from_id_salt(key)：用字符串显式钉死这个 ComboBox 的 Id（salt=哈希加料）。
            // "展开/收起"状态存在 egui 的内存库里、钥匙就是 Id：不给的话默认按
            // "调用处 ui 树位置"推导，页面布局稍变状态就会串位——key 是稳定的配置字段名，
            // 正好一钥对一框。
            egui::ComboBox::from_id_salt(key)
                .selected_text(egui::RichText::new(selected_label).size(13.0))
                // 按钮和弹出列表同宽：120px 装得下 "192000 Hz"；设备下拉另外占满整行
                .width(width)
                // 设备名超出宽度时截断显示，不把整张设置页撑破
                .truncate()
                .show_ui(ui, |ui| {
                    for (i, o) in items.iter().enumerate() {
                        ui.selectable_value(
                            &mut picked,
                            i,
                            egui::RichText::new(o.label.as_str()).size(13.0),
                        );
                    }
                });
        });

        // 下拉没有"打到一半"的中间状态：一次点击就是一个完整决定，当场记为可落盘。
        if picked != current {
            if let Some(o) = items.get(picked) {
                *value = o.value.clone();
            }
            match dirty.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = true,
                None => dirty.push((key, true)),
            }
        }
    }

    /// 攒着的改动什么时候写盘：有任意一项"改完了"（焦点离开该框 / 在该框里按了回车 /
    /// 下拉框选了一项）就整批落盘。
    fn flush_settings_if_ready(&mut self) {
        if !self.settings_dirty.iter().any(|(_, done)| *done) {
            return;
        }
        let entries = std::mem::take(&mut self.settings_dirty);
        let keys: Vec<&'static str> = entries.into_iter().map(|(k, _)| k).collect();
        self.persist_settings(&keys);
    }

    /// 重新枚举一遍音频设备，填进设置页那几个设备下拉。
    /// 启动时调一次；插了耳机 / 新装了虚拟声卡不必重启程序，点设置页的【重新检测】即可。
    fn refresh_audio_devices(&mut self) {
        let (outputs, inputs) = audio_devices();
        log::info!(
            "[Gui] audio devices: {} output, {} input",
            outputs.len(),
            inputs.len()
        );
        self.output_devices = outputs;
        self.input_devices = inputs;
    }

    /// 把界面上的音频/网络参数 + "麦忙时暂停外放"写回 config.json。
    /// 数字解析不出来（打字打到一半、误填了字母）时保留文件里的旧值，
    /// 界面上那串字符也原样留着让用户改完 —— 绝不静默把用户输入清零。
    fn persist_settings(&mut self, keys: &[&'static str]) {
        let mut cfg = config::get();
        let mut rejected: Vec<&str> = Vec::new();

        for key in keys {
            match *key {
                "network.port" => match self.port.trim().parse::<u16>() {
                    Ok(v) => cfg.network.port = v,
                    Err(_) => rejected.push(key),
                },
                "speaker.sample_rate" => match self.sample_rate.trim().parse::<u32>() {
                    Ok(v) => cfg.speaker.sample_rate = v,
                    Err(_) => rejected.push(key),
                },
                "speaker.channels" => match self.channels.trim().parse::<u16>() {
                    Ok(v) => cfg.speaker.channels = v,
                    Err(_) => rejected.push(key),
                },
                "speaker.buffer_frames" => match self.buffer_size.trim().parse::<u32>() {
                    Ok(v) => cfg.speaker.buffer_frames = v,
                    Err(_) => rejected.push(key),
                },
                "speaker.pause_while_mic_live" => {
                    cfg.speaker.pause_while_mic_live = self.pause_speaker_live
                }
                // ── v3.8.2：麦克风上行 / 摄像头输出 / 诊断 三组设置 ──
                "mic.uplink_sample_rate" => {
                    match self.mic_uplink_rate.trim().parse::<u32>() {
                        Ok(v) => cfg.mic.uplink_sample_rate = v,
                        Err(_) => rejected.push(key),
                    }
                }
                "mic.max_queue_ms" => match self.mic_max_queue_ms.trim().parse::<u32>() {
                    Ok(v) => cfg.mic.max_queue_ms = v,
                    Err(_) => rejected.push(key),
                },
                // 设备名是字符串，没有"打到一半"的非法中间态（下拉里选出来的必是完整名），
                // 所以直接赋值。空格两端会被 config::sanitize 里 trim 掉。
                "speaker.capture_device_hint" => {
                    cfg.speaker.capture_device_hint = self.capture_device.clone();
                }
                "mic.inject_device_hint" => cfg.mic.inject_device_hint = self.inject_device.clone(),
                "mic.monitor_capture_hint" => {
                    cfg.mic.monitor_capture_hint = self.monitor_device.clone();
                }
                // 分辨率在界面上是一整串 "960x720"，这里拆成两个数分别落盘。
                // 拆不出来（用户手改过 config.json 里的宽或高，组合不在选项里）
                // 时保留旧值，别把一半写进去。
                "camera.resolution" => match parse_resolution(&self.cam_resolution) {
                    Some((w, h)) => {
                        cfg.camera.width = w;
                        cfg.camera.height = h;
                    }
                    None => rejected.push(key),
                },
                "camera.fps" => match self.cam_fps.trim().parse::<u32>() {
                    Ok(v) => cfg.camera.fps = v,
                    Err(_) => rejected.push(key),
                },
                // 两条通道各一个开关：关掉 = 那个虚拟设备干脆不注册，
                // 系统里的摄像头列表会少一项（有人只要 OBS，有人只要 Unity）。
                "camera.unity_enabled" => cfg.camera.unity_enabled = self.cam_unity,
                "camera.obs_enabled" => cfg.camera.obs_enabled = self.cam_obs,
                "diagnostics.log_level" => cfg.diagnostics.log_level = self.log_level.clone(),
                "diagnostics.show_console" => cfg.diagnostics.show_console = self.show_console,
                other => log::warn!("[Config] unknown settings key {other}"),
            }
        }

        match config::update(cfg) {
            Ok(()) => {
                // 写完以文件里的值为准刷新界面：update() 内部会夹紧越界取值
                // （端口 0、声道 5、缓冲区 100 万帧…），刷新后用户看到的就是他
                // 实际得到的那个数，而不是他刚打进去的那串。
                let saved = config::get();
                self.port = saved.network.port.to_string();
                self.sample_rate = saved.speaker.sample_rate.to_string();
                self.channels = saved.speaker.channels.to_string();
                self.buffer_size = saved.speaker.buffer_frames.to_string();
                self.pause_speaker_live = saved.speaker.pause_while_mic_live;
                self.mic_uplink_rate = saved.mic.uplink_sample_rate.to_string();
                self.mic_max_queue_ms = saved.mic.max_queue_ms.to_string();
                self.cam_resolution =
                    format!("{}x{}", saved.camera.width, saved.camera.height);
                self.cam_fps = saved.camera.fps.to_string();
                self.cam_unity = saved.camera.unity_enabled;
                self.cam_obs = saved.camera.obs_enabled;
                self.log_level = saved.diagnostics.log_level.clone();
                self.show_console = saved.diagnostics.show_console;
                self.capture_device = saved.speaker.capture_device_hint.clone();
                self.inject_device = saved.mic.inject_device_hint.clone();
                self.monitor_device = saved.mic.monitor_capture_hint.clone();
                // 日志级别是这一堆里唯一【不用重启】就生效的：当场换掉过滤器，
                // 下一行日志就按新级别走。其余（端口/采样率/设备/控制台）都要重启。
                if keys.contains(&"diagnostics.log_level") {
                    apply_log_level(parse_log_level(&saved.diagnostics.log_level));
                }
                if !rejected.is_empty() {
                    self.add_log(format!(
                        "[Config] kept previous value for {} (not a number yet)",
                        rejected.join(", ")
                    ));
                }
                log::info!("[Config] saved {} field(s)", keys.len());
                if let Some(p) = config::config_path() {
                    self.add_log(format!("[Config] saved to {}", p.display()));
                } else {
                    self.add_log("[Config] saved (config dir unavailable)".to_string());
                }
            }
            Err(e) => {
                // 写不进去（磁盘只读、目录被删…）必须说清楚，否则用户以为改了
                log::warn!("[Config] save failed: {e}");
                self.add_log(format!("[Config] save failed: {e}"));
            }
        }
    }

    fn add_log(&mut self, msg: String) {
        // 这是"日志页显示用的尾部缓存"，和写到磁盘的完整 audioserver.log 是两回事
        // （后者一行不落；这里只留最近 200 条给界面看，找全量去设置页印的路径）。
        // 超限不是逐条挤掉，而是一次删最旧的 50 条（drain 会 memmove 整个 Vec，
        // 攒批删摊薄成本）；200 也顺便封顶了 Log Tab 每帧要绘制的 label 数量——
        // 即时模式里"列表多长=每帧画多少个 galley"，上限就是性能阀门。
        let timestamp = chrono_now();
        self.logs.push(format!("[{}] {}", timestamp, msg));
        if self.logs.len() > 200 {
            self.logs.drain(0..50);
        }
    }

    fn start_server(&mut self) {
        // 已经在跑就直接返回：这个函数由 Toggle/门禁撤除两处调用，防重入比信任调用方可靠。
        if self.server_status == ServerStatus::Running {
            return;
        }
        // ── 四个默认魔数（界面字符串 parse 失败时的兜底值）──
        // · 8080：WebSocket 服务端口。config.rs 默认配置同样是 8080，手机 App 里写死
        //   连这个；改这里不影响 config.json（它只是"输入框里打的全不是数字"时兜底），
        //   真要换端口去设置页/配置文件改，两边不一致时手机会连不上。
        // · 48000：采样率 48kHz，Windows WASAPI 混音格式和视频行业的标准档；
        //   44100 是 CD 档。改低省带宽音质差，改高带宽翻倍（手机端要同步改才连贯）。
        // · 2：声道数，2=立体声/1=单声道；config.rs 的 clamp 只放行 1 或 2，写 3 也回 2。
        // · 1024：捕获缓冲（单位=帧，不是毫秒！48kHz 下 1024 帧 ≈ 21ms）。调小延迟低
        //   但容易爆音，调大稳但声音发闷（config.rs 夹在 128~8192 帧之间）。
        // ⚠ 注意（server.rs 里已有人用 ⚠ 标注，这里只转述、不改代码）：server.rs 把
        //   缓冲换算成 WASAPI 的"100 纳秒时长"时写的是 采样率 × 10_000，注释说意图是
        //   10ms——但 10ms 应是 100_000 个 100ns（×100 才对得上）。按现在的公式
        //   48000×10000 = 4.8 亿个 100ns = 48 秒，量纲疑似写错。那是硬件侧的事，
        //   本文件一行不动；排查缓冲行为时先看 server.rs 那一处。
        let config = ServerConfig {
            port: self.port.parse().unwrap_or(8080),
            sample_rate: self.sample_rate.parse().unwrap_or(48000),
            channels: self.channels.parse().unwrap_or(2),
            buffer_size: self.buffer_size.parse().unwrap_or(1024),
        };
        // 两条通道各方向一条（对照文件头 import 处的说明）：
        // · std mpsc::channel()：服务线程往里 send(ServerEvent)，GUI 每帧 try_recv 取走。
        //   std 版收端是同步 API，正合"在 update 里顺手 poll 一把"的用法。
        // · tokio_mpsc::unbounded_channel()：GUI 往里 send(ServerCommand)，服务在
        //   async 循环里 .await 收。异步侧必须用 tokio 版：std 接收端 await 不了，
        //   换成阻塞 recv() 会把单线程执行器整个卡死。
        // "unbounded"=队列无上限：send 永不失败永不等待（GUI 不被后台拖住），
        // 代价是没有背压——命令发疯了一样只会排队。本程序命令量级完全够用。
        let (event_tx, event_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();
        self.event_rx = Some(event_rx);
        self.cmd_tx = Some(cmd_tx);
        self.client_count = 0;
        self.logs.clear();
        self.start_time = Some(Instant::now());
        // 服务跑在【独立 OS 线程】：GUI 这边 spawn 完立刻返回继续画界面——
        // 绝不在 update 里干等，这正是"界面不能阻塞"的落地方式。
        // move ||：把 config / event_tx / cmd_rx 的所有权整体搬进新线程
        // （Rust 跨线程只能搬所有权或共享不可变数据，编译期就杜绝了数据竞争）。
        // 线程内部：Runtime::new() 建一个单线程 tokio 异步执行器，
        // rt.block_on(run_server(..)) 让 async 的服务主循环成为这个线程的"全部工作"
        // （收命令、连手机、推流都在这一台小事件循环里并发）。
        // 这里的 unwrap 若炸（建不起运行时）：panic 钩子会记进日志（见 install_panic_logger），
        // event_tx 随线程销毁 → GUI 下一次 try_recv 得到 Disconnected → 状态回落 Stopped，
        // 和 poll_server_events 里的处理正好闭环。
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

    // 本机 IPv4：问系统路由表"默认网卡是哪块"，不发包、纯本地查询，所以每帧
    // 现取也便宜。断网/多网卡选错时返回 "Unknown"，界面上的地址跟着变——
    // 手机要连的就是这里显示的那台机器，值不值对得上，现场一眼就能核。
    fn get_local_ip() -> String {
        local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "Unknown".to_string())
    }

    // 运行时长：Instant 是"单调时钟的一个时间点"（不受系统改表影响），
    // elapsed() 拿到"从那个点到现在的时长"，取总秒数后拆 时/分/秒。
    // 没人重绘就不刷新——update() 末尾那条 1000ms 心跳就是专门喂它的，
    // 所以完全空闲时秒数最多慢一秒，肉眼无感，CPU 却省了 96%+。
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
        lang::tf(
            "footer.audio_summary",
            &[("rate", &self.sample_rate), ("ch", &self.channels)],
        )
    }

    // ── Header：状态 + 开关 ──
    fn show_header(&mut self, ui: &mut egui::Ui, is_running: bool) {
        // 布局容器第一课：ui.horizontal(|ui| {...}) = "这一段的东西排成一行"，
        // 从左往右逐个领宽度；Ui 的默认方向本来就是竖排（往下堆），
        // ui.vertical() 只是显式包一层。同类还有：ui.columns(n, ...) 横切 n 等份
        // 各放一个 ui；egui::Grid 跨行对齐列（见 show_settings_tab 里 Grid 的注释——有坑）；
        // ui.with_layout(Layout::right_to_left(..)) 反过来从右往左排，专做"标签靠左、控件靠右"。
        // ui.allocate_exact_size 是"手绘控件三件套"的第一步（本文件到处是这个模式）：
        // ① allocate：在当前布局位置占住一块 Rect，返回 (Rect, Response)。
        //    Response 是这一帧交互的"回执"：clicked()/hovered()/changed() 只在当事那帧为真；
        //    Sense 声明这块区域感知什么——Sense::hover() 只跟鼠标，Sense::click() 认点击，
        //    Sense::drag() 管拖拽。兄弟 API ui.allocate_rect(自己算好的 Rect, sense) 用于
        //    手动摆位；ui.put(rect, widget) 是"把现成控件塞进指定矩形"（本文件全走
        //    自动布局 + allocate，没用 put，但读其它 egui 代码时一定会遇到）。
        // ② 查 Response：这一帧用户碰了它没有。
        // ③ if ui.is_rect_visible(rect) 再 ui.painter() 画：滚出可视区就跳过绘画但照常占位，
        //    布局不会忽闪。Toggle、模式胶囊、Tab、芯片全是这套骨架，学会一处全懂。
        ui.horizontal(|ui| {
            // 小圆点状态指示器
            // 12.0 = 逻辑像素直径，纯观感值；改大会顶高整行 Header。
            // 它只需要"看起来是个点"，不吃交互，所以 Sense::hover()、Response 用 _ 丢掉。
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

            // 状态文字（v3：随模式变化；v3.6：全部走 i18n）
            // ── i18n 三件事，初学者必知 ──
            // ① 文案本体在仓库根的 locales/en.toml、locales/zh.toml；lib.rs 里的
            //    `i18n!("locales", fallback = "en")` 宏在【编译期】把它们展开进二进制，
            //    所以发行包是单 exe、运行时没有语言文件。
            // ② 因此改了 toml 必须重新编译才看得到变化。Cargo 默认只盯 .rs 文件，
            //    build.rs 用 cargo:rerun-if-changed 逐个登记 locales/*.toml 才救回这条
            //    （2026-10-01 就翻过车：改了 toml 没重编 lib，界面直接印键名原文）。
            // ③ 运行时 lang::t("键")/lang::tf("键", 占位表) 从 rust-i18n 的内存状态取句子，
            //    不碰磁盘；切语言只改内存 + 写 config.json。正因为 egui 每帧重新执行
            //    这些 t() 调用，"换语言"不需要重建任何控件——下一帧全站自动是新文案。
            //    缺键时 rust-i18n 回退英文，再缺就原样显示键名（界面上看到 "settings.xxx"
            //    这种字样 = 缺键，不是显示坏了）。
            ui.vertical(|ui| {
                let title = if !is_running {
                    lang::t("header.stopped")
                } else if self.mode == AppMode::Mic {
                    if self.mic_live && self.mic_muted {
                        lang::t("header.mic_muted")
                    } else if self.mic_live {
                        lang::t("header.mic_live")
                    } else if self.mic_session {
                        lang::t("header.mic_standby")
                    } else {
                        lang::t("header.mic_idle")
                    }
                } else if self.mode == AppMode::Camera {
                    // v3.4：摄像头模式标题（LIVE = 有应用真的在看，手机相机已开）
                    if self.cam_live {
                        lang::t("header.cam_live")
                    } else if self.cam_session {
                        lang::t("header.cam_standby")
                    } else {
                        lang::t("header.cam_idle")
                    }
                } else {
                    lang::t("header.running")
                };
                // 文字表现两套写法：ui.label("纯文本") 走默认样式（TextStyle::Body，
                // 字号可由用户全局设定）；egui::RichText::new(...) 是带样式的链式构造器——
                // .size(px) 覆盖字号（15 主标题 / 13 正文 / 11 提示 / 10 脚注是本页的节奏）、
                // .strong() 加粗、.monospace() 换等宽字体族、.color() 覆盖颜色。
                // 内部对应关系：size→FontId、monospace→FontFamily::Monospace、
                // strong→FontId 的粗体变体；RichText 只是把 TextStyle 显式化的快捷方式。
                ui.label(
                    egui::RichText::new(title)
                        .size(15.0)
                        .strong()
                        .color(colors::TEXT_PRIMARY),
                );
                let uptime = self.uptime_str();
                let subtitle = if !is_running {
                    lang::t("header.toggle_to_start")
                } else if self.mode == AppMode::Mic {
                    if self.mic_live {
                        lang::tf("header.mic_sub_live", &[("uptime", &uptime)])
                    } else if self.mic_session {
                        lang::t("header.mic_sub_standby")
                    } else {
                        lang::t("header.mic_sub_idle")
                    }
                } else if self.mode == AppMode::Camera {
                    if self.cam_live {
                        lang::tf("header.cam_sub_live", &[("uptime", &uptime)])
                    } else if self.cam_session {
                        lang::t("header.cam_sub_standby")
                    } else {
                        lang::t("header.cam_sub_idle")
                    }
                } else {
                    lang::tf("header.uptime", &[("uptime", &uptime)])
                };
                ui.label(
                    egui::RichText::new(subtitle)
                        .size(11.0)
                        .color(colors::TEXT_MUTED),
                );
            });

            // Toggle 开关
            // with_layout(right_to_left)：这一行从窗口右缘往左摆 —— 开关永远贴右上角，
            // 中间的标题文字占剩余空间。尺寸魔数一组的含义：44x24 = 轨道的宽/高（逻辑像素，
            // 模仿 iOS 开关比例）；knob_r=10 旋钮半径；旋钮圆心距轨道端 12 = knob_r(10)+2，
            // 上下也各留 2px 缝（(24-20)/2）。这四个数互相咬合，只改一个会"旋钮戳出轨道"。
            // Sense::click()：这块矩形认点击，clicked() 那帧直接调 start/stop_server——
            // 即时模式的"事件处理"就是当场问回执、当场改状态，没有回调注册。
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
    // 骨架和 Header 的 Toggle 一样：allocate → 查回执 → 画（见 show_header 的三件套注释）。
    // 新东西只有宽度算式：btn_w = (本行剩余宽度 - 间隙) ÷ 按钮数 —— 先问
    // ui.available_width()（即时模式里"还剩多少地方"是每帧现算的），等分后窗口变宽变窄都自适应。
    fn show_mode_switch(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(34.0);
            let modes = [
                (AppMode::Speaker, lang::t("mode.speaker")),
                (AppMode::Mic, lang::t("mode.microphone")),
                (AppMode::Camera, lang::t("mode.camera")),
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
                    // 排版与绘制是两步：layout_no_wrap 把字符串排成 galley
                    // （"排好版的一块文字"，可查询宽高），painter().galley() 再把它画上去。
                    // FontId::proportional(12.0) = "比例字体、字号 12"（比例=不同字符宽度不同，
                    // 相对的是 monospace 等宽）。选中档 12.5 比未选中 12.0 只大半号，
                    // 纯粹为了"重一点"的观感，没有任何数值含义。
                    // text_pos = 矩形中心 − 文字半宽/半高，就是"水平+垂直居中"的通用公式。
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
    // 尺寸魔数：bar_w=7 一根条的宽、gap=3 条间距、max_h=44 满值时的高度（都是逻辑像素）。
    // n = 可用宽度里塞得下几根就画几根（(宽+间距)/(条宽+间距) 向下取整），上限 34 根——
    // 正好是 mic_bars 数组长度（handle_event 里维护的"左旧右新"滚动历史）。
    // 数据来自 MicLevel 事件（约 20Hz 一条），这也是 update() 末尾"有事件就立刻重绘"的原因之一。
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
    // 又一个小控件范本：先 layout 量出文字尺寸，再加 pad_x/pad_y（8/3 逻辑像素）
    // 算出总大小交给 allocate，最后 rect_filled 画胶囊底 + galley 叠文字。
    // "egui 内置控件没有现成样式时怎么办"的标准答案就是：照这个套路自己画一个。
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
    // 同样是 allocate→回执→绘制三件套；tab_width = 行宽 ÷ 3 等分，38.0/底部 2px 蓝线是观感值。
    // "当前选中的是哪个 Tab"不存在控件里，而是 self.active_tab 字段——即时模式的选中态
    // 永远自己维护：每帧拿字段和这张 Tab 比对来决定高亮，点击那帧只负责改字段。
    fn show_tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(38.0);
            let tabs = [
                (AppTab::Connection, lang::t("tabs.connection")),
                (AppTab::Settings, lang::t("tabs.settings")),
                (AppTab::Log, lang::t("tabs.log")),
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
            // env!("CARGO_PKG_VERSION")：编译期把 Cargo.toml 的 version 字段直接嵌进
            // 二进制成字符串字面量——发新版本号，页脚自动跟着变，不用改这里的代码。
            ui.label(
                egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(10.0)
                    .color(colors::TEXT_DISABLED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // v3：摘要随模式变化
                let summary = if !is_running {
                    lang::t("footer.idle")
                } else if self.mode == AppMode::Mic {
                    match &self.mic_source {
                        Some((_, sr, ch)) => lang::tf(
                            "footer.mic_summary",
                            &[("rate", &sr.to_string()), ("ch", &ch.to_string())],
                        ),
                        None => lang::t("footer.mic_summary_idle"),
                    }
                } else if self.mode == AppMode::Camera {
                    // v3.4：底部摘要显示当前上行画质（有统计时）
                    match &self.cam_stats {
                        Some(s) if s.width > 0 => lang::tf(
                            "footer.cam_summary",
                            &[
                                ("w", &s.width.to_string()),
                                ("h", &s.height.to_string()),
                                ("fps", &s.fps.to_string()),
                            ],
                        ),
                        _ => lang::t("footer.cam_summary_idle"),
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
                // 页脚状态小圆点：6px（Header 那朵是 12px——同一语言的小尺寸版），
                // 同样 allocate(Sense::hover) + circle_filled 两步，纯装饰不吃点击。
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
        // 第三个元素 = 该方式现在能不能点（WiFi=true；USB/蓝牙 是画出来占位的禁用态：
        // Sense::hover() 只挂鼠标不挂点击，灰字提示"以后这里有"。加新方式时把 true 打开即可）。
        // 行高 56、按钮 52、圆角 8、间隙 8 全是指定像素的观感值，窗口窄时 btn_w 由
        // (行宽 - 2×gap)/3 自适应，数字本身没有物理含义。
        ui.horizontal(|ui| {
            ui.set_height(56.0);
            let conns = [
                (ConnectionType::Wifi, lang::t("conn.wifi"), true),
                (ConnectionType::Usb, lang::t("conn.usb"), false),
                (
                    ConnectionType::Bluetooth,
                    lang::t("conn.bluetooth"),
                    false,
                ),
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
        // ── "卡片"模式（全文件重复十几次的那段配方）──
        // egui::Frame::none() 是"什么装饰都没有的画框起点"，链式点上四样：
        //   .fill 底色（BG_WHITE 纯白卡片）· .stroke 边框（1px 浅灰 BORDER）
        //   .rounding(8.0) 圆角半径（逻辑像素）· .inner_margin(14.0) 内边距。
        // 然后 card_frame.show(ui, |ui| {...}) 在父布局里开出这块带底色的区域，
        // 闭包内的 ui 就是"框内剩余空间"，继续 horizontal/label 照常排。
        // 想统一改圆角/留白，搜 `Frame::none()` 批量对齐即可；
        // 页面真正的"骨架分区"（顶栏/内容/底栏）则由 update() 里的 Panel 负责，
        // Frame 只管"一块带背景的矩形"这一层。
        let card_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        card_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("conn.info"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);

            // IP 行
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(lang::t("conn.local_ip"))
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(lang::t("conn.copy"))
                                    .size(10.0)
                                    .color(colors::ACCENT),
                            )
                            .fill(colors::ACCENT_BG)
                            .stroke(egui::Stroke::new(1.0_f32, colors::ACCENT_BORDER))
                            .rounding(4.0),
                        )
                        .clicked()
                    {
                        // ui.output_mut = 这一帧结束时把 copied_text 交给系统剪贴板，
                        // 是 egui"界面向外发东西"的官方出口（cursor_icon 也走这条路）。
                        // 顺带对比：内置 Button 不需要手绘三件套——ui.add(控件) 自己
                        //  allocate、自己画外观、返回 Response，链上 .clicked() 当场消费。
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
            // 拼的是手机 App 实际连的端点：端口 = 界面当前值（默认 8080），路径 /ws/audio
            // 由 server.rs 的路由决定（改这里不改服务端 = 展示错方向，两边要一起动）。
            // 每帧现拼：port 是内存字段、local_ip 也是这一帧现问操作系统的，无需缓存。
            let ws_addr = format!("ws://{}:{}/ws/audio", local_ip, self.port);
            ui.horizontal(|ui| {
                ui.label(
                    // WebSocket 是全球通用词，两种语言同字，但仍然走文案表（保持"界面
                    // 上没有一处硬编码文案"这条纪律，将来加语言只改 toml）
                    egui::RichText::new(lang::t("conn.websocket"))
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(lang::t("conn.copy"))
                                    .size(10.0)
                                    .color(colors::ACCENT),
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
                        egui::RichText::new(lang::t("conn.clients"))
                            .size(12.0)
                            .color(colors::TEXT_MUTED),
                    );
                    if self.client_count == 0 {
                        ui.label(
                            egui::RichText::new(lang::t("conn.waiting"))
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
                egui::RichText::new(lang::t("mic.virtual_card"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(lang::t("mic.phone_mic"))
                        .size(14.0)
                        .strong()
                        .color(colors::TEXT_PRIMARY),
                );
                ui.add_space(8.0);
                if self.mic_engine_device.is_some() {
                    Self::show_chip(
                        ui,
                        &lang::t("mic.chip_ready"),
                        egui::Color32::from_rgb(4, 120, 87),
                        egui::Color32::from_rgb(236, 253, 245),
                    );
                } else {
                    Self::show_chip(
                        ui,
                        &lang::t("mic.chip_driver_needed"),
                        colors::WARN_TEXT,
                        colors::WARN_BG,
                    );
                }
                // v3.1：上行状态芯片（自动感知：应用占用 → LIVE，空闲 → STANDBY）
                if self.mic_live && self.mic_muted {
                    Self::show_chip(
                        ui,
                        &lang::t("mic.chip_muted"),
                        egui::Color32::from_rgb(120, 113, 108),
                        egui::Color32::from_rgb(245, 245, 244),
                    );
                } else if self.mic_live {
                    Self::show_chip(
                        ui,
                        &lang::t("mic.chip_live"),
                        egui::Color32::from_rgb(190, 18, 60),
                        egui::Color32::from_rgb(255, 228, 230),
                    );
                } else if self.mic_session {
                    Self::show_chip(
                        ui,
                        &lang::t("mic.chip_standby"),
                        egui::Color32::from_rgb(4, 120, 87),
                        egui::Color32::from_rgb(236, 253, 245),
                    );
                }
            });
            ui.add_space(4.0);
            self.show_level_bars(ui);
            ui.add_space(4.0);
            let hint = if self.mic_engine_device.is_some() {
                lang::t("mic.hint_ready")
            } else {
                lang::t("mic.hint_missing")
            };
            ui.label(
                egui::RichText::new(hint)
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            // 下行暂停开关按钮（默认全双工同时进行）
            let btn_label = if self.pause_speaker_live {
                lang::t("mic.speaker_paused")
            } else {
                lang::t("mic.speaker_live")
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
                // 麦克风主页上这个快捷开关和【设置】页的复选框是同一个值，
                // 点了也要落进 config.json，否则重启就回到老样子
                self.persist_settings(&["speaker.pause_while_mic_live"]);
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
                egui::RichText::new(lang::t("mic.source_card"))
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
                                egui::RichText::new(lang::tf(
                                    "mic.source_fmt",
                                    &[("rate", &sr.to_string()), ("ch", &ch.to_string())],
                                ))
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
                                    // 红点"呼吸"动画：0.6 + 0.4×sin(运行秒数×4) 在 0.2~1.0 摆动，
                                    // ×4 决定频率（一个来回约 1.6 秒）；linear_multiply(pulse)
                                    // 按系数压暗红色。每帧重画时用的是当帧新值 —— 即时模式做动画
                                    // 不需要定时器控件，只要"每帧算一次 + 有人肯重绘"（250ms 心跳，见 update 末尾）。
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
                        egui::RichText::new(lang::t("mic.source_waiting"))
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
                    cell(ui, format!("{}ms", stats.interval_ms), &lang::t("mic.stat_chunk"));
                    ui.add_space(8.0);
                    cell(ui, format!("{}k", stats.kbps), &lang::t("mic.stat_uplink"));
                    ui.add_space(8.0);
                    cell(ui, format!("{}KB", stats.total_kb), &lang::t("mic.stat_received"));
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
                egui::RichText::new(lang::t("mic.target_card"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            let target = self
                .mic_engine_device
                .clone()
                .unwrap_or_else(|| lang::t("mic.target_none"));
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(lang::t("mic.render_device"))
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
                egui::RichText::new(lang::t("cam.virtual_card"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(lang::t("cam.driver_names"))
                        .size(14.0)
                        .strong()
                        .color(colors::TEXT_PRIMARY),
                );
                ui.add_space(8.0);
                // 状态芯片：LIVE=红（应用正在看，手机相机已开）/ STANDBY=绿（待命）
                if self.cam_live {
                    Self::show_chip(
                        ui,
                        &lang::t("cam.chip_live"),
                        egui::Color32::from_rgb(190, 18, 60),
                        egui::Color32::from_rgb(255, 228, 230),
                    );
                } else if self.cam_session {
                    Self::show_chip(
                        ui,
                        &lang::t("cam.chip_standby"),
                        egui::Color32::from_rgb(4, 120, 87),
                        egui::Color32::from_rgb(236, 253, 245),
                    );
                }
            });
            ui.add_space(4.0);
            let hint = if self.cam_live {
                lang::t("cam.hint_live")
            } else if self.cam_session {
                lang::t("cam.hint_standby")
            } else {
                lang::t("cam.hint_none")
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
                    egui::RichText::new(format!("\u{1F4F7} {}", lang::t("cam.btn_request")))
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
                    self.add_log(lang::t("cam.log_request"));
                }
                let stop_enabled = self.cam_session;
                let stop_btn = egui::Button::new(
                    egui::RichText::new(format!("\u{1F6D1} {}", lang::t("cam.btn_force_stop")))
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
                    self.add_log(lang::t("cam.log_force_stop"));
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
                egui::RichText::new(lang::t("cam.preview_card"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            // 预览区：宽 = 卡片内剩余宽度，高按图片真实比例反推
            // h = preview_w × 图高/图宽（.max(1) 防除零）——等比缩放，永远不拉伸变形。
            // tex 是 TextureHandle：CamFrame 事件（1fps）把 JPEG 解码后经
            // ctx.load_texture 上传给渲染器；egui 的内置 Image 控件直接贴它。
            // 没帧时的占位同样占 preview_w × 3/4 高（4:3 = 手机主摄常见比例），
            // 黑位换成有图都不会引起卡片跳动。
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
                        egui::RichText::new(lang::t("cam.preview_off"))
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
                        &lang::t("cam.stat_resolution"),
                    );
                    ui.add_space(8.0);
                    cell(ui, format!("{}fps", stats.fps), &lang::t("cam.stat_fps"));
                    ui.add_space(8.0);
                    cell(ui, format!("{}k", stats.kbps), &lang::t("cam.stat_uplink"));
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
                egui::RichText::new(lang::t("cam.source_card"))
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
                        egui::RichText::new(lang::t("cam.source_waiting"))
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
                    egui::RichText::new(lang::t("cam.caps_card"))
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
    fn show_camera_settings_tab(&mut self, ui: &mut egui::Ui, is_running: bool) {
        // ── 虚拟摄像头输出卡片（v3.8.2）──
        // 分辨率 / 帧率 / 两条通道开关原来只能手改 config.json，现在挪到界面上。
        // 为什么做成下拉而不是输入框：宽和高必须成对且都是偶数（NV12 的色度是 2x2
        // 子采样，奇数尺寸会被 config.rs 的 sanitize 改掉），手打很容易填出一个
        // "看着填对了、其实被系统改过"的值。这里直接给一组现成档位。
        let resolution_options = vec![
            ComboOption { label: "640x480".into(), value: "640x480".into() },
            ComboOption { label: "854x480".into(), value: "854x480".into() },
            ComboOption { label: "960x540".into(), value: "960x540".into() },
            ComboOption { label: "960x720".into(), value: "960x720".into() },
            ComboOption { label: "1280x720".into(), value: "1280x720".into() },
            ComboOption { label: "1920x1080".into(), value: "1920x1080".into() },
        ];
        let fps_options = number_options(&["15", "24", "30", "60"], " fps");

        let out_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        out_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("cam.output"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            // 服务在跑 = 两个共享内存通道已经按当前尺寸建好了，中途改尺寸会让
            // 已经打开摄像头的软件拿到错乱的帧 —— 和音频参数一样锁起来。
            // ui.add_enabled_ui(false, ...)：把整棵子树设为"禁用"——控件照常绘制
            // 但改变 sense 不再吃输入、配色走 noninteractive 那组（所以 INPUT_BG_DISABLED
            // 那套专门做得"看得见但改不动"）。这是 egui 锁表单的最省写法，不用逐控件 add_enabled。
            ui.add_enabled_ui(!is_running, |ui| {
                // Grid 用法 / "TextEdit 边框被 Grid 吞掉要靠 ui.scope 修"的大坑，
                // 详解见后面 egui::Grid::new("audio_settings") 上方的长注释，此处不再重复。
                egui::Grid::new("camera_output_settings")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(lang::t("cam.resolution"))
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                Self::config_dropdown(
                                    ui,
                                    &mut self.cam_resolution,
                                    "camera.resolution",
                                    &resolution_options,
                                    &mut self.settings_dirty,
                                );
                            },
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new(lang::t("cam.fps"))
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                Self::config_dropdown(
                                    ui,
                                    &mut self.cam_fps,
                                    "camera.fps",
                                    &fps_options,
                                    &mut self.settings_dirty,
                                );
                            },
                        );
                        ui.end_row();
                    });
                ui.add_space(6.0);
                // 开关不放进 Grid：中文标签长短不一，跟右边的下拉框对不齐，
                // 而且勾选框自己就有"方块 + 文字"的形状，一眼看得懂。
                let old_unity = self.cam_unity;
                ui.checkbox(&mut self.cam_unity, lang::t("cam.unity"));
                if self.cam_unity != old_unity {
                    // 和"麦忙时暂停外放"同一套：开关没有中间态，点一下当场落盘；
                    // persist_settings 写完会以文件为准刷新界面（sanitize 过的那份）
                    self.persist_settings(&["camera.unity_enabled"]);
                }
                ui.label(
                    egui::RichText::new(lang::t("cam.unity_hint"))
                        .size(11.0)
                        .color(colors::TEXT_MUTED),
                );
                ui.add_space(4.0);
                if ui.checkbox(&mut self.cam_obs, lang::t("cam.obs")).changed() {
                    self.persist_settings(&["camera.obs_enabled"]);
                }
                ui.label(
                    egui::RichText::new(lang::t("cam.obs_hint"))
                        .size(11.0)
                        .color(colors::TEXT_MUTED),
                );
            });
        });

        ui.add_space(8.0);

        let card = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("cam.how_to_use"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(lang::t("cam.steps"))
                    .size(12.0)
                    .color(colors::TEXT_SECONDARY),
            );
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(lang::t("cam.drivers_note"))
                    .size(11.0)
                    .monospace()
                    .color(colors::TEXT_MUTED),
            );
            ui.label(
                egui::RichText::new(lang::t("cam.placeholder_note"))
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

    /// v3.6：语言设置卡片 —— 三种模式的【设置】页底部都会挂上它。
    /// ----------------------------------------------------------------------------
    /// 三个按钮 = config.json 里 language 的三种取值：
    ///   auto = 跟随系统（默认） / en = 强制英文 / zh = 强制简体中文
    /// 点下去立刻生效（egui 每帧重新取文案，下一帧整站换语言），
    /// 同时写进 %APPDATA%\PCAssistant\config.json —— 不写注册表；
    /// v3.8 之前这个选择存在 exe 同目录的 settings.txt，升级时会自动搬过来一次。
    fn show_language_card(&mut self, ui: &mut egui::Ui) {
        let card = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        // 当前生效的语言 + 用户在 settings.txt 里存过的选择（用来决定哪个按钮高亮）。
        // current_choice() 读的是内存缓存：这里每帧都会执行，早先用 saved_choice()
        // 等于每帧读一次文件 + 打一行日志，audioserver.log 被同一行灌了三万多次。
        let current = lang::locale();
        let saved = lang::current_choice();

        card.show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(lang::t("settings.language"))
                        .size(11.0)
                        .color(colors::TEXT_MUTED),
                );
                // 当前真正生效的语言（auto 时 = 系统语言），语言名本身不翻译
                Self::show_chip(
                    ui,
                    lang::display_name(&current),
                    colors::ACCENT,
                    colors::ACCENT_BG,
                );
            });
            ui.add_space(8.0);

            let items = [
                (lang::LanguageChoice::Auto, lang::t("settings.lang_auto")),
                (lang::LanguageChoice::En, lang::t("settings.lang_en")),
                (lang::LanguageChoice::Zh, lang::t("settings.lang_zh")),
            ];

            ui.horizontal(|ui| {
                for (choice, label) in items {
                    let active = saved == choice;
                    let btn = egui::Button::new(
                        egui::RichText::new(label).size(11.0).color(if active {
                            colors::ACCENT
                        } else {
                            colors::TEXT_SECONDARY
                        }),
                    )
                    .fill(if active { colors::ACCENT_BG } else { colors::BG_LIGHT })
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if active {
                            colors::ACCENT_BORDER
                        } else {
                            colors::BORDER
                        },
                    ))
                    .rounding(6.0);
                    if ui.add(btn).clicked() && !active {
                        match lang::switch_to(choice) {
                            Ok(()) => {
                                self.lang_error = None;
                                self.add_log(format!(
                                    "[Lang] switched to {}",
                                    lang::locale()
                                ));
                            }
                            // 保存失败（例如 exe 放在只读目录）也要让用户看见，
                            // 不然他会以为切换没生效
                            Err(e) => {
                                self.lang_error = Some(lang::tf(
                                    "settings.lang_save_failed",
                                    &[("error", &e.to_string())],
                                ));
                            }
                        }
                    }
                }
            });

            ui.add_space(6.0);
            let hint = lang::tf(
                "settings.lang_hint",
                &[("locale", lang::display_name(lang::system_locale()))],
            );
            ui.label(
                egui::RichText::new(hint)
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            if let Some(err) = &self.lang_error {
                ui.label(
                    egui::RichText::new(err)
                        .size(11.0)
                        .color(colors::WARN_TEXT),
                );
            }
        });
    }

    /// v3.8.2：诊断卡片 —— 三种模式的【设置】页底部都有。
    /// ----------------------------------------------------------------------------
    /// 日志级别：这一项是整页唯一【改完立刻生效】的（见 apply_log_level）。
    ///          故意不在服务运行时锁掉它 —— 排障时最有用的动作恰恰是"正跑着的时候
    ///          点开 debug 看细节"，锁住就等于把这个场景删了。
    /// 控制台窗口：必须重启才生效，而且原因没法绕过：Rust 的 stdout/stderr 句柄是
    ///          "第一次用到时才缓存"，程序一开头就把输出写进了空句柄，中途再
    ///          AllocConsole 也接不回已经丢掉的输出（详见文件头 windows_subsystem 那段）。
    ///          所以这里勾完要在界面上说清楚"重启后才有窗口"，别让人以为没生效。
    fn show_diagnostics_card(&mut self, ui: &mut egui::Ui, _is_running: bool) {
        let level_options = vec![
            ComboOption { label: "error".into(), value: "error".into() },
            ComboOption { label: "warn".into(), value: "warn".into() },
            ComboOption { label: "info".into(), value: "info".into() },
            ComboOption { label: "debug".into(), value: "debug".into() },
            ComboOption { label: "trace".into(), value: "trace".into() },
        ];

        let card = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("settings.diagnostics"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(lang::t("settings.log_level"))
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    Self::config_dropdown(
                        ui,
                        &mut self.log_level,
                        "diagnostics.log_level",
                        &level_options,
                        &mut self.settings_dirty,
                    );
                });
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(lang::t("settings.log_level_hint"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            let old_console = self.show_console;
            ui.checkbox(&mut self.show_console, lang::t("settings.show_console"));
            if self.show_console != old_console {
                self.persist_settings(&["diagnostics.show_console"]);
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(lang::t("settings.show_console_hint"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            // 日志写在哪：这一行原来只在【日志】页脚出现，设置页里看不到。
            // 收反馈时第一句问的就是路径，所以直接印在设置页上。
            ui.label(
                egui::RichText::new(lang::tf("settings.log_path", &[("path", &log_path().display().to_string())]))
                    .size(11.0)
                    .monospace()
                    .color(colors::TEXT_MUTED),
            );
        });
    }

    // ── Settings Tab ──
    fn show_settings_tab(&mut self, ui: &mut egui::Ui, is_running: bool) {
        // v3：Mic 模式呈现麦克风设置
        if self.mode == AppMode::Mic {
            self.show_mic_settings_tab(ui, is_running);
            ui.add_space(8.0);
            self.show_language_card(ui);
            ui.add_space(8.0);
            self.show_diagnostics_card(ui, is_running);
            ui.add_space(8.0);
            self.show_config_card(ui);
            self.flush_settings_if_ready();
            return;
        }
        // v3.4：Camera 模式呈现摄像头设置（设备名提示 + 待命说明）
        if self.mode == AppMode::Camera {
            self.show_camera_settings_tab(ui, is_running);
            ui.add_space(8.0);
            self.show_language_card(ui);
            ui.add_space(8.0);
            self.show_diagnostics_card(ui, is_running);
            ui.add_space(8.0);
            self.show_config_card(ui);
            self.flush_settings_if_ready();
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
                        egui::RichText::new(lang::t("settings.warn_running"))
                            .size(12.0)
                            .color(colors::WARN_TEXT),
                    );
                });
            });
            ui.add_space(8.0);
        }

        // ── 下拉选项（v3.8.1）──
        // 每帧现建，不缓存：声道数和缓冲区的单位要跟着界面语言走，
        // 而语言是可以在设置页里当场切换的（切完下一帧就是新文案）。
        // 三组都是"合法取值就那么几个"的枚举：
        //   采样率 ← WASAPI 混音格式实测常用档；
        //   缓冲区 ← 2 的幂，太短会爆音、太长就是延迟；
        //   声道数 ← config.rs 只放行 1 或 2。
        let rate_options = number_options(&["44100", "48000", "96000", "192000"], " Hz");
        let channel_options = vec![
            ComboOption {
                label: lang::t("settings.ch_mono"),
                value: "1".to_string(),
            },
            ComboOption {
                label: lang::t("settings.ch_stereo"),
                value: "2".to_string(),
            },
        ];
        let buffer_options = number_options(
            &["256", "512", "1024", "2048", "4096"],
            &format!(" {}", lang::t("settings.unit_frames")),
        );

        // 音频参数卡片
        let audio_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        audio_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("settings.audio"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.add_enabled_ui(!is_running, |ui| {
                // ── egui::Grid 用法与它的大坑（这段务必读完再动下面任何一行）──
                // Grid = 跨行对齐的表格布局：num_columns(2) 声明两列，第一列全部
                // 左对齐、第二列全部对齐到同一竖线（普通 horizontal 每行各排各的，
                // 做不到"每列同宽"，这就是设置页用 Grid 的原因）。spacing([8.0, 6.0])
                // = [列间距, 行间距]（逻辑像素）；ui.end_row() 说"本行完，换下一行"。
                // Grid::new("audio_settings") 的字符串是这个 Grid 的 Id（见 update()
                // 上方总览：Id 是 egui 存控件内部状态的钥匙，改名等于换钥匙，
                // 内部布局状态作废重来——别随手重命名这些字符串）。
                // ⚠ 大坑（2026-10-01 专门提交修过，别图省事退回去）：
                // TextEdit/ComboBox 画背景和边框用的是"先占位、后回填"——show() 先往
                // 本次布局的 shape 列表塞一个 Noop 占位记住下标，收尾时 painter().set(下标, 矩形)
                // 把底色/边框插回那个位置。而 Grid 为了对齐会把单元格里的 shape 列表
                // 【重排/搬移】，占位下标就错位失效，结果是"Grid 里的输入框永远画不出边框"
                // ——实测：停止服务后 48000/2/1024 三行白字贴白卡，一个框都没有。
                // 修法不是改 Grid，而是给每个输入控件包一层 ui.scope（见 config_text_edit /
                // dropdown_at_width）：scope 会开出子 Ui，子 Ui 有【自己的 shape 列表】，
                // 占位下标落在里面，父级 Grid 无论怎么搬动整段都不会踩到下标。
                // 所以 Grid 单元里调的是 Self::config_text_edit / Self::config_dropdown，
                // 而不是裸 ui.add(TextEdit) —— 看着绕，其实是必需的。
                egui::Grid::new("audio_settings")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(lang::t("settings.sample_rate"))
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                Self::config_dropdown(
                                    ui,
                                    &mut self.sample_rate,
                                    "speaker.sample_rate",
                                    &rate_options,
                                    &mut self.settings_dirty,
                                );
                            },
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new(lang::t("settings.channels"))
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                Self::config_dropdown(
                                    ui,
                                    &mut self.channels,
                                    "speaker.channels",
                                    &channel_options,
                                    &mut self.settings_dirty,
                                );
                            },
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new(lang::t("settings.buffer_size"))
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                Self::config_dropdown(
                                    ui,
                                    &mut self.buffer_size,
                                    "speaker.buffer_frames",
                                    &buffer_options,
                                    &mut self.settings_dirty,
                                );
                            },
                        );
                        ui.end_row();
                    });
            });
        });

        ui.add_space(8.0);

        // ── 音频设备卡片（v3.8.2）──────────────────────────────────────
        // 设备选择以前只存在于 config.json 的字符串里：拼错一个字母不会报错，
        // 只会静默退回默认设备 —— 正是最难查的那种"怎么没声音"。
        // 现在把系统里真实存在的设备枚举出来挑，选出来的名字一定是这台机器上有的。
        let dev_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        // 为什么先 clone 一份设备表：同一次调用里既要 &self.output_devices（不可变）
        // 又要 &mut self.settings_dirty（可变），Rust 不允许同时借两次 self。
        // 这里是十几条短字符串，而且只在设置页被重绘时走到（不是每帧，见 update() 的按需重绘）。
        let outputs = self.output_devices.clone();
        let mut rescan = false;
        dev_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("settings.device"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(lang::t("settings.capture_device"))
                    .size(13.0)
                    .color(colors::TEXT_SECONDARY),
            );
            ui.add_space(2.0);
            // 和采样率同理：服务跑起来之后两条 WASAPI 通道已经按当前设备建好了，
            // 中途换设备不会让正在推的声音改道，只会让人以为改了没生效 → 锁住。
            ui.add_enabled_ui(!is_running, |ui| {
                Self::device_dropdown(
                    ui,
                    &mut self.capture_device,
                    "speaker.capture_device_hint",
                    &outputs,
                    // 留空 = 跟着系统默认播放设备走（server.rs 里没匹配到也是这个行为）
                    Some(lang::t("settings.system_default")),
                    &mut self.settings_dirty,
                );
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(lang::t("settings.capture_device_hint"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            // 枚举只在启动时做一次，插了耳机 / 新装虚拟声卡得有个不重启就能重扫的口子
            if ui.button(lang::t("settings.rescan_devices")).clicked() {
                rescan = true;
            }
        });
        if rescan {
            self.refresh_audio_devices();
        }

        ui.add_space(8.0);

        // 网络参数卡片
        let net_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        net_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("settings.network"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.add_enabled_ui(!is_running, |ui| {
                // Grid + 端口输入框：ui.scope 防吞边框的原理见 "audio_settings" 上方注释。
                egui::Grid::new("net_settings")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(lang::t("settings.port"))
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                // 端口没有"就那么几个"的合法取值（0-65535 里除了占用都算对），
                                // 所以保留手打，但只允许输入数字。
                                Self::config_text_edit(
                                    ui,
                                    &mut self.port,
                                    "network.port",
                                    &mut self.settings_dirty,
                                    true,
                                );
                            },
                        );
                        ui.end_row();
                    });
            });
        });

        ui.add_space(8.0);
        self.show_language_card(ui);
        ui.add_space(8.0);
        self.show_diagnostics_card(ui, is_running);
        ui.add_space(8.0);
        self.show_config_card(ui);

        // 本帧结束前统一判断：有输入框"改完了"就写一次 config.json。
        // 每个提前 return 的分支都要带上这句，否则用户在设置页改一半切走，改动就丢了。
        self.flush_settings_if_ready();
    }

    /// v3.8：配置文件卡片 —— 三种模式的【设置】页底部都有。
    /// 为什么要在界面上把路径写出来：安装版把 exe 放进 Program Files、配置放进
    /// %APPDATA%，用户（和收 bug 反馈的人）第一个问题永远是"我改的那个文件在哪"。
    /// 这一行只读内存里已经算好的路径字符串，不碰磁盘、不打日志。
    fn show_config_card(&self, ui: &mut egui::Ui) {
        let card = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("settings.config_file"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            let text = match &self.config_path_text {
                Some(p) => lang::tf("settings.config_hint", &[("path", p)]),
                // 拿不到 APPDATA（极端环境）：说清楚本次只按内置默认值跑
                None => lang::t("settings.config_missing"),
            };
            ui.label(
                egui::RichText::new(text)
                    .size(11.0)
                    .color(colors::TEXT_SECONDARY),
            );
            // 日志文件位置也摊开：绿色版写在 exe 旁边，装进 Program Files 会写在
            // %APPDATA%\PCAssistant，用户/客服不用猜。
            ui.add_space(4.0);
            let log_line = lang::tf(
                "settings.log_path",
                &[("path", &log_path().display().to_string())],
            );
            ui.label(
                egui::RichText::new(log_line)
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
        });
    }

    // ── v3：Mic 模式 Settings Tab ──
    fn show_mic_settings_tab(&mut self, ui: &mut egui::Ui, is_running: bool) {
        let card = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        card.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("settings.behavior"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(lang::t("settings.pause_speaker"))
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let old = self.pause_speaker_live;
                    ui.checkbox(&mut self.pause_speaker_live, "");
                    if self.pause_speaker_live != old {
                        self.sync_speaker_pause();
                        // 开关没有"打到一半"的中间状态：一次点击就是一个完整决定，
                        // 所以这里立刻落盘，不用走输入框那套 dirty 队列
                        self.persist_settings(&["speaker.pause_while_mic_live"]);
                    }
                });
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(lang::t("settings.pause_speaker_hint"))
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
                egui::RichText::new(lang::t("settings.uplink_format"))
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
                    egui::RichText::new(lang::t("settings.sample_rate"))
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
                    egui::RichText::new(lang::t("settings.channels"))
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
                egui::RichText::new(lang::t("settings.uplink_hint"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
        });

        ui.add_space(8.0);

        // ── 上行注入参数（v3.8.2 从"只能手改 config.json"挪到界面上）──
        // 上面那张卡片显示的是【手机实际推上来】的格式（只读），
        // 这一张是【电脑往虚拟声卡里灌】时用的是什么格式 —— 两者可以不一样：
        // 手机推 48k 单声道，电脑按 CABLE 的混音格式（一般 48k 立体声）灌进去。
        let rate_options = number_options(&["44100", "48000", "96000", "192000"], " Hz");
        // 抖动缓冲上限档位（毫秒，config.rs 夹 20~2000）：网络抖动比这还大就丢帧保实时。
        // 100 激进（卡网直接断音）、600 保守（ worst case 半秒延迟像对讲机）——
        // 档位就是"实时性 vs 流畅度"的五个刻度，不是随便五个数。
        let queue_options = number_options(&["100", "150", "250", "400", "600"], " ms");
        let card3 = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        card3.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("settings.inject"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            // 注入流已经按当前格式开着了，中途换采样率/缓冲等于抽掉地基 —— 锁
            ui.add_enabled_ui(!is_running, |ui| {
                // Grid + ui.scope 防吞边框的原理：见 egui::Grid::new("audio_settings") 上方长注释
                egui::Grid::new("mic_inject_settings")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(lang::t("settings.uplink_rate"))
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                Self::config_dropdown(
                                    ui,
                                    &mut self.mic_uplink_rate,
                                    "mic.uplink_sample_rate",
                                    &rate_options,
                                    &mut self.settings_dirty,
                                );
                            },
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new(lang::t("settings.max_queue"))
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                Self::config_dropdown(
                                    ui,
                                    &mut self.mic_max_queue_ms,
                                    "mic.max_queue_ms",
                                    &queue_options,
                                    &mut self.settings_dirty,
                                );
                            },
                        );
                        ui.end_row();
                    });
            });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(lang::t("settings.inject_hint"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
        });

        // ── 注入设备（v3.8.2 下拉化）──────────────────────────────────
        // 这两项是整套"手机当麦克风"里最容易配错的地方：注入端要选虚拟声卡的
        // 【输入】(CABLE Input)，占用检测要选它的【输出】(CABLE Output)，
        // 两条必须描述同一根线，配错了表现是"引擎显示正常但全场静音"。
        // 以前只能对着 config.json 手打字符串，现在直接从系统设备里挑。
        let card4 = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));
        let outputs = self.output_devices.clone();
        let inputs = self.input_devices.clone();
        let mut rescan = false;
        card4.show(ui, |ui| {
            ui.label(
                egui::RichText::new(lang::t("settings.inject_device_card"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(lang::t("settings.inject_device"))
                    .size(13.0)
                    .color(colors::TEXT_SECONDARY),
            );
            ui.add_space(2.0);
            ui.add_enabled_ui(!is_running, |ui| {
                Self::device_dropdown(
                    ui,
                    &mut self.inject_device,
                    "mic.inject_device_hint",
                    &outputs,
                    Some(lang::t("settings.cable_default")),
                    &mut self.settings_dirty,
                );
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(lang::t("settings.inject_device_hint"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(10.0);

            ui.label(
                egui::RichText::new(lang::t("settings.monitor_device"))
                    .size(13.0)
                    .color(colors::TEXT_SECONDARY),
            );
            ui.add_space(2.0);
            ui.add_enabled_ui(!is_running, |ui| {
                Self::device_dropdown(
                    ui,
                    &mut self.monitor_device,
                    "mic.monitor_capture_hint",
                    &inputs,
                    Some(lang::t("settings.cable_default")),
                    &mut self.settings_dirty,
                );
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(lang::t("settings.monitor_device_hint"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(6.0);
            if ui.button(lang::t("settings.rescan_devices")).clicked() {
                rescan = true;
            }
        });
        if rescan {
            self.refresh_audio_devices();
        }
    }

    // ── Log Tab ──
    fn show_log_tab(&mut self, ui: &mut egui::Ui) {
        // 工具栏
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(lang::t("log.runtime"))
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(lang::t("log.clear")).size(11.0).color(colors::RED),
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

        // ScrollArea = "内容超出可视区就出滚动条"的容器：vertical = 只允许上下滚。
        // max_height 由可用高度现算（即时模式里 UI 尺寸是这一帧问这一帧的，-4 是留边）；
        // stick_to_bottom = 新日志追加进来时自动滚到最底看最新一行（终端的经典行为）。
        // 它和设置页那个不设上限的 ScrollArea 用法互补：那边装长表单，这边装无限增长的列表。
        let available_height = ui.available_height() - 4.0;
        log_frame.show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(available_height)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.logs.is_empty() {
                        ui.label(
                            egui::RichText::new(lang::t("log.empty"))
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

// ── eframe::App：即时模式的主场景，update() 每一帧都会被重新调用 ──────────
// 什么时候被调？三种触发源：① winit 送来输入事件（鼠标移动/点击/键盘/窗口缩放/
// 重获焦点），egui 自动安排一帧；② 代码主动 ctx.request_repaint()（尽快画下一帧）；
// ③ ctx.request_repaint_after(d)（最迟 d 之后必须画一帧）。没有第四种——
// 什么都没发生、也没挂心跳时，一帧都不会画。这就是"按需重绘"省 CPU 的原理：
// 原来这里是【无条件】request_repaint()，等于每帧都说"立刻再来一帧"，
// 窗口以能跑多快跑多快的频率永久空转（实测烧掉约 1.15 个核心，任务管理器 14~16%；
// 改成"有事件才快绘、没事挂 250ms/1000ms 心跳"后降到 ~0.4%）。音频/视频搬运
// 在后台线程，GUI 只是"照片"，空转重绘画出的照片一模一样，纯属白烧。
// 每次进入 update，egui 从零开始重建整个界面：先按声明顺序切窗口矩形
// （TopBottomPanel 逐条收边走，CentralPanel 拿剩余），再在每块里摆控件、
// 比对输入、发 Response。所以：
//   · 不能把"只执行一次"的逻辑写在 update 里（该去 App::new / main 头部）；
//   · 状态改动（点开关、收事件）都写回 self 字段，下一帧的描述自然读到新值；
//   · 控件的"内部记忆"（悬停、聚焦、滚动条位置、下拉开合）由 egui 按 Id 存在
//     ctx 内存库里，Id 来自 ui 树调用位置或 Grid::new("名字")/from_id_salt 显式指定——
//     这就是 id/Id 与 widget 状态持久化的关系：钥匙稳，记忆才稳。
// 顺带对照保留模式：那边"改数据→控件自己刷新"；这边"改数据→等下一帧整体重描述"，
// 两套心智模型，混着写必翻车。
impl eframe::App for AudioServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 入参速记：ctx = 整窗上下文（重绘请求/内存库/字体都在它身上，调任何
        // request_repaint 都走它）；_frame = eframe 的窗口帧句柄（本程序没用到，
        // 下划线开头表示"参数在但我不读"）；&mut self = 那个装着全部界面状态的 App。
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(lang::t("app.title")));
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

        let had_events = self.poll_server_events(ctx);
        let is_running = self.server_status == ServerStatus::Running;

        // ── 布局系统第二课：Panel / Frame / Area / Window 各管什么 ──
        // egui 的"分区"工具按用途分四档：
        //   · TopBottomPanel::top/bottom —— 从窗口上/下缘切走一条固定高度的横带；
        //     同名不同 id 的多个 top 按【声明顺序】从上往下叠（下面正好三条 top）。
        //   · SidePanel::left/right —— 左右竖带；本窗口窄（420 逻辑像素）用不上，原理同 top。
        //   · CentralPanel —— "剩下的全给我"：所有边缘带瓜分后剩余的中央矩形。
        //   · Frame —— 不是分区，是"带背景/边框/圆角/内边距的一块画布"，卡片全靠它（见卡片注释）。
        //   · Area / Window —— 浮动层：Area 无框随处漂（ComboBox 的弹出菜单就是 Area），
        //     Window 是带标题可拖动的浮动小窗；本程序没有用它们，加弹窗时再找。
        // 顺序铁律：top/bottom 写在 CentralPanel【之前】。egui 按代码顺序切矩形，
        // 反过来 CentralPanel 先拿走整窗，后声明的边带就没地可站了。
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
                // Spacing = "同一容器里相邻控件之间留多少缝"，item_spacing 是 vec2(横向, 纵向)。
                // spacing_mut() 改的只是这个面板这一棵子树（即时模式改样式不外溢），
                // 影响之后创建的所有自动布局控件。想手动插一段空：ui.add_space(8.0) 一次性
                // 顶出固定高度；ui.separator() 则画一条浅灰横线并自带间距（连接卡片里在用）。
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                match self.active_tab {
                    AppTab::Connection => self.show_connection_tab(ui),
                    // v3.8：设置页套一层滚动区。这一页现在最长（音频 / 网络 / 语言 /
                    // 配置文件四张卡片），420x540 的默认窗口高度装不下，卡片底部会被
                    // 直接裁掉 —— 用户看不到"配置文件在哪"这句话，等于白加。
                    // 只套这一页：日志页自己管滚动（show_log_tab 用 available_height
                    // 算高度），外面再套一层会和它打架。
                    // auto_shrink([false,false]) = 横竖都占满剩余空间，保持原来的整宽卡片样式。
                    AppTab::Settings => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| self.show_settings_tab(ui, is_running));
                    }
                    AppTab::Log => self.show_log_tab(ui),
                }
            });

        // ── v3.7 CPU 优化：按需重绘（这里原来是满帧空转）──────────────────
        // 原来这一句是【无条件】ctx.request_repaint()，等于告诉 eframe
        // "画完这张马上画下一张" —— 窗口就以能跑到的最高帧率永久重绘。
        // 实测：什么都没干的情况下烧掉 1.15 个 CPU 核心（任务管理器 14~16%，
        // 电源计划直接判定"非常高"），而音频/视频链路本身几乎不占 CPU。
        // 现在分三条：
        //   1) 这一帧确实取到了事件（电平 ≈20Hz、链路统计 1Hz、日志、预览帧）
        //      → 立刻再画一帧，数据一到就上屏，不用等心跳，波形不会变卡；
        //   2) 没取到事件 → 只挂一个定时心跳兜底，让顶栏运行时长照常走秒；
        //   3) 心跳看"屏幕上有活的东西"：录音/推流中 250ms（红点呼吸 +
        //      万一事件稀疏也不僵），完全空闲 1000ms。
        // 不用操心"空闲不重绘会不会点不动"：鼠标移动、点击、下拉框开合
        // 这些交互由 winit 输入事件和 egui 自己的动画机制触发重绘，
        // 与本条心跳无关。门禁页早就是这套写法（见上面 env_gate 分支），
        // 主界面是漏改的那一处。
        // 把上面三条翻译成 API：request_repaint()="画完这帧尽快画下一帧"，
        // request_repaint_after(d)="最迟 d 之后必画一帧"；两者排队不叠加，
        // 一帧里调一百次也只排一帧。窗口最小化时 eframe 自动停掉所有重绘，
        // 这也是按需模式白赚的一档（满帧循环时代最小化照烧 CPU）。
        if had_events {
            ctx.request_repaint();
        } else {
            let busy = self.mic_live || self.cam_live;
            let every_ms = if busy { 250 } else { 1000 };
            ctx.request_repaint_after(std::time::Duration::from_millis(every_ms));
        }
    }

    // 窗口关闭时的最后回调（App trait 的第二个钩子，整个生命周期只跑一次——
    // 对比 update 的"每帧一次"）。往命令通道塞一条 Stop 让服务线程自行收尾；
    // stop_server 不 join 等线程死透，关窗流程绝不在这段时间卡住。
    // 参数 _gl = 底层 OpenGL 上下文（需要手动释放 GPU 资源才用得到，我们没有）。
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
    // 【v3.10】jpeg 实际是 [方向标记][JPEG...]：先拆标记，解码后按它摆正。
    // GUI 预览只有 1fps，在这里转一次 RGB 完全无感（旋转已经从手机搬到 PC）。
    let (orient, jpeg_only) = crate::vcam::split_orient(jpeg);
    let mut decoder = jpeg_decoder::Decoder::new(jpeg_only);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (pixels, w, h) = crate::vcam::orient_bytes(
        &pixels,
        info.width as i32,
        info.height as i32,
        3, // RGB
        orient,
    );
    let image = egui::ColorImage::from_rgb([w as usize, h as usize], &pixels);
    Some(ctx.load_texture("cam_frame", image, egui::TextureOptions::LINEAR))
}

// 日志页时间戳：手写 HH:MM:SS，不用 chrono 库——只为一个秒针不值得多一个依赖。
// 算法：取 Unix 纪元（1970-01-01 UTC 零点）到现在的总秒数，%86400（一天=24h×3600s）
// 得"今天第几秒"，再拆成时/分/秒。
// ⚠ 注意：这条链全程是 UTC，没有换算本地时区——日志页时间可能比你手表慢/快几个钟头。
// 只影响界面显示，磁盘 audioserver.log 的时间由 log 框架另写，不改代码，留给你决策。
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
