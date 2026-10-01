// cap_probe.rs — 验证"独占试开"检测法：
// 每 500ms 尝试以 EXCLUSIVE 模式初始化 CABLE Output 捕获端。
//   成功            → 没有应用在录（立即释放）
//   AUDCLNT_E_DEVICE_IN_USE → 有应用正在录
// 运行时并行开一个共享捕获（mic_probe）即可模拟"应用在录"。
//
// ══════════════ 初学者导读（独立小程序，不参与正常启动）══════════════
// 【它验证什么】能否用"独占试开"判断有没有应用在录 CABLE Output（VB-CABLE
//   虚拟声卡的录音端）：以 EXCLUSIVE 模式调 IAudioClient::Initialize，
//   成功=设备空闲（立刻释放）；失败且错误码 AUDCLNT_E_DEVICE_IN_USE
//   (0x8889000A)=有应用正在录。结论以本机实测打印为准，代码只反映方法本身。
//   注意：主干最终采用的是"会话枚举"法（见 session_probe / src/mic_out.rs 的
//   capture_monitor），本探针是并行的另一条检测思路，留作交叉验证。
// 【怎么跑】cargo run --bin cap_probe —— 不需要管理员权限，但机器上必须装着
//   VB-CABLE 驱动；想看到"占用"态，另开一个终端跑 cargo run --bin mic_probe
//   模拟应用在录。Ctrl+C 退出。
// 【输出怎么读】每 500ms 一轮，关键行有三类：
//   IDLE (exclusive open OK) = 此刻没人在录；
//   hr=0x8889000A ...        = 被占用（这就是检测目标）；
//   device-gone              = 连 CABLE Output 都没枚举到（驱动没装/被禁用）。
//   IsFormatSupported(exclusive) 返回 0x00000000(=S_OK) 表示驱动承认该格式，
//   只是辅助信息，不代表占用与否。
// 【对应主干】src/mic_out.rs（虚拟麦克风注入引擎 + 捕获占用检测）。
//
// ── 读这个文件需要的 Windows FFI 背景（一次讲清，后面文件同理）──
// · windows crate：微软官方维护的 FFI 绑定库，把 Win32 的 C 函数和 COM 接口
//   翻译成了 Rust 签名，不需要自己手写 extern 声明。
// · COM 接口命名：一律 I 开头（I=m）。IMMDevice=多媒体设备、IMMDeviceEnumerator
//   =设备枚举器、IAudioClient=音频客户端。"I+名词"读作"某某接口"。
// · unsafe 块：调用 Windows API 时编译器无法保证传入的指针/句柄有效，
//   这份责任由程序员口头担保，担保动作就是写 unsafe。
// · HRESULT：Win32 的统一返回值（32 位整数，0x00000000=S_OK 表示成功，
//   最高位为 1 表示失败）。windows crate 已把它包成 Rust 的 Result：Ok=成功，
//   Err(windows::core::Error) 里能用 .code() 取回原始 HRESULT。

// 条件编译：下面整个 main 只在 Windows 上存在；非 Windows 用文件末尾的空 main 占位
#[cfg(windows)]
fn main() {
    // std 是 Rust 标准库。sleep + Duration 用来做 500ms 一轮的轮询节奏。
    use std::thread::sleep;
    use std::time::Duration;
    // GUID=Globally Unique Identifier，128 位"全局唯一身份证号"。
    // COM 世界不用名字而用 GUID 指认一切类和接口（见下面 0xa45c254e-... 那串）。
    use windows::core::GUID;
    // WASAPI（Windows 音频会话 API）相关声明。eCapture=枚举"采集(录音)类"端点；
    // DEVICE_STATE_ACTIVE=只要处于启用状态的设备；AUDCLNT_SHAREMODE_EXCLUSIVE=
    // 独占模式（绕过系统混音器直连设备，同一时刻只允许一个客户端）。
    // 注：IMMDevice 这一条其实没被显式用到（cargo 报 unused import warning），
    // 但删它属于改代码，不在"只加注释"范围内，留给后续。
    use windows::Win32::Media::Audio::{
        eCapture, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_EXCLUSIVE,
        IAudioClient, DEVICE_STATE_ACTIVE,
    };
    // COM 基础设施：CoInitializeEx=在本线程初始化 COM 库（任何 COM 调用之前必须做一次）；
    // COINIT_MULTITHREADED=把线程设为 MTA 套间（多线程公寓，调用不加窗口消息泵也安全，
    // 音频 API 用 MTA 即可；DirectShow 那两个探针用的 STA 见 dshow_consumer_probe 注释）。
    // CoTaskMemFree=释放"由系统分配给你读"的内存（GetMixFormat 等函数按约定这样要求）。
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    // PROPERTYKEY=设备属性的"键"，由 类别GUID + 序号pid 二元组组成。
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    // 下面整个函数体都在一个 unsafe 块里：里面全是裸指针解引用和 FFI 调用，
    // 逐行标注风险点比切十几个小 unsafe 更易读——这是一次性探针的常见写法。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // CoCreateInstance：让 COM 现场实例化一个类对象，并询问它"你实现了我想要的
        // 哪个接口"。&MMDeviceEnumerator 是类的 CLSID，返回值类型（冒号左边）声明
        // 想要的接口。CLSCTX_ALL=服务器加载位置随系统挑。
        // .expect("COM enum")：Result 是 Ok 就取出值，是 Err 就 panic 并打印这句话。
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).expect("COM enum");
        // 设备友好名的属性键：fmtid 是系统写死的"设备接口属性类别" GUID，pid=14
        // 在该类别里固定表示 FriendlyName（即设置页里看到的设备名）。
        const PKEY_NAME: PROPERTYKEY = PROPERTYKEY {
            fmtid: GUID::from_values(
                0xa45c254e, 0xdf1c, 0x4efd, [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
            ),
            pid: 14,
        };

        // 无限轮询：每圈重新枚举一遍设备（设备可能热插拔/被禁用），试开一次就睡 500ms。
        // 500ms 是"检测灵敏度 vs 白耗 CPU"的折中：改 50ms 更灵敏但枚举+试开频率×10，
        // 且独占试开太频繁可能与真实使用者抢设备产生 glitch。
        loop {
            // EnumAudioEndpoints：按"方向(eCapture=录音) + 状态(Active)"过滤，
            // 返回一个 IMMDeviceCollection（带 GetCount 计数、Item(i) 按下标取设备）。
            let collection = enumerator
                .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
                .expect("enum");
            let count = collection.GetCount().unwrap();
            // String=可拥有、可增长的字符串（UTF-8 字节向量）；&str 是"借用的字符串切片"。
            // 这里默认结论先写成 device-gone，若循环里匹配到 CABLE Output 会被覆盖。
            let mut result = "device-gone".to_string();
            for i in 0..count {
                let device = collection.Item(i).unwrap();
                // 每台设备挂着一个只读属性仓库（OpenPropertyStore，STGM_READ=以读方式打开），
                // GetValue(&PKEY_NAME) 取出的 PROPERTYVARIANT 是个 C union：
                // 头 2 字节是值类型 vt，偏移 8 处（x64 布局）才是数据。
                // vt==31 即常量 VT_LPWSTR，表示"UTF-16 宽字符串指针"。
                // 为什么是 UTF-16：Windows API 的字符串历史沿用"宽字符"（每字 2 字节），
                // 而 Rust 的 &str 是 UTF-8，两者必须经 encode_utf16 / from_utf16_lossy 互转。
                let store = device.OpenPropertyStore(STGM_READ).unwrap();
                let pv = store.GetValue(&PKEY_NAME).unwrap();
                // &pv as *const _ as *const u8：拿地址并当成字节指针，方便按偏移手动读字段。
                let pv_ptr = &pv as *const _ as *const u8;
                let vt = *(pv_ptr as *const u16);
                let mut name = String::new();
                if vt == 31 {
                    let pwsz = *(pv_ptr.add(8) as *const *const u16);
                    if !pwsz.is_null() {
                        // 宽字符串以 0 结尾：从 0 开始数到遇到终止符得到长度，
                        // 再用 from_raw_parts 把"裸指针+长度"重组成 &[u16] 切片交给
                        // String::from_utf16_lossy 转成 Rust 字符串（坏码点替换成  ）。
                        let len = (0..).take_while(|&k| *pwsz.add(k) != 0).count();
                        name = String::from_utf16_lossy(std::slice::from_raw_parts(pwsz, len));
                    }
                }
                // 只看名字含 "CABLE OUTPUT" 的那台（大小写不敏感），其余设备跳过。
                if !name.to_uppercase().contains("CABLE OUTPUT") {
                    continue;
                }
                // Activate：把"设备壳"升级为"能干活的接口"。冒号左边写 IAudioClient，
                // COM 内部就走 QueryInterface（接口查询）把对象转型成音频客户端。
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).expect("activate");
                // GetMixFormat：问驱动"你这个设备的默认混音格式是什么"，
                // 返回指向 WAVEFORMATEX 的指针，约定用完后要 CoTaskMemFree 归还。
                let fmt = client.GetMixFormat().expect("format");
                // 先问驱动：独占模式支持什么格式
                // IsFormatSupported(共享模式, 想要的格式, 出参"最接近的格式")：
                // 返回值 hr 是 HRESULT——windows crate 里用 .0 as u32 取出原始 32 位码，
                // 0x00000000=S_OK（支持），0x88890004=AUDCLNT_E_UNSUPPORTED_FORMAT 等。
                // closest 出参若被驱动填了非空指针，同样要 CoTaskMemFree 释放。
                // WAVEFORMATEX 是 1 字节对齐的 packed 结构，字段可能落在"不齐"的地址上，
                // 直接引用会未定义行为，所以逐字段 read_unaligned 按值拷出来。
                let mut closest: *mut windows::Win32::Media::Audio::WAVEFORMATEX =
                    core::ptr::null_mut();
                let hr_sup = client.IsFormatSupported(
                    windows::Win32::Media::Audio::AUDCLNT_SHAREMODE_EXCLUSIVE,
                    fmt,
                    Some(&mut closest),
                );
                // {:#?} 是 Debug 带缩进版；这里是 {:08X}=把整数印成 8 位大写十六进制，
                // 便于对着微软文档查 HRESULT 错误码。
                println!("IsFormatSupported(exclusive) -> 0x{:08X}", hr_sup.0 as u32);
                if !closest.is_null() {
                    let cf = closest;
                    let tag = core::ptr::read_unaligned(core::ptr::addr_of!((*cf).wFormatTag));
                    let ch = core::ptr::read_unaligned(core::ptr::addr_of!((*cf).nChannels));
                    let sr = core::ptr::read_unaligned(core::ptr::addr_of!((*cf).nSamplesPerSec));
                    let bits = core::ptr::read_unaligned(core::ptr::addr_of!((*cf).wBitsPerSample));
                    // tag=1 是 PCM 整型，tag=0xFFFE 是扩展 WAVEFORMATEXTENSIBLE（常见 float32）。
                    println!("  closest exclusive fmt: tag={} {}Hz {}ch {}bit", tag, sr, ch, bits);
                    CoTaskMemFree(Some(cf as *mut _));
                }
                // 独占模式缓冲时长必须对齐设备 period，用默认 period 初始化
                // GetDevicePeriod 输出的单位是"100 纳秒"（Windows 传统 tick）：
                // 10_000 tick = 1ms。period.max(3_000_000) 里的 3_000_000 = 300ms 兜底值——
                // 若驱动报的 period 异常小，用 300ms 防止 Initialize 因缓冲过小失败；
                // 改大会让"试开"占住设备更久，可能干扰真实使用者。
                let mut period: i64 = 0;
                let _ = client.GetDevicePeriod(None, Some(&mut period));
                // 本次探测的主角：以 EXCLUSIVE 试开设备。
                // 最后一个 None 是独占模式的事件句柄（用回调驱动时需要），探针不需要。
                let hr = client.Initialize(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    Default::default(),
                    period.max(3_000_000),
                    0,
                    fmt,
                    None,
                );
                CoTaskMemFree(Some(fmt as *mut _));
                // match：Rust 的分支表达式，这里对 Result 拆包——
                // Ok(()) 表示 HRESULT==S_OK，即独占初始化成功。
                result = match hr {
                    Ok(()) => {
                        // 试开成功 = 设备空闲；立刻放下引用释放
                        // windows-rs 的接口对象实现 Drop：变量离开作用域或显式 drop()
                        // 时自动调用 COM 的 Release()。不先 drop 就直接进入下一轮循环的话，
                        // 上一轮的独占锁还没放，第二轮必然自己跟自己"冲突"。
                        drop(client);
                        "IDLE (exclusive open OK)".to_string()
                    }
                    // format! 与 println! 语法相同，只是返回 String 不打印。
                    // e.code().0 as u32：从错误对象里挖回原始 HRESULT——
                    // 看到 0x8889000A (AUDCLNT_E_DEVICE_IN_USE) 就是本探针要找的"被占用"。
                    Err(e) => format!("hr=0x{:08X} {}", e.code().0 as u32, e),
                };
                break;
            }
            println!("{}", result);
            sleep(Duration::from_millis(500));
        }
    }
}

// 非 Windows 平台的占位 main：bin 目标必须有且只有一个 main，否则该平台上
// cargo 直接报 E0601 编译失败。这里什么都不做。
#[cfg(not(windows))]
fn main() {}
