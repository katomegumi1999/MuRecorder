# MuRecorder

**Windows 屏幕录制引擎，Rust 实现，以 C ABI 导出。**

DXGI Desktop Duplication 采集 → Media Foundation 硬件编码（H.264 / HEVC / AV1），并保留一条纯 CPU 回退链路（OpenH264 + 手写 Hybrid MP4 封装）。所有路径最终都落到普通 MP4，而录制过程按崩溃安全的分片 MP4 写盘——H.264 与 HEVC 都是，AV1 因为样本格式与配置记录都不同暂时例外。不依赖 FFmpeg / GStreamer — 约 9700 行 Rust（6800 行生产代码 + 2900 行测试）加 Windows API。

为 .NET / Avalonia 桌面应用内嵌设计，但 C ABI 与宿主无关：任何能加载 `.dll`、传一个结构体的程序都能驱动它。

[English](README.md)

---

## 这个项目解决什么

网上大多数"用 Rust 录屏"的示例止步于 `AcquireNextFrame`。真正麻烦的部分在别处，而这个 crate 主要就是在写那些部分：

- **GPU 路径是真正的零拷贝。** 采集到的 `ID3D11Texture2D` 在显存内用 `CopySubresourceRegion` 裁剪，交给 `ID3D11VideoProcessor` 转 NV12，经 `MFCreateDXGISurfaceBuffer` 包装成 `IMFMediaBuffer` 直送 `IMFSinkWriter`。像素全程不进系统内存。
- **软件路径真的跟得上。** 四条固定线程（capture / encode / mux / stats），4 帧有界环满则丢帧而不阻塞，120 包队列把背压施加在磁盘上而不是 DXGI 上。
- **静止画面也能出片。** 桌面复制只在像素变化时交付帧；录副屏上的静态文档时连**首帧**都拿不到。`capture` 会检测到这种情况，用一次性 GDI 抓屏补首帧，之后按节拍重发上一帧维持 CFR。
- **panic 绝不越过 FFI 边界。** 每个导出函数都用 `catch_unwind` 包裹并映射为 `REC_ERR_PANIC`；release profile 特意保留 `panic = "unwind"`，否则这个守卫根本无法生效。
- **Hybrid MP4 写入器，崩溃不死。** 录制期间按分片 MP4 写盘，意外中断最多损失最后一个分片而不是整场录制；停止时只写 16 字节加索引，就能把它变成一个普通的 `ftyp`/`mdat`/`moov` 文件。零预留、零重封装、不搬移任何媒体数据。`co64` 负责 64 位 chunk 偏移，`mdat` 用 64 位 largesize，单段录制超过 4 GiB 也不会损坏。
- **GPU 路径同样拿到崩溃保护。** 把容器类型设成 `MFTranscodeContainerType_FMPEG4`（只设 `MF_MPEG4SINK_MIN_FRAGMENT_DURATION` 完全无效），Media Foundation 就会写真正的分片。停止时扫一遍分片、据此重建完整 `moov`，再做同一套 16 字节 soft remux —— 媒体字节一个都不搬，成本只与样本数成正比，与文件大小无关。
- **HEVC 也拿到了同一份崩溃保护。** Media Foundation 自带的分片 MP4 sink 只接受 H.264，于是 HEVC 原本只能落普通 MP4：不抗崩溃、录制中不能切片、`mdat` 与偏移表还是 32 位。但 Hybrid 本身是**容器策略，与编码器无关**——堵住的是"谁来写盒"。所以这里自己实现了一个 `IMFMediaSink`：`IMFSinkWriter` 继续负责挑硬件 MFT、协商、编码，编出来的码流交给我们的封装器写盒。HEVC 于是与 H.264 完全同等：分片落盘、崩溃只丢最后一个分片、录制中随时可切。（AV1 的样本是 OBU 而非 Annex B，配置记录也不同，暂时仍走普通 MP4。）
- **录制中随时切任意一段，且与编码器无关。** `recorder_extract_range` 从活动文件里切出任意时间区间，产出一份可直接上传的普通 MP4。每个 `moof` 自带样本表，所以切片是纯文件拷贝、不需要修正任何偏移（只平移 `co64` 条目）；样本描述逐字节照抄源文件，于是 H.264 / HEVC / AV1 走同一条代码路径——不重编码，也不重建参数集。
- **失败不删素材，而且它还能播。** 索引写不出来时把录像改名为 `.partial` 保留，而不是删除。因为到最后一个完整分片为止它仍是合法 fMP4，Media Foundation 能逐帧读回未 finalize 的录像（实测：直接从活跃文件读出 120 帧 / 4.00 秒）——不需要 `untrunc`，不需要同源样本，不需要任何修复步骤。
- **设备丢失可恢复。** 显示器热插拔、分辨率切换、驱动复位：采集循环以 500 ms 间隔重建 duplication，最多 10 次；彻底失败会如实上报给宿主，而不是悄悄生成一个截断的文件。

## 当前进度

| 能力 | 状态 |
| --- | --- |
| C ABI + 状态机（`create` / `start` / `stop` / `stats` / `last_error` / `destroy`） | ✅ |
| DXGI 桌面复制采集，按区域裁剪，多显示器感知 | ✅ |
| GPU 编码：H.264 / HEVC / AV1（Media Foundation SinkWriter） | ✅ |
| GPU 色彩转换 BGRA → NV12（`ID3D11VideoProcessor`） | ✅ |
| CPU 编码：OpenH264（纯 Rust，静态链接），BT.709 BGRA → I420 | ✅ |
| MP4 封装（软编路径）：Hybrid（录制中崩溃安全，停止时转普通 MP4）、64 位 `mdat` largesize、`co64`、`colr`、双音视频轨 | ✅ |
| GPU 路径：分片输出 + 停止时 soft remux 成普通 MP4 | ✅（H.264 用 MF 自带 sink；HEVC 用自研 sink；AV1 只能落普通 MP4） |
| 自研 `IMFMediaSink`：编码器交给 MF、写盒归自己（`avcC` / `hvcC` / `mp4a`） | ✅ |
| 录制中区间切片（零重编码，音视频关键帧对齐，产物是普通 MP4） | ✅ |
| 从录制中的文件切出任意时间区间（`recorder_extract_range`） | ✅ |
| 与编码器无关的区间切片（H.264 / HEVC / AV1，`stsd` 原样照抄） | ✅ |
| 编码器阶梯：（编码器 × 容器）候选，HEVC/自研分片 → H.264/分片 → 普通 MP4 → 软件 | ✅ |
| 静止画面 CFR（重复帧 + GDI 首帧兜底） | ✅ |
| 设备丢失重建；锁屏 / UAC 安全桌面后自动续录 | ✅ |
| 线程安全句柄（一线程 stop、另一线程轮询 stats） | ✅ |
| 单文件录制超过 4 GiB（分片路径为 64 位 `mdat` + `co64`；AV1 普通 MP4 只能升级 `co64`，`mdat` 头超限会如实上报） | ✅ |
| 无锁帧计数 + 200 ms 采样快照 | ✅ |
| 音频录制（WASAPI loopback 默认设备 / 指定设备，进程树环回，AAC-LC 320k 双声道） | ✅ |
| 鼠标指针合成（单色 / 32bpp / masked-color 三种形状，位置精确，不留残影） | ✅ |
| 指针视觉特效烧进画面：点击扩散圆环（350 ms）+ 移动轨迹残影（40–3000 ms），均可在 C ABI 上单独开关 | ✅ |
| HDR / 广色域采集 | ❌ 仅 SDR |

## 环境要求

- Windows 10 1809+ / Windows 11，x64。
- Rust 1.85+（edition 2024）。首次构建需要 C 工具链 —— `openh264` 会编译约 27 个 C 文件。MSVC 开箱可用；不支持 `gcc` 工具链。
- 无运行期依赖：OpenH264 静态链接，产物 DLL 自包含。

## 快速开始

```bash
cargo build --release
# → target/release/murecorder_core.dll
```

把 `murecorder_core.dll` 放到可执行文件旁。
- C 头文件：[`include/recorder.h`](include/recorder.h)
- 详细 C# 调用文档与完整示例：[`docs/csharp-guide.md`](docs/csharp-guide.md)
- 指针合成的设计取舍与踩坑记录：[`docs/cursor-design.md`](docs/cursor-design.md)
- 指针特效（点击波纹 / 移动轨迹）的设计记录：[`docs/cursor-effects.md`](docs/cursor-effects.md)

```csharp
[StructLayout(LayoutKind.Sequential)]
struct RecorderConfig
{
    public uint AbiVersion;                     // 必须为 3
    public int RegionX, RegionY;                // 物理像素，虚拟桌面坐标
    public uint RegionW, RegionH;
    public uint Fps;
    public uint Encoder;                        // 0=auto, 1=hevc, 2=h264, 3=amf, 4=soft, 5=av1
    public uint BitrateKbps;
    public uint CaptureCursor;
    [MarshalAs(UnmanagedType.LPUTF8Str)] public string OutputPath;
    public uint AudioSourceType;                // 0=none, 1=system, 2=device, 3=window
    [MarshalAs(UnmanagedType.LPUTF8Str)] public string? AudioDeviceId;
    public ulong AudioWindowHwnd;
    public uint AudioProcessId;
    public uint AudioBitrateKbps;               // 0=默认(320k), 支持 96/128/160/192/256/320
    public uint CursorClickEffect;               // 0/1，需 CaptureCursor = 1
    public uint CursorTrailMs;                   // 0=关闭，否则 40~3000，需 CaptureCursor = 1
}

if (Native.Create(ref cfg, out var h) != 0) throw new Exception("create failed");
Native.Start(h);
// ... 录制 ...
Native.Stop(h);      // 阻塞至 MP4 索引写盘完成
Native.Destroy(h);
```

生命周期严格单向：`Created → Running → Stopped`。`stop` 幂等，并阻塞到封装器完成文件收尾。已停止的句柄不能重启 —— 新建一个。

## 架构

```
                 ┌────────────────── GPU 路径（encoder=auto 时默认）──────────────────────┐
 IDXGIOutputDuplication ──▶ ID3D11Texture2D ──▶ CopySubresourceRegion   （显存内裁剪）
                                             ──▶ ID3D11VideoProcessor   （BGRA→NV12）
                                             ──▶ MFCreateDXGISurfaceBuffer
                                             ──▶ IMFSinkWriter（挑编码器 + 编码）
                                                   │  H.264 ──▶ MF 分片 sink
                                                   │  HEVC  ──▶ 自研 IMFMediaSink
                                                   ▼              │
                                             fMP4 ◀───────────────┘
                                             ──▶ 停止时 soft remux ──▶ MP4
                 └───────────────────────────────────────────────────────────────────────┘

                 ┌──────────────────────── CPU 回退路径 ─────────────────────────────────┐
 IDXGIOutputDuplication ──▶ staging 纹理 ──Map──▶ BGRA ──▶ ring[4]（满则丢）
                                                    ──▶ BGRA→I420（BT.709）
                                                    ──▶ OpenH264 ──▶ packets[120]
                                                    ──▶ Hybrid MP4 封装器
                 └───────────────────────────────────────────────────────────────────────┘
```

落盘形态（`stop` 前后）：

```
软编           录制中：  ftyp │ free(16B) │ moov(init) │ moof mdat │ moof mdat │ ...
GPU H.264      录制中：  ftyp │ uuid │ pdin │ moov(init) │ moof mdat │ ... │ mfra
              停止后：  ftyp │ mdat（一个巨型盒，把上面整段吞进去）│ moov（全量索引）
GPU HEVC       录制中：  ftyp │ free(16B) │ moov(init) │ moof mdat │ moof mdat │ ...
              停止后：  ftyp │ mdat（同上）│ moov（同上）
GPU AV1        录制中：  ftyp │ uuid │ mdat（还在长，没有索引）
              停止后：  ftyp │ uuid │ mdat │ moov
```

前三种形态在录制中都是合法 fMP4：任何播放器都能打开，`recorder_extract_range` 也能切。
停止时在末尾追加完整 `moov`，再把 `ftyp` 之后那 16 字节改写成覆盖到它之前的
64 位 `mdat` 头——读取方只会看到经典的三盒布局。这与 OBS Studio 30.2 起的
Hybrid MP4 输出同源。

区别只在「谁写盒」：软编是我们自己的 `Mp4Muxer`，GPU H.264 是 Media Foundation
自带的分片 sink，GPU HEVC 是我们实现的 `IMFMediaSink`（MF 只负责挑编码器并编码，
码流交回来给我们写）。三者收尾方式一致，产物也同构。

GPU AV1 是最后一种形态：它的样本是 OBU 而不是 Annex B，配置记录也不同，自研封装
暂时还不支持，只能落普通 MP4 —— `moov` 要到停止录制才写，既不抗崩溃、也不能录制中切片。
落盘时会明确打一行日志说明这个代价。

软编与 GPU HEVC 路径本来就在内存里握着全部样本，直接写索引即可。GPU H.264 的盒是
Media Foundation 写的，所以由 `mux::remux` 重新读一遍分片：`stsd`（解码配置）逐字节照抄，
`stts`/`stss`/`stsc`/`stsz`/`co64` 从各 `moof` 的样本表重建，并丢掉 `mvex`。
`co64` 里的偏移就是样本的原始绝对偏移——没有任何数据被复制。

目录：

```
src/
├─ lib.rs            C ABI 导出 + panic 守卫
├─ ffi.rs            C 字符串工具 + 静默 panic hook
├─ config.rs         配置校验（ABI 版本、fps、码率、区域）
├─ error.rs          错误码，与 recorder.h 一一对应
├─ stats.rs          原子计数器 + 滑动窗口采样
├─ recorder.rs       状态机 + 线程流水线 + GPU/CPU 分派
├─ capture/dxgi.rs   Desktop Duplication、staging 纹理、QPC 配速
├─ capture/gdi.rs    一次性 BitBlt，仅用于静止画面补首帧
├─ encode/mf_sink.rs MF SinkWriter、D3D11 视频处理器、编码器探测
├─ encode/sink_raw.rs 自研 IMFMediaSink（编码器交给 MF、写盒归自己）
├─ encode/soft.rs    OpenH264 封装 + Annex-B NAL 处理
├─ convert/i420.rs   整数 BT.709 BGRA → I420，2×2 色度抽样
├─ mux/nal.rs        Annex B ⇄ 长度前缀、avcC / hvcC 构建
├─ mux/mp4.rs        Hybrid MP4 封装器（录制中 fMP4，停止后普通 MP4）
├─ mux/fragment.rs   两种写入器共用的分片/盒扫描器
├─ mux/remux.rs      把 Media Foundation 的 fMP4 收敛成普通 MP4
└─ mux/extract.rs    从录制中的文件切出区间 → 独立普通 MP4
include/recorder.h   C ABI，与 Rust 侧对齐的手工基准
```

## 性能

BGRA → I420 转换，单线程标量实现（软件回退链路每帧必付的代价），`cargo test --release` 实测于 Ryzen 5 5600GT：

| 分辨率 | 单帧耗时 | 30 fps 下单核占用 |
| --- | --- | --- |
| 1920 × 1080 | 4.01 ms | 12.0 % |
| 2560 × 1440 | 7.80 ms | 23.4 % |
| 3840 × 2160 | 16.20 ms | 48.6 % |

GPU 路径完全绕过这一步。上述数字含每帧 3 次 `Vec` 分配；引入缓冲池与 SIMD 后可显著下降。

## 设计取舍

- **`panic = "unwind"` 是关键前提。** 设成 `panic = "abort"` 会让 FFI 边界上的第一次 panic 直接终止宿主进程。
- **句柄是 `Mutex<Recorder>`。** 宿主可以在 UI 线程轮询 stats 的同时从另一线程 stop；若像早期实现那样直接把裸指针转成 `&mut Recorder`，就是未定义行为。
- **`co64` 还不够，`mdat` 也要 64 位。** 32 位 chunk 偏移在 4 GiB 处断裂，32 位盒长同样会——只修偏移救不了盒长。`mdat` 使用 `size == 1` + largesize（ISO/IEC 14496-12 §4.2），已用 Media Foundation 自带 MP4 解析器读回验证。
- **`mdhd` 用 version 1。** 微秒 timescale 配 32 位时长，正确上报的上限只有 71 分钟；录两小时时长字段会回绕。
- **样本时间戳只取 QPC，绝不用帧序号。** `frame_count × 帧间隔` 会在流水线跟不上时把时间轴压扁（60 秒实录播成 20 秒）。音频沿用同一条规则，但更严格：它的 PTS 由**累计采样数**推算（严格恒速），QPC 只用来建 anchor 和定期校正。
- **预留方案是量化后主动放弃的。** 早期版本在每个文件头预留约 1.7 MB 的 `free`，好让 `moov` 停止时原地写入（不搬数据就 faststart）。这是**每个文件**的固定开销：5 秒 1080p 短片因此膨胀 46%，而它腾位置的那个 `moov` 只有约 4 KB。现在 16 字节的 `free` 只是占位，唯一用途是被改写成 `mdat` 头。
- **faststart 是刻意不做的。** 成品里 `moov` 在 `mdat` 之后。本地播放无损（播放器都会跳到文件尾），上传后由平台转码的场景也无损（YouTube/B 站在服务端解析整个文件），只影响网页边下边播——那个场景用一次性的 `ffmpeg -movflags +faststart` 处理，比给每个文件预留空间划算得多。
- **收尾失败不许毁素材。** `moov` 写不出来时把文件改名为 `<name>.mp4.partial` 保留，而不是删除。因为到最后一个完整分片为止它仍是合法 fMP4，能直接播，也能用 `ffmpeg -i x.mp4.partial -c copy fixed.mp4` 转成正常 MP4——不需要 `untrunc` 那套同源样本修复。
- **按分片分 chunk，不按样本。** 分片化破坏了样本连续性，原来的单 chunk + 单 `co64` 写法失效。若每样本一个 chunk，`moov` 会涨到 24 B/样本（1 小时约 2.6 MB）；按分片分只需 8 B/分片，`stsc` 还能按相同样本数做 run-length 合并，1 小时约 36 KB。
- **色彩靠标注而非猜测。** 两条路径都按 BT.709 / 有限范围处理；软编路径写入 `colr`（nclx）盒并把 SPS VUI 设成同一套参数，让容器声明、码流声明、像素数据三者一致。
- **码控必须显式指定。** `openh264` 的 `EncoderConfig` 默认是 `RateControlMode::Quality`，此时目标码率被**静默忽略**。软编路径显式选 `Bitrate` + `ScreenContentRealTime`，并强制 2 秒 GOP，否则长录制的进度条拖不动。
- **`DuplicateOutput` 之后的第一帧是无效图像**（实测整帧全 0）。若把它当首帧，静止画面场景会一路重发这张黑帧，整段录像全黑——所以直接丢弃，交给 GDI 兜底取真实像素。
- **`MFTranscodeContainerType_FMPEG4`，而不是只设分片时长。** 单独设 `MF_MPEG4SINK_MIN_FRAGMENT_DURATION` 完全不分片——MF 仍然写 `ftyp/uuid/mdat/moov`；切换容器类型才是分片开关。
- **MF 自带的分片 MP4 sink 只支持 H.264——这个限制是真的，但它只存在于那一层。** 把输出类型换成 HEVC 后再要求 `MFTranscodeContainerType_FMPEG4`，`BeginWriting()` 直接返回 `MF_E_INVALIDMEDIATYPE`。这不是分辨率、profile、像素格式或码率的问题，也不是这条流水线的错——用裸 `IMFSinkWriter`（不挂 DXGI Device Manager）同样复现：320×240 到 1080p、NV12 或 ARGB32 输入、3–8 Mbps 全部失败，而同一套参数换 H.264 就成功。在搞清楚这一点之前，失败的尝试被降级链吞掉，`encoder = HEVC` 会**静默产出 H.264**。
- **于是把「谁写盒」拿了回来。** 自研一个 `IMFMediaSink`（`encode/sink_raw.rs`），用 `MFCreateSinkWriterFromMediaSink` 挂上去：挑编码器、协商媒体类型、调度硬件 MFT 仍归 `IMFSinkWriter`，只有编码后的码流由 `ProcessSample` 交给我们自己的 `Mp4Muxer`。HEVC 因此也拿到了分片态——Hybrid 是**容器策略**，本来就跟编码器无关，挡路的只是那一层 sink 的实现。降级链随之变成 `HEVC/自研分片 → H.264/分片 → 普通 MP4 → 软编`，选到普通容器时会明确打日志说明代价（不抗崩溃、录制中不能切片）。AV1 仍落普通 MP4：它的样本是 OBU 而不是 Annex B，`av1C` 的来源和写法都不一样，暂未实现。
- **代价是得补上 MF 原来替我们做的那一步。** MF 的编码器输出 **Annex B**（起始码分隔、参数集混在码流里），而 MP4 要的是长度前缀加 `avcC`/`hvcC` 配置盒。`mux/nal.rs` 干这件事：剥参数集、转长度前缀、从 SPS 的 `profile_tier_level` 构 `hvcC`。这里有个坑——HEVC 的 SPS 带仿真防竞争字节（`00 00 03`），解析前必须先剥掉，否则 `general_level_idc` 会读成 `0x00`（正确值是 `0x5A`）。也验证过 `MFVideoFormat_H264_ES` / `HEVC_ES` 走不通：`SetInputMediaType` 返回 `MF_E_TRANSFORM_NOT_FOUND`，因为 SinkWriter 的输入是 NV12，中间还得挂一个格式转换 MFT，所以这段转换只能自己做。
- **自定义 sink 得照 SinkWriter 的隐含契约来。** 这套握手文档里没写全，全靠试错：`GetStreamSinkCount()` 一开始必须返回 0（返回 1 会让它在 `BeginWriting` 按旧编号回查，拿 `MF_E_INVALIDSTREAMNUMBER`）；`AddStreamSink` 必须真的新建一条流且 `GetIdentifier` 与传入的 identifier 一致；每条流得是独立 COM 对象（复用会撞上同一事件队列的 `MF_E_MULTIPLE_BEGIN`）；`GetMediaTypeCount()` 不能是 0（否则死在 `MF_E_NO_MORE_TYPES`）；`AddStreamSink` 里要排 `MEStreamSinkStarted` 并按拉模式排 `MEStreamSinkRequestSample`，否则样本照收但编码器永不泵动；`PlaceMarker` 回排的事件 value 必须是 `pvarContextValue` 而非 `pvarMarkerValue`，不然 `Finalize()` 返回 `MF_E_INVALIDREQUEST`。完整清单见 `encode/sink_raw.rs` 的模块文档。
- **区间切片与编码器无关，因为它是照抄 `stsd` 而不是重建。** 早期实现把每个样本读出来交给手写的 H.264 写入器重新封装，于是 HEVC/AV1 完全切不了。现在产物的样本描述是源文件的**逐字节拷贝**（`avcC` / `hvcC` / `av1C`），媒体字节整段连续复制，只平移 `co64` 偏移。索引里没有任何与编码器相关的字段，也不再假设 NAL 长度前缀是 4 字节。
- **用 `MFCreateDXGISurfaceBuffer` 而非 CPU 上传。** HEVC/AV1 时直接告知 SinkWriter 输入已是 NV12，避免 Media Foundation 擅自插入软件色彩转换器（某些驱动上这是经典死锁源）。

## 构建与测试

```bash
cargo test                    # 79 项：ABI 守卫、状态机、NAL 切分、BT.709 数学、
                              # 码率控制、MP4 盒树（Hybrid 布局、fMP4 录制态、
                              # 按分片分 chunk、64 位 mdat / mdhd v1）、
                              # Annex B → 长度前缀、avcC / hvcC 构建（含 HEVC
                              # 仿真防竞争字节剥离）、别人的分片→普通 MP4 收敛、
                              # 与编码器无关的区间切片（HEVC/AV1 标记样本）、
                              # 真实编码→封装流水线（3 编码器 × 3 容器，产物用
                              # MF 解析器读回）
cargo clippy --all-targets    # 预期零告警
cargo fmt --all -- --check

# 需要真实桌面会话（GPU 项还需硬件编码器）。**必须串行**：
# 并行会争抢同一个 D3D 设备与桌面复制会话。
cargo test --test e2e_record -- --ignored --nocapture --test-threads=1
cargo test --lib -- --ignored --nocapture   # 真实 SinkWriter：每个编码器 × 容器组合
```

`pipeline_encodes_synthetic_frames_into_valid_mp4` 用合成帧驱动真实编码器与真实封装器，并校验产物的盒树结构——确定性覆盖，不需要桌面会话。

`tests/e2e_record.rs` 真录 2 秒屏幕，再用 Media Foundation 自带 MP4 解析器读回，校验帧数、尺寸与时间轴长度。它标了 `#[ignore]`（需要桌面），但它是唯一能证明"产物被**真实**解复用器接受"的用例。另有几个 GPU 用例：`e2e_gpu_recording_finalizes_to_plain_mp4` 断言容器收敛成 `ftyp`/`mdat`/`moov` 且 Media Foundation 接受重建后的索引；`e2e_slice_gpu_recording_while_running` 在**文件仍在写入时**切一段出来，再同时校验切片和原录制（AV1 是唯一例外：它确实拒绝，并且断言拒绝的理由说得明确，不会静默跳过）；`e2e_hevc_records_when_explicitly_requested` 证明 HEVC 走的是自研 sink——**录制中**文件里就有 `moof`、`moov` 带 `mvex`，停止后 `moov` 不再带 `mvex` 且 `hvc1` 真的出现，而不是被悄悄换成 H.264。`e2e_slice_from_recording_is_playable` 覆盖软编路径的同样能力。

`tests/e2e_cursor.rs` 会真的移动鼠标，断言「旧位置」和「新位置」两处像素块在两帧之间都发生了变化——这是唯一能证明指针真的走到了编码像素上的办法。它的断言刻意用**采集到的**指针位置而不是我们设的位置：机器是活的，鼠标随时可能被人接管，两者本来就可能差几个像素。另外 `encode::mf_sink::tests::gpu_cursor_is_composited_and_leaves_no_trail` 用合成光标驱动 GPU 写入器，再把喂给编码器的那份纹理读回来验像素，锁住四件事：位置对、光标移走后旧址恢复成桌面（不留残影）、指针不可见时一个像素都不画、以及「桌面和指针都不动但波纹还活着」的动画帧能从纯桌面副本重绘并随后抹干净。`e2e_gpu_recording_with_cursor_effects` 开着两个特效真录 2 秒，要求帧数与时间轴长度不变——这正是「动画帧真的进了编码器、而不是卡在『什么都没变』的快捷路径上」的证据。点击的**触发**刻意不进 e2e：那意味着要在用户桌面上真的按一下鼠标。

## 已知限制

- 鼠标指针是**烧进画面像素**的，不是独立叠加层：录完无法再关掉。GPU 路径只回读光标包围盒（通常 32×32）而不回读整帧，所以桌面部分仍是零拷贝。两个如实的缺口：只用来补首帧的一次性 GDI 兜底里没有指针（形状只能在持有 DXGI 帧时取到）；纯硬件覆盖层内容（部分全屏独占游戏）本来就不在桌面复制的视野内，与这个开关无关。点击检测是每帧轮询 `GetAsyncKeyState`，不装低级鼠标钩子（钩子要一条带消息循环的线程、是全局的、超时会被系统静默摘掉）——代价是短于一个帧间隔的按下-抬起可能漏掉，而人手点击持续 50–200 ms，不会漏。移动轨迹按采样点的外接矩形裁剪，快速斜向拖动会丢掉最旧的几段残影（是变淡消失，不是整条断掉）。
- **音频只有 AAC-LC**，48 kHz / 双声道，码率限定在 `audio_bitrate_kbps` 的六档（96/128/160/192/256/320 kbps）里。采样率不开放：48 kHz 是 WASAPI 混音引擎的原生速率，换别的只会平白多一次重采样。
- 音轨会被**真的解码一遍**再判定（`cargo test --test e2e_audio -- --ignored`）：`esds`/ASC 与码流不符会在这里失败，而不是等播放器表现为「有轨无声」。
- HDR 桌面按 SDR BGRA8 采集，画面会被压平。
- **成品不是 faststart**（`moov` 在 `mdat` 之后）。本地播放和「上传后由平台转码」都无感；真要网页直链边下边播，事后跑一次 `ffmpeg -i in.mp4 -c copy -movflags +faststart out.mp4`。
- **区间切片要求文件处于分片态，与编码器无关。** 前提是文件里有 `moof`（每片自带样本表）。软编、GPU H.264、GPU HEVC 都是这么写的，所以**录制中即可切**；只有 GPU AV1 不是——它退回普通 MP4，`moov` 要到停止录制才写，此时 `recorder_extract_range` 返回 `REC_ERR_IO` 并说明原因，而不是产出垃圾。
- 切片的时间粒度受关键帧间隔约束：起点会回溯到最近的关键帧（软编强制 2 秒 GOP），终点精确裁到样本。对齐的是关键帧**样本**而不是分片——Media Foundation 的分片按时间切，关键帧经常落在片中间。
- 超过 4 GiB 的 MP4 依赖 ISO 14496-12 的 `largesize` 形式；不认这种写法的小众解析器打不开这类文件。
- `encoder` 取值只是偏好提示，实际由驱动暴露的硬件 MFT 决定。
- 无暂停/恢复，录制中不能改区域。

## 路线图

1. 转换路径引入缓冲池与 SIMD（4K 目前要吃掉半个核心）。
2. 暂停/恢复、录制中热改区域。
3. AV1 的自研分片封装（`av1C` + OBU 样本），把崩溃安全与录制中切片补齐到最后一种编码器。

## 许可

MIT — 见 [LICENSE](LICENSE)。

第三方：[OpenH264](https://github.com/cisco/openh264)（BSD-2-Clause），经 `openh264` crate 从源码静态编译。注意 Cisco 的免专利费政策适用于**由 Cisco 分发**的二进制；若你再分发内嵌 OpenH264 的构建产物，请自行核对专利立场。
