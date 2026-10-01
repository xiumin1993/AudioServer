// vcam_probe —— 以"摄像头应用"的视角枚举 DirectShow 视频输入设备。
// 用途：验证 Unity Capture 虚拟摄像头是否已被系统应用可见。
// 运行：cargo run --bin vcam_probe
//
// ══════════════ 初学者导读（独立小程序，一次性验证工具）══════════════
// 【它验证什么】Unity Capture 虚拟摄像头有没有成功注册进系统：应用（微信/
//   钉钉/浏览器桌面版）选摄像头时看到的就是本程序这份 DirectShow 设备名单，
//   这里能看到 = 应用能选到。结论看最后一行：出现带 ★ 的 Unity 行即已就绪。
//   它只"列名单"，不会真正打开摄像头——真打开验证是 dshow_consumer_probe。
// 【怎么跑】cargo run --bin vcam_probe —— 秒级跑完自动退出；不需要管理员
//   权限、不需要插任何摄像头（列的是注册表里登记的滤镜，纯本地查询）。
// 【输出怎么读】每行 • 一个设备名；含 "Unity" 的行尾带 "← ★ Unity Capture
//   已就绪" 是关键结论；只打印 "(未找到 Unity Capture 设备)" = 驱动未注册
//   （需安装 Unity Capture 或跑仓库 scripts/ 里的注册脚本后重试）。
// 【对应主干】src/vcam.rs（Unity Capture 注入引擎——它写的共享内存要靠这个
//   DirectShow 滤镜转发给应用，设备不可见则整条摄像头链路失效）。
// ── FFI 概念速查（详见 cap_probe.rs 头部长版解释）：
// windows crate=微软官方 FFI 绑定；I 开头=COM 接口；GUID/CLSID=128 位身份证；
// HRESULT==S_OK(0) 为成功；unsafe=对指针有效性的手工担保；CoInitializeEx=线程
// 进 COM 公寓登记（本文件用 APARTMENTTHREADED=STA 套间，DirectShow 推荐 STA）。

// 只在 Windows 编译下面真正的实现；非 Windows 用文件末尾空壳
#[cfg(windows)]
use windows::core::{w, GUID, VARIANT};
// DirectShow=微软老牌多媒体图框架，摄像头采集至今走它。ICreateDevEnum=设备
// 枚举器接口，是打开"设备名单"的唯一入口。
#[cfg(windows)]
use windows::Win32::Media::DirectShow::ICreateDevEnum;
// IMoniker（名字后缀 -ker 读"莫尼克"）= "设备的名字壳"：COM 用它表示一个
//   可实例化对象的身份而不是对象本身；IEnumMoniker=莫尼克列表的枚举器，
//   命名规律 IEnum+被枚举的东西。VARIANT=COM 万能值盒子（这里装设备名字符串）。
#[cfg(windows)]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IEnumMoniker, IMoniker, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
// IPropertyBag=键值对属性袋接口，设备上挂的 FriendlyName 从这里读。
#[cfg(windows)]
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
// S_OK：HRESULT 的"成功"值（0x00000000）。枚举接口的 Next() 不走 Result 包装，
// 要手动和 S_OK 比较——判错的另一种姿势。
#[cfg(windows)]
use windows::Win32::Foundation::S_OK;

// CLSID_SystemDeviceEnum = {62BE5D10-60EB-11d0-BD3B-00A0C911CE86}
// 下面两对常量是从微软文档抄来的 GUID 字面量：CLSID=类标识（找哪个组件），
// "类别"GUID=设备分类（这里要"视频输入设备"这一类）。from_u128 只是把
// 128 位整数按 0x高8_中4_中4_尾16 分段的写法拼成 GUID，与点分十六进制等价。
#[cfg(windows)]
const CLSID_SYSTEM_DEVICE_ENUM: GUID =
    GUID::from_u128(0x62be5d10_60eb_11d0_bd3b_00a0c911ce86);
// CLSID_VideoInputDeviceCategory = {860BB310-5D01-11d0-BD3B-00A0C911CE86}
#[cfg(windows)]
const VIDEO_INPUT_CATEGORY: GUID =
    GUID::from_u128(0x860bb310_5d01_11d0_bd3b_00a0c911ce86);

#[cfg(windows)]
fn main() {
    // 整个函数体一个大 unsafe：全是 COM FFI 调用。STA（单线程公寓）套间里
    // COM 调用受消息泵保护，DirectShow 组件推荐这种模式。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // 实例化"系统设备枚举器"组件（CLSCTX_INPROC_SERVER=DLL 直接加载进本进程）。
        // match 拆 Result：失败打印错误 + exit(1)，一次性工具坏在起点就直说。
        let dev_enum: ICreateDevEnum =
            match CoCreateInstance(&CLSID_SYSTEM_DEVICE_ENUM, None, CLSCTX_INPROC_SERVER) {
                Ok(e) => e,
                Err(err) => {
                    eprintln!("CoCreateInstance(CLSID_SystemDeviceEnum) failed: {err}");
                    std::process::exit(1);
                }
            };

        // CreateClassEnumerator(类别 GUID, 出参枚举器, 标志位 0=保留必须传 0)：
        // 拿"视频输入设备"这一类的名单。注意出参是 Option<IEnumMoniker>——
        // 该类一个设备都没有时函数返回"成功"但出参为 None，两种失败分开处理。
        let mut enum_opt: Option<IEnumMoniker> = None;
        if let Err(err) =
            dev_enum.CreateClassEnumerator(&VIDEO_INPUT_CATEGORY, &mut enum_opt, 0)
        {
            println!("没有视频输入设备类别枚举器（{err}）");
            return;
        }
        // let ... else：从 Option 拆值，None 就直接走大括号/return 分支。
        let Some(enum_mon) = enum_opt else {
            println!("系统中没有任何已注册的视频输入设备");
            return;
        };

        println!("── 系统可见的视频输入设备 ──");
        let mut found_unity = false;
        let mut total = 0u32;
        // 经典 COM 枚举循环：Next 一次取一个元素放进"1 格槽数组"（[Option<IMoniker>; 1]
        // 是长度固定为 1 的数组类型），返回值不是 S_OK（走到表尾返回 S_FALSE）就停。
        loop {
            let mut slot: [Option<IMoniker>; 1] = [None];
            if enum_mon.Next(&mut slot, None) != S_OK {
                break;
            }
            let Some(m) = slot[0].clone() else { continue };
            total += 1;
            // 设备名不直接放在 moniker 上：BindToStorage 打开它的"属性存储"，
            // 期望拿到的接口类型是 IPropertyBag（属性袋），再 Read 键 "FriendlyName"。
            // w!("FriendlyName") 是 windows-rs 宏：编译期把字面量转成 UTF-16
            // 宽字符串（Windows API 字符串都是 UTF-16，见文件头速查）。
            // VARIANT 是万能值盒子，to_string() 把装着的 BSTR 字符串取出来。
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
            // 关键判定：名字含 "Unity" 即认为 Unity Capture 已注册可见。
            let mark = if name.contains("Unity") {
                found_unity = true;
                "  ← ★ Unity Capture 已就绪"
            } else {
                ""
            };
            println!("  • {name}{mark}");
        }
        // 收尾统计：total 是名单总数；没找到 Unity 时明确打一行失败结论。
        println!("(共枚举到 {total} 个设备)");
        if !found_unity {
            println!("(未找到 Unity Capture 设备)");
        }
    }
}

// 非 Windows 平台（如 macOS）编译时给出的空实现：
// 本探针只在 Windows 上有意义，但 bin 目标必须有一个 main，
// 否则 cargo build 在其他平台会报 E0601（找不到 main）而整体失败。
#[cfg(not(windows))]
fn main() {
    println!("This probe is Windows-only.");
}
