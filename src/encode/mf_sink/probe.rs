//! 硬件 MFT 编码器探测。

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MFMediaType_Video, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
};
use windows::Win32::System::Com::CoTaskMemFree;

use super::converter::MfRuntimeGuard;

/// 检查系统是否有支持指定视频子格式的硬件 MFT 编码器
pub fn has_hardware_encoder(subtype: &windows::core::GUID) -> bool {
    let _guard = match MfRuntimeGuard::new() {
        Ok(g) => g,
        Err(_) => return false,
    };

    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *subtype,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;

    let hr = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )
    };

    if hr.is_ok() && !activates.is_null() {
        unsafe {
            for i in 0..count as usize {
                let elem_ptr = activates.add(i);
                let _ = std::ptr::read(elem_ptr);
            }
            CoTaskMemFree(Some(activates as _));
        }
    }

    hr.is_ok() && count > 0
}
