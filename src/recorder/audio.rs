//! 音频链路：音源解析、start 阶段预检、静音补包与 WASAPI loopback 主循环。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::capture::wasapi::{AudioCapture, AudioSourceSpec};
use crate::config::{AudioSourceType, RecordingConfig};
use crate::encode::aac::AacEncoder;
use crate::error::{RecorderError, RecorderResult};

use super::env::PipelineEnv;

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

/// 把配置里的音源类型翻译成 WASAPI 采集规格；顺带校验必填参数。
pub(super) fn resolve_audio_spec(
    cfg: &RecordingConfig,
) -> Result<Option<AudioSourceSpec>, RecorderError> {
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

/// 在 `recorder_start` 阶段预检音频链路，让不可用的情况**立刻**报错。
///
/// 单独起一个线程做这件事，是因为 COM 套间是按线程的，而 `recorder_start` 的
/// 调用方是宿主（多半是 C# 的 UI 线程）。我们既不该假设它初始化过 COM，
/// 更不该在别人的线程上改套间模型——那会污染宿主的 COM 使用方式。
///
/// `need_aac` 为真时还会实例化一次 AAC MFT：这一步能提前抓到「系统缺少 AAC
/// 编码器」这类环境问题，而不是录到一半才发现。
pub(super) fn preflight_audio(
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
pub(super) fn audio_capture_loop<F>(
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
