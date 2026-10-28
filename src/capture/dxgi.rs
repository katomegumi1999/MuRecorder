//! DXGI Desktop Duplication 捕获会话。
//!
//! - 按区域左上角所在显示器 `DuplicateOutput`；
//! - 软件编码路径：逐帧拷贝到 staging 纹理后 Map 到 CPU（[`DxgiCapture::acquire_frame`]）；
//! - 硬件编码路径：纹理在闭包里直送编码器，全程不出显存
//!   （[`DxgiCapture::acquire_texture`]）。
//!
//! 设备丢失（设计文档 4.3）：调用方检测到 `AccessLost` / `DeviceRemoved` 后进入重建循环
//! （sleep 500ms 重试 `duplicate_output`，最多 10 次），本模块提供 [`DxgiCapture::rebuild`]。
//!
//! ## 指针与特效通道
//!
//! 桌面复制交付的桌面图像里**不含鼠标指针**，指针走独立旁路
//! （`PointerPosition` + `GetFramePointerShape`）。本模块负责维护这份状态，
//! 连同「移动轨迹 / 点击波纹」这两个动态特效一起，产出每帧要叠加的内容
//! （[`OverlayPlan`]）：CPU 路径直接画在整帧上，GPU 路径交给调用方在显存内合成
//! （见 [`crate::encode::mf_sink`]）。
//!
//! 这里最容易搞错的几件事，都在 [`super::cursor`] 与 [`super::overlay`] 的模块注释里
//! 展开：**位置是左上角而非热点**、**不是每帧都带指针更新**（必须持久化上一状态）、
//! 以及**特效是时间驱动的动画**——它需要打破本模块「什么都没变就返回 `Ok(None)`」
//! 的快捷路径（见 [`DxgiCapture::overlay_animating`]）。

use std::sync::Arc;
use std::sync::OnceLock;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_MORE_DATA,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::{Error as WinError, Interface, Result as WinResult};

use crate::capture::cursor::{Canvas, CursorShape, PointerState};
use crate::capture::overlay::{CursorOverlay, OverlayConfig, OverlayPlan};

/// 采集选项。
///
/// 单独做一个结构体而不是往 `CaptureTarget` 里塞字段：`CaptureTarget` 描述的是
/// 「录哪块屏幕」，属于几何信息；这里描述的是「怎么录」，两者生命周期与来源都不同
/// （前者由区域推算，后者来自用户配置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaptureOptions {
    /// 是否把鼠标指针合成进画面（对应 ABI 的 `capture_cursor`）。
    ///
    /// 默认 `false`：合成指针要额外维护一份「纯桌面」副本并每帧回读叠加区域，
    /// 不该在用户没要求时默默付出这份开销。
    pub capture_cursor: bool,
    /// 指针之上的动态特效（移动轨迹 / 点击波纹）。未开 `capture_cursor` 时无意义。
    pub overlay: OverlayConfig,
}

impl CaptureOptions {
    pub fn with_cursor(capture_cursor: bool) -> Self {
        Self {
            capture_cursor,
            overlay: OverlayConfig::default(),
        }
    }

    /// 追加特效开关（对应 ABI 的 `cursor_click_effect` / `cursor_trail_ms`）。
    pub fn with_effects(mut self, click_effect: bool, trail_ms: u32) -> Self {
        self.overlay = OverlayConfig {
            click_effect,
            trail_ms,
        };
        self
    }
}

/// 显存采集时交给闭包的帧元信息。
///
/// `desktop_updated == false` 表示本次 `AcquireNextFrame` 只带来了指针变化
/// （`AccumulatedFrames == 0`）：显存里的桌面图像没变，下游若自带上一帧副本就不必重拷。
#[derive(Clone)]
pub struct FrameMeta<'a> {
    pub desktop_updated: bool,
    /// `None` 表示没有开启指针合成。`Some` 时即使 `is_empty()`（这一帧什么都没得画）
    /// 也代表「下游要用那份纯桌面副本来派生画面」，因为清掉上一帧的指针同样需要它。
    pub overlay: Option<OverlayPlan<'a>>,
}

/// 一帧捕获结果：CPU 侧 BGRA 数据（已裁剪到 region）。
///
/// 用 `Arc<[u8]>` 而不是 `Vec<u8>`：静止画面时需要把「上一帧」再投递一次，
/// Vec 的 clone 是每帧 8 MB 级别的整块 memcpy（1080p），Arc 只加一次引用计数。
#[derive(Clone)]
pub struct CapturedFrame {
    /// BGRA8888，长度 = `stride * height`，行距 `stride`（紧凑行距）
    pub bgra: Arc<[u8]>,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
    /// QPC 计时戳（捕获完成时刻）
    pub qpc: u64,
}

/// 捕获目标：区域左上角所在的输出（显示器）及其桌面坐标。
pub struct CaptureTarget {
    pub adapter_index: u32,
    pub output_index: u32,
    /// 输出在虚拟桌面中的矩形（物理像素）
    pub output_rect: RECT,
    /// region 相对于输出左上角的偏移
    pub offset_x: i32,
    pub offset_y: i32,
    pub width: u32,
    pub height: u32,
}

/// DXGI 桌面复制会话。非 Clone；`rebuild` 消费并返回自身以保持所有权清晰。
pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    output_rect: RECT,
    offset_x: i32,
    offset_y: i32,
    width: u32,
    height: u32,
    /// `DuplicateOutput` 之后的第一帧是未初始化的桌面图像（实测整帧全 0）。
    ///
    /// 若把它当首帧收下，静止画面场景会一路重发这张黑帧，整段录制全黑——正对着
    /// 「录副屏静态文档」这个最需要兜底的用例。因此建立会话/重建后丢弃第一帧。
    discard_next_frame: bool,
    /// 是否合成鼠标指针。
    capture_cursor: bool,
    /// 指针的当前位置/可见性（区域坐标系）。
    ///
    /// **必须持久化**：`DXGI_OUTDUPL_FRAME_INFO.PointerPosition` 只在
    /// `LastMouseUpdateTime != 0` 的帧里有效，其余帧该字段是全 0。每帧无脑采信的话，
    /// 鼠标一停下指针就会跳到区域左上角然后消失。
    pointer: PointerState,
    /// 最近一次拿到的形状。形状变化很稀疏（换光标时才变），每帧复用。
    shape: Option<CursorShape>,
    /// 指针之上的动态特效状态机（轨迹采样、点击上升沿检测）。
    ///
    /// 始终存在：配置没开时它的 `observe`/`plan` 都是空操作，省掉一层 `Option` 判断。
    overlay: CursorOverlay,
    /// 桌面像素缓存（仅 CPU 路径使用）。
    ///
    /// 用途有二：① 指针移动而桌面不变时，不必重新从显存搬整帧，直接在这份缓存上
    /// 重新合成；② 从没变过的画面直接返回这份 `Arc`，连一次 memcpy 都不做。
    desktop_bgra: Option<Arc<[u8]>>,
    /// 下一次交付必须重新拷贝桌面：首帧与 `rebuild` 之后置位。
    ///
    /// 判据不能只看 `AccumulatedFrames > 0`：`DuplicateOutput`/重建后的第一帧被我们
    /// 丢弃了，若紧接着来一帧「只有指针动」的更新，`AccumulatedFrames` 会是 0，
    /// 而缓存的桌面还是上一次会话的旧图（甚至从来没拷过）。
    force_desktop_copy: bool,
}

unsafe impl Send for DxgiCapture {}

/// 判断 HRESULT 是否属于「需要重建 duplication」的设备丢失类错误。
pub fn is_device_lost(hr: windows::core::HRESULT) -> bool {
    hr == DXGI_ERROR_ACCESS_LOST || hr == DXGI_ERROR_DEVICE_REMOVED
}

/// 是否为「画面暂时不可见、等待即可恢复」类错误。
///
/// 锁屏、UAC 安全桌面、RDP 断开都会返回 `DXGI_ERROR_ACCESS_LOST`。这种中断是
/// 用户行为而非故障，必须无限重试——按设备丢失判负会让离开座位一次就丢整段录像。
pub fn is_resumable_loss(hr: windows::core::HRESULT) -> bool {
    hr == DXGI_ERROR_ACCESS_LOST
}

/// 按 region（虚拟桌面物理像素）定位所属显示器输出。
///
/// 容错策略：
/// 1. 优先完全包含 region 的显示器。
/// 2. 若跨屏或窗口边缘超出屏幕（如 Windows 边框阴影 -8px），选取重叠面积最大的显示器输出，
///    并将捕获区域安全裁剪至该显示器范围内，宽高偶数对齐。
pub fn find_target(
    region_x: i32,
    region_y: i32,
    region_w: u32,
    region_h: u32,
) -> WinResult<CaptureTarget> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };

    let mut best_target: Option<CaptureTarget> = None;
    let mut max_intersect_area: i64 = 0;

    let mut adapter_index = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(_) => break, // 没有更多适配器
        };

        let mut output_index = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(_) => break,
            };
            let desc = unsafe { output.GetDesc() }?;
            let rc = desc.DesktopCoordinates;

            // 1. 优先完全包含的情况
            if region_x >= rc.left
                && region_y >= rc.top
                && region_x + region_w as i32 <= rc.right
                && region_y + region_h as i32 <= rc.bottom
            {
                return Ok(CaptureTarget {
                    adapter_index,
                    output_index,
                    output_rect: rc,
                    offset_x: region_x - rc.left,
                    offset_y: region_y - rc.top,
                    width: (region_w & !1).max(2),
                    height: (region_h & !1).max(2),
                });
            }

            // 2. 计算交集面积，作为最大重叠兜底
            let inter_left = region_x.max(rc.left);
            let inter_top = region_y.max(rc.top);
            let inter_right = (region_x + region_w as i32).min(rc.right);
            let inter_bottom = (region_y + region_h as i32).min(rc.bottom);

            if inter_right > inter_left && inter_bottom > inter_top {
                let area = (inter_right - inter_left) as i64 * (inter_bottom - inter_top) as i64;
                if area > max_intersect_area {
                    max_intersect_area = area;
                    let clamped_w = ((inter_right - inter_left) as u32 & !1).max(2);
                    let clamped_h = ((inter_bottom - inter_top) as u32 & !1).max(2);
                    best_target = Some(CaptureTarget {
                        adapter_index,
                        output_index,
                        output_rect: rc,
                        offset_x: inter_left - rc.left,
                        offset_y: inter_top - rc.top,
                        width: clamped_w,
                        height: clamped_h,
                    });
                }
            }
            output_index += 1;
        }
        adapter_index += 1;
    }

    if let Some(target) = best_target {
        return Ok(target);
    }

    Err(WinError::from_hresult(
        windows::Win32::Foundation::E_INVALIDARG,
    ))
}

impl DxgiCapture {
    /// 在目标输出上创建 D3D11 设备并启动桌面复制。
    pub fn new(target: &CaptureTarget, options: CaptureOptions) -> WinResult<Self> {
        // 设备必须建在「持有该输出的适配器」上，否则 DuplicateOutput 直接失败。
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(target.adapter_index)? };

        let mut device_opt: Option<ID3D11Device> = None;
        let mut context_opt: Option<ID3D11DeviceContext> = None;
        let mut _level = Default::default();
        let creation_flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                creation_flags,
                None, // 默认 feature level 列表
                D3D11_SDK_VERSION,
                Some(&mut device_opt),
                Some(&mut _level),
                Some(&mut context_opt),
            )?
        };
        let device =
            device_opt.ok_or_else(|| WinError::from_hresult(windows::Win32::Foundation::E_FAIL))?;
        let context = context_opt
            .ok_or_else(|| WinError::from_hresult(windows::Win32::Foundation::E_FAIL))?;

        // 重新按序号拿同一个 output 并转为 Output1（DuplicateOutput 所在接口）。
        let output = unsafe { adapter.EnumOutputs(target.output_index)? };
        let output1: IDXGIOutput1 = output.cast()?;

        let duplication = unsafe { output1.DuplicateOutput(&device)? };

        // staging 纹理只覆盖 **region 尺寸**，不是整屏尺寸。
        // 早期按整屏开 staging，导致「录 300×200 小区域」也要每帧把整屏
        // （4K ≈ 33 MB）从显存搬到内存，白白吃掉约 1 GB/s 的 PCIe 带宽。
        let staging = create_staging_texture(&device, target.width, target.height)?;

        Ok(Self {
            device,
            context,
            duplication,
            staging,
            output_rect: target.output_rect,
            offset_x: target.offset_x,
            offset_y: target.offset_y,
            width: target.width,
            height: target.height,
            discard_next_frame: true,
            capture_cursor: options.capture_cursor,
            pointer: PointerState::default(),
            shape: None,
            overlay: CursorOverlay::new(options.overlay),
            desktop_bgra: None,
            force_desktop_copy: true,
        })
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    pub fn offset_x(&self) -> i32 {
        self.offset_x
    }

    pub fn offset_y(&self) -> i32 {
        self.offset_y
    }

    /// 设备丢失后重建 duplication（不重新创建设备：设备仍有效时这是最快的恢复路径；
    /// 若设备本身已 REMOVED，`duplicate_output` 会失败，由调用方升级为完整重建）。
    pub fn rebuild(&mut self) -> WinResult<()> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        // adapter/output 序号在重建期间不变（显示器热插拔由上层 10 次失败后终止兜底）。
        // 这里按序号重新枚举定位，与 find_target 的判定保持一致。
        let target = find_target_by_output(self.output_rect)?;
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(target.0)? };
        let output = unsafe { adapter.EnumOutputs(target.1)? };
        let output1: IDXGIOutput1 = output.cast()?;
        self.duplication = unsafe { output1.DuplicateOutput(&self.device)? };
        self.discard_next_frame = true;
        // 新会话的桌面内容与旧会话无关：缓存作废，形状也必须重取
        //（尺寸/样式都可能因为 DPI 或主题变化而不同）。
        self.desktop_bgra = None;
        self.force_desktop_copy = true;
        self.shape = None;
        // 特效状态也一并丢弃：断开期间可能过了很久，旧轨迹/旧波纹的时间戳
        // 相对新会话已经是「很久以前」，留着只会得到一堆立即过期的项。
        self.overlay = CursorOverlay::new(self.overlay.config());
        Ok(())
    }

    /// 指针当前位置（区域坐标系）。用于测试与诊断。
    pub fn pointer_state(&self) -> PointerState {
        self.pointer
    }

    /// 当前形状尺寸，`None` 表示还没收到过任何形状。
    pub fn cursor_shape_size(&self) -> Option<(u32, u32)> {
        self.shape.as_ref().map(|s| (s.width(), s.height()))
    }

    /// 叠加层此刻是否还有动画在进行（波纹扩散 / 残影淡出）。
    ///
    /// 采集循环必须用它做两件事：
    /// 1. 打破「桌面没变、指针也没动就返回 `Ok(None)`」的快捷路径；
    /// 2. 动画期间缩短 `AcquireNextFrame` 的超时，否则静止画面上每个动画帧都要
    ///    先白等一次超时，300 ms 的波纹会掉成个位数帧。
    pub fn overlay_animating(&self) -> bool {
        self.capture_cursor && self.overlay.is_animating(qpc_micros())
    }

    /// 现在这一刻要叠加的内容。`None` 表示没开指针合成。
    ///
    /// 给「桌面没变、但特效还在动」的帧用：调用方拿它去重画一帧，不需要新的显存帧。
    pub fn overlay_plan_now(&self) -> Option<OverlayPlan<'_>> {
        if !self.capture_cursor {
            return None;
        }
        Some(self.overlay.plan(
            self.shape.as_ref(),
            self.pointer,
            qpc_micros(),
            self.width,
            self.height,
        ))
    }

    /// 用缓存的桌面像素重新合成一帧（CPU 路径的「动画帧」）。
    ///
    /// 返回 `None` 表示还没拿到过桌面内容，调用方应退回「复用上一帧」。
    /// 没有任何可画内容时返回的是缓存 `Arc` 的克隆（零拷贝），不会白拷 8 MB。
    pub fn recompose_overlay(&mut self, qpc: u64) -> Option<CapturedFrame> {
        if !self.capture_cursor {
            return None;
        }
        let desktop = self.desktop_bgra.as_ref()?;
        let w = self.width;
        let h = self.height;
        let stride = (w * 4) as usize;

        let plan = self
            .overlay
            .plan(self.shape.as_ref(), self.pointer, qpc_micros(), w, h);
        let bgra: Arc<[u8]> = if plan.is_empty() {
            Arc::clone(desktop)
        } else {
            let mut buf = desktop.to_vec();
            plan.draw_into(&mut Canvas::new(&mut buf, stride, w, h), (0, 0));
            buf.into()
        };

        Some(CapturedFrame {
            bgra,
            stride: w * 4,
            width: w,
            height: h,
            qpc,
        })
    }

    /// 观察一次叠加层（采样轨迹、检测点击上升沿）。
    ///
    /// **每个采集时机都要调**，包括「等不到新帧」的时候——否则点在静止画面上的
    /// 那一下永远不会被记录，波纹也就永远不会出现。
    fn observe_overlay(&mut self, now_us: u64) {
        if !self.capture_cursor {
            return;
        }
        let hotspot = self.shape.as_ref().map(|s| s.hotspot()).unwrap_or((0, 0));
        self.overlay.observe(self.pointer, hotspot, now_us);
    }

    /// 刷新指针位置与形状，返回「本帧是否有可见变化」。
    ///
    /// 必须在 `ReleaseFrame` **之前**调用：`GetFramePointerShape` 要求调用方仍持有帧，
    /// 否则返回 `DXGI_ERROR_INVALID_CALL`。
    fn update_pointer(&mut self, frame_info: &DXGI_OUTDUPL_FRAME_INFO) -> bool {
        if !self.capture_cursor {
            return false;
        }
        let mut changed = false;

        // `LastMouseUpdateTime == 0` 表示这一帧根本没带指针更新——沿用上一状态，
        // **不要**去读 `PointerPosition`（此时它是全 0，采信会让指针瞬移到左上角）。
        if frame_info.LastMouseUpdateTime != 0 {
            let pos = frame_info.PointerPosition;
            // `Position` 是相对该输出左上角的坐标，换算到录制区域坐标。
            let next = PointerState {
                visible: pos.Visible.as_bool(),
                x: pos.Position.x - self.offset_x,
                y: pos.Position.y - self.offset_y,
            };
            // 位置没变就不算变化：否则鼠标每动一下都会逼下游重编码一帧相同画面。
            if next != self.pointer {
                self.pointer = next;
                changed = true;
            }
        }

        // 形状：`PointerShapeBufferSize > 0` 才代表**变了**；为 0 时不该重复取。
        //
        // 但**形状为空时必须无条件试一次**：刚建立复制会话时，形状不一定随着第一个
        // 鼠标更新一起给出。实测会遇到「`PointerPosition` 一直在更新、形状却连续
        // 几十帧都是空」的情况——若死守 `PointerShapeBufferSize > 0` 这个条件，
        // 录制开头（甚至整段）都不会有指针。`GetFramePointerShape` 在持有帧期间
        // 总能返回当前形状，不依赖该字段。
        let may_need_shape = frame_info.PointerShapeBufferSize > 0 || self.shape.is_none();
        if may_need_shape {
            match self.fetch_cursor_shape(frame_info.PointerShapeBufferSize) {
                Some(shape) => {
                    // 形状换了（或首次拿到）必然要重画，即使位置完全没动。
                    // GPU 侧按形状尺寸分配暂存纹理，所以尺寸变化也走这条。
                    self.shape = Some(shape);
                    changed = true;
                }
                None => {
                    // 取不到（驱动给了怪数据 / 这一帧确实没有形状）：
                    // 保留已有形状，别把上一帧的指针弄丢。
                }
            }
        }

        changed
    }

    /// 取一次指针形状。缓冲区不够时按 API 回填的 `required` 重试一次。
    ///
    /// `size_hint` 为 0 表示这次不是「形状变了」触发的主动探取：先用一个小缓冲试，
    /// 真装不下时 API 会返回 `DXGI_ERROR_MORE_DATA` 并回填所需大小。
    fn fetch_cursor_shape(&mut self, size_hint: u32) -> Option<CursorShape> {
        // 形状缓冲区理论上最大也就几百 KB（256×256 的 32bpp 只有 256 KB）。
        // 对异常值设上限，避免一次坏调用要求分配几百 MB。
        const MAX_SHAPE_BYTES: u32 = 8 * 1024 * 1024;
        /// 主动探取时的起步缓冲：绝大多数光标（16～64 px）都装得下。
        const PROBE_BYTES: u32 = 64 * 1024;

        let mut size = if size_hint == 0 {
            PROBE_BYTES
        } else if size_hint <= MAX_SHAPE_BYTES {
            size_hint
        } else {
            return None;
        };

        let mut buf = vec![0u8; size as usize];
        let mut required = 0u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();

        let mut hr = unsafe {
            self.duplication.GetFramePointerShape(
                size,
                buf.as_mut_ptr().cast(),
                &mut required,
                &mut info,
            )
        };
        // 缓冲区不足时按真实所需大小重试一次（文档保证会回填 required）。
        if let Err(e) = hr {
            if e.code() == DXGI_ERROR_MORE_DATA && required > 0 && required <= MAX_SHAPE_BYTES {
                size = required;
                buf.resize(size as usize, 0);
                hr = unsafe {
                    self.duplication.GetFramePointerShape(
                        size,
                        buf.as_mut_ptr().cast(),
                        &mut required,
                        &mut info,
                    )
                };
            } else {
                return None;
            }
        }
        hr.ok()?;
        CursorShape::parse(info.Type, &info, &buf)
    }

    /// 把显存里的 region 那一块搬成紧凑行距的 CPU 缓冲。
    fn copy_region_to_cpu(&mut self, frame_texture: &ID3D11Texture2D) -> WinResult<Arc<[u8]>> {
        let w = self.width as usize;
        let h = self.height as usize;
        let ox = self.offset_x.max(0) as u32;
        let oy = self.offset_y.max(0) as u32;

        let out_w = (self.output_rect.right - self.output_rect.left).max(1) as u32;
        let out_h = (self.output_rect.bottom - self.output_rect.top).max(1) as u32;

        // 输出尺寸可能在运行时缩小（分辨率切换瞬间）：能拷多少拷多少，其余留黑。
        let valid_w = w.min(out_w.saturating_sub(ox) as usize);
        let valid_h = h.min(out_h.saturating_sub(oy) as usize);

        // 紧凑行距（width*4）落地；只搬 region 那一块显存。
        let mut bgra = vec![0u8; w * h * 4];
        if valid_w == 0 || valid_h == 0 {
            // 理论上不可达（region 必与输出有交集）；留黑帧好过返回 Err 中断录制。
            return Ok(bgra.into());
        }

        let region = D3D11_BOX {
            left: ox,
            top: oy,
            front: 0,
            right: ox + valid_w as u32,
            bottom: oy + valid_h as u32,
            back: 1,
        };
        unsafe {
            self.context.CopySubresourceRegion(
                &self.staging,
                0,
                0,
                0,
                0,
                frame_texture,
                0,
                Some(&region),
            );
        }

        let mut mapped = Default::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }

        let src_stride = mapped.RowPitch as usize;
        let src_base = mapped.pData as *const u8;
        for row in 0..valid_h {
            let src =
                unsafe { std::slice::from_raw_parts(src_base.add(row * src_stride), valid_w * 4) };
            let dst_off = row * w * 4;
            bgra[dst_off..dst_off + valid_w * 4].copy_from_slice(src);
        }

        unsafe {
            self.context.Unmap(&self.staging, 0);
        }

        Ok(bgra.into())
    }

    /// 采集一帧。`timeout_ms` 内无新帧返回 `Ok(None)`（对应 DXGI_ERROR_WAIT_TIMEOUT）。
    ///
    /// 设备丢失错误原样返回 `Err`（HRESULT 可由 [`is_device_lost`] 判定）。
    pub fn acquire_frame(
        &mut self,
        timeout_ms: u32,
        qpc_now: u64,
    ) -> WinResult<Option<CapturedFrame>> {
        let now_us = qpc_micros();
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut resource: Option<windows::Win32::Graphics::Dxgi::IDXGIResource> = None;

        let hr = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
        };
        if let Err(e) = hr {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                // 等不到新帧也要采一次样：点击可能正发生在完全静止的画面上，
                // 而点击本身不会让 DXGI 交付帧。
                self.observe_overlay(now_us);
                return Ok(None);
            }
            return Err(e);
        }

        let resource = match resource {
            Some(r) => r,
            None => {
                // 理论上不可达（成功时 COM 会填 out 参数）；稳妥起见释放帧并跳过。
                let _ = unsafe { self.duplication.ReleaseFrame() };
                return Ok(None);
            }
        };
        let frame_texture: ID3D11Texture2D = resource.cast()?;

        // 指针状态必须在 ReleaseFrame **之前**刷新：`GetFramePointerShape` 要求
        // 调用方仍持有帧，否则返回 DXGI_ERROR_INVALID_CALL。
        let pointer_changed = self.update_pointer(&frame_info);
        self.observe_overlay(now_us);
        // 有无新的桌面像素。`force_desktop_copy` 覆盖「首帧 / 重建后」两种情况。
        let desktop_updated = frame_info.AccumulatedFrames > 0 || self.force_desktop_copy;
        // 特效还在动也得出帧：波纹扩散与残影淡出不依赖任何外部变化。
        let animating = self.overlay.is_animating(now_us);

        if self.discard_next_frame {
            // 见字段注释：建立/重建 duplication 后的第一帧是未初始化图像。
            // 这里**不**清 force_desktop_copy：这一帧没被采用，下一帧才是真正要拷的。
            self.discard_next_frame = false;
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
            return Ok(None);
        }

        // 桌面没变、指针也没动、特效也不动 → 与 WAIT_TIMEOUT 等价，让上层重发上一帧。
        // 这条分支同时省掉一次整帧 memcpy（静止画面下这是常态）。
        if !desktop_updated && !pointer_changed && !animating {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
            return Ok(None);
        }

        if desktop_updated {
            match self.copy_region_to_cpu(&frame_texture) {
                Ok(buf) => {
                    self.desktop_bgra = Some(buf);
                    self.force_desktop_copy = false;
                }
                Err(e) => {
                    unsafe {
                        let _ = self.duplication.ReleaseFrame();
                    }
                    return Err(e);
                }
            }
        }

        let Some(desktop) = self.desktop_bgra.as_ref() else {
            // 取不到缓存（只可能出现在 force 逻辑被破坏时）；跳过这一帧好过返回坏图。
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
            return Ok(None);
        };

        let w = self.width;
        let h = self.height;
        let stride = (w * 4) as usize;

        // 叠加合成。只有真的会画出像素时才复制整帧——1080p 一帧就是 8 MB，
        // 静止画面上每帧复制一遍的代价不可忽略。
        let plan = self
            .overlay
            .plan(self.shape.as_ref(), self.pointer, now_us, w, h);
        let composed: Option<Arc<[u8]>> = if plan.is_empty() {
            None
        } else {
            let mut buf = desktop.to_vec();
            plan.draw_into(&mut Canvas::new(&mut buf, stride, w, h), (0, 0));
            Some(buf.into())
        };

        // 没合成（未开指针 / 光标不在区域内 / 特效已结束）时直接复用缓存：零拷贝。
        let bgra = composed.unwrap_or_else(|| Arc::clone(desktop));

        unsafe {
            // ReleaseFrame 失败不影响本帧数据已拷贝的事实，忽略其错误。
            let _ = self.duplication.ReleaseFrame();
        }

        Ok(Some(CapturedFrame {
            bgra,
            stride: w * 4,
            width: w,
            height: h,
            qpc: qpc_now,
        }))
    }

    /// GPU 零拷贝直接采集：获取原始 D3D11 纹理并在闭包中消费（显存内裁剪与编码），
    /// 随后自动释放帧。
    ///
    /// 闭包额外拿到 [`FrameMeta`]：桌面是否真的更新了（`desktop_updated`），
    /// 以及这一刻要叠加的内容（`overlay`，未开启指针合成时为 `None`）。
    /// 前者让下游能跳过「只有指针动」时的整帧重拷。
    pub fn acquire_texture<F, R>(&mut self, timeout_ms: u32, f: F) -> WinResult<Option<R>>
    where
        F: FnOnce(&ID3D11Texture2D, FrameMeta<'_>) -> WinResult<R>,
    {
        let now_us = qpc_micros();
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut resource: Option<windows::Win32::Graphics::Dxgi::IDXGIResource> = None;

        let hr = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
        };
        if let Err(e) = hr {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                self.observe_overlay(now_us);
                return Ok(None);
            }
            return Err(e);
        }

        let resource = match resource {
            Some(r) => r,
            None => {
                let _ = unsafe { self.duplication.ReleaseFrame() };
                return Ok(None);
            }
        };
        let frame_texture: ID3D11Texture2D = resource.cast()?;

        let pointer_changed = self.update_pointer(&frame_info);
        self.observe_overlay(now_us);
        let desktop_updated = frame_info.AccumulatedFrames > 0 || self.force_desktop_copy;
        let animating = self.overlay.is_animating(now_us);

        if self.discard_next_frame {
            self.discard_next_frame = false;
            let _ = unsafe { self.duplication.ReleaseFrame() };
            return Ok(None);
        }

        if !desktop_updated && !pointer_changed && !animating {
            let _ = unsafe { self.duplication.ReleaseFrame() };
            return Ok(None);
        }

        let w = self.width;
        let h = self.height;
        let meta = FrameMeta {
            desktop_updated,
            overlay: if self.capture_cursor {
                Some(
                    self.overlay
                        .plan(self.shape.as_ref(), self.pointer, now_us, w, h),
                )
            } else {
                None
            },
        };

        let result = f(&frame_texture, meta);

        // 只有闭包成功接手之后才清掉强制重拷标记：闭包失败时下游的桌面副本
        // 依旧是半旧的，下一帧还得再拷一次。
        if desktop_updated && result.is_ok() {
            self.force_desktop_copy = false;
        }

        let _ = unsafe { self.duplication.ReleaseFrame() };

        result.map(Some)
    }
}

/// staging 纹理：CPU 可读、BGRA、与输出同尺寸。
/// 注意：D3D11_USAGE_STAGING 纹理的 BindFlags 必须为 0（D3D11 规范），
/// 误绑 SHADER_RESOURCE 会让 CreateTexture2D 直接返回 E_INVALIDARG。
fn create_staging_texture(device: &ID3D11Device, w: u32, h: u32) -> WinResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(|| WinError::from_hresult(windows::Win32::Foundation::E_FAIL))
}

/// 按输出矩形反查 (adapter_index, output_index)（用于 rebuild 时的重新定位）。
fn find_target_by_output(rect: RECT) -> WinResult<(u32, u32)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut adapter_index = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(_) => break,
        };
        let mut output_index = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(_) => break,
            };
            let desc = unsafe { output.GetDesc() }?;
            if desc.DesktopCoordinates == rect {
                return Ok((adapter_index, output_index));
            }
            output_index += 1;
        }
        adapter_index += 1;
    }
    Err(WinError::from_hresult(
        windows::Win32::Foundation::E_INVALIDARG,
    ))
}

/// QPC 读取（纳秒级单调时钟，音视频共同时间基准）。
#[inline]
pub fn qpc_now() -> u64 {
    let mut ticks: i64 = 0;
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceCounter(&mut ticks);
    }
    ticks as u64
}

/// QPC 频率（每秒 tick 数），启动时读取一次。
#[inline]
pub fn qpc_frequency() -> u64 {
    let mut freq: i64 = 0;
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut freq);
    }
    freq.max(1) as u64
}

/// 当前单调时间（微秒），供特效动画计时用。
///
/// 用 `u128` 中转再落回 `u64`：tick 数乘 1e6 的量级很容易超过 `u64`（QPC 频率常见
/// 10 MHz，开机一小时后 tick 数就有 3.6e10，乘完是 3.6e16），但 `u128` 绝不会溢出。
#[inline]
pub fn qpc_micros() -> u64 {
    (qpc_now() as u128 * 1_000_000 / qpc_frequency() as u128) as u64
}

/// 高精度可等待定时器句柄缓存（`isize` 中转，`HANDLE` 不实现 Send/Sync）。
///
/// Windows 10 1803+ 支持 `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`：0.5 ms 级精度且
/// **不改全局定时器分辨率**（不像 `timeBeginPeriod` 那样拖累整机功耗）。
static HIGH_RES_TIMER: OnceLock<Option<isize>> = OnceLock::new();

fn high_res_timer() -> Option<HANDLE> {
    HIGH_RES_TIMER
        .get_or_init(|| {
            let timer = unsafe {
                CreateWaitableTimerExW(
                    None,
                    None,
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS.0,
                )
            };
            timer.ok().map(|h| h.0 as isize)
        })
        .map(|raw| HANDLE(raw as *mut std::ffi::c_void))
}

/// 高精度休眠到「下一帧时刻」。
///
/// 帧率配速的精度直接决定 CFR 质量：`Sleep(1)` 在未提升定时器分辨率时可能睡到
/// 15.6 ms，靠末尾自旋兜底既费 CPU 又抖。这里改用高精度可等待定时器睡到
/// 「到期前 300 µs」，剩余用自旋收尾；拿不到定时器时退回旧策略。
pub fn precise_sleep_until(target_qpc: u64) {
    let freq = qpc_frequency();
    loop {
        let now = qpc_now();
        if now >= target_qpc {
            return;
        }
        let remain_ticks = target_qpc - now;
        // 剩余时间换算成纳秒，避免 tick → µs 的整除误差累积。
        let remain_ns = remain_ticks.saturating_mul(1_000_000_000) / freq;

        if remain_ns > 300_000 {
            if let Some(timer) = high_res_timer() {
                // SetWaitableTimer 的相对到期时间单位是 100 ns，负数表示相对。
                let due_ns = remain_ns - 200_000;
                let due = -((due_ns / 100).max(1) as i64);
                let armed = unsafe { SetWaitableTimer(timer, &due, 0, None, None, false) };
                if armed.is_ok() {
                    unsafe { WaitForSingleObject(timer, INFINITE) };
                    continue;
                }
            }
            unsafe {
                windows::Win32::System::Threading::Sleep(1);
            }
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpc_is_monotonic_and_fast() {
        let a = qpc_now();
        let b = qpc_now();
        assert!(b >= a);
        assert!(qpc_frequency() >= 1_000_000, "QPC 频率至少应为 1MHz");
    }

    // 注意：find_target / DxgiCapture::new 需要真实桌面会话（DXGI 桌面复制
    // 在无显示器/服务会话下不可用），归入集成测试手动验证，不进 CI 快档。
}
