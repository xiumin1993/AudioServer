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

use log::info;

/// 手机推来一帧 JPEG（邮箱类型/语义与 Unity 通道完全一致，直接复用）
pub fn push_frame(mailbox: &crate::vcam::FrameMailbox, jpeg: Vec<u8>) {
    crate::vcam::push_frame(mailbox, jpeg);
}

/// 启动 OBS 通道注入引擎线程（仅 Windows；其他平台是空 stub）。
/// 帧邮箱与 Unity 通道共用同一类型：手机 WS 读循环写入 JPEG，本引擎取走转码写入。
#[cfg(windows)]
pub fn spawn_vcam_obs(
    mailbox: crate::vcam::FrameMailbox,
    tx: tokio::sync::mpsc::UnboundedSender<bool>,
) {
    std::thread::spawn(move || windows_impl::engine(mailbox, tx));
}

/// 非 Windows stub（OBS Virtual Camera 是 Windows 专属驱动）
#[cfg(not(windows))]
pub fn spawn_vcam_obs(
    _mailbox: crate::vcam::FrameMailbox,
    _tx: tokio::sync::mpsc::UnboundedSender<bool>,
) {
}

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
    const VIDEO_NAME: &str = "OBSVirtualCamVideo";

    /// queue_header 的 sizeof（3×u32 + offsets[3] + type + cx + cy + pad + u64 + reserved[8]）
    const HEADER_SIZE: usize = 80;
    /// 每个帧槽前面的小头部字节数（前 8 字节放时间戳）
    const FRAME_HEADER_SIZE: usize = 32;

    // state 枚举值（对应 enum queue_state）
    const STATE_STARTING: u32 = 1;
    const STATE_READY: u32 = 2;
    const STATE_STOPPING: u32 = 3;

    /// 把 Rust 字符串转成 Windows 宽字符 C 串（末尾补 \0）
    fn wstr(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[inline]
    fn align32(v: usize) -> usize {
        (v + 31) & !31
    }

    /// 复刻 video_queue_create 的布局计算：返回 (总大小, 三个槽的偏移)
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
    struct ObsQueue {
        h_map: HANDLE,
        view: *mut u8,
        cx: u32,
        cy: u32,
        offsets: [usize; 3],
    }

    // 只有引擎线程自己读写 ObsQueue，跨不过去也不需要锁
    unsafe impl Send for ObsQueue {}

    impl Drop for ObsQueue {
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
    unsafe fn queue_create(cx: u32, cy: u32, fps: u32) -> Option<ObsQueue> {
        let name = wstr(VIDEO_NAME);
        // 先探测是否已存在
        let existing = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(name.as_ptr()));
        if let Ok(h) = existing {
            // 映射已存在（可能是上次会话残留，消费者还持有）→ 接管它
            info!("[VcamObs] Mapping already exists, taking over (old session residue)");
            return queue_open_existing(h, cx, cy, fps);
        }

        let (size, offsets) = layout(cx, cy);
        let interval = (10_000_000u64 / (fps.max(1) as u64)).max(1); // 每帧 100ns 数

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
        if GetLastError() == ERROR_ALREADY_EXISTS {
            info!("[VcamObs] Mapping created but already exists, taking over");
            return queue_open_existing(h_map, cx, cy, fps);
        }
        let view = MapViewOfFile(h_map, FILE_MAP_ALL_ACCESS, 0, 0, 0).Value as *mut u8;
        if view.is_null() {
            let _ = CloseHandle(h_map);
            return None;
        }

        // 整块清零后写头部（等价 C 侧 struct header = {0} + memcpy）
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
    fn rgb_to_nv12(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
        let mut out = vec![0u8; w * h * 3 / 2];
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
    fn black_nv12(w: u32, h: u32) -> Vec<u8> {
        let n = (w as usize) * (h as usize);
        let mut out = vec![128u8; n * 3 / 2];
        for y in out.iter_mut().take(n) {
            *y = 16;
        }
        out
    }

    /// 当前时间 → 100ns 单位时间戳（DShow REFERENCE_TIME 语义）
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
            let frame = mailbox.lock().unwrap().take();
            if let Some(jpeg) = frame {
                match decode_jpeg_to_rgb(&jpeg) {
                    Ok((rgb, w, h)) => {
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
