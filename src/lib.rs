// 编译期把 locales/*.toml 里的全部文案嵌进二进制：
//   · fallback = "en" —— 缺键时回英文，界面不会出空白
//   · 所以发行包仍然是一个 exe，不需要随包带语言文件
// 注意：这一行必须留在 crate 根（它会在本 crate 里生成 t! 宏的实现），
//       对外的封装函数在 lang 模块里，bin（main.rs / server.rs）只用 lang::t()。
//
// ── lib.rs 是什么：crate（编译单元）的"顶层入口声明" ──────────────────
// Rust 的一个包（Cargo.toml 定义的 audioserver）可以同时产出两种目标：
//   · 库（library）：由本文件 src/lib.rs 定义，别人 use 它才能编译；
//   · 可执行文件（binary）：由 src/main.rs 和 src/bin/ 下的每个 .rs 各定义一个。
// 为什么要拆成两个：界面程序（main.rs）和命令行探针（src/bin/mic_probe.rs 等）
// 都需要同样的 config / lang / server 模块。如果代码全写在 main.rs 里，
// 探针就只能把源码复制一份；放进 lib 后，所有 bin 都通过
//   use audioserver::config;   // ← audioserver 就是 Cargo.toml 里的包名
// 来复用同一套实现，改一处两边都生效。
//
// 下面每一行 `pub mod xxx;` 都是一条"模块声明"，Rust 按约定去找同名文件：
//   pub mod config;  ←→  src/config.rs（声明什么，就去 src/ 下找 <名字>.rs）
// 没有这行声明，config.rs 就算存在也不会被编译进来。`pub` 的意思是
// "对外可见"：只有 pub 的模块，main.rs / src/bin 里的 audioserver::config 才写得通；
// 去掉 pub，它就变成 lib 内部私有的模块。改这些行的名字 = 所有 bin 的 use 全断。
use rust_i18n::i18n;

// 这一行是宏调用（感叹号结尾就表示"这是宏不是普通函数"），它在【编译期】展开：
// rust-i18n 会去读仓库根目录的 locales/en.toml、locales/zh.toml，
// 把里面的每一条 key=文案 直接生成 Rust 代码塞进二进制——
// 所以运行时不需要磁盘上有语言文件，也所以：
//   ⚠ 改了 locales/*.toml 必须重新 cargo build 才生效，运行期改文件没有任何作用。
//   （Cargo 默认不知道 .toml 是输入，是根目录 build.rs 用 rerun-if-changed
//    把 locales 下每个文件登记进构建系统，改动才会触发重编译，参见 build.rs。）
// "locales" 是相对路径（相对 Cargo.toml 所在目录），改名或挪目录要同步这里。
// fallback = "en"：查不到某个 key 时先回退到英文；英文里也查不到，
// t! 会原样把 key 本身显示出来（界面上看到 "guide.vb_title" 这种字样就是缺键）。
i18n!("locales", fallback = "en");

// 逐个模块：删掉任何一行，对应文件就不再参与编译，所有引用它的 bin 一起编译失败。
// config —— src/config.rs   ：config.json 的读写、默认值、越界夹紧
pub mod config;
// env_check —— src/env_check.rs ：启动前的驱动/环境自检（VB-CABLE 装没装等）
pub mod env_check;
// lang —— src/lang.rs      ：语言选择与取文案的对外封装（t / tf / switch_to）
pub mod lang;
// mic_out —— src/mic_out.rs   ：手机麦克风音频注入虚拟声卡（上行）
pub mod mic_out;
// server —— src/server.rs    ：WebSocket 服务端，手机 App 连的就是它
pub mod server;
// vcam —— src/vcam.rs      ：虚拟摄像头之一（DirectShow/Unity 通道）
pub mod vcam;
// vcam_obs —— src/vcam_obs.rs  ：虚拟摄像头之二（OBS 共享内存通道）
pub mod vcam_obs;
