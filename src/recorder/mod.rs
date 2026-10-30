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
//!
//! 子模块划分：
//! - [`env`]：线程共享环境（停止/致命错误信号、统计）。
//! - [`messages`]：线程间流转的消息类型。
//! - [`audio`]：音频链路（音源解析、预检、静音补包、loopback 主循环）。
//! - [`gpu`]：GPU 零拷贝录制线程。
//! - [`soft`]：CPU 软编流水线（capture / encode / mux 三条线程）。

mod audio;
mod env;
mod gpu;
mod messages;
mod soft;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::thread::JoinHandle;

use crate::capture::{CaptureOptions, DxgiCapture, dxgi};
use crate::config::{AudioSourceType, EncoderPreference, RecordingConfig};
use crate::encode::aac::AacEncoder;
use crate::encode::mf_sink::{ContainerMode, GpuCodec, GpuSinkWriter};
use crate::error::{RecorderError, RecorderResult};
use crate::stats::{
    ENCODER_GPU_AV1, ENCODER_GPU_H264, ENCODER_GPU_HEVC, ENCODER_SOFT_OPENH264, RecorderStatsFfi,
    StatsSampler,
};

use self::audio::{audio_capture_loop, preflight_audio, resolve_audio_spec};
use self::env::PipelineEnv;
use self::gpu::gpu_recorder_thread;
use self::messages::{MuxMsg, VideoFrame};
use self::soft::{capture_thread, encode_thread, mux_thread};

/// 视频 ring 深度（设计文档 4.1：3–5 帧）。满则丢当前帧，绝不阻塞捕获线程。
const FRAME_RING_DEPTH: usize = 4;
/// 编码包队列深度。encode 线程在此队列上阻塞，形成对磁盘的背压。
const PACKET_QUEUE_DEPTH: usize = 120;
/// 设备丢失重建：每次间隔与最大次数（设计文档 4.3）。
const REBUILD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const MAX_REBUILD_ATTEMPTS: u32 = 10;
/// 通道等待超时（秒级粒度）：停止后用于排空在途帧，运行中用于检测无帧 periods。
const DRAIN_POLL: std::time::Duration = std::time::Duration::from_millis(200);
/// 首帧等待超时：屏幕持续静止时 DXGI 迟迟不交付首帧，给足时间避免过早判负。
const FIRST_FRAME_ACQUIRE_TIMEOUT_MS: u32 = 500;
/// 有帧后的采集超时：短超时让静止画面快速进入「重发副本」分支，维持帧率节奏。
const FRAME_ACQUIRE_TIMEOUT_MS: u32 = 50;
/// 有特效动画进行时的采集超时。
///
/// 50 ms 的超时在静止画面上等于「每个动画帧先白等 50 ms」，33 ms 的帧间隔会被拉成
/// 83 ms —— 350 ms 的点击波纹只剩 4 帧，看起来像闪了一下。动画期间改成 2 ms，
/// 让每一帧都按时重绘；代价只是这期间轮询密一点，且本来就该这么快。
const ANIMATING_ACQUIRE_TIMEOUT_MS: u32 = 2;
/// 首帧兜底：启动后这么久仍无帧（画面静止），改用 GDI 抓一张补首帧。
const FIRST_FRAME_GDI_FALLBACK_MS: u64 = 300;
/// 首帧兜底失败后的重试间隔。
const FIRST_FRAME_GDI_RETRY_MS: u64 = 1000;

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
        // 校验音源配置
        let _ = resolve_audio_spec(&config)?;
        let env = PipelineEnv {
            stop: Arc::new(AtomicBool::new(false)),
            fatal: Arc::new(AtomicBool::new(false)),
            last_error: Arc::new(std::sync::Mutex::new(None)),
            fatal_root: Arc::new(std::sync::Mutex::new(None)),
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
                let options = capture_options(&cfg);
                if let Ok(capture) = DxgiCapture::new(&target, options) {
                    let mut capture_opt = Some(capture);

                    let raw = ContainerMode::RawFragmented;
                    let frag = ContainerMode::Fragmented;
                    let plain = ContainerMode::Plain;
                    let candidates: Vec<(GpuCodec, ContainerMode)> = match cfg.encoder {
                        EncoderPreference::GpuAv1 => {
                            vec![(GpuCodec::Av1, raw), (GpuCodec::Av1, plain)]
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
                            (GpuCodec::Av1, raw),
                            (GpuCodec::Hevc, raw),
                            (GpuCodec::H264, frag),
                            (GpuCodec::Av1, plain),
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
                            if cfg.audio_source != AudioSourceType::None {
                                Some(cfg.audio_bitrate_kbps)
                            } else {
                                None
                            },
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

        let audio_spec = resolve_audio_spec(&cfg)?;

        // ★ 音频链路在 **start 阶段**就探一遍，不要等线程跑起来再失败。
        //
        // 早期实现只在音频线程内部建 `AudioCapture` / `AacEncoder`，失败了走
        // `env.fail`——而 `recorder_start` 早就返回成功了。宿主看到的是「开始录制
        // 成功」，直到几秒后点停止才拿到一个错误，中间还白录了一段没有声音的视频。
        // 录屏软件里这是最糟的交互：用户以为在录。
        //
        // 预检内容：设备能否打开（是否被禁用/独占）+（软编路径）AAC MFT 是否可用。
        // GPU 路径的 AAC 由 SinkWriter 内建，且 `GpuSinkWriter::try_new` 已经把流
        // 加进去了，所以只需要探设备。
        if let Some(spec) = &audio_spec {
            preflight_audio(spec, gpu_session.is_none(), cfg.audio_bitrate_kbps)?;
        }

        if let Some((capture, gpu_sink, enc_type)) = gpu_session {
            self.sampler.set_encoder_type(enc_type);

            let t0 = Arc::new(AtomicU64::new(0));
            let (audio_tx, audio_rx) = if audio_spec.is_some() {
                let (tx, rx) = sync_channel::<(Vec<u8>, u64)>(200);
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            if let Some(spec) = audio_spec {
                let tx = audio_tx.unwrap();
                let env_audio = self.env.clone();
                let t0_audio = Arc::clone(&t0);
                let freq = dxgi::qpc_frequency();
                let handle = std::thread::Builder::new()
                    .name("murec-audio".into())
                    .spawn(move || {
                        // GPU 路径的 AAC 由 SinkWriter 内建，本线程只做采集，
                        // 但 WASAPI 同样需要 COM 套间。
                        let _com = crate::com::ComApartment::init_mta();
                        audio_capture_loop(spec, t0_audio, freq, env_audio, |pcm, pts_us| {
                            if tx.try_send((pcm.to_vec(), pts_us)).is_err() {
                                eprintln!("[murec] 音频：队列溢出，丢弃音频包");
                            }
                            Ok(())
                        });
                    })
                    .map_err(|e| RecorderError::io(format!("audio 线程创建失败: {e}")))?;
                self.threads.push(handle);
            }

            let env = self.env.clone();
            let cfg_clone = cfg.clone();
            let handle = std::thread::Builder::new()
                .name("murec-gpu".into())
                .spawn(move || gpu_recorder_thread(cfg_clone, capture, gpu_sink, t0, audio_rx, env))
                .map_err(|e| RecorderError::io(format!("GPU 录制线程创建失败: {e}")))?;
            self.threads.push(handle);
        } else {
            // Level 3: CPU 软编流水线
            self.sampler.set_encoder_type(ENCODER_SOFT_OPENH264);

            let t0 = Arc::new(AtomicU64::new(0));
            let (frame_tx, frame_rx) = sync_channel::<VideoFrame>(FRAME_RING_DEPTH);
            let (mux_tx, mux_rx) = sync_channel::<MuxMsg>(PACKET_QUEUE_DEPTH);
            let audio_kbps = if audio_spec.is_some() {
                Some(cfg.audio_bitrate_kbps)
            } else {
                None
            };
            let env = self.env.clone();

            // ---- mux 线程（最先起，确保编码线程有消费者）----
            {
                let env = env.clone();
                let path = path.clone();
                let (w, h, fps) = (enc_w, enc_h, cfg.fps);
                let handle = std::thread::Builder::new()
                    .name("murec-mux".into())
                    .spawn(move || mux_thread(path, w, h, fps, audio_kbps, mux_rx, env))
                    .map_err(|e| RecorderError::io(format!("mux 线程创建失败: {e}")))?;
                self.threads.push(handle);
            }

            // ---- encode 线程 ----
            {
                let env = env.clone();
                let (w, h, fps, kbps) = (enc_w, enc_h, cfg.fps, cfg.bitrate_kbps);
                let mux_tx_enc = mux_tx.clone();
                let handle = std::thread::Builder::new()
                    .name("murec-encode".into())
                    .spawn(move || encode_thread(frame_rx, mux_tx_enc, w, h, fps, kbps, env))
                    .map_err(|e| RecorderError::io(format!("encode 线程创建失败: {e}")))?;
                self.threads.push(handle);
            }

            // ---- audio 线程（若启用）----
            if let Some(spec) = audio_spec {
                let env_audio = env.clone();
                let t0_audio = Arc::clone(&t0);
                let freq = dxgi::qpc_frequency();
                let mux_tx_audio = mux_tx.clone();
                let audio_bitrate = cfg.audio_bitrate_kbps;
                let handle = std::thread::Builder::new()
                    .name("murec-audio".into())
                    .spawn(move || {
                        // 套间必须在线程入口拿到，且**早于**任何 COM/MF 调用：
                        // 下面第一件事就是建 AAC 编码器（MFT 属于 COM 服务）。
                        // 这里必须最先声明：Rust 局部变量按声明**逆序**析构，
                        // 只有声明在最前面，它才会在最后析构。
                        let com = crate::com::ComApartment::init_mta();
                        let mut aac_enc = match AacEncoder::with_bitrate_kbps(audio_bitrate) {
                            Ok(enc) => enc,
                            Err(e) => {
                                env_audio.fail(
                                    RecorderResult::NoEncoder,
                                    format!("AAC 编码器创建失败: {e}"),
                                );
                                return;
                            }
                        };

                        audio_capture_loop(spec, t0_audio, freq, env_audio.clone(), |pcm, _| {
                            aac_enc
                                .push_pcm(pcm)
                                .map_err(|e| format!("AAC 喂入失败: {e}"))?;
                            let packets =
                                aac_enc.drain().map_err(|e| format!("AAC 取包失败: {e}"))?;
                            for aac in packets {
                                if mux_tx_audio.send(MuxMsg::Audio(aac)).is_err() {
                                    return Err("mux 队列已关闭".into());
                                }
                            }
                            Ok(())
                        });

                        if let Ok(finish_pkts) = aac_enc.finish() {
                            for aac in finish_pkts {
                                let _ = mux_tx_audio.send(MuxMsg::Audio(aac));
                            }
                        }

                        // ★ 顺序红线：`aac_enc` 持有 IMFTransform（COM 对象），必须在
                        // `com` 之前析构。早期实现把 `CoUninitialize()` 写在采集循环
                        // 末尾（比这里更早），于是 MFT 的 Release 跑在「COM 已关闭」的
                        // 线程上——它不报错，而是直接在进程里触发访问违例，
                        // 表现为「停止录制时随机崩溃且没有任何 Rust panic」。
                        drop(aac_enc);
                        drop(com);
                    })
                    .map_err(|e| RecorderError::io(format!("audio 线程创建失败: {e}")))?;
                self.threads.push(handle);
            }

            drop(mux_tx);

            // ---- capture 线程 ----
            {
                let env = env.clone();
                let t0_cap = Arc::clone(&t0);
                let handle = std::thread::Builder::new()
                    .name("murec-capture".into())
                    .spawn(move || capture_thread(cfg, t0_cap, frame_tx, env))
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
                        std::thread::sleep(std::time::Duration::from_millis(
                            crate::stats::SAMPLE_INTERVAL_MS,
                        ));
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
            if let Some((code, _)) = *self
                .env
                .last_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                self.sampler.set_last_error(code);
            }
        }

        // 流水线致命错误（设备丢失重建耗尽 / 磁盘错误 / 0 样本收尾失败）必须让
        // stop 的调用方知道：否则 UI 会报告"已保存"，用户拿到的是不完整文件。
        // 注意 stop 自身成功但录制中途失败也走这里。
        //
        // 报的是**根因**（首个致命错误）而不是最后一条：级联故障下最后一条往往是
        // 派生结论（「未捕获到任何帧」），会把人引向磁盘/画面静止，而真因在别处。
        if self.env.fatal.load(Ordering::Relaxed) {
            if let Some((code, msg)) = self.env.root_error() {
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
        // 与 `stop()` 一致，优先给**根因**：级联故障下最后一条往往是派生结论，
        // 对排查没有帮助。
        self.env
            .root_error()
            .map(|(_, msg)| msg)
            .unwrap_or_default()
    }
}

/// 把校验后的配置翻译成采集选项。
///
/// 单独抽出来是因为两条采集路径（GPU 零拷贝 / CPU 软编）各建一次 `DxgiCapture`，
/// 两处都必须带上同一组指针与特效开关——分开写很容易改了一处漏了另一处，
/// 现象是「软编写出来的录像没有鼠标」这种很难第一眼归因的差异。
fn capture_options(cfg: &RecordingConfig) -> CaptureOptions {
    CaptureOptions::with_cursor(cfg.capture_cursor)
        .with_effects(cfg.cursor_click_effect, cfg.cursor_trail_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::wasapi::AudioSourceSpec;

    fn test_config(audio: AudioSourceType) -> RecordingConfig {
        RecordingConfig {
            region_x: 0,
            region_y: 0,
            region_w: 640,
            region_h: 480,
            fps: 30,
            encoder: EncoderPreference::Auto,
            bitrate_kbps: 4000,
            capture_cursor: false,
            cursor_click_effect: false,
            cursor_trail_ms: 0,
            output_path: "x.mp4".into(),
            audio_source: audio,
            audio_device_id: None,
            audio_window_hwnd: 0,
            audio_process_id: 0,
            audio_bitrate_kbps: 320,
        }
    }

    #[test]
    fn created_accepts_audio_or_none() {
        assert!(Recorder::created(test_config(AudioSourceType::System)).is_ok());
        assert!(Recorder::created(test_config(AudioSourceType::None)).is_ok());
    }

    /// 音频不可用时 `start()` 必须**立刻**失败，不能等停止时才报。
    ///
    /// 这条用例同时锁住了两件事：软编路径不依赖桌面会话（不需要 DXGI），
    /// 以及音频预检发生在任何线程/文件之前——失败时不留垃圾、不留悬挂线程。
    #[test]
    fn start_fails_fast_when_audio_device_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(AudioSourceType::Device);
        cfg.encoder = EncoderPreference::Soft;
        cfg.output_path = dir.path().join("out.mp4").to_string_lossy().to_string();
        cfg.audio_device_id = Some("{0.0.0.00000000}.{not-a-real-endpoint}".into());

        let mut rec = Recorder::created(cfg).unwrap();
        let err = rec.start().expect_err("音频设备不存在时 start 必须失败");
        assert!(
            err.message().contains("音频不可用"),
            "start 的错误应点明音频不可用，实际：{}",
            err.message()
        );
        // 状态没有推进到 Running，宿主持有的句柄还能正常销毁。
        assert_eq!(rec.state(), RecorderState::Created);
        // 不该留下 0 字节的输出文件。
        assert!(
            !std::path::Path::new(&rec.config.output_path).exists(),
            "预检失败不应创建输出文件"
        );
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

    /// 音源解析：AUDIO_DEVICE 缺 device_id 必须立刻拒绝，不能等线程起来再失败。
    #[test]
    fn resolve_audio_spec_rejects_device_without_id() {
        let cfg = test_config(AudioSourceType::Device);
        assert!(resolve_audio_spec(&cfg).is_err());
    }

    /// 音源解析：AUDIO_WINDOW 在低版本系统上必须拒绝（进程环回需要 19041+）。
    #[test]
    fn resolve_audio_spec_validates_window_pid() {
        let cfg = test_config(AudioSourceType::Window);
        let resolved = resolve_audio_spec(&cfg);
        // 在 build < 19041 的系统上必须报「系统版本」错误；在 ≥19041 的系统上，
        // pid/hwnd 都没给时必须报「必须指定有效」错误——两种都拒绝，绝不能 Ok。
        match resolved {
            Err(e) => {
                let msg = e.message();
                assert!(
                    msg.contains("系统版本") || msg.contains("必须指定"),
                    "错误信息应指向系统版本或缺失 PID，实际：{msg}"
                );
            }
            Ok(Some(AudioSourceSpec::ProcessTree(pid))) => {
                assert_ne!(pid, 0, "解析出的 PID 不能为 0")
            }
            Ok(other) => panic!("AUDIO_WINDOW 缺 PID/HWND 不应成功解析: {other:?}"),
        }
    }
}
