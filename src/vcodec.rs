//! 上行视频帧的统一解码入口（JPEG / H.264 / H.265）。
//!
//! ## 为什么要有这一层
//! 手机端原来只有一种编码（JPEG），所以"解码"这件事散落在三个地方各写一遍：
//!   · `vcam.rs`     → Unity Capture 通道（要 RGBA）
//!   · `vcam_obs.rs` → OBS 虚拟摄像头通道（要 RGB）
//!   · `main.rs`     → GUI 预览（要 RGB）
//! 现在手机端可以按硬件能力选 H.264 / H.265，三处各自判断类型会立刻失控 ——
//! 所以把「认格式 + 解码 + 摆正」收在这里，三处只调一个函数。
//!
//! ## 帧格式（在 v3.10 的基础上扩展，向后兼容）
//! WebSocket Binary 帧的完整布局：
//! ```text
//! [0]      0x03
//! [1..4]   "CAM"                ← 与 PCM 音频包区分用的魔术头（见 server.rs）
//! [4]      (codec << 4) | orient ← 本文件负责解析的这一个字节
//! [5..]    编码数据（JPEG / Annex-B 起始码流）
//! ```
//!   · codec  = 高 4 位：0=JPEG、1=H.264、2=H.265
//!   · orient = 低 4 位：bit0~1 顺时针 90° 次数、bit2 水平镜像（语义不变）
//!
//! ### 为什么这么扩就能兼容老手机
//!   · 老包（裸 JPEG）：第 5 字节是 JPEG 自己的 SOI(FF)，第 6 字节是 D8
//!     → 见 `split_frame` 里的判据，走"旧包"分支，codec 按 JPEG、orient 按 0。
//!   · v3.10 包（[orient][JPEG]）：orient 取值只到 7，高 4 位恒为 0
//!     → 按新规则解出来正好是 codec=0(JPEG) + 原来的 orient，逐字节等价。
//! 也就是说不需要版本号，也不需要手机端和服务端同时升级才能通。
//!
//! ## 解码器选型：rust_h264 / rust_h265（纯 Rust）
//! 这两个是同族作者写的解码器，API 完全一致（Decoder::new / decode_nal / flush），
//! 输入都是 Annex B 起始码流 —— 恰好就是 Android MediaCodec 的输出格式。
//! 没选 openh264(C++) / ffmpeg-next(需系统库) 的理由：
//!   · 本项目 Mac 目标也要能编（Cargo.toml 里有 cfg(macos) 依赖段），
//!     引入 C/C++ 会要求目标机备好工具链与库，跨平台构建变脆；
//!   · 纯 Rust 无 build script、无 DLL 分发问题，Windows/Mac 一条 cargo build 通吃；
//!   · 我们推的是【全 I 帧】低延迟流，解码成本远低于常规带 P/B 帧的视频。

use anyhow::Context;

/// 帧头字节里的编码格式（占高 4 位）
pub const CODEC_JPEG: u8 = 0;
pub const CODEC_H264: u8 = 1;
pub const CODEC_H265: u8 = 2;

/// 给日志/界面看的格式名
pub fn codec_name(c: u8) -> &'static str {
    match c {
        CODEC_H264 => "H.264",
        CODEC_H265 => "H.265",
        _ => "JPEG",
    }
}

/// 最近一次成功解码的画面尺寸（宽高各 16 位），server.rs 拿去在界面显示当前画质。
///
/// 为什么要有这个：JPEG 可以从 SOI 头里读出宽高，H.264/H.265 不行（尺寸在 SPS 里，
/// 得解出来才知道）。与其让 server.rs 去解析 SPS，不如让真正解出像素的这里回填。
static LAST_DIMS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 取最近一次成功解码的尺寸 (w, h)；还没解出过任何帧就是 (0, 0)。
pub fn last_dims() -> (u16, u16) {
    let v = LAST_DIMS.load(std::sync::atomic::Ordering::Relaxed);
    ((v >> 16) as u16, (v & 0xFFFF) as u16)
}

fn remember_dims(w: i32, h: i32) {
    if w > 0 && h > 0 && w <= 0xFFFF && h <= 0xFFFF {
        LAST_DIMS.store(((w as u32) << 16) | (h as u32), std::sync::atomic::Ordering::Relaxed);
    }
}

/// 从帧载荷里拆出【编码格式】【方向标记】【编码数据本体】。
///
/// 兼容三种布局，全靠内容特征判断，不靠版本号（理由见文件头注释）：
///   · 老包裸 JPEG：[FF D8 ...]             → (JPEG, 0, 整包)
///   · v3.10 包   ：[orient][FF D8 ...]     → (JPEG, orient, 去掉首字节)
///   · 新包       ：[codec<<4|orient][数据] → (codec, orient, 去掉首字节)
///
/// 空的/太短的包一律按 (JPEG, 0, 原样) 返回，交给解码器去报错 —— 不在这里 panic。
pub fn split_frame(payload: &[u8]) -> (u8, u8, &[u8]) {
    // 判据为什么只看第 0 个字节：
    //   · 老包是裸 JPEG，第 0 字节必然是 JPEG 自己的 SOI 首字节 FF；
    //   · 我们自己的头字节里，orient 只用到 0~7、codec 只用到 0~2，
    //     凑出来的头字节永远不可能等于 FF —— 所以"首字节是 FF"必然是老包。
    // 比旧的判据（"第 1 字节 FF 且第 2 字节 D8"）更通用：那个判据只在
    // 载荷恰好是 JPEG 时成立，H.264 的载荷是 00 00 00 01 起始码，会认不出来。
    // len < 4 的碎片包同理按老包处理（现实中不存在的长度，交给解码器报错更诚实）。
    if payload.len() < 4 || payload[0] == 0xFF {
        return (CODEC_JPEG, 0, payload);
    }
    let head = payload[0];
    (head >> 4, head & 0x0F, &payload[1..])
}

/// 【兼容旧调用点】只要方向标记和数据本体。
///
/// 保留它是因为 vcam.rs / server.rs / main.rs 里都有调用，且单测也按这个签名写的。
/// 新代码请用 `split_frame`（多返回一个编码格式）。
pub fn split_orient(payload: &[u8]) -> (u8, &[u8]) {
    let (_, orient, body) = split_frame(payload);
    (orient, body)
}

/// 解一帧上行画面，并按方向标记摆正。返回 (像素缓冲, 宽, 高)。
///
/// `out_bpp` = 每个像素几个字节：3=RGB、4=RGBA。让调用方指定而不是统一输出 RGBA
/// 再转一次，是因为 OBS/GUI 两路本来就只要 RGB —— 从 YUV 直接算 RGB 能省掉
/// 一整趟 RGBA→RGB 的搬运（720p 一帧近 92 万像素，PC 上也是几毫秒）。
pub fn decode_payload(payload: &[u8], out_bpp: usize) -> anyhow::Result<(Vec<u8>, i32, i32)> {
    let (codec, orient, body) = split_frame(payload);
    let (px, w, h) = match codec {
        CODEC_H264 => decode_h264(body, out_bpp)?,
        CODEC_H265 => decode_h265(body, out_bpp)?,
        _ => decode_jpeg(body, out_bpp)?,
    };
    let (px, w, h) = crate::vcam::orient_bytes(&px, w, h, out_bpp, orient);
    remember_dims(w, h);
    Ok((px, w, h))
}

// ── JPEG ────────────────────────────────────────────────────────────────
/// JPEG → RGB/RGBA。jpeg_decoder 默认吐 RGB24，要 RGBA 时补一个不透明的 A。
fn decode_jpeg(jpeg: &[u8], out_bpp: usize) -> anyhow::Result<(Vec<u8>, i32, i32)> {
    let mut decoder = jpeg_decoder::Decoder::new(jpeg);
    let data = decoder.decode()?;
    let info = decoder.info().context("jpeg 流里没有图像帧")?;
    let (w, h) = (info.width as i32, info.height as i32);
    let npx = (w as usize) * (h as usize);
    anyhow::ensure!(npx > 0, "空图像 {w}x{h}");

    if out_bpp == 4 {
        // RGB24 → RGBA8：用【输出长度】判断通道数（jpeg-decoder 0.3 的 ImageInfo
        // 里没有"每像素字节数"字段，长度比较反而是最不会骗人的依据）。
        let mut rgba = vec![0u8; npx * 4];
        if data.len() == npx * 3 {
            for (i, px) in data.chunks_exact(3).enumerate() {
                let o = i * 4;
                rgba[o] = px[0];
                rgba[o + 1] = px[1];
                rgba[o + 2] = px[2];
                rgba[o + 3] = 255;
            }
        } else if data.len() == npx {
            // 灰度图：三通道取同一个值
            for (i, g) in data.iter().enumerate() {
                let o = i * 4;
                rgba[o] = *g;
                rgba[o + 1] = *g;
                rgba[o + 2] = *g;
                rgba[o + 3] = 255;
            }
        } else {
            anyhow::bail!("jpeg 输出长度异常 {}（{w}x{h}）", data.len());
        }
        Ok((rgba, w, h))
    } else {
        if data.len() != npx * 3 {
            anyhow::bail!("jpeg 输出长度异常 {}（{w}x{h}）", data.len());
        }
        Ok((data, w, h))
    }
}

// ── H.264 ───────────────────────────────────────────────────────────────
/// H.264（Annex B 起始码流）→ RGB/RGBA。
///
/// 【每帧新建一个解码器】，故意不复用：这是实时流，中间的帧会被"只保留最新一帧"
/// 的邮箱丢掉。若沿用同一个解码器，丢掉一帧参考帧就会让后续帧解错甚至花屏；
/// 每帧新建则天然无状态，丢帧只影响那一帧本身，下一帧立刻恢复。
/// 代价只是每帧重新解析一次 SPS/PPS（几十字节），换来的健壮性很划算；
/// 而且我们要求手机端每个 IDR 帧前都带 SPS/PPS（见手机端 MediaCodec 配置）。
fn decode_h264(body: &[u8], out_bpp: usize) -> anyhow::Result<(Vec<u8>, i32, i32)> {
    use rust_h264::decoder::Decoder;
    let nals = rust_h264::nal::parse_annex_b(body);
    anyhow::ensure!(!nals.is_empty(), "H.264 载荷里没有 NAL 单元");

    let mut dec = Decoder::new();
    let mut last: Option<rust_h264::decoder::Frame> = None;
    for nal in &nals {
        if let Some(f) = dec
            .decode_nal(nal)
            .map_err(|e| anyhow::anyhow!("H.264 解码失败: {e:?}"))?
        {
            last = Some(f);
        }
    }
    // 一帧可能由多个 NAL 组成（SPS / PPS / slice），取最后解出的那张图；
    // 若整包只是参数集（没有 slice），flush() 再兜一次。
    let f = match last {
        Some(f) => f,
        None => dec.flush().context("H.264 流里没有图像帧")?,
    };
    let px = yuv420_to_rgb(&f.y, &f.u, &f.v, f.width, f.height, out_bpp);
    Ok((px, f.width as i32, f.height as i32))
}

// ── H.265 / HEVC ────────────────────────────────────────────────────────
/// H.265（Annex B 起始码流）→ RGB/RGBA。解码策略同 H.264（每帧新建，见上）。
///
/// 只支持 8bit（PixelData::U8）。手机端 MediaCodec 的 HEVC 编码基本都是 8bit，
/// 真遇到 10bit 明确报错而不是解出一张错图 —— 让用户看到"不支持"好过看到花屏。
fn decode_h265(body: &[u8], out_bpp: usize) -> anyhow::Result<(Vec<u8>, i32, i32)> {
    use rust_h265::decoder::Decoder;
    let nals = rust_h265::parse_annex_b(body);
    anyhow::ensure!(!nals.is_empty(), "H.265 载荷里没有 NAL 单元");

    let mut dec = Decoder::new();
    let mut last: Option<rust_h265::decoder::Frame> = None;
    for nal in &nals {
        if let Some(f) = dec
            .decode_nal(nal)
            .map_err(|e| anyhow::anyhow!("H.265 解码失败: {e:?}"))?
        {
            last = Some(f);
        }
    }
    let f = match last {
        Some(f) => f,
        None => dec.flush().context("H.265 流里没有图像帧")?,
    };
    anyhow::ensure!(f.bit_depth == 8, "暂不支持 {}-bit H.265", f.bit_depth);
    let (y, u, v) = (
        f.y.as_u8().context("H.265 亮度平面不是 8bit 数据")?,
        f.u.as_u8().context("H.265 色度平面不是 8bit 数据")?,
        f.v.as_u8().context("H.265 色度平面不是 8bit 数据")?,
    );
    let px = yuv420_to_rgb(y, u, v, f.width, f.height, out_bpp);
    Ok((px, f.width as i32, f.height as i32))
}

// ── YUV 4:2:0 planar → RGB/RGBA ─────────────────────────────────────────
/// ITU-R BT.601【limited range】（Y 16~235、UV 16~240）的整数实现。
///
/// 为什么是 BT.601 而不是 BT.709：Android 摄像头 → MediaCodec 这条链默认按
/// BT.601 做色彩矩阵（除非设备显式声明 BT.709），用同一套系数才能和手机端
/// 预览看到的颜色对得上。若将来手机端声明了 BT.709，这里要跟着换系数。
///
/// 系数是标准的 8 位定点近似（298/409/100/208/516），比浮点快且不损失可辨精度。
fn yuv420_to_rgb(
    y: &[u8], u: &[u8], v: &[u8], w: u32, h: u32, out_bpp: usize,
) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let npx = w * h;
    let mut out = vec![0u8; npx * out_bpp];
    let cw = w / 2; // 色度平面宽（4:2:0 横向减半）
    for j in 0..h {
        let yrow = j * w;
        let crow = (j / 2) * cw;
        let orow = j * w * out_bpp;
        for i in 0..w {
            let yy = y[yrow + i] as i32;
            let cb = u[crow + (i / 2)] as i32 - 128;
            let cr = v[crow + (i / 2)] as i32 - 128;
            let base = (298 * (yy - 16) + 128) >> 8;
            let r = clamp_u8(base + ((409 * cr + 128) >> 8));
            let g = clamp_u8(base - ((100 * cb + 208 * cr + 128) >> 8));
            let b = clamp_u8(base + ((516 * cb + 128) >> 8));
            let o = orow + i * out_bpp;
            out[o] = r;
            out[o + 1] = g;
            out[o + 2] = b;
            if out_bpp == 4 {
                out[o + 3] = 255;
            }
        }
    }
    out
}

#[inline]
fn clamp_u8(x: i32) -> u8 {
    if x < 0 {
        0
    } else if x > 255 {
        255
    } else {
        x as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三種布局都要认得：裸 JPEG / v3.10 的 [orient][JPEG] / 新的 [codec<<4|orient][数据]
    #[test]
    fn split_frame_recognizes_all_layouts() {
        let jpeg = [0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10];

        // 老包裸 JPEG
        let (c, o, b) = split_frame(&jpeg);
        assert_eq!((c, o), (CODEC_JPEG, 0));
        assert_eq!(b, &jpeg);

        // v3.10 包：orient=3 的 JPEG
        let mut v310 = vec![0x03u8];
        v310.extend_from_slice(&jpeg);
        let (c, o, b) = split_frame(&v310);
        assert_eq!((c, o), (CODEC_JPEG, 3), "v3.10 包必须解成 JPEG + 原 orient");
        assert_eq!(b, &jpeg);

        // 新包：H.264（codec=1）+ orient=1，本体是 Annex B 起始码
        let h264_body = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0x88];
        let mut pkt = vec![(CODEC_H264 << 4) | 1];
        pkt.extend_from_slice(&h264_body);
        let (c, o, b) = split_frame(&pkt);
        assert_eq!((c, o), (CODEC_H264, 1));
        assert_eq!(b, &h264_body);

        // H.265（codec=2）
        let mut pkt = vec![(CODEC_H265 << 4) | 2];
        pkt.extend_from_slice(&h264_body);
        let (c, o, _) = split_frame(&pkt);
        assert_eq!((c, o), (CODEC_H265, 2));

        // 退化输入不 panic
        assert_eq!(split_frame(&[]).0, CODEC_JPEG);
        assert_eq!(split_frame(&[0x01, 0xFF]).0, CODEC_JPEG);
    }

    /// BT.601 limited-range 的已知值校验：
    /// 中灰 Y=128,U=V=128 不该是纯黑；黑 Y=16,U=V=128 应接近 0；
    /// 白 Y=235,U=V=128 应接近 255。
    #[test]
    fn yuv_to_rgb_known_values() {
        let (w, h) = (2, 2);
        let y = vec![16u8, 128, 235, 128];
        let u = vec![128u8];
        let v = vec![128u8];

        let rgb = yuv420_to_rgb(&y, &u, &v, w, h, 3);
        assert_eq!(rgb.len(), w as usize * h as usize * 3);

        let px = |i: usize| (rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2]);
        let black = px(0);
        let gray = px(1);
        let white = px(2);

        // 黑：Y=16 是 limited range 的下界
        assert!(black.0 <= 2 && black.1 <= 2 && black.2 <= 2, "黑应接近 0: {black:?}");
        // 白：Y=235 是上界
        assert!(white.0 >= 250 && white.1 >= 250 && white.2 >= 250, "白应接近 255: {white:?}");
        // 中灰：三通道相等且明显在中间
        assert_eq!(gray.0, gray.1);
        assert_eq!(gray.1, gray.2);
        assert!((80..180).contains(&gray.0), "中灰应在中间段: {gray:?}");
        // 单调递增
        assert!(black.0 < gray.0 && gray.0 < white.0);
    }

    /// 色度确实在起作用：U/V 偏离 128 时不能再是灰的
    #[test]
    fn yuv_chroma_affects_color() {
        let y = vec![128u8, 128, 128, 128];
        // U(Cb) 低、V(Cr) 高 → 偏红
        let rgb = yuv420_to_rgb(&y, &[64], &[192], 2, 2, 3);
        let (r, _g, b) = (rgb[0], rgb[1], rgb[2]);
        assert!(r > b + 20, "V 高 U 低时应明显偏红: r={r} b={b}");

        // RGB 输出就该是 npx*3，没有 alpha 通道
        assert_eq!(rgb.len(), 4 * 3);

        // RGBA 输出：第 4 个字节是不透明的 alpha
        let rgba = yuv420_to_rgb(&y, &[64], &[192], 2, 2, 4);
        assert_eq!(rgba.len(), 4 * 4);
        assert_eq!(rgba[3], 255, "RGBA 的 alpha 必须是不透明");
        assert_eq!(rgba[7], 255);
    }

    #[test]
    fn codec_name_is_stable() {
        assert_eq!(codec_name(CODEC_JPEG), "JPEG");
        assert_eq!(codec_name(CODEC_H264), "H.264");
        assert_eq!(codec_name(CODEC_H265), "H.265");
    }
}
