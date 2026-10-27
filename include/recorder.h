/* recorder.h — MuRecorder C ABI（由 cbindgen 生成的手工对照版，字段以本文件为准）
 *
 * 消费者：ShawMuOCR (C# / Avalonia)，通过 [LibraryImport] 绑定。
 * 生产者：recorder_core.dll (Rust cdylib)。
 *
 * ABI 约束（评审清单，任何改动必须同步本节）：
 *   1. recorder_create 只分配、不启动；线程在 recorder_start 中创建。
 *   2. recorder_stop / recorder_destroy 均幂等；destroy 内部先 stop 再 join。
 *   3. 所有导出函数用 catch_unwind 包裹，panic 转为 REC_ERR_PANIC，
 *      绝不让 panic 展开到 C# 宿主进程。
 *   4. 字符串参数 Rust 只读、不持有，调用返回后 C# 可立即释放。
 *   5. 结构体首字段固定为 abi_version，后续新增字段只能尾部追加。
 *   6. extern "C"，Windows x64 下即统一 ABI（Microsoft x64 ABI）。
 *   7. 句柄内部是 Mutex<Recorder>：可从任意线程并发调用（UI 线程轮询 stats 的同时
 *      另一线程 stop 是合法用法）。stop 会阻塞到索引写盘完成，拿锁期间 stats 会等待。
 */
#ifndef MURECORDER_RECORDER_H
#define MURECORDER_RECORDER_H

#include <stdint.h>

#define RECORDER_ABI_VERSION 1

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RecorderHandle RecorderHandle;

typedef enum {
    REC_OK              = 0,
    REC_ERR_INVALID     = 1,  /* 参数非法 / 空指针 / 版本不匹配 */
    REC_ERR_NO_ENCODER  = 2,  /* 探测不到可用编码器 */
    REC_ERR_DEVICE_LOST = 3,  /* DXGI 设备丢失且重建失败 */
    REC_ERR_IO          = 4,  /* 文件打开 / 写入失败 */
    REC_ERR_STATE       = 5,  /* 非法状态转换，如重复 start */
    REC_ERR_PANIC       = 6,  /* Rust 内 panic 被捕获，详见 last_error */
} RecorderResult;

typedef enum {
    AUDIO_NONE   = 0,
    AUDIO_SYSTEM = 1,  /* 默认输出设备 loopback */
    AUDIO_DEVICE = 2,  /* 指定渲染设备 loopback（audio_device_id） */
    AUDIO_WINDOW = 3,  /* 进程环回（audio_window_hwnd / audio_process_id） */
} AudioSourceType;

typedef struct {
    uint32_t abi_version;        /* 必须填 RECORDER_ABI_VERSION */
    int32_t  region_x, region_y; /* 物理像素，虚拟桌面坐标系 */
    uint32_t region_w, region_h;
    uint32_t fps;
    uint32_t encoder;            /* 0=auto, 1=gpu_hevc, 2=gpu_h264, 3=amf, 4=x264(soft), 5=gpu_av1 */
    uint32_t bitrate_kbps;
    uint32_t capture_cursor;     /* 0/1，M1 忽略 */
    const char* output_path;     /* UTF-8，NUL 结尾；Rust 只读不持有 */
    uint32_t audio_source_type;  /* AudioSourceType */
    const char* audio_device_id; /* UTF-8，可空；AUDIO_DEVICE 时必填 */
    uint64_t audio_window_hwnd;  /* AUDIO_WINDOW 时使用 */
    uint32_t audio_process_id;   /* AUDIO_WINDOW 时使用（0 表示仅按 HWND 解析） */
} RecorderConfig;

typedef struct {
    uint64_t frames_captured;
    uint64_t frames_encoded;
    uint64_t frames_dropped;
    uint32_t current_fps;        /* 滑动窗口，由 stats 线程每 200ms 计算 */
    uint32_t bitrate_kbps;       /* 滑动窗口瞬时码率 */
    uint32_t queue_depth;        /* 编码队列积压（视频 ring 待编码帧数） */
    uint32_t encoder_type;       /* 实际选定的编码器：1=GPU HEVC, 2=GPU H.264, 4=Soft(OpenH264), 5=GPU AV1 */
    int32_t  last_error;         /* RecorderResult，0 表示无错误 */
    uint64_t elapsed_ms;
} RecorderStats;

/* 生命周期与状态机：Idle --create--> Created --start--> Running --stop--> Stopped */
int32_t recorder_create(const RecorderConfig* cfg, RecorderHandle** out);
int32_t recorder_start(RecorderHandle* h);
int32_t recorder_stop(RecorderHandle* h);           /* 幂等，阻塞到线程退出 */
int32_t recorder_get_stats(RecorderHandle* h, RecorderStats* out); /* 无锁快照 */
int32_t recorder_get_last_error(RecorderHandle* h, char* buf, uint32_t len);
void    recorder_destroy(RecorderHandle* h);        /* 内部先 stop */
const char* recorder_version(void);                 /* 静态字符串，禁止 free */
int32_t recorder_has_hardware_encoder(uint32_t encoder); /* 探测指定编码器是否有硬件支持 */

/* 从**录制中**的文件里切出 [start_us, end_us)，写成独立的普通 MP4（可播放、可上传）。
 * 不停止录制、不重编码，纯文件 I/O；大区间会阻塞几百毫秒，别在关键 UI 帧里调用。
 * 与编码器无关：样本描述原样照抄、不重建参数集，所以这条路径不看编码器。
 * 前提是文件处于**分片式**录制态：软编、GPU H.264、GPU HEVC 都这么写，随时可切；
 * 只有 GPU AV1 会落盘普通 MP4（自研封装暂不支持 OBU 样本），
 * 那种文件要等停止录制后才能处理（本函数返回 REC_ERR_IO 并说明原因）。
 * 起点对齐到覆盖 start_us 的最近关键帧，终点精确裁到 end_us 之内；
 * 实际时长由 out_duration_us 回传（可为 null）。全程约几十毫秒到几百毫秒。
 * 对已完成封装的文件（没有活跃录制）会返回 REC_ERR_IO 并提示改用 ffmpeg。 */
int32_t recorder_extract_range(const char* src_path, const char* dst_path,
                               uint64_t start_us, uint64_t end_us,
                               uint32_t fps, uint64_t* out_duration_us);

#ifdef __cplusplus
}
#endif

#endif /* MURECORDER_RECORDER_H */
