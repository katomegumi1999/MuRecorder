//! MuRecorder C ABI 导出层。
//!
//! 铁律（对应设计文档 3.2，评审清单）：
//! 1. `recorder_create` 只分配、不启动；线程在 `recorder_start` 中创建。
//! 2. `recorder_stop` / `recorder_destroy` 均幂等；`destroy` 内部先 `stop` 再 join。
//! 3. 每个导出函数 `catch_unwind` 包裹，panic 转为 `REC_ERR_PANIC`，
//!    绝不让 panic 展开到 C# 宿主（release profile 因此必须保持 unwind）。
//! 4. 字符串参数只读、不持有，调用返回后 C# 可立即释放。
//! 5. `RecorderConfig` 首字段固定 `abi_version`，新增字段只能尾部追加。
//! 6. 调用约定 `extern "C"`，Windows x64 即 Microsoft x64 ABI。
//! 7. 句柄内部是 `Mutex<Recorder>`：宿主可以从任意线程并发调用（典型场景是 UI 线程
//!    每 200ms 轮询 stats、同时另一个线程点停止），所有导出函数在锁上串行化。
//!    `Recorder` 本身不是 Send/Sync，绝不能把它直接交出去当 `&mut`。

// MSVC 链接器把「正在创建库 xxx.lib 和对象 xxx.exp」写到 stdout，rustc 会把它
// 当作 linker_messages 警告抛出来。cdylib 的 .lib/.exp 我们并不使用，属纯噪声；
// 真正的链接问题是 error 而非 warning，允许该通道不影响故障可见性。
#![allow(linker_messages)]

pub mod capture;
pub mod config;
pub mod convert;
pub mod encode;
pub mod error;
mod ffi;
pub mod mux;
pub mod recorder;
pub mod stats;

use std::ffi::c_char;
use std::os::raw::c_int;
use std::sync::{Mutex, MutexGuard};

use config::{RecorderConfigFfi, RecordingConfig};
use error::{RecorderError, RecorderResult};
use recorder::{Recorder, RecorderState};
use stats::RecorderStatsFfi;

/// 不透明句柄（C 侧只见指针）。零大小 + `#[repr(C)]` 保证指针转换安全。
#[repr(C)]
pub struct RecorderHandle {
    _private: [u8; 0],
}

/// 句柄实际指向的内容：`Mutex<Recorder>`。
///
/// 加锁不是为了性能，是为了**正确性**：宿主完全可能在一个线程里轮询 stats，
/// 另一个线程里 stop。若像早期实现那样直接把裸指针转成 `&mut Recorder`，
/// 两个线程就会同时持有 `&mut`（未定义行为），且 `Recorder` 本身不是 `Send`。
type HandleInner = Mutex<Recorder>;

/// FFI 统一守卫：panic → REC_ERR_PANIC；RecorderError → 对应错误码。
fn guard<F>(f: F) -> c_int
where
    F: FnOnce() -> Result<c_int, RecorderError>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => e.code() as c_int,
        Err(_) => RecorderResult::Panic as c_int,
    }
}

/// 空指针检查 + 加锁，返回 RAII 守卫。
///
/// 锁中毒（持锁线程 panic）时不复用 panic，直接取回内部值继续——
/// 在 FFI 边界上"带病继续"远好过把整个宿主拖死。
fn handle_ref<'a>(h: *mut RecorderHandle) -> Result<MutexGuard<'a, Recorder>, RecorderError> {
    if h.is_null() {
        return Err(RecorderError::invalid("RecorderHandle 为空指针"));
    }
    // Safety: 非空指针由 recorder_create 产出（指向 HandleInner），生命周期内有效。
    let inner = unsafe { &*h.cast::<HandleInner>() };
    Ok(inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()))
}

/// # Safety
/// `cfg` 必须指向调用期间有效的 `RecorderConfigFfi`（含有效的 C 字符串指针）；
/// `out` 必须指向可写的 `*mut RecorderHandle`（初值 null）。
/// 这是 C ABI 函数，指针契约由调用方（C# [LibraryImport] 绑定）保证。
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn recorder_create(
    cfg: *const RecorderConfigFfi,
    out: *mut *mut RecorderHandle,
) -> c_int {
    ffi::install_panic_hook_once();
    guard(|| {
        if cfg.is_null() || out.is_null() {
            return Err(RecorderError::invalid("cfg 或 out 为空指针"));
        }
        // Safety: 调用方契约 + 非空检查。
        let ffi = unsafe { &*cfg };
        let config: RecordingConfig = unsafe { RecordingConfig::from_ffi(ffi) }?;
        let rec = Recorder::created(config)?;
        // Safety: out 非空；要求调用方传入的 *out 初值为 null（不覆盖未知指针）。
        unsafe {
            *out = Box::into_raw(Box::new(Mutex::new(rec))).cast::<RecorderHandle>();
        }
        Ok(RecorderResult::Ok as c_int)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn recorder_start(h: *mut RecorderHandle) -> c_int {
    guard(|| {
        let mut rec = handle_ref(h)?;
        if rec.state() != RecorderState::Created {
            return Err(RecorderError::state(
                "重复 start 或状态非法（仅 Created 可启动）",
            ));
        }
        rec.start()?;
        Ok(RecorderResult::Ok as c_int)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn recorder_stop(h: *mut RecorderHandle) -> c_int {
    guard(|| {
        let mut rec = handle_ref(h)?;
        // 幂等：Stopped 直接返回 OK。
        if rec.state() == RecorderState::Stopped {
            return Ok(RecorderResult::Ok as c_int);
        }
        rec.stop()?;
        Ok(RecorderResult::Ok as c_int)
    })
}

/// # Safety
/// `out` 必须指向可写的 `RecorderStatsFfi`。C ABI 契约由调用方保证。
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn recorder_get_stats(h: *mut RecorderHandle, out: *mut RecorderStatsFfi) -> c_int {
    guard(|| {
        if out.is_null() {
            return Err(RecorderError::invalid("out 为空指针"));
        }
        let rec = handle_ref(h)?;
        let snap = rec.stats_snapshot();
        // Safety: out 非空；快照为 repr(C) POD。
        unsafe {
            *out = snap;
        }
        Ok(RecorderResult::Ok as c_int)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn recorder_get_last_error(
    h: *mut RecorderHandle,
    buf: *mut c_char,
    len: u32,
) -> c_int {
    guard(|| {
        if buf.is_null() || len == 0 {
            return Err(RecorderError::invalid("buf 为空或 len 为 0"));
        }
        let rec = handle_ref(h)?;
        let msg = rec.last_error_string();
        let written = ffi::copy_string_to_buf(&msg, buf, len);
        // 返回值约定：REC_OK；实际写入字节数由 C# 侧按需读取（此处仅状态码）。
        let _ = written;
        Ok(RecorderResult::Ok as c_int)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn recorder_destroy(h: *mut RecorderHandle) {
    // destroy 无返回值；内部先 stop（幂等）再释放。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if h.is_null() {
            return;
        }
        // Safety: h 由 create 产出且未被 destroy 过（幂等由状态机外的
        // 「指针置 null」约定保证——C# SafeHandle 保证只释放一次）。
        // Safety: 句柄由 create 产出且只释放一次（C# SafeHandle 保证）。
        // 先加锁 stop，再把 Box 还原并释放——锁保证不会与其它线程的调用交错。
        {
            let inner = unsafe { &*h.cast::<HandleInner>() };
            let mut rec = inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if rec.state() == RecorderState::Running {
                let _ = rec.stop();
            }
        }
        drop(unsafe { Box::from_raw(h.cast::<HandleInner>()) });
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn recorder_version() -> *const c_char {
    // 静态版本串，禁止 C# 侧 free。
    ffi::static_cstr(env!("CARGO_PKG_VERSION"))
}

#[unsafe(no_mangle)]
pub extern "C" fn recorder_has_hardware_encoder(encoder: u32) -> c_int {
    let pref = match crate::config::EncoderPreference::from_u32(encoder) {
        Some(p) => p,
        None => return 0,
    };
    let subtype = match pref {
        crate::config::EncoderPreference::GpuHevc => {
            &windows::Win32::Media::MediaFoundation::MFVideoFormat_HEVC
        }
        crate::config::EncoderPreference::GpuH264 => {
            &windows::Win32::Media::MediaFoundation::MFVideoFormat_H264
        }
        crate::config::EncoderPreference::GpuAv1 => {
            &windows::Win32::Media::MediaFoundation::MFVideoFormat_AV1
        }
        _ => return 0,
    };
    if crate::encode::mf_sink::has_hardware_encoder(subtype) {
        1
    } else {
        0
    }
}

/// 从**录制中**的 MP4 里切出 `[start_us, end_us)`，写成独立的普通 MP4。
///
/// 不需要停止录制，也不占用录制线程：整段处理是纯文件 I/O（零重编码），
/// 可以在 UI 线程调用（大区间会阻塞几百毫秒）。
///
/// **与编码器无关**：切片照抄源文件的 `stsd`（`avcC`/`hvcC`/`av1C` 一个字节不改）
/// 再整段复制样本字节，所以这条路径不看编码器。
///
/// 要注意的是**容器**而不是编码器：只有分片式 MP4（`moof` 自带样本表）才能切。
/// 软编、GPU H.264、GPU HEVC 都用分片写，录制中即可切；只有 GPU AV1 会以普通
/// MP4 落盘（自研封装暂不支持 OBU 样本），这种文件要等停止录制、写完 `moov`
/// 之后才能处理（此时本函数会返回 `REC_ERR_IO` 并说明原因）。
///
/// 起点会对齐到覆盖 `start_us` 的最近关键帧（没有关键帧无法独立解码），
/// 终点精确裁到 `end_us` 之内。实际区间由 `out_duration_us` 回传。
///
/// # Safety
/// `src_path` / `dst_path` 必须指向调用期间有效的 NUL 结尾 UTF-8 字符串；
/// `out_duration_us` 可为 null。
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn recorder_extract_range(
    src_path: *const u8,
    dst_path: *const u8,
    start_us: u64,
    end_us: u64,
    fps: u32,
    out_duration_us: *mut u64,
) -> c_int {
    guard(|| {
        // Safety: 调用方契约（NUL 结尾 UTF-8）。
        let src = unsafe { config::read_cstr(src_path) }?
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| RecorderError::invalid("src_path 为空"))?;
        // Safety: 同上。
        let dst = unsafe { config::read_cstr(dst_path) }?
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| RecorderError::invalid("dst_path 为空"))?;

        let info = crate::mux::extract::extract_range(
            std::path::Path::new(&src),
            std::path::Path::new(&dst),
            start_us,
            end_us,
            fps.max(1),
        )
        .map_err(|e| RecorderError::io(format!("切片失败: {e}")))?;

        if !out_duration_us.is_null() {
            // Safety: 非空且由调用方保证可写。
            unsafe {
                *out_duration_us = info.duration_us;
            }
        }
        Ok(RecorderResult::Ok as c_int)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_maps_panic_to_panic_code() {
        let code = guard(|| -> Result<c_int, RecorderError> {
            panic!("boom");
        });
        assert_eq!(code, RecorderResult::Panic as c_int);
    }

    #[test]
    fn guard_maps_error_to_code() {
        let code = guard(|| Err::<c_int, _>(RecorderError::io("x")));
        assert_eq!(code, RecorderResult::Io as c_int);
    }

    #[test]
    fn null_handle_rejected() {
        let code = recorder_start(std::ptr::null_mut());
        assert_eq!(code, RecorderResult::Invalid as c_int);
    }

    #[test]
    fn version_is_non_null_static() {
        let v = recorder_version();
        assert!(!v.is_null());
    }

    #[test]
    fn hardware_encoder_probe_does_not_crash() {
        // 探测 H.264, HEVC, AV1 以及无效编码器
        let _ = recorder_has_hardware_encoder(1); // HEVC
        let _ = recorder_has_hardware_encoder(2); // H264
        let _ = recorder_has_hardware_encoder(5); // AV1
        let invalid = recorder_has_hardware_encoder(999);
        assert_eq!(invalid, 0);
    }
}
