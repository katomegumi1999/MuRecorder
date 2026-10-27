//! 错误码与错误类型。与 C 侧 `RecorderResult` 枚举一一对应。

use std::fmt;

/// C ABI 返回值，数值必须与 recorder.h 完全一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum RecorderResult {
    Ok = 0,
    Invalid = 1,
    NoEncoder = 2,
    DeviceLost = 3,
    Io = 4,
    State = 5,
    Panic = 6,
}

impl RecorderResult {
    pub fn code(self) -> i32 {
        self as i32
    }

    /// 由线程记录的错误码还原枚举（未知码归为 Panic，绝不丢错误）。
    pub fn from_code(code: i32) -> Self {
        match code {
            0 => RecorderResult::Ok,
            1 => RecorderResult::Invalid,
            2 => RecorderResult::NoEncoder,
            3 => RecorderResult::DeviceLost,
            4 => RecorderResult::Io,
            5 => RecorderResult::State,
            _ => RecorderResult::Panic,
        }
    }
}

/// 内部错误类型：错误码 + 可读信息（会经 `recorder_get_last_error` 暴露给 C#）。
#[derive(Debug, Clone)]
pub struct RecorderError {
    code: RecorderResult,
    message: String,
}

#[allow(dead_code)] // no_encoder/device_lost/message 为完整错误 API，M2 起使用
impl RecorderError {
    pub fn new(code: RecorderResult, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(RecorderResult::Invalid, message)
    }

    pub fn state(message: impl Into<String>) -> Self {
        Self::new(RecorderResult::State, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(RecorderResult::Io, message)
    }

    pub fn no_encoder(message: impl Into<String>) -> Self {
        Self::new(RecorderResult::NoEncoder, message)
    }

    pub fn device_lost(message: impl Into<String>) -> Self {
        Self::new(RecorderResult::DeviceLost, message)
    }

    pub fn code(&self) -> RecorderResult {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RecorderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for RecorderError {}

/// 把任意 `Result` 的错误统一折算为 `RecorderError`（保留错误码）。
pub trait IntoRecorderError<T> {
    fn map_rec_err(self, code: RecorderResult, context: &str) -> Result<T, RecorderError>;
}

impl<T, E: fmt::Display> IntoRecorderError<T> for Result<T, E> {
    fn map_rec_err(self, code: RecorderResult, context: &str) -> Result<T, RecorderError> {
        self.map_err(|e| RecorderError::new(code, format!("{context}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_c_header() {
        // 与 recorder.h 的枚举值逐项对齐；改了头文件必须同步这里。
        assert_eq!(RecorderResult::Ok.code(), 0);
        assert_eq!(RecorderResult::Invalid.code(), 1);
        assert_eq!(RecorderResult::NoEncoder.code(), 2);
        assert_eq!(RecorderResult::DeviceLost.code(), 3);
        assert_eq!(RecorderResult::Io.code(), 4);
        assert_eq!(RecorderResult::State.code(), 5);
        assert_eq!(RecorderResult::Panic.code(), 6);
    }

    #[test]
    fn display_contains_message() {
        let e = RecorderError::io("disk full");
        assert!(e.to_string().contains("disk full"));
        assert_eq!(e.code(), RecorderResult::Io);
    }
}
