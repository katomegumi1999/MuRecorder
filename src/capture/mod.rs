//! 视频捕获模块。DXGI Desktop Duplication（按输出/显示器整屏捕获，裁剪到 region）。
//!
//! - `dxgi`：桌面复制会话、帧与指针状态机、帧率配速。
//! - `cursor`：鼠标指针形状解析与合成（桌面图像本身**不含**指针，必须自己画）。
//! - `gdi`：只做「静止画面补首帧」的一次性兜底，不属于常规采集路径。
//! - `wasapi`：系统/进程环回音频采集。

pub mod cursor;
pub mod dxgi;
pub mod gdi;
pub mod wasapi;

pub use cursor::{CursorShape, PointerFrame, PointerState};
pub use dxgi::{CaptureOptions, CaptureTarget, CapturedFrame, DxgiCapture};
pub use wasapi::{AudioCapture, AudioSourceSpec, PcmPacket};
