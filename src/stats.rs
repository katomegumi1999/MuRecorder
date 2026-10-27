//! 统计：热路径只做无锁原子累加，stats 线程每 200ms 采样生成快照。
//!
//! 设计约束（对应设计文档 4.5）：
//! - 编码/捕获线程只碰 `AtomicU64::fetch_add`，绝不上锁；
//! - `recorder_get_stats` 只做一次 Mutex 快照拷贝，不触碰热路径。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// 采样周期，与 C# 侧 DispatcherTimer 间隔一致。
pub const SAMPLE_INTERVAL_MS: u64 = 200;

/// C ABI 快照结构（`#[repr(C)]` 与 recorder.h 严格对齐）。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RecorderStatsFfi {
    pub frames_captured: u64,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub current_fps: u32,
    pub bitrate_kbps: u32,
    pub queue_depth: u32,
    pub encoder_type: u32,
    pub last_error: i32,
    pub elapsed_ms: u64,
}

/// 编码器类型码（写入 `RecorderStats.encoder_type`）。
pub const ENCODER_GPU_HEVC: u32 = 1; // GPU H.265 (HEVC)
pub const ENCODER_GPU_H264: u32 = 2; // GPU H.264 (AVC)
pub const ENCODER_SOFT_OPENH264: u32 = 4; // 与 C 侧 encoder=4 (x264/soft) 对齐
pub const ENCODER_GPU_AV1: u32 = 5; // GPU AV1

/// 热路径计数器（无锁）。
#[derive(Debug, Default)]
pub struct AtomicStats {
    frames_captured: AtomicU64,
    frames_encoded: AtomicU64,
    frames_dropped: AtomicU64,
    bytes_encoded: AtomicU64,
    /// 待编码帧数（视频 ring 积压），由捕获线程入队 +1、编码线程出队 -1。
    queue_depth: AtomicUsize,
}

impl AtomicStats {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn on_frame_captured(&self) {
        self.frames_captured.fetch_add(1, Ordering::Relaxed);
        self.queue_depth.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn on_frame_encoded(&self, bytes: u64) {
        self.frames_encoded.fetch_add(1, Ordering::Relaxed);
        self.bytes_encoded.fetch_add(bytes, Ordering::Relaxed);
        // 防止在 0 下方回绕（异常路径理论不可达，防御性处理）。
        let _ = self
            .queue_depth
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    #[inline]
    pub fn on_frame_dropped(&self) {
        self.frames_dropped.fetch_add(1, Ordering::Relaxed);
        // 丢弃的帧从未进入编码队列，只需抵消入队时的 +1。
        let _ = self
            .queue_depth
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    #[inline]
    pub fn queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// 累计捕获帧数（直接读原子量，不受 200ms 采样窗口影响）。
    ///
    /// 收尾路径用它区分「一帧都没拿到」与「拿到了但写盘失败」两种情况，
    /// 从而给出不同的错误文案；读快照会因为采样滞后而误判。
    #[inline]
    pub fn frames_captured(&self) -> u64 {
        self.frames_captured.load(Ordering::Relaxed)
    }
}

/// 采样器：周期性把原子量折算成速率，写入快照。
pub struct StatsSampler {
    stats: AtomicStats,
    snapshot: Mutex<RecorderStatsFfi>,
    last_sample: Mutex<(u64, u64, Instant)>, // (frames_encoded, bytes_encoded, 时刻)
    start: Instant,
    encoder_type: std::sync::atomic::AtomicU32,
    last_error: std::sync::atomic::AtomicI32,
}

impl StatsSampler {
    pub fn new(encoder_type: u32) -> Self {
        Self {
            stats: AtomicStats::new(),
            snapshot: Mutex::new(RecorderStatsFfi::default()),
            last_sample: Mutex::new((0, 0, Instant::now())),
            start: Instant::now(),
            encoder_type: std::sync::atomic::AtomicU32::new(encoder_type),
            last_error: std::sync::atomic::AtomicI32::new(0),
        }
    }

    pub fn stats(&self) -> &AtomicStats {
        &self.stats
    }

    pub fn start_instant(&self) -> Instant {
        self.start
    }

    pub fn set_last_error(&self, code: i32) {
        self.last_error.store(code, Ordering::Relaxed);
    }

    pub fn set_encoder_type(&self, encoder_type: u32) {
        self.encoder_type.store(encoder_type, Ordering::Relaxed);
    }

    /// stats 线程每次 tick 调用：计算滑动窗口速率并刷新快照。
    pub fn sample(&self) {
        let now = Instant::now();
        let (prev_frames, prev_bytes, prev_at) = {
            let g = self.last_sample.lock().unwrap();
            *g
        };
        let dt = now.saturating_duration_since(prev_at);
        let dt_ms = dt.as_millis() as u64;

        let frames = self.stats.frames_encoded.load(Ordering::Relaxed);
        let bytes = self.stats.bytes_encoded.load(Ordering::Relaxed);
        let captured = self.stats.frames_captured.load(Ordering::Relaxed);
        let dropped = self.stats.frames_dropped.load(Ordering::Relaxed);
        let depth = self.stats.queue_depth.load(Ordering::Relaxed);

        // 速率按 dt 线性折算；dt 为 0（极端 tick 抖动）时保持上一周期值。
        // 码率单位是 kbit/s：bytes → bits（×8）→ 每秒 → 千（÷1000）。
        // 早期实现只除 1024（KiB/s），与字段名差 8 倍，UI 上的码率是真实值的 1/8。
        let (current_fps, bitrate_kbps) = if dt_ms > 0 {
            let df = frames.saturating_sub(prev_frames);
            let db = bytes.saturating_sub(prev_bytes);
            let fps = (df as u128 * 1000 / dt_ms as u128) as u32;
            let kbps = (db as u128 * 8_000 / dt_ms as u128 / 1_000) as u32;
            (fps, kbps)
        } else {
            let snap = self.snapshot.lock().unwrap();
            (snap.current_fps, snap.bitrate_kbps)
        };

        {
            let mut last = self.last_sample.lock().unwrap();
            *last = (frames, bytes, now);
        }

        let mut snap = self.snapshot.lock().unwrap();
        snap.frames_captured = captured;
        snap.frames_encoded = frames;
        snap.frames_dropped = dropped;
        snap.current_fps = current_fps;
        snap.bitrate_kbps = bitrate_kbps;
        snap.queue_depth = depth.min(u32::MAX as usize) as u32;
        snap.encoder_type = self.encoder_type.load(Ordering::Relaxed);
        snap.last_error = self.last_error.load(Ordering::Relaxed);
        snap.elapsed_ms = now.saturating_duration_since(self.start).as_millis() as u64;
    }

    /// `recorder_get_stats`：一次读锁拷贝，纳秒级。
    pub fn snapshot(&self) -> RecorderStatsFfi {
        let mut snap = *self.snapshot.lock().unwrap();
        snap.encoder_type = self.encoder_type.load(Ordering::Relaxed);
        snap.last_error = self.last_error.load(Ordering::Relaxed);
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn counters_track_queue_depth() {
        let s = AtomicStats::new();
        s.on_frame_captured();
        s.on_frame_captured();
        assert_eq!(s.queue_depth(), 2);
        s.on_frame_encoded(100);
        assert_eq!(s.queue_depth(), 1);
        s.on_frame_dropped();
        assert_eq!(s.queue_depth(), 0);
        // 防御性：0 下方不回绕
        s.on_frame_dropped();
        assert_eq!(s.queue_depth(), 0);
    }

    #[test]
    fn sampler_computes_rates() {
        let sampler = StatsSampler::new(ENCODER_SOFT_OPENH264);
        // 第一个采样窗口前先记一帧，制造 200ms 内 5 帧 / 5120 字节
        // → 25 fps、5120 B / 0.2 s × 8 = 204.8 kbit/s
        sampler.stats().on_frame_captured();
        sampler.stats().on_frame_encoded(1024);
        sampler.sample();
        sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
        for _ in 0..5 {
            sampler.stats().on_frame_captured();
            sampler.stats().on_frame_encoded(1024);
        }
        sampler.sample();

        let snap = sampler.snapshot();
        assert_eq!(snap.frames_encoded, 6);
        assert_eq!(snap.encoder_type, ENCODER_SOFT_OPENH264);
        assert!(
            snap.current_fps >= 20 && snap.current_fps <= 30,
            "fps={}",
            snap.current_fps
        );
        assert!(
            (190..=215).contains(&snap.bitrate_kbps),
            "kbps={}（5120 B / 200 ms ≈ 204.8 kbit/s）",
            snap.bitrate_kbps
        );
        assert!(snap.elapsed_ms >= SAMPLE_INTERVAL_MS);
    }
}
