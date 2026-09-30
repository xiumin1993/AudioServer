// ============================================================================
// env_check.rs —— 启动环境自检门禁（v3.5 新增）
// ----------------------------------------------------------------------------
// 为什么要有这个文件：
//   本程序自己不装任何驱动，也不往注册表写东西（绿色单 exe）。
//   但三种工作模式里，麦克风模式必须有 VB-CABLE 虚拟音频线，
//   摄像头模式必须有 Unity Capture 或 OBS Virtual Camera 任一虚拟摄像头。
//   以前缺驱动时，程序照样开主界面、照样显示"运行中"，用户连上手机才发现
//   选不到设备 —— 排查成本很高。
//   现在改成：双击 exe 先做一次只读检测；
//     · 必需项齐 → 直接进主界面（行为和以前完全一样）
//     · 必需项缺 → 主窗口只显示"环境准备"向导页，服务端线程【根本不起】，
//                  页面上只给"缺什么 + 去哪下载 + 怎么装"的文字提示，
//                  装完回来点【重新检测】。
//
// 本程序与驱动的关系，一句话：【只检测、只提示】。
//   不随包附带任何安装脚本（没有 Install.bat），不代跑任何 regsvr32 / msiexec，
//   也不写注册表、不改服务配置。要不要装、怎么装，决定权完全在用户手上。
//
// 三条检测都是"看现场、不自查注册"：
//   1) VB-CABLE —— 用 cpal 枚举声卡设备名，出现 CABLE Input / CABLE Output 即已装
//      （这是最终用户真的能选到的东西，比查注册表更接近事实）
//   2) Unity Capture / OBS 虚拟摄像头 —— 查 DirectShow 设备类别注册表里那两个
//      固定 CLSID 是否注册（本机实测：Unity Video Capture = {5C2CD55C-…}，
//      OBS Virtual Camera = {A3FCE0F5-…}，各自 InprocServer32 指向滤镜 DLL）
//   3) Windows Camera Frame Server 服务 —— 只作为【提示】，不参与放行判断：
//      浏览器枚举不到摄像头八成是它没跑，但桌面会议软件不受影响。
//
// 约定：本文件只做只读检测 + 把用户要去的官网用默认浏览器打开，
//       绝不写注册表、绝不改服务配置、绝不运行任何驱动安装脚本。
// ============================================================================

use eframe::egui;
use std::process::Command;

use crate::lang;

/// 一次自检的结果快照（全部只读，不改系统任何东西）
#[derive(Clone)]
pub struct EnvReport {
    /// VB-CABLE 虚拟音频线是否就位（麦克风模式必需）
    pub vb_cable: bool,
    /// Unity Capture 虚拟摄像头是否注册（摄像头模式可选其一）
    pub unity: bool,
    /// OBS Virtual Camera 是否注册（摄像头模式可选其一，浏览器场景需要它）
    pub obs: bool,
    /// Windows 相机框架服务是否在跑；None = 查不到（权限/系统差异）
    pub frameserver_running: Option<bool>,
    /// 枚举到的音频设备名（调试用，向导页可展开查看）
    pub audio_devices: Vec<String>,
    /// 非 Windows 平台（Mac 移植/调试期）：跳过门禁，避免挡住开发
    pub skipped: bool,
}

impl EnvReport {
    /// 演示/自检用的"什么都没装"报告：
    /// 只在 PCSPEAKER_FORCE_ENV_GUIDE=1 时使用，让开发者不用找一台没装驱动的
    /// 电脑，也能看到向导页的"缺项 + 下载按钮"完整形态（真实检测不受影响）。
    pub fn demo_missing() -> Self {
        Self {
            vb_cable: false,
            unity: false,
            obs: false,
            frameserver_running: None,
            audio_devices: Vec::new(),
            skipped: false,
        }
    }

    /// 必需项是否齐全：麦克风驱动 + 两路虚拟摄像头里至少一路
    pub fn ready(&self) -> bool {
        self.skipped || (self.vb_cable && (self.unity || self.obs))
    }

    /// 还缺哪些必需项（CLI 版 server 用它打印缺项后退出；文案按当前语言取）
    pub fn missing(&self) -> Vec<String> {
        let mut v = Vec::new();
        if !self.vb_cable {
            v.push(lang::t("guide.missing_vb"));
        }
        if !self.unity && !self.obs {
            v.push(lang::t("guide.missing_cam"));
        }
        v
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 检测实现：Windows 走真实检测，其它平台返回"跳过"
// ──────────────────────────────────────────────────────────────────────────

#[cfg(not(windows))]
pub fn detect() -> EnvReport {
    // macOS/Linux 上还没有对应的虚拟音频/摄像头方案（见 README §8），
    // 这里不拦路，否则开发者连调试界面都进不去。
    EnvReport {
        vb_cable: true,
        unity: true,
        obs: true,
        frameserver_running: None,
        audio_devices: Vec::new(),
        skipped: true,
    }
}

#[cfg(windows)]
pub fn detect() -> EnvReport {
    let audio = audio_device_names();
    // 设备名大小写由厂商决定（"CABLE Input" / "cable input"…），统一小写再匹配
    let lower: Vec<String> = audio.iter().map(|n| n.to_lowercase()).collect();
    let vb_cable = lower
        .iter()
        .any(|n| n.contains("cable input") || n.contains("cable output"));

    let report = EnvReport {
        vb_cable,
        unity: clsid_registered(UNITY_CAPTURE_CLSID),
        obs: clsid_registered(OBS_VIRTUAL_CAMERA_CLSID),
        frameserver_running: frameserver_running(),
        audio_devices: audio,
        skipped: false,
    };
    log::info!(
        "[EnvCheck] vb_cable={} unity={} obs={} frameserver={:?} devices={}",
        report.vb_cable,
        report.unity,
        report.obs,
        report.frameserver_running,
        report.audio_devices.len()
    );
    report
}

/// Unity Capture 滤镜的 CLSID（固定值，来自驱动自身安装逻辑）
#[cfg(windows)]
const UNITY_CAPTURE_CLSID: &str = "{5C2CD55C-92AD-4999-8666-912BD3E70010}";
/// OBS Virtual Camera 滤镜的 CLSID（OBS Studio 的 win-dshow 插件注册）
#[cfg(windows)]
const OBS_VIRTUAL_CAMERA_CLSID: &str = "{A3FCE0F5-3493-419F-958A-ABA1250EC20B}";

/// 用 cpal 列出全部音频设备名（播放 + 录音）。失败返回空表，不做任何猜测。
#[cfg(windows)]
fn audio_device_names() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    // cpal 0.15：default_host() 直接返回 Host；设备分"播放/录音"两个列表，
    // 本机没有 all_devices()（那是更新版本的 API）。
    let host = cpal::default_host();
    let mut names = Vec::new();
    for (label, list) in [
        ("output", host.output_devices()),
        ("input", host.input_devices()),
    ] {
        match list {
            Ok(devs) => {
                for d in devs {
                    names.push(d.name().unwrap_or_default());
                }
            }
            Err(e) => log::warn!("[EnvCheck] cpal {label} devices failed: {e}"),
        }
    }
    names
}

/// 某个 DirectShow 设备 CLSID 是否已在系统里注册（InprocServer32 键存在即注册过）
#[cfg(windows)]
fn clsid_registered(clsid: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    // RegOpenKeyExW 要 UTF-16 且以 NUL 结尾的宽字符串
    let path = format!("SOFTWARE\\Classes\\CLSID\\{clsid}\\InprocServer32");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    let mut h = HKEY::default();
    // windows 0.58：注册表函数返回 WIN32_ERROR（不是 Result），用 .ok() 转
    let opened = unsafe {
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(wide.as_ptr()), 0, KEY_READ, &mut h)
            .ok()
            .is_ok()
    };
    if opened {
        unsafe {
            let _ = RegCloseKey(h);
        }
    }
    log::info!("[EnvCheck] CLSID {clsid} registered = {opened}");
    opened
}

/// Windows Camera Frame Server 是否在跑。
/// 这里不碰服务控制 API（省掉一组 Windows 依赖 + 一堆句柄生命周期），
/// 直接问系统自带的 `sc query`：普通用户就有权查询服务状态，输出里的
/// "STATE : 4 RUNNING" 各语言版本都是英文，解析稳妥。查不到返回 None。
#[cfg(windows)]
fn frameserver_running() -> Option<bool> {
    let out = Command::new("sc").args(["query", "FrameServer"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("RUNNING") {
        Some(true)
    } else if text.contains("STOPPED") || text.contains("1053") || text.contains("DOES_NOT_EXIST")
    {
        Some(false)
    } else {
        None
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 动作按钮：只用系统默认浏览器打开驱动官网页面。
// 本程序不附带、不查找、也不运行任何驱动安装脚本 —— 缺驱动时只做文字提示，
// 用户自己去官网下载、自己决定要不要装。
// ──────────────────────────────────────────────────────────────────────────

/// 用系统默认浏览器打开链接
pub fn open_url(url: &str) {
    log::info!("[EnvCheck] opening {url}");
    #[cfg(windows)]
    {
        // cmd /c start "" "url"：第一个空串是窗口标题占位，否则 URL 会被当成标题
        let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 向导面板：由主窗口在"环境未就绪"时整页显示（不起服务端、不画主界面）
// ──────────────────────────────────────────────────────────────────────────

/// 用户点出来的动作，交给 main.rs 决定后续（起服务 / 关窗口）
#[derive(Debug, PartialEq)]
pub enum GuideAction {
    /// 只是看看，没动作
    None,
    /// 重新检测通过 → 可以进入主界面了
    Ready,
    /// 用户选择退出
    Quit,
}

/// 门禁状态：一份自检结果 + 页面局部状态
pub struct EnvGuide {
    pub report: EnvReport,
    /// 最近一次点击的反馈文案
    status: String,
    /// 是否展开"检测到的音频设备"明细
    show_devices: bool,
}

/// 一行检测项：绿色 √ / 红色 × + 标题 + 说明
/// 为什么用 √/× 而不是 ✓/✗：egui 内置字体和 SimHei 都没有 U+2713/U+2717，
/// 那两个会画成空心方框；√(U+221A)/×(U+00D7) 属于 GB2312 基本符号，中文字体必有。
fn item_row(ui: &mut egui::Ui, ok: bool, title: &str, desc: &str) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(if ok { "√" } else { "×" })
                .size(18.0)
                .strong()
                .color(if ok {
                    egui::Color32::from_rgb(34, 197, 94)
                } else {
                    egui::Color32::from_rgb(220, 38, 38)
                }),
        );
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).size(14.0).strong());
            ui.label(
                egui::RichText::new(desc)
                    .size(12.0)
                    .color(egui::Color32::from_rgb(102, 102, 102)),
            );
        });
    });
}

impl EnvGuide {
    pub fn new(report: EnvReport) -> Self {
        Self {
            report,
            status: String::new(),
            show_devices: false,
        }
    }

    /// 主窗口标题：让任务栏/标题栏一眼看出"这是准备页，还没开服务"
    pub fn viewport_title(&self) -> String {
        lang::t("app.title_gate")
    }

    /// 重新检测：齐了返回 Ready（调用方起服务），否则刷新页面并把结果写日志
    fn recheck(&mut self) -> GuideAction {
        self.report = detect();
        self.status = if self.report.ready() {
            lang::t("guide.status_ready")
        } else {
            let miss = self.report.missing();
            lang::tf(
                "guide.status_missing",
                &[
                    ("count", &miss.len().to_string()),
                    ("items", &miss.join(" / ")),
                ],
            )
        };
        log::info!(
            "[EnvCheck] recheck ready={} ({})",
            self.report.ready(),
            self.status
        );
        if self.report.ready() {
            GuideAction::Ready
        } else {
            GuideAction::None
        }
    }

    /// 画整页向导（调用方已把它放进 CentralPanel），返回用户动作
    ///
    /// 布局用"嵌套面板"：顶部说明、底部按钮各自固定，中间检测项放滚动区。
    /// 为什么不能按顺序直接往下写：ScrollArea 会把 Ui 的剩余高度全部吃掉，
    /// 写在它后面的按钮条就被挤到可视区外裁掉（第一版就是底部按钮看不见）。
    pub fn render(&mut self, ui: &mut egui::Ui) -> GuideAction {
        let mut action = GuideAction::None;

        // ── 底部：动作条（必须先声明，才能把这一条高度从中间预留出来）──
        egui::TopBottomPanel::bottom("guide_foot")
            .frame(egui::Frame::none().inner_margin(egui::Margin::symmetric(0.0, 10.0)))
            .show_inside(ui, |ui| {
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new(lang::t("guide.quit")).size(14.0))
                        .clicked()
                    {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        action = GuideAction::Quit;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(
                                // 注意：本项目是浅色主题，按钮底色是白的，
                                // 这里若把文字也写成白色就等于隐形（第一版踩过）。
                                egui::RichText::new(lang::t("guide.recheck"))
                                    .size(16.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(37, 99, 235)),
                            )
                            .clicked()
                        {
                            if self.recheck() == GuideAction::Ready {
                                action = GuideAction::Ready;
                            }
                        }
                    });
                });
            });

        // ── 顶部：标题 + 说明 ──
        egui::TopBottomPanel::top("guide_head")
            .frame(egui::Frame::none())
            .show_inside(ui, |ui| {
                ui.label(
                    egui::RichText::new(lang::t("guide.title"))
                        .size(20.0)
                        .strong(),
                );
                ui.label(egui::RichText::new(lang::t("guide.intro")).size(13.0));
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
            });

        // ── 中间：检测项（可滚动）──
        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                // ① VB-CABLE —— 麦克风模式必需
                let vb_ok = self.report.vb_cable;
                item_row(
                    ui,
                    vb_ok,
                    &lang::t("guide.vb_title"),
                    &lang::t(if vb_ok { "guide.vb_ok" } else { "guide.vb_missing" }),
                );
                if !vb_ok {
                    ui.indent("vb", |ui| {
                        if ui
                            .button(lang::t("guide.vb_btn"))
                            .on_hover_text(lang::t("guide.vb_hover"))
                            .clicked()
                        {
                            open_url("https://vb-audio.com/Cable/");
                            self.status = lang::t("guide.status_opened");
                        }
                        ui.label(egui::RichText::new(lang::t("guide.vb_steps")).size(12.0));
                    });
                }
                ui.add_space(10.0);

                // ② 虚拟摄像头 —— 两选一
                let cam_ok = self.report.unity || self.report.obs;
                let detail = match (self.report.unity, self.report.obs) {
                    (true, true) => lang::t("guide.cam_both"),
                    (true, false) => lang::t("guide.cam_unity_only"),
                    (false, true) => lang::t("guide.cam_obs_only"),
                    (false, false) => lang::t("guide.cam_none"),
                };
                item_row(ui, cam_ok, &lang::t("guide.cam_title"), &detail);
                if !cam_ok {
                    ui.indent("cam", |ui| {
                        let mut opened = false;
                        if ui
                            .button(lang::t("guide.cam_btn_obs"))
                            .on_hover_text(lang::t("guide.cam_hover_obs"))
                            .clicked()
                        {
                            open_url("https://obsproject.com/download");
                            opened = true;
                        }
                        if ui
                            .button(lang::t("guide.cam_btn_unity"))
                            .on_hover_text(lang::t("guide.cam_hover_unity"))
                            .clicked()
                        {
                            open_url("https://github.com/Unity-Technologies/Unity-Capture");
                            opened = true;
                        }
                        if opened {
                            self.status = lang::t("guide.status_cam_opened");
                        }
                        ui.label(egui::RichText::new(lang::t("guide.cam_note")).size(12.0));
                    });
                }
                ui.add_space(10.0);

                // ③ FrameServer —— 仅提示，不参与放行
                let fs_txt = match self.report.frameserver_running {
                    Some(true) => lang::t("guide.fs_running"),
                    Some(false) => lang::t("guide.fs_stopped"),
                    None => lang::t("guide.fs_unknown"),
                };
                ui.label(
                    egui::RichText::new(fs_txt)
                        .size(12.0)
                        .color(if self.report.frameserver_running == Some(false) {
                            egui::Color32::from_rgb(180, 120, 0)
                        } else {
                            egui::Color32::from_rgb(90, 90, 90)
                        }),
                );

                // 明细：检测到的音频设备名（排错时一眼看清设备到底叫什么）
                ui.add_space(8.0);
                if !self.report.audio_devices.is_empty() {
                    let open = self.show_devices;
                    if ui
                        .small_button(if open {
                            lang::t("guide.devices_hide")
                        } else {
                            lang::t("guide.devices_show")
                        })
                        .clicked()
                    {
                        self.show_devices = !open;
                    }
                    if self.show_devices {
                        let joined = self.report.audio_devices.join("　|　");
                        ui.label(
                            egui::RichText::new(joined)
                                .size(11.0)
                                .color(egui::Color32::from_rgb(110, 110, 110)),
                        );
                    }
                }

                if !self.status.is_empty() {
                    ui.add_space(8.0);
                    let st = self.status.clone();
                    ui.label(
                        egui::RichText::new(st).color(egui::Color32::from_rgb(37, 99, 235)),
                    );
                }
                });
            });

        // 注：这里不主动 request_repaint —— 门禁页是静态的，
        //     用户点击/输入本身就会触发重绘（main.rs 另有 1 秒兜底重绘）。
        action
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 放行规则单测：这条规则决定"能不能进主界面"，写死在测试里最划算
// （真实 detect() 依赖本机装了哪些驱动，换台机器结果就不同，不适合断言）
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一份纯逻辑用的报告（不走真实检测）
    fn fake(vb_cable: bool, unity: bool, obs: bool) -> EnvReport {
        EnvReport {
            vb_cable,
            unity,
            obs,
            frameserver_running: None,
            audio_devices: Vec::new(),
            skipped: false,
        }
    }

    #[test]
    fn mic_driver_is_mandatory() {
        // 没有 VB-CABLE 就不放行，哪怕两个虚拟摄像头都在
        assert!(!fake(false, true, true).ready());
    }

    #[test]
    fn either_virtual_camera_is_enough() {
        assert!(fake(true, true, false).ready());
        assert!(fake(true, false, true).ready());
        assert!(!fake(true, false, false).ready());
    }

    #[test]
    fn all_present_passes() {
        assert!(fake(true, true, true).ready());
    }

    #[test]
    fn missing_list_names_both_gaps() {
        let r = fake(false, false, false);
        assert_eq!(r.missing().len(), 2);
        // 摄像头两选一：只要有一路在，就不算缺
        assert_eq!(fake(true, false, true).missing().len(), 0);
    }

    #[test]
    fn non_windows_skips_gate() {
        let r = EnvReport {
            vb_cable: false,
            unity: false,
            obs: false,
            frameserver_running: None,
            audio_devices: Vec::new(),
            skipped: true,
        };
        assert!(r.ready(), "非 Windows 平台不该被门禁挡住");
    }
}
