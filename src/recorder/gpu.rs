//! GPU 零拷贝录制线程：DXGI 纹理采集 + 显存裁剪 + MF 硬件编码 + 封装单线程闭环。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::capture::DxgiCapture;
use crate::capture::dxgi;
use crate::config::RecordingConfig;
use crate::encode::mf_sink::GpuSinkWriter;
use crate::error::RecorderResult;

use super::env::PipelineEnv;
use super::{
    ANIMATING_ACQUIRE_TIMEOUT_MS, FIRST_FRAME_ACQUIRE_TIMEOUT_MS, FIRST_FRAME_GDI_FALLBACK_MS,
    FIRST_FRAME_GDI_RETRY_MS, FRAME_ACQUIRE_TIMEOUT_MS, MAX_REBUILD_ATTEMPTS, REBUILD_INTERVAL,
};

/// GPU 零拷贝直接录制线程（DXGI 纹理采集 + 显存裁剪 + MF 硬件编码 + MP4封装在单线程中闭环）。
pub(super) fn gpu_recorder_thread(
    cfg: RecordingConfig,
    mut capture: DxgiCapture,
    mut gpu_sink: GpuSinkWriter,
    t0: Arc<AtomicU64>,
    audio_rx: Option<std::sync::mpsc::Receiver<(Vec<u8>, u64)>>,
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
            FIRST_FRAME_ACQUIRE_TIMEOUT_MS
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
        let note = super::soft::preserve_or_remove(&cfg.output_path, frames);
        env.fail(RecorderResult::Io, format!("{msg}{note}"));
    }
    // `com` 在这里（形参之后）才析构，COM 对象才不会悬空。
}
