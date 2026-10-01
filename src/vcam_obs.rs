// ── 摄像头模式：OBS 虚拟摄像头注入引擎（v3.4 双通道·第二条路）──────
//
// 为什么需要第二条通道：
//   Unity Capture 是传统 DirectShow 滤镜，Edge/新版 Chrome 浏览器用
//   Media Foundation 框架采集摄像头，机制上永远不会枚举它。
//   OBS Virtual Camera 两种框架都认，是覆盖"浏览器 + 桌面会议软件"
//   全部场景的免费方案。两条通道同时注入，应用端选哪个设备都能用。
//
// 协议来源：obs-studio/plugins/win-dshow/shared-memory-queue.c（OBS 32.x）。
// 我们扮演"生产者"（相当于 OBS 自己点下"启动虚拟摄像机"）：
//   - 创建命名共享内存 "OBSVirtualCamVideo"
//     （若 OBS 本体正在跑虚拟摄像机，创建会因占用失败 → 稍后重试）
//   - 头部 queue_header 共 80 字节（32 对齐后 96）：
//       [0]write_idx [4]read_idx [8]state
//       [12/16/20]offsets[3]（三个槽各自的字节偏移）
//       [24]type=0(视频) [28]cx [32]cy [40]interval(每帧100ns数,u64)
//   - state 枚举：0=INVALID 1=STARTING 2=READY 3=STOPPING
//   - 三槽环形队列，每槽 = 32 字节小头（前 8 字节是时间戳 u64）+ NV12 像素
//   - 写帧语义（与 video_queue_write 逐行对应）：
//       inc = ++write_idx; idx = inc % 3;
//       写时间戳、Y 平面(cx*cy)、交错 UV 平面(cx*cy/2)；
//       read_idx = inc; state = READY
//   - 像素格式 NV12（BT.601 limited range），所以我们把 JPEG 解出来的
//     RGB24 现场转成 NV12 再写入
//
// "有没有应用正在看摄像头"的检测：OBS 协议本身没有 Unity 那样的 Want 事件
//   （read_idx 是生产者可自己写的，消费者不回写任何状态），
//   改用【命名 Section 对象的内核句柄计数】：
//   OBS Virtual Camera 是进程内 DirectShow 滤镜——应用打开摄像头时，
//   virtualcam-module.dll 被加载进那个应用自己的进程，在其中执行
//   OpenFileMappingW("OBSVirtualCamVideo")，给共享内存对象增加一个句柄。
//     句柄数 == 1 → 只有我们（生产者）持有 → 没有应用在观看
//     句柄数 >= 2 → 至少一个应用进程打开了映射 → 有应用正在用摄像头
//   之前用隐私注册表 CapabilityAccessManager 检测是无效的：它只跟踪
//   UWP / Media Foundation 框架的摄像头访问，对 DirectShow 滤镜完全不记录。
//
// ── 本文件说明书（初学者请先读完下面这些行再往下翻）────────────────────
// 我是谁：OBS Virtual Camera 通道的"生产者"引擎 —— 一条常驻后台线程，把手机推来的
//   JPEG 解码成 RGB、转成 NV12，写进命名共享内存 "OBSVirtualCamVideo" 的三槽环形队列。
// 上游（数据从哪来）：src/server.rs 的 WebSocket 读循环把手机相机帧（魔术头
//   [0x03,'C','A','M']）复制两份，另一份调本文件 push_frame() 塞进 cam_mailbox_obs。
//   邮箱类型与 Unity 通道共用：FrameMailbox = Arc<Mutex<Option<Vec<u8>>>> ——
//   Arc=多个线程共享同一份数据（引用计数），Mutex=同一时刻只许一方碰，
//   Option=可能还没有帧，Vec<u8>=一整帧 JPEG 的原始字节。
//   泛型尖括号读法：Vec<u8> 读作"装 u8 元素的可增长数组"；UnboundedSender<bool> 读作
//   "只会发 bool 值的通道发送端"。类型写在 <> 里，编译期就定死，运行不会变。
// 下游（数据往哪去）：virtualcam-module.dll —— 它被"正在看摄像头的软件"（OBS/微信/
//   浏览器）加载进【那个软件自己的进程】去读这块内存；我们与那个软件零通信，
//   全靠同一块内存 + 同一套字节布局约定说话。本文件不直接调 COM（DirectShow 滤镜
//   那侧才用 COM）；我们这里的 unsafe 只涉及裸指针读写与 Win32 API。
// 谁启动我：server.rs 启动流程调 spawn_vcam_obs()；tx 发出的 bool（有没有应用在看）
//   由 server.rs 打包成 cam_state 推给手机，手机端搭档是
//   D:\code\PCAssistant\lib\providers\camera_provider.dart（据此开/关相机硬件）。
// 兄弟对比：src/vcam.rs(Unity)=单块共享内存+Want/Sent 事件门铃、驱动是创建者我们是
//   加入者；本文件=三槽环形队列（OBS 协议没有催更事件）、我们反而是创建者，
//   活跃检测改用"命名对象的内核句柄计数"（见文件下半部"消费者检测"块注释）。
// 单独验证：① audioserver.log 搜 [VcamObs]（log:: 宏经 main.rs 的 DualLogger 同时
//   写 stderr 和该文件）；② 句柄计数可单跑探针 src/bin/queue_handle_probe.rs；
//   ③ 画面侧：任何应用选 "OBS Virtual Camera" 应见黑帧，手机推流后应见实时画面。
// 改动红线：头部字段偏移 0/4/8/12..40、槽大小、32 字节对齐，全是与 OBS 滤镜源码
//   逐字节对齐的协议数字 —— 改一个就花屏或黑屏（各处 ⚠ 与常量注释已标明后果）。

use log::info;

/// 手机推来一帧 JPEG（邮箱类型/语义与 Unity 通道完全一致，直接复用）
// 这里没有"另一套邮箱"：两路摄像头共用同一个 crate::vcam::FrameMailbox 类型，
//   server.rs 收到一帧后两边各塞一份（各 clone 一次字节）。函数本体只是转发，
//   真正的"覆盖旧帧、只留最新一帧"逻辑在 vcam.rs 的 push_frame（含逐步讲解）。
// 参数写法快速复习：mailbox: &FrameMailbox = 只【借用】不拥有（调用方继续用）；
//   jpeg: Vec<u8] = 按【值】传入 = 把这块字节的所有权交进来，内部 move 进邮箱，零拷贝。
pub fn push_frame(mailbox: &crate::vcam::FrameMailbox, jpeg: Vec<u8>) {
    crate::vcam::push_frame(mailbox, jpeg);
}

/// 启动 OBS 通道注入引擎线程（仅 Windows；其他平台是空 stub）。
/// 帧邮箱与 Unity 通道共用同一类型：手机 WS 读循环写入 JPEG，本引擎取走转码写入。
// #[cfg(windows)] / #[cfg(not(windows))]：条件编译属性，决定这段代码参不参与编译。
//   OBS Virtual Camera 是 Windows 生态的产物（CreateFileMappingW 等 API 只存在于
//   Windows），Mac/Linux 上给出一个空壳函数，让 server.rs 可以无条件调用，
//   平台差异不会外溢到其他文件。
// std::thread::spawn(move || ...)：开一条【独立操作系统线程】跑 engine 死循环。
//   move 关键字把 mailbox（Arc 的一份克隆）与 tx 的所有权搬进新线程。
//   为什么不用 tokio 任务：engine 里有大量 sleep 和阻塞式全系统扫描，
//   放进异步运行时会把一个工作线程占死（与 vcam.rs 同一个道理）。
// tx: tokio::sync::mpsc::UnboundedSender<bool> —— "无界多通道发送端"：
//   本线程 send(active)，server.rs 的异步任务 recv()，把状态翻转广播给手机。
#[cfg(windows)]
pub fn spawn_vcam_obs(
    mailbox: crate::vcam::FrameMailbox,
    tx: tokio::sync::mpsc::UnboundedSender<bool>,
) {
    std::thread::spawn(move || windows_impl::engine(mailbox, tx));
}

/// 非 Windows stub（OBS Virtual Camera 是 Windows 专属驱动）
// 参数名前缀下划线（_mailbox）= "接口要求有这个参数但我用不到"，
//   以此关闭编译器"未使用变量"警告 —— 这是 Rust 社区惯例，不是语法要求。
#[cfg(not(windows))]
pub fn spawn_vcam_obs(
    _mailbox: crate::vcam::FrameMailbox,
    _tx: tokio::sync::mpsc::UnboundedSender<bool>,
) {
}

// ── Windows 实现整体包进一个 mod（模块）──────────────────────────────
// mod = Rust 的命名空间/文件组织单元。这里把 unsafe 裸指针和 Win32 依赖全部关进
//   windows_impl 这间"屋子"，只有屋子内部能直接访问；文件顶部的公共 API
//   （push_frame / spawn_vcam_obs）保持干净。
// use super::*：把"父模块"（本文件顶层）已经引入的名字（如 info、FrameMailbox）
//   全部再导入一次，所以下面能直接写 info! 而不用 crate::log 全路径。
#[cfg(windows)]
mod windows_impl {
    use super::*;
    use log::warn;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
        MEMORY_MAPPED_VIEW_ADDRESS, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
    };

    /// 与 OBS 源码一致的段名（UTF-16 命名共享内存）
    // ── "命名共享内存"为什么能跨进程 ──────────────────────────────────
    // Windows 内核维护一棵"对象命名空间"树（\BaseNamedObjects\...）。任何进程用
    //   【同一个名字】Create/Open 同一个内核对象，拿到的就是【同一份】内存 ——
    //   这就是我们和"看摄像头的软件"之间没有 socket、不引用彼此代码却能协作的全部原理。
    // ⚠ 这个名字一个字都不能改、也不能拼错：OBS 滤镜 DLL 里硬编码了同一个名字。
    //   改了 = 应用打开 "OBS Virtual Camera" 时找不到我们的内存 = 永远黑屏且不报错。
    // 名字【不带 Global\ 前缀】= 会话级对象：对当前登录桌面里的所有进程可见，够用
    //   也不需要 SeCreateGlobalPrivilege 特权（与 vcam.rs 的 Unity 名字同理）。
    const VIDEO_NAME: &str = "OBSVirtualCamVideo";

    /// queue_header 的 sizeof（3×u32 + offsets[3] + type + cx + cy + pad + u64 + reserved[8]）
    // 80 是"C 侧结构体有效字节数"；真正放槽之前还要把它【向上对齐到 32 的倍数】= 96
    //   （见 align32 与 layout）。⚠ 改这个数 = 所有槽起点全体位移 = 滤镜按旧布局读 = 花屏。
    const HEADER_SIZE: usize = 80;
    /// 每个帧槽前面的小头部字节数（前 8 字节放时间戳）
    // 槽头 32 字节里目前只用前 8 字节（u64 时间戳），其余留白（OBS 协议为未来字段预留）。
    //   写像素前要先跳过这 32 字节 —— 忘了跳 = 把时间戳当第一行像素覆盖 = 顶部彩条。
    const FRAME_HEADER_SIZE: usize = 32;

    // state 枚举值（对应 enum queue_state）
    // 这三个数写进头部字节 [8..12)，是"生产者当前状态"的广播，滤镜每帧都读它：
    //   STARTING=我要开始供货了 / READY=有一帧全新的可读 / STOPPING=我走了别等了。
    //   0=INVALID 不用常量：整块内存清零后 [8] 天然是 0，语义就是"还没有生产者"。
    //   ⚠ 数值 1/2/3 是协议规定的顺序，改成别的值滤镜会认成非法状态而黑屏。
    const STATE_STARTING: u32 = 1;
    const STATE_READY: u32 = 2;
    const STATE_STOPPING: u32 = 3;

    /// 把 Rust 字符串转成 Windows 宽字符 C 串（末尾补 \0）
    // Rust 的 &str 是 UTF-8（只带长度、不保证结尾有 0）；Win32 的 W 系 API 要的是
    //   "UTF-16 单元数组 + 结尾一个 0"。encode_utf16 把字符逐个转成 u16，
    //   chain(once(0)) 在尾部接一个 0，collect 收进 Vec<u16>。
    // ⚠ 返回 Vec 而不是指针是刻意的：Vec 活着指针才有效。调用方必须先 let 一个
    //   局部变量接住它、再取 .as_ptr() 传给 API；写成"函数直接返回指针"就是悬空指针。
    fn wstr(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // ── 32 字节对齐：为什么到处都是 align32 ────────────────────────────
    // OBS 的 C 侧把头部和每个帧槽的起点都向上取整到 32 的倍数（缓存行友好的协议约定）。
    //   我们算的槽偏移必须和它一分不差，否则滤镜按它的布局读、按我们的布局写 → 错位花屏。
    // (v + 31) & !31 的位运算读法：!31 = 把低 5 位全变 0 的掩码（...11100000），
    //   v+31 保证进位到下一个 32 的倍数，再与掩码把多余的低位削掉 = "向上取整到 32"。
    // 例：80 → (80+31)=111 → 111 & !31 = 96。v 本来是 32 倍数时结果不变（不会多跳一格）。
    #[inline]
    fn align32(v: usize) -> usize {
        (v + 31) & !31
    }

    /// 复刻 video_queue_create 的布局计算：返回 (总大小, 三个槽的偏移)
    // ── 三槽环形队列：一块内存切成"头部 + 3 个帧槽"，它解决什么问题 ──────────
    // 场景：生产者（我们）和消费者（滤镜）在两个进程里，节奏互不相等。
    // 若只有一格缓冲：写的人 memcpy 到半截、读的人来抄 → 拿到"上半新、下半旧"的
    //   画面 = 所谓的【画面撕裂】。这就是"必须等消费者读完再覆盖"的原因 ——
    //   单格时生产者每次都得等，等待又反过来卡生产者帧率。
    // 三槽轮转把"等"变成"几乎不用等"：生产者永远写 (write_idx+1)%3 那一格，
    //   消费者按 read_idx%3 读最新格。3 = 正在写 1 格 + 写完等读 1 格 + 正在被读 1 格；
    //   生产者要连写 3 帧才可能绕回消费者正在抄的那一格 —— 30fps 下意味着消费者
    //   卡顿了 100ms 以上，实际几乎不发生；真发生了也只坏一帧、下一帧自愈。
    // 返回值 (usize, [usize; 3]) 是元组：调用方用 let (size, offsets) = ... 一次拆开。
    //   帧字节数 cx*cy*3/2 —— 这个 1.5 来自 NV12（见 rgb_to_nv12 的讲解）。
    //   每格 = 帧像素 + 32 字节槽头，起点各自 align32 —— 与 C 侧算法逐行对应。
    fn layout(cx: u32, cy: u32) -> (usize, [usize; 3]) {
        let frame_size = (cx as usize) * (cy as usize) * 3 / 2;
        let mut size = align32(HEADER_SIZE);
        let mut offsets = [0usize; 3];
        for i in 0..3 {
            offsets[i] = size;
            size = align32(size + frame_size + FRAME_HEADER_SIZE);
        }
        (size, offsets)
    }

    /// 引擎持有的映射对象：句柄 + 视图指针 + 当前分辨率
    // struct = "把相关数据捆在一起"的自定义类型；这里的每个字段都是引擎线程
    //   继续干活需要的状态。字段类型快速读法：
    //   h_map: HANDLE —— 内核给"那块共享内存对象"发的门牌（内部包一个指针，8 字节）；
    //   view: *mut u8 —— 裸指针，内存映射进本进程后的第一个字节地址（8 字节）。
    //     为什么不用 &[u8] 切片：这块内存不属于 Rust 所有权体系、长度只由协议规定，
    //     没法标注生命周期 —— 只能裸指针 + 手动 unsafe 访问（对照 vcam.rs 的 Sender.view）。
    //   cx/cy: u32 —— 当前映射的宽高（协议头 [28]/[32] 处那两个数的本地缓存）。
    //   offsets: [usize; 3] —— layout() 算出的三个槽字节偏移（也写进了共享内存头部）。
    // ⚠ 本结构体【只活在我们进程里】，不镜像到共享内存，所以不需要 #[repr(C)]；
    //   需要 #[repr(C)] 的是"要按 C 布局读外部内存"的结构（见下面 HandleEntryEx）。
    struct ObsQueue {
        h_map: HANDLE,
        view: *mut u8,
        cx: u32,
        cy: u32,
        offsets: [usize; 3],
    }

    // 只有引擎线程自己读写 ObsQueue，跨不过去也不需要锁
    // ── 这行 unsafe impl Send 是在干什么 ────────────────────────────────
    // Rust 默认禁止"含裸指针的类型"跨线程移动（*mut u8 没实现 Send trait），因为
    //   编译器无法验证裸指针的访问纪律。ObsQueue 事实上只被 spawn 出来的那一条
    //   引擎线程持有，安全由【人】担保：unsafe impl Send = "我签个字，它确实能过线程"。
    // trait = "具备某种能力"的接口约定；Send = "可以安全地 move 到别的线程"这个能力。
    //   （对照 mic_out.rs/vcam.rs：那边数据要在两线程间共享，用的是 Arc<Mutex<..>>，
    //   由锁保证安全，就不需要这种手写担保。）
    // ⚠ 担保义务：今后绝不能再把 ObsQueue 借给第二条线程并发读写，那才是真数据竞争。
    unsafe impl Send for ObsQueue {}

    impl Drop for ObsQueue {
        // Drop = Rust 的 RAII 清理钩子：值离开作用域被销毁时，编译器自动调这里的
        //   drop(&mut self)。好处是"关句柄/解除映射"不可能被忘记 —— 即便 engine() 里
        //   全是 return/提前退出路径，也会走到这一步（对照 C 里满地的 goto cleanup）。
        //   两条纪律：① 用 is_null()/is_invalid() 判断"可能还没拿到的资源"；
        //   ② 清理调用的返回值用 let _ = 明确丢弃 —— 都要退出了，报错也无从处理，
        //     但在 Drop 里 panic 是极坏的主意（可能二次 panic 中止进程）。
        // 动作顺序讲究：先写 STOPPING（滤镜读到就改显示"摄像头已停止"，不再等新帧），
        //   再 UnmapViewOfFile（还回本进程访问这块内存的地址），最后 CloseHandle
        //   （退回对象门牌）。后两者各管一半：Unmap 管"地址视图"，Close 管"句柄引用"，
        //   少做哪个都会泄漏（句柄泄漏是全进程范围的资源泄漏）。
        fn drop(&mut self) {
            unsafe {
                // 告诉消费者"生产者要停了"（与 video_queue_close 一致）
                std::ptr::write_unaligned(self.view.add(8) as *mut u32, STATE_STOPPING);
                if !self.view.is_null() {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.view as *mut core::ffi::c_void,
                    });
                }
                if !self.h_map.is_invalid() {
                    let _ = CloseHandle(self.h_map);
                }
            }
        }
    }

    /// 创建（或重建）OBS 虚拟摄像头共享内存。
    /// 返回 None = 系统拒绝，稍后由引擎重试。
    // ── 命名共享内存"三件套"在生产者一侧的完整用法 ────────────────────────
    // 返回 Option<ObsQueue>：Some=成功拿到，None=失败但不需要知道为什么（调用方
    //   只会"过会儿再试"，所以不必用 Result 携带错误详情 —— 选类型的原则见
    //   vcam.rs try_attach 的说明）。unsafe fn：整条函数体默认处于 unsafe 上下文，
    //   因为里面全是裸指针写入；调用方仍要包 unsafe { }（双保险，见 vcam.rs 说明）。
    // 命名互斥量：本文件【没有】用任何锁 —— OBS 协议不要求生产者持命名 Mutex，
    //   防撕裂全靠三槽轮转 + "写完像素才更新索引"的顺序（见 queue_write）。
    //   想看"跨进程共享内存 + 命名 Mutex"的写法，对比 vcam.rs 的 "UnityCapture_Mutx"。
    unsafe fn queue_create(cx: u32, cy: u32, fps: u32) -> Option<ObsQueue> {
        let name = wstr(VIDEO_NAME);
        // 先探测是否已存在
        // OpenFileMappingW(权限, 是否被子进程继承, 名字)：按名字找现成的对象。
        //   它返回 windows crate 包装过的 Result —— if let Ok(h) 是"只关心成功值"
        //   的写法（match 的 Ok 分支 + 什么都不做的 Err 分支，读法见 vcam.rs）。
        let existing = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(name.as_ptr()));
        if let Ok(h) = existing {
            // 映射已存在（可能是上次会话残留，消费者还持有）→ 接管它
            info!("[VcamObs] Mapping already exists, taking over (old session residue)");
            return queue_open_existing(h, cx, cy, fps);
        }

        let (size, offsets) = layout(cx, cy);
        // interval = 每帧占多少个"100 纳秒"：一秒有 10_000_000 个 100ns，除以 fps 即得。
        //   fps.max(1) 防除零；结果 .max(1) 防 fps 巨大时算出 0（协议里 0 无意义）。
        //   ⚠ 这是写进头部 [40] 的【协议字段】，滤镜按它决定送帧节奏，改错=节奏乱/黑屏。
        let interval = (10_000_000u64 / (fps.max(1) as u64)).max(1); // 每帧 100ns 数

        // CreateFileMappingW 六个参数依次：
        //   INVALID_HANDLE_VALUE = 不关联磁盘文件，直接从系统页文件分配（纯内存）；
        //   None = 默认安全属性；PAGE_READWRITE = 映射后可读可写；
        //   0, size as u32 = 大小的高 32 位/低 32 位（我们不超过 4GB 所以高 32 位恒 0）；
        //   PCWSTR = 名字。返回 Result<HANDLE>：match 拆开，Err 就 return None 让主循环稍后重试。
        // 若 OBS 本体正跑着虚拟摄像机：同名对象已存在 → 走下面的 ERROR_ALREADY_EXISTS
        //   接管分支（文件头说的"创建失败会重试"，实际路径就是这里）。
        let h_map = match CreateFileMappingW(
            INVALID_HANDLE_VALUE, // 纯页面文件（匿名分页，仅靠名字共享）
            None,
            PAGE_READWRITE,
            0,
            size as u32,
            PCWSTR(name.as_ptr()),
        ) {
            Ok(h) => h,
            Err(_) => return None,
        };
        // 名字竞争：另一进程刚好抢先建了同名段 → 尝试接管
        // GetLastError() 读的是"本线程上一次 Win32 调用"的错误码（线程局部！），
        //   所以必须紧跟在 CreateFileMappingW 后面判，中间插别的 API 就作废了。
        //   对比：多数 windows crate API 已包装成 Result；这里少数遗留接口仍要手动查。
        if GetLastError() == ERROR_ALREADY_EXISTS {
            info!("[VcamObs] Mapping created but already exists, taking over");
            return queue_open_existing(h_map, cx, cy, fps);
        }
        // MapViewOfFile(句柄, 权限, 高32位偏移, 低32位偏移, 字节数)：后三个全 0 =
        //   "从头映射整个对象"。返回体包在 MEMORY_MAPPED_VIEW_ADDRESS 里，.Value 取出
        //   void* 再 as *mut u8 换成按字节算偏移的指针。null = 映射失败（地址空间不足等）。
        let view = MapViewOfFile(h_map, FILE_MAP_ALL_ACCESS, 0, 0, 0).Value as *mut u8;
        if view.is_null() {
            // 失败了也要把已拿到的句柄关掉 —— 谁创建谁负责关（Drop 只会在对象
            //   "组装成功"之后兜底，这里还没组装 ObsQueue，只能手工收尾）。
            let _ = CloseHandle(h_map);
            return None;
        }

        // 整块清零后写头部（等价 C 侧 struct header = {0} + memcpy）
        // 下面每一行都是"字节偏移 + 类型强转 + write_unaligned"三段式：
        //   view.add(n) 把指针后移 n 个【字节】（*mut u8 的步长是 1）；
        //   as *mut u32 声明"从这开始是一个 4 字节整数"；
        //   write_unaligned 不要求 n 是 4/8 的倍数，任何偏移都能安全写（见 vcam.rs 同款说明）。
        // ⚠ 每个偏移数字都是协议（对照文件头注释的布局表），挪一位滤镜就读错位一位。
        std::ptr::write_bytes(view, 0, size);
        std::ptr::write_unaligned(view.add(8) as *mut u32, STATE_STARTING);
        for i in 0..3 {
            std::ptr::write_unaligned(view.add(12 + i * 4) as *mut u32, offsets[i] as u32);
        }
        std::ptr::write_unaligned(view.add(24) as *mut u32, 0); // type = 视频
        std::ptr::write_unaligned(view.add(28) as *mut u32, cx);
        std::ptr::write_unaligned(view.add(32) as *mut u32, cy);
        std::ptr::write_unaligned(view.add(40) as *mut u64, interval);

        Some(ObsQueue { h_map, view, cx, cy, offsets })
    }

    /// 接管已存在的共享内存（上次会话残留或 OBS 本体创建）。
    /// 读取头部获取分辨率，重置状态为 STARTING，然后可写入新帧。
    // 与 queue_create 的差别只在方向：创建是"我写布局给你看"，接管是"我【读】你
    //   已有的布局来配合你"。read_unaligned 是 write_unaligned 的反向操作（只读不写）。
    //   ⚠ 注意（可疑点，未改动）：若 OBS 本体正在输出虚拟摄像机，接管成功后我们
    //   会把自己的帧覆盖进它的队列（协议允许任何持有者写头）；当前代码不区分
    //   "残留空段"与"OBS 正在用"，要不要拒绝接管（如 READY+分辨率一致时）由产品决定。
    unsafe fn queue_open_existing(h_map: HANDLE, cx: u32, cy: u32, fps: u32) -> Option<ObsQueue> {
        let view = MapViewOfFile(h_map, FILE_MAP_ALL_ACCESS, 0, 0, 0).Value as *mut u8;
        if view.is_null() {
            let _ = CloseHandle(h_map);
            warn!("[VcamObs] Failed to map existing shared memory");
            return None;
        }

        // 读取头部：分辨率和槽偏移
        let existing_cx = std::ptr::read_unaligned(view.add(28) as *const u32);
        let existing_cy = std::ptr::read_unaligned(view.add(32) as *const u32);
        let offsets = [
            std::ptr::read_unaligned(view.add(12) as *const u32) as usize,
            std::ptr::read_unaligned(view.add(16) as *const u32) as usize,
            std::ptr::read_unaligned(view.add(20) as *const u32) as usize,
        ];

        // 如果分辨率不匹配，我们需要重建（但消费者可能还持有旧映射）
        // 这里先接受旧分辨率，后续引擎检测到帧分辨率变化时会尝试重建
        let (use_cx, use_cy) = if existing_cx > 0 && existing_cy > 0 {
            (existing_cx, existing_cy)
        } else {
            (cx, cy)
        };

        // 更新帧率间隔
        let interval = (10_000_000u64 / (fps.max(1) as u64)).max(1);
        std::ptr::write_unaligned(view.add(40) as *mut u64, interval);

        // 重置状态为 STARTING（告诉消费者"生产者重新上线了"）
        std::ptr::write_unaligned(view.add(8) as *mut u32, STATE_STARTING);

        info!(
            "[VcamObs] Took over existing mapping at {}x{} (requested {}x{})",
            use_cx, use_cy, cx, cy
        );

        Some(ObsQueue {
            h_map,
            view,
            cx: use_cx,
            cy: use_cy,
            offsets,
        })
    }

    /// 写一帧 NV12（逐行对应 video_queue_write）
    // ── 这一小段就是"三槽环形队列防撕裂"的全部机制 ──────────────────────
    // 步骤顺序（与 OBS C 侧逐行对应，顺序本身就是发布协议）：
    //   1. inc = ++write_idx：先在【自己的】计数上加一（读-改-写不是原子的，
    //      但全系统只有我们一个生产者会写 [0]，所以单写者下安全）；
    //   2. idx = inc % 3：轮转到三格之一 —— 永远轮着写，不盯着同一格覆盖；
    //   3. 写槽头时间戳、Y 平面、UV 平面：此刻这一格处于"半成品"状态，但消费者
    //      还【不知道】它是最新帧（它只按 read_idx 找帧）；
    //   4. 最后才写 read_idx = inc 和 state = READY —— "写完才挂牌"。
    //      消费者任何时刻看到的 read_idx 要么还是旧帧号（抄旧格，完整），
    //      要么已是新帧号（此时新格已经写完，也完整）。这就是不用锁也基本不撕裂的原因。
    // ⚠ 注意（既有设计，未改动）：步骤 3 的 memcpy 并非瞬时 —— 只有当消费者慢到
    //   让我们多写满一圈（3 帧）绕回它正在抄的那格，才会真撕裂；30fps 下等于
    //   消费者卡顿 >100ms，实际罕见，坏也只坏一帧。若发现持续花屏可考虑加命名
    //   Mutex（OBS 读侧会配合等待）—— 属于协议增强，是否做由维护者定。
    // nv12.len() 不够就直接 return：宁可这一帧不发（消费者继续用上一帧完整画面），
    //   也绝不把半截数据写进共享内存 —— 这是所有写帧路径统一的兜底原则。
    unsafe fn queue_write(q: &ObsQueue, nv12: &[u8], ts: u64) {
        let hdr = q.view;
        let inc = (std::ptr::read_unaligned(hdr as *const u32) + 1) as u32;
        std::ptr::write_unaligned(hdr as *mut u32, inc); // write_idx = inc

        let idx = (inc % 3) as usize;
        let off = q.offsets[idx];
        let y_size = (q.cx as usize) * (q.cy as usize);
        if nv12.len() < y_size + y_size / 2 {
            return;
        }
        // 槽头 8 字节 = 时间戳（100ns 单位，DShow 采样时间直接用）
        std::ptr::write_unaligned((hdr.add(off)) as *mut u64, ts);
        // Y 平面
        std::ptr::copy_nonoverlapping(nv12.as_ptr(), hdr.add(off + FRAME_HEADER_SIZE), y_size);
        // 交错 UV 平面（NV12：U0V0U1V1...，尺寸是 Y 的一半）
        std::ptr::copy_nonoverlapping(
            nv12.as_ptr().add(y_size),
            hdr.add(off + FRAME_HEADER_SIZE + y_size),
            y_size / 2,
        );

        std::ptr::write_unaligned(hdr.add(4) as *mut u32, inc); // read_idx = inc
        std::ptr::write_unaligned(hdr.add(8) as *mut u32, STATE_READY);
    }

    /// RGB24 → NV12（BT.601 limited range：Y 16..235，UV 16..240）
    /// UV 用 2x2 四像素平均后取色度，简单稳定；960x720 单核 <3ms
    // ── NV12 到底是什么（为什么是"Y 平面 + UV 交错"、为什么 1.5 字节/像素）────
    // JPEG 解出来是 RGB24：每像素 3 字节(R,G,B) 平铺 —— 直观但大，而且视频管线不爱它。
    // NV12 属于 YUV(YCbCr) 家族：把"亮度 Y"和"颜色偏移 U/V"拆开存。人眼对亮度细节
    //   远比对颜色敏感，所以颜色可以大幅缩水而看不出差别 —— 这就是视频都用它的原因。
    // NV12 的内存布局（两段连续平面，这就是"1.5 字节/像素"的来历）：
    //   [0 .. w*h)          Y 平面：每像素 1 字节亮度（单独拎出来就是一张灰度图）
    //   [w*h .. w*h*3/2)    UV 交错平面：4:2:0 采样 —— 每 2x2 像素共享一组 (U,V)，
    //                        所以 UV 平面只有 Y 的 1/2 大；且 U、V 不分开存，而是
    //                        U0 V0 U1 V1 … 一行一行的"交错"排列（NV21 则相反是 VUVU）。
    //   合计 = w*h + w*h/2 = 1.5 字节/像素。对照：RGBA=4 字节/像素（屏幕格式，无压缩
    //   冗余）、JPEG=整帧压缩编码（几十 KB，必须先解码成像素才能用；NV12 是未压缩排列）。
    // 为什么 OBS 协议选 NV12（头部 type=0）：摄像头/D3D 视频管线原生格式，滤镜零转换。
    // BT.601 limited range 系数（标清视频标准）：
    //   Y = 0.299R+0.587G+0.114B 的定点化（乘 66/129/25 再 >>8 = 除以 256 取整，
    //   避开浮点循环）；+16 是因为 limited range 的黑色不是 0 而是 16、白色是 235
    //   （0..15 留给同步信号）；UV 以 128 为"无色"中性值，允许范围 16..240。
    //   clamp(16,235)/clamp(16,240) 就是把系数舍入误差导致的越界值拉回合法带内。
    // chunks_exact(3)：按 3 字节切"一个像素"，长度不是 3 的倍数会 panic —— 上游
    //   decode_jpeg_to_rgb 保证 npx*3，所以永远整除；_exact_ 版还给编译器"每片定长 3"
    //   的证明，于是 px[0..2] 无需判界（Rust 用类型消灭越界的小例子，对照 vcam.rs）。
    // ⚠ 注意（可疑点，未改动）：宽高【都是奇数】时 UV 平面会越界 panic —— 例如 3x3：
    //   输出缓冲 w*h*3/2 整除后 UV 只剩 4 字节，但 2x2 分组需要 ceil(3/2)*ceil(3/2)=4 组
    //   共 8 字节。手机当前推偶数分辨率（960x720 等）所以从未触发；若哪天允许任意
    //   分辨率推流，要么先把 w/h 向下取偶，要么换 usize 检查后 bail。
    fn rgb_to_nv12(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
        let mut out = vec![0u8; w * h * 3 / 2];
        // split_at_mut 一次拿到两段【都可写】切片（y 平面和 uv 平面）：
        //   不用它就得每次 out[y_size + dst] 手加偏移，易错且编译器没法证明不重叠。
        let (y_plane, uv_plane) = out.split_at_mut(w * h);
        for (i, px) in rgb.chunks_exact(3).enumerate() {
            let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[i] = y.clamp(16, 235) as u8;
        }
        let mut dst = 0usize;
        for by in (0..h).step_by(2) {
            for bx in (0..w).step_by(2) {
                // 2x2 平均（右下角可能在奇数高度时越界，逐点判界）
                let (mut sr, mut sg, mut sb, mut n) = (0i32, 0i32, 0i32, 0i32);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (yy, xx) = (by + dy, bx + dx);
                        if yy < h && xx < w {
                            let o = (yy * w + xx) * 3;
                            sr += rgb[o] as i32;
                            sg += rgb[o + 1] as i32;
                            sb += rgb[o + 2] as i32;
                            n += 1;
                        }
                    }
                }
                let (r, g, b) = (sr / n, sg / n, sb / n);
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                uv_plane[dst] = u.clamp(16, 240) as u8; // U 在前（NV12 交错序）
                uv_plane[dst + 1] = v.clamp(16, 240) as u8;
                dst += 2;
            }
        }
        out
    }

    /// RGB24 最近邻缩放（用于帧分辨率与映射不匹配时）
    // 最近邻 = 目标像素直接取"离它最近的源像素"，不做加权：最便宜、偶尔有锯齿，
    //   对我们够用（手机帧与映射尺寸通常一致，这里只是兜底，不丢映射不卡死）。
    // 源像素是紧凑 RGB24：跳过一行 = 前进 src_w*3 字节，一个像素 = 3 字节（R,G,B）。
    //   注意 NV12 才有 1.5 字节/像素的说法，RGB24 是 3 字节 —— 本函数还在 RGB 域。
    fn scale_rgb_nearest(rgb: &[u8], src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Vec<u8> {
        let mut out = vec![0u8; dst_w * dst_h * 3];
        let x_ratio = src_w as f64 / dst_w as f64;
        let y_ratio = src_h as f64 / dst_h as f64;

        for dy in 0..dst_h {
            let sy = (dy as f64 * y_ratio) as usize;
            let sy = sy.min(src_h - 1);
            for dx in 0..dst_w {
                let sx = (dx as f64 * x_ratio) as usize;
                let sx = sx.min(src_w - 1);
                let src_idx = (sy * src_w + sx) * 3;
                let dst_idx = (dy * dst_w + dx) * 3;
                out[dst_idx] = rgb[src_idx];
                out[dst_idx + 1] = rgb[src_idx + 1];
                out[dst_idx + 2] = rgb[src_idx + 2];
            }
        }
        out
    }

    /// 黑帧 NV12（Y=16、UV=128）：应用刚打开、手机还没出图时给它干净信号
    // 为什么黑色是 16 而不是 0：limited range 里 16 才是"黑"，0..15 是同步保留区
    //   （写 0 有的滤镜会当超黑压暗、有的直接拒收）；UV=128 = "零色度"，即纯灰/黑。
    // 实现就是 vec![128; n*3/2] 先把整块（Y+UV）填中性值，再把前 n 字节（Y 平面）
    //   改成 16 —— iter_mut().take(n) 是"只改前 n 个元素"的惯用迭代器写法。
    fn black_nv12(w: u32, h: u32) -> Vec<u8> {
        let n = (w as usize) * (h as usize);
        let mut out = vec![128u8; n * 3 / 2];
        for y in out.iter_mut().take(n) {
            *y = 16;
        }
        out
    }

    /// 当前时间 → 100ns 单位时间戳（DShow REFERENCE_TIME 语义）
    // 链式读法：SystemTime::now() 取时刻 → duration_since(UNIX_EPOCH) 得到"距 1970
    //   年过了多久"（返回 Result，系统时钟早于纪元才会 Err）→ .map(...) 只在 Ok 时换算
    //   （as_nanos()/100 = 换成 100ns 一格）→ .unwrap_or(0) 给 Err 一个保底值。
    //   这是"用适配器链代替一堆 ?/match"的常见风格：每一步都可能在类型上说"没有"，
    //   最后一环兜住即可，函数因此不会失败（返回 u64 而不是 Result）。
    fn timestamp_100ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64 / 100)
            .unwrap_or(0)
    }

    // ── 消费者检测：全系统句柄表扫描 ────────────────────────────
    //
    // 已实测验证（src/bin/queue_handle_probe.rs）：
    //   只有生产者 → 1；另一进程 OpenFileMapping 后 → 2；其退出 → 回到 1。
    //   单次扫描 10.8 万条句柄耗时 21~53ms，所以轮询间隔取 1 秒。
    //
    // 注：NtQueryObject 的 ObjectHandleInformation(class 2) 对 Section 对象
    //   不可用（返回 STATUS_INFO_LENGTH_MISMATCH），必须走 SystemExtended
    //   HandleInformation 快照，这是 Sysinternals Handle.exe 的同款算法。

    /// SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX（ntexapi.h，40 字节）
    // ── #[repr(C)] 与内存布局对齐：为什么这个 struct 一个字节都不能飘 ──────
    // 背景：我们要把内核吐出的【C 结构体数组】按字节切开读。Rust 默认布局连字段
    //   【顺序】都不保证（编译器可为对齐自由重排），C 则按声明顺序排 —— 两边要
    //   逐字节对上，就必须写 #[repr(C)] 显式选择"C 布局"。这是所有 FFI/共享内存
    //   结构的标准做法（对比 ObsQueue：它不出本进程，所以不需要 repr(C)）。
    // 64 位下每个字段占多少字节（40 的构成，usize/指针=8、u32=4、u16=2）：
    //   object_id(指针) 8 + unique_process_id(usize) 8 + handle_value(usize) 8
    //   + granted_access(u32) 4 + creator_back_trace_index(u16) 2
    //   + object_type_index(u16) 2 + handle_attributes(u32) 4 + reserved(u32) 4 = 40。
    // 对齐/padding 规则：每个字段的起始地址必须是"自身大小"的倍数，不够就插填充字节；
    //   整个结构大小也补齐到最大字段(8)的倍数。本例巧到无填充：u32 后跟两个 u16
    //   正好拼成 4+2+2=8，再两个 u32=8，40 已是 8 的倍数 → 无尾部 padding。
    // ⚠ padding 为什么会导致读写错位：假如有人把两个 u16 换成一个 u32 之外的任何
    //   改动（比如删掉 reserved），从改动点开始后面所有字段整体平移，我们把
    //   "对象地址"读成"访问权限"——句柄数变垃圾且【不会有任何报错】，极难排查。
    // derive(Clone, Copy)：这 40 字节是纯数值、没有堆资源，可以按位复制 ——
    //   所以下面能用 read_unaligned 一次生成一个完整值而不引起所有权问题。
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct HandleEntryEx {
        object_id: *mut core::ffi::c_void,
        unique_process_id: usize,
        handle_value: usize,
        granted_access: u32,
        creator_back_trace_index: u16,
        object_type_index: u16,
        handle_attributes: u32,
        reserved: u32,
    }

    /// SYSTEM_INFORMATION_CLASS::SystemExtendedHandleInformation = 64
    const SYS_EXT_HANDLE_INFO: u32 = 64;
    /// STATUS_INFO_LENGTH_MISMATCH：缓冲区不够，按 ReturnLength 重试
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;

    // ── extern "system"：FFI（跨语言函数调用）是怎么回事 ─────────────────
    // 这里只是【声明】"ntdll.dll 里存在这个签名的 C 函数"，Rust 不实现它，链接期
    //   把调用直接指过去。"system" = Windows 的 __stdcall 调用约定（参数怎么压栈、
    //   栈由谁清理由被调方规定），写错约定会栈损坏 —— 不能随便换成 "C"。
    // 参数里的 *mut core::ffi::c_void = C 的 void*（类型未知的内存地址）；
    //   返回 i32 = NTSTATUS（0=成功，负数=错误码，所以下面拿它和常量比对而不是判 bool）。
    // 调它的每一行都必须包在 unsafe { }：签名与实际 DLL 是否一致，编译器无法验证。
    #[link(name = "ntdll")]
    extern "system" {
        fn NtQuerySystemInformation(
            class: u32,
            buffer: *mut core::ffi::c_void,
            length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    /// 全系统范围内持有该映射的句柄数。1 = 只有生产者自己，>= 2 = 有应用打开了虚拟摄像头。
    /// 返回 None 表示查询失败（调用方应保持上一次判定，避免状态抖动）。
    // ── 函数体里的几个新手易懵点，一次讲清 ──────────────────────────────
    // buf: Vec<u8> 是一整块【未解析的原始字节】，格式由内核定义 —— 所以解析全靠
    //   read_unaligned（把任意偏移处的 40 字节按 HandleEntryEx 布局拷贝出来）和
    //   usize::from_le_bytes（把固定 8 字节小端拼成 usize），而不是"结构体直接转"。
    // try_into().ok()? 三连读法（本函数是 Option<u32>，这里的 ? 作用在 Option 上）：
    //   buf[0..8] 是 &[u8]（长度运行时才知道）→ try_into() 想要 [u8; 8]（长度写在
    //   【类型】里的数组，Rust 的"泛型带长度"：Vec<u8> 的 <u8> 是元素类型，
    //   [u8; 8] 的 ;8 是编译期长度）→ 长度不符会给 Err，.ok() 换成 None → ? 让
    //   函数立刻返回 None。一个表达式走完"转换+容错+早退"，是 Result→Option→? 的
    //   标准衔接，详细讲解见本文件底部 decode_jpeg_to_rgb 上方的块注释。
    // 缓冲区"先 4MB，不够按 ReturnLength 加倍重试"的循环：内核接口不保证你给的
    //   缓冲够用，标准打法就是"问 → 按要求的长度放大再来"；上限 256MB 防失控。
    unsafe fn mapping_handle_count(h: HANDLE) -> Option<u32> {
        let my_pid = std::process::id() as usize;
        let my_handle = h.0 as usize;
        let stride = std::mem::size_of::<HandleEntryEx>();
        let base = 16usize; // NumberOfHandles(usize) + Reserved(usize)
        let mut size = 4usize << 20;

        loop {
            let mut buf = vec![0u8; size];
            let mut ret: u32 = 0;
            let st = NtQuerySystemInformation(
                SYS_EXT_HANDLE_INFO,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                size as u32,
                &mut ret,
            );
            if st == STATUS_INFO_LENGTH_MISMATCH {
                size = (ret as usize) + (1 << 20);
                if size > (256usize << 20) {
                    return None; // 异常膨胀，放弃本轮
                }
                continue;
            }
            if st != 0 {
                return None;
            }

            let n = usize::from_le_bytes(buf[0..8].try_into().ok()?);
            if base + n * stride > buf.len() {
                return None; // 快照被截断
            }

            // —— 第一趟：找到我们自己那条句柄，拿到 Section 的内核对象地址 ——
            let mut obj: usize = 0;
            for i in 0..n {
                let e: HandleEntryEx =
                    std::ptr::read_unaligned(buf.as_ptr().add(base + i * stride) as *const _);
                if e.unique_process_id == my_pid && e.handle_value == my_handle {
                    obj = e.object_id as usize;
                    break;
                }
            }
            if obj == 0 {
                return None;
            }

            // —— 第二趟：统计全系统指向同一对象的句柄数 ——
            // 必须分两趟：消费者可能比我们更早入表，单趟会在找到 obj 之前漏数。
            let mut count = 0u32;
            for i in 0..n {
                let e: HandleEntryEx =
                    std::ptr::read_unaligned(buf.as_ptr().add(base + i * stride) as *const _);
                if e.object_id as usize == obj {
                    count += 1;
                }
            }
            return Some(count);
        }
    }

    // ── 引擎主循环 ─────────────────────────────────────────────

    // 这个函数是整条通道的"心脏"：一条独立 OS 线程里的 loop，永不正常退出。
    // 节奏一览（每轮 8ms 醒一次）：①1 秒一次数句柄判"有没有应用在看" → ②有帧就
    //   解码转 NV12 写队列 → ③停推超时补黑帧 → ④睡眠。四个步骤都在下面循环体内，
    //   各步骤的"魔数后果清单"见循环体第一处块注释。
    // 参数 mailbox/tx 的所有权：从 spawn_vcam_obs 的 move 一路传进来 ——
    //   mailbox 是 Arc 克隆（和 server.rs、Unity 通道共享同一份邮箱），
    //   tx 是发送端本体（这条线程独占这个发送句柄，用完自动释放）。
    pub fn engine(
        mailbox: crate::vcam::FrameMailbox,
        tx: tokio::sync::mpsc::UnboundedSender<bool>,
    ) {
        info!("[VcamObs] OBS Virtual Camera injection engine started");

        // ── v3.8：分辨率 / 帧率 / 无信号黑帧延时来自 config.json 的 camera 段 ──
        // 出厂默认还是原来的 960x720@30 与 1500ms，行为不变，只是现在能改了。
        // 只在引擎启动时读一次：下面那个主循环 8ms 一轮，每轮去取配置纯属白干活。
        let cam_cfg = crate::config::get().camera;
        let (default_w, default_h) = (cam_cfg.width, cam_cfg.height);
        let fps = cam_cfg.fps;
        let blackout_after = Duration::from_millis(cam_cfg.blackout_after_ms);
        info!(
            "[VcamObs] config: {default_w}x{default_h}@{fps}fps, blackout {}ms",
            cam_cfg.blackout_after_ms
        );

        let mut q: Option<ObsQueue> = None;
        let mut q_size = (0u32, 0u32);
        let mut last_try = Instant::now() - Duration::from_secs(1);
        let mut frames_written: u64 = 0;
        let mut frames_dropped: u64 = 0;
        let mut first_frame_logged = false;

        // —— 消费者（应用）活跃检测状态 ——
        // 每次扫描全系统句柄表约 20~50ms，所以 1 秒轮一次足够（应用开/关摄像头
        // 是秒级人工操作，不需要更灵敏）
        let mut last_poll = Instant::now() - Duration::from_secs(2);
        let mut active = false;
        // 刚创建映射时消费者还没来得及 OpenFileMapping，给 2s 宽限期避免误报释放
        let mut q_created_at = Instant::now();
        let mut last_seen_handles = 0u32;

        // —— 无信号保护 ——
        // 手机断推后，共享内存里会永远留着【断线前的最后一帧】。OBS 滤镜把它
        // 当作实时画面持续送给应用 → 视频会议里显示一张"过期的照片"（既错又涉隐私）。
        // 所以超过 1.5 秒没有新帧就主动写一帧纯黑覆盖掉它。
        let mut last_frame_at: Option<Instant> = None;
        let mut blanked = true; // 启动时是黑帧，无需再覆盖

        // ── 启动时立即创建默认共享内存 ──
        // 不等手机推流、不等注册表——先建好默认分辨率的黑帧映射，
        // 让 OBS Virtual Camera 设备立刻可见，应用才能打开它。
        // （否则陷入死锁：应用要设备存在才能打开 → 设备要应用打开才创建）
        {
            let nq = unsafe { queue_create(default_w, default_h, fps) };
            match nq {
                Some(nq) => {
                    let black = black_nv12(default_w, default_h);
                    unsafe { queue_write(&nq, &black, timestamp_100ns()) };
                    info!("[VcamObs] Initial black mapping created {default_w}x{default_h}");
                    q = Some(nq);
                    q_size = (default_w, default_h);
                }
                None => warn!(
                    "[VcamObs] Initial mapping creation failed (OBS running?), will retry"
                ),
            }
        }

        // ── 本循环的魔数清单（动手改之前先读后果）────────────────────────
        // 8ms 轮询：上限约 125 轮/秒，30fps（33ms 一帧）绰绰有余；改 1ms = 白烧一个
        //   核（帧没那么快来），改 50ms = 帧率直接掉到 20fps 以下、黑帧兜底也变钝。
        // 1s 句柄扫描间隔：单次全系统扫描实测 21~53ms（见上方块注释），1 秒一轮开销
        //   可忽略；开/关摄像头是秒级人工操作，更密只会浪费 CPU。
        // 2s 新建宽限期（settled）：刚建映射时应用还没来得及 OpenFileMapping，
        //   立刻数句柄必是 1 → 误报"没人看"；宽限期只拦"判为释放"，判"有人看"立即生效。
        // 500ms 重建限速：映射被 OBS 本体抢占时别每秒几百次硬撞；也让日志不刷屏。
        // 300ms 回拨 last_poll：新映射后让第一次扫描提前发生，不用干等一整秒。
        // blackout_after_ms（默认 1500）：隐私覆盖延时，来自 config.json —— 改它只改
        //   用户体验，不改协议。太短=手机一卡画面就黑；太长=过期照片挂得久。
        // 10_000_000（queue_create 里的 interval 分子）：每秒的 100ns 数 —— 那是
        //   【协议字段】（头部 [40]），写错滤镜按错误节奏送帧；与上面的"运行节流"无关。
        // 1500/4MB/256MB 等数字散在各处，凡带"⚠"的注释都写明了改动代价。
        // 关于 Arc/Mutex/Atomic 的分工（本文件视角）：帧数据走 Arc<Mutex<邮箱>>（下面
        //   lock 处详述）；AtomicBool/AtomicU32+Ordering 这类"无锁原子量"本文件没用到，
        //   项目里的用例是 main.rs 的 LOG_LEVEL 与 mic_out.rs 的 UPLINK_RATE ——
        //   Ordering 就是原子操作"与其他线程的读写之间保证什么可见性顺序"的强度档位
        //   （Relaxed 最快最弱、SeqCst 最慢最强），本文件跨进程状态全在共享内存裸 u32 上，
        //   靠"单写者 + 写完才挂牌"排序，连 Atomic 都不需要（见 queue_write 块注释）。
        loop {
            // —— 1. 消费者活跃检测：全系统句柄表扫描（1 秒一次）——
            if q.is_some() && last_poll.elapsed() >= Duration::from_secs(1) {
                last_poll = Instant::now();
                match unsafe { mapping_handle_count(q.as_ref().unwrap().h_map) } {
                    Some(count) => {
                        if count != last_seen_handles {
                            info!("[VcamObs] Virtual camera mapping handle count: {count}");
                            last_seen_handles = count;
                        }
                        // 新建映射后的宽限期内不判定"释放"（消费者可能刚打开还没建句柄）
                        let settled = q_created_at.elapsed() >= Duration::from_secs(2);
                        let now_active = count >= 2;
                        if now_active != active && (now_active || settled) {
                            active = now_active;
                            info!(
                                "[VcamObs] OBS camera {}",
                                if active { "is being watched by an app" } else { "released by all apps" }
                            );
                            tx.send(active).ok();
                        }
                    }
                    // 查询失败：保持上一次判定，不抖动状态
                    None => warn!("[VcamObs] handle scan failed, keeping previous state"),
                }
            }
            // 映射丢失 → 无法检测，若之前是活跃状态则报释放
            if q.is_none() && active {
                active = false;
                info!("[VcamObs] OBS camera released (mapping gone)");
                tx.send(false).ok();
            }

            // —— 2. 取最新帧：解码 → RGB24 → NV12 → 写入 OBS 共享内存 ——
            // mailbox.lock().unwrap().take() 三步一口气（Arc<Mutex<Option<Vec<u8>>>> 的
            //   标准消费姿势，逐词讲解见 vcam.rs 引擎第 3 步的同款注释）：
            //   lock() 拿 MutexGuard（RAII：这条语句结束自动解锁，不可能忘记 unlock）；
            //   unwrap() 拆"锁中毒"（持锁线程 panic 过 → 这里跟着 panic，⚠ 引擎线程挂掉
            //     = OBS 通道从此黑屏；全项目统一取舍，不在本次注释改动范围）；
            //   take() 把 Some(jpeg) 整个【挪走】留下 None —— 不复制字节，天然防重复消费。
            // 注意只取"最新一帧"：中间积压的旧帧早被 push_frame 覆盖了，直播宁丢帧不积延迟。
            let frame = mailbox.lock().unwrap().take();
            if let Some(payload) = frame {
                // 【v3.10】同 Unity 通道：先拆方向标记，解码成 RGB 后按标记摆正。
                let (orient, jpeg) = crate::vcam::split_orient(&payload);
                match decode_jpeg_to_rgb(jpeg) {
                    Ok((rgb0, w0, h0)) => {
                        let (rgb, w, h) =
                            crate::vcam::orient_bytes(&rgb0, w0 as i32, h0 as i32, 3, orient);
                        if !first_frame_logged {
                            first_frame_logged = true;
                            info!("[VcamObs] First JPEG from phone: {}x{}, mapping is {}x{}",
                                w, h, q_size.0, q_size.1);
                        }

                        // 分辨率变了 → 缩放到映射分辨率（不丢弃映射，避免卡死）
                        let (rgb_out, out_w, out_h) = if q.is_some() && (w as u32, h as u32) != q_size {
                            let scaled = scale_rgb_nearest(&rgb, w as usize, h as usize, q_size.0 as usize, q_size.1 as usize);
                            (scaled, q_size.0 as usize, q_size.1 as usize)
                        } else {
                            (rgb, w as usize, h as usize)
                        };

                        // 映射不存在 → 创建（500ms 限速重试）
                        if q.is_none() && last_try.elapsed() >= Duration::from_millis(500) {
                            last_try = Instant::now();
                            let nq = unsafe { queue_create(out_w as u32, out_h as u32, fps) };
                            match nq {
                                Some(nq) => {
                                    info!("[VcamObs] OBSVirtualCamVideo mapping created {out_w}x{out_h}");
                                    q = Some(nq);
                                    q_size = (out_w as u32, out_h as u32);
                                    // 新映射：重置检测基线，句柄数从"只有我们"重新开始数
                                    q_created_at = Instant::now();
                                    last_poll = Instant::now() - Duration::from_millis(300);
                                    last_seen_handles = 0;
                                }
                                None => {
                                    frames_dropped += 1;
                                    if frames_dropped % 60 == 1 {
                                        warn!(
                                            "[VcamObs] mapping at {}x{} unavailable ({} frames dropped) — consumer holding old mapping?",
                                            out_w, out_h, frames_dropped
                                        );
                                    }
                                }
                            }
                        }

                        if let Some(qq) = q.as_ref() {
                            let nv12 = rgb_to_nv12(&rgb_out, out_w, out_h);
                            unsafe { queue_write(qq, &nv12, timestamp_100ns()) };
                            frames_written += 1;
                            // 收到真实画面：记下时刻，撤销黑帧标记
                            last_frame_at = Some(Instant::now());
                            blanked = false;
                            if frames_written == 1 {
                                info!("[VcamObs] First frame written to OBS virtual camera at {}x{}", out_w, out_h);
                            }
                        } else if q.is_none() && q_size == (0, 0) {
                            // waiting for mapping creation
                            frames_dropped += 1;
                        }
                    }
                    Err(e) => warn!("[VcamObs] JPEG decode failed: {e}"),
                }
            } else if q.is_none()
                && last_try.elapsed() >= Duration::from_millis(500)
            {
                // 映射丢失（可能被 OBS 本体抢占后释放）→ 重建默认黑帧
                last_try = Instant::now();
                if let Some(nq) = unsafe { queue_create(default_w, default_h, fps) } {
                    let black = black_nv12(default_w, default_h);
                    unsafe { queue_write(&nq, &black, timestamp_100ns()) };
                    q = Some(nq);
                    q_size = (default_w, default_h);
                    q_created_at = Instant::now();
                    last_poll = Instant::now() - Duration::from_millis(300);
                    last_seen_handles = 0;
                    info!("[VcamObs] Placeholder black mapping recreated {default_w}x{default_h}");
                }
            }

            // —— 3. 无信号保护：手机停推超过 camera.blackout_after_ms 就用黑帧盖掉旧画面 ——
            if let Some(qq) = q.as_ref() {
                if let Some(t) = last_frame_at {
                    if !blanked && t.elapsed() >= blackout_after {
                        let black = black_nv12(q_size.0, q_size.1);
                        unsafe { queue_write(qq, &black, timestamp_100ns()) };
                        blanked = true;
                        info!("[VcamObs] 手机已停止推流 → 写入黑帧覆盖旧画面（应用不再显示过期照片）");
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(8)); // 上限 ~125 次循环/秒，够 30fps+
        }
    }

    /// JPEG 字节 → RGB24 缓冲，返回 (像素数据, 宽, 高)。
    /// jpeg-decoder 0.3 默认输出 RGB24；灰度图输出 L8，这里就地扩三通道。
    // ── Result 与 ?：Rust 的"可失败函数"标准写法（本文件唯一范例，讲透一次）──
    // 返回类型 anyhow::Result<(Vec<u8>, u32, u32)> 拆开读：
    //   Result<成功值, 错误> 是一个枚举，只有两种形态：Ok(...) / Err(...)；
    //   尖括号里的 (Vec<u8>, u32, u32) 才是我们的"成功值"——一个三元组（元组：
    //   一次捆多个不同类型值，调用方 match Ok((rgb, w, h)) 一行解构全拿到）。
    //   anyhow::Result 的简写把错误类型统一成 anyhow::Error（能装下任何错误），
    //   所以 ? 上抛不同来源的错（jpeg_decoder 的、自己 bail 的）也不用写转换。
    // ? 运算符读法：`let data = decoder.decode()?;` = "取 Ok 里的值继续走；
    //   若是 Err 就【立即 return】把它交给调用方" —— 错误路径写在类型上、编译期
    //   强制处理，等价别的语言的 try/throw 但没有"忘了 catch"这种事故。
    //   ? 也能作用在 Option 上（None 即早退，见上面 mapping_handle_count 的 try_into().ok()?）。
    // use anyhow::Context：trait（能力接口）要先进作用域，方法才可用 ——
    //   .context("…") 就是 Context trait 给 Result 加的方法：给 Err 补一句人话说明；
    //   decoder.info() 返回 Option，None 也会被 context 转成 Err（Option→Result 的桥）。
    // ensure!/bail! 是 anyhow 的宏：条件不成立就返回 Err / 无条件返回 Err，省写 if。
    // Ok((data, w, h))：最外层 Ok 包元组，成功就这么告诉调用方。
    fn decode_jpeg_to_rgb(jpeg: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
        use anyhow::Context;
        let mut decoder = jpeg_decoder::Decoder::new(jpeg);
        let data = decoder.decode()?;
        let info = decoder.info().context("jpeg 流里没有图像帧")?;
        let (w, h) = (info.width as u32, info.height as u32);
        let npx = (w as usize) * (h as usize);
        anyhow::ensure!(npx > 0, "空图像 {w}x{h}");
        if data.len() == npx * 3 {
            Ok((data, w, h))
        } else if data.len() == npx {
            // L8 灰度 → RGB24
            let mut rgb = vec![0u8; npx * 3];
            for (i, &g) in data.iter().enumerate() {
                let o = i * 3;
                rgb[o] = g;
                rgb[o + 1] = g;
                rgb[o + 2] = g;
            }
            Ok((rgb, w, h))
        } else {
            anyhow::bail!("未知 JPEG 输出格式: {} 字节 / {w}x{h}", data.len());
        }
    }
}
