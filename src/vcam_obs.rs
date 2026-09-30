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
// "有没有应用正在看摄像头"的检测：OBS 协议里没有 Unity 那样的 Want 事件，
//   改用 Windows 隐私监控注册表 CapabilityAccessManager：
//   这台电脑没有物理摄像头，所以 webcam 节点下任何进程条目
//   LastUsedTimeStop == 0（正在访问）→ 一定是有应用打开了虚拟摄像头
//   → 服务器据此唤醒手机开相机；应用关闭 → 立刻回待命（隐私语义不变）。

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
        MEMORY_MAPPED_VIEW_ADDRESS, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, PAGE_READWRITE,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, REG_QWORD,
    };

    /// 与 OBS 源码一致的段名（UTF-16 命名共享内存）
    const VIDEO_NAME: &str = "OBSVirtualCamVideo";

    /// 隐私监控注册表：摄像头（webcam）访问记录根节点
    const CONSENT_PATH: &str =
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\webcam";

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
    /// 返回 None = 已被占用（OBS 本体在跑）或系统拒绝，稍后由引擎重试。
    unsafe fn queue_create(cx: u32, cy: u32, fps: u32) -> Option<ObsQueue> {
        let name = wstr(VIDEO_NAME);
        // 先探测是否已存在：OBS 源码同款防冲突（fail if already in use）
        let existing = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr()));
        if let Ok(h) = existing {
            let _ = CloseHandle(h);
            return None;
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
        // 名字竞争：另一进程刚好抢先建了同名段 → 放弃本轮
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(h_map);
            return None;
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

    // ── 隐私注册表：判断"有没有应用正在使用摄像头" ──────────────

    /// 读一个应用条目的 LastUsedTimeStop（REG_QWORD）。0 = 正在访问中
    unsafe fn key_is_active(h: HKEY) -> bool {
        let name = wstr("LastUsedTimeStop");
        let mut buf: u64 = u64::MAX;
        let mut cb: u32 = 8;
        let mut kind = REG_QWORD;
        let r = RegQueryValueExW(
            h,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(&mut buf as *mut u64 as *mut u8),
            Some(&mut cb),
        );
        r.is_ok() && kind == REG_QWORD && buf == 0
    }

    /// 递归枚举子键（深度限制 2 层：webcam\<应用> 与 webcam\NonPackaged\<exe 路径>）
    unsafe fn scan_subkeys(h_parent: HKEY, depth: u32) -> bool {
        if depth == 0 {
            return false;
        }
        let mut idx = 0u32;
        loop {
            let mut buf = [0u16; 512];
            let st = RegEnumKeyW(h_parent, idx, Some(&mut buf));
            if !st.is_ok() {
                break; // ERROR_NO_MORE_ITEMS 或出错 → 结束
            }
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let sub = String::from_utf16_lossy(&buf[..end]);
            let subw = wstr(&sub);
            let mut h_sub = HKEY(std::ptr::null_mut());
            if RegOpenKeyExW(h_parent, PCWSTR(subw.as_ptr()), 0, KEY_READ, &mut h_sub).is_ok() {
                let hit = key_is_active(h_sub) || scan_subkeys(h_sub, depth - 1);
                let _ = RegCloseKey(h_sub);
                if hit {
                    return true;
                }
            }
            idx += 1;
        }
        false
    }

    /// 摄像头是否正被任何进程访问（这台电脑没有物理摄像头，
    /// 所以答案等价于"有应用正在看我们的虚拟摄像头"）
    unsafe fn webcam_access_active() -> bool {
        let path = wstr(CONSENT_PATH);
        let mut h = HKEY(std::ptr::null_mut());
        if !RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), 0, KEY_READ, &mut h).is_ok() {
            info!("[VcamObs] Failed to open webcam consent registry key");
            return false;
        }
        let hit = scan_subkeys(h, 2);
        let _ = RegCloseKey(h);
        info!("[VcamObs] webcam_access_active() = {}", hit);
        hit
    }

    // ── 引擎主循环 ─────────────────────────────────────────────

    /// 默认占位分辨率：应用先打开、手机还没推流时用它给干净黑帧
    const DEFAULT_W: u32 = 960;
    const DEFAULT_H: u32 = 720;

    pub fn engine(
        mailbox: crate::vcam::FrameMailbox,
        tx: tokio::sync::mpsc::UnboundedSender<bool>,
    ) {
        info!("[VcamObs] OBS Virtual Camera injection engine started");
        let mut q: Option<ObsQueue> = None;
        let mut q_size = (0u32, 0u32);
        let mut last_try = Instant::now() - Duration::from_secs(1);
        let mut last_poll = Instant::now() - Duration::from_secs(1);
        let mut active = false;
        let mut frames_written: u64 = 0;

        loop {
            // —— 1. 每 500ms 查一次隐私注册表 → 活跃状态上报（cam_state 第二信号源）——
            if last_poll.elapsed() >= Duration::from_millis(500) {
                last_poll = Instant::now();
                let now_active = unsafe { webcam_access_active() };
                if now_active != active {
                    active = now_active;
                    info!(
                        "[VcamObs] Camera {} (via OBS vcam registry poll)",
                        if active { "is being watched" } else { "released" }
                    );
                    tx.send(active).ok();
                }
            }

            // —— 2. 取最新帧：解码 → RGB24 → NV12 → 写入 OBS 共享内存 ——
            let frame = mailbox.lock().unwrap().take();
            if let Some(jpeg) = frame {
                match decode_jpeg_to_rgb(&jpeg) {
                    Ok((rgb, w, h)) => {
                        // 映射不存在或分辨率变了 → 重建（500ms 限速重试）
                        if (q.is_none() || q_size != (w, h))
                            && last_try.elapsed() >= Duration::from_millis(500)
                        {
                            last_try = Instant::now();
                            let nq = unsafe { queue_create(w, h, 30) };
                            match nq {
                                Some(nq) => {
                                    if q.is_some() {
                                        info!("[VcamObs] Mapping recreated at {w}x{h}");
                                    } else {
                                        info!("[VcamObs] OBSVirtualCamVideo mapping created {w}x{h}");
                                    }
                                    q = Some(nq);
                                    q_size = (w, h);
                                }
                                None => warn!(
                                    "[VcamObs] create mapping failed (OBS 本体占用或权限问题)，稍后重试"
                                ),
                            }
                        }
                        if let Some(qq) = q.as_ref() {
                            if (w, h) == q_size {
                                let nv12 = rgb_to_nv12(&rgb, w as usize, h as usize);
                                unsafe { queue_write(qq, &nv12, timestamp_100ns()) };
                                frames_written += 1;
                                if frames_written == 1 {
                                    info!("[VcamObs] First frame written to OBS virtual camera");
                                }
                            }
                        }
                    }
                    Err(e) => warn!("[VcamObs] JPEG decode failed: {e}"),
                }
            } else if active && q.is_none()
                && last_try.elapsed() >= Duration::from_millis(500)
            {
                // 有应用在等画面、但手机还没推流：先建默认黑帧映射，
                // 让应用的采集管线能正常协商分辨率，不至于报错关闭摄像头
                last_try = Instant::now();
                if let Some(nq) = unsafe { queue_create(DEFAULT_W, DEFAULT_H, 30) } {
                    let black = black_nv12(DEFAULT_W, DEFAULT_H);
                    unsafe { queue_write(&nq, &black, timestamp_100ns()) };
                    q = Some(nq);
                    q_size = (DEFAULT_W, DEFAULT_H);
                    info!("[VcamObs] Placeholder black mapping created {DEFAULT_W}x{DEFAULT_H}");
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
