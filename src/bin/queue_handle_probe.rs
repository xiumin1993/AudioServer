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

#[cfg(windows)]
fn main() {
    use std::time::{Duration, Instant};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
    };

    const NAME: &str = "QoderHandleProbeSegment";

    #[repr(C)]
    #[derive(Default)]
    struct ObjectHandleInformation {
        handle_count: u32,
        access_mask: u32,
    }

    const OBJ_CLASS_HANDLE_INFORMATION: u32 = 2;

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
        let my_handle = target.0 as usize;
        let mut size = 4usize * 1024 * 1024;
        let t0 = Instant::now();
        loop {
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
            if st != 0 {
                println!("    (NtQuerySystemInformation 返回 0x{:08X})", st as u32);
                return None;
            }
            // 头部：NumberOfHandles(usize) + Reserved(usize)，之后是条目数组
            let n = usize::from_le_bytes(
                buf[0..8].try_into().ok()?
            );
            let base = 16;
            let stride = std::mem::size_of::<HandleEntryEx>();
            if base + n * stride > buf.len() {
                println!("    (快照截断：n={n} 需要 {} 字节，只有 {})", base + n * stride, buf.len());
                return None;
            }
            let mut obj: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut count = 0u32;
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
        #[repr(C)]
        struct UnicodeString {
            len: u16,
            max: u16,
            buf: *mut u16,
        }
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

    fn wstr(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn handle_count(h: HANDLE) -> u32 {
        // 留足空间：不同 Windows 版本该结构的 ACCESS_MASK 宽度/对齐可能有差异
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

    let args: Vec<String> = std::env::args().collect();

    // ── 消费者模式：打开映射并持有一段时间 ──
    if args.len() > 1 && args[1] == "holder" {
        let name = wstr(NAME);
        let hold_ms: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3000);
        unsafe {
            match OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(name.as_ptr())) {
                Ok(h) => {
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
        std::thread::sleep(Duration::from_millis(800));
        let (_, c1, ms1, _) = match count_handles_via_snapshot(h) {
            Some(v) => v,
            None => return,
        };
        println!("[2] 消费者已打开映射 → handle_count = {c1}   期望 >= 2   (扫描 {ms1}ms)");

        // 消费者退出后再查
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(300));
        let (_, c2, ms2, _) = match count_handles_via_snapshot(h) {
            Some(v) => v,
            None => return,
        };
        println!("[3] 消费者已退出 → handle_count = {c2}   期望回到 1   (扫描 {ms2}ms)");

        println!();
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
