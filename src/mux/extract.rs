//! 从录制中的 Hybrid MP4 里提取任意时间区间（零重编码）。
//!
//! 录制中的文件是 Fragmented MP4：每个 `moof` 自带本片样本表，样本位置要么相对
//! `moof` 起点，要么是 `tfhd` 里的显式绝对偏移。两种情况都不需要**修正**偏移，
//! 所以切片既不用重新编码，也不用搬移数据。
//!
//! ## 怎么做到与编解码器无关
//! 早期实现把每个样本读出来交给 [`super::mp4::Mp4Muxer`] 重新封装——那是个
//! H.264 专用写入器，于是 HEVC / AV1 的录制直接切不了。现在换成另一条路：
//!
//! 1. 从 init moov 里**照抄** `stsd`（`avcC` / `hvcC` / `av1C` 一个字节都不改）；
//! 2. 把选中的样本字节**整段连续复制**到新文件（中间夹着的 `moof` 头当空隙留下）；
//! 3. 用 [`build_plain_moov`] 生成全量索引，末尾追加，再做同一套 16 字节 soft remux。
//!
//! 索引里只有绝对偏移和计数，没有一行跟编解码器有关，所以 H.264 / HEVC / AV1
//! 走的是同一条代码路径。这也顺带修掉一个隐患：不再假设 NAL 长度前缀是 4 字节。
//!
//! 成品是**普通 MP4**（`ftyp / mdat / moov`）：`mdat` 里那点 `moof` 残余对读取方
//! 是黑盒，播放器只按 `moov` 定位样本。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::fragment::{
    SampleInfo, box_body, fill_missing_durations, parse_fragment_multi, parse_layouts, read_at,
    scan_top_level,
};
use super::mp4::build_mdat_header;

/// 复制媒体数据时的缓冲区上限。
const COPY_BUF: usize = 1 << 20;

/// 切片结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceInfo {
    /// 实际起点：对齐到关键帧后，相对**源文件**的时间
    pub source_start_us: u64,
    /// 实际终点（不含），相对源文件
    pub source_end_us: u64,
    /// 输出文件时长
    pub duration_us: u64,
    /// 输出文件字节数
    pub bytes: u64,
    /// 输出文件包含的样本数
    pub sample_count: usize,
}

/// 从 `src` 里取出覆盖 `[start_us, end_us)` 的样本，写到 `dst`。
///
/// `fps` 只在样本连时长信息都缺失时用作标称帧间隔兜底（可从录制配置取）。
pub fn extract_range(
    src: &Path,
    dst: &Path,
    start_us: u64,
    end_us: u64,
    fps: u32,
) -> io::Result<SliceInfo> {
    if end_us <= start_us {
        return Err(io::Error::other(format!(
            "时间区间非法：[{start_us}, {end_us}) 要求 end > start"
        )));
    }

    let mut source = File::open(src)?;
    let file_len = source.metadata()?.len();
    let boxes = scan_top_level(&mut source, file_len)?;

    let ftyp = boxes
        .iter()
        .find(|b| &b.kind == b"ftyp")
        .ok_or_else(|| io::Error::other("源文件缺少 ftyp 盒，不是有效的 MP4"))?;
    let ftyp_len = ftyp.size;

    // 切片的前提是「分片态」：样本表在 `moof` 里。
    // 两种非分片情况要分开报错，因为它们的原因和处置完全不同。
    let first_moof = boxes.iter().position(|b| &b.kind == b"moof");
    let has_moov = boxes.iter().any(|b| &b.kind == b"moov");
    const FINALIZED: &str = "该文件已完成封装（ftyp/mdat/moov），切片请对录制中的文件调用，\
         或用 ffmpeg -c copy -ss/-to 处理";
    const NOT_FRAGMENTED: &str = "该文件不是分片式 MP4：索引（moov）要到停止录制时才写，录制中无法切片。\
         GPU AV1 只能用普通 MP4 落盘，停止后再切，\
         或用 ffmpeg -c copy -ss/-to 处理";
    match first_moof {
        Some(m) => {
            if let Some(mdat) = boxes.iter().position(|b| &b.kind == b"mdat")
                && mdat < m
            {
                return Err(io::Error::other(FINALIZED));
            }
        }
        None => {
            return Err(io::Error::other(if has_moov {
                FINALIZED
            } else {
                NOT_FRAGMENTED
            }));
        }
    }

    let init_moov = boxes
        .iter()
        .find(|b| &b.kind == b"moov")
        .ok_or_else(|| io::Error::other("录制中文件缺少 init moov（尚未收到关键帧）"))?;
    let init_bytes = read_at(&mut source, init_moov.offset, init_moov.size)?;
    let init_body = box_body(&init_bytes)?;
    let layouts = parse_layouts(init_body)?;

    let (&video_track_id, video_layout) = layouts
        .iter()
        .find(|(_, l)| &l.handler_type == b"vide")
        .or_else(|| layouts.iter().find(|(id, _)| **id == 1))
        .or_else(|| layouts.iter().next())
        .ok_or_else(|| io::Error::other("未找到视频轨"))?;

    let audio_track = layouts
        .iter()
        .find(|(id, l)| &l.handler_type == b"soun" || **id == 2);

    // ---- 解析全部样本（按轨归类） ----
    let mut track_samples: std::collections::HashMap<u32, Vec<SampleInfo>> =
        std::collections::HashMap::new();
    for moof in boxes.iter().filter(|b| &b.kind == b"moof") {
        let bytes = read_at(&mut source, moof.offset, moof.size)?;
        parse_fragment_multi(&bytes, moof.offset, &layouts, &mut track_samples)?;
    }

    let mut video_samples = track_samples.remove(&video_track_id).unwrap_or_default();
    if video_samples.is_empty() {
        return Err(io::Error::other("尚未落盘任何完整分片"));
    }

    let v_ts = video_layout.media_timescale.max(1) as u64;
    let nominal_video = if fps > 0 {
        (v_ts / fps as u64).max(1) as u32
    } else {
        0
    };
    fill_missing_durations(&mut video_samples, nominal_video);

    let mut audio_samples = if let Some((&a_id, _)) = audio_track {
        track_samples.remove(&a_id).unwrap_or_default()
    } else {
        Vec::new()
    };
    if !audio_samples.is_empty() {
        fill_missing_durations(&mut audio_samples, 1024);
    }

    // ---- 微秒 ↔ 视频时基 ----
    let to_video_tick = |us: u64| (us as u128 * v_ts as u128 / 1_000_000) as u64;
    let to_video_us = |tick: u64| (tick as u128 * 1_000_000 / v_ts as u128) as u64;

    let start_tick = to_video_tick(start_us);
    let end_tick = to_video_tick(end_us);
    let tail = video_samples[video_samples.len() - 1];
    let total_end = tail.dts + tail.duration as u64;
    if start_tick >= total_end {
        return Err(io::Error::other(format!(
            "起点 {start_us} µs 超出已落盘范围（共 {} µs）",
            to_video_us(total_end - video_samples[0].dts)
        )));
    }

    // 起点对齐到关键帧样本
    let mut first = video_samples
        .iter()
        .position(|s| s.dts + s.duration as u64 > start_tick)
        .unwrap_or(video_samples.len() - 1);
    while first > 0 && !video_samples[first].is_key {
        first -= 1;
    }
    if !video_samples[first].is_key {
        return Err(io::Error::other(
            "找不到关键帧，无法独立解码（首个关键帧之前的片段不可切片）",
        ));
    }

    let last = video_samples[first..]
        .iter()
        .rposition(|s| s.dts + s.duration as u64 <= end_tick)
        .map(|i| first + i)
        .ok_or_else(|| io::Error::other("区间内没有任何样本"))?;
    let kept_video = &video_samples[first..=last];
    let aligned_start_us = to_video_us(kept_video[0].dts);
    // 视频真实结束点：**不是**入参 `end_us`。当 `end_us` 落在两个已落盘分片之间、
    // 或者最后一个完整帧还没写盘时，视频只能切到 `end_us` 之前，而音频包更粗
    // （每包 1024 采样 ≈ 21 ms），如果仍按 `end_us` 裁，切片末尾会挂出一段
    // 「没有画面只有声音」的尾巴——播放器里表现为片尾静止或黑屏却还在出声。
    let video_tail = kept_video[kept_video.len() - 1];
    let video_end_us = to_video_us(video_tail.dts + video_tail.duration as u64);

    // 音频样本选取：起点跟随视频关键帧的真实 tick，终点跟随视频真实结束点。
    // 两端都服从视频，音频轨才是这切片的一段，而不是被顺手多带出来的。
    let kept_audio: Vec<SampleInfo> = if let Some((_, a_layout)) = audio_track {
        let a_ts = a_layout.media_timescale.max(1) as u64;
        let to_audio_us = |tick: u64| (tick as u128 * 1_000_000 / a_ts as u128) as u64;
        audio_samples
            .iter()
            .filter(|s| {
                let tick_us = to_audio_us(s.dts);
                tick_us >= aligned_start_us && tick_us < video_end_us
            })
            .copied()
            .collect()
    } else {
        Vec::new()
    };

    // ---- 连续复制媒体字节并为各轨重定位偏移 ----
    let data_base = ftyp_len + 16;
    let mut cur_dst_offset = data_base;

    let mut shifted_video = Vec::with_capacity(kept_video.len());
    for s in kept_video {
        shifted_video.push(SampleInfo {
            offset: cur_dst_offset,
            ..*s
        });
        cur_dst_offset += s.size as u64;
    }

    let mut shifted_audio = Vec::with_capacity(kept_audio.len());
    for s in &kept_audio {
        shifted_audio.push(SampleInfo {
            offset: cur_dst_offset,
            ..*s
        });
        cur_dst_offset += s.size as u64;
    }

    let copy_len = cur_dst_offset - data_base;

    let mut slice_layouts = std::collections::HashMap::new();
    let mut slice_samples = std::collections::HashMap::new();
    slice_layouts.insert(video_track_id, video_layout.clone());
    slice_samples.insert(video_track_id, shifted_video);

    if let Some((&a_id, a_layout)) = audio_track {
        if !shifted_audio.is_empty() {
            slice_layouts.insert(a_id, a_layout.clone());
            slice_samples.insert(a_id, shifted_audio);
        }
    }

    let moov = super::remux::build_plain_moov_multi(init_body, &slice_layouts, &slice_samples)?;

    let mut out = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    out.write_all(&read_at(&mut source, ftyp.offset, ftyp_len)?)?;
    out.write_all(&[0u8; 16])?; // mdat 头占位，最后回填

    // 按轨流式写入样本数据
    copy_sample_runs(&mut source, &mut out, kept_video)?;
    if !kept_audio.is_empty() {
        copy_sample_runs(&mut source, &mut out, &kept_audio)?;
    }

    out.write_all(&moov)?;

    // 收尾：改写 64 位 mdat 头
    out.seek(SeekFrom::Start(ftyp_len))?;
    out.write_all(&build_mdat_header(16 + copy_len))?;
    out.flush()?;
    out.sync_all()?;

    let v_head = kept_video[0];
    let v_tail = kept_video[kept_video.len() - 1];
    let duration_us = to_video_us(v_tail.dts + v_tail.duration as u64 - v_head.dts);
    let source_start_us = aligned_start_us;

    Ok(SliceInfo {
        source_start_us,
        source_end_us: source_start_us + duration_us,
        duration_us,
        bytes: data_base + copy_len + moov.len() as u64,
        sample_count: kept_video.len() + kept_audio.len(),
    })
}

/// 把一组样本从 source 中连续的区间流式复制到 out。
fn copy_sample_runs(source: &mut File, out: &mut File, samples: &[SampleInfo]) -> io::Result<()> {
    let mut i = 0;
    while i < samples.len() {
        let start_offset = samples[i].offset;
        let mut len = samples[i].size as u64;
        let mut j = i + 1;
        while j < samples.len()
            && samples[j].offset == samples[j - 1].offset + samples[j - 1].size as u64
        {
            len += samples[j].size as u64;
            j += 1;
        }
        copy_range(source, out, start_offset, len)?;
        i = j;
    }
    Ok(())
}

/// 把 `source` 的 `[offset, offset+len)` 流式复制到 `out`。
fn copy_range(source: &mut File, out: &mut File, offset: u64, len: u64) -> io::Result<()> {
    source.seek(SeekFrom::Start(offset))?;
    let mut reader = source.take(len);
    let mut buf = vec![0u8; COPY_BUF.min(len.max(1) as usize)];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::fragment::{box_body, descend_full, split_boxes};

    fn make_sps_pps() -> (Vec<u8>, Vec<u8>) {
        (
            vec![0x67, 0x42, 0x00, 0x1E, 0xAB],
            vec![0x68, 0xCE, 0x3C, 0x80],
        )
    }

    /// 造一个「录制中」的文件：5 个分片，每片 30 帧 @30fps（每片 1 秒，最后一片未闭合）。
    ///
    /// `tag` 用来换掉样本描述的盒类型：`(avc1, avcC)` 是真实 H.264，
    /// 换成 `(hvc1, hvcC)` 就是一个 HEVC 形状的文件——两者长度完全相同，
    /// 所以改完所有偏移依然有效，可以直接验证「与编解码器无关」这条路径。
    fn make_recording_with_tag(
        dir: &std::path::Path,
        tag: ([u8; 4], [u8; 4]),
    ) -> std::path::PathBuf {
        let path = dir.join("recording.mp4");
        let (sps, pps) = make_sps_pps();
        {
            let mut muxer = crate::mux::mp4::Mp4Muxer::create(&path, 320, 240, 30).unwrap();
            for i in 0..150u64 {
                // 0/30/60/90/120 是关键帧 → 5 个分片，前 4 片已落盘
                let is_key = i % 30 == 0;
                let mut data = Vec::new();
                if is_key {
                    data.extend_from_slice(&(sps.len() as u32).to_be_bytes());
                    data.extend_from_slice(&sps);
                    data.extend_from_slice(&(pps.len() as u32).to_be_bytes());
                    data.extend_from_slice(&pps);
                }
                let payload = [0x65u8; 100];
                data.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                data.extend_from_slice(&payload);
                muxer
                    .write_sample(&data, i * 1_000_000 / 30, is_key, &sps, &pps)
                    .unwrap();
            }
        }
        retag(&path, tag);
        path
    }

    fn make_recording(dir: &std::path::Path) -> std::path::PathBuf {
        make_recording_with_tag(dir, (*b"avc1", *b"avcC"))
    }

    /// 原地把样本描述的盒类型改名（等长替换，不影响任何偏移）。
    fn retag(path: &std::path::Path, tag: ([u8; 4], [u8; 4])) {
        if tag == (*b"avc1", *b"avcC") {
            return;
        }
        let mut bytes = std::fs::read(path).unwrap();
        for (from, to) in [(*b"avc1", tag.0), (*b"avcC", tag.1)] {
            let mut hits = 0;
            let mut pos = 0;
            while let Some(off) = bytes[pos..]
                .windows(4)
                .position(|w| w == from)
                .map(|p| p + pos)
            {
                bytes[off..off + 4].copy_from_slice(&to);
                pos = off + 4;
                hits += 1;
            }
            assert!(
                hits >= 1,
                "源文件里找不到 {}",
                String::from_utf8_lossy(&from)
            );
        }
        std::fs::write(path, bytes).unwrap();
    }

    /// 断言顶层盒是 `ftyp/mdat/moov`（并因此覆盖整个文件）。
    fn assert_top_level(bytes: &[u8]) {
        let kinds: Vec<Vec<u8>> = split_boxes(bytes)
            .unwrap()
            .iter()
            .map(|b| b.kind.to_vec())
            .collect();
        assert_eq!(
            kinds,
            vec![b"ftyp".to_vec(), b"mdat".to_vec(), b"moov".to_vec()],
            "切片产物应是普通 MP4"
        );
    }

    /// 切中间一段：产物必须是可直接播放的普通 MP4，起点对齐关键帧、终点精确裁。
    #[test]
    fn extracts_middle_range_into_plain_mp4() {
        let dir = tempfile::tempdir().unwrap();
        let src = make_recording(dir.path());
        let dst = dir.path().join("slice.mp4");

        let info = extract_range(&src, &dst, 1_100_000, 2_500_000, 30).unwrap();
        assert_eq!(info.source_start_us, 1_000_000, "应对齐到第 2 个分片起点");
        assert!(
            (2_495_000..=2_500_000).contains(&info.source_end_us),
            "终点应裁到 2.5 s 之内，实际 {}",
            info.source_end_us
        );
        assert_eq!(info.sample_count, 45, "1.5 秒 × 30fps");
        assert!(
            (1_490_000..=1_510_000).contains(&info.duration_us),
            "时长应约 1.5 s，实际 {} µs",
            info.duration_us
        );

        let bytes = std::fs::read(&dst).unwrap();
        assert_eq!(info.bytes as usize, bytes.len());
        assert_top_level(&bytes);
    }

    /// 起点落在分片中间时，必须回溯到含该时间的关键帧，否则解不出来；
    /// 终点则精确裁到请求位置。
    #[test]
    fn start_aligns_to_keyframe() {
        let dir = tempfile::tempdir().unwrap();
        let src = make_recording(dir.path());
        let dst = dir.path().join("aligned.mp4");

        // 3.7 s 落在第 4 片（3..4 s）中间 → 回溯到 3 s 的关键帧
        let info = extract_range(&src, &dst, 3_700_000, 3_900_000, 30).unwrap();
        assert_eq!(info.source_start_us, 3_000_000);
        assert_eq!(info.source_end_us, 3_900_000);
        assert_eq!(info.sample_count, 27, "0.9 秒 × 30fps");
    }

    /// 对着已 finalize 的文件切片应给出明确错误，而不是产出垃圾。
    #[test]
    fn finalized_file_is_rejected_with_hint() {
        let dir = tempfile::tempdir().unwrap();
        let src = make_recording(dir.path());
        let dst = dir.path().join("bad.mp4");
        let (sps, pps) = make_sps_pps();
        {
            let mut muxer = crate::mux::mp4::Mp4Muxer::create(&src, 320, 240, 30).unwrap();
            muxer
                .write_sample(&[0x41u8; 32], 0, true, &sps, &pps)
                .unwrap();
            muxer.finish().unwrap();
        }
        let err = extract_range(&src, &dst, 0, 500_000, 30).unwrap_err();
        assert!(
            err.to_string().contains("已完成封装"),
            "错误信息应提示文件已封装，实际：{err}"
        );
    }

    #[test]
    fn invalid_range_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let src = make_recording(dir.path());
        let dst = dir.path().join("bad.mp4");
        assert!(extract_range(&src, &dst, 500_000, 500_000, 30).is_err());
        assert!(extract_range(&src, &dst, 900_000, 100_000, 30).is_err());
    }

    /// 取 `moov → trak → mdia → minf → stbl → stsd` 的完整字节。
    fn stsd_of(bytes: &[u8]) -> &[u8] {
        let moov = split_boxes(bytes)
            .unwrap()
            .into_iter()
            .find(|b| &b.kind == b"moov")
            .expect("文件里应有 moov");
        descend_full(
            moov.body(bytes),
            &[*b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd"],
        )
        .expect("moov 里应有 stsd")
    }

    /// **HEVC / AV1 的核心保证**：切片照抄样本描述，一个字节都不改。
    ///
    /// 把一个 H.264 形状的分片文件改名成 `hvc1`/`hvcC` 或 `av01`/`av1C`
    /// （长度不变，偏移全部有效），切片产物的 `stsd` 必须与源文件**逐字节相同**
    /// ——这说明整条路径里没有任何编码器相关的逻辑（既不重建 `avcC`，
    /// 也不假设 NAL 长度前缀是 4 字节）。
    #[test]
    fn sample_description_is_copied_verbatim() {
        for (tag, label, params) in [
            ((*b"hvc1", *b"hvcC"), "H.265", *b"hvcC"),
            ((*b"av01", *b"av1C"), "AV1", *b"av1C"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let src = make_recording_with_tag(dir.path(), tag);
            let dst = dir.path().join("slice.mp4");

            let info = extract_range(&src, &dst, 1_100_000, 2_500_000, 30).unwrap();
            assert_eq!(info.sample_count, 45, "[{label}] 样本数");

            let src_bytes = std::fs::read(&src).unwrap();
            let out_bytes = std::fs::read(&dst).unwrap();
            assert_top_level(&out_bytes);

            let src_stsd = stsd_of(&src_bytes);
            let out_stsd = stsd_of(&out_bytes);
            assert_eq!(
                src_stsd, out_stsd,
                "[{label}] stsd 必须逐字节相同（原样拷贝，不重建）"
            );
            assert!(
                out_stsd.windows(4).any(|w| w == tag.0),
                "[{label}] 产物样本描述应是 {}",
                String::from_utf8_lossy(&tag.0)
            );
            assert!(
                out_stsd.windows(4).any(|w| w == params),
                "[{label}] 产物应保留参数集盒",
            );
            assert!(
                !out_stsd.windows(4).any(|w| w == b"avc1" || w == b"avcC"),
                "[{label}] 不应混入 H.264 的 avc1/avcC"
            );
        }
    }

    /// 平移后的 chunk 偏移必须落在新文件的 mdat 内部，否则产物打不开。
    #[test]
    fn shifted_chunk_offsets_point_inside_new_mdat() {
        let dir = tempfile::tempdir().unwrap();
        let src = make_recording(dir.path());
        let dst = dir.path().join("shifted.mp4");
        let info = extract_range(&src, &dst, 2_100_000, 3_500_000, 30).unwrap();

        let bytes = std::fs::read(&dst).unwrap();
        let boxes = split_boxes(&bytes).unwrap();
        let mdat = boxes.iter().find(|b| &b.kind == b"mdat").unwrap();
        let mdat_end = mdat.offset + mdat.size;
        let moov = boxes.iter().find(|b| &b.kind == b"moov").unwrap();
        let stbl =
            descend_full(moov.body(&bytes), &[*b"trak", *b"mdia", *b"minf", *b"stbl"]).unwrap();
        let stbl_body = box_body(stbl).unwrap();
        let inner = split_boxes(stbl_body).unwrap();
        let co64 = inner
            .iter()
            .find(|b| &b.kind == b"co64")
            .expect("切片应写 co64");
        let body = co64.body(stbl_body);
        let n = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
        assert!(n > 0, "应有至少一个 chunk");
        for i in 0..n {
            let off = u64::from_be_bytes(body[8 + i * 8..16 + i * 8].try_into().unwrap());
            assert!(
                off >= mdat.offset as u64 && off < mdat_end as u64,
                "chunk {i} 偏移 {off} 不在 mdat 区间内"
            );
        }
        assert_eq!(info.sample_count, 45);
    }

    fn make_recording_multi(
        dir: &std::path::Path,
        video_frames: u64,
        audio_packets: u64,
    ) -> std::path::PathBuf {
        let path = dir.join(format!("rec_multi_{video_frames}_{audio_packets}.mp4"));
        let (sps, pps) = make_sps_pps();
        {
            let mut muxer = crate::mux::mp4::Mp4Muxer::create_multi(
                &path,
                Some((320, 240, 30)),
                Some((2, 48000)),
            )
            .unwrap();

            for i in 0..video_frames {
                let is_key = i % 30 == 0;
                let mut data = Vec::new();
                if is_key {
                    data.extend_from_slice(&(sps.len() as u32).to_be_bytes());
                    data.extend_from_slice(&sps);
                    data.extend_from_slice(&(pps.len() as u32).to_be_bytes());
                    data.extend_from_slice(&pps);
                }
                let payload = [0x65u8; 100];
                data.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                data.extend_from_slice(&payload);
                muxer
                    .write_sample(&data, i * 1_000_000 / 30, is_key, &sps, &pps)
                    .unwrap();
            }

            for i in 0..audio_packets {
                let aac_frame = [0x21u8; 150];
                muxer.write_audio_sample(&aac_frame, i * 1024).unwrap();
            }
        }
        path
    }

    /// 入参 start_us 落在 GOP 中间时，音频起点跟随关键帧。
    #[test]
    fn slice_aligns_audio_to_video_keyframe() {
        let dir = tempfile::tempdir().unwrap();
        let src = make_recording_multi(dir.path(), 150, 250);
        let dst = dir.path().join("aligned_audio.mp4");

        // 请求 [1_600_000, 2_800_000)，落在第 2 个 GOP（1.0s..2.0s）的中间
        let info = extract_range(&src, &dst, 1_600_000, 2_800_000, 30).unwrap();

        // 视频关键帧在 1_000_000 µs (1.0s)
        assert_eq!(
            info.source_start_us, 1_000_000,
            "视频起点应对齐到 1.0s 的关键帧"
        );

        let bytes = std::fs::read(&dst).unwrap();
        let boxes = split_boxes(&bytes).unwrap();
        let moov = boxes.iter().find(|b| &b.kind == b"moov").unwrap();
        let moov_body = moov.body(&bytes);

        let traks: Vec<_> = split_boxes(moov_body)
            .unwrap()
            .into_iter()
            .filter(|b| &b.kind == b"trak")
            .collect();
        assert_eq!(traks.len(), 2, "切片产物应包含视频与音频两轨");
        assert!(info.sample_count > 54, "应包含视频帧 + 跟随对齐的音频包");
    }

    /// 切片产物两轨都在，且 co64 各指各的 mdat。
    #[test]
    fn slice_output_has_two_trak() {
        let dir = tempfile::tempdir().unwrap();
        let src = make_recording_multi(dir.path(), 150, 250);
        let dst = dir.path().join("two_track_slice.mp4");

        let info = extract_range(&src, &dst, 1_000_000, 3_000_000, 30).unwrap();

        let bytes = std::fs::read(&dst).unwrap();
        let boxes = split_boxes(&bytes).unwrap();
        let mdat = boxes.iter().find(|b| &b.kind == b"mdat").unwrap();
        let mdat_start = mdat.offset as u64;
        let mdat_end = mdat_start + mdat.size as u64;

        let moov = boxes.iter().find(|b| &b.kind == b"moov").unwrap();
        let moov_body = moov.body(&bytes);
        let traks: Vec<_> = split_boxes(moov_body)
            .unwrap()
            .into_iter()
            .filter(|b| &b.kind == b"trak")
            .collect();
        assert_eq!(traks.len(), 2, "两轨都在");

        let mut chunk_offsets = Vec::new();
        for trak in &traks {
            let trak_body = trak.body(moov_body);
            let stbl = descend_full(trak_body, &[*b"mdia", *b"minf", *b"stbl"]).unwrap();
            let stbl_body = box_body(stbl).unwrap();
            let co64 = split_boxes(stbl_body)
                .unwrap()
                .into_iter()
                .find(|b| &b.kind == b"co64")
                .unwrap();
            let body = co64.body(stbl_body);
            let count = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
            assert!(count > 0);
            let mut offsets = Vec::new();
            for i in 0..count {
                let off = u64::from_be_bytes(body[8 + i * 8..16 + i * 8].try_into().unwrap());
                assert!(off >= mdat_start && off < mdat_end, "co64 必须落在 mdat 内");
                offsets.push(off);
            }
            chunk_offsets.push(offsets);
        }

        let v_off = chunk_offsets[0][0];
        let a_off = chunk_offsets[1][0];
        assert_ne!(v_off, a_off, "co64 视频和音频偏移不能重叠");
        assert_eq!(v_off, mdat_start + 16, "视频数据紧随 mdat 头");
        assert!(a_off > v_off, "音频数据排在视频数据之后");
        assert_eq!(info.sample_count, (30 * 2) + 94); // 60 video frames + 94 audio frames
    }

    /// 取 `handler` 为 `vide` / `soun` 那条轨的 `stsz` 样本数。
    ///
    /// 不能按 trak 出现顺序取：多轨封装里音频先注册（ASC 是常量），
    /// 所以 `traks[0]` 是音频而不是视频。
    fn trak_sample_count(bytes: &[u8], handler: &[u8; 4]) -> usize {
        let boxes = split_boxes(bytes).unwrap();
        let moov = boxes.iter().find(|b| &b.kind == b"moov").unwrap();
        let moov_body = moov.body(bytes);
        let traks: Vec<_> = split_boxes(moov_body)
            .unwrap()
            .into_iter()
            .filter(|b| &b.kind == b"trak")
            .collect();
        let trak = traks
            .iter()
            .find(|t| {
                let body = t.body(moov_body);
                body.windows(4).any(|w| w == handler)
            })
            .unwrap_or_else(|| {
                panic!("找不到 handler={:?} 的轨", String::from_utf8_lossy(handler))
            });
        let trak_body = trak.body(moov_body);
        let stbl = descend_full(trak_body, &[*b"mdia", *b"minf", *b"stbl"]).unwrap();
        let stbl_body = box_body(stbl).unwrap();
        let stsz = split_boxes(stbl_body)
            .unwrap()
            .into_iter()
            .find(|b| &b.kind == b"stsz")
            .unwrap();
        let body = stsz.body(stbl_body);
        // stsz: version+flags(4) | sample_size(4) | sample_count(4)
        u32::from_be_bytes(body[8..12].try_into().unwrap()) as usize
    }

    /// 音频终点必须服从**视频真实结束点**，而不是入参 `end_us`。
    ///
    /// 场景：请求的 `end_us` 远超已落盘范围。视频停在最后一个**已落盘**分片
    /// （未闭合的 pending 样本还没写成 `moof`，读不到），音频的 flush 节奏不同，
    /// 往往比视频多落一段。若音频仍按 `end_us` 裁，片尾就会挂出
    /// 「只有声音、没有画面」的尾巴——肉眼在静止画面里几乎看不出来。
    #[test]
    fn audio_tail_is_clamped_to_video_end() {
        let dir = tempfile::tempdir().unwrap();
        // 150 帧 = 5.0 s 视频；250 包 = 250*1024/48000 ≈ 5.33 s 音频。
        // 注意：muxer 不 finish，所以只有**已闭合分片**里的样本在盘上——
        // 视频按关键帧闭片，盘上只剩 0..3.97 s（120 帧）；音频按 500 ms 闭片，
        // 盘上几乎到 5.3 s。两者天然不对齐，正是本用例要覆盖的情形。
        let src = make_recording_multi(dir.path(), 150, 250);
        let dst = dir.path().join("clamped_tail.mp4");

        // 请求 1.0 s 起、10 s 止——远超盘上内容。
        let info = extract_range(&src, &dst, 1_000_000, 10_000_000, 30).unwrap();
        let bytes = std::fs::read(&dst).unwrap();

        let video_n = trak_sample_count(&bytes, b"vide");
        let audio_n = trak_sample_count(&bytes, b"soun");
        // 视频：盘上 120 帧，从 1.0 s 的关键帧切 → 90 帧（1.0 s..4.0 s）。
        assert_eq!(video_n, 90, "视频应为 90 帧");

        // 音频：必须止步于视频结束点 4.0 s。
        // 对齐后的包号区间是 [round_up(1.0s / 21.33ms), floor(4.0s / 21.33ms)] = [47, 187]，
        // 共 141 包。若按 `end_us`=10 s 裁剪则会带上 203 包——多出的 62 包
        // 就是片尾那 1.32 s 裸音频。
        assert_eq!(
            audio_n, 141,
            "音频应裁到视频结束点（141 包）；按 end_us 裁会变成 203 包"
        );
        assert_eq!(
            info.sample_count,
            video_n + audio_n,
            "SliceInfo.sample_count 应等于两轨之和"
        );
    }
}
