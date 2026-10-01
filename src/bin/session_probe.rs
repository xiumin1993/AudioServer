// session_probe.rs — 验证"音频会话枚举"检测法：
// 每 500ms 枚举 CABLE Output 端点上的音频会话数。
// 有应用在录该麦克风 → 会话数 > 0（录音软件在混音器里可见的原理相同）。
//
// ══════════════ 初学者导读（独立小程序，一次性验证工具）══════════════
// 【它验证什么】"音频会话枚举"检测法能否发现"有应用在录 CABLE Output"。
//   结论已在主干落地时实测确认（见 src/mic_out.rs capture_monitor 段落注释）：
//   此法不依赖虚拟声卡驱动配合、由 WASAPI 系统层自己维护，虚拟设备同样有效；
//   而更早的 v1 方案（PKEY 占用标志）VB-CABLE 从不上报、永远检测不到占用。
//   运行本探针可在这台机器上随时复验该结论。
// 【怎么跑】cargo run --bin session_probe —— 不需要管理员权限，需要装有
//   VB-CABLE。另开 cargo run --bin mic_probe（或任何录音软件选 CABLE Output
//   为输入）即可观察 sessions 数从 0 变 1。Ctrl+C 退出。
// 【输出怎么读】每 500ms 一行：sessions=N [session0(AudioSessionStateActive,
//   sys=false), ...]。N>0 且某个会话 state 是 Active = 有应用在录（这就是关键
//   结论）；sessions=0 [] = 没人录；device-gone = 设备没枚举到（驱动异常）。
//   sys=true 是系统声音占位会话，判占用时应忽略。{:?} 是 Debug 格式化，
//   会把 Vec 打印成 [..,..] 列表（{:#?} 则是带换行缩进的豪华版）。
// 【对应主干】src/mic_out.rs 的 windows_impl::capture_monitor —— 线上版检测
//   与本探针逻辑相同，只是轮询周期不同（那边为了省电降到几百 ms 级别）。

#[cfg(windows)]
fn main() {
    use std::thread::sleep;
    use std::time::Duration;
    // Interface 这个 trait（特质=像"接口声明"的共享行为约定）在这里被 use 进来，
    // 是为了能调用 .cast()——COM 的 QueryInterface 在 windows-rs 里的样子：
    // 把"一个 COM 对象"从它实现的接口 A 升级到接口 B。GUID=128 位全局唯一标识。
    use windows::core::{GUID, Interface};
    // IAudioSessionManager2=会话管理器接口：挂在每台设备上，能列出"此刻谁在
    // 用这台设备"的会话（音量合成器里能看到的"正在录音的应用"就是同一份数据）。
    // IMMDevice=设备接口本体，下面 60 行左右有显式类型标注会用到它。
    use windows::Win32::Media::Audio::{
        eCapture, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    // 整个函数体一个大 unsafe：全是 COM FFI 与裸指针（概念速查见 cap_probe.rs 头部）。
    unsafe {
        // CoInitializeEx：线程先"登记进 COM 公寓"（MTA 套间）才能调一切 COM 对象。
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // 实例化设备枚举器（CLSID MMDeviceEnumerator → 接口 IMMDeviceEnumerator）。
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).expect("COM enum");
        // 设备友好名属性键（类别 GUID 固定 + pid=14 表示 FriendlyName）。
        const PKEY_NAME: PROPERTYKEY = PROPERTYKEY {
            fmtid: GUID::from_values(
                0xa45c254e, 0xdf1c, 0x4efd, [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
            ),
            pid: 14,
        };

        // 每 500ms 一轮：主干轮询是几百 ms 级，这里取 500ms 与 cap_probe 对齐，
        // 便于两个探针并排对比灵敏度；改 50ms 更灵敏但 COM 枚举频率×10 白耗 CPU。
        loop {
            let collection = enumerator
                .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
                .expect("enum");
            let count = collection.GetCount().unwrap();
            // String 是拥有所有权的 UTF-8 字符串；to_string() 把 &str 字面量变成它，
            // 因为下面循环里可能要覆盖内容（默认值 device-gone=连设备都没找到）。
            let mut line = "device-gone".to_string();
            for i in 0..count {
                // collection.Item(i) 返回的正是 IMMDevice；显式标注类型只是为了读码清晰。
                let device: IMMDevice = collection.Item(i).unwrap();
                // 读设备友好名：PROPERTYVARIANT 是 C union，头 2 字节是值类型 vt，
                // vt==31(VT_LPWSTR) 时偏移 8 处是 UTF-16 宽字符串指针（Windows 字符串
                // 都是 UTF-16 宽字符，须 from_utf16_lossy 转成 Rust 的 UTF-8 String）。
                // 逐字节手读的完整解释见 cap_probe.rs 同一代码段。
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
                // 只在 CABLE Output 这台设备上做检测，其他录音设备跳过。
                if !name.to_uppercase().contains("CABLE OUTPUT") {
                    continue;
                }
                // Activate：设备 → 会话管理器接口（内部是 QueryInterface 接口查询）。
                // 用 match 拆 Result：失败时把错误文本塞进 line 打印出来而不是 panic，
                // 探针要能容忍驱动偶尔抽风。
                let mgr: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                    Ok(m) => m,
                    Err(e) => {
                        line = format!("activate session mgr failed: {}", e);
                        break;
                    }
                };
                // GetSessionEnumerator → 数出这台设备上系统登记的会话个数。
                // 会话=Windows 给"每个应用×每个设备"组合建的记账对象。
                let enumerator_sessions = mgr.GetSessionEnumerator().unwrap();
                let n = enumerator_sessions.GetCount().unwrap();
                // Vec<T>=可增长的动态数组（类比其它语言的 ArrayList）；
                // 这里攒每个会话的一小段人话描述，最后用 {:?} 整个打印成列表。
                let mut desc = Vec::new();
                for k in 0..n {
                    if let Ok(s) = enumerator_sessions.GetSession(k) {
                        // s.cast()：COM QueryInterface——基础接口 IAudioSessionControl
                        // 升级到 Control2（拿状态、查系统声音会话都靠 2 版）。
                        // 升级失败返回 Err，用 Result 显式接住（老驱动也可能只给 1 版）。
                        let s2: Result<windows::Win32::Media::Audio::IAudioSessionControl2, _> =
                            s.cast();
                        if let Ok(s2) = s2 {
                            // GetState：Active(正在出声/收声) / Expired / Inactive。
                            // unwrap_or_default：失败就用枚举默认值，探针不因个别会话报错而崩。
                            let state = s2.GetState().unwrap_or_default();
                            // IsSystemSoundsSession：系统声音占位会话（永远算不上"应用在录"）。
                            // .is_ok() 只表示"查询这个动作成功了"，并非"它是系统会话"——
                            // 该 API 对非系统会话返回错误码，所以 is_ok()==true 才是系统会话。
                            let sys = s2.IsSystemSoundsSession().is_ok();
                            desc.push(format!("session{}({:?},sys={})", k, state, sys));
                        }
                    }
                }
                // 这就是本轮的关键结论行：sessions=N 后跟逐会话描述。
                line = format!("sessions={} {:?}", n, desc);
                break;
            }
            println!("{}", line);
            sleep(Duration::from_millis(500));
        }
    }
}

// 非 Windows 平台占位 main：bin 目标必须有 main，否则该平台 cargo 报 E0601。
#[cfg(not(windows))]
fn main() {}
