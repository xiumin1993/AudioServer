// session_probe.rs — 验证"音频会话枚举"检测法：
// 每 500ms 枚举 CABLE Output 端点上的音频会话数。
// 有应用在录该麦克风 → 会话数 > 0（录音软件在混音器里可见的原理相同）。

#[cfg(windows)]
fn main() {
    use std::thread::sleep;
    use std::time::Duration;
    use windows::core::{GUID, Interface};
    use windows::Win32::Media::Audio::{
        eCapture, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
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
            let mut line = "device-gone".to_string();
            for i in 0..count {
                let device: IMMDevice = collection.Item(i).unwrap();
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
                let mgr: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                    Ok(m) => m,
                    Err(e) => {
                        line = format!("activate session mgr failed: {}", e);
                        break;
                    }
                };
                let enumerator_sessions = mgr.GetSessionEnumerator().unwrap();
                let n = enumerator_sessions.GetCount().unwrap();
                let mut desc = Vec::new();
                for k in 0..n {
                    if let Ok(s) = enumerator_sessions.GetSession(k) {
                        let s2: Result<windows::Win32::Media::Audio::IAudioSessionControl2, _> =
                            s.cast();
                        if let Ok(s2) = s2 {
                            let state = s2.GetState().unwrap_or_default();
                            let sys = s2.IsSystemSoundsSession().is_ok();
                            desc.push(format!("session{}({:?},sys={})", k, state, sys));
                        }
                    }
                }
                line = format!("sessions={} {:?}", n, desc);
                break;
            }
            println!("{}", line);
            sleep(Duration::from_millis(500));
        }
    }
}

#[cfg(not(windows))]
fn main() {}
