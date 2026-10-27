//! Hybrid MP4 封装器（单视频轨 / H.264 in avcC / 抗崩溃 / 零预留）。
//!
//! ## 为什么不是普通 MP4
//! 普通 MP4 把 moov（样本索引）写在文件末尾，且它是「读懂 mdat」的唯一依据。
//! 录制中途断电 / 崩溃 / 磁盘写满 → 没有 moov → 文件里躺着几十分钟的码流却完全无法播放。
//!
//! ## Hybrid 布局（录制中）
//! ```text
//! [ftyp][free(16B 占位)][moov: 仅解码初始化 + mvex][moof][mdat 分片数据]...
//! ```
//! 此时文件是一个**合法的 Fragmented MP4**：每个 `moof` 自带本片样本表，
//! 因此中断时最长只损失最后一个未写完的分片（可用 ffmpeg 直接 remux 出来）。
//!
//! ## Hybrid 布局（停止后）
//! 追加一个**完整的普通 moov** 到文件末尾，再把开头的 `free` 覆写成覆盖到
//! 该 moov 之前的巨型 `mdat` 头（64 位 largesize）：
//! ```text
//! [ftyp][mdat: 把上面整段吞进来][moov: 全量样本索引]
//! ```
//! `mdat` 内容对读取方是黑盒，不会有人去解析里面的 init moov / moof，
//! 于是成品在所有播放器、剪辑软件、上传平台的转码器眼里就是一个普通 MP4。
//! 整个「soft remux」只写 16 + moov 字节，不搬移任何媒体数据。
//!
//! 这个方案与 OBS Studio 30.2 起的 Hybrid MP4 输出同源，区别是 free 占位由
//! FFmpeg 的 8 字节扩到 16 字节——正好等于 64 位 mdat 头，从而单段录制不受 4 GiB 盒长限制。
//!
//! ## 为什么按 fragment 分 chunk
//! 分片后样本之间隔着 moof 头，不再物理连续，单 chunk + 单 co64 偏移的写法失效。
//! 每样本一个 chunk 会让 moov 膨胀到 24 B/样本（1 小时 30fps ≈ 2.6 MB）；
//! 而一个分片内的样本是连续的，按分片分 chunk 后 co64 只需 8 B/分片、
//! stsc 还能按相同 samples_per_chunk 做 run-length 合并，1 小时 ≈ 36 KB。
//!
//! 时基：视频轨 timescale = 1_000_000（微秒，`mdhd` v1 携带 64 位时长），
//! mvhd timescale = 1000（毫秒）。

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

/// `free` 占位长度：刻意取 16，正好等于 64 位 mdat 头长度，停止时原地覆写即可。
const RESERVED_HEADER_LEN: u64 = 16;
/// 顶层 mdat 盒头长度：size(4) + "mdat"(4) + largesize(8)。
const MDAT_HEADER_LEN: u64 = 16;
/// 分片内 mdat 盒头长度（分片数据量远小于 4 GiB，用 32 位 size 即可）。
const FRAG_MDAT_HEADER_LEN: u64 = 8;

/// 同步样本（IDR）：sample_depends_on = 2（I 帧），is_non_sync = 0。
const FLAG_SYNC_SAMPLE: u32 = 0x0200_0000;
/// 非同步样本（P 帧）：sample_depends_on = 1，is_non_sync = 1。
const FLAG_NON_SYNC_SAMPLE: u32 = 0x0101_0000;

/// 单个分片的目标时长上限。关键帧间隔正常时为 2 s，这里只做兜底：
/// 编码器长时间不出 IDR 时，避免未落盘样本在内存里无限积压。
const MAX_FRAGMENT_SECONDS: u32 = 10;

/// 已落盘样本的元数据（末尾 moov 的 stbl 用）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SampleRec {
    /// 样本字节数（长度前缀后的 NAL 序列）
    pub(crate) size: u32,
    /// PTS（微秒，timescale = 1_000_000）
    pub(crate) pts_us: u64,
    pub(crate) is_key: bool,
}

/// 一个 chunk（本实现里 = 一个分片）的元数据（末尾 moov 的 stsc/co64 用）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChunkRec {
    /// 该 chunk 第一个样本的绝对文件偏移
    pub(crate) data_offset: u64,
    pub(crate) sample_count: usize,
}

/// 待落盘的样本（分片闭合时才写 moof，因为 trun 需要每个样本的 duration）。
struct PendingSample {
    data: Vec<u8>,
    pts_us: u64,
    is_key: bool,
}

/// 样本描述：`stsd` 里那个 sample entry。**与编码器无关**。
///
/// 软编路径不需要它（`sps`/`pps` 直接构 `avcC`）；硬件路径必须由调用方给，
/// 因为 MF 只吐 Annex B 码流，参数集怎么装进 `avcC`/`hvcC` 得我们决定。
#[derive(Debug, Clone)]
pub struct SampleDescription {
    /// sample entry 四字类型：`avc1` / `hvc1` / `hev1` / `av01`
    pub entry: [u8; 4],
    /// 配置盒（**含盒头**，如 `avcC` / `hvcC` / `av1C`）
    pub config_boxes: Vec<Vec<u8>>,
    /// 写入 compressor name 的显示名（纯装饰）
    pub label: String,
}

pub struct Mp4Muxer {
    file: File,
    width: u32,
    height: u32,
    fps: u32,
    /// ftyp 长度，也是 free/mdat 占位区的起始偏移
    ftyp_len: u64,
    samples: Vec<SampleRec>,
    chunks: Vec<ChunkRec>,
    /// 当前未闭合分片的样本
    pending: Vec<PendingSample>,
    /// mfhd 序列号
    seq: u32,
    /// 首个关键帧的 SPS/PPS（avcC 用）
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// init moov 是否已写（要等第一个关键帧拿到 SPS/PPS）
    header_written: bool,
    /// 外部给的样本描述。`None` = 沿用「从 SPS/PPS 现场构 avcC」的软编老路。
    desc: Option<SampleDescription>,
    finished: bool,
}

/// 构造 free 占位盒（16 字节全零，停止时被改写成 mdat 头）。
fn build_free_placeholder() -> [u8; RESERVED_HEADER_LEN as usize] {
    let mut h = [0u8; RESERVED_HEADER_LEN as usize];
    h[0..4].copy_from_slice(&(RESERVED_HEADER_LEN as u32).to_be_bytes());
    h[4..8].copy_from_slice(b"free");
    h
}

/// 构造 64 位 mdat 盒头（size == 1 + largesize）。
pub(crate) fn build_mdat_header(total_len: u64) -> [u8; MDAT_HEADER_LEN as usize] {
    let mut h = [0u8; MDAT_HEADER_LEN as usize];
    h[0..4].copy_from_slice(&1u32.to_be_bytes());
    h[4..8].copy_from_slice(b"mdat");
    h[8..16].copy_from_slice(&total_len.to_be_bytes());
    h
}

impl Mp4Muxer {
    /// 创建文件并写入 `ftyp` + `free(16B)`。
    ///
    /// init moov 要等第一个关键帧拿到 SPS/PPS 才能写，所以这里只铺好前两个盒。
    pub fn create(path: &Path, width: u32, height: u32, fps: u32) -> io::Result<Self> {
        let mut file = File::create(path)?;

        let ftyp = build_ftyp();
        let ftyp_len = ftyp.len() as u64;
        file.write_all(&ftyp)?;
        file.write_all(&build_free_placeholder())?;

        Ok(Self {
            file,
            width,
            height,
            fps: fps.max(1),
            ftyp_len,
            samples: Vec::new(),
            chunks: Vec::new(),
            pending: Vec::new(),
            seq: 0,
            sps: Vec::new(),
            pps: Vec::new(),
            header_written: false,
            desc: None,
            finished: false,
        })
    }

    /// 创建文件并**立刻**写 init moov——样本描述由调用方给定，不必等关键帧。
    ///
    /// 硬件编码路径走这条：参数集在 `SetInputMediaType` 阶段就从
    /// `MF_MT_MPEG_SEQUENCE_HEADER` 拿到了，没有「等第一个关键帧」的理由。
    pub fn create_with_description(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        desc: SampleDescription,
    ) -> io::Result<Self> {
        let mut muxer = Self::create(path, width, height, fps)?;
        muxer.desc = Some(desc);
        let init = muxer.build_init_moov()?;
        muxer.file.write_all(&init)?;
        muxer.header_written = true;
        Ok(muxer)
    }

    /// 写入一个**已经是长度前缀**的样本（`stsd` 已由 [`Self::create_with_description`] 定好）。
    pub fn write_prefixed_sample(
        &mut self,
        data: &[u8],
        pts_us: u64,
        is_key: bool,
    ) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::other("muxer已结束，不能继续写样本"));
        }
        if data.is_empty() {
            return Ok(());
        }
        if !self.header_written {
            let init = self.build_init_moov()?;
            self.file.write_all(&init)?;
            self.header_written = true;
        }
        if is_key && !self.pending.is_empty() {
            self.flush_fragment()?;
        }
        self.pending.push(PendingSample {
            data: data.to_vec(),
            pts_us,
            is_key,
        });
        let max_samples = self.max_fragment_samples();
        if self.pending.len() >= max_samples {
            self.flush_fragment()?;
        }
        Ok(())
    }

    /// 写入一个已编码样本（长度前缀 NAL 序列）。`pts_us` 为微秒 PTS。
    ///
    /// 关键帧会闭合上一个分片；`MAX_FRAGMENT_SECONDS` 内仍无关键帧时强制闭合。
    /// 在拿到首个关键帧（含 SPS/PPS）之前，样本会被丢弃——没有参数集就写不出 init moov。
    pub fn write_sample(
        &mut self,
        data: &[u8],
        pts_us: u64,
        is_key: bool,
        sps: &[u8],
        pps: &[u8],
    ) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::other("muxer已结束，不能继续写样本"));
        }
        if self.sps.is_empty() && !sps.is_empty() {
            self.sps = sps.to_vec();
        }
        if self.pps.is_empty() && !pps.is_empty() {
            self.pps = pps.to_vec();
        }

        if !self.header_written {
            if !is_key || self.sps.is_empty() || self.pps.is_empty() {
                // 必须等到关键帧：init moov 的 avcC 需要真实 SPS/PPS。
                return Ok(());
            }
            let init = self.build_init_moov()?;
            self.file.write_all(&init)?;
            self.header_written = true;
        }

        // 新关键帧 → 上一个分片到此为止。
        if is_key && !self.pending.is_empty() {
            self.flush_fragment()?;
        }

        self.pending.push(PendingSample {
            data: data.to_vec(),
            pts_us,
            is_key,
        });

        let max_samples = self.max_fragment_samples();
        if self.pending.len() >= max_samples {
            self.flush_fragment()?;
        }
        Ok(())
    }

    /// 已写入的样本数（统计/测试用）。
    pub fn sample_count(&self) -> usize {
        self.samples.len() + self.pending.len()
    }

    /// 已落盘的分片数（测试用）。
    pub fn fragment_count(&self) -> usize {
        self.chunks.len()
    }

    /// 单个分片的样本数上限。
    fn max_fragment_samples(&self) -> usize {
        (self.fps as usize)
            .saturating_mul(MAX_FRAGMENT_SECONDS as usize)
            .max(1)
    }

    /// 标称帧间隔（微秒），用于分片末样本的 duration 与单样本时的时长。
    fn nominal_interval(&self) -> u32 {
        (1_000_000 / self.fps.max(1) as u64).max(1) as u32
    }

    /// 把当前 pending 样本落成一个 `moof` + `mdat`。
    fn flush_fragment(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if !self.header_written {
            return Err(io::Error::other("init moov 未写入，不能落盘分片"));
        }

        let pending = std::mem::take(&mut self.pending);
        let nominal = self.nominal_interval();

        // trun 的 data_offset 相对 moof 起点（tfhd flags 0x020000 = default-base-is-moof），
        // 而它本身占 4 字节定长 → 可以先量一次 moof 长度再回填。
        let probe = self.build_moof(self.seq, 0, &pending, nominal);
        let moof_len = probe.len() as u64;
        let data_offset_rel = (moof_len + FRAG_MDAT_HEADER_LEN) as i32;
        let moof = self.build_moof(self.seq, data_offset_rel, &pending, nominal);
        debug_assert_eq!(
            moof.len() as u64,
            moof_len,
            "moof 长度不应随 data_offset 变化"
        );

        let moof_offset = self.file.stream_position()?;
        self.file.write_all(&moof)?;

        let data_total: u64 = pending.iter().map(|p| p.data.len() as u64).sum();
        let mdat_size = FRAG_MDAT_HEADER_LEN + data_total;
        if mdat_size > u32::MAX as u64 {
            return Err(io::Error::other(format!(
                "单个分片数据量 {mdat_size} 超过 4 GiB，无法用 32 位 mdat 盒"
            )));
        }
        self.file.write_all(&(mdat_size as u32).to_be_bytes())?;
        self.file.write_all(b"mdat")?;

        let sample_start = self.samples.len();
        for p in &pending {
            self.file.write_all(&p.data)?;
            self.samples.push(SampleRec {
                size: p.data.len() as u32,
                pts_us: p.pts_us,
                is_key: p.is_key,
            });
        }

        self.chunks.push(ChunkRec {
            data_offset: moof_offset + moof_len + FRAG_MDAT_HEADER_LEN,
            sample_count: pending.len(),
        });
        debug_assert_eq!(self.samples.len(), sample_start + pending.len());
        self.seq += 1;
        Ok(())
    }

    /// 结束录制：落盘最后一个分片、末尾追加完整 moov、把开头 free 改写成巨型 mdat。
    ///
    /// 返回最终文件字节数。
    pub fn finish(mut self) -> io::Result<u64> {
        if self.finished {
            return Err(io::Error::other("muxer已结束"));
        }
        self.finished = true;

        self.flush_fragment()?;

        if self.samples.is_empty() {
            self.file.flush()?;
            return Err(io::Error::other("没有样本，无法生成 moov"));
        }

        // 末尾 moov 的起点：它之前的一切（init moov + 所有 moof/mdat）都会被
        // 那个由 free 改写来的巨型 mdat 吞掉。
        let moov_pos = self.file.stream_position()?;
        let moov = self.build_moov()?;
        self.file.write_all(&moov)?;

        // soft remux：整个「隐藏分片结构」的动作就是下面这 16 字节。
        self.file.seek(SeekFrom::Start(self.ftyp_len))?;
        self.file
            .write_all(&build_mdat_header(moov_pos - self.ftyp_len))?;

        self.file.flush()?;
        self.file.sync_all()?;
        Ok(moov_pos + moov.len() as u64)
    }

    // ---------------------------------------------------------------- moov

    /// 录制中的 init moov：只含解码初始化信息 + mvex，不含任何样本表。
    fn build_init_moov(&self) -> io::Result<Vec<u8>> {
        if self.desc.is_none() && (self.sps.is_empty() || self.pps.is_empty()) {
            return Err(io::Error::other("缺少 SPS/PPS（未收到关键帧）"));
        }

        let mut trak = Vec::new();
        trak.extend_from_slice(&build_tkhd(0, self.width, self.height));
        trak.extend_from_slice(&self.build_mdia(0, true)?);
        let trak = build_box(b"trak", &trak);

        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&self.build_mvhd(0));
        moov_payload.extend_from_slice(&trak);
        moov_payload.extend_from_slice(&build_mvex());
        Ok(build_box(b"moov", &moov_payload))
    }

    /// 停止时的完整 moov：普通 MP4 结构，不带 mvex。
    fn build_moov(&self) -> io::Result<Vec<u8>> {
        if self.samples.is_empty() {
            return Err(io::Error::other("没有样本，无法生成 moov"));
        }
        if self.desc.is_none() && (self.sps.is_empty() || self.pps.is_empty()) {
            return Err(io::Error::other("缺少 SPS/PPS（未收到关键帧）"));
        }

        let duration_us = self.samples.last().unwrap().pts_us + self.nominal_interval() as u64;

        let mut trak = Vec::new();
        trak.extend_from_slice(&build_tkhd(duration_us, self.width, self.height));
        trak.extend_from_slice(&self.build_mdia(duration_us, false)?);
        let trak = build_box(b"trak", &trak);

        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&self.build_mvhd(duration_us));
        moov_payload.extend_from_slice(&trak);
        Ok(build_box(b"moov", &moov_payload))
    }

    /// mvhd：timescale = 1000（毫秒）。
    fn build_mvhd(&self, duration_us: u64) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        p.extend_from_slice(&1000u32.to_be_bytes()); // timescale (ms)
        p.extend_from_slice(&(duration_us / 1000).to_be_bytes()); // duration
        p.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        p.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        p.extend_from_slice(&[0u8; 10]); // reserved
        p.extend_from_slice(&UNIT_MATRIX);
        p.extend_from_slice(&[0u8; 24]); // pre_defined
        p.extend_from_slice(&2u32.to_be_bytes()); // next_track_ID
        build_full_box(b"mvhd", 0, 0, &p)
    }

    fn build_mdia(&self, duration_us: u64, init: bool) -> io::Result<Vec<u8>> {
        // mdhd：timescale = 1_000_000（µs）。
        // 必须用 version 1 携带 64 位 duration——v0 的 32 位时长在 µs 时基下
        // 上限只有 4294.97 s（≈71.6 分钟），超过后时长字段回绕、进度条失效。
        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&0u64.to_be_bytes()); // creation_time
        mdhd.extend_from_slice(&0u64.to_be_bytes()); // modification_time
        mdhd.extend_from_slice(&1_000_000u32.to_be_bytes()); // timescale
        mdhd.extend_from_slice(&duration_us.to_be_bytes()); // duration (64-bit)
        mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // language "und"
        mdhd.extend_from_slice(&[0u8; 2]);
        let mdhd = build_full_box(b"mdhd", 1, 0, &mdhd);

        // hdlr：vide
        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0u8; 12]); // reserved
        hdlr.push(0); // name: 空 C 字符串
        let hdlr = build_full_box(b"hdlr", 0, 0, &hdlr);

        // vmhd
        let vmhd = build_full_box(b"vmhd", 0, 1, &[0u8; 8]);

        // dinf → dref → url
        let url = build_full_box(b"url ", 0, 1, &[]);
        let mut dref_payload = Vec::new();
        dref_payload.extend_from_slice(&1u32.to_be_bytes());
        dref_payload.extend_from_slice(&url);
        let dref = build_full_box(b"dref", 0, 0, &dref_payload);
        let dinf = build_box(b"dinf", &dref);

        let stbl = if init {
            self.build_stbl_init()?
        } else {
            self.build_stbl_full()?
        };

        let mut minf_payload = Vec::new();
        minf_payload.extend_from_slice(&vmhd);
        minf_payload.extend_from_slice(&dinf);
        minf_payload.extend_from_slice(&stbl);
        let minf = build_box(b"minf", &minf_payload);

        let mut mdia_payload = Vec::new();
        mdia_payload.extend_from_slice(&mdhd);
        mdia_payload.extend_from_slice(&hdlr);
        mdia_payload.extend_from_slice(&minf);
        Ok(build_box(b"mdia", &mdia_payload))
    }

    /// stsd（avc1 + avcC + colr）：init moov 与完整 moov 都需要。
    fn build_stsd(&self) -> io::Result<Vec<u8>> {
        let entry = match &self.desc {
            Some(d) => build_visual_sample_entry(
                &d.entry,
                &d.label,
                self.width,
                self.height,
                &d.config_boxes,
            ),
            None => {
                let avcc = build_avcc(&self.sps, &self.pps)?;
                build_visual_sample_entry(
                    b"avc1",
                    "MuRecorder H.264",
                    self.width,
                    self.height,
                    &[avcc],
                )
            }
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        payload.extend_from_slice(&entry);
        Ok(build_full_box(b"stsd", 0, 0, &payload))
    }

    /// init moov 的 stbl：只有 stsd，样本表由各 moof 提供。
    fn build_stbl_init(&self) -> io::Result<Vec<u8>> {
        Ok(build_box(b"stbl", &self.build_stsd()?))
    }

    /// 完整 moov 的 stbl：stsd + stts + stss + stsc + stsz + co64。
    fn build_stbl_full(&self) -> io::Result<Vec<u8>> {
        let stsd = self.build_stsd()?;

        // stts：run-length (count, delta)
        let entries = build_stts_entries(&self.samples, self.fps);
        let mut stts_payload = Vec::new();
        stts_payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (count, delta) in &entries {
            stts_payload.extend_from_slice(&count.to_be_bytes());
            stts_payload.extend_from_slice(&delta.to_be_bytes());
        }
        let stts = build_full_box(b"stts", 0, 0, &stts_payload);

        // stss：关键帧样本号（1-based）
        let keys: Vec<u32> = self
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_key)
            .map(|(i, _)| (i + 1) as u32)
            .collect();
        let mut stss_payload = Vec::new();
        stss_payload.extend_from_slice(&(keys.len() as u32).to_be_bytes());
        for k in &keys {
            stss_payload.extend_from_slice(&k.to_be_bytes());
        }
        let stss = build_full_box(b"stss", 0, 0, &stss_payload);

        // stsc：每个分片一个 chunk；相邻分片样本数相同时做 run-length 合并。
        let mut sc_entries: Vec<(u32, u32)> = Vec::new(); // (first_chunk, samples_per_chunk)
        for (i, f) in self.chunks.iter().enumerate() {
            let spc = f.sample_count as u32;
            match sc_entries.last() {
                Some(&(_, last_spc)) if last_spc == spc => {}
                _ => sc_entries.push(((i + 1) as u32, spc)),
            }
        }
        if sc_entries.is_empty() {
            sc_entries.push((1, self.samples.len() as u32));
        }
        let mut stsc_payload = Vec::new();
        stsc_payload.extend_from_slice(&(sc_entries.len() as u32).to_be_bytes());
        for (first_chunk, spc) in &sc_entries {
            stsc_payload.extend_from_slice(&first_chunk.to_be_bytes());
            stsc_payload.extend_from_slice(&spc.to_be_bytes());
            stsc_payload.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
        }
        let stsc = build_full_box(b"stsc", 0, 0, &stsc_payload);

        // stsz：每样本大小
        let mut stsz_payload = Vec::new();
        stsz_payload.extend_from_slice(&0u32.to_be_bytes()); // sample_size=0 → 变长
        stsz_payload.extend_from_slice(&(self.samples.len() as u32).to_be_bytes());
        for s in &self.samples {
            stsz_payload.extend_from_slice(&s.size.to_be_bytes());
        }
        let stsz = build_full_box(b"stsz", 0, 0, &stsz_payload);

        // co64：每分片一个 chunk 偏移（64 位，不受 4 GiB 限制）
        let mut co64_payload = Vec::new();
        co64_payload.extend_from_slice(&(self.chunks.len() as u32).to_be_bytes());
        for f in &self.chunks {
            co64_payload.extend_from_slice(&f.data_offset.to_be_bytes());
        }
        let co64 = build_full_box(b"co64", 0, 0, &co64_payload);

        let mut stbl_payload = Vec::new();
        stbl_payload.extend_from_slice(&stsd);
        stbl_payload.extend_from_slice(&stts);
        stbl_payload.extend_from_slice(&stss);
        stbl_payload.extend_from_slice(&stsc);
        stbl_payload.extend_from_slice(&stsz);
        stbl_payload.extend_from_slice(&co64);
        Ok(build_box(b"stbl", &stbl_payload))
    }

    // ---------------------------------------------------------------- moof

    /// 构建一个 `moof`。
    ///
    /// - `tfhd` flags = `0x020000`（default-base-is-moof）：样本位置相对 moof 起点，
    ///   不依赖绝对文件偏移 —— 这正是「录制中随时切一段」能零修正拷贝的前提。
    /// - `trun` flags = `0x000701`：data-offset / sample-duration / sample-size / sample-flags。
    fn build_moof(
        &self,
        seq: u32,
        data_offset_rel: i32,
        pending: &[PendingSample],
        nominal: u32,
    ) -> Vec<u8> {
        let mfhd = build_full_box(b"mfhd", 0, 0, &seq.to_be_bytes());

        let tfhd = build_full_box(b"tfhd", 0, 0x020000, &1u32.to_be_bytes());

        // tfdt v1：64 位 baseMediaDecodeTime（微秒时基）
        let base_pts = pending.first().map(|p| p.pts_us).unwrap_or(0);
        let tfdt = build_full_box(b"tfdt", 1, 0, &base_pts.to_be_bytes());

        let mut trun_payload = Vec::new();
        trun_payload.extend_from_slice(&(pending.len() as u32).to_be_bytes());
        trun_payload.extend_from_slice(&data_offset_rel.to_be_bytes());
        for (i, p) in pending.iter().enumerate() {
            // 末样本用标称帧间隔：下一个分片的首帧此刻还没到。
            // 误差只影响该样本的显示时长（不累积到时间轴），因为 tfdt 用的都是真实 PTS。
            let duration = match pending.get(i + 1) {
                Some(next) => (next.pts_us.saturating_sub(p.pts_us)).max(1) as u32,
                None => nominal,
            };
            trun_payload.extend_from_slice(&duration.to_be_bytes());
            trun_payload.extend_from_slice(&(p.data.len() as u32).to_be_bytes());
            let flags = if p.is_key {
                FLAG_SYNC_SAMPLE
            } else {
                FLAG_NON_SYNC_SAMPLE
            };
            trun_payload.extend_from_slice(&flags.to_be_bytes());
        }
        let trun = build_full_box(b"trun", 0, 0x000701, &trun_payload);

        let mut traf_payload = Vec::new();
        traf_payload.extend_from_slice(&tfhd);
        traf_payload.extend_from_slice(&tfdt);
        traf_payload.extend_from_slice(&trun);
        let traf = build_box(b"traf", &traf_payload);

        let mut moof_payload = Vec::new();
        moof_payload.extend_from_slice(&mfhd);
        moof_payload.extend_from_slice(&traf);
        build_box(b"moof", &moof_payload)
    }
}

/// 单位矩阵（16.16 定点对角线）。
const UNIT_MATRIX: [u8; 36] = [
    0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0x40, 0x00, 0x00, 0x00,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// ftyp：isom 兼容品牌集。
///
/// 录制中的文件是 fMP4，但 major_brand 仍用 `isom`——成品是普通 MP4，
/// 而且多数解析器并不依靠 brand 判断是否分片（它们扫 top-level 盒）。
fn build_ftyp() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"isom"); // major_brand
    payload.extend_from_slice(&512u32.to_be_bytes()); // minor_version
    payload.extend_from_slice(b"isom");
    payload.extend_from_slice(b"iso2");
    payload.extend_from_slice(b"avc1");
    payload.extend_from_slice(b"mp41");
    build_box(b"ftyp", &payload)
}

/// 普通盒：size(4) + type(4) + payload。
pub(crate) fn build_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

/// full 盒：version(1) + flags(3) + payload。
pub(crate) fn build_full_box(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + payload.len());
    inner.push(version);
    inner.extend_from_slice(&[(flags >> 16) as u8, (flags >> 8) as u8, flags as u8]);
    inner.extend_from_slice(payload);
    build_box(kind, &inner)
}

/// mvex → trex：告知读取方「样本表在各 moof 里，且用上表的默认值」。
fn build_mvex() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    p.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
    p.extend_from_slice(&0u32.to_be_bytes()); // default_sample_duration
    p.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
    p.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
    let trex = build_full_box(b"trex", 0, 0, &p);
    build_box(b"mvex", &trex)
}

/// tkhd：track 头（flags=3：enabled | in movie）。
fn build_tkhd(duration_us: u64, width: u32, height: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    p.extend_from_slice(&0u32.to_be_bytes()); // reserved
    p.extend_from_slice(&(duration_us / 1000).to_be_bytes()); // duration (mvhd timescale)
    p.extend_from_slice(&[0u8; 8]); // reserved
    p.extend_from_slice(&0i16.to_be_bytes()); // layer
    p.extend_from_slice(&0i16.to_be_bytes()); // alternate_group
    p.extend_from_slice(&0i16.to_be_bytes()); // volume
    p.extend_from_slice(&[0u8; 2]); // reserved
    p.extend_from_slice(&UNIT_MATRIX);
    p.extend_from_slice(&(width << 16).to_be_bytes()); // width 16.16
    p.extend_from_slice(&(height << 16).to_be_bytes()); // height 16.16
    build_full_box(b"tkhd", 0, 3, &p)
}

/// avcC：由 SPS/PPS 构建（4 字节长度前缀模式）。
fn build_avcc(sps: &[u8], pps: &[u8]) -> io::Result<Vec<u8>> {
    if sps.len() < 4 || pps.is_empty() {
        return Err(io::Error::other("SPS/PPS 长度不足，无法构建 avcC"));
    }
    let mut p = vec![
        1,      // configurationVersion
        sps[1], // AVCProfileIndication
        sps[2], // profile_compatibility
        sps[3], // AVCLevelIndication
        0xFF,   // 6b reserved(111111) + lengthSizeMinusOne=3 (→ 4 字节长度)
        0xE1,   // 3b reserved(111) + numOfSequenceParameterSets=1
    ];
    p.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    p.extend_from_slice(sps);
    p.push(1); // numOfPictureParameterSets
    p.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    p.extend_from_slice(pps);
    Ok(build_box(b"avcC", &p))
}

/// 通用视觉样本描述项（`avc1` / `hvc1` / `hev1` / `av01` 共用同一套 78 字节头）。
fn build_visual_sample_entry(
    entry: &[u8; 4],
    label: &str,
    width: u32,
    height: u32,
    config_boxes: &[Vec<u8>],
) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&[0u8; 6]); // reserved
    p.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    p.extend_from_slice(&[0u8; 16]); // predefined/reserved
    p.extend_from_slice(&(width as u16).to_be_bytes());
    p.extend_from_slice(&(height as u16).to_be_bytes());
    p.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizresolution 72dpi
    p.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertresolution 72dpi
    p.extend_from_slice(&0u32.to_be_bytes()); // reserved
    p.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    let mut compressor = [0u8; 32];
    let name = label.as_bytes();
    let n = name.len().min(31); // pascal 字符串最长 31 字节
    compressor[0] = n as u8;
    compressor[1..1 + n].copy_from_slice(&name[..n]);
    p.extend_from_slice(&compressor);
    p.extend_from_slice(&0x0018u16.to_be_bytes()); // depth 24bit
    p.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined -1
    for cb in config_boxes {
        p.extend_from_slice(cb);
    }
    // 色彩元数据必须显式写：软编路径的 BGRA→I420 使用 BT.709，
    // 与 GPU 路径 MF 侧标注一致；缺省时播放器只能猜，猜错就是偏色。
    p.extend_from_slice(&build_colr());
    build_box(entry, &p)
}

/// colr（nclx）：BT.709 原色 + BT.709 传输 + BT.709 矩阵 + 有限范围（16–235）。
fn build_colr() -> Vec<u8> {
    const PRIMARIES_BT709: u16 = 1;
    const TRANSFER_BT709: u16 = 1;
    const MATRIX_BT709: u16 = 1;
    let mut p = Vec::with_capacity(11);
    p.extend_from_slice(b"nclx");
    p.extend_from_slice(&PRIMARIES_BT709.to_be_bytes());
    p.extend_from_slice(&TRANSFER_BT709.to_be_bytes());
    p.extend_from_slice(&MATRIX_BT709.to_be_bytes());
    p.push(0); // full_range_flag = 0（limited range）
    build_box(b"colr", &p)
}

/// stts run-length 条目：合并连续相同 delta。
///
/// 首样本 PTS 为 0，其 delta 不能取 `PTS-0`（会是 0 或极小值造成虚假条目），
/// 统一按第二样本的 delta（即实际帧间隔）回填。
fn build_stts_entries(samples: &[SampleRec], fps: u32) -> Vec<(u32, u32)> {
    let nominal = (1_000_000 / fps.max(1) as u64).max(1);
    let mut entries: Vec<(u32, u32)> = Vec::new();
    let mut prev_pts: u64 = 0;
    for (i, s) in samples.iter().enumerate() {
        let delta = if i == 0 {
            // 首样本：取第二样本的 delta；只有一个样本时用标称帧间隔。
            samples
                .get(1)
                .map(|n| (n.pts_us - s.pts_us) as u32)
                .unwrap_or(nominal as u32)
        } else {
            (s.pts_us - prev_pts) as u32
        };
        prev_pts = s.pts_us;
        let delta = delta.max(1);
        match entries.last_mut() {
            Some(last) if last.1 == delta => last.0 += 1,
            _ => entries.push((1, delta)),
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sps_pps() -> (Vec<u8>, Vec<u8>) {
        // 最小可解析 SPS/PPS（内容合法即可，avcC 只做字节拷贝）
        let sps = vec![0x67, 0x42, 0x00, 0x1E, 0xAB];
        let pps = vec![0x68, 0xCE, 0x3C, 0x80];
        (sps, pps)
    }

    /// 30 帧：首帧 IDR，其余 P 帧，CFR；第 15 帧再插一个 IDR → 两个分片。
    fn write_test_file(dir: &std::path::Path) -> (std::path::PathBuf, u64) {
        let path = dir.join("test.mp4");
        let (sps, pps) = make_sps_pps();
        let mut muxer = Mp4Muxer::create(&path, 320, 240, 30).unwrap();

        let frame_interval = 1_000_000 / 30;
        let mut pts = 0u64;
        for i in 0..30u64 {
            let is_key = i == 0 || i == 15;
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
            muxer.write_sample(&data, pts, is_key, &sps, &pps).unwrap();
            pts += frame_interval;
        }
        let size = muxer.finish().unwrap();
        (path, size)
    }

    /// 成品必须恰好是 `ftyp | mdat | moov` —— 分片结构已被那个巨型 mdat 吞掉。
    #[test]
    fn finalized_file_has_plain_mp4_layout() {
        let dir = tempfile::tempdir().unwrap();
        let (path, size) = write_test_file(dir.path());
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(size as usize, bytes.len());

        let types = top_level_box_types(&bytes);
        assert_eq!(
            types,
            vec![b"ftyp".to_vec(), b"mdat".to_vec(), b"moov".to_vec()],
            "成品顶层应只有 ftyp/mdat/moov，实际 {types:?}"
        );
        // 顶层盒序列必须恰好覆盖整个文件，不能有残字节。
        let mut pos = 0usize;
        for t in &types {
            let sz = read_box_size(&bytes, pos);
            assert!(sz >= 8 && pos + sz <= bytes.len(), "盒 {t:?} 长度越界");
            pos += sz;
        }
        assert_eq!(pos, bytes.len(), "顶层盒序列应恰好覆盖整个文件");
    }

    /// 成品里不能有游离的 moof —— 它们必须全部落在 mdat 内部。
    #[test]
    fn no_top_level_moof_in_finalized_file() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_test_file(dir.path());
        let bytes = std::fs::read(&path).unwrap();
        let types = top_level_box_types(&bytes);
        assert!(
            !types.iter().any(|t| t == b"moof"),
            "moof 不应出现在顶层：{types:?}"
        );
    }

    /// 末尾 moov 必须同时含 avcC 与完整样本表，且**不含** mvex（否则读取方会当成分片文件）。
    #[test]
    fn final_moov_is_complete_and_not_fragmented() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_test_file(dir.path());
        let bytes = std::fs::read(&path).unwrap();

        let moov = find_top_level_box(&bytes, b"moov").expect("缺少顶层 moov");
        let end = moov + read_box_size(&bytes, moov);
        let moov_bytes = &bytes[moov..end];

        for kind in [b"avcC", b"avc1", b"stts", b"stsz", b"stsc", b"co64"] {
            assert!(
                moov_bytes.windows(4).any(|w| w == kind),
                "末尾 moov 缺少 {:?}",
                std::str::from_utf8(kind).unwrap()
            );
        }
        assert!(
            !moov_bytes.windows(4).any(|w| w == b"mvex"),
            "末尾 moov 不应带 mvex"
        );
    }

    /// 录制中的文件（未 finish）必须是一个合法 fMP4：ftyp/free/init-moov/moof/mdat 齐全。
    #[test]
    fn recording_file_is_valid_fmp4() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.mp4");
        let (sps, pps) = make_sps_pps();
        let mut muxer = Mp4Muxer::create(&path, 320, 240, 30).unwrap();

        // 3 个分片（每个分片以 IDR 开头）
        let mut pts = 0u64;
        for i in 0..90u64 {
            let is_key = i % 30 == 0;
            let data = [0x41u8; 64];
            muxer.write_sample(&data, pts, is_key, &sps, &pps).unwrap();
            pts += 1_000_000 / 30;
        }
        // 不调用 finish：模拟崩溃/断电瞬间
        let bytes = std::fs::read(&path).unwrap();

        let types = top_level_box_types(&bytes);
        assert_eq!(types[0], b"ftyp");
        assert_eq!(types[1], b"free", "free 占位必须还在（尚未 soft remux）");
        assert_eq!(types[2], b"moov", "init moov 紧随 free");
        assert_eq!(types[3], b"moof", "首个分片紧随 init moov");
        assert_eq!(types[4], b"mdat", "分片数据紧随 moof");
        // 90 帧 / 每片 30 帧：前两片在 i=30、i=60 遇到关键帧时已落盘，
        // 第三片（i=60..89）仍在内存中 —— 录制中不提前 flush 是正确行为。
        assert_eq!(
            types,
            vec![
                b"ftyp".to_vec(),
                b"free".to_vec(),
                b"moov".to_vec(),
                b"moof".to_vec(),
                b"mdat".to_vec(),
                b"moof".to_vec(),
                b"mdat".to_vec(),
            ],
            "录制中应看到 2 个已落盘分片，实际 {types:?}"
        );

        // init moov 必须带 mvex/trex，否则读取方无法解码分片
        let moov = find_top_level_box(&bytes, b"moov").unwrap();
        let moov_end = moov + read_box_size(&bytes, moov);
        assert!(bytes[moov..moov_end].windows(4).any(|w| w == b"mvex"));
        assert!(bytes[moov..moov_end].windows(4).any(|w| w == b"trex"));
        assert!(
            !bytes[moov..moov_end].windows(4).any(|w| w == b"stts"),
            "init moov 不应含样本表"
        );
    }

    /// mdat 必须使用 size==1 + 64 位 largesize，且长度精确到末尾 moov 之前。
    #[test]
    fn mdat_uses_64bit_largesize() {
        let dir = tempfile::tempdir().unwrap();
        let (path, file_size) = write_test_file(dir.path());
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(file_size as usize, bytes.len());

        let mdat_pos = find_top_level_box(&bytes, b"mdat").expect("缺少 mdat");
        let moov_pos = find_top_level_box(&bytes, b"moov").expect("缺少 moov");
        let size_field = u32::from_be_bytes(bytes[mdat_pos..mdat_pos + 4].try_into().unwrap());
        assert_eq!(size_field, 1, "mdat 应使用 64 位 largesize 形式");
        let largesize = u64::from_be_bytes(bytes[mdat_pos + 8..mdat_pos + 16].try_into().unwrap());
        assert_eq!(
            largesize as usize,
            moov_pos - mdat_pos,
            "largesize 应恰好覆盖到末尾 moov 之前"
        );
    }

    /// mdhd v1 的 64 位时长：2 小时录制必须精确还原（v0 在 71.6 分钟处回绕）。
    #[test]
    fn mdhd_v1_reports_long_duration_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.mp4");
        let (sps, pps) = make_sps_pps();
        let mut muxer = Mp4Muxer::create(&path, 320, 240, 1).unwrap();

        // 1 fps × 7200 样本 = 2 小时；每样本 1 秒。
        let total = 7_200u64;
        for i in 0..total {
            let data = [0x65u8; 32];
            muxer
                .write_sample(&data, i * 1_000_000, i == 0, &sps, &pps)
                .unwrap();
        }
        muxer.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let mdhd = find_in_moov(&bytes, b"mdhd").expect("缺少 mdhd");
        assert_eq!(bytes[mdhd + 8], 1, "mdhd 必须是 version 1");
        let timescale = u32::from_be_bytes(
            bytes[mdhd + 8 + 4 + 16..mdhd + 8 + 4 + 20]
                .try_into()
                .unwrap(),
        );
        assert_eq!(timescale, 1_000_000);
        let duration = u64::from_be_bytes(bytes[mdhd + 8 + 24..mdhd + 8 + 32].try_into().unwrap());
        assert!(
            duration > 7_000 * 1_000_000,
            "2 小时录制的时长应超过 7000 s，实际 {duration} µs"
        );
    }

    /// 关键帧间隔超过 MAX_FRAGMENT_SECONDS 时必须强制切分，否则未落盘样本会无限积压。
    #[test]
    fn long_keyframe_gap_forces_fragment_split() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nokey.mp4");
        let (sps, pps) = make_sps_pps();
        let fps = 10u32;
        let mut muxer = Mp4Muxer::create(&path, 320, 240, fps).unwrap();

        // 只有第 0 帧是关键帧，之后 300 帧全是 P 帧（30 秒无 IDR）。
        for i in 0..300u64 {
            let data = [0x41u8; 16];
            muxer
                .write_sample(&data, i * (1_000_000 / fps as u64), i == 0, &sps, &pps)
                .unwrap();
        }
        let fragmented = muxer.fragment_count();
        // 10 fps × MAX_FRAGMENT_SECONDS(10 s) = 100 样本/片上限 → 300 帧切成 3 片。
        // 若没有强制切分，只有第 0 帧是关键帧，300 帧会全部滞留在内存里。
        assert_eq!(
            fragmented, 3,
            "300 帧在 100 样本/片上限下应切出 3 个分片，实际 {fragmented}"
        );
        muxer.finish().unwrap();
    }

    /// 分片必须一 chunk 一片：co64 条目数 == 分片数，且 stsc 按相同样本数合并。
    #[test]
    fn chunks_are_one_per_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chunks.mp4");
        let (sps, pps) = make_sps_pps();
        let mut muxer = Mp4Muxer::create(&path, 320, 240, 30).unwrap();

        // 3 个分片，每片 30 帧（帧 0/30/60 为 IDR）
        for i in 0..90u64 {
            let data = [0x41u8; 32];
            muxer
                .write_sample(&data, i * (1_000_000 / 30), i % 30 == 0, &sps, &pps)
                .unwrap();
        }
        // 前两片已落盘；第三片（i=60..89）在内存里，要到 finish 才落盘。
        assert_eq!(muxer.fragment_count(), 2);
        muxer.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let co64 = find_in_moov(&bytes, b"co64").expect("缺少 co64");
        let co64_end = co64 + read_box_size(&bytes, co64);
        let entry_count = u32::from_be_bytes(bytes[co64 + 12..co64 + 16].try_into().unwrap());
        assert_eq!(entry_count, 3, "co64 条目数应等于分片数");

        // 每个 chunk 偏移必须落在文件内，且严格递增。
        let mut prev = 0u64;
        for i in 0..entry_count as usize {
            let off = u64::from_be_bytes(
                bytes[co64 + 16 + i * 8..co64 + 24 + i * 8]
                    .try_into()
                    .unwrap(),
            );
            assert!(off > prev, "chunk 偏移应严格递增");
            assert!((off as usize) < bytes.len(), "chunk 偏移越界");
            prev = off;
        }
        assert_eq!(co64_end, co64 + 16 + entry_count as usize * 8);

        // 三个分片样本数相同 → stsc 应合并成 1 条记录。
        let stsc = find_in_moov(&bytes, b"stsc").expect("缺少 stsc");
        let stsc_entries = u32::from_be_bytes(bytes[stsc + 12..stsc + 16].try_into().unwrap());
        assert_eq!(
            stsc_entries, 1,
            "相同 samples_per_chunk 的分片应合并为一条 stsc"
        );
        let spc = u32::from_be_bytes(bytes[stsc + 20..stsc + 24].try_into().unwrap());
        assert_eq!(spc, 30, "samples_per_chunk 应为每分片帧数");
    }

    /// 截断到分片边界必须仍是合法 fMP4（崩溃可救的前提）。
    #[test]
    fn truncated_at_fragment_boundary_stays_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trunc.mp4");
        let (sps, pps) = make_sps_pps();
        let mut muxer = Mp4Muxer::create(&path, 320, 240, 30).unwrap();

        for i in 0..120u64 {
            let data = [0x41u8; 48];
            muxer
                .write_sample(&data, i * (1_000_000 / 30), i % 30 == 0, &sps, &pps)
                .unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();

        // 找到前两个分片的结束位置（第 2 个 mdat 结束处）
        let types = top_level_box_types(&bytes);
        assert!(types.len() >= 7, "应有 4 个分片: {types:?}");
        let mut pos = 0usize;
        // 跳过 ftyp / free / init-moov
        for _ in 0..3 {
            pos += read_box_size(&bytes, pos);
        }
        // 两个分片 = moof+mdat ×2
        for _ in 0..4 {
            pos += read_box_size(&bytes, pos);
        }
        let truncated = &bytes[..pos];

        // 截断后顶层盒必须完整闭合
        let mut p = 0usize;
        let mut count = 0;
        while p + 8 <= truncated.len() {
            let sz = read_box_size(truncated, p);
            assert!(sz >= 8, "盒大小非法: {sz}");
            assert!(p + sz <= truncated.len(), "截断处出现半个盒（不可接受）");
            p += sz;
            count += 1;
        }
        assert_eq!(p, truncated.len(), "截断点必须落在盒边界上");
        assert_eq!(count, 7, "应为 ftyp/free/moov + 2×(moof/mdat)");
    }

    /// PTS 抖动不再影响文件布局（原预留方案会因此退化成追加），只需保证盒序合法。
    #[test]
    fn jittery_pts_still_produces_plain_mp4() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jitter.mp4");
        let (sps, pps) = make_sps_pps();
        let fps = 30u32;
        let mut muxer = Mp4Muxer::create(&path, 320, 240, fps).unwrap();

        let nominal = 1_000_000 / fps as u64;
        let mut pts = 0u64;
        for i in 0..5_000u64 {
            // 复现真实采集的抖动：±800 µs 锯齿，任何相邻差值都不同。
            let jitter = (i * 7919 % 1600) as i64 - 800;
            let data = [0x41u8; 64];
            muxer.write_sample(&data, pts, i == 0, &sps, &pps).unwrap();
            pts = (pts as i64 + nominal as i64 + jitter).max(0) as u64;
        }
        muxer.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let types = top_level_box_types(&bytes);
        assert_eq!(types[0], b"ftyp");
        assert_eq!(types[1], b"mdat", "抖动 PTS 下布局不变，实际盒序 {types:?}");
        assert_eq!(*types.last().unwrap(), b"moov");
    }

    /// avc1 内必须带 colr(nclx) 且标注 BT.709 / limited range。
    #[test]
    fn avc1_carries_bt709_colr_box() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_test_file(dir.path());
        let bytes = std::fs::read(&path).unwrap();
        let colr = find_in_moov(&bytes, b"colr").expect("moov 缺少 colr 盒");
        assert_eq!(&bytes[colr + 8..colr + 12], b"nclx");
        let primaries = u16::from_be_bytes(bytes[colr + 12..colr + 14].try_into().unwrap());
        let matrix = u16::from_be_bytes(bytes[colr + 16..colr + 18].try_into().unwrap());
        assert_eq!(primaries, 1, "primaries 应为 BT.709");
        assert_eq!(matrix, 1, "matrix 应为 BT.709");
        assert_eq!(bytes[colr + 18] & 0x80, 0, "应为 limited range");
    }

    /// 递归遍历 moov 盒树，校验每个 full 盒的 version 位置正确
    /// （防「payload 手写 v/f + build_full_box 又写一遍」的双份回归）。
    #[test]
    fn full_box_tree_is_structurally_valid() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_test_file(dir.path());
        let bytes = std::fs::read(&path).unwrap();

        fn walk(bytes: &[u8], full_types: &[(&[u8; 4], u8)]) {
            let mut pos = 0usize;
            while pos + 8 <= bytes.len() {
                let size = read_box_size(bytes, pos);
                assert!(size >= 8, "盒大小非法: {size}");
                if pos + size > bytes.len() {
                    break;
                }
                let kind: [u8; 4] = bytes[pos + 4..pos + 8].try_into().unwrap();
                let body = &bytes[pos + 8..pos + size];
                // 容器盒：递归
                match &kind {
                    b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"dinf" => {
                        walk(body, full_types);
                    }
                    _ => {
                        if let Some((_, expected)) = full_types.iter().find(|(t, _)| *t == &kind) {
                            assert_eq!(body[0], *expected, "full 盒 {kind:?} version 不符");
                        }
                    }
                }
                pos += size;
            }
        }

        // mdhd 必须为 v1（64 位时长），其余 full 盒为 v0。
        let full_types: Vec<(&[u8; 4], u8)> = vec![
            (b"mvhd", 0),
            (b"tkhd", 0),
            (b"mdhd", 1),
            (b"hdlr", 0),
            (b"vmhd", 0),
            (b"dref", 0),
            (b"url ", 0),
            (b"stsd", 0),
            (b"stts", 0),
            (b"stss", 0),
            (b"stsc", 0),
            (b"stsz", 0),
            (b"co64", 0),
        ];
        // 只从顶层 moov 开始走：mdat 里还埋着 init moov，它不是成品的一部分。
        let moov = find_top_level_box(&bytes, b"moov").unwrap();
        let moov_end = moov + read_box_size(&bytes, moov);
        walk(&bytes[moov..moov_end], &full_types);
    }

    /// 逐盒验证 stsd 内的 avc1 宽度/高度与请求一致（16.16 前的 16 位字段）。
    #[test]
    fn avc1_records_requested_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_test_file(dir.path());
        let bytes = std::fs::read(&path).unwrap();
        // 注意：ftyp 的 compatible brands 里也有 "avc1" 字面量，
        // 必须从顶层 moov 盒内开始找，否则定位到品牌区。
        let moov_pos = find_top_level_box(&bytes, b"moov").unwrap();
        let avc1_pos = bytes[moov_pos..]
            .windows(4)
            .position(|w| w == b"avc1")
            .unwrap()
            + moov_pos
            - 4;
        // avc1 payload: 6 reserved + 2 di + 16 pre + 2 w + 2 h
        let w = u16::from_be_bytes([bytes[avc1_pos + 8 + 24], bytes[avc1_pos + 8 + 25]]);
        let h = u16::from_be_bytes([bytes[avc1_pos + 8 + 26], bytes[avc1_pos + 8 + 27]]);
        assert_eq!(w, 320);
        assert_eq!(h, 240);
    }

    /// 顶层盒尺寸（正确处理 size==1 的 64 位 largesize 与 size==0 的「到文件尾」）。
    fn read_box_size(bytes: &[u8], pos: usize) -> usize {
        let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
        match size {
            0 => bytes.len() - pos,
            1 => u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize,
            n => n as usize,
        }
    }

    fn top_level_box_types(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut types = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= bytes.len() {
            types.push(bytes[pos + 4..pos + 8].to_vec());
            let sz = read_box_size(bytes, pos);
            if sz < 8 {
                break;
            }
            pos += sz;
        }
        types
    }

    fn find_top_level_box(bytes: &[u8], kind: &[u8; 4]) -> Option<usize> {
        let mut pos = 0usize;
        while pos + 8 <= bytes.len() {
            if &bytes[pos + 4..pos + 8] == kind {
                return Some(pos);
            }
            let sz = read_box_size(bytes, pos);
            if sz < 8 {
                return None;
            }
            pos += sz;
        }
        None
    }

    /// 在顶层 moov 盒内定位子盒，返回其盒头绝对偏移。
    fn find_in_moov(bytes: &[u8], kind: &[u8; 4]) -> Option<usize> {
        let moov = find_top_level_box(bytes, b"moov")?;
        let moov_size = read_box_size(bytes, moov);
        let end = (moov + moov_size).min(bytes.len());
        let mut pos = moov + 8;
        while pos + 8 <= end {
            let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            if size < 8 {
                break;
            }
            if &bytes[pos + 4..pos + 8] == kind {
                return Some(pos);
            }
            pos += size;
        }
        // 深层的 mdhd 位于 trak/mdia 内，退化为全文（moov 区内）暴力搜索。
        bytes[moov..end]
            .windows(4)
            .position(|w| w == kind)
            .map(|p| p + moov - 4)
    }
}
