# MuRecorder 音频设计（最终形态）

> 状态：设计定稿，**尚未实现**。当前代码里音频只有 ABI 形状——`recorder.rs:122`
> 直接拒绝 `audio_source != AUDIO_NONE`。本文件是目标状态，不是施工顺序。

本文件描述音频功能**建成之后**系统应该长什么样——不是施工顺序，是目标状态。
所有 API 签名、常量、feature 门控都在本机 windows-rs 0.62.2 上核对过。

---

## 1. 最终行为

`RecorderConfig.audio_source_type` 的四个取值对应两种产物：

| `audio_source_type` | 产物 |
|---|---|
| `AUDIO_NONE` (0) | 一条视频轨。**与今天的产物逐字节一致**，音频代码全部不参与 |
| `AUDIO_SYSTEM` (1) | 两条轨：track 1 视频 + track 2 音频 |
| `AUDIO_DEVICE` (2) | 同上，音源换成 `audio_device_id` 指定的渲染设备 |
| `AUDIO_WINDOW` (3) | 同上，音源是 `audio_process_id` / `audio_window_hwnd` 对应进程树的输出 |

音频轨参数：**AAC-LC、48000 Hz、2 声道、16 bit 输入**，码率由
`audio_bitrate_kbps` 决定（默认 **320 kbps**，可选 96/128/160/192/256/320，见 §9.2）。

四条编码路径的最终形态（音频在三条上都可用）：

| 路径 | 录制中容器 | 音频编码者 | 录制中切片 |
|---|---|---|---|
| 软编 OpenH264 | 自研 `Mp4Muxer` 写 fMP4 | 自驱 AAC MFT | ✅ |
| GPU H.264 | MF 分片 sink 写 fMP4 | SinkWriter 内建 AAC MFT | ✅ |
| GPU HEVC | 自研 `IMFMediaSink` + `Mp4Muxer` 写 fMP4 | SinkWriter 内建 AAC MFT | ✅ |
| GPU AV1 | MF 普通 MP4 | SinkWriter 内建 AAC MFT | ❌（停止后才可切，既有边界） |

**系统不变量**（任何一条不满足即为 bug）：

1. `audio_source != 0` 时，产物一定含 track 2；**不存在「静默产出无音轨文件」的路径**。
   音频初始化失败就是 `recorder_create` / `recorder_start` 失败，带 `REC_ERR_INVALID`
   或 `REC_ERR_IO` 和明确原因。
2. 两条轨的时长差 < 50 ms。
3. 录制中文件是合法 fMP4，崩溃只丢最后一个分片（视频或音频之一）。
4. 停止后是普通 `ftyp/mdat/moov`，两条轨都在 `moov` 里。
5. 任何时刻切片，产物带音轨，且切片内音画起点严格对齐。

**实现阶段补上的三条硬约束**（都是踩过之后才写进来的，详见 §12）：

6. **COM 套间（`ComApartment`）必须比同一线程上的所有 COM 对象活得更久。**
   声明顺序决定析构顺序：守卫声明在最前，才最后析构。违反的后果不是报错，
   而是 `CoUninitialize` 之后再 `Release` 一个 MFT/D3D 对象 → 进程当场
   `STATUS_ACCESS_VIOLATION`，没有任何 Rust panic 可以先捕获。
7. **静音补包的起表不能依赖「第一个真实数据包」。** 桌面完全静音时 WASAPI
   loopback 可以一个包都不给；若 anchor 只在收到真实包时才建立，补包逻辑就
   永远不会触发，音轨是 0 个样本。anchor 必须以录制起点 `t0` 起表，
   首个真实包只用来做一次校正。
8. **多故障级联时，报给宿主的必须是根因。** `last_error`（最新一条）会被下游
   线程覆盖成派生结论（典型：「未捕获到任何帧」），真因（音频设备打不开）
   就此丢失。所以另存一份只写一次的 `fatal_root`。

---

## 2. 架构

```
                     ┌──────────────── capture 线程 ────────────────┐
IDXGI Duplication ───┤ 裁剪 → 光标合成 → video ring[4]（满则丢）  │
                     └──────────────────────────────────────────────┘
                                        │
                     ┌──────────────── audio 线程（新）─────────────┐
WASAPI loopback ─────┤ GetBuffer → PCM → 静音补包 → AAC → audio_q   │
                     └──────────────────────────────────────────────┘
                                        │
                     ┌──────────────── encode 线程 ─────────────────┐
                     │ 软编：OpenH264 + 自驱 AAC MFT                 │
                     │ GPU：IMFSinkWriter（视频纹理 + 音频 PCM 两条流）│
                     └──────────────────────────────────────────────┘
                                        │  MuxMsg::{Video, Audio}
                     ┌──────────────── mux 线程 ─────────────────────┐
                     │ Mp4Muxer：双轨、按轨各自分片                   │
                     └──────────────────────────────────────────────┘
                                        │
                                 fMP4 ──► 停止时 soft remux ──► MP4
```

新增模块：

```
src/capture/wasapi.rs   WASAPI loopback 采集（三种音源统一抽象）
src/encode/aac.rs       AAC MFT 驱动 + ASC 常量（仅软编路径使用）
```

改造模块：`mux/mp4.rs`（多轨）、`mux/extract.rs`（双轨切片）、`mux/remux.rs`（双轨重建）、
`encode/sink_raw.rs`（多流）、`encode/mf_sink.rs`（音频流）、`recorder.rs`（音频线程 + 统一通道）。

---

## 3. 时间基与同步

**唯一时基是 QPC。** `t0` = 第一个视频帧的 QPC（现有实现，不改）。所有时间戳都是相对 `t0`。

### 3.1 视频

```text
video_pts_us = (frame_qpc - t0) / 10
```

### 3.2 音频

WASAPI 每包给一个 `pu64qpcposition`（100 ns 单位）。但**时间轴由累计采样数定义，
不由逐包 QPC 定义**——QPC 有微秒级抖动，逐包用它会在 `stts` 里制造 ±1 tick 毛刺。

```text
# 第一包（建立锚点，可能为负：音频先于第一个视频帧）
anchor_us = (qpc_first_packet - t0) / 10
if anchor_us < 0: 丢弃该包，继续取下一包，直到 anchor_us >= 0

# 之后每一包
pts_us = anchor_us + total_frames * 1_000_000 / 48_000
total_frames += frames_in_packet

# 每 30 秒校正一次（只动 anchor，绝不动 total_frames）
drift_us = (qpc_now - t0)/10 - (anchor_us + total_frames*1_000_000/48_000)
if |drift_us| > 5000: anchor_us += drift_us
```

**为什么只能动 anchor**：改 `total_frames` 等于改已经写出去样本的时间轴，会让前半段录制变错；
改 `anchor` 只是让后续样本平移，`stts` 里表现为一次 delta 微调，播放器无感。

### 3.3 两条轨的 timescale 不同

| 轨 | `mdhd.timescale` | 理由 |
|---|---|---|
| track 1 视频 | `1_000_000` | 沿用现有实现，µs 精度 |
| track 2 音频 | `48_000` | AAC 每帧固定 1024 采样，delta 恒等 → `stts` 可折叠成一条 entry |

`mvhd.timescale` 固定 `1000`（毫秒），`mvhd.duration` 取两轨换算到毫秒后的**最大值**。

---

## 4. 采集层：`src/capture/wasapi.rs`

### 4.1 统一抽象

```rust
pub enum AudioSourceSpec<'a> {
    DefaultRender,                       // AUDIO_SYSTEM
    DeviceId(&'a str),                   // AUDIO_DEVICE
    ProcessTree { pid: u32, hwnd: u64 }, // AUDIO_WINDOW
}

pub struct AudioCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    started_at: u64,        // 首包 QPC，用于建立 anchor
    format: PcmFormat,      // 恒为 48k/16/2
}

impl AudioCapture {
    pub fn open(spec: AudioSourceSpec) -> WinResult<Self>;
    /// 非阻塞取包；无数据时返回 Ok(None)，由调用方补静音
    pub fn poll(&mut self) -> WinResult<Option<PcmPacket>>;
    pub fn reset(&mut self) -> WinResult<()>;  // 设备失效后重建
}

pub struct PcmPacket {
    pub data: Vec<u8>,   // interleaved 16-bit LE
    pub frames: u32,
    pub qpc: u64,        // 100ns
}
```

### 4.2 共享/设备音源的初始化（`DefaultRender` / `DeviceId`）

```rust
let enumerator: IMMDeviceEnumerator =
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

let device = match spec {
    DefaultRender => unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? },
    DeviceId(id)  => unsafe { enumerator.GetDevice(&HSTRING::from(id))? },
    // ProcessTree 走 4.4
};

let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };

let fmt = WAVEFORMATEX {
    wFormatTag: WAVE_FORMAT_PCM as u16,  // 1
    nChannels: 2,
    nSamplesPerSec: 48_000,
    nAvgBytesPerSec: 48_000 * 4,
    nBlockAlign: 4,                      // 2ch × 16bit
    wBitsPerSample: 16,
    cbSize: 0,
};

unsafe {
    client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_LOOPBACK          // 0x0002_0000
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM        // 0x8000_0000
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,  // 0x0800_0000
        200 * 10_000,   // 200 ms，100ns 单位
        0,              // loopback 必须 0
        &fmt,
        None,
    )?;
    let capture: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
}
```

**`AUTOCONVERTPCM` 是这套设计的地基**：不加它，`pformat` 必须与系统混音器格式逐字段一致，
否则 `AUDCLNT_E_UNSUPPORTED_FORMAT`（用户换个 44.1k 耳机就录不了）。加上之后 WASAPI
内部完成格式转换与重采样，本 crate 不实现 SRC。`SRC_DEFAULT_QUALITY` 必须同时给，
它管的是转换质量。

### 4.3 取包规程

```rust
pub fn poll(&mut self) -> WinResult<Option<PcmPacket>> {
    let avail = unsafe { self.capture.GetNextPacketSize()? };
    if avail == 0 { return Ok(None); }

    let (mut p, mut frames, mut flags) = (null_mut(), 0u32, 0u32);
    let mut qpc = 0u64;
    unsafe {
        self.capture.GetBuffer(&mut p, &mut frames, &mut flags, None, Some(&mut qpc))?;
    }
    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
    let bytes = frames as usize * 4;
    let data = if silent { vec![0u8; bytes] }
               else { unsafe { std::slice::from_raw_parts(p, bytes) }.to_vec() };

    unsafe { self.capture.ReleaseBuffer(frames)? };   // ★ 任何分支都必须调
    Ok(Some(PcmPacket { data, frames, qpc }))
}
```

**每个分支都要 `ReleaseBuffer`。** 漏掉会让下一次 `GetBuffer` 一直返回同一包，
现象是「音频卡住不动」。

### 4.4 进程环回（`AUDIO_WINDOW`）

进程环回不能走 `IMMDeviceEnumerator`，必须异步激活：

```rust
let params = AUDIOCLIENT_ACTIVATION_PARAMS {
    ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
        ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
            TargetProcessId: pid,
            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        },
    },
};
// 包成 PROPVARIANT(VT_BLOB)，device interface path 传 VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK
ActivateAudioInterfaceAsync(path, &IAudioClient::IID, Some(&prop), handler)?;
// handler 实现 IActivateAudioInterfaceCompletionHandler，等 ActivateCompleted 回调
```

- `AUDIO_WINDOW` 且 `audio_process_id == 0` 时，先由 `audio_window_hwnd` 查
  `GetWindowThreadProcessId` 得到 pid；两者都为 0 是 `REC_ERR_INVALID`（`config.rs` 已有校验）。
- 需要 Windows 10 2004（build 19041）以上；更早的系统返回 `REC_ERR_INVALID`
  并说明「进程环回需要 Windows 10 2004 及以上」。
- COM 回调对象用 `#[implement]`——`sink_raw.rs` 已经趟过这套（`windows-core` 直接依赖
  已在 Cargo.toml 里）。

### 4.5 静音补包（这是 loopback 最经典的坑）

**loopback 只在「有音频正在渲染」时交付数据。** 用户录静态桌面、后台不播任何东西时，
`GetNextPacketSize` 永远返回 0。如果音频线程按包驱动，音轨就是断的，成品音轨比视频短。

音频线程必须以**墙钟**驱动：

```rust
let mut wall = Instant::now();
let mut produced_frames: u64 = 0;   // 已产出的总帧数（含补的静音）

loop {
    if stop { break; }
    match capture.poll()? {
        Some(pkt) => {
            produced_frames += pkt.frames as u64;
            send(MuxMsg::Audio(pkt));
        }
        None => {
            // 目标帧数 = 已经过的墙钟 × 48000
            let target = (wall.elapsed().as_micros() as u64 * 48_000 / 1_000_000);
            if target > produced_frames + 480 {   // 落后超过 10 ms 才补，避免碎包
                let need = ((target - produced_frames) as u32).min(48_000 / 4); // 单次最多 250ms
                send(MuxMsg::Audio(PcmPacket {
                    data: vec![0u8; need as usize * 4],
                    frames: need,
                    qpc: 0,       // 0 = 由 muxer 按累计帧数推算 pts
                }));
                produced_frames += need as u64;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
```

补出来的静音包 `qpc = 0`，时间戳完全由累计帧数决定（正是 3.2 的公式）。

**注意 `target` 用的是音频线程自己的起点，不是 `t0`。** 两者的差值在 muxer 里用
`anchor_us` 统一，见 3.2。

#### 4.5.1 起表时机：这个坑在实现阶段又踩了一次

上面这段伪码有个致命前提没写出来：`produced_frames` 的比较对象必须是**已经起过表**的
墙钟。而「起表」如果把条件写成「收到第一个真实包时才建 anchor」，整段逻辑就会在
桌面完全静音时彻底失效：

```
真实包永远不来 → anchor 永远是 None → 补静音的分支被锚点判空跳过 → 音轨 0 样本
```

产物是一条**完全正常、只是没有声音**的 MP4：录制成功、停止成功、没有任何报错。

正确的起表点就是录制起点 `t0`：

```rust
// t0 就绪后立刻起表，不等真实数据包
if anchor_us.is_none() {
    anchor_us = Some(0);
    last_calib_qpc = t0_val;
    wall = Instant::now();
}
```

anchor 先置 0（= 音频与视频同起点），等**第一个真实包**到达时再做一次强制性校正：
用它的 QPC 算出设备时钟的相位，与当前 `pts` 比较，偏差超过 2 ms 就整体平移 anchor。
这样既保证了「静音也能起表」，又不会让设备时钟和墙钟之间的固有偏差一路累积。

注意校正只动 `anchor_us`，**永远不动 `produced_frames`**——后者是 `stts` 长度的唯一
来源，动它会让样本表与实际字节数脱节。

### 4.6 设备失效

`GetNextPacketSize` / `GetBuffer` 返回 `AUDCLNT_E_DEVICE_INVALIDATED` 时，
调 `AudioCapture::reset()`：重新枚举默认设备 → 重新 `Activate`/`Initialize`/`Start`。

- 重建期间（典型 100–500 ms）继续按 4.5 补静音，**时间轴不能断**。
- 连续 10 次重建失败（间隔 500 ms，复用 `recorder.rs` 的 `REBUILD_INTERVAL`）→
  置致命错误 `REC_ERR_IO`，整条流水线停止，与视频设备丢失同一套处理。
- 重建成功后**不重置** `total_frames`，只把新设备的首包 QPC 用于一次漂移校正。

「重建期间继续补静音」不能靠 `sleep(500ms)` 蒙混过去——那样等于这半秒一帧音频都不出：

```rust
// 重建等待期间，按 5 ms 一轮持续补静音，直到 reset 完成
while Instant::now() < deadline && !stop {
    pad_silence(&wall, anchor_us, &mut produced_frames, &mut on_pcm)?;
    sleep(5ms);
}
// 重建成功后把墙钟基准挪到「已产出帧数」对应的位置，
// 否则下一轮会把重建期间补过的静音再补一遍。
if reset().is_ok() {
    wall = Instant::now() - Duration::from_micros(produced_frames * 1_000_000 / 48_000);
}
```

不做这一步的后果是**静默的音画不同步**：切一次耳机，从那一刻起音频比画面短一块，
播放器不会报任何错。这条缺陷在实现阶段真实存在过，是 `e2e_audio_timeline_matches_video_when_desktop_is_quiet`
把它逼出来的。

---

## 5. 编码层

### 5.1 统一输出形态

无论哪条路径，音频样本的形态相同：

```rust
pub struct AacPacket {
    pub data: Vec<u8>,     // 一个裸 AAC 帧（无 ADTS 头、无长度前缀）
    pub pts_us: u64,
}
```

**必须是裸 AAC（`MF_MT_AAC_PAYLOAD_TYPE = 0`）**。默认可能是 ADTS(1)，带 7 字节帧头；
塞进 `mdat` 就是每帧多 7 字节垃圾，且播放器行为不一致。

### 5.2 软编路径：`src/encode/aac.rs`

```rust
pub struct AacEncoder {
    transform: IMFTransform,
    in_type: IMFMediaType,
    out_type: IMFMediaType,
    async_mode: bool,          // 由 MF_TRANSFORM_ASYNC 决定
    pending_in: VecDeque<Vec<u8>>,
    out_buf: Option<IMFSample>,
}

impl AacEncoder {
    pub fn new() -> WinResult<Self>;
    pub fn push_pcm(&mut self, pcm: &[u8]) -> WinResult<()>;
    pub fn drain(&mut self) -> WinResult<Vec<AacPacket>>;   // 取走当前可取的输出
    pub fn finish(&mut self) -> WinResult<Vec<AacPacket>>;  // 冲刷尾巴
}
```

MFT 获取：

```rust
MFTEnumEx(
    MFT_CATEGORY_AUDIO_ENCODER,
    MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
    Some(&in_attrs),    // 输入 MFAudioFormat_PCM
    Some(&out_attrs),   // 输出 MFAudioFormat_AAC
    &mut activates, &mut count,
)
```

**必须读 `MF_TRANSFORM_ASYNC`**：为 `TRUE` 时走 `METransformNeedInput` /
`METransformHaveOutput` 事件循环，为 `FALSE` 时直接 `ProcessInput` + 循环 `ProcessOutput`。
两种都要实现（不同 Windows 版本/驱动下不同）。

输出类型属性（常量均确认可用）：

```
MF_MT_MAJOR_TYPE                          = MFMediaType_Audio
MF_MT_SUBTYPE                             = MFAudioFormat_AAC
MF_MT_AUDIO_NUM_CHANNELS                  = 2
MF_MT_AUDIO_SAMPLES_PER_SECOND            = 48000
MF_MT_AUDIO_BITS_PER_SAMPLE               = 16
MF_MT_AUDIO_BLOCK_ALIGNMENT               = 4
MF_MT_AUDIO_AVG_BYTES_PER_SECOND          = 40000        // = kbps * 1000 / 8；320 → 40000，128 → 16000
MF_MT_AAC_PAYLOAD_TYPE                    = 0            // raw
MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION  = 0x29         // AAC-LC
```

### 5.3 GPU 路径：交给 SinkWriter

`mf_sink.rs` 里，`audio_source != NONE` 时给 SinkWriter 加第二条流：

```rust
let audio_out = MFCreateMediaType()?;
audio_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
audio_out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
// 其余属性同 5.2
writer.AddStream(&audio_out)?;   // 返回音频流下标
```

音频输入类型用 `MFAudioFormat_PCM` + 5.2 的格式，`SetInputMediaType(audio_stream, ...)`。
之后 `WriteSample(audio_stream, pcm_sample)`，时间戳用 3.2 算出的 `pts_us * 10`（100 ns）。

音频样本**不经过** D3D11 视频处理器、不碰显存——纯 CPU 侧，视频零拷贝链路不受影响。

### 5.4 AudioSpecificConfig 是常量，不需要猜

`esds` 里的 `DecoderSpecificInfo` 需要 AudioSpecificConfig。因为我们**完全固定了编码参数**，
它可以直接算出来，不需要从 `MF_MT_USER_DATA` 里猜偏移：

```text
audioObjectType           = 2  (AAC-LC)      → 5 bits: 00010
samplingFrequencyIndex    = 3  (48000 Hz)     → 4 bits: 0011
channelConfiguration      = 2  (立体声)        → 4 bits: 0010
GASpecificConfig                              → 3 bits: 000
                                   拼起来 = 0001 0001 1001 0000 = 0x11 0x90
```

**ASC = `[0x11, 0x90]`，两字节常量。**

（参考：44100 Hz 是 index 4 → `0x12 0x10`。本设计固定 48 kHz，所以恒为 `0x11 0x90`。）

---

## 6. 封装层：多轨 MP4

### 6.1 数据结构

```rust
pub struct TrackSpec {
    pub track_id: u32,          // 1 = 视频，2 = 音频
    pub kind: TrackKind,
    pub timescale: u32,         // 视频 1_000_000；音频 48_000
    pub desc: SampleDescription,
}

pub enum TrackKind {
    Video { width: u32, height: u32 },
    Audio { channels: u16, sample_rate: u32 },
}

struct TrackState {
    spec: TrackSpec,
    samples: Vec<SampleRec>,    // SampleRec.tick 是本轨 timescale 的 tick
    chunks: Vec<ChunkRec>,
    pending: Vec<PendingSample>,
}

pub struct Mp4Muxer {
    file: File,
    ftyp_len: u64,
    tracks: Vec<TrackState>,    // 顺序即 track_id 顺序
    seq: u32,                   // mfhd 序号，全局递增
    header_written: bool,       // 视频 desc（SPS/PPS）到齐后写 init moov
    finished: bool,
}
```

`SampleRec.tick`（原 `pts_us`）语义改为「本轨 timescale 下的时间戳」。视频轨写入时
`tick = pts_us`（timescale 1e6）；音频轨 `tick = pts_ticks`（timescale 48000）。
`build_stts_entries` 改成按本轨实际 delta 计算 + run-length 合并，不再需要 `fps` 参数。

### 6.2 接口

```rust
impl Mp4Muxer {
    /// 音轨 desc 先注册（ASC 是常量，构造时即知），视频 desc 等首帧关键帧
    pub fn create_multi(
        path: &Path,
        video: Option<(u32, u32, u32)>,   // (w, h, fps)；None = 纯音频（保留给测试）
        audio: Option<(u16, u32)>,        // (channels, sample_rate)
    ) -> io::Result<Self>;

    pub fn set_video_description(&mut self, desc: SampleDescription) -> io::Result<()>;

    pub fn write_video_sample(&mut self, data: &[u8], pts_us: u64, is_key: bool) -> io::Result<()>;
    pub fn write_audio_sample(&mut self, data: &[u8], pts_ticks: u64) -> io::Result<()>;

    pub fn finish(self) -> io::Result<u64>;
}
```

`Mp4Muxer::create` / `create_with_description` / `write_prefixed_sample` / `write_sample`
保留为薄封装（内部调 `create_multi` + `set_video_description`），保证现有调用点与
78 项单测零改动。

### 6.3 分片策略

- **视频片**：遇到关键帧即闭合上一片（现有逻辑不变）。
- **音频片**：累积 ≥ 500 ms 即闭合。崩溃最多丢 0.5 s 音频。
- **每个 `moof` 只含一个 `traf`（一条轨）**。MP4 允许，实现最简，两条轨的片在文件里
  交错出现，顺序无所谓——各自 `co64` 管各自的位置。
- `mfhd.sequence_number` 全局递增，跨轨连续。

### 6.4 box 级规格

`moov` 结构（停止后）：

```
moov
├ mvhd               timescale=1000, duration=max(视频ms, 音频ms)
├ trak (id=1, 视频)
│ └ mdia ─ minf ─ vmhd … ─ stbl
└ trak (id=2, 音频)
  └ mdia ─ minf ─ smhd … ─ stbl
```

每轨字段对照：

| 字段 | track 1（视频） | track 2（音频） |
|---|---|---|
| `tkhd.track_ID` | 1 | 2 |
| `tkhd.flags` | 3 | 3 |
| `tkhd.volume` | `0x0000` | `0x0100` |
| `tkhd.width/height` | 编码宽高（16.16） | 0 / 0 |
| `mdhd.timescale` | 1_000_000 | 48_000 |
| `mdhd.language` | `0x55C4`（und） | `0x55C4` |
| `hdlr.handler_type` | `vide` | `soun` |
| `minf` 头盒 | `vmhd` | `smhd`（balance=0, reserved=0） |
| `stsd` 条目 | `avc1` / `hvc1` / `av01` + 配置盒 | `mp4a` + `esds` |
| `stss` | 有 | **不写**（音频样本全是同步点） |
| `stts` | 按真实 delta 合并 | 一条 entry：`(sample_count, 1024)` |
| `stsz` | 每样本 size | 每样本 size（AAC 变长） |

录制中的 init `moov` 额外带 `mvex`：

```
mvex
├ mehd (可选，不写)
├ trex (track_ID=1, dur/size/flags=0)
└ trex (track_ID=2, dur=1024, size=0, flags=0)
```

音频 `traf` 的 `tfhd` 用 `default-base-is-moof`，`trun` 逐样本写 `duration=1024`
（不依赖 `trex` 默认值，读回更稳）。

### 6.5 `mp4a` 与 `esds` 字节级

`mp4a` 条目（含盒头共 78 字节：36 字节固定体 + 42 字节 `esds`）：

```
[8]  box header  'mp4a', size
[6]  reserved = 0
[2]  data_reference_index = 1
[8]  reserved（version=0, revision=0, vendor=0, 后 2 字节 0）
[2]  channelcount   = 2
[2]  samplesize     = 16
[2]  pre_defined    = 0
[2]  reserved       = 0
[4]  samplerate     = 48000 << 16
[..] esds box
```

`esds`（共 42 字节）：

```
[8]  box header 'esds', size=42
[4]  version/flags = 0
—— ES_Descriptor ——            tag 0x03, len 0x80 0x80 0x80 0x19
[2]  ES_ID = 0
[1]  flags = 0
  —— DecoderConfigDescriptor —— tag 0x04, len 0x11
  [1]  objectTypeIndication = 0x40            (MPEG-4 Audio)
  [1]  streamType/upStream/reserved = 0x15    (AudioStream, reserved=1)
  [3]  bufferSizeDB = 0x00 0x18 0x00
  [4]  maxBitrate = 128000
  [4]  avgBitrate = 128000
    —— DecoderSpecificInfo ——   tag 0x05, len 0x02
    [2]  ASC = 0x11 0x90
  —— SLConfigDescriptor ——      tag 0x06, len 0x01, 0x02
```

---

## 7. 收尾与切片

### 7.1 soft remux（停止时）

**完全不动。** 它是轨无关的：末尾追加完整 `moov`，再把 `ftyp` 之后那 16 字节改写成
覆盖到 `moov` 之前的 64 位 `mdat` 头。两条轨只是让 `moov` 大了一点。

### 7.2 `remux::finalize_fragmented`（GPU H.264 路径）

输入是 MF 写的双轨 fMP4。改动：

1. 从 init `moov` 里按 `trak` 收集每条轨的 `tkhd.track_ID` / `mdhd.timescale` / `stsd`。
2. 扫所有 `moof`：每个 `traf` 的 `tfhd.track_ID` 决定归属。
3. 每条轨各自重建 `stts` / `stsc` / `stsz` / `co64`（视频轨可能有 `stss`，音频轨没有）。
4. 丢掉 `mvex`，写两条 `trak`。
5. 16 字节 soft remux 照旧。

`co64` 里仍是样本的原始绝对偏移——不复制任何媒体字节。

### 7.3 `extract::extract_range`（录制中切片）

双轨照抄 + 按轨裁切：

1. 读 init `moov`，收集**每条** `trak` 的 `track_id` / `timescale` / `stsd`。
2. 扫描 `moof`，每个 `traf` 按 `tfhd.track_ID` 归轨，得到每轨的样本表
   （`pts_ticks`、`size`、`file_offset`）。
3. **确定对齐后的真实起点**：
   ```text
   video_start_tick = 覆盖 start_us 的最近关键帧样本的 tick（timescale 1e6）
   aligned_start_us = video_start_tick
   ```
4. 每轨各自筛选：
   - 视频：`tick >= aligned_start_us`
   - 音频：`tick_us = tick * 1_000_000 / 48000`，条件 `tick_us >= aligned_start_us`
     且 `tick_us < end_us`
5. 把选中的样本字节按轨**各自连续复制**到新 `mdat`，记录新绝对偏移。
6. 写两条 `trak` 的完整 `moov`，再做同一套 16 字节 soft remux。

**对齐必须用视频关键帧的真实 tick，不能用入参 `start_us`。** 否则切片一播音画就差最多一个
GOP（软编 2 秒）。这是这个函数唯一的正确性红线。

音频轨的 `stts` 仍折叠成一条 entry（`(n, 1024)`）。切片产物是**普通 MP4**（没有分片，
所以也没有 `tfdt`）：每轨的第一个被选中的样本就是该轨的时间零点，`stts` 的 delta 从第一帧
开始累加，`co64` 指向新 `mdat` 里的新偏移。不需要 `elst`——两轨都从 0 开始，天然对齐。

---

## 8. 自研 sink 多流：`encode/sink_raw.rs`

`RawMediaSink` 从「一条流」变成「按需增长的多条流」。

```rust
struct SinkState {
    streams: HashMap<u32, StreamState>,   // key = identifier（SinkWriter 分配的）
    total_samples: u64,
    shut_down: bool,
    error: Option<String>,
}

struct StreamState {
    identifier: u32,
    major: TrackKind,            // 由 AddStreamSink 收到的 media type 决定
    subtype: GUID,               // MFVideoFormat_HEVC / MFAudioFormat_AAC
    sequence_header: Vec<u8>,    // 视频的 VPS/SPS/PPS（Annex B）；音频不用
    frame_size: Option<(u32, u32)>,
    samples: Vec<EncodedSample>,
}
```

`AddStreamSink(identifier, media_type)` 的分派规则：

```text
读 media_type 的 MF_MT_MAJOR_TYPE
├ MFMediaType_Video → 视频流：track_id 1
│    记 subtype（H264/HEVC）、读 MF_MT_MPEG_SEQUENCE_HEADER、读 frame_size
│    排 MEStreamSinkStarted + MEStreamSinkRequestSample
└ MFMediaType_Audio → 音频流：track_id 2
     校验 MF_MT_AAC_PAYLOAD_TYPE == 0（不是 0 直接返回错误，不静默兼容）
     记 subtype = MFAudioFormat_AAC
     排 MEStreamSinkStarted + MEStreamSinkRequestSample
```

`ProcessSample` 按 `self.identifier` 找到对应 `StreamState` 入队，**然后立刻再排一次
`MEStreamSinkRequestSample`（该流自己的队列）**——拉模式必须是每流独立的。

音频样本**不做 Annex B 转换**：它已经是裸 AAC 帧，直接交给 `write_audio_sample`。
视频样本走 `mux::nal::annexb_to_mp4_sample`（现有逻辑）。

`mf_sink.rs` 的 `drain_raw` 改成按流分派：

```rust
for (id, st) in sink.take_streams() {
    match st.major {
        Video => for s in st.samples { muxer.write_video_sample(&conv(s), s.pts_us, s.is_key)? },
        Audio => for s in st.samples { muxer.write_audio_sample(&s.data, s.pts_us * 48 / 1000)? },
    }
}
```

（音频 tick 由 µs 换算：`tick = pts_us * 48000 / 1_000_000`，用 `u128` 中间量避免溢出。）

**多条流的 7 条握手契约仍然全部适用，只是作用域变成「每条流各自一份」**：
`meStreamSinkStarted`、`MEStreamSinkRequestSample`、`PlaceMarker` 的
`pvarContextValue` 都要按流的 identifier 区分。

---

## 9. 配置与 ABI

**本节写作时 ABI 升到 v2**（`RECORDER_ABI_VERSION = 2`）。相比 v1 只在尾部追加了
`audio_bitrate_kbps` 一个字段，其余音频字段（`audio_source_type`、
`audio_device_id`、`audio_window_hwnd`、`audio_process_id`）与四值枚举在 v1 就存在。
（当前 ABI 已是 v3：v2 之上又在尾部追加了光标特效字段 `cursor_click_effect` /
`cursor_trail_ms`，变更记录见 `include/recorder.h`；音频字段自 v2 起未再变动。）

```c
uint32_t audio_source_type;  /* AudioSourceType */
const char* audio_device_id; /* AUDIO_DEVICE 时必填 */
uint64_t audio_window_hwnd;  /* AUDIO_WINDOW 时用于解析 pid */
uint32_t audio_process_id;   /* AUDIO_WINDOW 的首选 pid */
uint32_t audio_bitrate_kbps; /* v2 新增：0=默认 320；96/128/160/192/256/320 */
```

### 9.1 为什么这条「尾部追加」必须升版本

设计初稿的判断是「尾部追加天然兼容、不必升」。这个判断是错的，纠正如下：

`abi_version` 的契约是**已编译的旧宿主能继续安全调用新 dll**，不是「宿主和库一起重编译
时结构体对得上」。旧宿主带着 v1 的 `RecorderConfig`（比 v2 短 4 字节）调进来，
`from_ffi` 读到 `audio_process_id` 之后的那 4 字节时已经越过调用方实际分配的边界：

- 最好情况：读到垃圾值 → 落在非法档位 → `REC_ERR_INVALID`，但错误信息是
  「不支持的 audio_bitrate_kbps: 3735928559」，宿主完全看不懂，排查成本高；
- 最坏情况：垃圾值凑巧是 96/128/160/192/256/320 之一 → 静默按错误码率编码；
- 无论哪种，**越界读本身都是 UB**，只是恰好没崩。

所以结论改成：**任何尾部追加都要升版本号**。让旧宿主拿到一句明确的
「abi_version 不匹配：期望 2，实际 1」，宁可拒绝，不要 UB。升级成本只是 C# 侧
`RecorderAbiVersion` 常量改一个数字（见 `docs/csharp-guide.md`）。

这条规则已固化进 `include/recorder.h` 的注释和 `config.rs` 的单测
（`abi_v1_is_rejected_with_actionable_message`，断言错误信息里同时出现期望值和实际值）。

### 9.2 音频编码参数

`audio_bitrate_kbps` 只接受 `0 | 96 | 128 | 160 | 192 | 256 | 320`，0 视为 320。
采样率与声道数**写成常量，不开放配置**：48000 Hz / 2 声道 / AAC-LC。

这两个值不开放的真正理由不是「避免 ABI 变更」（那条已经站不住了），而是：

- 48 kHz 是 WASAPI 混音引擎的原生速率，改采样率只会凭空引入一次重采样；
- 立体声是 loopback 的天然上限，声道数一变，`ASC` 常量（`[0x11, 0x90]`）和
  `esds` 里的 `channelConfiguration` 全要跟着变，收益为零、出错面变大。

`recorder_get_stats` **不新增音频计数器**（保持 `RecorderStats` 布局不变，
因此本轮不需要再升 v3）。丢包/补静音走日志：

```
[murec] 音频：补静音 120 ms（后台无音频渲染）
[murec] 音频：队列溢出，丢弃 3 个包（共 64 ms）
```

---

## 10. 错误与降级矩阵

| 情形 | 行为 |
|---|---|
| `audio_source = NONE` | 不建音频轨，不启音频线程 |
| `AUDIO_DEVICE` 但 `audio_device_id` 为空 | `recorder_create` 返回 `REC_ERR_INVALID`（已有） |
| `AUDIO_WINDOW` 但 hwnd 与 pid 都是 0 | `recorder_create` 返回 `REC_ERR_INVALID`（已有） |
| 默认设备不存在 / 设备被禁用 | `recorder_create` 返回 `REC_ERR_IO` + 原因 |
| `AUDIO_WINDOW` 且系统 < Win10 2004 | `recorder_create` 返回 `REC_ERR_INVALID` + 版本要求 |
| AAC MFT 不存在（极罕见） | `recorder_create` 返回 `REC_ERR_INVALID` + 「系统缺少 AAC 编码器」 |
| 录制中设备失效 | 补静音 + 最多 10 次重建；全失败 → `REC_ERR_IO`，流水线停止 |
| 音频队列溢出（>2 秒积压） | 丢最旧的包并打日志，**不中断录制** |

**没有「音频失败就静默出无声文件」这条路径。** 这是与 `encoder=HEVC` 静默降级
H.264 同类的问题，已经栽过一次，设计上直接堵死。

---

## 11. 不变量与测试矩阵

### 单元测试（不需要桌面）

| 用例 | 断言 |
|---|---|
| `two_track_moov_has_two_trak` | 停止后 `moov` 里恰好 2 个 `trak`，`tkhd.track_ID` = 1,2 |
| `init_moov_has_trex_per_track` | `mvex` 里 2 个 `trex` |
| `audio_trak_uses_smhd_and_mp4a` | 音频 `minf` 是 `smhd`、`stsd` 是 `mp4a`、含 `esds` |
| `esds_matches_expected_bytes` | 42 字节，`objectTypeIndication=0x40`、ASC=`11 90` |
| `audio_stts_is_single_entry` | 音频 `stts` 只有一条 entry 且 delta=1024 |
| `audio_trak_has_no_stss` | 音频轨无 `stss` |
| `mvhd_duration_is_max_of_tracks` | 音频比视频长时 `mvhd.duration` 取音频 |
| `video_only_output_is_unchanged` | 不传音频时产物与改造前逐字节一致 |
| `slice_aligns_audio_to_video_keyframe` | 入参 `start_us` 落在 GOP 中间时，音频起点跟随关键帧 |
| `slice_output_has_two_trak` | 切片产物两轨都在，且 `co64` 各指各的 `mdat` |
| `silence_padding_keeps_timeline` | 只喂静音时，音轨时长 ≈ 墙钟时长 |

### e2e（需桌面，`#[ignore]`）

| 用例 | 断言 |
|---|---|
| `e2e_audio_with_software_encoder` | 录 3 s + `Beep()`，MF 读回两条流，音频采样率 48000/2ch，时长差 < 50 ms |
| `e2e_audio_with_gpu_encoder` | 同上，GPU 路径 |
| `e2e_audio_timeline_matches_video_when_desktop_is_quiet` | 音轨时间轴与视频轨差 ≤ 250 ms（治静音补包；桌面有声无声都成立） |
| `e2e_audio_survives_slicing` | 录制中切片，产物带音轨且音画等长 |
| `e2e_audio_device_invalidated_recovers` | 录制中切换默认设备，产物时间轴不断 |

读回方法（无 ffmpeg，全用 MF）：

```rust
let reader = MFCreateSourceReaderFromURL(path, None)?;
reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS, false)?;
// FIRST_VIDEO_STREAM → 帧数 / 宽高（复用现有 mf_readback）
// FIRST_AUDIO_STREAM → MF_MT_AUDIO_SAMPLES_PER_SECOND / NUM_CHANNELS / 总帧数
// 两条流的 MF_PD_DURATION 之差 < 50 ms
```

---

## 12. 改动文件清单

| 文件 | 动作 |
|---|---|
| `src/capture/wasapi.rs` | **新建**：`AudioCapture`（三种音源）、设备重建、进程环回激活 |
| `src/com.rs` | **新建**：`ComApartment`，COM 套间的 RAII 守卫（解决 13.1 / 13.3） |
| `src/capture/mod.rs` | `pub mod wasapi;` |
| `src/encode/aac.rs` | **新建**：AAC MFT 驱动（**只驱动同步 MFT**，异步的在枚举阶段就排除）、`ASC_48K_STEREO` 常量 |
| `src/encode/mod.rs` | `pub mod aac;` |
| `src/encode/sink_raw.rs` | `SinkState` 多流化、`AddStreamSink` 按 major type 分派 |
| `src/encode/mf_sink.rs` | SinkWriter 加音频流、PCM 写入、`drain_raw` 按流分派 |
| `src/mux/mp4.rs` | `TrackState` / `create_multi` / `build_audio_sample_entry` / `build_esds` / 双 `trex` / 音频 `pending` 上限 |
| `src/mux/extract.rs` | 双轨照抄与裁切（音频起点跟关键帧、**终点跟视频真实结束点**） |
| `src/mux/remux.rs` | 双轨重建（按 `tfhd.track_ID` 归轨） |
| `src/mux/mod.rs` | 模块文档更新 |
| `src/recorder/` | 音频线程（`audio.rs`）、`MuxMsg::{Video,Audio}` 通道、`pad_silence`、`preflight_audio`、`fatal_root`（`env.rs`） |
| `src/config.rs` | `ABI_VERSION` 1 → 2 + 版本历史说明 |
| `tests/e2e_audio.rs` | **新建**：三个真录真解码的端到端用例 |
| `Cargo.toml` | 加 `"Win32_Media_Audio"`（WASAPI）与 `"Win32_UI_WindowsAndMessaging"`（HWND → PID） |
| `include/recorder.h` | ABI 升到 v2 + 变更记录 |
| `docs/csharp-guide.md` | **新建**：C# 绑定与使用指南 |
| `README.md` / `README.zh-CN.md` | 状态表、已知限制、路线图 |

---

## 13. 实现复盘：五个只有真跑起来才会暴露的缺陷

设计稿把「盒结构怎么写」写得足够细，但下面这五个问题一条都不在图纸上——它们
全部是靠 `tests/e2e_audio.rs`（真的录、真的用 MF 解码器读回）逼出来的。记在这里，
因为每一条的**症状都比原因吓人**。

### 13.1 COM 析构顺序：停止录制时进程直接消失

| | |
|---|---|
| 症状 | `recorder_stop` 期间进程以 `STATUS_ACCESS_VIOLATION (0xC0000005)` 退出，没有任何 Rust panic、没有日志 |
| 原因 | `CoUninitialize()` 写在了采集循环末尾，而这之后 `AacEncoder`（持有 `IMFTransform`）才被 drop。COM 套间已关闭的线程上 `Release` 一个 COM 对象 = 访问违例 |
| 定位方法 | 在 `stop()` 的每次 `join` 前后打标记，发现崩在 join 音频/封装线程期间；再加细分标记才锁定到「音频线程退出时」 |
| 修法 | 新增 `src/com.rs::ComApartment`（RAII）。**声明在最前 = 最后析构**，并用 `drop(aac_enc)` 显式表达「MFT 必须先于套间释放」。`gpu_recorder_thread` 里同样的隐患（形参 `gpu_sink`/`capture` 在 `CoUninitialize()` 之后才析构）一并改成守卫 |

这条最容易再犯的地方是**新增任何持有 COM 对象的局部变量**：它必须声明在
`ComApartment` 之后。这种「声明顺序即生命周期」的约束在 Rust 里没有编译器帮忙，
只能在注释里写清楚。

### 13.2 静音桌面 → 音轨 0 个样本

见 §4.5.1。本质是「补静音的前置条件依赖了它本该替代的东西」。

### 13.3 音频线程在 COM 未初始化的线程上建 MFT

| | |
|---|---|
| 症状 | `AAC 编码器创建失败: 无法激活 AAC 编码器 MFT (0x80004005)` ——一个光秃秃的 `E_FAIL`，什么都没说 |
| 原因 | `CoInitializeEx` 原本写在 `audio_capture_loop` 内部，而调用方在**进入这个函数之前**就要建 `AacEncoder`。`MFTEnumEx` 在没有 COM 的线程上照样能枚举成功，失败的是 `ActivateObject` |
| 修法 | 套间初始化上移到线程入口（`ComApartment`），`audio_capture_loop` 改为由调用方负责 |

值得记住的一点：**`MFTEnumEx` 成功不代表 COM 可用**。枚举和激活是两条完全不同的路径。

### 13.4 级联故障把根因冲掉

音频初始化失败 → 置 `stop` → 采集线程立刻退出 → 封装线程收尾时发现 0 样本 →
报「录制期间未捕获到任何帧，画面可能完全无变化」。用户看到的是最后这句，
于是去检查磁盘和画面，而真正的原因在别处。

修法：`PipelineEnv` 增加只写一次的 `fatal_root`；`stop()` 和
`recorder_get_last_error` 都优先返回根因。`last_error` 保留「最新一条」的语义给 UI。

### 13.5 切片的音频**尾巴**超出视频结束点

| | |
|---|---|
| 症状 | 切片可播放、两轨都在，但音频比视频长 0.24 s——片尾是「有声无画」 |
| 原因 | 音频按入参 `end_us` 裁剪，而视频实际只能切到**最后一个已落盘分片**的末尾。两者不是同一个时刻 |
| 修法 | 音频终点改为服从 `video_end_us = 最后一个保留视频样本的 dts + duration`。起点服从关键帧、终点服从视频，两端都由视频说了算 |
| 回归 | `mux::extract::tests::audio_tail_is_clamped_to_video_end`（141 包 vs 未修时的 203 包） |

### 13.6 顺带补上的两处

- **ABI 版本**：加了 `audio_bitrate_kbps` 却仍是 v1。尾部追加在「宿主和库一起重编」时
  安全，但 ABI 的契约是「已编译的旧宿主能继续用」——旧宿主结构体短 4 字节，会越界读。
  已升到 v2，旧宿主拿到的是明确的「abi_version 不匹配」，不是 UB。
- **音频 `pending` 无上限**：init moov 落盘前音频只进内存。视频首个关键帧迟迟不来时
  会无限增长。加 2 秒（94 包）上限，超出丢最旧的包，时间轴由 `stts` 按真实 tick 计算，
  不受影响。
