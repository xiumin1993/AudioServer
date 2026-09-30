# Audio Server（PC 端服务器）

把 Windows 电脑变成"音频/视频交换中心"：手机当电脑的**外放音箱**、**麦克风**、**网络摄像头**，全部走局域网 WebSocket 实时传输，端到端延迟 100ms 以内。（macOS 作为服务器：音频双通路见 §8；iPhone 连本 Windows 服务器零改动可用作 §8.6）

本仓库是电脑端：一个 Rust 编写的 egui 图形界面服务器。手机端见姊妹仓库 `PCAssistant`（Flutter）。

```
手机麦克风 ──WebSocket──▶ AudioServer ──WASAPI 注入──▶ VB-CABLE ──▶ 钉钉/腾讯会议等当作"麦克风"
电脑系统声音 ──WASAPI 采集──▶ AudioServer ──WebSocket──▶ 手机扬声器
手机摄像头 ──WebSocket(JPEG)──▶ AudioServer ──共享内存──▶ Unity Video Capture / OBS Virtual Camera
                                                            └─▶ 任何识别虚拟摄像头的软件（浏览器/会议/OBS）
```

---

## 1. 前置条件（先检查，再动手）

| # | 条件 | 必须? | 说明 |
|---|------|-------|------|
| 1 | Windows 10/11 x64 | ✅ | 音频注入与虚拟摄像头均为 Windows 专用（WASAPI + DirectShow） |
| 2 | 手机与电脑在**同一局域网** | ✅ | 建议 5GHz WiFi 或 USB 有线（见 §5）；2.4GHz 干扰大时帧率会掉 |
| 3 | [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) | 麦克风模式必须 | 免费驱动。装完后播放设备里出现 `CABLE Input`、录音设备里出现 `CABLE Output` |
| 4 | 虚拟摄像头驱动（Unity Capture 或 OBS 任一） | 摄像头模式必须 | 需**自行下载安装**：[Unity Capture](https://github.com/Unity-Technologies/Unity-Capture) 或 OBS Studio（见第 5 行）。本程序不附带任何驱动或安装脚本；仓库里的 `third_party/UnityCapture-master/` 只是开发期参考源码（Rust 发送端按它的共享内存格式实现），不进发行包 |
| 5 | OBS Virtual Camera（可选） | 浏览器场景推荐 | 安装任意版本的 [OBS Studio](https://obsproject.com)。Unity 驱动是 DirectShow 的，**Edge/Chrome 浏览器只认 MF 设备**，需要 OBS 虚拟摄像头兜底（服务器会同时向两个驱动注帧，装哪个都无所谓） |
| 6 | Windows 服务 `FrameServer`（Windows Camera Frame Server） | 浏览器场景必须 | 若浏览器枚举不到任何摄像头：`services.msc` → 找到 "Windows Camera Frame Server" → 启动。设为"自动"需管理员：`sc config FrameServer start=auto`（改系统服务，请自行决定） |
| 7 | Rust 工具链 | 仅自行编译时 | [rustup](https://rustup.rs/) 稳定版即可；直接下载 Release  exe 则不需要 |

> **隐私模型**：手机相机/麦克风硬件默认**彻底关闭**。服务器通过驱动握手事件（Want）与系统注册表实时检测"有没有应用真的在用"，只在使用的瞬间通知手机开硬件，用完 1 秒内自动关。手机上有红色 REC 横幅=硬件真实开启，任何时刻可一键强停。

> 表里第 3、4、5 项**不用自己核对**：程序启动时会只读检测这三项，缺必需驱动就不进主界面、不起服务端，并在向导页给出下载入口（见 §4）。

## 2. 安装驱动

### 2.1 VB-CABLE（麦克风模式）
1. 官网下载 → 右键"以管理员运行"安装 → 重启。
2. 验证：`Win+R` → `mmsys.cpl`，"录制"页出现 `CABLE Output`。

### 2.2 Unity Capture（摄像头模式，可选其一）
1. 从项目主页下载并解压 [Unity-Capture](https://github.com/Unity-Technologies/Unity-Capture)（官方 zip 内含 `Install` 目录）；
2. 进入解压后的 `Install\`，右键 `Install.bat` → **以管理员运行**（会 regsvr32 注册 DirectShow filter）；
3. 验证：设备管理器或任意摄像头选择界面出现 **"Unity Video Capture"**。

> 本程序**不会**替你执行上面任何一步，也不随包附带 `Install.bat`；缺驱动时只做检测与文字提示。

### 2.3 OBS Virtual Camera（可选，浏览器兜底）
安装 OBS Studio 即自动注册（无需启动 OBS 本体，服务器直接写它的共享内存队列）。

## 3. 编译

```bash
git clone git@github.com:xiumin1993/AudioServer.git
cd AudioServer
cargo build --release
# 产物：target/release/audioserver.exe（约 6.9MB）
```

调试用探针（可选，`cargo run --bin xxx`）：`vcam_probe`（枚举 DirectShow 摄像头）、`mic_probe`（枚举 WASAPI 设备）、`session_probe`、`cap_probe`。

## 4. 运行

双击 `audioserver.exe`（弹出 egui 窗口）或命令行：

```bash
audioserver.exe --port 8080 --sample-rate 48000 --channels 2 --buffer-size 1024
```

> 注：上面这组参数只有命令行版 `server.exe` 认（`server.exe --port 8080 ...`）；
> GUI 版的端口/采样率在窗口 Settings 页里改，启动时不解析命令行参数。

**启动自检门禁（v3.5）**：GUI 与 CLI 启动时都会先做一次**只读**检测——
VB-CABLE（查声卡列表里的 `CABLE Input/Output`）+ Unity Capture / OBS Virtual Camera
任一（查两个滤镜的 CLSID 是否注册）。必需项齐全才进主界面并起服务端；
缺项时 GUI 整页显示"需要准备驱动"向导，**只有文字提示 + 打开对应官网下载页的按钮**
（不附带安装脚本、不替你装任何东西），**8080 根本不监听**；CLI 打印缺项与下载网址后以退出码 2 结束。
FrameServer 只作为提示，不参与放行。
开发开关：`PCSPEAKER_SKIP_ENV_CHECK=1` 跳过门禁，`PCSPEAKER_FORCE_ENV_GUIDE=1` 强制停在向导页，
CLI 用 `--skip-env-check`。

GUI 顶部三个模式胶囊：**Speaker Mode**（电脑→手机声音）/ **Mic Mode**（手机→电脑麦克风）/ **Camera Mode**（手机→电脑摄像头）。每个模式页内有：连接客户端列表、实时统计（分辨率/fps/码率）、Request（主动唤起手机）与 Force Stop（一键强停）。

**防火墙**：首次运行 Windows 会弹窗，勾选"专用网络"允许即可。若被拦截，手动放行：
```powershell
# 需管理员，自行执行
New-NetFirewallRule -DisplayName "AudioServer" -Direction Inbound -LocalPort 8080 -Protocol TCP -Action Allow
```

## 5. 连接方式

### WiFi（默认）
手机端首页填 `电脑IP:8080`（电脑 `ipconfig` 查 IPv4，如 `192.168.0.103:8080`）。

### USB 有线（延迟最低、不受 WiFi 波动影响）
手机 USB 线插电脑并开启"USB 调试"后，在电脑执行：
```bash
adb reverse tcp:8080 tcp:8080
```
手机端点"USB 有线直连"（自动填入 `127.0.0.1:8080`）→ 连接。原理：adb 把手机本回环端口经 USB 镜像回电脑，等效直连。
注意：`adb reverse` 在**无线 adb**下不可靠，请插线使用。

### 蓝牙
❌ 视频不可行：经典蓝牙实测带宽 ~2Mbps，JPEG 视频流需 3–10Mbps。
⚠️ 音频：手机连电脑蓝牙音频网关属于系统级路由（A2DP），不是本应用职责；用本应用走 WiFi/USB 效果更好。

## 6. 常见问题

| 症状 | 原因与解法 |
|------|-----------|
| 手机连不上 | ① 不同网段（访客 WiFi/AP 隔离）；② 防火墙没放行 8080；③ 电脑 IP 变了（建议路由器绑定静态 DHCP） |
| 会议软件里没有 CABLE Output | VB-CABLE 未装/未重启；在会议软件音频设置里手动选 "CABLE Output" |
| 浏览器枚举不到虚拟摄像头 | FrameServer 服务没启动（§1 第 6 条）；启动后**完全重启 Edge**（设备列表按进程缓存） |
| Unity 通道显示 "Unity has not started…" | 服务器没在注帧：确认 Camera Mode 页有码率；确认手机在取景状态 |
| 画面 180° 颠倒 | 旧版本 bug，v3.4.2 起已修（Unity 共享内存为 bottom-up 行序，服务器已翻转） |
| 声音卡顿 | 2.4GHz WiFi 干扰 → 换 5GHz 或 USB；任务管理器看 CPU 是否被杀毒软件扫描占满 |
| 手机息屏后掉线 | 手机端开启"麦克风守护/摄像头守护"（前台服务常驻通知保活）；另外关闭系统对 App 的电池优化 |

## 7. 项目结构

```
src/main.rs      egui GUI + 模式切换
src/server.rs    WebSocket 服务、协议分发、WASAPI 环回采集、cpal 播放
src/mic_out.rs   手机上行 PCM → VB-CABLE 注入（含 44.1k↔48k 线性重采样）
src/vcam.rs      Unity Capture 共享内存发送端（协议移植自官方 shared.inl）
src/vcam_obs.rs  OBS Virtual Camera 共享内存队列发送端（NV12 三槽环）
src/bin/         设备枚举探针（调试用）
third_party/     UnityCapture 官方源码+安装包（MIT）
```

协议：文本 JSON 控制帧 + 二进制媒体帧。麦克风 PCM 无标记直传；摄像头 JPEG 帧带 4 字节魔术头 `[0x03,'C','A','M']` 分流。

---

## 8. macOS 移植（v3.5，音频双通路已实现，待 Mac 首编验证）

> 状态：`mic_out.rs` / `server.rs` 的 macOS 实现（`#[cfg(target_os = "macos")]`）已按 cpal 0.15 与 CoreAudio API 写好，Windows 生产路径零影响。**这段代码在 Windows 上无法编译验证**（coreaudio-sys 绑定必须在苹果环境生成），到 Mac 上首次 `cargo build --release` 若报错属预期，逐个修即可，逻辑框架不变。

### 8.1 为什么 Mac 比 Windows 麻烦

macOS **没有** Windows WASAPI loopback 那种"直接录声卡输出"的系统能力，也**禁止**第三方内核态驱动。两个方向都要靠用户态虚拟声卡 BlackHole 转发，且要用"多输出设备"把系统声音一分为二。

### 8.2 前置条件（按顺序做）

| # | 步骤 | 说明 |
|---|------|------|
| 1 | 装 Rust：`rustup` 稳定版 | 与 Windows 同 |
| 2 | 装 BlackHole **两个实例**：2ch + 16ch | <https://github.com/ExistentialAudio/BlackHole> 的 .pkg 安装（安装界面可勾选两个变体）；首次加载去 系统设置→隐私与安全性 放行。一设备一用途：16ch 收系统声（下行），2ch 注手机麦（上行），互不干扰，占用检测才不会被自己点着 |
| 3 | 创建"多输出设备" | 打开 **音频 MIDI 设置**（Launchpad 搜 "Audio MIDI Setup"）→ 左下角 ＋ → 创建多输出设备 → 勾选【你的扬声器 + BlackHole 16ch】 |
| 4 | 系统声音输出指到该多输出设备 | 系统设置 → 声音 → 输出 选"多输出设备"。这样电脑发声的同时被抄送进 BlackHole，服务器才有东西可录 |
| 5 | 会议/录音软件输入选 BlackHole 2ch | 对应 Windows 上选 "CABLE Output" 的那一步 |

### 8.3 数据流（对照 Windows）

```
下行(当音箱)：  系统声音 →[多输出设备]→ BlackHole 16ch 输入侧 →cpal捕获→ AudioServer →WebSocket→ 手机扬声器
上行(当麦克风)：手机 →WebSocket→ AudioServer →cpal渲染→ BlackHole 2ch 输出侧 →会议软件从"BlackHole 2ch"录音
摄像头：       macOS 需 CoreMediaIO Camera Extension，本仓库暂未实现（见 §8.5）
```

设备名匹配默认值：下行捕获找 `BlackHole 16ch`、上行注入找 `BlackHole 2ch`。不一致时用环境变量覆盖（子串匹配、大小写不敏感）：

```bash
PCSPEAKER_CAPTURE_DEVICE="blackhole 16ch" \
PCSPEAKER_INJECT_DEVICE="blackhole 2ch" \
./target/release/audioserver
```

### 8.4 编译与运行

```bash
cargo build --release        # 在 Mac 本机原生编译
./target/release/audioserver # egui 图形界面与 Windows 版完全相同
```

首次编译若报错，位置几乎必在 `src/mic_out.rs` 的 `macos_impl`（盲写代码与真实 FFI 签名的差异），按报错逐行修即可，架构不用动。

麦克风占用检测（驱动手机"按需录音"的 mic_state 信号）用 CoreAudio `IsRunningSomewhere` 属性 250ms 轮询。若该属性在特定系统表现异常，可设 `PCSPEAKER_MAC_MIC_ALWAYS_ACTIVE=1` 强制视为占用——手机常驻采集，隐私兜底仍由手机端静音/强停开关承担。

### 8.5 Mac 上做虚拟摄像头（未实现，需额外立项）

macOS 禁止 DirectShow 式虚拟摄像头；正路是写一个 **CoreMediaIO Camera Extension**（系统扩展：Xcode 工程 + Apple 开发者签名 + 用户手动批准加载），UnityCapture/OBS-for-Mac 均无法被我们的 Rust 进程直接注帧（OBS 的 Mac 虚拟摄像头只能由 OBS 自己喂流）。iPhone→Mac 的摄像头需求可先用系统自带"连续互通相机"。

### 8.6 iPhone + Windows（零移植成本，推荐先测）

手机跑 iPhone 客户端、电脑继续用本 Windows 服务器：**协议完全一致，服务器一行不改**。限制两条：相机必须停留在取景页亮屏（iOS 禁止后台采集）；USB 有线不可用（无 adb），请走 WiFi。

---

## 9. 界面语言（国际化，v3.7）

服务端 GUI（含缺驱动时的环境向导页）全部中英双语，方案是 **rust-i18n**（编译期把
`locales/*.toml` 嵌进二进制，所以发行包仍然是绿色单 exe，不带语言文件）。

**默认跟随系统**：启动时读 Windows 的"显示语言"（`GetUserDefaultUILanguage()`），
中文系统进中文，其余一律英文（本版只有两套文案，落到英文至少可读）。

改语言的三个入口，优先级从高到低：

| 方式 | 怎么做 | 用途 |
|------|--------|------|
| 环境变量 | `set PCSPEAKER_LANG=zh`（或 `en`）后再启动 | 排错/截图验证，临时覆盖不留痕 |
| 设置文件 | exe 同目录 `settings.txt` 写 `language=auto\|en\|zh` | 用户手动指定；GUI 里 LANGUAGE 卡片选档会自动写这里 |
| 系统语言 | 不用管 | 默认（`auto`） |

> `settings.txt` 与 `audioserver.log` 一样放在 exe 同目录：**不写注册表、不碰 AppData**，
> 删掉文件即恢复默认（绿色单文件的承诺不变）。

其他约定：

- **运行日志保持英文**（`audioserver.log`）：日志主要给排错和 grep 用，不随界面语言变。
- **通用缩写不翻译**：WiFi / USB / IP / WebSocket / PCM / Hz / kbps / fps / ms / kB 两边写法
  一致；品牌名 "PC Assistant / AudioServer" 不译；语言名用母语写法（"简体中文"永远写作
  "简体中文"），这是全球软件的通行惯例。
- 中文界面依赖系统中文字体兜底（`C:\Windows\Fonts\simhei.ttf` 等，启动时自动挂载）。
  极少数纯英文系统没有中文字体时，中文会显示成方框 —— 切回 English 即可，日志里有
  `no usable CJK font found` 一行说明。
- 加第三种语言：新增 `locales/ja.toml`（键必须与 en 完全一致，`cargo test --lib lang::`
  会检查），在 `src/lang.rs` 的 `primary_language_of_system()` 补一个 LANGID 分支，
  再把 GUI 的语言卡片加一档。
