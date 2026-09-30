# AudioServer（Windows 正式版）快速开始

这个包里的 `audioserver.exe` 是**正式发布版**（Rust release 编译：`opt-level=3` + LTO + 去掉调试符号），
不需要安装 Rust、不需要 .NET/VC 运行库，双击就能跑。

## 1. 包里有什么

| 文件 / 目录 | 作用 |
|---|---|
| `audioserver.exe` | 主程序（egui 图形界面），双击即用 |
| `server.exe` | 无界面命令行版，功能相同。只在"开机自启 / 挂成计划任务"时才需要它 |
| `start-audioserver.bat` | 在当前目录窗口启动主程序（日志会同时写到同目录 `audioserver.log`） |
| `VERSION.txt` | 本次打包对应的代码提交与编译时间 |
| `README.md` | 完整文档（协议、架构、常见问题、macOS 移植说明） |

> 包里**只有程序与文档，不含任何驱动或驱动安装脚本**。需要的虚拟驱动（VB-CABLE / 虚拟摄像头）
> 请自己去官网下载、自己安装 —— 程序不替你装，也不碰注册表和服务配置。

> 请把整个文件夹放在**普通用户可写**的位置（例如 `D:\AudioServer\`），不要放进 `C:\Program Files`：
> 程序要在自己所在目录写 `audioserver.log`，Program Files 需要管理员权限才会失败。

## 2. 启动时会先自检驱动（缺了就不进主界面）

双击 `audioserver.exe` 后，程序先做一次**只读**检查（不写注册表、不装驱动、不运行任何安装脚本）：

| 检查项 | 判定方式 | 缺失后果 |
|---|---|---|
| VB-CABLE 虚拟音频线 | 系统声卡列表里有没有 `CABLE Input` / `CABLE Output` | 麦克风模式必需 |
| 虚拟摄像头 | 注册表里 Unity Capture 或 OBS Virtual Camera 任一滤镜 CLSID 是否注册 | 摄像头模式必需，二选一 |
| Windows 相机框架服务 | `sc query FrameServer` | 只提示，不影响放行（浏览器才会用到它） |

- **必需项齐全** → 和以前一样，直接进主界面并开始监听 8080。
- **必需项缺失** → 窗口标题变成"首次运行：需要准备驱动"，页面只显示检测结果与安装指引，
  **服务端线程根本不起**（8080 不监听，手机连不上，也就不会出现"看着在跑其实没设备"的情况）。
  页面上只会给你两样东西：**缺哪一项的文字说明** + **对应官网的打开按钮**
  （VB-CABLE 官网 / OBS Studio 下载页 / Unity Capture 项目主页）。
  程序不附带安装脚本、也不会替你装驱动，请自己按官网说明装好，回来点【重新检测】即进入主界面。

> 开发者开关（正常用户不需要）：`PCSPEAKER_SKIP_ENV_CHECK=1` 强行进主界面；
> `PCSPEAKER_FORCE_ENV_GUIDE=1` 停在向导页并显示"全都没装"的样例界面。
> 命令行版 `server.exe` 同样会自检，缺驱动直接打印缺项并以退出码 2 结束，可用 `--skip-env-check` 跳过。

## 3. 三步跑起来

1. 双击 `audioserver.exe` → 通过自检后窗口出现即已开始监听 **8080** 端口。
2. 第一次用：Windows 弹窗问"是否允许访问网络"→ 勾选**专用网络**并允许。
   若当初点了"取消"，现在手动放行：
   `控制面板 → Windows Defender 防火墙 → 允许应用通过防火墙` → 允许 `audioserver.exe`（专用+公用）。
3. 手机装好 PC Assistant App，首页填 `电脑IP:8080` → 点"连接"。
   电脑 IP 在命令行执行 `ipconfig` 看"IPv4 地址"（手机与电脑要连同一个 WiFi）。

## 4. 三种模式各自还需要什么

| 模式 | 额外前置 | 说明 |
|---|---|---|
| 手机当**音箱**（电脑声音→手机） | 无 | 连上就能用 |
| 手机当**麦克风** | 装 [VB-CABLE](https://vb-audio.com/Cable/)（免费） | 装完播放设备出现 `CABLE Input`、录音设备出现 `CABLE Output`；会议软件把**输入**选成 `CABLE Output` 即可 |
| 手机当**摄像头** | 自己装 [OBS Studio](https://obsproject.com/download)（自带 OBS Virtual Camera），或 [Unity Capture](https://github.com/Unity-Technologies/Unity-Capture)（解压后右键管理员运行其 `Install.bat`） | 装完任意软件可选 "OBS Virtual Camera" / "Unity Video Capture"。**浏览器（Edge/Chrome）只认 OBS Virtual Camera**，想要浏览器里能用就必须装 OBS；两个驱动可以同时装、服务器会自动同时注帧 |
| USB 有线（延迟最低，仅安卓） | 数据线 + `adb reverse tcp:8080 tcp:8080` | 手机首页点"USB 有线直连"即可；iPhone 只能用 WiFi |

## 5. 摄像头打不开时先查这条

浏览器里一个摄像头都看不到 = 系统服务 **Windows Camera Frame Server** 没在跑：
`Win+R → services.msc` → 找 "Windows Camera Frame Server" → 右键启动（该服务默认可能是"手动"）。
启动后**重启浏览器**再刷新页面。想让它开机自动启动需要管理员执行
`sc config FrameServer start=auto`——这会改动系统服务配置，请你自己决定要不要做。

## 6. 隐私

手机相机/麦克风硬件默认**彻底关闭**：服务器靠驱动握手事件与内核句柄计数实时判断"有没有软件真的在用"，
只在用的那一刻通知手机开硬件，用完 1 秒内自动关。手机显示红色 REC = 硬件真的开着。
手机"冻结/停止"或电脑 GUI 的 Force Stop 都能立刻彻底关闭；手机首页守护开关关掉后，
断线重连也不会被自动恢复。

## 7. 出问题先看日志

程序同目录的 `audioserver.log` 记录了完整流程（哪个 IP 连上、会话登记、驱动挂载、第一帧上传、
2 秒一次的吞吐统计）。排查时把它发给对方即可。
