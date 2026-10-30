//! GPU 硬件 SinkWriter 封装：媒体类型协商、帧注入、自研 sink 抽水与收尾。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_MAP_READ_WRITE, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQuality,
    CODECAPI_AVEncCommonRateControlMode, ICodecAPI, IMFAttributes,
    IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSinkWriter,
    MF_E_TOPO_CODEC_NOT_FOUND, MF_LOW_LATENCY, MF_MPEG4SINK_MIN_FRAGMENT_DURATION,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_D3D_MANAGER,
    MF_TRANSCODE_CONTAINERTYPE, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFCreateSinkWriterFromMediaSink, MFCreateSinkWriterFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFNominalRange_16_235, MFTranscodeContainerType_FMPEG4,
    MFTranscodeContainerType_MPEG4, MFVideoFormat_ARGB32, MFVideoFormat_AV1, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709,
    MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
    eAVEncCommonRateControlMode_PeakConstrainedVBR,
    eAVEncCommonRateControlMode_UnconstrainedVBR,
};
use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use windows::core::{Error as WinError, Interface, PCWSTR, Result as WinResult};

use crate::capture::dxgi::FrameMeta;
use crate::capture::overlay::OverlayPlan;
use crate::encode::sink_raw::{RawMediaSink, StreamKind};
use crate::mux::mp4::Mp4Muxer;
use crate::mux::nal::annexb_to_mp4_sample;
use crate::mux::remux::{finalize_fragmented, finalize_plain};

use super::converter::{
    GpuColorConverter, MfRuntimeGuard, create_cursor_staging_texture, create_render_texture,
};
use super::probe::has_hardware_encoder;
use super::types::{ContainerMode, GpuCodec, sample_description};
use super::{FRAGMENT_DURATION_100NS, set_attribute_ratio, set_attribute_size};

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
    converter: Option<GpuColorConverter>,
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

            // 2. 若启用 H.264 / HEVC / AV1，构建自适应双保险颜色转换器 (BGRA -> NV12)
            // 优先启用 D3D11 Compute Shader 直写 NV12，不支持 NV12 UAV 时自动平滑降级至纯净版 VideoProcessor
            let converter = if codec.requires_nv12() {
                Some(GpuColorConverter::try_new(
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

            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            const MF_MT_MPEG2_PROFILE: windows::core::GUID =
                windows::core::GUID::from_u128(0xad76a00b_2d55_4e60_aa05_0207527019da);
            const E_AV_ENC_H265_V_PROFILE_MAIN_420_8: u32 = 1;
            const E_AV_ENC_H264_V_PROFILE_HIGH: u32 = 100;

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
                    out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, E_AV_ENC_H264_V_PROFILE_HIGH)?;
                }
            }
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
            set_attribute_size(&out_type, &MF_MT_FRAME_SIZE, aligned_width, aligned_height)?;
            set_attribute_ratio(&out_type, &MF_MT_FRAME_RATE, fps, 1)?;
            set_attribute_ratio(&out_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;

            // 显式写入 BT.709 色彩标准与有限动态范围 (16-235) 元数据，彻底匹配 OBS 与播放器标准色彩
            out_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            out_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            out_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            out_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;

            let stream_index = sink_writer.AddStream(&out_type)?;

            // 7. 配置输入流格式
            // 核心要点：
            // H.264 / HEVC / AV1 统一在 GPU 显存内通过 D3D11 Compute Shader 完成高精度转换，
            // 直接传入标准 NV12（Limited Range 16-235），杜绝 MF 内部插入多余软件转换器导致死锁或色彩劣化。
            let in_type: IMFMediaType = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
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
            configure_codec_vbr_peak(&sink_writer, stream_index, bitrate_bps);

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
                StreamKind::Video => {
                    for s in st.samples {
                        let data = if raw.codec == GpuCodec::Av1 {
                            // AV1 的样本是 OBU 序列（Temporal Unit），MP4 格式直接收纳
                            s.data
                        } else {
                            annexb_to_mp4_sample(&s.data, hevc)
                        };
                        muxer
                            .write_video_sample(&data, s.pts_us, s.is_key)
                            .map_err(|e| WinError::new(E_FAIL, format!("写视频样本失败: {e}")))?;
                    }
                }
                StreamKind::Audio => {
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

    /// 核心调用：在显存内完成子区域裁剪、（可选）指针与特效叠加，并直接包装送入 GPU 编码器。
    ///
    /// `pts_us` 必须是**真实采集时刻**（相对录制起点的 QPC 微秒）。
    /// 早期实现用 `frame_count × 帧间隔` 生成时间戳，一旦编码或写盘跟不上，
    /// 时间轴就按帧数压缩——60 秒的实录可能只播 20 秒，接入音频后必然音画不同步。
    ///
    /// `meta.overlay` 为 `Some` 表示本次录制开启了指针合成（形状是否就绪、有没有要
    /// 画的东西另说）；`meta.desktop_updated` 表示显存里的桌面图像本帧是否真的变了。
    /// 后者为 false 时只需把上次存下的纯桌面副本再摊一次，不必再截一次显存
    /// ——「只有鼠标在动、画面别处完全静止」是很常见的情形。
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

        match meta.overlay {
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
            // 开启叠加：先维护「纯桌面」副本，再从它派生本帧画面。
            Some(plan) => {
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
                self.repaint_from_desktop(&plan)?;
            }
        }

        unsafe { self.write_current_buffer(pts_us) }
    }

    /// 桌面没变、但叠加层还在动（波纹扩散 / 残影淡出）时，重新派生一帧。
    ///
    /// 为什么必须单独有这条路：DXGI 只在画面或指针变化时交付帧，而特效是**时间**
    /// 驱动的——用户点一下不动鼠标，桌面与指针都不变，靠 `push_frame` 永远等不到
    /// 下一帧，波纹就冻在第一帧。调用方在采集超时后走这里，从 `desktop_texture`
    /// 重画一遍即可，不需要新的显存帧。
    ///
    /// 返回 `false` 表示还没拿到过桌面内容（首帧未就绪），调用方应退回「复制上一帧」。
    pub fn push_overlay_frame(&mut self, pts_us: u64, plan: &OverlayPlan<'_>) -> WinResult<bool> {
        if !self.desktop_valid {
            return Ok(false);
        }
        self.repaint_from_desktop(plan)?;
        unsafe { self.write_current_buffer(pts_us) }.map(|()| true)
    }

    /// 从「纯桌面」副本重新派生 `cropped_texture` 并叠加 `plan`。
    ///
    /// **每帧都从干净底重来**，这是「不留残影」的全部要点：新指针不会画在旧指针上，
    /// 残影的淡出也不会一层层叠死。
    fn repaint_from_desktop(&mut self, plan: &OverlayPlan<'_>) -> WinResult<()> {
        let desktop = self
            .desktop_texture
            .as_ref()
            .ok_or_else(|| WinError::from_hresult(E_FAIL))?;
        unsafe { self.context.CopyResource(&self.cropped_texture, desktop) };
        if !plan.is_empty() {
            self.composite_overlay(plan)?;
        }
        Ok(())
    }

    /// 把叠加层画到 `cropped_texture` 上：只回读包围盒，合成后写回。
    fn composite_overlay(&mut self, plan: &OverlayPlan<'_>) -> WinResult<()> {
        // 包围盒由叠加层自己算（所有元素的并集，已裁到画面内），CPU/GPU 两条路共用，
        // 避免对「部分可见」的判断出现分歧。
        let Some((left, top, bw, bh)) = plan.bounds(self.width, self.height) else {
            // 完全在画面外（常见：鼠标在另一块屏上）。
            return Ok(());
        };

        let staging = self.ensure_overlay_staging(bw, bh)?.clone();

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
        let touched = {
            // 行距按 RowPitch 算，不要假设等于 bw*4（驱动常按 256 字节对齐）。
            let stride = mapped.RowPitch as usize;
            let len = stride * bh as usize;
            // Safety: staging 纹理是本函数刚建的、至少 bw×bh，Map 保证可读写整块。
            let buf = unsafe { std::slice::from_raw_parts_mut(mapped.pData as *mut u8, len) };
            // 缓冲区原点对应画面 (left, top)：把原点告诉叠加层，它自己处理越界裁剪。
            plan.draw_into(
                &mut crate::capture::Canvas::new(buf, stride, bw, bh),
                (left as i32, top as i32),
            )
        };
        unsafe {
            self.context.Unmap(&staging, 0);
        }

        // 一个像素都没改（例如指针正好完全落在画面外）就别白搬一趟显存。
        if touched == 0 {
            return Ok(());
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

    /// 按需创建/扩容叠加区域回读用的暂存纹理（STAGING，CPU 可读写）。
    ///
    /// 只在需要的区域变大时重建；单一光标时区域很小，带轨迹时上限由叠加层的面积
    /// 上限兜住，所以不会出现「一路扩容到整帧」。
    fn ensure_overlay_staging(&mut self, w: u32, h: u32) -> WinResult<&ID3D11Texture2D> {
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

/// 将 u32 数值封装为 VT_UI4 类型的 COM VARIANT
fn variant_from_u32(val: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        (*var.Anonymous.Anonymous).Anonymous.ulVal = val;
    }
    var
}

/// 针对底层 GPU 编码器（NVENC / Intel QuickSync / AMD AMF）配置
/// 动态峰值 VBR（Peak-Constrained VBR / 动态突发浮动缓冲）。
///
/// 传统固定码率 CBR 在屏幕出现局部高频形变（如鼠标发光拖尾、快速划动）时，
/// 帧内残差暴增导致虚拟缓冲瞬间被击穿，编码器被迫将 QP 拉升至 40~50 从而产生马赛克模糊。
/// 本函数通过 ICodecAPI 开放 2.0x 动态峰值突发与弹性 VBV 缓冲区，
/// 既保证平时平均码率合规，又能在光轨运动时提供充足码率，消除局部模糊。
fn configure_codec_vbr_peak(
    sink_writer: &IMFSinkWriter,
    stream_index: u32,
    bitrate_bps: u32,
) {
    unsafe {
        let mut codec_api_raw: *mut core::ffi::c_void = std::ptr::null_mut();
        // Media Foundation 规范：通过 GetServiceForStream 传入 GUID_NULL 查询底层编码器 MFT 的 ICodecAPI
        let hr = sink_writer.GetServiceForStream(
            stream_index,
            &windows::core::GUID::zeroed(),
            &ICodecAPI::IID,
            &mut codec_api_raw,
        );
        if hr.is_err() || codec_api_raw.is_null() {
            return;
        }

        let codec_api: ICodecAPI = core::mem::transmute(codec_api_raw);

        // 1. 设置码率控制模式为 Peak-Constrained VBR (1)
        // 若驱动不支持 PeakConstrainedVBR，则尝试降级为 UnconstrainedVBR (2)
        let vbr_mode = variant_from_u32(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32);
        if codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &vbr_mode).is_err() {
            let unconstrained_vbr = variant_from_u32(eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32);
            let _ = codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &unconstrained_vbr);
        }

        // 2. 设置平均目标码率 (Mean Bitrate)
        let mean_bitrate = variant_from_u32(bitrate_bps);
        let _ = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &mean_bitrate);

        // 3. 设置峰值码率 (Max Bitrate) 为平均码率的 2.0 倍（例如 8 Mbps 目标码率允许突发至 16 Mbps）
        // 在屏幕发生大面积非刚体运动（如光轨涂抹、快速滚动）时，动态爆发码率防止 QP 暴涨导致的马赛克
        let max_bitrate = variant_from_u32(bitrate_bps.saturating_mul(2));
        let _ = codec_api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &max_bitrate);

        // 4. 设置 VBV / HRD 编码虚拟缓冲区大小（以字节为单位）
        // 允许约 250ms 的码率平滑突发缓冲（bitrate_bps / 8 / 4 字节），避免瞬时峰值击穿缓冲区
        let buffer_size_bytes = bitrate_bps / 32;
        let buffer_size = variant_from_u32(buffer_size_bytes.max(64 * 1024));
        let _ = codec_api.SetValue(&CODECAPI_AVEncCommonBufferSize, &buffer_size);

        // 5. 设置编码质量基准 (75 / 100)
        let quality = variant_from_u32(75);
        let _ = codec_api.SetValue(&CODECAPI_AVEncCommonQuality, &quality);
    }
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
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
        };

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
        let mut desktop_bgra = Vec::with_capacity((W * H * 4) as usize);
        for _ in 0..(W * H) {
            desktop_bgra.extend_from_slice(&DESKTOP);
        }
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

        let cursor_plan = |x: i32, y: i32| {
            crate::capture::overlay::OverlayPlan::cursor_only(
                &shape,
                crate::capture::PointerState {
                    visible: true,
                    x,
                    y,
                },
            )
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
                overlay: Some(cursor_plan(50, 60)),
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
                overlay: Some(cursor_plan(200, 100)),
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
        let invisible = crate::capture::overlay::OverlayPlan::cursor_only(
            &shape,
            crate::capture::PointerState {
                visible: false,
                x: 200,
                y: 100,
            },
        );
        sink.push_frame(
            &source,
            0,
            0,
            66_666,
            FrameMeta {
                desktop_updated: false,
                overlay: Some(invisible),
            },
        )
        .unwrap();
        assert_eq!(
            read_pixel(&sink, 204, 104),
            DESKTOP,
            "指针不可见时不应留下任何光标像素"
        );

        // 第 4 步：动画帧。桌面与指针都不动，只有波纹在扩散——走 push_overlay_frame。
        let mut overlay =
            crate::capture::overlay::CursorOverlay::new(crate::capture::overlay::OverlayConfig {
                click_effect: true,
                trail_ms: 0,
            });
        let still = crate::capture::PointerState {
            visible: false,
            x: 0,
            y: 0,
        };
        overlay.observe(still, (0, 0), 0);
        overlay.push_ripple_for_test(100, 120, 0);
        let ripple_plan = overlay.plan(Some(&shape), still, 50_000, W, H);
        assert!(!ripple_plan.is_empty(), "波纹应当产生非空的叠加计划");
        assert!(
            sink.push_overlay_frame(100_000, &ripple_plan).unwrap(),
            "桌面就绪后动画帧应当写成功"
        );

        // 圆环半径此刻约 16 px：沿 x 轴扫一遍，环上必须有像素被改写。
        let ring_touched = (100..130).any(|x| read_pixel(&sink, x, 120) != DESKTOP);
        assert!(ring_touched, "波纹圆环应当画进画布");

        // 第 5 步：波纹过期后，同一块区域必须恢复成桌面色。
        // 这一步验证的正是「每帧从纯桌面重来」——否则环会永久留在画布上。
        let settled = crate::capture::overlay::OverlayPlan::cursor_only(
            &shape,
            crate::capture::PointerState {
                visible: false,
                x: 0,
                y: 0,
            },
        );
        sink.push_overlay_frame(200_000, &settled).unwrap();
        let ring_gone = (100..130).all(|x| read_pixel(&sink, x, 120) == DESKTOP);
        assert!(ring_gone, "波纹消失后必须抹干净，不能留下静态圆环");

        sink.finish().unwrap();
        let _ = std::fs::remove_file(&path);
        println!("GPU 叠加通过：位置正确、无残影、不可见时不绘制、动画帧可重绘并抹净");
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
