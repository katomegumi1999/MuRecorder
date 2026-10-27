//! FFI 辅助：C 字符串读写与 panic hook 管理。

use std::ffi::{CStr, CString, c_char};
use std::sync::OnceLock;

/// 安装一次性的静默 panic hook：FFI 边界的 panic 由 `catch_unwind` 转成
/// `REC_ERR_PANIC` 返回码，默认 hook 刷屏到 stderr 对宿主（C# 控制台）是噪音。
static HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

pub fn install_panic_hook_once() {
    HOOK_INSTALLED.get_or_init(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // 保留位置信息以便诊断，但抑制默认的完整回溯输出。
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            eprintln!("[recorder_core] panic at {location}");
            let _ = &default; // 不再调用默认 hook（避免回溯噪音）
        }));
    });
}

/// 读取 C 字符串为 `String`（只读借用后复制）。
///
/// # Safety
/// `ptr` 必须为空或指向调用期间有效的 NUL 结尾字节串。
#[allow(dead_code)] // 通用 FFI 工具，配置读取路径已用 owned 复制；M2 音频设备 ID 直读时启用
pub unsafe fn cstr_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // Safety: 调用方契约保证。
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(|s| s.to_owned())
}

/// 把 `String` 拷入调用方缓冲区（NUL 结尾，超长截断）。
/// 返回写入的字节数（不含 NUL）。
pub fn copy_string_to_buf(s: &str, buf: *mut c_char, len: u32) -> usize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    let cap = (len - 1) as usize;
    let bytes = s.as_bytes();
    let n = bytes.len().min(cap);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast(), n);
        *buf.add(n) = 0;
    }
    n
}

/// `&'static str` → C 字符串指针（进程内只转换一次并缓存，禁止 C# 侧 free）。
///
/// 原始指针本身不实现 Send/Sync，无法直接放进 `static`；以 `usize` 中转。
pub fn static_cstr(s: &'static str) -> *const c_char {
    static CACHE: OnceLock<usize> = OnceLock::new();
    let cached = CACHE.get_or_init(|| {
        // 静态 ASCII 字符串，CString::new 不会失败；指针进程存活期内有效。
        CString::new(s).expect("静态字符串不含 NUL").into_raw() as usize
    });
    *cached as *const c_char
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_string_truncates_and_terminates() {
        let mut buf = [0u8; 8];
        let n = copy_string_to_buf("hello world", buf.as_mut_ptr().cast(), 8);
        assert_eq!(n, 7);
        assert_eq!(&buf[..7], b"hello w");
        assert_eq!(buf[7], 0);
    }

    #[test]
    fn copy_string_zero_len_is_noop() {
        let mut buf = [0xFFu8; 4];
        let n = copy_string_to_buf("abc", buf.as_mut_ptr().cast(), 0);
        assert_eq!(n, 0);
        assert_eq!(buf, [0xFF; 4]);
    }

    #[test]
    fn cstr_roundtrip() {
        let s = CString::new("测试 UTF-8").unwrap();
        let out = unsafe { cstr_to_string(s.as_ptr()) };
        assert_eq!(out.as_deref(), Some("测试 UTF-8"));
        assert!(unsafe { cstr_to_string(std::ptr::null()) }.is_none());
    }
}
