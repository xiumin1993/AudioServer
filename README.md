# Audio Server（PC 端服务器）

把 Windows 电脑变成"音频/视频交换中心"：手机当电脑的**外放音箱**、**麦克风**、**网络摄像头**，全部走局域网 WebSocket 实时传输，端到端延迟 100ms 以内。

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
| 4 | Unity Capture 虚拟摄像头驱动 | 摄像头模式必须 | 本仓库已附带：`third_party/UnityCapture-master/Install/Install.bat`（MIT 协议源码+预编译 DLL） |
| 5 | OBS Virtual Camera（可选） | 浏览器场景推荐 | 安装任意版本的 [OBS Studio](https://obsproject.com)。Unity 驱动是 DirectShow 的，**Edge/Chrome 浏览器只认 MF 设备**，需要 OBS 虚拟摄像头兜底（服务器会同时向两个驱动注帧，装哪个都无所谓） |
| 6 | Windows 服务 `FrameServer`（Windows Camera Frame Server） | 浏览器场景必须 | 若浏览器枚举不到任何摄像头：`services.msc` → 找到 "Windows Camera Frame Server" → 启动。设为"自动"需管理员：`sc config FrameServer start=auto`（改系统服务，请自行决定） |
| 7 | Rust 工具链 | 仅自行编译时 | [rustup](https://rustup.rs/) 稳定版即可；直接下载 Release  exe 则不需要 |

> **隐私模型**：手机相机/麦克风硬件默认**彻底关闭**。服务器通过驱动握手事件（Want）与系统注册表实时检测"有没有应用真的在用"，只在使用的瞬间通知手机开硬件，用完 1 秒内自动关。手机上有红色 REC 横幅=硬件真实开启，任何时刻可一键强停。

## 2. 安装驱动

### 2.1 VB-CABLE（麦克风模式）
1. 官网下载 → 右键"以管理员运行"安装 → 重启。
2. 验证：`Win+R` → `mmsys.cpl`，"录制"页出现 `CABLE Output`。

### 2.2 Unity Capture（摄像头模式）
1. 进入 `third_party\UnityCapture-master\Install\`；
2. 右键 `Install.bat` → **以管理员运行**（会 regsvr32 注册 DirectShow filter）；
3. 验证：设备管理器或任意摄像头选择界面出现 **"Unity Video Capture"**。

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
