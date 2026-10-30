//! 线程共享环境：停止/致命错误信号 + 错误记录 + 统计。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::RecorderResult;
use crate::stats::StatsSampler;

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
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((code.code(), msg));
    }

    /// 置致命错误并请求全流水线停止。
    ///
    /// `fatal_root` 只保留第一次调用时的信息（根因）；`last_error` 每次都更新
    /// （给 UI 显示最新状态）。两者在单故障场景下相同，多故障级联时才有区别。
    pub fn fail(&self, code: RecorderResult, msg: String) {
        self.fatal.store(true, Ordering::SeqCst);
        self.stats.set_last_error(code.code());
        {
            let mut root = self
                .fatal_root
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if root.is_none() {
                *root = Some((code.code(), msg.clone()));
            }
        }
        self.record(code, msg);
        self.stop.store(true, Ordering::SeqCst);
    }

    /// 根因（首个致命错误），没有则回退到最新错误。
    pub fn root_error(&self) -> Option<(i32, String)> {
        if let Some(e) = self
            .fatal_root
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return Some(e);
        }
        self.last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁中毒（持锁线程 panic）不应让后续线程连锁 panic：与 FFI 边界（lib.rs
    /// `handle_ref`）的策略一致，取回内部值「带病继续」，否则一个线程持锁
    /// panic 会让所有后续 record/fail 连环炸，错误信息全部失真。
    #[test]
    fn record_survives_poisoned_last_error_lock() {
        let env = PipelineEnv {
            stop: Arc::new(AtomicBool::new(false)),
            fatal: Arc::new(AtomicBool::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            fatal_root: Arc::new(Mutex::new(None)),
            stats: Arc::new(StatsSampler::new(crate::stats::ENCODER_SOFT_OPENH264)),
        };

        // 在持锁期间 panic，制造中毒锁。
        let lock = Arc::clone(&env.last_error);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock.lock().unwrap();
            panic!("poison the lock");
        }));

        // 中毒后 record / root_error 必须照常工作。
        env.record(RecorderResult::Io, "after poison".to_string());
        assert_eq!(
            env.root_error(),
            Some((RecorderResult::Io.code(), "after poison".to_string()))
        );
    }
}
