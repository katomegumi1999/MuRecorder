//! 分片态 MP4 的只读扫描器（两条编码路径共用）。
//!
//! 软编路径写出的 fMP4 和 Media Foundation 写出的 fMP4 长得不一样，但都符合
//! ISO BMFF 的 fragmented MP4 规范。这里把「读懂它」的脏活集中到一个地方，
//! 供两个上层使用：
//!
//! - [`super::remux`]：停止录制时把分片态收敛成普通 MP4（重建全量 moov）；
//! - [`super::extract`]：录制中按时间区间切一段出来。
//!
//! ## 不能假设写盒的人是谁
//! 各家实现的差异实测如下（都是合法的）：
//!
//! | 项目 | 本仓库的 `Mp4Muxer` | Media Foundation |
//! |---|---|---|
//! | 样本基准 | `tfhd` flags=0x020000（`default-base-is-moof`） | `tfhd` flags=0x000001（显式 `base_data_offset`） |
//! | 分片起始时间 | `tfdt` v1（64 位，微秒） | **不写 `tfdt`**，靠前一片继续推进 |
//! | 时基 | `mdhd` timescale = 1_000_000（µs） | `mdhd` timescale = 30000（帧率时钟） |
//! | `trun` 字段 | 逐样本 duration/size/flags | 逐样本 + `ctts` 合成偏移（B 帧） |
//!
//! 所以解析器必须：`tfdt` 可选、支持 `tfhd` 默认值与 `trex` 回退、
//! 吞掉合成时间偏移，并且**不假定时基是微秒**。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// 单个分片允许的最大样本数（防御畸形 trun 造成的巨额分配）。
const MAX_SAMPLES_PER_FRAGMENT: u32 = 1_000_000;

/// 顶层盒（文件坐标）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileBox {
    pub(crate) offset: u64,
    pub(crate) kind: [u8; 4],
    pub(crate) size: u64,
    /// 盒头字节数：8（32 位长度）或 16（`size==1` + 64 位 largesize）
    pub(crate) header_len: usize,
}

/// 嵌套盒（内存坐标）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct MemBox {
    pub(crate) offset: usize,
    pub(crate) kind: [u8; 4],
    pub(crate) size: usize,
    pub(crate) header_len: usize,
}

impl MemBox {
    pub(crate) fn full<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        &buf[self.offset..self.offset + self.size]
    }

    pub(crate) fn body<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        &buf[self.offset + self.header_len..self.offset + self.size]
    }
}

/// 从 init moov 里解出的、重建末尾 moov 需要的布局信息。
#[derive(Debug, Clone)]
pub(crate) struct TrackLayout {
    pub(crate) track_id: u32,
    /// mvhd 时基（tkhd / mvhd 的时长用它）
    pub(crate) movie_timescale: u32,
    /// mdhd 时基（stts / trun 的时长用它）
    pub(crate) media_timescale: u32,
    /// trex 的样本默认值
    pub(crate) defaults: SampleDefaults,
    /// stsd 盒完整字节（含盒头）
    pub(crate) stsd: Vec<u8>,
    /// handler_type（b"vide" / b"soun"）
    pub(crate) handler_type: [u8; 4],
}

/// trex / tfhd 的「默认样本值」。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SampleDefaults {
    duration: u32,
    size: u32,
    flags: u32,
}

/// 一个最终要写进 stbl 的样本。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SampleInfo {
    /// 绝对文件偏移（未被搬移，可直接作为 co64 的值）
    pub(crate) offset: u64,
    /// 解码时间（mdhd 时基）；用于在 `duration` 缺失时兜底推算时长，以及切片的区间定位
    pub(crate) dts: u64,
    /// 解码时长（mdhd 时基）
    pub(crate) duration: u32,
    /// 合成时间偏移（B 帧用；`trun` version 1 的 signed 值）
    pub(crate) cts: i64,
    pub(crate) is_key: bool,
    pub(crate) size: u32,
}

/// 读指定范围到一个新 `Vec`。
pub(crate) fn read_at(f: &mut File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len as usize];
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// 扫顶层盒；末尾未写完的半截盒视为文件截断，忽略。
pub(crate) fn scan_top_level(f: &mut File, len: u64) -> io::Result<Vec<FileBox>> {
    let mut out = Vec::new();
    let mut pos = 0u64;
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos))?;
        let mut buf = [0u8; 16];
        f.read_exact(&mut buf[..8])?;
        let size32 = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        let kind: [u8; 4] = buf[4..8].try_into().unwrap();
        let (size, header_len) = match size32 {
            0 => (len - pos, 8),
            1 => {
                if pos + 16 > len {
                    break;
                }
                f.read_exact(&mut buf[8..16])?;
                (u64::from_be_bytes(buf[8..16].try_into().unwrap()), 16u64)
            }
            n => (n as u64, 8u64),
        };
        if size < header_len || pos + size > len {
            break;
        }
        out.push(FileBox {
            offset: pos,
            kind,
            size,
            header_len: header_len as usize,
        });
        pos += size;
    }
    Ok(out)
}

/// 把一段**payload**（不含盒头）解析成并列的子盒列表。
///
/// 入参必须是 payload：`size==0` 的盒表示「延伸到父级末尾」，含义依赖这一点。
pub(crate) fn split_boxes(buf: &[u8]) -> io::Result<Vec<MemBox>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        let size32 = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap());
        let kind: [u8; 4] = buf[pos + 4..pos + 8].try_into().unwrap();
        let (size, header_len) = match size32 {
            0 => (buf.len() - pos, 8usize),
            1 => {
                if pos + 16 > buf.len() {
                    return Err(io::Error::other(format!("盒 {} 头被截断", kind_str(&kind))));
                }
                (
                    u64::from_be_bytes(buf[pos + 8..pos + 16].try_into().unwrap()) as usize,
                    16usize,
                )
            }
            n => (n as usize, 8usize),
        };
        if size < header_len || pos + size > buf.len() {
            return Err(io::Error::other(format!(
                "盒 {} 声明长度 {size} 越界（剩余 {}）",
                kind_str(&kind),
                buf.len() - pos
            )));
        }
        out.push(MemBox {
            offset: pos,
            kind,
            size,
            header_len,
        });
        pos += size;
    }
    Ok(out)
}

pub(crate) fn kind_str(kind: &[u8; 4]) -> String {
    String::from_utf8_lossy(kind).to_string()
}

/// 沿 `path` 逐层下钻，返回**最后一级盒的完整字节**（含盒头）。
///
/// 入参与每级推进的对象都是「payload」（不含盒头），因为 `split_boxes` 把参数
/// 当作一组并列兄弟盒来解析。
pub(crate) fn descend_full<'a>(mut payload: &'a [u8], path: &[[u8; 4]]) -> io::Result<&'a [u8]> {
    let mut full: Option<&'a [u8]> = None;
    for kind in path {
        let boxes = split_boxes(payload)?;
        let found = boxes
            .iter()
            .find(|b| &b.kind == kind)
            .ok_or_else(|| io::Error::other(format!("缺少 {} 盒", kind_str(kind))))?;
        let (start, header_len) = (found.offset, found.header_len);
        let end = found.offset + found.size;
        full = Some(&payload[start..end]);
        payload = &payload[start + header_len..end];
    }
    full.ok_or_else(|| io::Error::other("下钻路径为空"))
}

/// 取出某个盒自身的 payload（入参是含盒头的完整盒）。
pub(crate) fn box_body(buf: &[u8]) -> io::Result<&[u8]> {
    let b = split_boxes(buf)?
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::other("盒为空"))?;
    Ok(&buf[b.header_len..b.size])
}

// ---------------------------------------------------------------- moov 解析

/// 从 init moov 的 payload 里解出所有轨道的时基、`trex` 默认值与 `stsd`。
pub(crate) fn parse_layouts(
    moov: &[u8],
) -> io::Result<std::collections::HashMap<u32, TrackLayout>> {
    let boxes = split_boxes(moov)?;

    let mvhd = boxes
        .iter()
        .find(|b| &b.kind == b"mvhd")
        .ok_or_else(|| io::Error::other("moov 缺少 mvhd"))?;
    let movie_timescale = read_timescale(mvhd.body(moov))?;

    let mut mvex_defaults: std::collections::HashMap<u32, SampleDefaults> =
        std::collections::HashMap::new();
    if let Some(mvex) = boxes.iter().find(|b| &b.kind == b"mvex") {
        for trex in split_boxes(mvex.body(moov))?
            .iter()
            .filter(|b| &b.kind == b"trex")
        {
            let body = trex.body(mvex.body(moov));
            if body.len() >= 20 {
                let track_id = u32::from_be_bytes(body[4..8].try_into().unwrap());
                let duration = u32::from_be_bytes(body[8..12].try_into().unwrap());
                let size = u32::from_be_bytes(body[12..16].try_into().unwrap());
                let flags = if body.len() >= 24 {
                    u32::from_be_bytes(body[16..20].try_into().unwrap())
                } else {
                    0
                };
                mvex_defaults.insert(
                    track_id,
                    SampleDefaults {
                        duration,
                        size,
                        flags,
                    },
                );
            }
        }
    }

    let mut layouts = std::collections::HashMap::new();
    for trak in boxes.iter().filter(|b| &b.kind == b"trak") {
        let trak_body = trak.body(moov);
        let trak_boxes = split_boxes(trak_body)?;

        let tkhd = trak_boxes
            .iter()
            .find(|b| &b.kind == b"tkhd")
            .ok_or_else(|| io::Error::other("trak 缺少 tkhd"))?;
        let tkhd_body = tkhd.body(trak_body);
        let version = *tkhd_body
            .first()
            .ok_or_else(|| io::Error::other("tkhd 为空"))?;
        let track_id = if version == 1 {
            if tkhd_body.len() < 24 {
                return Err(io::Error::other("tkhd v1 长度不足"));
            }
            u32::from_be_bytes(tkhd_body[20..24].try_into().unwrap())
        } else {
            if tkhd_body.len() < 16 {
                return Err(io::Error::other("tkhd v0 长度不足"));
            }
            u32::from_be_bytes(tkhd_body[12..16].try_into().unwrap())
        };

        let mdia = trak_boxes
            .iter()
            .find(|b| &b.kind == b"mdia")
            .ok_or_else(|| io::Error::other("trak 缺少 mdia"))?;
        let mdia_body = mdia.body(trak_body);
        let mdia_boxes = split_boxes(mdia_body)?;

        let mdhd = mdia_boxes
            .iter()
            .find(|b| &b.kind == b"mdhd")
            .ok_or_else(|| io::Error::other("mdia 缺少 mdhd"))?;
        let media_timescale = read_timescale(mdhd.body(mdia_body))?;

        let mut handler_type = [0u8; 4];
        if let Some(hdlr) = mdia_boxes.iter().find(|b| &b.kind == b"hdlr") {
            let hdlr_body = hdlr.body(mdia_body);
            if hdlr_body.len() >= 12 {
                handler_type.copy_from_slice(&hdlr_body[8..12]);
            }
        }

        let stsd_full = descend_full(trak_body, &[*b"mdia", *b"minf", *b"stbl", *b"stsd"])?;
        let defaults = mvex_defaults.get(&track_id).copied().unwrap_or_default();

        layouts.insert(
            track_id,
            TrackLayout {
                track_id,
                movie_timescale,
                media_timescale,
                defaults,
                stsd: stsd_full.to_vec(),
                handler_type,
            },
        );
    }

    if layouts.is_empty() {
        return Err(io::Error::other("moov 中未找到任何 trak"));
    }

    Ok(layouts)
}

/// 从 init moov 的 payload 里解出单轨时基与 `trex` 默认值（向后兼容）。
#[allow(dead_code)]
pub(crate) fn parse_layout(moov: &[u8]) -> io::Result<TrackLayout> {
    let map = parse_layouts(moov)?;
    if map.len() == 1 {
        Ok(map.into_values().next().unwrap())
    } else {
        Err(io::Error::other(format!(
            "只支持单轨文件，实际有 {} 条 trak",
            map.len()
        )))
    }
}

/// 读 mvhd / mdhd 的 timescale（两者布局一致）。
fn read_timescale(body: &[u8]) -> io::Result<u32> {
    let off = if body.first() == Some(&1) { 20 } else { 12 };
    body.get(off..off + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .filter(|&ts| ts > 0)
        .ok_or_else(|| io::Error::other("timescale 缺失或为 0"))
}

/// 取出 init moov 里的 `stsd` 盒原始字节（含盒头）。
///
/// 刻意逐字节拷贝而不是重建：`avc1` / `hvc1` / `av01` 的配置盒（`avcC` / `hvcC` /
/// `av1C`）由 MF 填写，任何重写都是在冒险——解码器只认它自己写的那一份。
#[allow(dead_code)]
pub(crate) fn find_stsd(moov: &[u8]) -> io::Result<Vec<u8>> {
    let stsd = descend_full(moov, &[*b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd"])?;
    Ok(stsd.to_vec())
}

// --------------------------------------------------------------- 分片解析

/// 解析一个 `moof`，按每个 `traf` 的 `tfhd.track_ID` 分派到对应的样本列表。
pub(crate) fn parse_fragment_multi(
    moof: &[u8],
    moof_abs: u64,
    layouts: &std::collections::HashMap<u32, TrackLayout>,
    track_samples: &mut std::collections::HashMap<u32, Vec<SampleInfo>>,
) -> io::Result<()> {
    let moof_body = box_body(moof)?;
    for child in split_boxes(moof_body)? {
        if &child.kind != b"traf" {
            continue;
        }
        let traf = child.body(moof_body);
        let traf_boxes = split_boxes(traf)?;

        let tfhd = traf_boxes
            .iter()
            .find(|b| &b.kind == b"tfhd")
            .ok_or_else(|| io::Error::other("traf 缺少 tfhd"))?;
        let (id, base_data_offset, tfhd_defaults) = parse_tfhd(tfhd.body(traf))?;

        let layout = layouts.get(&id).ok_or_else(|| {
            io::Error::other(format!("traf 中的 track_ID {id} 在 init moov 中未定义"))
        })?;

        let out = track_samples.entry(id).or_default();

        let mut dts = match traf_boxes.iter().find(|b| &b.kind == b"tfdt") {
            Some(tfdt) => read_tfdt(tfdt.body(traf))?,
            None => out.last().map(|s| s.dts + s.duration as u64).unwrap_or(0),
        };

        let base = base_data_offset.unwrap_or(moof_abs);
        let defaults = SampleDefaults {
            duration: pick(tfhd_defaults.duration, layout.defaults.duration),
            size: pick(tfhd_defaults.size, layout.defaults.size),
            flags: pick(tfhd_defaults.flags, layout.defaults.flags),
        };
        let mut cursor = base;

        for trun in traf_boxes.iter().filter(|b| &b.kind == b"trun") {
            let (data_offset, runs) = parse_trun(trun.body(traf), &defaults)?;
            if let Some(off) = data_offset {
                cursor = (base as i64 + off as i64).max(0) as u64;
            }
            for run in runs {
                out.push(SampleInfo {
                    offset: cursor,
                    dts,
                    duration: run.duration,
                    cts: run.cts,
                    is_key: run.is_key,
                    size: run.size,
                });
                cursor += run.size as u64;
                dts += run.duration as u64;
            }
        }
    }
    Ok(())
}

/// 解析单轨 `moof`（向后兼容）。
#[allow(dead_code)]
pub(crate) fn parse_fragment(
    moof: &[u8],
    moof_abs: u64,
    layout: &TrackLayout,
    out: &mut Vec<SampleInfo>,
) -> io::Result<()> {
    let mut map = std::collections::HashMap::new();
    map.insert(layout.track_id, layout.clone());
    let mut out_map = std::collections::HashMap::new();
    let prev = std::mem::take(out);
    out_map.insert(layout.track_id, prev);
    parse_fragment_multi(moof, moof_abs, &map, &mut out_map)?;
    if let Some(samples) = out_map.remove(&layout.track_id) {
        *out = samples;
    }
    Ok(())
}

/// `tfhd` 里给出的默认值优先，未给出（= 0）时退回 `trex`。
fn pick(tfhd: u32, trex: u32) -> u32 {
    if tfhd != 0 { tfhd } else { trex }
}

/// `trun` / `tfhd` / `trex` 全都没给时长时，用相邻样本的 dts 差兜底。
///
/// 时长缺失会让 `stts` 全是 0，播放器算出总长为 0、进度条直接失效；
/// 而 `tfdt` / dts 是可靠的，拿它反推比留 0 好得多。
///
/// `nominal` 是最后的兜底值（媒体时基下的标称帧间隔，0 表示不启用），
/// 供「只有一个样本、连 dts 差都算不出来」这种极端情况使用。
pub(crate) fn fill_missing_durations(samples: &mut [SampleInfo], nominal: u32) {
    if !samples.iter().any(|s| s.duration == 0) {
        return;
    }
    for i in 0..samples.len() {
        if samples[i].duration != 0 {
            continue;
        }
        let next = match samples.get(i + 1) {
            Some(n) => n.dts.saturating_sub(samples[i].dts),
            // 末样本没有后继：退回前一个样本的时长。
            None => samples
                .get(i.wrapping_sub(1))
                .map(|p| p.duration as u64)
                .unwrap_or(nominal as u64),
        };
        let next = if next == 0 { nominal as u64 } else { next };
        samples[i].duration = next.clamp(1, u32::MAX as u64) as u32;
    }
}

fn parse_tfhd(body: &[u8]) -> io::Result<(u32, Option<u64>, SampleDefaults)> {
    if body.len() < 8 {
        return Err(io::Error::other("tfhd 长度不足"));
    }
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let track_id = u32::from_be_bytes(body[4..8].try_into().unwrap());
    let mut p = 8usize;

    let base_data_offset = if flags & 0x0000_0001 != 0 {
        let v = body
            .get(p..p + 8)
            .ok_or_else(|| io::Error::other("tfhd 的 base_data_offset 越界"))?;
        p += 8;
        Some(u64::from_be_bytes(v.try_into().unwrap()))
    } else {
        None
    };
    if flags & 0x0000_0002 != 0 {
        p += 4; // sample_description_index
    }

    let take4 = |p: &mut usize| -> io::Result<u32> {
        let v = body
            .get(*p..*p + 4)
            .ok_or_else(|| io::Error::other("tfhd 默认值越界"))?;
        *p += 4;
        Ok(u32::from_be_bytes(v.try_into().unwrap()))
    };

    // 用 Option 语义区分「tfhd 没给」与「给了 0」：没给时由 trex 兜底。
    let duration = if flags & 0x0000_0008 != 0 {
        Some(take4(&mut p)?)
    } else {
        None
    };
    let size = if flags & 0x0000_0010 != 0 {
        Some(take4(&mut p)?)
    } else {
        None
    };
    let sample_flags = if flags & 0x0000_0020 != 0 {
        Some(take4(&mut p)?)
    } else {
        None
    };

    Ok((
        track_id,
        base_data_offset,
        SampleDefaults {
            duration: duration.unwrap_or(0),
            size: size.unwrap_or(0),
            flags: sample_flags.unwrap_or(0),
        },
    ))
}

fn read_tfdt(body: &[u8]) -> io::Result<u64> {
    if body.len() < 4 {
        return Err(io::Error::other("tfdt 长度不足"));
    }
    if body[0] == 1 {
        body.get(4..12)
            .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
            .ok_or_else(|| io::Error::other("tfdt v1 的 baseMediaDecodeTime 越界"))
    } else {
        body.get(4..8)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as u64)
            .ok_or_else(|| io::Error::other("tfdt v0 的 baseMediaDecodeTime 越界"))
    }
}

struct RunInfo {
    duration: u32,
    size: u32,
    cts: i64,
    is_key: bool,
}

/// 解析 trun，返回 `(data_offset, 每样本描述)`。
///
/// 刻意支持 `tfhd` 默认值驱动的紧凑布局与 `ctts` 偏移（B 帧）：
/// 这些盒是 **MF 写的**，不能假设它跟我们自己的封装器一样逐样本全字段输出。
fn parse_trun(body: &[u8], defaults: &SampleDefaults) -> io::Result<(Option<i32>, Vec<RunInfo>)> {
    if body.len() < 8 {
        return Err(io::Error::other("trun 长度不足"));
    }
    let version = body[0];
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let count = u32::from_be_bytes(body[4..8].try_into().unwrap());
    if count > MAX_SAMPLES_PER_FRAGMENT {
        return Err(io::Error::other(format!("trun 样本数 {count} 超出上限")));
    }

    let has_offset = flags & 0x0000_0001 != 0;
    let has_first_flags = flags & 0x0000_0004 != 0;
    let has_duration = flags & 0x0000_0100 != 0;
    let has_size = flags & 0x0000_0200 != 0;
    let has_flags = flags & 0x0000_0400 != 0;
    let has_cts = flags & 0x0000_0800 != 0;

    let mut p = 8usize;
    let take4 = |p: &mut usize| -> io::Result<u32> {
        let v = body
            .get(*p..*p + 4)
            .ok_or_else(|| io::Error::other("trun 字段越界"))?;
        *p += 4;
        Ok(u32::from_be_bytes(v.try_into().unwrap()))
    };

    let data_offset = if has_offset {
        Some(take4(&mut p)? as i32)
    } else {
        None
    };
    let first_flags = if has_first_flags {
        Some(take4(&mut p)?)
    } else {
        None
    };

    let mut runs = Vec::with_capacity(count as usize);
    for i in 0..count {
        let duration = if has_duration {
            take4(&mut p)?
        } else {
            defaults.duration
        };
        let size = if has_size {
            take4(&mut p)?
        } else {
            defaults.size
        };
        let sample_flags = if has_flags {
            take4(&mut p)?
        } else if i == 0 && first_flags.is_some() {
            first_flags.unwrap_or(defaults.flags)
        } else {
            defaults.flags
        };
        let cts = if has_cts {
            let raw = take4(&mut p)?;
            if version == 0 {
                raw as i64
            } else {
                raw as i32 as i64
            }
        } else {
            0
        };
        runs.push(RunInfo {
            duration,
            size,
            cts,
            // bit16 = sample_is_non_sync_sample
            is_key: sample_flags & 0x0001_0000 == 0,
        });
    }
    Ok((data_offset, runs))
}

#[cfg(test)]
mod tests {
    //! 测试策略：所有输入都是**手工构造的合成字节流**，不依赖任何真实录制文件。
    //! 两组风格都覆盖：本仓库 `Mp4Muxer` 的写法（`tfhd` 默认值 + `tfdt` v1）
    //! 与 Media Foundation 的写法（显式 `base_data_offset`、不写 `tfdt`、
    //! 时长/尺寸走 `trex` 回退、v1 `trun` 带符号 `cts`）。

    use super::*;

    // -------------------------------------------------------------- 构盒工具

    /// 32 位长度的普通盒。
    fn box_full(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + payload.len());
        v.extend_from_slice(&((payload.len() as u32 + 8).to_be_bytes()));
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    /// `size==1` + 64 位 largesize 的盒。
    fn box_large(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(kind);
        v.extend_from_slice(&((payload.len() as u64 + 16).to_be_bytes()));
        v.extend_from_slice(payload);
        v
    }

    fn be32(v: u32) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }

    /// 构造一个最小但合法的单轨 init moov payload：
    /// mvhd(1000) + mvex(trex) + trak(tkhd v0 + mdia(mdhd + hdlr + minf(stbl(stsd))))。
    fn sample_moov(track_id: u32, trex: Option<(u32, u32, u32)>) -> Vec<u8> {
        // mvhd v0：body 前 12 字节后是 timescale
        let mut mvhd_payload = vec![0u8; 12];
        mvhd_payload.extend_from_slice(&1000u32.to_be_bytes());
        mvhd_payload.extend_from_slice(&0u32.to_be_bytes()); // duration

        let tkhd = {
            let mut body = vec![0u8; 16]; // version/flags + (creation, modification) + ...
            body[12..16].copy_from_slice(&track_id.to_be_bytes());
            box_full(b"tkhd", &body)
        };
        let mdhd = {
            let mut body = vec![0u8; 12];
            body.extend_from_slice(&90000u32.to_be_bytes()); // media timescale
            body.extend_from_slice(&0u32.to_be_bytes());
            box_full(b"mdhd", &body)
        };
        let hdlr_body = {
            let mut b = vec![0u8; 8];
            b.extend_from_slice(b"vide");
            b
        };
        let hdlr_box = box_full(b"hdlr", &hdlr_body);
        let stsd = box_full(b"stsd", &[0xAA; 16]);
        let stbl = box_full(b"stbl", &stsd);
        let minf = box_full(b"minf", &stbl);
        let mdia = box_full(b"mdia", &[mdhd, hdlr_box.clone(), minf].concat());
        let trak = box_full(b"trak", &[tkhd, mdia].concat());

        let mvex = trex.map(|(duration, size, flags)| {
            let mut body = vec![0u8; 4]; // version/flags
            body.extend_from_slice(&track_id.to_be_bytes());
            body.extend_from_slice(&duration.to_be_bytes());
            body.extend_from_slice(&size.to_be_bytes());
            body.extend_from_slice(&flags.to_be_bytes());
            body.extend_from_slice(&0u32.to_be_bytes()); // 补齐到 24 B，flags 才会被解析
            box_full(b"mvex", &box_full(b"trex", &body))
        });

        let mut children = vec![box_full(b"mvhd", &mvhd_payload), hdlr_box, trak];
        if let Some(m) = mvex {
            children.push(m);
        }
        children.concat()
    }

    /// 单轨 `TrackLayout`（单轨场景的便捷构造）。
    fn single_layout(track_id: u32, defaults: (u32, u32, u32)) -> TrackLayout {
        TrackLayout {
            track_id,
            movie_timescale: 1000,
            media_timescale: 90000,
            defaults: SampleDefaults {
                duration: defaults.0,
                size: defaults.1,
                flags: defaults.2,
            },
            stsd: vec![0xAB; 8],
            handler_type: *b"vide",
        }
    }

    /// 本仓库风格的 moof：`tfhd` flags=0x020000（default-base-is-moof），
    /// `tfdt` v1，`trun` 逐样本 duration/size/flags。
    fn muxer_style_moof(dts: u64, samples: &[(u32, u32, u32)], data_offset: i32) -> Vec<u8> {
        // tfhd：flags 0x020000 + track_ID，无默认值
        let mut tfhd = vec![0x00, 0x02, 0x00, 0x00];
        tfhd.extend_from_slice(&1u32.to_be_bytes());

        // tfdt v1：64 位 baseMediaDecodeTime
        let mut tfdt = vec![1u8, 0, 0, 0];
        tfdt.extend_from_slice(&dts.to_be_bytes());

        // trun：data-offset + per-sample duration/size/flags
        let flags: u32 = 0x0000_0001 | 0x0000_0100 | 0x0000_0200 | 0x0000_0400;
        let mut trun = Vec::new();
        trun.extend_from_slice(&[0u8, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8]);
        trun.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        trun.extend_from_slice(&data_offset.to_be_bytes());
        for &(duration, size, sample_flags) in samples {
            trun.extend_from_slice(&duration.to_be_bytes());
            trun.extend_from_slice(&size.to_be_bytes());
            trun.extend_from_slice(&sample_flags.to_be_bytes());
        }

        let traf = box_full(
            b"traf",
            &[
                box_full(b"tfhd", &tfhd),
                box_full(b"tfdt", &tfdt),
                box_full(b"trun", &trun),
            ]
            .concat(),
        );
        let mfhd = box_full(b"mfhd", &be32(1));
        box_full(b"moof", &[mfhd, traf].concat())
    }

    // ------------------------------------------------------------ split_boxes

    #[test]
    fn split_boxes_parses_sibling_boxes() {
        let a = box_full(b"free", &[1, 2, 3]);
        let b = box_full(b"moof", &[4]);
        let buf = [a.as_slice(), b.as_slice()].concat();
        let boxes = split_boxes(&buf).unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!(&boxes[0].kind, b"free");
        assert_eq!(boxes[0].header_len, 8);
        assert_eq!(&boxes[1].kind, b"moof");
    }

    /// `size==0` 的盒表示「延伸到 payload 末尾」（ISO 14496-12 §4.2）。
    #[test]
    fn split_boxes_zero_size_extends_to_end() {
        let mut a = box_full(b"free", &[1]);
        let b = box_full(b"mdat", &[9; 4]);
        a[0..4].copy_from_slice(&0u32.to_be_bytes());
        let buf = [a.as_slice(), b.as_slice()].concat();
        let boxes = split_boxes(&buf).unwrap();
        assert_eq!(boxes.len(), 1, "size==0 的 free 吞掉了后面的字节");
        assert_eq!(&boxes[0].kind, b"free");
        assert_eq!(boxes[0].size, buf.len());
    }

    /// `size==1` 走 64 位 largesize，盒头为 16 字节。
    #[test]
    fn split_boxes_largesize_header() {
        let big = box_large(b"mdat", &[7; 100]);
        let boxes = split_boxes(&big).unwrap();
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].header_len, 16);
        assert_eq!(boxes[0].size, 116);
    }

    #[test]
    fn split_boxes_rejects_truncated_and_overrun() {
        // 头被截断：声明 largesize 但 16 字节头不完整
        let mut truncated = box_large(b"mdat", &[7; 8]);
        truncated.truncate(12);
        assert!(split_boxes(&truncated).is_err());

        // 声明长度越界
        let mut bad = box_full(b"mdat", &[7; 4]);
        bad[0..4].copy_from_slice(&999u32.to_be_bytes());
        assert!(split_boxes(&bad).is_err());
    }

    // ---------------------------------------------------------- descend / body

    #[test]
    fn descend_full_drills_path_and_box_body_strips_header() {
        let stsd = box_full(b"stsd", &[0xCC; 8]);
        let stbl = box_full(b"stbl", &stsd);
        let minf = box_full(b"minf", &stbl);
        let mdia = box_full(b"mdia", &minf);

        // descend_full 的入参是 payload（不含外层盒头），与生产代码用法一致
        let payload = box_body(&mdia).unwrap();
        let full = descend_full(payload, &[*b"minf", *b"stbl", *b"stsd"]).unwrap();
        assert_eq!(full.len(), 8 + 8);
        assert_eq!(&full[4..8], b"stsd");

        let body = box_body(full).unwrap();
        assert_eq!(body, &[0xCC; 8]);

        // 缺失的盒必须报错，而不是静默返回空
        assert!(descend_full(payload, &[*b"minf", *b"stsd"]).is_err());
    }

    // ------------------------------------------------------------ scan_top_level

    #[test]
    fn scan_top_level_reads_boxes_and_ignores_truncated_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan.mp4");
        let a = box_full(b"ftyp", &[0; 8]);
        let b = box_large(b"mdat", &[1; 32]);
        // 末尾故意留一个半截盒（录制中被截断的常态）
        let mut bytes = [a.as_slice(), b.as_slice()].concat();
        bytes.extend_from_slice(&[0, 0, 1, 0]); // 声明了 256 字节但只有 4 字节
        std::fs::write(&path, &bytes).unwrap();

        let mut f = File::open(&path).unwrap();
        let len = f.metadata().unwrap().len();
        let boxes = scan_top_level(&mut f, len).unwrap();
        assert_eq!(boxes.len(), 2, "半截盒必须被忽略");
        assert_eq!(&boxes[0].kind, b"ftyp");
        assert_eq!(boxes[0].header_len, 8);
        assert_eq!(&boxes[1].kind, b"mdat");
        assert_eq!(boxes[1].header_len, 16, "largesize 盒头应为 16 字节");

        let payload = read_at(&mut f, boxes[1].offset + 16, boxes[1].size - 16).unwrap();
        assert_eq!(payload, vec![1; 32]);
    }

    // -------------------------------------------------------------- parse_layouts

    #[test]
    fn parse_layouts_reads_timescales_stsd_and_trex_defaults() {
        let moov = sample_moov(1, Some((33, 120, 0x0200_0000)));
        let layouts = parse_layouts(&moov).unwrap();
        let layout = layouts.get(&1).expect("track 1 必须存在");

        assert_eq!(layout.movie_timescale, 1000);
        assert_eq!(layout.media_timescale, 90000);
        assert_eq!(&layout.handler_type, b"vide");
        assert_eq!(layout.defaults.duration, 33);
        assert_eq!(layout.defaults.size, 120);
        assert_eq!(layout.defaults.flags, 0x0200_0000);
        // stsd 必须逐字节保留（含盒头）——上层靠它原样拷贝 avcC/hvcC/av1C
        assert_eq!(&layout.stsd[4..8], b"stsd");
        assert_eq!(
            layout.stsd.len(),
            8 + 16,
            "stsd 完整盒 = 8 字节盒头 + 16 字节 payload"
        );
    }

    #[test]
    fn parse_layouts_rejects_moov_without_trak() {
        let moov = box_full(b"mvhd", &[0u8; 20]);
        assert!(parse_layouts(&moov).is_err());
    }

    // -------------------------------------------------------- parse_fragment_multi

    /// 本仓库 muxer 风格：default-base-is-moof + tfdt v1 + 逐样本全字段 trun。
    #[test]
    fn parse_fragment_multi_handles_muxer_style_fragment() {
        let layouts = std::collections::HashMap::from([(1u32, single_layout(1, (0, 0, 0)))]);
        // 两个样本：关键帧(10 B) + 非同步帧(12 B)；基址取 moof 绝对偏移 4096
        let moof = muxer_style_moof(5_000, &[(33, 10, 0x0200_0000), (33, 12, 0x0101_0000)], 100);
        let moof_abs = 4096u64;

        let mut track_samples = std::collections::HashMap::new();
        parse_fragment_multi(&moof, moof_abs, &layouts, &mut track_samples).unwrap();

        let samples = &track_samples[&1];
        assert_eq!(samples.len(), 2);
        // base = moof_abs（default-base-is-moof），cursor = base + data_offset
        assert_eq!(samples[0].offset, 4096 + 100);
        assert_eq!(samples[1].offset, 4096 + 100 + 10);
        assert_eq!(samples[0].dts, 5_000);
        assert_eq!(samples[1].dts, 5_033);
        assert_eq!(samples[0].duration, 33);
        assert_eq!(samples[0].size, 10);
        assert_eq!(samples[1].size, 12);
        assert!(samples[0].is_key, "bit16 未置位应为关键帧");
        assert!(!samples[1].is_key, "bit16 置位应为非同步样本");
        assert_eq!(samples[0].cts, 0);
    }

    /// MF 风格：显式 `base_data_offset`、不写 `tfdt`（dts 从上一片延续）、
    /// 时长/尺寸走 `trex` 默认、v1 `trun` 带符号 `cts`（B 帧）。
    #[test]
    fn parse_fragment_multi_handles_media_foundation_style_fragment() {
        let layouts = std::collections::HashMap::from([(1u32, single_layout(1, (40, 5, 0)))]);
        // tfhd：flags 0x000001（显式 base_data_offset = 2048）
        let mut tfhd = vec![0x00, 0x00, 0x00, 0x01];
        tfhd.extend_from_slice(&1u32.to_be_bytes());
        tfhd.extend_from_slice(&2048u64.to_be_bytes());

        // trun v1：data-offset + cts（合成偏移 -2），duration/size 全走默认
        // 注意 flags 是 24 位大端：0x000801 → 字节 [0x00, 0x08, 0x01]
        let mut trun = vec![1u8, 0x00, 0x08, 0x01];
        trun.extend_from_slice(&1u32.to_be_bytes());
        trun.extend_from_slice(&16i32.to_be_bytes());
        trun.extend_from_slice(&(-2i32).to_be_bytes());

        // 不写 tfdt：dts 应从 track 已有样本延续
        let traf = box_full(
            b"traf",
            &[box_full(b"tfhd", &tfhd), box_full(b"trun", &trun)].concat(),
        );
        let moof = box_full(
            b"moof",
            &box_full(b"mfhd", &be32(1))
                .into_iter()
                .chain(traf)
                .collect::<Vec<u8>>(),
        );

        let mut track_samples = std::collections::HashMap::from([(
            1u32,
            vec![SampleInfo {
                offset: 0,
                dts: 1_000,
                duration: 33,
                cts: 0,
                is_key: true,
                size: 5,
            }],
        )]);
        parse_fragment_multi(&moof, 0, &layouts, &mut track_samples).unwrap();

        let samples = &track_samples[&1];
        assert_eq!(samples.len(), 2);
        let s = &samples[1];
        assert_eq!(s.offset, 2048 + 16, "base_data_offset + trun data_offset");
        assert_eq!(s.dts, 1_033, "无 tfdt 时 dts 应延续上一片");
        assert_eq!(s.duration, 40, "trun 未给时长应走 trex 默认");
        assert_eq!(s.size, 5, "trun 未给尺寸应走 trex 默认");
        assert_eq!(s.cts, -2, "v1 trun 的 cts 应按有符号解析");
        assert!(s.is_key, "默认 flags=0 应视为关键帧");
    }

    /// tfhd 自带默认值时必须压过 trex（ISO 语义：作用域越近优先级越高）。
    #[test]
    fn parse_fragment_multi_tfhd_defaults_override_trex() {
        let layouts = std::collections::HashMap::from([(1u32, single_layout(1, (999, 999, 0)))]);
        // tfhd：flags 0x000018（default_sample_duration + default_sample_size）
        let mut tfhd = vec![0x00, 0x00, 0x00, 0x18];
        tfhd.extend_from_slice(&1u32.to_be_bytes());
        tfhd.extend_from_slice(&25u32.to_be_bytes()); // duration
        tfhd.extend_from_slice(&7u32.to_be_bytes()); // size

        // trun：只给 count（0x000000），字段全走默认
        let trun = {
            let mut b = vec![0u8, 0x00, 0x00, 0x00];
            b.extend_from_slice(&2u32.to_be_bytes());
            b
        };
        let traf = box_full(
            b"traf",
            &[box_full(b"tfhd", &tfhd), box_full(b"trun", &trun)].concat(),
        );
        let moof = box_full(
            b"moof",
            &box_full(b"mfhd", &be32(1))
                .into_iter()
                .chain(traf)
                .collect::<Vec<u8>>(),
        );

        let mut track_samples = std::collections::HashMap::new();
        parse_fragment_multi(&moof, 100, &layouts, &mut track_samples).unwrap();

        let samples = &track_samples[&1];
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].duration, 25, "tfhd 默认应压过 trex");
        assert_eq!(samples[0].size, 7);
        // 无 data_offset 的 trun：cursor 从 base 连续推进
        assert_eq!(samples[0].offset, 100);
        assert_eq!(samples[1].offset, 107);
        assert_eq!(samples[0].dts, 0, "无 tfdt 且无前序样本时 dts 从 0 起");
        assert_eq!(samples[1].dts, 25, "dts 按默认时长逐样本推进");
    }

    /// traf 引用 init moov 未定义的 track_ID 必须报错，不能静默丢样本。
    #[test]
    fn parse_fragment_multi_rejects_unknown_track_id() {
        let layouts = std::collections::HashMap::from([(1u32, single_layout(1, (33, 10, 0)))]);
        let mut tfhd = vec![0x00, 0x02, 0x00, 0x00];
        tfhd.extend_from_slice(&7u32.to_be_bytes()); // 未定义的 track
        let trun = {
            let mut b = vec![0u8, 0x00, 0x00, 0x00];
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
        let traf = box_full(
            b"traf",
            &[box_full(b"tfhd", &tfhd), box_full(b"trun", &trun)].concat(),
        );
        let moof = box_full(
            b"moof",
            &box_full(b"mfhd", &be32(1))
                .into_iter()
                .chain(traf)
                .collect::<Vec<u8>>(),
        );

        let mut track_samples = std::collections::HashMap::new();
        assert!(parse_fragment_multi(&moof, 0, &layouts, &mut track_samples).is_err());
    }

    // -------------------------------------------------------- fill_missing_durations

    #[test]
    fn fill_missing_durations_derives_from_next_dts_and_falls_back() {
        let mut samples = vec![
            SampleInfo {
                offset: 0,
                dts: 0,
                duration: 0,
                cts: 0,
                is_key: true,
                size: 1,
            },
            SampleInfo {
                offset: 1,
                dts: 33,
                duration: 0,
                cts: 0,
                is_key: false,
                size: 1,
            },
            SampleInfo {
                offset: 2,
                dts: 66,
                duration: 0,
                cts: 0,
                is_key: false,
                size: 1,
            },
        ];
        fill_missing_durations(&mut samples, 40);
        // 前两个样本由后继 dts 差推出 33；末样本无后继，回退前一个样本的时长
        assert_eq!(samples[0].duration, 33);
        assert_eq!(samples[1].duration, 33);
        assert_eq!(samples[2].duration, 33, "末样本应回退到前一样本的时长");
    }

    #[test]
    fn fill_missing_durations_uses_nominal_for_single_sample() {
        let mut samples = vec![SampleInfo {
            offset: 0,
            dts: 0,
            duration: 0,
            cts: 0,
            is_key: true,
            size: 1,
        }];
        fill_missing_durations(&mut samples, 41);
        assert_eq!(samples[0].duration, 41, "孤立样本应使用标称帧间隔兜底");
    }

    #[test]
    fn fill_missing_durations_is_noop_when_all_present() {
        let mut samples = vec![SampleInfo {
            offset: 0,
            dts: 0,
            duration: 33,
            cts: 0,
            is_key: true,
            size: 1,
        }];
        fill_missing_durations(&mut samples, 0);
        assert_eq!(samples[0].duration, 33, "已有时长绝不能被覆盖");
    }

    // -------------------------------------------------------------- parse_layout（单轨兼容层）

    #[test]
    fn parse_layout_rejects_multi_track() {
        let trak2 = {
            let tkhd = {
                let mut body = vec![0u8; 16];
                body[12..16].copy_from_slice(&2u32.to_be_bytes());
                box_full(b"tkhd", &body)
            };
            let mdhd = {
                let mut body = vec![0u8; 12];
                body.extend_from_slice(&48000u32.to_be_bytes());
                body.extend_from_slice(&0u32.to_be_bytes());
                box_full(b"mdhd", &body)
            };
            let hdlr = {
                let mut body = vec![0u8; 8];
                body.extend_from_slice(b"soun");
                box_full(b"hdlr", &body)
            };
            let stsd = box_full(b"stsd", &[0xBB; 8]);
            let mdia = box_full(
                b"mdia",
                &[mdhd, hdlr, box_full(b"minf", &box_full(b"stbl", &stsd))].concat(),
            );
            box_full(b"trak", &[tkhd, mdia].concat())
        };
        let moov = [sample_moov(1, None), trak2].concat();
        let err = parse_layout(&moov).expect_err("双轨必须被单轨兼容层拒绝");
        assert!(
            err.to_string().contains("2"),
            "错误信息应给出实际轨道数，实际：{err}"
        );
    }
}
