//! Media Foundation SinkWriter GPU 零拷贝硬编模块。
//!
//! 流程：
//! 1. 初始化 COM 与 MF 运行时 (CoInitializeEx / MFStartup)。
//! 2. D3D11 设备创建时开启 `D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT`。
//! 3. 通过 `IMFDXGIDeviceManager` 将显卡上下文绑定到 `IMFSinkWriter`，并启用 `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS`。
//! 4. 逐帧截取后在 GPU 内完成裁剪 (`CopySubresourceRegion`)，
//!    调用 `MFCreateDXGISurfaceBuffer` 直接包装显存纹理句柄，通过 `IMFSinkWriter::WriteSample` 直送硬件编码器。
//!
//! ## 指针合成为什么是这样做的
//!
//! 桌面复制的桌面图像里没有鼠标指针，必须自己画（详见 [`crate::capture::cursor`]）。
//! 合成点在**裁剪后的 BGRA 纹理**上、`BGRA→NV12` 之前，这样 H.264（ARGB32 输入）
//! 与 HEVC/AV1（NV12 输入）两条路走的是同一份像素。
//!
//! 三个关键决定：
//!
//! 1. **两份纹理。** `desktop_texture` 存「纯桌面」，`cropped_texture` 存
//!    「桌面 + 指针」。没有前者的话，指针移动时新指针会画在旧指针上，
//!    鼠标划过的轨迹会留下一串残影。
//! 2. **只回读光标的包围盒，不整帧回读。** 整帧回读会让 4K 每帧多走 33 MB
//!    PCIe（等于把零拷贝的收益全吐回去）。包围盒通常只有 32×32～64×64。
//! 3. **像素逻辑复用 CPU 那一份**（[`crate::capture::CursorShape::blend_into`]），
//!    不在 GPU 侧另写一套。单色光标的「反色」分支必须读到桌面像素才能算，用着色器
//!    实现得再做一次全屏采样；而包围盒回读天然把桌面像素摆在手边。代价是每帧一次小
//!    Map（约 0.1–0.3 ms），换来的是 CPU/GPU 两条路不可能出现像素分歧。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_FLAG,
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_READ_WRITE, D3D11_RESOURCE_MISC_FLAG,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
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
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample,
    IMFSinkWriter, MF_E_TOPO_CODEC_NOT_FOUND, MF_LOW_LATENCY, MF_MPEG4SINK_MIN_FRAGMENT_DURATION,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_D3D_MANAGER,
    MF_TRANSCODE_CONTAINERTYPE, MF_VERSION, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromMediaSink,
    MFCreateSinkWriterFromURL, MFMediaType_Audio, MFMediaType_Video, MFNominalRange_16_235,
    MFSTARTUP_FULL, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFTranscodeContainerType_FMPEG4, MFTranscodeContainerType_MPEG4, MFVideoFormat_ARGB32,
    MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{Error as WinError, Interface, PCWSTR, Result as WinResult};

use crate::capture::PointerFrame;
use crate::capture::cursor::overlay_rect;
use crate::capture::dxgi::FrameMeta;
use crate::encode::sink_raw::RawMediaSink;
use crate::mux::mp4::{Mp4Muxer, SampleDescription};
use crate::mux::nal::{annexb_to_mp4_sample, build_avcc, build_hvcc, parse_parameter_sets};
use crate::mux::remux::{finalize_fragmented, finalize_plain};

/// 分片目标时长（100ns 单位，即 1 s）。
///
/// MF 的 MP4 sink 会在「累计到这个时长后的第一个关键帧」处切分，所以实际分片
/// 粒度仍由 GOP 决定；这个值只是「至少多久切一次」的下限。
const FRAGMENT_DURATION_100NS: u64 = 10_000_000;

#[inline]
unsafe fn set_attribute_size(
    attrs: &IMFMediaType,
    key: &windows::core::GUID,
    width: u32,
    height: u32,
) -> WinResult<()> {
    unsafe { attrs.SetUINT64(key, ((width as u64) << 32) | (height as u64)) }
}

#[inline]
unsafe fn set_attribute_ratio(
    attrs: &IMFMediaType,
    key: &windows::core::GUID,
    numerator: u32,
    denominator: u32,
) -> WinResult<()> {
    unsafe { attrs.SetUINT64(key, ((numerator as u64) << 32) | (denominator as u64)) }
}

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
        // bit 0: Usage = 1 (Video processing/encoding)
        // bit 1: RGB_Range = 0 (0-255 Full Range)
        // bit 2: YCbCr_Matrix = 1 (BT.709 高清标准矩阵)
        // bits 4-5: Nominal_Range = 2 (0-255 Full)
        let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x25 };
        unsafe {
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &in_cs);
        }

        // (3) 输出目标: NV12 显存目标为标准 BT.709 Studio Range (16-235)
        // bit 0: Usage = 1
        // bit 1: RGB_Range = 0
        // bit 2: YCbCr_Matrix = 1 (BT.709)
        // bits 4-5: Nominal_Range = 1 (16-235 Studio/Limited Range)
        let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x15 };
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

/// 自研 sink 模式的运行时状态（`ContainerMode::RawFragmented` 专有）。
struct RawPipeline {
    sink: RawMediaSink,
    codec: GpuCodec,
    /// 收尾时要吃（`finish` 消费 self），所以是 Option
    muxer: Option<Mp4Muxer>,
}

/// GPU 硬件 SinkWriter 封装
pub struct GpuSinkWriter {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    sink_writer: IMFSinkWriter,
    stream_index: u32,
    audio_stream_index: Option<u32>,
    cropped_texture: ID3D11Texture2D,
    /// 只含桌面、不含指针的一份副本。
    ///
    /// 指针每帧都要重画，必须有一份「干净的底」可以重来；否则新指针叠在旧指针上，
    /// 快速移动鼠标会拖出一串残影。仅在开启指针合成时按需创建。
    desktop_texture: Option<ID3D11Texture2D>,
    /// `desktop_texture` 是否已被本会话的有效帧填过。
    /// 未填过时第一帧必须强制拷贝（`FrameMeta.desktop_updated` 可能为 false）。
    desktop_valid: bool,
    /// 指针包围盒的 CPU 回读暂存纹理（尺寸随光标变化重建）。
    cursor_staging: Option<(ID3D11Texture2D, u32, u32)>,
    converter: Option<D3D11VideoConverter>,
    width: u32,
    height: u32,
    fps: u32,
    frame_count: i64,
    /// 上一帧的样本时间（100ns），用于按真实间隔计算 duration
    last_sample_time: i64,
    /// 输出文件路径：`Finalize()` 之后要做一次 soft remux，需要重新打开它
    output_path: PathBuf,
    /// 落盘形态：决定 `finish()` 后是否还需要收敛分片结构
    container: ContainerMode,
    /// 只有 `RawFragmented` 才有：自研 sink + 自研封装器
    raw: Option<RawPipeline>,
    finalized: AtomicBool,
    _guard: MfRuntimeGuard,
}

/// 支持的 GPU 硬件编码目标格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuCodec {
    H264,
    Hevc,
    Av1,
}

/// MF 自带的 MP4 sink 有两种容器模式，能力差别很大。
///
/// **实测（Windows 11 + AMD 核显）**：`MFTranscodeContainerType_FMPEG4` 只对 H.264
/// 生效，换成 HEVC 时 `BeginWriting()` 直接返回 `MF_E_INVALIDMEDIATYPE`
/// ——与分辨率、profile、像素格式、码率全部无关，也与此处是否挂了 DXGI
/// Device Manager 无关（裸 SinkWriter 复现一致）。也就是说 MF 自带的分片
/// MP4 sink 不支持 HEVC（AV1 同理）。
///
/// 这个限制**只存在于「谁写盒」这一层**，与编码器无关。所以我们把写盒拿回来
/// （[`crate::encode::sink_raw`]），三种落盘模式的差别只剩「盒是谁写的」：
///
/// | 模式 | 录制中可读/可切片 | 崩溃最多丢 | 编码器 |
/// |---|---|---|---|
/// | `Fragmented` | 是（`moof` 自带样本表） | 最后一个分片 | 仅 H.264（MF 限制） |
/// | `RawFragmented` | 是 | 最后一个分片 | 任意（H.264 / HEVC） |
/// | `Plain` | 否（`moov` 只 `Finalize` 时写） | 整场录制 | 任意 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerMode {
    Fragmented,
    RawFragmented,
    Plain,
}

impl ContainerMode {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fragmented => "分片 fMP4（MF 写盒）",
            Self::RawFragmented => "分片 fMP4（自研写盒）",
            Self::Plain => "普通 MP4",
        }
    }

    /// 录制中是否可读、可切片。
    pub fn is_fragmented(&self) -> bool {
        matches!(self, Self::Fragmented | Self::RawFragmented)
    }

    /// 纯 ASCII 短名（用在文件名/日志里，避免中文与全角括号）。
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Fragmented => "frag-mf",
            Self::RawFragmented => "frag-raw",
            Self::Plain => "plain",
        }
    }
}

impl GpuCodec {
    pub fn subtype(&self) -> &'static windows::core::GUID {
        match self {
            Self::H264 => &MFVideoFormat_H264,
            Self::Hevc => &MFVideoFormat_HEVC,
            Self::Av1 => &MFVideoFormat_AV1,
        }
    }

    pub fn requires_nv12(&self) -> bool {
        matches!(self, Self::Hevc | Self::Av1)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "H.265 (HEVC)",
            Self::Av1 => "AV1",
        }
    }
}

/// 由「编码器给出的参数集」构建 `stsd` 的样本描述。
///
/// `header` 是 `MF_MT_MPEG_SEQUENCE_HEADER` 的原文——**Annex B 形式**的
/// VPS/SPS/PPS（不是 `avcC`/`hvcC` 盒体！）。这一点很容易搞错：MF 自带 MP4 sink
/// 的内部分工是「编码器吐 Annex B → sink 转 AVCC + 造配置盒」，
/// 换成自研 sink 之后这段转换就得我们补上，见 [`crate::mux::nal`]。
fn sample_description(codec: GpuCodec, header: &[u8]) -> Option<SampleDescription> {
    match codec {
        GpuCodec::H264 => {
            let ps = parse_parameter_sets(header, false);
            let avcc = build_avcc(&ps.sps, &ps.pps)?;
            Some(SampleDescription {
                entry: *b"avc1",
                config_boxes: vec![avcc],
                label: "MuRecorder H.264 (MF)".into(),
            })
        }
        GpuCodec::Hevc => {
            let ps = parse_parameter_sets(header, true);
            let hvcc = build_hvcc(&ps)?;
            Some(SampleDescription {
                entry: *b"hvc1",
                config_boxes: vec![hvcc],
                label: "MuRecorder HEVC (MF)".into(),
            })
        }
        // AV1 的样本是 OBU 而不是 Annex B，配置记录也不同，暂时留在普通 MP4 路径。
        GpuCodec::Av1 => None,
    }
}

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

impl GpuSinkWriter {
    /// 尝试初始化 GPU 硬件 SinkWriter (H.264, H.265, AV1)
    ///
    /// `container` 决定落盘形态：`Fragmented` 抗崩溃且录制中可切片，但只有
    /// H.264 能被 MF 接受（见 [`ContainerMode`]）；`Plain` 编码器支持面更广。
    /// 调用方负责在失败时换下一个组合重试。
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        output_file: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        codec: GpuCodec,
        container: ContainerMode,
        audio_bitrate_kbps: Option<u32>,
    ) -> WinResult<Self> {
        let guard = MfRuntimeGuard::new()?;

        // 若用户选择 AV1，必须确保系统存在硬件 MFT，严防回退到 CPU 软编导致 100% 掉帧卡死
        if codec == GpuCodec::Av1 && !has_hardware_encoder(&MFVideoFormat_AV1) {
            return Err(WinError::from_hresult(MF_E_TOPO_CODEC_NOT_FOUND));
        }

        // 硬件编码器硬性要求：宽高对齐到偶数
        let aligned_width = (width & !1).max(2);
        let aligned_height = (height & !1).max(2);
        let bitrate_bps = bitrate_kbps.saturating_mul(1000);

        unsafe {
            // 0. 开启 D3D11 设备多线程保护，防止与 MF 线程死锁
            if let Ok(multithread) = device.cast::<ID3D10Multithread>() {
                let _ = multithread.SetMultithreadProtected(true);
            }

            // 1. 创建用于裁剪区域的显存目标纹理 (GPU VRAM 零拷贝承载)
            let cropped_texture = create_render_texture(device, aligned_width, aligned_height)?;

            // 2. 若启用 HEVC 或 AV1，构建 D3D11 VideoProcessor 显存内硬件颜色转换器 (BGRA -> NV12)
            let converter = if codec.requires_nv12() {
                Some(D3D11VideoConverter::try_new(
                    device,
                    context,
                    &cropped_texture,
                    aligned_width,
                    aligned_height,
                    fps,
                )?)
            } else {
                None
            };

            // 3. 创建 MF DXGI Device Manager 并重置绑定当前显卡设备
            let mut reset_token = 0u32;
            let mut dxgi_manager_opt: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut dxgi_manager_opt)?;
            let dxgi_manager = dxgi_manager_opt.ok_or_else(|| WinError::from_hresult(E_FAIL))?;
            dxgi_manager.ResetDevice(device, reset_token)?;

            // 4. 配置 SinkWriter 全局属性（开启全链路 GPU 硬件加速与低延迟）
            let mut writer_attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut writer_attributes, 8)?;
            let writer_attrs = writer_attributes.ok_or_else(|| WinError::from_hresult(E_FAIL))?;

            writer_attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &dxgi_manager)?;
            writer_attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            writer_attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;

            match container {
                ContainerMode::Fragmented => {
                    writer_attrs.SetGUID(
                        &MF_TRANSCODE_CONTAINERTYPE,
                        &MFTranscodeContainerType_FMPEG4,
                    )?;
                    // 分片输出：让 MF 直接写 Fragmented MP4（每片自带 moof），
                    // 这样进程崩溃/断电时最多损失最后一个分片，而不是整场录制。
                    // 单位是 100ns。注意：只设这个属性**无效**，必须同时把容器类型改成
                    // MFTranscodeContainerType_FMPEG4（实测仍是 ftyp/uuid/mdat/moov）。
                    writer_attrs
                        .SetUINT64(&MF_MPEG4SINK_MIN_FRAGMENT_DURATION, FRAGMENT_DURATION_100NS)?;
                }
                ContainerMode::Plain => {
                    writer_attrs
                        .SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
                }
                // 自研 sink：容器类型由我们自己决定，MF 这层不参与，
                // 所以这里**不设** MF_TRANSCODE_CONTAINERTYPE。
                ContainerMode::RawFragmented => {}
            }

            // 5. 创建 SinkWriter 实例
            //
            // 两条路：
            // - MF 自带 sink：给它一个路径，它自己写文件；
            // - 自研 sink：给它我们的 IMFMediaSink，它只负责挑编码器 + 编码，
            //   编码后的码流交回来给我们写盒。
            let raw_sink = if container == ContainerMode::RawFragmented {
                Some(RawMediaSink::new(&[codec.subtype()])?)
            } else {
                None
            };
            let sink_writer = match &raw_sink {
                Some(raw) => MFCreateSinkWriterFromMediaSink(raw.sink(), &writer_attrs)?,
                None => {
                    let wide_path: Vec<u16> = output_file
                        .encode_utf16()
                        .chain(std::iter::once(0))
                        .collect();
                    MFCreateSinkWriterFromURL(PCWSTR(wide_path.as_ptr()), None, &writer_attrs)?
                }
            };

            // 6. 配置输出视频流格式（H.264、HEVC 或 AV1）
            let out_type: IMFMediaType = MFCreateMediaType()?;

            out_type.SetGUID(
                &MF_MT_MAJOR_TYPE,
                &windows::Win32::Media::MediaFoundation::MFMediaType_Video,
            )?;
            const MF_MT_MPEG2_PROFILE: windows::core::GUID =
                windows::core::GUID::from_u128(0xad76a00b_2d55_4e60_aa05_0207527019da);
            const E_AV_ENC_H265_V_PROFILE_MAIN_420_8: u32 = 1;

            match codec {
                GpuCodec::Hevc => {
                    out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
                    out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, E_AV_ENC_H265_V_PROFILE_MAIN_420_8)?;
                }
                GpuCodec::Av1 => {
                    out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_AV1)?;
                    // AV1 硬件 MFT 根据输入 NV12 默认匹配 Main Profile (8-bit 4:2:0)
                }
                GpuCodec::H264 => {
                    out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                }
            }
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
            set_attribute_size(&out_type, &MF_MT_FRAME_SIZE, aligned_width, aligned_height)?;
            set_attribute_ratio(&out_type, &MF_MT_FRAME_RATE, fps, 1)?;
            set_attribute_ratio(&out_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;

            // 显式写入 BT.709 色彩标准与有限动态范围 (16-235) 元数据，杜绝播放器（Movies & TV、Edge、Chrome）泛白发灰
            out_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            out_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            out_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            out_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;

            let stream_index = sink_writer.AddStream(&out_type)?;

            // 7. 配置输入流格式
            // 核心要点：
            // 若为 HEVC / AV1：直接告知 SinkWriter 输入为 MFVideoFormat_NV12！输入输出都是 NV12，
            // 彻底杜绝 Media Foundation 自作主张插入软件颜色转换器导致与显卡驱动死锁。
            // 若为 H.264：直接传入 ARGB32。
            let in_type: IMFMediaType = MFCreateMediaType()?;
            in_type.SetGUID(
                &MF_MT_MAJOR_TYPE,
                &windows::Win32::Media::MediaFoundation::MFMediaType_Video,
            )?;
            if codec.requires_nv12() {
                in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                in_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
                in_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
                in_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
                in_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            } else {
                in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
            }
            set_attribute_size(&in_type, &MF_MT_FRAME_SIZE, aligned_width, aligned_height)?;
            set_attribute_ratio(&in_type, &MF_MT_FRAME_RATE, fps, 1)?;
            set_attribute_ratio(&in_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;

            sink_writer.SetInputMediaType(stream_index, &in_type, None)?;

            // 6b. 若启用音频，配置输出与输入音频流 (AAC-LC / 48kHz / 立体声 / 指定码率)
            let audio_stream_index = if let Some(audio_kbps) = audio_bitrate_kbps {
                let audio_out: IMFMediaType = MFCreateMediaType()?;
                audio_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                audio_out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                audio_out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
                audio_out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000)?;
                audio_out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                audio_out.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
                let avg_bytes = (audio_kbps * 1000) / 8;
                audio_out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bytes)?;
                audio_out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
                audio_out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;

                let audio_idx = sink_writer.AddStream(&audio_out)?;

                let audio_in: IMFMediaType = MFCreateMediaType()?;
                audio_in.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                audio_in.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                audio_in.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
                audio_in.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000)?;
                audio_in.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                audio_in.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
                audio_in.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 48000 * 4)?;

                sink_writer.SetInputMediaType(audio_idx, &audio_in, None)?;
                Some(audio_idx)
            } else {
                None
            };

            // 真正的协商在这里发生：MFT 找不到、或它不接受「该编码器 + 该容器」
            // 的组合时，只有 BeginWriting 会报 MF_E_INVALIDMEDIATYPE。
            sink_writer.BeginWriting()?;

            // 7. 自研 sink 模式：此刻参数集已经拿到了（`SetInputMediaType` 期间
            //    编码器把它写进了输出类型，我们的类型处理器顺手抄了下来），
            //    可以直接把 `stsd` 定下来并铺好 init moov。
            let raw = match raw_sink {
                None => None,
                Some(sink) => {
                    let header = sink.sequence_header();
                    let subtype = sink.subtype().unwrap_or(*codec.subtype());
                    let desc = sample_description(codec, &header).ok_or_else(|| {
                        WinError::new(
                            E_FAIL,
                            format!(
                                "编码器没有给出可用的参数集（{} 字节），无法构建 stsd",
                                header.len()
                            ),
                        )
                    })?;
                    let mut muxer = Mp4Muxer::create_multi_with_audio_bitrate(
                        &PathBuf::from(output_file),
                        Some((aligned_width, aligned_height, fps)),
                        audio_bitrate_kbps.map(|_| (2, 48000)),
                        audio_bitrate_kbps.unwrap_or(320) * 1000,
                    )
                    .map_err(|e| WinError::new(E_FAIL, format!("创建封装器失败: {e}")))?;
                    muxer
                        .set_video_description(desc)
                        .map_err(|e| WinError::new(E_FAIL, format!("设置视频描述失败: {e}")))?;
                    let _ = subtype;
                    Some(RawPipeline {
                        sink,
                        codec,
                        muxer: Some(muxer),
                    })
                }
            };

            Ok(Self {
                _device: device.clone(),
                context: context.clone(),
                sink_writer,
                stream_index,
                audio_stream_index,
                cropped_texture,
                desktop_texture: None,
                desktop_valid: false,
                cursor_staging: None,
                converter,
                width: aligned_width,
                height: aligned_height,
                fps,
                frame_count: 0,
                last_sample_time: 0,
                output_path: PathBuf::from(output_file),
                container,
                raw,
                finalized: AtomicBool::new(false),
                _guard: guard,
            })
        }
    }

    /// 把自研 sink 里攒着的样本搬进封装器（非 `RawFragmented` 模式是空操作）。
    ///
    /// 样本从 MF 出来时是 **Annex B**（起始码分隔），这里做两件事：
    /// 1. 转成 MP4 要求的 4 字节长度前缀；
    /// 2. 丢掉参数集 NAL —— 它们已经在 `stsd` 的 `avcC`/`hvcC` 里了。
    ///
    /// 音频样本则保持裸 AAC 帧直接写入音轨。
    fn drain_raw(&mut self) -> WinResult<()> {
        let Some(raw) = self.raw.as_mut() else {
            return Ok(());
        };
        if let Some(err) = raw.sink.take_error() {
            return Err(WinError::new(E_FAIL, err));
        }
        let streams = raw.sink.take_streams();
        if streams.is_empty() {
            return Ok(());
        }
        let hevc = raw.codec == GpuCodec::Hevc;
        let Some(muxer) = raw.muxer.as_mut() else {
            return Ok(());
        };
        for (_id, st) in streams {
            match st.kind {
                crate::encode::sink_raw::StreamKind::Video => {
                    for s in st.samples {
                        let data = annexb_to_mp4_sample(&s.data, hevc);
                        muxer
                            .write_video_sample(&data, s.pts_us, s.is_key)
                            .map_err(|e| WinError::new(E_FAIL, format!("写视频样本失败: {e}")))?;
                    }
                }
                crate::encode::sink_raw::StreamKind::Audio => {
                    for s in st.samples {
                        let pts_ticks = (s.pts_us as u128 * 48_000 / 1_000_000) as u64;
                        muxer
                            .write_audio_sample(&s.data, pts_ticks)
                            .map_err(|e| WinError::new(E_FAIL, format!("写音频样本失败: {e}")))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// 注入 CPU 内存中的双声道 48kHz 16-bit PCM 音频并编码送入 SinkWriter
    pub fn push_audio_pcm(&mut self, pcm: &[u8], pts_us: u64) -> WinResult<()> {
        let Some(audio_idx) = self.audio_stream_index else {
            return Ok(());
        };
        if pcm.is_empty() {
            return Ok(());
        }
        let frames = (pcm.len() / 4) as u32;
        unsafe {
            let media_buffer: IMFMediaBuffer = MFCreateMemoryBuffer(pcm.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            let mut max_len = 0u32;
            let mut cur_len = 0u32;
            media_buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), ptr, pcm.len());
            media_buffer.Unlock()?;
            media_buffer.SetCurrentLength(pcm.len() as u32)?;

            let sample: IMFSample = MFCreateSample()?;
            sample.AddBuffer(&media_buffer)?;

            let sample_time = (pts_us as i64).saturating_mul(10);
            let duration = (frames as i64 * 10_000_000 / 48_000).max(1);
            sample.SetSampleTime(sample_time)?;
            sample.SetSampleDuration(duration)?;

            self.sink_writer.WriteSample(audio_idx, &sample)?;
        }
        self.drain_raw()
    }

    /// 核心调用：在显存内完成子区域裁剪、（可选）指针合成，并直接包装送入 GPU 编码器。
    ///
    /// `pts_us` 必须是**真实采集时刻**（相对录制起点的 QPC 微秒）。
    /// 早期实现用 `frame_count × 帧间隔` 生成时间戳，一旦编码或写盘跟不上，
    /// 时间轴就按帧数压缩——60 秒的实录可能只播 20 秒，接入音频后必然音画不同步。
    ///
    /// `meta.pointer` 为 `Some` 表示本次录制开启了指针合成（形状是否就绪另说）；
    /// `meta.desktop_updated` 表示显存里的桌面图像本帧是否真的变了。后者为 false
    /// 时只需把上次存下的纯桌面副本再摊一次，不必再截一次显存——「只有鼠标在动、
    /// 画面别处完全静止」是很常见的情形。
    pub fn push_frame(
        &mut self,
        source_texture: &ID3D11Texture2D,
        crop_x: u32,
        crop_y: u32,
        pts_us: u64,
        meta: FrameMeta<'_>,
    ) -> WinResult<()> {
        let crop_box = D3D11_BOX {
            left: crop_x,
            top: crop_y,
            front: 0,
            right: crop_x + self.width,
            bottom: crop_y + self.height,
            back: 1,
        };

        match meta.pointer {
            // 未开启指针合成：一条路走到底，不引入任何额外拷贝。
            None => unsafe {
                self.context.CopySubresourceRegion(
                    &self.cropped_texture,
                    0,
                    0,
                    0,
                    0,
                    source_texture,
                    0,
                    Some(&crop_box),
                );
            },
            // 开启指针合成：先维护「纯桌面」副本，再从它派生本帧画面。
            Some(pointer) => {
                self.ensure_desktop_texture()?;
                if meta.desktop_updated || !self.desktop_valid {
                    let desktop = self.desktop_texture.as_ref().unwrap();
                    unsafe {
                        self.context.CopySubresourceRegion(
                            desktop,
                            0,
                            0,
                            0,
                            0,
                            source_texture,
                            0,
                            Some(&crop_box),
                        );
                    }
                    self.desktop_valid = true;
                }
                let desktop = self.desktop_texture.as_ref().unwrap();
                // 每帧都从干净底重来：这是「不留残影」的全部要点。
                unsafe { self.context.CopyResource(&self.cropped_texture, desktop) };

                // 形状与可见性都就绪才真正落笔。
                if pointer.is_drawable() {
                    self.composite_cursor(&pointer)?;
                }
            }
        }

        unsafe { self.write_current_buffer(pts_us) }
    }

    /// 把指针画到 `cropped_texture` 上：只回读光标包围盒，合成后写回。
    fn composite_cursor(&mut self, pointer: &PointerFrame<'_>) -> WinResult<()> {
        let Some(shape) = pointer.shape else {
            return Ok(());
        };
        // 包围盒与 CPU 路径共用同一份裁剪逻辑，避免两条路对「部分可见」判断不一致。
        let Some((left, top, bw, bh)) = overlay_rect(pointer.state, shape, self.width, self.height)
        else {
            // 完全在画面外（常见：鼠标在另一块屏上）。
            return Ok(());
        };
        // `PointerPosition` 给的是光标图像**左上角**（不是热点）。
        let (sx, sy) = (pointer.state.x, pointer.state.y);

        let staging = self.ensure_cursor_staging(bw, bh)?.clone();

        // 1. 把目标包围盒读回 CPU 暂存纹理
        let read_back = D3D11_BOX {
            left,
            top,
            front: 0,
            right: left + bw,
            bottom: top + bh,
            back: 1,
        };
        unsafe {
            self.context.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                &self.cropped_texture,
                0,
                Some(&read_back),
            );
        }

        // 2. CPU 合成（与软编路径同一份像素逻辑）
        let mut mapped = Default::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut mapped))?;
        }
        {
            // 行距按 RowPitch 算，不要假设等于 bw*4（驱动常按 256 字节对齐）。
            let stride = mapped.RowPitch as usize;
            let len = stride * bh as usize;
            // Safety: staging 纹理是本函数刚建的、至少 bw×bh，Map 保证可读写整块。
            let buf = unsafe { std::slice::from_raw_parts_mut(mapped.pData as *mut u8, len) };
            // 缓冲区原点对应画面 (left, top)，光标原点在 (sx, sy)，两者相减即可
            // 让 blend_into 自己处理越界裁剪。
            shape.blend_into(buf, stride, bw, bh, sx - left as i32, sy - top as i32);
        }
        unsafe {
            self.context.Unmap(&staging, 0);
        }

        // 3. 写回同一位置
        let write_back = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: bw,
            bottom: bh,
            back: 1,
        };
        unsafe {
            self.context.CopySubresourceRegion(
                &self.cropped_texture,
                0,
                left,
                top,
                0,
                &staging,
                0,
                Some(&write_back),
            );
        }
        Ok(())
    }

    /// 按需创建「纯桌面」副本纹理。
    fn ensure_desktop_texture(&mut self) -> WinResult<()> {
        if self.desktop_texture.is_none() {
            let tex = create_render_texture(&self._device, self.width, self.height)?;
            self.desktop_texture = Some(tex);
            self.desktop_valid = false;
        }
        Ok(())
    }

    /// 按需创建/扩容指针包围盒的暂存纹理（STAGING，CPU 可读写）。
    ///
    /// 只在光标尺寸变大时重建；换光标很稀疏，这点分配可以忽略。
    fn ensure_cursor_staging(&mut self, w: u32, h: u32) -> WinResult<&ID3D11Texture2D> {
        let fits = matches!(&self.cursor_staging, Some((_, ew, eh)) if *ew >= w && *eh >= h);
        if !fits {
            let tex = create_cursor_staging_texture(&self._device, w.max(1), h.max(1))?;
            self.cursor_staging = Some((tex, w.max(1), h.max(1)));
        }
        Ok(&self.cursor_staging.as_ref().unwrap().0)
    }

    /// 画面无变化时，复用已在显存中的上一帧内容直接编码，维持恒定帧率 (CFR)。
    /// 返回 `true` 表示确实写入了一个样本（首帧尚未就绪时返回 `false`）。
    pub fn push_duplicate_frame(&mut self, pts_us: u64) -> WinResult<bool> {
        if self.frame_count == 0 {
            return Ok(false);
        }
        unsafe { self.write_current_buffer(pts_us) }.map(|()| true)
    }

    /// 注入 CPU 内存中的 BGRA 数据（如静止画面的首帧 GDI 兜底）到显存纹理并编码
    pub fn push_raw_bgra(&mut self, bgra: &[u8], stride: u32, pts_us: u64) -> WinResult<()> {
        unsafe {
            self.context.UpdateSubresource(
                &self.cropped_texture,
                0,
                None,
                bgra.as_ptr() as *const std::ffi::c_void,
                stride,
                0,
            );
            self.write_current_buffer(pts_us)
        }
    }

    /// 私有辅助：将当前显存纹理（NV12 或 BGRA）包装为 IMFMediaBuffer 并送入 GPU 编码器
    unsafe fn write_current_buffer(&mut self, pts_us: u64) -> WinResult<()> {
        unsafe {
            let (target_texture, buffer_len) = if let Some(ref conv) = self.converter {
                let nv12 = conv.convert_nv12()?;
                (nv12, self.width * self.height * 3 / 2)
            } else {
                (&self.cropped_texture, self.width * self.height * 4)
            };

            // 步骤 B: 包装显存纹理为 IMFMediaBuffer (核心零拷贝点)
            let media_buffer: IMFMediaBuffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, target_texture, 0, false)?;

            let actual_len = match media_buffer.GetMaxLength() {
                Ok(len) if len > 0 => len,
                _ => buffer_len,
            };
            let _ = media_buffer.SetCurrentLength(actual_len);

            // 步骤 C: 组装 IMFSample 并附带精准时间戳（100ns 单位）
            let sample: IMFSample = MFCreateSample()?;
            sample.AddBuffer(&media_buffer)?;

            // 时间戳取自真实 QPC（µs → 100ns）；时长取与上一帧的实际间距，
            // 这样丢帧或卡顿留下的空隙在容器里如实体现，成片时长与墙钟一致。
            let sample_time = (pts_us as i64).saturating_mul(10);
            let nominal = (10_000_000i64 / self.fps.max(1) as i64).max(1);
            let duration = if self.frame_count == 0 {
                nominal
            } else {
                (sample_time - self.last_sample_time).max(1)
            };
            sample.SetSampleTime(sample_time)?;
            sample.SetSampleDuration(duration)?;
            self.last_sample_time = sample_time;

            // 步骤 D: 直接递入硬件编码器
            self.sink_writer.WriteSample(self.stream_index, &sample)?;

            self.frame_count += 1;
        }
        // 自研 sink 模式下，编码是在 MF 的工作线程上异步完成的：这里只是把
        // 已经吐出来的样本搬走，喂给封装器。还没吐的留在 sink 里，`finish()` 再收尾。
        self.drain_raw()
    }

    /// 停止录制：Flush 硬件编码器，写入 MP4 索引头，并把分片态收敛成普通 MP4。
    ///
    /// 两步是分开的：
    /// 1. `Finalize()` 让 MF 把编码器缓冲吐完、补一个 `mfra` —— **此刻文件已是
    ///    合法的 fMP4**，即使后面出任何问题它也能播、也能被 ffmpeg 救。
    /// 2. [`finalize_fragmented`] 只读头部、不搬媒体数据，把
    ///    `ftyp/uuid/pdin/moov/moof…/mfra` 收敛成 `ftyp/mdat/moov`。
    ///
    /// 第 2 步失败时**故意不返回错误**：文件仍是可播放的 fMP4，把一次「格式不够通用」
    /// 当成录制失败会误导宿主。这里只打一行警告，让上层继续正常收尾。
    pub fn finish(&mut self) -> WinResult<()> {
        // 幂等：`finish()` 可能被显式调用一次、再被 Drop 调用一次。
        // 第二次必须**直接返回**，否则会对着已被上层删掉的文件再做一次 remux
        // （报一个毫无意义、还会让宿主误判的"文件不存在"）。
        if self.finalized.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        unsafe {
            self.sink_writer.Finalize()?;
        }
        // 两种容器的收尾动作不同：
        // - 分片态：收敛成普通 MP4（soft remux，媒体字节不搬）；
        // - 普通 MP4：把 32 位 `stco` 升级成 `co64`，否则超过 4 GiB 偏移会回绕。
        // 两者都失败也不返回错误——文件仍是合法的（分片或普通）MP4，
        // 把「格式不够完美」当成录制失败会让宿主误判。
        if self.frame_count > 0 {
            match self.container {
                ContainerMode::Fragmented => {
                    if let Err(e) = finalize_fragmented(&self.output_path) {
                        eprintln!(
                            "[murec] soft remux 未完成（{}）：{e}；文件仍是合法的分片式 MP4，可直接播放",
                            self.output_path.display()
                        );
                    }
                }
                // 自研 sink：`Finalize()` 之后编码器缓冲才全吐完，所以要再收一次样本，
                // 再让封装器补末尾 moov、把开头那个 16 字节 free 改写成巨型 mdat。
                // 这一套跟软编路径完全一致，产物直接就是普通 MP4（含 co64）。
                ContainerMode::RawFragmented => {
                    if let Err(e) = self.drain_raw() {
                        eprintln!("[murec] 收尾取样失败：{e}");
                    }
                    if let Some(mut raw) = self.raw.take()
                        && let Some(muxer) = raw.muxer.take()
                    {
                        match muxer.finish() {
                            Ok(bytes) => eprintln!(
                                "[murec] {}：已由自研封装器收尾（{bytes} 字节，{}）",
                                self.output_path.display(),
                                raw.codec.name()
                            ),
                            Err(e) => eprintln!(
                                "[murec] 封装收尾失败（{}）：{e}；文件仍是合法的分片式 MP4，可直接播放",
                                self.output_path.display()
                            ),
                        }
                    }
                }
                ContainerMode::Plain => match finalize_plain(&self.output_path) {
                    Ok(r) => {
                        if r.oversize_risk {
                            eprintln!(
                                "[murec] {} 已超过 4 GiB，但容器头是 32 位（mdat 长度与 chunk 偏移表）：\
                                 该文件可能无法完整读取。长录制请改用 H.264，或分段录制",
                                self.output_path.display()
                            );
                        } else if r.changed {
                            eprintln!(
                                "[murec] {}：{} 个 chunk 的偏移表已从 stco 升级为 co64（解除 4 GiB 上限）",
                                self.output_path.display(),
                                r.chunks
                            );
                        }
                    }
                    Err(e) => eprintln!(
                        "[murec] 偏移表升级未完成（{}）：{e}；文件可播放，但超过 4 GiB 后可能损坏",
                        self.output_path.display()
                    ),
                },
            }
        }
        Ok(())
    }

    /// 本次落盘是否为分片形态（决定录制中能否切片）。
    pub fn is_fragmented(&self) -> bool {
        self.container.is_fragmented()
    }
}

unsafe impl Send for GpuSinkWriter {}

impl Drop for GpuSinkWriter {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// 辅助函数：创建承载指针包围盒回读的暂存纹理（CPU 可读可写）。
///
/// 只覆盖光标大小，不是整帧——这是「合成指针又不毁掉零拷贝」的关键：
/// 整帧回读在 4K 下每帧要走 33 MB 的 PCIe，而光标边长通常只有 32～64 像素。
fn create_cursor_staging_texture(
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
fn create_render_texture(
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
        MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
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
        MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
    };

    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or_else(|| WinError::from_hresult(E_FAIL))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 MF 源读取器把文件当视频读一遍，返回 (帧数, 宽, 高)。
    ///
    /// 这是**唯一**能证明「我们手搓的 stsd + 长度前缀样本真的能解码」的手段：
    /// 光看盒结构正确，`avcC`/`hvcC` 里参数集拼错了照样是坏文件。
    fn mf_readback(path: &std::path::Path) -> (u32, u32, u32) {
        use windows::Win32::Media::MediaFoundation::{
            IMFMediaType, MF_MT_FRAME_SIZE, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
            MF_SOURCE_READERF_ENDOFSTREAM, MFCreateSourceReaderFromURL,
        };
        let wide: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let reader =
                MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), None).expect("MF 打不开该 MP4");
            let mt: IMFMediaType = reader
                .GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)
                .expect("MF 读不到视频轨（stsd 结构可能有问题）");
            let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap();
            let (w, h) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
            reader
                .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &mt)
                .unwrap();

            let mut frames = 0u32;
            loop {
                let mut flags = 0u32;
                let mut sample = None;
                reader
                    .ReadSample(
                        MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                        0,
                        None,
                        Some(&mut flags),
                        None,
                        Some(&mut sample),
                    )
                    .unwrap();
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
                if sample.is_some() {
                    frames += 1;
                    assert!(frames < 10_000, "样本数异常");
                }
            }
            (frames, w, h)
        }
    }

    #[test]
    fn mf_runtime_guard_inits_and_cleans_up() {
        let guard = MfRuntimeGuard::new();
        assert!(
            guard.is_ok(),
            "MfRuntimeGuard should initialize successfully"
        );
        drop(guard);
    }

    #[test]
    fn attribute_size_and_ratio_encoding() {
        let _guard = MfRuntimeGuard::new().unwrap();
        unsafe {
            let media_type = MFCreateMediaType().unwrap();
            set_attribute_size(&media_type, &MF_MT_FRAME_SIZE, 1920, 1080).unwrap();
            let size_packed = media_type.GetUINT64(&MF_MT_FRAME_SIZE).unwrap();
            let w = (size_packed >> 32) as u32;
            let h = (size_packed & 0xFFFF_FFFF) as u32;
            assert_eq!(w, 1920);
            assert_eq!(h, 1080);

            set_attribute_ratio(&media_type, &MF_MT_FRAME_RATE, 60, 1).unwrap();
            let rate_packed = media_type.GetUINT64(&MF_MT_FRAME_RATE).unwrap();
            let num = (rate_packed >> 32) as u32;
            let den = (rate_packed & 0xFFFF_FFFF) as u32;
            assert_eq!(num, 60);
            assert_eq!(den, 1);
        }
    }

    /// GPU 路径的指针合成：真的把光标像素写进了喂给编码器的那份纹理，且不留残影。
    ///
    /// 这是唯一能验证「两份纹理 + 包围盒回读」这套做法的用例。它检查三件事：
    /// 1. 光标位置上的像素被换成了光标色；
    /// 2. 远离光标的位置仍是桌面原色（说明包围盒没有写歪）；
    /// 3. **光标移走之后，原来位置恢复成桌面色**——没有第 3 条的话，鼠标快速划过
    ///    会留下一串光标残影，而单帧截图完全看不出来。
    ///
    /// CPU 路径的合成由 `capture::cursor` 的单测覆盖，这里专测 GPU 这层搬运。
    #[test]
    #[ignore = "需要真实 GPU 硬件编码器（本地手动验证）"]
    fn gpu_cursor_is_composited_and_leaves_no_trail() {
        use windows::Win32::Graphics::Direct3D::*;
        use windows::Win32::Graphics::Direct3D11::*;

        const W: u32 = 320;
        const H: u32 = 240;
        /// 桌面底色（BGRA）
        const DESKTOP: [u8; 4] = [0x40, 0x80, 0xC0, 0xFF];
        /// 光标色（BGRA）：纯红
        const CURSOR: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF];

        let _guard = MfRuntimeGuard::new().unwrap();
        let mut device_opt: Option<ID3D11Device> = None;
        let mut context_opt: Option<ID3D11DeviceContext> = None;
        let creation_flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let hr = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                creation_flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device_opt),
                None,
                Some(&mut context_opt),
            )
        };
        if hr.is_err() {
            println!("没有可用的 D3D11 硬件设备，跳过");
            return;
        }
        let device = device_opt.unwrap();
        let context = context_opt.unwrap();

        let dir = std::env::temp_dir();
        let path = dir.join("test_gpu_cursor.mp4");
        let _ = std::fs::remove_file(&path);
        let mut sink = GpuSinkWriter::try_new(
            &device,
            &context,
            &path.to_string_lossy(),
            W,
            H,
            30,
            2000,
            GpuCodec::H264,
            ContainerMode::Plain,
            None,
        )
        .expect("H264 普通 MP4 应能打开");

        // 假桌面：一整片纯色，方便判断某个像素到底是谁画上去的。
        let source = create_render_texture(&device, W, H).unwrap();
        let desktop = vec![DESKTOP[0]; (W * H) as usize];
        let mut desktop_bgra = Vec::with_capacity((W * H * 4) as usize);
        for _ in 0..(W * H) {
            desktop_bgra.extend_from_slice(&DESKTOP);
        }
        let _ = desktop;
        unsafe {
            context.UpdateSubresource(
                &source,
                0,
                None,
                desktop_bgra.as_ptr() as *const std::ffi::c_void,
                W * 4,
                0,
            );
        }

        // 8×8 不透明纯红光标
        let cursor_data: Vec<u8> = std::iter::repeat_n(CURSOR, 64).flatten().collect();
        let cursor_info = windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
            Width: 8,
            Height: 8,
            Pitch: 32,
            HotSpot: windows::Win32::Foundation::POINT { x: 0, y: 0 },
        };
        let shape =
            crate::capture::CursorShape::parse(cursor_info.Type, &cursor_info, &cursor_data)
                .expect("8×8 的 32bpp 光标应能解析");

        let pointer_at = |x: i32, y: i32| crate::capture::PointerFrame {
            state: crate::capture::PointerState {
                visible: true,
                x,
                y,
            },
            shape: Some(&shape),
        };

        // 回读辅助：把 cropped_texture 拷到 staging 再取某个像素。
        // `sink` 显式作为参数传进来，避免闭包长期借用它而挡住后面的 `push_frame`。
        let read_pixel = |sink: &GpuSinkWriter, x: u32, y: u32| -> [u8; 4] {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: W,
                Height: H,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            unsafe {
                device
                    .CreateTexture2D(&desc, None, Some(&mut staging))
                    .unwrap()
            };
            let staging = staging.unwrap();
            unsafe {
                context.CopyResource(&staging, &sink.cropped_texture);
                let mut mapped = Default::default();
                context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .unwrap();
                let base = mapped.pData as *const u8;
                let off = y as usize * mapped.RowPitch as usize + x as usize * 4;
                let px = std::slice::from_raw_parts(base.add(off), 4);
                let out = [px[0], px[1], px[2], px[3]];
                context.Unmap(&staging, 0);
                out
            }
        };

        // 第 1 帧：光标画在 (50, 60)
        sink.push_frame(
            &source,
            0,
            0,
            0,
            FrameMeta {
                desktop_updated: true,
                pointer: Some(pointer_at(50, 60)),
            },
        )
        .unwrap();

        assert_eq!(read_pixel(&sink, 54, 64), CURSOR, "光标中心应被换成光标色");
        assert_eq!(
            read_pixel(&sink, 10, 10),
            DESKTOP,
            "远离光标处应保持桌面原色"
        );

        // 第 2 帧：光标移到 (200, 100)。此时桌面没有更新（只有指针动）。
        sink.push_frame(
            &source,
            0,
            0,
            33_333,
            FrameMeta {
                desktop_updated: false,
                pointer: Some(pointer_at(200, 100)),
            },
        )
        .unwrap();

        assert_eq!(read_pixel(&sink, 204, 104), CURSOR, "光标新位置应被画上");
        assert_eq!(
            read_pixel(&sink, 54, 64),
            DESKTOP,
            "光标旧址必须恢复成桌面色——否则快速移动会拖出一串残影"
        );

        // 第 3 帧：指针不可见，整个画面都该是桌面色。
        let invisible = crate::capture::PointerFrame {
            state: crate::capture::PointerState {
                visible: false,
                x: 200,
                y: 100,
            },
            shape: Some(&shape),
        };
        sink.push_frame(
            &source,
            0,
            0,
            66_666,
            FrameMeta {
                desktop_updated: false,
                pointer: Some(invisible),
            },
        )
        .unwrap();
        assert_eq!(
            read_pixel(&sink, 204, 104),
            DESKTOP,
            "指针不可见时不应留下任何光标像素"
        );

        sink.finish().unwrap();
        let _ = std::fs::remove_file(&path);
        println!("GPU 指针合成通过：位置正确、无残影、不可见时不绘制");
    }

    /// 需要真实硬件编码器与 MF 运行时；无头 CI / 无 GPU 环境请用 `--ignored` 显式启用。

    #[test]
    #[ignore = "需要真实 GPU 硬件编码器（本地手动验证）"]
    fn test_gpu_sink_writer_pipeline() {
        use windows::Win32::Graphics::Direct3D::*;
        use windows::Win32::Graphics::Direct3D11::*;

        let _guard = MfRuntimeGuard::new().unwrap();
        let mut device_opt: Option<ID3D11Device> = None;
        let mut context_opt: Option<ID3D11DeviceContext> = None;
        let creation_flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;

        let hr = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                creation_flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device_opt),
                None,
                Some(&mut context_opt),
            )
        };
        if hr.is_err() {
            println!("No hardware D3D11 device available, skipping test");
            return;
        }

        let device = device_opt.unwrap();
        let context = context_opt.unwrap();

        let temp_dir = std::env::temp_dir();
        for codec in [GpuCodec::H264, GpuCodec::Hevc, GpuCodec::Av1] {
            let label = codec.name();
            let ext_label = match codec {
                GpuCodec::H264 => "h264",
                GpuCodec::Hevc => "hevc",
                GpuCodec::Av1 => "av1",
            };
            // 三种容器模式都要试：MF 自带的分片 sink 对 HEVC/AV1 会拒绝，
            // 自研 sink 能救 HEVC，AV1 只有普通 MP4 一条路。
            for container in [
                ContainerMode::Fragmented,
                ContainerMode::RawFragmented,
                ContainerMode::Plain,
            ] {
                let mode = container.name();
                let test_output = temp_dir.join(format!(
                    "test_gpu_mfsink_{ext_label}_{}.mp4",
                    container.tag()
                ));
                let output_str = test_output.to_string_lossy().to_string();
                let _ = std::fs::remove_file(&test_output);

                let mut sink = match GpuSinkWriter::try_new(
                    &device,
                    &context,
                    &output_str,
                    640,
                    480,
                    30,
                    2000,
                    codec,
                    container,
                    None,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        // 失败的尝试也会留下（空）文件，先清掉再换下一种组合。
                        let _ = std::fs::remove_file(&test_output);
                        // 分片路径失败是已知的平台/编码器限制（Fragmented 只认 H.264，
                        // RawFragmented 还不支持 AV1），不算失败；连普通 MP4 都失败
                        // 才算这台机器真的编不了这个编码器。
                        if container != ContainerMode::Plain {
                            println!("{label} + {mode} 不可用（{e}），换下一种容器");
                            continue;
                        }
                        if codec == GpuCodec::Av1 {
                            println!("AV1 hardware encoder not available ({e}), safely skipped");
                            continue;
                        }
                        panic!("GpuSinkWriter::try_new ({label} / {mode}) failed: {e}");
                    }
                };

                let raw_frame = vec![128u8; 640 * 480 * 4];
                let push_res = sink.push_raw_bgra(&raw_frame, 640 * 4, 0);
                assert!(
                    push_res.is_ok(),
                    "push_raw_bgra failed ({label} / {mode}): {:?}",
                    push_res
                );

                for i in 1..=10u64 {
                    let dup_res = sink.push_duplicate_frame(i * 33_333);
                    assert!(
                        dup_res.is_ok(),
                        "push_duplicate_frame failed ({label} / {mode}): {:?}",
                        dup_res
                    );
                }

                let finish_res = sink.finish();
                assert!(
                    finish_res.is_ok(),
                    "finish failed ({label} / {mode}): {:?}",
                    finish_res
                );

                let file_size = std::fs::metadata(&test_output)
                    .unwrap_or_else(|e| panic!("{label} / {mode} 没有产物: {e}"))
                    .len();
                assert!(file_size > 0, "{label} / {mode} 产物不应为空");

                // 停止后必须是普通 MP4：顶层盒里能找到 moov，且没有 moof 残留。
                let bytes = std::fs::read(&test_output).unwrap();
                let mut kinds: Vec<[u8; 4]> = Vec::new();
                let mut pos = 0usize;
                while pos + 8 <= bytes.len() {
                    let size32 = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
                    let size = match size32 {
                        0 => bytes.len() - pos,
                        1 => u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap())
                            as usize,
                        n => n as usize,
                    };
                    kinds.push(bytes[pos + 4..pos + 8].try_into().unwrap());
                    pos += size;
                }
                assert_eq!(pos, bytes.len(), "{label} / {mode} 顶层盒应覆盖整个文件");
                assert!(kinds.contains(b"moov"), "{label} / {mode} 停止后应有 moov");
                assert!(
                    !kinds.contains(b"moof"),
                    "{label} / {mode} 不应残留分片结构"
                );

                // stsd 里的 sample entry 四字类型必须对得上编码器——这是
                // 「自研写盒」最容易写错的地方（AV1/HEVC/AVC 的 entry 名各不相同）。
                let expect_entry: &[u8; 4] = match codec {
                    GpuCodec::H264 => b"avc1",
                    GpuCodec::Hevc => b"hvc1",
                    GpuCodec::Av1 => b"av01",
                };
                let expect_config: &[u8; 4] = match codec {
                    GpuCodec::H264 => b"avcC",
                    GpuCodec::Hevc => b"hvcC",
                    GpuCodec::Av1 => b"av1C",
                };
                assert!(
                    bytes.windows(4).any(|w| w == expect_entry),
                    "{label} / {mode} 缺少 sample entry {:?}",
                    std::str::from_utf8(expect_entry).unwrap()
                );
                assert!(
                    bytes.windows(4).any(|w| w == expect_config),
                    "{label} / {mode} 缺少配置盒 {:?}",
                    std::str::from_utf8(expect_config).unwrap()
                );

                // 最后一道裁判：让 MF 自己把文件当视频读一遍。
                // 结构写错（stsd 的 entry 名、avcC/hvcC 的参数集团、长度前缀）
                // 只有真解码器才会报出来。
                let (frames, tw, th) = mf_readback(&test_output);
                assert_eq!((tw, th), (640, 480), "{label} / {mode} 读回的分辨率不对");
                assert!(frames > 0, "{label} / {mode} 一帧都解不出来");
                println!(
                    "{label} / {mode} OK, {file_size} 字节, 顶层盒 {kinds:?}, 读回 {frames} 帧"
                );

                let _ = std::fs::remove_file(&test_output);
            }
        }
    }
}
