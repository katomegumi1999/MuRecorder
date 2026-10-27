//! 软件 H.264 编码器（openh264 纯 Rust 实现，回退链末位）。
//!
//! 输入 I420（由 [`crate::convert::i420`] 从 BGRA 转得），输出 Annex-B 码流。
//! openh264 在 ConstantId 策略下，每个 IDR 前都携带 SPS/PPS，本模块逐帧提取，
//! 首个关键帧的 SPS/PPS 同时用于构建 MP4 avcC。

use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType,
    VuiConfig,
};
use openh264::formats::YUVBuffer;

use crate::convert::i420::I420Frame;
use crate::encode::{EncodedPacket, annexb_to_length_prefixed, nal_type, split_annexb};

pub struct SoftEncoder {
    inner: Encoder,
    width: u32,
    height: u32,
    frame_index: u64,
}

impl SoftEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self, String> {
        // openh264 要求偶数边长（4:2:0 抽样）；调用方（config 校验）已排除 0，
        // 这里对奇数向上取整到偶数，避免宏块对齐问题。
        let width = width + (width & 1);
        let height = height + (height & 1);

        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            // 关键：`EncoderConfig` 默认是 `RateControlMode::Quality`，此时目标码率被
            // **完全忽略**，屏幕内容会按固定画质编码——实测 320×240@3Mbps 配置下
            // 输出 27 Mbps，是配置值的 9 倍。录屏必须让用户设置的码率真正生效。
            .rate_control_mode(RateControlMode::Bitrate)
            // 屏幕内容专用的码控调优（静止大色块、文字边缘）。
            .usage_type(UsageType::ScreenContentRealTime)
            // VUI 与 MP4 colr 盒、BGRA→I420 的 BT.709 矩阵保持一致，别让播放器猜。
            .vui(VuiConfig::bt709())
            // 周期性 IDR：默认 auto 在长录制里可能只有一个关键帧，进度条拖不动。
            .intra_frame_period(IntraFramePeriod::from_num_frames(
                fps.saturating_mul(2).max(1),
            ))
            .num_threads(2) // 2 线程片编码，单帧延迟优先场景下的折中
            .skip_frames(true); // 允许跳帧以满足码率目标

        // from_source() = 纯 Rust 实现（crate 默认 feature，无外部 DLL 依赖）。
        let inner = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .map_err(|e| format!("openh264 初始化失败: {e}"))?;

        Ok(Self {
            inner,
            width,
            height,
            frame_index: 0,
        })
    }

    /// 编码一帧。`is_key` 依据码流中是否出现 IDR NAL（type 5）判定。
    pub fn encode(&mut self, frame: &I420Frame) -> Result<EncodedPacket, String> {
        assert_eq!(frame.width, self.width, "帧宽与编码器不一致");
        assert_eq!(frame.height, self.height, "帧高与编码器不一致");

        // 组 I420 平面缓冲：Y | U | V。
        let mut yuv = Vec::with_capacity(frame.y.len() + frame.u.len() + frame.v.len());
        yuv.extend_from_slice(&frame.y);
        yuv.extend_from_slice(&frame.u);
        yuv.extend_from_slice(&frame.v);

        let source = YUVBuffer::from_vec(yuv, self.width as usize, self.height as usize);

        // 首帧强制 IDR（openh264 默认首帧即 IDR，这里显式声明意图）。
        if self.frame_index == 0 {
            self.inner.force_intra_frame();
        }
        self.frame_index += 1;

        let bitstream = self
            .inner
            .encode(&source)
            .map_err(|e| format!("openh264 编码失败: {e}"))?;

        let annexb = bitstream.to_vec();
        let nals = split_annexb(&annexb);
        let is_key = nals.iter().any(|n| nal_type(n) == Some(5));
        let mut sps = Vec::new();
        let mut pps = Vec::new();
        for nal in &nals {
            match nal_type(nal) {
                Some(7) if sps.is_empty() => sps = nal.to_vec(),
                Some(8) if pps.is_empty() => pps = nal.to_vec(),
                _ => {}
            }
        }

        Ok(EncodedPacket {
            data: annexb_to_length_prefixed(&annexb),
            is_key,
            sps,
            pps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::i420::bgra_to_i420;

    fn test_frame(w: u32, h: u32, seed: u8) -> I420Frame {
        let stride = w * 4;
        let mut bgra = vec![0u8; (stride * h) as usize];
        for i in 0..(w * h) as usize {
            bgra[i * 4] = seed.wrapping_add(i as u8);
            bgra[i * 4 + 1] = seed.wrapping_mul(3).wrapping_add(i as u8);
            bgra[i * 4 + 2] = seed.wrapping_mul(7).wrapping_sub(i as u8);
            bgra[i * 4 + 3] = 255;
        }
        bgra_to_i420(&bgra, stride, w, h)
    }

    #[test]
    fn encoder_produces_keyframe_with_sps_pps_then_deltas() {
        let mut enc = SoftEncoder::new(128, 96, 30, 2000).expect("编码器创建失败");
        let f = test_frame(128, 96, 1);

        let p0 = enc.encode(&f).expect("首帧编码失败");
        assert!(p0.is_key, "首帧必须是 IDR");
        assert!(!p0.sps.is_empty(), "IDR 必须携带 SPS");
        assert!(!p0.pps.is_empty(), "IDR 必须携带 PPS");
        assert!(!p0.data.is_empty());

        // 第二帧内容不变（真实录屏里的静止画面重发）→ 必须是 P 帧。
        // 注意：内容剧变时 openh264 的场景切换检测会主动插 IDR，所以这里刻意不变。
        let f2 = test_frame(128, 96, 1);
        let p1 = enc.encode(&f2).expect("第二帧编码失败");
        assert!(!p1.is_key, "静止画面的第二帧不应是 IDR");
        assert!(!p1.data.is_empty());
    }

    /// GOP 必须周期性插入 IDR，否则长录制的进度条无法拖动。
    #[test]
    fn idr_is_inserted_periodically() {
        const FPS: u32 = 30;
        let mut enc = SoftEncoder::new(160, 120, FPS, 2000).expect("编码器创建失败");
        let f = test_frame(160, 120, 5);
        // intra period 设为 fps × 2 = 60 帧；编 75 帧应看到 2 个 IDR。
        let keys = (0..75)
            .filter(|_| enc.encode(&f).expect("编码失败").is_key)
            .count();
        assert!(keys >= 2, "75 帧内应至少 2 个 IDR（实测 {keys}）");
        assert!(keys <= 5, "IDR 过密会浪费码率（实测 {keys}）");
    }

    #[test]
    fn odd_dimensions_are_rounded_up() {
        // 129x97 → 编码器内部对齐到 130x98；帧尺寸必须与之匹配。
        let mut enc = SoftEncoder::new(129, 97, 30, 2000).expect("编码器创建失败");
        assert_eq!(enc.width, 130);
        assert_eq!(enc.height, 98);
        // 用对齐后的尺寸造帧即可编码（调用方按 config 取整后的尺寸准备数据）。
        let f = test_frame(130, 98, 3);
        let p = enc.encode(&f).expect("编码失败");
        assert!(p.is_key);
    }

    /// 目标码率必须真正生效。
    ///
    /// `EncoderConfig::new()` 默认 `RateControlMode::Quality`——那种模式下 `bitrate()`
    /// 被完全忽略，实测 320×240 配 3 Mbps 输出 27 Mbps（9 倍）。本用例用高熵画面
    /// 对比低/高两档码率的产出字节数：若码控失灵，两者会几乎相同。
    #[test]
    fn bitrate_target_is_honoured() {
        fn total_bytes(kbps: u32, frames: u32) -> usize {
            let mut enc = SoftEncoder::new(320, 240, 30, kbps).expect("编码器创建失败");
            let stride = 320u32 * 4;
            let mut total = 0usize;
            for i in 0..frames {
                // 伪随机高熵内容，逼编码器只能靠量化满足码率。
                let mut bgra = vec![0u8; (stride * 240) as usize];
                for (idx, px) in bgra.chunks_mut(4).enumerate() {
                    let v = (idx as u32)
                        .wrapping_mul(2_654_435_761)
                        .wrapping_add(i.wrapping_mul(97))
                        >> 7;
                    px[0] = v as u8;
                    px[1] = (v >> 3) as u8;
                    px[2] = (v >> 5) as u8;
                    px[3] = 255;
                }
                let f = bgra_to_i420(&bgra, stride, 320, 240);
                total += enc.encode(&f).unwrap().data.len();
            }
            total
        }

        let low = total_bytes(200, 30);
        let high = total_bytes(8000, 30);
        assert!(
            low * 2 < high,
            "低码率产出应明显小于高码率：low={low} high={high}（码控可能失效）"
        );
    }
}
