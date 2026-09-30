# AudioServer（Windows 正式版）快速开始

这个包里的 `audioserver.exe` 是**正式发布版**（Rust release 编译：`opt-level=3` + LTO + 去掉调试符号），
不需要安装 Rust、不需要 .NET/VC 运行库，双击就能跑。

## 1. 包里有什么

| 文件 / 目录 | 作用 |
|---|---|
| `audioserver.exe` | 主程序（egui 图形界面），双击即用 |
| `server.exe` | 无界面命令行版，功能相同。只在"开机自启 / 挂成计划任务"时才需要它 |
| `start-audioserver.bat` | 在当前目录窗口启动主程序（日志会同时写到同目录 `audioserver.log`） |
| `驱动/UnityCapture/` | 手机当电脑摄像头所需的虚拟摄像头驱动（含 `Install.bat` / `Uninstall.bat`） |
| `版本信息.txt` | 本次打包对应的代码提交与编译时间 |
| `README.md` | 完整文档（协议、架构、常见问题、macOS 移植说明） |

> 请把整个文件夹放在**普通用户可写**的位置（例如 `D:\AudioServer\`），不要放进 `C:\Program Files`：
> 程序要在自己所在目录写 `audioserver.log`，Program Files 需要管理员权限才会失败。

## 2. 三步跑起来

1. 双击 `audioserver.exe` → 窗口出现即已开始监听 **8080** 端口。
2. 第一次用：Windows 弹窗问"是否允许访问网络"→ 勾选**专用网络**并允许。
   若当初点了"取消"，现在手动放行：
   `控制面板 → Windows Defender 防火墙 → 允许应用通过防火墙` → 允许 `audioserver.exe`（专用+公用）。
3. 手机装好 PC Assistant App，首页填 `电脑IP:8080` → 点"连接"。
   电脑 IP 在命令行执行 `ipconfig` 看"IPv4 地址"（手机与电脑要连同一个 WiFi）。

## 3. 三种模式各自还需要什么

| 模式 | 额外前置 | 说明 |
|---|---|---|
| 手机当**音箱**（电脑声音→手机） | 无 | 连上就能用 |
| 手机当**麦克风** | 装 [VB-CABLE](https://vb-audio.com/Cable/)（免费） | 装完播放设备出现 `CABLE Input`、录音设备出现 `CABLE Output`；会议软件把**输入**选成 `CABLE Output` 即可 |
| 手机当**摄像头** | `驱动/UnityCapture/Install.bat`（右键管理员运行一次） | 装完任意软件可选 "Unity Video Capture"。**浏览器（Edge/Chrome）只认 OBS Virtual Camera**：需要另外安装 [OBS Studio](https://obsproject.com) 才能给浏览器用，两个驱动可以同时装、服务器会自动同时注帧 |
| USB 有线（延迟最低，仅安卓） | 数据线 + `adb reverse tcp:8080 tcp:8080` | 手机首页点"USB 有线直连"即可；iPhone 只能用 WiFi |

## 4. 摄像头打不开时先查这条

浏览器里一个摄像头都看不到 = 系统服务 **Windows Camera Frame Server** 没在跑：
`Win+R → services.msc` → 找 "Windows Camera Frame Server" → 右键启动（该服务默认可能是"手动"）。
启动后**重启浏览器**再刷新页面。想让它开机自动启动需要管理员执行
`sc config FrameServer start=auto`——这会改动系统服务配置，请你自己决定要不要做。

## 5. 隐私

手机相机/麦克风硬件默认**彻底关闭**：服务器靠驱动握手事件与内核句柄计数实时判断"有没有软件真的在用"，
只在用的那一刻通知手机开硬件，用完 1 秒内自动关。手机显示红色 REC = 硬件真的开着。
手机"冻结/停止"或电脑 GUI 的 Force Stop 都能立刻彻底关闭；手机首页守护开关关掉后，
断线重连也不会被自动恢复。

## 6. 出问题先看日志

程序同目录的 `audioserver.log` 记录了完整流程（哪个 IP 连上、会话登记、驱动挂载、第一帧上传、
2 秒一次的吞吐统计）。排查时把它发给对方即可。
