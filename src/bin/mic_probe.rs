// mic_probe.rs — 调试工具：直接录制 "CABLE Output" 3 秒并报告电平
// 用途：验证 AudioServer 的 WASAPI 注入引擎是否真的把 PCM 写进了虚拟声卡。
// 用法：先让上行数据流动（Node 正弦波或手机），再运行 cargo run --bin mic_probe
//
// ══════════════ 初学者导读（独立小程序，一次性验证工具）══════════════
// 【它验证什么】AudioServer 的 WASAPI 注入引擎是否真的把手机 PCM 写进了虚拟
//   声卡：本探针站在纯第三方视角，用 WASAPI 共享模式录 CABLE Output 共 8 秒并
//   算电平。peak > 0.001 → 注入链路通；全零 → 引擎没写数据或手机数据没到达。
// 【怎么跑】先启动 AudioServer（GUI 或 cargo run --bin server），让手机
//   （PCAssistant App）进入麦克风模式并出声，然后 cargo run --bin mic_probe。
//   不需要管理员权限，但必须装有 VB-CABLE 驱动。跑完自动退出。
// 【输出怎么读】capture[i]=… 是逐台录音设备名单；mix format 行是设备混音格式；
//   关键是两行 RESULT：第一行含前 3 秒冷启动（探针触发手机开麦→首包到达的
//   爬坡期），RESULT(warm, after 3s) 只统计后 5 秒稳态——以 warm 行为准。
//   rms/peak 是 0~1 的线性电平，括号里的 dBFS 是对数刻度（0=满刻度，越小越轻，
//   -60 dBFS 以下≈静音）。最后一行 => 开头是程序直接给出的人话结论。
// 【对应主干】src/mic_out.rs（虚拟麦克风注入引擎）。它的 capture_monitor 用
//   同样的"打开 CABLE Output 捕获"思路做占用检测；本探针跑起来时，
//   cap_probe/session_probe 也能观察到"有应用在录"来交叉验证。
// ── FFI 背景速查（与 cap_probe.rs 头部相同的一套概念）：
// windows crate=微软官方 FFI 绑定；I 开头=COM 接口；HRESULT==S_OK(0) 为成功，
// crate 已包成 Result；CoInitializeEx 必须先调；Windows 字符串是 UTF-16 宽字符。

#[cfg(windows)]
fn main() {
    // Instant=秒表，用来给 8 秒录音窗口和"前 3 秒冷启动"分段计时。
    use std::thread::sleep;
    use std::time::{Duration, Instant};
    use windows::core::GUID;
    // AUDCLNT_SHAREMODE_SHARED=共享模式：走系统混音器，允许多个程序同时用设备
    //   （这正是普通录音软件的工作方式，与 cap_probe 的 EXCLUSIVE 对照）。
    // IAudioCaptureClient=捕获客户端接口，GetService 从 IAudioClient 上取下来，
    //   负责一段一段递给你录音数据。
    // 注：IMMDevice 同样是未显式用到的 import（warning 保留不修）。
    use windows::Win32::Media::Audio::{
        eCapture, IAudioCaptureClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, IAudioClient,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    // 整个函数体一个大 unsafe：全是 FFI 调用与裸指针操作（详见文件头导读）。
    // 提示：cargo 会报 3 处 "unnecessary unsafe block"——函数体内层又嵌套了几个小
    // unsafe（读格式、拼切片、ReleaseBuffer），冗余但无害；去掉它们属于代码改动，留给后续。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // 和 cap_probe 一样的开场：实例化设备枚举器 → 列出所有活动的录音端点。
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).expect("COM enum");
        let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).expect("enum");
        let count = collection.GetCount().unwrap();
        // Option<T>：Rust 表达"可能没有值"的类型（Some(设备) 或 None），
        // 相当于其他语言里的 nullable，但编译器强制你使用前先拆包。
        let mut target: Option<IMMDevice> = None;
        // pid=14 + 那串固定 GUID = 设备友好名属性键（解释见 cap_probe.rs 同段落）。
        const PKEY_NAME: PROPERTYKEY = PROPERTYKEY {
            fmtid: GUID::from_values(
                0xa45c254e, 0xdf1c, 0x4efd, [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
            ),
            pid: 14,
        };
        // 逐台设备读名字（PROPERTYVARIANT 按字节手读 UTF-16 指针，原理详见
        // cap_probe.rs 对应注释），打印名单并挑出 CABLE OUTPUT 那台。
        // 注意：循环没有 break，最后一台匹配上的胜出——正常只有一台。
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
        // expect：Option/Result 为 None/Err 时直接 panic。走到这里说明 VB-CABLE 没装好，
        // 探针没有存在的意义，panic 出这句话就是给用户看的诊断。
        let device = target.expect("CABLE Output not found");
        // Activate 把设备升级成 IAudioClient（内部是 COM QueryInterface 接口查询）。
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).expect("activate");
        // GetMixFormat：共享模式录音必须用设备自己的混音格式初始化，先要过来；
        // 这块内存按约定用完要 CoTaskMemFree 归还（下面第 67 行附近）。
        let fmt_ptr = client.GetMixFormat().expect("mix format");
        // WAVEFORMATEX 是 1 字节对齐的 packed 结构，字段必须 read_unaligned 拷贝
        // （字段可能落在未对齐地址，直接引用是未定义行为，只能按值"抓"出来）。
        // 外层这个 unsafe 与函数体的大 unsafe 重复，就是 cargo 提示
        // "unnecessary unsafe block" 的地方之一——冗余无害，删它属于代码改动，保留。
        let (rate, chans, bits, tag) = unsafe {
            (
                core::ptr::read_unaligned(core::ptr::addr_of!((*fmt_ptr).nSamplesPerSec)),
                core::ptr::read_unaligned(core::ptr::addr_of!((*fmt_ptr).nChannels)),
                core::ptr::read_unaligned(core::ptr::addr_of!((*fmt_ptr).wBitsPerSample)),
                core::ptr::read_unaligned(core::ptr::addr_of!((*fmt_ptr).wFormatTag)),
            )
        };
        // 采样率（常见 44100/48000）、声道数、位深。tag=3 表示 IEEE float，
        // tag=1 是 16-bit PCM——决定下面按 f32 还是 i16 解读字节流。
        println!("mix format: {}Hz {}ch {}-bit tag=0x{:X}", rate, chans, bits, tag);
        // Initialize(共享模式…)：注册一个捕获客户端。共享模式下缓冲时长参数由系统
        // 接管（传 0 即可），这也是与 cap_probe 独占初始化的关键差异。
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, Default::default(), 0, 0, fmt_ptr, None)
            .expect("init");
        CoTaskMemFree(Some(fmt_ptr as *mut _));
        // GetService：从 IAudioClient 上取"捕获服务"，得到每轮搬数据的把手。
        let capture: IAudioCaptureClient = client.GetService().expect("capture client");
        client.Start().expect("start");

        let is_f32 = bits == 32;
        let ch = chans as usize;
        let t0 = Instant::now();
        // 全窗口统计量：sum_sq=平方和、peak=绝对值峰值、n=样本计数（都是可变局部变量）。
        // f64 累积浮点能量，u64 计数——量小无所谓，探针不关心精度。
        let (mut sum_sq, mut peak, mut n) = (0f64, 0f64, 0u64);
        // 分段统计：前 3 秒是"探针触发唤醒→手机开麦→首包到达"的冷启动期，
        // 只统计后 5 秒的电平，才能真实反映稳态链路是否通
        // （8s-3s=5s 稳态窗；3s 改短会把爬坡噪声算进结论，改长则稳态样本变少。
        //  8s 总时长本身是"手机开麦+传输+稳态"够用即可的经验值，改长只是更磨人。）
        let (mut sum_sq_late, mut peak_late, mut n_late) = (0f64, 0f64, 0u64);
        while t0.elapsed() < Duration::from_secs(8) {
            // GetNextPacketSize：问"驱动攒了几段新数据"。0 表示暂无，睡 10ms 再看——
            // 10ms 轮询粒度对电平统计绰绰有余，改 1ms 只是空转更热的 CPU。
            let size = capture.GetNextPacketSize().unwrap_or(0);
            if size == 0 {
                sleep(Duration::from_millis(10));
                continue;
            }
            // GetBuffer 三个出参：数据指针、帧数（一帧=各声道同时一个样本）、
            // 标志位（可能带静音/丢弃标记，本探针不区分照单全收）。
            let mut data_ptr: *mut u8 = core::ptr::null_mut();
            let mut frames_read = 0u32;
            let mut flags = 0u32;
            capture
                .GetBuffer(&mut data_ptr, &mut frames_read, &mut flags, None, None)
                .expect("buffer");
            let frames = frames_read as usize;
            let late = t0.elapsed().as_secs() >= 3;
            // 把"裸指针"重组成 Rust 切片 &[u8]：总字节数 = 帧数 × 声道数 × 每样本字节
            // （f32 是 4 字节，i16 是 2 字节）。unsafe 的代价：长度算错就越界读。
            // 外层 unsafe 同样属于 cargo 提示 redundant 的那几处之一，保留。
            let data = unsafe {
                std::slice::from_raw_parts(data_ptr, frames * ch * if is_f32 { 4 } else { 2 })
            };
            if is_f32 {
                // chunks_exact(4)：按 4 字节不重叠切块；step_by(ch)：多声道时只看左声道。
                // from_le_bytes：小端 4 字节 → f32（WASAPI 与 x86 都是小端）。
                for s in data
                    .chunks_exact(4)
                    .step_by(ch)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                {
                    // WASAPI 共享模式浮点样本约定在 [-1.0, 1.0]，直接当电平用。
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
                // i16 路径：2 字节 → 有符号整数 → 除以 32768 归一到 [-1, 1) 当电平。
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
            // GetBuffer/ReleaseBuffer 必须成对：不 Release，驱动认为你还在读，
            // 下一段数据永远不给（外层 unsafe 也是 cargo 提示 redundant 处，保留）。
            unsafe { capture.ReleaseBuffer(size).expect("release") };
        }
        // RMS=均方根（连续能量的"有效值"电平），peak=最大瞬时电平。
        let rms = if n > 0 { (sum_sq / n as f64).sqrt() } else { 0.0 };
        let rms_late = if n_late > 0 { (sum_sq_late / n_late as f64).sqrt() } else { 0.0 };
        // dBFS=相对满刻度的分贝：20*log10(电平)。0=满刻度，-20≈音量 10%，
        // max(1e-9) 防止 log10(0) 出 -inf。
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
        // 判定阈值 0.001（约 -60 dBFS）：低于此视为底噪/静音。调大容易把轻说话
        // 误判成静音，调小容易被电流声误报"有信号"。
        if peak_late > 0.001 {
            println!("=> CABLE Output HAS SIGNAL (injection path OK)");
        } else {
            println!("=> CABLE Output SILENT — server injection is broken or phone data not arriving");
        }
    }
}

// 非 Windows 平台占位 main：bin 目标必须有 main，否则该平台 cargo 报 E0601。
#[cfg(not(windows))]
fn main() {
    println!("Windows only");
}
