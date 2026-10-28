# MuRecorder C# (.NET / Avalonia) 调用指南

本文档为 C# / .NET 开发者（特别是 Avalonia、WPF、WinUI 桌面应用）提供调用 `murecorder_core.dll` 的完整规范、原生 P/Invoke 绑定代码以及端到端使用示例。

---

## 1. 核心特性与环境要求

- **音画规格**：
  - **视频**：最高 4K 60FPS，GPU 零拷贝硬件编码（HEVC / H.264 / AV1）或 CPU 纯软回退（OpenH264）。
  - **音频**：**AAC-LC 320 kbps（广播母带级上限） / 48 kHz / 立体声**，WASAPI Loopback 默认设备、指定设备或进程隔离。
  - **容器**：Hybrid MP4，录制中为分片抗崩溃结构，停止后秒级转为普通 MP4；支持录制中无损提取切片。
- **环境**：
  - Windows 10 1809+ / Windows 11 (x64)
  - .NET 7.0 / 8.0 / 9.0+（推荐使用现代 `[LibraryImport]` 源生成器，支持 Native AOT）
  - 将编译生成的 `murecorder_core.dll` 放置在与 `.exe` 同级目录，或 `runtimes/win-x64/native/` 目录下。

---

## 2. 原生类型与 P/Invoke 接口定义

在项目中新建 `MuRecorderNative.cs`，直接使用以下结构体与接口定义：

```csharp
using System;
using System.Runtime.InteropServices;

namespace MuRecorder.Interop;

/// <summary>
/// 函数执行结果状态码
/// </summary>
public enum RecorderResult : int
{
    Ok = 0,
    ErrInvalid = 1,     // 参数非法 / 空指针 / 版本不匹配
    ErrNoEncoder = 2,   // 未探测到可用的硬件或软件编码器
    ErrDeviceLost = 3,  // DXGI 采集设备丢失且重建失败（如显卡驱动重置）
    ErrIo = 4,          // 磁盘文件打开 / 写入失败
    ErrState = 5,       // 非法状态转换（例如尚未 create 就 start，或重复 start）
    ErrPanic = 6        // Rust 内部 panic（已安全拦截，详见 last_error）
}

/// <summary>
/// 音频捕获来源
/// </summary>
public enum AudioSourceType : uint
{
    None = 0,    // 不录制音频（生成单视频轨 MP4）
    System = 1,  // 默认音频渲染设备 Loopback（录制系统全部声音）
    Device = 2,  // 指定设备 Endpoint ID 录制（需配置 AudioDeviceId）
    Window = 3   // 进程/窗口声音环回隔离（仅录制指定窗口/进程的声音，需 Windows 10 2004+）
}

/// <summary>
/// 编码器偏好策略
/// </summary>
public enum EncoderPreference : uint
{
    Auto = 0,     // 自动策略：优先 GPU HEVC/分片 -> GPU H.264/分片 -> GPU AV1 -> CPU OpenH264
    GpuHevc = 1,  // 强制首选 GPU HEVC（自研分片 IMFMediaSink，抗崩溃）
    GpuH264 = 2,  // 强制首选 GPU H.264（系统分片 MP4 SinkWriter）
    Amf = 3,      // AMD 硬件编码首选（保留别名）
    Soft = 4,     // 强制走纯 CPU 软编（OpenH264，BT.709）
    GpuAv1 = 5    // 强制首选 GPU AV1
}

/// <summary>
/// 录制启动配置（与 C ABI 严格内存对齐）
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct RecorderConfig
{
    public uint AbiVersion;          // 固定填 2（RECORDER_ABI_VERSION）；填 1 等旧值会被明确拒绝
    public int RegionX;              // 虚拟屏幕物理像素坐标 X
    public int RegionY;              // 虚拟屏幕物理像素坐标 Y
    public uint RegionW;             // 录制区域物理像素宽度（建议为偶数）
    public uint RegionH;             // 录制区域物理像素高度（建议为偶数）
    public uint Fps;                 // 目标帧率（1 ~ 120，推荐 30 或 60）
    public uint Encoder;             // 见 EncoderPreference
    public uint BitrateKbps;         // 视频目标码率（kbps，0 则引擎自动计算默认值）
    public uint CaptureCursor;       // 是否捕获鼠标（0=否，1=是）
    
    [MarshalAs(UnmanagedType.LPUTF8Str)]
    public string OutputPath;        // 绝对路径，UTF-8，必须有写入权限且所在文件夹存在

    public uint AudioSourceType;     // 见 AudioSourceType (0=无, 1=系统, 2=设备, 3=窗口/进程)
    
    [MarshalAs(UnmanagedType.LPUTF8Str)]
    public string? AudioDeviceId;    // 当 AudioSourceType == Device 时必填；其余传 null
    
    public ulong AudioWindowHwnd;    // 当 AudioSourceType == Window 时传目标窗口句柄 (HWND)
    public uint AudioProcessId;      // 当 AudioSourceType == Window 时传 PID（传 0 则自动按 HWND 查找）
    public uint AudioBitrateKbps;    // 音频码率（kbps，传 0 默认 320；支持 96, 128, 160, 192, 256, 320）
}

/// <summary>
/// 运行时实时性能快照（无锁采集，耗时微秒级，供 UI 仪表盘高频轮询）
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct RecorderStats
{
    public ulong FramesCaptured;     // DXGI 实际捕获帧数（静态画面会自动维持节奏）
    public ulong FramesEncoded;      // 实际送入编码器的帧数
    public ulong FramesDropped;      // 环形缓冲积压时丢弃的帧数（正常为 0）
    public uint CurrentFps;          // 最近 200ms 滑动窗口瞬时帧率
    public uint BitrateKbps;         // 最近 200ms 滑动窗口瞬时视频码率
    public uint QueueDepth;          // 当前待编码的积压帧数
    public uint EncoderType;         // 实际生效的编码器（1=GPU HEVC, 2=GPU H.264, 4=Soft OpenH264, 5=GPU AV1）
    public int LastError;            // RecorderResult 状态码，0 表示运行正常
    public ulong ElapsedMs;          // 已录制时长（毫秒）
}

/// <summary>
/// 原生 C 导出函数
/// </summary>
public static partial class MuRecorderNative
{
    private const string DllName = "murecorder_core";

    public const uint RecorderAbiVersion = 2;

    [LibraryImport(DllName, EntryPoint = "recorder_create")]
    public static partial int Create(in RecorderConfig config, out IntPtr handle);

    [LibraryImport(DllName, EntryPoint = "recorder_start")]
    public static partial int Start(IntPtr handle);

    [LibraryImport(DllName, EntryPoint = "recorder_stop")]
    public static partial int Stop(IntPtr handle);

    [LibraryImport(DllName, EntryPoint = "recorder_destroy")]
    public static partial void Destroy(IntPtr handle);

    [LibraryImport(DllName, EntryPoint = "recorder_get_stats")]
    public static partial int GetStats(IntPtr handle, out RecorderStats stats);

    [LibraryImport(DllName, EntryPoint = "recorder_get_last_error")]
    public static partial int GetLastError(IntPtr handle, IntPtr buffer, uint bufferLength);

    [LibraryImport(DllName, EntryPoint = "recorder_version")]
    public static partial IntPtr GetVersion();

    [LibraryImport(DllName, EntryPoint = "recorder_has_hardware_encoder")]
    public static partial int HasHardwareEncoder(uint encoder);

    [LibraryImport(DllName, EntryPoint = "recorder_extract_range", StringMarshalling = StringMarshalling.Utf8)]
    public static partial int ExtractRange(
        string srcPath,
        string dstPath,
        ulong startUs,
        ulong endUs,
        uint fps,
        out ulong outDurationUs);

    /// <summary>
    /// 便捷读取错误明细字符串
    /// </summary>
    public static string ReadLastError(IntPtr handle)
    {
        if (handle == IntPtr.Zero) return string.Empty;
        IntPtr buf = Marshal.AllocHGlobal(1024);
        try
        {
            int res = GetLastError(handle, buf, 1024);
            return res == 0 ? (Marshal.PtrToStringUTF8(buf) ?? string.Empty) : string.Empty;
        }
        finally
        {
            Marshal.FreeHGlobal(buf);
        }
    }
}
```

---

## 3. 面向对象的 C# 封装类（推荐业务层使用）

为了防止忘记调用 `destroy` 导致句柄泄漏，推荐将句柄封装为实现 `IDisposable` 的类：

```csharp
using System;
using System.IO;
using MuRecorder.Interop;

namespace MuRecorder;

public sealed class RecorderSession : IDisposable
{
    private IntPtr _handle;
    private bool _isDisposed;

    public bool IsRunning { get; private set; }

    private RecorderSession(IntPtr handle)
    {
        _handle = handle;
    }

    /// <summary>
    /// 创建录制会话（只分配资源，未开始录制）
    /// </summary>
    public static RecorderSession Create(RecorderConfig config)
    {
        config.AbiVersion = MuRecorderNative.RecorderAbiVersion;

        // 确保目标输出目录存在
        string? dir = Path.GetDirectoryName(config.OutputPath);
        if (!string.IsNullOrEmpty(dir) && !Directory.Exists(dir))
        {
            Directory.CreateDirectory(dir);
        }

        int ret = MuRecorderNative.Create(in config, out IntPtr handle);
        if (ret != (int)RecorderResult.Ok)
        {
            string detail = MuRecorderNative.ReadLastError(handle);
            MuRecorderNative.Destroy(handle);
            throw new InvalidOperationException($"创建录屏会话失败: ({(RecorderResult)ret}) {detail}");
        }

        return new RecorderSession(handle);
    }

    /// <summary>
    /// 开始录制（启动采集、编码、封装线程）
    /// </summary>
    public void Start()
    {
        ThrowIfDisposed();
        int ret = MuRecorderNative.Start(_handle);
        if (ret != (int)RecorderResult.Ok)
        {
            string detail = MuRecorderNative.ReadLastError(_handle);
            throw new InvalidOperationException($"启动录屏失败: ({(RecorderResult)ret}) {detail}");
        }
        IsRunning = true;
    }

    /// <summary>
    /// 停止录制（幂等，会同步阻塞直至 MP4 索引和文件头写盘完毕）
    /// </summary>
    public void Stop()
    {
        if (_handle == IntPtr.Zero || !IsRunning) return;

        int ret = MuRecorderNative.Stop(_handle);
        IsRunning = false;
        if (ret != (int)RecorderResult.Ok)
        {
            string detail = MuRecorderNative.ReadLastError(_handle);
            throw new IOException($"停止录屏并封装 MP4 失败: ({(RecorderResult)ret}) {detail}");
        }
    }

    /// <summary>
    /// 获取实时监控快照（无锁高频轮询，适合 UI 刷新）
    /// </summary>
    public RecorderStats GetStats()
    {
        ThrowIfDisposed();
        int ret = MuRecorderNative.GetStats(_handle, out RecorderStats stats);
        if (ret != (int)RecorderResult.Ok)
        {
            throw new InvalidOperationException($"获取 Stats 失败: ({(RecorderResult)ret})");
        }
        return stats;
    }

    /// <summary>
    /// 释放会话。内部会先自动 stop 再释放 Rust 句柄
    /// </summary>
    public void Dispose()
    {
        if (_isDisposed) return;
        if (_handle != IntPtr.Zero)
        {
            MuRecorderNative.Destroy(_handle);
            _handle = IntPtr.Zero;
        }
        IsRunning = false;
        _isDisposed = true;
        GC.SuppressFinalize(this);
    }

    private void ThrowIfDisposed()
    {
        if (_isDisposed || _handle == IntPtr.Zero)
            throw new ObjectDisposedException(nameof(RecorderSession));
    }

    ~RecorderSession() => Dispose();
}
```

---

## 4. 四种典型场景代码示例

### 场景 A：全屏录制 + 电脑声音（AAC 320kbps 极高音质）

这是最常见的录屏场景：全屏录制主显示器，同时录制扬声器里发出的所有声音（游戏声、会议声音、网页视频声）。

```csharp
using MuRecorder;
using MuRecorder.Interop;

// 1. 组装配置
var config = new RecorderConfig
{
    RegionX = 0,
    RegionY = 0,
    RegionW = 1920,
    RegionH = 1080,
    Fps = 60,
    Encoder = (uint)EncoderPreference.Auto, // 优先 GPU HEVC / H.264
    BitrateKbps = 8000,                    // 8 Mbps 视频码率
    CaptureCursor = 1,
    OutputPath = @"D:\Recordings\gameplay.mp4",
    
    // 关键：开启系统声音录制
    AudioSourceType = (uint)AudioSourceType.System,
    AudioDeviceId = null,
    AudioWindowHwnd = 0,
    AudioProcessId = 0,

    // 可选音频码率（kbps）：
    // 传 0 默认 320 kbps；也支持 96, 128, 160, 192, 256, 320
    AudioBitrateKbps = 320 // 或 128 / 192 / 256 等
};

// 2. 启动录制
using var session = RecorderSession.Create(config);
session.Start();
Console.WriteLine("正在录制中（带 320k 双声道高保真音频）...");

// 3. 模拟录制 10 秒并实时监控
for (int i = 0; i < 10; i++)
{
    Thread.Sleep(1000);
    var stats = session.GetStats();
    Console.WriteLine($"[Stats] 时长: {stats.ElapsedMs / 1000}s | 帧率: {stats.CurrentFps} FPS | 瞬时码率: {stats.BitrateKbps} kbps");
}

// 4. 停止录制并生成标准化 MP4
session.Stop();
Console.WriteLine("录制完成，MP4 已就绪！");
```

---

### 场景 B：进程隔离录音录屏（只录某个指定软件的声音）

比如你只想录制 Discord、网易云音乐或特定游戏的画面和声音，不希望把系统的微信提示音录进去：

```csharp
IntPtr targetHwnd = targetWindow.Handle; // 目标窗口句柄
uint targetPid = 12345;                 // 目标进程 PID

var config = new RecorderConfig
{
    RegionX = 100,
    RegionY = 100,
    RegionW = 1280,
    RegionH = 720,
    Fps = 30,
    Encoder = (uint)EncoderPreference.Auto,
    BitrateKbps = 4000,
    OutputPath = @"D:\Recordings\app_only.mp4",
    
    // 关键：使用进程隔离环回
    AudioSourceType = (uint)AudioSourceType.Window,
    AudioWindowHwnd = (ulong)targetHwnd,
    AudioProcessId = targetPid
};

using var session = RecorderSession.Create(config);
session.Start();
// ...
session.Stop();
```

---

### 场景 C：纯静音录屏（单视频轨）

如果不需要声音，只需将 `AudioSourceType` 设为 `AudioSourceType.None`（0）。产物将是完全干净的纯单视频轨 MP4。

```csharp
config.AudioSourceType = (uint)AudioSourceType.None;
```

---

### 场景 D：录制中途无损切片（提取高光时刻，零重编码）

如果用户在录制了 30 分钟的游戏，突然点击了“保存刚才 30 秒高光”：
你**不需要停止当前录制**，直接调用 `ExtractRange` 即可！

```csharp
// 截取第 10 秒到第 30 秒的内容（时间单位：微秒 us）
ulong startUs = 10_000_000;
ulong endUs = 30_000_000;

int ret = MuRecorderNative.ExtractRange(
    @"D:\Recordings\gameplay.mp4",         // 正在录制中的源文件路径
    @"D:\Recordings\highlight_clip.mp4",    // 切片输出路径
    startUs,
    endUs,
    60,                                    // 帧率
    out ulong actualDurationUs
);

if (ret == (int)RecorderResult.Ok)
{
    Console.WriteLine($"切片成功！实际切出时长: {actualDurationUs / 1000} ms，音画完全对齐！");
}
```

---

## 5. 常见问题排查与注意事项

1. **静止画面无数据吗？**
   - 不会。引擎内部检测到静态画面时会自动启动定时注入逻辑补全帧率，录制出来的 MP4 严格保持 CFR（恒定帧率），拖动进度条丝滑流畅。
2. **AAC 码率说明**：
   - 现版本已全面开放并默认配置为 **320 kbps**（Media Foundation 最高硬件保真限制，40,000 字节/秒），双声道 48kHz，听感达到全频段无损透明级。
3. **录制异常崩溃会不会导致文件损坏？**
   - 不会。在未调用 `Stop()` 前，底层文件始终保持为标准的 `Fragmented MP4`。若突然断电或强退，到上一个分片为止的音视频内容完全保留，且可被播放器直接播放。
4. **`RegionW` 和 `RegionH` 的奇偶限制**：
   - 宽度和高度建议为 **偶数**（能被 2 整除）。部分 GPU 编码器在遇到奇数分辨率时可能会自动向上做对齐填充。
