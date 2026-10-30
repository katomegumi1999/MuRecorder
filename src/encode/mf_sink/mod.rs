//! Media Foundation SinkWriter GPU 零拷贝硬编模块。
//!
//! 流程：
//! 1. 初始化 COM 与 MF 运行时 (CoInitializeEx / MFStartup)。
//! 2. D3D11 设备创建时开启 `D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT`。
//! 3. 通过 `IMFDXGIDeviceManager` 将显卡上下文绑定到 `IMFSinkWriter`，并启用 `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS`。
//! 4. 逐帧截取后在 GPU 内完成裁剪 (`CopySubresourceRegion`)，
//!    调用 `MFCreateDXGISurfaceBuffer` 直接包装显存纹理句柄，通过 `IMFSinkWriter::WriteSample` 直送硬件编码器。
//!
//! ## 指针与特效合成为什么是这样做的
//!
//! 桌面复制的桌面图像里没有鼠标指针，必须自己画（详见 [`crate::capture::cursor`]）。
//! 合成点在**裁剪后的 BGRA 纹理**上、`BGRA→NV12` 之前，这样 H.264（ARGB32 输入）
//! 与 HEVC/AV1（NV12 输入）两条路走的是同一份像素。
//!
//! 三个关键决定：
//!
//! 1. **两份纹理。** `desktop_texture` 存「纯桌面」，`cropped_texture` 存
//!    「桌面 + 指针 + 特效」。没有前者的话，指针移动时新指针会画在旧指针上，
//!    鼠标划过的轨迹会留下一串擦不掉的残影；移动轨迹特效也正好需要「每一帧都从
//!    干净的桌面重新叠」才谈得上残影按时间淡出。
//! 2. **只回读叠加区域的包围盒，不整帧回读。** 整帧回读会让 4K 每帧多走 33 MB
//!    PCIe（等于把零拷贝的收益全吐回去）。只有指针时包围盒通常 32×32～64×64；
//!    带轨迹时上限由 [`crate::capture::overlay`] 控制。
//! 3. **像素逻辑复用 CPU 那一份**（[`crate::capture::CursorShape::blend_into`] 与
//!    [`crate::capture::overlay::OverlayPlan::draw_into`]），不在 GPU 侧另写一套。
//!    单色光标的「反色」分支必须读到桌面像素才能算，用着色器实现得再做一次全屏采样；
//!    而包围盒回读天然把桌面像素摆在手边。代价是每帧一次小 Map（约 0.1–0.3 ms），
//!    换来的是 CPU/GPU 两条路不可能出现像素分歧。
//!
//! 子模块划分：
//! - [`converter`]：MF 运行时守卫、D3D11 VideoProcessor（BGRA→NV12）、纹理助手。
//! - [`probe`]：硬件 MFT 编码器探测。
//! - [`types`]：编码器与容器模式的类型定义、`stsd` 样本描述构建。
//! - [`writer`]：`GpuSinkWriter` 本体（线程入口的全部公开接口）。

mod converter;
mod probe;
mod types;
mod writer;

use windows::Win32::Media::MediaFoundation::IMFMediaType;
use windows::core::Result as WinResult;

pub use converter::{D3D11VideoConverter, MfRuntimeGuard};
pub use probe::has_hardware_encoder;
pub use types::{ContainerMode, GpuCodec};
pub use writer::GpuSinkWriter;

/// 分片目标时长（100ns 单位，即 1 s）。
///
/// MF 的 MP4 sink 会在「累计到这个时长后的第一个关键帧」处切分，所以实际分片
/// 粒度仍由 GOP 决定；这个值只是「至少多久切一次」的下限。
pub(super) const FRAGMENT_DURATION_100NS: u64 = 10_000_000;

#[inline]
unsafe fn set_attribute_size(
    attrs: &IMFMediaType,
    key: &windows::core::GUID,
    width: u32,
    height: u32,
) -> WinResult<()> {
    unsafe { attrs.SetUINT64(key, ((width as u64) << 32) | (height as u64)) }
}

#[inline]
unsafe fn set_attribute_ratio(
    attrs: &IMFMediaType,
    key: &windows::core::GUID,
    numerator: u32,
    denominator: u32,
) -> WinResult<()> {
    unsafe { attrs.SetUINT64(key, ((numerator as u64) << 32) | (denominator as u64)) }
}
