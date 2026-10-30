//! CPU 软编流水线的三条线程：capture → encode → mux。
//!
//! 通道拓扑（设计文档 4.1）：
//! ```text
//! capture ──(ring[FRAME_RING_DEPTH], 满则丢帧)──▶ encode ──(queue[PACKET_QUEUE_DEPTH],
//!                                                  阻塞背压终点)──▶ mux
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};

use crate::capture::DxgiCapture;
use crate::capture::dxgi;
use crate::config::RecordingConfig;
use crate::encode::VideoEncoder;
use crate::error::RecorderResult;
use crate::mux::mp4::Mp4Muxer;

use super::env::PipelineEnv;
use super::messages::{MuxMsg, PacketMsg, VideoFrame};
use super::{
    ANIMATING_ACQUIRE_TIMEOUT_MS, DRAIN_POLL, FIRST_FRAME_ACQUIRE_TIMEOUT_MS,
    FIRST_FRAME_GDI_FALLBACK_MS, FIRST_FRAME_GDI_RETRY_MS, FRAME_ACQUIRE_TIMEOUT_MS,
    MAX_REBUILD_ATTEMPTS, REBUILD_INTERVAL,
};

/// capture 线程主循环。
pub(super) fn capture_thread(
    cfg: RecordingConfig,
    t0: Arc<AtomicU64>,
    frame_tx: SyncSender<VideoFrame>,
    env: PipelineEnv,
) {
    let target = match dxgi::find_target(cfg.region_x, cfg.region_y, cfg.region_w, cfg.region_h) {
        Ok(t) => t,
        Err(e) => {
            env.fail(
                RecorderResult::Invalid,
                format!("找不到区域所在的显示器输出: {e}"),
            );
            return;
        }
    };

    let options = super::capture_options(&cfg);
    let mut capture = match DxgiCapture::new(&target, options) {
        Ok(c) => c,
        Err(e) => {
            env.fail(
                RecorderResult::NoEncoder,
                format!("DXGI 捕获会话创建失败: {e}"),
            );
            return;
        }
    };

    let start_qpc = dxgi::qpc_now();
    t0.store(start_qpc, Ordering::SeqCst);
    let freq = dxgi::qpc_frequency();
    let interval_ticks = (freq / cfg.fps as u64).max(1);
    let mut next_frame_qpc = start_qpc + interval_ticks;
    let mut rebuild_attempts = 0u32;
    // 最近一帧缓存：静止画面上 DXGI 不再交付新帧，重发副本保证 CFR 输出
    // （与 OBS 的 duplicate-frame 策略一致；静态屏幕录屏否则会产出 0 帧）。
    let mut last_frame: Option<dxgi::CapturedFrame> = None;
    // GDI 兜底帧必须与编码器输入尺寸一致（偶数对齐）。
    let (enc_w, enc_h) = cfg.aligned_size();
    // 首帧等待用较长超时（屏幕可能持续静止）；有帧后改用短超时快速补发副本。
    let mut first_frame_seen = false;
    // 静止画面的首帧兜底（见下方 GDI 补抓）；失败后每秒重试一次。
    let loop_started = std::time::Instant::now();
    let mut next_gdi_attempt =
        loop_started + std::time::Duration::from_millis(FIRST_FRAME_GDI_FALLBACK_MS);

    while !env.stop.load(Ordering::Relaxed) {
        // 帧率配速：到期才采集；未到期精确休眠（避免忙等烧 CPU）。
        dxgi::precise_sleep_until(next_frame_qpc);
        next_frame_qpc = next_frame_qpc.saturating_add(interval_ticks);
        // 落后太多（调试器断点/系统卡顿）时重置节奏，不追帧。
        let now = dxgi::qpc_now();
        if now > next_frame_qpc + interval_ticks {
            next_frame_qpc = now + interval_ticks;
        }

        let qpc = dxgi::qpc_now();
        let acquire_timeout_ms = if !first_frame_seen {
            FIRST_FRAME_ACQUIRE_TIMEOUT_MS
        } else if capture.overlay_animating() {
            ANIMATING_ACQUIRE_TIMEOUT_MS
        } else {
            FRAME_ACQUIRE_TIMEOUT_MS
        };

        // 本周期拿到的「新帧」（DXGI 交付或 GDI 兜底），None 表示本周期画面无变化。
        let mut incoming: Option<dxgi::CapturedFrame> = None;

        match capture.acquire_frame(acquire_timeout_ms, qpc) {
            Ok(Some(frame)) => {
                first_frame_seen = true;
                rebuild_attempts = 0;
                incoming = Some(frame);
            }
            Ok(None) => {
                // 静止画面：DXGI 不交付任何帧是 API 语义。若连首帧都还没拿到，
                // 用 GDI 抓一张补上——否则「录制一块静止区域」（副屏静态文档等）
                // 会整段失败。拿到后立刻回到 DXGI 主导的变化驱动模式。
                if !first_frame_seen && std::time::Instant::now() >= next_gdi_attempt {
                    next_gdi_attempt = std::time::Instant::now()
                        + std::time::Duration::from_millis(FIRST_FRAME_GDI_RETRY_MS);
                    match crate::capture::gdi::grab_region(cfg.region_x, cfg.region_y, enc_w, enc_h)
                    {
                        Ok(frame) => {
                            first_frame_seen = true;
                            incoming = Some(frame);
                        }
                        Err(e) => env.record(
                            RecorderResult::Invalid,
                            format!("静止画面首帧 GDI 兜底抓取失败: {e}"),
                        ),
                    }
                } else if first_frame_seen && capture.overlay_animating() {
                    // 特效还在动：从缓存的纯桌面重画一帧。这时 DXGI 给不出新帧，
                    // 不重画的话波纹就停在它出现的那一帧。
                    incoming = capture.recompose_overlay(qpc);
                }
            }
            Err(e) => {
                if dxgi::is_device_lost(e.code()) {
                    // 锁屏 / UAC 安全桌面 / 会话断开只返回 ACCESS_LOST：重试到用户回来为止，
                    // 不计入致命次数（旧实现 5 秒后判负并终止整段录制）。
                    let resumable = dxgi::is_resumable_loss(e.code());
                    if !resumable {
                        rebuild_attempts += 1;
                    }
                    if rebuild_attempts >= MAX_REBUILD_ATTEMPTS {
                        env.fail(
                            RecorderResult::DeviceLost,
                            format!("DXGI 设备丢失，连续 {MAX_REBUILD_ATTEMPTS} 次重建失败: {e}"),
                        );
                        break;
                    }
                    std::thread::sleep(REBUILD_INTERVAL);
                    if let Err(re) = capture.rebuild() {
                        env.record(
                            RecorderResult::DeviceLost,
                            format!("duplication 重建中（第 {rebuild_attempts} 次）: {re}"),
                        );
                    }
                } else {
                    // 非设备丢失的瞬时错误：记一条但不中断（如偶发 INVALID_CALL）。
                    env.record(RecorderResult::Invalid, format!("捕获帧失败: {e}"));
                    env.stats.set_last_error(RecorderResult::Invalid.code());
                }
            }
        }

        if let Some(frame) = incoming {
            // 缓存最近一帧：静止画面上 DXGI 不再交付新帧，重发副本保证 CFR 输出。
            last_frame = Some(frame.clone());
            let pts_us = frame.qpc.saturating_sub(start_qpc) * 1_000_000 / freq;
            let vf = VideoFrame {
                bgra: frame.bgra,
                stride: frame.stride,
                width: frame.width,
                height: frame.height,
                pts_us,
            };
            env.stats.stats().on_frame_captured();
            // 有界 ring：满则丢当前帧（磁盘/编码慢不得拖垮 DXGI 帧交付）。
            if frame_tx.try_send(vf).is_err() {
                env.stats.stats().on_frame_dropped();
            }
        } else if let Some(prev) = &last_frame {
            // 本周期画面无变化：重发上一帧副本（Arc 引用计数 +1，不拷贝像素），
            // 时间戳取本周期起点，维持恒定帧率节奏；ring 满则丢（优先丢副本）。
            let pts_us = qpc.saturating_sub(start_qpc) * 1_000_000 / freq;
            let vf = VideoFrame {
                bgra: Arc::clone(&prev.bgra),
                stride: prev.stride,
                width: prev.width,
                height: prev.height,
                pts_us,
            };
            if frame_tx.try_send(vf).is_err() {
                env.stats.stats().on_frame_dropped();
            }
        }
    }
}

/// encode 线程主循环：BGRA→I420→H.264→包队列。
pub(super) fn encode_thread(
    frame_rx: Receiver<VideoFrame>,
    mux_tx: SyncSender<MuxMsg>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    env: PipelineEnv,
) {
    let mut encoder = match VideoEncoder::new_soft(width, height, fps, bitrate_kbps) {
        Ok(e) => e,
        Err(e) => {
            env.fail(RecorderResult::NoEncoder, format!("编码器创建失败: {e}"));
            return;
        }
    };

    while let Some(frame) = drain_one(&frame_rx, &env) {
        let i420 = crate::convert::i420::bgra_to_i420(
            &frame.bgra,
            frame.stride,
            frame.width,
            frame.height,
        );
        match encoder.encode(&i420) {
            Ok(packet) => {
                env.stats.stats().on_frame_encoded(packet.data.len() as u64);
                if mux_tx
                    .send(MuxMsg::Video(PacketMsg {
                        data: packet.data,
                        is_key: packet.is_key,
                        sps: packet.sps,
                        pps: packet.pps,
                        pts_us: frame.pts_us,
                    }))
                    .is_err()
                {
                    // mux 线程已退出：不应发生（mux 只在通道关闭后退出），防御性处理。
                    env.record(RecorderResult::Io, "编码包队列已关闭".to_string());
                    break;
                }
            }
            Err(e) => {
                env.fail(RecorderResult::Panic, format!("编码失败: {e}"));
                break;
            }
        }
    }
}

/// 从通道取一帧：优先收消息；通道关闭且已排空返回 None。
/// `stop` 置位后允许短暂排空（已在途的帧），但不无限等待。
fn drain_one(rx: &Receiver<VideoFrame>, env: &PipelineEnv) -> Option<VideoFrame> {
    loop {
        match rx.recv_timeout(DRAIN_POLL) {
            Ok(frame) => return Some(frame),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if env.stop.load(Ordering::Relaxed) {
                    // 停止后尝试最后一次非阻塞排空。
                    return rx.try_recv().ok();
                }
                // 未停止却超时（capture 侧长时间无帧）：继续等。
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return rx.try_recv().ok(),
        }
    }
}

/// mux 线程主循环：流式写多轨 MP4，收尾时生成 faststart moov。
pub(super) fn mux_thread(
    path: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    audio_bitrate_kbps: Option<u32>,
    msg_rx: Receiver<MuxMsg>,
    env: PipelineEnv,
) {
    let mut muxer = match Mp4Muxer::create_multi_with_audio_bitrate(
        &path,
        Some((width, height, fps)),
        audio_bitrate_kbps.map(|_| (2, 48_000)),
        audio_bitrate_kbps.unwrap_or(320) * 1000,
    ) {
        Ok(m) => m,
        Err(e) => {
            env.fail(
                RecorderResult::Io,
                format!("创建输出文件失败（{}）: {e}", path.display()),
            );
            return;
        }
    };

    while let Some(msg) = drain_mux_msg(&msg_rx, &env) {
        match msg {
            MuxMsg::Video(packet) => {
                if let Err(e) = muxer.write_sample(
                    &packet.data,
                    packet.pts_us,
                    packet.is_key,
                    &packet.sps,
                    &packet.pps,
                ) {
                    env.fail(RecorderResult::Io, format!("写入视频样本失败: {e}"));
                    break;
                }
            }
            MuxMsg::Audio(packet) => {
                let pts_ticks = (packet.pts_us as u128 * 48_000 / 1_000_000) as u64;
                if let Err(e) = muxer.write_audio_sample(&packet.data, pts_ticks) {
                    env.fail(RecorderResult::Io, format!("写入音频样本失败: {e}"));
                    break;
                }
            }
        }
    }

    if let Err(e) = muxer.finish() {
        // 0 样本最常见的成因不是磁盘故障，而是「整段录制画面完全无变化」——DXGI 桌面
        // 复制只在画面变化时交付帧（静止画面连首帧都拿不到）。直接透出"封装 MP4 失败"
        // 会让用户去查磁盘/权限。这里按是否拿到过帧给出不同文案。
        let frames = env.stats.stats().frames_captured();
        let msg = if frames == 0 {
            format!("封装 MP4 失败（录制期间未捕获到任何帧，画面可能完全无变化）: {e}")
        } else {
            format!("封装 MP4 失败: {e}")
        };
        let note = preserve_or_remove(&path.to_string_lossy(), frames);
        env.fail(RecorderResult::Io, format!("{msg}{note}"));
    }
}

/// 收尾失败时的素材处置：**绝不因为封装失败就删掉用户录了几十分钟的内容**。
///
/// - 一帧都没有 → 删掉空文件（留着只会误导）。
/// - 有帧 → 改名为 `<name>.mp4.partial` 保留。这类文件缺的是索引（moov），
///   `untrunc` / `ffmpeg` 之类工具通常能把码流救回来。
///
/// 返回追加到错误信息后面的说明（空串表示已删除，无需说明）。
pub(super) fn preserve_or_remove(path: &str, frames: u64) -> String {
    let p = std::path::Path::new(path);
    if frames == 0 {
        let _ = std::fs::remove_file(p);
        return String::new();
    }
    let partial = p.with_extension("mp4.partial");
    match std::fs::rename(p, &partial) {
        Ok(()) => format!(
            "；素材已保留为 {}（缺索引，可用 untrunc / ffmpeg 尝试修复）",
            partial.display()
        ),
        // 改名失败（被占用/权限）时保留原文件，任何情况下都不删。
        Err(e) => format!("；素材保留在原路径（改名失败: {e}）"),
    }
}

fn drain_mux_msg(rx: &Receiver<MuxMsg>, env: &PipelineEnv) -> Option<MuxMsg> {
    loop {
        match rx.recv_timeout(DRAIN_POLL) {
            Ok(p) => return Some(p),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if env.stop.load(Ordering::Relaxed) {
                    return rx.try_recv().ok();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return rx.try_recv().ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::aac::AacPacket;
    use crate::stats::{ENCODER_SOFT_OPENH264, StatsSampler};
    use std::sync::Mutex;

    use super::super::{FRAME_RING_DEPTH, PACKET_QUEUE_DEPTH};

    /// 编码 → 封装 的真实流水线测试：合成 BGRA 帧驱动 `encode_thread` + `mux_thread`
    /// （与生产运行时完全相同的两个线程函数），校验落盘的 mp4 结构。
    #[test]
    fn pipeline_encodes_synthetic_frames_into_valid_mp4() {
        const W: u32 = 320;
        const H: u32 = 240;
        const FPS: u32 = 15;
        const KBPS: u32 = 800;
        const FRAMES: u32 = 12;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipeline.mp4");

        let env = PipelineEnv {
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fatal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            fatal_root: Arc::new(Mutex::new(None)),
            stats: Arc::new(StatsSampler::new(ENCODER_SOFT_OPENH264)),
        };

        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<VideoFrame>(FRAME_RING_DEPTH);
        let (packet_tx, packet_rx) = std::sync::mpsc::sync_channel::<MuxMsg>(PACKET_QUEUE_DEPTH);

        let encode_env = env.clone();
        let encode = std::thread::spawn(move || {
            encode_thread(frame_rx, packet_tx, W, H, FPS, KBPS, encode_env)
        });
        let mux_env = env.clone();
        let mux_path = path.clone();
        let mux =
            std::thread::spawn(move || mux_thread(mux_path, W, H, FPS, None, packet_rx, mux_env));

        // 合成帧：每帧一个向右移动的色块，保证帧间有真实残差（不是全静止画面）。
        let stride = W * 4;
        for i in 0..FRAMES {
            let mut bgra = vec![0x20u8; (stride * H) as usize];
            let x0 = (i * 12) as usize;
            for y in 0..H as usize {
                for x in x0..(x0 + 24).min(W as usize) {
                    let p = y * stride as usize + x * 4;
                    bgra[p] = 0x30; // B
                    bgra[p + 1] = 0x80; // G
                    bgra[p + 2] = 0xE0; // R
                    bgra[p + 3] = 0xFF; // A
                }
            }
            frame_tx
                .send(VideoFrame {
                    bgra: bgra.into(),
                    stride,
                    width: W,
                    height: H,
                    pts_us: i as u64 * 1_000_000 / FPS as u64,
                })
                .unwrap();
        }
        drop(frame_tx); // 关闭通道 → encode 排空后退出 → 关闭包通道 → mux 收尾

        encode.join().unwrap();
        mux.join().unwrap();

        assert!(
            !env.fatal.load(Ordering::Relaxed),
            "流水线报致命错误: {:?}",
            env.last_error.lock().unwrap()
        );

        // 计数器：注入的帧全部完成编码（frames_captured 由捕获线程累加，
        // 本测试直接注入帧、不经捕获线程，故此处为 0）。
        env.stats.sample();
        let snap = env.stats.snapshot();
        assert_eq!(snap.frames_encoded, FRAMES as u64, "注入帧应全部编码");

        let bytes = std::fs::read(&path).expect("产物文件必须存在");
        assert!(bytes.len() > 1024, "文件过小: {} 字节", bytes.len());

        // Hybrid MP4 成品布局：ftyp → mdat（巨型，把 init moov 与所有 moof 吞掉）→ moov。
        // mdat 用 size==1 + 64 位 largesize，解析时必须按该形式取长度。
        let mut types: Vec<(Vec<u8>, usize)> = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= bytes.len() {
            let size =
                u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
            let size = match size {
                0 => bytes.len() - pos,
                1 => u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize,
                n => n as usize,
            };
            assert!(size >= 8, "盒大小非法: {size} @ {pos}");
            types.push((bytes[pos + 4..pos + 8].to_vec(), pos));
            pos += size;
        }
        assert_eq!(pos, bytes.len(), "顶层盒序列必须恰好覆盖整个文件");
        let kinds: Vec<&[u8]> = types.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(
            kinds,
            vec![b"ftyp".as_slice(), b"mdat".as_slice(), b"moov".as_slice()],
            "Hybrid 成品顶层应只有 ftyp/mdat/moov，实际 {kinds:?}"
        );

        // moov 内必须有 avc1/avcC，且 stsz 样本数等于喂进去的帧数。
        // 注意：mdat 里还埋着 init moov，不能用字节搜索定位"最后一个 moov"。
        let moov_start = types.last().unwrap().1;
        let moov = &bytes[moov_start..];
        assert!(moov.windows(4).any(|w| w == b"avc1"), "moov 缺少 avc1");
        assert!(moov.windows(4).any(|w| w == b"avcC"), "moov 缺少 avcC");

        let stsz = bytes.windows(4).position(|w| w == b"stsz").unwrap() - 4;
        let sample_count = u32::from_be_bytes([
            bytes[stsz + 16],
            bytes[stsz + 17],
            bytes[stsz + 18],
            bytes[stsz + 19],
        ]);
        assert_eq!(sample_count, FRAMES, "stsz 样本数应等于送入的帧数");
    }

    #[test]
    fn pipeline_encodes_synthetic_video_and_audio_into_dual_track_mp4() {
        const W: u32 = 320;
        const H: u32 = 240;
        const FPS: u32 = 15;
        const KBPS: u32 = 800;
        const FRAMES: u32 = 15; // 1 秒视频

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dual_track.mp4");

        let env = PipelineEnv {
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fatal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            fatal_root: Arc::new(Mutex::new(None)),
            stats: Arc::new(StatsSampler::new(ENCODER_SOFT_OPENH264)),
        };

        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<VideoFrame>(FRAME_RING_DEPTH);
        let (mux_tx, mux_rx) = std::sync::mpsc::sync_channel::<MuxMsg>(PACKET_QUEUE_DEPTH);

        let encode_env = env.clone();
        let encode_mux_tx = mux_tx.clone();
        let encode = std::thread::spawn(move || {
            encode_thread(frame_rx, encode_mux_tx, W, H, FPS, KBPS, encode_env)
        });
        let mux_env = env.clone();
        let mux_path = path.clone();
        let mux =
            std::thread::spawn(move || mux_thread(mux_path, W, H, FPS, Some(320), mux_rx, mux_env));

        let stride = W * 4;
        for i in 0..FRAMES {
            let mut bgra = vec![0x20u8; (stride * H) as usize];
            let x0 = (i * 12) as usize;
            for y in 0..H as usize {
                for x in x0..(x0 + 24).min(W as usize) {
                    let p = y * stride as usize + x * 4;
                    bgra[p] = 0x30;
                    bgra[p + 1] = 0x80;
                    bgra[p + 2] = 0xE0;
                    bgra[p + 3] = 0xFF;
                }
            }
            frame_tx
                .send(VideoFrame {
                    bgra: bgra.into(),
                    stride,
                    width: W,
                    height: H,
                    pts_us: i as u64 * 1_000_000 / FPS as u64,
                })
                .unwrap();
        }
        drop(frame_tx); // 结束视频输入

        // 注入 1 秒的音频 AAC 包（47 包 * 1024 采样 ≈ 1 秒 @ 48kHz）
        for i in 0..47 {
            let pts_us = (i as u64 * 1024 * 1_000_000) / 48_000;
            mux_tx
                .send(MuxMsg::Audio(AacPacket {
                    data: vec![0x21; 120],
                    pts_us,
                }))
                .unwrap();
        }
        drop(mux_tx); // 结束音频输入并关闭 mux 通道

        encode.join().unwrap();
        mux.join().unwrap();

        assert!(
            !env.fatal.load(Ordering::Relaxed),
            "流水线报致命错误: {:?}",
            env.last_error.lock().unwrap()
        );

        let bytes = std::fs::read(&path).expect("产物文件必须存在");
        assert!(bytes.len() > 1024, "文件过小: {} 字节", bytes.len());

        let mut types: Vec<(Vec<u8>, usize)> = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= bytes.len() {
            let size =
                u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
            let size = match size {
                0 => bytes.len() - pos,
                1 => u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize,
                n => n as usize,
            };
            assert!(size >= 8, "盒大小非法: {size} @ {pos}");
            types.push((bytes[pos + 4..pos + 8].to_vec(), pos));
            pos += size;
        }
        assert_eq!(pos, bytes.len(), "顶层盒序列必须恰好覆盖整个文件");
        let kinds: Vec<&[u8]> = types.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(
            kinds,
            vec![b"ftyp".as_slice(), b"mdat".as_slice(), b"moov".as_slice()],
            "Hybrid 成品顶层应只有 ftyp/mdat/moov，实际 {kinds:?}"
        );

        let moov_start = types.last().unwrap().1;
        let moov = &bytes[moov_start..];
        assert!(moov.windows(4).any(|w| w == b"avc1"), "moov 缺少 avc1");
        assert!(moov.windows(4).any(|w| w == b"avcC"), "moov 缺少 avcC");
        assert!(moov.windows(4).any(|w| w == b"mp4a"), "moov 缺少 mp4a");
        assert!(moov.windows(4).any(|w| w == b"esds"), "moov 缺少 esds");

        let trak_count = moov.windows(4).filter(|w| *w == b"trak").count();
        assert_eq!(trak_count, 2, "moov 应包含两条 trak（视频和音频）");
    }

    #[test]
    fn pipeline_encodes_synthetic_video_and_audio_with_custom_128kbps_rate() {
        const W: u32 = 320;
        const H: u32 = 240;
        const FPS: u32 = 15;
        const KBPS: u32 = 800;
        const FRAMES: u32 = 5;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dual_track_128k.mp4");

        let env = PipelineEnv {
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fatal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            fatal_root: Arc::new(Mutex::new(None)),
            stats: Arc::new(StatsSampler::new(ENCODER_SOFT_OPENH264)),
        };

        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<VideoFrame>(FRAME_RING_DEPTH);
        let (mux_tx, mux_rx) = std::sync::mpsc::sync_channel::<MuxMsg>(PACKET_QUEUE_DEPTH);

        let encode_env = env.clone();
        let encode_mux_tx = mux_tx.clone();
        let encode = std::thread::spawn(move || {
            encode_thread(frame_rx, encode_mux_tx, W, H, FPS, KBPS, encode_env)
        });
        let mux_env = env.clone();
        let mux_path = path.clone();
        let mux =
            std::thread::spawn(move || mux_thread(mux_path, W, H, FPS, Some(128), mux_rx, mux_env));

        let stride = W * 4;
        for i in 0..FRAMES {
            let bgra = vec![0x20u8; (stride * H) as usize];
            frame_tx
                .send(VideoFrame {
                    bgra: bgra.into(),
                    stride,
                    width: W,
                    height: H,
                    pts_us: i as u64 * 1_000_000 / FPS as u64,
                })
                .unwrap();
        }
        drop(frame_tx);

        for i in 0..15 {
            let pts_us = (i as u64 * 1024 * 1_000_000) / 48_000;
            mux_tx
                .send(MuxMsg::Audio(AacPacket {
                    data: vec![0x21; 80],
                    pts_us,
                }))
                .unwrap();
        }
        drop(mux_tx);

        encode.join().unwrap();
        mux.join().unwrap();

        let bytes = std::fs::read(&path).expect("产物文件必须存在");
        assert!(bytes.len() > 1024);

        let esds_pos = bytes
            .windows(4)
            .position(|w| w == b"esds")
            .expect("moov 缺少 esds");
        let esds_box = &bytes[esds_pos - 4..];
        // maxBitrate 在 esds_box[27..31]
        assert_eq!(
            &esds_box[27..31],
            &128_000u32.to_be_bytes(),
            "esds maxBitrate 应为 128000"
        );
        assert_eq!(
            &esds_box[31..35],
            &128_000u32.to_be_bytes(),
            "esds avgBitrate 应为 128000"
        );
    }
}
