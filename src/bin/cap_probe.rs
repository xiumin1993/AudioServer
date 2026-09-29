// cap_probe.rs — 验证"独占试开"检测法：
// 每 500ms 尝试以 EXCLUSIVE 模式初始化 CABLE Output 捕获端。
//   成功            → 没有应用在录（立即释放）
//   AUDCLNT_E_DEVICE_IN_USE → 有应用正在录
// 运行时并行开一个共享捕获（mic_probe）即可模拟"应用在录"。

#[cfg(windows)]
fn main() {
    use std::thread::sleep;
    use std::time::Duration;
    use windows::core::GUID;
    use windows::Win32::Media::Audio::{
        eCapture, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_EXCLUSIVE,
        IAudioClient, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).expect("COM enum");
        const PKEY_NAME: PROPERTYKEY = PROPERTYKEY {
            fmtid: GUID::from_values(
                0xa45c254e, 0xdf1c, 0x4efd, [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
            ),
            pid: 14,
        };

        loop {
            let collection = enumerator
                .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
                .expect("enum");
            let count = collection.GetCount().unwrap();
            let mut result = "device-gone".to_string();
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
                if !name.to_uppercase().contains("CABLE OUTPUT") {
                    continue;
                }
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).expect("activate");
                let fmt = client.GetMixFormat().expect("format");
                // 先问驱动：独占模式支持什么格式
                let mut closest: *mut windows::Win32::Media::Audio::WAVEFORMATEX =
                    core::ptr::null_mut();
                let hr_sup = client.IsFormatSupported(
                    windows::Win32::Media::Audio::AUDCLNT_SHAREMODE_EXCLUSIVE,
                    fmt,
                    Some(&mut closest),
                );
                println!("IsFormatSupported(exclusive) -> 0x{:08X}", hr_sup.0 as u32);
                if !closest.is_null() {
                    let cf = closest;
                    let tag = core::ptr::read_unaligned(core::ptr::addr_of!((*cf).wFormatTag));
                    let ch = core::ptr::read_unaligned(core::ptr::addr_of!((*cf).nChannels));
                    let sr = core::ptr::read_unaligned(core::ptr::addr_of!((*cf).nSamplesPerSec));
                    let bits = core::ptr::read_unaligned(core::ptr::addr_of!((*cf).wBitsPerSample));
                    println!("  closest exclusive fmt: tag={} {}Hz {}ch {}bit", tag, sr, ch, bits);
                    CoTaskMemFree(Some(cf as *mut _));
                }
                // 独占模式缓冲时长必须对齐设备 period，用默认 period 初始化
                let mut period: i64 = 0;
                let _ = client.GetDevicePeriod(None, Some(&mut period));
                let hr = client.Initialize(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    Default::default(),
                    period.max(3_000_000),
                    0,
                    fmt,
                    None,
                );
                CoTaskMemFree(Some(fmt as *mut _));
                result = match hr {
                    Ok(()) => {
                        // 试开成功 = 设备空闲；立刻放下引用释放
                        drop(client);
                        "IDLE (exclusive open OK)".to_string()
                    }
                    Err(e) => format!("hr=0x{:08X} {}", e.code().0 as u32, e),
                };
                break;
            }
            println!("{}", result);
            sleep(Duration::from_millis(500));
        }
    }
}

#[cfg(not(windows))]
fn main() {}
