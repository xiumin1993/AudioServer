// ── 摄像头模式：Unity Capture 虚拟摄像头注入引擎 ──────────────────
//
// 职责：把手机上传的 JPEG 视频帧解码成 RGBA 图像，写入 Unity Capture
//       虚拟摄像头驱动（DirectShow filter）暴露的共享内存。任何 PC 应用
//       （微信/钉钉/会议软件）把摄像头选为 "Unity Video Capture" 就能看到手机画面。
//
// 协议来源：third_party/UnityCapture-master/Source/shared.inl（MIT 授权）。
// 我们是"发送端"（对应 Unity 插件那一侧），驱动 filter 是"接收端"：
//   - 接收端（驱动）创建：Mutex "UnityCapture_Mutx"、Sent 事件、Data 内存映射
//   - 发送端（我们）创建：Want 事件；其余全部是"打开已存在的对象"
//   - 握手：filter 每要一帧就 SetEvent(Want)；我们写完一帧 SetEvent(Sent)
//   - 共享内存头布局（8 个 4 字节整数 = 32 字节，图像数据从偏移 32 开始，
//     与 shared.inl 的 SharedMemHeader 逐字段对齐）：
//       [0] maxSize  [4] width  [8] height  [12] stride  [16] format
//       [20] resizemode  [24] mirrormode  [28] timeout
//   - format=0 (FORMAT_UINT8)：每像素 4 字节，RGBA 顺序（filter 负责换成 BGR(A)）
//
// ★★ v3.4.11 花屏根因（两处字段单位/取值写错，逐条对照驱动源码）：
//   1) stride 的单位是【像素/行】，不是字节。
//      filter 的拷贝函数把源缓冲当 uint32_t* 处理：
//        UnityCaptureFilter.cpp:190/227 → src = BufIn + RowStart * RGBAInStride
//      官方发送端 UnityCapturePlugin.cpp:134 传的是 `RowPitch / 4`（除 bpp）。
//      我们以前写 w*4（字节），filter 理解为"每行 w*4 个像素" → 每行多跳 4 倍
//      → 画面斜向拉丝 = 花屏。
//   2) resizemode 必须给 RESIZEMODE_LINEAR(1)。
//      ProcessImage（Filter.cpp:537-550）一旦发现共享内存宽高 ≠ 应用协商的
//      输出宽高，且 resizemode==DISABLED(0)，就【直接丢弃图像】改画彩色错误条纹
//      ("please set these to match")。驱动默认输出是 _media[0]=1920x1080，
//      手机固定推 960x720 → 永远不匹配 → 表现为"花屏、没有视频"。
//      给 1 之后 filter 自己线性缩放到应用要的分辨率，我们无需知道目标尺寸。
//   3) timeout 不能写 0。Filter.cpp:535 把它换算成"允许连续错过多少帧"：
//      missMax = (timeout + 200 - 1) / 200，timeout=0 → missMax=0，
//      只要有一帧没在 200ms 内应答就立刻显示"Unity has stopped"条纹。
//      写 1000ms（驱动自己的默认值 5 帧）才正常。
//
// 设计要点（和 v3 麦克风引擎同款思路）：
//   1. 引擎线程随服务器启动，常开；驱动没装好时静默重试，不影响其他功能。
//   2. 帧先进"邮箱"（只保留最新一帧，旧帧直接丢弃保实时性）。
//   3. 通过 Want 事件的"新鲜度"判断是否有应用正在取流：
//      最近 1 秒内收到过 Want → 摄像头正被使用（active=true）。
//      服务器据此向手机推 cam_state，手机决定开/关相机硬件
//      （隐私模型对齐 v3.3 按需麦克风：没人用 = 硬件彻底关闭）。

// ── 本文件说明书（初学者请先读完这 15 行再往下翻）────────────────────────
// 我是谁：Unity Capture 虚拟摄像头的"发送端"。全进程一个后台线程，把 JPEG 变成 RGBA
//   写进一块【跨进程共享内存】，剩下怎么变成摄像头画面由驱动 filter（DirectShow）负责。
// 上游（数据从哪来）：src/server.rs 的 WebSocket 读循环 —— 带 4 字节魔术头 [0x03,'C','A','M']
//   的二进制帧，去掉头之后调本文件的 push_frame() 塞进邮箱（同一帧还会复制一份给 vcam_obs.rs）。
// 下游（数据往哪去）：内核命名文件映射 "UnityCapture_Data"。消费它的是 UnityCaptureFilter.dll，
//   它被"看摄像头的那个应用进程"（微信/钉钉/浏览器）加载，我们从没和那个应用直接说过话。
// 谁启动我：src/server.rs 的启动流程（camera.unity_enabled=true 时）调 spawn_vcam()；
//   GUI（src/main.rs）只收 ServerEvent::CamState 之类的事件刷新"有人在观看"灯。
// 手机端搭档：D:\code\PCAssistant\lib\providers\camera_provider.dart（idle→standby→live）。
//   它发 cam_start/cam_stop/cam_capabilities，收 cam_ack/cam_state/cam_request；
//   本文件通过 Want 事件算出的 active 就是 cam_state 的 Unity 路来源（另一路在 vcam_obs.rs）。
// 兄弟实现：src/vcam_obs.rs 是 OBS Virtual Camera 通道（我们自己当共享内存的【创建者】，
//   因此那里才用 CreateFileMappingW；本文件是"加入别人已经建好的"那一侧，只用 Open*）。
// 关键概念索引：命名内核对象=跨进程桥梁 / Mutex+Event=门铃 / #[repr(C)] 与 shared.inl 对齐 /
//   stride 与"每行字节数" / bottom-up 行序 / BGRA 与 RGBA 字节序 / unsafe FFI / Drop 关句柄。
// 改动红线：头部 8 个字段的【顺序、单位、取值】都是和驱动源码逐字对齐的协议，改一个就是花屏。
//
use log::info;
use std::sync::{Arc, Mutex};

/// 最新帧邮箱：手机 WS 读循环写入 JPEG 字节，引擎线程取走并解码。
/// 只存一帧——网络快于显示时自动丢旧帧，永远保持实时。
// ── 拆开看这个类型（和 mic_out 的 MicQueue 是"同族但不同用途"）───────────
// Arc<Mutex<...>>：和音频队列一样 —— Arc 让网络线程与引擎线程共享同一份数据（引用计数），
//   Mutex 保证任意时刻只有一方能碰。区别全在内层：
// Option<Vec<u8>>：Option = "可能没有帧"（None）或"有一帧"（Some(字节)）。
//   注意这里【只有一格】而不是队列：视频和音频的容错方式根本不同 ——
//   音频少一段 = 明显断流，所以要排队；视频少一帧 = 无所谓（下一帧马上到），
//   所以宁可覆盖掉没来得及画的旧帧，也绝不让延迟累积（旧帧攒着 = "通话里看到 3 秒前的自己"）。
// Vec<u8>：拥有所有权的字节缓冲，装的是【完整 JPEG 编码】数据（不是解码后的像素），
//   解码放在引擎线程做，网络线程只搬运 —— 谁的工作谁做，锁的持有时间也最短。
pub type FrameMailbox = Arc<Mutex<Option<Vec<u8>>>>;

// 工厂函数：上层（src/server.rs）建状态时调用，这样"邮箱长什么样"只在本文件定义一次。
pub fn new_mailbox() -> FrameMailbox {
    Arc::new(Mutex::new(None))
}

/// 手机推来一帧 JPEG（直接换进邮箱，替换掉还没消费的旧帧）
pub fn push_frame(mailbox: &FrameMailbox, jpeg: Vec<u8>) {
    // 参数 jpeg: Vec<u8> 按【值】传入 = 所有权交给本函数；下面直接 move 进邮箱，全程零拷贝。
    //   （server.rs 那边给两路引擎时自己 clone 了一份，成本在那儿，不在这里。）
    // 左边是 *：先解引用 MutexGuard 拿到它里面的 Option<Vec<u8>>，再整体赋值 = 旧的帧被替换并丢弃。
    //   这一句就是"只保留最新一帧"的全部实现 —— 没有 push、没有长度、没有 drain。
    // ⚠ 注意（既有设计，未改动）：.unwrap() 在锁"中毒"（持锁线程 panic）时会连带 panic；
    //   这个函数跑在 WebSocket 读任务里，真 panic 会让该连接断开，而不是静默坏掉，可以接受。
    *mailbox.lock().unwrap() = Some(jpeg);
}

/// 启动注入引擎线程（仅 Windows；其他平台是空 stub）。
/// tx 在"是否有应用正在观看摄像头"状态翻转时回传（true=正在被取流）。
// #[cfg(windows)] / #[cfg(not(windows))]：属性，决定这段代码参不参与编译。
//   Unity Capture 是 Windows 专属 DirectShow 驱动，Mac/Linux 上连"打开共享内存"这些 API 都不存在，
//   所以给出一个空 stub 让上层 src/server.rs 无条件调用 spawn_vcam(...)，平台差异不外溢。
// tx: tokio::sync::mpsc::UnboundedSender<bool> —— 发送方是普通 std 线程（只能同步 send），
//   接收方在 server.rs 的 async 任务里 await recv()，翻转时广播 cam_state 给手机。
// thread::spawn(move || ...)：move 把 mailbox（Arc 克隆）与 tx 的所有权搬进新线程；
//   引擎是死循环 + 阻塞等待，绝不能放进 tokio 工作线程（会占死一个执行槽）。
#[cfg(windows)]
pub fn spawn_vcam(mailbox: FrameMailbox, tx: tokio::sync::mpsc::UnboundedSender<bool>) {
    std::thread::spawn(move || windows_impl::engine(mailbox, tx));
}

/// 非 Windows stub：什么都不做（Unity Capture 是 Windows 专属驱动）
#[cfg(not(windows))]
pub fn spawn_vcam(_mailbox: FrameMailbox, _tx: tokio::sync::mpsc::UnboundedSender<bool>) {}

// ── Windows 实现：整块包进 mod，把 unsafe 和 Win32 依赖关在一个房间里 ─────
// 公共 API（spawn_vcam / push_frame / FrameMailbox）在文件顶部，任何平台都能引用；
//   内核对象、裸指针这些细节只有 windows_impl 内部看得见。
#[cfg(windows)]
mod windows_impl {
    use super::*;
    use log::warn;
    // Duration = 一段时间（用来比较"过了多久"）；Instant = 一个时刻（掐表用的"现在"）。
    //   Rust 刻意区分这两者：t.elapsed() 返回 Duration，两个 Instant 相减也是 Duration，
    //   而 Duration 之间不能相减 —— 类型上就防止了"拿时刻当时长用"这类 bug。
    use std::time::{Duration, Instant};
    // PCSTR = 指向以 NUL 结尾的 8 位（ANSI）字符串指针。Win32 API 每个都有 A(ANSI)/W(Unicode)
    //   两套入口：这里跟着官方 shared.inl 用 A 版（OpenMutexA/OpenEventA/CreateEventA/
    //   OpenFileMappingA），因为驱动那边建对象时用的就是 ANSI 名 —— 名字必须逐字节对得上，
    //   换成 W 版反而会打不开。代价：对象名只能是 ASCII（我们这几个名字确实都是）。
    use windows::core::PCSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    // ── 共享内存三件套：它们各自干什么 ─────────────────────────────────
    // 文件映射（File Mapping）= 内核里一块"有名字、可以多个进程同时挂上"的内存页。
    //   CreateFileMappingW/A：【创建】这块内存并给它起名（没有则新建，已有则打开已有的）。
    //                  —— 本文件不调它：驱动 filter 才是创建者，我们是来"加入"的。
    //                  （想看创建版怎么写，参考 src/vcam_obs.rs:155，那边我们是生产者/创建者。）
    //   OpenFileMappingA：按【全局名字】拿到已存在映射对象的句柄（第二个参数 bInheritHandle=false
    //                  = 不要被子进程继承；第三个参数是名字）。打不开 = 驱动还没跑起来。
    //   MapViewOfFile：把这块内存【映射进本进程的地址空间】，返回一个可以直接读写的首地址。
    //                  句柄本身不能当指针用 —— 必须 MapView 之后才有可访问的字节。
    //                  最后参数传 0 = "映射整个对象"；返回值包在 MEMORY_MAPPED_VIEW_ADDRESS 里。
    //   UnmapViewOfFile：解除映射（Drop 里调）。句柄另由 CloseHandle 关，两者是分开的两步。
    use windows::Win32::System::Memory::{
        MapViewOfFile, OpenFileMappingA, UnmapViewOfFile,
        MEMORY_MAPPED_VIEW_ADDRESS, FILE_MAP_WRITE,
    };
    // ── Mutex 与 Event：内核提供的"排队锁"和"门铃" ──────────────────────
    //   CreateEventA：建一个事件对象（我们是 Want 事件的建铃人）。第二参 false = auto-reset：
    //                  WaitForSingleObject 被叫醒后信号自动复位，一次 Wait 只消化一次 Set。
    //   OpenEventA：按名字打开已存在的事件（Sent 由驱动建，我们只要"能按铃"的权限）。
    //   SetEvent：把事件置为有信号 → 叫醒正在 Wait 的那一方（= 按门铃，不等待、不阻塞）。
    //   WaitForSingleObject(句柄, 超时)：等信号。本文件用到两种超时：
    //                  INFINITE = 一直等（只有取 Mutex 用，临界区极短所以可接受）；
    //                  0 = 【探测】：不等，立刻返回"有信号/没信号"（探测 Want 用，绝不卡循环）。
    //                  返回值 WAIT_OBJECT_0 表示"确实等到了信号"。
    //   ReleaseMutex：交出互斥锁。必须和成功获取配对（见 send_frame 的说明）。
    //   EVENT_MODIFY_STATE = "只要求置信号"的最小权限；INFINITE 是特殊超时值（0xFFFFFFFF）。
    use windows::Win32::System::Threading::{
        CreateEventA, OpenEventA, ReleaseMutex, SetEvent, WaitForSingleObject,
        EVENT_MODIFY_STATE, INFINITE,
    };
    use windows::Win32::System::WindowsProgramming::OpenMutexA;

    // OpenMutexA 收裸 u32：SYNCHRONIZE = 0x0010_0000
    // 这是 Windows 的访问权限位（access mask）："只允许我用它来做同步等待"。
    // 为什么是裸 u32 而不是类型：windows crate 给多数 API 发了 NewType 包装，
    //   OpenMutexA 这一路没包（历史遗留），所以只能自己传数字。
    // ⚠ 要更大的权限（如 MUTEX_ALL_ACCESS）反而会因服务/策略被拒 —— 最小权限原则是对的。
    const SYNCHRONIZE_U32: u32 = 0x0010_0000;

    // 同步对象名字（cap 0：名称以 '\0' 结尾，与 shared.inl 完全一致）
    // ── "命名对象"为什么能当跨进程桥梁 ────────────────────────────────
    // Windows 内核维护一棵"对象命名空间"树（形如 \BaseNamedObjects\UnityCapture_Data）。
    //   任何进程用【同一个名字】Create/Open 同一个内核对象，拿到的就是【同一份】内核数据 ——
    //   这就是"没有 socket、没有管道、也没引用彼此代码"的两个进程能协作的全部原理。
    //   所以我们不需要驱动 DLL 的任何导出函数，只要名字、布局、时序三件事对得上。
    // 名字前缀的讲究：这里用的是【不带 Global\ 前缀】的会话级名字（官方 shared.inl 也如此）。
    //   Global\ 是跨登录会话可见，需要 SeCreateGlobalPrivilege 特权；会话级名字对本桌面的
    //   所有进程可见，正合适（我们的服务和应用都在同一个交互桌面会话里）。
    //   ⚠ 若驱动哪天改成 Global\ 建对象，我们这边不加前缀就永远 Open 不到（表现为永不挂接）。
    // 这四个名字一个都不能拼错、一个都不能改：改 = 打不开对象 = 摄像头彻底没画面，
    //   而且不会有任何错误提示以外的现象（只有 [Vcam] 日志停在没挂上那句）。
    // "cap 0"的含义：官方源码名字模板是 "UnityCapture_Data0"，把末位数字换成摄像头序号；
    //   序号 0 时它把那一格写成 '\0'（即截断），所以第 0 路的真实名字就是去掉尾数的这四个。
    const NAME_MUTEX: &str = "UnityCapture_Mutx";
    const NAME_WANT: &str = "UnityCapture_Want";
    const NAME_SENT: &str = "UnityCapture_Sent";
    const NAME_DATA: &str = "UnityCapture_Data";

    /// 共享内存里的最大图像字节数（4K RGBA 16bit，与 shared.inl 的
    /// MAX_SHARED_IMAGE_SIZE = 3840*2160*4*2 一致）
    // 算式拆开：3840 × 2160 = 4K 像素数；× 4 = 每像素 4 通道；× 2 = 每通道最多 16 位。
    //   ≈ 63.3 MB（3840*2160*8 = 66,355,200 字节）。
    // ⚠ 这个数是【协议常量】，不是"想省内存就能改的小节"：驱动 filter 用同一个常量建映射，
    //   并在初始化时检查头部 maxSize 是否等于它的值（shared.inl:140）。
    //   改小 → filter 认为 maxSize 不符、可能直接重建/拒收（现象：写进去没画面）；
    //   改大 → 我们以为能写更多，越界写到映射之外（未定义行为，可能踩坏别的内存）。
    //   手机现在只推 960×720（约 2.7MB），远用不满 —— 用不满是好事，说明安全。
    const MAX_SHARED_IMAGE_SIZE: usize = 3840 * 2160 * 4 * 2;

    /// 引擎持有的四个内核对象 + 映射视图指针。
    /// ★ 与原版 C++ SharedImageMemory 一致：这是"增量累积"的状态结构——
    /// 每次重试只补做缺失的一步，已拿到的句柄跨重试保留、绝不提前关闭。
    /// 原因：Want 事件由发送端创建、filter 用 OpenEvent 找它；若发送端在
    /// 打开 Sent/映射失败时把 Want 一起关掉销毁，filter 就永远打不开 Want，
    /// 双方互相等对方 → 握手死锁（v3.4 联调时实测踩中）。
    struct Sender {
        // HANDLE 是内核对象句柄（本质一个不透明指针）。四个分别对应：
        //   h_mutex = 排队锁（写帧时防止和别人同时写）、h_want = "驱动要我出一帧"的门铃、
        //   h_sent = "我写好了一帧"的门铃、h_map = 那块共享内存对象本身。
        h_mutex: HANDLE,
        h_want: HANDLE,
        h_sent: HANDLE,
        h_map: HANDLE,
        // view 是 MapViewOfFile 回来的裸指针：共享内存里【第一个字节】的地址。
        //   前 32 字节是协议头，之后才是像素（见 write_header）。
        //   为什么是 *mut u8 而不是 &mut [u8]：这块内存不属于 Rust 的所有权体系、长度只由
        //   协议规定，无法给它是生命周期标注 —— 只能用裸指针手动管（因此处处要 unsafe）。
        //   约定：null = 还没映射（同时充当"挂接是否完成"的标志位，见 try_attach 的 if）。
        view: *mut u8,
    }

    // 线程间只有引擎线程自己读写 Sender，邮箱帧用 Arc<Mutex> 传递。
    // ── 这行 unsafe impl Send 是什么意思 ────────────────────────────────
    // 编译器默认认为"含裸指针的类型不能安全地跨线程移动"（*mut u8 没实现 Send），
    //   因为裸指针的访问纪律编译器无法验证。Sender 只被 spawn 出来的那一个引擎线程持有，
    //   事实安全，所以要【人】写一行 unsafe impl Send 来签字担保。
    // 这条担保的义务：以后绝不能再把 Sender 的引用交给第二个线程并发读写（那才是真炸）。
    //   （对照 MicQueue：那边用的是 Arc<Mutex<..>>，安全边界由 Mutex 保证，就不需要这种担保。）
    unsafe impl Send for Sender {}

    impl Drop for Sender {
        // Drop = Rust 的 RAII：值离开作用域（或被显式 drop）时系统自动调这段清理代码。
        //   好处是"关句柄"这件事不可能被忘记、也不可能被 return / ? 提前跳路绕开 ——
        //   本文件的 engine() 里全是 ? 和 continue，却一处不需要写清理。
        //   与之配套的两点纪律：① 用 is_null()/is_invalid() 判断"可能还没拿到"的资源，
        //   ② 忽略清理调用的返回值（let _ =）：进程/线程都要退了，报错也无从处理，
        //      但绝不能不 panic（在 drop 里 panic 是极坏的主意）。
        //   反过来说：Drop 只在 Sender【被丢弃】时跑。本引擎设计上一直持有它（不重连时不释放），
        //   所以这段实际是"线程退出/未来重构才生效"的安全网。
        fn drop(&mut self) {
            unsafe {
                if !self.view.is_null() {
                    // UnmapViewOfFile 要的是 MEMORY_MAPPED_VIEW_ADDRESS{ Value: *mut c_void }，
                    //   所以这里做一次指针类型换算（视图地址换成"通用 void 指针"）。
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.view as *mut core::ffi::c_void,
                    });
                }
                // [a, b, c, d] 是数组字面量（元素同类型），for 遍历把四个句柄挨个 CloseHandle。
                for h in [self.h_mutex, self.h_want, self.h_sent, self.h_map] {
                    // 句柄"从未成功获取"时是 NULL；is_invalid() 把它和 INVALID_HANDLE_VALUE 都算无效。
                    if !h.is_invalid() {
                        let _ = CloseHandle(h);
                    }
                }
            }
        }
    }

    /// 把 Rust 字符串转成 C 风格 UTF-8 字节串（末尾补 '\0'，供 PCSTR 用）
    // Rust 的 &str 只带"长度"，C 的字符串靠"结尾一个 0 字节"表示结束，两者不兼容。
    //   PCSTR 要的是后者，所以这里复制一份字节 + 补 \0。
    // 返回的 Vec<u8> 必须活到 API 调用之后：调用点都写成
    //   `let name = cstr(NAME_MUTEX); ... PCSTR(name.as_ptr())`，name 在语句结束时才 drop。
    //   ⚠ 千万别把它写成返回值里的指针 —— 那样 Vec 一 drop，指针立刻悬空（经典 FFI 事故）。
    // 注意这里用 UTF-8 字节而对象名其实都是 ASCII：ASCII 下 UTF-8 和 ANSI 逐字节相同，所以恰好
    //   和驱动用的 ANSI API 对得上；将来若把名字改成中文，A 版 API 就会对不上（需要换 W 版）。
    fn cstr(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    impl Sender {
        /// 全空状态：所有句柄 NULL、视图未映射
        // 关联函数（没有 self 参数）相当于其他语言的"静态工厂方法"，这里用私有 fn 而不是 pub。
        // Self = "本类型"的别名（写 Sender { .. } 或 Self { .. } 等价）。
        // HANDLE(*mut _) 包装的就是裸指针；这里全填 NULL，等于"四样东西一个都还没有"。
        //   为什么不用 Option<HANDLE>：try_attach 里到处是 is_invalid() 判断，跟 C 的
        //   "NULL 句柄"语义完全一致，反而比包一层 Option 再 unwrap 更省事、更少分支。
        fn empty() -> Self {
            Sender {
                h_mutex: HANDLE(std::ptr::null_mut()),
                h_want: HANDLE(std::ptr::null_mut()),
                h_sent: HANDLE(std::ptr::null_mut()),
                h_map: HANDLE(std::ptr::null_mut()),
                view: std::ptr::null_mut(),
            }
        }
        /// 是否已全部挂上（可以收发帧）
        // ⚠ 注意（基线里就有的警告，未处理）：这个方法目前【没有被调用】，
        //   所以 cargo check 报 "method ready is never used"。engine() 用的是自己的
        //   attached 布尔变量来记同一件事。要么删掉它、要么在 engine 里改用它 ——
        //   都属于改代码，本次只加注释，所以留着让警告继续存在。
        fn ready(&self) -> bool {
            !self.h_mutex.is_invalid()
                && !self.h_want.is_invalid()
                && !self.h_sent.is_invalid()
                && !self.h_map.is_invalid()
                && !self.view.is_null()
        }
    }

    /// 增量尝试挂接四个共享对象：每步只在"还没有句柄"时才去拿，
    /// 失败就原样返回 false——**已拿到的句柄保留在 p 里**（关键，见结构体注释）。
    /// 全部拿到后返回 true。
    // unsafe fn：整条函数体默认处于 unsafe 上下文（2021 edition），所以里面调 Win32 不再逐个包 unsafe。
    //   ⚠ 代价是"读代码的人看不出哪一行危险"，也和编译器未来的 `unsafe_op_in_unsafe_fn` 方向不一致；
    //   本文件其它地方（engine 里的调用点）仍写了 unsafe { } 块，两处都写不会出错。
    // 参数 p: &mut Sender = "可 borrowing 地改它"：句柄直接写回调用方的 sender，
    //   所以失败返回时已经拿到的部分【不会丢】—— 这正是"增量累积"的实现方式。
    // 返回 bool 而不是 Result：这里没有"需要向上报的错误"，失败的含义只是"驱动还没准备好，下轮再来"。
    unsafe fn try_attach(p: &mut Sender) -> bool {
        // 1) 互斥体：由驱动（filter）创建，我们只能打开
        // is_invalid() 为真 = "这一步还没做过"，所以整个函数可以被反复调用而不会重复拿句柄。
        if p.h_mutex.is_invalid() {
            let name = cstr(NAME_MUTEX);
            // OpenMutexA(想要的权限, 是否被子进程继承, 名字)：拿不到就是无效句柄。
            //   注意 Windows 的"打开内核对象"失败通常不 panic 也不返回 Result，而是给个无效值，
            //   所以这里用 is_invalid() 判断，风格和其他返回 Result 的调用不同（对比第 3 步）。
            let h = OpenMutexA(SYNCHRONIZE_U32, false, PCSTR(name.as_ptr()));
            if h.is_invalid() {
                return false; // filter 还没跑起来，下轮再试（Want 也还没创建）
            }
            p.h_mutex = h;
        }

        // 2) Want 事件：发送端负责创建（filter 用同名打开），auto-reset。
        //    创建成功后必须一直持有——这就是修复握手死锁的核心。
        if p.h_want.is_invalid() {
            let name = cstr(NAME_WANT);
            // CreateEventA(安全属性=None, bManualReset=false(auto-reset),
            //               bInitialState=false(初始无信号), 名字)。
            //   auto-reset 是关键：WaitForSingleObject 成功等待一次就自动把信号清掉，
            //   于是"一次索帧"只对应"一次唤醒"，不会在我们这边留下重复的旧信号。
            // if let Ok(h) = ... { ... } 是"只关心成功值"的写法（等价于 match 的 Ok 分支 +
            //   一个什么也不做的 Err 分支）：创建失败（名字被占用/权限问题）就什么也不做，
            //   保持无效句柄，下轮再试 —— 这正是增量语义想要的效果。
            if let Ok(h) = CreateEventA(None, false, false, PCSTR(name.as_ptr())) {
                p.h_want = h;
            }
        }

        // 3) Sent 事件：由驱动创建，我们只需要"置信号"权限。
        //    打开失败 = filter 还没走到创建这一步，保留 mutex+want 等下轮。
        if p.h_sent.is_invalid() {
            let name = cstr(NAME_SENT);
            // match 把 Result 的两个分支都写明：成功 → 存句柄；失败 → 立刻 return false。
            //   EVENT_MODIFY_STATE 是最小权限（只要 SetEvent 能用），拿不到更宽的权限不影响功能。
            match OpenEventA(EVENT_MODIFY_STATE, false, PCSTR(name.as_ptr())) {
                Ok(h) => p.h_sent = h,
                Err(_) => return false,
            }
        }

        // 4) 图像共享内存：由驱动创建，我们写入
        if p.h_map.is_invalid() {
            let name = cstr(NAME_DATA);
            // OpenFileMappingA(FILE_MAP_WRITE, false, 名字)：只要"可写映射"的权限。
            //   FILE_MAP_WRITE.0 里的 .0 是取出包装类型内部那个 u32（访问掩码），因为
            //   这个 API 的参数类型是裸 u32 —— 和上面 SYNCHRONIZE_U32 一个道理。
            match OpenFileMappingA(FILE_MAP_WRITE.0, false, PCSTR(name.as_ptr())) {
                Ok(h) => p.h_map = h,
                Err(_) => return false,
            }
        }
        // 有了映射【句柄】还不够：句柄只是"对象的名字牌"，必须 MapViewOfFile 才有能读写的地址。
        if p.view.is_null() {
            // MapViewOfFile(句柄, 权限, 高 32 位偏移, 低 32 位偏移, 映射字节数)
            //   偏移写 0、字节数写 0 = "从最开头映射整个对象"（Windows 对 0 的约定）。
            //   返回 MEMORY_MAPPED_VIEW_ADDRESS（结构体包一个 *mut c_void），.Value 取出来再
            //   换成 *mut u8 —— 之后一切按字节算偏移，正是下面 write_header 的做法。
            p.view = MapViewOfFile(p.h_map, FILE_MAP_WRITE, 0, 0, 0).Value as *mut u8;
            if p.view.is_null() {
                // 映射失败（地址空间不够等）：句柄保留，下轮只重试这一步。
                return false;
            }
        }
        // 四样齐全 = 可以开始握手收帧（返回值只在 true 时被 engine 当成"挂上了"）。
        true
    }

    /// 写共享内存头部的 8 个字段。stride 单位=像素（见文件头 ★★ 说明 1），
    /// resizemode=LINEAR（说明 2），timeout=1000ms（说明 3）。
    // ── 内存布局与 #[repr(C)]：为什么这块内存必须"逐字节照抄上游" ──────────
    // 官方 shared.inl 的 SharedMemHeader 是个 C 结构体（8 个 int，共 32 字节）。
    //   C 语言允许编译器【自由排列】结构体字段的顺序和填充，而 Rust 默认布局连顺序都不保证。
    //   所以只要想让 Rust 结构和 C 端对上，就必须写 #[repr(C)]（"按 C 的规则排：声明顺序 +
    //   C 的对齐与填充）。参考同项目 src/vcam_obs.rs:355 就是一个 #[repr(C)] struct。
    // 本函数选了另一条更直白的路：不定义结构体，直接按【字节偏移】手写每个字段 ——
    //   偏移量本身就是协议（0/4/8/12/16/20/24/28），读代码时一眼能对着 shared.inl 核。
    //   两种做法效果相同；改成 #[repr(C)] 结构体也行，但必须保证 i32 × 8 = 32 字节、零填充。
    // 为什么用 write_unaligned 而不是 *(ptr as *mut i32) = val 或直接转 &mut：
    //   共享内存基址、字段偏移都不保证 4 字节对齐，而 Rust 的引用会假设"自然对齐"；
    //   未对齐上做 32 位写在 x86 上通常能用（性能略损），在 ARM 上是硬件错误 —— 而这里
    //   恰恰是"字节级内存操作"，最稳的就是 write_unaligned/read_unaligned。
    // ⚠ 八个字段的【顺序、单位、取值】全是和驱动源码逐字对齐的协议，改任何一项的代价：
    //   maxSize 改 → 见上面常量的说明；width/height 与像素不符 → 图像被裁/被拉伸错位；
    //   stride 写成字节 → 花屏（文件头说明 1）；format 非 0 → filter 按 float16 读 → 彩色雪花；
    //   resizemode 写 0 → 分辨率不匹配时直接错误条纹（说明 2）；mirrormode 写 1/2 → 画面左右翻；
    //   timeout 写 0 → 一帧没赶上就显示 "Unity has stopped"（说明 3）。
    unsafe fn write_header(s: &Sender, w: i32, h: i32) {
        let hdr = s.view;
        // hdr.add(n)：指针向后移 n 个【字节】（hdr 是 *mut u8，add 的步长是元素大小 1）。
        //   换到行首就是"偏移 n 处那个字段"，再 as *mut i32 表示"从这里开始放一个 4 字节整数"。
        // 注意下面每行都是"先算偏移、再写值"，没有一次内存读，所以不存在读到半写状态的风险
        //   （filter 那一侧读同一块内存时是持着同一个 Mutex 的，见 send_frame）。
        std::ptr::write_unaligned(hdr as *mut i32, MAX_SHARED_IMAGE_SIZE as i32);
        std::ptr::write_unaligned(hdr.add(4) as *mut i32, w);
        std::ptr::write_unaligned(hdr.add(8) as *mut i32, h);
        // stride：每行有多少个【像素】。我们的数据是紧凑排列的，所以 == w。
        // （原版是 D3D11 纹理，RowPitch 可能带尾部填充，才需要除以 bpp。）
        // ── stride / pitch / "每行字节数" 三者关系（初学者最容易绕晕的地方）──────
        // 图像在内存里是一行一行平铺的。GPU 纹理为了 SIMD 访存效率，常把每行字节数向上取整
        //   到某个对齐值 —— 这个"实际占用的每行字节数"就是 pitch（DX 叫 RowPitch）。
        //   于是：每行字节数 = 有效像素数 × 每像素字节数 + 行尾填充。
        // 但 filter 需要的 stride 单位是【像素/行】（它按 uint32_t* 逐行跳），所以官方发送端
        //   传的是 RowPitch / 4（字节÷每像素字节数 = 像素个数）。
        // 我们的像素是自己从 JPEG 解出来、按 w×4 字节紧凑排的，没有任何填充，所以
        //   pitch = w×4 字节、stride = w 像素 —— 两者只差那个 ÷4，这正是当年花屏的根因。
        //   如果哪天改成复用一块带填充的缓冲（例如直接接 GPU 回读），这行就要跟着改成 pitch/4。
        std::ptr::write_unaligned(hdr.add(12) as *mut i32, w);
        std::ptr::write_unaligned(hdr.add(16) as *mut i32, 0); // format = FORMAT_UINT8（RGBA8）
        std::ptr::write_unaligned(hdr.add(20) as *mut i32, 1); // resizemode = RESIZEMODE_LINEAR
        std::ptr::write_unaligned(hdr.add(24) as *mut i32, 0); // mirrormode = DISABLED
        std::ptr::write_unaligned(hdr.add(28) as *mut i32, 1000); // timeout(ms)
    }

    /// 在互斥锁保护下写入共享内存头 + RGBA 像素（对应 shared.inl 的 Send 前半段）
    // 补充：本函数自己【不加锁】，加锁在调用方 send_frame（文档注释里说的"互斥锁保护下"
    //   指的是整条 Send 路径，不要误以为这里已经锁过了）。
    unsafe fn write_header_and_data(
        s: &Sender,
        w: i32,
        h: i32,
        rgba: &[u8],
    ) -> bool {
        // 参数 rgba: &[u8] = 借用一段连续字节（调用方保留所有权），长度由 .len() 自带 ——
        //   所以 Rust 里不需要"指针 + 长度"分开传，也就少一类越界错误。
        // 一帧的字节数 = 宽 × 高 × 4（每像素 RGBA 各 1 字节 = 4 字节）。
        let data_size = (w as usize) * (h as usize) * 4;
        // 四层保险，任何一条不满足就【拒绝写】并返回 false（调用方据此决定要不要按 Sent 的铃）：
        //   w/h <= 0 → 负数/零尺寸经过 as usize 转换后会变成巨大数字（这就是先判 <=0 的原因）；
        //   data_size > MAX_SHARED_IMAGE_SIZE → 会写出映射末尾，属于内存破坏，绝不放行；
        //   rgba.len() < data_size → 调用方给的像素不够，宁可这帧不发（filter 继续用上一帧）。
        // ⚠ 注意（既有行为，未改动）：坏帧只是"这一帧丢掉"，不会让引擎退出，也不会通知手机。
        if w <= 0 || h <= 0 || data_size > MAX_SHARED_IMAGE_SIZE || rgba.len() < data_size {
            return false;
        }
        write_header(s, w, h);
        // ── 行序翻转（v3.4.1 修复 Unity 通道上下颠倒）──
        // UnityCapture 的共享内存沿用 Unity 纹理约定 = **从下往上**（OpenGL 行序），
        // filter 会把第 0 行原样拷进 bottom-up DIB 的最后一行。
        // 而 jpeg-decoder 输出是**从上往下**的常规图像行序，
        // 直接写入 → 浏览器（MF→FrameServer→DShow 桥）里画面 180° 上下颠倒。
        // 解决：逐行倒序拷贝（OBS 通道是 top-down 约定，那边不动）。
        // ── 把"行序"讲透（初学者一定会问"到底哪行在最上面"）──────────────────
        // 同一张图在内存里有两种约定，差别只在"第一行字节"画的是头顶还是下巴：
        //   top-down（从上往下）：JPEG 解码输出、多数图像缓冲区 —— 第 0 行 = 画面最上面。
        //   bottom-up（从下往上）：OpenGL/D3D 纹理（坐标 v=0 在底部）、Unity 纹理约定 ——
        //     第 0 行 = 画面最下面。共享内存跟的是这一套。
        // 所以我们写进共享内存的第 0 行必须是【画面的最后一行】；写反了的后果是整幅画面
        //   上下颠倒（头朝下），既不报错也不花屏，极易被误判成"驱动坏了"或"手机预览问题"
        //   —— 这个坑真踩过，才留下下面这段倒序拷贝。
        {
            // row = 每行字节数（= w × 4）。偏移和长度一律用 usize（Rust 的下标/长度类型）。
            let row = (w as usize) * 4;
            // 32 = 协议头长度（8 个 i32）。像素区从偏移 32 开始，这个数字改了就全盘花屏。
            //   ⚠ 它和 write_header 里 add(28) 再 +4 是同一事实的两处写法，改布局必须一起改。
            let dst_base = s.view.add(32);
            for y in 0..h as usize {
                // copy_nonoverlapping(源, 目标, 字节数)：等价 C 的 memcpy，要求两块内存不重叠
                //   （源在 Rust 堆上、目标在共享内存，当然不重叠）且两边空间足够
                //   （上面那条 rgba.len() < data_size 检查正是为这一行准备的）。
                std::ptr::copy_nonoverlapping(
                    rgba.as_ptr().add((h as usize - 1 - y) * row),
                    dst_base.add(y * row),
                    row,
                );
            }
        }
        true
    }

    /// 锁互斥体 → 写帧 → 解锁 → SetEvent(Sent)（对应 shared.inl 的 Send()）
    ///
    /// ★ 这里【不再】碰 Want 事件。Want 只在引擎循环里消费一次（见 engine 步骤 2），
    /// 否则两个消费点会互相偷信号，活跃判定就变成竞态。
    // ── 这一小段就是"共享内存 + 门铃"通信的全貌 ─────────────────────────
    // 1. WaitForSingleObject(h_mutex, INFINITE)：拿到排队锁。跨进程安全的 —— Mutex 是内核对象，
    //    别的进程（filter）用同一把锁读同一块内存，所以我们写完之前它不会读到半成品。
    //    INFINITE（无限等待）在这里可接受：临界区只是几 MB 的 memcpy，持锁期极短；
    //    如果以后要在锁里加解码/网络之类耗时活，就必须换成带超时的等待。
    // 2. write_header_and_data：填协议头 + 像素（可能因参数非法返回 false）。
    // 3. ReleaseMutex(h_mutex)：交锁。【必须成对】—— 互斥锁是计数式资源，拿了不还就等于
    //    把门铃线永久占住，filter 下一次读帧会永远阻塞 → 摄像头全线卡死。
    //    Windows 规定只有持有者可释放（重复 Release 会失败），而且这里的写法是
    //    "无论写成功与否都 Release"，所以 false 分支也不会漏解锁（这点很重要）。
    // 4. SetEvent(h_sent) = 按"我有一帧了"的门铃；SetEvent 永不阻塞，所以按铃不拖慢我们。
    // ⚠ 注意（可疑点，未改动）：第 1 步的返回值被丢弃了。WaitForSingleObject 可能返回
    //    WAIT_FAILED（句柄失效）或 WAIT_ABANDONED（上一个持有者进程崩了，内核把锁交给我们）。
    //    目前代码不做区分，最坏情况是：锁其实没拿到却照样写内存（有数据竞争风险）。
    //    更稳的写法是判返回值、遇 ABANDONED 就重连；本次只标注，代码保持原样。
    unsafe fn send_frame(s: &Sender, w: i32, h: i32, rgba: &[u8]) -> bool {
        WaitForSingleObject(s.h_mutex, INFINITE);
        let ok = write_header_and_data(s, w, h, rgba);
        let _ = ReleaseMutex(s.h_mutex);
        // if ok { ... }：Rust 的 if 条件必须是 bool 表达式（没有"非 0 即真"），
        //   这里 ok 正是前面那个函数的 bool 结果 —— 写失败就【不】按门铃，
        //   让 filter 继续用上一帧（宁可画面旧一点，也不给半帧脏数据）。
        if ok {
            let _ = SetEvent(s.h_sent);
        }
        // 函数体最后一行不写分号 = 本函数的返回值（Rust 表达式语言的默认返回方式）。
        ok
    }

    /// 挂一张黑底占位图（头部字段走 write_header，与真实帧完全一致），
    /// 让摄像头应用打开后不会看到随机内存垃圾。
    // 为什么全 0 内存 = 黑画面：RGBA 四个字节都是 0 → R=G=B=0 就是纯黑；A=0 严格说是全透明，
    //   但消费端会把它当不透明处理，实际看到的仍是黑场（真实帧走 black_rgba 会把 A 写成 255）。
    // 关键点在于【header 必须与真实帧同一套写法】：width/height/stride/format/resizemode 若和
    //   像素区不一致，filter 会算错行长 —— 于是"占位图"反而变成满屏花屏。
    // 这一张图只在刚挂上共享内存时写一次（engine 步骤 1），不是每帧都写，所以读配置也不心疼。
    unsafe fn write_placeholder(s: &Sender) {
        // v3.8：占位帧尺寸来自 config.json 的 camera.placeholder_width/height
        // （默认仍是 640x480）。这个函数只在"刚挂上共享内存"时调用，不是每帧，
        // 所以在这里现读一次配置没有代价。
        let cfg = crate::config::get();
        // 元组解构：一次把宽高分开取好，顺便 as i32（协议里 w/h 是有符号 int，
        //   而 Rust 配置存的是无符号 —— 类型不同就必须显式转换，Rust 不隐式转）。
        let (pw, ph) = (
            cfg.camera.placeholder_width as i32,
            cfg.camera.placeholder_height as i32,
        );
        // 和真实帧同一条纪律：拿锁 → 写 → 交锁 → 按铃，四步一步都不能少。
        WaitForSingleObject(s.h_mutex, INFINITE);
        write_header(s, pw, ph);
        // 数据区清零（640*480*4 ≈ 1.2MB，只在挂上时做一次）
        // write_bytes(目标, 填充字节, 个数) = C 的 memset。偏移同样是 32（跳过协议头）。
        // ⚠ 注意（既有假设，未改动）：这里只保证 pw*ph*4 ≤ MAX_SHARED_IMAGE_SIZE（常量成立即可），
        //   没有像 write_header_and_data 那样做边界检查；把配置里的占位分辨率填成超大值会越界写。
        //   默认 640×480 安全；如果允许用户自由填，就该把这条也走一遍同一套校验。
        std::ptr::write_bytes(s.view.add(32), 0, (pw as usize) * (ph as usize) * 4);
        let _ = ReleaseMutex(s.h_mutex);
        let _ = SetEvent(s.h_sent);
    }

    /// 纯黑一帧（与 OBS 通道同款隐私处理：手机停推后覆盖旧画面）
    // 为什么要主动覆盖黑帧：不覆盖的话，filter 会把手机停推前的【最后一帧】一直显示下去，
    //   等于"视频通话里挂着一张我的过期照片"（隐私问题）；再等 timeout 到点，驱动还会画
    //   彩色错误条纹（看起来像坏了）。所以这里用一帧干净的黑色把画面"盖住"。
    fn black_rgba(w: i32, h: i32) -> Vec<u8> {
        // vec![0u8; n] = 长度 n、初值全 0 的 Vec（一帧大小的像素缓冲，960×720 约 2.7MB）。
        let mut v = vec![0u8; (w as usize) * (h as usize) * 4];
        // RGB=0 是黑；A 填 255，避免 32 位 ARGB 应用把整帧当透明
        // chunks_exact_mut(4)：按 4 字节切成"一个像素"一组（尾部不足 4 字节不会出现在这里，
        //   因为长度本身就是 4 的倍数）；_exact_ 版本给出的是【确定长度 4】的切片，
        //   于是 px[3] 永远合法，不用判边界 —— 这是 Rust 用类型消灭越界的典型小例子。
        for px in v.chunks_exact_mut(4) {
            px[3] = 255;
        }
        v
    }

    /// JPEG 字节 → RGBA8 缓冲，返回 (像素数据, 宽, 高)
    /// jpeg-decoder 0.3 默认把 YCbCr 转成 RGB24；灰度图输出 L8。
    /// 这里按输出长度判断通道数，统一扩成 RGBA（A 恒为 255）。
    // ── 字节序：RGBA 还是 BGRA？（颜色错位的唯一来源）────────────────────
    // 名字指的是【内存里字节的先后】，不是"数值大小"：一个像素按 R,G,B,A 四个字节紧挨着存
    //   就叫 RGBA8。但把同一串字节当成一个 32 位小端整数读时，低位在前，看起来就成了 0xAABBGGRR，
    //   于是很多系统（DirectShow 的 32bit DIB、GDI 的 BI_RGB）管这种内存布局叫 BGRA ——
    //   差别只在"你按什么顺序解释这四个字节"，所以跨模块传图必须先讲清楚是哪一种。
    // 本引擎按 shared.inl 的 FORMAT_UINT8 约定交【RGBA】（内存顺序 R,G,B,A），
    //   转成消费端要的 BGR(A) 由 filter 负责（文件头已注明）。
    // ⚠ 如果哪天画面变成"红蓝互换"（人脸发蓝、天空发红），第一嫌疑就是这两侧对字节序的理解
    //   错位了：要么 format 填错，要么有人在中间多做了一次 R/B 交换。改一处就能修好，别乱试。
    // 性能提醒：下面两条分支都在做逐像素的搬运循环（960×720 约 69 万次），
    //   但这段跑在引擎线程（非实时回调）上，8ms 一轮的节奏足够宽裕，可读性优先。
    fn decode_jpeg_to_rgba(jpeg: &[u8]) -> anyhow::Result<(Vec<u8>, i32, i32)> {
        // Context trait 给 Result 加 .context("...")：把 Err 换成带一句人话说明的错误，
        //   这样 warn! 打出来的日志能看出"是没解出图像帧"，不只是一个裸错误码。
        use anyhow::Context;
        let mut decoder = jpeg_decoder::Decoder::new(jpeg);
        // decode() 返回 Result<Vec<u8>>，? = 失败立刻把错误上抛给调用方（engine 的 Err 分支）。
        //   decoder 之所以要 mut：decode 会往里写解码中间状态（Rust 要求改动就必须声明可变）。
        let data = decoder.decode()?;
        // info() 返回 Option<ImageInfo>：.context() 把 None 也转成错误（附带下面那句话），
        //   于是又能继续用 ? 上抛 —— Option 和 Result 之间最常用的桥。
        let info = decoder.info().context("jpeg 流里没有图像帧")?;
        let (w, h) = (info.width as i32, info.height as i32);
        let npx = (w as usize) * (h as usize);
        // ensure! = "条件不成立就抛这个错误"（anyhow 提供的宏，省去手写 if + bail!）。
        anyhow::ensure!(npx > 0, "空图像 {w}x{h}");

        // RGBA 目的缓冲先全 0：下面 RGB24 分支会覆盖前 4 字节里的 3 个，A 单独写 255。
        let mut rgba = vec![0u8; npx * 4];
        if data.len() == npx * 3 {
            // RGB24 → RGBA8（每 3 字节补一个 255）
            // 用【输出长度】判断通道数而不是让库告诉我们是彩色/灰度：jpeg-decoder 0.3 的
            //   ImageInfo 里没有直接的"每像素字节数"字段，长度比较反而是最不会骗人的依据。
            // chunks_exact(3) 每次给一个像素的 3 个字节；enumerate() 再给一个序号 i（像素下标）。
            //   偏移 o = i * 4 是"第 i 个像素在 RGBA 缓冲里的起始字节"。
            for (i, px) in data.chunks_exact(3).enumerate() {
                let o = i * 4;
                rgba[o] = px[0];
                rgba[o + 1] = px[1];
                rgba[o + 2] = px[2];
                rgba[o + 3] = 255;
            }
        } else if data.len() == npx {
            // L8 灰度 → RGBA8（三通道同值）
            // 灰度 JPEG 每像素 1 字节。R=G=B=同一个灰度值就是"没有颜色的彩色图"。
            for (i, &g) in data.iter().enumerate() {
                let o = i * 4;
                rgba[o] = g;
                rgba[o + 1] = g;
                rgba[o + 2] = g;
                rgba[o + 3] = 255;
            }
        } else {
            // 兜底：长度既不是 npx*3 也不是 npx，说明遇到了没预料到的输出格式，
            //   明确报错而不是硬猜（{:?}/数字塞进消息里方便日后按日志加一条分支）。
            anyhow::bail!("未知 JPEG 输出格式: {} 字节 / {w}x{h}", data.len());
        }
        // 元组返回：调用方 engine 里 match Ok((rgba, w, h)) 一次解构三个值。
        Ok((rgba, w, h))
    }

    /// 引擎主循环：打开驱动共享内存 → 循环"取帧、解码、上传、探测活跃"
    ///
    /// Want 事件是**真实可靠**的活跃信号：filter 由应用进程加载，应用调用
    /// Start() 后每次取帧都会 SetEvent(h_want)；应用关闭摄像头 → filter 停止
    /// 索要 → Want 不再触发 → 1 秒内判为释放。所以这里保留上报（v3.4.9）。
    // ── 这个循环的节奏与它的六个魔数（改之前先看这里）───────────────────
    // 循环周期 8ms（→ 上限约 125 轮/秒）：够跑 60fps，又不像"完全不睡"那样烧核。
    //   改小（1ms）：CPU 白耗、解码次数不变（帧没那么快来）；改大（50ms）：60fps 直接变 20fps。
    // 挂接重试 500ms：驱动没装/没跑起来时的探测频率；更短没意义（filter 启动本身要秒级）。
    // 未挂接时的 200ms 睡眠：这时什么也干不了，纯省钱；它【不】影响挂上后的帧率。
    // Want 新鲜度 1000ms：判"有应用在观看"的窗口。改短（200ms）→ 网络/调度一抖就误判成释放，
    //   手机会关掉相机（画面闪断）；改长（5s）→ 应用都关了还占着"使用中"，手机相机迟迟不睡。
    // blackout_after（默认 1500ms）：手机停推多久之后覆盖黑帧。太短 → 手机卡顿一下画面就黑；
    //   太长 → 过期画面挂得久（隐私）。它和协议头里的 timeout=1000ms 是【两回事】：
    //   后者是"filter 允许连续错过多少帧才画错误条纹"（见文件头说明 3），前者是我们的隐私动作；
    //   实际不会撞上，因为无新帧时我们仍会立刻应答 Sent（见下面 None 分支）。
    // 统计打印 2 秒：只影响日志密度，不影响行为。
    pub fn engine(mailbox: FrameMailbox, tx: tokio::sync::mpsc::UnboundedSender<bool>) {
        info!("[Vcam] Unity Capture injection engine started");
        // v3.8：无信号黑帧延时来自 config.json 的 camera.blackout_after_ms（默认 1500ms）。
        // 只在这里读一次：主循环 8ms 一轮，每轮去取配置是白干活。
        // Duration::from_millis(n) = 一段时间（n 毫秒）。它和 Instant（时刻）是两种类型：
        //   时刻 - 时刻 = 时长，时刻 + 时长 = 时刻，时长之间不能相减 —— 类型帮你不误用。
        // Unity 通道这一路【不】用 camera.width/height —— 它的头部尺寸永远跟随手机
        // 真实推流分辨率（历史上写死过尺寸，结果是"花屏、没有视频"，见文件顶部说明）。
        let blackout_after = Duration::from_millis(crate::config::get().camera.blackout_after_ms);
        // 增量挂接状态：句柄跨重试累积，挂上后一直持有（与原版 C++ 行为一致）
        let mut sender = Sender::empty();
        let mut attached = false;
        let mut last_try = Instant::now() - Duration::from_secs(1);
        let mut last_want = Instant::now() - Duration::from_secs(10);
        let mut active = false;
        let mut frames_sent: u64 = 0;
        // ── 隐私/防花屏：手机停推超 1.5 秒就覆盖黑帧 ──
        // 不做这件事的话，filter 会把最后一帧一直显示下去（"视频通话显示过期照片"），
        // 而且 timeout 到期后还会切成彩色错误条纹。
        let mut last_frame_at: Option<Instant> = None;
        let mut last_dims = (0i32, 0i32);
        let mut blanked = true;
        // 诊断用：每 2 秒清零一次的索帧计数 + 上次打印统计的时刻
        let mut wants: u64 = 0;
        let mut stats_at = Instant::now();

        // loop 是永不退出的循环（除非 panic 或进程结束）：本引擎是"常驻服务"，没有正常退出路径。
        loop {
            // —— 1. 确保共享对象已挂上（驱动没装/没跑时每 500ms 增量重试）——
            // elapsed() 返回"距上次时刻过了多久"（Duration），和 500ms 比较就是简易计时器：
            //   不引入任何定时器/线程，只用"上一次动手的时刻 + 间隔要求"来控制频率。
            // && 短路：attached 已经为 true 时后面整段（包括 elapsed() 调用）都不会执行，
            //   所以"挂上之后"每轮在这一个 if 上就过去了，成本几乎为零。
            if !attached && last_try.elapsed() >= Duration::from_millis(500) {
                last_try = Instant::now();
                attached = unsafe { try_attach(&mut sender) };
                if attached {
                    info!("[Vcam] Unity Capture shared memory attached");
                    // 刚挂上就先糊一张黑底占位图：此刻数据区是驱动分配的【未清零内存】，
                    //   filter 若马上读一帧就是"随机彩色雪花"（用户会以为程序坏了）。
                    unsafe { write_placeholder(&sender) };
                }
            }
            // let s = &sender：从这里开始全部用只读借用 s，避免后面每处都写 sender 且
            //   保证不会有人偷偷改状态（借用检查器会在编译期拦住 &mut sender 并存的情况）。
            let s = &sender;
            if !attached {
                // 驱动未就绪：状态强制 inactive，稍后再试
                // 只在 active 为 true（也就是"上次上报过 true"）时才补发 false ——
                //   边沿触发：绝不重复发消息，也不漏掉"从有到无"这一次。
                // tx.send(false).ok()：接收端（server.rs 的任务）可能已消失，这里 .ok() 忽略。
                if active {
                    active = false;
                    tx.send(false).ok();
                }
                // 200ms：没挂上时每轮干等这么久。这里 sleep 完全无害（本线程不干别的），
                //   不 sleep 就会 100% 占一个核空转 —— 和 mic_out.rs 的 v3.7 CPU 修复同一个道理。
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }

            // —— 2. 探测 filter 的索帧请求 ——
            // ★ 这里是整个循环【唯一】消费 Want 的地方（v3.4.11 修正）。
            // 以前 send_frame 里也消费一次、活跃检测又消费一次，两个消费点互相
            // 抢信号 → "有没有应用在观看"变成拼运气的竞态判定。
            // WaitForSingleObject(h_want, 0)：超时 0 = 【只问不等的探测】，立刻返回。
            //   返回值 == WAIT_OBJECT_0 就说明"这一轮里驱动确实按过门铃"（auto-reset 顺手把铃复位）。
            //   为什么绝不传 INFINITE：那样循环会被"没有应用要看"彻底冻住，黑帧/统计/状态上报
            //   全都不做了 —— 这个循环必须每 8ms 自己走一遍，所以所有等待都必须带超时。
            let wanted = unsafe { WaitForSingleObject(s.h_want, 0) == WAIT_OBJECT_0 };
            if wanted {
                // last_want 就是"最后一次被索要"的时刻，第 5 步靠它的年龄判断活跃与否。
                // wants 是本轮计数（每 2 秒打印一次后清零），只用于日志诊断。
                last_want = Instant::now();
                wants += 1;
            }

            // —— 3. 取最新帧并解码上传 ——
            // mailbox.lock().unwrap().take() 三步一口气做完：
            //   lock() 拿到互斥锁的智能指针（MutexGuard，出了这条语句的临时作用域就自动解锁，
            //     这是 RAII：不需要手写 unlock，也就不会忘记）；
            //   unwrap() 把 LockResult 拆成真正的锁 —— ⚠ 注意：如果有别的线程在持锁时 panic，
            //     锁会"中毒"，这里 unwrap() 就直接 panic（本引擎线程挂掉 = 虚拟相机从此黑屏）。
            //     全项目都这么写，属于已知取舍，见 mic_out.rs 里同样的说明。
            //   take() 把 Option<Vec<u8>> 变成 None 并把旧的 Some(jpeg) 挪出来给我们（不复制字节）。
            // 注意是"取最新一帧、丢掉中间所有帧"：直播场景宁可丢帧也不能让画面越来越延迟。
            // match frame：Rust 的 match 必须覆盖所有分支（Option 只有 Some/None 两种），
            //   编译器替你保证"忘了处理某一种情况"根本编译不过去。
            // 嵌套 match decode_jpeg_to_rgba(...)：Ok/Err 再各自处理，Result 就是"带错误信息的 Option"。
            let frame = mailbox.lock().unwrap().take();
            match frame {
                Some(jpeg) => match decode_jpeg_to_rgba(&jpeg) {
                    Ok((rgba, w, h)) => {
                        // 本函数内 send_frame 只有两处调用（这里的真实帧、第 4 步的黑帧），
                        //   签名是 (s, w, h, &像素)：s 是只读借用的 Sender，
                        //   w/h 必须与像素缓冲的宽高一致（否则步长算错 → 画面斜切/花屏）。
                        // send_frame 返回 bool（true = 驱动接下了这一帧）。
                        // ⚠ 注意：宽高和像素缓冲的一致性编译期查不出来，只能靠 write_header_and_data
                        //   里那句 rgba.len() < data_size 在运行期兜着（不合格就丢这一帧，不会崩）。
                        //   也就是说"改解码输出的布局"必须同步改这里的 w/h，否则表现为画面错位。
                        if unsafe { send_frame(s, w, h, &rgba) } {
                            frames_sent += 1;
                            // last_dims 记住"共享内存里现在是多大尺寸"：第 4 步写黑帧要用同一个
                            //   宽高（尺寸不一致就等于偷偷改了协议头，filter 会按旧尺寸解读新数据 → 花屏）。
                            last_dims = (w, h);
                            // last_frame_at = Some(...)：把"手机最后一次真的推进画面"的时刻记下来。
                            //   类型是 Option<Instant> —— None 表示"开播以来一帧都没成功过"，
                            //   此时第 4 步的 if let Some(t) 自然不成立，也就不会去写用不到的黑帧。
                            last_frame_at = Some(Instant::now());
                            // blanked = false：现在是"有真实画面"的状态，第 4 步才有资格再次覆盖黑帧。
                            //   它同时是个防重复开关：一旦糊过黑帧就置 true，直到有新帧才放开。
                            blanked = false;
                            if frames_sent == 1 {
                                // 只在第一帧打印：分辨率/步长是排查"画面比例不对、被拉伸"最关键的信息，
                                //   每帧都打会把日志淹掉。stride={w}px 说明我们用的是"像素步长"写法。
                                info!("[Vcam] First frame uploaded to driver: {w}x{h} (stride={w}px, resizemode=LINEAR)");
                            }
                        }
                    }
                    // 解码失败：只打日志、不崩溃，也不上报错误状态 —— 手机侧下一帧大概率就是好的。
                    // {e} 走 Display（人话），如果用 {:?} 会走 Debug（带类型名的机器话）。
                    Err(e) => warn!("[Vcam] JPEG decode failed: {e}"),
                },
                None => {
                    // 没有新帧，但应用确实在等 → 立刻应答 Sent，让它继续用共享内存
                    // 里的上一帧。不应答的话 filter 要白等 200ms 才拿到 OLDFRAME，
                    // 连续几次就撞上"Unity has stopped"彩色条纹。
                    // 这一段是"活锁保险"：帧的到达节奏（手机推流）和取帧节奏（filter 索要）
                    //   本来就不是同一个时钟，必须允许"被要了但没有新货"这种情况存在。
                    // wanted 用的是第 2 步刚探测到的结果 —— 同一轮里只消费一次 Want，
                    //   所以这里的应答不会和活跃检测抢信号（v3.4.11 的修正点）。
                    // let _ = SetEvent(...)：SetEvent 返回 WIN32 布尔，这里明确丢弃（`let _ =`）。
                    //   ⚠ 注意：失败（比如句柄无效）时不会有任何提示，画面表现为"卡住不动"，
                    //   排查时先看第 6 步的统计里 wants 是否还在涨。
                    if wanted {
                        unsafe {
                            let _ = SetEvent(s.h_sent);
                        }
                    }
                }
            }

            // —— 4. 手机停推超过 camera.blackout_after_ms → 覆盖黑帧（隐私：不把最后一帧一直挂着）——
            // 三层 if 都是"守卫式写法"（先确认条件再动手），读起来像嵌套，实际是三种不同问题：
            //   !blanked —— 幂等开关：已经糊过黑帧就别每 8ms 再糊一次（memcpy 几 MB 很贵）。
            //   if let Some(t) = last_frame_at —— Option 的惯用法：只在"曾经成功推过帧"时才有过期可言。
            //     如果 last_frame_at 是 None（从没成功过），这里整段跳过；共享内存里仍是挂上时那张
            //     占位黑图（write_placeholder），所以不会出现"既没画面也没黑底"的空档。
            //   w > 0 && h > 0 —— 防御性：last_dims 初值是 (0,0)，理论上一帧成功后就不是 0，
            //     多判一次的成本为零，换来的是"绝不用 0 尺寸去写头部"。
            if !blanked {
                if let Some(t) = last_frame_at {
                    if t.elapsed() >= blackout_after {
                        // 元组解构 let (w, h) = last_dims：直接拿到上一帧的宽高，
                        //   用【同一尺寸】写黑帧，协议头就不会中途变更（改了尺寸必须整帧一起改）。
                        let (w, h) = last_dims;
                        if w > 0 && h > 0 {
                            // black_rgba 现算一整块纯黑缓冲（w×h×4 字节，一次性的，不是每轮）。
                            let black = black_rgba(w, h);
                            // 走的是和真实帧【完全相同】的 send_frame 通路（含锁、协议头、行序翻转），
                            //   这样黑帧不可能被"用另一种格式写"而留下花屏残影。返回值这里不判：
                            //   ⚠ 注意（既有行为，未改动）：如果黑帧因尺寸非法被拒，blanked 仍会被置 true，
                            //   于是本轮之后不再重试 —— 但既然最后一帧用同一尺寸写成功过，实际不会发生。
                            unsafe { send_frame(s, w, h, &black) };
                            blanked = true;
                            info!("[Vcam] 手机已停止推流 → 写入黑帧覆盖旧画面（应用不再显示过期照片）");
                        }
                    }
                }
            }

            // —— 5. Want 新鲜度 → 活跃状态上报（cam_state 的源头）——
            // 一行看懂："最后一次被索要"距今不足 1 秒 → 认为有应用正在看这路相机。
            //   这个 bool 就是 WebSocket 协议里 cam_state 的取值（main.rs 收到后刷新界面并转发手机）。
            // 为什么用"索帧事件的年龄"而不是去枚举占用进程（音频侧用的是那套）：
            //   UnityCapture 的 filter 在应用进程内，应用调用 Start() 后每取一帧都会 SetEvent(Want)，
            //   这个门铃本身就是最可靠的占用信号 —— 不用驱动版本、不用管理员权限。
            // 1000ms 是本文件"六个魔数"里的一个（详见 engine 顶部注释），改它要同时想到：
            //   改短 → 一次调度抖动就误判"释放"，手机相机被关掉（画面闪断）；
            //   改长 → 应用都关了还对外宣称"使用中"，手机相机迟迟不睡（耗电 + 隐私观感差）。
            let now_active = last_want.elapsed() < Duration::from_millis(1000);
            // 边沿触发：只在【真的变化】时才上报，避免每 8ms 给手机灌一条重复消息。
            //   Rust 的 bool 可以直接 != 比较（没有"整数当布尔"的写法）。
            if now_active != active {
                active = now_active;
                // 注意 log 宏的调用形态：info!(格式串, 参数) 或 info!("...{}", x)。
                //   这里第一个参数是带 {} 占位的字符串，第二个参数是个 if/else 表达式 ——
                //   if 在 Rust 里是【表达式】，有值（两个分支都给 &str，类型一致才能当值用）。
                //   日志输出到哪里：见 mic_out.rs 顶部说明（DualLogger → stderr + audioserver.log）。
                info!(
                    "[Vcam] Camera {}",
                    if active { "is being watched by an app" } else { "released" }
                );
                // tx.send(active).ok()：把 bool 塞给 tokio 的无界通道，由 server.rs 侧的任务
                //   负责打包成 cam_state 帧发给手机。.ok() 把 Result 换成 Option 再丢弃 ——
                //   接收端可能已经随连接断开而消失，这里不是错误（本线程要继续活着）。
                //   ⚠ 注意：无界通道意味着"读得慢就一直堆"，但因为只在变化时发，实际最多堆几条。
                tx.send(active).ok();
            }

            // —— 6. 每 2 秒打印一次吞吐，排查"有画面吗/谁在拉流"用 ——
            // && 短路：没人在看（active == false）时直接跳过，连 elapsed() 都不调用。
            // 这三个计数合起来能回答最常见的两个问题：
            //   wants 涨、frames_sent 不涨 → 驱动在索要但手机没推流（或解码一直失败）；
            //   wants 不涨 → 根本没有应用打开这路相机（和步骤 5 的 released 日志互相印证）。
            if active && stats_at.elapsed() >= Duration::from_secs(2) {
                // 先刷新时刻再打印：这样"下一次满 2 秒"是从现在起算，间隔稳定。
                stats_at = Instant::now();
                let (w, h) = last_dims;
                info!(
                    "[Vcam] 2s 统计：filter 索帧 {wants} 次 / 已上传 {frames_sent} 帧 / 共享内存 {w}x{h}"
                );
                // 只清零"索帧次数"（它本来就是每 2 秒窗口的计数），
                //   frames_sent 是全生命周期累计，用来确认"第一帧之后到底在不在动"。
                wants = 0;
            }

            // 循环收尾：睡 8ms（→ 上限约 125 轮/秒）。三个理由：
            //   1) 没帧没事件时不让一个 CPU 核被空转吃满（和 mic_out.rs 的按需开关是同一套思路）；
            //   2) 60fps 需要 16.7ms 一帧，8ms 的轮询粒度足够快（最坏多等 8ms）；
            //   3) 这里的等待只能是"定长 sleep"，不能换成 WaitForSingleObject(INFINITE)：
            //      本循环同时要干黑帧、活跃检测、统计三件事，被一个事件冻住就全停了。
            // std::thread::sleep 只阻塞【本线程】—— 这个引擎跑在 spawn 出来的独立 OS 线程里，
            //   不会卡住 tokio 运行时，也不会卡住界面（见文件顶部说明 1）。
            std::thread::sleep(Duration::from_millis(8)); // 上限 ~125 次循环/秒，够 60fps
        }
    }
}
