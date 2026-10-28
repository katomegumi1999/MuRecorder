//! 录制配置：C ABI 输入结构体的安全 Rust 视图 + 校验。
//!
//! C# 只传「标识」：区域物理像素坐标、输出路径、设备 ID 字符串。
//! 字符串在 Rust 侧只读、不持有（调用期间借用，返回后即可释放）。

use crate::error::RecorderError;

/// ABI 版本，必须与 C 侧 `RECORDER_ABI_VERSION` 一致。
///
/// ## 版本历史
///
/// * **v1** —— 首版：视频区域/帧率/编码器/码率 + 音频音源四字段。
/// * **v2** —— 尾部追加 `audio_bitrate_kbps`（音频编码码率，kbps）。
///
/// ## 为什么 v1 → v2 必须升版本，而不是「尾部追加天然兼容」
///
/// 「只追加尾部字段」在**宿主与库一起重编译**时确实安全，但 ABI 的契约是
/// **已编译的旧宿主能继续用新 dll**。旧宿主的 `RecorderConfig` 比新 dll 的
/// 短 4 字节，`read_cstr` 之后的 `audio_process_id` 之后就会读到调用方栈上的
/// 垃圾值，被当成 `audio_bitrate_kbps` 校验 → 大概率 `REC_ERR_INVALID`（看起来
/// 像误报），小概率凑巧落在合法档位（变成静默的错误码率）。
/// 更糟的是越界读本身就是 UB。
///
/// 与其让旧宿主拿到不可解释的失败，不如让它拿到一句明确的
/// 「abi_version 不匹配：期望 2，实际 1」——**宁可拒绝，不要 UB**。
/// 升级成本只有 C# 侧 `AbiVersion` 常量改一个数字。
pub const ABI_VERSION: u32 = 2;

/// 音源类型，与 C 侧 `AudioSourceType` 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AudioSourceType {
    None = 0,
    System = 1,
    Device = 2,
    Window = 3,
}

impl AudioSourceType {
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::System),
            2 => Some(Self::Device),
            3 => Some(Self::Window),
            _ => None,
        }
    }
}

/// 编码器选择偏好（C 侧 `encoder` 字段语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPreference {
    Auto = 0,
    GpuHevc = 1,
    GpuH264 = 2,
    Amf = 3,
    Soft = 4,
    GpuAv1 = 5,
}

impl EncoderPreference {
    pub const NVENC: Self = Self::GpuHevc;
    pub const QSV: Self = Self::GpuH264;

    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Auto),
            1 => Some(Self::GpuHevc),
            2 => Some(Self::GpuH264),
            3 => Some(Self::Amf),
            4 => Some(Self::Soft),
            5 => Some(Self::GpuAv1),
            _ => None,
        }
    }
}

/// 校验后的配置（Rust 内部表示）。字符串已复制为 owned，生命周期自洽。
#[derive(Debug, Clone)]
pub struct RecordingConfig {
    pub region_x: i32,
    pub region_y: i32,
    pub region_w: u32,
    pub region_h: u32,
    pub fps: u32,
    pub encoder: EncoderPreference,
    pub bitrate_kbps: u32,
    pub capture_cursor: bool,
    pub output_path: String,
    pub audio_source: AudioSourceType,
    pub audio_device_id: Option<String>,
    pub audio_window_hwnd: u64,
    pub audio_process_id: u32,
    pub audio_bitrate_kbps: u32,
}

/// C ABI 传入的原始配置（`#[repr(C)]` 与 recorder.h 严格对齐）。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RecorderConfigFfi {
    pub abi_version: u32,
    pub region_x: i32,
    pub region_y: i32,
    pub region_w: u32,
    pub region_h: u32,
    pub fps: u32,
    pub encoder: u32,
    pub bitrate_kbps: u32,
    pub capture_cursor: u32,
    pub output_path: *const u8,
    pub audio_source_type: u32,
    pub audio_device_id: *const u8,
    pub audio_window_hwnd: u64,
    pub audio_process_id: u32,
    pub audio_bitrate_kbps: u32,
}

/// 从 FFI 指针读取 C 字符串（不持有，只读借用）。
///
/// # Safety
/// `ptr` 必须为空或指向调用期间有效的 NUL 结尾 UTF-8 字节串。
pub(crate) unsafe fn read_cstr(ptr: *const u8) -> Result<Option<String>, RecorderError> {
    if ptr.is_null() {
        return Ok(None);
    }
    // Safety: 调用方保证指针在调用期间有效且以 NUL 结尾。
    let cstr = unsafe { std::ffi::CStr::from_ptr(ptr.cast()) };
    let s = cstr
        .to_str()
        .map_err(|_| RecorderError::invalid("字符串参数不是合法 UTF-8"))?;
    Ok(Some(s.to_owned()))
}

/// FPS / 码率合理区间（超出按非法参数拒绝，避免编码器内部换算溢出）。
const MIN_FPS: u32 = 1;
const MAX_FPS: u32 = 240;
const MIN_BITRATE_KBPS: u32 = 100;
const MAX_BITRATE_KBPS: u32 = 200_000;
/// 单边像素上限（8K 上限，防止 0 或天文数字传入）。
const MAX_DIMENSION: u32 = 16_384;

impl RecordingConfig {
    /// 编码器要求偶数边长（4:2:0 宏块/色度抽样）。
    ///
    /// 采集、编码、封装三处**必须使用同一组对齐后的尺寸**：早期封装用的是
    /// `region_w/h` 原值，而编码器对齐到偶数，奇数宽高时 avc1 里声明的尺寸
    /// 与真实码流不一致，播放器会显示错位或拉伸。
    pub fn aligned_size(&self) -> (u32, u32) {
        ((self.region_w & !1).max(2), (self.region_h & !1).max(2))
    }

    /// 校验并转换 FFI 配置。
    ///
    /// # Safety
    /// `ffi` 必须指向调用期间有效的 `RecorderConfigFfi`；
    /// 其中指针字段的约束同 [`read_cstr`]。
    pub unsafe fn from_ffi(ffi: &RecorderConfigFfi) -> Result<Self, RecorderError> {
        if ffi.abi_version != ABI_VERSION {
            return Err(RecorderError::invalid(format!(
                "abi_version 不匹配：期望 {ABI_VERSION}，实际 {}",
                ffi.abi_version
            )));
        }

        let fps = ffi.fps;
        if !(MIN_FPS..=MAX_FPS).contains(&fps) {
            return Err(RecorderError::invalid(format!(
                "fps 超出范围 [{MIN_FPS}, {MAX_FPS}]：{fps}"
            )));
        }

        let bitrate = ffi.bitrate_kbps;
        if !(MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS).contains(&bitrate) {
            return Err(RecorderError::invalid(format!(
                "bitrate_kbps 超出范围 [{MIN_BITRATE_KBPS}, {MAX_BITRATE_KBPS}]：{bitrate}"
            )));
        }

        let (w, h) = (ffi.region_w, ffi.region_h);
        if w == 0 || h == 0 || w > MAX_DIMENSION || h > MAX_DIMENSION {
            return Err(RecorderError::invalid(format!(
                "region 尺寸非法：{w}x{h}（须在 1..={MAX_DIMENSION}）"
            )));
        }

        let encoder = EncoderPreference::from_u32(ffi.encoder)
            .ok_or_else(|| RecorderError::invalid(format!("未知 encoder 值：{}", ffi.encoder)))?;

        // Safety: 由本函数的调用契约保证。
        let output_path = unsafe { read_cstr(ffi.output_path) }?
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| RecorderError::invalid("output_path 为空"))?;

        let audio_source = AudioSourceType::from_u32(ffi.audio_source_type).ok_or_else(|| {
            RecorderError::invalid(format!("未知 audio_source_type：{}", ffi.audio_source_type))
        })?;

        // Safety: 同上。
        let audio_device_id = unsafe { read_cstr(ffi.audio_device_id) }?;

        if audio_source == AudioSourceType::Device && audio_device_id.is_none() {
            return Err(RecorderError::invalid(
                "AUDIO_DEVICE 音源要求提供 audio_device_id",
            ));
        }
        if audio_source == AudioSourceType::Window && ffi.audio_window_hwnd == 0 {
            return Err(RecorderError::invalid(
                "AUDIO_WINDOW 音源要求提供 audio_window_hwnd",
            ));
        }

        let audio_bitrate_kbps = match ffi.audio_bitrate_kbps {
            0 => 320,
            kbps @ (96 | 128 | 160 | 192 | 256 | 320) => kbps,
            other => {
                return Err(RecorderError::invalid(format!(
                    "不支持的 audio_bitrate_kbps: {other}，须为 0（默认 320）或 [96, 128, 160, 192, 256, 320] 之一"
                )));
            }
        };

        Ok(Self {
            region_x: ffi.region_x,
            region_y: ffi.region_y,
            region_w: w,
            region_h: h,
            fps,
            encoder,
            bitrate_kbps: bitrate,
            capture_cursor: ffi.capture_cursor != 0,
            output_path,
            audio_source,
            audio_device_id,
            audio_window_hwnd: ffi.audio_window_hwnd,
            audio_process_id: ffi.audio_process_id,
            audio_bitrate_kbps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_ffi() -> RecorderConfigFfi {
        RecorderConfigFfi {
            abi_version: ABI_VERSION,
            region_x: 0,
            region_y: 0,
            region_w: 1920,
            region_h: 1080,
            fps: 30,
            encoder: 0,
            bitrate_kbps: 8000,
            capture_cursor: 0,
            output_path: c"out.mp4".as_ptr().cast(),
            audio_source_type: 0,
            audio_device_id: std::ptr::null(),
            audio_window_hwnd: 0,
            audio_process_id: 0,
            audio_bitrate_kbps: 0,
        }
    }

    #[test]
    fn valid_config_passes() {
        let cfg = unsafe { RecordingConfig::from_ffi(&base_ffi()) }.unwrap();
        assert_eq!(cfg.region_w, 1920);
        assert_eq!(cfg.fps, 30);
        assert_eq!(cfg.output_path, "out.mp4");
        assert_eq!(cfg.audio_source, AudioSourceType::None);
    }

    #[test]
    fn abi_version_mismatch_rejected() {
        let mut ffi = base_ffi();
        ffi.abi_version = 99;
        let err = unsafe { RecordingConfig::from_ffi(&ffi) }.unwrap_err();
        assert_eq!(err.code().code(), 1); // REC_ERR_INVALID
    }

    /// v1 是「加了尾部字段但没升版本」的那个版本。
    ///
    /// 必须被**明确拒绝**而不是放行：v1 结构体比 v2 短 4 字节，
    /// 放行会让 `audio_bitrate_kbps` 读到调用方栈上的垃圾。拒绝信息要能
    /// 直接指导宿主改代码（期望 v 实际 v）。
    #[test]
    fn abi_v1_is_rejected_with_actionable_message() {
        assert_eq!(ABI_VERSION, 2, "本测试锁定 v2；升版本时请连带更新");

        let mut ffi = base_ffi();
        ffi.abi_version = 1;
        let err = unsafe { RecordingConfig::from_ffi(&ffi) }.unwrap_err();
        assert_eq!(err.code().code(), 1); // REC_ERR_INVALID

        let msg = err.message();
        assert!(
            msg.contains("abi_version"),
            "错误信息应点名 abi_version：{msg}"
        );
        assert!(msg.contains('2'), "错误信息应写出期望版本：{msg}");
        assert!(msg.contains('1'), "错误信息应写出实际版本：{msg}");
    }

    #[test]
    fn zero_dimension_rejected() {
        let mut ffi = base_ffi();
        ffi.region_w = 0;
        assert!(unsafe { RecordingConfig::from_ffi(&ffi) }.is_err());
    }

    #[test]
    fn fps_out_of_range_rejected() {
        let mut ffi = base_ffi();
        ffi.fps = 0;
        assert!(unsafe { RecordingConfig::from_ffi(&ffi) }.is_err());
        ffi.fps = 1000;
        assert!(unsafe { RecordingConfig::from_ffi(&ffi) }.is_err());
    }

    #[test]
    fn device_audio_requires_id() {
        let mut ffi = base_ffi();
        ffi.audio_source_type = 2;
        ffi.audio_device_id = std::ptr::null();
        assert!(unsafe { RecordingConfig::from_ffi(&ffi) }.is_err());
    }

    #[test]
    fn window_audio_requires_hwnd() {
        let mut ffi = base_ffi();
        ffi.audio_source_type = 3;
        ffi.audio_window_hwnd = 0;
        assert!(unsafe { RecordingConfig::from_ffi(&ffi) }.is_err());
    }

    #[test]
    fn null_output_path_rejected() {
        let mut ffi = base_ffi();
        ffi.output_path = std::ptr::null();
        assert!(unsafe { RecordingConfig::from_ffi(&ffi) }.is_err());
    }

    #[test]
    fn audio_bitrate_defaults_to_320_when_zero() {
        let mut ffi = base_ffi();
        ffi.audio_bitrate_kbps = 0;
        let cfg = unsafe { RecordingConfig::from_ffi(&ffi) }.unwrap();
        assert_eq!(cfg.audio_bitrate_kbps, 320);
    }

    #[test]
    fn audio_bitrate_accepts_supported_tiers() {
        for &tier in &[96, 128, 160, 192, 256, 320] {
            let mut ffi = base_ffi();
            ffi.audio_bitrate_kbps = tier;
            let cfg = unsafe { RecordingConfig::from_ffi(&ffi) }.unwrap();
            assert_eq!(cfg.audio_bitrate_kbps, tier);
        }
    }

    #[test]
    fn audio_bitrate_rejects_unsupported_values() {
        for &invalid in &[32, 64, 100, 200, 384, 512] {
            let mut ffi = base_ffi();
            ffi.audio_bitrate_kbps = invalid;
            assert!(unsafe { RecordingConfig::from_ffi(&ffi) }.is_err());
        }
    }
}
