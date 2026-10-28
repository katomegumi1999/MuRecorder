//! WASAPI loopback 音频采集模块。
//!
//! 支持三种音源：
//! - `DefaultRender`（系统扬声器/默认渲染设备）
//! - `DeviceId`（指定端点 ID 字符串）
//! - `ProcessTree`（指定进程及其子进程环回音频）
//!
//! 关键技术点：
//! - `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` 让系统完成重采样与格式转换，固定输出 48000Hz 16-bit 立体声。
//! - 静音标志：检测 `AUDCLNT_BUFFERFLAGS_SILENT` 自动生成全零 PCM。
//! - 设备失效：捕获 `AUDCLNT_E_DEVICE_INVALIDATED` 并支持重连重试。

use std::sync::Mutex;
use std::sync::mpsc::channel;
use std::time::Duration;
use windows::Win32::Foundation::*;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{HRESULT, HSTRING, Interface, Result as WinResult, implement};

/// 单个 PCM 采集包（固定 48000 Hz / 16-bit / 2 声道）。
#[derive(Debug, Clone)]
pub struct PcmPacket {
    /// 双声道 16 位小端 PCM 数据，字节数 = frames * 4
    pub data: Vec<u8>,
    /// 采样点数
    pub frames: u32,
    /// 采集时刻 QPC（100ns 单位，0 表示由累计帧数推算）
    pub qpc: u64,
}

/// 音源规格。
#[derive(Debug, Clone)]
pub enum AudioSourceSpec {
    DefaultRender,
    DeviceId(String),
    ProcessTree(u32),
}

/// 进程环回异步激活完成回调句柄。
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivateHandler {
    tx: Mutex<Option<std::sync::mpsc::Sender<WinResult<IAudioClient>>>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateHandler_Impl {
    fn ActivateCompleted(
        &self,
        operation: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> WinResult<()> {
        let res = (|| -> WinResult<IAudioClient> {
            let op = operation
                .as_ref()
                .ok_or_else(|| windows::core::Error::from_hresult(E_POINTER))?;
            let mut hr = HRESULT(0);
            let mut unk: Option<windows::core::IUnknown> = None;
            unsafe {
                op.GetActivateResult(&mut hr, &mut unk)?;
            }
            if hr.is_err() {
                return Err(windows::core::Error::from_hresult(hr));
            }
            let client: IAudioClient = unk
                .ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?
                .cast()?;
            Ok(client)
        })();

        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(res);
        }
        Ok(())
    }
}

/// WASAPI 音频采集客户端。
pub struct AudioCapture {
    spec: AudioSourceSpec,
    client: IAudioClient,
    capture: IAudioCaptureClient,
}

unsafe impl Send for AudioCapture {}

impl AudioCapture {
    /// 创建并启动 WASAPI 采集客户端。
    pub fn new(spec: AudioSourceSpec) -> WinResult<Self> {
        let (client, capture) = Self::init_client(&spec)?;
        Ok(Self {
            spec,
            client,
            capture,
        })
    }

    /// 轮询读取当前可用的 PCM 数据包。
    /// 若当前无可用音频数据返回 `Ok(None)`。
    pub fn poll(&mut self) -> WinResult<Option<PcmPacket>> {
        let avail = unsafe { self.capture.GetNextPacketSize()? };
        if avail == 0 {
            return Ok(None);
        }

        let mut p = std::ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        let mut qpc = 0u64;
        unsafe {
            self.capture
                .GetBuffer(&mut p, &mut frames, &mut flags, None, Some(&mut qpc))?;
        }

        let silent = (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0;
        let bytes = frames as usize * 4;
        let data = if silent || p.is_null() {
            vec![0u8; bytes]
        } else {
            unsafe { std::slice::from_raw_parts(p, bytes) }.to_vec()
        };

        // ★ 每个分支都必须调 ReleaseBuffer，否则音频管道会永久停滞
        unsafe {
            self.capture.ReleaseBuffer(frames)?;
        }

        Ok(Some(PcmPacket { data, frames, qpc }))
    }

    /// 设备失效时重置采集端（如切换耳机、默认输出变更）。
    pub fn reset(&mut self) -> WinResult<()> {
        let _ = unsafe { self.client.Stop() };
        let (client, capture) = Self::init_client(&self.spec)?;
        self.client = client;
        self.capture = capture;
        Ok(())
    }

    fn init_client(spec: &AudioSourceSpec) -> WinResult<(IAudioClient, IAudioCaptureClient)> {
        let client = match spec {
            AudioSourceSpec::DefaultRender => {
                let enumerator: IMMDeviceEnumerator =
                    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
                let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
                let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
                client
            }
            AudioSourceSpec::DeviceId(id) => {
                let enumerator: IMMDeviceEnumerator =
                    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
                let device = unsafe { enumerator.GetDevice(&HSTRING::from(id.as_str()))? };
                let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
                client
            }
            AudioSourceSpec::ProcessTree(pid) => {
                let (tx, rx) = channel();
                let handler: IActivateAudioInterfaceCompletionHandler = ActivateHandler {
                    tx: Mutex::new(Some(tx)),
                }
                .into();

                let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                    ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                    Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                        ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                            TargetProcessId: *pid,
                            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                        },
                    },
                };

                let mut prop = PROPVARIANT::default();
                unsafe {
                    (*prop.Anonymous.Anonymous).vt = windows::Win32::System::Variant::VT_BLOB;
                    (*prop.Anonymous.Anonymous).Anonymous.blob =
                        windows::Win32::System::Com::BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        };
                }

                let _op = unsafe {
                    ActivateAudioInterfaceAsync(
                        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                        &IAudioClient::IID,
                        Some(&prop),
                        &handler,
                    )?
                };

                rx.recv_timeout(Duration::from_secs(2))
                    .map_err(|_| windows::core::Error::new(E_FAIL, "进程环回音频接口激活超时"))??
            }
        };

        // 统一格式：48000 Hz / 16-bit / 双声道 (PCM)
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        unsafe {
            // AUTOCONVERTPCM + SRC_DEFAULT_QUALITY 由系统混音器保证格式统一重采样为 48kHz
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                200 * 10_000, // 200 ms 缓冲区
                0,            // loopback 模式 periodicity 必须为 0
                &fmt,
                None,
            )?;

            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok((client, capture))
        }
    }
}
