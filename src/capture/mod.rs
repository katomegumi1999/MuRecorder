//! 视频捕获模块。M1：DXGI Desktop Duplication（按输出/显示器整屏捕获，裁剪到 region）。
//!
//! `gdi` 子模块只做「静止画面补首帧」的一次性兜底，不属于常规采集路径。

pub mod dxgi;
pub mod gdi;

pub use dxgi::{CaptureTarget, CapturedFrame, DxgiCapture};
