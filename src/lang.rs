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
// 优先级：环境变量 PCSPEAKER_LANG  >  settings.txt 里的 language  >  系统语言
// （环境变量排在最前，是为了截图验证/排错时能一行命令切换界面语言，
//   不用去动用户的 settings.txt。）
//
// 文案本体在仓库根的 locales/en.toml 与 locales/zh.toml，
// 由 lib.rs 里的 `i18n!("locales", fallback = "en")` 在【编译期】读进来，
// 所以发行包仍然是单个 exe，不需要带语言文件。
// ============================================================================

use std::path::{Path, PathBuf};

/// 取一条文案。key 可以是运行时变量，所以这里包一层，
/// 让 main.rs / env_check.rs 不必自己写 `t!` 宏（宏还必须在调用它的 crate 里初始化）。
///
/// 找不到键时 rust-i18n 会回退到 fallback（英文），再找不到就原样返回 key 本身 ——
/// 界面上会看到 "guide.vb_title" 这种字样，很好认，不会静默显示空白。
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
pub fn set_locale(code: &str) {
    rust_i18n::set_locale(code);
}

/// 当前生效的语言代码（"en" / "zh"）
pub fn locale() -> String {
    rust_i18n::locale().to_string()
}

/// 系统显示语言 → 我们支持的语言代码。
/// 只认"主语言是中文"这一条，其余一律英文：本程序目前只提供中英两种文案，
/// 德语/日语系统落到英文是有意为之（至少每句话都看得懂），
/// 以后加 locales/ja.toml 只要在这里补一个分支。
pub fn system_locale() -> &'static str {
    primary_language_of_system().unwrap_or("en")
}

/// 读系统的 UI 语言（拿不到返回 None）。
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

/// 语言选择（存进 settings.txt 的就是这三个字符串之一）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageChoice {
    /// 跟随系统
    Auto,
    /// 强制英文
    En,
    /// 强制简体中文
    Zh,
}

impl LanguageChoice {
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
pub fn settings_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("settings.txt")
}

/// 读出用户存的设置项（键不存在 / 文件不存在都返回 None，绝不报错）
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

/// 整份重写 settings.txt（只有一行有用，重写比增量合并简单可靠）
///
/// 注释头为什么中英双语写在一起：这个文件是给用户拿记事本打开改的，
/// 不是给程序读的 —— 只写一种语言，另一种语言的用户就不知道该怎么改。
/// 注释行以 # 开头，read_setting 会跳过，所以随便写都不影响解析；
/// 但正文字面上不能出现 "language="（单测靠它数行数），所以示例里写成 "language = "。
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
pub fn apply_startup_locale() -> (&'static str, &'static str) {
    let path = settings_path();
    let (code, source) = if let Ok(env_v) = std::env::var("PCSPEAKER_LANG") {
        (LanguageChoice::parse(&env_v).resolve(), "env PCSPEAKER_LANG")
    } else {
        let saved = saved_choice(&path);
        let source = if saved == LanguageChoice::Auto {
            "system language"
        } else {
            "settings.txt"
        };
        (saved.resolve(), source)
    };
    set_locale(code);
    log::info!("[Lang] UI locale = {code} (from {source})");
    (code, source)
}

/// 在设置页里切换语言：立刻生效 + 落盘。返回落盘错误（界面会显示出来）
pub fn switch_to(choice: LanguageChoice) -> std::io::Result<()> {
    set_locale(choice.resolve());
    save_choice(&settings_path(), choice)
}

/// 把 "zh" / "en" 显示成给用户看的名字（语言名一律用它自己的写法，不翻译）
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

    #[test]
    fn parse_falls_back_to_auto() {
        assert_eq!(LanguageChoice::parse("zh"), LanguageChoice::Zh);
        assert_eq!(LanguageChoice::parse("  EN "), LanguageChoice::En);
        assert_eq!(LanguageChoice::parse("zh-CN"), LanguageChoice::Zh);
        assert_eq!(LanguageChoice::parse("klingon"), LanguageChoice::Auto);
        assert_eq!(LanguageChoice::parse(""), LanguageChoice::Auto);
    }

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
