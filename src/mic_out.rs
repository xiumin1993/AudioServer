// ── v3 虚拟麦克风注入引擎 ──────────────────────────────────────────
//
// 职责：把手机上行的小端 16-bit PCM（单声道）持续写入 VB-CABLE 的
//       **播放端 "CABLE Input"**。系统录音端 "CABLE Output" 上出现的就是
//       手机话筒的实时声音——任何 PC 应用（会议/语音输入/录音软件）把输入
//       设备选为 "CABLE Output" 即完成"手机当 PC 麦克风"。
//
// 设计要点：
//   1. 引擎随服务器启动，常开。没有上行数据时写静音帧，保证设备始终"活着"，
//      应用打开/关闭麦克风不需要我们做任何事（这就是"自动启用"）。
//   2. 上行数据先进 mic_queue（约 250ms 上限，超出丢最旧样本保实时性），
//      注入线程按 WASAPI 请求的帧数从队列取数、混成设备 mix 格式。
//   3. 设备掉线/拔出时 pump 返回，引擎 3 秒后自动重开。

// ── 本文件说明书（初学者请先读完这 16 行再往下翻）────────────────────────
// 我是谁：虚拟麦克风注入引擎。全进程只有我这一个后台线程在"写音频"。
// 上游（数据从哪来）：src/server.rs 的 WebSocket 读循环。手机发来的【无魔术头 Binary 帧】
//   = s16le 裸 PCM，server.rs 调本文件的 push_uplink() 把它们压进 mic_queue。
// 下游（数据往哪去）：VB-CABLE 虚拟声卡的播放端 "CABLE Input"。PC 应用（钉钉/腾讯会议/
//   Windows 语音输入/录音机）把输入设备选成 "CABLE Output" 就听到手机，我们碰不到应用本身。
// 谁启动我：src/server.rs 启动阶段（约 502~511 行）依次调 apply_startup_rate() →
//   spawn_mic_output() → spawn_capture_monitor()。GUI（src/main.rs）不直接调我，只被动收
//   ServerEvent::MicEngine / MicState 两种事件来刷新界面上的设备名与"占用中"灯。
// 手机端搭档：D:\code\PCAssistant\lib\providers\mic_provider.dart（idle→standby→live 状态机）。
//   它发 mic_start/mic_stop/mic_mute，收 mic_ack/mic_state；本文件的占用检测就是 mic_state 的唯一来源。
// 本文件用到的概念索引：VB-CABLE 虚拟音频线 / cpal（只在 macOS 分支）/ WASAPI 共享模式 /
//   线性插值重采样 44.1k→48k / Arc<Mutex<VecDeque>> 生产者-消费者 / AtomicU32 / unsafe FFI。
// 日志去哪：info!/warn! 由 src/main.rs 装的 DualLogger 落 stderr + audioserver.log。
// 改动红线：pump() 循环里任何"顺手加的耗时操作"都会变成录音卡顿；删掉它的 sleep 会烧满一个核心。
//
use crate::server::ServerEvent;
use log::{info, warn};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

/// 上行样本队列：手机线程写入，注入线程消费。存的是 i16 单声道样本。
// ── 这一行"类型别名"里塞了四个 Rust 核心概念，拆开看 ─────────────────────
// type MicQueue = ...：类型别名，不创造新类型，只是给右边那串长类型起个好记的名字。
// Arc<T>（Atomic Reference Counted）：原子引用计数智能指针。多个线程要碰同一份数据时，
//   不能各拿一份拷贝（VecDeque 有唯一所有权），于是用 Arc 包住它：克隆 Arc 只把计数 +1，
//   所有克隆都指向堆上同一个队列；最后一个 Arc 被丢弃时队列才真正释放（RAII，见 Drop 一节）。
// Mutex<T>：互斥锁。同一时刻只允许一个持有者 lock() 到内部数据；别处再 lock() 会【阻塞排队】。
//   网络线程（写队列）和音频线程（读队列）就是在这一把锁上排队的。
// VecDeque<i16>：双端队列。和 Vec 的区别是 push_front/pop_front 都是 O(1)：
//   尾部进（生产者）、头部出（消费者），两头都不用搬运已有内存。
// 为什么存 i16 样本而不是原始字节：只有按"样本"计数才能做长度上限和插值重采样；
//   字节→样本的换算（每样本 2 字节，小端）在 push_uplink() 里一次性做完。
pub type MicQueue = Arc<Mutex<VecDeque<i16>>>;

/// 队列积压上限（样本数）。
///
/// v3.8 起由 config.json 算出来：`mic.max_queue_ms × mic.uplink_sample_rate / 1000`
/// （默认 250ms × 48000 = 12000 个样本，和以前写死的值一模一样）。
/// 超过上限说明网络比实时快，丢最旧的保最新（不丢就会越积越多变成回声）。
/// 用 OnceLock 缓存：push_uplink 每个上行包都会进来一次，配置只在第一次用时读。
fn max_queue_samples() -> usize {
    // 函数体内部的 static：CACHE 全进程唯一、生命周期等于整个程序，但第一次执行到这行才存在。
    // OnceLock<usize> = "只允许写一次的盒子"：多线程同时首次调用时，只有一个线程赢下去写值，
    //   其余线程在 get_or_init 里短暂等待后拿到同一个值 —— 天然的"线程安全的一次性初始化"。
    // 为什么每个上行包都调这个函数却不慢：见下条注释，真正的配置读取只发生第一次。
    // ⚠ 注意（既有设计，未改动）：值被永久缓存，运行中修改 config.json 的 mic.max_queue_ms
    //   不会生效，必须重启服务器；这是"每包零开销"换来的代价。
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // get_or_init 收一个闭包（closure，匿名函数写法 |参数| { 身体 }）：盒子为空时才执行它。
    // 返回值是 &usize（借用），前面的 * 解引用抄出一个 usize 值作为函数返回值。
    *CACHE.get_or_init(|| {
        // 样本数换算：毫秒 × 赫兹 ÷ 1000。默认 250 × 48000 ÷ 1000 = 12000 个样本 = 0.25 秒声音。
        // 魔数 1024 是下限防呆：配置被写成 0 或极小值时，队列至少还能存 1024 个样本（48k 下约 21ms），
        //   否则泵每轮都取不到样本、只能疯狂写静音。改大它 = 弱网时更容易延迟；改小它 = 失去防呆。
        // 12000（250ms）这个量级的取舍：太小 → WiFi 抖一下就断流（听感是"字被吃掉"）；
        //   太大 → 网络恢复后要把攒下的几秒音频播完，变成延迟和回声。
        let cfg = crate::config::get();
        let n = (cfg.mic.max_queue_ms as usize * cfg.mic.uplink_sample_rate as usize) / 1000;
        let n = n.max(1024);
        info!(
            "[MicOut] uplink queue cap = {} samples ({} ms @ {} Hz, from config)",
            n, cfg.mic.max_queue_ms, cfg.mic.uplink_sample_rate
        );
        n
    })
}

// ── v3.4.4 上行采样率开关 + 重采样 ─────────────────────────────────
/// 手机实际发来的 PCM 采样率（44100 / 48000），服务器收到 mic_start 时更新。
/// 默认 48000：与声卡设备速率一致时，注入路径与旧版【逐字节相同】（零风险）；
/// 只有用户在手机上主动切到 44.1k，泵里才会启用线性插值重采样，
/// 否则把 44.1k 当 48k 直接写会音调变快、声音变尖。
// ── 为什么这个全局值用 AtomicU32 而不是 Mutex<u32> ───────────────────────
// 要跨线程共享的就是"一个 32 位整数"：CPU 对它的一次读写本身就是原子的，不会出现
//   "读到写了一半的值"。用 Mutex 反而更重（要排队、可能把音频线程卡住），
//   所以 Rust 的惯例是：单个小整数/布尔标志 → 原子类型；多字段必须一起看 → Mutex。
// pub static：全局变量（进程内唯一一份，不在线程栈上）。static 要求内容能在编译期确定，
//   AtomicU32::new(48000) 正好是编译期常量表达式，所以能直接写在 static 上。
// 默认 48000 是刻意选的：手机不声明时和声卡混音速率一致 → 走"逐样本直通"的老路径，新代码零风险。
pub static UPLINK_RATE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(48000);

/// 手机上报上行采样率时调用（非法值忽略，维持上一个好值）
pub fn set_uplink_rate(sr: u32) {
    // 区间校验：8000..=192000（含右端点）覆盖从电话音质到 Hi-Res 的全部常见速率。
    // 非法值【静默忽略】：这个数来自手机（外部输入，可能被伪造或写错）。最坏后果只是音调不对，
    // 不值得为它 panic 把 WebSocket 任务搞死。把区间放宽 = 允许更多设备，也允许更多垃圾值。
    if (8000..=192000).contains(&sr) {
        // store = 写原子值（对应的读叫 load）。contains(&sr) 要的是引用 &u32，所以这里带 &。
        // Ordering::Relaxed 是"内存序"：只保证这个数自身不会撕裂，不要求它和其他内存操作
        //   排成同一条全局顺序。这里够用，因为采样率是个孤立事实：泵下一轮 load 到新值就
        //   自己重算 step，晚一轮看到也只是多/少插值一轮，没有"配套数据要同步"的问题。
        //   需要配套同步时才会用 Ordering::SeqCst/Acquire/Release（本文件没有这种需求）。
        UPLINK_RATE.store(sr, std::sync::atomic::Ordering::Relaxed);
        info!("[MicOut] Uplink sample rate set to {} Hz", sr);
    }
}

// ── VB-CABLE 是什么，以及手机麦克风为什么非得绕它一跳 ────────────────────
// VB-CABLE（VB-Audio Virtual Cable，免费驱动）在系统里假装自己是一张声卡，但它有两个"头"：
//   CABLE Input  = 播放端（在系统里长得像扬声器/输出设备）→ 我们把 PCM 写进这里
//   CABLE Output = 录音端（长得像麦克风/输入设备）→ 会议软件从这里读
// 两个头在驱动内部直连，不出喇叭、不占真实麦克风，等价于一根"虚拟音频线"。
// 为什么非走这条路不可：Windows 只允许应用从【音频设备】录音。一个 WebSocket 进程无论
//   写文件、发命名管道、注册虚拟设备，都不可能凭空变成"麦克风"；唯一稳妥的做法就是
//   借一个已经装好的虚拟声卡驱动当翻译 —— 我们负责喂 Input，系统负责让应用录到 Output。
// 也正因如此，本引擎不关心是谁在录、录去干什么（见下面的 capture_monitor 才知道）。
/// 目标设备名片段（VB-CABLE 的播放端叫 "CABLE Input"，大小写不敏感匹配）
// const：编译期常量，每个使用点直接展开，不占独立内存地址（和 static 的区别）。
// &str：字符串【切片】= 指针 + 长度，指向二进制里内嵌的只读 UTF-8 数据，不拥有内存。
//   要拥有就调 .to_string()/to_uppercase() 得到 String（会复制一份）。
// 存大写是因为比较前两边都 to_uppercase()，这样配置里怎么写都能命中。
const CABLE_RENDER_HINT: &str = "CABLE INPUT";

/// VB-CABLE 的录音端名片段（占用检测用：应用录的是 "CABLE Output"）
// ⚠ 两个 hint 不能填反：CABLE Input 是【播放端】（我们写），CABLE Output 是【录音端】
//   （应用录）。填反了 → 注入找不到设备、占用检测也永远 false，现象是"界面显示正常但没人能听见"。
const CABLE_CAPTURE_HINT: &str = "CABLE OUTPUT";

// ── v3.8：设备名可以从 config.json 改 ────────────────────────────────────
// 为什么要能改：换一根虚拟声卡（VoiceMeeter、Matrix 之类）以前得改代码重编译，
// 现在改两行配置就行。匹配规则没变：设备友好名【转大写后做包含匹配】。
// 填成空串 = 回到默认那对 VB-CABLE 名字（不是"随便挑一台"）：
// 宁可找不到设备、界面上明说"没找到"，也绝不能把手机的声音默默灌进真实扬声器 ——
// 那会变成当场回授啸叫，是最难查的一种事故。

/// 注入目标（播放端）设备名片段，已转大写
// 返回 String 而不是 &str：配置里的名字是运行时才知道的内容，必须新分配一份拥有所有权的串。
// 为什么返回"片段"而不是完整设备名：调用方一律用 contains 匹配，配置只需写能唯一区分的那部分。
// trim().is_empty()：只填空白也当没填 → 回到 VB-CABLE 默认名（安全默认，绝不"随便挑一台设备"）。
fn inject_hint() -> String {
    let h = crate::config::get().mic.inject_device_hint;
    if h.trim().is_empty() {
        CABLE_RENDER_HINT.to_string()
    } else {
        // to_uppercase() 返回 String（可能是新分配的），所以下面 contains(&hint) 时
        //   hint 已是大写，两边口径一致 —— 匹配规则改这里就够了，不用动每个调用点。
        h.to_uppercase()
    }
}

/// 占用检测（录音端）设备名片段，已转大写
// 与 inject_hint 同一家族，只是指向"另一头"：这里匹配的是应用【录音】用的设备名。
// ⚠ 注意（设计耦合）：这两条名字必须描述【同一根】虚拟线的两端，否则会出现
//   "我们喂的那根没人录、应用录的那根我们没喂" —— 表现正是"引擎显示正常但全场静音"。
fn monitor_hint() -> String {
    let h = crate::config::get().mic.monitor_capture_hint;
    if h.trim().is_empty() {
        CABLE_CAPTURE_HINT.to_string()
    } else {
        h.to_uppercase()
    }
}

/// 手机还没上报自己的上行采样率时，用 config.json 里的 mic.uplink_sample_rate 打底。
/// 手机一旦发过 mic_start 声明了自己的采样率，set_uplink_rate 会覆盖这个值。
// 调用时机：server.rs 在 spawn_mic_output 之前调它一次（见文件头说明书）。
// 校验规则和 set_uplink_rate 一样：宁可保留默认的 48000，也不要一个脏值。
pub fn apply_startup_rate() {
    let sr = crate::config::get().mic.uplink_sample_rate;
    if (8000..=192_000).contains(&sr) {
        // 数字里写下划线 192_000 只是为了好读，编译后与 192000 完全相同（Rust 的数值字面量分隔符）。
        UPLINK_RATE.store(sr, std::sync::atomic::Ordering::Relaxed);
        info!("[MicOut] Default uplink sample rate from config: {sr} Hz");
    }
}

// ── 下面两个函数是"数据入口"，跑在【网络线程（tokio 异步任务）】上 ─────────
// 服务器每收到一个上行 Binary 包就会走到这里，一秒可达上百次，所以只能干轻活：
// 解字节、塞队列。千万不要在这里加文件 IO / sleep / 复杂计算，那会拖住整条异步任务。
pub fn new_queue() -> MicQueue {
    // with_capacity(2048)：一次性预分配 2048 个样本（4KB）的连续内存。
    // 只是"起步容量"，队列不够时 VecDeque 自己会扩容；预分配是为了首包到达时不触发堆分配。
    // 改成 100 万不会更快（只是启动多点内存），改成 16 也无害（只是可能多几次扩容搬运）。
    Arc::new(Mutex::new(VecDeque::with_capacity(2048)))
}

/// 把一段 PCM s16le 字节流推入上行队列（超长自动丢弃最旧样本）
pub fn push_uplink(queue: &MicQueue, bytes: &[u8]) {
    // PCM s16le = 最裸的音频格式：每秒 N 个样本，每样本一个 16 位有符号整数，小端在前
    // （小端 little-endian：样本 0x1234 在内存里排成 [0x34, 0x12]）。手机 AudioRecord、
    // WebRTC、VB-CABLE 通行小端，所以这里不必判断字节序（大端要写 from_be_bytes）。
    // 参数 bytes: &[u8] 是"字节切片"：一个借用来的连续内存视图（指针 + 长度），
    //   不拥有数据、不拷贝，函数返回后原数据仍归调用者 —— Rust 里传"看一眼"的数据都用 &。
    // lock() 返回 Option/Result 包装的 MutexGuard（守卫）：变量 q 离开作用域时自动解锁（RAII）。
    // ⚠ 注意（既有设计，未改动）：.unwrap() 在两种情况下会炸 —— ① 持锁线程 panic 使锁"中毒"
    //   （Rust 的 Mutex 默认策略：宁可让后来者也 panic，也不要交出可能被改坏的数据）；
    //   ② 同一线程重复 lock 自己已持有的锁 → 不是 panic 而是永久卡死。
    //   现在持锁方都只做几行代码，实践安全；但别在已经握着同一把锁的作用域里再调 push_uplink。
    //   选择让它 panic 而不是吞错误：静默丢音频比崩溃更难查。
    let mut q = queue.lock().unwrap();
    // chunks_exact(2)：按 2 字节切片；尾部不足 2 字节的【残字节被丢弃】（半个样本无法还原）。
    // 手机 10ms 一包（48k 单声道 480 样本 = 960 字节，恒为偶数），正常永远不剩尾。
    // 注意是 chunks_exact 不是 chunks：后者会多返回一个长度 1 的尾巴，这里不想要它。
    for chunk in bytes.chunks_exact(2) {
        // [chunk[0], chunk[1]] 是数组字面量，类型正好是 from_le_bytes 要的 [u8; 2]。
        // Rust 对数组长度是"类型级别精确匹配"，写成 (chunk[0], chunk[1]) 元组就编译不过。
        q.push_back(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    // 溢出保护：超过上限就丢【最旧】的（头部），留住最新的（尾部）。
    // 为什么不是丢最新：积压说明网络比实时快，越攒越延迟，最终变成对着喇叭的延迟回声；
    // 丢最旧的只损失"已经来不及听的过去"。这是实时音视频一律的取舍（保实时不保完整）。
    // saturating_sub = 减法不下溢：len 小于上限时得 0，而不是回绕成天文数字把队列清空。
    // drain(..overflow)：从头部一次移除并丢弃 overflow 个元素（范围语法 ..n = 0..n）。
    let overflow = q.len().saturating_sub(max_queue_samples());
    if overflow > 0 {
        q.drain(..overflow);
    }
}

/// 启动注入引擎线程（Windows 见 windows_impl；macOS 见 macos_impl；其余平台 stub）
// ── #[cfg(...)] 条件编译：同一个函数名三套实现，编译期只保留一套 ──────────
// #[cfg(windows)] 是"属性(attribute)"，意思是"仅当编译目标是 Windows 时才把下面这段
//   放进编译"。好处：不存在的 API 不会污染通用代码，各平台也不用装各自的依赖。
// 三个实现对外签名完全一致 → 上层 src/server.rs 只管调 spawn_mic_output(...)，
//   【不需要】任何 if 平台判断；平台差异被彻底关在本文件里。
// 参数 std::sync::mpsc::Sender<ServerEvent>：标准库"多生产者单消费者"通道的发送端。
//   引擎线程是普通同步线程、不会 await，所以用 std 通道（tokio 异步通道留给会 await 的一方，
//   见下面 spawn_capture_monitor 用 tokio 通道的对比说明）。
// thread::spawn(move || ...)：开一个【操作系统线程】跑引擎死循环。
//   move 把 queue（一次 Arc 克隆，只是计数 +1）和 event_tx 的所有权【搬进】闭包 ——
//   不写 move 就是借用，而新线程活多久谁都不知道，借用原栈上的变量编译器会直接拒绝。
//   为什么必须独占一个线程：引擎内部全是阻塞调用（sleep、lock、COM 同步调用），
//   放进 tokio 的工作线程会把那个线程占死，同进程里所有连接一起卡住。
#[cfg(windows)]
pub fn spawn_mic_output(queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
    std::thread::spawn(move || windows_impl::engine(queue, event_tx));
}

/// macOS：cpal 输出流渲染进 BlackHole 2ch（对端应用把输入选为 BlackHole 2ch 即用）
// BlackHole 是 macOS 上的虚拟声卡（等价物），一次装好后系统里就多一对
// "BlackHole 2ch 的输出/输入"；本分支把手机 PCM 喂进它的输出侧。
#[cfg(target_os = "macos")]
pub fn spawn_mic_output(queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
    std::thread::spawn(move || macos_impl::engine(queue, event_tx));
}

/// 其他平台 stub：直接上报"不可用"，保持上层逻辑统一
// 函数名前的下划线 _queue 表示"这个参数声明了但故意不用"：不加下划线 Rust 会报未使用变量警告。
// 类型仍要保持一致，否则三套实现的签名就对不上、上层代码就得分平台写。
// .send(...).ok()：send 返回 Result<(), SendError>，接收端（GUI/服务器任务）可能已经关掉。
//   对我们来说那不算错误（没人听也要继续跑），.ok() 把 Result 转成 Option 再丢弃 = 明确忽略。
//   ⚠ 这类"吞掉发送错误"的写法在本文件很常见：日志/状态上报失败绝不该让音频链路停下。
#[cfg(not(any(windows, target_os = "macos")))]
pub fn spawn_mic_output(_queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
    event_tx.send(ServerEvent::MicEngine { device: None }).ok();
    info!("[MicOut] Virtual mic injection is implemented for Windows/macOS only.");
}

// ── log::info! / warn! 打出来的东西到底去哪了？──────────────────────────
// log crate 只是"门面"：本文件只负责喊，实际写到哪由程序入口决定。
// GUI 版（src/main.rs）装了自写的 DualLogger → 同时进 stderr（有控制台时）和
//   exe 目录下的 audioserver.log（Program Files 里写不动时退回 %APPDATA%\PCAssistant\audioserver.log）。
// CLI 版（src/bin/server.rs）用 env_logger → 只进 stderr，级别看 RUST_LOG。
// 所以本文件里 [MicOut]/[MicMon] 前缀的每条日志都能在上述日志文件里回溯，
// 排查"手机没声"就靠这些行的时间戳。前缀是给人 grep 用的约定，不是框架要求。
/// 启动"CABLE Output 被应用占用"检测线程。
/// 状态翻转时通过通道回传（true = 有应用在录），服务器据此向手机推
/// {"type":"mic_state","active":bool} —— v3.3：这条指令就是手机的
/// 麦克风硬件开关信号：true 才开录、false 立刻停（按需录音）。
/// Windows 实现见 windows_impl::capture_monitor（会话枚举法）。
#[cfg(windows)]
pub fn spawn_capture_monitor(tx: tokio::sync::mpsc::UnboundedSender<bool>) {
    // ── tokio::sync::mpsc::UnboundedSender<bool>：为什么这里换成 tokio 的通道 ──
    // 检测线程（发送方）是普通 std 线程，只能调同步的 send()；接收方在 server.rs 的 async 任务里
    //   await recv()。UnboundedSender 正好适合"发送方不会 async、接收方要异步等"这种组合：
    //   队列无上限（这里一秒最多 4 个 bool，不可能积压），send() 立即返回、永不阻塞检测线程。
    // 对比 std::sync::mpsc：它的 recv() 会【阻塞线程】，放进 async 任务里会占死一个 tokio
    //   工作线程。所以两个通道不是随便挑的：接收端 await → 用 tokio；接收端同步 loop → 用 std。
    // "Unbounded = 无界"的代价是理论上能撑爆内存，本场景每条消息只有 1 字节，忽略不计。
    std::thread::spawn(move || windows_impl::capture_monitor(tx));
}

/// macOS：CoreAudio"有人在录 BlackHole 注入端"检测（见 macos_impl::capture_monitor）
#[cfg(target_os = "macos")]
pub fn spawn_capture_monitor(tx: tokio::sync::mpsc::UnboundedSender<bool>) {
    std::thread::spawn(move || macos_impl::capture_monitor(tx));
}

/// 其他平台 stub：不产生任何事件
#[cfg(not(any(windows, target_os = "macos")))]
pub fn spawn_capture_monitor(_tx: tokio::sync::mpsc::UnboundedSender<bool>) {}

// ── Windows 实现：为什么整块包进一个 mod（模块）─────────────────────────
// mod = Rust 的命名空间。把 Win32/COM 那一大堆 use 和 unsafe 调用关进 windows_impl，
//   对外只露出 engine / capture_monitor 两个 pub(crate) 函数，其他平台完全看不到这些类型。
// pub(crate) 表示"整个 crate（本项目）内可见，但不导出给外部使用者"—— 比 pub 收一档。
// use super::*：把父模块（也就是本文件顶部）已有的名字（MicQueue、ServerEvent、info!、
//   UPLINK_RATE、inject_hint…）一次性再导入，省掉每个子模块重复书写。
#[cfg(windows)]
mod windows_impl {
    use super::*;
    // anyhow::Result<T> 是别名 = std::result::Result<T, anyhow::Error>。
    //   anyhow 的哲学：我不打算按错误类型分支处理，只想把"到底为什么失败"一路带上去，
    //   所以错误值里可以塞任意上下文（.context("...")）并自动串成错误链。
    //   标准库的 Result 要求你为每种错误定义 enum（适合库作者），本项目这种"只要日志"的场景
    //   用 anyhow 写起来短得多 —— 下面所有函数几乎都返回它，配 ? 一路外抛。
    use anyhow::Result;
    use windows::core::GUID;
    use windows::Win32::Media::Audio::{
        eRender, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, IAudioClient, IAudioRenderClient,
        IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
        STGM_READ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    /// 引擎主循环：打开 CABLE Input → pump 注入 → 出错关闭 → 3 秒后重试
    pub(crate) fn engine(queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
        // HRESULT 不消费（已初始化时返回 S_FALSE 也算成功），显式丢弃避免 must_use 警告
        // ── 为什么每个音频线程都要先 CoInitializeEx ────────────────────────
        // WASAPI 的接口（IMMDeviceEnumerator / IAudioClient …）都是 COM 对象，COM 的规矩是
        //   "每个线程在首次使用 COM 之前自己初始化一次"，没初始化就 CoCreateInstance 直接失败。
        // COINIT_MULTITHREADED = 该线程不建消息泵，跨套间调用直接进（我们只用最普通的同步调用）。
        // 返回值 HRESULT 被 #[must_use] 标注，不接就警告；这里用 `let _ =` 明确表达
        //   "我知道有返回值，我故意不用"（同线程重复初始化返回 S_FALSE，也是成功码）。
        // 全程不调 CoUninitialize：这个线程和进程同生共死，退出时系统统一回收，是有意的简化。
        // unsafe { } 块：编译器无法验证 Win32 调用的参数/指针是否合法，必须由写代码的人
        //   手写 unsafe 来"签字担保"。这不是"危险代码"的标记，而是责任边界标记。
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // ── CoCreateInstance + match：两个 Rust/COM 基础点一次讲清 ────────────
        // CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)：按 CLSID 创建 COM 对象。
        //   返回的 Result<T> 里 T 是哪个接口，是靠下一行的类型标注 IMMDeviceEnumerator 反推的
        //   （同一个 CLSID 能问出多种接口，编译器自己猜不出来）。
        //   None = 不做对象聚合；CLSCTX_ALL = 让系统自己决定组件在哪个执行位置加载
        //   （音频枚举器其实住在系统音频服务里，调用是跨进程代理，但写法和本地对象一模一样）。
        // match 必须穷尽所有分支：Result 只有 Ok/Err 两种，两个都得写，漏一个编译器直接拒绝。
        //   Rust 没有"异常穿透"，错误要么显式处理，要么用 ? 显式上抛 —— 这是它可靠性的底座。
        let enumerator: IMMDeviceEnumerator = match unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        } {
            Ok(e) => e,
            Err(e) => {
                // 失败就 return：线程退出、界面显示"引擎不可用"，但服务器其他功能照常。
                //   本文件的总原则：硬件再烂也不能把 WebSocket 服务带崩。
                warn!("[MicOut] COM enumerator failed: {}", e);
                event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                return;
            }
        };

        // ── 无限重试环：常驻后台线程的标配形状 ────────────────────────────
        // loop { } 是永不自然退出的循环（不同于 while，它没有 break 就绝对出不去）。
        // 每轮先尝试打开设备：成功就进入 pump()（它自己内部死循环），只有设备掉了才返回；
        //   VB-CABLE 被卸载重装、系统休眠唤醒、音频服务复位，都是靠这个环自愈的。
        // 魔数 3 秒：更短（100ms）在"设备永久不存在"时会疯狂刷日志和重试；
        //   更长（30s）会让用户在会议软件里选好 CABLE Output 之后干等半分钟才有声音。
        loop {
            match open_cable_render(&enumerator) {
                Ok(stream) => {
                    // 把"正在用哪台设备"上报：Some(名字) = 引擎就绪，None = 不可用/已掉线。
                    // GUI（main.rs 的 MicEngine 分支）据此显示设备名，日志里也能看到格式细节。
                    event_tx
                        .send(ServerEvent::MicEngine {
                            device: Some(stream.device_name.clone()),
                        })
                        .ok();
                    info!(
                        "[MicOut] Injecting into '{}' ({}Hz, {}ch, {}-bit)",
                        stream.device_name, stream.sample_rate, stream.channels, stream.bits
                    );
                    // pump 是阻塞调用：它内部自己 loop，只在设备消失/句柄报错时 return。
                    // 走到下一行说明流已经废了 → 报 None 让界面变灰 → 落到下面的 sleep 重试。
                    pump(&stream, &queue);
                    event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                    warn!("[MicOut] Pump exited (device lost?), retry in 3s");
                }
                Err(e) => {
                    // Err 分支：e 是 anyhow::Error，Display 出来就是"找不到设备"这类完整中文/英文原因。
                    event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                    warn!("[MicOut] {}", e);
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    }

    /// ── 捕获占用检测 v2（"音频会话枚举"法）────────────────────────
    ///
    /// 原理：任何应用把 "CABLE Output" 当麦克风录音时，Windows 会在该
    /// 捕获端点上创建一个音频会话（音量合成器能列出"正在录音的应用"
    /// 就是同一份数据）。每 400ms 枚举一次，只要有一个非系统会话处于
    /// Active 状态 → 有应用正在用麦克风。
    ///
    /// 历史教训：v1 用 PKEY_AudioEndpoint_Supports_EventDriven_Mode 的
    /// CAPTURE_ACTIVE 标志，但 VB-CABLE 虚拟驱动从不上报该值 → 永远检测
    /// 不到占用 → 手机永远等不到唤醒 → 无声。会话枚举不依赖驱动配合，
    /// 由 WASAPI 系统层自己维护，虚拟声卡同样有效（已实测验证）。
    ///
    /// v3.3：检测结果重新成为功能信号 —— 手机按 mic_state 开/关麦克风
    /// 硬件（按需录音，平时硬件关闭不耗电不侵犯隐私）。轮询 250ms，
    /// 开/关感知延迟 ≈ 轮询 250ms + 手机开麦 ~300ms < 0.6 秒。
    // ── 为什么必须"按需开关设备"，而不是让手机一直录（产品规则，不是性能妥协）──
    // 手机麦克风在 App 后台常开 = 耗电 + 隐私红线（用户看到状态栏橙色麦克风点会不安）。
    // 所以产品定死了三态：idle（守护未开）→ standby（已连服务器、麦克风硬件【关闭】）
    //   → live（收到 mic_state{active:true} 才真正 open 硬件开始采集）。
    // 服务器这边的职责被这条规则限定成一句话：准确、及时地告诉手机"现在有没有 PC 应用在录"。
    //   这就是本函数存在的唯一理由 —— 引擎线程（写 CABLE Input）不需要它，常开写静音就行；
    //   但【硬件】必须由手机自己管，所以判断权在 PC、执行权在手机，靠 mic_state 一条消息缝合。
    // 反过来说：如果不做这个检测、让手机永远常录，手机会 24 小时耗电并上传环境声，
    //   这是明确不可接受的，所以这段逻辑不能删，只能更准。
    // macos_impl::capture_monitor 是同一份职责的另一套实现（CoreAudio 属性轮询）。
    pub(crate) fn capture_monitor(tx: tokio::sync::mpsc::UnboundedSender<bool>) {
        // 同上：这个线程也要碰 COM（枚举会话），所以照样先 CoInitializeEx 一次。
        // 两个线程各自初始化各自的 —— COM 的初始化状态是【按线程】记录的，不是全局一份。
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let enumerator: IMMDeviceEnumerator = match unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        } {
            Ok(e) => e,
            Err(e) => {
                warn!("[MicMon] COM enumerator failed: {}", e);
                return;
            }
        };

        // last_active 缓存上一轮结论：只在【状态翻转】的那一帧上报（边沿触发）。
        // 若改成每轮都 send，手机每 250ms 收到一条重复 mic_state，白白耗电也刷屏日志。
        // Rust 里 bool 可以直接 != 比较（bool 实现了 PartialEq），不需要额外处理。
        let mut last_active = false;
        loop {
            // unwrap_or(false)：Result<bool> 的"柔性取法" —— 出错（设备消失、COM 调用失败）
            //   一律当作"没人用"。这是【安全方向】的默认值：检测失败时手机退回 standby 不录音，
            //   最多是"这次没唤醒成功"，绝不会变成"偷偷常开麦克风"。
            //   注意这里没有用 ?：本函数返回 ()，没有可上抛的 Result，只能就地消化。
            let active = cable_output_in_use(&enumerator).unwrap_or(false);
            if active != last_active {
                last_active = active;
                info!(
                    "[MicMon] CABLE Output capture {}",
                    if active { "ACTIVE (an app is using the mic)" } else { "idle (nobody recording)" }
                );
                // 这里的 if/else 表达式直接嵌在宏参数里：Rust 的 if 是【表达式】，
                //   有值（这段是 &str），所以能当"三元运算符"用（Rust 没有三目 ?:）。
                // let _ = tx.send(active)：和 .ok() 同目的——忽略"接收端已关闭"的错误。
                let _ = tx.send(active);
            }
            // 250ms 轮询：这个魔数决定"应用点录音 → 手机开麦"的延迟下限（约 0.25s + 手机 ~0.3s）。
            //   改 50ms：更灵敏，但枚举 COM 会话的频率 ×5，白耗 CPU；
            //   改 1s：用户会明显感到"我这边都开了你那边还没声"。
            // sleep 发生在 std 线程上（不是 async 任务），这里阻塞是完全无害的。
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    /// 枚举虚拟声卡录音端点上的音频会话：有 Active 会话 = 应用在录
    // ── 这个函数怎么"看见"别人在录音（不看驱动脸色）──────────────────────
    // Windows 音频内核给每个"设备 + 客户端"组合都登记一个音频会话对象（音量合成器里
    //   能列出的"正在录音的应用"就是它）。会话有状态：Active / Expired / Inactive。
    // 我们要的只是布尔值：CABLE Output 上存在任何一个 Active 会话 → 有应用在录。
    // 不依赖虚拟声卡驱动配合（VB-CABLE 不实现很多标准属性），由系统自己维护，所以可靠。
    fn cable_output_in_use(enumerator: &IMMDeviceEnumerator) -> Result<bool> {
        // use 写在函数体内部：这些类型【只在这个函数里】用，就近导入让模块顶部保持干净。
        // Interface trait 提供 .cast()（就是 COM 的 QueryInterface 的 Rust 封装）。
        use windows::core::Interface;
        use windows::Win32::Media::Audio::{
            eCapture, IAudioSessionControl2, IAudioSessionManager2, AudioSessionStateActive,
        };

        // 设备名片段每次调用取一次（不是每台设备取一次）：这个函数由占用检测线程
        // 每 250ms 调一轮，配置里改的名字下一轮就生效
        let hint = monitor_hint();
        // EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)：
        //   eCapture = 录音端（麦克风那一侧）；对应 eRender = 播放端（见 open_cable_render）。
        //   第二个参数是【状态过滤器】，DEVICE_STATE_ACTIVE = 只列"现在插着且可用"的设备，
        //   被禁用/已卸载的幽灵设备系统直接帮我们滤掉了，所以循环里不必再判断状态。
        // 结尾的 ?：Result 上的问号 = "成功就取出里面的值继续，失败就立刻 return Err 给调用者"。
        //   能写 ? 的前提是本函数返回 Result<bool>。? 还会自动做错误类型转换
        //   （Win32 的 HRESULT → anyhow::Error，靠 windows crate 实现的 From 转换）。
        let collection = unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)? };
        let count = unsafe { collection.GetCount()? };
        for i in 0..count {
            // 0..count 是 Range（迭代器），配合 for 自动逐个取值 —— Rust 没有 C 的 for(i=0;i<n;i++)。
            // COM 集合只能 GetCount + Item(i) 索引访问（没有 Rust 的迭代器/切片），所以要手写循环。
            let device = unsafe { collection.Item(i)? };
            // 名字转大写后做"包含"匹配（hint 本身也已是大写，见 monitor_hint）。
            if !device_friendly_name(&device).to_uppercase().contains(&hint) {
                continue;
            }
            // device.Activate::<IAudioSessionManager2>()：把"一台设备"变成"能干活的接口"。
            //   目标类型由左边的标注决定 —— 这里要能枚举会话的 SessionManager2，
            //   open_cable_render 里同一手法要的是能开流的 IAudioClient。一台设备，问不同接口得不同能力。
            let mgr: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None)? };
            let sessions = unsafe { mgr.GetSessionEnumerator()? };
            let n = unsafe { sessions.GetCount()? };
            for k in 0..n {
                let ctrl = unsafe { sessions.GetSession(k)? };
                // .cast()：向 COM 对象询问"你还支持 IAudioSessionControl2 吗"（QueryInterface）。
                //   Control2 比 Control 多出进程 ID、更完整的状态信息，所以要升一级接口再问。
                //   升级失败（老驱动/系统会话）用 continue 跳过这一个会话，不影响其他会话 ——
                //   match 的小用途：这里只想"成功取值 / 失败换下一个"两种走法，不需要穷尽错误细节。
                let ctrl2: IAudioSessionControl2 = match ctrl.cast() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                // 系统声音占位会话永远 inactive，跳过纯保险
                // GetState() 返回 AudioSessionState 枚举；这里用 == 比较（枚举默认派生 PartialEq）。
                let state = unsafe { ctrl2.GetState()? };
                if state == AudioSessionStateActive {
                    return Ok(true);
                }
            }
            // 已经命中目标设备名 → 结论出来了，不再看后面的设备（避免同名项/多声道重复判断）。
            return Ok(false);
        }
        Ok(false) // 设备不存在（驱动被卸载）
    }

    /// 已打开的注入流：持有 IAudioClient 与格式信息
    // struct（结构体）= 把相关数据捆成一个类型。这里的捆法很有 Rust 味道：
    //   不写 Close()/Dispose()，而是把 IAudioClient【所有权】放进字段。
    //   MicOutputStream 离开作用域（pump 返回、engine 的 match 分支结束）时自动 Drop，
    //   接口引用计数 -1 → WASAPI 关掉这条流。句柄不可能被忘记关闭，这是 RAII 的核心承诺。
    // 字段全是 Copy/普通值类型，所以 pump() 里可以用 &stream 只读借用而不影响所有权。
    struct MicOutputStream {
        client: IAudioClient,
        // device_name 用 String（拥有所有权的堆上字符串）而不是 &'static str：
        //   名字是运行时从系统读出来的，寿命不属于程序静态区，必须自己持有这块内存。
        device_name: String,
        // 下面三个数是设备 mix 格式的原样抄录，只为日志显示和 pump 里的分支判断。
        // 类型沿用 Windows 头文件的定义（u32 赫兹 / u16 声道 / u16 位深），
        //   Rust 不隐式转换，所以下面用到时都显式 `as usize`。
        sample_rate: u32,
        channels: u16,
        bits: u16,
        // is_f32：设备是否用 32 位浮点存样本。共享模式几乎都是 true。
        //   用 bool 字段而不是每次重算 wFormatTag，是为了把"格式判断"集中在一处。
        is_f32: bool,
    }

    /// 枚举播放设备，找名字含 inject_device_hint（默认 "CABLE Input"）的那台并建立共享渲染流
    fn open_cable_render(enumerator: &IMMDeviceEnumerator) -> Result<MicOutputStream> {
        // hint 每次进函数读一次配置（成本极小，这里 3 秒最多一次）：
        //   用户在 config.json 改 mic.inject_device_hint 后，下一轮重试就能生效，不必重启。
        let hint = inject_hint();
        let collection =
            unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
        let count = unsafe { collection.GetCount()? };
        // Option<(IMMDevice, String)>：元组打包"设备接口 + 它的名字"。
        //   还没找到 = None，找到 = Some((设备, 名字))。Rust 用 Option 取代 NULL：
        //   "可能没有这个东西"必须写在类型里，后面 match 强制处理 None 分支，
        //   所以不可能出现 C 那种"忘了判空指针直接解引用"的事故。
        // mut chosen：变量默认不可变，要重新赋值必须显式写 mut（Rust 的默认是 const）。
        let mut chosen: Option<(IMMDevice, String)> = None;
        for i in 0..count {
            let device = unsafe { collection.Item(i)? };
            let name = device_friendly_name(&device);
            // 转大写后"包含匹配"：驱动版本/系统语言会让显示名变成
            // "CABLE Input (VB-Audio Virtual Cable)" 这种带前后缀的形式，精确等于会失配。
            if name.to_uppercase().contains(&hint) {
                chosen = Some((device, name));
                // break 跳出 for：找到第一个就够（同名多台时选先出现的那台）。
                //   ⚠ 注意：所以若系统里同时存在两根名字含 "CABLE INPUT" 的设备，
                //   实际用的是枚举顺序靠前那根，改 config 里的 hint 更精确才能换。
                break;
            }
        }
        // match 一次把 Option 拆成两个分支，顺便把元组【解构】成两个具名变量。
        //   解构 pattern `Some(d)` 直接把里面的元组交给 (device, device_name)，不需要 .unwrap()。
        let (device, device_name) = match chosen {
            Some(d) => d,
            // 报错里带上"我在找谁"，用户改了配置写错名字时一眼能看出来
            // anyhow::bail! = "抛出这个错误并立刻 return"，等价于 return Err(anyhow!(...))。
            None => anyhow::bail!(
                "virtual audio render device '{}' not found — install VB-Audio Virtual Cable \
                 or fix mic.inject_device_hint in config.json",
                hint
            ),
        };

        // ── WASAPI、共享模式 vs 独占模式（本文件最关键的系统概念）────────────
        // WASAPI = Windows Audio Session API（Vista 起的音频内核），入口就是 IAudioClient。
        // 打开一台设备时可以选两种"合作方式"：
        //   共享模式 AUDCLNT_SHAREMODE_SHARED：数据交给系统混音器（Audio Engine），
        //     格式【由混音器定】（GetMixFormat 问出来的就是它，通常 2ch/48000Hz/float32），
        //     我们写的那段和别人的声音一起被混合播放/录制。
        //   独占模式 AUDCLNT_SHAREMODE_EXCLUSIVE：绕过混音器直连硬件，延迟最低、格式自选，
        //     但一台设备同一时刻只能有一个独占者，会把其他人的声音全部挤掉；
        //     而且【虚拟声卡驱动基本不支持独占】，硬试会得到 AUDCLNT_E_UNSUPPORTED_FORMAT。
        // 本文件永远用共享模式：我们要的就是"和系统里的其他声音共存"，让会议软件同时听到
        //   手机 + 扬声器；独占在这里没有任何好处。
        // 另外注意我们【不自己挑格式】：共享模式下只有混音器格式是唯一合法输入，
        //   传别的格式指针 Initialize 会失败 —— 所以必须先 GetMixFormat 再照原样喂回去。
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        // GetMixFormat() 返回 *mut WAVEFORMATEX（C 结构体指针），内存是系统用 CoTaskMemAlloc 给的，
        //   按契约必须由我们自己 CoTaskMemFree 归还（下面释放；⚠ 见中途 ? 的泄漏说明）。
        let fmt_ptr = unsafe { client.GetMixFormat()? };
        // &*fmt_ptr：先解引用拿到 C 结构体的引用，交给 Rust 做字段读取。
        //   ⚠ 这一步是 unsafe 的经典场景：指针为 null 时这里是未定义行为。
        //   依赖的是"GetMixFormat 成功返回就一定非空"这条系统契约（它失败会返回 Err 而不是 NULL）。
        let fmt = unsafe { &*fmt_ptr };
        let sample_rate = fmt.nSamplesPerSec;
        let channels = fmt.nChannels;
        let bits = fmt.wBitsPerSample;
        // 共享模式下 mix 格式通常是 float32（含 0xFFFE extensible），PCM 则是 s16
        // WAVE_FORMAT_IEEE_FLOAT = 3，WAVE_FORMAT_EXTENSIBLE = 0xFFFE（真类型藏在子 GUID 里，
        //   这里按惯例当 float32 处理，与所有共享模式实现一致）。
        //   这两个魔数是 Microsoft 的格式编号表，改了判断就会把 float 设备认成 16 位设备，
        //   后果是 pump 走错分支、按 i16 写 float 缓冲区 → 全是噪音。
        let is_f32 = bits == 32 && (fmt.wFormatTag == 3 || fmt.wFormatTag == 0xFFFE);

        // GetDevicePeriod 问设备"默认每隔多久喂一次数据"，单位是 100 纳秒
        //   （1 秒 = 10_000_000 个这样的单位；常见取值 30_000~100_000，即 3~10 毫秒）。
        // None = 不指定事件驱动句柄；Some(&mut period) = 把结果写进我的变量。
        //   Rust 用 Option<&mut T> 表达 C 里"这个出参可以是 NULL"的语义。
        let mut period: i64 = 0;
        unsafe { client.GetDevicePeriod(None, Some(&mut period))? };
        // ⚠ 注意（可疑缺陷，未改动）：上面这行的 ? 会让函数在 GetDevicePeriod 失败时【立即返回】，
        //   于是下面那句 CoTaskMemFree 永远执行不到，fmt_ptr 这块系统内存就泄漏了。
        //   引擎每 3 秒重试一次，如果长期卡在 GetDevicePeriod，就是缓慢的内存泄漏。
        //   规范做法：用一个实现 Drop 的小包装（RAII guard）持有 fmt_ptr，或把这里的 ? 也
        //   改成"先记结果、释放后再判断"。留给你判断，不在这次纯注释改动里动手。
        let init_result = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,    // 渲染流，无 stream flags
                period,
                0,
                fmt_ptr,
                None,
            )
        };
        // Initialize 六个参数依次是：共享模式 / 流标志(0=无) /
        //   hnsBufferDuration(缓冲时长，100ns 单位；传 period 表示开约一个周期的环形缓冲，
        //   写 0 = 用系统默认大小) / 事件驱动句柄(0 = 不用事件，本文件靠轮询 padding) /
        //   格式指针(必须是 GetMixFormat 那个) / 隐藏声道掩码 None。
        // 成功与否【先存进变量】而不直接 ?，就是为了下面这行释放能一定执行：
        //   ? 会立即 return，写在它后面的代码不会跑；先存再释放最后才 ? 是 FFI 里的常用顺序。
        unsafe { CoTaskMemFree(Some(fmt_ptr as *mut _)) };
        // 到这儿才把错误抛给调用方（engine 会打日志 + 3 秒后重试）。
        init_result?;

        Ok(MicOutputStream {
            client,
            device_name,
            sample_rate,
            channels,
            bits,
            is_f32,
        })
    }

    /// 帧泵：轮询缓冲区空余量（GetBufferSize - GetCurrentPadding），
    /// 从队列取 i16 单声道样本、复制到各声道、转成设备位深写入。
    /// 队列空时写静音 —— 这就是"没说话也是合法麦克风"。
    // ── pump 是全文件的心脏：一个自己掌握节拍的写入循环 ────────────────────
    // 共享模式下设备侧有一块【环形缓冲区】，音频引擎按实时速度从里面取走播出去：
    //   GetBufferSize()   = 整块缓冲能装多少【帧】（1 帧 = 同一时刻的一组声道样本）
    //   GetCurrentPadding() = 里面已经有多少帧还没被取走（等着播）
    //   available         = 容量 − 已占用 = 我此刻还能安全写入的帧数
    // 写超了 → 覆盖还没播完的数据（爆音）；写少了 → 缓冲区出现空洞（咔哒/断续）。
    // 所以每一步都以 available 为准，绝不"凭感觉"写固定数量。
    // "帧"和"样本"必须分清：立体声 1 帧 = 2 个样本，所以内存偏移永远写成 帧数 × 声道数。
    fn pump(stream: &MicOutputStream, queue: &MicQueue) {
        // GetService::<IAudioRenderClient>()：从 IAudioClient 里再问出"真正操作缓冲区"的服务接口。
        //   match 在这里只做"成功拿到 / 失败退出"两件事，失败直接 return 让 engine 重试。
        let render: IAudioRenderClient = match unsafe { stream.client.GetService() } {
            Ok(r) => r,
            Err(e) => {
                warn!("[MicOut] GetService(IAudioRenderClient) failed: {}", e);
                return;
            }
        };
        // Start()：让渲染流开始被音频引擎消费（此前写进去的东西不会被播）。
        //   is_err() 只问"是不是 Err"，不需要拿到里面的具体值，比写整个 match 更简洁；
        //   同类写法还有 is_ok() / ok() / unwrap_or()，都是"不关心细节只求个方向"的取法。
        if unsafe { stream.client.Start() }.is_err() {
            warn!("[MicOut] IAudioClient::Start failed");
            return;
        }
        let ch = stream.channels as usize;
        // as usize：Rust 不做隐式整数转换（避免无声溢出），所有拓宽都得写 as。
        //   转成 usize 是因为切片长度、下标只能用 usize（平台指针宽度整数）。
        // unwrap_or(1024)：问不到缓冲区大小时用 1024 帧兜底（48k 下约 21ms）。
        //   这是"取一个安全的保守默认值"惯用法：宁可少写一点，也不要 panic。
        // ⚠ 注意（既有行为，未改动）：buf_frames 只在进循环前读一次，之后不复查。
        //   若系统中途改了缓冲大小，available 会一直按旧值算，最坏导致 GetBuffer 失败 →
        //   本函数 return → engine 3 秒后重开设备，走的是自愈路径，所以不算致命问题。
        let buf_frames = unsafe { stream.client.GetBufferSize() }.unwrap_or(1024);
        // v3.4.4 重采样进位状态（仅上行速率≠设备速率时使用）：
        // carry = 跨泵周期留存的少量上行样本；rpos = 读取位置相对 carry 头部的小数偏移
        // 这两个变量必须活在整个循环之外（每轮接着上一轮的进度继续），所以声明在这里。
        //   carry 只有个位数长度；rpos 用 f64（浮点）因为 step 不是整数，整数存不下小数进度。
        // 不带这套进位状态的话，每轮都从 carry 头部重新开始，44.1k→48k 的零头永远被丢弃，
        //   结果是音调缓慢漂移 + 周期性重复样本（听感是"轻微金属感/颤动"）。
        let mut carry: std::collections::VecDeque<i16> = std::collections::VecDeque::new();
        let mut rpos: f64 = 0.0;
        loop {
            let padding: u32 = match unsafe { stream.client.GetCurrentPadding() } {
                Ok(p) => p,
                // Err 一律 return：这是"设备消失/流被系统销毁"的信号（拔驱动、音频服务重启、
                // 设备被独占）。回到 engine() 报 None，3 秒后重建 —— 本文件所有硬故障都走这条路。
                Err(_) => return,
            };
            // 容量 − 已占用 = 可写帧数；saturating_sub 保证不会被"padding 突然大于容量"
            //   这种边界情况减成负数下溢（真发生时下溢会变成一个巨大数字，直接内存炸裂）。
            let available = buf_frames.saturating_sub(padding);
            if available == 0 {
                // 缓冲区被还没播完的数据填满 → 睡 2ms 再看。
                // 为什么是 2ms：设备周期一般 3~10ms，睡得远小于周期就不会来不及写；
                //   睡太久（比如 50ms）会出现下一轮来不及写够 → 空洞爆音；
                //   完全不睡（0）就是纯轮询烧 CPU。2ms 是"远小于周期又不空转"的折中。
                // continue = 跳过本轮剩下的所有代码，回到 loop 顶部重新问 padding。
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            // GetBuffer(帧数) 返回 *mut u8：指向缓冲区里允许我们写的那段【裸内存】。
            //   它不属于我们、不能保存跨轮使用、也不能在 ReleaseBuffer 之后再碰。
            //   之所以是裸指针而不是 &mut [u8]：这块内存既没有 Rust 的生命周期，长度也只由
            //   我们自己记住（就是 available × 声道 × 字节数），完全靠纪律 —— unsafe 的实质就在这。
            let ptr: *mut u8 = match unsafe { render.GetBuffer(available) } {
                Ok(p) => p,
                Err(_) => return,
            };
            // 一次性从队列取需要的帧数样本，不足补静音
            let frames = available as usize;
            // 每轮新建一个 Vec<i16> 作为"单声道中转数组"。Vec 本身在栈上只有
            //   （指针, 长度, 容量）三个字段，数据在堆上；离开作用域自动释放，无需手写 free。
            // ⚠ 注意（可选优化，未改动）：高频路径上每轮一次堆分配。理论上可以复用同一个 Vec
            //   （clear() 保留 capacity），但省下的那点 malloc 远不如"复用忘了清空"这类风险贵。
            let mut mono: Vec<i16> = Vec::with_capacity(frames);
            // 本轮到底有没有拿到真实上行音频。下面节流要用：
            // 有真实音频时【绝不许睡觉】（睡了就是录音延迟），
            // 只有"纯写静音保活"的那种空转才需要被限速。
            // 这里故意不写初始值：下面那个块一定会赋值，写了 rustc 反而报
            // "value assigned is never read"。
            // 补充：`let had_uplink;`（先声明不赋值）是合法的 —— Rust 做"确定初始化"检查
            //   （definite initialization），只要所有能走到使用的路径都赋过一次值就放行，
            //   少写一个假初始值既省一次赋值也避免"初始值忘了覆盖"的 bug。
            let had_uplink;
            // 一个不带变量的【裸作用域 { }】：里面 lock() 得到的 MutexGuard 在这个花括号
            //   结束时必定 drop（= 必定解锁）。这是 Rust 缩小锁范围的标准手法 ——
            //   锁只覆盖真正碰队列的这几行，后面的浮点插值和写指针都不占锁，
            //   网络线程 push 数据不会被音频线程长时间卡住。
            {
                // 每轮现读原子值（不用锁、不阻塞）：手机中途切 44.1k/48k，下一轮自动跟上。
                let urate = UPLINK_RATE.load(std::sync::atomic::Ordering::Relaxed);
                // resample 条件 = 速率不一致 且 上行速率看着合法（>=8000）。
                //   带上 >=8000 是防脏值：万一读到 0，step 会变成 0，插值退化成"每个输出样本都取
                //   同一个点"（声音被拉长成卡顿），宁可这时也走直取路径。
                let resample = urate != stream.sample_rate && urate >= 8000;
                let mut q = queue.lock().unwrap();
                had_uplink = !q.is_empty();
                if !resample {
                    // 常见路径：速率一致，直接逐样本搬运（与旧版完全相同）
                    // pop_front 取队头；队列空则 unwrap_or(0) 补数字静音 —— 这就是
                    //   "没人说话时也是一支持续供电的合法麦克风"的实现：永不断流。
                    for _ in 0..frames {
                        mono.push(q.pop_front().unwrap_or(0));
                    }
                } else {
                    // 线性插值重采样：每个设备样本 = 两个相邻上行样本的加权平均。
                    // step = 平均每产出一个设备样本要消耗多少上行样本
                    //（44.1k→48k 时 step≈0.91875）。
                    let step = urate as f64 / stream.sample_rate as f64;
                    // ── 44100 → 48000 到底怎么算（把算式写明白）───────────────
                    // step = 上行速率 ÷ 设备速率 = 44100 ÷ 48000 = 0.91875
                    //   含义：每产出【1 个】设备样本，需要在上行样本轴上前进 0.91875 个位置。
                    //   小于 1 是因为 44.1k 的 44100 个样本要"摊"满 48000 个播放位置 → 序列被拉长。
                    // 第 i 个输出样本对应上行轴位置 p = rpos + i × step（一般是小数）。
                    // 取 p 左右两个整数样本 a = carry[i0]、b = carry[i0+1]（i0 = ⌊p⌋），
                    //   小数部分 f = p − i0，则：
                    //       out[i] = round( a × (1 − f) + b × f )      ← 线性插值（一次方）
                    //   f=0 时完全取左点，f=0.5 时正好是两点平均 —— 这就是"线性"的含义。
                    // 为什么非插值不可：不插值等于把 44100 个样本当成 48000 个来播，
                    //   速度变成原来的 48000/44100 ≈ 1.0884 倍，音高高约 1.5 个半音，
                    //   听感就是"人声变尖、语速变快"（旧版本踩过这个坑）。
                    // 反方向 48000 → 44100（step≈1.0884，抽稀）也走同一段代码，公式不用改。
                    // need = 本轮需要从队列再补进 carry 的样本数：
                    //   最后一个输出用到的轴位置是 (frames−1)×step，加上进位 rpos，向上取整，
                    //   再 +1 是因为插值要用到 i0 的【右邻点】。少留 1 个就会在每轮末尾
                    //   取不到右邻点、退化成复制左点（听感是极轻微的周期性粗糙）。
                    let need = ((frames as f64 - 1.0) * step + rpos).ceil() as usize + 1;
                    // 缺多少补多少；队列空时补 0（数字静音），保证插值窗口永远有两点可读。
                    while carry.len() < need {
                        carry.push_back(q.pop_front().unwrap_or(0));
                    }
                    for i in 0..frames {
                        // i as f64 * step：整数不能和小数直接运算，必须先 as 转换（Rust 无隐式转换）。
                        let p = rpos + i as f64 * step;
                        // p as usize = 向零取整（这里 p 恒非负，所以等价于 floor），得到左点下标。
                        let i0 = p as usize;
                        // f = 小数权重，范围 [0,1)：越接近 1 越偏向右点 b。
                        let f = p - i0 as f64;
                        // carry.get(i) 返回 Option<&i16>：不 panic 的越界安全读法，
                        //   所以默认值要写成引用 &0（不是 0），再用 * 解引用成 i16，最后 as f64。
                        //   为什么先转 f64 再算：i16 乘 (1.0 − f) 在 Rust 里根本不存在这个运算，
                        //   而整数截断乘法则会把 0.x 直接变成 0，精度全丢。
                        let a = *carry.get(i0).unwrap_or(&0) as f64;
                        // 右邻点不存在时退化成"复制左点"（等价于忽略 f）。正常路径永远拿得到，
                        //   这层兜底是为了"指针/长度边界绝不 panic"—— 音频线程 panic 等于全线断电。
                        //   unwrap_or_else 与 unwrap_or 的区别：后者的默认值无条件计算，
                        //   前者只在真需要时才执行闭包（这里默认值本身是一次 get，能省则省）。
                        let b = *carry
                            .get(i0 + 1)
                            .unwrap_or_else(|| carry.get(i0).unwrap_or(&0))
                            as f64;
                        // 加权平均后【必须 round 再 as i16】：Rust 的浮点→整数 as 是向零截断，
                        //   不 round 会让音量系统性偏小约 0.5 LSB，且把插值误差变成可听的量化噪声。
                        mono.push((a * (1.0 - f) + b * f).round() as i16);
                    }
                    // 丢弃已经插值过去的前端样本，rpos 归到 [0,1) 区间
                    // last_pos = 本轮最后一个输出用到的轴位置；floor 之后就是"左边这些点
                    //   以后再也用不到了"，可以整批扔掉。
                    // ⚠ 注意（别顺手改成 round/ceil）：多扔一个 → 下轮起点前跳，出现周期性咔哒；
                    //   少扔一个 → 同一个样本被重复插值，出现轻微回声感。floor 是唯一正确的取整方向。
                    let last_pos = rpos + (frames as f64 - 1.0) * step;
                    let consumed = last_pos.floor() as usize;
                    if consumed > 0 {
                        // min(carry.len()) 只是防越界（正常时 consumed ≤ carry.len()）。
                        for _ in 0..consumed.min(carry.len()) {
                            carry.pop_front();
                        }
                    }
                    // 减掉整数部分，把进度归一化回 [0,1)：整数进度已经交给 carry 的下标表达了。
                    rpos = last_pos - consumed as f64;
                    // 保险阀：浮点抖动也不允许 carry 无限膨胀（正常应 ≤3 个）
                    // ⚠ 注意（未改动）：真被触发时丢的是【真实音频样本】。1 个样本 = 48k 下约 21µs，
                    //   听不出来；它的意义是"绝不让内存无限增长"，属于兜底而非常规路径。
                    //   正常情况这段 while 一次都不会执行，若日志/火焰图里看到它频繁生效，
                    //   说明重采样逻辑有 bug，该查的是上面 need/consumed 的算式。
                    while carry.len() > 4 {
                        carry.pop_front();
                    }
                }
            }
            unsafe {
                // ── 把裸指针变成能 for 循环的切片（slice）─────────────────────
                // from_raw_parts_mut(指针, 元素个数)：unsafe 的"我要一个 &mut [T]"操作。
                //   元素个数必须是【帧数 × 声道数】（frames * ch），写成 frames 就是只覆盖半个缓冲，
                //   写大了就是越界写系统内存 —— 这个数是整段代码里最需要盯住的一行。
                // 变成切片之后就是安全的普通 Rust：chunks_mut(ch) 把扁平数组切成"每帧一组声道"，
                //   zip(mono.iter()) 把每组和对应的单声道样本配对，一趟循环同时完成"上混 + 格式转换"。
                if stream.is_f32 {
                    let out = std::slice::from_raw_parts_mut(ptr as *mut f32, frames * ch);
                    // 解构模式 (f, &s)：左边 f 是 &mut [f32]（这一帧的声道们），
                    //   右边写 &s 是把 i16【按值拷出来】（i16 是 Copy，写 &s 只是匹配掉引用）。
                    for (f, &s) in out.chunks_mut(ch).zip(mono.iter()) {
                        // i16 → float 归一化：i16 的范围是 -32768..32767，除以 32768 落到 [-1, 1)，
                        //   这正是 WASAPI float 域"满幅"的定义。
                        //   ⚠ 魔数 32768（=2^15）不能随手改：改成 32767 音量略抬（正峰可能溢出到 >1.0），
                        //   改成 16384 = 突然响 6dB，改成 65536 = 小声 6dB —— 它只由格式定义决定。
                        let v = s as f32 / 32768.0;
                        // 单声道 → 多声道：同一个值复制给每个声道（虚拟声卡是立体声，
                        //   两耳一样才像一个"正常的全指向麦克风"；只写左声道会出现"只有一边有声音"）。
                        for c in f.iter_mut() {
                            *c = v;
                        }
                    }
                } else if stream.bits == 16 {
                    // 16 位整数格式（老驱动/非常规配置）：样本原样搬运，不做归一化。
                    let out = std::slice::from_raw_parts_mut(ptr as *mut i16, frames * ch);
                    for (f, &s) in out.chunks_mut(ch).zip(mono.iter()) {
                        for c in f.iter_mut() {
                            *c = s;
                        }
                    }
                } else {
                    // 其他位深（少见）：写静音保活
                    // ⚠ 注意（既有取舍，未改动）：走到这条分支时设备状态仍显示"注入中"，
                    //   但手机声音是【听不见的】。24bit 需要按 3 字节手拼样本，目前没有任何
                    //   真实虚拟声卡会走到这里（共享模式几乎恒为 float32），所以保持"保活不发言"。
                    // frames * ch * (bits / 8)：整数除法把位深换算成字节数（24/8=3）。
                    //   fill(0) 等价于 memset，把这段全部写成 0 = 数字静音。
                    std::slice::from_raw_parts_mut(ptr, frames * ch * (stream.bits as usize / 8))
                        .fill(0);
                }
                // ReleaseBuffer(本次写入的帧数, 标志)：把缓冲区交还给 WASAPI。
                //   交还之后 ptr 立刻失效，再用就是野指针（所以本函数每个循环都必须重新 GetBuffer）。
                //   第二个参数可以传 AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY 告诉系统"这段和上段不连续"，
                //   我们一直写连续数据、也不需要静音标记，所以传 0。
                if render.ReleaseBuffer(available, 0).is_err() {
                    return;
                }
            }
            // ── v3.7 CPU 修复：给"没人听的静音泵"限速 ─────────────────────
            // 这个循环原本【整条写入路径上没有任何节流】，只靠 WASAPI 的
            // padding 天然阻塞。问题是：只要没有应用把 CABLE Output 当麦克风
            // 录音（手机断开时就是常态），音频引擎根本不向前推进，
            // GetCurrentPadding() 永远返回 0 → available 恒等于整个缓冲区
            // → 于是以极限速度反复写同一份静音，实测白烧 100% 一个核心
            // （任务管理器里 audioserver 常年 14~16%，电源计划判"非常高"）。
            //
            // 只在"本轮没取到真实上行音频"时歇 2ms：
            //   · 静音保活路径 500 轮/秒封顶，CPU 直接归零到百分之一以下；
            //   · 真实录音路径 had_uplink=true，一行都不睡，延迟完全不变。
            // ── 再补一句"为什么没人听就必须睡"（给初学者讲透这个坑）──────────
            // 共享模式的环形缓冲只有在【有人真的从 CABLE Output 录音】时才被音频引擎消费。
            // 手机断开、没有应用录它 = 常态，此时 padding 恒为 0 → available 恒等于整个缓冲区
            //   → 循环变成"以 CPU 极限速度反复写同一份静音"，每轮还多付一次 COM 调用开销。
            // 实测：任务管理器里 audioserver 常年 14~16%（相当于白烧满一个核心），
            //   电源计划因此判定"功耗：非常高"；加上这个 2ms 之后降到不足 1%（约 0.4%）。
            // 思路不是"把线程优先级调低"，而是"没有意义就别跑"：空转路径封顶 500 轮/秒。
            // 为什么判据是 had_uplink 而不是 padding/available：队列非空是唯一可靠的
            //   "有人在供数据"证据（手机一断网队列立刻空），而且它已经在手里，不必再问系统。
            // 改这里的后果：删掉这段 = 回到 16% CPU；改成睡 20ms = 真实录音时也可能来不及写
            //   （因为条件写反或 had_uplink 判定变松的话），延迟立刻上来 —— 别顺手"优化"。
            if !had_uplink {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }

    /// 通过属性存储读设备友好名（与 server.rs 捕获端同一手法）
    // ── 为什么"就读个设备名"要写这么多 unsafe 指针算术 ─────────────────────
    // Windows 把每台设备的元数据放在"属性存储 PropertyStore"里，取回来的值是 PROPVARIANT ——
    //   一个 C 语言的"带类型标签的联合体"：开头几个字节说明类型，值按类型放在固定偏移处。
    // windows crate 把这个 C 结构的原始内存直接交给我们，而 Rust 没有任何安全类型能表达
    //   "这里可能是宽字符串、可能是整数、也可能是数组"的联合体，所以只能自己按偏移量读。
    // 这就是 unsafe 在这类项目里不可避免的根本原因：只要和操作系统 API 对话，边界处
    //   必然要么手算偏移（本函数），要么自己按 C 规则声明结构体布局（见 vcam.rs 的 #[repr(C)] 讨论）。
    // 另一条更稳的路是让 windows crate 提供 PROPVARIANT 的 String 封装，但那需要额外 feature；
    //   目前的代价是：这几行必须跟着 Windows 的 ABI 定义走，改错一个数字就永远读不到名字。
    fn device_friendly_name(device: &IMMDevice) -> String {
        // PKEY_Device_FriendlyName（Windows SDK 标准定义）
        // PROPERTYKEY = { GUID fmtid, u32 pid }，是系统里"哪个属性的哪一列"的坐标。
        // const（不是 static）：编译期常量，每个使用点直接展开，不占独立内存地址。
        // ⚠ 这一串 GUID 数字和 pid:14 都是从 Windows SDK 头文件抄来的【协议编号】，
        //   改了不会报错、只会读不到值（函数返回 "Unknown"，表现为"找不到 CABLE Input"）。
        //   想读别的属性（例如设备 ID）就换另一对官方 GUID/pid，别改这对。
        const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
            fmtid: GUID::from_values(
                0xa45c254e,
                0xdf1c,
                0x4efd,
                [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
            ),
            pid: 14,
        };
        unsafe {
            // 这里是一串【嵌套 match】：OpenPropertyStore 失败、GetValue 失败、类型不是宽字符串、
            //   指针为 NULL —— 四种情况全部落到同一个答案 "Unknown"。
            // 这么写的道理：本函数的结果只用于字符串匹配和日志显示，任何一步读不出来都不值得
            //   向上抛错（抛错会让整台设备被跳过，反而连"有一台设备"这个事实都丢了）。
            // STGM_READ = 只读打开属性存储（我们没资格改系统设备的属性）。
            match device.OpenPropertyStore(STGM_READ) {
                Ok(store) => match store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) {
                    Ok(pv) => {
                        // &pv as *const _ as *const u8：拿到 PROPVARIANT 的起始地址，并当成
                        //   "字节地址"来看 —— 因为接下来要按字节偏移读联合体内容，只能用 u8 指针。
                        let pv_ptr = &pv as *const _ as *const u8;
                        // 头两个字节是 VARTYPE（类型标签），读成 u16。
                        let vt = *(pv_ptr as *const u16);
                        if vt == 31 {
                            // VT_LPWSTR：宽字符串指针在 offset 8
                            // 31 = Windows VARTYPE 表里"VT_LPWSTR"的编号（又是协议魔数，改了判断就失灵）。
                            // offset 8：VARTYPE(2 字节) + wReserved(6 字节) 之后正好是联合体起始，
                            //   字符串指针就放在那里。两次解引用：外层是"属性值里存的那个指针"。
                            let pwsz = *(pv_ptr.add(8) as *const *const u16);
                            if !pwsz.is_null() {
                                // C 风格字符串以 NUL(0) 结尾，但 Rust 的 String 必须带长度，所以先扫一遍。
                                // (0..) 是无限 Range 迭代器，take_while 遇到 0 就停，count() 数出长度 ——
                                //   这是"用迭代器写一个 C 循环"的地道 Rust 写法，不写 while + 可变下标。
                                // ⚠ 注意（未改动）：这里信任系统的字符串一定有终止符。万一系统给了
                                //   一个没结尾的缓冲区，本行会一路向后读内存（越界读）。
                                //   更硬的做法是先调 lstrlenW 问长度；实践中 SDK 契约成立，故保持原样。
                                let len =
                                    (0..).take_while(|&i| *pwsz.add(i) != 0).count();
                                // from_raw_parts(指针, 元素个数) 把裸指针借成 &[u16]，元素是 UTF-16 码元。
                                let slice = std::slice::from_raw_parts(pwsz, len);
                                // lossy：遇到无法解码的码元序列用替代字符（而不是返回 Err）。
                                //   设备名只用来做匹配和显示，为它失败不值得 —— 同类还有
                                //   String::from_utf8_lossy（见 macos_impl::device_name）。
                                String::from_utf16_lossy(slice)
                            } else {
                                "Unknown".to_string()
                            }
                        } else {
                            "Unknown".to_string()
                        }
                    }
                    Err(_) => "Unknown".to_string(),
                },
                Err(_) => "Unknown".to_string(),
            }
        }
    }
}

// ═════════════════════════ macOS 实现（v3.5 移植）═════════════════════════
//
// 与 Windows 版一一对应，只是把"WASAPI 渲染进 VB-CABLE"换成
// "cpal 输出流渲染进 BlackHole 2ch"，把"会话枚举占用检测"换成
// "CoreAudio IsRunningSomewhere 属性轮询"。
//
// ⚠️ 诚实声明：本模块在 Windows 上【无法编译验证】（coreaudio-sys 的绑定
// 要在苹果环境生成），是照着 cpal 0.15.3 源码与 CoreAudio C API 写的。
// 明天在 Mac 上首次 `cargo build` 时这里最可能报错，属预期内，逐个修即是。
// 好消息：它被 #[cfg(target_os = "macos")] 门控，对 Windows 生产路径零影响。
// ── cpal 是什么，为什么 Mac 这条路换用它 ────────────────────────────────
// cpal（"Cross-Platform Audio Library"，Cargo.toml 里的 cpal = "0.15"）是 Rust 生态标准的
//   跨平台音频 I/O 库：同一套 API，在 Windows 底下走 WASAPI、macOS 走 CoreAudio、
//   Linux 走 ALSA/JACK。它把"打开设备 + 建流 + 回调喂样本"这套脏活封装掉了。
// 为什么 Windows 分支不用 cpal 而手写 WASAPI：我们需要 [1] 按设备名精确挑虚拟声卡、
//   [2] 自己控制共享模式与 mix 格式、[3] 枚举"音频会话"判断谁在录音。第三项 cpal 完全没有，
//   前两项在当时的版本上也不如直调 API 可控 —— 于是 Windows 直连系统，macOS 用 cpal 省事。
// 三个核心 trait（trait = Rust 的接口/抽象行为，类似其他语言 interface）：
//   HostTrait：音频主机（一个平台的音频系统入口），提供 default_host() 和枚举设备的方法；
//   DeviceTrait：一台音频设备，能读名字/默认配置，最重要的就是 build_output_stream；
//   StreamTrait：建好的流本身，play()/pause() 控制它是否真的在跑。
//   use cpal::traits::{DeviceTrait, HostTrait, StreamTrait}; 这三行不是可选项 ——
//   Rust 里"能用某个方法"等于"那个 trait 必须在作用域内"，少 import 一行就报 no method found。
#[cfg(target_os = "macos")]
mod macos_impl {
    use super::*;
    use anyhow::Result;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    // 局部导入 std 通道的 channel()：下面用 (err_tx, err_rx) = channel::<()>() 建一条
    //   "只能传『发生了』这一个信号"的通道（单元类型 () 不占空间，纯粹当事件用）。
    use std::sync::mpsc::channel;

    /// 注入目标设备名片段（BlackHole 的播放端；env PCSPEAKER_INJECT_DEVICE 可覆盖）
    // 环境变量当"临时开关"：调试时改一行命令就能换目标设备，不用重新编译；
    //   正式用户走 config.json。to_ascii_lowercase 后与设备名做包含匹配（和 Windows 同一套规则）。
    fn render_needle() -> String {
        let w = std::env::var("PCSPEAKER_INJECT_DEVICE").unwrap_or_default();
        if w.is_empty() {
            "blackhole 2ch".to_string()
        } else {
            w.to_ascii_lowercase()
        }
    }

    /// 引擎主循环：建流 → 报错/掉线 → 释放 → 3 秒后重试（与 windows_impl::engine 同构）
    // 结构和 Windows 版一模一样（loop + match + 3 秒重试），差别只在"流"的所有权：
    //   cpal::Stream 一旦被 drop 就【立即停止并释放】，所以必须把它当变量养在作用域里
    //   （Rust 的 RAII：资源寿命 = 变量寿命）。这里故意让 stream 活过 err_rx.recv()，
    //   收到错误信号后才显式 drop(stream) —— 先关流、再重建，顺序不能反。
    pub(crate) fn engine(queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
        loop {
            match build_stream(&queue) {
                Err(e) => {
                    // 建流失败（多半是 BlackHole 没装）：报"不可用"、打日志、睡 3 秒再来。
                    event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                    warn!("[MicOut] {}", e);
                }
                // Ok 分支里把元组【解构】成三个具名变量：设备名 / 流本体 / 错误信号接收端。
                Ok((name, stream, err_rx)) => {
                    // if let 是"只关心某一种 pattern"的轻量 match：
                    //   这里只想处理 Err（play 失败），成功就继续往下走，不值得为它写整个 match。
                    //   同理下面还有 if let Err(e) = stream.play() / let Some(name) = ... else。
                    if let Err(e) = stream.play() {
                        event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                        warn!("[MicOut] cpal render play failed: {}", e);
                        // 显式 drop 只是把"这里流结束了"写明白（不写也会在作用域结束时自动 drop），
                        // 因为后面还要 continue 回到循环顶部重建，先释放再睡比较干净。
                        drop(stream);
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        continue;
                    }
                    event_tx
                        .send(ServerEvent::MicEngine { device: Some(name.clone()) })
                        .ok();
                    info!("[MicOut] Injecting into '{}' (macOS cpal render)", name);
                    // 阻塞等 cpal 错误回调投信号（设备消失/流被系统终止）
                    // err_rx.recv() 会【睡着等】，因此这个线程在正常工作时 CPU 几乎为 0
                    //   （不需要像 Windows 那样自己轮询 padding —— cpal 的回调线程才是干活的）。
                    // 返回 Option：通道被关掉（发送端全 drop）时得到 None，这里同样继续走重建流程。
                    let _ = err_rx.recv();
                    event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                    warn!("[MicOut] Render stream lost — retry in 3s");
                    drop(stream);
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    }

    /// 找 BlackHole 输出侧设备并按其真实默认格式建 cpal 渲染流。
    /// 返回（设备名, 流, 错误信号接收端）；流由 engine 持有保活。
    fn build_stream(
        queue: &MicQueue,
    ) -> Result<(String, cpal::Stream, std::sync::mpsc::Receiver<()>)> {
        // 返回值是 Result<元组>：三个东西要一起交出去（名字给日志、stream 给上层养着、
        //   err_rx 给上层当"掉线通知"）。元组类型 (A, B, C) 是"固定个数的异构打包"，
        //   比定义一个小 struct 更省事，因为这三样只在这一处成对出现。
        // cpal::default_host()：本平台的音频系统入口（macOS 上就是 CoreAudio）。
        let host = cpal::default_host();
        let needle = render_needle();
        // ── 设备枚举（对应 Windows 那边的 EnumAudioEndpoints + Item(i)）────────
        // host.output_devices() 返回一个【迭代器】（cpal 里的 Devices，相当于
        //   Windows 的 IMMDeviceCollection + DeviceEnum 的角色）：按需逐个产出 cpal::Device。
        // .find(闭包) = "取第一个满足条件的元素"，返回 Option<Device>（找不到就是 None）。
        //   条件闭包里 d.name() 是 Result<String>，unwrap_or_default() 把"读不到名字"
        //   当成空串（空串永远不含 needle，于是自然被跳过）；to_ascii_lowercase 做大小写不敏感匹配。
        // .ok_or_else(|| anyhow!(...))? = 把 Option 翻译成 Result 的两步惯用法：
        //   None → 用闭包现造一个错误（有真实值时不会执行闭包，省一次字符串构造），
        //   然后 ? 立刻把错误上抛给 engine() 的 Err 分支 → 打日志 + 3 秒后重试。
        let device = host
            .output_devices()?
            .find(|d| {
                d.name()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .contains(&needle)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Virtual mic render device '{}' not found — install BlackHole first \
                     (见 README Mac 章节)",
                    needle
                )
            })?;
        // 名字单独再读一次：只给日志/GUI 显示用，读不到就退回 "BlackHole"（unwrap_or_else 同理）。
        let name = device.name().unwrap_or_else(|_| "BlackHole".to_string());
        // default_output_config()：这台设备当前推荐的输出格式（SampleFormat + 采样率 + 声道数）。
        //   与 Windows 那边 GetMixFormat() 是同一思想：共享模式下【跟着系统选】，不自己定格式。
        let cfg = device.default_output_config()?;
        // cfg.sample_rate() 是新类型包装 SampleRate(u32)，.0 取出里面的 u32
        //   （Rust 常用这种"包一层"来防止不同整数含义被混用；元组结构体的字段用下标 .0 访问）。
        let dev_rate = cfg.sample_rate().0;
        let dev_ch: usize = cfg.channels() as usize;
        let format = cfg.sample_format();
        // StreamConfig::from(cfg)：把"设备推荐格式"转成"建流参数"（所有权被 from 拿走，
        //   所以上面的 format/dev_rate/dev_ch 要提前抄出来 —— cfg 在这行之后就不能再用了）。
        let stream_cfg = cpal::StreamConfig::from(cfg);
        // 错误信号通道：cpal 的错误回调在【它自己的音频线程】里执行，不能直接调我们上层的
        //   异步逻辑，所以只在回调里 send 一个 ()，engine 那边 recv 到就重建 —— 这是
        //   "回调 → 外部线程"最省事的传话方式（对比 Windows 版是靠函数返回来通知）。
        let (err_tx, err_rx) = channel::<()>();

        let stream = match format {
            // BlackHole 默认以 float32 报告 —— 主路径
            // ── build_output_stream 与"音频回调绝不能阻塞" ──────────────────
            // 签名：device.build_output_stream(&配置, 数据回调, 错误回调, 停机回调)
            //   → Result<Stream>。数据回调就是 Windows 那边 pump 的"cpal 版"：
            //   cpal 的音频线程每隔几毫秒把一段 &mut [样本] 递给你，要求【在这段函数里】填好，
            //   填完才返回；它跑在实时优先级线程上，没有第二次机会。
            // 因此回调里禁止：网络 IO、磁盘 IO、sleep、log 落盘、大内存分配、长时间持锁 ——
            //   只要返回晚了，CoreAudio 就拿着一段旧数据/静音播出去，听感是"咔哒/断续"，
            //   而且会连累系统里【其他所有】音频应用（共享同一个音频服务器）。
            //   本回调只做三件极快的事：读队列（一次短锁 + memcpy 级别）、缩放、填充。
            //   ⚠ 注意（既有权衡，未改动）：fill_frames 内部会 lock 上行队列，如果同一时刻
            //     网络线程正持锁，这里会短暂阻塞。目前临界区只有几行 push/drain，实测安全；
            //     若将来队列锁变重（例如在锁里做重采样/写文件），必须改成无锁环形缓冲。
            // 第二个参数写成 `_|`（下划线）= "这个参数我不关心"：cpal 传的 OutputCallbackInfo
            //   里含时间戳/ xruns 统计，本引擎用不上，忽略比接一个变量更省事（也不会有警告）。
            // move 闭包：把下面这些变量（q/et/carry/rpos/dev_ch/dev_rate）的所有权搬进闭包。
            //   carry/rpos 声明在闭包【外面】再 move 进去，正是"跨回调保存状态"的做法 ——
            //   闭包是 fnmut，内部变量每次调用都会重置，只有挪到捕获层才能长存。
            cpal::SampleFormat::F32 => {
                let q = queue.clone();
                let et = err_tx.clone();
                // 跨回调存活的重采样进位状态（与 windows_impl::pump 一致）
                let mut carry: VecDeque<i16> = VecDeque::new();
                let mut rpos: f64 = 0.0;
                device.build_output_stream(
                    &stream_cfg,
                    move |data: &mut [f32], _| {
                        // data 是【扁平】的"样本×声道"一维数组（帧交织），所以帧数 = 长度 ÷ 声道数。
                        // max(1) 防的是 dev_ch 万一为 0（畸形设备报告）时的除零 panic —— 宁可得 0 帧也不崩。
                        let frames = data.len() / dev_ch.max(1);
                        // ⚠ 每次回调都新建一个 Vec（fill_frames 内部）。回调里堆分配本该避免，
                        //   但 cpal 的帧数通常 480（10ms）很小、系统分配器也够快，实测没出问题；
                        //   真要精调就换成复用的固定缓冲区（这次只标注，不改代码）。
                        let mono = fill_frames(&q, frames, dev_rate, &mut carry, &mut rpos);
                        // chunks_mut(dev_ch)：把扁平数组切成"每帧一组声道"；zip 与单声道样本配对；
                        //   内层 iter_mut 把同一个样本写进该帧的所有声道（上混），除以 32768 归一化。
                        for (frame, &s) in data.chunks_mut(dev_ch).zip(mono.iter()) {
                            let v = s as f32 / 32768.0;
                            for c in frame.iter_mut() {
                                *c = v;
                            }
                        }
                    },
                    // 错误回调：cpal 在设备消失/流被系统终止时调它。里面只 warn + send(())。
                    move |e| {
                        warn!("[MicOut] cpal render error: {}", e);
                        let _ = et.send(());
                    },
                    None,
                )?
            }
            // 少数设备以 s16 报告默认格式 —— 备路径
            // 与 F32 分支只差"样本类型"：结构与规则完全一致，所以解释同上不再重复。
            // 为什么要写两遍？因为 build_output_stream 的回调类型是【按样本类型单态化】的泛型
            //   （closure 收 &mut [f32] 还是 &mut [i16] 是两种不同函数类型），
            //   Rust 没有"运行时按类型分支填数组"的写法，只能一个格式一条路径 ——
            //   这也是 cpal 官方示例的统一形态（match sample_format! 宏）。
            cpal::SampleFormat::I16 => {
                let q = queue.clone();
                let et = err_tx.clone();
                let mut carry: VecDeque<i16> = VecDeque::new();
                let mut rpos: f64 = 0.0;
                device.build_output_stream(
                    &stream_cfg,
                    move |data: &mut [i16], _| {
                        let frames = data.len() / dev_ch.max(1);
                        let mono = fill_frames(&q, frames, dev_rate, &mut carry, &mut rpos);
                        // i16 路径不做归一化：样本本身就是整数满幅值，直接复制给各声道。
                        for (frame, &s) in data.chunks_mut(dev_ch).zip(mono.iter()) {
                            for c in frame.iter_mut() {
                                *c = s;
                            }
                        }
                    },
                    move |e| {
                        warn!("[MicOut] cpal render error: {}", e);
                        let _ = et.send(());
                    },
                    None,
                )?
            }
            // match 的兜底分支：既不是 f32 也不是 i16（比如 f64/u8）就直接放弃这台设备。
            //   other 绑到剩余 pattern 本身，{:?} 是"调试格式"打印（Display {} 打印不了枚举结构），
            //   错误消息里带出实际格式，遇到没见过的设备时一眼能看懂该加哪个分支。
            other => anyhow::bail!("Unsupported render sample format: {:?}", other),
        };
        drop(err_tx); // 原件释放；回调里各持一份克隆，err_rx 仍连着
        // 为什么要主动 drop 发送端原件：只要还剩一个 sender 活着，recv() 就永远阻塞。
        //   此刻两份 clone 已经在两个错误回调里，原件没有用处；丢掉它，语义就变成
        //   "只有回调报错（流被销毁、sender 随之释放）时 err_rx 才会收到信号"。
        //   err_rx.recv() 的另一条解锁路径：所有 sender 都 drop → 通道关闭 → recv 返回 None。
        Ok((name, stream, err_rx))
    }

    /// 产出 frames 个单声道 i16 样本：上行速率=设备速率直取；否则线性插值重采样。
    /// 算法与 windows_impl::pump 内联版逐行对应（v3.4.4 carry/rpos 方案）。
    // 这里把它写成独立函数而不是复制粘贴：cpal 的回调必须由我们掌控长度，抽出来更好读；
    //   carry / rpos 用 &mut 传引用 = "函数可以改它们，但所有权仍归调用方（闭包）"，
    //   这正是跨回调保持进位状态的唯一干净写法。
    // 完整算式说明在 windows_impl::pump 里（step = 上行速率 ÷ 设备速率，
    //   out[i] = round(carry[i0] × (1 − f) + carry[i0+1] × f)，f 为轴位置的小数部分），
    //   两处逻辑必须严格一致，否则两个平台的音调表现会不一样。
    fn fill_frames(
        queue: &MicQueue,
        frames: usize,
        dev_rate: u32,
        carry: &mut VecDeque<i16>,
        rpos: &mut f64,
    ) -> Vec<i16> {
        // frames == 0 提前返回：省掉后面所有浮点运算（也避免 (frames−1) 作为 usize 下溢）。
        let mut mono: Vec<i16> = Vec::with_capacity(frames);
        if frames == 0 {
            return mono;
        }
        let urate = UPLINK_RATE.load(std::sync::atomic::Ordering::Relaxed);
        let mut q = queue.lock().unwrap();
        // urate < 8000 也走"直取"：脏值/未初始化时宁可不重采样（同 Windows 版的保险条件）。
        // ── 直取分支：不需要重采样时最省事的写法 ────────────────────────────
        // urate == dev_rate → 手机推的速率就是设备速率，一帧对应一帧，直接搬。
        // urate < 8000 → 速率还没被首包建立（0 或脏值），此时做除法会算出垃圾步长，
        //   所以宁可"按需要多少个就取多少个、取不到补 0"，也就是先出声再谈音质。
        // unwrap_or(0) 是这里的关键安全垫：队空时给"静音"而不是 panic —— 音频回调里 panic
        //   会让整条流被销毁（见上面 data callback 的说明）。
        if urate == dev_rate || urate < 8000 {
            for _ in 0..frames {
                mono.push(q.pop_front().unwrap_or(0));
            }
            return mono;
        }
        // ── 线性插值重采样（44100 → 48000 的全部数学都在这里）───────────────
        // step = 源速率 ÷ 目的速率 = 44100/48000 = 0.91875：
        //   含义是"输出每走 1 帧，要在输入这条时间轴上前进 0.91875 帧"。
        //   小于 1 → 输出比输入"密"（48000 > 44100），所以会有相邻输出帧落到同一对输入样本之间。
        // need = 这一轮要从队列新取多少样本：
        //   最后一个输出帧的源位置 = (frames−1)×step + rpos，向上取整（ceil）再加 1 个
        //   作为插值右端点，就是"至少要有这么多源样本才够算完整这一轮"。
        // while 补齐：不够就继续从队列 pop_front（拿不到补 0），保证 carry 长度 ≥ need。
        // 循环体三行就是线性插值公式（对第 i 个输出帧）：
        //   p  = rpos + i × step            源时间轴上的浮点位置
        //   i0 = ⌊p⌋（`as usize` 直接截断小数，非负数时等价于向下取整）
        //   f  = p − i0 ∈ [0,1)             落在两个源样本之间的比例
        //   out = round(a×(1−f) + b×f)      两端点加权平均 —— f=0 取 a，f=1 取 b
        // 例：44100→48000 时第 1 个输出帧 p=0.91875 → i0=0, f=0.91875 → 输出约 8% a + 92% b。
        // a/b 用 get().unwrap_or：越过 carry 末尾时退化为 0（或重复 a），依然不出 panic。
        // ⚠ 注意：`p as usize` 是【向零截断】，只对非负 p 成立 —— rpos 全程 ≥0 所以安全，
        //   如果哪天改成允许负偏移，这里会取错样本（表现为周期性抖一下，很难查）。
        let step = urate as f64 / dev_rate as f64;
        let need = ((frames as f64 - 1.0) * step + *rpos).ceil() as usize + 1;
        while carry.len() < need {
            carry.push_back(q.pop_front().unwrap_or(0));
        }
        for i in 0..frames {
            let p = *rpos + i as f64 * step;
            let i0 = p as usize;
            let f = p - i0 as f64;
            let a = *carry.get(i0).unwrap_or(&0) as f64;
            // b 的兜底写成"取不到 i0+1 就退回 a"（即 f 再大也只会输出 a），
            //   这样尾巴上少一个样本时输出会略微变平，但绝不会越界或静音。
            let b = *carry
                .get(i0 + 1)
                .unwrap_or_else(|| carry.get(i0).unwrap_or(&0)) as f64;
            // .round() 之后再 as i16：浮点转整型 Rust 是【向零截断】，先 round 才符合听感预期。
            //   ⚠ 注意：这里没做饱和钳制（clamp），插值结果理论上不会超过 max(|a|,|b|)，
            //   所以不会溢出；但若上游塞过 32768 这类非法值，截断行为会变（保持原样，仅标注）。
            mono.push((a * (1.0 - f) + b * f).round() as i16);
        }
        // ── 跨轮状态收尾（这部分错了就会"越听越漂"）─────────────────────────
        // last_pos = 本轮最后一个输出帧落在源轴上的位置。
        let last_pos = *rpos + (frames as f64 - 1.0) * step;
        // consumed = 这些源样本已经用尽，可以从 carry 丢掉的数量（向下取整：
        //   ⚠ 必须是 floor —— 用 ceil 会把"下一个输出帧还要用的右端点"提前丢掉，插值就断档）。
        let consumed = last_pos.floor() as usize;
        // .min(carry.len())：队列空时用 0 补齐过 carry，长度可能刚好等于 need，
        //   取 min 保证 pop_front 次数不会超过实际元素数（多弹会 panic）。
        for _ in 0..consumed.min(carry.len()) {
            carry.pop_front();
        }
        // rpos 保留小数部分：这一轮"没用完的那半格"要带到下一轮，否则相位会重置 → 周期性咔哒声。
        *rpos = last_pos - consumed as f64;
        // 保险阀：carry 只留最近几个样本足够支撑下一次插值。
        //   ⚠ 注意（既有行为，未改动）：若某种异常导致 need 长期大于 4，这里会丢弃【真实样本】，
        //   表现为声音被吃掉一段；正常路径下 need 只取决于 frames/step，不会触发。
        while carry.len() > 4 {
            carry.pop_front();
        }
        // 函数体最后一行 mono（无分号）= 返回值：调用方拿到 Vec<i16> 后按设备声道展开。
        mono
    }

    /// macOS"有没有应用正在录注入设备"检测 → 驱动手机按需录音的 mic_state 信号。
    /// 原理：CoreAudio 设备属性 kAudioDevicePropertyDeviceIsRunningSomewhere
    /// （FourCC 'isrn'）在【输入域】非零 = 有客户端真正跑着采集 IO。
    /// 我们的注入流在 BlackHole 2ch 的【输出侧】、下行捕获故意用 16ch 另一台设备，
    /// 所以不会自己把自己误判成"有人在用"。
    /// 兜底：env PCSPEAKER_MAC_MIC_ALWAYS_ACTIVE=1 → 强制视为占用中。
    pub(crate) fn capture_monitor(tx: tokio::sync::mpsc::UnboundedSender<bool>) {
        // 兜底开关：环境变量【存在】就永远视为"有人在用"（is_ok() 只判断有没有设，不看值）。
        //   用途：某些系统上 IsRunningSomewhere 属性表现异常导致手机永远不录音时，
        //   用它把链路强行打通（隐私底线这时由手机侧的静音键/强停承担）。
        //   ⚠ 注意：环境变量只在进程启动时读一次（写在循环外），运行中改环境变量不会生效。
        let forced = std::env::var("PCSPEAKER_MAC_MIC_ALWAYS_ACTIVE").is_ok();
        let mut last = false;
        loop {
            // || 短路：forced 为真时【根本不会】调用 inject_input_running()，省掉一轮 COM/FFI 枚举。
            // unwrap_or(false)：读取属性失败就当"没人用"（同 Windows 版：失败一律倒向安全方向）。
            let active = forced || inject_input_running().unwrap_or(false);
            if active != last {
                last = active;
                info!(
                    "[MicMon] Virtual mic capture {}",
                    if active { "ACTIVE (an app is using the mic)" } else { "idle (nobody recording)" }
                );
                let _ = tx.send(active);
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    // ── 裸 CoreAudio FFI（与 cpal 内部同一套 coreaudio-sys 绑定）──────────
    // 属性选择器一律用 FourCC 字面量，规避"绑定里到底有没有这个常量"的不确定性
    // ── 这一段是给"unsafe 为什么不可避免"举个完整例子 ─────────────────────
    // coreaudio-sys 是 C 头文件的【一一硬翻译】产物（bindgen 生成）：函数是 extern "C"、
    //   参数是裸指针、返回值是 OSStatus（C 的错误码）。调用 extern "C" 函数本身在 Rust 里
    //   就被归为 unsafe —— 编译器无法证明 C 那边不会乱动内存，只能由人签字。
    // FourCC（四字符码）：Apple 把属性 ID 定义为"四个 ASCII 字符当一个大端 u32"。
    //   'd''e''v''#' = 0x6465_7623（每字节就是一个字符的编码，下划线只是可读分隔，不参与值）。
    //   为什么手写数字而不引用常量：不同版本绑定里常量名可能不存在/被挪走，数字是 ABI 事实，
    //   永远对得上。⚠ 但这意味着改错一位就是问错属性（不会编译报错），改时要对着 CoreAudio 头文件。
    // 魔数含义逐条：'dev#' 硬件设备列表；'pnmr' 设备名(CFString)；'isrn' 这台设备"别处是否在跑"；
    //   'glob' 全局作用域；'inpt' 输入(录音)侧作用域 —— 作用域选错会得到"属性不存在"的 OSStatus。
    const K_DEVICES: u32 = 0x6465_7623; // kAudioHardwarePropertyDevices            'dev#'
    const K_NAME_CFSTRING: u32 = 0x706E_6D72; // kAudioDevicePropertyDeviceNameCFString 'pnmr'
    const K_IS_RUNNING_SOMEWHERE: u32 = 0x6973_726E; //                          'isrn'
    const K_SCOPE_GLOBAL: u32 = 0x676C_6F62; // kAudioObjectPropertyScopeGlobal  'glob'
    const K_SCOPE_INPUT: u32 = 0x696E_7074; // kAudioObjectPropertyScopeInput     'inpt'

    // AudioObjectPropertyAddress 就是 C 结构体"我要问哪个属性"，三个字段全填对才问得到。
    // mElement: 0 = kAudioObjectPropertyElementMaster（"整体"这一列，不按声道细分）。
    // 抽成小函数的价值：下面三处查询都用同一套 scope/element 规则，写错一处不如写错三次。
    fn prop_addr(selector: u32, scope: u32) -> coreaudio_sys::AudioObjectPropertyAddress {
        coreaudio_sys::AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: 0, // kAudioObjectPropertyElementMaster
        }
    }

    /// 全部音频设备 ID
    // 结构是 CoreAudio 查询属性的标准"两步 dance"：先 GetPropertyDataSize 问要多少字节，
    //   再按大小开好 Vec 把数据读进来。为什么不能一步到位：系统不知道你想装多少个元素。
    // vec![0 as AudioDeviceID; n]：先填零占好空间（size 已确定），再把可写指针交给 C 填。
    //   把未初始化内存交给 C 写是允许的（后面立刻整体读出），但必须保证长度足够 —— 这也是
    //   ids.as_mut_ptr() 必须在 unsafe 里调的原因：越界一寸就是踩内存。
    fn device_ids() -> Result<Vec<coreaudio_sys::AudioDeviceID>> {
        unsafe {
            let a = prop_addr(K_DEVICES, K_SCOPE_GLOBAL);
            let mut size: u32 = 0;
            let st = coreaudio_sys::AudioObjectGetPropertyDataSize(
                0, // kAudioObjectSystemObject
                &a,
                0,
                std::ptr::null(),
                &mut size,
            );
            if st != 0 {
                anyhow::bail!("enumerate audio devices failed: OSStatus {}", st);
            }
            let n = size as usize / std::mem::size_of::<coreaudio_sys::AudioDeviceID>();
            let mut ids = vec![0 as coreaudio_sys::AudioDeviceID; n];
            let st = coreaudio_sys::AudioObjectGetPropertyData(
                0,
                &a,
                0,
                std::ptr::null(),
                &mut size,
                ids.as_mut_ptr() as *mut std::ffi::c_void,
            );
            if st != 0 {
                anyhow::bail!("read device list failed: OSStatus {}", st);
            }
            Ok(ids)
        }
    }

    /// 设备名（CFString → Rust String；手法照 cpal macOS 后端，但补了 CFRelease）
    // 返回 Option<String> 而不是 Result：这台设备读不到名字是【常态】（很多对象不是设备、
    //   没名字属性），不是异常；用 Option 表达"可能就是没有"，调用方自己决定跳过还是报错。
    // 这里能看出 Rust 管理 C 资源的纪律：CFString 是 CoreFoundation 的引用计数对象，
    //   AudioObjectGetPropertyData 按 GetRule 给了你一个【持有引用】，必须 CFRelease 归还，
    //   否则每 250ms 漏一个字符串 —— 本函数每条 return 路径都记得先归还。
    // ⚠ 注意（未改动）：CFRelease 在快路径之外的两条错误分支上做了，但如果将来在两个
    //   分支之间新增提前 return，很容易漏掉一次释放。要加固就在拿到 s 之后立刻用一个
    //   Drop 类型接住（RAII guard），让"归还"这件事也交给编译器。
    fn device_name(id: coreaudio_sys::AudioDeviceID) -> Option<String> {
        use core_foundation_sys::base::{CFRelease, CFTypeRef, kCFStringEncodingUTF8};
        use core_foundation_sys::string::{
            CFStringGetCString, CFStringGetCStringPtr, CFStringGetLength,
            CFStringGetMaximumSizeForEncoding, CFStringRef,
        };
        unsafe {
            let a = prop_addr(K_NAME_CFSTRING, K_SCOPE_GLOBAL);
            let mut s: CFStringRef = std::ptr::null();
            let mut size = std::mem::size_of::<CFStringRef>() as u32;
            let st = coreaudio_sys::AudioObjectGetPropertyData(
                id,
                &a,
                0,
                std::ptr::null(),
                &mut size,
                &mut s as *mut _ as *mut std::ffi::c_void,
            );
            // OSStatus 的约定是【0 = noErr 成功】，非 0 全是失败码；指针为空也算失败。
            //   看到 `if st != 0 { ... }` 就等价于"这次系统调用没成功"。
            if st != 0 || s.is_null() {
                return None;
            }
            let bytes: Vec<u8> = {
                // 快路径：系统缓存的 UTF-8 指针；慢路径：自己开缓冲区拷
                // 这里演示了 Rust 的一个好习惯：{ } 代码块本身是【表达式】，
                //   块最后一行（不写分号）就是它的值 → 可以直接赋给 bytes。
                //   于是"两条路径产出同一种结果"不用先声明 mut bytes 再各自赋值，
                //   也就不会出现"某条分支忘了赋值"（编译器会检查）。
                let p = CFStringGetCStringPtr(s, kCFStringEncodingUTF8);
                if !p.is_null() {
                    let len = (0..).take_while(|&i| *p.offset(i) != 0).count();
                    std::slice::from_raw_parts(p as *const u8, len).to_vec()
                } else {
                    let n = CFStringGetLength(s);
                    let cap = (CFStringGetMaximumSizeForEncoding(
                        n as usize,
                        kCFStringEncodingUTF8,
                    ) + 1) as usize;
                    let mut buf = vec![0u8; cap];
                    let ok = CFStringGetCString(
                        s,
                        buf.as_mut_ptr() as *mut std::os::raw::c_char,
                        cap as _,
                        kCFStringEncodingUTF8,
                    );
                    // CFStringGetCString 返回 C 的 Boolean（i8）：0 = false 转换失败。
                    //   失败也不能直接 return —— 手里那个 CFString 引用必须先 CFRelease 再走。
                    if ok == 0 {
                        CFRelease(s as CFTypeRef);
                        return None;
                    }
                    // position 找第一个 NUL 字节；万一没找到就退化成"整个缓冲区都是内容"（防越界）。
                    //   闭包写 |&b| 是把迭代器给的 &u8 直接解构成 u8，比较起来更直观。
                    // truncate(len)：把 NUL 及之后的未使用部分砍掉，Vec 只剩真正的名字字节。
                    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                    buf.truncate(len);
                    // 块尾这一行不写分号 = 整个 { } 表达式的值（借用规则：Vec 被移动进 bytes）。
                    buf
                }
            };
            // 成功路径的归还：CFString 是引用计数对象，用完必须 CFRelease，否则每 250ms 漏一个。
            CFRelease(s as CFTypeRef);
            // from_utf8_lossy：非法 UTF-8 字节替换成 U+FFFD，绝不为"一个设备名"报错中断检测；
            //   它返回 Cow<str>（可能借用可能新建），.into_owned() 统一变成拥有所有权的 String。
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
    }

    /// 读一个 u32 型设备属性
    // 通用小工具：把"问一个 4 字节属性"这件事收在一处。
    //   size_of::<u32>() 是编译期算出的字节数（写成常量 4 也一样，但这样跟着类型走不会错）。
    //   返回 Option：OSStatus != 0（属性不存在/不可读）就返回 None，让上层用 unwrap_or(0) 消化。
    //   &mut val as *mut _ as *mut c_void：Rust 里"把一个 u32 变量的地址交给 C 的 void**"的固定套路，
    //   两次指针类型转换都在 unsafe 下做，尺寸必须与 C 期望的一致（这里由 size 参数声明）。
    fn u32_prop(id: coreaudio_sys::AudioDeviceID, selector: u32, scope: u32) -> Option<u32> {
        unsafe {
            let a = prop_addr(selector, scope);
            let mut val: u32 = 0;
            let mut size = std::mem::size_of::<u32>() as u32;
            let st = coreaudio_sys::AudioObjectGetPropertyData(
                id,
                &a,
                0,
                std::ptr::null(),
                &mut size,
                &mut val as *mut _ as *mut std::ffi::c_void,
            );
            if st == 0 {
                Some(val)
            } else {
                None
            }
        }
    }

    /// 注入设备的"录音侧"此刻是否有客户端 IO 在跑
    fn inject_input_running() -> Result<bool> {
        let needle = render_needle();
        for id in device_ids()? {
            // let-else：把 Option 拆包，拆不出来就执行 else 里的发散分支（continue）。
            //   等价于 match device_name(id) { Some(n) => ..., None => continue }，
            //   但少一层缩进、主流程保持平铺 —— Rust 处理 Option 时非常常用的现代写法。
            let Some(name) = device_name(id) else { continue };
            if !name.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            // 找到目标设备：它输入域的运转状态就是答案
            // 属性值语义：非 0 = 至少有一个客户端在这台设备的【输入作用域】真正跑 IO。
            // unwrap_or(0)：读不到就当作"没人在录"，方向和 Windows 版一致（失败倒向隐私安全侧）。
            let running = u32_prop(id, K_IS_RUNNING_SOMEWHERE, K_SCOPE_INPUT).unwrap_or(0);
            return Ok(running != 0);
        }
        // 循环走完都没匹配到名字 → 抛 Err（上层 capture_monitor 用 unwrap_or(false) 消化成"没人用"）。
        anyhow::bail!("inject device '{}' not found for capture monitor", needle)
    }
}
