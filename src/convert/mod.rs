//! 色彩空间转换模块。M1：CPU 路径 BGRA→I420（供软件编码器）。
//! M3 起将追加 GPU 路径（D3D11 VideoProcessorBlt / CS，BGRA→NV12）。

pub mod i420;
