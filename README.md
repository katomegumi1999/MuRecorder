# MuRecorder

**A screen-recording engine for Windows, written in Rust, exposed as a C ABI.**

DXGI Desktop Duplication → Media Foundation hardware encoder (H.264 / HEVC / AV1), with a pure-CPU fallback path (OpenH264 + a hand-written Hybrid MP4 muxer). Every path lands on a plain MP4 at stop, and recording is done as a crash-safe fragmented MP4 first — H.264 and HEVC both; AV1 is the one exception, because its samples and codec config record are shaped differently. No FFmpeg, no GStreamer — 9.7k lines of Rust (6.8k production + 2.9k tests) and the Windows API.

Designed to be embedded in a .NET / Avalonia desktop app, but the C ABI is host-agnostic: anything that can load a `.dll` and pass a struct can drive it.

[中文说明](README.zh-CN.md)

---

## Why this exists

Most "record the screen from Rust" tutorials stop at `AcquireNextFrame`. The hard parts are elsewhere, and this crate is mostly about them:

- **The GPU path is truly zero-copy.** The captured `ID3D11Texture2D` is cropped inside VRAM with `CopySubresourceRegion`, converted to NV12 by an `ID3D11VideoProcessor`, wrapped as an `IMFMediaBuffer` via `MFCreateDXGISurfaceBuffer` and handed straight to `IMFSinkWriter`. Pixels never visit system RAM.
- **A software path that actually keeps up.** Four fixed threads (capture / encode / mux / stats), a bounded 4-frame ring that drops instead of blocking, and a 120-packet queue that applies backpressure to the disk rather than to DXGI.
- **Static screens still produce video.** Desktop Duplication only delivers a frame when the pixels change. Record a static document on a secondary monitor and you get *nothing* — not even a first frame. `capture` detects that, falls back to a one-shot GDI grab for the first frame, then re-sends the last frame at the requested cadence to keep output CFR.
- **Panic never crosses the FFI boundary.** Every export is wrapped in `catch_unwind` and mapped to `REC_ERR_PANIC`; the release profile deliberately keeps `panic = "unwind"` so that guard can work at all.
- **A Hybrid MP4 writer that survives a crash.** The file is written as a fragmented MP4 while recording, so an interruption costs at most the last fragment instead of the whole session — yet at stop time it is converted into a plain `ftyp`/`mdat`/`moov` file by writing 16 bytes plus the index. No reserved padding, no remux, no moved media. `co64` for chunk offsets plus a 64-bit `mdat` largesize, so a single recording can pass 4 GiB without corruption.
- **The GPU path gets the same crash safety.** `IMFSinkWriter` is asked for `MFTranscodeContainerType_FMPEG4` (setting `MF_MPEG4SINK_MIN_FRAGMENT_DURATION` alone does nothing), so Media Foundation writes real fragments. At stop the fragments are scanned, a full `moov` is rebuilt from them, and the same 16-byte soft remux is applied — media bytes are never copied, so the cost scales with the sample count, not the file size.
- **HEVC gets that same crash protection.** Media Foundation's own fragmented MP4 sink accepts H.264 only, so HEVC used to be stuck on a plain MP4: no crash safety, no mid-recording slicing, 32-bit `mdat` and chunk offsets. But Hybrid is a **container strategy and has nothing to do with the codec** — what blocks it is *who writes the boxes*. So this crate implements its own `IMFMediaSink`: `IMFSinkWriter` still picks the hardware MFT, negotiates media types and encodes, and the encoded bitstream is handed to our muxer through `ProcessSample`. HEVC is now on equal footing with H.264 — fragmented on disk, a crash costs the last fragment, any range can be cut while recording. (AV1 samples are OBUs rather than Annex B and its config record differs, so it still lands as a plain MP4.)
- **Slice a range while still recording — with any codec.** `recorder_extract_range` cuts an arbitrary time range out of the active file into a fresh, uploadable plain MP4. Every `moof` carries its own sample table, so the cut is a pure file copy with zero offset fix-ups (only `co64` entries are shifted). The sample description is copied byte-for-byte, so H.264, HEVC and AV1 all go through the same code path — nothing is re-encoded and no parameter set is rebuilt.
- **Footage is never deleted on failure, and it stays playable.** If finalizing the index fails, the recording is renamed to `.partial` rather than removed. Because the file is a valid fragmented MP4 up to that point, Media Foundation reads an un-finalized recording back frame-for-frame (verified: 120 frames / 4.00 s straight off the live file) — no `untrunc`, no reference clip, no repair step.
- **Device loss is recoverable.** Monitor hot-plug, resolution change, driver reset: the capture loop re-creates the duplication up to 10 times (500 ms apart) before giving up, and the failure is surfaced to the host instead of silently truncating your file.

## Status

| Capability | State |
| --- | --- |
| C ABI + state machine (`create` / `start` / `stop` / `stats` / `last_error` / `destroy`) | ✅ |
| DXGI Desktop Duplication capture, region-clipped, multi-monitor aware | ✅ |
| GPU encode: H.264 / HEVC / AV1 via Media Foundation SinkWriter | ✅ |
| GPU color conversion BGRA → NV12 via `ID3D11VideoProcessor` | ✅ |
| CPU encode: OpenH264 (pure Rust, statically linked), BT.709 BGRA → I420 | ✅ |
| MP4 muxing (CPU path): Hybrid (crash-safe fMP4 → plain MP4 at stop), 64-bit `mdat` largesize, `co64`, `colr`, single video track | ✅ |
| GPU path: fragmented output + soft remux to a plain MP4 at stop | ✅ (H.264 uses MF's fragmented sink; HEVC uses our own sink; AV1 lands as plain MP4) |
| Custom `IMFMediaSink` (encode via `IMFSinkWriter`, mux our own boxes) | ✅ |
| Range extraction while recording (zero re-encode, plain MP4 out) | ✅ |
| Cut an arbitrary time range out of a live recording (`recorder_extract_range`) | ✅ |
| Codec-agnostic range extraction (H.264 / HEVC / AV1 — `stsd` copied verbatim) | ✅ |
| Encoder ladder: (codec × container) candidates, HEVC/own-fragment → H.264/fragment → plain MP4 → software | ✅ |
| Static-screen CFR (duplicate frame + GDI first-frame fallback) | ✅ |
| Device-lost recovery, auto-resume after lock screen / UAC secure desktop | ✅ |
| Thread-safe handle (concurrent `stop` + `get_stats` from different threads) | ✅ |
| Recordings larger than 4 GiB in a single file (64-bit `mdat` + `co64` on the fragmented paths; the AV1 plain path upgrades `co64` only and reports an `mdat`-header risk honestly) | ✅ |
| Lock-free frame counters, 200 ms sampled snapshot | ✅ |
| Cursor compositing | ❌ not yet |
| Audio (WASAPI loopback / per-process loopback) | ❌ not yet |
| HDR / wide-gamut capture | ❌ SDR only |

## Requirements

- Windows 10 1809+ / Windows 11, x64.
- Rust 1.85+ (edition 2024). A C toolchain is required — `openh264` compiles ~27 C files on first build. MSVC works out of the box; a `gcc` toolchain is not supported.
- No runtime dependencies: OpenH264 is linked statically, so the produced DLL is self-contained.

## Quick start

```bash
cargo build --release
# → target/release/recorder_core.dll  (+ recorder_core.lib if you want to link C)
```

Drop `recorder_core.dll` next to your executable. The C header is [`include/recorder.h`](include/recorder.h).

Minimal C# consumer (abridged — see the header for the full contract):

```csharp
using System.Runtime.InteropServices;

[StructLayout(LayoutKind.Sequential)]
struct RecorderConfig
{
    public uint AbiVersion;                     // must be 1
    public int RegionX, RegionY;                // physical pixels, virtual-desktop coords
    public uint RegionW, RegionH;
    public uint Fps;
    public uint Encoder;                        // 0 = auto, 1 = hevc, 2 = h264, 3 = amf, 4 = soft, 5 = av1
    public uint BitrateKbps;
    public uint CaptureCursor;
    [MarshalAs(UnmanagedType.LPUTF8Str)] public string OutputPath;
    public uint AudioSourceType;                // must be 0 (audio is not implemented yet)
    [MarshalAs(UnmanagedType.LPUTF8Str)] public string? AudioDeviceId;
    public ulong AudioWindowHwnd;
    public uint AudioProcessId;
}

static partial class Native
{
    [LibraryImport("recorder_core", EntryPoint = "recorder_create")]
    internal static partial int Create(ref RecorderConfig cfg, out IntPtr handle);

    [LibraryImport("recorder_core", EntryPoint = "recorder_start")]
    internal static partial int Start(IntPtr handle);

    [LibraryImport("recorder_core", EntryPoint = "recorder_stop")]
    internal static partial int Stop(IntPtr handle);

    [LibraryImport("recorder_core", EntryPoint = "recorder_destroy")]
    internal static partial void Destroy(IntPtr handle);

    // Cut [startUs, endUs) out of a *live* recording into a standalone MP4.
    [LibraryImport("recorder_core", EntryPoint = "recorder_extract_range")]
    internal static partial int ExtractRange(
        [MarshalAs(UnmanagedType.LPUTF8Str)] string src,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string dst,
        ulong startUs, ulong endUs, uint fps, out ulong durationUs);
}

var cfg = new RecorderConfig
{
    AbiVersion = 1, RegionX = 0, RegionY = 0, RegionW = 1280, RegionH = 720,
    Fps = 30, Encoder = 0, BitrateKbps = 6000, OutputPath = @"C:\tmp\demo.mp4",
};

if (Native.Create(ref cfg, out var h) != 0) throw new Exception("create failed");
Native.Start(h);
// ... record ...
Native.Stop(h);      // blocks until the MP4 index is flushed
Native.Destroy(h);
```

Lifecycle is strict and one-way: `Created → Running → Stopped`. `stop` is idempotent and blocks until the muxer has finalized the file. A stopped handle cannot be restarted — create a new one.

## Architecture

```
                 ┌────────────────── GPU path (default for encoder=auto) ─────────────────┐
 IDXGIOutputDuplication ──▶ ID3D11Texture2D ──▶ CopySubresourceRegion   (crop, in VRAM)
                                             ──▶ ID3D11VideoProcessor   (BGRA→NV12, HEVC/AV1)
                                             ──▶ MFCreateDXGISurfaceBuffer
                                             ──▶ IMFSinkWriter (picks encoder, then encodes)
                                                   │  H.264 ──▶ MF's fragmented sink
                                                   │  HEVC  ──▶ our IMFMediaSink
                                                   ▼              │
                                             fMP4 ◀───────────────┘
                                             ──▶ soft remux at stop ──▶ MP4
                 └───────────────────────────────────────────────────────────────────────┘

                 ┌────────────────────────── CPU fallback path ──────────────────────────┐
 IDXGIOutputDuplication ──▶ staging texture ──Map──▶ BGRA ──▶ ring[4]   (drop on full)
                                                          ──▶ BGRA→I420 (BT.709)
                                                          ──▶ OpenH264 ──▶ packets[120]
                                                          ──▶ Hybrid MP4 muxer
                 └───────────────────────────────────────────────────────────────────────┘

                 stats thread: 200 ms sample of lock-free counters → RecorderStats snapshot
```

The on-disk shape, before and after `stop`:

```
CPU path             recording:  ftyp │ free(16B) │ moov(init) │ moof mdat │ moof mdat │ ...
GPU H.264            recording:  ftyp │ uuid │ pdin │ moov(init) │ moof mdat │ ... │ mfra
                               finalized:  ftyp │ mdat (one huge box swallowing everything above) │ moov (full index)
GPU HEVC             recording:  ftyp │ free(16B) │ moov(init) │ moof mdat │ moof mdat │ ...
                               finalized:  ftyp │ mdat (same as above) │ moov (same as above)
GPU AV1              recording:  ftyp │ uuid │ mdat (still growing, no index yet)
                               finalized:  ftyp │ uuid │ mdat │ moov
```

While recording, the first three shapes are valid fragmented MP4s that any player can open and
that `recorder_extract_range` can slice. At stop, a complete `moov` is appended and the 16 bytes right after `ftyp` are rewritten into
a 64-bit `mdat` header that spans up to it — so readers only ever see the classic three-box
layout. This is the same approach as OBS Studio's Hybrid MP4 output (30.2+).

The only difference is *who writes the boxes*: the CPU path uses our own `Mp4Muxer`, GPU H.264
uses Media Foundation's fragmented sink, and GPU HEVC uses the `IMFMediaSink` implemented here
(MF only picks the encoder and encodes; the bitstream comes back to us). All three finalize the
same way and produce the same structure.

GPU AV1 is the last shape: its samples are OBUs rather than Annex B and its config record differs,
so the hand-written muxer does not support it yet and it lands as a plain MP4 — `moov` only
appears at stop, so it is neither crash-safe nor sliceable mid-recording. That trade-off is
logged when it happens.

The CPU and GPU HEVC paths already hold every sample in memory, so they write the index directly.
The GPU H.264 boxes belong to Media Foundation, so `mux::remux` re-reads the fragments, keeps `stsd`
(the codec config) byte-for-byte, rebuilds `stts`/`stss`/`stsc`/`stsz`/`co64` from the `moof`
tables, and drops `mvex`. The chunk offsets are the untouched absolute offsets of the samples —
nothing is copied.

Repository layout:

```
src/
├─ lib.rs            C ABI exports + panic guard
├─ ffi.rs            C string helpers, silent panic hook
├─ config.rs         RecorderConfig validation (abi version, fps, bitrate, region)
├─ error.rs          error codes, mirrored 1:1 with recorder.h
├─ stats.rs          atomic counters + sliding-window sampler
├─ recorder.rs       state machine + thread pipeline + GPU/CPU dispatch
├─ capture/dxgi.rs   Desktop Duplication, staging texture, QPC pacing
├─ capture/gdi.rs    one-shot BitBlt, first frame for static screens only
├─ encode/mf_sink.rs Media Foundation SinkWriter, D3D11 video processor, encoder probing
├─ encode/sink_raw.rs custom IMFMediaSink (MF encodes, we write the boxes)
├─ encode/soft.rs    OpenH264 wrapper + Annex-B NAL handling
├─ convert/i420.rs   integer BT.709 BGRA → I420, 2×2 chroma subsampling
├─ mux/nal.rs        Annex B ⇄ length prefixes, avcC / hvcC construction
├─ mux/mp4.rs        Hybrid MP4 muxer (fMP4 while recording, plain MP4 after stop)
├─ mux/fragment.rs   fragment/box scanner shared by both writers
├─ mux/remux.rs      fMP4 → plain MP4 for Media Foundation's output
└─ mux/extract.rs    range extraction from a live recording → standalone plain MP4
include/recorder.h   C ABI, hand-maintained reference for the Rust side
```

## Performance

BGRA → I420 conversion, single-threaded scalar path (this is what the software fallback pays per frame), measured with `cargo test --release` on a Ryzen 5 5600GT:

| Resolution | Time per frame | Core occupancy @ 30 fps |
| --- | --- | --- |
| 1920 × 1080 | 4.01 ms | 12.0 % |
| 2560 × 1440 | 7.80 ms | 23.4 % |
| 3840 × 2160 | 16.20 ms | 48.6 % |

The GPU path skips this entirely. Numbers include three per-frame `Vec` allocations; a buffer pool and SIMD would cut them substantially. Reproduce with any harness that loops `convert::i420::bgra_to_i420` over a synthetic frame.

## Design notes

- **`panic = "unwind"` is load-bearing.** Setting `panic = "abort"` would make `catch_unwind` abort the host process on the first panic at the FFI boundary.
- **The handle is a `Mutex<Recorder>`.** The host is allowed to poll `get_stats` from the UI thread while another thread calls `stop`; handing out `&mut Recorder` from a raw pointer would be undefined behaviour.
- **`co64` *and* a 64-bit `mdat`.** 32-bit chunk offsets die at 4 GiB, and so does a 32-bit box size — offsets alone are not enough. `mdat` uses `size == 1` + largesize (ISO/IEC 14496-12 §4.2), verified to be read back correctly by Media Foundation's own MP4 parser.
- **`mdhd` is version 1.** With a microsecond timescale, a 32-bit duration caps correct reporting at 71 minutes; streaming a two-hour session would wrap the duration field.
- **Sample timestamps come from QPC, never from a frame counter.** Generating `frame_count × frame_interval` compresses the timeline whenever the pipeline falls behind (a 60 s capture plays in 20 s), and guarantees A/V drift once audio lands.
- **The reservation trick was measured and dropped.** An earlier revision reserved ~1.7 MB of `free` space at the head of every file so `moov` could be written into it at stop time (faststart without moving data). That is a fixed cost on *every* recording: a 5-second 1080p clip grew by 46 %, while the `moov` it was making room for is only ~4 KB. The 16-byte `free` box now exists only as a placeholder to be rewritten into an `mdat` header.
- **faststart is deliberately not implemented.** `moov` ends up after `mdat` in the finalized file. That costs nothing for local playback (every player seeks to the tail), nothing for upload-and-transcode platforms (YouTube/Bilibili parse the whole file server-side), and only matters for progressive web playback — which is better served by a one-off `ffmpeg -movflags +faststart` pass than by a per-file reservation.
- **Failing to finalize must not destroy footage.** If `moov` cannot be written, the recording is renamed to `<name>.mp4.partial` instead of being deleted. Since the file is still a valid fragmented MP4 up to the last complete fragment, it plays as-is and can be remuxed with `ffmpeg -i x.mp4.partial -c copy fixed.mp4` — no `untrunc` reference clip required.
- **One chunk per fragment, not one per sample.** Fragmentation breaks sample contiguity, so the old single-chunk/single-`co64` scheme no longer held. Per-sample chunks would cost 24 B of `moov` per sample (~2.6 MB/hour at 30 fps); per-fragment chunks cost 8 B per fragment plus a run-length-merged `stsc`, which is ~36 KB/hour.
- **Colour is tagged, not guessed.** Both paths target BT.709 / limited range; the software path writes a `colr` (`nclx`) box and sets the SPS VUI to match, so the container's claim and the pixel data agree.
- **Bitrate control must be explicit.** `openh264`'s `EncoderConfig` defaults to `RateControlMode::Quality`, where the target bitrate is silently ignored. The software path selects `Bitrate` + `ScreenContentRealTime`, and forces a 2-second GOP so the seek bar actually works on long recordings.
- **The first frame after `DuplicateOutput` is garbage.** It arrives fully transparent/black. Accepting it as the first frame makes a static-screen recording stay black forever, because every subsequent cycle re-sends that frame — so `capture` discards it and lets the GDI fallback supply real pixels.
- **`MFCreateDXGISurfaceBuffer` over CPU upload.** For HEVC/AV1 the SinkWriter is told the input is already NV12, which prevents Media Foundation from silently inserting a software colour converter (a classic deadlock source on some drivers).
- **`MFTranscodeContainerType_FMPEG4`, not just a fragment duration.** Setting `MF_MPEG4SINK_MIN_FRAGMENT_DURATION` on its own produces no fragmentation at all — MF still writes `ftyp/uuid/mdat/moov`. The container type is what switches the sink into fragment mode.
- **MF's own fragmented MP4 sink is H.264-only — the limit is real, but only in that layer.** Asking for `MFTranscodeContainerType_FMPEG4` with a HEVC output type makes `BeginWriting()` fail with `MF_E_INVALIDMEDIATYPE`. It is not a resolution, profile, pixel-format or bitrate problem, and it reproduces with a bare `IMFSinkWriter` (no DXGI device manager), so it is the sink, not this code: resolution from 320×240 to 1080p, NV12 or ARGB32 input, 3–8 Mbps — all fail; identical settings with H.264 succeed. Before this was understood, the failed attempt was swallowed by the fallback chain and `encoder = HEVC` silently produced H.264.
- **So we took the box-writing back.** A custom `IMFMediaSink` (`encode/sink_raw.rs`) is attached through `MFCreateSinkWriterFromMediaSink`: picking the encoder, negotiating media types and driving the hardware MFT still belong to `IMFSinkWriter`, and only the encoded bitstream is handed to our own `Mp4Muxer` via `ProcessSample`. HEVC therefore gets the fragmented state too — Hybrid is a **container strategy** that never had anything to do with the codec; what blocked it was one sink implementation. The ladder is now `HEVC/own-fragment → H.264/fragment → plain MP4 → software`, with the cost logged when the plain container is picked (no crash safety, no mid-recording slicing). AV1 still lands as a plain MP4: its samples are OBUs rather than Annex B and `av1C` is shaped differently, so that is not implemented yet.
- **The price is doing the step MF used to hide.** MF's encoder emits **Annex B** (start codes, parameter sets inline in the stream), while MP4 wants length prefixes plus an `avcC`/`hvcC` config box. `mux/nal.rs` does that: strip the parameter sets, convert to length prefixes, build `hvcC` from the SPS `profile_tier_level`. One trap — HEVC SPS carries emulation-prevention bytes (`00 00 03`), and unless those are removed first `general_level_idc` reads as `0x00` instead of `0x5A`. The `MFVideoFormat_H264_ES` / `HEVC_ES` route was checked and does not work either: `SetInputMediaType` returns `MF_E_TRANSFORM_NOT_FOUND`, because the SinkWriter input is NV12 and a colour-conversion MFT would have to sit in between — so this conversion has to be ours.
- **A custom sink has to honour SinkWriter's implicit contract.** The documentation does not spell this out; it was all found by trial. `GetStreamSinkCount()` must initially return 0 (returning 1 makes it look the stream up by the old number in `BeginWriting` and fail with `MF_E_INVALIDSTREAMNUMBER`); `AddStreamSink` must actually create a new stream whose `GetIdentifier` matches the identifier passed in; each stream must be a *separate* COM object (sharing one queues `BeginGetEvent` twice on the same event queue → `MF_E_MULTIPLE_BEGIN`); `GetMediaTypeCount()` must not be 0 (otherwise it dies with `MF_E_NO_MORE_TYPES`); `AddStreamSink` has to queue `MEStreamSinkStarted` and, in pull mode, `MEStreamSinkRequestSample`, or samples are accepted but the encoder never pumps; and the event queued from `PlaceMarker` must carry `pvarContextValue` as its value rather than `pvarMarkerValue`, or `Finalize()` returns `MF_E_INVALIDREQUEST`. The full checklist lives in the module docs of `encode/sink_raw.rs`.
- **Range extraction is codec-agnostic because it copies `stsd` instead of rebuilding it.** An earlier version re-muxed every sample through the hand-written H.264 writer — which is why HEVC/AV1 could not be sliced at all. Now the output's sample description is a byte-for-byte copy of the source's (`avcC` / `hvcC` / `av1C`), the media bytes are copied as one contiguous run, and only the `co64` offsets are shifted. Nothing in the index is codec-specific, and the NAL length-prefix size is never assumed to be 4.

## Building and testing

```bash
cargo test                    # 79 tests: ABI guards, state machine, NAL splitting,
                              # BT.709 math, bitrate control, MP4 box tree
                              # (Hybrid layout, fMP4 recording state, per-fragment
                              # chunks, 64-bit mdat / mdhd v1), Annex B → length
                              # prefixes, avcC / hvcC construction (including HEVC
                              # emulation-prevention stripping), fMP4→plain remux of a
                              # foreign writer's fragments, codec-agnostic range
                              # extraction (HEVC/AV1 tagged fixtures), real
                              # encode→mux pipeline (3 codecs × 3 containers, output
                              # read back with MF's own parser)
cargo clippy --all-targets    # expected: zero warnings
cargo fmt --all -- --check

# Needs a live desktop session (+ a GPU encoder for the GPU ones).
# Run serial: parallel cases fight over the same D3D device.
cargo test --test e2e_record -- --ignored --nocapture --test-threads=1
cargo test --lib -- --ignored --nocapture   # real SinkWriter: every codec × container
```

`pipeline_encodes_synthetic_frames_into_valid_mp4` drives the real encoder and the real muxer with synthetic frames and validates the resulting box tree — deterministic, no desktop session needed.

`tests/e2e_record.rs` records 2 seconds of the actual screen and reads the file back with Media Foundation's own MP4 parser, checking frame count, dimensions and timeline length. It is `#[ignore]`d because it needs a desktop, but it is the only test that proves the container is acceptable to a *real* demuxer. Several more cases cover the GPU writer: `e2e_gpu_recording_finalizes_to_plain_mp4` asserts the container collapses to `ftyp`/`mdat`/`moov` and that Media Foundation accepts the rebuilt index; `e2e_slice_gpu_recording_while_running` cuts a range **while the file is still being written** and then checks both the slice and the untouched recording (AV1 is the only exception: it does refuse, and the assertion pins down that the reason is stated clearly, so the case never silently skips); `e2e_hevc_records_when_explicitly_requested` proves HEVC goes through the custom sink — the file already contains `moof` with a `mvex`-bearing `moov` **while recording**, and after stop the `moov` no longer has `mvex` while `hvc1` really is present, rather than a silent H.264 downgrade. `e2e_slice_from_recording_is_playable` does the CPU-path equivalent.

## Known limitations

- The mouse cursor is **not** composited into the recording. `capture_cursor` is accepted and ignored.
- Audio is not implemented; `audio_source != 0` is rejected at `create`.
- HDR desktops are captured as SDR BGRA8 and will look flattened.
- **The finalized file is not faststart** (`moov` sits after `mdat`). Irrelevant for local playback and for upload-then-transcode sites; if you need progressive web playback, run `ffmpeg -i in.mp4 -c copy -movflags +faststart out.mp4` once afterwards.
- **Range extraction needs a *fragmented* file, not a particular codec.** The sample description is copied verbatim and the index carries no codec-specific fields, so codec choice never enters the picture. What it does need is the fragmented recording state (`moof` boxes carrying their own sample tables). Software encode, GPU H.264 and GPU HEVC all record that way, so they can be sliced **while recording**. Only GPU AV1 cannot — it falls back to a plain MP4 with `moov` written at stop, and `recorder_extract_range` returns a clear `REC_ERR_IO` explaining that instead of emitting garbage.
- Range extraction granularity is bounded by the keyframe interval: a cut starts at the nearest preceding keyframe (2 s GOP with the software encoder) and the end is trimmed exactly to the sample. It aligns to a keyframe **sample**, not a fragment, because Media Foundation's fragments are time-based and often carry a keyframe in the middle.
- Writing an MP4 larger than 4 GiB relies on the ISO 14496-12 `largesize` form; ancient/partial parsers that reject it will not open such a file.
- `encoder` values are preference hints. The actual choice depends on which hardware MFTs the driver exposes.
- No pause/resume, no region change while recording.

## Roadmap

1. Cursor compositing from `DXGI_OUTDUPL_FRAME_INFO.PointerPosition`.
2. WASAPI loopback audio + AAC track in the MP4, QPC-aligned with video.
3. Buffer pooling and SIMD in the conversion path (4K currently costs ~half a core).
4. Pause/resume and hot region change while recording.
5. Fragmented muxing for AV1 (`av1C` + OBU samples), to bring crash safety and
   mid-recording slicing to the last remaining codec.

## License

MIT — see [LICENSE](LICENSE).

Third-party: [OpenH264](https://github.com/cisco/openh264) (BSD-2-Clause), compiled from source and linked statically through the `openh264` crate. Note that Cisco's royalty-free patent policy applies to binaries distributed *by Cisco*; if you redistribute a build that embeds OpenH264, review your own patent position.
