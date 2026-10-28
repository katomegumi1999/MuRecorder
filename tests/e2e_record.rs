//! 端到端集成测试：真的录 2 秒屏幕，再交给 Media Foundation 独立读回。
//!
//! 这是唯一同时覆盖「DXGI 采集 → 编码 → 封装 → 真实解复用器」的用例，因此需要
//! 真实桌面会话与可用的编码器，默认标 `#[ignore]`：
//!
//! ```text
//! cargo test --test e2e_record -- --ignored --nocapture
//! ```
//!
//! 它验证的东西，单元测试做不到：
//! - `mdat` 的 64 位 largesize、`mdhd` v1、`colr` 能被 MF 自带解析器接受；
//! - 样本时间戳来自 QPC，成片时长与墙钟一致（帧序号时间戳会在这里暴露）；
//! - 软编与硬编两条路径产出同构的、可解码的文件。

use murecorder_core::config::{AudioSourceType, EncoderPreference, RecordingConfig};
use murecorder_core::recorder::Recorder;
use std::time::{Duration, Instant};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFSTARTUP_FULL, MFStartup,
};
use windows::core::PCWSTR;

const REGION: (i32, i32, u32, u32) = (100, 100, 320, 240);
const DURATION_MS: u64 = 2_000;

fn recording_config(path: &str, encoder: EncoderPreference) -> RecordingConfig {
    RecordingConfig {
        region_x: REGION.0,
        region_y: REGION.1,
        region_w: REGION.2,
        region_h: REGION.3,
        fps: 30,
        encoder,
        bitrate_kbps: 3000,
        capture_cursor: false,
        output_path: path.to_string(),
        audio_source: AudioSourceType::None,
        audio_device_id: None,
        audio_window_hwnd: 0,
        audio_process_id: 0,
        audio_bitrate_kbps: 320,
    }
}

/// 用 MF 源读取器遍历全部样本，返回 (帧数, 时间轴长度秒数)。
fn inspect_with_mf(path: &str) -> (u32, f64) {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup 失败");
        let reader =
            MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), None).expect("MF 打不开该 MP4");
        let mt: IMFMediaType = reader
            .GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)
            .expect("MF 读不到视频轨（stsd/avc1 结构可能有问题）");
        assert!(
            mt.GetGUID(&MF_MT_MAJOR_TYPE).unwrap() == MFMediaType_Video,
            "轨道主类型应为视频"
        );
        let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap();
        assert_eq!(size >> 32, REGION.2 as u64, "宽度与请求不一致");
        assert_eq!(size & 0xFFFF_FFFF, REGION.3 as u64, "高度与请求不一致");

        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &mt)
            .unwrap();

        let mut frames = 0u32;
        let mut timeline_end = 0i64;
        loop {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample = None;
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample),
                )
                .unwrap();
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if let Some(s) = sample {
                frames += 1;
                timeline_end = ts + s.GetSampleDuration().unwrap_or(0);
                assert!(frames < 10_000, "样本数异常");
            }
        }
        (frames, timeline_end as f64 / 10_000_000.0)
    }
}

/// 顶层盒类型序列（严格按盒链遍历，能同时发现越界与垃圾）。
fn top_level_boxes(path: &str) -> Vec<String> {
    let bytes = std::fs::read(path).expect("产物文件不存在");
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
        kinds.push(String::from_utf8_lossy(&bytes[pos + 4..pos + 8]).to_string());
        pos += size;
    }
    assert_eq!(pos, bytes.len(), "顶层盒序列应恰好覆盖整个文件");
    kinds
}

/// 在 `moov` 范围内查一段四字节标记（用来确认样本描述是 `hvc1`/`hev1` 而不是 `avc1`）。
fn moov_contains(path: &str, needle: &[u8]) -> bool {
    let bytes = std::fs::read(path).unwrap();
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let size32 = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
        let size = match size32 {
            0 => bytes.len() - pos,
            1 => u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize,
            n => n as usize,
        };
        if &bytes[pos + 4..pos + 8] == b"moov" {
            let body = &bytes[pos..(pos + size).min(bytes.len())];
            return body.windows(needle.len()).any(|w| w == needle);
        }
        pos += size;
    }
    false
}

fn record_and_verify(label: &str, encoder: EncoderPreference) {
    let dir = std::env::temp_dir().join("murecorder_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("e2e_{label}.mp4"));
    let _ = std::fs::remove_file(&path);
    let path_str = path.to_string_lossy().to_string();

    let mut rec = Recorder::created(recording_config(&path_str, encoder)).expect("create 失败");
    rec.start().expect("start 失败");
    std::thread::sleep(Duration::from_millis(DURATION_MS));
    rec.stop().expect("stop 失败（封装可能失败）");

    let file_size = std::fs::metadata(&path).expect("产物文件不存在").len();
    let (frames, seconds) = inspect_with_mf(&path_str);
    let snap = rec.stats_snapshot();
    println!(
        "[{label}] {file_size} B, {frames} 帧, 时长 {seconds:.2}s, 编码器={} \
         (captured={} encoded={} dropped={} fps={})",
        snap.encoder_type,
        snap.frames_captured,
        snap.frames_encoded,
        snap.frames_dropped,
        snap.current_fps
    );

    assert!(frames > 30, "[{label}] 帧数过少: {frames}");
    let expected = DURATION_MS as f64 / 1000.0;
    assert!(
        (expected - 0.4..=expected + 0.6).contains(&seconds),
        "[{label}] 时长应约 {expected:.1}s，实际 {seconds:.2}s（时间戳可能不是墙钟）"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
#[ignore = "需要真实桌面会话；用 --ignored 手动运行"]
fn e2e_software_encoder() {
    record_and_verify("soft", EncoderPreference::Soft);
}

#[test]
#[ignore = "需要真实桌面会话与硬件编码器；用 --ignored 手动运行"]
fn e2e_gpu_encoder() {
    record_and_verify("gpu", EncoderPreference::Auto);
}

/// 开启 `capture_cursor` 录一段，期间**持续移动鼠标**，产物必须仍然可解码且时长正常。
///
/// 这条用例覆盖的是接线本身：`capture_cursor` 从 C# 配置一路传到
/// `DxgiCapture::new(…, CaptureOptions)`、`acquire_texture` 的 `FrameMeta`、
/// 再到 `GpuSinkWriter::push_frame`。其中任何一环没接上，要么崩在闭包里，
/// 要么时间戳错乱，要么干脆没帧——都不是「指针没画出来」这么温和的失败。
///
/// 顺带压测「只有指针更新」的帧：桌面不动、鼠标动，`AccumulatedFrames` 为 0，
/// 走的是「复用纯桌面副本 + 重画光标」那条分支，而不是重新截屏。
#[test]
#[ignore = "需要真实桌面会话与硬件编码器，且会移动真实鼠标指针（结束会还原）"]
fn e2e_gpu_recording_with_cursor() {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

    let mut origin = POINT::default();
    // Safety: 本地变量地址，调用期间有效。
    let saved = unsafe { GetCursorPos(&mut origin) }.is_ok();

    let dir = std::env::temp_dir().join("murecorder_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("e2e_gpu_cursor.mp4");
    let _ = std::fs::remove_file(&path);
    let path_str = path.to_string_lossy().to_string();

    let mut cfg = recording_config(&path_str, EncoderPreference::Auto);
    cfg.capture_cursor = true;

    let mut rec = Recorder::created(cfg).expect("create 失败");
    rec.start().expect("start 失败");

    // 在区域内部按网格走一圈：既有位移也有方向变化，能覆盖
    // 「位置变了」与「位置没变但形状换了」两类更新。
    let steps = (DURATION_MS / 30) as i32;
    for i in 0..steps {
        let x = REGION.0 + 4 + (i % 12) * 24;
        let y = REGION.1 + 4 + ((i / 12) % 8) * 26;
        unsafe {
            let _ = SetCursorPos(x, y);
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    rec.stop().expect("stop 失败（封装可能失败）");

    if saved {
        unsafe {
            let _ = SetCursorPos(origin.x, origin.y);
        }
    }

    let file_size = std::fs::metadata(&path).expect("产物文件不存在").len();
    let (frames, seconds) = inspect_with_mf(&path_str);
    let snap = rec.stats_snapshot();
    println!(
        "[gpu+cursor] {file_size} B, {frames} 帧, 时长 {seconds:.2}s, \
         captured={} encoded={} dropped={}",
        snap.frames_captured, snap.frames_encoded, snap.frames_dropped
    );

    assert!(frames > 30, "[gpu+cursor] 帧数过少: {frames}");
    let expected = DURATION_MS as f64 / 1000.0;
    assert!(
        (expected - 0.4..=expected + 0.6).contains(&seconds),
        "[gpu+cursor] 时长应约 {expected:.1}s，实际 {seconds:.2}s"
    );

    let _ = std::fs::remove_file(&path);
}

/// 录制中切片 → 产物必须被 MF 独立读回，且帧数/时长与切出的区间一致。
///
/// 这条用例覆盖 Hybrid 的关键收益：`moof` 自带样本表 + 样本位置相对 moof 起点，
/// 所以切片既不需要重编码，也不需要修正任何偏移。
#[test]
#[ignore = "需要 Media Foundation；用 --ignored 手动运行"]
fn e2e_slice_from_recording_is_playable() {
    use murecorder_core::convert::i420::bgra_to_i420;
    use murecorder_core::encode::VideoEncoder;
    use murecorder_core::mux::extract::extract_range;
    use murecorder_core::mux::mp4::Mp4Muxer;

    const W: u32 = 320;
    const H: u32 = 240;
    const FPS: u32 = 30;
    const BAR_W: usize = 24;

    let dir = std::env::temp_dir().join("murecorder_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("slice_src.mp4");
    let dst = dir.join("slice_out.mp4");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);

    let mut enc = VideoEncoder::new_soft(W, H, FPS, 3000).unwrap();
    let mut muxer = Mp4Muxer::create(&src, W, H, FPS).unwrap();

    // 灰底 + 一条横移白条：既保证画面有真实变化，也避免整段被编码成静止帧。
    let stride: u32 = W * 4;
    let stride_usize = stride as usize;
    let mut bgra = vec![0u8; stride_usize * H as usize];
    for px in bgra.chunks_mut(4) {
        px[0] = 128;
        px[1] = 128;
        px[2] = 128;
        px[3] = 255;
    }
    let mut bar_x = 0usize;
    for i in 0..180u64 {
        // 擦掉旧白条
        for y in 0..H as usize {
            for x in bar_x..(bar_x + BAR_W).min(W as usize) {
                let o = y * stride_usize + x * 4;
                bgra[o] = 128;
                bgra[o + 1] = 128;
                bgra[o + 2] = 128;
            }
        }
        bar_x = (bar_x + 3) % (W as usize - BAR_W);
        // 画新白条
        for y in 0..H as usize {
            for x in bar_x..bar_x + BAR_W {
                let o = y * stride_usize + x * 4;
                bgra[o] = 255;
                bgra[o + 1] = 255;
                bgra[o + 2] = 255;
            }
        }

        let i420 = bgra_to_i420(&bgra, stride, W, H);
        let pkt = enc.encode(&i420).unwrap();
        muxer
            .write_sample(
                &pkt.data,
                i * 1_000_000 / FPS as u64,
                pkt.is_key,
                &pkt.sps,
                &pkt.pps,
            )
            .unwrap();
    }
    // 刻意不调用 finish：文件此刻停在「录制中」状态，正是随时可切片的形态。

    // 关键验证：未 finalize 的文件必须已经是合法 fMP4 —— 这正是崩溃断电后
    // 能救回素材的前提。MF 若能直接读回，说明它不需要任何修复步骤就能播。
    let (raw_frames, raw_secs) = inspect_with_mf(src.to_str().unwrap());
    println!("[slice] 录制中文件（未 finalize）: {raw_frames} 帧, {raw_secs:.2}s");
    assert!(
        raw_frames > 100,
        "录制中的 fMP4 应能被 MF 直接读回，实际 {raw_frames} 帧"
    );

    let info = extract_range(&src, &dst, 2_100_000, 3_500_000, FPS).unwrap();
    let (frames, seconds) = inspect_with_mf(dst.to_str().unwrap());
    println!(
        "[slice] {} B, {frames} 帧, 时长 {seconds:.2}s, 源区间 [{}, {}) µs",
        info.bytes, info.source_start_us, info.source_end_us
    );

    assert_eq!(
        frames, info.sample_count as u32,
        "MF 读到的帧数应等于切片样本数"
    );
    // 起点被对齐到 2 s 的关键帧（OpenH264 每 2 s 一个 IDR），终点裁到 3.5 s → 1.5 s。
    assert!(
        (1.4..=1.6).contains(&seconds),
        "切 2.1–3.5 s 区间应对齐出 1.5 s，实际 {seconds:.2}s"
    );

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}

/// GPU 录制中「随时切一段、同时继续录」——这是 GPU 路径上最容易被忽略的能力。
///
/// MF 产物与自家封装器有两处不同：**不写 `tfdt`**、时基是 30000 而不是微秒。
/// 这个用例在录制进行时（文件仍被 MF 打开着）切一段出来，再让 MF 独立读回，
/// 同时确认原录制没有被中断、停止后仍然是普通 MP4。
#[test]
#[ignore = "需要真实 GPU 硬件编码器与桌面会话（本地手动验证）"]
fn e2e_slice_gpu_recording_while_running() {
    use murecorder_core::mux::extract::extract_range;
    use murecorder_core::stats::ENCODER_SOFT_OPENH264;

    let dir = std::env::temp_dir().join("murecorder_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("gpu_live.mp4");
    let slice = dir.join("gpu_live_slice.mp4");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&slice);
    let path_str = path.to_string_lossy().to_string();

    let mut rec = Recorder::created(recording_config(&path_str, EncoderPreference::Auto)).unwrap();
    rec.start().unwrap();
    std::thread::sleep(Duration::from_millis(3_000));

    let snap = rec.stats_snapshot();
    if snap.encoder_type == ENCODER_SOFT_OPENH264 {
        rec.stop().ok();
        let _ = std::fs::remove_file(&path);
        println!("[slice-gpu] 本机无硬件编码器，跳过（软编路径由其他用例覆盖）");
        return;
    }

    // 关键动作：录制还在进行时就切片。
    let info = extract_range(&path, &slice, 1_000_000, 2_000_000, 30);
    // 无论切片成功与否，录制都必须能正常收尾。
    rec.stop().unwrap();

    // 落盘形态决定能否在录制中切片，而形态取决于**实际选中的编码器**：
    // H.264 走 MF 自带分片 sink，HEVC 走自研 sink（两者都分片，都能切）；
    // AV1 目前只有普通 MP4 一条路，录制中没有 `moov`，必须给出明确拒绝原因。
    let info = match info {
        Ok(info) => info,
        Err(err) => {
            println!(
                "[slice-gpu] 编码器={} 落盘为普通 MP4，切片按预期被拒绝：{err}",
                snap.encoder_type
            );
            assert!(
                err.to_string().contains("不是分片式 MP4"),
                "错误信息应说明该文件不是分片式 MP4，实际：{err}"
            );
            assert!(!slice.exists(), "拒绝时不应留下半成品文件");
            // 停止后文件仍是可播放的普通 MP4。
            let (frames, secs) = inspect_with_mf(&path_str);
            println!("[slice-gpu] 原录制 MF 读回 {frames} 帧 / {secs:.2}s");
            assert!(frames > 20, "非分片录制停止后应可播放，实际 {frames} 帧");
            let _ = std::fs::remove_file(&path);
            return;
        }
    };

    println!(
        "[slice-gpu] 切片 {} B / {} 帧 / {:.2}s，源区间 [{}, {}) µs",
        info.bytes,
        info.sample_count,
        info.duration_us as f64 / 1e6,
        info.source_start_us,
        info.source_end_us
    );
    assert!(info.sample_count > 0, "切片不应为空");

    let (frames, seconds) = inspect_with_mf(slice.to_str().unwrap());
    println!("[slice-gpu] MF 读回切片 {frames} 帧 / {seconds:.2}s");
    assert_eq!(frames, info.sample_count as u32, "MF 帧数应等于切片样本数");
    assert!(
        (seconds - info.duration_us as f64 / 1e6).abs() < 0.15,
        "MF 读到的时长应与切片一致，实际 {seconds:.2}s"
    );

    // 原录制不能因为切片而受损。
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 10_000, "原录制文件不应被破坏");
    let (total_frames, total_secs) = inspect_with_mf(&path_str);
    println!("[slice-gpu] 原录制 MF 读回 {total_frames} 帧 / {total_secs:.2}s");
    assert!(total_frames > frames, "原录制应长于切片");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&slice);
}

/// 显式要求 HEVC 时，必须真的录出 HEVC，而不是悄悄换成 H.264。
///
/// 这条用例盯的是一个真实踩过的坑：MF 的分片 MP4 sink 不接受 HEVC，
/// 早先的实现会把 `MF_E_INVALIDMEDIATYPE` 吞掉、直接降级成 H.264，
/// 于是「选了 HEVC 却得到 H.264」——比报错更糟。
///
/// 现在 HEVC 走**自研 sink**（编码器仍由 MF 挑，写盒归我们），所以它跟 H.264
/// 一样是分片落盘：录制中有 `moof`、随时可切片、崩溃只丢最后一个分片。
#[test]
#[ignore = "需要支持 HEVC 硬编的 GPU 与桌面会话（本地手动验证）"]
fn e2e_hevc_records_when_explicitly_requested() {
    use murecorder_core::stats::ENCODER_GPU_HEVC;

    let dir = std::env::temp_dir().join("murecorder_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hevc.mp4");
    let _ = std::fs::remove_file(&path);
    let path_str = path.to_string_lossy().to_string();

    let mut rec =
        Recorder::created(recording_config(&path_str, EncoderPreference::GpuHevc)).unwrap();
    rec.start().unwrap();

    // 录制中就必须是分片态：有 `moof`、`moov` 只是初始化段（索引到停止时才补）。
    // 这一条直接证明「HEVC 也能 Hybrid」，而不只是「能录出 HEVC」。
    //
    // 为什么要**轮询**而不是固定等 1.2 s：分片是在「下一个关键帧到来」时闭合的
    // （见 `mp4.rs::write_video_sample`），而 HEVC 硬编的关键帧间隔约 30 帧 ≈ 1 s，
    // 再叠加启动延迟，1.2 s 那个时刻恰好压在边界上——同一台机器上「已出 moof」和
    // 「还没出 moof」都出现过（实测 35 帧→有，30 帧→没有）。定点检查会假失败，
    // 轮询到出现为止既稳定、又比原来更严格（原来只是「1.2 s 时恰好有」）。
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut live = top_level_boxes(&path_str);
    while !live.iter().any(|k| k == "moof") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(80));
        live = top_level_boxes(&path_str);
    }
    // 录制中的 moov 是**初始化段**（只有解码参数，没有样本表），标志是 `mvex`。
    // 必须在停止前检查：停止后 init moov 会被那个巨型 mdat 吞掉，只剩末尾的完整 moov。
    let live_init_mvex = moov_contains(&path_str, b"mvex");
    println!("[hevc] 录制中顶层盒: {live:?}, init moov 带 mvex={live_init_mvex}");

    // 再录一会儿，保证停止时不止一个分片。
    std::thread::sleep(Duration::from_millis(1_300));
    rec.stop().unwrap();

    let snap = rec.stats_snapshot();
    if snap.encoder_type != ENCODER_GPU_HEVC {
        // 本机没有 HEVC 硬编，降级是合理行为。
        println!(
            "[hevc] 本机无 HEVC 硬件编码器（编码器={}），跳过",
            snap.encoder_type
        );
        let _ = std::fs::remove_file(&path);
        return;
    }

    assert!(
        live.iter().any(|k| k == "moof"),
        "录制中应是分片态（有 moof），实际 {live:?}"
    );
    assert!(live_init_mvex, "录制中的 moov 应带 mvex（分片初始化段）");
    // 停止后的完整 moov **不能**带 mvex，否则读取方会把它当成分片文件。
    assert!(
        !moov_contains(&path_str, b"mvex"),
        "停止后末尾的 moov 不应带 mvex"
    );

    let kinds = top_level_boxes(&path_str);
    println!(
        "[hevc] {} 字节, 顶层盒: {kinds:?}",
        std::fs::metadata(&path).unwrap().len()
    );
    // 非分片 sink 会在 ftyp 后塞一个自用的 `uuid` 盒，可以忽略；
    // 关键是「以 moov 收尾、没有 moof」——也就是一份普通 MP4。
    assert_eq!(
        kinds.first().map(String::as_str),
        Some("ftyp"),
        "应以 ftyp 开头"
    );
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("moov"),
        "应以 moov 收尾"
    );
    assert!(
        kinds.iter().any(|k| k == "mdat"),
        "应有 mdat，实际 {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k == "moof"),
        "不应残留分片结构，实际 {kinds:?}"
    );

    assert!(
        moov_contains(&path_str, b"hvc1") || moov_contains(&path_str, b"hev1"),
        "moov 里应有 hvc1/hev1 样本描述，才是真的 HEVC"
    );
    assert!(
        !moov_contains(&path_str, b"avc1"),
        "选了 HEVC 却出现 avc1，说明被悄悄换成了 H.264"
    );

    // 收尾必须用 64 位 `co64`：`mdat` 已经是 64 位 largesize，偏移表若还是
    // 32 位 `stco`，超过 4 GiB 就会回绕。自研封装器直接写 co64。
    assert!(
        moov_contains(&path_str, b"co64"),
        "偏移表应为 co64（解除 4 GiB 上限）"
    );
    assert!(!moov_contains(&path_str, b"stco"), "不应出现 32 位的 stco");

    let (frames, seconds) = inspect_with_mf(&path_str);
    println!("[hevc] MF 读回 {frames} 帧 / {seconds:.2}s");
    assert!(frames > 20, "HEVC 产物应能被 MF 解出帧，实际 {frames}");
    assert!(
        (2.0..=3.4).contains(&seconds),
        "2.5 s 录制的时长应在合理区间，实际 {seconds:.2}s"
    );

    let _ = std::fs::remove_file(&path);
}

/// GPU（IMFSinkWriter）路径：停止后必须收敛成与软编同构的普通 MP4。
///
/// 两条路径的最终产物必须都是 `ftyp / mdat / moov`——否则「上传平台转码器不认」
/// 的问题就只在 GPU 路径上出现，而那正是绝大多数实际使用场景。
#[test]
#[ignore = "需要真实 GPU 硬件编码器与桌面会话（本地手动验证）"]
fn e2e_gpu_recording_finalizes_to_plain_mp4() {
    use murecorder_core::stats::ENCODER_SOFT_OPENH264;

    let dir = std::env::temp_dir().join("murecorder_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("gpu_finalize.mp4");
    let _ = std::fs::remove_file(&path);
    let path_str = path.to_string_lossy().to_string();

    let mut rec = Recorder::created(recording_config(&path_str, EncoderPreference::Auto)).unwrap();
    rec.start().unwrap();
    std::thread::sleep(Duration::from_millis(2_500));
    rec.stop().unwrap();

    let snap = rec.stats_snapshot();
    if snap.encoder_type == ENCODER_SOFT_OPENH264 {
        println!("[gpu-finalize] 本机无硬件编码器，跳过（软编路径由其他用例覆盖）");
        return;
    }

    let kinds = top_level_boxes(&path_str);
    println!(
        "[gpu-finalize] 编码器={} 顶层盒: {kinds:?}",
        snap.encoder_type
    );
    assert_eq!(
        kinds,
        vec!["ftyp", "mdat", "moov"],
        "GPU 路径停止后必须收敛成普通 MP4，实际 {kinds:?}"
    );

    // 真正的判据：MF 自带解析器能否读回重建后的 moov。
    let (frames, seconds) = inspect_with_mf(&path_str);
    println!("[gpu-finalize] MF 读回 {frames} 帧 / {seconds:.2}s");
    assert!(frames > 20, "GPU 产物应能被 MF 解出帧，实际 {frames}");
    assert!(
        (2.0..=3.4).contains(&seconds),
        "2.5 s 录制的时长应在合理区间，实际 {seconds:.2}s"
    );

    let _ = std::fs::remove_file(&path);
}
