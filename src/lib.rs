// 编译期把 locales/*.toml 里的全部文案嵌进二进制：
//   · fallback = "en" —— 缺键时回英文，界面不会出空白
//   · 所以发行包仍然是一个 exe，不需要随包带语言文件
// 注意：这一行必须留在 crate 根（它会在本 crate 里生成 t! 宏的实现），
//       对外的封装函数在 lang 模块里，bin（main.rs / server.rs）只用 lang::t()。
use rust_i18n::i18n;

i18n!("locales", fallback = "en");

pub mod env_check;
pub mod lang;
pub mod mic_out;
pub mod server;
pub mod vcam;
pub mod vcam_obs;
