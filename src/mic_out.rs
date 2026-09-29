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

/// 启动注入引擎线程（Windows 实现见下方 windows_impl，其他平台为空 stub）
#[cfg(windows)]
pub fn spawn_mic_output(queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
    std::thread::spawn(move || windows_impl::engine(queue, event_tx));
}

/// 非 Windows 平台 stub：直接上报"不可用"，保持上层逻辑统一
#[cfg(not(windows))]
pub fn spawn_mic_output(_queue: MicQueue, event_tx: std::sync::mpsc::Sender<ServerEvent>) {
    event_tx.send(ServerEvent::MicEngine { device: None }).ok();
    info!("[MicOut] Virtual mic injection is Windows-only (WASAPI render).");
}

/// 启动"CABLE Output 被应用占用"检测线程（仅 Windows 有意义）。
/// 状态翻转时通过通道回传（true = 有应用在录），服务器据此向手机推
/// {"type":"mic_state","active":bool} —— v3.3：这条指令就是手机的
/// 麦克风硬件开关信号：true 才开录、false 立刻停（按需录音）。
#[cfg(windows)]
pub fn spawn_capture_monitor(tx: tokio::sync::mpsc::UnboundedSender<bool>) {
    std::thread::spawn(move || windows_impl::capture_monitor(tx));
}

/// 非 Windows stub：不产生任何事件
#[cfg(not(windows))]
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
            {
                let urate = UPLINK_RATE.load(std::sync::atomic::Ordering::Relaxed);
                let resample = urate != stream.sample_rate && urate >= 8000;
                let mut q = queue.lock().unwrap();
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
