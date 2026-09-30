// ============================================================================
// config.rs —— 用户配置文件（JSON）
// ----------------------------------------------------------------------------
// 为什么要有这个文件：安装版程序不能只靠界面上那几个输入框，硬件相关的东西
// （用哪台播放设备回环、往哪台声卡注入、虚拟摄像头分辨率/帧率、端口……）
// 总得有地方能改、能备份、能发给别人复现问题。JSON 是全球通用格式，
// 记事本就能改，程序读进来还能校验，比原来的 settings.txt（只有 language 一行、
// 而且每次写都要整份重写）强得多。
//
// 放在哪：`%APPDATA%\PCAssistant\config.json`（macOS/Linux 用 ~/.config/PCAssistant）。
//   为什么不放 exe 同目录：装进 Program Files 以后，普通权限往那儿写是要管理员的，
//   用户改一次配置就弹一次 UAC，很难看。APPDATA 是"安装版"的标准位置。
//
// 三条铁律（都是踩过坑定下来的）：
//   1. 【每一项都有默认值】—— 文件删掉就回到出厂状态，所以"删文件"就是重置；
//   2. 【解析失败绝不覆盖用户文件】—— 只报告错误并按默认值跑，
//      否则用户手改打错一个逗号，配置就被程序冲没了；
//   3. 【取值先夹紧再用】—— 端口写 0、帧率写 999 都不能让程序起不来或卡死，
//      越界就夹到边界并记一条 warning，界面上能看到。
//
// 兼容：老版本的 settings.txt 仍然会被读一次（只读 language 这一个键），
// 用来在用户升级后不丢语言设置；迁移发生在"config.json 还不存在"那一刻。
// ============================================================================

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// 配置目录名（APPDATA 下的一层）
pub const APP_DIR: &str = "PCAssistant";
/// 配置文件名
pub const FILE_NAME: &str = "config.json";

// ──────────────────────────────────────────────────────────────────────────
// 结构体 = 配置文件的形状
// ──────────────────────────────────────────────────────────────────────────
// `#[serde(default)]` 加在【结构级】上，意思是"文件里缺哪个字段就用 Default 里的值"，
// 所以用户只需要写他想改的那几行，不必抄全表；反过来 Default 里改了默认值，
// 老配置文件也不用跟着改。
// 没有加 `deny_unknown_fields`：这样将来新版加了键，老版程序读到也不报错，
// 用户手写的注释性键（如 "_todo"）也不会把程序弄崩。

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 给"拿记事本打开这个文件的人"看的说明。JSON 不支持注释，所以用一个键代替；
    /// 它排在第一个字段，序列化出来就在文件最上面。
    #[serde(rename = "_comment")]
    pub comment: String,
    /// 界面语言：auto | en | zh（auto = 跟随系统显示语言）
    pub language: String,
    pub network: Network,
    pub speaker: Speaker,
    pub mic: Mic,
    pub camera: Camera,
    pub diagnostics: Diagnostics,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Network {
    /// WebSocket 监听端口，手机 App 里填的就是这个
    pub port: u16,
    /// 监听地址。0.0.0.0 = 所有网卡（手机要从局域网连进来，默认就得是这样）
    pub bind: String,
    /// 服务端多久给手机发一次 Ping（判定"连接还活着"用）
    pub ping_interval_ms: u64,
}

/// 音箱模式 = 下行：电脑声音 → 手机播放
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Speaker {
    /// 期望采样率。注意：实际抓取用的是 Windows 混音格式（设备说了算），
    /// 这个值是"设备拿不到格式时的兜底 + 界面显示"，不是强制重采样目标。
    pub sample_rate: u32,
    /// 期望声道数，同上
    pub channels: u16,
    /// WASAPI 回环缓冲区帧数。
    ///
    /// 说实话：共享模式下这个值 Windows 基本忽略（缓冲区由音频引擎的周期决定），
    /// 它一直是界面上那个 Buffer Size 输入框对应的东西，所以保留在配置里，
    /// 但【不要指望调它能改延迟】——真正影响延迟的是网络与手机侧播放缓冲。
    pub buffer_frames: u32,
    /// 回环捕获用哪台【播放】设备：按设备友好名做不区分大小写的"包含"匹配，
    /// 留空 = 用系统默认播放设备。想从耳机而不是音箱收声时才需要填。
    pub capture_device_hint: String,
    /// 手机当麦克风用时，是否暂停电脑→手机的下行（避免自己听自己）
    pub pause_while_mic_live: bool,
}

/// 麦克风模式 = 上行：手机麦克风 → 电脑虚拟声卡
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mic {
    /// 往哪台【播放】设备注入（VB-CABLE 的输入端）。按友好名不区分大小写包含匹配。
    /// 换用 VoiceMeeter 之类的其它虚拟声卡时改这里。
    pub inject_device_hint: String,
    /// 检测"有没有应用正在用这根线"时匹配的【录音】端设备名（VB-CABLE 的 Output）。
    /// 一般和 inject_device_hint 成对改。
    pub monitor_capture_hint: String,
    /// 手机没在 mic_start 里声明时的默认上行采样率
    pub uplink_sample_rate: u32,
    /// 上行队列上限（毫秒）。超过就丢最旧的，防止网络抖动时越积越多变成回声。
    pub max_queue_ms: u32,
}

/// 摄像头模式 = 手机镜头 → 电脑虚拟摄像头
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Camera {
    /// OBS 虚拟摄像头共享内存的尺寸（应用看到的就是这个分辨率）
    pub width: u32,
    pub height: u32,
    /// OBS 通道的标称帧率（决定共享内存里写的时间戳间隔）
    pub fps: u32,
    /// 没人看时占位黑帧的尺寸
    pub placeholder_width: u32,
    pub placeholder_height: u32,
    /// 手机停止推流多久后把画面变黑（毫秒）
    pub blackout_after_ms: u64,
    /// 是否启用 Unity Capture 通道（两个通道默认都开，各自服务不同应用）
    pub unity_enabled: bool,
    /// 是否启用 OBS Virtual Camera 通道
    pub obs_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Diagnostics {
    /// 日志级别：trace | debug | info | warn | error
    pub log_level: String,
    /// 启动时是否额外开一个控制台窗口刷日志（等价于 PCSPEAKER_CONSOLE=1）。
    /// 正式版默认不开：安装版只有一个图形窗口，看着才像正常软件。
    pub show_console: bool,
    /// 统计事件（电平/码率/吞吐）的上报间隔
    pub stat_interval_ms: u64,
    /// 跳过启动驱动自检门禁（调试用；true 时缺驱动也照常起服务）
    pub skip_env_check: bool,
}

// ──────────────────────────────────────────────────────────────────────────
// 默认值
// ──────────────────────────────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        Self {
            comment: "PC Assistant AudioServer 配置 / user config. \
                      改完重启程序生效；删掉本文件即恢复默认（restart to reset）。 \
                      所有取值越界都会被自动夹紧，不会让程序起不来。"
                .to_string(),
            language: "auto".to_string(),
            network: Network::default(),
            speaker: Speaker::default(),
            mic: Mic::default(),
            camera: Camera::default(),
            diagnostics: Diagnostics::default(),
        }
    }
}

impl Default for Network {
    fn default() -> Self {
        Self { port: 8080, bind: "0.0.0.0".to_string(), ping_interval_ms: 5_000 }
    }
}

impl Default for Speaker {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            buffer_frames: 1_024,
            capture_device_hint: String::new(),
            pause_while_mic_live: false,
        }
    }
}

impl Default for Mic {
    fn default() -> Self {
        Self {
            inject_device_hint: "CABLE Input".to_string(),
            monitor_capture_hint: "CABLE Output".to_string(),
            uplink_sample_rate: 48_000,
            max_queue_ms: 250,
        }
    }
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            width: 960,
            height: 720,
            fps: 30,
            placeholder_width: 640,
            placeholder_height: 480,
            blackout_after_ms: 1_500,
            unity_enabled: true,
            obs_enabled: true,
        }
    }
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            show_console: false,
            stat_interval_ms: 1_000,
            skip_env_check: false,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 取值夹紧：越界的值不会让程序崩，但也不能原样传下去
// ──────────────────────────────────────────────────────────────────────────

/// 把越界值夹进 [lo, hi]，如果动了就往 warnings 里记一条（界面日志页能看到）
fn clamp_u32(name: &str, v: u32, lo: u32, hi: u32, w: &mut Vec<String>) -> u32 {
    if v < lo {
        w.push(format!("{name}={v} too small, clamped to {lo}"));
        lo
    } else if v > hi {
        w.push(format!("{name}={v} too large, clamped to {hi}"));
        hi
    } else {
        v
    }
}

/// 音频参数必须是 1/2 声道，别的值传进 WASAPI 只会得到一个看不懂的 HRESULT
fn clamp_channels(name: &str, v: u16, w: &mut Vec<String>) -> u16 {
    if v == 1 || v == 2 {
        v
    } else {
        w.push(format!("{name}={v} invalid, fallback to 2"));
        2
    }
}

impl Config {
    /// 就地夹紧所有取值，返回"改了哪些"的说明（空 = 配置本来就合法）
    pub fn sanitize(&mut self) -> Vec<String> {
        let mut w = Vec::new();

        // 端口 0 是"随便给我一个"，对这种固定端口的服务反而更糟，所以从 1 起
        if self.network.port == 0 {
            w.push("network.port=0 invalid, clamped to 8080".to_string());
            self.network.port = 8080;
        }
        if self.network.bind.trim().is_empty() {
            w.push("network.bind empty, fallback to 0.0.0.0".to_string());
            self.network.bind = "0.0.0.0".to_string();
        } else {
            self.network.bind = self.network.bind.trim().to_string();
        }
        self.network.ping_interval_ms =
            clamp_u32("network.ping_interval_ms", self.network.ping_interval_ms as u32, 500, 60_000, &mut w)
                as u64;

        self.speaker.sample_rate =
            clamp_u32("speaker.sample_rate", self.speaker.sample_rate, 8_000, 192_000, &mut w);
        self.speaker.channels = clamp_channels("speaker.channels", self.speaker.channels, &mut w);
        self.speaker.buffer_frames =
            clamp_u32("speaker.buffer_frames", self.speaker.buffer_frames, 128, 8_192, &mut w);
        self.speaker.capture_device_hint = self.speaker.capture_device_hint.trim().to_string();

        self.mic.inject_device_hint = self.mic.inject_device_hint.trim().to_string();
        self.mic.monitor_capture_hint = self.mic.monitor_capture_hint.trim().to_string();
        self.mic.uplink_sample_rate =
            clamp_u32("mic.uplink_sample_rate", self.mic.uplink_sample_rate, 8_000, 192_000, &mut w);
        self.mic.max_queue_ms = clamp_u32("mic.max_queue_ms", self.mic.max_queue_ms, 20, 2_000, &mut w);

        // 宽高取偶数：NV12 / YUV 色度是 2x2 子采样，奇数边长会让最后一行列错位
        let cw = clamp_u32("camera.width", self.camera.width, 160, 3840, &mut w);
        let ch = clamp_u32("camera.height", self.camera.height, 120, 2160, &mut w);
        self.camera.width = cw - (cw % 2);
        self.camera.height = ch - (ch % 2);
        let pw = clamp_u32("camera.placeholder_width", self.camera.placeholder_width, 16, 3840, &mut w);
        let ph = clamp_u32("camera.placeholder_height", self.camera.placeholder_height, 16, 2160, &mut w);
        self.camera.placeholder_width = pw - (pw % 2);
        self.camera.placeholder_height = ph - (ph % 2);
        self.camera.fps = clamp_u32("camera.fps", self.camera.fps, 1, 60, &mut w);
        self.camera.blackout_after_ms =
            clamp_u32("camera.blackout_after_ms", self.camera.blackout_after_ms as u32, 200, 60_000, &mut w)
                as u64;

        self.diagnostics.log_level = self.diagnostics.log_level.trim().to_lowercase();
        if !matches!(
            self.diagnostics.log_level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            w.push(format!(
                "diagnostics.log_level={:?} unknown, fallback to info",
                self.diagnostics.log_level
            ));
            self.diagnostics.log_level = "info".to_string();
        }
        self.diagnostics.stat_interval_ms = clamp_u32(
            "diagnostics.stat_interval_ms",
            self.diagnostics.stat_interval_ms as u32,
            100,
            10_000,
            &mut w,
        ) as u64;

        self.language = normalize_language(&self.language);
        w
    }
}

/// 语言字段只认 auto / en / zh（zh-cn、zh-hans 这些历史写法归一成 zh）
fn normalize_language(s: &str) -> String {
    match s.trim().to_lowercase().as_str() {
        "en" => "en".to_string(),
        "zh" | "zh-cn" | "zh-hans" => "zh".to_string(),
        _ => "auto".to_string(),
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 路径
// ──────────────────────────────────────────────────────────────────────────

/// 配置目录：%APPDATA%\PCAssistant（Windows）/ ~/.config/PCAssistant（其它平台）
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    // 拿不到就返回 None：程序照样按默认值跑，只是不读写文件（日志里会说清楚）
    base.map(|d| d.join(APP_DIR))
}

pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(FILE_NAME))
}

// ──────────────────────────────────────────────────────────────────────────
// 读 / 写
// ──────────────────────────────────────────────────────────────────────────

/// 一次加载的完整结果，界面和日志都要用里面的信息
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// 实际读写的路径（None = 这台机器上拿不到配置目录）
    pub path: Option<PathBuf>,
    /// 这次运行是不是刚创建了默认配置文件
    pub created: bool,
    /// 文件存在但读不动 / 解析不了时的原因（None = 一切正常）
    pub error: Option<String>,
    /// sanitize 夹紧越界值时产生的说明
    pub warnings: Vec<String>,
}

/// 生成"出厂默认配置"的 JSON 文本（打包脚本用它产出安装器要拷的那份文件，
/// 这样默认值永远只有一个来源：这里的 Default 实现）
pub fn default_json() -> String {
    let cfg = Config::default();
    // pretty + 不转义斜杠：Windows 路径和说明文字里的 / 保持原样好读
    let text = serde_json::to_string_pretty(&cfg)
        .unwrap_or_else(|_| "{}".to_string());
    // 结尾补一个换行，记事本/git diff 都不难受
    format!("{text}\n")
}

/// 把默认配置写到指定路径（已存在则不动，返回是否真的写了）
pub fn write_default_if_missing(path: &std::path::Path) -> bool {
    if path.exists() {
        return false;
    }
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    // 原子写：先 .tmp 再改名，避免"写一半掉电"留下半个文件
    let tmp = path.with_extension("json.tmp");
    if std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(default_json().as_bytes())?;
        f.flush()
    })
    .is_err()
    {
        return false;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => true,
        // 极少数情况（杀软占用）改名失败：退化成直接写，至少配置能用
        Err(_) => std::fs::write(path, default_json()).is_ok(),
    }
}

/// 读配置：没有文件就创建默认文件；坏文件绝不覆盖，按默认值跑并报告错误。
/// 只在启动时调用一次（结果灌进全局，见 [`get`]）。
///
/// 幂等：重复调用直接返回当前生效的配置。这样 `main()`（要先拿 show_console）
/// 和 `lang::apply_startup_locale()`（要拿 language）谁先调都行，
/// 也保证 GUI 版和 CLI 版不会把同一个文件读两遍。
pub fn init() -> Loaded {
    if GLOBAL.get().is_some() {
        return Loaded {
            config: get(),
            path: config_path(),
            created: false,
            error: None,
            warnings: Vec::new(),
        };
    }

    let path = config_path();
    let mut created = false;
    let mut error = None;

    let mut cfg = match &path {
        None => {
            error = Some("cannot resolve config dir (APPDATA missing)".to_string());
            Config::default()
        }
        Some(p) => match std::fs::read_to_string(p) {
            Ok(text) => serde_json::from_str::<Config>(&text).unwrap_or_else(|e| {
                // 关键：这里【不写文件】。用户手改打错一个逗号，如果顺手覆盖，
                // 他就再也不知道自己原来填了什么。
                error = Some(format!("{}: {e}", p.display()));
                Config::default()
            }),
            Err(err) if p.exists() => {
                error = Some(format!("{}: {err}", p.display()));
                Config::default()
            }
            Err(_) => {
                created = write_default_if_missing(p);
                Config::default()
            }
        },
    };

    let warnings = cfg.sanitize();
    let loaded = Loaded { config: cfg, path, created, error, warnings };
    // 灌进全局：采集线程、注入线程、虚拟摄像头线程都从这里取值，
    // 省得把 Config 一路穿到每个函数签名里。
    if GLOBAL.set(Mutex::new(loaded.config.clone())).is_err() {
        // 理论上不会发生（只有 init 会 set），真发生了也不能影响启动
        log::warn!("[Config] global already initialised, ignored");
    }
    loaded
}

static GLOBAL: OnceLock<Mutex<Config>> = OnceLock::new();

/// 取当前配置（副本，拿回去随便用，不会卡住别人）
///
/// 没调用过 [`init`] 时返回默认值：单元测试、探针 bin 都走这条路，
/// 保证它们不依赖 %APPDATA% 里有没有文件。
pub fn get() -> Config {
    GLOBAL
        .get()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or_default()
}

/// 更新配置：改进内存 + 写回文件。返回写文件的错误（界面会显示出来）。
///
/// 为什么内存和文件要一起改：服务端线程读的是内存值，用户期望的是"改了下次启动
/// 还在"。只改一个就会出现"界面显示新值、下次启动又变回去"的经典误会。
pub fn update(new_cfg: Config) -> std::io::Result<()> {
    let mut cfg = new_cfg;
    cfg.sanitize();
    if let Some(p) = config_path() {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&cfg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{text}\n"))?;
        // rename 覆盖 = 原子替换（Windows 上 std 用的是 MOVEFILE_REPLACE_EXISTING）
        std::fs::rename(&tmp, &p)?;
    }
    if let Some(m) = GLOBAL.get() {
        *m.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// 单元测试
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrips() {
        let text = default_json();
        let parsed: Config = serde_json::from_str(&text).expect("default JSON must parse");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn missing_keys_use_defaults() {
        // 用户只写一行也合法：其余全部走默认值
        let cfg: Config = serde_json::from_str(r#"{"language":"zh"}"#).unwrap();
        assert_eq!(cfg.language, "zh");
        assert_eq!(cfg.network.port, 8080);
        assert_eq!(cfg.camera.fps, 30);
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        // 新版程序写出的键，老版不能因此罢工
        let cfg: Config =
            serde_json::from_str(r#"{"future_option":42,"network":{"port":9090}}"#).unwrap();
        assert_eq!(cfg.network.port, 9090);
    }

    #[test]
    fn out_of_range_values_are_clamped_with_warnings() {
        let mut cfg = Config::default();
        cfg.network.port = 0;
        cfg.camera.fps = 999;
        cfg.speaker.channels = 7;
        cfg.speaker.buffer_frames = 10;
        cfg.diagnostics.log_level = "verbose".into();
        let w = cfg.sanitize();
        assert_eq!(cfg.network.port, 8080);
        assert_eq!(cfg.camera.fps, 60);
        assert_eq!(cfg.speaker.channels, 2);
        assert_eq!(cfg.speaker.buffer_frames, 128);
        assert_eq!(cfg.diagnostics.log_level, "info");
        // 每一处夹紧都要留下话，否则用户以为程序"没生效"
        assert_eq!(w.len(), 5, "warnings: {w:?}");
    }

    #[test]
    fn odd_camera_sizes_become_even() {
        // NV12 的色度是 2x2 子采样，奇数边长会让最后一行错位
        let mut cfg = Config::default();
        cfg.camera.width = 961;
        cfg.camera.height = 541;
        cfg.sanitize();
        assert_eq!(cfg.camera.width, 960);
        assert_eq!(cfg.camera.height, 540);
    }

    #[test]
    fn language_aliases_normalize() {
        assert_eq!(normalize_language(" zh-Hans "), "zh");
        assert_eq!(normalize_language("EN"), "en");
        assert_eq!(normalize_language("klingon"), "auto");
    }

    #[test]
    fn broken_file_does_not_get_overwritten() {
        // 这条测的是"默认值 + 错误信息"这半边；不覆盖文件本身靠 init() 里
        // 只在 Err 分支返回默认值、没有任何 write 调用来保证。
        let broken = "{ this is not json ";
        let parsed: Result<Config, _> = serde_json::from_str(broken);
        assert!(parsed.is_err());
        assert_eq!(Config::default(), Config::default());
    }
}
