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

use crate::server::ServerEvent;
use log::{info, warn};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

/// 上行样本队列：手机线程写入，注入线程消费。存的是 i16 单声道样本。
pub type MicQueue = Arc<Mutex<VecDeque<i16>>>;

/// 队列积压上限（样本数）。48kHz 下约 250ms，再大说明网络比实时快，丢旧保新。
const MAX_QUEUE_SAMPLES: usize = 12000;

// ── v3.4.4 上行采样率开关 + 重采样 ─────────────────────────────────
/// 手机实际发来的 PCM 采样率（44100 / 48000），服务器收到 mic_start 时更新。
/// 默认 48000：与声卡设备速率一致时，注入路径与旧版【逐字节相同】（零风险）；
/// 只有用户在手机上主动切到 44.1k，泵里才会启用线性插值重采样，
/// 否则把 44.1k 当 48k 直接写会音调变快、声音变尖。
pub static UPLINK_RATE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(48000);

/// 手机上报上行采样率时调用（非法值忽略，维持上一个好值）
pub fn set_uplink_rate(sr: u32) {
    if (8000..=192000).contains(&sr) {
        UPLINK_RATE.store(sr, std::sync::atomic::Ordering::Relaxed);
        info!("[MicOut] Uplink sample rate set to {} Hz", sr);
    }
}

/// 目标设备名片段（VB-CABLE 的播放端叫 "CABLE Input"，大小写不敏感匹配）
const CABLE_RENDER_HINT: &str = "CABLE INPUT";

/// VB-CABLE 的录音端名片段（占用检测用：应用录的是 "CABLE Output"）
const CABLE_CAPTURE_HINT: &str = "CABLE OUTPUT";

pub fn new_queue() -> MicQueue {
    Arc::new(Mutex::new(VecDeque::with_capacity(2048)))
}

/// 把一段 PCM s16le 字节流推入上行队列（超长自动丢弃最旧样本）
pub fn push_uplink(queue: &MicQueue, bytes: &[u8]) {
    let mut q = queue.lock().unwrap();
    for chunk in bytes.chunks_exact(2) {
        q.push_back(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    let overflow = q.len().saturating_sub(MAX_QUEUE_SAMPLES);
    if overflow > 0 {
        q.drain(..overflow);
    }
}

/// 启动注入引擎线程（Windows 见 windows_impl；macOS 见 macos_impl；其余平台 stub）
#[cfg(windows)]
pub fn spawn_mic_output(queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
    std::thread::spawn(move || windows_impl::engine(queue, event_tx));
}

/// macOS：cpal 输出流渲染进 BlackHole 2ch（对端应用把输入选为 BlackHole 2ch 即用）
#[cfg(target_os = "macos")]
pub fn spawn_mic_output(queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
    std::thread::spawn(move || macos_impl::engine(queue, event_tx));
}

/// 其他平台 stub：直接上报"不可用"，保持上层逻辑统一
#[cfg(not(any(windows, target_os = "macos")))]
pub fn spawn_mic_output(_queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
    event_tx.send(ServerEvent::MicEngine { device: None }).ok();
    info!("[MicOut] Virtual mic injection is implemented for Windows/macOS only.");
}

/// 启动"CABLE Output 被应用占用"检测线程。
/// 状态翻转时通过通道回传（true = 有应用在录），服务器据此向手机推
/// {"type":"mic_state","active":bool} —— v3.3：这条指令就是手机的
/// 麦克风硬件开关信号：true 才开录、false 立刻停（按需录音）。
/// Windows 实现见 windows_impl::capture_monitor（会话枚举法）。
#[cfg(windows)]
pub fn spawn_capture_monitor(tx: tokio::sync::mpsc::UnboundedSender<bool>) {
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

#[cfg(windows)]
mod windows_impl {
    use super::*;
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
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let enumerator: IMMDeviceEnumerator = match unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        } {
            Ok(e) => e,
            Err(e) => {
                warn!("[MicOut] COM enumerator failed: {}", e);
                event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                return;
            }
        };

        loop {
            match open_cable_render(&enumerator) {
                Ok(stream) => {
                    event_tx
                        .send(ServerEvent::MicEngine {
                            device: Some(stream.device_name.clone()),
                        })
                        .ok();
                    info!(
                        "[MicOut] Injecting into '{}' ({}Hz, {}ch, {}-bit)",
                        stream.device_name, stream.sample_rate, stream.channels, stream.bits
                    );
                    pump(&stream, &queue);
                    event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                    warn!("[MicOut] Pump exited (device lost?), retry in 3s");
                }
                Err(e) => {
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
    pub(crate) fn capture_monitor(tx: tokio::sync::mpsc::UnboundedSender<bool>) {
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

        let mut last_active = false;
        loop {
            let active = cable_output_in_use(&enumerator).unwrap_or(false);
            if active != last_active {
                last_active = active;
                info!(
                    "[MicMon] CABLE Output capture {}",
                    if active { "ACTIVE (an app is using the mic)" } else { "idle (nobody recording)" }
                );
                let _ = tx.send(active);
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    /// 枚举 CABLE Output 捕获端点上的音频会话：有 Active 会话 = 应用在录
    fn cable_output_in_use(enumerator: &IMMDeviceEnumerator) -> Result<bool> {
        use windows::core::Interface;
        use windows::Win32::Media::Audio::{
            eCapture, IAudioSessionControl2, IAudioSessionManager2, AudioSessionStateActive,
        };

        let collection = unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)? };
        let count = unsafe { collection.GetCount()? };
        for i in 0..count {
            let device = unsafe { collection.Item(i)? };
            if !device_friendly_name(&device).to_uppercase().contains(CABLE_CAPTURE_HINT) {
                continue;
            }
            let mgr: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None)? };
            let sessions = unsafe { mgr.GetSessionEnumerator()? };
            let n = unsafe { sessions.GetCount()? };
            for k in 0..n {
                let ctrl = unsafe { sessions.GetSession(k)? };
                let ctrl2: IAudioSessionControl2 = match ctrl.cast() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                // 系统声音占位会话永远 inactive，跳过纯保险
                let state = unsafe { ctrl2.GetState()? };
                if state == AudioSessionStateActive {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        Ok(false) // 设备不存在（驱动被卸载）
    }

    /// 已打开的注入流：持有 IAudioClient 与格式信息
    struct MicOutputStream {
        client: IAudioClient,
        device_name: String,
        sample_rate: u32,
        channels: u16,
        bits: u16,
        is_f32: bool,
    }

    /// 枚举播放设备，找名字含 "CABLE Input" 的那台并建立共享渲染流
    fn open_cable_render(enumerator: &IMMDeviceEnumerator) -> Result<MicOutputStream> {
        let collection =
            unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
        let count = unsafe { collection.GetCount()? };
        let mut chosen: Option<(IMMDevice, String)> = None;
        for i in 0..count {
            let device = unsafe { collection.Item(i)? };
            let name = device_friendly_name(&device);
            if name.to_uppercase().contains(CABLE_RENDER_HINT) {
                chosen = Some((device, name));
                break;
            }
        }
        let (device, device_name) = match chosen {
            Some(d) => d,
            None => anyhow::bail!(
                "VB-CABLE render device '{}' not found — install VB-Audio Virtual Cable",
                CABLE_RENDER_HINT
            ),
        };

        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let fmt_ptr = unsafe { client.GetMixFormat()? };
        let fmt = unsafe { &*fmt_ptr };
        let sample_rate = fmt.nSamplesPerSec;
        let channels = fmt.nChannels;
        let bits = fmt.wBitsPerSample;
        // 共享模式下 mix 格式通常是 float32（含 0xFFFE extensible），PCM 则是 s16
        let is_f32 = bits == 32 && (fmt.wFormatTag == 3 || fmt.wFormatTag == 0xFFFE);

        let mut period: i64 = 0;
        unsafe { client.GetDevicePeriod(None, Some(&mut period))? };
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
        unsafe { CoTaskMemFree(Some(fmt_ptr as *mut _)) };
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
    fn pump(stream: &MicOutputStream, queue: &MicQueue) {
        let render: IAudioRenderClient = match unsafe { stream.client.GetService() } {
            Ok(r) => r,
            Err(e) => {
                warn!("[MicOut] GetService(IAudioRenderClient) failed: {}", e);
                return;
            }
        };
        if unsafe { stream.client.Start() }.is_err() {
            warn!("[MicOut] IAudioClient::Start failed");
            return;
        }
        let ch = stream.channels as usize;
        let buf_frames = unsafe { stream.client.GetBufferSize() }.unwrap_or(1024);
        // v3.4.4 重采样进位状态（仅上行速率≠设备速率时使用）：
        // carry = 跨泵周期留存的少量上行样本；rpos = 读取位置相对 carry 头部的小数偏移
        let mut carry: std::collections::VecDeque<i16> = std::collections::VecDeque::new();
        let mut rpos: f64 = 0.0;
        loop {
            let padding: u32 = match unsafe { stream.client.GetCurrentPadding() } {
                Ok(p) => p,
                Err(_) => return,
            };
            let available = buf_frames.saturating_sub(padding);
            if available == 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            let ptr: *mut u8 = match unsafe { render.GetBuffer(available) } {
                Ok(p) => p,
                Err(_) => return,
            };
            // 一次性从队列取需要的帧数样本，不足补静音
            let frames = available as usize;
            let mut mono: Vec<i16> = Vec::with_capacity(frames);
            // 本轮到底有没有拿到真实上行音频。下面节流要用：
            // 有真实音频时【绝不许睡觉】（睡了就是录音延迟），
            // 只有"纯写静音保活"的那种空转才需要被限速。
            // 这里故意不写初始值：下面那个块一定会赋值，写了 rustc 反而报
            // "value assigned is never read"。
            let had_uplink;
            {
                let urate = UPLINK_RATE.load(std::sync::atomic::Ordering::Relaxed);
                let resample = urate != stream.sample_rate && urate >= 8000;
                let mut q = queue.lock().unwrap();
                had_uplink = !q.is_empty();
                if !resample {
                    // 常见路径：速率一致，直接逐样本搬运（与旧版完全相同）
                    for _ in 0..frames {
                        mono.push(q.pop_front().unwrap_or(0));
                    }
                } else {
                    // 线性插值重采样：每个设备样本 = 两个相邻上行样本的加权平均。
                    // step = 平均每产出一个设备样本要消耗多少上行样本
                    //（44.1k→48k 时 step≈0.91875）。
                    let step = urate as f64 / stream.sample_rate as f64;
                    let need = ((frames as f64 - 1.0) * step + rpos).ceil() as usize + 1;
                    while carry.len() < need {
                        carry.push_back(q.pop_front().unwrap_or(0));
                    }
                    for i in 0..frames {
                        let p = rpos + i as f64 * step;
                        let i0 = p as usize;
                        let f = p - i0 as f64;
                        let a = *carry.get(i0).unwrap_or(&0) as f64;
                        let b = *carry
                            .get(i0 + 1)
                            .unwrap_or_else(|| carry.get(i0).unwrap_or(&0))
                            as f64;
                        mono.push((a * (1.0 - f) + b * f).round() as i16);
                    }
                    // 丢弃已经插值过去的前端样本，rpos 归到 [0,1) 区间
                    let last_pos = rpos + (frames as f64 - 1.0) * step;
                    let consumed = last_pos.floor() as usize;
                    if consumed > 0 {
                        for _ in 0..consumed.min(carry.len()) {
                            carry.pop_front();
                        }
                    }
                    rpos = last_pos - consumed as f64;
                    // 保险阀：浮点抖动也不允许 carry 无限膨胀（正常应 ≤3 个）
                    while carry.len() > 4 {
                        carry.pop_front();
                    }
                }
            }
            unsafe {
                if stream.is_f32 {
                    let out = std::slice::from_raw_parts_mut(ptr as *mut f32, frames * ch);
                    for (f, &s) in out.chunks_mut(ch).zip(mono.iter()) {
                        let v = s as f32 / 32768.0;
                        for c in f.iter_mut() {
                            *c = v;
                        }
                    }
                } else if stream.bits == 16 {
                    let out = std::slice::from_raw_parts_mut(ptr as *mut i16, frames * ch);
                    for (f, &s) in out.chunks_mut(ch).zip(mono.iter()) {
                        for c in f.iter_mut() {
                            *c = s;
                        }
                    }
                } else {
                    // 其他位深（少见）：写静音保活
                    std::slice::from_raw_parts_mut(ptr, frames * ch * (stream.bits as usize / 8))
                        .fill(0);
                }
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
            if !had_uplink {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }

    /// 通过属性存储读设备友好名（与 server.rs 捕获端同一手法）
    fn device_friendly_name(device: &IMMDevice) -> String {
        // PKEY_Device_FriendlyName（Windows SDK 标准定义）
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
            match device.OpenPropertyStore(STGM_READ) {
                Ok(store) => match store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) {
                    Ok(pv) => {
                        let pv_ptr = &pv as *const _ as *const u8;
                        let vt = *(pv_ptr as *const u16);
                        if vt == 31 {
                            // VT_LPWSTR：宽字符串指针在 offset 8
                            let pwsz = *(pv_ptr.add(8) as *const *const u16);
                            if !pwsz.is_null() {
                                let len =
                                    (0..).take_while(|&i| *pwsz.add(i) != 0).count();
                                let slice = std::slice::from_raw_parts(pwsz, len);
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
#[cfg(target_os = "macos")]
mod macos_impl {
    use super::*;
    use anyhow::Result;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::mpsc::channel;

    /// 注入目标设备名片段（BlackHole 的播放端；env PCSPEAKER_INJECT_DEVICE 可覆盖）
    fn render_needle() -> String {
        let w = std::env::var("PCSPEAKER_INJECT_DEVICE").unwrap_or_default();
        if w.is_empty() {
            "blackhole 2ch".to_string()
        } else {
            w.to_ascii_lowercase()
        }
    }

    /// 引擎主循环：建流 → 报错/掉线 → 释放 → 3 秒后重试（与 windows_impl::engine 同构）
    pub(crate) fn engine(queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
        loop {
            match build_stream(&queue) {
                Err(e) => {
                    event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                    warn!("[MicOut] {}", e);
                }
                Ok((name, stream, err_rx)) => {
                    if let Err(e) = stream.play() {
                        event_tx.send(ServerEvent::MicEngine { device: None }).ok();
                        warn!("[MicOut] cpal render play failed: {}", e);
                        drop(stream);
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        continue;
                    }
                    event_tx
                        .send(ServerEvent::MicEngine { device: Some(name.clone()) })
                        .ok();
                    info!("[MicOut] Injecting into '{}' (macOS cpal render)", name);
                    // 阻塞等 cpal 错误回调投信号（设备消失/流被系统终止）
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
        let host = cpal::default_host();
        let needle = render_needle();
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
        let name = device.name().unwrap_or_else(|_| "BlackHole".to_string());
        let cfg = device.default_output_config()?;
        let dev_rate = cfg.sample_rate().0;
        let dev_ch: usize = cfg.channels() as usize;
        let format = cfg.sample_format();
        let stream_cfg = cpal::StreamConfig::from(cfg);
        let (err_tx, err_rx) = channel::<()>();

        let stream = match format {
            // BlackHole 默认以 float32 报告 —— 主路径
            cpal::SampleFormat::F32 => {
                let q = queue.clone();
                let et = err_tx.clone();
                // 跨回调存活的重采样进位状态（与 windows_impl::pump 一致）
                let mut carry: VecDeque<i16> = VecDeque::new();
                let mut rpos: f64 = 0.0;
                device.build_output_stream(
                    &stream_cfg,
                    move |data: &mut [f32], _| {
                        let frames = data.len() / dev_ch.max(1);
                        let mono = fill_frames(&q, frames, dev_rate, &mut carry, &mut rpos);
                        for (frame, &s) in data.chunks_mut(dev_ch).zip(mono.iter()) {
                            let v = s as f32 / 32768.0;
                            for c in frame.iter_mut() {
                                *c = v;
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
            // 少数设备以 s16 报告默认格式 —— 备路径
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
            other => anyhow::bail!("Unsupported render sample format: {:?}", other),
        };
        drop(err_tx); // 原件释放；回调里各持一份克隆，err_rx 仍连着
        Ok((name, stream, err_rx))
    }

    /// 产出 frames 个单声道 i16 样本：上行速率=设备速率直取；否则线性插值重采样。
    /// 算法与 windows_impl::pump 内联版逐行对应（v3.4.4 carry/rpos 方案）。
    fn fill_frames(
        queue: &MicQueue,
        frames: usize,
        dev_rate: u32,
        carry: &mut VecDeque<i16>,
        rpos: &mut f64,
    ) -> Vec<i16> {
        let mut mono: Vec<i16> = Vec::with_capacity(frames);
        if frames == 0 {
            return mono;
        }
        let urate = UPLINK_RATE.load(std::sync::atomic::Ordering::Relaxed);
        let mut q = queue.lock().unwrap();
        if urate == dev_rate || urate < 8000 {
            for _ in 0..frames {
                mono.push(q.pop_front().unwrap_or(0));
            }
            return mono;
        }
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
            let b = *carry
                .get(i0 + 1)
                .unwrap_or_else(|| carry.get(i0).unwrap_or(&0)) as f64;
            mono.push((a * (1.0 - f) + b * f).round() as i16);
        }
        let last_pos = *rpos + (frames as f64 - 1.0) * step;
        let consumed = last_pos.floor() as usize;
        for _ in 0..consumed.min(carry.len()) {
            carry.pop_front();
        }
        *rpos = last_pos - consumed as f64;
        while carry.len() > 4 {
            carry.pop_front();
        }
        mono
    }

    /// macOS"有没有应用正在录注入设备"检测 → 驱动手机按需录音的 mic_state 信号。
    /// 原理：CoreAudio 设备属性 kAudioDevicePropertyDeviceIsRunningSomewhere
    /// （FourCC 'isrn'）在【输入域】非零 = 有客户端真正跑着采集 IO。
    /// 我们的注入流在 BlackHole 2ch 的【输出侧】、下行捕获故意用 16ch 另一台设备，
    /// 所以不会自己把自己误判成"有人在用"。
    /// 兜底：env PCSPEAKER_MAC_MIC_ALWAYS_ACTIVE=1 → 强制视为占用中。
    pub(crate) fn capture_monitor(tx: tokio::sync::mpsc::UnboundedSender<bool>) {
        let forced = std::env::var("PCSPEAKER_MAC_MIC_ALWAYS_ACTIVE").is_ok();
        let mut last = false;
        loop {
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
    const K_DEVICES: u32 = 0x6465_7623; // kAudioHardwarePropertyDevices            'dev#'
    const K_NAME_CFSTRING: u32 = 0x706E_6D72; // kAudioDevicePropertyDeviceNameCFString 'pnmr'
    const K_IS_RUNNING_SOMEWHERE: u32 = 0x6973_726E; //                          'isrn'
    const K_SCOPE_GLOBAL: u32 = 0x676C_6F62; // kAudioObjectPropertyScopeGlobal  'glob'
    const K_SCOPE_INPUT: u32 = 0x696E_7074; // kAudioObjectPropertyScopeInput     'inpt'

    fn prop_addr(selector: u32, scope: u32) -> coreaudio_sys::AudioObjectPropertyAddress {
        coreaudio_sys::AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: 0, // kAudioObjectPropertyElementMaster
        }
    }

    /// 全部音频设备 ID
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
            if st != 0 || s.is_null() {
                return None;
            }
            let bytes: Vec<u8> = {
                // 快路径：系统缓存的 UTF-8 指针；慢路径：自己开缓冲区拷
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
                    if ok == 0 {
                        CFRelease(s as CFTypeRef);
                        return None;
                    }
                    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                    buf.truncate(len);
                    buf
                }
            };
            CFRelease(s as CFTypeRef);
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
    }

    /// 读一个 u32 型设备属性
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
            let Some(name) = device_name(id) else { continue };
            if !name.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            // 找到目标设备：它输入域的运转状态就是答案
            let running = u32_prop(id, K_IS_RUNNING_SOMEWHERE, K_SCOPE_INPUT).unwrap_or(0);
            return Ok(running != 0);
        }
        anyhow::bail!("inject device '{}' not found for capture monitor", needle)
    }
}
