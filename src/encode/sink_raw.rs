//! 自定义 `IMFMediaSink`：**编码器交给 Media Foundation，容器留给自己**。
//!
//! ## 为什么需要它
//! Media Foundation 自带的分片 MP4 sink 只支持 H.264——把输出类型换成 HEVC
//! 后再要求 `MFTranscodeContainerType_FMPEG4`，`BeginWriting()` 直接返回
//! `MF_E_INVALIDMEDIATYPE`（与分辨率、profile、像素格式、码率、是否挂 DXGI
//! Device Manager 全部无关）。也就是说，只要写盒这层还用 MF 自带的 sink，
//! HEVC 就只能落普通 MP4：不抗崩溃、录制中不能切片、`mdat` 与 chunk 偏移
//! 还是 32 位（4 GiB 就到头）。
//!
//! （AV1 目前仍然这样：它的样本是 OBU 而不是 Annex B，配置记录 `av1C` 的
//! 来源与写法都不同，`mux::nal` 这套转换还没覆盖，所以 `mf_sink` 只把
//! H.264 / HEVC 交给本模块。）
//!
//! Hybrid 本身跟编码器无关，堵住的是「谁来写盒」。所以这一层把写盒拿回来：
//!
//! ```text
//! 采集 → D3D11 纹理 ──▶ IMFSinkWriter ──▶ [硬件 MFT: 挑最合适的编码器]
//!                                          │
//!                         我们的 IMFMediaSink ◀┘  （收到的是编码后的码流样本）
//!                                          │
//!                                          ▼
//!                                   Mp4Muxer（自己写 fMP4）
//! ```
//!
//! 好处是我们不用碰最难的部分（异步 MFT 事件、D3D11 零拷贝输入、编码器协商），
//! 这些继续由 `IMFSinkWriter` 负责——它只是把「写文件」换成了「交给我们的 sink」。
//!
//! ## SinkWriter 对 sink 的隐含要求（实测出来的，靠读文档想不到）
//! 1. **一开始必须是 0 条流**。`GetStreamSinkCount` 报 1 会让 SinkWriter 认为
//!    已经存在一条流，随后它又会 `AddStreamSink(2)` 再加一条，最后在
//!    `BeginWriting` 里按旧编号回查 → `MF_E_INVALIDSTREAMNUMBER`。
//! 2. **`AddStreamSink(identifier)` 必须真的新建一条流**，且它的 `GetIdentifier()`
//!    要返回这个 identifier。`MFCreateSinkWriterFromMediaSink` 建完立刻
//!    `GetStreamSinkById(identifier)` 回查，查不到就失败。
//! 3. **每条流必须是各自独立的 COM 对象**。若两条 id 复用同一个对象，
//!    它会在同一个事件队列上排两次 `BeginGetEvent` →
//!    `MF_E_MULTIPLE_BEGIN`（0xC00D36DA）。
//! 4. **类型处理器必须报出非空的媒体类型列表**（`GetMediaTypeCount`）。
//!    返回 0 会让 `MFCreateSinkWriterFromMediaSink` 直接死在
//!    `MF_E_NO_MORE_TYPES`（0xC00D36B9）——连 H.264 都轮不到谈。
//!
//! ## windows-rs 0.62 的实现约定（踩过坑，写下来）
//! * `#[implement(IFoo)] struct Bar` 会**额外生成**一个 `Bar_Impl` 包装类型，
//!   真正的 trait 要写在它身上：`impl IFoo_Impl for Bar_Impl`。`Bar_Impl` 通过
//!   `Deref` 暴露 `Bar` 的字段，所以 `self.state` 照样能用。
//! * 参数用 `windows_core::Ref<T>` 而不是 `&T`；取引用用 `.as_ref()`。
//! * 宏展开后用的是 `::windows_core::` **绝对路径**，所以 Cargo.toml 里必须把
//!   `windows-core` 也列为直接依赖。
//! * `IMFStreamSink` / `IMFMediaEventGenerator` 的 trait 被
//!   `all(Com_StructuredStorage, System_Variant)` 门控（签名里有 `PROPVARIANT`），
//!   少了这两个 feature 连 trait 都看不见。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HRESULT, Result as WinResult, implement};

/// 协议追踪：`MUREC_TRACE_SINK=1` 时才打印。
///
/// 保留它是为了**排障**——这套 `IMFMediaSink` 与 SinkWriter 的握手顺序全是实测
/// 出来的（见模块文档那 4 条），换驱动/换 Windows 版本出问题时，打开这个开关就能
/// 看到它停在哪一步。默认关闭，避免每个样本刷一行。
macro_rules! strace {
    ($($arg:tt)*) => {
        if trace_on() {
            eprintln!($($arg)*);
        }
    };
}

fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MUREC_TRACE_SINK").is_some())
}

/// 收到的一个编码样本。
/// 流类型：视频或音频。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
}

/// 收到的一个编码样本。
#[derive(Debug, Clone)]
pub struct EncodedSample {
    /// 编码后的码流字节（原样，视频未做 Annex B → 长度前缀转换，音频为裸 AAC 帧）
    pub data: Vec<u8>,
    /// 显示时间（微秒，相对录制起点）
    pub pts_us: u64,
    /// 是否关键帧（`MFSampleExtension_CleanPoint`）
    pub is_key: bool,
}

/// 单条流的状态。
#[derive(Debug, Clone)]
pub struct StreamState {
    pub identifier: u32,
    pub kind: StreamKind,
    pub subtype: GUID,
    pub sequence_header: Vec<u8>,
    pub frame_size: Option<(u32, u32)>,
    pub samples: Vec<EncodedSample>,
}

/// 全部 COM 对象共享的状态（只存纯数据，避免跨线程持有 COM 指针）。
#[derive(Default)]
struct SinkState {
    streams: HashMap<u32, StreamState>,
    total_samples: u64,
    header_revisions: u32,
    shut_down: bool,
    error: Option<String>,
}

impl SinkState {
    fn lock(state: &Arc<Mutex<Self>>) -> std::sync::MutexGuard<'_, Self> {
        state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// 供上层使用的 sink 句柄：持有 COM 对象，并提供取样本的窗口。
pub struct RawMediaSink {
    sink: IMFMediaSink,
    state: Arc<Mutex<SinkState>>,
}

impl RawMediaSink {
    /// 创建 sink。`video_types` 是我们愿意接受的编码输出格式（H.264 / HEVC / AV1）。
    pub fn new(video_types: &[&GUID]) -> WinResult<Self> {
        let state = Arc::new(Mutex::new(SinkState::default()));
        // 父子互相引用会造成循环；这里用一个「槽」先占位，等父对象建好再填。
        #[allow(clippy::arc_with_non_send_sync)]
        let parent_slot: Arc<Mutex<Option<IMFMediaSink>>> = Arc::new(Mutex::new(None));

        let media_sink: IMFMediaSink = RawMediaSinkImpl {
            state: state.clone(),
            accepted: video_types.iter().map(|g| **g).collect(),
            parent: parent_slot.clone(),
            #[allow(clippy::arc_with_non_send_sync)]
            streams: Arc::new(Mutex::new(Vec::new())),
        }
        .into();

        // 填上反向引用（流的 `GetMediaSink` 需要）。
        *parent_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(media_sink.clone());

        Ok(Self {
            sink: media_sink,
            state,
        })
    }

    /// 交给 `MFCreateSinkWriterFromMediaSink` 的接口。
    pub fn sink(&self) -> &IMFMediaSink {
        &self.sink
    }

    /// 取走所有流中攒下的样本（写盘后调用，避免无限增长）。
    pub fn take_streams(&self) -> HashMap<u32, StreamState> {
        let mut s = SinkState::lock(&self.state);
        let mut result = HashMap::new();
        for (&id, stream) in s.streams.iter_mut() {
            let mut st = stream.clone();
            st.samples = std::mem::take(&mut stream.samples);
            result.insert(id, st);
        }
        result
    }

    /// 取走攒下的样本（单视频流兼容接口）。
    pub fn take_samples(&self) -> Vec<EncodedSample> {
        let mut s = SinkState::lock(&self.state);
        if let Some(stream) = s
            .streams
            .values_mut()
            .find(|st| st.kind == StreamKind::Video)
        {
            std::mem::take(&mut stream.samples)
        } else if let Some(first) = s.streams.values_mut().next() {
            std::mem::take(&mut first.samples)
        } else {
            Vec::new()
        }
    }

    /// 当前攒着但还没取走的样本数。
    pub fn pending_samples(&self) -> usize {
        let s = SinkState::lock(&self.state);
        s.streams.values().map(|st| st.samples.len()).sum()
    }

    /// `stsd` 需要的参数集（`avcC` / `hvcC` / `av1C` 盒体）。可能一开始为空，
    /// 编码器产出第一个关键帧后才有。
    pub fn sequence_header(&self) -> Vec<u8> {
        let s = SinkState::lock(&self.state);
        s.streams
            .values()
            .find(|st| st.kind == StreamKind::Video)
            .map(|st| st.sequence_header.clone())
            .unwrap_or_default()
    }

    /// 参数集被改写过几次（>1 说明编码器中途重发了参数集）。
    pub fn header_revisions(&self) -> u32 {
        SinkState::lock(&self.state).header_revisions
    }

    /// 协商出来的视频编码格式。
    pub fn subtype(&self) -> Option<GUID> {
        let s = SinkState::lock(&self.state);
        s.streams
            .values()
            .find(|st| st.kind == StreamKind::Video)
            .map(|st| st.subtype)
    }

    /// 编码后视频样本的宽高。
    pub fn frame_size(&self) -> Option<(u32, u32)> {
        let s = SinkState::lock(&self.state);
        s.streams
            .values()
            .find(|st| st.kind == StreamKind::Video)
            .and_then(|st| st.frame_size)
    }

    pub fn total_samples(&self) -> u64 {
        SinkState::lock(&self.state).total_samples
    }

    pub fn take_error(&self) -> Option<String> {
        SinkState::lock(&self.state).error.take()
    }
}

// ------------------------------------------------------------ 类型处理器

// ------------------------------------------------------------ 类型处理器

/// 告诉 SinkWriter「这些编码格式我能写」。每条流一个。
#[implement(IMFMediaTypeHandler)]
struct RawTypeHandler {
    state: Arc<Mutex<SinkState>>,
    identifier: u32,
    kind: StreamKind,
    /// 接受的编码子类型（视频为 H.264 / HEVC / AV1，音频为 AAC）
    accepted: Vec<GUID>,
    /// `SetCurrentMediaType` 设进来的输出类型，`GetCurrentMediaType` 要原样还回去
    current: Mutex<Option<IMFMediaType>>,
}

impl IMFMediaTypeHandler_Impl for RawTypeHandler_Impl {
    fn IsMediaTypeSupported(
        &self,
        pmediatype: windows::core::Ref<IMFMediaType>,
        ppmediatype: windows::core::OutRef<IMFMediaType>,
    ) -> WinResult<()> {
        let _ = ppmediatype.write(None);
        let Some(mt) = pmediatype.as_ref() else {
            return Err(windows::core::Error::from_hresult(E_POINTER));
        };
        let major = unsafe { mt.GetGUID(&MF_MT_MAJOR_TYPE) }?;
        let expected_major = match self.kind {
            StreamKind::Video => MFMediaType_Video,
            StreamKind::Audio => MFMediaType_Audio,
        };
        if major != expected_major {
            return Err(windows::core::Error::from_hresult(MF_E_INVALIDMEDIATYPE));
        }
        let sub = unsafe { mt.GetGUID(&MF_MT_SUBTYPE) }?;
        if !self.accepted.contains(&sub) {
            return Err(windows::core::Error::from_hresult(MF_E_INVALIDMEDIATYPE));
        }
        if self.kind == StreamKind::Audio {
            // 校验 MF_MT_AAC_PAYLOAD_TYPE == 0（Raw AAC）
            if let Ok(payload_type) = unsafe { mt.GetUINT32(&MF_MT_AAC_PAYLOAD_TYPE) } {
                if payload_type != 0 {
                    return Err(windows::core::Error::from_hresult(MF_E_INVALIDMEDIATYPE));
                }
            }
        }
        Ok(())
    }

    fn GetMediaTypeCount(&self) -> WinResult<u32> {
        strace!("handler.GetMediaTypeCount -> {}", self.accepted.len());
        Ok(self.accepted.len() as u32)
    }

    fn GetMediaTypeByIndex(&self, index: u32) -> WinResult<IMFMediaType> {
        strace!("handler.GetMediaTypeByIndex({index})");
        let subtype = self
            .accepted
            .get(index as usize)
            .ok_or_else(|| windows::core::Error::from_hresult(MF_E_NO_MORE_TYPES))?;
        let mt: IMFMediaType = unsafe { MFCreateMediaType()? };
        match self.kind {
            StreamKind::Video => unsafe {
                mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                mt.SetGUID(&MF_MT_SUBTYPE, subtype)?;
            },
            StreamKind::Audio => unsafe {
                mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                mt.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                mt.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
                mt.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000)?;
                mt.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                mt.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
                mt.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 40000)?;
                mt.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
                mt.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            },
        }
        Ok(mt)
    }

    fn SetCurrentMediaType(&self, pmediatype: windows::core::Ref<IMFMediaType>) -> WinResult<()> {
        let hdr = pmediatype
            .as_ref()
            .and_then(|m| unsafe { m.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.ok())
            .unwrap_or(0);
        strace!(
            "handler.SetCurrentMediaType (stream={}, seq_header={hdr})",
            self.identifier
        );
        let Some(mt) = pmediatype.as_ref() else {
            return Err(windows::core::Error::from_hresult(E_POINTER));
        };
        {
            let mut s = SinkState::lock(&self.state);
            let stream = s
                .streams
                .entry(self.identifier)
                .or_insert_with(|| StreamState {
                    identifier: self.identifier,
                    kind: self.kind,
                    subtype: GUID::zeroed(),
                    sequence_header: Vec::new(),
                    frame_size: None,
                    samples: Vec::new(),
                });

            if let Ok(sub) = unsafe { mt.GetGUID(&MF_MT_SUBTYPE) } {
                stream.subtype = sub;
            }
            if self.kind == StreamKind::Video {
                if let Ok(size) = unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) } {
                    stream.frame_size = Some(((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32));
                }
                if let Ok(len) = unsafe { mt.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }
                    && len > 0
                {
                    let mut buf = vec![0u8; len as usize];
                    let mut got = 0u32;
                    if unsafe { mt.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, Some(&mut got)) }
                        .is_ok()
                    {
                        buf.truncate(got as usize);
                        if stream.sequence_header != buf {
                            stream.sequence_header = buf;
                            s.header_revisions += 1;
                        }
                    }
                }
            } else if self.kind == StreamKind::Audio {
                if let Ok(payload) = unsafe { mt.GetUINT32(&MF_MT_AAC_PAYLOAD_TYPE) } {
                    if payload != 0 {
                        return Err(windows::core::Error::new(
                            windows::Win32::Foundation::E_INVALIDARG,
                            format!("音频流必须是 Raw AAC (payload type 0)，但收到了 {payload}"),
                        ));
                    }
                }
            }
        }
        // `GetCurrentMediaType` 要还回去，存一份克隆（Ref 只在调用期有效）。
        *self.current.lock().unwrap_or_else(|p| p.into_inner()) = pmediatype.cloned();
        Ok(())
    }

    fn GetCurrentMediaType(&self) -> WinResult<IMFMediaType> {
        self.current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| windows::core::Error::from_hresult(MF_E_NOT_INITIALIZED))
    }

    fn GetMajorType(&self) -> WinResult<GUID> {
        match self.kind {
            StreamKind::Video => Ok(MFMediaType_Video),
            StreamKind::Audio => Ok(MFMediaType_Audio),
        }
    }
}

// ------------------------------------------------------------ 流 sink

/// 一条视频流：接样本 + 事件队列。
#[implement(IMFStreamSink, IMFMediaEventGenerator)]
struct RawStreamSink {
    state: Arc<Mutex<SinkState>>,
    queue: IMFMediaEventQueue,
    handler: IMFMediaTypeHandler,
    /// 父 sink（`GetMediaSink` 用）。
    parent: Arc<Mutex<Option<IMFMediaSink>>>,
    /// 本流编号，由 `AddStreamSink` 指定，之后**不再变**。
    identifier: u32,
}

impl IMFMediaEventGenerator_Impl for RawStreamSink_Impl {
    fn GetEvent(&self, dwflags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> WinResult<IMFMediaEvent> {
        strace!("stream({}).GetEvent({})", self.identifier, dwflags.0);
        unsafe { self.queue.GetEvent(dwflags.0) }
    }

    fn BeginGetEvent(
        &self,
        pcallback: windows::core::Ref<IMFAsyncCallback>,
        punkstate: windows::core::Ref<windows::core::IUnknown>,
    ) -> WinResult<()> {
        strace!("stream({}).BeginGetEvent", self.identifier);
        unsafe {
            self.queue
                .BeginGetEvent(pcallback.as_ref(), punkstate.as_ref())
        }
    }

    fn EndGetEvent(&self, presult: windows::core::Ref<IMFAsyncResult>) -> WinResult<IMFMediaEvent> {
        unsafe { self.queue.EndGetEvent(presult.as_ref()) }
    }

    fn QueueEvent(
        &self,
        met: u32,
        guidextendedtype: *const GUID,
        hrstatus: HRESULT,
        pvvalue: *const PROPVARIANT,
    ) -> WinResult<()> {
        unsafe {
            self.queue
                .QueueEventParamVar(met, guidextendedtype, hrstatus, pvvalue)
        }
    }
}

impl IMFStreamSink_Impl for RawStreamSink_Impl {
    fn GetMediaSink(&self) -> WinResult<IMFMediaSink> {
        let guard = self.parent.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .clone()
            .ok_or_else(|| windows::core::Error::from_hresult(MF_E_NOT_INITIALIZED))
    }

    fn GetIdentifier(&self) -> WinResult<u32> {
        strace!("stream({}).GetIdentifier", self.identifier);
        Ok(self.identifier)
    }

    fn GetMediaTypeHandler(&self) -> WinResult<IMFMediaTypeHandler> {
        Ok(self.handler.clone())
    }

    fn ProcessSample(&self, psample: windows::core::Ref<IMFSample>) -> WinResult<()> {
        strace!("stream({}).ProcessSample", self.identifier);
        let Some(sample) = psample.as_ref() else {
            return Ok(());
        };
        let packed = match pack_sample(sample) {
            Ok(v) => v,
            Err(e) => {
                SinkState::lock(&self.state).error = Some(format!("读取样本失败: {e}"));
                return Err(e);
            }
        };
        {
            let mut s = SinkState::lock(&self.state);
            s.total_samples += 1;
            if let Some(stream) = s.streams.get_mut(&self.identifier) {
                stream.samples.push(packed);
            }
        }
        // 拉模式：交出一帧就再要一帧。
        queue_event(self.queue.clone(), MEStreamSinkRequestSample.0 as u32);
        Ok(())
    }

    fn PlaceMarker(
        &self,
        marker: MFSTREAMSINK_MARKER_TYPE,
        value: *const PROPVARIANT,
        _context: *const PROPVARIANT,
    ) -> WinResult<()> {
        // ★ `IMFSinkWriter::Finalize()` 内部就是对每条流调 `PlaceMarker(ENDOFSEGMENT)`，
        // 然后**同步等**流 sink 把 `MEStreamSinkMarker` 排回队列。不排这个事件，
        // Finalize 会永久卡死（前面所有样本都能正常走完，就是最后一步不动）。
        strace!(
            "stream({}).PlaceMarker(type={}) context_null={}",
            self.identifier,
            marker.0,
            _context.is_null()
        );
        // ★ 事件的 value 必须是 **pvarContextValue**，不是 pvarMarkerValue。
        // 文档原话：「pvarContextValue … is attached to the MEStreamSinkMarker
        // event. Call IMFMediaEvent::GetValue to get this value.」
        // 传错（或传 NULL）SinkWriter 认不出这是它排的 marker，
        // `Finalize()` 直接返回 MF_E_INVALIDREQUEST（0xC00D36BB）。
        unsafe {
            self.queue.QueueEventParamVar(
                MEStreamSinkMarker.0 as u32,
                &GUID::zeroed(),
                HRESULT(0),
                _context,
            )?;
        }
        let _ = value;
        Ok(())
    }

    fn Flush(&self) -> WinResult<()> {
        // 丢掉还没取走的样本。注意 MF 没有 `MEStreamSinkFlushed` 这个事件类型
        // （流 sink 的 Flush 是同步语义），所以这里不需要排任何事件。
        strace!("stream({}).Flush", self.identifier);
        Ok(())
    }
}

/// 往事件队列里排一个无参数事件（Started / RequestSample 这类）。
fn queue_event(queue: IMFMediaEventQueue, met: u32) {
    unsafe {
        let _ = queue.QueueEventParamVar(met, &GUID::zeroed(), HRESULT(0), std::ptr::null());
    }
}

/// 把一个 `IMFSample` 摊平成 (字节, 时间戳, 关键帧)。
fn pack_sample(sample: &IMFSample) -> WinResult<EncodedSample> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    let mut data = Vec::new();
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        if !ptr.is_null() && len > 0 {
            data.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
        }
        let _ = buffer.Unlock();
    }
    let ts = unsafe { sample.GetSampleTime() }.unwrap_or(0);
    let is_key = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
        .map(|v| v != 0)
        .unwrap_or(false);
    Ok(EncodedSample {
        data,
        pts_us: (ts.max(0) as u64) / 10,
        is_key,
    })
}

// ------------------------------------------------------------ 媒体 sink

/// `IMFMediaSink` 本体。
#[implement(IMFMediaSink)]
struct RawMediaSinkImpl {
    state: Arc<Mutex<SinkState>>,
    /// 接受的编码子类型，用来给新建的流造类型处理器
    accepted: Vec<GUID>,
    parent: Arc<Mutex<Option<IMFMediaSink>>>,
    /// 已创建的流。**初始为空**，由 `AddStreamSink` 逐条创建。
    streams: Arc<Mutex<Vec<IMFStreamSink>>>,
}

impl RawMediaSinkImpl_Impl {
    fn streams(&self) -> std::sync::MutexGuard<'_, Vec<IMFStreamSink>> {
        self.streams.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl IMFMediaSink_Impl for RawMediaSinkImpl_Impl {
    fn GetCharacteristics(&self) -> WinResult<u32> {
        strace!("sink.GetCharacteristics");
        // RATELESS：这是文件型 sink，不参与时钟调度（MF 的 MP4 sink 同样声明）。
        // 这里**不能**声明 FIXED_STREAMS：那样 SinkWriter 就不会调 AddStreamSink，
        // 我们也就永远不会有流。
        Ok(MEDIASINK_RATELESS)
    }

    fn AddStreamSink(
        &self,
        identifier: u32,
        pmediatype: windows::core::Ref<IMFMediaType>,
    ) -> WinResult<IMFStreamSink> {
        strace!("sink.AddStreamSink(id={identifier})");
        let major = pmediatype
            .as_ref()
            .and_then(|m| unsafe { m.GetGUID(&MF_MT_MAJOR_TYPE) }.ok())
            .unwrap_or(MFMediaType_Video);

        let (kind, accepted) = if major == MFMediaType_Audio {
            (StreamKind::Audio, vec![MFAudioFormat_AAC])
        } else {
            (StreamKind::Video, self.accepted.clone())
        };

        let handler: IMFMediaTypeHandler = RawTypeHandler {
            state: self.state.clone(),
            identifier,
            kind,
            accepted,
            current: Mutex::new(None),
        }
        .into();
        // 把 SinkWriter 给的目标编码格式记下来（参数集就藏在这里）。
        let _ = unsafe { handler.SetCurrentMediaType(pmediatype.as_ref()) };

        // ★ 每条流必须是各自独立的事件队列！
        let queue: IMFMediaEventQueue = unsafe { MFCreateEventQueue()? };

        let stream: IMFStreamSink = RawStreamSink {
            state: self.state.clone(),
            queue: queue.clone(),
            handler,
            parent: self.parent.clone(),
            identifier,
        }
        .into();
        // ★ 告诉 SinkWriter「这条流已经开始」。少了这个事件，`WriteSample` 会
        // 照收不误（先缓冲），但编码器永远不被泵动、`ProcessSample` 一次都不会
        // 被调用，最后卡死在 `Finalize()`。
        unsafe {
            queue.QueueEventParamVar(
                MEStreamSinkStarted.0 as u32,
                &GUID::zeroed(),
                HRESULT(0),
                std::ptr::null(),
            )?;
        }
        queue_event(queue.clone(), MEStreamSinkRequestSample.0 as u32);
        self.streams().push(stream.clone());
        Ok(stream)
    }

    fn RemoveStreamSink(&self, identifier: u32) -> WinResult<()> {
        self.streams()
            .retain(|s| unsafe { s.GetIdentifier() } != Ok(identifier));
        Ok(())
    }

    fn GetStreamSinkCount(&self) -> WinResult<u32> {
        let n = self.streams().len() as u32;
        strace!("sink.GetStreamSinkCount -> {n}");
        Ok(n)
    }

    fn GetStreamSinkByIndex(&self, index: u32) -> WinResult<IMFStreamSink> {
        strace!("sink.GetStreamSinkByIndex({index})");
        self.streams()
            .get(index as usize)
            .cloned()
            .ok_or_else(|| windows::core::Error::from_hresult(MF_E_OUT_OF_RANGE))
    }

    fn GetStreamSinkById(&self, identifier: u32) -> WinResult<IMFStreamSink> {
        strace!("sink.GetStreamSinkById({identifier})");
        self.streams()
            .iter()
            .find(|s| unsafe { s.GetIdentifier() } == Ok(identifier))
            .cloned()
            .ok_or_else(|| windows::core::Error::from_hresult(MF_E_INVALIDSTREAMNUMBER))
    }

    fn SetPresentationClock(
        &self,
        _clock: windows::core::Ref<IMFPresentationClock>,
    ) -> WinResult<()> {
        strace!("sink.SetPresentationClock(null={})", _clock.is_null());
        // RATELESS：接受但不使用（时间戳直接来自样本）。
        Ok(())
    }

    fn GetPresentationClock(&self) -> WinResult<IMFPresentationClock> {
        Err(windows::core::Error::from_hresult(MF_E_NO_CLOCK))
    }

    fn Shutdown(&self) -> WinResult<()> {
        strace!("sink.Shutdown");
        SinkState::lock(&self.state).shut_down = true;
        Ok(())
    }
}
