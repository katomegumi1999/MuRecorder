//! 线程间流转的消息类型。

use std::sync::Arc;

use crate::encode::aac::AacPacket;

/// 线程间流转的视频帧（CPU 侧 BGRA，已裁剪到 region）。
///
/// `bgra` 用 `Arc<[u8]>`：静止画面要重复投递同一帧，Vec 的每次 clone 都是
/// 一次整帧 memcpy（1080p ≈ 8 MB、4K ≈ 33 MB），Arc 只递增引用计数。
pub struct VideoFrame {
    pub bgra: Arc<[u8]>,
    /// BGRA 行距（字节）；裁剪后为紧凑 width*4
    pub stride: u32,
    pub width: u32,
    pub height: u32,
    /// PTS（微秒，相对录制起点）
    pub pts_us: u64,
}

/// 线程间流转的视频编码包。
pub struct PacketMsg {
    pub data: Vec<u8>,
    pub is_key: bool,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub pts_us: u64,
}

/// 封装线程接收的多轨消息枚举。
pub enum MuxMsg {
    Video(PacketMsg),
    Audio(AacPacket),
}
