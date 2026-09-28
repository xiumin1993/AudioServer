# PC Audio Server

将电脑音频实时流式传输到手机或其他设备的 WebSocket 服务器。

## 功能

- 捕获 Windows 系统音频输出（WASAPI loopback）
- 通过 WebSocket 实时传输 PCM 音频数据
- 支持多个客户端同时连接
- 低延迟设计（本地网络 < 100ms）
- 可配置采样率、通道数、缓冲区大小

## 使用方法

### 运行服务器

```bash
# 默认配置（端口 8080，48kHz，立体声）
./audioserver.exe

# 自定义配置
./audioserver.exe --port 9000 --sample-rate 44100 --channels 2 --buffer-size 512
```

### 命令行参数

| 参数 | 简写 | 默认值 | 说明 |
|------|------|--------|------|
| `--port` | `-p` | 8080 | WebSocket 监听端口 |
| `--sample-rate` | `-s` | 48000 | 音频采样率 (Hz) |
| `--channels` | `-c` | 2 | 音频通道数 |
| `--buffer-size` | `-b` | 1024 | 缓冲区大小（采样点数） |

### 客户端连接

客户端连接到 `ws://<电脑IP>:8080`

服务器会先发送一个 JSON 配置消息：
```json
{
  "type": "audio_config",
  "sample_rate": 48000,
  "channels": 2,
  "format": "pcm_s16le"
}
```

之后持续发送二进制音频数据（PCM 16-bit, little-endian）。

## 配合 Flutter 客户端使用

1. 在电脑上运行 `audioserver.exe`
2. 在手机上打开 PC Speaker App
3. 输入电脑的 IP 地址（例如 `192.168.1.100:8080`）
4. 点击连接，开始接收音频

## 编译

```bash
# 开发版本
cargo build

# 发布版本（优化）
cargo build --release
```

## 技术栈

- **Rust** - 高性能、低延迟
- **cpal** - 跨平台音频 I/O
- **tokio** - 异步运行时
- **tokio-tungstenite** - WebSocket 实现

## 延迟优化

降低延迟的方法：
- 减小 `--buffer-size`（如 256 或 512）
- 使用有线网络连接
- 关闭其他占用网络的应用

## 许可证

MIT
