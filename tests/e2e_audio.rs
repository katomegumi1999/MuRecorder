//! 音频端到端测试：真的录出带声音的 MP4，再交给 Media Foundation 独立读回。
//!
//! 为什么必须有这个文件：单元测试里的音频帧全是**合成字节**
//! （`vec![0x21; 120]` 这类），它们只能证明封装器的盒结构写得对，
//! 证明不了「AAC 码流能被解码器接受」「`esds` 里的 `ASC` 与码流一致」
//! 「音轨时间轴与视频对齐」。这三件事任何一件错了，产物都是**能播但没声音**
//! 或者**时间轴错位**——只有真的把 MF 的 AAC 解码器插上去才测得出来。
//!
//! ```text
//! cargo test --test e2e_audio -- --ignored --nocapture
//! ```
//!
//! 需要真实桌面会话（WASAPI loopback 要有默认渲染设备）。桌面是否真的在
//! 发出声音**不影响**判定：静音时走的正是「墙钟驱动补静音」那条路径，
//! 断言的是时间轴不塌缩，而不是「一定录到了人声」。

use murecorder_core::config::{AudioSourceType, EncoderPreference, RecordingConfig};
use murecorder_core::mux::extract::extract_range;
use murecorder_core::recorder::Recorder;
use std::time::Duration;
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSourceReader, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFAudioFormat_AAC,
    MFAudioFormat_PCM, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio,
    MFSTARTUP_FULL, MFStartup,
};
use windows::core::PCWSTR;

const REGION: (i32, i32, u32, u32) = (100, 100, 320, 240);
/// 录制时长。留够余量让 AAC 编码器至少吐出几十个包，同时让 e2e 不至于太久。
const DURATION_MS: u64 = 2_500;
/// 设计定稿参数（见 docs/audio-design.md）：48 kHz / 立体声 / AAC-LC。
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
/// 一个 AAC-LC 帧恒为 1024 个采样。
const SAMPLES_PER_FRAME: u32 = 1024;

// mfreadwrite.h 里的枚举值。写成字面量是为了能在 `const` 上下文里用；
// 名字与头文件一一对应，改错会立刻被下面的断言打出来。
/// `MF_SOURCE_READER_FIRST_AUDIO_STREAM` = 0xFFFFFFFD
const FIRST_AUDIO_STREAM: u32 = 0xFFFF_FFFD;
/// `MF_SOURCE_READER_ALL_STREAMS` = 0xFFFFFFFE
const ALL_STREAMS: u32 = 0xFFFF_FFFE;

fn config_with_audio(path: &str, encoder: EncoderPreference) -> RecordingConfig {
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
        audio_source: AudioSourceType::System,
        audio_device_id: None,
        audio_window_hwnd: 0,
        audio_process_id: 0,
        audio_bitrate_kbps: 320,
    }
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("murecorder_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    p
}

fn open_reader(path: &str) -> IMFSourceReader {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup 失败");
        MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), None).expect("MF 打不开该 MP4")
    }
}

/// 容器层探测结果（不解码，只按压缩域读样本）。
struct AudioTrackInfo {
    sample_rate: u32,
    channels: u32,
    /// 容器里实际有多少个 AAC 帧。
    packet_count: u32,
    /// 音频轨时间轴长度（秒），最后一个样本的时间戳 + 时长。
    seconds: f64,
}

/// 探测第 2 条轨是不是音频、参数对不对、有多少帧、时间轴多长。
///
/// 返回 `None` 表示**没有音频轨**——单轨文件会走到这里，
/// 断言「应该有音频」的测试必须把它当失败而不是跳过。
fn probe_audio_track(path: &str) -> Option<AudioTrackInfo> {
    let reader = open_reader(path);
    unsafe {
        let native: IMFMediaType = reader.GetNativeMediaType(FIRST_AUDIO_STREAM, 0).ok()?;

        assert_eq!(
            native.GetGUID(&MF_MT_MAJOR_TYPE).unwrap(),
            MFMediaType_Audio,
            "第 2 条轨的主类型应为音频"
        );
        assert_eq!(
            native.GetGUID(&MF_MT_SUBTYPE).unwrap(),
            MFAudioFormat_AAC,
            "音频子类型应为 AAC —— mp4a/esds 结构可能有问题"
        );

        let sample_rate = native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap();
        let channels = native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap();

        // 只留音频流：两条流都被选中时 ReadSample 会在流之间交替返回。
        reader.SetStreamSelection(ALL_STREAMS, false).unwrap();
        reader.SetStreamSelection(FIRST_AUDIO_STREAM, true).unwrap();
        reader
            .SetCurrentMediaType(FIRST_AUDIO_STREAM, None, &native)
            .expect("无法把音频轨设为原生 AAC —— stsd/esds 可能不完整");

        let mut packet_count = 0u32;
        let mut end = 0i64;
        loop {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample = None;
            reader
                .ReadSample(
                    FIRST_AUDIO_STREAM,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample),
                )
                .expect("读取 AAC 样本失败（样本表或 data_offset 可能算错）");
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if let Some(s) = sample {
                packet_count += 1;
                end = ts + s.GetSampleDuration().unwrap_or(0);
                assert!(packet_count < 200_000, "AAC 样本数异常");
            }
        }

        Some(AudioTrackInfo {
            sample_rate,
            channels,
            packet_count,
            seconds: end as f64 / 10_000_000.0,
        })
    }
}

/// 把音频轨接成 PCM 解码出来，返回 (PCM 总字节数, 解码后时间轴长度秒)。
///
/// 这一步是整条音频链最硬的判据：`SetCurrentMediaType(PCM)` 会逼 MF 插入
/// AAC 解码器，**解码器初始化时用的正是 `esds` 里的 ASC**。
/// 如果 ASC 与真实码流不符（采样率/声道/objectType 任一错），这里会失败，
/// 而不是等到播放器里表现为「有轨无声」。
fn decode_audio_to_pcm(path: &str) -> Option<(u64, f64)> {
    let reader = open_reader(path);
    unsafe {
        let native: IMFMediaType = reader.GetNativeMediaType(FIRST_AUDIO_STREAM, 0).ok()?;
        let rate = native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap();
        let channels = native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap();

        reader.SetStreamSelection(ALL_STREAMS, false).ok()?;
        reader.SetStreamSelection(FIRST_AUDIO_STREAM, true).ok()?;

        let pcm: IMFMediaType = MFCreateMediaType().unwrap();
        pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
        pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).unwrap();
        pcm.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels).unwrap();
        pcm.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)
            .unwrap();
        pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).unwrap();
        pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2)
            .unwrap();
        pcm.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels * 2)
            .unwrap();

        reader
            .SetCurrentMediaType(FIRST_AUDIO_STREAM, None, &pcm)
            .expect("MF 无法把音频轨接成 PCM —— esds 里的 ASC 与真实 AAC 码流不匹配");

        let mut total_bytes = 0u64;
        let mut end = 0i64;
        loop {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample = None;
            reader
                .ReadSample(
                    FIRST_AUDIO_STREAM,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample),
                )
                .expect("AAC 解码失败（码流本身可能损坏）");
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if let Some(s) = sample {
                // PCM 样本的 buffer 长度直接就是字节数，不需要锁缓冲区。
                total_bytes += s.GetTotalLength().unwrap_or(0) as u64;
                end = ts + s.GetSampleDuration().unwrap_or(0);
            }
        }

        Some((total_bytes, end as f64 / 10_000_000.0))
    }
}

/// 按设计参数推算「这段时长应该有多少个 AAC 帧」。
fn expected_packets(seconds: f64) -> f64 {
    seconds * SAMPLE_RATE as f64 / SAMPLES_PER_FRAME as f64
}

/// 顶层盒类型序列，用来确认停止后收敛成普通 MP4。
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

/// 在 `moov` 范围内数某个四字节标记出现次数（用来数 `trak` 有几条）。
fn count_in_moov(path: &str, needle: &[u8]) -> usize {
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
            return body.windows(needle.len()).filter(|w| *w == needle).count();
        }
        pos += size;
    }
    0
}

/// 软编路径：**自驱 AAC MFT + 自研 Mp4Muxer** 这条链的完整验证。
///
/// 与 GPU 路径的区别在于 AAC 是我们自己从 MFT 里拽出来的，
/// `esds`/`ASC` 全部手写（`aac.rs::ASC_48K_STEREO`），
/// 因此这条链最容易在「ASC 写死成常量」这件事上翻车——正是本用例重点。
#[test]
#[ignore = "需要真实桌面会话与默认音频渲染设备；用 --ignored 手动运行"]
fn e2e_audio_with_software_encoder() {
    let path = temp_path("audio_soft.mp4");
    let path_str = path.to_string_lossy().to_string();

    let mut rec = Recorder::created(config_with_audio(&path_str, EncoderPreference::Soft))
        .expect("create 失败");
    rec.start().expect("start 失败（音频设备不可用？）");
    std::thread::sleep(Duration::from_millis(DURATION_MS));
    rec.stop().expect("stop 失败（音频收尾可能出错）");

    // 停止后必须收敛成普通 MP4，且结论与视频路径一致：两条轨都进 moov。
    let kinds = top_level_boxes(&path_str);
    println!("[audio-soft] 顶层盒: {kinds:?}");
    assert_eq!(
        kinds,
        vec!["ftyp", "mdat", "moov"],
        "停止后应为普通 MP4，实际 {kinds:?}"
    );
    assert_eq!(
        count_in_moov(&path_str, b"trak"),
        2,
        "moov 里应有视频 + 音频两条 trak"
    );

    let info = probe_audio_track(&path_str).expect("产物里没有音频轨 —— 音频轨没写进 moov");
    println!(
        "[audio-soft] 音频轨: {} Hz / {} 声道 / {} 个 AAC 帧 / 时间轴 {:.2}s",
        info.sample_rate, info.channels, info.packet_count, info.seconds
    );
    assert_eq!(info.sample_rate, SAMPLE_RATE, "采样率应为 48 kHz");
    assert_eq!(info.channels, CHANNELS, "应为立体声");

    let expected = DURATION_MS as f64 / 1000.0;
    let expect_pkt = expected_packets(expected);
    assert!(
        (expect_pkt * 0.85..=expect_pkt * 1.15).contains(&(info.packet_count as f64)),
        "{}s 应约 {expect_pkt:.0} 个 AAC 帧，实际 {}（编码器丢包或时间轴断裂）",
        expected,
        info.packet_count
    );

    // 最硬的一步：真的把 AAC 解成 PCM。
    let (pcm_bytes, decoded_secs) = decode_audio_to_pcm(&path_str).expect("没有音频轨");
    let pcm_rate = (SAMPLE_RATE * CHANNELS * 2) as f64; // 字节/秒
    println!(
        "[audio-soft] 解码出 {pcm_bytes} B PCM（{:.2}s @ 192 kB/s），时间轴 {decoded_secs:.2}s",
        pcm_bytes as f64 / pcm_rate
    );
    assert!(
        pcm_bytes as f64 / pcm_rate > expected * 0.85,
        "解码出的 PCM 只有 {:.2}s，远少于录制的 {expected:.1}s",
        pcm_bytes as f64 / pcm_rate
    );
    assert!(
        (decoded_secs - expected).abs() <= 0.30,
        "音轨时间轴应约 {expected:.1}s（AAC 编码延迟约 43 ms），实际 {decoded_secs:.2}s"
    );

    let _ = std::fs::remove_file(&path);
}

/// 桌面**没有在放声音**时，音轨不允许塌缩成几帧。
///
/// 这是静音补包算法的通关条件。WASAPI loopback 在无声时不产生任何
/// 数据包；如果不按墙钟补静音，音轨就只有开头几帧、后面整段缺失——
/// 播放器里的表现是「视频 10 分钟、音频 0.3 秒」，而且不报任何错。
///
/// 断言用的是**两条轨的时间轴必须一致**，而不是「必须有声音」：
/// 桌面正好有声音时这条断言同样成立，所以在任何环境下都可以跑。
#[test]
#[ignore = "需要真实桌面会话与默认音频渲染设备；用 --ignored 手动运行"]
fn e2e_audio_timeline_matches_video_when_desktop_is_quiet() {
    let path = temp_path("audio_quiet.mp4");
    let path_str = path.to_string_lossy().to_string();

    let mut rec = Recorder::created(config_with_audio(&path_str, EncoderPreference::Soft))
        .expect("create 失败");
    rec.start().expect("start 失败（音频设备不可用？）");
    std::thread::sleep(Duration::from_millis(DURATION_MS));
    rec.stop().expect("stop 失败");

    let video_secs = video_duration_secs(&path_str);
    let audio = probe_audio_track(&path_str).expect("产物里没有音频轨");
    println!(
        "[audio-quiet] 视频 {video_secs:.2}s / 音频 {:.2}s / {} 个 AAC 帧",
        audio.seconds, audio.packet_count
    );

    // 红线：两条轨的时间轴必须几乎一样长。差值超过 0.25s 就意味着音频中途
    // 断流并且没有被补上——正是「静音桌面」会触发的那种 bug。
    let drift = (video_secs - audio.seconds).abs();
    assert!(
        drift <= 0.25,
        "音视频时间轴偏差 {drift:.2}s 过大（视频 {video_secs:.2}s、音频 {:.2}s）；\
         音频在静音期间断流且未被补包",
        audio.seconds
    );

    // 帧数也要对得上：48000/1024 ≈ 46.875 帧/秒。
    let expect_pkt = expected_packets(audio.seconds);
    assert!(
        (audio.packet_count as f64) >= expect_pkt * 0.9,
        "音频只有 {} 帧，按 {:.2}s 应约 {expect_pkt:.0} 帧",
        audio.packet_count,
        audio.seconds
    );

    let _ = std::fs::remove_file(&path);
}

/// 视频轨时间轴长度（秒），用于和音频轨对比。
fn video_duration_secs(path: &str) -> f64 {
    use windows::Win32::Media::MediaFoundation::MF_SOURCE_READER_FIRST_VIDEO_STREAM;
    let reader = open_reader(path);
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    unsafe {
        let mt: IMFMediaType = reader.GetNativeMediaType(stream, 0).expect("读不到视频轨");
        reader.SetStreamSelection(ALL_STREAMS, false).unwrap();
        reader.SetStreamSelection(stream, true).unwrap();
        reader.SetCurrentMediaType(stream, None, &mt).unwrap();

        let mut end = 0i64;
        loop {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample = None;
            reader
                .ReadSample(
                    stream,
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
                end = ts + s.GetSampleDuration().unwrap_or(0);
            }
        }
        end as f64 / 10_000_000.0
    }
}

/// 录制中切片：切出来的片段必须**同时带走视频和与之对齐的音频**。
///
/// 这是切片最容易写错的地方：音频包的时间戳不是关键帧边界，
/// 如果按 `start_us` 入参切音频、按关键帧切视频，音画会错开几十到几百毫秒，
/// 而且文件本身仍然「可播放」，肉眼在静音片段里完全看不出来。
/// 设计上的红线是：**音频起点必须服从视频关键帧的真实 tick**。
#[test]
#[ignore = "需要真实桌面会话与默认音频渲染设备；用 --ignored 手动运行"]
fn e2e_audio_survives_slicing() {
    let src = temp_path("audio_slice_src.mp4");
    let dst = temp_path("audio_slice_out.mp4");
    let src_str = src.to_string_lossy().to_string();

    let mut rec = Recorder::created(config_with_audio(&src_str, EncoderPreference::Soft))
        .expect("create 失败");
    rec.start().expect("start 失败（音频设备不可用？）");
    // 录 4 s 而不是刚好 2 s：切片只能切**已落盘的分片**，而第一个视频分片要等到
    // 下一个关键帧才闭合（GOP = fps × 2）。窗口留窄了会撞上「还没写盘」，
    // 测出来的就不是音频对齐，而是时序运气。
    std::thread::sleep(Duration::from_millis(4_000));

    // 关键动作：**录制还在进行时**切片。
    let info = extract_range(&src, &dst, 1_200_000, 2_400_000, 30);
    rec.stop().expect("stop 失败");

    let info = info.expect("录制中切片失败（文件应处于分片态，且该区间应已落盘）");
    println!(
        "[audio-slice] 切片 {} B / {} 样本 / {:.2}s，源区间 [{}, {}) µs",
        info.bytes,
        info.sample_count,
        info.duration_us as f64 / 1e6,
        info.source_start_us,
        info.source_end_us
    );

    // 切片产物必须是带两条 trak 的普通 MP4。
    let kinds = top_level_boxes(dst.to_str().unwrap());
    println!("[audio-slice] 切片顶层盒: {kinds:?}");
    assert_eq!(
        kinds,
        vec!["ftyp", "mdat", "moov"],
        "切片应为普通 MP4，实际 {kinds:?}"
    );
    assert_eq!(
        count_in_moov(dst.to_str().unwrap(), b"trak"),
        2,
        "切片必须同时带视频轨和音频轨"
    );

    let audio = probe_audio_track(dst.to_str().unwrap()).expect("切片里没有音频轨");
    let video_secs = video_duration_secs(dst.to_str().unwrap());
    println!(
        "[audio-slice] 切片视频 {video_secs:.2}s / 音频 {:.2}s / {} 个 AAC 帧",
        audio.seconds, audio.packet_count
    );

    assert!(
        audio.packet_count > 10,
        "切片音频帧数过少: {}",
        audio.packet_count
    );
    let drift = (video_secs - audio.seconds).abs();
    assert!(
        drift <= 0.25,
        "切片里音视频时间轴偏差 {drift:.2}s 过大（视频 {video_secs:.2}s、音频 {:.2}s）；\
         音频起点没有对齐到视频关键帧",
        audio.seconds
    );

    // 切片后的音频也要真的能解码 —— 只拷样本表而漏掉 esds 的实现在这里会露馅。
    let (pcm_bytes, _) = decode_audio_to_pcm(dst.to_str().unwrap()).expect("切片音频无法解码");
    assert!(pcm_bytes > 0, "切片音频解码出 0 字节");

    // 原录制不能被切片破坏。
    let audio_full = probe_audio_track(&src_str).expect("原录制的音频轨被切片破坏了");
    println!(
        "[audio-slice] 原录制音频 {:.2}s / {} 帧",
        audio_full.seconds, audio_full.packet_count
    );
    assert!(
        audio_full.seconds >= audio.seconds,
        "切片不应长于原录制（原 {:.2}s、切片 {:.2}s）",
        audio_full.seconds,
        audio.seconds
    );

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}
