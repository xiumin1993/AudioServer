# AudioServer（Windows 正式版）快速开始

这份文档面向**拿到安装包的人**：不需要装 Rust，不需要 .NET / VC 运行库，也不需要看懂代码。
开发者视角的完整文档（协议、架构、编译、常见问题）在同目录 `README.md`。

## 0. 你需要哪一种包

| 你拿到的文件 | 是什么 | 怎么用 |
|---|---|---|
| `AudioServer-<版本>-win-x64-setup.exe` | **安装程序（推荐）** | 双击 → 跟着向导点下去 → 装完有开始菜单快捷方式和卸载项 |
| `AudioServer-<版本>-win-x64.zip` | 绿色免安装版 | 解压到任意目录，双击里面的 `audioserver.exe` |

两种包装的是**同一个程序**，功能、配置文件位置、日志位置都一样。区别只在"有没有卸载项和快捷方式"。

---

# 第一部分：安装

## 1. 双击安装程序

向导里只有三个决定值得看一眼：

1. **安装范围**。默认是"仅为当前用户"，装在 `%LOCALAPPDATA%\Programs\PCAssistant`，**全程不弹 UAC**。
   想装进 `C:\Program Files\PCAssistant` 就选"为所有用户安装"，那一步才会要管理员权限。
2. **桌面图标**。默认不勾；开始菜单里一定有。
3. **完成页的"立即启动"**。勾了就直接跑，不勾以后从开始菜单启动。

装完这些东西：开始菜单 → `PC Assistant AudioServer`（启动）、`Config file (config.json)`（打开配置文件）、
`Quick start`（本文档）、`Uninstall`（卸载）。控制面板"程序和功能"里有正常卸载项。

## 2. 安装程序**不装驱动**（这是有意的设计）

包里没有任何 `.msi / .inf / .sys / .dll`，也没有安装脚本 —— 它不会替你装 VB-CABLE、
Unity Capture 或 OBS Virtual Camera，不碰系统服务、不改启动项、除了卸载登记之外不写注册表。

缺什么驱动由**程序启动时**告诉你（见下一节），并给出对应官网的打开按钮，你自己去下载、自己装。
好处是：装坏了不会留东西在系统里，卸载程序就是干净地删掉那几个文件。

## 3. 第一次启动：先自检驱动，缺了就不进主界面

启动后程序做一次**只读**检查：

| 检查项 | 判定方式 | 缺了影响 |
|---|---|---|
| VB-CABLE 虚拟音频线 | 系统声卡列表里有没有 `CABLE Input` / `CABLE Output` | 麦克风模式必需 |
| 虚拟摄像头 | Unity Capture 或 OBS Virtual Camera 任一滤镜是否注册 | 摄像头模式必需，二选一 |
| Windows 相机框架服务 | `FrameServer` 是否在跑 | 只提示，不影响放行（浏览器才会用到它） |

- **必需项齐全** → 进主界面，开始监听 8080。
- **必需项缺失** → 窗口标题变成"首次运行：需要准备驱动"，页面只显示检测结果与安装指引，
  **服务端线程根本不起**（8080 不监听，手机连不上，不会出现"看着在跑其实没设备"）。
  页面上只有两样东西：**缺哪一项的文字说明** + **对应官网的打开按钮**。装完回来点【重新检测】即进主界面。

各模式自己还需要什么：

| 模式 | 额外前置 | 说明 |
|---|---|---|
| 手机当**音箱**（电脑声音→手机） | 无 | 连上就能用 |
| 手机当**麦克风** | 装 [VB-CABLE](https://vb-audio.com/Cable/)（免费） | 装完播放设备出现 `CABLE Input`、录音设备出现 `CABLE Output`；会议软件把**输入**选成 `CABLE Output` |
| 手机当**摄像头** | 自己装 [OBS Studio](https://obsproject.com/download)（自带 OBS Virtual Camera），或 [Unity Capture](https://github.com/Unity-Technologies/Unity-Capture)（解压后右键管理员运行其 `Install.bat`） | 应用里能选 "OBS Virtual Camera" / "Unity Video Capture"。**浏览器（Edge/Chrome）只认 OBS Virtual Camera**，要在浏览器里用就必须装 OBS；两个驱动可同时装，服务器自动同时注帧 |
| USB 有线（延迟最低，仅安卓） | 数据线 + `adb reverse tcp:8080 tcp:8080` | 手机首页点"USB 有线直连"；iPhone 只能用 WiFi |

> 开发者开关（正常用户不需要）：`PCSPEAKER_SKIP_ENV_CHECK=1` 或配置项 `diagnostics.skip_env_check`
> 强行进主界面；`PCSPEAKER_FORCE_ENV_GUIDE=1` 停在向导页看样例界面。
> 命令行版 `server.exe` 同样自检，缺驱动打印缺项后以退出码 2 结束，可用 `--skip-env-check` 跳过。

## 4. 连上手机（三步）

1. 启动 `audioserver.exe` → 通过自检后窗口出现即已开始监听 **8080** 端口。
2. 第一次用：Windows 弹窗问"是否允许访问网络"→ 勾选**专用网络**并允许。
   当初点了"取消"就手动放行：`控制面板 → Windows Defender 防火墙 → 允许应用通过防火墙`
   → 允许 `audioserver.exe`（专用+公用）。
3. 手机装 PC Assistant App，首页填 `电脑IP:8080` → 点"连接"。
   电脑 IP 用 `ipconfig` 看"IPv4 地址"（手机与电脑要连同一个 WiFi）。

---

# 第二部分：配置

## 5. 配置文件 config.json

位置：**`%APPDATA%\PCAssistant\config.json`**（一般是 `C:\Users\你的用户名\AppData\Roaming\PCAssistant\config.json`）。
开始菜单里的 `Config file (config.json)` 快捷方式直接打开它；安装目录里那份
`config.default.json` 只是出厂参考，程序不读它。

安装程序会预先放一份默认配置；如果那个位置已经有文件（你之前装过），**绝不覆盖**。
没放成功也没关系，程序首次启动会自己创建。

改配置要记住的三条：

1. **改完要重启**。配置只在启动时读一次（端口、设备名、分辨率这类硬件参数不能在运行中改）。
   关窗口不算退出，请在界面里停服务或直接退出程序再重开。
2. **少写键、写错值都不会让程序起不来**。每个键都有默认值；越界的值会被夹回边界，
   并在日志里留一条 `[Config] camera.fps=300 too large, clamped to 60` 这样的告警。
3. **写坏了不会冲掉你的文件**。JSON 语法错误时程序按默认值跑，日志写
   `[Config] invalid config file, fell back to defaults: <原因>`，原文件保持不动，你自己修好再重启。
   想彻底重置：删掉 `config.json` 即可（下次启动回到出厂默认）。

常用项速查（完整表格和每一项的含义见 `README.md` 第 4.1 节）：

| 键 | 默认 | 什么时候要改 |
|---|---|---|
| `language` | `auto` | 想强制中文 `zh` / 英文 `en`（`auto` = 跟随系统显示语言） |
| `network.port` | `8080` | 端口被别的程序占了 |
| `network.bind` | `"0.0.0.0"` | 只想本机连（配合 USB adb reverse）就写 `"127.0.0.1"` |
| `speaker.capture_device_hint` | `""` | 想从耳机而不是音箱收声：填播放设备名的一部分，如 `"Realtek"` |
| `mic.inject_device_hint` | `"CABLE Input"` | 换用别的虚拟声卡（VoiceMeeter 等） |
| `mic.monitor_capture_hint` | `"CABLE Output"` | 同上，成对改 |
| `camera.width` / `height` / `fps` | `960` / `720` / `30` | OBS 虚拟摄像头的分辨率与帧率 |
| `camera.unity_enabled` / `obs_enabled` | `true` | 只用其中一个虚拟摄像头时关掉另一个 |
| `diagnostics.log_level` | `"info"` | 排障时改 `debug` / `trace` |
| `diagnostics.show_console` | `false` | 想要一个实时滚日志的黑窗口就改 `true` |

设备名怎么填：名称匹配是"不区分大小写的子串包含"，取第一个命中。写错了程序不会报错退出，
只会 warn 一句然后退回系统默认设备 —— 去日志看 `Using audio device: … (…)` 括号里的来源就知道走没走。

界面"设置"页里能改的那几项（端口/采样率/声道/缓冲区/是否暂停下行）改完会直接写回这个文件，
两者是同一份配置，不存在"界面一套、文件另一套"。

## 6. 日志在哪 / 怎么看

`audioserver.log` 记录了完整流程（哪个 IP 连上、会话登记、驱动挂载、第一帧上传、2 秒一次的吞吐统计），
排查问题时把它发给对方即可。位置按顺序取第一个能写的：

1. **exe 同目录**（绿色版、开发时就是这个位置；装进 `C:\Program Files` 之外的目录也在这儿）；
2. 写不进去（装在 Program Files 等只读位置）→ 自动退到 `%APPDATA%\PCAssistant\audioserver.log`。

实际用的哪个路径，每次启动都打在日志第一屏，界面设置页的"日志文件"一行也写着。
主程序**不再附带控制台黑窗口**；界面上的"日志"页与文件内容同步，不用开文件。
真要控制台：`diagnostics.show_console = true`，或环境变量 `PCSPEAKER_CONSOLE=1`，
或直接用命令行版 `server.exe`。后台线程 panic 也会进日志（`[Panic] …`），不会悄悄少一条线。

## 7. 卸载

控制面板"程序和功能"或开始菜单里的 `Uninstall`。

卸载**只删程序文件**，**故意保留** `%APPDATA%\PCAssistant`（配置是你调出来的东西，日志是排障证据）。
想彻底清空：卸载后自己删掉那个目录。装回来时那份旧配置会被继续用（安装程序不会覆盖它）。

---

## 8. 隐私

手机相机/麦克风硬件默认**彻底关闭**：服务器靠驱动握手事件与内核句柄计数实时判断"有没有软件真的在用"，
只在用的那一刻通知手机开硬件，用完 1 秒内自动关。手机显示红色 REC = 硬件真的开着。
手机"冻结/停止"或电脑 GUI 的 Force Stop 都能立刻彻底关闭；手机首页守护开关关掉后，
断线重连也不会被自动恢复。

## 9. 界面语言

默认**跟随系统**：Windows 显示语言是中文就出中文，其余出英文；缺驱动的自检向导页同样双语。

想固定一种：改 `config.json` 里的 `language`（`zh` / `en` / `auto`）后重启，
或在主界面 LANGUAGE 卡片点 English / 简体中文 / Follow system（点完立刻生效，并写进同一个配置文件，
下次启动仍保留）。老版本的 `settings.txt` 只会在升级后第一次启动时被读一次语言设置，之后不再使用。

日志 `audioserver.log` 故意保持英文（便于排错检索），不随界面语言切换。

## 10. 摄像头在浏览器里打不开

浏览器一个摄像头都看不到 = 系统服务 **Windows Camera Frame Server** 没在跑：
`Win+R → services.msc` → 找 "Windows Camera Frame Server" → 右键启动（默认可能是"手动"）。
启动后**完全重启浏览器**再刷新页面。想让它开机自动启动需要管理员执行
`sc config FrameServer start=auto` —— 这会改动系统服务配置，请你自己决定要不要做。
