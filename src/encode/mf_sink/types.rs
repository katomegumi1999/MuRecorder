//! 编码器与容器模式的类型定义，以及 `stsd` 样本描述构建。

use windows::Win32::Media::MediaFoundation::{
    MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC,
};

use crate::mux::mp4::SampleDescription;
use crate::mux::nal::{build_av1c, build_avcc, build_hvcc, parse_parameter_sets};

/// 支持的 GPU 硬件编码目标格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuCodec {
    H264,
    Hevc,
    Av1,
}

impl GpuCodec {
    pub fn subtype(&self) -> &'static windows::core::GUID {
        match self {
            Self::H264 => &MFVideoFormat_H264,
            Self::Hevc => &MFVideoFormat_HEVC,
            Self::Av1 => &MFVideoFormat_AV1,
        }
    }

    pub fn requires_nv12(&self) -> bool {
        matches!(self, Self::Hevc | Self::Av1)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "H.265 (HEVC)",
            Self::Av1 => "AV1",
        }
    }
}

/// MF 自带的 MP4 sink 有两种容器模式，能力差别很大。
///
/// **实测（Windows 11 + AMD 核显）**：`MFTranscodeContainerType_FMPEG4` 只对 H.264
/// 生效，换成 HEVC 时 `BeginWriting()` 直接返回 `MF_E_INVALIDMEDIATYPE`
/// ——与分辨率、profile、像素格式、码率全部无关，也与此处是否挂了 DXGI
/// Device Manager 无关（裸 SinkWriter 复现一致）。也就是说 MF 自带的分片
/// MP4 sink 不支持 HEVC（AV1 同理）。
///
/// 这个限制**只存在于「谁写盒」这一层**，与编码器无关。所以我们把写盒拿回来
/// （[`crate::encode::sink_raw`]），三种落盘模式的差别只剩「盒是谁写的」：
///
/// | 模式 | 录制中可读/可切片 | 崩溃最多丢 | 编码器 |
/// |---|---|---|---|
/// | `Fragmented` | 是（`moof` 自带样本表） | 最后一个分片 | 仅 H.264（MF 限制） |
/// | `RawFragmented` | 是 | 最后一个分片 | 任意（H.264 / HEVC / AV1） |
/// | `Plain` | 否（`moov` 只 `Finalize` 时写） | 整场录制 | 任意 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerMode {
    Fragmented,
    RawFragmented,
    Plain,
}

impl ContainerMode {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fragmented => "分片 fMP4（MF 写盒）",
            Self::RawFragmented => "分片 fMP4（自研写盒）",
            Self::Plain => "普通 MP4",
        }
    }

    /// 录制中是否可读、可切片。
    pub fn is_fragmented(&self) -> bool {
        matches!(self, Self::Fragmented | Self::RawFragmented)
    }

    /// 纯 ASCII 短名（用在文件名/日志里，避免中文与全角括号）。
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Fragmented => "frag-mf",
            Self::RawFragmented => "frag-raw",
            Self::Plain => "plain",
        }
    }
}

/// 由「编码器给出的参数集」构建 `stsd` 的样本描述。
///
/// `header` 是 `MF_MT_MPEG_SEQUENCE_HEADER` 的原文——**Annex B 形式**的
/// VPS/SPS/PPS（不是 `avcC`/`hvcC` 盒体！）。这一点很容易搞错：MF 自带 MP4 sink
/// 的内部分工是「编码器吐 Annex B → sink 转 AVCC + 造配置盒」，
/// 换成自研 sink 之后这段转换就得我们补上，见 [`crate::mux::nal`]。
pub(super) fn sample_description(codec: GpuCodec, header: &[u8]) -> Option<SampleDescription> {
    match codec {
        GpuCodec::H264 => {
            let ps = parse_parameter_sets(header, false);
            let avcc = build_avcc(&ps.sps, &ps.pps)?;
            Some(SampleDescription {
                entry: *b"avc1",
                config_boxes: vec![avcc],
                label: "MuRecorder H.264 (MF)".into(),
            })
        }
        GpuCodec::Hevc => {
            let ps = parse_parameter_sets(header, true);
            let hvcc = build_hvcc(&ps)?;
            Some(SampleDescription {
                entry: *b"hvc1",
                config_boxes: vec![hvcc],
                label: "MuRecorder HEVC (MF)".into(),
            })
        }
        GpuCodec::Av1 => {
            let av1c = build_av1c(header)?;
            Some(SampleDescription {
                entry: *b"av01",
                config_boxes: vec![av1c],
                label: "MuRecorder AV1 (MF)".into(),
            })
        }
    }
}
