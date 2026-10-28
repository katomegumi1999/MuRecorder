//! AAC-LC 音频编码模块（Media Foundation MFT 驱动）。
//!
//! 固定规格：48000 Hz / 16-bit / 双声道 / 128 kbps / 裸 AAC 帧（Raw AAC，无 ADTS 头）。
//! 对应的 AudioSpecificConfig 为常量 `[0x11, 0x90]`。

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
    MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio,
    MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{Error as WinError, Result as WinResult};

/// 48kHz 双声道 AAC-LC 的 AudioSpecificConfig（2 字节常量：0x11 0x90）。
pub const ASC_48K_STEREO: [u8; 2] = [0x11, 0x90];

/// 编码后的单帧裸 AAC 样本。
#[derive(Debug, Clone)]
pub struct AacPacket {
    /// 裸 AAC 帧数据（无 ADTS 头，无 4 字节长度前缀）
    pub data: Vec<u8>,
    /// PTS（微秒）
    pub pts_us: u64,
}

/// AAC 音频编码器（驱动 Media Foundation AAC MFT）。
pub struct AacEncoder {
    transform: IMFTransform,
    pcm_buffer: Vec<u8>,
    total_input_samples: u64,
    output_queue: VecDeque<AacPacket>,
    output_cb_size: usize,
    mft_provides_output_sample: bool,
}

impl AacEncoder {
    /// 创建并初始化 AAC MFT（默认 48k/16bit/2ch 320kbps Raw AAC）。
    pub fn new() -> WinResult<Self> {
        Self::with_bitrate_kbps(320)
    }

    /// 创建并初始化指定码率的 AAC MFT（48k/16bit/2ch Raw AAC，支持 96/128/160/192/256/320 kbps）。
    pub fn with_bitrate_kbps(bitrate_kbps: u32) -> WinResult<Self> {
        let in_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_PCM,
        };
        let out_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_AAC,
        };

        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        unsafe {
            // **只枚举同步 MFT**（`MFT_ENUM_FLAG_SYNCMFT`）。
            //
            // 本模块是按同步契约驱动的：`ProcessInput` 立刻收下、`ProcessOutput`
            // 立刻出包。异步 MFT 的契约完全不同——输入要靠 `METransformNeedInput`
            // 事件驱动，输出要靠 `METransformHaveOutput`，还得 `MF_TRANSFORM_ASYNC_UNLOCK`
            // 解锁，用同步方式驱动它只会得到 `MF_E_TRANSFORM_ASYNC_LOCKED` 或空转。
            //
            // 早先这里同时要了 `MFT_ENUM_FLAG_ASYNCMFT`，但没有一行异步代码；
            // 如果哪天系统里排在前面的 AAC 编码器恰好是异步的，`ActivateObject`
            // 会成功、然后一路失败在一个看不出原因的地方。宁可在枚举阶段就把它排除。
            MFTEnumEx(
                MFT_CATEGORY_AUDIO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&in_info),
                Some(&out_info),
                &mut activates,
                &mut count,
            )?;
        }

        if count == 0 || activates.is_null() {
            return Err(WinError::new(
                E_FAIL,
                "系统未找到同步模式的 AAC 音频编码器 (MFT)",
            ));
        }

        let slice = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        let mut chosen_transform: Option<IMFTransform> = None;

        for act in slice.iter().flatten() {
            if let Ok(transform) = unsafe { act.ActivateObject::<IMFTransform>() } {
                // 双重确认：即便枚举旗标已经过滤过，也要对**实际拿到的对象**
                // 再验一次，因为枚举结果和激活结果不保证一致。
                if is_async_mft(&transform) {
                    continue;
                }
                chosen_transform = Some(transform);
                break;
            }
        }

        unsafe {
            CoTaskMemFree(Some(activates as *const _));
        }

        let transform = chosen_transform.ok_or_else(|| {
            WinError::new(
                E_FAIL,
                "无法激活同步模式的 AAC 编码器 MFT（枚举到的都是异步 MFT，\
                 或激活全部失败）",
            )
        })?;

        // 1. 设置输出类型：48kHz / 2ch / Raw AAC / 指定码率
        let avg_bytes_per_sec = (bitrate_kbps * 1000) / 8;
        let out_type: IMFMediaType = unsafe { MFCreateMediaType()? };
        unsafe {
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            out_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)?;
            out_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            out_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bytes_per_sec)?;
            out_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // 0 = Raw AAC (重要！)
            out_type.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?; // AAC-LC
            transform.SetOutputType(0, &out_type, 0)?;
        }

        // 2. 设置输入类型：48kHz / 16bit / 2ch PCM
        let in_type: IMFMediaType = unsafe { MFCreateMediaType()? };
        unsafe {
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            in_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            in_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)?;
            in_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            in_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            in_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 48_000 * 4)?; // 192000 B/s
            transform.SetInputType(0, &in_type, 0)?;
        }

        // 3. 查询输出流参数
        let stream_info = unsafe { transform.GetOutputStreamInfo(0)? };
        let mft_provides_output_sample =
            stream_info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
        let output_cb_size = (stream_info.cbSize as usize).max(2048);

        Ok(Self {
            transform,
            pcm_buffer: Vec::with_capacity(4096 * 4),
            total_input_samples: 0,
            output_queue: VecDeque::new(),
            output_cb_size,
            mft_provides_output_sample,
        })
    }

    /// 输入 PCM 数据（16-bit LE, 48kHz, 立体声，每采样点 4 字节）。
    pub fn push_pcm(&mut self, pcm: &[u8]) -> WinResult<()> {
        self.pcm_buffer.extend_from_slice(pcm);
        self.process_pending_pcm(false)?;
        Ok(())
    }

    /// 取走所有当前已编码就绪的 AAC 帧。
    pub fn drain(&mut self) -> WinResult<Vec<AacPacket>> {
        let mut packets = Vec::with_capacity(self.output_queue.len());
        while let Some(pkt) = self.output_queue.pop_front() {
            packets.push(pkt);
        }
        Ok(packets)
    }

    /// 冲刷编码器缓冲并返回所有剩余输出帧。
    pub fn finish(&mut self) -> WinResult<Vec<AacPacket>> {
        // 如果还有剩余不足 1 帧的 PCM，补零补齐到 1024 采样帧（4096 字节）
        if !self.pcm_buffer.is_empty() {
            let rem = self.pcm_buffer.len() % 4096;
            if rem != 0 {
                self.pcm_buffer
                    .resize(self.pcm_buffer.len() + (4096 - rem), 0);
            }
            self.process_pending_pcm(true)?;
        }

        // 发送 DRAIN 消息促使 MFT 冲刷所有内部保留的 lookahead / 残留帧
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }

        // 冲刷 MFT 内部残留
        self.drain_mft_outputs()?;

        self.drain()
    }

    fn process_pending_pcm(&mut self, flush: bool) -> WinResult<()> {
        // AAC-LC 每帧固定 1024 采样点 = 1024 * 4 = 4096 字节
        const FRAME_BYTES: usize = 4096;

        while self.pcm_buffer.len() >= FRAME_BYTES {
            let chunk = self.pcm_buffer.drain(..FRAME_BYTES).collect::<Vec<u8>>();
            let sample_idx = self.total_input_samples;
            self.total_input_samples += 1024;

            let _pts_us = (sample_idx as u128 * 1_000_000 / 48_000) as u64;
            let time_100ns = (sample_idx as i128 * 10_000_000 / 48_000) as i64;
            let duration_100ns = (1024i128 * 10_000_000 / 48_000) as i64;

            // 创建 IMFSample 包装 PCM 数据
            let media_buffer: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(FRAME_BYTES as u32)? };
            let mut ptr = std::ptr::null_mut();
            let mut max_len = 0u32;
            let mut cur_len = 0u32;
            unsafe {
                media_buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
                std::ptr::copy_nonoverlapping(chunk.as_ptr(), ptr, FRAME_BYTES);
                media_buffer.Unlock()?;
                media_buffer.SetCurrentLength(FRAME_BYTES as u32)?;
            }

            let sample: IMFSample = unsafe { MFCreateSample()? };
            unsafe {
                sample.AddBuffer(&media_buffer)?;
                sample.SetSampleTime(time_100ns)?;
                sample.SetSampleDuration(duration_100ns)?;
            }

            // 送入 MFT
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // MFT 缓冲区满，先拉取输出再重试
                    self.drain_mft_outputs()?;
                    unsafe { self.transform.ProcessInput(0, &sample, 0)? };
                }
                Err(e) => return Err(e),
            }

            // 每次喂入后尝试拉取输出
            self.drain_mft_outputs()?;
        }

        if flush {
            self.drain_mft_outputs()?;
        }

        Ok(())
    }

    fn drain_mft_outputs(&mut self) -> WinResult<()> {
        loop {
            let output_sample: Option<IMFSample> = if !self.mft_provides_output_sample {
                let sample: IMFSample = unsafe { MFCreateSample()? };
                let buf: IMFMediaBuffer =
                    unsafe { MFCreateMemoryBuffer(self.output_cb_size as u32)? };
                unsafe {
                    sample.AddBuffer(&buf)?;
                }
                Some(sample)
            } else {
                None
            };

            let mut output_data = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(output_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };

            let mut status = 0u32;
            let res = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output_data), &mut status)
            };

            let sample_opt = ManuallyDrop::into_inner(output_data.pSample);
            let _ = ManuallyDrop::into_inner(output_data.pEvents);

            match res {
                Ok(()) => {
                    if let Some(sample) = sample_opt {
                        let pkt = extract_aac_packet(&sample)?;
                        self.output_queue.push_back(pkt);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// 判断一个已激活的 MFT 是不是异步模式。
///
/// `MF_TRANSFORM_ASYNC` 是**属性存在即为真**的约定；缺省（返回未找到）视为同步。
fn is_async_mft(transform: &IMFTransform) -> bool {
    let attrs = match unsafe { transform.GetAttributes() } {
        Ok(a) => a,
        // 取不到属性就按同步处理：绝大多数 MFT 都实现了 GetAttributes，
        // 真拿不到时也没法用异步方式驱动它。
        Err(_) => return false,
    };
    matches!(unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }, Ok(v) if v != 0)
}

fn extract_aac_packet(sample: &IMFSample) -> WinResult<AacPacket> {
    let time_100ns = unsafe { sample.GetSampleTime().unwrap_or(0) };
    let pts_us = (time_100ns.max(0) as u128 / 10) as u64;

    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    let mut ptr = std::ptr::null_mut();
    let mut cur_len = 0u32;
    unsafe {
        buffer.Lock(&mut ptr, None, Some(&mut cur_len))?;
    }
    let data = unsafe { std::slice::from_raw_parts(ptr, cur_len as usize) }.to_vec();
    unsafe {
        let _ = buffer.Unlock();
    }

    Ok(AacPacket { data, pts_us })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_encoder_produces_raw_frames_and_matches_asc() {
        // 初始化 MF 运行时
        let _guard = crate::encode::mf_sink::MfRuntimeGuard::new().unwrap();

        let mut encoder = AacEncoder::new().expect("创建 AAC 编码器失败");
        assert_eq!(ASC_48K_STEREO, [0x11, 0x90]);

        // 喂入 1 秒的立体声 48kHz 正弦波 / 静音 PCM (48000 * 4 字节)
        let pcm = vec![0u8; 48000 * 4];
        encoder.push_pcm(&pcm).unwrap();

        let packets = encoder.drain().unwrap();
        // 48000 / 1024 ≈ 46.875 帧，drain 应该能拉出 46 帧左右
        assert!(
            packets.len() >= 40 && packets.len() <= 48,
            "已输出帧数: {}",
            packets.len()
        );

        for pkt in &packets {
            assert!(!pkt.data.is_empty());
            // 裸 AAC 帧不应以 ADTS 同步字 (0xFFF) 开头
            if pkt.data.len() >= 2 {
                let is_adts = pkt.data[0] == 0xFF && (pkt.data[1] & 0xF0) == 0xF0;
                assert!(!is_adts, "编码输出必须是 Raw AAC，不能带 ADTS 头！");
            }
        }

        let finish_packets = encoder.finish().unwrap();
        let total = packets.len() + finish_packets.len();
        // 1 秒正好是 48000 采样，向上整除 1024 帧是 47 帧
        assert_eq!(total, 47, "1 秒音频经 finish 后应恰好产出 47 帧 AAC");
    }

    #[test]
    fn aac_encoder_supports_selectable_bitrates() {
        let _guard = crate::encode::mf_sink::MfRuntimeGuard::new().unwrap();
        for &kbps in &[96, 128, 160, 192, 256, 320] {
            let mut encoder = AacEncoder::with_bitrate_kbps(kbps)
                .unwrap_or_else(|e| panic!("初始化 {kbps} kbps 编码器失败: {e}"));
            let pcm = vec![0u8; 48000 * 4];
            encoder.push_pcm(&pcm).unwrap();
            let packets = encoder.drain().unwrap();
            assert!(!packets.is_empty(), "码率 {kbps} 应产出数据包");
        }
    }
}
