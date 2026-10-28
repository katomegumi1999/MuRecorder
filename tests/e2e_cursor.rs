//! 端到端：鼠标指针是不是真的被画进画面了？
//!
//! ```text
//! cargo test --test e2e_cursor -- --ignored --nocapture
//! ```
//!
//! ## 为什么这条用例必须存在
//!
//! 指针合成有一整类错误，在单测里**完全看不出来**：单测只能证明「给定形状 +
//! 给定位置，`blend_into` 算得对」。但真实链路里还有三层，每一层都可能悄悄失效：
//!
//! 1. `PointerPosition` 有没有被正确解读（它给的是**左上角**，不是热点）；
//! 2. 形状有没有真的从 `GetFramePointerShape` 取到——只在
//!    `PointerShapeBufferSize > 0` 时才取会漏掉开头几十帧（实测过）；
//! 3. 合成是否发生在喂给编码器的那一份像素上。
//!
//! 这里用真实桌面把它整条走通：把光标停在 A 取一帧、挪到 B 取一帧，
//! 「A 处的像素块」和「B 处的像素块」在两帧之间都必须发生变化——前者是光标离开，
//! 后者是光标到达。
//!
//! ## 断言为什么是容错的
//!
//! 这条用例跑在**用户正在使用的机器上**：鼠标随时可能被人接管，`SetCursorPos`
//! 之后读回来的位置并不保证等于我们设的值（实测偏 1～2 px 很常见，甚至整个被
//! 挪到别处）。所以断言不写死坐标，一律以「采集到的实际指针位置」为准，只要求
//! 两次位置相距够远、且两处像素块都发生了变化。
//!
//! ## 注意
//!
//! 需要已解锁的桌面会话，且**会移动真实鼠标**（结束会还原），故标 `#[ignore]`。

use std::time::{Duration, Instant};

use murecorder_core::capture::{CaptureOptions, CaptureTarget, CapturedFrame, DxgiCapture, dxgi};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

const REGION_W: u32 = 320;
const REGION_H: u32 = 240;
/// 光标包围盒的比对边长。光标常见是 32，最大能到 256；取 48 足够覆盖常见情形。
const BLOCK: u32 = 48;
/// 两次采样位置的最小间距：必须大于 `BLOCK`，否则两个比对块会重叠、结论不可信。
const MIN_SEPARATION: i32 = 64;
/// 位置 A 的等待容差。比对块用的是**采集到的**位置，所以这只是一个「确实到了
/// 我们要的地方」的宽松校验——机器活人在用，`SetCursorPos` 与读回值差几个像素很正常。
const POS_TOLERANCE: i32 = 16;

/// 测试会移动真实指针，无论如何都要放回原位。
struct CursorRestore(POINT);

impl CursorRestore {
    fn save() -> Self {
        let mut p = POINT::default();
        // Safety: 传的是本地变量的地址，调用期间有效。
        let ok = unsafe { GetCursorPos(&mut p) }.is_ok();
        Self(if ok { p } else { POINT::default() })
    }
}

impl Drop for CursorRestore {
    fn drop(&mut self) {
        unsafe {
            let _ = SetCursorPos(self.0.x, self.0.y);
        }
    }
}

/// 在屏幕左上角附近挑一块区域。返回 `None` 表示当前没有可用的桌面会话。
fn pick_region() -> Option<(i32, i32, u32, u32)> {
    // 先用 2×2 探一下原点落在哪个输出上，再在该输出内部取一块，
    // 避免区域跨屏或多屏坐标为负。
    let probe: CaptureTarget = dxgi::find_target(0, 0, 2, 2).ok()?;
    let x = probe.output_rect.left + 80;
    let y = probe.output_rect.top + 80;
    if x + REGION_W as i32 > probe.output_rect.right
        || y + REGION_H as i32 > probe.output_rect.bottom
    {
        return None;
    }
    Some((x, y, REGION_W, REGION_H))
}

/// 一直取帧，直到 `pred` 对当前 capture 状态成立，返回那一帧。
fn acquire_until<F>(cap: &mut DxgiCapture, mut pred: F) -> Option<CapturedFrame>
where
    F: FnMut(&DxgiCapture) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match cap.acquire_frame(50, dxgi::qpc_now()) {
            Ok(Some(frame)) if pred(cap) => return Some(frame),
            // 没有新帧（静止画面）或状态还没到位：继续等。
            Ok(_) => {}
            Err(e) => panic!("采集失败: {e}"),
        }
    }
    None
}

/// 比较两帧在同一块矩形内的像素是否有差异。
fn block_differs(a: &CapturedFrame, b: &CapturedFrame, x: u32, y: u32, w: u32, h: u32) -> bool {
    assert_eq!((a.width, a.height), (b.width, b.height), "两帧尺寸应一致");
    let stride = a.stride as usize;
    for row in y..(y + h).min(a.height) {
        for col in x..(x + w).min(a.width) {
            let off = row as usize * stride + col as usize * 4;
            if a.bgra[off..off + 4] != b.bgra[off..off + 4] {
                return true;
            }
        }
    }
    false
}

/// 把区域坐标系里的位置夹成「比对块的左上角」。
fn block_origin(x: i32, y: i32) -> (u32, u32) {
    let clamp = |v: i32, limit: u32| (v.max(0) as u32).min(limit.saturating_sub(BLOCK));
    (clamp(x, REGION_W), clamp(y, REGION_H))
}

/// 一次采样的结果：两帧 + 各自**实际**的光标位置（区域坐标系）。
struct Sample {
    frame_a: CapturedFrame,
    pos_a: (i32, i32),
    frame_b: CapturedFrame,
    pos_b: (i32, i32),
}

/// 采一对帧：光标先停在 A 取一帧，再挪到 B 取一帧。
fn capture_pair(cap: &mut DxgiCapture, rx: i32, ry: i32) -> Option<Sample> {
    let (ax, ay) = (rx + 60, ry + 60);
    unsafe {
        let _ = SetCursorPos(ax, ay);
    }
    let frame_a = acquire_until(cap, |c| {
        let p = c.pointer_state();
        c.pointer_state().visible
            && c.cursor_shape_size().is_some()
            && (p.x - (ax - rx)).abs() <= POS_TOLERANCE
            && (p.y - (ay - ry)).abs() <= POS_TOLERANCE
    })?;
    let pos_a = cap.pointer_state();

    let (bx, by) = (rx + 220, ry + 180);
    unsafe {
        let _ = SetCursorPos(bx, by);
    }
    let frame_b = acquire_until(cap, |c| {
        let p = c.pointer_state();
        c.pointer_state().visible
            && c.cursor_shape_size().is_some()
            && (p.x - pos_a.x).abs().max((p.y - pos_a.y).abs()) >= MIN_SEPARATION
    })?;
    let pos_b = cap.pointer_state();

    Some(Sample {
        frame_a,
        pos_a: (pos_a.x, pos_a.y),
        frame_b,
        pos_b: (pos_b.x, pos_b.y),
    })
}

/// 指针移动时，画面里对应位置必须真的发生变化。
#[test]
#[ignore = "需要已解锁的桌面会话，且会移动真实鼠标指针（结束会还原）"]
fn cursor_is_composited_at_pointer_position() {
    let _restore = CursorRestore::save();

    let Some((rx, ry, rw, rh)) = pick_region() else {
        println!("没有可用的桌面输出，跳过（需要真实桌面会话）");
        return;
    };
    let target = dxgi::find_target(rx, ry, rw, rh).expect("区域应能定位到输出");
    let mut cap = DxgiCapture::new(&target, CaptureOptions::with_cursor(true))
        .expect("开启指针合成时桌面复制应能建立");

    // 先确认链路能给出指针状态与形状。这一条本身就锁住了「主动探形状」：
    // 若只在 `PointerShapeBufferSize > 0` 时才取，实测会有几十帧拿不到形状。
    acquire_until(&mut cap, |c| {
        c.pointer_state().visible && c.cursor_shape_size().is_some()
    })
    .expect("应能取到带指针状态与形状的帧");
    let (cw, ch) = cap.cursor_shape_size().expect("应已解析出光标形状");
    assert!(cw > 0 && ch > 0, "光标尺寸非法: {cw}x{ch}");
    println!("光标形状 {cw}x{ch}，区域 {rw}x{rh}");

    // 最多试三次。重试**不会**削弱判定力：真缺陷（合成没发生、位置算错）每次都会失败。
    // 它吸收的是「机器正被使用」这类噪声——用户在拖拽窗口、切换虚拟桌面、或正在
    // 移动鼠标时，Windows 会短暂不更新光标位置（拖拽期间尤其明显），那一次采样就
    // 可能两帧看起来一样。
    let mut last_pos = None;
    for attempt in 1..=3 {
        let Some(s) = capture_pair(&mut cap, rx, ry) else {
            println!("第 {attempt} 次采样没等到符合条件的两帧，重试");
            continue;
        };
        let (a_col, a_row) = block_origin(s.pos_a.0, s.pos_a.1);
        let (b_col, b_row) = block_origin(s.pos_b.0, s.pos_b.1);
        let a_changed = block_differs(&s.frame_a, &s.frame_b, a_col, a_row, BLOCK, BLOCK);
        let b_changed = block_differs(&s.frame_a, &s.frame_b, b_col, b_row, BLOCK, BLOCK);

        if a_changed && b_changed {
            println!(
                "通过（第 {attempt} 次采样）：A{:?} 与 B{:?} 两处都随指针变化",
                s.pos_a, s.pos_b
            );
            return;
        }
        println!(
            "第 {attempt} 次采样异常：A={a_changed} B={b_changed}（A{:?} B{:?}），重试",
            s.pos_a, s.pos_b
        );
        last_pos = Some((s.pos_a, s.pos_b));
    }

    panic!("三次采样后 A/B 两处像素都没有随指针变化，最后位置 {last_pos:?}");
}

/// 关闭 `capture_cursor` 时，指针状态必须完全不被采集。
///
/// 这条不只是「省开销」的断言——如果关掉开关后仍然去取形状，就说明开关没接进
/// 采集层，那么它在「开着」的时候大概率也没真的生效（只是碰巧看不见症状）。
#[test]
#[ignore = "需要已解锁的桌面会话，且会移动真实鼠标指针（结束会还原）"]
fn cursor_is_ignored_when_disabled() {
    let _restore = CursorRestore::save();

    let Some((rx, ry, rw, rh)) = pick_region() else {
        println!("没有可用的桌面输出，跳过（需要真实桌面会话）");
        return;
    };
    let target = dxgi::find_target(rx, ry, rw, rh).expect("区域应能定位到输出");
    let mut cap = DxgiCapture::new(&target, CaptureOptions::default()).expect("桌面复制应能建立");

    // 制造一次指针更新，确保「没解析到形状」不是因为压根没发生鼠标事件。
    unsafe {
        let _ = SetCursorPos(rx + 60, ry + 60);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = cap.acquire_frame(50, dxgi::qpc_now()) {
            break;
        }
    }

    assert!(
        cap.cursor_shape_size().is_none(),
        "关闭指针合成时不应该去取形状，实际拿到了 {:?}",
        cap.cursor_shape_size()
    );
    assert!(
        !cap.pointer_state().visible,
        "关闭指针合成时指针状态应保持默认（不可见）"
    );
}
