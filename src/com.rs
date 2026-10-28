//! COM 套间（apartment）RAII 守卫。
//!
//! 为什么需要这个模块：COM 是**按线程**初始化的，而 Media Foundation、WASAPI、
//! D3D/DXGI 全部依赖 COM。少一次 `CoInitializeEx` 的症状极具误导性——
//! 例如在没有初始化 COM 的线程上 `MFTEnumEx` 能正常枚举出 AAC 编码器，
//! 但 `ActivateObject` 会返回一个光秃秃的 `E_FAIL`（0x80004005），
//! 错误信息里完全看不出「你没初始化 COM」。
//!
//! 把它做成 RAII 而不是裸调用，是为了让「初始化点」和「作用域」在代码里显式对应：
//! 守卫活多久，套间就活多久，忘记配对 `CoUninitialize` 也不可能发生。

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

/// 当前线程的 COM 多线程套间守卫。
pub struct ComApartment {
    /// `S_FALSE` 表示本线程**早已**初始化过，此时不需要（也不应该）由我们
    /// 去 `CoUninitialize`——计数必须自己还自己借的那一份。
    initialized_here: bool,
    /// `RPC_E_CHANGED_MODE`：线程已按别的套间模型（多半是 STA）初始化。
    /// 这种情况下 COM 本身可用，只需要不去反初始化。
    already_other_model: bool,
}

impl ComApartment {
    /// 在当前线程初始化 MTA 套间。
    ///
    /// 已有其它模型（STA）时不报错，直接沿用——录屏流水线用到的
    /// MF/WASAPI 在 MTA 下最好，但 STA 也能工作，为了不把一个纯粹的
    /// 套间偏好变成致命错误，这里只降级不失败。
    pub fn init_mta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            initialized_here: hr.is_ok(),
            already_other_model: hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE,
        }
    }

    /// 本线程的套间是否可用（无论是我们建的还是别人建的）。
    pub fn is_usable(&self) -> bool {
        self.initialized_here || self.already_other_model
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized_here {
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同一线程重复初始化：只有第一次是我们"借"的，嵌套守卫只有最外层
    /// 才真正反初始化。这里只验证不崩溃、不破坏后续 COM 使用。
    #[test]
    fn nested_guards_are_safe() {
        let outer = ComApartment::init_mta();
        assert!(outer.is_usable());
        {
            let inner = ComApartment::init_mta();
            assert!(inner.is_usable());
        }
        // 内层析构后外层仍然可用。
        assert!(outer.is_usable());
    }

    /// 守卫在**新建线程**上必须真的把 COM 初始化起来。
    #[test]
    fn guard_initializes_com_on_fresh_thread() {
        let h = std::thread::spawn(|| {
            let com = ComApartment::init_mta();
            // 新建线程上必然是 S_OK（借到一份），除非宿主环境已手工初始化。
            assert!(com.is_usable(), "新线程上 COM 套间应可用");
        });
        h.join().unwrap();
    }
}
