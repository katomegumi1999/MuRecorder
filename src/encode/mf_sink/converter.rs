//! MF 运行时守卫与 D3D11 硬件色彩转换（BGRA → NV12）。

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_FLAG,
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{Error as WinError, Interface, Result as WinResult};

/// 运行期 COM / MF 初始化看护守卫（RAII）
pub struct MfRuntimeGuard {
    com_initialized: bool,
    mf_initialized: bool,
}

impl MfRuntimeGuard {
    pub fn new() -> WinResult<Self> {
        let com_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let com_initialized = com_hr.is_ok();

        let mf_res = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
        if let Err(e) = mf_res {
            if com_initialized {
                unsafe { CoUninitialize() };
            }
            return Err(e);
        }

        Ok(Self {
            com_initialized,
            mf_initialized: true,
        })
    }
}

impl Drop for MfRuntimeGuard {
    fn drop(&mut self) {
        if self.mf_initialized {
            let _ = unsafe { MFShutdown() };
            self.mf_initialized = false;
        }
        if self.com_initialized {
            unsafe { CoUninitialize() };
            self.com_initialized = false;
        }
    }
}

/// D3D11 硬件视频色彩空间转换器（纯 GPU 硬件执行，显存零拷贝，实现 BGRA -> NV12 转换）
pub struct D3D11VideoConverter {
    video_context: ID3D11VideoContext,
    video_processor: ID3D11VideoProcessor,
    _enumerator: ID3D11VideoProcessorEnumerator,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
    nv12_texture: ID3D11Texture2D,
}

impl D3D11VideoConverter {
    pub fn try_new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        bgra_texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
        fps: u32,
    ) -> WinResult<Self> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;

        // 1. 创建目标 NV12 显存纹理
        let nv12_texture = create_nv12_render_texture(device, width, height)?;

        // 2. 构造视频处理器描述与枚举器
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputWidth: width,
            OutputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc)? };

        // 3. 创建硬件视频处理器实例
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };

        // 显式色彩空间校准与防泛白配置：
        // (1) 关闭驱动动态对比度/自动提亮滤镜
        unsafe {
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        }

        // (2) 输入流 (Stream 0): 桌面物理 BGRA 纹理为 sRGB / BT.709 Full Range (0-255)
        // bit 0: Usage = 0 (Playback 标准视频渲染)
        // bit 1: RGB_Range = 0 (0-255 Full Range)
        // bit 2: YCbCr_Matrix = 1 (BT.709 高清标准矩阵)
        // bits 4-5: Nominal_Range = 2 (0-255 Full)
        let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x24 };
        unsafe {
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &in_cs);
        }

        // (3) 输出目标: NV12 显存目标为高质量 BT.709 Full Range (0-255)
        // 彻底解决屏幕录制动态范围压缩导致的画面泛白、浅淡与发灰问题，100% 还原桌面纯正黑度与高对比度
        // bit 0: Usage = 0 (Playback)
        // bit 1: RGB_Range = 0
        // bit 2: YCbCr_Matrix = 1 (BT.709)
        // bits 4-5: Nominal_Range = 2 (0-255 Full Range)
        let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x24 };
        unsafe {
            video_context.VideoProcessorSetOutputColorSpace(&processor, &out_cs);
        }

        // 4. 为 BGRA 输入纹理创建 InputView
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous:
                windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
        };
        let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            video_device.CreateVideoProcessorInputView(
                bgra_texture,
                &enumerator,
                &in_desc,
                Some(&mut input_view),
            )?;
        }
        let input_view = input_view.ok_or_else(|| WinError::from_hresult(E_FAIL))?;

        // 5. 为 NV12 目标纹理创建 OutputView
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous:
                windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
        };
        let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &nv12_texture,
                &enumerator,
                &out_desc,
                Some(&mut output_view),
            )?;
        }
        let output_view = output_view.ok_or_else(|| WinError::from_hresult(E_FAIL))?;

        Ok(Self {
            video_context,
            video_processor: processor,
            _enumerator: enumerator,
            input_view,
            output_view,
            nv12_texture,
        })
    }

    /// 在显存内部瞬间完成色彩转换 (BGRA -> NV12)
    pub fn convert_nv12(&self) -> WinResult<&ID3D11Texture2D> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            ppFutureSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(self.input_view.clone())),
            ppPastSurfacesRight: std::ptr::null_mut(),
            ppFutureSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
        };

        unsafe {
            self.video_context.VideoProcessorBlt(
                &self.video_processor,
                &self.output_view,
                0,
                &[stream],
            )?;
        }

        Ok(&self.nv12_texture)
    }
}

/// 辅助函数：创建承载指针包围盒回读的暂存纹理（CPU 可读可写）。
///
/// 只覆盖光标大小，不是整帧——这是「合成指针又不毁掉零拷贝」的关键：
/// 整帧回读在 4K 下每帧要走 33 MB 的 PCIe，而光标边长通常只有 32～64 像素。
pub(super) fn create_cursor_staging_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
) -> WinResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        // STAGING 纹理的 BindFlags 必须为 0（D3D11 规范），误绑会 E_INVALIDARG。
        BindFlags: 0,
        CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or_else(|| WinError::from_hresult(E_FAIL))
}

/// 辅助函数：创建用于承载裁剪区域的显存纹理
pub(super) fn create_render_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> WinResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_FLAG(0).0 as u32, // 绝对禁止 CPU 读取，全在 VRAM
        MiscFlags: windows::Win32::Graphics::Direct3D11::D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
    };

    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or_else(|| WinError::from_hresult(E_FAIL))
}

/// 辅助函数：创建用于承载 NV12 视频处理器转换的显存目标纹理
fn create_nv12_render_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> WinResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_FLAG(0).0 as u32,
        MiscFlags: windows::Win32::Graphics::Direct3D11::D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
    };

    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or_else(|| WinError::from_hresult(E_FAIL))
}
