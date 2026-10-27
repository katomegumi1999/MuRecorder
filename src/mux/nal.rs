//! 码流层面的转换：**Annex B ⇄ MP4 长度前缀**，以及 `avcC` / `hvcC` 的构建。
//!
//! ## 为什么需要它
//! Media Foundation 的编码器 MFT（就是这个路径下 `IMFSinkWriter` 挑出来的那个）
//! 输出的是 **Annex B**：NAL 之间用 `00 00 01` / `00 00 00 01` 起始码分隔，
//! `MF_MT_MPEG_SEQUENCE_HEADER` 里给的也是 Annex B 形式的 VPS/SPS/PPS。
//! 平时看不到这些，是因为 MF 自带的 MP4 sink 替我们做了转换——
//! 而 [`crate::encode::sink_raw`] 把写盒拿回来之后，这活就得自己做。
//!
//! MP4 要的是三样东西：
//! 1. 样本内 NAL 改用 **4 字节大端长度前缀**；
//! 2. `stsd` 里一个 `avcC` / `hvcC` 配置盒（参数集 out-of-band）；
//! 3. 参数集**不要**再重复出现在样本里（`avc1`/`hvc1` 的约定）。
//!
//! ## 实测样例（AMD 硬件编码器 + 640×480）
//! ```text
//! H.264 参数集: 00 00 00 01 67 42 04 1E …  00 00 00 01 68 CE 3C 80
//! HEVC  参数集: 00 00 00 01 42 01 01 01 60 …  00 00 00 01 44 01 …  00 00 00 01 40 01 …
//! ```
//! 注意 HEVC 的顺序是 **SPS → PPS → VPS**，所以必须按 NAL 类型分拣，
//! 不能按出现顺序假定。

use crate::mux::mp4::build_box;

/// 一组参数集（已去掉起始码的裸 NAL）。
#[derive(Debug, Default, Clone)]
pub struct ParameterSets {
    pub vps: Vec<u8>,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

/// H.264 NAL 类型（`nal[0] & 0x1F`）。
pub fn h264_nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1F)
}

/// HEVC NAL 类型（`(nal[0] >> 1) & 0x3F`）。
pub fn hevc_nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3F)
}

/// 按 Annex B 起始码切分 NAL（**不含**起始码本身）。
///
/// 同时接受 3 字节（`00 00 01`）与 4 字节（`00 00 00 01`）起始码；
/// 起始码之前的前导零会被吞掉（标准允许 `leading_zero_8bits`）。
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0usize;
    let mut start: Option<usize> = None;
    while i < data.len() {
        let sc3 = i + 3 <= data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let sc4 = i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1;
        if sc4 {
            if let Some(s) = start.take() {
                nals.push(&data[s..i]);
            }
            start = Some(i + 4);
            i += 4;
        } else if sc3 {
            if let Some(s) = start.take() {
                nals.push(&data[s..i]);
            }
            start = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(s) = start
        && s < data.len()
    {
        nals.push(&data[s..]);
    }
    nals
}

/// 数据是否以 Annex B 起始码开头。
pub fn looks_like_annexb(data: &[u8]) -> bool {
    data.len() >= 3
        && data[0] == 0
        && data[1] == 0
        && (data[2] == 1 || (data.len() >= 4 && data[2] == 0 && data[3] == 1))
}

/// 从 Annex B 描述里挑出参数集。`hevc = false` 时按 H.264 类型解析。
pub fn parse_parameter_sets(annexb: &[u8], hevc: bool) -> ParameterSets {
    let mut ps = ParameterSets::default();
    for nal in split_annexb(annexb) {
        if hevc {
            match hevc_nal_type(nal) {
                Some(32) => ps.vps = nal.to_vec(),
                Some(33) => ps.sps = nal.to_vec(),
                Some(34) => ps.pps = nal.to_vec(),
                _ => {}
            }
        } else {
            match h264_nal_type(nal) {
                Some(7) => ps.sps = nal.to_vec(),
                Some(8) => ps.pps = nal.to_vec(),
                _ => {}
            }
        }
    }
    ps
}

/// Annex B → MP4 长度前缀（4 字节大端），并丢掉参数集 NAL。
///
/// 参数集在 `stsd` 里给过了（out-of-band），样本里再出现一遍既浪费空间，
/// 也让部分严格解析器（如部分 Apple 播放栈）不满意——`avc1`/`hvc1` 的语义
/// 就是「参数集只在样本描述里」。
///
/// 已经是长度前缀的数据原样返回（幂等），这样重复调用不会损坏码流。
pub fn annexb_to_mp4_sample(data: &[u8], hevc: bool) -> Vec<u8> {
    if !looks_like_annexb(data) {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    for nal in split_annexb(data) {
        let is_param = if hevc {
            matches!(hevc_nal_type(nal), Some(32..=34))
        } else {
            matches!(h264_nal_type(nal), Some(7) | Some(8))
        };
        if is_param || nal.is_empty() {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// `avcC` 盒（含盒头）。`sps`/`pps` 是**裸 NAL**（不含起始码）。
pub fn build_avcc(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if sps.len() < 4 || pps.is_empty() {
        return None;
    }
    let mut p = vec![
        1,      // configurationVersion
        sps[1], // AVCProfileIndication
        sps[2], // profile_compatibility
        sps[3], // AVCLevelIndication
        0xFF,   // 6b reserved(111111) + lengthSizeMinusOne=3（4 字节长度）
        0xE1,   // 3b reserved(111) + numOfSequenceParameterSets=1
    ];
    p.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    p.extend_from_slice(sps);
    p.push(1); // numOfPictureParameterSets
    p.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    p.extend_from_slice(pps);
    Some(build_box(b"avcC", &p))
}

/// `hvcC` 盒（含盒头）。
///
/// ## 哪些字段是「抄」来的，哪些是「定死」的
/// 抄自 SPS 的 `profile_tier_level` 前 12 字节（原样对齐，`hvcC` 的
/// 剪接布局与它逐位相同：2b profile_space + 1b tier + 5b profile_idc +
/// 32b compatibility + 48b constraint，共 11 字节，再接 1 字节 level_idc）。
///
/// 定死的是这几项，理由都是「本项目的管线决定了它」：
/// * `chromaFormat = 1`（4:2:0）、`bitDepth = 8`：输入就是 NV12，
///   编码器不可能吐别的；
/// * `minSpatialSegmentation = 0`（未知）、`parallelismType = 0`、
///   `avgFrameRate = 0`（未知）、`constantFrameRate = 0`：都是可选的优化提示，
///   写 0 表示「未指定」，解码器不会因此出错；
/// * `lengthSizeMinusOne = 3`：我们统一写 4 字节长度前缀；
/// * `numTemporalLayers` 抄 SPS 的 `sps_max_sub_layers_minus1 + 1`。
pub fn build_hvcc(ps: &ParameterSets) -> Option<Vec<u8>> {
    if ps.vps.is_empty() || ps.sps.len() < 16 || ps.pps.is_empty() {
        return None;
    }
    // 跳过 2 字节 NAL 头：sps_video_parameter_set_id(4) / max_sub_layers_minus1(3) /
    // temporal_id_nesting(1)，然后就是 profile_tier_level。
    let rbsp = unescape_ebsp(&ps.sps[2..]);
    if rbsp.len() < 13 {
        return None;
    }
    let max_sub_layers_minus1 = (rbsp[0] >> 1) & 0x07;
    let ptl = &rbsp[1..13]; // 11 字节 PTL + 1 字节 general_level_idc

    let mut p = Vec::with_capacity(23 + ps.sps.len() + ps.pps.len() + ps.vps.len() + 18);
    p.push(1); // configurationVersion
    p.extend_from_slice(&ptl[0..11]); // profile_space/tier/profile_idc + compat + constraint
    p.push(ptl[11]); // general_level_idc
    p.extend_from_slice(&0xF000u16.to_be_bytes()); // 4b reserved + 12b min_spatial_segmentation_idc=0
    p.push(0xFC); // 6b reserved + 2b parallelismType=0
    p.push(0xFD); // 6b reserved + 2b chromaFormat=1 (4:2:0)
    p.push(0xF8); // 5b reserved + 3b bitDepthLumaMinus8=0
    p.push(0xF8); // 5b reserved + 3b bitDepthChromaMinus8=0
    p.extend_from_slice(&0u16.to_be_bytes()); // avgFrameRate
    // 2b constantFrameRate=0 | 3b numTemporalLayers | 1b temporalIdNested=1 | 2b lengthSizeMinusOne=3
    let layers = (max_sub_layers_minus1 + 1) & 0x07;
    p.push((layers << 3) | 0b100 | 0b11);
    p.push(3); // numOfArrays
    for (nal_type, nal) in [(32u8, &ps.vps), (33, &ps.sps), (34, &ps.pps)] {
        // array_completeness=1 | reserved=0 | 6b NAL_unit_type
        p.push(0x80 | (nal_type & 0x3F));
        p.extend_from_slice(&1u16.to_be_bytes()); // numNalus
        p.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        p.extend_from_slice(nal);
    }
    Some(build_box(b"hvcC", &p))
}

/// 去掉仿真防竞争字节（emulation prevention）。
///
/// 码流里 `00 00 00` / `00 00 01` / `00 00 02` / `00 00 03` 会被插入一个 `03`，
/// 所以 `00 00 03 00` 实际是 `00 00 00`。**解析 SPS 前必须先剥掉**，
/// 否则算出来的 `general_level_idc` 会整体错位——实测就是这么踩到的：
/// 不剥的时候 level 读成 `0x00`，剥完才是正确的 `0x5A`（Level 3.0）。
fn unescape_ebsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue; // 丢掉这个 03
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实抓到的 H.264 参数集（AMD 硬编 640×480）。
    const H264_PARAMS: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x04, 0x1E, 0xE8, 0xA0, 0x50, 0x1E, 0xDF, 0xF8, 0x00,
        0x08, 0x00, 0x0A, 0x10, 0x00, 0x00, 0x24, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03,
        0x00, 0xF2, 0x3C, 0x22, 0x11, 0x96, 0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80,
    ];

    /// 真实抓到的 HEVC 参数集，注意顺序是 SPS → PPS → VPS。
    const HEVC_PARAMS: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xB0, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x5A, 0xA0, 0x05, 0x02, 0x01, 0xE1, 0x72, 0xE4, 0x90,
        0xA5, 0x64, 0xB8, 0xFF, 0xC0, 0x00, 0x40, 0x00, 0x5A, 0x80, 0x00, 0x00, 0x03, 0x02, 0x00,
        0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x3D, 0x88, 0x00, 0x00, 0x00, 0x01, 0x44,
        0x01, 0xE0, 0xF3, 0xC0, 0x4C, 0x90, 0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0C, 0x01, 0xFF,
        0xFF, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xB0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00,
        0x5A, 0x2C, 0x09,
    ];

    #[test]
    fn splits_annexb_with_both_start_codes() {
        let data = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, // 4 字节头
            0x00, 0x00, 0x01, 0x68, 0xCC, // 3 字节头
        ];
        let nals = split_annexb(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x67, 0xAA]);
        assert_eq!(nals[1], &[0x68, 0xCC]);
    }

    #[test]
    fn parses_h264_parameter_sets_by_nal_type() {
        let ps = parse_parameter_sets(H264_PARAMS, false);
        assert_eq!(h264_nal_type(&ps.sps), Some(7));
        assert_eq!(h264_nal_type(&ps.pps), Some(8));
        assert!(ps.vps.is_empty());
        // 裸 NAL，不应残留起始码
        assert_ne!(&ps.sps[..2], &[0x00, 0x00]);
    }

    #[test]
    fn parses_hevc_parameter_sets_regardless_of_order() {
        let ps = parse_parameter_sets(HEVC_PARAMS, true);
        // 文件里出现顺序是 SPS/PPS/VPS，解析结果必须按类型归位
        assert_eq!(hevc_nal_type(&ps.sps), Some(33));
        assert_eq!(hevc_nal_type(&ps.pps), Some(34));
        assert_eq!(hevc_nal_type(&ps.vps), Some(32));
    }

    #[test]
    fn avcc_matches_profile_and_level_from_sps() {
        let ps = parse_parameter_sets(H264_PARAMS, false);
        let avcc = build_avcc(&ps.sps, &ps.pps).expect("avcC 应能构建");
        // 盒头 8 字节 + configurationVersion
        assert_eq!(&avcc[4..8], b"avcC");
        assert_eq!(avcc[8], 1); // configurationVersion
        assert_eq!(avcc[9], 0x42); // profile_idc：SPS 第 2 字节
        assert_eq!(avcc[10], 0x04); // compatibility
        assert_eq!(avcc[11], 0x1E); // level 3.0
        assert_eq!(avcc[12], 0xFF); // lengthSizeMinusOne = 3
    }

    #[test]
    fn hvcc_level_idc_is_read_after_unescaping() {
        let ps = parse_parameter_sets(HEVC_PARAMS, true);
        let hvcc = build_hvcc(&ps).expect("hvcC 应能构建");
        assert_eq!(&hvcc[4..8], b"hvcC");
        assert_eq!(hvcc[8], 1); // configurationVersion
        // general_profile_space=0 / tier=0 / profile_idc=1(Main)
        assert_eq!(hvcc[9], 0x01);
        // 盒头 8 字节 + configurationVersion(1) + PTL(11) = 20
        // general_level_idc：原始 SPS 里被 00 00 03 00 隔开，剥掉仿真字节后是 0x5A
        assert_eq!(hvcc[20], 0x5A, "level 应为 0x5A（Level 3.0）");
        // chromaFormat=1（4:2:0）、bitDepth=8
        assert_eq!(hvcc[24], 0xFD);
        assert_eq!(hvcc[25], 0xF8);
        assert_eq!(hvcc[26], 0xF8);
        // numOfArrays=3，且首个数组类型是 VPS(32)
        assert_eq!(hvcc[30], 3);
        assert_eq!(hvcc[31] & 0x3F, 32);
    }

    #[test]
    fn hvcc_arrays_are_ordered_vps_sps_pps() {
        let ps = parse_parameter_sets(HEVC_PARAMS, true);
        let hvcc = build_hvcc(&ps).unwrap();
        // 三个数组的类型依次出现（固定头共 23 字节，加盒头 8 → 31）
        let mut offset = 31usize;
        let mut kinds = Vec::new();
        for _ in 0..3 {
            kinds.push(hvcc[offset] & 0x3F);
            let nalu_count = u16::from_be_bytes([hvcc[offset + 1], hvcc[offset + 2]]);
            assert_eq!(nalu_count, 1);
            let len = u16::from_be_bytes([hvcc[offset + 3], hvcc[offset + 4]]) as usize;
            offset += 5 + len;
        }
        assert_eq!(kinds, vec![32, 33, 34]);
        assert_eq!(offset, hvcc.len(), "hvcC 长度应刚好等于游标位置");
    }

    #[test]
    fn conversion_strips_parameter_sets_and_uses_length_prefix() {
        // 一个 SPS + 一个 IDR
        let mut data = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA];
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0xDE, 0xAD]);
        let out = annexb_to_mp4_sample(&data, false);
        // 只剩 IDR：4 字节长度 3 + 3 字节载荷
        assert_eq!(out, vec![0x00, 0x00, 0x00, 0x03, 0x65, 0xDE, 0xAD]);
    }

    #[test]
    fn conversion_is_idempotent_on_length_prefixed_input() {
        let prefixed = vec![0x00, 0x00, 0x00, 0x02, 0x65, 0x01];
        assert_eq!(annexb_to_mp4_sample(&prefixed, false), prefixed);
    }

    #[test]
    fn unescape_removes_emulation_prevention_only_after_two_zeros() {
        assert_eq!(unescape_ebsp(&[0, 0, 3, 0]), vec![0, 0, 0]);
        assert_eq!(unescape_ebsp(&[0, 0, 2]), vec![0, 0, 2]);
        assert_eq!(unescape_ebsp(&[0, 3, 0]), vec![0, 3, 0]);
    }
}
