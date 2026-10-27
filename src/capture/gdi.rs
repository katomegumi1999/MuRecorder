//! 一次性 GDI 抓屏：**仅用于补首帧**。
//!
//! ## 为什么需要
//! DXGI 桌面复制只在画面发生变化时交付帧。若录制区域长期静止（典型：副屏上的静态
//! 文档/图表，而主屏才在操作），[`super::dxgi`] 的采集循环会一直拿到
//! `DXGI_ERROR_WAIT_TIMEOUT`，连**首帧**都拿不到，整段录制最终以「未捕获到任何帧」
//! 失败。用户点下开始录制时，画面完全有可能就是静止的。
//!
//! 这里用一次 `BitBlt` 抓当前区域补上首帧；此后仍由 DXGI 接管（变化驱动 + 重发副本
//! 维持 CFR）。GDI 路径只做像素搬运，不参与日常热路径。
//!
//! ## 与 DXGI 的差异
//! GDI 抓的是「屏幕合成结果」，不含硬件覆盖层（部分全屏独占视频/游戏看不到），
//! 因此只作兜底、不作常规路径。

use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, RGBQUAD,
    ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::core::{Error as WinError, Result as WinResult};

use super::dxgi::{CapturedFrame, qpc_now};

/// 抓取虚拟桌面坐标 `(x, y, w, h)` 的当前画面，返回紧凑行距的 BGRA 帧。
pub fn grab_region(x: i32, y: i32, w: u32, h: u32) -> WinResult<CapturedFrame> {
    if w == 0 || h == 0 || w > i32::MAX as u32 || h > i32::MAX as u32 {
        return Err(WinError::from_hresult(E_INVALIDARG));
    }
    let (wi, hi) = (w as i32, h as i32);

    unsafe {
        let screen = GetDC(None);
        if screen.0.is_null() {
            return Err(WinError::from_hresult(windows::Win32::Foundation::E_FAIL));
        }
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, wi, hi);
        if mem.0.is_null() || bmp.0.is_null() {
            if !mem.0.is_null() {
                let _ = DeleteDC(mem);
            }
            if !bmp.0.is_null() {
                let _ = DeleteObject(bmp.into());
            }
            let _ = ReleaseDC(None, screen);
            return Err(WinError::from_hresult(windows::Win32::Foundation::E_FAIL));
        }

        // CAPTUREBLT：把分层窗口（含部分合成层）一并纳入 BitBlt 结果。
        let old = SelectObject(mem, bmp.into());
        let blt = BitBlt(
            mem,
            0,
            0,
            wi,
            hi,
            Some(screen),
            x,
            y,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        // GetDIBits 要求位图未被选入任何 DC，先还原。
        let _ = SelectObject(mem, old);

        if blt.is_err() {
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(None, screen);
            return Err(WinError::from_hresult(windows::Win32::Foundation::E_FAIL));
        }

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: wi,
                biHeight: -hi, // 负数 = 自上而下的行序，与 DXGI 一致
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD::default()],
        };

        let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];
        let scanned = GetDIBits(
            mem,
            bmp,
            0,
            h,
            Some(bgra.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );

        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, screen);

        if scanned == 0 {
            return Err(WinError::from_hresult(windows::Win32::Foundation::E_FAIL));
        }

        Ok(CapturedFrame {
            bgra: bgra.into(),
            stride: w * 4,
            width: w,
            height: h,
            qpc: qpc_now(),
        })
    }
}
