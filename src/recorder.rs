//! 录制器状态机与流水线调度。
//!
//! 状态机（对应设计文档 3.3）：
//! ```text
//! Idle ──create──▶ Created ──start──▶ Running ──stop──▶ Stopped
//! ```
//! `destroy` 在任意非 Running 状态可直接销毁；Running 中 destroy 内部先 stop。
//!
//! 线程模型（4 条固定线程，设计文档 4.1）：
//! - capture：DXGI 采集 + 帧率配速，try_send 进有界 ring（满则丢帧计数）
//! - encode：BGRA→I420→H.264，产出包阻塞进 mux 队列（磁盘慢的背压终点）
//! - mux：流式写 MP4，通道关闭时 `finish()` 生成 faststart moov
//! - stats：200ms 采样原子计数器生成快照
//!
//! 关停顺序：stop 标志位 → capture 退出并 drop frame_tx → encode 排空后退出并
//! drop packet_tx → mux 排空后 finish 退出 → join 全部线程 → 状态转 Stopped。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::capture::{DxgiCapture, dxgi};
use crate::config::{AudioSourceType, EncoderPreference, RecordingConfig};
use crate::encode::VideoEncoder;
use crate::encode::mf_sink::{ContainerMode, GpuCodec, GpuSinkWriter};
use crate::error::{RecorderError, RecorderResult};
use crate::mux::mp4::Mp4Muxer;
use crate::stats::{
    ENCODER_GPU_AV1, ENCODER_GPU_H264, ENCODER_GPU_HEVC, ENCODER_SOFT_OPENH264, RecorderStatsFfi,
    StatsSampler,
};

/// 视频 ring 深度（设计文档 4.1：3–5 帧）。满则丢当前帧，绝不阻塞捕获线程。
const FRAME_RING_DEPTH: usize = 4;
/// 编码包队列深度。encode 线程在此队列上阻塞，形成对磁盘的背压。
const PACKET_QUEUE_DEPTH: usize = 120;
/// 设备丢失重建：每次间隔与最大次数（设计文档 4.3）。
const REBUILD_INTERVAL: Duration = Duration::from_millis(500);
const MAX_REBUILD_ATTEMPTS: u32 = 10;
/// 通道等待超时（秒级粒度）：停止后用于排空在途帧，运行中用于检测无帧 periods。
const DRAIN_POLL: Duration = Duration::from_millis(200);
/// 首帧等待超时：屏幕持续静止时 DXGI 迟迟不交付首帧，给足时间避免过早判负。
const FIRST_FRAME_TIMEOUT_MS: u32 = 500;
/// 有帧后的采集超时：短超时让静止画面快速进入「重发副本」分支，维持帧率节奏。
const FRAME_ACQUIRE_TIMEOUT_MS: u32 = 50;
/// 首帧兜底：启动后这么久仍无帧（画面静止），改用 GDI 抓一张补首帧。
const FIRST_FRAME_GDI_FALLBACK_MS: u64 = 300;
/// 首帧兜底失败后的重试间隔。
const FIRST_FRAME_GDI_RETRY_MS: u64 = 1000;

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

/// 线程间流转的编码包。
struct PacketMsg {
    data: Vec<u8>,
    is_key: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
    pts_us: u64,
}

/// 线程共享环境：停止/致命错误信号 + 错误记录 + 统计。
/// 打包传递避免线程函数参数过多（clippy::too_many_arguments）。
#[derive(Clone)]
pub struct PipelineEnv {
    pub stop: Arc<AtomicBool>,
    pub fatal: Arc<AtomicBool>,
    pub last_error: Arc<Mutex<Option<(i32, String)>>>,
    pub stats: Arc<StatsSampler>,
}

impl PipelineEnv {
    /// 记录一条带错误码的错误信息（线程安全，可重复调用，后写覆盖先写）。
    pub fn record(&self, code: RecorderResult, msg: String) {
        *self.last_error.lock().unwrap() = Some((code.code(), msg));
    }

    /// 置致命错误并请求全流水线停止。
    pub fn fail(&self, code: RecorderResult, msg: String) {
        self.fatal.store(true, Ordering::SeqCst);
        self.stats.set_last_error(code.code());
        self.record(code, msg);
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderState {
    Created,
    Running,
    Stopped,
}

/// 不透明句柄的实际内容。C 侧只持有 `*mut RecorderHandle`。
pub struct Recorder {
    state: RecorderState,
    config: RecordingConfig,
    sampler: Arc<StatsSampler>,
    env: PipelineEnv,
    threads: Vec<JoinHandle<()>>,
}

impl Recorder {
    /// `recorder_create`：只分配、不启动。
    pub fn created(config: RecordingConfig) -> Result<Self, RecorderError> {
        // M1 仅支持无音频；音频能力随 M4 WASAPI 落地开放。
        if config.audio_source != AudioSourceType::None {
            return Err(RecorderError::invalid(
                "M1 版本不支持音频录制（audio_source 必须为 AUDIO_NONE）",
            ));
        }
        let env = PipelineEnv {
            stop: Arc::new(AtomicBool::new(false)),
            fatal: Arc::new(AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            stats: Arc::new(StatsSampler::new(ENCODER_SOFT_OPENH264)),
        };
        Ok(Self {
            state: RecorderState::Created,
            config,
            sampler: Arc::clone(&env.stats),
            env,
            threads: Vec::new(),
        })
    }

    pub fn state(&self) -> RecorderState {
        self.state
    }

    /// `recorder_start`：创建 4 条流水线线程。
    pub fn start(&mut self) -> Result<(), RecorderError> {
        if self.state != RecorderState::Created {
            return Err(RecorderError::state(
                "状态非法：仅 Created 状态允许 start（可能重复启动）",
            ));
        }

        let cfg = self.config.clone();
        let path = PathBuf::from(&cfg.output_path);
        // 采集/编码/封装统一使用偶数对齐后的尺寸（见 RecordingConfig::aligned_size）
        let (enc_w, enc_h) = cfg.aligned_size();

        // 尽早暴露路径问题（目录不存在等），避免线程起来再失败。
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(RecorderError::io(format!(
                    "输出目录不存在：{}",
                    parent.display()
                )));
            }
        }

        // 硬件路径的降级链是「编码器 × 容器模式」的组合：
        //
        // 1. 若配置显式指定 Soft (x264/openh264)，直接走 CPU 软编流水线。
        // 2. 否则按候选表逐个尝试 GPU 硬件编码，全部失败才回退软件编码。
        //
        // 为什么要枚举容器模式：MF **自带**的分片 MP4 sink 只支持 H.264，
        // HEVC/AV1 配 `MFTranscodeContainerType_FMPEG4` 时 `BeginWriting()`
        // 会返回 MF_E_INVALIDMEDIATYPE（详见 `ContainerMode`）。
        // 自研 sink（`RawFragmented`）把「写盒」拿回来之后 HEVC 也能分片了，
        // 所以候选表按「先自研分片、再 MF 分片、最后普通 MP4」排。
        //
        // 顺序即偏好：能分片就分片（崩溃只丢最后一个分片 + 录制中可随时切片）。
        let mut gpu_session: Option<(DxgiCapture, GpuSinkWriter, u32)> = None;

        if cfg.encoder != EncoderPreference::Soft {
            if let Ok(target) =
                dxgi::find_target(cfg.region_x, cfg.region_y, cfg.region_w, cfg.region_h)
            {
                if let Ok(capture) = DxgiCapture::new(&target) {
                    let mut capture_opt = Some(capture);

                    let raw = ContainerMode::RawFragmented;
                    let frag = ContainerMode::Fragmented;
                    let plain = ContainerMode::Plain;
                    let candidates: Vec<(GpuCodec, ContainerMode)> = match cfg.encoder {
                        EncoderPreference::GpuAv1 => {
                            // AV1 的样本不是 Annex B，自研封装还没有 av1C 的支持，
                            // 先走 MF 自带的两条路。
                            vec![(GpuCodec::Av1, frag), (GpuCodec::Av1, plain)]
                        }
                        EncoderPreference::GpuHevc => {
                            vec![(GpuCodec::Hevc, raw), (GpuCodec::Hevc, plain)]
                        }
                        EncoderPreference::GpuH264 => {
                            vec![
                                (GpuCodec::H264, frag),
                                (GpuCodec::H264, raw),
                                (GpuCodec::H264, plain),
                            ]
                        }
                        // Auto / Amf：小体积优先，再考虑兼容性。
                        _ => vec![
                            (GpuCodec::Hevc, raw),
                            (GpuCodec::H264, frag),
                            (GpuCodec::Hevc, plain),
                            (GpuCodec::H264, plain),
                        ],
                    };

                    for (codec, container) in candidates {
                        let cap_ref = capture_opt.as_ref().unwrap();
                        // 每次尝试都会创建/覆盖输出文件，失败后必须清掉，
                        // 免得留下一个 0 字节文件让上层误以为录制成功。
                        let _ = std::fs::remove_file(&cfg.output_path);
                        match GpuSinkWriter::try_new(
                            cap_ref.device(),
                            cap_ref.context(),
                            &cfg.output_path,
                            enc_w,
                            enc_h,
                            cfg.fps,
                            cfg.bitrate_kbps,
                            codec,
                            container,
                        ) {
                            Ok(sink) => {
                                if container == ContainerMode::Plain {
                                    eprintln!(
                                        "[murec] {} 只能以普通 MP4 落盘：崩溃会丢失整场录制，\
                                         录制中也不能切片",
                                        codec.name()
                                    );
                                }
                                let enc_type = match codec {
                                    GpuCodec::H264 => ENCODER_GPU_H264,
                                    GpuCodec::Hevc => ENCODER_GPU_HEVC,
                                    GpuCodec::Av1 => ENCODER_GPU_AV1,
                                };
                                gpu_session = Some((capture_opt.take().unwrap(), sink, enc_type));
                                break;
                            }
                            Err(e) => eprintln!(
                                "[murec] {} + {} 打不开（{e}），继续降级",
                                codec.name(),
                                container.name()
                            ),
                        }
                    }

                    // 硬件编码均不可用，清理可能留下的空文件，降级到软件编码。
                    if gpu_session.is_none() {
                        let _ = std::fs::remove_file(&cfg.output_path);
                    }
                }
            }
        }

        if let Some((capture, gpu_sink, enc_type)) = gpu_session {
            self.sampler.set_encoder_type(enc_type);

            let env = self.env.clone();
            let cfg_clone = cfg.clone();
            let handle = std::thread::Builder::new()
                .name("murec-gpu".into())
                .spawn(move || gpu_recorder_thread(cfg_clone, capture, gpu_sink, env))
                .map_err(|e| RecorderError::io(format!("GPU 录制线程创建失败: {e}")))?;
            self.threads.push(handle);
        } else {
            // Level 3: CPU 软编流水线
            self.sampler.set_encoder_type(ENCODER_SOFT_OPENH264);

            let (frame_tx, frame_rx) = sync_channel::<VideoFrame>(FRAME_RING_DEPTH);
            let (packet_tx, packet_rx) = sync_channel::<PacketMsg>(PACKET_QUEUE_DEPTH);
            let env = self.env.clone();

            // ---- mux 线程（最先起，确保编码线程有消费者）----
            {
                let env = env.clone();
                let path = path.clone();
                let (w, h, fps) = (enc_w, enc_h, cfg.fps);
                let handle = std::thread::Builder::new()
                    .name("murec-mux".into())
                    .spawn(move || mux_thread(path, w, h, fps, packet_rx, env))
                    .map_err(|e| RecorderError::io(format!("mux 线程创建失败: {e}")))?;
                self.threads.push(handle);
            }

            // ---- encode 线程 ----
            {
                let env = env.clone();
                let (w, h, fps, kbps) = (enc_w, enc_h, cfg.fps, cfg.bitrate_kbps);
                let handle = std::thread::Builder::new()
                    .name("murec-encode".into())
                    .spawn(move || encode_thread(frame_rx, packet_tx, w, h, fps, kbps, env))
                    .map_err(|e| RecorderError::io(format!("encode 线程创建失败: {e}")))?;
                self.threads.push(handle);
            }

            // ---- capture 线程 ----
            {
                let env = env.clone();
                let handle = std::thread::Builder::new()
                    .name("murec-capture".into())
                    .spawn(move || capture_thread(cfg, frame_tx, env))
                    .map_err(|e| RecorderError::io(format!("capture 线程创建失败: {e}")))?;
                self.threads.push(handle);
            }
        }

        // ---- stats 线程 ----
        {
            let sampler = Arc::clone(&self.sampler);
            let stop = Arc::clone(&self.env.stop);
            let handle = std::thread::Builder::new()
                .name("murec-stats".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(crate::stats::SAMPLE_INTERVAL_MS));
                        sampler.sample();
                    }
                    sampler.sample(); // 收尾一次，保证 elapsed/fps 落地
                })
                .map_err(|e| RecorderError::io(format!("stats 线程创建失败: {e}")))?;
            self.threads.push(handle);
        }

        self.state = RecorderState::Running;
        Ok(())
    }

    /// `recorder_stop`：幂等；置停止位并 join 全部线程（阻塞到文件落定）。
    pub fn stop(&mut self) -> Result<(), RecorderError> {
        if self.state == RecorderState::Stopped {
            return Ok(());
        }
        if self.state != RecorderState::Running {
            return Err(RecorderError::state("状态非法：仅 Running 状态允许 stop"));
        }

        self.env.stop.store(true, Ordering::SeqCst);

        // 先取出句柄再 join，避免与 self.env 的 &self 借用冲突。
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut self.threads);
        for handle in handles {
            let name = handle.thread().name().unwrap_or("?").to_string();
            if let Err(e) = handle.join() {
                // 线程 panic 已被 catch 在函数内部转化为 last_error；这里仅记录。
                let msg = if let Some(s) = e.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "未知 panic".to_string()
                };
                self.env.record(
                    RecorderResult::Panic,
                    format!("线程 {name} 异常退出: {msg}"),
                );
            }
        }

        self.state = RecorderState::Stopped;

        // 把线程记录的最后一个错误透传到统计快照，供 UI 拉取时发现。
        if self.env.fatal.load(Ordering::Relaxed) {
            if let Some((code, _)) = *self.env.last_error.lock().unwrap() {
                self.sampler.set_last_error(code);
            }
        }

        // 流水线致命错误（设备丢失重建耗尽 / 磁盘错误 / 0 样本收尾失败）必须让
        // stop 的调用方知道：否则 UI 会报告"已保存"，用户拿到的是不完整文件。
        // 注意 stop 自身成功但录制中途失败也走这里。
        if self.env.fatal.load(Ordering::Relaxed) {
            if let Some((code, msg)) = self.env.last_error.lock().unwrap().clone() {
                return Err(RecorderError::new(RecorderResult::from_code(code), msg));
            }
        }
        Ok(())
    }

    /// `recorder_get_stats` 使用的快照。
    pub fn stats_snapshot(&self) -> RecorderStatsFfi {
        let mut snap = self.sampler.snapshot();
        // is-recording 语义由 C# 层结合状态机派生；这里只保证 last_error 一致。
        if !self.env.fatal.load(Ordering::Relaxed) && self.state == RecorderState::Running {
            snap.last_error = 0;
        }
        snap
    }

    /// `recorder_get_last_error`：拷贝最近一次错误信息（无错误时空串）。
    pub fn last_error_string(&self) -> String {
        self.env
            .last_error
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, msg)| msg.clone())
            .unwrap_or_default()
    }
}

/// GPU 零拷贝直接录制线程（DXGI 纹理采集 + 显存裁剪 + MF 硬件编码 + MP4封装在单线程中闭环）。
fn gpu_recorder_thread(
    cfg: RecordingConfig,
    mut capture: DxgiCapture,
    mut gpu_sink: GpuSinkWriter,
    env: PipelineEnv,
) {
    let _ = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
    };

    let start_qpc = dxgi::qpc_now();
    let freq = dxgi::qpc_frequency();
    let interval_ticks = (freq / cfg.fps as u64).max(1);
    let mut next_frame_qpc = start_qpc + interval_ticks;
    let mut rebuild_attempts = 0u32;
    let mut first_frame_seen = false;
    let loop_started = std::time::Instant::now();
    let mut next_gdi_attempt =
        loop_started + std::time::Duration::from_millis(FIRST_FRAME_GDI_FALLBACK_MS);

    let crop_x = capture.offset_x().max(0) as u32;
    let crop_y = capture.offset_y().max(0) as u32;
    // GDI 兜底帧必须与编码器/目标纹理同尺寸（偶数对齐），否则 UpdateSubresource 会错位。
    let (enc_w, enc_h) = cfg.aligned_size();

    // 估算每帧平均字节数，用于统计瞬时码率
    let approx_bytes_per_frame = ((cfg.bitrate_kbps as u64 * 1000) / 8) / (cfg.fps.max(1) as u64);

    while !env.stop.load(Ordering::Relaxed) {
        dxgi::precise_sleep_until(next_frame_qpc);
        next_frame_qpc = next_frame_qpc.saturating_add(interval_ticks);
        let now = dxgi::qpc_now();
        if now > next_frame_qpc + interval_ticks {
            next_frame_qpc = now + interval_ticks;
        }

        let acquire_timeout_ms = if first_frame_seen {
            FRAME_ACQUIRE_TIMEOUT_MS
        } else {
            FIRST_FRAME_TIMEOUT_MS
        };

        // 真实采集时刻（QPC 微秒）。GPU 路径的样本时间戳必须用它，
        // 不能用帧序号 × 帧间隔——后者会在跟不上时把时间轴压扁。
        let pts_us = now.saturating_sub(start_qpc) * 1_000_000 / freq;

        // 尝试零拷贝采集显存纹理并直接送入 GPU 编码器
        let acquire_res = capture.acquire_texture(acquire_timeout_ms, |src_tex| {
            gpu_sink.push_frame(src_tex, crop_x, crop_y, pts_us)
        });

        match acquire_res {
            Ok(Some(())) => {
                first_frame_seen = true;
                rebuild_attempts = 0;
                env.stats.stats().on_frame_captured();
                env.stats.stats().on_frame_encoded(approx_bytes_per_frame);
            }
            Ok(None) => {
                // 静止画面
                if !first_frame_seen {
                    // 首帧尚未就绪，尝试 GDI 兜底抓一张注入显存
                    if std::time::Instant::now() >= next_gdi_attempt {
                        next_gdi_attempt = std::time::Instant::now()
                            + std::time::Duration::from_millis(FIRST_FRAME_GDI_RETRY_MS);
                        match crate::capture::gdi::grab_region(
                            cfg.region_x,
                            cfg.region_y,
                            enc_w,
                            enc_h,
                        ) {
                            Ok(frame) => {
                                if let Ok(()) =
                                    gpu_sink.push_raw_bgra(&frame.bgra, frame.stride, pts_us)
                                {
                                    first_frame_seen = true;
                                    env.stats.stats().on_frame_captured();
                                    env.stats.stats().on_frame_encoded(approx_bytes_per_frame);
                                }
                            }
                            Err(e) => env.record(
                                RecorderResult::Invalid,
                                format!("GPU 录制首帧 GDI 兜底抓取失败: {e}"),
                            ),
                        }
                    }
                } else {
                    // 已有首帧，静止画面复用上一帧显存内容送入编码器维持 CFR
                    if let Ok(true) = gpu_sink.push_duplicate_frame(pts_us) {
                        env.stats.stats().on_frame_captured();
                        env.stats.stats().on_frame_encoded(approx_bytes_per_frame);
                    }
                }
            }
            Err(e) => {
                if dxgi::is_device_lost(e.code()) {
                    // 锁屏 / UAC 安全桌面 / 会话断开只返回 ACCESS_LOST：画面不可见属正常
                    // 现象，重试到用户回来为止，不计入致命次数——否则离开座位一次就丢整段录像。
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
                    // 暂停期间用最后一帧占位，避免成片出现突兀的时间跳跃。
                    if first_frame_seen || resumable {
                        if let Ok(true) = gpu_sink.push_duplicate_frame(pts_us) {
                            env.stats.stats().on_frame_captured();
                            env.stats.stats().on_frame_encoded(approx_bytes_per_frame);
                        }
                    }
                } else {
                    env.record(RecorderResult::Invalid, format!("捕获帧失败: {e}"));
                    env.stats.set_last_error(RecorderResult::Invalid.code());
                }
            }
        }
    }

    // 录制结束，收尾硬件编码器并写入 MP4 索引头
    if let Err(e) = gpu_sink.finish() {
        let frames = env.stats.stats().frames_captured();
        let msg = if frames == 0 {
            format!("GPU 录制封装 MP4 失败（未捕获到任何帧，画面可能完全无变化）: {e}")
        } else {
            format!("GPU 录制封装 MP4 失败: {e}")
        };
        let note = preserve_or_remove(&cfg.output_path, frames);
        env.fail(RecorderResult::Io, format!("{msg}{note}"));
    }

    unsafe { windows::Win32::System::Com::CoUninitialize() };
}

/// capture 线程主循环。
fn capture_thread(cfg: RecordingConfig, frame_tx: SyncSender<VideoFrame>, env: PipelineEnv) {
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

    let mut capture = match DxgiCapture::new(&target) {
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
        let acquire_timeout_ms = if first_frame_seen {
            FRAME_ACQUIRE_TIMEOUT_MS
        } else {
            FIRST_FRAME_TIMEOUT_MS
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
fn encode_thread(
    frame_rx: Receiver<VideoFrame>,
    packet_tx: SyncSender<PacketMsg>,
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
                if packet_tx
                    .send(PacketMsg {
                        data: packet.data,
                        is_key: packet.is_key,
                        sps: packet.sps,
                        pps: packet.pps,
                        pts_us: frame.pts_us,
                    })
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

/// mux 线程主循环：流式写 MP4，收尾时生成 faststart moov。
fn mux_thread(
    path: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    packet_rx: Receiver<PacketMsg>,
    env: PipelineEnv,
) {
    let mut muxer = match Mp4Muxer::create(&path, width, height, fps) {
        Ok(m) => m,
        Err(e) => {
            env.fail(
                RecorderResult::Io,
                format!("创建输出文件失败（{}）: {e}", path.display()),
            );
            return;
        }
    };

    while let Some(packet) = drain_packet(&packet_rx, &env) {
        if let Err(e) = muxer.write_sample(
            &packet.data,
            packet.pts_us,
            packet.is_key,
            &packet.sps,
            &packet.pps,
        ) {
            env.fail(RecorderResult::Io, format!("写入样本失败: {e}"));
            break;
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
fn preserve_or_remove(path: &str, frames: u64) -> String {
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

fn drain_packet(rx: &Receiver<PacketMsg>, env: &PipelineEnv) -> Option<PacketMsg> {
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

    fn test_config(audio: AudioSourceType) -> RecordingConfig {
        RecordingConfig {
            region_x: 0,
            region_y: 0,
            region_w: 640,
            region_h: 480,
            fps: 30,
            encoder: crate::config::EncoderPreference::Auto,
            bitrate_kbps: 4000,
            capture_cursor: false,
            output_path: "x.mp4".into(),
            audio_source: audio,
            audio_device_id: None,
            audio_window_hwnd: 0,
            audio_process_id: 0,
        }
    }

    #[test]
    fn created_rejects_audio_in_m1() {
        assert!(Recorder::created(test_config(AudioSourceType::System)).is_err());
        assert!(Recorder::created(test_config(AudioSourceType::None)).is_ok());
    }

    #[test]
    fn double_start_rejected() {
        // 无桌面会话环境下 start 会因找不到输出而失败；
        // 这里只验证状态门：Created 之外的 start 必须返回 State 错误。
        let mut rec = Recorder::created(test_config(AudioSourceType::None)).unwrap();
        // 强行推进状态以验证门（不启动真实线程）。
        rec.state = RecorderState::Running;
        let err = rec.start().unwrap_err();
        assert_eq!(err.code(), RecorderResult::State);
    }

    #[test]
    fn stop_when_not_running_rejected() {
        let mut rec = Recorder::created(test_config(AudioSourceType::None)).unwrap();
        let err = rec.stop().unwrap_err();
        assert_eq!(err.code(), RecorderResult::State);
    }

    #[test]
    fn start_rejects_missing_output_dir() {
        let mut cfg = test_config(AudioSourceType::None);
        cfg.output_path = "Z:\\nonexistent_dir_xyz\\out.mp4".into();
        let mut rec = Recorder::created(cfg).unwrap();
        let err = rec.start().unwrap_err();
        assert_eq!(err.code(), RecorderResult::Io);
    }

    /// 编码 → 封装 的真实流水线测试：合成 BGRA 帧驱动 `encode_thread` + `mux_thread`
    /// （与生产运行时完全相同的两个线程函数），校验落盘的 mp4 结构。
    ///
    /// 为什么不走 DXGI 捕获：桌面复制只在画面变化时交付帧，CI/无人值守环境下桌面
    /// 静止会导致一帧都拿不到，测试结果取决于环境。捕获层由 ffmpeg/浏览器级手工
    /// 验证与 `capture` 模块的单元测试覆盖，这里用合成帧把「真实编码器 + 真实封装器」
    /// 这一段锁死，属于确定性覆盖。
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
            stop: Arc::new(AtomicBool::new(false)),
            fatal: Arc::new(AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            stats: Arc::new(StatsSampler::new(ENCODER_SOFT_OPENH264)),
        };

        let (frame_tx, frame_rx) = sync_channel::<VideoFrame>(FRAME_RING_DEPTH);
        let (packet_tx, packet_rx) = sync_channel::<PacketMsg>(PACKET_QUEUE_DEPTH);

        let encode_env = env.clone();
        let encode = std::thread::spawn(move || {
            encode_thread(frame_rx, packet_tx, W, H, FPS, KBPS, encode_env)
        });
        let mux_env = env.clone();
        let mux_path = path.clone();
        let mux = std::thread::spawn(move || mux_thread(mux_path, W, H, FPS, packet_rx, mux_env));

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
    fn recorder_created_initializes_soft_encoder_type() {
        let rec = Recorder::created(test_config(AudioSourceType::None)).unwrap();
        let snap = rec.stats_snapshot();
        assert_eq!(snap.encoder_type, ENCODER_SOFT_OPENH264);
    }

    #[test]
    fn recorder_soft_preference_configured() {
        let mut cfg = test_config(AudioSourceType::None);
        cfg.encoder = EncoderPreference::Soft;
        let rec = Recorder::created(cfg).unwrap();
        assert_eq!(rec.config.encoder, EncoderPreference::Soft);
    }
}
