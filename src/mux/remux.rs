//! 把「分片态」MP4 收敛成普通 MP4（`ftyp / mdat / moov`）。
//!
//! ## 为什么需要它
//! 软编路径由 [`super::mp4::Mp4Muxer`] 自己写盒，`finish()` 里已经在内存里握着
//! 全部样本信息，soft remux 只是「末尾写 moov + 开头 16 字节覆写」。
//!
//! 但 GPU 路径的盒是 **Media Foundation 写的**：我们把容器类型设成
//! `MFTranscodeContainerType_FMPEG4` 换来崩溃安全（每片自带 `moof`），代价是
//! `Finalize()` 只会补一个 `mfra`，成品停在分片态，且布局由 MF 决定——
//! `ftyp / uuid / pdin / moov / moof mdat × N / mfra`。
//!
//! 于是这里做一次**只读头部、不搬移媒体数据**的收敛：
//!
//! ```text
//! 收敛前：  ftyp │ uuid │ pdin │ moov(init) │ moof mdat │ moof mdat │ … │ mfra
//! 收敛后：  ftyp │ mdat ← 把上面整段吞进去 ────────────────────────→ │ moov(全量索引)
//! ```
//!
//! 具体动作只有三步：
//! 1. 扫 `moof` 解析出每个样本的大小 / 时长 / 关键帧标记 / **绝对文件偏移**；
//! 2. 在文件末尾追加一个普通 moov（`stbl` 用刚解析出的完整样本表）；
//! 3. 把 `ftyp` 之后那 16 字节覆写成覆盖到旧文件末尾的 64 位 `mdat` 头。
//!
//! 样本字节一个都没动，`co64` 里的偏移就是扫出来的原值——所以这个操作的成本
//! 与文件大小无关，只与样本数成正比（1 小时 60fps 约几十毫秒）。
//!
//! ## 为什么 mdat 能吞掉 init moov 和所有 moof
//! `mdat` 的内容对读取方是黑盒，没有任何解析器会进去找盒。而分片文件里样本位置
//! 相对 `moof` 起点（`default-base-is-moof`）或是 `tfhd` 的显式绝对偏移，
//! 两种情况在「不搬移字节」时都依然成立。

use std::fs::OpenOptions;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use super::fragment::{
    FileBox, SampleInfo, TrackLayout, box_body, fill_missing_durations, find_stsd, parse_fragment,
    parse_layout, read_at, scan_top_level, split_boxes,
};
use super::mp4::{build_box, build_full_box, build_mdat_header};

/// 收敛结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemuxReport {
    /// 是否真的改写了文件（原本就是普通 MP4 时为 `false`）
    pub changed: bool,
    /// 吞进 mdat 的分片数
    pub fragments: usize,
    /// 写进 moov 的样本数
    pub samples: usize,
    /// 追加的 moov 字节数
    pub moov_bytes: u64,
    /// 收敛后的文件字节数
    pub file_bytes: u64,
}

/// 把 `path` 上的分片态 MP4 原地收敛成普通 MP4。
///
/// 已经是普通 MP4（顶层没有 `moof`）时直接返回 `changed: false`，可重复调用。
pub fn finalize_fragmented(path: &Path) -> io::Result<RemuxReport> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();

    // ---- 1. 顶层盒扫描 ----
    let boxes = scan_top_level(&mut file, len)?;
    if boxes.is_empty() {
        return Err(io::Error::other("文件为空或不是有效的 MP4"));
    }

    let ftyp = boxes
        .iter()
        .find(|b| &b.kind == b"ftyp")
        .ok_or_else(|| io::Error::other("文件缺少 ftyp 盒"))?;
    let ftyp_end = ftyp.offset + ftyp.size;

    let moofs: Vec<&FileBox> = boxes.iter().filter(|b| &b.kind == b"moof").collect();
    if moofs.is_empty() {
        // 已经是普通 MP4（或本就不分片）——无事可做。
        return Ok(RemuxReport {
            changed: false,
            fragments: 0,
            samples: 0,
            moov_bytes: 0,
            file_bytes: len,
        });
    }

    let init_moov = boxes
        .iter()
        .find(|b| &b.kind == b"moov")
        .ok_or_else(|| io::Error::other("分片文件缺少 init moov"))?;
    if init_moov.offset > moofs[0].offset {
        return Err(io::Error::other("init moov 位于首个分片之后，布局不支持"));
    }

    let init_bytes = read_at(&mut file, init_moov.offset, init_moov.size)?;
    let init_body = box_body(&init_bytes)?;
    let layout = parse_layout(init_body)?;
    let stsd = find_stsd(init_body)?;

    // ---- 2. 逐分片解析出样本表 ----
    let mut samples: Vec<SampleInfo> = Vec::new();
    for moof in &moofs {
        let bytes = read_at(&mut file, moof.offset, moof.size)?;
        parse_fragment(&bytes, moof.offset, &layout, &mut samples)?;
    }
    if samples.is_empty() {
        return Err(io::Error::other("分片里没有任何样本"));
    }
    fill_missing_durations(&mut samples, 0);

    // 覆写 16 字节做 mdat 头的前提：不能压到样本数据上。
    if let Some(first) = samples.first() {
        if first.offset < ftyp_end + 16 {
            return Err(io::Error::other("首个样本落在文件头 16 字节内，拒绝覆写"));
        }
    }

    // ---- 3. 末尾追加普通 moov ----
    let moov = build_plain_moov(init_body, &layout, &stsd, &samples)?;

    file.seek(SeekFrom::Start(len))?;
    file.write_all(&moov)?;

    // ---- 4. soft remux：整个「隐藏分片结构」就是下面这 16 字节 ----
    file.seek(SeekFrom::Start(ftyp_end))?;
    file.write_all(&build_mdat_header(len - ftyp_end))?;

    file.flush()?;
    file.sync_all()?;

    Ok(RemuxReport {
        changed: true,
        fragments: moofs.len(),
        samples: samples.len(),
        moov_bytes: moov.len() as u64,
        file_bytes: len + moov.len() as u64,
    })
}

// ------------------------------------------------------------------ 文件扫描

// -------------------------------------------------------- 普通 MP4 的收尾加固

/// 普通 MP4（moov 由 Media Foundation 自己写）的收尾报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlainReport {
    /// 是否改写了文件（把 `stco` 升级成 `co64`）
    pub changed: bool,
    /// 升级过的轨道数
    pub tracks: usize,
    /// 升级过的 chunk 数
    pub chunks: usize,
    /// `mdat` 用的是 32 位长度头
    pub mdat_32bit: bool,
    /// **危险**：文件已超过 4 GiB，而偏移表/mdat 头还是 32 位
    pub oversize_risk: bool,
    /// 文件字节数
    pub file_bytes: u64,
}

/// 4 GiB 边界：32 位字段能表达的最大值。
const U32_MAX: u64 = u32::MAX as u64;

/// 给「moov 由别人写」的普通 MP4 做收尾加固：`stco` → `co64`。
///
/// ## 为什么必须做
/// 走普通 MP4 的路径（目前是 GPU AV1，以及降级到 `Plain` 的 H.264）里，moov 由
/// Media Foundation 写，实测它在这种模式下写的是 **32 位** 偏移表（`stco`）和
/// 32 位 mdat 头：
///
/// ```text
/// MF 普通 MP4:   mdat 头 8 B / size32      stbl: … stco  ← 4 GiB 处回绕
/// MF 分片 MP4:   mdat 头 16 B / largesize  stbl: … co64  ← 没问题
/// ```
///
/// 也就是说录到 4 GiB 时 chunk 偏移会回绕、文件读不回来。而 moov 是文件
/// 最后一个盒，截掉重写即可——**媒体字节一个都不用搬**，成本只与 chunk 数成正比。
///
/// `mdat` 的 32 位头是另一半问题：它的长度字段已经写死，超过 4 GiB 时无法原地
/// 扩成 16 字节 large-size 形式（后面紧跟第一帧数据）。所以那种情况只能**如实报告**
/// （[`PlainReport::oversize_risk`]），由调用方决定怎么告知用户——**不能假装修好了**。
pub fn finalize_plain(path: &Path) -> io::Result<PlainReport> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    let boxes = scan_top_level(&mut file, len)?;
    if boxes.is_empty() {
        return Err(io::Error::other("文件为空或不是有效的 MP4"));
    }
    if boxes.iter().any(|b| &b.kind == b"moof") {
        return Err(io::Error::other(
            "该文件是分片态，应调用 finalize_fragmented",
        ));
    }

    let mdat_32bit = boxes
        .iter()
        .find(|b| &b.kind == b"mdat")
        .map(|b| b.header_len == 8)
        .unwrap_or(false);

    let Some(moov) = boxes.iter().find(|b| &b.kind == b"moov") else {
        return Ok(PlainReport {
            changed: false,
            tracks: 0,
            chunks: 0,
            mdat_32bit,
            oversize_risk: false,
            file_bytes: len,
        });
    };

    // 只有当 moov 是最后一个盒时才能截掉重写（否则要搬后面的数据）。
    let moov_at_end = moov.offset + moov.size == len;
    let moov_bytes = read_at(&mut file, moov.offset, moov.size)?;
    let moov_body = box_body(&moov_bytes)?;

    let Some((new_body, tracks, chunks)) = upgrade_stbl_to_co64(moov_body)? else {
        return Ok(PlainReport {
            changed: false,
            tracks: 0,
            chunks: 0,
            mdat_32bit,
            oversize_risk: mdat_32bit && len > U32_MAX,
            file_bytes: len,
        });
    };
    if !moov_at_end {
        return Err(io::Error::other(
            "moov 不在文件末尾，无法就地重写（该文件可能做过 faststart）",
        ));
    }

    let new_moov = build_box(b"moov", &new_body);
    file.set_len(moov.offset)?;
    file.seek(SeekFrom::Start(moov.offset))?;
    file.write_all(&new_moov)?;
    file.flush()?;
    file.sync_all()?;

    let new_len = moov.offset + new_moov.len() as u64;
    Ok(PlainReport {
        changed: true,
        tracks,
        chunks,
        mdat_32bit,
        oversize_risk: mdat_32bit && new_len > U32_MAX,
        file_bytes: new_len,
    })
}

/// 逐级下钻把 `stbl` 里的 `stco` 换成 `co64`；没有 `stco` 时返回 `None`。
fn upgrade_stbl_to_co64(moov_body: &[u8]) -> io::Result<Option<(Vec<u8>, usize, usize)>> {
    let (body, tracks, chunks) = map_children(moov_body, b"trak", upgrade_trak)?;
    Ok(if tracks == 0 {
        None
    } else {
        Some((body, tracks, chunks))
    })
}

fn upgrade_trak(trak_body: &[u8]) -> io::Result<Option<(Vec<u8>, usize)>> {
    let (body, hits, chunks) = map_children(trak_body, b"mdia", upgrade_mdia)?;
    Ok(if hits == 0 {
        None
    } else {
        Some((body, chunks))
    })
}

fn upgrade_mdia(mdia_body: &[u8]) -> io::Result<Option<(Vec<u8>, usize)>> {
    let (body, hits, chunks) = map_children(mdia_body, b"minf", upgrade_minf)?;
    Ok(if hits == 0 {
        None
    } else {
        Some((body, chunks))
    })
}

fn upgrade_minf(minf_body: &[u8]) -> io::Result<Option<(Vec<u8>, usize)>> {
    let (body, hits, chunks) = map_children(minf_body, b"stbl", upgrade_stbl)?;
    Ok(if hits == 0 {
        None
    } else {
        Some((body, chunks))
    })
}

/// `stbl` 内的转换：`stco` → `co64`，其余子盒逐字节保留。返回 (新 body, chunk 数)。
fn upgrade_stbl(stbl_body: &[u8]) -> io::Result<Option<(Vec<u8>, usize)>> {
    let children = split_boxes(stbl_body)?;
    let Some(stco) = children.iter().find(|b| &b.kind == b"stco") else {
        return Ok(None);
    };

    // stco: full box → count(4) + offset(4) × count
    let body = stco.body(stbl_body);
    if body.len() < 8 {
        return Err(io::Error::other("stco 长度不足"));
    }
    let count = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
    if body.len() < 8 + count * 4 {
        return Err(io::Error::other("stco 条目数超出盒长"));
    }
    let mut payload = Vec::with_capacity(4 + count * 8);
    payload.extend_from_slice(&(count as u32).to_be_bytes());
    for i in 0..count {
        let off = u32::from_be_bytes(body[8 + i * 4..12 + i * 4].try_into().unwrap());
        payload.extend_from_slice(&(off as u64).to_be_bytes());
    }
    let co64 = build_full_box(b"co64", 0, 0, &payload);

    let mut out = Vec::with_capacity(stbl_body.len() + count * 4);
    for child in &children {
        if std::ptr::eq(child, stco) {
            out.extend_from_slice(&co64);
        } else {
            out.extend_from_slice(child.full(stbl_body));
        }
    }
    Ok(Some((out, count)))
}

/// 把 `body` 里的 `target` 子盒交给 `f` 处理，其余原样保留。
///
/// 返回 (新 body, 命中数, `f` 报回的 chunk 数之和)。
fn map_children<F>(body: &[u8], target: &[u8; 4], f: F) -> io::Result<(Vec<u8>, usize, usize)>
where
    F: Fn(&[u8]) -> io::Result<Option<(Vec<u8>, usize)>>,
{
    let children = split_boxes(body)?;
    let mut out = Vec::with_capacity(body.len());
    let mut hits = 0usize;
    let mut chunks = 0usize;
    for child in &children {
        if &child.kind == target
            && let Some((new_body, n)) = f(child.body(body))?
        {
            hits += 1;
            chunks += n;
            out.extend_from_slice(&build_box(target, &new_body));
            continue;
        }
        out.extend_from_slice(child.full(body));
    }
    Ok((out, hits, chunks))
}

// ---------------------------------------------------------------- stbl 构建

/// 用「init moov 骨架 + 照抄的 `stsd` + 全量样本表」拼出一个普通 moov。
///
/// 两条路径共用：
/// - [`finalize_fragmented`] 把它追加到录制文件末尾（样本原地不动）；
/// - [`super::extract::extract_range`] 把它写到切片文件末尾（样本已复制到新偏移）。
///
/// **`stsd` 一律照抄，绝不重建**：`avcC` / `hvcC` / `av1C` 里的参数集与
/// `lengthSizeMinusOne` 由编码器决定，重建就是给自己挖坑——这也是切片能
/// 同时支持 H.264 / HEVC / AV1 的根本原因。
pub(crate) fn build_plain_moov(
    init_body: &[u8],
    layout: &TrackLayout,
    stsd: &[u8],
    samples: &[SampleInfo],
) -> io::Result<Vec<u8>> {
    let track_duration: u64 = samples.iter().map(|s| s.duration as u64).sum();
    let movie_duration = scale_duration(
        track_duration,
        layout.media_timescale,
        layout.movie_timescale,
    );
    let stbl = build_stbl(stsd, samples);
    rebuild_moov(init_body, &stbl, movie_duration, track_duration)
}

/// 组装 `stbl`：`stsd` 原样拷贝 + 新算的样本表。
fn build_stbl(stsd: &[u8], samples: &[SampleInfo]) -> Vec<u8> {
    let chunks = build_chunks(samples);

    // stts：按时长做 run-length
    let mut stts_entries: Vec<(u32, u32)> = Vec::new();
    for s in samples {
        match stts_entries.last_mut() {
            Some(last) if last.1 == s.duration => last.0 += 1,
            _ => stts_entries.push((1, s.duration)),
        }
    }
    let mut p = Vec::new();
    p.extend_from_slice(&(stts_entries.len() as u32).to_be_bytes());
    for (count, delta) in &stts_entries {
        p.extend_from_slice(&count.to_be_bytes());
        p.extend_from_slice(&delta.to_be_bytes());
    }
    let stts = build_full_box(b"stts", 0, 0, &p);

    // ctts：只有出现非零合成偏移（B 帧）才写
    let has_cts = samples.iter().any(|s| s.cts != 0);
    let ctts = if has_cts {
        let version = if samples.iter().any(|s| s.cts < 0) {
            1
        } else {
            0
        };
        let mut entries: Vec<(u32, i64)> = Vec::new();
        for s in samples {
            match entries.last_mut() {
                Some(last) if last.1 == s.cts => last.0 += 1,
                _ => entries.push((1, s.cts)),
            }
        }
        let mut p = Vec::new();
        p.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (count, off) in &entries {
            p.extend_from_slice(&count.to_be_bytes());
            p.extend_from_slice(&(*off as u32).to_be_bytes());
        }
        Some(build_full_box(b"ctts", version, 0, &p))
    } else {
        None
    };

    // stss：全部是同步样本时可省略（省略即代表全同步）
    let keys: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_key)
        .map(|(i, _)| (i + 1) as u32)
        .collect();
    let stss = if keys.len() == samples.len() {
        None
    } else {
        let mut p = Vec::new();
        p.extend_from_slice(&(keys.len() as u32).to_be_bytes());
        for k in &keys {
            p.extend_from_slice(&k.to_be_bytes());
        }
        Some(build_full_box(b"stss", 0, 0, &p))
    };

    // stsc：相邻 chunk 样本数相同时做 run-length
    let mut sc: Vec<(u32, u32)> = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        match sc.last() {
            Some(&(_, last)) if last == c.sample_count as u32 => {}
            _ => sc.push(((i + 1) as u32, c.sample_count as u32)),
        }
    }
    if sc.is_empty() {
        sc.push((1, samples.len() as u32));
    }
    let mut p = Vec::new();
    p.extend_from_slice(&(sc.len() as u32).to_be_bytes());
    for (first_chunk, spc) in &sc {
        p.extend_from_slice(&first_chunk.to_be_bytes());
        p.extend_from_slice(&spc.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
    }
    let stsc = build_full_box(b"stsc", 0, 0, &p);

    // stsz：逐样本长度
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0 → 变长
    p.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for s in samples {
        p.extend_from_slice(&s.size.to_be_bytes());
    }
    let stsz = build_full_box(b"stsz", 0, 0, &p);

    // co64：chunk 偏移（64 位，不受 4 GiB 限制）
    let mut p = Vec::new();
    p.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
    for c in &chunks {
        p.extend_from_slice(&c.offset.to_be_bytes());
    }
    let co64 = build_full_box(b"co64", 0, 0, &p);

    let mut body = Vec::new();
    body.extend_from_slice(stsd);
    body.extend_from_slice(&stts);
    if let Some(ctts) = ctts {
        body.extend_from_slice(&ctts);
    }
    if let Some(stss) = stss {
        body.extend_from_slice(&stss);
    }
    body.extend_from_slice(&stsc);
    body.extend_from_slice(&stsz);
    body.extend_from_slice(&co64);
    build_box(b"stbl", &body)
}

struct ChunkInfo {
    offset: u64,
    sample_count: usize,
}

/// 把样本划成 chunk：物理上连续的样本归同一个 chunk。
///
/// 分片态里样本之间隔着 `moof`，所以 chunk 天然按分片切；
/// 若相邻分片恰好首尾相接，合并成一个更长的 chunk 也无妨——两者索引等价。
fn build_chunks(samples: &[SampleInfo]) -> Vec<ChunkInfo> {
    let mut chunks: Vec<ChunkInfo> = Vec::new();
    let mut expected_next: Option<u64> = None;
    for s in samples {
        match (&expected_next, chunks.last_mut()) {
            (Some(next), Some(last)) if *next == s.offset => {
                last.sample_count += 1;
            }
            _ => chunks.push(ChunkInfo {
                offset: s.offset,
                sample_count: 1,
            }),
        }
        expected_next = Some(s.offset + s.size as u64);
    }
    chunks
}

// ---------------------------------------------------------------- moov 重建

/// 把 init moov 改造成普通 moov：去掉 `mvex`、补全 `stbl`、刷新三处时长。
fn rebuild_moov(
    init: &[u8],
    stbl: &[u8],
    movie_duration: u64,
    track_duration: u64,
) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();

    for child in split_boxes(init)? {
        match &child.kind {
            // mvex 宣告「样本表在 moof 里」，成品里必须去掉，否则读取方会当成分片文件。
            b"mvex" => {}
            b"mvhd" => {
                let nb = set_timescale_box_duration(child.body(init), movie_duration)?;
                body.extend_from_slice(&build_box(b"mvhd", &nb));
            }
            b"trak" => body.extend_from_slice(&rebuild_trak(
                child.body(init),
                stbl,
                movie_duration,
                track_duration,
            )?),
            _ => body.extend_from_slice(child.full(init)),
        }
    }
    Ok(build_box(b"moov", &body))
}

fn rebuild_trak(
    trak: &[u8],
    stbl: &[u8],
    movie_duration: u64,
    track_duration: u64,
) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    for child in split_boxes(trak)? {
        match &child.kind {
            b"tkhd" => {
                let nb = set_tkhd_duration(child.body(trak), movie_duration)?;
                body.extend_from_slice(&build_box(b"tkhd", &nb));
            }
            b"mdia" => {
                body.extend_from_slice(&rebuild_mdia(child.body(trak), stbl, track_duration)?)
            }
            _ => body.extend_from_slice(child.full(trak)),
        }
    }
    Ok(build_box(b"trak", &body))
}

fn rebuild_mdia(mdia: &[u8], stbl: &[u8], track_duration: u64) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    for child in split_boxes(mdia)? {
        match &child.kind {
            b"mdhd" => {
                let nb = set_timescale_box_duration(child.body(mdia), track_duration)?;
                body.extend_from_slice(&build_box(b"mdhd", &nb));
            }
            b"minf" => {
                let mut minf_body = Vec::new();
                for g in split_boxes(child.body(mdia))? {
                    if &g.kind == b"stbl" {
                        minf_body.extend_from_slice(stbl);
                    } else {
                        minf_body.extend_from_slice(g.full(child.body(mdia)));
                    }
                }
                body.extend_from_slice(&build_box(b"minf", &minf_body));
            }
            _ => body.extend_from_slice(child.full(mdia)),
        }
    }
    Ok(build_box(b"mdia", &body))
}

/// mvhd / mdhd 的布局完全一致：`vf(4) creation mod timescale duration tail`。
///
/// v0 的 duration 只有 32 位——在 10 MHz 时基下约 429 s 就回绕，
/// 所以超出范围时会**升级成 v1**（而不是截断）。
fn set_timescale_box_duration(body: &[u8], duration: u64) -> io::Result<Vec<u8>> {
    // v0：vf(4) creation(4) modification(4) timescale(4) duration(4) → duration 在 16..20
    // v1：vf(4) creation(8) modification(8) timescale(4) duration(8) → duration 在 24..32
    let version = *body.first().ok_or_else(|| io::Error::other("时长盒为空"))?;
    match version {
        0 => {
            if body.len() < 20 {
                return Err(io::Error::other("时长盒（v0）长度不足"));
            }
            if duration <= u32::MAX as u64 {
                let mut out = body.to_vec();
                out[16..20].copy_from_slice(&(duration as u32).to_be_bytes());
                Ok(out)
            } else {
                let mut out = Vec::with_capacity(body.len() + 8);
                out.extend_from_slice(&body[0..4]); // version + flags
                out.extend_from_slice(&zero_extend_u32(&body[4..8])); // creation_time
                out.extend_from_slice(&zero_extend_u32(&body[8..12])); // modification_time
                out.extend_from_slice(&body[12..16]); // timescale
                out.extend_from_slice(&duration.to_be_bytes());
                out.extend_from_slice(&body[20..]); // rate/volume/… 或 language
                out[0] = 1;
                Ok(out)
            }
        }
        1 => {
            if body.len() < 32 {
                return Err(io::Error::other("时长盒（v1）长度不足"));
            }
            let mut out = body.to_vec();
            out[24..32].copy_from_slice(&duration.to_be_bytes());
            Ok(out)
        }
        v => Err(io::Error::other(format!("时长盒 version {v} 未知"))),
    }
}

/// tkhd 布局与 mvhd 不同：`vf(4) creation mod track_ID reserved duration tail`。
fn set_tkhd_duration(body: &[u8], duration: u64) -> io::Result<Vec<u8>> {
    let version = *body.first().ok_or_else(|| io::Error::other("tkhd 为空"))?;
    match version {
        0 => {
            if body.len() < 24 {
                return Err(io::Error::other("tkhd（v0）长度不足"));
            }
            if duration <= u32::MAX as u64 {
                let mut out = body.to_vec();
                out[20..24].copy_from_slice(&(duration as u32).to_be_bytes());
                Ok(out)
            } else {
                let mut out = Vec::with_capacity(body.len() + 8);
                out.extend_from_slice(&body[0..4]); // version + flags
                out.extend_from_slice(&zero_extend_u32(&body[4..8])); // creation_time
                out.extend_from_slice(&zero_extend_u32(&body[8..12])); // modification_time
                out.extend_from_slice(&body[12..16]); // track_ID
                out.extend_from_slice(&body[16..20]); // reserved
                out.extend_from_slice(&duration.to_be_bytes());
                out.extend_from_slice(&body[24..]); // layer/…/width/height
                out[0] = 1;
                Ok(out)
            }
        }
        1 => {
            if body.len() < 36 {
                return Err(io::Error::other("tkhd（v1）长度不足"));
            }
            let mut out = body.to_vec();
            out[28..36].copy_from_slice(&duration.to_be_bytes());
            Ok(out)
        }
        v => Err(io::Error::other(format!("tkhd version {v} 未知"))),
    }
}

fn zero_extend_u32(v: &[u8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[4..].copy_from_slice(&v[..4]);
    out
}

/// 在两个时基之间换算是纯比例运算，溢出时饱和即可（时长字段的取舍不影响解读样本）。
fn scale_duration(duration: u64, from: u32, to: u32) -> u64 {
    if from == 0 || from == to {
        return duration;
    }
    ((duration as u128 * to as u128) / from as u128).min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::fragment::{descend_full, kind_str};
    use crate::mux::mp4::Mp4Muxer;

    fn make_sps_pps() -> (Vec<u8>, Vec<u8>) {
        (
            vec![0x67, 0x42, 0x00, 0x1E, 0xAB],
            vec![0x68, 0xCE, 0x3C, 0x80],
        )
    }

    /// 造一个「录制中」的分片文件：`ftyp / free / moov(init) / moof mdat × N`。
    ///
    /// 这就是 MF 输出的结构同构体（MF 只是多写 uuid / pdin / mfra，它们都落在
    /// mdat 覆盖范围内，不参与任何解析）。
    ///
    /// 注意末尾那个「收尾关键帧」：`Mp4Muxer` 只在遇到关键帧时闭合分片，
    /// 所以最后一帧必须是关键帧，前一片才会真正落盘——对应 MF 里 `Finalize()`
    /// 把编码器缓冲全部吐完的效果。
    fn make_fragmented(dir: &std::path::Path, frames: u64) -> std::path::PathBuf {
        let path = dir.join("fragmented.mp4");
        let (sps, pps) = make_sps_pps();
        let mut muxer = Mp4Muxer::create(&path, 320, 240, 30).unwrap();
        for i in 0..frames {
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
        // 不调用 finish：文件停留在分片态。
        path
    }

    fn top_level_kinds(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut kinds = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= bytes.len() {
            let size32 = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
            let size = match size32 {
                0 => bytes.len() - pos,
                1 => u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize,
                n => n as usize,
            };
            assert!(size >= 8, "盒大小非法: {size}");
            kinds.push(bytes[pos + 4..pos + 8].to_vec());
            pos += size;
        }
        assert_eq!(pos, bytes.len(), "顶层盒序列应恰好覆盖整个文件");
        kinds
    }

    /// 解析成品文件的完整样本表，返回 `(偏移, 大小, 时长, 关键帧)`。
    fn read_sample_table(bytes: &[u8]) -> Vec<(u64, u32, u32, bool)> {
        let boxes = split_boxes(bytes).unwrap();
        let moov = boxes
            .iter()
            .find(|b| &b.kind == b"moov")
            .expect("缺少顶层 moov");
        let stbl =
            descend_full(moov.body(bytes), &[*b"trak", *b"mdia", *b"minf", *b"stbl"]).unwrap();
        let stbl_body = box_body(stbl).unwrap();
        let inner = split_boxes(stbl_body).unwrap();
        let get = |k: &[u8; 4]| {
            inner
                .iter()
                .find(|b| &b.kind == k)
                .unwrap_or_else(|| panic!("缺少 {}", kind_str(k)))
                .body(stbl_body)
                .to_vec()
        };

        // 注意：这些表都是 full 盒，body 的前 4 字节是 version+flags。
        let u32_at = |b: &[u8], at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());

        let stsz = get(b"stsz");
        let count = u32_at(&stsz, 8) as usize; // [4..8] 是 sample_size（0=变长）
        let sizes: Vec<u32> = (0..count).map(|i| u32_at(&stsz, 12 + i * 4)).collect();

        // stts / stss / stsc / co64：vf(4) + entry_count(4) + entries
        let stts = get(b"stts");
        let stts_count = u32_at(&stts, 4) as usize;
        let mut durations = Vec::with_capacity(count);
        for i in 0..stts_count {
            let base = 8 + i * 8;
            let n = u32_at(&stts, base);
            let d = u32_at(&stts, base + 4);
            for _ in 0..n {
                durations.push(d);
            }
        }

        let stss = inner.iter().find(|b| &b.kind == b"stss");
        let keys: Vec<u32> = match stss {
            Some(b) => {
                let body = b.body(stbl_body);
                let n = u32_at(body, 4) as usize;
                (0..n).map(|i| u32_at(body, 8 + i * 4)).collect()
            }
            None => (1..=count as u32).collect(),
        };

        // stsc + co64 → 每样本绝对偏移
        let stsc = get(b"stsc");
        let sc_n = u32_at(&stsc, 4) as usize;
        let sc: Vec<(u32, u32)> = (0..sc_n)
            .map(|i| {
                let base = 8 + i * 12;
                (u32_at(&stsc, base), u32_at(&stsc, base + 4))
            })
            .collect();
        let co64 = get(b"co64");
        let ch_n = u32_at(&co64, 4) as usize;
        let offsets: Vec<u64> = (0..ch_n)
            .map(|i| u64::from_be_bytes(co64[8 + i * 8..16 + i * 8].try_into().unwrap()))
            .collect();

        let mut sample_offsets = Vec::with_capacity(count);
        for (ci, &chunk_off) in offsets.iter().enumerate() {
            let spc = sc
                .iter()
                .rev()
                .find(|(first, _)| (*first as usize) <= ci + 1)
                .map(|(_, n)| *n)
                .unwrap() as usize;
            let mut off = chunk_off;
            for k in 0..spc {
                let idx = sample_offsets.len();
                sample_offsets.push(off);
                off += sizes[idx] as u64;
                let _ = k;
            }
        }

        (0..count)
            .map(|i| {
                (
                    sample_offsets[i],
                    sizes[i],
                    durations.get(i).copied().unwrap_or(0),
                    keys.contains(&((i + 1) as u32)),
                )
            })
            .collect()
    }

    /// 核心用例：分片文件收敛后必须是 `ftyp/mdat/moov`，且样本表逐项正确、
    /// co64 指向的字节就是原始媒体数据。
    #[test]
    fn fragmented_file_becomes_plain_mp4_with_valid_sample_table() {
        let dir = tempfile::tempdir().unwrap();
        // 151 帧 → 第 150 帧是关键帧，把第 120..149 帧那片顶下去，
        // 于是盘上恰好是 5 个完整分片 / 150 个样本。
        let path = make_fragmented(dir.path(), 151);

        // 收敛前：顶层有 moof
        let before = std::fs::read(&path).unwrap();
        assert!(
            top_level_kinds(&before).iter().any(|k| k == b"moof"),
            "测试前置条件：文件应处于分片态"
        );

        let report = finalize_fragmented(&path).unwrap();
        assert!(report.changed);
        assert_eq!(report.samples, 150, "盘上应有 5 个完整分片 × 30 帧");
        assert_eq!(report.fragments, 5);

        let after = std::fs::read(&path).unwrap();
        assert_eq!(report.file_bytes as usize, after.len());
        assert_eq!(
            top_level_kinds(&after),
            vec![b"ftyp".to_vec(), b"mdat".to_vec(), b"moov".to_vec()],
            "收敛后顶层只能是 ftyp/mdat/moov"
        );

        // 样本表必须与录制时写入的一致：150 个样本、每样本 100(+参数集) 字节。
        let table = read_sample_table(&after);
        assert_eq!(table.len(), 150);
        for (i, (off, size, dur, is_key)) in table.iter().enumerate() {
            assert!(
                (*off as usize) + (*size as usize) <= after.len(),
                "样本 {i} 越界"
            );
            // 数据区必须落在 mdat 内部
            assert!(*off >= 16 + 24, "样本 {i} 落在文件头里");
            // PTS 按 i*1e6/30 取整，相邻差值在 33333 / 33334 之间摆动。
            assert!(
                (33_333..=33_334).contains(dur),
                "样本 {i} 时长应约 1/30 s，实际 {dur}"
            );
            assert_eq!(*is_key, i % 30 == 0, "样本 {i} 关键帧标记错误");
        }
        // 关键帧样本前 4 字节是 SPS 长度前缀，非关键帧前 4 字节是 100。
        let key_off = table[30].0 as usize;
        let key_prefix = u32::from_be_bytes(after[key_off..key_off + 4].try_into().unwrap());
        assert_eq!(key_prefix, 5, "关键帧应以 SPS 长度前缀开头");
        let p_off = table[31].0 as usize;
        let p_prefix = u32::from_be_bytes(after[p_off..p_off + 4].try_into().unwrap());
        assert_eq!(p_prefix, 100, "P 帧应以 NAL 长度前缀开头");

        // 总时长必须与分片态一致（150 帧 ≈ 5 s），这是 moov 能否被信任的关键。
        let total: u64 = table.iter().map(|(_, _, d, _)| *d as u64).sum();
        assert!(
            (4_990_000..=5_010_000).contains(&total),
            "stts 总时长应约 5 s，实际 {total}"
        );
    }

    /// mdat 必须把分片结构整段吞掉：长度恰好到旧的 EOF，且用 64 位 largesize。
    #[test]
    fn mdat_swallows_the_whole_fragment_structure() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_fragmented(dir.path(), 90);
        let before_len = std::fs::metadata(&path).unwrap().len();

        let report = finalize_fragmented(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();

        let boxes = split_boxes(&bytes).unwrap();
        let ftyp = boxes
            .iter()
            .find(|b| &b.kind == b"ftyp")
            .expect("缺少 ftyp");
        let mdat = boxes
            .iter()
            .find(|b| &b.kind == b"mdat")
            .expect("缺少 mdat");
        assert_eq!(mdat.header_len, 16, "mdat 应使用 64 位 largesize");
        assert_eq!(
            mdat.size as u64,
            before_len - ftyp.size as u64,
            "mdat 应恰好覆盖到旧文件末尾"
        );
        assert_eq!(
            report.moov_bytes as usize,
            bytes.len() - before_len as usize,
            "末尾 moov 长度应与报告一致"
        );
    }

    /// 末尾 moov 不能带 mvex，否则读取方仍会当成分片文件。
    #[test]
    fn final_moov_has_no_mvex() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_fragmented(dir.path(), 60);
        finalize_fragmented(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let moov = split_boxes(&bytes)
            .unwrap()
            .into_iter()
            .find(|b| &b.kind == b"moov")
            .unwrap();
        let kinds: Vec<Vec<u8>> = split_boxes(moov.body(&bytes))
            .unwrap()
            .iter()
            .map(|b| b.kind.to_vec())
            .collect();
        assert!(
            !kinds.contains(&b"mvex".to_vec()),
            "moov 不应含 mvex: {kinds:?}"
        );
        assert!(kinds.contains(&b"mvhd".to_vec()));
        assert!(kinds.contains(&b"trak".to_vec()));
    }

    /// 对已经是普通 MP4 的文件重复调用必须是无操作（幂等）。
    #[test]
    fn plain_mp4_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.mp4");
        let (sps, pps) = make_sps_pps();
        {
            let mut muxer = Mp4Muxer::create(&path, 320, 240, 30).unwrap();
            for i in 0..10u64 {
                muxer
                    .write_sample(&[0x41u8; 32], i * 33_333, i == 0, &sps, &pps)
                    .unwrap();
            }
            muxer.finish().unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let report = finalize_fragmented(&path).unwrap();
        assert!(!report.changed, "普通 MP4 不应被改写");
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "文件内容必须逐字节不变");
    }

    /// 长时长跨过 v0 的 32 位上限时必须升级到 v1，而不是回绕。
    #[test]
    fn long_duration_upgrades_to_version_1() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_fragmented(dir.path(), 30);
        finalize_fragmented(&path).unwrap();

        // 直接测两个时长盒的升级行为：v0 布局 + 超大时长
        // v0：vf(4) creation(4) mod(4) timescale(4) duration(4) tail
        // v1：vf(4) creation(8) mod(8) timescale(4) duration(8) tail
        let mut mvhd = vec![0u8; 20];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes()); // timescale
        let out = set_timescale_box_duration(&mvhd, 5_000_000_000).unwrap();
        assert_eq!(out[0], 1, "超大时长应升级为 v1");
        assert_eq!(&out[24..32], &5_000_000_000u64.to_be_bytes());
        assert_eq!(&out[20..24], &1000u32.to_be_bytes(), "timescale 应平移保留");

        // tail 必须原样保留（v0 里 duration 之后的 rate/volume 等）
        let mut mvhd_v0 = vec![0u8; 28];
        mvhd_v0[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd_v0[20..24].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
        let out = set_timescale_box_duration(&mvhd_v0, 5_000_000_000).unwrap();
        assert_eq!(
            &out[32..36],
            &0x0001_0000u32.to_be_bytes(),
            "tail 应平移保留"
        );

        let tkhd = vec![0u8; 24];
        let out = set_tkhd_duration(&tkhd, 5_000_000_000).unwrap();
        assert_eq!(out[0], 1);
        assert_eq!(&out[28..36], &5_000_000_000u64.to_be_bytes());

        // v1 原地更新：duration 在 24..32（mvhd/mdhd）/ 28..36（tkhd）
        let mut v1 = vec![0u8; 32];
        v1[0] = 1;
        let out = set_timescale_box_duration(&v1, 42).unwrap();
        assert_eq!(&out[24..32], &42u64.to_be_bytes());
    }

    /// 时长换算：媒体时基 → 电影时基。
    #[test]
    fn duration_scaling_between_timescales() {
        assert_eq!(scale_duration(1_000_000, 1_000_000, 1000), 1000);
        assert_eq!(scale_duration(1_000_000, 1_000_000, 1_000_000), 1_000_000);
        assert_eq!(scale_duration(123, 0, 1000), 123, "时基为 0 时原样返回");
    }

    /// 没有任何分片的残文件必须明确报错，而不是写出半截 moov。
    #[test]
    fn non_mp4_input_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.bin");
        std::fs::write(&path, [0u8; 64]).unwrap();
        // 全零 → 首个盒 size=0 覆盖到末尾，kind 非法但没有 moof，
        // 于是当作「无需处理」；关键是不得 panic。
        let _ = finalize_fragmented(&path);
    }

    // ------------------------------------------------------ 普通 MP4 的收尾加固

    /// 造一个「MF 风格」的普通 MP4：`ftyp / mdat(8 字节头) / moov(带 stco)`。
    ///
    /// 只放解析器真正会走到的盒：`stbl` 里有 `stsd/stts/stsc/stsz/stco`。
    fn make_plain_with_stco(dir: &std::path::Path, offsets: &[u32]) -> std::path::PathBuf {
        let path = dir.join("plain_stco.mp4");

        let stsd = build_full_box(b"stsd", 0, 0, &0u32.to_be_bytes());
        let mut stts_p = Vec::new();
        stts_p.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        stts_p.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        stts_p.extend_from_slice(&1000u32.to_be_bytes());
        let stts = build_full_box(b"stts", 0, 0, &stts_p);

        let mut stsc_p = Vec::new();
        stsc_p.extend_from_slice(&1u32.to_be_bytes());
        stsc_p.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
        stsc_p.extend_from_slice(&(offsets.len() as u32).to_be_bytes()); // samples_per_chunk
        stsc_p.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
        let stsc = build_full_box(b"stsc", 0, 0, &stsc_p);

        let mut stsz_p = Vec::new();
        stsz_p.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0（变长）
        stsz_p.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for _ in offsets {
            stsz_p.extend_from_slice(&100u32.to_be_bytes());
        }
        let stsz = build_full_box(b"stsz", 0, 0, &stsz_p);

        let mut stco_p = Vec::new();
        stco_p.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for o in offsets {
            stco_p.extend_from_slice(&o.to_be_bytes());
        }
        let stco = build_full_box(b"stco", 0, 0, &stco_p);

        let mut stbl_body = Vec::new();
        for b in [stsd, stts, stsc, stsz, stco] {
            stbl_body.extend_from_slice(&b);
        }
        let stbl = build_box(b"stbl", &stbl_body);
        let minf = build_box(b"minf", &stbl);
        let mdia = build_box(b"mdia", &minf);
        let trak = build_box(b"trak", &mdia);
        let moov = build_box(b"moov", &trak);

        let ftyp = build_box(b"ftyp", b"isomisom");
        let mut mdat = Vec::new();
        mdat.extend_from_slice(&(0u32).to_be_bytes()); // 占位
        mdat.extend_from_slice(b"mdat");
        mdat.extend_from_slice(&[0xABu8; 300]);
        let mdat_len = mdat.len() as u32;
        mdat[0..4].copy_from_slice(&mdat_len.to_be_bytes()); // 32 位长度头

        let mut file = Vec::new();
        file.extend_from_slice(&ftyp);
        file.extend_from_slice(&mdat);
        file.extend_from_slice(&moov);
        std::fs::write(&path, file).unwrap();
        path
    }

    /// 顶层盒序列（严格走盒链，能发现越界）。
    fn plain_kinds(bytes: &[u8]) -> Vec<String> {
        let mut kinds = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= bytes.len() {
            let size32 = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
            let size = match size32 {
                0 => bytes.len() - pos,
                1 => u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize,
                n => n as usize,
            };
            assert!(size >= 8);
            kinds.push(String::from_utf8_lossy(&bytes[pos + 4..pos + 8]).to_string());
            pos += size;
        }
        assert_eq!(pos, bytes.len(), "顶层盒应覆盖整个文件");
        kinds
    }

    /// **4 GiB 边界的核心修复**：MF 写普通 MP4 时用 32 位 `stco`，
    /// 超过 4 GiB 偏移就回绕。收尾时必须原地升级成 `co64`，
    /// 而 moov 在文件尾部 → 截掉重写即可，媒体字节一个都不搬。
    #[test]
    fn finalize_plain_upgrades_stco_to_co64() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_plain_with_stco(dir.path(), &[48, 148, 248]);

        let before = std::fs::metadata(&path).unwrap().len();
        let report = finalize_plain(&path).unwrap();
        assert!(report.changed, "有 stco 时必须升级");
        assert_eq!(report.chunks, 3);
        assert!(report.mdat_32bit, "这个夹具刻意用 32 位 mdat 头");
        assert!(!report.oversize_risk, "小文件没有越界风险");

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(plain_kinds(&bytes), vec!["ftyp", "mdat", "moov"]);
        assert_eq!(
            report.file_bytes as usize,
            bytes.len(),
            "报告的文件长度应与实际一致"
        );
        assert!(bytes.len() > before as usize, "co64 每项多 4 字节");

        let moov = split_boxes(&bytes)
            .unwrap()
            .into_iter()
            .find(|b| &b.kind == b"moov")
            .unwrap();
        let stbl =
            descend_full(moov.body(&bytes), &[*b"trak", *b"mdia", *b"minf", *b"stbl"]).unwrap();
        let stbl_body = box_body(stbl).unwrap();
        let inner = split_boxes(stbl_body).unwrap();
        let has = |k: &[u8; 4]| inner.iter().any(|b| &b.kind == k);
        assert!(has(b"co64"), "应出现 co64");
        assert!(!has(b"stco"), "stco 应被替换掉");
        // 其余表必须逐字节保留。
        assert!(has(b"stsd") && has(b"stts") && has(b"stsc") && has(b"stsz"));

        let co64 = inner
            .iter()
            .find(|b| &b.kind == b"co64")
            .unwrap()
            .body(stbl_body);
        let n = u32::from_be_bytes(co64[4..8].try_into().unwrap()) as usize;
        assert_eq!(n, 3);
        let got: Vec<u64> = (0..n)
            .map(|i| u64::from_be_bytes(co64[8 + i * 8..16 + i * 8].try_into().unwrap()))
            .collect();
        assert_eq!(got, vec![48, 148, 248], "偏移值必须原样搬迁");

        // 幂等：再跑一次不应再改。
        let again = finalize_plain(&path).unwrap();
        assert!(!again.changed, "已经升级过的文件不应再次改写");
        assert_eq!(std::fs::read(&path).unwrap().len(), bytes.len());
    }

    /// 分片态文件走错函数时必须明确报错，而不是把 moov 写坏。
    #[test]
    fn finalize_plain_rejects_fragmented_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_fragmented(dir.path(), 61);
        let err = finalize_plain(&path).unwrap_err();
        assert!(
            err.to_string().contains("分片态"),
            "错误应指出该走 finalize_fragmented，实际：{err}"
        );
    }

    /// moov 不在末尾（例如做过 faststart）时不能就地重写，必须报错而不是截坏文件。
    #[test]
    fn finalize_plain_refuses_when_moov_is_not_last() {
        let dir = tempfile::tempdir().unwrap();
        let src = make_plain_with_stco(dir.path(), &[48, 148]);

        // 把 moov 挪到 mdat 之前，模拟 faststart 布局。
        let bytes = std::fs::read(&src).unwrap();
        let boxes = split_boxes(&bytes).unwrap();
        let get = |k: &[u8; 4]| boxes.iter().find(|b| &b.kind == k).unwrap().full(&bytes);
        let mut reordered = Vec::new();
        reordered.extend_from_slice(get(b"ftyp"));
        reordered.extend_from_slice(get(b"moov"));
        reordered.extend_from_slice(get(b"mdat"));
        let path = dir.path().join("faststart.mp4");
        std::fs::write(&path, &reordered).unwrap();

        let err = finalize_plain(&path).unwrap_err();
        assert!(
            err.to_string().contains("moov 不在文件末尾"),
            "应拒绝就地重写，实际：{err}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            reordered,
            "拒绝时不得改动文件"
        );
    }
}
