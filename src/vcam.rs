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
//   - 共享内存头布局（共 32 字节，图像数据从偏移 32 开始）：
//       [0] maxSize  [4] width  [8] height  [12] stride(像素)
//       [16] format  [20] resizemode  [24] mirrormode  [28] timeout
//   - format=0 (FORMAT_UINT8)：每像素 4 字节，RGBA 顺序
//
// 设计要点（和 v3 麦克风引擎同款思路）：
//   1. 引擎线程随服务器启动，常开；驱动没装好时静默重试，不影响其他功能。
//   2. 帧先进"邮箱"（只保留最新一帧，旧帧直接丢弃保实时性）。
//   3. 通过 Want 事件的"新鲜度"判断是否有应用正在取流：
//      最近 1 秒内收到过 Want → 摄像头正被使用（active=true）。
//      服务器据此向手机推 cam_state，手机决定开/关相机硬件
//      （隐私模型对齐 v3.3 按需麦克风：没人用 = 硬件彻底关闭）。

use log::info;
use std::sync::{Arc, Mutex};

/// 最新帧邮箱：手机 WS 读循环写入 JPEG 字节，引擎线程取走并解码。
/// 只存一帧——网络快于显示时自动丢旧帧，永远保持实时。
pub type FrameMailbox = Arc<Mutex<Option<Vec<u8>>>>;

pub fn new_mailbox() -> FrameMailbox {
    Arc::new(Mutex::new(None))
}

/// 手机推来一帧 JPEG（直接换进邮箱，替换掉还没消费的旧帧）
pub fn push_frame(mailbox: &FrameMailbox, jpeg: Vec<u8>) {
    *mailbox.lock().unwrap() = Some(jpeg);
}

/// 启动注入引擎线程（仅 Windows；其他平台是空 stub）。
/// tx 在"是否有应用正在观看摄像头"状态翻转时回传（true=正在被取流）。
#[cfg(windows)]
pub fn spawn_vcam(mailbox: FrameMailbox, tx: tokio::sync::mpsc::UnboundedSender<bool>) {
    std::thread::spawn(move || windows_impl::engine(mailbox, tx));
}

/// 非 Windows stub：什么都不做（Unity Capture 是 Windows 专属驱动）
#[cfg(not(windows))]
pub fn spawn_vcam(_mailbox: FrameMailbox, _tx: tokio::sync::mpsc::UnboundedSender<bool>) {}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use log::warn;
    use std::time::{Duration, Instant};
    use windows::core::PCSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Memory::{
        MapViewOfFile, OpenFileMappingA, UnmapViewOfFile,
        MEMORY_MAPPED_VIEW_ADDRESS, FILE_MAP_WRITE,
    };
    use windows::Win32::System::Threading::{
        CreateEventA, OpenEventA, ReleaseMutex, SetEvent, WaitForSingleObject,
        EVENT_MODIFY_STATE, INFINITE,
    };
    use windows::Win32::System::WindowsProgramming::OpenMutexA;

    // OpenMutexA 收裸 u32：SYNCHRONIZE = 0x0010_0000
    const SYNCHRONIZE_U32: u32 = 0x0010_0000;

    // 同步对象名字（cap 0：名称以 '\0' 结尾，与 shared.inl 完全一致）
    const NAME_MUTEX: &str = "UnityCapture_Mutx";
    const NAME_WANT: &str = "UnityCapture_Want";
    const NAME_SENT: &str = "UnityCapture_Sent";
    const NAME_DATA: &str = "UnityCapture_Data";

    /// 共享内存里的最大图像字节数（4K RGBA 16bit，与 shared.inl 的
    /// MAX_SHARED_IMAGE_SIZE = 3840*2160*4*2 一致）
    const MAX_SHARED_IMAGE_SIZE: usize = 3840 * 2160 * 4 * 2;

    /// 引擎持有的四个内核对象 + 映射视图指针。
    /// ★ 与原版 C++ SharedImageMemory 一致：这是"增量累积"的状态结构——
    /// 每次重试只补做缺失的一步，已拿到的句柄跨重试保留、绝不提前关闭。
    /// 原因：Want 事件由发送端创建、filter 用 OpenEvent 找它；若发送端在
    /// 打开 Sent/映射失败时把 Want 一起关掉销毁，filter 就永远打不开 Want，
    /// 双方互相等对方 → 握手死锁（v3.4 联调时实测踩中）。
    struct Sender {
        h_mutex: HANDLE,
        h_want: HANDLE,
        h_sent: HANDLE,
        h_map: HANDLE,
        view: *mut u8,
    }

    // 线程间只有引擎线程自己读写 Sender，邮箱帧用 Arc<Mutex> 传递。
    unsafe impl Send for Sender {}

    impl Drop for Sender {
        fn drop(&mut self) {
            unsafe {
                if !self.view.is_null() {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.view as *mut core::ffi::c_void,
                    });
                }
                for h in [self.h_mutex, self.h_want, self.h_sent, self.h_map] {
                    if !h.is_invalid() {
                        let _ = CloseHandle(h);
                    }
                }
            }
        }
    }

    /// 把 Rust 字符串转成 C 风格 UTF-8 字节串（末尾补 '\0'，供 PCSTR 用）
    fn cstr(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    impl Sender {
        /// 全空状态：所有句柄 NULL、视图未映射
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
    unsafe fn try_attach(p: &mut Sender) -> bool {
        // 1) 互斥体：由驱动（filter）创建，我们只能打开
        if p.h_mutex.is_invalid() {
            let name = cstr(NAME_MUTEX);
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
            if let Ok(h) = CreateEventA(None, false, false, PCSTR(name.as_ptr())) {
                p.h_want = h;
            }
        }

        // 3) Sent 事件：由驱动创建，我们只需要"置信号"权限。
        //    打开失败 = filter 还没走到创建这一步，保留 mutex+want 等下轮。
        if p.h_sent.is_invalid() {
            let name = cstr(NAME_SENT);
            match OpenEventA(EVENT_MODIFY_STATE, false, PCSTR(name.as_ptr())) {
                Ok(h) => p.h_sent = h,
                Err(_) => return false,
            }
        }

        // 4) 图像共享内存：由驱动创建，我们写入
        if p.h_map.is_invalid() {
            let name = cstr(NAME_DATA);
            match OpenFileMappingA(FILE_MAP_WRITE.0, false, PCSTR(name.as_ptr())) {
                Ok(h) => p.h_map = h,
                Err(_) => return false,
            }
        }
        if p.view.is_null() {
            p.view = MapViewOfFile(p.h_map, FILE_MAP_WRITE, 0, 0, 0).Value as *mut u8;
            if p.view.is_null() {
                return false;
            }
        }
        true
    }

    /// 在互斥锁保护下写入共享内存头 + RGBA 像素（对应 shared.inl 的 Send 前半段）
    unsafe fn write_header_and_data(
        s: &Sender,
        w: i32,
        h: i32,
        rgba: &[u8],
    ) -> bool {
        let data_size = (w as usize) * (h as usize) * 4;
        if w <= 0 || h <= 0 || data_size > MAX_SHARED_IMAGE_SIZE || rgba.len() < data_size {
            return false;
        }
        let hdr = s.view; // 头部是 8 个 4 字节整数
        std::ptr::write_unaligned(hdr as *mut i32, MAX_SHARED_IMAGE_SIZE as i32);
        std::ptr::write_unaligned(hdr.add(4) as *mut i32, w);
        std::ptr::write_unaligned(hdr.add(8) as *mut i32, h);
        std::ptr::write_unaligned(hdr.add(12) as *mut i32, w * 4); // stride = 每行字节数（RGBA = 宽×4）
        std::ptr::write_unaligned(hdr.add(16) as *mut i32, 0); // format = FORMAT_UINT8（RGBA8）
        std::ptr::write_unaligned(hdr.add(20) as *mut i32, 0); // resizemode
        std::ptr::write_unaligned(hdr.add(24) as *mut i32, 0); // mirrormode
        std::ptr::write_unaligned(hdr.add(28) as *mut i32, 0); // timeout
        // ── 行序翻转（v3.4.1 修复 Unity 通道上下颠倒）──
        // UnityCapture 的共享内存沿用 Unity 纹理约定 = **从下往上**（OpenGL 行序），
        // filter 会把第 0 行原样拷进 bottom-up DIB 的最后一行。
        // 而 jpeg-decoder 输出是**从上往下**的常规图像行序，
        // 直接写入 → 浏览器（MF→FrameServer→DShow 桥）里画面 180° 上下颠倒。
        // 解决：逐行倒序拷贝（OBS 通道是 top-down 约定，那边不动）。
        {
            let row = (w as usize) * 4;
            let dst_base = hdr.add(32);
            for y in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    rgba.as_ptr().add((h as usize - 1 - y) * row),
                    dst_base.add(y * row),
                    row,
                );
            }
        }
        true
    }

    /// 锁互斥体 → 写帧 → 解锁 → SetEvent(Sent)（完整对应 shared.inl 的 Send()）
    unsafe fn send_frame(s: &Sender, w: i32, h: i32, rgba: &[u8]) -> bool {
        WaitForSingleObject(s.h_mutex, INFINITE);
        let ok = write_header_and_data(s, w, h, rgba);
        let _ = ReleaseMutex(s.h_mutex);
        if ok {
            let _ = SetEvent(s.h_sent);
            // 顺手消耗 Want 事件（shared.inl 同款：没消耗说明 filter 还想要更多帧）
            WaitForSingleObject(s.h_want, 0);
        }
        ok
    }

    /// 拿一次互斥锁把 maxSize/width 清零标记"无信号"，并挂一张黑底占位图，
    /// 让摄像头应用打开后不会看到随机内存垃圾。
    unsafe fn write_placeholder(s: &Sender) {
        const PW: i32 = 640;
        const PH: i32 = 480;
        // 纯黑 RGBA 全 0 即可（数据缓冲全零已由首次映射提供，这里只写头部）
        WaitForSingleObject(s.h_mutex, INFINITE);
        let hdr = s.view;
        std::ptr::write_unaligned(hdr as *mut i32, MAX_SHARED_IMAGE_SIZE as i32);
        std::ptr::write_unaligned(hdr.add(4) as *mut i32, PW);
        std::ptr::write_unaligned(hdr.add(8) as *mut i32, PH);
        std::ptr::write_unaligned(hdr.add(12) as *mut i32, PW * 4); // stride = 每行字节数
        std::ptr::write_unaligned(hdr.add(16) as *mut i32, 0);
        std::ptr::write_unaligned(hdr.add(20) as *mut i32, 0);
        std::ptr::write_unaligned(hdr.add(24) as *mut i32, 0);
        std::ptr::write_unaligned(hdr.add(28) as *mut i32, 0);
        // 数据区清零（640*480*4 ≈ 1.2MB，只在打开时做一次）
        std::ptr::write_bytes(hdr.add(32), 0, (PW as usize) * (PH as usize) * 4);
        let _ = ReleaseMutex(s.h_mutex);
        let _ = SetEvent(s.h_sent);
    }

    /// JPEG 字节 → RGBA8 缓冲，返回 (像素数据, 宽, 高)
    /// jpeg-decoder 0.3 默认把 YCbCr 转成 RGB24；灰度图输出 L8。
    /// 这里按输出长度判断通道数，统一扩成 RGBA（A 恒为 255）。
    fn decode_jpeg_to_rgba(jpeg: &[u8]) -> anyhow::Result<(Vec<u8>, i32, i32)> {
        use anyhow::Context;
        let mut decoder = jpeg_decoder::Decoder::new(jpeg);
        let data = decoder.decode()?;
        let info = decoder.info().context("jpeg 流里没有图像帧")?;
        let (w, h) = (info.width as i32, info.height as i32);
        let npx = (w as usize) * (h as usize);
        anyhow::ensure!(npx > 0, "空图像 {w}x{h}");

        let mut rgba = vec![0u8; npx * 4];
        if data.len() == npx * 3 {
            // RGB24 → RGBA8（每 3 字节补一个 255）
            for (i, px) in data.chunks_exact(3).enumerate() {
                let o = i * 4;
                rgba[o] = px[0];
                rgba[o + 1] = px[1];
                rgba[o + 2] = px[2];
                rgba[o + 3] = 255;
            }
        } else if data.len() == npx {
            // L8 灰度 → RGBA8（三通道同值）
            for (i, &g) in data.iter().enumerate() {
                let o = i * 4;
                rgba[o] = g;
                rgba[o + 1] = g;
                rgba[o + 2] = g;
                rgba[o + 3] = 255;
            }
        } else {
            anyhow::bail!("未知 JPEG 输出格式: {} 字节 / {w}x{h}", data.len());
        }
        Ok((rgba, w, h))
    }

    /// 引擎主循环：打开驱动共享内存 → 循环"取帧、解码、上传、探测活跃"
    ///
    /// Want 事件是**真实可靠**的活跃信号：filter 由应用进程加载，应用调用
    /// Start() 后每次取帧都会 SetEvent(h_want)；应用关闭摄像头 → filter 停止
    /// 索要 → Want 不再触发 → 1 秒内判为释放。所以这里保留上报（v3.4.9）。
    pub fn engine(mailbox: FrameMailbox, tx: tokio::sync::mpsc::UnboundedSender<bool>) {
        info!("[Vcam] Unity Capture injection engine started");
        // 增量挂接状态：句柄跨重试累积，挂上后一直持有（与原版 C++ 行为一致）
        let mut sender = Sender::empty();
        let mut attached = false;
        let mut last_try = Instant::now() - Duration::from_secs(1);
        let mut last_want = Instant::now() - Duration::from_secs(10);
        let mut active = false;
        let mut frames_sent: u64 = 0;

        loop {
            // —— 1. 确保共享对象已挂上（驱动没装/没跑时每 500ms 增量重试）——
            if !attached && last_try.elapsed() >= Duration::from_millis(500) {
                last_try = Instant::now();
                attached = unsafe { try_attach(&mut sender) };
                if attached {
                    info!("[Vcam] Unity Capture shared memory attached");
                    unsafe { write_placeholder(&sender) };
                }
            }
            let s = &sender;
            if !attached {
                // 驱动未就绪：状态强制 inactive，稍后再试
                if active {
                    active = false;
                    tx.send(false).ok();
                }
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }

            // —— 2. 取最新帧并解码上传 ——
            let frame = mailbox.lock().unwrap().take();
            if let Some(jpeg) = frame {
                match decode_jpeg_to_rgba(&jpeg) {
                    Ok((rgba, w, h)) => {
                        if unsafe { send_frame(s, w, h, &rgba) } {
                            frames_sent += 1;
                            if frames_sent == 1 {
                                info!("[Vcam] First frame uploaded to driver: {w}x{h}");
                            }
                        }
                    }
                    Err(e) => warn!("[Vcam] JPEG decode failed: {e}"),
                }
            } else {
                // 没有新帧：仍要回应 filter 的索要请求（重发上一帧会造成画面冻结，
                // 这里只消耗 Want 保持握手活着，filter 超时返回旧帧即可）
                unsafe {
                    if WaitForSingleObject(s.h_want, 0) == WAIT_OBJECT_0 {
                        let _ = SetEvent(s.h_sent); // 告知"没有新帧"，filter 继续用旧帧
                    }
                }
            }

            // —— 3. Want 新鲜度 → 活跃状态上报（cam_state 的源头）——
            // 注意：上面 take() 到帧说明手机在推流；filter 索要帧说明有应用在观看。
            if unsafe { WaitForSingleObject(s.h_want, 0) } == WAIT_OBJECT_0 {
                last_want = Instant::now();
            }
            let now_active = last_want.elapsed() < Duration::from_millis(1000);
            if now_active != active {
                active = now_active;
                info!(
                    "[Vcam] Camera {}",
                    if active { "is being watched by an app" } else { "released" }
                );
                tx.send(active).ok();
            }

            std::thread::sleep(Duration::from_millis(8)); // 上限 ~125 次循环/秒，够 60fps
        }
    }
}
