// ============================================================================
// lang.rs —— 国际化（i18n）的语言选择层
// ----------------------------------------------------------------------------
// 为什么单独一个文件：rust-i18n 只负责"给个 key 返回一句话"，
// 而"这台电脑该用哪种语言、用户手动选过没有、选完要不要记住"这三件事
// 是本程序自己的策略，全部收在这里，main.rs / env_check.rs 只管调 t("键名")。
//
// 三种语言状态：
//   auto —— 跟随系统（默认）：读 Windows 的"显示语言"，中文系统 → zh，其它 → en
//   en   —— 强制英文
//   zh   —— 强制简体中文
// 优先级：环境变量 PCSPEAKER_LANG  >  config.json 的 language
//                        > （升级迁移）老 settings.txt  >  系统语言
// （环境变量排在最前，是为了截图验证/排错时能一行命令切换界面语言，
//   不用去动用户的配置文件。）
//
// 文案本体在仓库根的 locales/en.toml 与 locales/zh.toml，
// 由 lib.rs 里的 `i18n!("locales", fallback = "en")` 在【编译期】读进来，
// 所以发行包仍然是单个 exe，不需要带语言文件。
// ============================================================================

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 取一条文案。key 可以是运行时变量，所以这里包一层，
/// 让 main.rs / env_check.rs 不必自己写 `t!` 宏（宏还必须在调用它的 crate 里初始化）。
///
/// 找不到键时 rust-i18n 会回退到 fallback（英文），再找不到就原样返回 key 本身 ——
/// 界面上会看到 "guide.vb_title" 这种字样，很好认，不会静默显示空白。
// 拆开看这一层包装在干什么：
//   rust_i18n::t!(key)  ← 这是 lib.rs 里 `i18n!("locales", fallback = "en")` 宏
//                          生成的编译期查表代码，按"当前 locale"去嵌进二进制的
//                          locales/en.toml、zh.toml 里找 key；
//   .into_owned()        ← t! 返回 Cow<str>（"可能是借用、可能是拥有"的字符串），
//                          转成 String 让调用方不用操心生命周期。
// 为什么缺键会把 raw key 亮在界面上（而不是空白/崩溃）：给开发者和用户同一份
// 线索 —— 看到 "settings.log_path" 就知道是 zh.toml 少了这一条，直接补文案即可。
// ⚠ 提醒：文案是【编译期】嵌进 exe 的，改 locales/*.toml 必须重新构建才生效
//   （cargo 之所以知道 .toml 变了，是根目录 build.rs 用 rerun-if-changed 登记的，
//   参见 build.rs 顶部的大段说明；漏了它就会出现"明明改了 toml 界面还是 raw key"）。
pub fn t(key: &str) -> String {
    rust_i18n::t!(key).into_owned()
}

/// 带占位符的取文案：文案里写 `{uptime}`，这里把 ("uptime", "00:12:34") 填进去。
///
/// 为什么不用 rust-i18n 自带的 `%{name}` 插值：那要求 key 是编译期字面量，
/// 而我们的 key 常常是变量（比如按状态拼出来的），所以自己做一次简单的 {} 替换。
pub fn tf(key: &str, args: &[(&str, &str)]) -> String {
    let mut s = t(key);
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), value);
    }
    s
}

/// 立刻切换界面语言（egui 每帧重新取文案，所以切完下一帧就是新语言）
// set_locale 是 rust-i18n 提供的全局开关：把它内部的"当前 locale"改成 code，
// 之后每一次 t! 查表都按新值取。它【只影响接下来取哪份文案】，
// 不碰配置、不落盘 —— 落盘是 switch_to 的事，二者分开才能"先预览后保存"。
// egui 是"即时模式"界面框架：每一帧都把整个界面重新画一遍，所以不需要
// "刷新所有标签"这种操作，下一帧所有 t() 自然拿到新语言。
pub fn set_locale(code: &str) {
    rust_i18n::set_locale(code);
}

/// 当前生效的语言代码（"en" / "zh"）
// 注意：这是"程序现在正用哪种语言显示"，是 set_locale 设出来的【结果】。
// 千万别拿它当"系统语言"用 —— 用户手动选过英文后它也返回 "en"，
// 那会让设置页"系统语言"一行说谎（正是下方 system_locale 讲的真实 bug）。
pub fn locale() -> String {
    rust_i18n::locale().to_string()
}

/// 系统显示语言 → 我们支持的语言代码。
/// 只认"主语言是中文"这一条，其余一律英文：本程序目前只提供中英两种文案，
/// 德语/日语系统落到英文是有意为之（至少每句话都看得懂），
/// 以后加 locales/ja.toml 只要在这里补一个分支。
// 这就是 LanguageChoice::Auto 背后真正干活的一行：补分支的位置在下个函数的 match。
pub fn system_locale() -> &'static str {
    primary_language_of_system().unwrap_or("en")
}

/// 读系统的 UI 语言（拿不到返回 None）。
///
/// 顺带说清一个最近修掉的真实 bug（为什么"系统语言"必须单独走 Windows API）：
/// 设置页有一行"系统语言：简体中文"的提示，早先它偷懒读了 rust_i18n 的
/// "当前生效语言"（本文件上方 locale()）。两者含义完全不同：
///   · locale()             = 本程序【正在用】哪种语言显示 —— 会被用户手动切换改变；
///   · GetUserDefaultUILanguage() = Windows 操作系统自己的显示语言 —— 外部事实，
///     本程序怎么切换都动不了它。
/// 用户手动选了英文后，被污染的旧代码让"系统语言"那行也跟着显示 English ——
/// 一行谎话，用户再也分不清"系统是英文"还是"只是我选了英文"。
/// 所以现在 auto 档的翻译、界面提示，一律走下面这个只读系统事实的函数。
/// 它用 Win32 API 而不是读注册表：两者背后是同一份系统数据，API 不用处理
/// 注册表路径/权限/编码这些坑（本仓库只有 env_check.rs 查驱动时才用注册表）。
///
/// 结果用 OnceLock 缓存：这个函数会被设置页的"系统语言：X"提示读到，
/// 而 egui 是每帧重建界面的 —— 不缓存的话每次重绘都调一次 Win32 API，
/// 顺带往日志里灌一行 LANGID，几秒钟就能把 audioserver.log 刷爆
/// （实测截图验证时就撞到了这个刷屏）。系统语言在运行期不会变，缓存安全。
#[cfg(windows)]
fn primary_language_of_system() -> Option<&'static str> {
    use std::sync::OnceLock;
    use windows::Win32::Globalization::GetUserDefaultUILanguage;

    static CACHED: OnceLock<Option<&'static str>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        // LANGID：低 10 位是主语言，0x04 = 中文（zh-Hans / zh-Hant 都一样先按中文处理）
        // 给看不懂位运算的人：0x03FF 是二进制的低 10 位全 1，`langid & 0x03FF`
        // 把"语言"和"国家/地区"打包成的 16 位数拆出前半截。
        // ⚠ 改了会怎样：掩码写错会把地区位掺进比较值，中文系统再也对不上 0x04，
        //   auto 档全体变英文；0x04 写错同理。动这两处前查 MSDN 的 LANGID 表。
        // 为什么要 unsafe 块：GetUserDefaultUILanguage 是 Win32 的 C 函数（FFI），
        //   Rust 编译器无法验证外部代码的安全性，要求手写 unsafe 声明"我保证没问题"。
        //   这里无参数、只返回一个整数，实际不可能出安全问题。
        let langid = unsafe { GetUserDefaultUILanguage() } as u32;
        let primary = langid & 0x03FF;
        log::info!("[Lang] Windows UI language LANGID = {langid:#06x} (primary {primary:#x})");
        match primary {
            0x04 => Some("zh"),
            _ => Some("en"),
        }
    })
}

/// 非 Windows（Mac 移植期）：看 LANG / LC_ALL，形如 zh_CN.UTF-8
// for 数组的顺序就是 Unix 惯例的优先级：LC_ALL（强制覆盖一切）> LC_MESSAGES
// （只管界面文字）> LANG（总默认值）。反过来排会让兜底的 LANG 压住用户的显式覆盖。
// "C"/"POSIX" 是"没有任何本地化"的占位值，必须跳过继续找下一个，不能当英文处理——
// 否则 HOME 里啥都没设的机器会被误判成"人选了英文"。
#[cfg(not(windows))]
fn primary_language_of_system() -> Option<&'static str> {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if v.to_lowercase().starts_with("zh") {
                return Some("zh");
            }
            if !v.is_empty() && v.to_lowercase() != "c" && v != "POSIX" {
                return Some("en");
            }
        }
    }
    None
}

// ──────────────────────────────────────────────────────────────────────────
// settings.txt：exe 同目录的两行键值文件，只用来记住用户手动选过的语言
// ──────────────────────────────────────────────────────────────────────────
//
// 为什么不用注册表 / AppData：本程序是绿色单文件，"不写注册表"是对用户的承诺；
// 日志 audioserver.log 本来就写在 exe 同目录，设置文件跟着它放，行为一致、好删好查。
// ⚠ 注意（标题已跟不上现实，说明留给读者别改代码）：v3.8 起语言选择改存
//   config.json（AppData），settings.txt 只剩"升级迁移时被读一次"的历史角色，
//   上方"只用来记住用户手动选过的语言"是它退休前的职责描述。
//   两段路径策略并存（exe 目录 vs %APPDATA%）的由来见 config.rs 文件头。

/// 语言选择（存进 settings.txt 的就是这三个字符串之一）
// enum（枚举）= "值只能是这几种情况之一"的类型，每个情况叫一个 variant。
// Rust 的 enum 比普通语言强：variant 可以带数据（这里没带，是最简单的形态）。
// derive 的四个 trait 各有各的用途：
//   Debug/Copy/Clone → 打印、赋值即拷贝（它是零数据枚举，Copy 很廉价）
//   PartialEq/Eq    → 能用 == 比较（下面 apply_startup_locale 里
//                     `legacy != LanguageChoice::Auto` 靠的就是 PartialEq）
// 三个 variant 与文件的对应关系（as_str/parse 负责互转）：
//   Auto → "auto" 跟随系统显示语言（默认，没人动过设置时就是这个）
//   En   → "en"   强制英文，无论系统是什么语言
//   Zh   → "zh"   强制简体中文，同上
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageChoice {
    /// 跟随系统
    // 实际读哪个语言：走 system_locale() → 只读 Windows 系统事实（见那里的 bug 注释）
    Auto,
    /// 强制英文
    En,
    /// 强制简体中文
    Zh,
}

impl LanguageChoice {
    // ── 三个方法各司其职，别混用 ──
    // as_str  ：选择 → 落盘字符串。写 settings.txt / config.json 时用它。
    // parse   ：任意字符串 → 选择。读文件时对外来数据做防御性解析。
    // resolve ：选择 → 实际生效的语言代码。要 set_locale 之前问它。
    // 为什么拆三步：磁盘上可能是用户手打的脏值（"ZH-cn "），必须先 parse 洗成
    // 枚举，程序内部从此只流转"干净的选择"；落盘再 as_str，保证文件里永远
    // 只有三个合法值 —— 文件不会被坏值越写越脏。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::En => "en",
            Self::Zh => "zh",
        }
    }

    /// 解析用户输入；认不出来一律回落到 Auto（宁可跟随系统，也不要卡在一个坏值上）
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "en" => Self::En,
            "zh" | "zh-cn" | "zh-hans" => Self::Zh,
            _ => Self::Auto,
        }
    }

    /// 这个选择实际生效的语言代码
    pub fn resolve(self) -> &'static str {
        match self {
            Self::Auto => system_locale(),
            Self::En => "en",
            Self::Zh => "zh",
        }
    }
}

/// settings.txt 路径（exe 同目录）。拿不到 exe 路径时退回当前工作目录。
// 逐段读这条链（初学者常见卡点）：
//   current_exe() → io::Result<PathBuf>，.ok() 把 Err 变成 None；
//   .and_then(|p| p.parent()...) → 取所在目录，没有父目录也变 None；
//   .unwrap_or_else(|| PathBuf::from(".")) → 前两步任一失败，用 "."（当前工作目录）。
// 为什么 "." 是安全兜底：拿不到 exe 位置多半是在奇怪环境里跑探针，
// 读错目录顶多"迁移失败按 auto 启动"，不该 panic。
// ⚠ 注意：这导致 settings.txt 和 config.json 分居两处（exe 目录 vs %APPDATA%，
//   见 config.rs 头部）—— 便携版/绿色版用户会以为它们是同一个地方的文件。
pub fn settings_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("settings.txt")
}

/// 读出用户存的设置项（键不存在 / 文件不存在都返回 None，绝不报错）
// 极简 key=value 解析：跳过 # 注释行，按第一个 = 劈成 (k, v)，两边 trim。
// split_once('=') 与 split('=') 的区别：只劈一刀，值里含 '=' 也不会碎成三段
// （语言值虽然不含等号，但这写法对未来的键也成立）。
// 返回 Option<String>："没有"和"有但为空"都是 None——空值在 read_setting 里
// 被 `!v.is_empty()` 挡掉，与"文件不存在"同等对待，都让上层回落到默认档。
fn read_setting(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// 读用户选过的语言；没选过 / 文件不存在 = 跟随系统
///
/// 注意：这个函数【每次调用都会真去读一次文件】，所以只允许在启动时调一次。
/// 界面里想知道当前选的是哪一档，请用 [`current_choice`]（内存缓存）。
/// 历史事故：设置页每帧都调它，audioserver.log 里同一行刷了三万多次。
pub fn saved_choice(path: &Path) -> LanguageChoice {
    match read_setting(path, "language") {
        Some(v) => {
            let c = LanguageChoice::parse(&v);
            log::info!("[Lang] settings.txt language = {v} → {c:?}");
            c
        }
        None => LanguageChoice::Auto,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 当前选择档位的内存缓存（给界面用，避免每帧读文件）
// ──────────────────────────────────────────────────────────────────────────

// std::sync::Mutex 而不是 OnceLock：用户在界面里点按钮会改变档位，需要能写。
// 用 Mutex 包一层，读的时候 lock().clone()，几纳秒的事，绝不影响帧率。
// 补充给初学者：这里其实还是用了 OnceLock —— 但只借它"懒建一个 Mutex"，
// 档位本身在 Mutex 里随便改。OnceLock 管"盒子只造一次"，Mutex 管"内容可变"，
// 和 config.rs 的 GLOBAL 是同一个套路（那边的注释更详细）。
// 为什么界面绝不能再走 saved_choice（每帧读文件的坑）：egui 界面每秒重绘几十帧，
// 每帧一次磁盘 I/O 不但白费，还会每帧往 audioserver.log 追加一行
// （saved_choice 里有 log::info!）—— 历史上真发生过：同一行刷了三万多次。
// 所以：文件只在启动读一次 → remember_choice 灌进这里 → 界面每帧读内存。
static CURRENT_CHOICE: OnceLock<Mutex<LanguageChoice>> = OnceLock::new();

fn choice_cell() -> &'static Mutex<LanguageChoice> {
    CURRENT_CHOICE.get_or_init(|| Mutex::new(LanguageChoice::Auto))
}

/// 界面上的三个按钮该高亮哪个 —— 来自内存，不做任何 I/O
pub fn current_choice() -> LanguageChoice {
    *choice_cell().lock().unwrap_or_else(|e| e.into_inner())
}

/// 把档位记进内存（启动时和界面切换时各调一次）
// 调用方只有两个：apply_startup_locale（env 或 config 定档后）和 switch_to（用户点击后）。
// 它【不写文件】—— 落盘是 switch_to 内部调 config::update 的事。分开很关键：
// 启动时如果档位来自环境变量 PCSPEAKER_LANG，只该临时生效，写盘反而会把
// 排错用的临时覆盖变成"重启后还赖在配置里"的永久状态。
fn remember_choice(choice: LanguageChoice) {
    *choice_cell().lock().unwrap_or_else(|e| e.into_inner()) = choice;
}

/// 整份重写 settings.txt（只有一行有用，重写比增量合并简单可靠）
///
/// 注释头为什么中英双语写在一起：这个文件是给用户拿记事本打开改的，
/// 不是给程序读的 —— 只写一种语言，另一种语言的用户就不知道该怎么改。
/// 注释行以 # 开头，read_setting 会跳过，所以随便写都不影响解析；
/// 但正文字面上不能出现 "language="（单测靠它数行数），所以示例里写成 "language = "。
// ⚠ 注意：切到 config.json 之后，生产代码已经【没有任何地方】调用 save_choice
//   （grep 全仓只剩本文件的单测在用）。它留着是为了：单测 roundtrip、
//   万一要写老文件的兼容路径。别删（删了单测断不动），但也别在界面逻辑里
//   重新用它 —— 那等于把"两份会说谎的配置"请回来。由你定夺是否标 #[allow(dead_code)]
//   以外的处置。
pub fn save_choice(path: &Path, choice: LanguageChoice) -> std::io::Result<()> {
    let body = format!(
        "\
# PC Assistant AudioServer settings
# Delete this file to reset everything to defaults.
#
# language = auto | en | zh
#   auto = follow the system language (跟随系统语言)
#   en   = English
#   zh   = 简体中文
#
language={}\n",
        choice.as_str()
    );
    std::fs::write(path, body)
}

// ──────────────────────────────────────────────────────────────────────────
// 启动时的一次性决定
// ──────────────────────────────────────────────────────────────────────────

/// 决定并应用启动语言，返回"最终生效的语言 + 是谁决定的"（写日志用）
///
/// v3.8 起优先级是：环境变量 PCSPEAKER_LANG > config.json 的 language
/// > （升级迁移）老 settings.txt > 系统显示语言。
/// 这里顺带负责把 config.json 读进来（config::init 幂等，谁先调都行）。
// ── 四层优先级为什么是这个顺序（从高到低逐层解释）──
// 1. 环境变量 PCSPEAKER_LANG：排最前是给"人指挥程序"留的应急通道 ——
//    截图验证/远程排错时一行命令定语言，优先级低于它的那些配置全被跳过；
//    它是一次性的（进程结束就没了），所以永远不落盘（见 remember_choice 注释）。
// 2. config.json 的 language：现役配置文件，用户在界面里点按钮写的就是它。
// 3. 老 settings.txt：【只出现在迁移那一刻】—— 不是运行期的第三层，
//    而是"升级前用户的选择"的抢救通道，搬进 config.json 后它就不再参与决策。
// 4. 系统显示语言：以上都没说话时的最终兜底（Auto 档 → system_locale()）。
// 顺序本质：临时应急 > 现役配置 > 历史遗留 > 环境默认。反过来排会让老文件
// 永远压住新配置，用户改了界面选项却"不改行为"。
// ── 为什么下面还要对 cfg.language 调一次 parse？──
// 主因是类型转换：config.json 里存的是字符串（"zh"），lang.rs 内部流转的是
// LanguageChoice 枚举，parse 就是"字符串→枚举"那道门（config::sanitize 已把它
// 归一成三个合法值之一，这里的 parse 是零成本透传）。
// 次因是防御：将来若有别的调用路径把脏字符串直连此处，parse 认不出来一律回
// Auto（跟随系统），宁多一道保险。
pub fn apply_startup_locale() -> (&'static str, &'static str) {
    let loaded = crate::config::init();
    let mut cfg = loaded.config.clone();

    // 升级迁移：老版本只有 settings.txt。如果 config.json 是【这次新建】的，
    // 而 settings.txt 里存过一个明确选择（不是 auto），就把它搬进 config.json ——
    // 老用户升级后语言不会莫名其妙变回跟随系统。
    // created 由 config::init 如实报告（只有它真的建了文件才 true），
    // 所以整个 if 一生只进一次；迁移失败（磁盘问题）只 warn 不拦启动 ——
    // 最坏结果回到"按 config.json 的 auto 走"，老文件还在，下次启动还能再迁。
    // 注意反向是不做的：Auto 档不写 settings.txt，也不把 settings.txt 删掉，
    // 它对程序从此只是"只读的历史资料"（理由见 switch_to 注释）。
    if loaded.created {
        let legacy = saved_choice(&settings_path());
        if legacy != LanguageChoice::Auto {
            cfg.language = legacy.as_str().to_string();
            if let Err(e) = crate::config::update(cfg.clone()) {
                log::warn!("[Lang] migrate settings.txt -> config.json failed: {e}");
            }
        }
    }

    // 元组 (code, source) = （最终生效语言，谁定的）。source 不是摆设：
    // 会进本函数末尾的 log，也被 main.rs 拿去再记一行（参见 src/main.rs 的
    // init_logger 与 apply_startup_locale 调用处），排错时一眼看出语言是谁说了算。
    // if let Ok(...) = env::var：环境变量存在且能读就赢 —— 对应上面的优先级第 1 层。
    // 两个分支都 remember_choice(c)：不管来源是谁，界面按钮高亮档都要跟着改。
    // 注意 env 分支只灌内存不落盘（优先级 1 是一次性应急通道，见 remember_choice）。
    let (code, source) = if let Ok(env_v) = std::env::var("PCSPEAKER_LANG") {
        let c = LanguageChoice::parse(&env_v);
        remember_choice(c);
        (c.resolve(), "env PCSPEAKER_LANG")
    } else {
        let c = LanguageChoice::parse(&cfg.language);
        remember_choice(c);
        // Auto 档的功劳记在"system language"头上：config.json 只是说"跟随"，
        // 真正定出生效语言的是系统 —— 日志这么写，用户才知道去哪改。
        let source = if c == LanguageChoice::Auto {
            "system language"
        } else {
            "config.json"
        };
        (c.resolve(), source)
    };
    // 最后一步才 set_locale：前面全是"决定"，这行起 t() 才真的换语言。
    // 磁盘上没写过的东西（env 覆盖）不在这一步之后画蛇添足地去保存。
    set_locale(code);
    log::info!("[Lang] UI locale = {code} (from {source})");
    (code, source)
}

/// 在设置页里切换语言：立刻生效 + 落盘。返回落盘错误（界面会显示出来）
///
/// 落盘写的是 config.json。settings.txt 从此【只读不写】：
/// 老实现是整文件重写，继续留着它等于让用户面对两份会说谎的配置。
// ── 展开讲"会说谎的文件"（这是设计决定，不是随手改的）──
// 若继续双写：settings.txt 和 config.json 各存一份 language，迟早分叉 ——
// 用户手改了 config.json、程序按它显示，而 settings.txt 里还躺着三年前的
// "en"；哪天有代码"贴心地"读了 settings.txt，用户就被一个早已作废的文件
// 拽回旧语言。两份同一语义的配置 = 必有一份过时 = 那份在说谎。
// 现在唯一真相是 config.json（见 config.rs），settings.txt 降级为
// "只被迁移逻辑读一次的遗物"（apply_startup_locale 的 created 分支）。
//
// 函数内三步的顺序（改前想清楚）：
//   1. set_locale —— 先生效，用户立刻看到界面变语言，即使落盘失败也不反悔
//      （回滚语言反而让用户以为点错了，比"文件没写进去"体验差）；
//   2. remember_choice —— 更新内存档位，界面按钮下一帧就高亮正确；
//   3. config::update —— 落盘；返回的 Err 一路传到界面提示"保存失败"。
// 第 3 步内部（config::get() 取全量 → 只改 language → update 整份写回）
// 的并发风险见 config.rs update() 上方的 ⚠ 注释。
pub fn switch_to(choice: LanguageChoice) -> std::io::Result<()> {
    set_locale(choice.resolve());
    remember_choice(choice);
    let mut cfg = crate::config::get();
    cfg.language = choice.as_str().to_string();
    crate::config::update(cfg)
}

/// 把 "zh" / "en" 显示成给用户看的名字（语言名一律用它自己的写法，不翻译）
// 为什么"简体中文"永远用中文写、"English"永远用英文写（而不是跟着界面变
// "Chinese"/"中文"）：正在学/母语者认自己的写法最不容易认错；
// 语言切换按钮里翻来翻去反而会造成"哪个才是中文"的混乱。这是行业惯例。
pub fn display_name(code: &str) -> &'static str {
    match code {
        "zh" => "简体中文",
        _ => "English",
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 单测：占位符替换和"跟随系统"的回落逻辑，是最容易在改文案时悄悄坏掉的
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tf_replaces_every_placeholder() {
        // 注意：这里不 set_locale —— 单测是多线程并行跑的，语言是全局状态，
        // 断言必须对"当前是哪国语言"不敏感。footer.audio_summary 两种语言都是
        // "{rate}Hz · {ch}ch · PCM"，所以只断言占位符都被填掉、且以 PCM 结尾。
        let s = tf(
            "footer.audio_summary",
            &[("rate", "48000"), ("ch", "2")],
        );
        assert!(!s.contains('{'), "占位符没被替换：{s}");
        assert!(
            s.contains("48000") && s.contains("2") && s.ends_with("PCM"),
            "替换结果不对：{s}"
        );
    }

    #[test]
    fn unknown_key_falls_back_to_itself() {
        // 键不存在时不能 panic，也不能返回空串 —— 界面上要能看出是漏了文案
        assert_eq!(t("nope.not_a_real_key"), "nope.not_a_real_key");
    }

    #[test]
    fn both_locales_have_every_key_for_the_surfaces_we_render() {
        // 抽查几个"界面一定用得到"的键：两种语言都必须有，且不能等于 key 本身
        for key in [
            "header.running",
            "mode.camera",
            "settings.lang_auto",
            "guide.title",
            "guide.vb_title",
            "cli.check_failed",
        ] {
            for loc in ["en", "zh"] {
                set_locale(loc);
                let v = t(key);
                assert_ne!(v, key, "{key} missing in {loc}.toml");
                assert!(!v.trim().is_empty(), "{key} empty in {loc}.toml");
            }
        }
        set_locale("en");
    }

    // parse 的容错面全测试：大小写、空格、历史别名、空串、乱码都不许把程序
    // 带进第四个状态 —— Rust 枚举保证结果只能是 Auto/En/Zh 三者之一。
    #[test]
    fn parse_falls_back_to_auto() {
        assert_eq!(LanguageChoice::parse("zh"), LanguageChoice::Zh);
        assert_eq!(LanguageChoice::parse("  EN "), LanguageChoice::En);
        assert_eq!(LanguageChoice::parse("zh-CN"), LanguageChoice::Zh);
        assert_eq!(LanguageChoice::parse("klingon"), LanguageChoice::Auto);
        assert_eq!(LanguageChoice::parse(""), LanguageChoice::Auto);
    }

    // En/Zh 的 resolve 是常量映射；Auto 的 resolve 依赖真实操作系统，
    // 断言只能放宽成"结果必须是 en 或 zh"——测试要写成"对运行机器不敏感"的样式。
    #[test]
    fn explicit_choices_ignore_the_system() {
        assert_eq!(LanguageChoice::En.resolve(), "en");
        assert_eq!(LanguageChoice::Zh.resolve(), "zh");
        // Auto 必须给出我们真正支持的语言之一
        assert!(["en", "zh"].contains(&LanguageChoice::Auto.resolve()));
    }

    #[test]
    fn en_and_zh_have_exactly_the_same_keys() {
        // 这是整套 i18n 最重要的一条测试：往 en.toml 加了键、忘了 zh.toml，
        // 中文界面上那一处就会悄悄变成英文；反过来加个没人用的键也没人发现。
        let read = |name: &str| -> std::collections::BTreeSet<String> {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("locales")
                .join(name);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("读不到 {}: {e}", path.display()));
            let mut keys = std::collections::BTreeSet::new();
            let mut section = String::new();
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some(s) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                    section = s.to_string();
                    continue;
                }
                if let Some((k, _)) = line.split_once('=') {
                    keys.insert(format!("{section}.{}", k.trim()));
                }
            }
            keys
        };
        let en = read("en.toml");
        let zh = read("zh.toml");
        assert!(!en.is_empty(), "en.toml 没解析出任何键 —— 格式改坏了？");
        let only_en: Vec<&String> = en.difference(&zh).collect();
        let only_zh: Vec<&String> = zh.difference(&en).collect();
        assert!(
            only_en.is_empty() && only_zh.is_empty(),
            "两份文案键不一致\n  只有 en 有：{only_en:?}\n  只有 zh 有：{only_zh:?}"
        );
    }

    #[test]
    fn settings_roundtrip_and_missing_file_is_auto() {
        let dir = std::env::temp_dir().join(format!(
            "audioserver_lang_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.txt");

        // 文件不存在 → 跟随系统，且不报错
        assert_eq!(saved_choice(&path), LanguageChoice::Auto);

        save_choice(&path, LanguageChoice::Zh).unwrap();
        assert_eq!(saved_choice(&path), LanguageChoice::Zh);
        // 重写而不是追加：文件里只能有一行 language=
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("language=").count(), 1);

        save_choice(&path, LanguageChoice::Auto).unwrap();
        assert_eq!(saved_choice(&path), LanguageChoice::Auto);

        // 坏值不能把程序带进奇怪状态：回落 Auto
        std::fs::write(&path, "language=@@@\n").unwrap();
        assert_eq!(saved_choice(&path), LanguageChoice::Auto);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
