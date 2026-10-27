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

/// 饱和自减：计数到 0 就停，不会回绕成 `usize::MAX`。
///
/// 不用 `AtomicUsize::fetch_update`：新版本 stable 已把它改名为 `try_update`
/// 并弃用旧名（`-D warnings` 下直接编译失败），而 `try_update` 在 MSRV 1.85
/// 上还不存在——两端各有一半环境不可用。这里手写 CAS 循环，语义相同且与
/// 工具链版本无关。
#[inline]
fn saturating_dec(counter: &AtomicUsize) {
    let mut cur = counter.load(Ordering::Relaxed);
    while cur > 0 {
        match counter.compare_exchange_weak(cur, cur - 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => cur = actual,
        }
    }
}

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
        saturating_dec(&self.queue_depth);
    }

    #[inline]
    pub fn on_frame_dropped(&self) {
        self.frames_dropped.fetch_add(1, Ordering::Relaxed);
        // 丢弃的帧从未进入编码队列，只需抵消入队时的 +1。
        saturating_dec(&self.queue_depth);
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
    use std::time::{Duration, Instant};

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
        // 第一个采样窗口前先记一帧，后面再记 5 帧 / 5120 字节。
        sampler.stats().on_frame_captured();
        sampler.stats().on_frame_encoded(1024);
        sampler.sample();

        // 期望值按**实测窗口**推算，而不是假设 sleep 恰好睡了 200 ms。
        // 这条断言原本写死「200ms → 25 fps / 204.8 kbit/s」，可是机器一忙
        // （例如同时在编译另一个 workspace）sleep 就会超到 220 ms 以上，
        // 码率随之跌到 186 kbit/s、fps 跌到 22 以下，于是随机假失败。
        // 该用例要验的是「计数器 → 速率」的换算公式，不是 OS 的睡眠精度。
        let window = Instant::now();
        sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
        for _ in 0..5 {
            sampler.stats().on_frame_captured();
            sampler.stats().on_frame_encoded(1024);
        }
        sampler.sample();
        let dt_ms = window.elapsed().as_millis().max(1);

        let snap = sampler.snapshot();
        assert_eq!(snap.frames_encoded, 6);
        assert_eq!(snap.encoder_type, ENCODER_SOFT_OPENH264);

        // sample() 内部用自己的 Instant 取窗口，与上面的起点差两次取时的开销，
        // 整除截断也可能差 1，所以允许 ±2。
        let expect_fps = (5u128 * 1000 / dt_ms) as u32;
        let expect_kbps = (5 * 1024u128 * 8_000 / dt_ms / 1_000) as u32;
        assert!(
            snap.current_fps.abs_diff(expect_fps) <= 2,
            "fps={} 期望≈{}（实测窗口 {} ms）",
            snap.current_fps,
            expect_fps,
            dt_ms
        );
        assert!(
            snap.bitrate_kbps.abs_diff(expect_kbps) <= 2,
            "kbps={} 期望≈{}（实测窗口 {} ms）",
            snap.bitrate_kbps,
            expect_kbps,
            dt_ms
        );
        assert!(snap.elapsed_ms >= SAMPLE_INTERVAL_MS);
    }
}
