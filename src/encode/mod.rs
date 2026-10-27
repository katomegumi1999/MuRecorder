//! 编码器模块。
//!
//! - [`soft`]：软件 H.264（openh264 纯 Rust 实现），纯 CPU 回退链路。
//! - [`mf_sink`]：硬件路径。`IMFSinkWriter` 负责挑硬件 MFT、协商媒体类型、编码，
//!   H.264 / HEVC / AV1 都从这里出去。
//! - [`sink_raw`]：我们自己的 `IMFMediaSink`。编码器依然交给 [`mf_sink`] 的
//!   SinkWriter 挑选和驱动，但编码后的码流会被它接回来交给 `mux::Mp4Muxer`——
//!   MF 自带的分片 sink 只认 H.264，把「写盒」拿回来后 HEVC 也能分片写盘。

pub mod mf_sink;
pub mod sink_raw;
pub mod soft;

use crate::convert::i420::I420Frame;

/// 已编码包：MP4 用的长度前缀 NAL 序列 + 关键帧标记。
pub struct EncodedPacket {
    /// 4 字节大端长度前缀的 NAL 序列（可直接写入 MP4 mdat）
    pub data: Vec<u8>,
    pub is_key: bool,
    /// SPS/PPS（仅关键帧出现；由编码器元数据得到，与 data 中的内容一致）
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

/// 编码器统一入口（M1 唯一实现：软件）。
pub enum VideoEncoder {
    Soft(soft::SoftEncoder),
}

impl VideoEncoder {
    pub fn new_soft(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self, String> {
        Ok(Self::Soft(soft::SoftEncoder::new(
            width,
            height,
            fps,
            bitrate_kbps,
        )?))
    }

    pub fn encode(&mut self, frame: &I420Frame) -> Result<EncodedPacket, String> {
        match self {
            Self::Soft(e) => e.encode(frame),
        }
    }
}

/// 按 Annex-B 起始码切分 NAL。
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0usize;
    let mut start: Option<usize> = None;
    while i + 2 < data.len() + 1 && i < data.len() {
        if i + 3 <= data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start.take() {
                nals.push(&data[s..i]);
            }
            start = Some(i + 3);
            i += 3;
        } else if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            if let Some(s) = start.take() {
                nals.push(&data[s..i]);
            }
            start = Some(i + 4);
            i += 4;
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < data.len() {
            nals.push(&data[s..]);
        }
    }
    nals
}

/// NAL 类型（`nal[0] & 0x1F`）：5=IDR, 1=non-IDR, 7=SPS, 8=PPS, 6=SEI。
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1F)
}

/// Annex-B NAL 序列转 MP4 长度前缀格式（4 字节大端长度，去掉起始码）。
pub fn annexb_to_length_prefixed(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annexb.len());
    for nal in split_annexb(annexb) {
        let len = nal.len() as u32;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_annexb_handles_3_and_4_byte_start_codes() {
        let data = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB, // 4 字节头 SPS
            0x00, 0x00, 0x01, 0x68, 0xCC, // 3 字节头 PPS
            0x00, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02, // IDR
        ];
        let nals = split_annexb(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[0]), Some(7));
        assert_eq!(nal_type(nals[1]), Some(8));
        assert_eq!(nal_type(nals[2]), Some(5));
    }

    #[test]
    fn split_annexb_ignores_trailing_garbage() {
        let data = [0x00, 0x00, 0x01, 0x65, 0xFF];
        let nals = split_annexb(&data);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0], &[0x65, 0xFF]);
    }

    #[test]
    fn length_prefix_conversion_preserves_nal_payloads() {
        let annexb = [
            0x00, 0x00, 0x00, 0x01, 0x65, 0xDE, 0xAD, 0x00, 0x00, 0x01, 0x41, 0x42,
        ];
        let lp = annexb_to_length_prefixed(&annexb);
        // NAL1 = [65,DE,AD]（3 字节），NAL2 = [41,42]（2 字节）
        assert_eq!(
            lp,
            vec![0, 0, 0, 3, 0x65, 0xDE, 0xAD, 0, 0, 0, 2, 0x41, 0x42]
        );
    }
}
