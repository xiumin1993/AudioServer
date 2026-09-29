// mic_probe.rs — 调试工具：直接录制 "CABLE Output" 3 秒并报告电平
// 用途：验证 AudioServer 的 WASAPI 注入引擎是否真的把 PCM 写进了虚拟声卡。
// 用法：先让上行数据流动（Node 正弦波或手机），再运行 cargo run --bin mic_probe

#[cfg(windows)]
fn main() {
    use std::thread::sleep;
    use std::time::{Duration, Instant};
    use windows::core::GUID;
    use windows::Win32::Media::Audio::{
        eCapture, IAudioCaptureClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, IAudioClient,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).expect("COM enum");
        let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).expect("enum");
        let count = collection.GetCount().unwrap();
        let mut target: Option<IMMDevice> = None;
        const PKEY_NAME: PROPERTYKEY = PROPERTYKEY {
            fmtid: GUID::from_values(
                0xa45c254e, 0xdf1c, 0x4efd, [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
            ),
            pid: 14,
        };
        for i in 0..count {
            let device = collection.Item(i).unwrap();
            let store = device.OpenPropertyStore(STGM_READ).unwrap();
            let pv = store.GetValue(&PKEY_NAME).unwrap();
            let pv_ptr = &pv as *const _ as *const u8;
            let vt = *(pv_ptr as *const u16);
            let mut name = String::new();
            if vt == 31 {
                let pwsz = *(pv_ptr.add(8) as *const *const u16);
                if !pwsz.is_null() {
                    let len = (0..).take_while(|&k| *pwsz.add(k) != 0).count();
                    name = String::from_utf16_lossy(std::slice::from_raw_parts(pwsz, len));
                }
            }
            println!("capture[{}] = {}", i, name);
            if name.to_uppercase().contains("CABLE OUTPUT") {
                target = Some(device);
            }
        }
        let device = target.expect("CABLE Output not found");
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).expect("activate");
        let fmt_ptr = client.GetMixFormat().expect("mix format");
        // WAVEFORMATEX 是 1 字节对齐的 packed 结构，字段必须 read_unaligned 拷贝
        let (rate, chans, bits, tag) = unsafe {
            (
                core::ptr::read_unaligned(core::ptr::addr_of!((*fmt_ptr).nSamplesPerSec)),
                core::ptr::read_unaligned(core::ptr::addr_of!((*fmt_ptr).nChannels)),
                core::ptr::read_unaligned(core::ptr::addr_of!((*fmt_ptr).wBitsPerSample)),
                core::ptr::read_unaligned(core::ptr::addr_of!((*fmt_ptr).wFormatTag)),
            )
        };
        println!("mix format: {}Hz {}ch {}-bit tag=0x{:X}", rate, chans, bits, tag);
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, Default::default(), 0, 0, fmt_ptr, None)
            .expect("init");
        CoTaskMemFree(Some(fmt_ptr as *mut _));
        let capture: IAudioCaptureClient = client.GetService().expect("capture client");
        client.Start().expect("start");

        let is_f32 = bits == 32;
        let ch = chans as usize;
        let t0 = Instant::now();
        let (mut sum_sq, mut peak, mut n) = (0f64, 0f64, 0u64);
        // 分段统计：前 3 秒是"探针触发唤醒→手机开麦→首包到达"的冷启动期，
        // 只统计后 5 秒的电平，才能真实反映稳态链路是否通
        let (mut sum_sq_late, mut peak_late, mut n_late) = (0f64, 0f64, 0u64);
        while t0.elapsed() < Duration::from_secs(8) {
            let size = capture.GetNextPacketSize().unwrap_or(0);
            if size == 0 {
                sleep(Duration::from_millis(10));
                continue;
            }
            let mut data_ptr: *mut u8 = core::ptr::null_mut();
            let mut frames_read = 0u32;
            let mut flags = 0u32;
            capture
                .GetBuffer(&mut data_ptr, &mut frames_read, &mut flags, None, None)
                .expect("buffer");
            let frames = frames_read as usize;
            let late = t0.elapsed().as_secs() >= 3;
            let data = unsafe {
                std::slice::from_raw_parts(data_ptr, frames * ch * if is_f32 { 4 } else { 2 })
            };
            if is_f32 {
                for s in data
                    .chunks_exact(4)
                    .step_by(ch)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                {
                    let v = s as f64;
                    sum_sq += v * v;
                    n += 1;
                    if v.abs() > peak {
                        peak = v.abs();
                    }
                    if late {
                        sum_sq_late += v * v;
                        n_late += 1;
                        if v.abs() > peak_late {
                            peak_late = v.abs();
                        }
                    }
                }
            } else {
                for s in data
                    .chunks_exact(2)
                    .step_by(ch)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f64 / 32768.0)
                {
                    sum_sq += s * s;
                    n += 1;
                    if s.abs() > peak {
                        peak = s.abs();
                    }
                    if late {
                        sum_sq_late += s * s;
                        n_late += 1;
                        if s.abs() > peak_late {
                            peak_late = s.abs();
                        }
                    }
                }
            }
            unsafe { capture.ReleaseBuffer(size).expect("release") };
        }
        let rms = if n > 0 { (sum_sq / n as f64).sqrt() } else { 0.0 };
        let rms_late = if n_late > 0 { (sum_sq_late / n_late as f64).sqrt() } else { 0.0 };
        println!(
            "RESULT: samples={} rms={:.5} ({:.1} dBFS) peak={:.5} ({:.1} dBFS)",
            n,
            rms,
            20.0 * rms.max(1e-9).log10(),
            peak,
            20.0 * peak.max(1e-9).log10()
        );
        println!(
            "RESULT(warm, after 3s): samples={} rms={:.5} ({:.1} dBFS) peak={:.5} ({:.1} dBFS)",
            n_late,
            rms_late,
            20.0 * rms_late.max(1e-9).log10(),
            peak_late,
            20.0 * peak_late.max(1e-9).log10()
        );
        if peak_late > 0.001 {
            println!("=> CABLE Output HAS SIGNAL (injection path OK)");
        } else {
            println!("=> CABLE Output SILENT — server injection is broken or phone data not arriving");
        }
    }
}

#[cfg(not(windows))]
fn main() {
    println!("Windows only");
}
