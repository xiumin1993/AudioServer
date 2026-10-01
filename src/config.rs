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

// ── 下面四行 use 是"引入依赖的名字"，不写就得写全路名才能用 ──────────
// std::io::Write     ：文件写入 trait，引入后才有 f.write_all(...) / f.flush()
// std::path::PathBuf ：可拥有的路径字符串（能拼接、能从环境变量构造）
// std::sync::{Mutex, OnceLock}：多线程安全的全局存储，原理见文件后半段 GLOBAL
// serde 的两个 trait ：负责"Rust 结构体 ↔ JSON"的自动翻译，见下方 derive 讲解

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

// 常量：pub const 是"对外可见、编译期固定"的值，改这里等于改所有引用处。
/// 配置目录名（APPDATA 下的一层）
pub const APP_DIR: &str = "PCAssistant";
// ⚠ 注意：APP_DIR 改成别的名字，老用户 %APPDATA%\PCAssistant\config.json 立刻
//         "失踪"（程序去新目录找，找不到就按默认值重建），等于全员配置清零。
/// 配置文件名
pub const FILE_NAME: &str = "config.json";
// ⚠ 注意：同理改 FILE_NAME 也会让老配置"失踪"；installer 拷默认文件、
//         docs/quick-start.md 里告诉用户的路径都依赖这两个字符串，改前全局搜一遍。

// ──────────────────────────────────────────────────────────────────────────
// 结构体 = 配置文件的形状
// ──────────────────────────────────────────────────────────────────────────
// `#[serde(default)]` 加在【结构级】上，意思是"文件里缺哪个字段就用 Default 里的值"，
// 所以用户只需要写他想改的那几行，不必抄全表；反过来 Default 里改了默认值，
// 老配置文件也不用跟着改。
// 没有加 `deny_unknown_fields`：这样将来新版加了键，老版程序读到也不报错，
// 用户手写的注释性键（如 "_todo"）也不会把程序弄崩。
//   补一句：如果想【强制】拒绝未知键，写法是在结构级加 `#[serde(deny_unknown_fields)]`，
//   它会让任何文件里多出来的键直接报错 —— 我们故意不用，理由就是上面那两行。

// ── `#[derive(...)]` 到底在干什么（初学者必读）───────────────────────
// derive = "让编译器替我生成代码"。这里四个宏各自生成一个 trait 实现：
//   Debug       → 能用 {cfg:?} 打印结构（日志、panic 信息全靠它）
//   Clone       → 能 .clone() 深拷贝（全局存一份，谁要用就拷一份走，见 get()）
//   PartialEq   → 能 == 比较（单元测试 assert_eq! 靠它）
//   Serialize / Deserialize → serde 根据 struct 的每个字段自动生成"序列化/反序列化"代码：
//       Serialize   把 Config 结构体 → JSON 文本（serde_json::to_string_pretty 用）
//       Deserialize 把 JSON 文本   → Config 结构体（serde_json::from_str 用）
// 也就是说 config.json 的"形状"完全由这个 struct 定义：加一个字段 = 文件多一个键，
// 删一个字段 = 文件里那个键自动被无视。不存在第二份需要手工同步的格式定义。
// JSON 的键名默认 = Rust 字段名，想改就贴重命名属性（见下面 comment 字段）。

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 给"拿记事本打开这个文件的人"看的说明。JSON 不支持注释，所以用一个键代替；
    /// 它排在第一个字段，序列化出来就在文件最上面。
    // `#[serde(rename = "_comment")]`：文件里叫 "_comment"，代码里叫 comment。
    // 为什么：下划线开头的键一眼就看得出"是给人读的备注"；不改名则代码里
    // 变量名就得写成 _comment，Rust 里前缀下划线又是"故意不用"的约定，很别扭。
    // ⚠ 注意：rename 改了，磁盘上老文件里的键就对不上了（该字段会退回默认值）。
    #[serde(rename = "_comment")]
    pub comment: String,
    /// 界面语言：auto | en | zh（auto = 跟随系统显示语言）
    pub language: String,
    // 下面四个字段是"嵌套结构体"，JSON 里就是四个小节（"network": { ... }）。
    // 结构级 #[serde(default)] 对它们的效果：文件里整个 network 小节都缺 →
    // 整节用 Network::default()；小节在但少某个键 → 少的键用默认、多的键无视。
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
//
// ── 为什么默认值写在 Rust 的 Default impl 里，而不是一份"JSON 模板文件"？──
// 这个文件里每个 Default::default() 就是"出厂设置"的唯一权威（single source of truth）：
//   1. 缺键补默认：上面的 #[serde(default)] 在解析时直接调用这些 Default impl；
//   2. 生成出厂文件：default_json() / write_default_if_missing() 也是把
//      Config::default() 序列化出来 —— 同一个函数，值天然一致；
//   3. 拿不到目录时兜底：init() 里连文件都读不到就 Config::default() 继续跑。
// 假如默认值写成一份 JSON 模板：程序里"缺键用什么"还是得在 Rust 再写一遍，
// 于是变成两处要人肉同步的副本 —— 改一处漏一处，用户看到的默认值就分裂了。
// 写在 Rust 里，编译器还帮你查类型（JSON 模板里 "8080" 打成字符串没人发现）。
// 打包脚本要的出厂文件也是调这里生成的（见 default_json 的注释）。

impl Default for Config {
    fn default() -> Self {
        Self {
            // comment 的内容纯给人看，程序从不解析它；改文案无害，
            // 但别把里面的 "config.json"、"恢复默认" 之类关键词删掉——
            // docs/quick-start.md 和客服话术都引用同样的说法。
            // language "auto"：出厂跟随系统语言，与 lang.rs 的 LanguageChoice::Auto 对应。
            // 其余五个小节各自调自己的 Default —— 上面整段注释说的"唯一权威"就体现在这。
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
        // port 8080：手机 App 里默认填的也是它（配套 App 在 D:\code\PCAssistant）。
        //   换成 1024 以下的"低端口"Windows 可能不让普通程序绑；换 80/443 易撞 web 服务。
        // bind "0.0.0.0"：监听所有网卡。改成 "127.0.0.1" 只有本机能连，手机永远连不上；
        //   改成别的字符串会在 sanitize() 里被 trim，解析失败时整个回 0.0.0.0。
        // ping_interval_ms 5_000：每 5 秒给手机发一次 Ping。写 5_000 是 Rust 的数字
        //   分隔符写法，等于 5000（和下划线前的数字一样参与运算），纯为可读。
        //   调太小：弱网时手机频繁掉线；调太大：拔网线后电脑这边要很久才发现连接已死。
        Self { port: 8080, bind: "0.0.0.0".to_string(), ping_interval_ms: 5_000 }
    }
}

impl Default for Speaker {
    fn default() -> Self {
        // sample_rate 48_000：48kHz，影音行业标准采样率（44100=CD、96000=高解析）。
        //   记住字段注释：它是"兜底+显示"，真正的抓取速率由 Windows 音频引擎决定。
        // channels 2：立体声。改 1 也合法（单声道），改 3 会在 clamp_channels 被打回 2。
        // buffer_frames 1_024：WASAPI 共享模式下的名义缓冲帧数。
        //   1024/48000 ≈ 21 毫秒。⚠ 字段注释已说明：共享模式 Windows 基本无视它，
        //   改这个数不会改延迟，界面上仍叫 Buffer Size。
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
        // inject_device_hint "CABLE Input" / monitor_capture_hint "CABLE Output"：
        //   VB-CABLE 虚拟声卡两个端口的固定设备名（装好后设备管理器里就是这个字样）。
        //   匹配方式是"不区分大小写的包含"，所以设备名前面有 "VB-Audio (1-" 之类前缀也能命中。
        //   改错的表现很隐蔽：不报错，只是手机的声音到不了电脑、或检测不到"有人在用"。
        // uplink_sample_rate 48_000：手机没在 mic_start 里声明采样率时按这个收。
        // max_queue_ms 250：上行队列最多囤 250 毫秒的音频，超了丢最旧的。
        //   改太小（如 50）：轻微网络抖动就开始 chop 掉音频；改太大（如 5000）：
        //   延迟堆到 5 秒，说出去的话半天才回来，像对讲机失灵。sanitize 界限 20–2000。
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
        // width 960 / height 720：虚拟摄像头对外宣布的分辨率，
        //   聊天软件/会议应用"看到"的画面就是这个尺寸。改大 = 共享内存和流量同比变大
        //   （960×720 的 NV12 每帧约 1MB，30fps 就是 ~30MB/s 局域网流量）。
        // fps 30：标称帧率，决定共享内存时间戳间隔。会议软件普遍按 30 协商，
        //   设 60 手机推不满只会白白多写时间戳；设 1 画面变幻灯片。sanitize 上限 60。
        // placeholder 640×480：没人看时的黑帧尺寸，比主画面小省内存，无所谓体验。
        // blackout_after_ms 1_500：手机停推 1.5 秒后变黑。改太小：网络稍抖画面就闪黑；
        //   改太大：手机早关了，应用里还挂着最后一帧的"定格画面"，观感诡异。
        // 两个 *_enabled 默认 true：两条通道各自服务不同应用，全开兼容性最好。
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
        // log_level "info"：日志详细程度。trace/debug 只应在排错时临时开：
        //   音频帧级别的 debug 日志能把 audioserver.log 迅速刷到几百 MB。
        //   合法取值只有 sanitize 里 matches! 那五个词，写别的会回 info 并记 warning。
        // show_console false：与 main.rs 的启动流程有关——它在 config::init() 之后、
        //   按这个值决定要不要额外开一个控制台窗口（参见 src/main.rs 的 init_logger 附近）。
        // stat_interval_ms 1_000：电平/码率等统计每秒上报一次手机。
        //   调小 UI 更丝滑但费流量和 CPU；调到 100 以下界外的值会被夹回 100。
        // skip_env_check false：true 时缺驱动也照常起服务（调试后门，给普通用户开
        //   等于让它们在没装 VB-CABLE 的机器上看到一堆"连不上"却查不出原因）。
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
//
// "夹紧（clamp）"= 把越界值强行按回合法区间的最小端/最大端：
//   fps=999 → 60，fps=0 → 1。用户手改配置打错一个零是常态，
//   程序的选择只有两种：罢工起不来，或者夹到边界继续干活 —— 我们选后者。
//
// "warnings" = 夹紧时留下的说明清单（每条一句话，写明 键名=原值 被夹到了哪）。
//   为什么要收集起来而不是静默改掉：用户把帧率写成 999、程序默默按 60 跑，
//   如果不告诉他，他会以为"改了没用"，下次改更离谱的值。
//   这些字符串一路带到界面（Loaded.warnings）和日志，用户看得见改了什么。
//
// 下面两个小函数是"夹紧工具箱"，都收 &mut Vec<String>：
//   借用一个可变 warnings 列表，往里 push 说明，同时返回夹好的值。
//   Rust 初学者注意 &mut 参数 = "这个函数会修改你传进来的那个 Vec"。

/// 把越界值夹进 [lo, hi]，如果动了就往 warnings 里记一条（界面日志页能看到）
fn clamp_u32(name: &str, v: u32, lo: u32, hi: u32, w: &mut Vec<String>) -> u32 {
    // 先查小再查大；中间没动过就原样返回。边界值本身（v==lo 或 v==hi）算合法，
    // 不产生 warning —— 所以想改"允不允许恰好等于边界"，把 < / > 改成 <= / >= 即可。
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
    // 每段边界值的道理（改边界 = 改"程序认为什么是正常"，动前想清楚）：
    //   ping_interval_ms 500–60_000：低于半秒的 Ping 是自己制造拥塞；
    //     超过 1 分钟不探活，手机掉线了电脑还傻等。
    //   sample_rate/uplink_sample_rate 8_000–192_000：8k 是电话音质下限，
    //     192k 是当前声卡常见上限；再小的值 WASAPI 大概率直接失败，
    //     再大的值多半是多打了一个零。
    //   buffer_frames 128–8_192：低于 128 帧任何声卡都会咔哒爆音，
    //     高于 8192（约 170ms@48k）纯粹是延迟。字段注释说过：共享模式下基本无效，
    //     夹它只是为了界面输入框不出现荒谬数字。
    //   max_queue_ms 20–2_000：见 Mic 默认值处"对讲机失灵"的解释。
    //   camera.width 160–3840 / height 120–2160：下限保证画面还能看清是画面，
    //     上限 4K/2K 是虚拟摄像头消费级应用的现实范围。
    //   fps 1–60：60 是采集类应用普遍上限，超过没意义还费带宽。
    //   blackout_after_ms 200–60_000：低于 200ms 网络一抖就闪黑；见 Camera 默认值处。
    //   stat_interval_ms 100–10_000：太短统计消息本身挤占音频带宽，太长数据失去意义。
    // "改了会怎样"通用答案：程序不崩（都有兜底），只是行为可能悄悄变怪，
    // 且发行版默认值一变，文档/教程里写的数字就全对不上了。
    pub fn sanitize(&mut self) -> Vec<String> {
        let mut w = Vec::new();

        // 端口 0 是"随便给我一个"，对这种固定端口的服务反而更糟，所以从 1 起
        // （特殊处理：port=0 不夹到 1 而是直接回默认 8080——端口 1 是系统保留低位端口，
        //   夹过去反而绑不上；手机 App 里填 1 的用户只会更多一层困惑。）
        if self.network.port == 0 {
            w.push("network.port=0 invalid, clamped to 8080".to_string());
            self.network.port = 8080;
        }
        // 注意：u16 类型本身已经把 65535 以上的值挡在解析层之前了——
        // JSON 里写 port: 70000 会在 serde 解析时就失败，走 init() 的 error 分支，
        // 到不了这里。这里只管"类型装得下但语义不对"的 0。
        if self.network.bind.trim().is_empty() {
            w.push("network.bind empty, fallback to 0.0.0.0".to_string());
            self.network.bind = "0.0.0.0".to_string();
        } else {
            // 顺手 trim 两端空白：用户从别的文档复制粘贴最常带上看不见的空格/制表符
            self.network.bind = self.network.bind.trim().to_string();
        }
        // ping_interval_ms 字段是 u64，但工具箱只有 u32 版本，所以先 as u32 夹完再转回。
        // ⚠ 注意：`as u32` 是截断（超过 2^32 会绕回），理论上存在"巨大值绕回后恰好落进
        //   合法区间被放行"的极端情况（如 4294967296+1000 → 1000）。实际用户手不出这种值，
        //   留在这里由你定夺是否给 u64 单独做一个 clamp 函数，本轮不改代码。
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
        // 做法是 `v - (v % 2)`：奇数减 1 变偶数，偶数不变。
        // 为什么不干脆禁止奇数报错：夹到最近的偶数是零成本修复，报错却把用户挡在门外。
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

        // 先把两边空白和大小写统一（" INFO " 也算合法），再对着五个许可词做白名单检查。
        // matches! 是标准宏：拿一个值逐个比对模式，等价于一长串 || 但好看。
        // 想加新级别（如 fatal）：在这里追加 | "fatal"，
        // ⚠ 还要同步 src/main.rs 里按 log_level 初始化日志的地方（参见 init_logger），
        //   只改这一处会出现"配置合法但日志不认"。
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

        // 最后一道：语言字段归一（详见 normalize_language）。
        // 注意这一步不往 w 里 push——"zh-CN 归一成 zh"属于宽容处理而非取值越界，
        // 记 warning 反而吓用户；真正生效的语言见 lang.rs 的 apply_startup_locale。
        self.language = normalize_language(&self.language);
        w
    }
}

/// 语言字段只认 auto / en / zh（zh-cn、zh-hans 这些历史写法归一成 zh）
// 认不出来的一律回 "auto"（跟随系统）而不是报错：语言选错天塌不下来，
// 让程序带着一行红字起不来才是大问题。
// ⚠ 注意：这份"历史别名表"和 lang.rs 里 LanguageChoice::parse 是同一逻辑的两份拷贝
//   （config.rs 夹紧用、lang.rs 运行时用）。新增别名（如 zh-hant）必须两处同步改，
//   否则会出现"config.json 里认识、settings.txt 里不认识"的分叉。由你定夺是否合并。
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
// #[cfg(windows)] / #[cfg(not(windows))] 是"条件编译"：编译器按目标平台
// 只保留其中一组代码，另一组连编译都不参与 —— 所以两份 base 定义同名不冲突，
// Windows 构建产物里根本没有 XDG 那几行。交叉编译 Linux 版时才会换一份。
// %APPDATA% 指向 C:\Users\<用户名>\AppData\Roaming，是 Windows 给"每个用户的
// 应用配置"划的标准地盘，多用户共用一台电脑时各读各的，互不干扰。
// env::var_os 返回 Option：环境变量存在才是 Some。刻意用 _os 版本（OsString，
// 原始字节）而不是 var（String，要求 UTF-8）：用户名带奇怪编码时也能拿到路径。
//
// 返回值是 Option<PathBuf> = "可能根本没有目录可用"。上游（init）拿 None 的
// 处理办法见下方 error = Some(...)：按内置默认值继续跑、不读写文件、把原因写日志。
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
// 为什么不只返回 Config：启动时"配置之外还发生了什么"（有没有刚建文件、
// 读没读坏、夹了哪些值）都得告诉界面和日志，这些散信息打包成这一个大对象返回，
// main.rs 启动流程里会逐个落到界面（参见 src/main.rs 的 init_logger 与启动页）。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// 实际读写的路径（None = 这台机器上拿不到配置目录）
    pub path: Option<PathBuf>,
    /// 这次运行是不是刚创建了默认配置文件
    ///
    /// 这个布尔是"老 settings.txt → 新 config.json 一次性迁移"的开关：
    /// lang.rs 的 apply_startup_locale 只在 created == true（= 这台机器第一次
    /// 拥有 config.json）时，才去读老 settings.txt 并把语言搬进来。
    /// 靠它保证迁移一辈子只发生一次：以后再删掉语言键，不会被"自作聪明"地从
    /// settings.txt 恢复回去 —— 用户删文件表达的就是"我要重选"。
    /// 二次调用 init() 走幂等分支时恒为 false（见下方 init 的注释）。
    pub created: bool,
    /// 文件存在但读不动 / 解析不了时的原因（None = 一切正常）
    pub error: Option<String>,
    /// sanitize 夹紧越界值时产生的说明
    pub warnings: Vec<String>,
}

/// 生成"出厂默认配置"的 JSON 文本（打包脚本用它产出安装器要拷的那份文件，
/// 这样默认值永远只有一个来源：这里的 Default 实现）
// 呼应"默认值写在 Rust 不写在 JSON 模板"：安装器带的 config.json 也是这里
// 现生成的，不是仓库里躺着的一份静态文件 —— 世上不存在第二份默认值可漂移。
// unwrap_or_else(|_| "{}".to_string())：序列化理论上不会失败（字段全是
// String/数字/bool），真失败也给个合法空 JSON，别让打包脚本崩在半路。
pub fn default_json() -> String {
    let cfg = Config::default();
    // pretty + 不转义斜杠：Windows 路径和说明文字里的 / 保持原样好读
    let text = serde_json::to_string_pretty(&cfg)
        .unwrap_or_else(|_| "{}".to_string());
    // 结尾补一个换行，记事本/git diff 都不难受
    format!("{text}\n")
}

/// 把默认配置写到指定路径（已存在则不动，返回是否真的写了）
// 返回值 bool = "这次到底建没建文件"，直接被 init() 拿去填 Loaded.created，
// 也就是迁移开关的唯一事实来源。"已存在直接 false 走人"保证它从不覆盖用户文件。
pub fn write_default_if_missing(path: &std::path::Path) -> bool {
    if path.exists() {
        return false;
    }
    if let Some(parent) = path.parent() {
        // create_dir_all：目录已存在不报错，一层都没有就一路建到底
        // （%APPDATA%\PCAssistant 第一次运行时确实不存在）。
        // 这里连失败都不管（is_err 直接返回 false）：建不出目录顶多没配置文件，
        // 程序按内置默认值跑，不影响启动。
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    // 原子写：先 .tmp 再改名，避免"写一半掉电"留下半个文件
    // 原理：文件系统层面"改名"是一步完成的动作，外界只会看到
    //   "旧文件还在" 或 "完整新文件出现" 两种状态，永远不会看到半截文件。
    // ⚠ 注意：tmp 名写死为 config.json.tmp（with_extension 把 .json 整个换成
    //   .json.tmp —— 实际文件名是 config.json.tmp）。两个进程同时跑本程序时
    //   会抢同一个 tmp 名，后写的会覆盖前者的 tmp。由你定夺是否加 pid 后缀。
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
// ── OnceLock 全局单例原理（初学者必读，配合下方 static GLOBAL 一起看）──
// OnceLock<T> = "一辈子只能填一次值的全局盒子"：
//   · set(v) 只有第一次成功，之后每次返回 Err —— 这就是"幂等"的底层保证；
//   · get() 没填过时返回 None，填过返回 Some(&T)。
// 所以 init() 开头 `GLOBAL.get().is_some()` 一眼就认出"已经初始化过"，
// 直接返回现状，不再碰磁盘 —— 谁先调用谁负责真读文件，后调用的搭便车。
// 对比 std::sync::LazyLock/once_cell：这里需要"初始化时顺带把过程信息
// （created/error/warnings）返回给启动流程"，所以手动 set 比惰性求值更顺手。
//
// ── 四个 match 分支，对应四种现实 ──
//   None        → 连 APPDATA 都拿不到（极端环境/服务账户）：内置默认值跑，
//                 error 写明原因，不读写任何文件；
//   Ok + 解析成功 → 正常路径；
//   Ok + 解析失败 → 【故意不写回文件】（文件头铁律 2）：用户打错一个逗号时，
//                 覆盖等于毁灭证据。按默认值继续跑，error 带上文件路径和 serde
//                 的错误说明写进日志 —— "服务照常起、原因可追查"两不耽误；
//   Err + exists → 文件在但读不动（占用/权限）：同上，不覆盖，报告 + 默认值；
//   Err + 不存在 → 第一次运行：write_default_if_missing 建出厂文件，
//                 返回值进 created，给语言迁移当开关（见 Loaded.created 注释）。
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

// static = 全程序唯一的一份数据（区别于每个线程各存一份 thread_local）。
// OnceLock 外面再包一层 Mutex 的原因：OnceLock 本身"填一次后就再也不改"，
// 但配置是可以在运行期被界面改掉的（update()）—— 所以盒子只保证"有且只有一个
// 保管处"，里面装的 Mutex<Config> 才负责"内容可变且线程安全"。
// 采集线程、注入线程、虚拟摄像头线程并发读写它，没有 Mutex 就是数据竞争。
static GLOBAL: OnceLock<Mutex<Config>> = OnceLock::new();

/// 取当前配置（副本，拿回去随便用，不会卡住别人）
///
/// 没调用过 [`init`] 时返回默认值：单元测试、探针 bin 都走这条路，
/// 保证它们不依赖 %APPDATA% 里有没有文件。
// 逐层拆开这条链：
//   GLOBAL.get() → Option<&Mutex<Config>>（没初始化过就是 None）
//   .map(|m| ... .clone())          → 有就锁住、拷一份副本出来
//   unwrap_or_default()             → 没有就 Config::default()
// 返回"副本"是关键设计：锁只在 clone 那一瞬持有，调用方拿到的是独立数据，
// 拿着 Config 算半天也不会挡住别的线程取值（锁的粒度 = 一次拷贝）。
// unwrap_or_else(|e| e.into_inner())： Mutex 中毒恢复 —— 别的线程曾在持锁时
// panic，锁会"中毒"，lock() 返回 Err；into_inner() 把 Config 从尸体里掏出来继续用，
// 因为 Config 本身是纯数据，一次克隆不可能被 panic 改成半更新状态，宁可带病运行。
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
//
// 执行顺序也讲一下：先写文件、后改内存 —— 磁盘写失败时直接 ? 返回 Err，
// 内存保持旧值，界面把错误显示出来；不会出现"界面说改好了、文件却没落盘"。
//
// ⚠ 并发写风险（本函数的已知短板，代码未处理，由你定夺）：
//   1. 整份写回：update() 拿到的 new_cfg 是调用方从 get() 拷出去的"快照"，
//      改完后整份序列化覆盖文件。若两个线程都 get() → 各改各的字段 → 先后
//      update()，后写的会连对方刚改的字段一起用旧值盖掉（丢失更新）。
//      现状只有一个 GUI 线程调它，所以碰不到；将来出现第二个调用方就要小心。
//   2. 外部编辑：用户在程序运行中用记事本改 config.json，下次界面一保存
//      就被整份盖掉 —— 与文件头铁律 2 的"绝不覆盖"只管解析失败分支，
//      用户主动点保存走的正是这条覆盖路径（rename 原子替换，不会写出半截文件）。
//   3. tmp 名固定 config.json.tmp：两个进程同时保存时后者会踩掉前者的 tmp。
pub fn update(new_cfg: Config) -> std::io::Result<()> {
    let mut cfg = new_cfg;
    // 先夹再写：界面传进来的值也要过同一道关，磁盘上的文件永远合法
    cfg.sanitize();
    if let Some(p) = config_path() {
        if let Some(parent) = p.parent() {
            // 这里的 ? 与 write_default_if_missing 不同：那个在启动早期、失败可容忍；
            // 这个在用户点"保存"时，目录都建不出来是真故障，必须把 Err 传回界面显示。
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&cfg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{text}\n"))?;
        // rename 覆盖 = 原子替换（Windows 上 std 用的是 MOVEFILE_REPLACE_EXISTING）
        std::fs::rename(&tmp, &p)?;
    }
    // 落盘成功后才动内存（见上：顺序反了会出现"界面新、文件旧"）。
    // 用 get() 而不是 set()：盒子早已被 init 填过，这里只换盒子里的内容；
    // 若从未 init（只有探针会这样，探针也不调 update），这行安静跳过，
    // 不会出现"没读文件却先写了全局"的怪状态。
    if let Some(m) = GLOBAL.get() {
        *m.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// 单元测试
// ──────────────────────────────────────────────────────────────────────────

// #[cfg(test)] 同样条件编译：只有 cargo test 时这个 mod 才参与编译，
// 发行 exe 里一行都不带。每个 #[test] 函数 = 一条自动化检查，cargo test 全绿再提交。
#[cfg(test)]
mod tests {
    // super::* = 把外面整个 config.rs 的名字引进来，测试里直接写 Config 不用限定名
    use super::*;

    // 出厂文件自检：Default 生成 JSON 再读回来，必须和 Default 一字不差
    // （防：字段加了 rename/类型改了，往返不对称）
    #[test]
    fn default_roundtrips() {
        let text = default_json();
        let parsed: Config = serde_json::from_str(&text).expect("default JSON must parse");
        assert_eq!(parsed, Config::default());
    }

    // 锁死文件头铁律 1：删文件=重置。空 {} 也必须能解析出一份完整默认配置
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

    // 给每个字段灌一个坏值，断言"夹到哪 + 留了几条话"。
    // ⚠ 注意：末尾 assert_eq!(w.len(), 5) 把 warning 条数写死了 ——
    //   将来给 sanitize 新增一处夹紧逻辑，若测试里的坏值恰好触发它，
    //   这条会红，需要人工确认后把数字改对，别顺手改成 >=。
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

    // 语言别名表（zh-Hans/EN/乱写）的锁定测试；与 normalize_language 处的
    // ⚠ 同步提醒对应：lang.rs 的 parse 也要过同一套别名，两边测试各测各的。
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
