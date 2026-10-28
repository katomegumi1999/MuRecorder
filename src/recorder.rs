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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::capture::wasapi::{AudioCapture, AudioSourceSpec};
use crate::capture::{CaptureOptions, DxgiCapture, dxgi};
use crate::config::{AudioSourceType, EncoderPreference, RecordingConfig};
use crate::encode::VideoEncoder;
use crate::encode::aac::{AacEncoder, AacPacket};
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
struct PacketMsg {
    data: Vec<u8>,
    is_key: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
    pts_us: u64,
}

/// 封装线程接收的多轨消息枚举。
enum MuxMsg {
    Video(PacketMsg),
    Audio(AacPacket),
}

#[repr(C)]
struct OsVersionInfoExW {
    dw_os_version_info_size: u32,
    dw_major_version: u32,
    dw_minor_version: u32,
    dw_build_number: u32,
    dw_platform_id: u32,
    sz_csd_version: [u16; 128],
    w_service_pack_major: u16,
    w_service_pack_minor: u16,
    w_suite_mask: u16,
    w_product_type: u8,
    w_reserved: u8,
}

unsafe extern "system" {
    fn RtlGetVersion(lpVersionInformation: *mut OsVersionInfoExW) -> i32;
}

fn windows_build_number() -> u32 {
    let mut info: OsVersionInfoExW = unsafe { std::mem::zeroed() };
    info.dw_os_version_info_size = std::mem::size_of::<OsVersionInfoExW>() as u32;
    if unsafe { RtlGetVersion(&mut info) } == 0 {
        info.dw_build_number
    } else {
        0
    }
}

fn resolve_audio_spec(cfg: &RecordingConfig) -> Result<Option<AudioSourceSpec>, RecorderError> {
    match cfg.audio_source {
        AudioSourceType::None => Ok(None),
        AudioSourceType::System => Ok(Some(AudioSourceSpec::DefaultRender)),
        AudioSourceType::Device => {
            let id = cfg
                .audio_device_id
                .as_ref()
                .ok_or_else(|| RecorderError::invalid("AUDIO_DEVICE 必须指定 audio_device_id"))?;
            Ok(Some(AudioSourceSpec::DeviceId(id.clone())))
        }
        AudioSourceType::Window => {
            if windows_build_number() < 19041 {
                return Err(RecorderError::invalid(
                    "进程环回需要 Windows 10 2004（build 19041）及以上系统版本",
                ));
            }
            let mut pid = cfg.audio_process_id;
            if pid == 0 && cfg.audio_window_hwnd != 0 {
                unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                        windows::Win32::Foundation::HWND(cfg.audio_window_hwnd as *mut _),
                        Some(&mut pid),
                    );
                }
            }
            if pid == 0 {
                return Err(RecorderError::invalid(
                    "AUDIO_WINDOW 必须指定有效的 window_hwnd 或 process_id",
                ));
            }
            Ok(Some(AudioSourceSpec::ProcessTree(pid)))
        }
    }
}

/// 线程共享环境：停止/致命错误信号 + 错误记录 + 统计。
/// 打包传递避免线程函数参数过多（clippy::too_many_arguments）。
#[derive(Clone)]
pub struct PipelineEnv {
    pub stop: Arc<AtomicBool>,
    pub fatal: Arc<AtomicBool>,
    pub last_error: Arc<Mutex<Option<(i32, String)>>>,
    /// **首个**致命错误，只写一次。`last_error` 是「最新一条」，会被后来的
    /// 线程覆盖，而流水线里后面的线程往往是**受害者**：音频线程先挂 → 采集
    /// 线程立刻退出 → mux 线程收尾时发现 0 样本 → 报「未捕获到任何帧」。
    /// 真正的原因（音频初始化失败）就这样被埋掉，用户看到的永远是最后那条
    /// 派生的、且指向错误方向的信息。所以根因单独留一份。
    pub fatal_root: Arc<Mutex<Option<(i32, String)>>>,
    pub stats: Arc<StatsSampler>,
}

impl PipelineEnv {
    /// 记录一条带错误码的错误信息（线程安全，可重复调用，后写覆盖先写）。
    pub fn record(&self, code: RecorderResult, msg: String) {
        *self.last_error.lock().unwrap() = Some((code.code(), msg));
    }

    /// 置致命错误并请求全流水线停止。
    ///
    /// `fatal_root` 只保留第一次调用时的信息（根因）；`last_error` 每次都更新
    /// （给 UI 显示最新状态）。两者在单故障场景下相同，多故障级联时才有区别。
    pub fn fail(&self, code: RecorderResult, msg: String) {
        self.fatal.store(true, Ordering::SeqCst);
        self.stats.set_last_error(code.code());
        {
            let mut root = self.fatal_root.lock().unwrap();
            if root.is_none() {
                *root = Some((code.code(), msg.clone()));
            }
        }
        self.record(code, msg);
        self.stop.store(true, Ordering::SeqCst);
    }

    /// 根因（首个致命错误），没有则回退到最新错误。
    pub fn root_error(&self) -> Option<(i32, String)> {
        if let Some(e) = self.fatal_root.lock().unwrap().clone() {
            return Some(e);
        }
        self.last_error.lock().unwrap().clone()
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
        // 校验音源配置
        let _ = resolve_audio_spec(&config)?;
        let env = PipelineEnv {
            stop: Arc::new(AtomicBool::new(false)),
            fatal: Arc::new(AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            fatal_root: Arc::new(Mutex::new(None)),
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
                            if cfg.audio_source != crate::config::AudioSourceType::None {
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

/// 在 `recorder_start` 阶段预检音频链路，让不可用的情况**立刻**报错。
///
/// 单独起一个线程做这件事，是因为 COM 套间是按线程的，而 `recorder_start` 的
/// 调用方是宿主（多半是 C# 的 UI 线程）。我们既不该假设它初始化过 COM，
/// 更不该在别人的线程上改套间模型——那会污染宿主的 COM 使用方式。
///
/// `need_aac` 为真时还会实例化一次 AAC MFT：这一步能提前抓到「系统缺少 AAC
/// 编码器」这类环境问题，而不是录到一半才发现。
fn preflight_audio(
    spec: &AudioSourceSpec,
    need_aac: bool,
    bitrate_kbps: u32,
) -> Result<(), RecorderError> {
    let spec = spec.clone();
    let probe = std::thread::Builder::new()
        .name("murec-audio-probe".into())
        .spawn(move || -> Result<(), String> {
            let com = crate::com::ComApartment::init_mta();
            if !com.is_usable() {
                return Err("无法在当前线程初始化 COM 套间".to_string());
            }
            // 打开再关闭一次：只验证「设备可用、格式能被接受」，不采任何数据。
            let capture =
                AudioCapture::new(spec).map_err(|e| format!("音频采集设备不可用: {e}"))?;
            // 先放掉采集端再建编码器：两者同时持有时某些驱动会拒绝。
            drop(capture);
            if need_aac {
                let enc = AacEncoder::with_bitrate_kbps(bitrate_kbps)
                    .map_err(|e| format!("系统缺少可用的 AAC 编码器: {e}"))?;
                drop(enc);
            }
            Ok(())
        })
        .map_err(|e| RecorderError::io(format!("音频预检线程创建失败: {e}")))?;

    match probe.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(msg)) => Err(RecorderError::io(format!("音频不可用：{msg}"))),
        Err(_) => Err(RecorderError::io("音频预检线程异常退出")),
    }
}

/// 按墙钟把已产出的采样数追平，差多少补多少静音。
///
/// loopback 在桌面静音时不出包、设备切换时也会空窗，缺的这段必须用**静音样本**
/// 顶上去。不补的话音轨会短一截，音画从此永久错位（播放器不会报错，只是口型对不上）。
///
/// 只写 `produced_frames`，不动 anchor：anchor 是「音频起点在录制时间轴上的位置」，
/// 补静音不改变起点；而 `produced_frames` 是样本表长度的唯一来源，两者不能混。
fn pad_silence<F>(
    wall: &std::time::Instant,
    anchor_us: Option<u64>,
    produced_frames: &mut u64,
    on_pcm: &mut F,
) -> Result<(), String>
where
    F: FnMut(&[u8], u64) -> Result<(), String>,
{
    let Some(base_anchor) = anchor_us else {
        return Ok(());
    };
    let target_frames = wall.elapsed().as_micros() as u64 * 48_000 / 1_000_000;
    // 480 采样 = 10 ms 的容差，避免每轮都补一两帧导致包过碎。
    if target_frames <= *produced_frames + 480 {
        return Ok(());
    }
    // 一次最多补 250 ms：设备卡死很久时也不要一口气灌出几秒静音，
    // 让下一轮继续追，时间轴更平滑。
    let need = ((target_frames - *produced_frames) as u32).min(48_000 / 4);
    let pts_us = base_anchor + (*produced_frames as u128 * 1_000_000 / 48_000) as u64;
    let silent = vec![0u8; need as usize * 4];
    *produced_frames += need as u64;
    on_pcm(&silent, pts_us)
}

/// 通用 WASAPI loopback 采集与静音补包主循环。
///
/// **调用方负责 COM 套间**：本函数不会再调 `CoInitializeEx`。原因是采集循环之前
/// 往往还要在同一线程里建 AAC 编码器（`AacEncoder` 走 MFT，属于 COM 服务），
/// 若把套间初始化埋在循环内部，那些更早的调用就会在「未初始化 COM」的线程上跑——
/// 症状是 `ActivateObject` 返回一个没有任何线索的 `E_FAIL`。初始化必须由线程入口
/// 用 [`crate::com::ComApartment`] 统一持有。
///
/// ## 时基
///
/// PTS 一律由**累计采样数**推算（`anchor + produced * 1e6 / 48000`），QPC 只用来
/// 建 anchor 和定期校正。这样音频时间轴是严格恒速的，不会因为某次 `poll` 迟到而
/// 抖动——视频那边同理（帧号 × 间隔），两轨才对得齐。
fn audio_capture_loop<F>(
    spec: AudioSourceSpec,
    t0: Arc<AtomicU64>,
    freq: u64,
    env: PipelineEnv,
    mut on_pcm: F,
) where
    F: FnMut(&[u8], u64) -> Result<(), String>,
{
    let mut capture = match AudioCapture::new(spec) {
        Ok(c) => c,
        Err(e) => {
            env.fail(
                RecorderResult::Io,
                format!("WASAPI 音频采集初始化失败: {e}"),
            );
            return;
        }
    };

    let mut wall = std::time::Instant::now();
    let mut produced_frames: u64 = 0;
    // `Some` 表示已经起表。**不会**为空等真实数据包而一直挂着（见下）。
    let mut anchor_us: Option<u64> = None;
    let mut last_calib_qpc = 0u64;
    let mut first_real_packet = true;
    let mut rebuild_attempts = 0u32;

    while !env.stop.load(Ordering::Relaxed) {
        let t0_val = t0.load(Ordering::SeqCst);
        if t0_val == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }

        // ★ 起表不能等第一个真实数据包。
        //
        // WASAPI loopback 在**桌面完全静音**时可以直接一个包都不给
        // （音频引擎空闲，`GetNextPacketSize()` 恒为 0）。如果 anchor 只在收到
        // 真实包时才建立，那么静音桌面上：anchor 永远为空 → 补静音永不触发 →
        // 录出来的 MP4 有一条 **0 样本**的音轨。症状极隐蔽：录制正常、停止正常、
        // 没有报错，只是没有声音。
        //
        // 正确的起表点就是录制起点 t0：音频和视频从同一时刻开始，
        // anchor 先置 0，等第一个真实包到达时再按 QPC 校正一次（见下）。
        if anchor_us.is_none() {
            anchor_us = Some(0);
            last_calib_qpc = t0_val;
            wall = std::time::Instant::now();
        }

        match capture.poll() {
            Ok(Some(pkt)) => {
                rebuild_attempts = 0;

                // 真实包时间戳早于录制起点时整包丢弃：那是 loopback 缓冲区里
                // t0 之前就录到的内容，收进来会让音频比视频超前。
                if pkt.qpc != 0 && pkt.qpc < t0_val {
                    continue;
                }

                let base_anchor = anchor_us.unwrap_or(0);
                let pts_us = base_anchor + (produced_frames as u128 * 1_000_000 / 48_000) as u64;

                // 校正 anchor（只动 anchor，绝不动 produced_frames —— 后者是
                // 样本表长度的唯一来源，动它会让 stts 与实际字节数脱节）。
                // 首个真实包必须校正一次：静音期间补出来的静音帧是按墙钟拍的，
                // 而设备时钟可能比墙钟快/慢一点，第一包就是对齐设备时钟的机会。
                let force_calib = first_real_packet && pkt.qpc != 0;
                if pkt.qpc != 0 && (force_calib || pkt.qpc > last_calib_qpc + 30 * freq) {
                    let current_qpc_us = pkt.qpc.saturating_sub(t0_val) * 1_000_000 / freq;
                    let drift_us = current_qpc_us as i64 - pts_us as i64;
                    // 首包门槛放宽到 2 ms 以内；周期性校正沿用 5 ms。
                    let limit = if force_calib { 2_000 } else { 5_000 };
                    if drift_us.abs() > limit {
                        anchor_us = Some((base_anchor as i64 + drift_us).max(0) as u64);
                    }
                    last_calib_qpc = pkt.qpc;
                }
                first_real_packet = false;

                produced_frames += pkt.frames as u64;
                if let Err(e) = on_pcm(&pkt.data, pts_us) {
                    // 打一行 stderr：音频线程静默退出会让 mux 只能报「0 样本」，
                    // 排查时看不到真正的原因。
                    eprintln!("[murec] 音频：下游处理失败，线程退出: {e}");
                    env.record(RecorderResult::Io, e);
                    break;
                }
            }
            Ok(None) => {
                if let Err(e) = pad_silence(&wall, anchor_us, &mut produced_frames, &mut on_pcm) {
                    eprintln!("[murec] 音频：补静音失败，线程退出: {e}");
                    env.record(RecorderResult::Io, e);
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                if e.code() == windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED {
                    rebuild_attempts += 1;
                    if rebuild_attempts > 10 {
                        env.fail(
                            RecorderResult::Io,
                            format!("音频设备失效，连续 10 次重建失败: {e}"),
                        );
                        break;
                    }
                    eprintln!("[murec] 音频：设备失效，第 {rebuild_attempts} 次重建");
                    // ★ 重建期间**必须继续补静音**。
                    //
                    // 切耳机、改默认输出设备、蓝牙重连，都会走到这里，耗时从几十
                    // 毫秒到一两秒不等。早期实现只是 sleep(500ms) 然后 reset，
                    // 这半秒钟一帧音频都不出，音轨就短了一块——而且**不会报错**，
                    // 表现是「录了三分钟，从 1:20 开始音画不同步」。
                    // 这里按 5 ms 一轮持续补静音，直到重建完成，时间轴不断。
                    let deadline = std::time::Instant::now() + Duration::from_millis(500);
                    while std::time::Instant::now() < deadline && !env.stop.load(Ordering::Relaxed)
                    {
                        if let Err(e) =
                            pad_silence(&wall, anchor_us, &mut produced_frames, &mut on_pcm)
                        {
                            eprintln!("[murec] 音频：重建期间补静音失败: {e}");
                            env.record(RecorderResult::Io, e);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    // 重建成功后墙钟基准要跟着走：否则下一轮会把重建期间
                    // 已经补过的静音再补一遍（重复计时间）。
                    match capture.reset() {
                        Ok(()) => {
                            rebuild_attempts = 0;
                            let produced_us = (produced_frames as u128 * 1_000_000 / 48_000) as u64;
                            wall = std::time::Instant::now()
                                - std::time::Duration::from_micros(produced_us);
                        }
                        Err(re) => env.record(
                            RecorderResult::DeviceLost,
                            format!("音频设备重连中（第 {rebuild_attempts} 次）: {re}"),
                        ),
                    }
                } else {
                    env.record(RecorderResult::Invalid, format!("音频采集轮询错误: {e}"));
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
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

/// GPU 零拷贝直接录制线程（DXGI 纹理采集 + 显存裁剪 + MF 硬件编码 + MP4封装在单线程中闭环）。
fn gpu_recorder_thread(
    cfg: RecordingConfig,
    mut capture: DxgiCapture,
    mut gpu_sink: GpuSinkWriter,
    t0: Arc<AtomicU64>,
    audio_rx: Option<Receiver<(Vec<u8>, u64)>>,
    env: PipelineEnv,
) {
    // 套间必须**最先声明**：本函数的形参（`capture` / `gpu_sink`）持有 DXGI 与
    // MF 的 COM 对象，按「逆序析构」规则它们会先于 `com` 释放。若像早期实现那样
    // 在函数末尾显式 `CoUninitialize()`，那些对象就会在 COM 已关闭的线程上 Release，
    // 直接访问违例（没有 Rust panic，进程当场死）。
    let _com = crate::com::ComApartment::init_mta();

    let start_qpc = dxgi::qpc_now();
    t0.store(start_qpc, Ordering::SeqCst);
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
        // 及时清空音频积压并注入 SinkWriter
        if let Some(rx) = &audio_rx {
            while let Ok((pcm, pts_us)) = rx.try_recv() {
                if let Err(e) = gpu_sink.push_audio_pcm(&pcm, pts_us) {
                    env.record(RecorderResult::Io, format!("GPU 写入音频失败: {e}"));
                    break;
                }
            }
        }

        dxgi::precise_sleep_until(next_frame_qpc);
        next_frame_qpc = next_frame_qpc.saturating_add(interval_ticks);
        let now = dxgi::qpc_now();
        if now > next_frame_qpc + interval_ticks {
            next_frame_qpc = now + interval_ticks;
        }

        let acquire_timeout_ms = if !first_frame_seen {
            FIRST_FRAME_TIMEOUT_MS
        } else if capture.overlay_animating() {
            // 特效还在动：别让超时把动画帧的节奏拖成一半。
            ANIMATING_ACQUIRE_TIMEOUT_MS
        } else {
            FRAME_ACQUIRE_TIMEOUT_MS
        };

        // 真实采集时刻（QPC 微秒）。GPU 路径的样本时间戳必须用它，
        // 不能用帧序号 × 帧间隔——后者会在跟不上时把时间轴压扁。
        let pts_us = now.saturating_sub(start_qpc) * 1_000_000 / freq;

        // 零拷贝采集显存纹理并直接送入 GPU 编码器。
        //
        // 闭包拿到 `FrameMeta`：桌面本帧是否真有新像素（「只有鼠标在动」时为 false，
        // 不必再截一次显存）+ 指针状态。指针合成发生在 `push_frame` 内部、
        // BGRA→NV12 之前，所以 H.264 与 HEVC/AV1 用的是同一份像素。
        let acquire_res = capture.acquire_texture(acquire_timeout_ms, |src_tex, meta| {
            gpu_sink.push_frame(src_tex, crop_x, crop_y, pts_us, meta)
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
                    // 首帧尚未就绪，尝试 GDI 兜底抓一张注入显存。
                    // 注意：GDI 这条路拿不到指针形状（`GetFramePointerShape` 要求先持有
                    // DXGI 帧），所以这张兜底首帧没有鼠标——只影响开头一两帧，
                    // 紧随其后的 DXGI 帧就会带上。
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
                    // 已有首帧，静止画面复用上一帧显存内容送入编码器维持 CFR。
                    //
                    // 但特效还在动时不能直接复用：波纹扩散、残影淡出是**时间**驱动
                    // 的动画，桌面与指针都不动也会变。DXGI 这时候不会再交付帧
                    // （`AcquireNextFrame` 已超时），所以只能从存下的纯桌面副本重画一帧。
                    let pushed = if capture.overlay_animating() {
                        match capture.overlay_plan_now() {
                            Some(plan) => gpu_sink.push_overlay_frame(pts_us, &plan),
                            None => gpu_sink.push_duplicate_frame(pts_us),
                        }
                    } else {
                        gpu_sink.push_duplicate_frame(pts_us)
                    };
                    if let Ok(true) = pushed {
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

    // 排空剩余音频
    if let Some(rx) = &audio_rx {
        while let Ok((pcm, pts_us)) = rx.try_recv() {
            let _ = gpu_sink.push_audio_pcm(&pcm, pts_us);
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
    // `com` 在这里（形参之后）才析构，COM 对象才不会悬空。
}

/// capture 线程主循环。
fn capture_thread(
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

    let options = capture_options(&cfg);
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
            FIRST_FRAME_TIMEOUT_MS
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
fn encode_thread(
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
fn mux_thread(
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

    /// 预检必须在下标不存在的端点上报错，且错误信息指向「音频不可用」，
    /// 这样宿主能立刻把「开始录制失败」呈现给用户，而不是录完才发现没声音。
    #[test]
    fn preflight_rejects_unknown_endpoint() {
        let spec = AudioSourceSpec::DeviceId("{0.0.0.00000000}.{not-a-real-endpoint}".into());
        let err = preflight_audio(&spec, false, 320).expect_err("不存在的端点必须预检失败");
        assert!(
            err.message().contains("音频不可用"),
            "错误信息应点明音频不可用，实际：{}",
            err.message()
        );
    }

    /// 静音补包：按墙钟把采样数追平，且 pts 从 anchor 起算。
    #[test]
    fn pad_silence_catches_up_to_wall_clock() {
        let mut produced = 0u64;
        let mut got: Vec<(usize, u64)> = Vec::new();
        let mut sink = |data: &[u8], pts: u64| {
            got.push((data.len() / 4, pts));
            Ok(())
        };

        // 墙钟已经走了 100 ms → 应补约 4800 帧
        let wall = std::time::Instant::now() - Duration::from_millis(100);
        pad_silence(&wall, Some(5_000), &mut produced, &mut sink).unwrap();

        // 不要写死 4800：`wall` 构造完成到 `pad_silence` 真正读时钟之间还会走掉
        // 几十微秒，4801、4802 都是正常值。这里断言「追上 10 ms 容差以内」。
        let target = 4_800u64;
        assert!(
            produced >= target && produced <= target + 480,
            "100 ms 后应补到约 4800 采样（容差 10 ms），实际 {produced}"
        );
        assert_eq!(got.len(), 1, "一次补齐，不要切成很多碎包");
        assert_eq!(got[0].1, 5_000, "pts 应等于 anchor");
    }

    /// 音频不可用时 `start()` 必须**立刻**失败，不能等停止时才报。
    ///
    /// 这条用例同时锁住了两件事：软编路径不依赖桌面会话（不需要 DXGI），
    /// 以及音频预检发生在任何线程/文件之前——失败时不留垃圾、不留悬挂线程。
    #[test]
    fn start_fails_fast_when_audio_device_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(AudioSourceType::Device);
        cfg.encoder = crate::config::EncoderPreference::Soft;
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
    /// 已经追平就不该再补：否则每轮都多吐几帧，音轨会越补越长。
    #[test]
    fn pad_silence_is_idempotent_within_tolerance() {
        let mut produced = 0u64;
        let mut count = 0usize;
        let mut sink = |_d: &[u8], _p: u64| {
            count += 1;
            Ok(())
        };
        let wall = std::time::Instant::now() - Duration::from_millis(100);
        pad_silence(&wall, Some(0), &mut produced, &mut sink).unwrap();
        let after_first = produced;
        // 同一个墙钟再补一次：应完全不动
        pad_silence(&wall, Some(0), &mut produced, &mut sink).unwrap();
        assert_eq!(produced, after_first, "同一时刻重复补静音不应叠加");
        assert_eq!(count, 1, "第二次调用不应产出任何包");
    }

    /// 尚未起表（anchor 为 None）时不补——此时连录制起点都不知道。
    #[test]
    fn pad_silence_noop_without_anchor() {
        let mut produced = 0u64;
        let mut count = 0usize;
        let mut sink = |_d: &[u8], _p: u64| {
            count += 1;
            Ok(())
        };
        let wall = std::time::Instant::now() - Duration::from_millis(1000);
        pad_silence(&wall, None, &mut produced, &mut sink).unwrap();
        assert_eq!(produced, 0);
        assert_eq!(count, 0);
    }

    /// 单次补静音有 250 ms 上限：设备卡死很久时不要一口气灌出几秒静音，
    /// 而是分几轮追上，时间轴更平滑。
    #[test]
    fn pad_silence_caps_single_burst() {
        let mut produced = 0u64;
        let mut sink = |_d: &[u8], _p: u64| Ok(());
        // 墙钟走了 3 秒
        let wall = std::time::Instant::now() - Duration::from_millis(3_000);
        pad_silence(&wall, Some(0), &mut produced, &mut sink).unwrap();
        assert_eq!(produced, 12_000, "单次最多补 250 ms（12000 采样）");
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
            fatal_root: Arc::new(Mutex::new(None)),
            stats: Arc::new(StatsSampler::new(ENCODER_SOFT_OPENH264)),
        };

        let (frame_tx, frame_rx) = sync_channel::<VideoFrame>(FRAME_RING_DEPTH);
        let (packet_tx, packet_rx) = sync_channel::<MuxMsg>(PACKET_QUEUE_DEPTH);

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
            stop: Arc::new(AtomicBool::new(false)),
            fatal: Arc::new(AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            fatal_root: Arc::new(Mutex::new(None)),
            stats: Arc::new(StatsSampler::new(ENCODER_SOFT_OPENH264)),
        };

        let (frame_tx, frame_rx) = sync_channel::<VideoFrame>(FRAME_RING_DEPTH);
        let (mux_tx, mux_rx) = sync_channel::<MuxMsg>(PACKET_QUEUE_DEPTH);

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
            stop: Arc::new(AtomicBool::new(false)),
            fatal: Arc::new(AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            fatal_root: Arc::new(Mutex::new(None)),
            stats: Arc::new(StatsSampler::new(ENCODER_SOFT_OPENH264)),
        };

        let (frame_tx, frame_rx) = sync_channel::<VideoFrame>(FRAME_RING_DEPTH);
        let (mux_tx, mux_rx) = sync_channel::<MuxMsg>(PACKET_QUEUE_DEPTH);

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
