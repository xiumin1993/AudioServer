// ── 探针：验证「命名共享内存 Section 的内核句柄计数」能否检测消费者 ──
//
// 背景：OBS Virtual Camera 协议里没有"应用在观看"的回调信号，
// 而隐私注册表 CapabilityAccessManager 只记录 UWP/Media Foundation 的访问，
// 对 DirectShow 滤镜完全无效。vcam_obs.rs 改用句柄计数检测：
//   句柄数 == 1 → 只有生产者自己 → 没有应用打开摄像头
//   句柄数 >= 2 → 至少一个别的进程 OpenFileMapping 了 → 有应用在观看
//
// 本探针在不依赖 OBS 的前提下验证这个机制：
//   1) 父进程创建唯一命名的映射 → 查句柄数（期望 1）
//   2) 父进程拉起一个"消费者"子进程（自己 + 参数 holder），子进程 OpenFileMapping
//      同一个映射并持有 3 秒
//   3) 父进程再查句柄数（期望 2），子进程退出后再查（期望回到 1）
//
// 运行：cargo run --release --bin queue_handle_probe
//   （不需要管理员权限；程序会自己再启动一个自己当"消费者"，秒级跑完自动退出。
//    手工玩法：另开终端 cargo run --release --bin queue_handle_probe -- holder 8000
//    可以让消费者多持 8 秒，方便你中途观察。）
//
// ══════════════ 初学者导读（独立小程序，一次性验证工具）══════════════
// 1) 它验证什么问题：见上面背景——一句话版：确认"给命名共享内存段数内核句柄"
//    这条路能可靠地回答"有没有第二个进程打开了这块内存"（结论：成立，判据是
//    输出末行"✅ 机制成立"；三步计数 1 → >=2 → 回 1 全对才算过）。
// 2) 对应主干哪个模块：src/vcam_obs.rs 的 mapping_handle_count 占用检测——
//    OBS 虚拟摄像头的"有没有应用在看"就是靠数这个句柄实现的。
// 3) 打印怎么读：[1] 期望 handle_count = 1；[2] 期望 >= 2；[3] 期望回到 1。
//    行尾括号里 (扫描 Xms / N 条句柄, obj=地址) 是性能与定位信息：扫描耗时反映
//    全系统句柄表大小，obj 是两个进程共享的内核对象地址（计数就是数它出现的次数）。
//    u32::MAX(4294967295) 出现 = 底层查询失败的哨兵值，不是真实句柄数。
// 4) 需要的概念速查（长版见 cap_probe.rs 头部）：
//    HANDLE=内核对象把手（进程号的"资源版"），进程退出时系统自动回收全部把手，
//      所以本探针不显式 CloseHandle；INVALID_HANDLE_VALUE=对"页文件支撑的匿名
//      内存段"说"没有现成文件，请新建"的惯用哨兵；PCWSTR=指向 UTF-16 宽字符串
//      的裸指针（Windows API 字符串格式）；NTSTATUS=内核版 HRESULT，0 为成功。

#[cfg(windows)]
fn main() {
    use std::time::{Duration, Instant};
    // PCWSTR：LPCWStr/"const 宽字符指针"的 Rust 形态——只指向一块以 0 结尾的
    // UTF-16 数组，本身不拥有内存（所以那块 Vec<u16> 必须活得比它久）。
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
    };

    // 命名段的名字必须全系统唯一且不与其他探针撞车；改成别的名字要两个模式
    // （生产者/holder 消费者）同步改，否则互相找不到。
    const NAME: &str = "QoderHandleProbeSegment";

    // ── 方案 A（直接问内核对象）的数据结构 ──
    // #[repr(C)]：命令编译器"按 C 语言规则排布字段"。默认布局 Rust 会重排字段，
    // 而我们要把 NtQueryObject 写回来的字节流按这个布局解读，顺序必须钉死。
    // 注：ObjectHandleInformation/handle_count 这一套是方案 A 的备用路径，
    // 主干最终用方案 B（快照法），cargo 的 3 条 never used warning 就来自这里
    // ——留作交叉验证工具，删它们属于代码改动，不做。
    #[repr(C)]
    #[derive(Default)]
    struct ObjectHandleInformation {
        handle_count: u32,
        access_mask: u32,
    }

    // NtQueryObject 的"问什么"编号：2=ObjectHandleInformation（该对象的句柄数）。
    const OBJ_CLASS_HANDLE_INFORMATION: u32 = 2;

    // extern "system"：声明"这函数不在 Rust 代码里，去链接的 DLL 找"；
    // #[link(name = "ntdll")] 指定 ntdll.dll。ntdll 是未公开的内核垫片 API，
    // 签名要自己照 Windows 内部文档抄——这也是整个探针要手写这么多结构体的原因。
    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryObject(
            handle: *mut core::ffi::c_void,
            object_information_class: u32,
            object_information: *mut core::ffi::c_void,
            object_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    /// 方案 B：SystemExtendedHandleInformation 全系统句柄表扫描
    /// （Sysinternals Handle.exe 的算法）：
    ///   1) 在快照里找到 {Pid == 自己, Handle == 我们的映射句柄} 那条 → 得到内核对象地址
    ///   2) 统计整个快照里 ObjectId 相同的条数 = 全系统持有该映射的句柄数
    // 快照条目：系统全句柄表里"一行记录"的内存布局（同样必须 repr(C) 钉死）。
    // 字段顺序照 Windows 内部结构 SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX 抄。
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct HandleEntryEx {
        object_id: *mut core::ffi::c_void,
        unique_process_id: usize,
        handle_value: usize,
        granted_access: u32,
        creator_back_trace_index: u16,
        object_type_index: u16,
        handle_attributes: u32,
        reserved: u32,
    }

    // NtQuerySystemInformation 的类别号：64=SystemExtendedHandleInformation
    // （全系统句柄表快照）。0xC0000004=STATUS_INFO_LENGTH_MISMATCH："缓冲区不够大，
    // 需要的字节数写在出参里，重来一次"——ntdll 查询函数的通用套路。
    const SYS_EXT_HANDLE_INFO: u32 = 64;
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQuerySystemInformation(
            class: u32,
            buffer: *mut core::ffi::c_void,
            length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    /// 返回 (本进程该句柄对应的对象地址, 全系统持有同一对象的句柄数, 扫描耗时ms, 条目总数)
    unsafe fn count_handles_via_snapshot(target: HANDLE) -> Option<(*mut core::ffi::c_void, u32, u128, usize)> {
        let my_pid = std::process::id() as usize;
        // HANDLE 是元组结构体包着一个裸指针：target.0 取出内部指针值，
        // 再转 usize 才能和快照里的 handle_value 列做整数比较。
        let my_handle = target.0 as usize;
        // 初始缓冲 4MB：一次装下全系统句柄表的大多数情况。装不下时靠下面的
        // LENGTH_MISMATCH 分支按"实际所需 +1MB"重试——首值改小只是多循环几轮，
        // 改大则白占内存；+1MB 是给扫描期间新增句柄留的余量。
        let mut size = 4usize * 1024 * 1024;
        let t0 = Instant::now();
        loop {
            // vec![0u8; size]：分配 size 字节、全零的可增长缓冲区（Vec<u8>）。
            let mut buf = vec![0u8; size];
            let mut ret: u32 = 0;
            let st = NtQuerySystemInformation(
                SYS_EXT_HANDLE_INFO,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                size as u32,
                &mut ret,
            );
            if st == STATUS_INFO_LENGTH_MISMATCH {
                size = (ret as usize) + (1 << 20);
                continue;
            }
            // NTSTATUS 约定 0=成功；非 0 打印十六进制错误码并放弃（返回 None，
            // 调用方拿 Option 拆包决定去留——? 运算符同款语义这里用了显式 match）。
            if st != 0 {
                println!("    (NtQuerySystemInformation 返回 0x{:08X})", st as u32);
                return None;
            }
            // 头部：NumberOfHandles(usize) + Reserved(usize)，之后是条目数组
            // buf[0..8].try_into() 把切片转成 [u8;8] 定长数组（.ok()? 转换失败/
            // 快照太短就直接让本函数返回 None），from_le_bytes 按小端拼回 usize。
            let n = usize::from_le_bytes(
                buf[0..8].try_into().ok()?
            );
            // base=16：头部两枚 usize（x64 每个 8 字节）共 16 字节，条目数组紧随其后。
            let base = 16;
            // stride=步长：一条 HandleEntryEx 占多少字节，用 size_of 算而非手写，
            // 保证和结构体定义永远一致。
            let stride = std::mem::size_of::<HandleEntryEx>();
            if base + n * stride > buf.len() {
                println!("    (快照截断：n={n} 需要 {} 字节，只有 {})", base + n * stride, buf.len());
                return None;
            }
            let mut obj: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut count = 0u32;
            // 两遍合一趟：先找到"我的进程 × 我的句柄"那行拿到对象地址 obj，
            // 之后每见一行 object_id==obj 就 +1 —— 得到全系统共享该对象者数。
            // read_unaligned：按 C 布局"抓"出一条记录（不要求地址对齐，避免 UB）。
            for i in 0..n {
                let off = base + i * stride;
                let e: HandleEntryEx = std::ptr::read_unaligned(
                    buf.as_ptr().add(off) as *const HandleEntryEx,
                );
                if obj.is_null() {
                    if e.unique_process_id == my_pid && e.handle_value == my_handle {
                        obj = e.object_id;
                    }
                }
                if !obj.is_null() && e.object_id == obj {
                    count += 1;
                }
            }
            if obj.is_null() {
                println!("    (快照里没找到自己进程的映射句柄)");
                return None;
            }
            return Some((obj, count, t0.elapsed().as_millis(), n));
        }
    }

    /// 方案 A 的 sanity check：class 1 = ObjectNameInformation，能拿到 \BaseNamedObjects\...
    const OBJ_CLASS_NAME_INFORMATION: u32 = 1;

    unsafe fn object_name(h: HANDLE) -> String {
        // 内核的 UNICODE_STRING 三段式：字节长度 + 缓冲区容量 + UTF-16 数据指针
        // （与 Rust String/Windows 宽指针都不同，须原样定义才能解读返回缓冲）。
        #[repr(C)]
        struct UnicodeString {
            len: u16,
            max: u16,
            buf: *mut u16,
        }
        // 2048 字节的返回缓冲：装一个对象路径绰绰有余；对象真超了会得到
        // LENGTH_MISMATCH 非零状态，走下面的 <NTSTATUS ...> 打印而不是越界。
        let mut out = [0u8; 2048];
        let mut ret: u32 = 0;
        let st = NtQueryObject(
            h.0,
            OBJ_CLASS_NAME_INFORMATION,
            out.as_mut_ptr() as *mut core::ffi::c_void,
            out.len() as u32,
            &mut ret,
        );
        if st != 0 {
            return format!("<NTSTATUS 0x{:08X}>", st as u32);
        }
        let us = &*(out.as_ptr() as *const UnicodeString);
        let s = std::slice::from_raw_parts(us.buf, us.len as usize / 2);
        String::from_utf16_lossy(s)
    }

    // 把 &str(UTF-8) 转成"UTF-16 + 结尾 0"的 Vec<u16>：Windows 宽字符串的标准
    // 构造法（encode_utf16 逐字符转码，chain(0) 补终止符）。调用处把
    // name.as_ptr() 装进 PCWSTR——所有权在 Vec 手里，所以 name 必须先于 PCWSTR 使用处存在。
    fn wstr(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // 方案 A：直接向"这个对象"要句柄数（class=2）。返回值不是 Result 而是
    // NTSTATUS 裸整数，所以要手写 st == 0 判成功；失败时返回 u32::MAX 当
    // "坏值哨兵"——打印出来一眼能认出 4294967295 不是真实计数。
    unsafe fn handle_count(h: HANDLE) -> u32 {
        // 留足空间：不同 Windows 版本该结构的 ACCESS_MASK 宽度/对齐可能有差异
        // （64 字节缓冲区是给版本差异兜底，改 8 字节在老结构布局下可能截断）。
        let mut buf = [0u8; 64];
        let mut ret: u32 = 0;
        let st = NtQueryObject(
            h.0,
            OBJ_CLASS_HANDLE_INFORMATION,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            &mut ret,
        );
        if st == 0 {
            u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
        } else {
            println!("    (NtQueryObject 返回 NTSTATUS 0x{:08X}, ret_len={ret})", st as u32);
            u32::MAX
        }
    }

    // 同一个可执行文件跑两种角色：带 "holder" 参数=消费者，否则=生产者。
    // （env::args 第 0 个是程序路径，所以取 args[1]。）
    let args: Vec<String> = std::env::args().collect();

    // ── 消费者模式：打开映射并持有一段时间 ──
    if args.len() > 1 && args[1] == "holder" {
        let name = wstr(NAME);
        // 第三个参数=持有毫秒数，默认 3000；生产者拉子进程时显式传 4000。
        // 太短会让父进程"第二次计数"赶在前面读到 1（假阴性），改大只是拖长实验。
        let hold_ms: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3000);
        unsafe {
            // OpenFileMappingW(权限, 名字是否可继承=一般 false, 段名字)：
            // 按名字"认领"别的进程建的段——这一步成功与否就是整个检测的题眼。
            // FILE_MAP_ALL_ACCESS.0：元组访问取出内部的 u32 权限掩码值。
            match OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(name.as_ptr())) {
                Ok(h) => {
                    // MapViewOfFile：把段映射进本进程地址空间才能真读写；
                    // 最后两个 0 = 从文件偏移 0 映射整段。探针从不碰内容，
                    // 建视图只为证明"和真实消费者一样握到了可用的把手"。
                    // .Value 取出视图裸指针，{:p} 按指针格式打印。
                    let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, 0).Value;
                    println!("[holder pid={}] 已打开映射并建立视图 (view={:p})，持有 {}ms",
                        std::process::id(), view, hold_ms);
                    std::thread::sleep(Duration::from_millis(hold_ms));
                    println!("[holder pid={}] 退出（句柄随之释放）", std::process::id());
                }
                Err(e) => println!("[holder pid={}] OpenFileMapping 失败: {e}", std::process::id()),
            }
        }
        return;
    }

    // ── 生产者模式 ──
    println!("=== 句柄计数检测探针 ===\n");
    let name = wstr(NAME);
    let size = 1 << 20; // 1MB 足够

    unsafe {
        // CreateFileMappingW：第一个参数给 INVALID_HANDLE_VALUE 表示"不 backing 真
        // 文件、用页文件新建一块"；传了名字段就进全局命名对象表（\BaseNamedObjects\），
        // 别的进程才能按名 Open。PAGE_READWRITE=段属性；第 4/5 参数=大小的高/低 32 位。
        let h = match CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            size as u32,
            PCWSTR(name.as_ptr()),
        ) {
            Ok(h) => h,
            Err(e) => {
                println!("CreateFileMappingW 失败: {e}");
                return;
            }
        };

        println!("    对象名 = {}（ABI/句柄有效性 sanity check）", object_name(h));

        let (obj, c0, ms0, total) = match count_handles_via_snapshot(h) {
            Some(v) => v,
            None => return,
        };
        println!(
            "[1] 只有生产者自己 → handle_count = {c0}   期望 1   (扫描 {ms0}ms / {total} 条句柄, obj={obj:p})"
        );

        // 拉起消费者
        // current_exe()=本程序自己的路径（可能还没运行完呢就自己拉自己）；
        // Command::new(exe).arg("holder").arg("4000") = 以消费者角色重启自己、
        // 持有 4000ms（必须盖过父进程"启动+800ms+一次全表扫描"的间隔，留了余量；
        // 改小到和扫描耗时同量级会随机测不到 [2]）。spawn=不等它跑完、后台跑。
        let exe = std::env::current_exe().unwrap();
        let child = std::process::Command::new(exe).arg("holder").arg("4000").spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                println!("无法启动消费者子进程: {e}");
                return;
            }
        };

        // 等消费者把句柄建起来
        // 800ms 是给"进程启动 + OpenFileMapping"的宽限：改太短可能在子进程还没
        // 打开时就计数，读到 1 会误判"机制失效"（[2] 行低于期望）。
        std::thread::sleep(Duration::from_millis(800));
        let (_, c1, ms1, _) = match count_handles_via_snapshot(h) {
            Some(v) => v,
            None => return,
        };
        println!("[2] 消费者已打开映射 → handle_count = {c1}   期望 >= 2   (扫描 {ms1}ms)");

        // 消费者退出后再查
        // child.wait() 阻塞到子进程结束（进程一死，它手里的句柄被系统强制回收）；
        // 再睡 300ms 是给内核回收/快照稳定留的余量，改 0 有几率读到旧值。
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(300));
        let (_, c2, ms2, _) = match count_handles_via_snapshot(h) {
            Some(v) => v,
            None => return,
        };
        println!("[3] 消费者已退出 → handle_count = {c2}   期望回到 1   (扫描 {ms2}ms)");

        println!();
        // 三步全对才算机制成立：c0 精确 1、c1 至少 2、c2 精确回 1。
        // （&&  短路逻辑与；if/else 在这里当表达式用，直接嵌进打印参数。）
        let pass = c0 == 1 && c1 >= 2 && c2 == 1;
        println!(
            "结论：{}（检测逻辑可用 = 句柄数 >= 2 判为有应用在观看）",
            if pass { "✅ 机制成立" } else { "❌ 机制不成立，需换方案" }
        );
    }
}

#[cfg(not(windows))]
fn main() {
    println!("本探针仅在 Windows 上有意义");
}
