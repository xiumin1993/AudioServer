// dshow_consumer_probe —— 扮演"使用摄像头的应用"，真实打开一个 DirectShow 摄像头设备。
//
// 为什么需要它：
//   OBS Virtual Camera 通道靠"共享内存被别的进程打开"来判断有没有应用在用摄像头
//   （见 src/vcam_obs.rs 的 mapping_handle_count）。这个机制已用
//   queue_handle_probe 验证过计数本身可靠，但还缺最后一环：
//   **OBS 的滤镜 DLL 到底会不会在应用进程里 OpenFileMapping("OBSVirtualCamVideo")**。
//   只有真正跑起一条采集图才能证实，本探针就是干这个的。
//
// 它做的事和一个普通会议软件打开摄像头完全一样：
//   枚举 VideoInputDeviceCategory → 按名字绑定滤镜 → 加入 FilterGraph
//   → Render 输出引脚 → IMediaControl::Run() → 保持 N 秒 → Stop()
//
// 运行：
//   cargo run --release --bin dshow_consumer_probe            # 默认 OBS Virtual Camera，持 10s
//   cargo run --release --bin dshow_consumer_probe -- "Unity Capture" 12
#![cfg(windows)]

use windows::core::{Interface, GUID, VARIANT, w};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::DirectShow::{
    IBaseFilter, ICreateDevEnum, IEnumPins, IGraphBuilder, IMediaControl, IPin, PINDIR_OUTPUT,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IEnumMoniker, IMoniker, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};

// CLSID_SystemDeviceEnum = {62BE5D10-60EB-11d0-BD3B-00A0C911CE86}
const CLSID_SYSTEM_DEVICE_ENUM: GUID = GUID::from_u128(0x62be5d10_60eb_11d0_bd3b_00a0c911ce86);
// CLSID_VideoInputDeviceCategory = {860BB310-5D01-11d0-BD3B-00A0C911CE86}
const VIDEO_INPUT_CATEGORY: GUID = GUID::from_u128(0x860bb310_5d01_11d0_bd3b_00a0c911ce86);
// CLSID_FilterGraph = {E5F188C1-B7BA-11CF-BA53-0020AF0BA770}  (quartz.dll)
const CLSID_FILTER_GRAPH: GUID = GUID::from_u128(0xe5f188c1_b7ba_11cf_ba53_0020af0ba770);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let want = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "OBS Virtual Camera".to_string());
    let hold_s: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // ── 1. 枚举视频输入设备，按名字找到目标滤镜 ──
        let dev_enum: ICreateDevEnum = match CoCreateInstance(
            &CLSID_SYSTEM_DEVICE_ENUM,
            None,
            CLSCTX_INPROC_SERVER,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("CoCreateInstance(CLSID_SystemDeviceEnum) 失败: {e}");
                std::process::exit(1);
            }
        };
        let mut enum_opt: Option<IEnumMoniker> = None;
        if let Err(e) = dev_enum.CreateClassEnumerator(&VIDEO_INPUT_CATEGORY, &mut enum_opt, 0) {
            eprintln!("没有视频输入设备类别枚举器: {e}");
            std::process::exit(1);
        }
        let Some(enum_mon) = enum_opt else {
            eprintln!("系统中没有任何已注册的视频输入设备");
            std::process::exit(1);
        };

        println!("── 枚举到以下视频输入设备，寻找「{want}」 ──");
        let mut target: Option<IMoniker> = None;
        loop {
            let mut slot: [Option<IMoniker>; 1] = [None];
            if enum_mon.Next(&mut slot, None) != S_OK {
                break;
            }
            let Some(m) = slot[0].clone() else { continue };
            let name = match m.BindToStorage::<_, _, IPropertyBag>(None, None) {
                Ok(bag) => {
                    let mut var = VARIANT::default();
                    if bag
                        .Read(w!("FriendlyName"), &mut var as *mut VARIANT, None)
                        .is_ok()
                    {
                        var.to_string()
                    } else {
                        String::new()
                    }
                }
                Err(_) => String::new(),
            };
            let hit = name.contains(&want);
            println!("  • {name}{}", if hit { "   ← 命中" } else { "" });
            if hit && target.is_none() {
                target = Some(m);
            }
        }
        let Some(moniker) = target else {
            eprintln!("\n没找到「{want}」设备。请确认 OBS Virtual Camera 已注册（装过 OBS 即可）。");
            std::process::exit(1);
        };

        // ── 2. 绑定成滤镜实例：这一步会把 obsVirtualCam.dll 载入本进程 ──
        let filter: IBaseFilter = match moniker.BindToObject::<_, _, IBaseFilter>(None, None) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("BindToObject(IBaseFilter) 失败: {e}");
                std::process::exit(1);
            }
        };
        println!("\n[2] 滤镜实例已创建（obsVirtualCam.dll 已载入本进程）");

        // ── 3. 建图（可选）：这台机器的 CLSID_FilterGraph 未注册时会失败，
        //      那就退回到"直接驱动滤镜"的路子——滤镜的 Pause()/Run() 就是
        //      OBS 滤镜打开共享内存的时机，不依赖 graph 也能验证检测。
        let graph: Option<IGraphBuilder> =
            CoCreateInstance(&CLSID_FILTER_GRAPH, None, CLSCTX_INPROC_SERVER).ok();
        let mut rendered = false;
        if let Some(g) = graph.as_ref() {
            if g.AddFilter(&filter, w!("probe_cam")).is_ok() {
                // 找到输出引脚并 Render（可能因缺默认渲染器失败，不影响后续 Run）
                if let Ok(pins) = filter.EnumPins() {
                    let _ = pins.Reset();
                    loop {
                        let mut slot: [Option<IPin>; 1] = [None];
                        if pins.Next(&mut slot, None) != S_OK {
                            break;
                        }
                        let Some(p) = slot[0].clone() else { continue };
                        if matches!(p.QueryDirection(), Ok(d) if d == PINDIR_OUTPUT) {
                            match g.Render(&p) {
                                Ok(()) => println!("[3] 采集图已连接（Render 成功）"),
                                Err(e) => println!("[3] Render 失败（{e}），继续直接驱动滤镜"),
                            }
                            rendered = true;
                            break;
                        }
                    }
                }
            }
        }
        if !rendered {
            println!("[3] 未能建立/渲染采集图（本机 CLSID_FilterGraph 未注册），改为直接调用滤镜 Pause()/Run()");
        }

        // ── 4. 跑起来：这就是"应用正在使用摄像头"的状态 ──
        let mc: Option<IMediaControl> = graph.as_ref().and_then(|g| g.cast().ok());
        if let Some(m) = mc.as_ref() {
            match m.Run() {
                Ok(()) => println!("[4] IMediaControl::Run() 成功 —— 摄像头处于使用中"),
                Err(e) => println!("[4] IMediaControl::Run() 失败: {e}，退回直接驱动滤镜"),
            }
        }
        if mc.is_none() {
            match filter.Pause() {
                Ok(()) => println!("[4] IBaseFilter::Pause() 成功"),
                Err(e) => println!("[4] Pause() 失败: {e}"),
            }
            match filter.Run(0) {
                Ok(()) => println!("[4] IBaseFilter::Run(0) 成功 —— 滤镜已进入 Running 态"),
                Err(e) => println!("[4] Run() 失败: {e}"),
            }
        }

        println!("\n保持使用状态 {hold_s} 秒，期间请看 audioserver.log 的 handle count 是否变成 >= 2 ...");
        std::thread::sleep(std::time::Duration::from_secs(hold_s));

        if let Some(m) = mc.as_ref() {
            let _ = m.Stop();
        }
        let _ = filter.Stop();
        println!("已 Stop()，释放摄像头（进程退出后滤镜句柄一并关闭）");
        // 让对象在退出前析构干净
        drop(mc);
        drop(graph);
        drop(filter);
    }
}
