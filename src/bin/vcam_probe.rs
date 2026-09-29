// vcam_probe —— 以"摄像头应用"的视角枚举 DirectShow 视频输入设备。
// 用途：验证 Unity Capture 虚拟摄像头是否已被系统应用可见。
// 运行：cargo run --bin vcam_probe
#![cfg(windows)]

use windows::core::{w, GUID, VARIANT};
use windows::Win32::Media::DirectShow::ICreateDevEnum;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IEnumMoniker, IMoniker, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::Foundation::S_OK;

// CLSID_SystemDeviceEnum = {62BE5D10-60EB-11d0-BD3B-00A0C911CE86}
const CLSID_SYSTEM_DEVICE_ENUM: GUID =
    GUID::from_u128(0x62be5d10_60eb_11d0_bd3b_00a0c911ce86);
// CLSID_VideoInputDeviceCategory = {860BB310-5D01-11d0-BD3B-00A0C911CE86}
const VIDEO_INPUT_CATEGORY: GUID =
    GUID::from_u128(0x860bb310_5d01_11d0_bd3b_00a0c911ce86);

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let dev_enum: ICreateDevEnum =
            match CoCreateInstance(&CLSID_SYSTEM_DEVICE_ENUM, None, CLSCTX_INPROC_SERVER) {
                Ok(e) => e,
                Err(err) => {
                    eprintln!("CoCreateInstance(CLSID_SystemDeviceEnum) failed: {err}");
                    std::process::exit(1);
                }
            };

        let mut enum_opt: Option<IEnumMoniker> = None;
        if let Err(err) =
            dev_enum.CreateClassEnumerator(&VIDEO_INPUT_CATEGORY, &mut enum_opt, 0)
        {
            println!("没有视频输入设备类别枚举器（{err}）");
            return;
        }
        let Some(enum_mon) = enum_opt else {
            println!("系统中没有任何已注册的视频输入设备");
            return;
        };

        println!("── 系统可见的视频输入设备 ──");
        let mut found_unity = false;
        let mut total = 0u32;
        loop {
            let mut slot: [Option<IMoniker>; 1] = [None];
            if enum_mon.Next(&mut slot, None) != S_OK {
                break;
            }
            let Some(m) = slot[0].clone() else { continue };
            total += 1;
            let name = match m.BindToStorage::<_, _, IPropertyBag>(None, None) {
                Ok(bag) => {
                    let mut var = VARIANT::default();
                    if bag
                        .Read(w!("FriendlyName"), &mut var as *mut VARIANT, None)
                        .is_ok()
                    {
                        var.to_string()
                    } else {
                        "(读不到 FriendlyName)".to_string()
                    }
                }
                Err(_) => "(非 IPropertyBag)".to_string(),
            };
            let mark = if name.contains("Unity") {
                found_unity = true;
                "  ← ★ Unity Capture 已就绪"
            } else {
                ""
            };
            println!("  • {name}{mark}");
        }
        println!("(共枚举到 {total} 个设备)");
        if !found_unity {
            println!("(未找到 Unity Capture 设备)");
        }
    }
}
