//! DXGI Desktop Duplication 捕获会话。
//!
//! M1 范围（对应设计文档 4.2 的软件路径）：
//! - 按区域左上角所在显示器 `DuplicateOutput`，逐帧拷贝到 staging 纹理后 Map 到 CPU；
//! - 硬件编码路径（M3）将改为纹理直送编码器，本模块的 `Frame` 结构已为纹理引用预留位置。
//!
//! 设备丢失（设计文档 4.3）：调用方检测到 `AccessLost` / `DeviceRemoved` 后进入重建循环
//! （sleep 500ms 重试 `duplicate_output`，最多 10 次），本模块提供 [`DxgiCapture::rebuild`]。

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
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::{Error as WinError, Interface, Result as WinResult};

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
    pub fn new(target: &CaptureTarget) -> WinResult<Self> {
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
        Ok(())
    }

    /// 采集一帧。`timeout_ms` 内无新帧返回 `Ok(None)`（对应 DXGI_ERROR_WAIT_TIMEOUT）。
    ///
    /// 设备丢失错误原样返回 `Err`（HRESULT 可由 [`is_device_lost`] 判定）。
    pub fn acquire_frame(
        &mut self,
        timeout_ms: u32,
        qpc_now: u64,
    ) -> WinResult<Option<CapturedFrame>> {
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut resource: Option<windows::Win32::Graphics::Dxgi::IDXGIResource> = None;

        let hr = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
        };
        if let Err(e) = hr {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
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

        if self.discard_next_frame {
            // 见字段注释：建立/重建 duplication 后的第一帧是未初始化图像。
            self.discard_next_frame = false;
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
            return Ok(None);
        }

        // 鼠标指针元数据（frame_info.PointerPosition）尚未合成，capture_cursor 字段预留。
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
        if valid_w > 0 && valid_h > 0 {
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
                    &frame_texture,
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
                let src = unsafe {
                    std::slice::from_raw_parts(src_base.add(row * src_stride), valid_w * 4)
                };
                let dst_off = row * w * 4;
                bgra[dst_off..dst_off + valid_w * 4].copy_from_slice(src);
            }

            unsafe {
                self.context.Unmap(&self.staging, 0);
            }
        } else {
            // 理论上不可达（region 必与输出有交集）；留黑帧好过返回 Err 中断录制。
        }

        unsafe {
            // ReleaseFrame 失败不影响本帧数据已拷贝的事实，忽略其错误。
            let _ = self.duplication.ReleaseFrame();
        }

        Ok(Some(CapturedFrame {
            bgra: bgra.into(),
            stride: (w * 4) as u32,
            width: w as u32,
            height: h as u32,
            qpc: qpc_now,
        }))
    }

    /// GPU 零拷贝直接采集：获取原始 D3D11 纹理并在闭包中消费（显存内裁剪与编码），随后自动释放帧
    pub fn acquire_texture<F, R>(&mut self, timeout_ms: u32, f: F) -> WinResult<Option<R>>
    where
        F: FnOnce(&ID3D11Texture2D) -> WinResult<R>,
    {
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut resource: Option<windows::Win32::Graphics::Dxgi::IDXGIResource> = None;

        let hr = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
        };
        if let Err(e) = hr {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
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

        if self.discard_next_frame {
            self.discard_next_frame = false;
            let _ = unsafe { self.duplication.ReleaseFrame() };
            return Ok(None);
        }

        let result = f(&frame_texture);

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
