// ============================================================================
// env_check.rs —— 启动环境自检门禁（v3.5 新增）
// ----------------------------------------------------------------------------
// 为什么要有这个文件：
//   本程序自己不装任何驱动，也不往注册表写东西（绿色单 exe）。
//   但三种工作模式里，麦克风模式必须有 VB-CABLE 虚拟音频线，
//   摄像头模式必须有 Unity Capture 或 OBS Virtual Camera 任一虚拟摄像头。
//   以前缺驱动时，程序照样开主界面、照样显示"运行中"，用户连上手机才发现
//   选不到设备 —— 排查成本很高。
//   现在改成：双击 exe 先做一次只读检测；
//     · 必需项齐 → 直接进主界面（行为和以前完全一样）
//     · 必需项缺 → 主窗口只显示"环境准备"向导页，服务端线程【根本不起】，
//                  页面上只给"缺什么 + 去哪下载 + 怎么装"的文字提示，
//                  装完回来点【重新检测】。
//
// 本程序与驱动的关系，一句话：【只检测、只提示】。
//   不随包附带任何安装脚本（没有 Install.bat），不代跑任何 regsvr32 / msiexec，
//   也不写注册表、不改服务配置。要不要装、怎么装，决定权完全在用户手上。
//
// 三条检测都是"看现场、不自查注册"：
//   1) VB-CABLE —— 用 cpal 枚举声卡设备名，出现 CABLE Input / CABLE Output 即已装
//      （这是最终用户真的能选到的东西，比查注册表更接近事实）
//   2) Unity Capture / OBS 虚拟摄像头 —— 查 DirectShow 设备类别注册表里那两个
//      固定 CLSID 是否注册（本机实测：Unity Video Capture = {5C2CD55C-…}，
//      OBS Virtual Camera = {A3FCE0F5-…}，各自 InprocServer32 指向滤镜 DLL）
//   3) Windows Camera Frame Server 服务 —— 只作为【提示】，不参与放行判断：
//      浏览器枚举不到摄像头八成是它没跑，但桌面会议软件不受影响。
//
// 约定：本文件只做只读检测 + 把用户要去的官网用默认浏览器打开，
//       绝不写注册表、绝不改服务配置、绝不运行任何驱动安装脚本。
// ============================================================================
// ── 本文件说明书（初学者请先读完再往下翻）──────────────────────────────
// 我是谁：启动自检"门禁"。程序双击后第一件事就是调 detect() 做一次【纯只读】体检：
//   必需项齐 → 放行进主界面、起 WebSocket 服务；缺 → 窗口整页只显示本文件的向导页，
//   服务端线程根本不启动。它是产品原则的最硬一道防线（README 第 4 节同款要求）：
//   ★ 只检测、只提示，绝不代装程序 —— 不下载、不跑任何安装脚本（没有 Install.bat、
//   不碰 msiexec/regsvr32）、不写注册表、不改服务配置。为什么？用户明确要求：
//   未经同意不得改动系统状态。驱动装不装、从哪装、何时装，决定权 100% 在用户手里；
//   所以我们连"一键帮你装"按钮都不做，最多用默认浏览器打开驱动官网链接。
// 谁调用我：src/main.rs 的启动流程 → detect() → 不齐则 EnvGuide::new(report) 接管
//   整窗，用户装完点【重新检测】通过后才起服务端；CLI 版用 missing() 打印缺项后退出。
//   跳过开关：环境变量 PCSPEAKER_SKIP_ENV_CHECK=1 / config 的 diagnostics.skip_env_check。
// 数据从哪来（三条检测全"看现场"）：cpal 枚举声卡名判 VB-CABLE / HKLM 注册表查两个
//   CLSID 键是否存在判两路虚拟摄像头 / `sc query FrameServer` 判相机服务（仅提示）。
// 数据往哪去：EnvReport 只读快照 → EnvGuide.render 画成 egui 向导页（缺什么+去哪下+
//   怎么装的文字提示）→ 用户动作以 GuideAction 枚举返回 main.rs（Ready=起服务/Quit=关窗）。
// 与手机 App 的关系：门禁发生在 WebSocket 服务启动【之前】，手机此刻根本连不上；
//   它的意义正是"手机一旦连上（D:\code\PCAssistant\lib\providers\camera_provider.dart
//   的推流前提），设备上一定选得到"。本文件与手机没有任何直接通信。
// 怎么截图验收"全缺"形态：set PCSPEAKER_FORCE_ENV_GUIDE=1 再启动 → 界面必停向导页、
//   内容用 EnvReport::demo_missing() 假报告（详见该函数注释：真实检测一行代码不受影响）；
//   配 PCSPEAKER_LANG=zh / en 可截双语。日志：log:: 宏经 main.rs 的 DualLogger 同时写
//   stderr 与 exe 同目录 audioserver.log（搜 [EnvCheck]）。放行规则单测：cargo test。
// 新手概念在本文件的落点：struct/enum/impl（EnvReport、GuideAction）、Option<bool> 三态、
//   Result 与 ?（frameserver_running 的 .ok()?）、match/if let、trait 导入（cpal traits）、
//   #[cfg(windows)] 条件编译、闭包画图（egui 的 |ui| {...}）。泛型读法：Vec<String>=
//   "装 String 的可增长数组"，Option<bool>=true/false/未知 三态。Arc/Mutex/Atomic 这里
//   用不上：detect() 同步跑完、结果按值传，单线程无共享（多线程版见 vcam_obs.rs/vcam.rs）。
//
// ──────────────────────────────────────────────────────────────────────────

use eframe::egui;
use std::process::Command;

// use crate::lang —— i18n 入口。语言选择由 lang.rs 的 enum LanguageChoice 表达：
//   Auto（跟随系统 Windows 显示语言）/ En / Zh，优先级 环境变量 PCSPEAKER_LANG >
//   config.json language > 老 settings.txt > 系统语言；首屏语言页与设置页改的就是它。
// 本文件的用法极克制：只调 lang::t("guide.xxx") / lang::tf(key, 占位表) 拿现成句子，
//   自己不判断当前语言、不硬编码任何中英文 —— "该显示哪种语言"完全是 lang.rs 的事，
//   门禁页只是消费者。这也是能拿 PCSPEAKER_LANG=zh/en 直接截双语验收图的原因。
use crate::lang;

/// 一次自检的结果快照（全部只读，不改系统任何东西）
// struct = 自定义"数据捆"类型：把一次体检的六个结果装进一个具名值里整体传递。
// "快照"的含义：detect() 跑一次、生成一个 EnvReport，之后界面反复【读】这份旧答案，
//   只有点【重新检测】(recheck) 才再跑一次 detect() 并整体替换 self.report。
// #[derive(Clone)]：自动为类型生成"克隆"能力（trait 的实现），让向导页能廉价地
//   按值复制一份报告；EnvReport 里只有 bool/Option/Vec<String>，全是可复制数据。
#[derive(Clone)]
pub struct EnvReport {
    /// VB-CABLE 虚拟音频线是否就位（麦克风模式必需）
    pub vb_cable: bool,
    /// Unity Capture 虚拟摄像头是否注册（摄像头模式可选其一）
    pub unity: bool,
    /// OBS Virtual Camera 是否注册（摄像头模式可选其一，浏览器场景需要它）
    pub obs: bool,
    /// Windows 相机框架服务是否在跑；None = 查不到（权限/系统差异）
    // Option<bool> 是"三态布尔"：Some(true)=在跑 / Some(false)=没跑 / None=不知道。
    //   为什么不用普通 bool：把"查询失败"混进"没跑"会误导用户去重启一个本来就在跑的服务。
    //   Rust 没有 null —— "可能缺失"一律用 Option<T> 显式表达，编译器强制每个使用处
    //   都处理 None 分支（下面 render 里就是 match … { Some(true)/Some(false)/None }）。
    pub frameserver_running: Option<bool>,
    /// 枚举到的音频设备名（调试用，向导页可展开查看）
    // Vec<String>：可变长数组，每个元素是一条设备名（字符串带所有权，可随意转交）。
    pub audio_devices: Vec<String>,
    /// 非 Windows 平台（Mac 移植/调试期）：跳过门禁，避免挡住开发
    pub skipped: bool,
}

impl EnvReport {
    /// 演示/自检用的"什么都没装"报告：
    /// 只在 PCSPEAKER_FORCE_ENV_GUIDE=1 时使用，让开发者不用找一台没装驱动的
    /// 电脑，也能看到向导页的"缺项 + 下载按钮"完整形态（真实检测不受影响）。
    // ── "全缺"假环境开关怎么运作（以及为什么真实检测不受影响）────────────
    // 用法：命令行 `set PCSPEAKER_FORCE_ENV_GUIDE=1 && AudioServer.exe`（或 PowerShell
    //   的 $env:PCSPEAKER_FORCE_ENV_GUIDE=1）→ main.rs 首屏把真实报告【替换】为
    //   本函数造的假报告再构建 EnvGuide，窗口必停在向导页 —— 于是开发者不用找
    //   一台真裸机，就能给"缺 VB-CABLE + 缺两路摄像头"的完整文案截验收图；
    //   再配 PCSPEAKER_LANG=zh / en 各截一张，验证双语排版。
    //   ⚠ 细节（main.rs 注释也写了）：此模式下点【重新检测】走的是【真实】detect()，
    //   环境其实齐全的话照常进主界面 —— 开关只影响首屏展示，不锁定任何行为。
    // 为什么真实检测完全不受影响：detect() 内部【从不】读这个环境变量；
    //   替换只发生在 main.rs 首屏那一个 if 里。普通用户双击 exe 时环境里没有该变量，
    //   报告永远是 detect() 的真实结果 —— 开关不可能让真检测"以为驱动没装"。
    // Self = "本类型"的别名（写 EnvReport 或 Self 等价）；函数体是纯字面量构造，
    //   最后一行不带分号 = 它就是返回值（Rust：块尾表达式即返回值）。
    pub fn demo_missing() -> Self {
        Self {
            vb_cable: false,
            unity: false,
            obs: false,
            frameserver_running: None,
            audio_devices: Vec::new(),
            skipped: false,
        }
    }

    /// 必需项是否齐全：麦克风驱动 + 两路虚拟摄像头里至少一路
    // &self = "只读借用我自己"（方法不改字段；对应 render/recheck 里的 &mut self）。
    // || 短路：skipped 为 true 时后面整个表达式都不求值 —— 非 Windows 直接放行。
    // 布尔式翻译成人话：跳过门禁 或（有 VB-CABLE 且（有 Unity 或 有 OBS））。
    //   摄像头两选一是因为两条注入通道各覆盖一半场景（见 vcam_obs.rs 文件头）；
    //   VB-CABLE 必需则因为麦克风模式没有备选传输路径。规则变更请同步改底部单测。
    pub fn ready(&self) -> bool {
        self.skipped || (self.vb_cable && (self.unity || self.obs))
    }

    /// 还缺哪些必需项（CLI 版 server 用它打印缺项后退出；文案按当前语言取）
    // Vec::new() 造空数组，缺一项 push 一条 —— 返回"清单"而不是布尔，因为界面要把
    //   缺项逐条念给用户听。lang::t(key)：按当前语言把 key 换成一句人话（见 lang.rs：
    //   auto=跟随系统语言 / zh / en，可用 PCSPEAKER_LANG 环境变量临时切换）；
    //   找不到键会回退英文再回退键名本身，绝不会显示空白 —— 所以这里只传 key 不传文案，
    //   本文件因此【不含任何硬编码界面文字】，双语验收改 locales/*.toml 即可。
    pub fn missing(&self) -> Vec<String> {
        let mut v = Vec::new();
        if !self.vb_cable {
            v.push(lang::t("guide.missing_vb"));
        }
        if !self.unity && !self.obs {
            v.push(lang::t("guide.missing_cam"));
        }
        v
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 检测实现：Windows 走真实检测，其它平台返回"跳过"
// ──────────────────────────────────────────────────────────────────────────

// 同名两个 fn detect() 由 #[cfg(...)] 二选一编译 —— 这是 Rust 处理平台差异的
//   惯用法：调用方（main.rs、本文件 recheck）永远写 detect()，不用任何 if 判断平台。
#[cfg(not(windows))]
pub fn detect() -> EnvReport {
    // macOS/Linux 上还没有对应的虚拟音频/摄像头方案（见 README §8），
    // 这里不拦路，否则开发者连调试界面都进不去。
    EnvReport {
        vb_cable: true,
        unity: true,
        obs: true,
        frameserver_running: None,
        audio_devices: Vec::new(),
        skipped: true,
    }
}

#[cfg(windows)]
pub fn detect() -> EnvReport {
    // 检查项① VB-CABLE —— 麦克风模式必需。策略：用 cpal【枚举真实声卡】再认名字，
    //   而不是查注册表卸载信息 —— 文件头说过"看现场、不自查注册"：用户真的能在
    //   声音设置/麦克风模式里选到这台"虚拟声卡"，才算装好了（更接近事实）。
    let audio = audio_device_names();
    // 设备名大小写由厂商决定（"CABLE Input" / "cable input"…），统一小写再匹配
    // 迭代器链读法：audio.iter() 逐个借用 &String → .map(|n| n.to_lowercase())
    //   把每个换成小写新 String → .collect() 收进 Vec<String>。
    //   |n| ... 是闭包（匿名小函数，竖线包参数）；迭代器不逐项 for 循环、写法更直白。
    let lower: Vec<String> = audio.iter().map(|n| n.to_lowercase()).collect();
    // .any(条件) = "存在至少一个满足的"，短路求值（找到即停）。VB-CABLE 装好后
    //   必然同时提供 "CABLE Input"(播放端) 与 "CABLE Output"(录音端) 两台虚拟声卡，
    //   出现任一即认定已装（另一个名字是不同枚举顺序/独占模式下没列全的保险）。
    let vb_cable = lower
        .iter()
        .any(|n| n.contains("cable input") || n.contains("cable output"));

    // 检查项②③：两路虚拟摄像头（结构体里直接调函数给字段赋值，纯字面量构造）+
    //   相机服务三态。skipped=false 明确表示"这是真检测的结果，不是放行豁免"。
    let report = EnvReport {
        vb_cable,
        unity: clsid_registered(UNITY_CAPTURE_CLSID),
        obs: clsid_registered(OBS_VIRTUAL_CAMERA_CLSID),
        frameserver_running: frameserver_running(),
        audio_devices: audio,
        skipped: false,
    };
    log::info!(
        "[EnvCheck] vb_cable={} unity={} obs={} frameserver={:?} devices={}",
        report.vb_cable,
        report.unity,
        report.obs,
        report.frameserver_running,
        report.audio_devices.len()
    );
    report
}

/// Unity Capture 滤镜的 CLSID（固定值，来自驱动自身安装逻辑）
// ── CLSID 是什么、为什么查它 = "装了驱动" ──────────────────────────────
// CLSID（也叫 GUID）= 128 位全局唯一 ID 写成 "{8-4-4-4-12 位十六进制}" 字符串。
//   一个 DirectShow 滤镜（虚拟摄像头驱动）装好后，注册表
//   HKLM\SOFTWARE\Classes\CLSID\{...}\InprocServer32 会记下它的 DLL 路径，
//   系统枚举摄像头时才认得它。这两个花括号串是【上游驱动自己定义的死值】，
//   不是我们能改的配置：抄错一位 = 永远查不到 = 明明装了却报缺失。
//   ⚠ 注意（已知局限，未改动）：键存在只代表"曾经安装注册过"，不代表 DLL 今天
//   仍可加载（手动删 DLL 不反注册会误判为已装）。更硬的验证要 CoCreateInstance
//   真实例化滤镜 —— 那会把第三方 DLL 载入本进程，与"只读、零侵入"原则冲突，
//   故有意不做；用户"以为装了其实坏了"由使用侧日志兜底排查。
#[cfg(windows)]
const UNITY_CAPTURE_CLSID: &str = "{5C2CD55C-92AD-4999-8666-912BD3E70010}";
/// OBS Virtual Camera 滤镜的 CLSID（OBS Studio 的 win-dshow 插件注册）
// 注意：它由 OBS Studio 安装（勾了 win-dshow 插件即注册），不是本程序注册的东西；
//   我们只是"检查到位没有"，绝不代替 OBS 去注册这个键（产品铁律，见文件头）。
#[cfg(windows)]
const OBS_VIRTUAL_CAMERA_CLSID: &str = "{A3FCE0F5-3493-419F-958A-ABA1250EC20B}";

/// 用 cpal 列出全部音频设备名（播放 + 录音）。失败返回空表，不做任何猜测。
// use cpal::traits::{DeviceTrait, HostTrait};
//   ↑ trait（能力接口）先进作用域，它的方法才"能被打点调用"：host.output_devices()
//   来自 HostTrait、d.name() 来自 DeviceTrait —— 删掉这行 use 就编译报"no method"，
//   这是 Rust 初学者最常撞的 trait 导入问题，此处是一个活例子。
#[cfg(windows)]
fn audio_device_names() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    // cpal 0.15：default_host() 直接返回 Host；设备分"播放/录音"两个列表，
    // 本机没有 all_devices()（那是更新版本的 API）。
    let host = cpal::default_host();
    let mut names = Vec::new();
    for (label, list) in [
        ("output", host.output_devices()),
        ("input", host.input_devices()),
    ] {
        match list {
            Ok(devs) => {
                for d in devs {
                    names.push(d.name().unwrap_or_default());
                }
            }
            Err(e) => log::warn!("[EnvCheck] cpal {label} devices failed: {e}"),
        }
    }
    names
}

/// 某个 DirectShow 设备 CLSID 是否已在系统里注册（InprocServer32 键存在即注册过）
// ── 这是本文件唯一一处 unsafe：但它是"只读注册表"，不改任何值 ────────────
// 为什么这里要 unsafe：RegOpenKeyExW 是外部 C 函数（FFI），编译器无法验证传进去的
//   指针/参数是否合法，所以规定调用它必须写 unsafe 块（同 vcam_obs.rs 的 extern 道理）。
//   unsafe ≠ 一定危险：我们只做 RegOpenKeyExW(打开) + RegCloseKey(关闭)，
//   权限传的是 KEY_READ（只读位，Windows 强制，想写也写不了）—— 安全边界由 API 权限保证。
//   对照产品铁律：打开注册表键【查询存在性】是只读；真正"改系统"的是 RegSetValue 之类，
//   本文件一次都没出现 —— 读代码就能自证我们没写注册表。
#[cfg(windows)]
fn clsid_registered(clsid: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    // RegOpenKeyExW 要 UTF-16 且以 NUL 结尾的宽字符串
    // 与 vcam_obs.rs 的 wstr 同一件事的就地写法：encode_utf16 + chain(once(0)) + collect。
    //   format! 拼出完整键路径（{clsid} 是内插占位），⚠ 注意双反斜杠：Rust 字符串里
    //   \ 是转义符开头，要表示一个字面反斜杠必须写 \\。
    let path = format!("SOFTWARE\\Classes\\CLSID\\{clsid}\\InprocServer32");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    let mut h = HKEY::default();
    // windows 0.58：注册表函数返回 WIN32_ERROR（不是 Result），用 .ok() 转
    // 三连读法：.ok() 把"0=成功/非0=失败"的裸错误码换成 Result<(), WIN32_ERROR>
    //   （Err 里装着错误码但这里不关心哪种错，只关心成没成）→ .is_ok() 压成 bool。
    //   等价 match，但意图更直白："打开成功吗？"。键不存在 → Err → false = 没装。
    // HKEY::default() = 空句柄占位；&mut h 是"函数会往里写一个句柄"的出参模式
    //   （C API 常见，Rust 用 &mut 把它显式标出来：调用方知道 h 会被改）。
    let opened = unsafe {
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(wide.as_ptr()), 0, KEY_READ, &mut h)
            .ok()
            .is_ok()
    };
    if opened {
        // 打开了就得关：注册表句柄是系统资源，不关会泄漏（进程退出才回收）。
        //   ⚠ 这里没法用 vcam_obs.rs 的 Drop/RAII 办法（HKEY 不是我们自己包的结构），
        //   所以手写收尾；let _ = 明确丢弃返回值表示"关失败也无可奈何"。
        unsafe {
            let _ = RegCloseKey(h);
        }
    }
    log::info!("[EnvCheck] CLSID {clsid} registered = {opened}");
    opened
}

/// Windows Camera Frame Server 是否在跑。
/// 这里不碰服务控制 API（省掉一组 Windows 依赖 + 一堆句柄生命周期），
/// 直接问系统自带的 `sc query`：普通用户就有权查询服务状态，输出里的
/// "STATE : 4 RUNNING" 各语言版本都是英文，解析稳妥。查不到返回 None。
// 检查项③ 定位：FrameServer 只影响"浏览器/UWP 能不能枚举摄像头"（Media Foundation
//   链路），桌面会议软件走 DirectShow 不受它影响 —— 所以它【只提示、不参与放行】，
//   三态 Option<bool> 的第三种"不知道"也因此可以坦然展示而不必猜（见 EnvReport 注释）。
// ⚠ 注意（已知局限，未改动）：靠匹配英文子串判状态，若某系统 sc 输出全被本地化
//   到连 RUNNING/STOPPED 都不是英文（实测各版本都是英文，故风险极低），会落到 None。
#[cfg(windows)]
fn frameserver_running() -> Option<bool> {
    // std::process::Command 造子进程：args(["query","FrameServer"]) 是【参数数组】而非
    //   拼整条命令行 —— 不经过 shell 解析，天然免疫注入（"打开官网"那节同理）。
    // .output() 跑完并收集 stdout/stderr，返回 io::Result；.ok() 把 Result 换成 Option
    //   （失败多半是 sc.exe 不存在/无法创建进程），末尾 ? 作用在 Option 上：
    //   None 就让函数【立即返回 None】—— 这是 ? 在 Option 语境下的用法（Result 语境
    //   的详细讲解在 vcam_obs.rs 底部 decode_jpeg_to_rgb 上方）。
    let out = Command::new("sc").args(["query", "FrameServer"]).output().ok()?;
    // sc 输出未必是合法 UTF-8（老代码页），from_utf8_lossy 把坏字节换成  继续用，
    //   保证"解析失败"不会变成"程序 panic"—— 只读检测的最坏结果也应只是"不知道"。
    let text = String::from_utf8_lossy(&out.stdout);
    // 判定：RUNNING / STOPPED 是服务状态关键字；1053 = Windows 错误码"服务无法及时
    //   响应"（查询少见但出现过）；DOES_NOT_EXIST = 精简系统根本没有这服务。
    //   后两类归为"没跑"，同样只提示不拦路。⚠ 这些英文串是协议性常量：若某天 sc
    //   输出被彻底本地化就要改这里（实测中文版 Windows 的 STATE 行仍是英文）。
    if text.contains("RUNNING") {
        Some(true)
    } else if text.contains("STOPPED") || text.contains("1053") || text.contains("DOES_NOT_EXIST")
    {
        Some(false)
    } else {
        None
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 动作按钮：只用系统默认浏览器打开驱动官网页面。
// 本程序不附带、不查找、也不运行任何驱动安装脚本 —— 缺驱动时只做文字提示，
// 用户自己去官网下载、自己决定要不要装。
// ──────────────────────────────────────────────────────────────────────────

/// 用系统默认浏览器打开链接
// 这是全文件唯一"影响系统"的动作，而且只是"请你自己的浏览器打开一个网页"：
//   不下载文件、不执行任何安装程序、不询问提权 —— 与文件头产品铁律严格一致。
//   （对比反面做法：按钮里藏 regsvr32/msiexec/自动下载 —— 本项目明令禁止，别加。）
pub fn open_url(url: &str) {
    log::info!("[EnvCheck] opening {url}");
    #[cfg(windows)]
    {
        // cmd /c start "" "url"：第一个空串是窗口标题占位，否则 URL 会被当成标题
        // .spawn() = 启动子进程后【不等它】（fire-and-forget）：浏览器开没开、开多久
        //   都与本程序无关；返回值用 let _ = 丢弃。cmd 只负责"转交 start 命令"，
        //   真正打开链接的是系统 shell 关联的默认浏览器。
        let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    }
    #[cfg(not(windows))]
    {
        // macOS 的对应命令是 open；Linux 发行版不同（xdg-open），本项目当前只在
        //   Mac 移植期走到这里，保持最简。
        let _ = Command::new("open").arg(url).spawn();
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 向导面板：由主窗口在"环境未就绪"时整页显示（不起服务端、不画主界面）
// ──────────────────────────────────────────────────────────────────────────

/// 用户点出来的动作，交给 main.rs 决定后续（起服务 / 关窗口）
// ── enum（枚举）：Rust 表达"有限几种可能，且每次只取其一"的类型 ──────────
// GuideAction 只有 None / Ready / Quit 三个取值（这里是不带数据的"单元变体"）。
//   关键设计：本文件（和整个 UI 层）从不自己去起服务或关进程，只把"用户想干什么"
//   编码成一个返回值交给调用方 —— 视图与业务彻底解耦，方便日后挪到别处复用。
// #[derive(Debug, PartialEq)]：自动"实现"两个 trait（能力接口）——
//   Debug = 能 {:?} 打印（测试断言失败时输出可读），
//   PartialEq = 能用 == 比较值相等（所以下面才有 recheck() == GuideAction::Ready 这种写法）。
//   derive = "让编译器替我生成样板代码"，Rust 新手最常见的第一处"魔法"，其实只是省力宏。
#[derive(Debug, PartialEq)]
pub enum GuideAction {
    /// 只是看看，没动作
    // 变体名 None 与 Option 的 None 同名但【无关】：这是 GuideAction::None，
    //   不是 Option::None。有命名空间区分，读代码时看类型即可，不会混。
    None,
    /// 重新检测通过 → 可以进入主界面了
    Ready,
    /// 用户选择退出
    Quit,
}

/// 门禁状态：一份自检结果 + 页面局部状态
// 这是"门禁页"唯一的状态容器：report 是检测快照，status/show_devices 是页面交互态。
// impl EnvGuide（见下方实现块）= 给这个 struct 挂"方法"；Rust 没有 class，
//   "数据(struct) + 行为(impl)"分开写是标准形态。方法首参 &self=只读借用自己、
//   &mut self=可改借用自己（render/recheck 要改 status，所以是 &mut）。
// report 是 pub、另两个字段私有：外部（main.rs）能读检测结果，但不能越过
//   recheck()/render() 直接篡改页面状态 —— 用可见性把"入口"收窄，天然防误用。
pub struct EnvGuide {
    pub report: EnvReport,
    /// 最近一次点击的反馈文案
    status: String,
    /// 是否展开"检测到的音频设备"明细
    show_devices: bool,
}

/// 一行检测项：绿色 √ / 红色 × + 标题 + 说明
/// 为什么用 √/× 而不是 ✓/✗：egui 内置字体和 SimHei 都没有 U+2713/U+2717，
/// 那两个会画成空心方框；√(U+221A)/×(U+00D7) 属于 GB2312 基本符号，中文字体必有。
// ── egui 是"立即模式"GUI：画法和其他框架都不一样，新手先看这段 ──────────
// 没有"创建一个控件对象、以后一直存在"的事：每一帧（每次重绘）都从头执行一遍
//   这里的函数，控件"活不过一帧"；交互结果（点了没点）靠当帧返回值告诉你。
// ui.horizontal(|ui| { ... })：传一个【闭包】给布局方法。|ui| 是闭包参数（竖线包
//   起来，读作"拿到一个子 Ui"），大括号里在这个横排子面板上继续画。闭包=能当值
//   传来传去的小函数，是 Rust 里极常见的组合手法（对应其他语言的 lambda）。
// egui::RichText::new(文字).size().strong().color()：构建器/链式调用 ——
//   每环返回新状态，最后一步才交给 label。⚠ from_rgb 里的三色值只是外观调色，
//   和功能无关，想统一 UI 主题改这里，不影响检测/放行逻辑。
fn item_row(ui: &mut egui::Ui, ok: bool, title: &str, desc: &str) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(if ok { "√" } else { "×" })
                .size(18.0)
                .strong()
                .color(if ok {
                    egui::Color32::from_rgb(34, 197, 94)
                } else {
                    egui::Color32::from_rgb(220, 38, 38)
                }),
        );
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).size(14.0).strong());
            ui.label(
                egui::RichText::new(desc)
                    .size(12.0)
                    .color(egui::Color32::from_rgb(102, 102, 102)),
            );
        });
    });
}

impl EnvGuide {
    // 关联函数 new（没有 self 参数）= 其他语言的"构造函数"，约定俗成叫 new：
    //   参数 report 直接按【值】收进来（所有权转移给字段，调用方交完就没了——
    //   Rust 的所有权移动，不是复制一份的"传引用"），status 空串 = 初始无反馈。
    pub fn new(report: EnvReport) -> Self {
        Self {
            report,
            status: String::new(),
            show_devices: false,
        }
    }

    /// 主窗口标题：让任务栏/标题栏一眼看出"这是准备页，还没开服务"
    // 又是一处 lang::t：连标题都必须走 i18n key（app.title_gate），不许硬编码中文——
    //   这是全项目文案规则，本文件所有面向用户文字都经 lang::t/lang::tf（用法见 missing()）。
    pub fn viewport_title(&self) -> String {
        lang::t("app.title_gate")
    }

    /// 重新检测：齐了返回 Ready（调用方起服务），否则刷新页面并把结果写日志
    // 点【重新检测】的全部逻辑：
    //   1. self.report = detect()：再跑一次【真实】检测并整体替换旧快照 ——
    //      ⚠ 注意：即使首屏是 PCSPEAKER_FORCE_ENV_GUIDE 的假报告，这一句之后
    //      也回归真实结果（这正是该开关"只影响首屏、不锁行为"的实现点）。
    //   2. status = if/else：Rust 的 if/else 是【表达式】，能直接当值赋给变量
    //      （两个分支都返回 String，类型一致才行——对比 C 的 if 是语句没有值）。
    //   3. lang::tf(带占位的key, &[("名","值"),...])：把文案里的 {count}/{items}
    //      替换成实际缺项数与清单（tf 的实现见 lang.rs；key 是变量所以不能直接用宏）。
    //   4. miss.join(" / ")：Vec<String> 拼接成一个字符串展示。
    //   5. 返回 GuideAction：Ready 时 main.rs 才会 start_server()；None = 原地刷新。
    fn recheck(&mut self) -> GuideAction {
        self.report = detect();
        self.status = if self.report.ready() {
            lang::t("guide.status_ready")
        } else {
            let miss = self.report.missing();
            lang::tf(
                "guide.status_missing",
                &[
                    ("count", &miss.len().to_string()),
                    ("items", &miss.join(" / ")),
                ],
            )
        };
        log::info!(
            "[EnvCheck] recheck ready={} ({})",
            self.report.ready(),
            self.status
        );
        if self.report.ready() {
            GuideAction::Ready
        } else {
            GuideAction::None
        }
    }

    /// 画整页向导（调用方已把它放进 CentralPanel），返回用户动作
    ///
    /// 布局用"嵌套面板"：顶部说明、底部按钮各自固定，中间检测项放滚动区。
    /// 为什么不能按顺序直接往下写：ScrollArea 会把 Ui 的剩余高度全部吃掉，
    /// 写在它后面的按钮条就被挤到可视区外裁掉（第一版就是底部按钮看不见）。
    // ── render 里值得新手停一停的几件事 ──────────────────────────────────
    // ① 返回值 action 在【闭包内部】被改写：闭包按可变引用捕获了外层的 action
    //    （Rust 编译器自动决定按引用/按移动捕获），函数末尾把最终值 return 给 main.rs。
    //    "UI 只表达意图、不动全局"就靠这个模式实现 —— 本文件没有任何一处直接起服务。
    // ② 缺项文案的"三段式"：标题(guide.cam_title) + 状态说明(guide.cam_*) +
    //    分步指引(guide.vb_steps/guide.cam_note)，全在 locales/*.toml 里写"缺什么 +
    //    去哪下 + 怎么装"。为什么用【文字+打开官网按钮】而不是"一键安装"按钮：
    //    产品铁律（文件头）——装驱动必须用户知情且自己动手，程序代发/代装一次越权
    //    就是安全事故。按钮文案就叫"打开 ×× 官网下载页"（guide.vb_btn/cam_btn_*），
    //    hover/步骤文字（guide.vb_hover/vb_steps/cam_note）逐条写"自己去官网下载、
    //    自己右键管理员运行"——语言层面就把"程序不代装"钉死，用户不可能误点安装。
    // ③ button(...).clicked() 每帧都重新求值（立即模式），点了才为 true，天然无残留状态。
    // ④ send_viewport_cmd(Close)：向窗口系统发"关闭本窗口"命令；同时把 action
    //    也置 Quit —— 双保险，main.rs 无论先收到哪个都能正确收尾。
    pub fn render(&mut self, ui: &mut egui::Ui) -> GuideAction {
        let mut action = GuideAction::None;

        // ── 底部：动作条（必须先声明，才能把这一条高度从中间预留出来）──
        egui::TopBottomPanel::bottom("guide_foot")
            .frame(egui::Frame::none().inner_margin(egui::Margin::symmetric(0.0, 10.0)))
            .show_inside(ui, |ui| {
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new(lang::t("guide.quit")).size(14.0))
                        .clicked()
                    {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        action = GuideAction::Quit;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(
                                // 注意：本项目是浅色主题，按钮底色是白的，
                                // 这里若把文字也写成白色就等于隐形（第一版踩过）。
                                egui::RichText::new(lang::t("guide.recheck"))
                                    .size(16.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(37, 99, 235)),
                            )
                            .clicked()
                        {
                            if self.recheck() == GuideAction::Ready {
                                action = GuideAction::Ready;
                            }
                        }
                    });
                });
            });

        // ── 顶部：标题 + 说明 ──
        egui::TopBottomPanel::top("guide_head")
            .frame(egui::Frame::none())
            .show_inside(ui, |ui| {
                ui.label(
                    egui::RichText::new(lang::t("guide.title"))
                        .size(20.0)
                        .strong(),
                );
                ui.label(egui::RichText::new(lang::t("guide.intro")).size(13.0));
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
            });

        // ── 中间：检测项（可滚动）──
        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                // ① VB-CABLE —— 麦克风模式必需
                // 缺与齐共用一个 item_row，只是传 ok=false 时换一条 guide.vb_missing
                //   文案（if 挑 key）—— 界面文字全来自 locales，这里只做"选哪句"的判断。
                // 下面 ② 的 match(布尔元组) 与 ③ 的 match(Option) 是 Rust match
                //   "强制穷尽所有分支"的两个示范：四种摄像头组合、FrameServer 三态
                //   都必须逐条给出（漏一种编译器直接拒绝编译），比 if-else 链安全。
                let vb_ok = self.report.vb_cable;
                item_row(
                    ui,
                    vb_ok,
                    &lang::t("guide.vb_title"),
                    &lang::t(if vb_ok { "guide.vb_ok" } else { "guide.vb_missing" }),
                );
                if !vb_ok {
                    ui.indent("vb", |ui| {
                        if ui
                            .button(lang::t("guide.vb_btn"))
                            .on_hover_text(lang::t("guide.vb_hover"))
                            .clicked()
                        {
                            open_url("https://vb-audio.com/Cable/");
                            self.status = lang::t("guide.status_opened");
                        }
                        ui.label(egui::RichText::new(lang::t("guide.vb_steps")).size(12.0));
                    });
                }
                ui.add_space(10.0);

                // ② 虚拟摄像头 —— 两选一
                let cam_ok = self.report.unity || self.report.obs;
                let detail = match (self.report.unity, self.report.obs) {
                    (true, true) => lang::t("guide.cam_both"),
                    (true, false) => lang::t("guide.cam_unity_only"),
                    (false, true) => lang::t("guide.cam_obs_only"),
                    (false, false) => lang::t("guide.cam_none"),
                };
                item_row(ui, cam_ok, &lang::t("guide.cam_title"), &detail);
                if !cam_ok {
                    ui.indent("cam", |ui| {
                        let mut opened = false;
                        if ui
                            .button(lang::t("guide.cam_btn_obs"))
                            .on_hover_text(lang::t("guide.cam_hover_obs"))
                            .clicked()
                        {
                            open_url("https://obsproject.com/download");
                            opened = true;
                        }
                        if ui
                            .button(lang::t("guide.cam_btn_unity"))
                            .on_hover_text(lang::t("guide.cam_hover_unity"))
                            .clicked()
                        {
                            open_url("https://github.com/Unity-Technologies/Unity-Capture");
                            opened = true;
                        }
                        if opened {
                            self.status = lang::t("guide.status_cam_opened");
                        }
                        ui.label(egui::RichText::new(lang::t("guide.cam_note")).size(12.0));
                    });
                }
                ui.add_space(10.0);

                // ③ FrameServer —— 仅提示，不参与放行
                let fs_txt = match self.report.frameserver_running {
                    Some(true) => lang::t("guide.fs_running"),
                    Some(false) => lang::t("guide.fs_stopped"),
                    None => lang::t("guide.fs_unknown"),
                };
                ui.label(
                    egui::RichText::new(fs_txt)
                        .size(12.0)
                        .color(if self.report.frameserver_running == Some(false) {
                            egui::Color32::from_rgb(180, 120, 0)
                        } else {
                            egui::Color32::from_rgb(90, 90, 90)
                        }),
                );

                // 明细：检测到的音频设备名（排错时一眼看清设备到底叫什么）
                ui.add_space(8.0);
                if !self.report.audio_devices.is_empty() {
                    let open = self.show_devices;
                    if ui
                        .small_button(if open {
                            lang::t("guide.devices_hide")
                        } else {
                            lang::t("guide.devices_show")
                        })
                        .clicked()
                    {
                        self.show_devices = !open;
                    }
                    if self.show_devices {
                        let joined = self.report.audio_devices.join("　|　");
                        ui.label(
                            egui::RichText::new(joined)
                                .size(11.0)
                                .color(egui::Color32::from_rgb(110, 110, 110)),
                        );
                    }
                }

                if !self.status.is_empty() {
                    ui.add_space(8.0);
                    // 为什么先 clone 再交给 RichText：不 clone 就得把 &self.status 的
                    //   借用【横跨】RichText::new(...).color(...) 整条构建器链和 ui 的
                    //   可变使用，借用检查器对这种"引用与 UI 借用交叉持有"很容易报错。
                    //   一句短文案复制一份，成本在"每帧一次"的场景里仍可忽略 —— 这是
                    //   Rust 里"用小克隆换编译顺畅 + 代码直白"的常见手法（性能敏感处
                    //   才需要研究引用改写法）。
                    let st = self.status.clone();
                    ui.label(
                        egui::RichText::new(st).color(egui::Color32::from_rgb(37, 99, 235)),
                    );
                }
                });
            });

        // 注：这里不主动 request_repaint —— 门禁页是静态的，
        //     用户点击/输入本身就会触发重绘（main.rs 另有 1 秒兜底重绘）。
        action
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 放行规则单测：这条规则决定"能不能进主界面"，写死在测试里最划算
// （真实 detect() 依赖本机装了哪些驱动，换台机器结果就不同，不适合断言）
// ──────────────────────────────────────────────────────────────────────────

// ── 怎么读 Rust 的单元测试（新手三分钟版）────────────────────────────
// #[cfg(test)]：条件编译 —— 只有 `cargo test` 才把整个 mod tests 编进去，
//   发布用的 exe 里根本不包含这些测试代码（零成本）。
// use super::*：把上级模块（本文件）的所有名字拿进来，所以能直接写 EnvReport。
// 每个 #[test] 函数 = 一条可执行规格：assert!(条件) / assert_eq!(左, 右) 不成立
//   就让 cargo test 报红。改 ready()/missing() 的布尔规则前，先看这四条测试——
//   它们用 fake() 手工造报告、绕开真实检测，把"麦克风必需、摄像头二选一、
//   全缺报两条、非 Windows 放行"四条产品规则钉死（跑一遍只要 cargo test）。
#[cfg(test)]
mod tests {
    use super::*;

    /// 造一份纯逻辑用的报告（不走真实检测）
    // 测试专用"造假机器"：三个 bool 参数直接对应三个检查项，frameserver/设备列表
    //   置空——因为放行规则只看这三项（见 ready()），其余字段参与不了判断就不必传。
    fn fake(vb_cable: bool, unity: bool, obs: bool) -> EnvReport {
        EnvReport {
            vb_cable,
            unity,
            obs,
            frameserver_running: None,
            audio_devices: Vec::new(),
            skipped: false,
        }
    }

    #[test]
    // 每条 assert 都是一句"人话规格"的机器化写法：
    //   没有 VB-CABLE → 就算摄像头两个都装齐也【不放行】（麦克风模式是硬依赖）。
    //   注意 fake(false, true, true).ready() 里 false 在第一格 = vb_cable 参数位。
    fn mic_driver_is_mandatory() {
        // 没有 VB-CABLE 就不放行，哪怕两个虚拟摄像头都在
        assert!(!fake(false, true, true).ready());
    }

    #[test]
    // 三个 assert 把"二选一"规则钉死：Unity 独装 ✓ / OBS 独装 ✓ / 两个都缺 ✗。
    fn either_virtual_camera_is_enough() {
        assert!(fake(true, true, false).ready());
        assert!(fake(true, false, true).ready());
        assert!(!fake(true, false, false).ready());
    }

    #[test]
    fn all_present_passes() {
        assert!(fake(true, true, true).ready());
    }

    #[test]
    fn missing_list_names_both_gaps() {
        let r = fake(false, false, false);
        assert_eq!(r.missing().len(), 2);
        // 摄像头两选一：只要有一路在，就不算缺
        assert_eq!(fake(true, false, true).missing().len(), 0);
    }

    #[test]
    fn non_windows_skips_gate() {
        let r = EnvReport {
            vb_cable: false,
            unity: false,
            obs: false,
            frameserver_running: None,
            audio_devices: Vec::new(),
            skipped: true,
        };
        assert!(r.ready(), "非 Windows 平台不该被门禁挡住");
    }
}
