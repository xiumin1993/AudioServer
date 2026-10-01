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
//   （`--` 后面是传给程序自己的参数：第 1 个=设备名片段，第 2 个=持有秒数。
//    从代码看 --release 不是功能要求，去掉也能跑，只是调试版慢一点。）
//
// ══════════════ 初学者导读（独立小程序，一次性验证工具）══════════════
// 1) 它验证什么问题：见上面"为什么需要它"——一句话版：证明"应用打开 OBS 虚拟
//    摄像头时，OBS 的滤镜 DLL 会真的被加载进本进程并跑起来"，从而让
//    src/vcam_obs.rs 用句柄计数判断"有没有人在看摄像头"的前提成立。
// 2) 怎么单独跑：见上面"运行"。要求：装过 OBS（滤镜 DLL 注册后才有设备可开），
//    不需要管理员权限；跑之前确保没有其他软件（OBS 本体/会议软件）占着摄像头。
// 3) 打印怎么读：按步骤编号看。枚举名单里出现 "…OBS Virtual Camera ← 命中"=
//    找到目标；[2] 滤镜实例已创建=obsVirtualCam.dll 进了本进程；[3] 两种都行——
//    "Render 成功"或"改为直接调用滤镜 Pause()/Run()"是预期的降级路径（见 121 行
//    附近注释）；[4] 出现 Run()/Run(0) 成功=摄像头进入"使用中"态。最后拿着
//    hold 秒数窗口去查 AudioServer 日志的 handle count 是否 >= 2（主干检测生效）。
// 4) 对应主干：src/vcam_obs.rs（OBS 虚拟摄像头注入引擎，mapping_handle_count
//    占用检测）；设备枚举那半段与 vcam_probe.rs 是同一套 DirectShow API。
// ── FFI 概念速查（长版见 cap_probe.rs / vcam_probe.rs 头部）：
// CLSID/GUID=128 位组件身份证；I 开头=COM 接口；S_OK=HRESULT 0 表示成功；
// CoInitializeEx 的 APARTMENTTHREADED=STA 套间（DirectShow 要求）；
// 枚举器 Next 用"1 格槽数组"逐个取；w! 宏=编译期生成 UTF-16 字符串字面量。

#[cfg(windows)]
use windows::core::{Interface, GUID, VARIANT, w};
// Interface 是个 trait（特质≈其它语言的"接口声明"，规定实现者必须有这些方法）：
// use 它进来才能对 COM 对象调 .cast()——即 QueryInterface 接口查询/升级。
#[cfg(windows)]
use windows::Win32::Foundation::S_OK;
// DirectShow 三件套：IGraphBuilder=采集图容器（把滤镜接成流水线）、
// IMediaControl=图的播放开关（Run/Stop）、IBaseFilter=单个滤镜、
// IPin=滤镜上的插脚（输出引脚往外吐画面）。
// 注：IEnumPins 这条 import 实际未被显式用到（引脚枚举返回类型由编译器推断），
// cargo 的 unused import warning 就出在这——修它属于代码改动，留给后续。
#[cfg(windows)]
use windows::Win32::Media::DirectShow::{
    IBaseFilter, ICreateDevEnum, IEnumPins, IGraphBuilder, IMediaControl, IPin, PINDIR_OUTPUT,
};
#[cfg(windows)]
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
#[cfg(windows)]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IEnumMoniker, IMoniker, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};

// CLSID_SystemDeviceEnum = {62BE5D10-60EB-11d0-BD3B-00A0C911CE86}
#[cfg(windows)]
const CLSID_SYSTEM_DEVICE_ENUM: GUID = GUID::from_u128(0x62be5d10_60eb_11d0_bd3b_00a0c911ce86);
// CLSID_VideoInputDeviceCategory = {860BB310-5D01-11d0-BD3B-00A0C911CE86}
#[cfg(windows)]
const VIDEO_INPUT_CATEGORY: GUID = GUID::from_u128(0x860bb310_5d01_11d0_bd3b_00a0c911ce86);
// CLSID_FilterGraph = {E5F188C1-B7BA-11CF-BA53-0020AF0BA770}  (quartz.dll)
#[cfg(windows)]
const CLSID_FILTER_GRAPH: GUID = GUID::from_u128(0xe5f188c1_b7ba_11cf_ba53_0020af0ba770);

#[cfg(windows)]
fn main() {
    // std::env::args()=命令行参数迭代器（第 0 个是程序自身路径），collect 收成
    // Vec<String>（可增长数组）。后面是一条"取不到就用默认值"的链：
    // args.get(1) 返回 Option<&String> → cloned 去掉引用 → unwrap_or_else 兜底。
    // 默认设备名 "OBS Virtual Camera"；第 2 个参数按 u64 秒数解析，parse().ok()
    // 把解析失败也变成 None 再兜底 10 秒。hold 改短可能来不及看日志，改长只是白等。
    let args: Vec<String> = std::env::args().collect();
    let want = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "OBS Virtual Camera".to_string());
    let hold_s: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);

    // 大 unsafe 块：下面全是 COM FFI。STA 套间=这套 COM 对象按"单线程公寓"规则
    // 收发调用，DirectShow 图管理器要求这种模式（音频探针用的 MTA 见 cap_probe.rs）。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // ── 1. 枚举视频输入设备，按名字找到目标滤镜 ──
        // CoCreateInstance(类 CLSID, 无外层对象, 加载方式=进程内 DLL) → 设备枚举器。
        // match 拆 Result：失败就 eprintln（stderr，与 println 分家）+ exit(1)。
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
        // CreateClassEnumerator(类别 GUID, 出参, 保留位 0)：拿"视频输入设备"这一类
        // 的名单。出参类型是 Option<IEnumMoniker>：一类一个都没有时函数返回"成功"
        // 但出参保持 None，所以 if let Err / let-else 两个分支要分开处理。
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
        // COM 枚举器通用套路：Next 往"1 格槽数组"（[Option<T>; 1]=长度固定为 1 的
        // 数组）里放一个元素，返回 S_OK=取到一个、S_FALSE(≠S_OK)=到头了 → break。
        loop {
            let mut slot: [Option<IMoniker>; 1] = [None];
            if enum_mon.Next(&mut slot, None) != S_OK {
                break;
            }
            let Some(m) = slot[0].clone() else { continue };
            // moniker（"名字壳"）本身不带名字，要 BindToStorage 打开其属性存储，
            // 期望接口类型 IPropertyBag（键值袋），Read 键 "FriendlyName"。
            // w!(...) 宏在编译期把字面量转成 UTF-16 宽字符串（Windows API 字符串
            // 都是 UTF-16，Rust &str 是 UTF-8，必须转）。VARIANT 里的 BSTR 用
            // to_string() 取出；读不到就留空串，不影响后面继续扫名单。
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
            // 子串匹配即算命中（contains），所以传 "OBS" 也能碰上；多个命中只取第一个
            // （target.is_none() 守卫）。打印行尾的 "← 命中" 就是给读者的关键标记。
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
        // BindToObject（区别于上面只读属性的 BindToStorage）：让 moniker 按期望的
        // 接口类型 IBaseFilter 真正实例化对象——DLL 由此被 LoadLibrary 进本进程，
        // 这正是"应用在用摄像头"在系统层面发生的第一个动作。
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
        // CoCreateInstance(...).ok()：Result 转 Option、错误直接吞成 None——
        // 建不出图是预期内分支（见上方注释），所以不用 match 处理错误。
        let graph: Option<IGraphBuilder> =
            CoCreateInstance(&CLSID_FILTER_GRAPH, None, CLSCTX_INPROC_SERVER).ok();
        let mut rendered = false;
        if let Some(g) = graph.as_ref() {
            // AddFilter：把滤镜挂进图里；第二个参数是图内的显示名（w! 转 UTF-16）。
            if g.AddFilter(&filter, w!("probe_cam")).is_ok() {
                // 找到输出引脚并 Render（可能因缺默认渲染器失败，不影响后续 Run）
                // EnumPins→Reset→Next 循环 = 又见 COM 枚举器三板斧（同 IEnumMoniker）。
                // matches!(..., Ok(d) if d == PINDIR_OUTPUT)：模式匹配一步判
                // "调用成功 且 方向是输出"。Render=让图自动补一条从该引脚到
                // 渲染器的通路（真放画面需要采样渲染器在场）。
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
        // g.cast()：QueryInterface 接口查询——向图管理器要 IMediaControl 这个
        // "总开关"接口（能不能要到取决于对象实现了哪些接口，失败给 None）。
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
            // filter 本身就是媒体对象：Pause→Run 是滤镜状态机的标准走位，
            // Run(0) 的参数是"启动时间戳"（100ns 单位，0=立刻）。进入 Running 态
            // 后滤镜开始产帧——对 OBS Virtual Camera 而言就是打开共享内存。
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

// 非 Windows 平台（如 macOS）编译时给出的空实现：
// 本探针只在 Windows 上有意义，但 bin 目标必须有一个 main，
// 否则 cargo build 在其他平台会报 E0601（找不到 main）而整体失败。
#[cfg(not(windows))]
fn main() {
    println!("This probe is Windows-only.");
}
