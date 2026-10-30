//! 鼠标视觉特效：移动轨迹拖尾 + 点击扩散圆环。
//!
//! 这两个效果都要**烧进画面**（编码前的像素），所以必须在 Rust 侧画，C# 层的
//! Skia 叠加做不到。它们都叠加在光标合成之上，共用同一条「GPU 只回读一小块、
//! CPU 负责像素」的通道。
//!
//! ## 为什么单独一层
//!
//! 指针本身是「**状态**」：位置 + 形状，画一次就够。特效是「**动画**」：波纹会自己
//! 扩张淡出、残影会自己消失，即使桌面和指针一动不动。这带来一个必须显式处理的
//! 后果——采集层那条「什么都没变就直接返回 `Ok(None)`」的省事快捷路径，会让波纹
//! 只画出一帧就冻住。所以这里提供 [`CursorOverlay::is_animating`]，采集层要用它
//! 打破「静止」判定。
//!
//! ## 点击检测为什么不用钩子
//!
//! 用 `GetAsyncKeyState` 每帧轮询一次按键状态，检测「未按下 → 按下」的上升沿。
//! 代价是采样率等于帧率：短于一个帧间隔的极快点击会漏掉。但人手点击通常持续
//! 50～200 ms，30 fps 下必然命中，而低级鼠标钩子（`WH_MOUSE_LL`）要一条带消息循环
//! 的线程、是全局钩子、超时会被系统静默摘掉，为一个视觉特效付这个复杂度不划算。
//!
//! ## 落点用热点，不是左上角
//!
//! 画光标时不看 `HotSpot`（DXGI 给的位置就是光标图像的左上角，见
//! [`super::cursor`]），但**点击的落点是热点位置**——箭头尖端才是用户以为他点到
//! 的地方。所以轨迹记的是光标位置，波纹记的是 `光标位置 + 热点偏移`。
//!
//! ## 面积会膨胀，所以要设上限
//!
//! 单个光标的重绘区域是一个 32×32 的框；加了轨迹之后，「当前位置 + 一串历史位置」
//! 的并集会随鼠标速度膨胀。注意 [`OverlayPlan::bounds`] 返回的是**外接矩形**，所以
//! 斜向拖动时开销按包围盒算，比真实像素面积大得多（这是 GPU「按矩形回读」的固有
//! 代价，不是估算错误）。这里因此设了 [`MAX_PLAN_PIXELS`]，超限就**从最旧的残影
//! 开始丢**——最旧的也最淡，丢掉最不容易看出来；一刀切掉整条轨迹反而会让拖尾
//! 在快速拖动时突然整条消失。主光标与波纹永不丢：它们是这个特效的主体。

use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};

use super::cursor::{Canvas, CursorShape, PointerState};

/// 一次点击波纹的生命周期（微秒）。
///
/// 350 ms 是「看得见、又不至于碍事」的折中：短于 200 ms 在 30 fps 下只有 6 帧，
/// 观感像闪一下；长于 500 ms 连续点击时会糊成一片。
const RIPPLE_DURATION_US: u64 = 350_000;
/// 波纹起始/终止半径（像素）。起点不是 0——从 0 开始的那几帧在视觉上等于没有。
/// 终止半径设为 24px（直径 48px），紧凑优雅，刚好环抱标准 32x32 光标热点，避免扩散太大遮挡画面。
const RIPPLE_START_RADIUS: f32 = 4.0;
const RIPPLE_END_RADIUS: f32 = 24.0;
/// 波纹初始不透明度（0–255）。不做全不透明，避免完全盖住下方内容。
const RIPPLE_ALPHA: f32 = 235.0;
/// 轨迹残影的初始不透明度与最大采样点数。
const TRAIL_ALPHA: f32 = 200.0;
const TRAIL_MAX_POINTS: usize = 24;
/// 单帧叠加区域的像素上限下限：至少允许 512×512，小区域录制不被这个上限卡死。
const MAX_PLAN_PIXELS: u64 = 512 * 512;
/// 上限还与画面面积挂钩：大画面按面积给额度，避免 1080p 下额度形同虚设。
/// 取 1/4 是「回读量 vs 零拷贝收益」的折中——整帧回读就等于放弃了零拷贝。
const MAX_PLAN_FRAME_FRACTION: u64 = 4;

/// 特效配置（来自 ABI 的 `cursor_click_effect` / `cursor_trail_ms`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayConfig {
    /// 是否在点击处画扩散圆环
    pub click_effect: bool,
    /// 移动轨迹拖尾时长（毫秒），0 表示关闭
    pub trail_ms: u32,
}

impl OverlayConfig {
    pub fn is_enabled(&self) -> bool {
        self.click_effect || self.trail_ms > 0
    }
}

/// 一个已经算好几何形状的波纹，交给绘制层。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RippleShape {
    /// 圆心（画面坐标系）
    pub x: i32,
    pub y: i32,
    pub radius: f32,
    pub alpha: u8,
    /// 圆环线宽（像素）
    pub thickness: f32,
}

impl RippleShape {
    /// 圆环的外接矩形（左上角 + 宽高），已含线宽与 1 px 抗锯齿余量。
    fn bounds(&self) -> (i32, i32, i32, i32) {
        let outer = self.radius + self.thickness * 0.5 + 1.0;
        let side = (outer * 2.0).ceil() as i32;
        (self.x - side / 2, self.y - side / 2, side, side)
    }
}

/// 波纹在生命周期内某一点的形状。
///
/// `t` 为归一化时间（0 = 刚点击，1 = 结束）。抽成纯函数是为了能脱离时间单测：
/// 半径必须单调递增、alpha 必须单调递减，这两条一旦搞反，现象是「波纹倒着收回去」
/// 或「越扩散越实」，在播放时才看得出来。
fn ripple_at(t: f32) -> (f32, u8) {
    let t = t.clamp(0.0, 1.0);
    // 半径用 ease-out：一开始扩得快、末尾慢下来，比线性更自然。
    let eased = 1.0 - (1.0 - t) * (1.0 - t);
    let radius = RIPPLE_START_RADIUS + (RIPPLE_END_RADIUS - RIPPLE_START_RADIUS) * eased;
    let alpha = RIPPLE_ALPHA * (1.0 - t).powf(1.6);
    (radius, alpha.round().clamp(0.0, 255.0) as u8)
}

/// 残影在 `age_us` 时刻的不透明度：越旧越淡。
fn trail_alpha(age_us: u64, trail_ms: u32) -> u8 {
    if trail_ms == 0 {
        return 0;
    }
    let lifetime = trail_ms as f32 * 1000.0;
    let remain = (1.0 - age_us as f32 / lifetime).clamp(0.0, 1.0);
    (TRAIL_ALPHA * remain.powf(1.5)).round() as u8
}

/// 采集时的按键快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Buttons {
    left: bool,
    right: bool,
}

impl Buttons {
    fn read() -> Self {
        // 最高位 = 当前是否按下。用 `as u16` 再取位，避免 i16 的符号问题。
        // Safety: GetAsyncKeyState 是纯查询，无参数约束。
        let left = unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 };
        let right = unsafe { (GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16 & 0x8000) != 0 };
        Self { left, right }
    }
}

/// 一次点击留下的波纹。
#[derive(Debug, Clone, Copy)]
struct Ripple {
    x: i32,
    y: i32,
    start_us: u64,
}

/// 轨迹上的一个采样点（**不含当前位置**，当前位置由主光标负责）。
#[derive(Debug, Clone, Copy)]
struct TrailPoint {
    x: i32,
    y: i32,
    at_us: u64,
}

/// 特效状态机：喂它「每个采集时机」，它维护轨迹与波纹，产出每帧要画的东西。
#[derive(Debug)]
pub struct CursorOverlay {
    config: OverlayConfig,
    trail: Vec<TrailPoint>,
    ripples: Vec<Ripple>,
    buttons: Buttons,
    /// 上一次观察到的光标位置，用来识别「移动了」。
    last_pos: Option<(i32, i32)>,
}

impl CursorOverlay {
    pub fn new(config: OverlayConfig) -> Self {
        Self {
            config,
            trail: Vec::new(),
            ripples: Vec::new(),
            // 初值取「都按着」：这样第一帧若鼠标本来就按着，不会凭空冒出一个波纹。
            buttons: Buttons {
                left: true,
                right: true,
            },
            last_pos: None,
        }
    }

    pub fn config(&self) -> OverlayConfig {
        self.config
    }

    /// 观察一个采集时机。**必须每帧都调用**，即使这一帧拿不到新画面——
    /// 否则点击与轨迹都会漏采样。
    ///
    /// `pointer` 是当前指针状态（区域坐标），`hotspot` 是形状热点的偏移
    /// （没有形状时传 `(0, 0)`），`now_us` 是单调时钟（微秒）。
    pub fn observe(&mut self, pointer: PointerState, hotspot: (i32, i32), now_us: u64) {
        // 没开点击特效时连按键都不读：`GetAsyncKeyState` 虽然便宜，但每帧两次
        // 累计起来也是白花，而且轮询会让系统持续更新按键状态缓存。
        let buttons = if self.config.click_effect {
            Buttons::read()
        } else {
            self.buttons
        };
        self.observe_with_buttons(pointer, hotspot, now_us, buttons);
    }

    /// `observe` 的实现体，按键快照由调用方给。
    ///
    /// 拆出来是为了能测「按下那一瞬间」这条逻辑：真实路径的按键来自
    /// `GetAsyncKeyState`，测试里不可能让机器去点鼠标；把快照当参数传进来，
    /// 上升沿检测与热点偏移就能被完整验证。
    fn observe_with_buttons(
        &mut self,
        pointer: PointerState,
        hotspot: (i32, i32),
        now_us: u64,
        buttons: Buttons,
    ) {
        // 1) 轨迹：位置变了就把**旧位置**记进历史（不记当前位置，避免与主光标重复）。
        if let Some((lx, ly)) = self.last_pos {
            // 只有开着轨迹、且位置真的变了才记：静止时每帧都塞一个重复点，
            // 会把 24 个采样槽瞬间占满，真正的移动反而留不下痕迹。
            if (lx, ly) != (pointer.x, pointer.y) && self.config.trail_ms > 0 {
                self.trail.push(TrailPoint {
                    x: lx,
                    y: ly,
                    at_us: now_us,
                });
                if self.trail.len() > TRAIL_MAX_POINTS {
                    self.trail.remove(0);
                }
            }
        }
        self.last_pos = Some((pointer.x, pointer.y));

        // 2) 点击：检测上升沿，落点加上热点偏移（箭头尖端才是用户以为点到的地方）。
        if self.config.click_effect {
            if (buttons.left && !self.buttons.left) || (buttons.right && !self.buttons.right) {
                self.ripples.push(Ripple {
                    x: pointer.x + hotspot.0,
                    y: pointer.y + hotspot.1,
                    start_us: now_us,
                });
            }
            self.buttons = buttons;
        }

        // 3) 清理过期状态。每次 observe 都清，避免长时间录制里无限堆积。
        let ripple_life = RIPPLE_DURATION_US;
        self.ripples
            .retain(|r| now_us.saturating_sub(r.start_us) < ripple_life);
        if self.config.trail_ms > 0 {
            let life = self.config.trail_ms as u64 * 1000;
            self.trail.retain(|p| now_us.saturating_sub(p.at_us) < life);
        } else {
            self.trail.clear();
        }
    }

    /// 是否还有动画在进行。
    ///
    /// 采集层必须用它在「桌面没变、指针也没动」时继续出帧——波纹扩散与残影淡出
    /// 都不需要任何外部变化来驱动，靠的是时间。
    pub fn is_animating(&self, now_us: u64) -> bool {
        if self
            .ripples
            .iter()
            .any(|r| now_us.saturating_sub(r.start_us) < RIPPLE_DURATION_US)
        {
            return true;
        }
        if self.config.trail_ms > 0 {
            let life = self.config.trail_ms as u64 * 1000;
            return self
                .trail
                .iter()
                .any(|p| now_us.saturating_sub(p.at_us) < life);
        }
        false
    }

    /// 生成这一帧要叠加的内容。`frame_w/frame_h` 用于算出重绘包围盒与面积上限。
    pub fn plan<'a>(
        &self,
        shape: Option<&'a CursorShape>,
        pointer: PointerState,
        now_us: u64,
        frame_w: u32,
        frame_h: u32,
    ) -> OverlayPlan<'a> {
        // 主光标：可见且形状就绪才画。
        let main = match (shape, pointer.visible) {
            (Some(_), true) => Some((pointer.x, pointer.y)),
            _ => None,
        };

        // 轨迹残影，旧的在前（先画），新的在后（盖住旧的）。
        let mut trail = Vec::new();
        if self.config.trail_ms > 0 {
            for p in &self.trail {
                let age = now_us.saturating_sub(p.at_us);
                let alpha = trail_alpha(age, self.config.trail_ms);
                if alpha > 0 {
                    trail.push((p.x, p.y, alpha));
                }
            }
        }

        // 波纹几何。
        let ripples: Vec<RippleShape> = self
            .ripples
            .iter()
            .filter_map(|r| {
                let age = now_us.saturating_sub(r.start_us);
                if age >= RIPPLE_DURATION_US {
                    return None;
                }
                let t = age as f32 / RIPPLE_DURATION_US as f32;
                let (radius, alpha) = ripple_at(t);
                if alpha == 0 {
                    return None;
                }
                Some(RippleShape {
                    x: r.x,
                    y: r.y,
                    radius,
                    alpha,
                    // 线宽随时间略变细，末端更像散去而不是被掐断。
                    thickness: 3.0 - 1.0 * t,
                })
            })
            .collect();

        let mut plan = OverlayPlan {
            shape,
            trail,
            ripples,
            main,
        };

        // 面积上限：轨迹是唯一会让重绘区域膨胀的东西，超限就从最旧的开始丢。
        // 轨迹按时间升序存放，所以「最旧」就在队首；循环最多跑 TRAIL_MAX_POINTS 次。
        let frame_px = frame_w as u64 * frame_h as u64;
        let limit = (frame_px / MAX_PLAN_FRAME_FRACTION)
            .max(MAX_PLAN_PIXELS)
            .min(frame_px);
        while !plan.trail.is_empty() {
            let over = match plan.bounds(frame_w, frame_h) {
                Some((_, _, w, h)) => w as u64 * h as u64 > limit,
                None => false,
            };
            if !over {
                break;
            }
            plan.trail.remove(0);
        }
        plan
    }
}

/// 测试辅助：让别的模块（如 GPU 合成）能注入一个波纹来验证「动画帧」这条路径。
///
/// 真实路径里波纹由 `GetAsyncKeyState` 的上升沿触发，测试环境不该去按真鼠标，
/// 所以这里开一个小口子。`cfg(test)` 下才存在，不进生产二进制。
#[cfg(test)]
impl CursorOverlay {
    pub(crate) fn push_ripple_for_test(&mut self, x: i32, y: i32, start_us: u64) {
        self.ripples.push(Ripple { x, y, start_us });
    }
}

/// 一帧要叠加到画面上的全部内容。
///
/// 它同时承担「是否开启指针合成」的信号：`FrameMeta.overlay` 为 `Some` 就代表这一帧
/// 要走「纯桌面副本 + 派生 + 叠加」那条路（GPU 侧需要两份纹理）。
#[derive(Debug, Clone)]
pub struct OverlayPlan<'a> {
    shape: Option<&'a CursorShape>,
    trail: Vec<(i32, i32, u8)>,
    ripples: Vec<RippleShape>,
    main: Option<(i32, i32)>,
}

impl<'a> OverlayPlan<'a> {
    /// 只有主光标、不带任何特效的最小构造（「只开指针」的场景与测试用）。
    pub fn cursor_only(shape: &'a CursorShape, pointer: PointerState) -> Self {
        Self {
            shape: Some(shape),
            trail: Vec::new(),
            ripples: Vec::new(),
            main: pointer.visible.then_some((pointer.x, pointer.y)),
        }
    }

    /// 没有任何东西要画（指针不可见、没有残影、没有波纹）。
    pub fn is_empty(&self) -> bool {
        self.main.is_none() && self.trail.is_empty() && self.ripples.is_empty()
    }

    /// 这一帧实际会改动的像素区域 `(x, y, w, h)`；完全落在画面外时返回 `None`。
    pub fn bounds(&self, frame_w: u32, frame_h: u32) -> Option<(u32, u32, u32, u32)> {
        let mut acc: Option<(i32, i32, i32, i32)> = None;
        let mut merge = |left: i32, top: i32, w: i32, h: i32| {
            let rect = (left, top, left + w, top + h);
            acc = Some(match acc {
                None => rect,
                Some((al, at, ar, ab)) => (
                    al.min(rect.0),
                    at.min(rect.1),
                    ar.max(rect.2),
                    ab.max(rect.3),
                ),
            });
        };

        if let (Some(shape), Some((x, y))) = (self.shape, self.main) {
            merge(x, y, shape.width() as i32, shape.height() as i32);
        }
        let (hx, hy) = self.shape.map_or((0, 0), |s| s.hotspot());
        // 流光光轨外沿半径约为 6.5px（核心 + 外发光余量），样条曲线外拱增加余量至 12px
        const TRAIL_PAD: i32 = 12;
        for &(x, y, _) in &self.trail {
            let px = x + hx;
            let py = y + hy;
            merge(px - TRAIL_PAD, py - TRAIL_PAD, TRAIL_PAD * 2, TRAIL_PAD * 2);
        }
        if let Some((mx, my)) = self.main {
            if !self.trail.is_empty() {
                let px = mx + hx;
                let py = my + hy;
                merge(px - TRAIL_PAD, py - TRAIL_PAD, TRAIL_PAD * 2, TRAIL_PAD * 2);
            }
        }
        for r in &self.ripples {
            let (x, y, w, h) = r.bounds();
            merge(x, y, w, h);
        }

        let (l, t, r, b) = acc?;
        // 裁到画面内：越界部分不该扩大回读范围。
        let l = l.max(0);
        let t = t.max(0);
        let r = r.min(frame_w as i32);
        let b = b.min(frame_h as i32);
        if r <= l || b <= t {
            return None;
        }
        Some((l as u32, t as u32, (r - l) as u32, (b - t) as u32))
    }

    /// 把内容画进画布。
    ///
    /// `canvas` 覆盖画面中的一块矩形，`origin` 是它的左上角在画面坐标系里的位置
    /// （CPU 路径传 `(0, 0)` 表示整帧，GPU 路径传回读包围盒的左上角）。
    /// 返回被改动过的像素数，调用方可据此判断这一帧是否真的需要重编码。
    ///
    /// 绘制顺序是**光轨 → 波纹 → 主光标**：波纹要压在流光上，主光标箭头置于最顶层
    /// （保证光轨紧随指针尖端流出，同时完全不遮挡光标指针）。
    pub fn draw_into(&self, canvas: &mut Canvas<'_>, origin: (i32, i32)) -> usize {
        let mut touched = 0;
        touched += draw_light_trail(canvas, origin, &self.trail, self.main, self.shape);
        for r in &self.ripples {
            touched += draw_ripple(canvas, origin, r);
        }
        if let (Some(shape), Some((x, y))) = (self.shape, self.main) {
            touched += shape.blend_into_alpha(canvas, x - origin.0, y - origin.1, 255);
        }
        touched
    }
}

/// 绘制流光光轨（Neon Streamer Light Trail）。
///
/// 沿着历史移动采样点平滑连接到当前鼠标尖端（热点），形成一束渐变流动的霓虹光轨：
/// - 头部（靠近鼠标尖端）：光束较粗，带有明亮的纯白光核与青蓝发光；
/// - 尾部（旧轨迹）：光束平滑收窄并淡出消散；
/// - 视觉层级：内芯高亮白 [255, 255, 255] + 外缘青蓝霓虹光 [255, 229, 0]（BGRA 字节序）+ 光晕柔化。
fn draw_light_trail(
    canvas: &mut Canvas<'_>,
    origin: (i32, i32),
    trail: &[(i32, i32, u8)],
    main: Option<(i32, i32)>,
    shape: Option<&CursorShape>,
) -> usize {
    if trail.is_empty() {
        return 0;
    }

    let (hx, hy) = shape.map_or((0, 0), |s| s.hotspot());

    #[derive(Debug, Clone, Copy)]
    struct TrailNode {
        x: f32,
        y: f32,
        radius: f32,
        alpha: f32,
    }

    let mut nodes = Vec::with_capacity(trail.len() + 1);
    for &(tx, ty, a) in trail {
        let norm_a = a as f32 / 255.0;
        let radius = 0.8 + 2.8 * norm_a;
        let alpha = (a as f32 * 0.9).clamp(0.0, 255.0);
        nodes.push(TrailNode {
            x: (tx + hx) as f32,
            y: (ty + hy) as f32,
            radius,
            alpha,
        });
    }

    if let Some((mx, my)) = main {
        let head_x = (mx + hx) as f32;
        let head_y = (my + hy) as f32;
        let should_add = match nodes.last() {
            Some(last) => {
                let dx = head_x - last.x;
                let dy = head_y - last.y;
                dx * dx + dy * dy >= 1.0
            }
            None => true,
        };
        if should_add {
            nodes.push(TrailNode {
                x: head_x,
                y: head_y,
                radius: 3.8,
                alpha: 220.0,
            });
        }
    }

    if nodes.is_empty() {
        return 0;
    }

    // 使用 Chaikin 拐角细分平滑算法：将折线自然过渡为连续圆润的流线，且绝对不产生样条外拱自交
    let curved_nodes = if nodes.len() >= 3 {
        let mut cur = nodes;
        for _ in 0..2 {
            let mut next = Vec::with_capacity(cur.len() * 2);
            if let Some(first) = cur.first() {
                next.push(*first);
            }
            for i in 0..cur.len() - 1 {
                let p0 = &cur[i];
                let p1 = &cur[i + 1];
                next.push(TrailNode {
                    x: 0.75 * p0.x + 0.25 * p1.x,
                    y: 0.75 * p0.y + 0.25 * p1.y,
                    radius: 0.75 * p0.radius + 0.25 * p1.radius,
                    alpha: 0.75 * p0.alpha + 0.25 * p1.alpha,
                });
                next.push(TrailNode {
                    x: 0.25 * p0.x + 0.75 * p1.x,
                    y: 0.25 * p0.y + 0.75 * p1.y,
                    radius: 0.25 * p0.radius + 0.75 * p1.radius,
                    alpha: 0.25 * p0.alpha + 0.75 * p1.alpha,
                });
            }
            if let Some(last) = cur.last() {
                next.push(*last);
            }
            cur = next;
        }
        cur
    } else {
        nodes
    };

    let w = canvas.width as i32;
    let h = canvas.height as i32;
    let stride = canvas.stride;
    let dst: &mut [u8] = canvas.buf;
    let reachable_rows = (dst.len() / stride) as i32;
    let h_limit = h.min(reachable_rows);

    let mut touched = 0usize;

    let seg_count = if curved_nodes.len() == 1 { 1 } else { curved_nodes.len() - 1 };
    let glow_pad = 2.4f32;

    #[derive(Clone, Copy)]
    struct Segment {
        x0: f32,
        y0: f32,
        dx: f32,
        dy: f32,
        len_sq: f32,
        r0: f32,
        r1: f32,
        a0: f32,
        a1: f32,
        min_x: f32,
        max_x: f32,
        min_y: f32,
        max_y: f32,
    }

    let mut segments = Vec::with_capacity(seg_count);
    let mut total_min_x = f32::MAX;
    let mut total_max_x = f32::MIN;
    let mut total_min_y = f32::MAX;
    let mut total_max_y = f32::MIN;

    for i in 0..seg_count {
        let p0 = &curved_nodes[i];
        let p1 = if curved_nodes.len() == 1 { p0 } else { &curved_nodes[i + 1] };

        let dx = p1.x - p0.x;
        let dy = p1.y - p0.y;
        let len_sq = dx * dx + dy * dy;

        let max_r = p0.radius.max(p1.radius);
        let reach = max_r + glow_pad + 0.5;

        let min_x = p0.x.min(p1.x) - reach;
        let max_x = p0.x.max(p1.x) + reach;
        let min_y = p0.y.min(p1.y) - reach;
        let max_y = p0.y.max(p1.y) + reach;

        total_min_x = total_min_x.min(min_x);
        total_max_x = total_max_x.max(max_x);
        total_min_y = total_min_y.min(min_y);
        total_max_y = total_max_y.max(max_y);

        segments.push(Segment {
            x0: p0.x,
            y0: p0.y,
            dx,
            dy,
            len_sq,
            r0: p0.radius,
            r1: p1.radius,
            a0: p0.alpha,
            a1: p1.alpha,
            min_x,
            max_x,
            min_y,
            max_y,
        });
    }

    // 换算到画布坐标系并求交裁剪
    let x0 = ((total_min_x - origin.0 as f32).floor() as i32).max(0);
    let y0 = ((total_min_y - origin.1 as f32).floor() as i32).max(0);
    let x1 = ((total_max_x - origin.0 as f32).ceil() as i32 + 1).min(w);
    let y1 = ((total_max_y - origin.1 as f32).ceil() as i32 + 1).min(h_limit);

    if x1 <= x0 || y1 <= y0 {
        return 0;
    }

    for ty in y0..y1 {
        let row_off = ty as usize * stride;
        let py = ty as f32 + origin.1 as f32 + 0.5;
        for tx in x0..x1 {
            let px = tx as f32 + origin.0 as f32 + 0.5;

            // 遍历所有相交线段，挑选视觉贡献最强（最高 Alpha / 核心度）的线段，严格单次合成
            let mut best_color: Option<([u8; 3], f32)> = None;

            for seg in &segments {
                if px < seg.min_x || px > seg.max_x || py < seg.min_y || py > seg.max_y {
                    continue;
                }

                let (t, dist_sq) = if seg.len_sq < 0.001 {
                    let d2 = (px - seg.x0) * (px - seg.x0) + (py - seg.y0) * (py - seg.y0);
                    (0.0, d2)
                } else {
                    let wx = px - seg.x0;
                    let wy = py - seg.y0;
                    let c1 = wx * seg.dx + wy * seg.dy;
                    let t = (c1 / seg.len_sq).clamp(0.0, 1.0);
                    let qx = seg.x0 + t * seg.dx;
                    let qy = seg.y0 + t * seg.dy;
                    let d2 = (px - qx) * (px - qx) + (py - qy) * (py - qy);
                    (t, d2)
                };

                let r = seg.r0 + t * (seg.r1 - seg.r0);
                let r_glow = r + glow_pad;
                if dist_sq > r_glow * r_glow {
                    continue;
                }

                let dist = dist_sq.sqrt();
                let a_seg = seg.a0 + t * (seg.a1 - seg.a0);
                let r_core = r * 0.45;

                let (color, alpha) = if dist <= r_core {
                    // 内芯：纯白高亮
                    ([255u8, 255, 255], a_seg)
                } else if dist <= r {
                    // 核心到光柱边缘：白 -> 青蓝 [255, 229, 0] (BGRA)
                    let u = (dist - r_core) / (r - r_core);
                    let g = (255.0 - 26.0 * u).round() as u8;
                    let r_ch = (255.0 * (1.0 - u)).round() as u8;
                    ([255u8, g, r_ch], a_seg)
                } else {
                    // 光晕：青蓝色渐隐软过渡
                    let u = (dist - r) / glow_pad;
                    let cov = (1.0 - u).clamp(0.0, 1.0);
                    let smooth_cov = cov * cov * (3.0 - 2.0 * cov);
                    ([255u8, 229, 0], a_seg * smooth_cov * 0.7)
                };

                if alpha <= 0.5 {
                    continue;
                }

                match &mut best_color {
                    None => best_color = Some((color, alpha)),
                    Some((_, best_a)) if alpha > *best_a => {
                        best_color = Some((color, alpha));
                    }
                    _ => {}
                }
            }

            if let Some((color, alpha)) = best_color {
                let off = row_off + tx as usize * 4;
                if off + 3 < dst.len() {
                    let a = alpha.min(255.0);
                    let inv = 255.0 - a;
                    for c in 0..3 {
                        dst[off + c] = ((color[c] as f32 * a + dst[off + c] as f32 * inv) / 255.0)
                            .round()
                            .clamp(0.0, 255.0) as u8;
                    }
                    dst[off + 3] = 0xFF;
                    touched += 1;
                }
            }
        }
    }

    touched
}

/// 画一个圆环：先用深色画粗一圈做描边，再用亮色画细一圈。
///
/// 为什么要两层：单色圆环在某些背景上会消失在背景里（白圈落在浅色文档上、
/// 黑圈落在深色终端上）。深色描边 + 亮色主体的组合在两边都能看清，代价只是
/// 每个像素多算一次距离——区域本来就只有几十像素见方。
fn draw_ripple(canvas: &mut Canvas<'_>, origin: (i32, i32), ripple: &RippleShape) -> usize {
    let (w, h) = (canvas.width, canvas.height);
    let stride = canvas.stride;
    let (bx, by, bw, bh) = ripple.bounds();
    // 换算到缓冲区坐标后与缓冲区求交。
    let x0 = (bx - origin.0).max(0);
    let y0 = (by - origin.1).max(0);
    let x1 = (bx - origin.0 + bw).min(w as i32);
    let y1 = (by - origin.1 + bh).min(h as i32);
    if x1 <= x0 || y1 <= y0 {
        return 0;
    }
    let dst: &mut [u8] = canvas.buf;
    let reachable_rows = (dst.len() / stride) as i32;
    let y1 = y1.min(reachable_rows);
    if y1 <= y0 {
        return 0;
    }

    let cx = ripple.x as f32;
    let cy = ripple.y as f32;
    let mut touched = 0usize;

    for ty in y0..y1 {
        let row = ty as usize * stride;
        for tx in x0..x1 {
            let dx = (tx as f32 + 0.5) - (cx - origin.0 as f32);
            let dy = (ty as f32 + 0.5) - (cy - origin.1 as f32);
            let dist = (dx * dx + dy * dy).sqrt();
            let dist_to_ring = (dist - ripple.radius).abs();

            // 描边层：比主体宽 1.5 px、更暗，只在主体外侧露出一点。
            let outer_half = (ripple.thickness * 0.5) + 1.5;
            let inner_half = ripple.thickness * 0.5;
            let off = row + tx as usize * 4;
            if off + 3 >= dst.len() {
                continue;
            }

            let outer_cov = coverage(dist_to_ring, outer_half);
            let inner_cov = coverage(dist_to_ring, inner_half);
            if outer_cov <= 0.0 {
                continue;
            }

            // 描边用深色，主体用亮色；两者按覆盖率加权到同一个像素上。
            let dark = [0u8, 0, 0];
            let light = [255u8, 255, 255];
            let a_outer = ripple.alpha as f32 * outer_cov * 0.55;
            let a_inner = ripple.alpha as f32 * inner_cov;
            // 先描边再主体：等价于把两种覆盖按「主体优先」合成。
            let (color, alpha) = if a_inner > 0.0 {
                (light, a_inner)
            } else {
                (dark, a_outer)
            };
            if alpha <= 0.5 {
                continue;
            }
            let a = alpha.min(255.0);
            let inv = 255.0 - a;
            for c in 0..3 {
                dst[off + c] = ((color[c] as f32 * a + dst[off + c] as f32 * inv) / 255.0)
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
            dst[off + 3] = 0xFF;
            touched += 1;
        }
    }
    touched
}

/// 像素落在环带内的覆盖率（0～1），边缘 1 px 软过渡以消除锯齿。
fn coverage(dist_to_ring: f32, half_width: f32) -> f32 {
    (half_width + 0.5 - dist_to_ring).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color_shape(w: u32, h: u32) -> CursorShape {
        use windows::Win32::Graphics::Dxgi::{
            DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
        };
        let pitch = w * 4;
        let data = vec![0x00u8; (pitch * h) as usize];
        let info = DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
            Width: w,
            Height: h,
            Pitch: pitch,
            HotSpot: windows::Win32::Foundation::POINT { x: 0, y: 0 },
        };
        CursorShape::parse(info.Type, &info, &data).unwrap()
    }

    /// 波纹半径必须单调增大、alpha 必须单调减小。
    ///
    /// 这两条任一搞反，现象分别是「波纹倒着缩回去」和「越扩散越实」——
    /// 都只有在播放录像时才看得出来，单帧截图永远正常。
    #[test]
    fn ripple_expands_and_fades_monotonically() {
        let mut prev_radius = -1.0f32;
        let mut prev_alpha = u8::MAX;
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let (radius, alpha) = ripple_at(t);
            assert!(
                radius >= prev_radius,
                "半径必须单调递增：t={t} 时 {radius} < {prev_radius}"
            );
            assert!(
                alpha <= prev_alpha,
                "alpha 必须单调递减：t={t} 时 {alpha} > {prev_alpha}"
            );
            prev_radius = radius;
            prev_alpha = alpha;
        }
    }

    /// 波纹的起止必须落在设计的半径区间内，且两端 alpha 合理。
    #[test]
    fn ripple_endpoints_match_design() {
        let (r0, a0) = ripple_at(0.0);
        assert!((r0 - RIPPLE_START_RADIUS).abs() < 0.01);
        assert!(a0 > 200, "起始应足够明显，实际 {a0}");

        let (r1, a1) = ripple_at(1.0);
        assert!((r1 - RIPPLE_END_RADIUS).abs() < 0.01);
        assert_eq!(a1, 0, "结束时必须完全透明");
    }

    /// 超出 [0,1] 的 t 必须被夹住，不能算出负半径或溢出。
    #[test]
    fn ripple_clamps_out_of_range_time() {
        assert_eq!(ripple_at(-5.0), ripple_at(0.0));
        assert_eq!(ripple_at(5.0), ripple_at(1.0));
    }

    /// 残影：越旧越淡，超过存活期直接为 0。
    #[test]
    fn trail_alpha_decays_with_age() {
        let ms = 200u32;
        assert!(trail_alpha(0, ms) > trail_alpha(50_000, ms));
        assert!(trail_alpha(50_000, ms) > trail_alpha(150_000, ms));
        assert_eq!(trail_alpha(200_000, ms), 0, "到期后必须为 0");
        assert_eq!(trail_alpha(10_000, 0), 0, "关闭时恒为 0");
    }

    /// 关闭轨迹时不应该积累任何采样点。
    #[test]
    fn disabled_trail_collects_nothing() {
        let mut ov = CursorOverlay::new(OverlayConfig {
            click_effect: false,
            trail_ms: 0,
        });
        for i in 0..10 {
            ov.observe(
                PointerState {
                    visible: true,
                    x: i * 5,
                    y: 0,
                },
                (0, 0),
                i as u64 * 33_000,
            );
        }
        let shape = color_shape(8, 8);
        let plan = ov.plan(
            Some(&shape),
            PointerState {
                visible: true,
                x: 45,
                y: 0,
            },
            330_000,
            320,
            240,
        );
        assert!(plan.trail.is_empty(), "轨迹关闭时不应有残影");
    }

    /// 移动时要把**旧位置**记进历史，而不是当前位置（否则会与主光标重复绘制）。
    #[test]
    fn trail_records_previous_positions() {
        let mut ov = CursorOverlay::new(OverlayConfig {
            click_effect: false,
            trail_ms: 1_000,
        });
        ov.observe(
            PointerState {
                visible: true,
                x: 10,
                y: 10,
            },
            (0, 0),
            0,
        );
        ov.observe(
            PointerState {
                visible: true,
                x: 20,
                y: 10,
            },
            (0, 0),
            33_000,
        );
        ov.observe(
            PointerState {
                visible: true,
                x: 30,
                y: 10,
            },
            (0, 0),
            66_000,
        );

        let shape = color_shape(8, 8);
        let plan = ov.plan(
            Some(&shape),
            PointerState {
                visible: true,
                x: 30,
                y: 10,
            },
            66_000,
            320,
            240,
        );
        let xs: Vec<i32> = plan.trail.iter().map(|p| p.0).collect();
        assert_eq!(xs, vec![10, 20], "历史应只含走过的位置：{xs:?}");
        assert_eq!(plan.main, Some((30, 10)));
    }

    /// 轨迹残影会让重绘面积膨胀，超限时必须**从最旧的开始丢**（而不是整条丢弃）。
    ///
    /// 为什么强调「从最旧」：最旧的残影最淡，丢掉它视觉上几乎无感；一刀切掉整条
    /// 轨迹的现象是「快速拖动时拖尾整条突然消失」，比没有残影更显眼。
    #[test]
    fn oversized_plan_trims_oldest_trail_points_first() {
        let mut ov = CursorOverlay::new(OverlayConfig {
            click_effect: false,
            trail_ms: 2_000,
        });
        // 斜向拉一条几乎覆盖整屏的轨迹：外接矩形会远超上限
        for i in 0..20 {
            ov.observe(
                PointerState {
                    visible: true,
                    x: i * 33,
                    y: i * 24,
                },
                (0, 0),
                i as u64 * 20_000,
            );
        }
        let shape = color_shape(32, 32);
        let pointer = PointerState {
            visible: true,
            x: 627,
            y: 456,
        };
        let now = 19 * 20_000;

        let collected = ov.trail.len();
        assert!(collected >= 19, "观察阶段应当确实收集到轨迹：{collected}");

        let plan = ov.plan(Some(&shape), pointer, now, 640, 480);
        let bound = plan.bounds(640, 480).expect("至少主光标还在");
        let limit = (640u64 * 480 / MAX_PLAN_FRAME_FRACTION).max(MAX_PLAN_PIXELS);
        assert!(
            bound.2 as u64 * bound.3 as u64 <= limit,
            "裁剪后包围盒必须回到上限内：{bound:?} > {limit}"
        );
        assert!(
            plan.trail.len() < collected,
            "超限时必须裁掉轨迹点：原 {collected}，裁后 {}",
            plan.trail.len()
        );
        // 保留下来的必须是最新的那一批（最旧的先被丢）
        if let Some(&(x, _, _)) = plan.trail.first() {
            let lost = collected - plan.trail.len();
            assert_eq!(x, lost as i32 * 33, "应从最旧端裁剪，实际首点 x={x}");
        }
        assert_eq!(plan.main, Some((627, 456)), "主光标永不丢");

        // 小范围移动时轨迹应当完整保留
        let mut ov = CursorOverlay::new(OverlayConfig {
            click_effect: false,
            trail_ms: 2_000,
        });
        for i in 0..5 {
            ov.observe(
                PointerState {
                    visible: true,
                    x: 100 + i * 6,
                    y: 100,
                },
                (0, 0),
                i as u64 * 20_000,
            );
        }
        let plan = ov.plan(
            Some(&shape),
            PointerState {
                visible: true,
                x: 124,
                y: 100,
            },
            80_000,
            640,
            480,
        );
        assert_eq!(plan.trail.len(), 4, "小范围移动不该被裁剪");
    }

    /// 动画期间必须报告「还在动」，否则采集层会把帧停掉、动画冻在半路。
    #[test]
    fn animating_while_ripple_or_trail_alive() {
        let mut ov = CursorOverlay::new(OverlayConfig {
            click_effect: true,
            trail_ms: 0,
        });
        ov.observe(
            PointerState {
                visible: true,
                x: 5,
                y: 5,
            },
            (0, 0),
            0,
        );
        assert!(!ov.is_animating(0), "没有点击时不该报告动画");

        // 手动塞一个波纹（绕过真实按键，测试只关心时间推进）
        ov.ripples.push(Ripple {
            x: 5,
            y: 5,
            start_us: 1_000,
        });
        assert!(ov.is_animating(1_000), "刚点击应当还在动");
        assert!(
            ov.is_animating(1_000 + RIPPLE_DURATION_US - 1),
            "到期前应当还在动"
        );
        assert!(
            !ov.is_animating(1_000 + RIPPLE_DURATION_US),
            "到期后应当停止"
        );
    }

    /// 点击的**上升沿**才产生波纹：落点还要加上热点偏移。
    ///
    /// 两个容易各错一半的点都在这里：
    /// 1. 判的是「由未按到按下」而不是「当前按着」——否则按住鼠标会每帧冒一圈。
    /// 2. 落点是 `光标左上角 + HotSpot`。画光标时不能用热点（见 [`super::cursor`]），
    ///    但点击位置必须用它：用户以为点到的是箭头尖端，不是图片左上角的空白。
    #[test]
    fn click_makes_one_ripple_at_hotspot_per_press() {
        let mut ov = CursorOverlay::new(OverlayConfig {
            click_effect: true,
            trail_ms: 0,
        });
        let at = PointerState {
            visible: true,
            x: 100,
            y: 200,
        };
        // 热点是箭头尖端的偏移，(2, 3) 表示尖端点比图像左上角右下各偏 2、3 像素。
        let hotspot = (2, 3);
        let idle = Buttons::default();
        let pressed = Buttons {
            left: true,
            right: false,
        };

        // 未按下：不该有波纹
        ov.observe_with_buttons(at, hotspot, 0, idle);
        assert!(ov.ripples.is_empty(), "没按就不该有波纹");

        // 按下（上升沿）：冒一圈，位置是光标左上角 + 热点
        ov.observe_with_buttons(at, hotspot, 33_000, pressed);
        assert_eq!(ov.ripples.len(), 1, "按下应当产生一圈波纹");
        assert_eq!(
            (ov.ripples[0].x, ov.ripples[0].y),
            (102, 203),
            "落点应为热点"
        );

        // 一直按着：不能每帧再冒一圈（这是最容易写错的地方）
        ov.observe_with_buttons(at, hotspot, 66_000, pressed);
        ov.observe_with_buttons(at, hotspot, 99_000, pressed);
        assert_eq!(ov.ripples.len(), 1, "按住期间不该重复产生波纹");

        // 松开再按：第二圈。此时第一圈已经过期，过期清理会把它剔掉。
        ov.observe_with_buttons(at, hotspot, 400_000, idle);
        ov.observe_with_buttons(at, hotspot, 433_000, pressed);
        assert_eq!(ov.ripples.len(), 1, "第二轮点击应有且只有一圈新波纹");
        assert_eq!(ov.ripples[0].start_us, 433_000);
    }

    /// 右键点击同样算点击（右键菜单也是用户想让人看见的动作）。
    #[test]
    fn right_click_also_makes_a_ripple() {
        let mut ov = CursorOverlay::new(OverlayConfig {
            click_effect: true,
            trail_ms: 0,
        });
        let at = PointerState {
            visible: true,
            x: 10,
            y: 10,
        };
        ov.observe_with_buttons(at, (0, 0), 0, Buttons::default());
        ov.observe_with_buttons(
            at,
            (0, 0),
            33_000,
            Buttons {
                left: false,
                right: true,
            },
        );
        assert_eq!(ov.ripples.len(), 1);
    }

    /// 关掉点击特效时，即使按键快照在变也不该产生波纹。
    #[test]
    fn disabled_click_effect_ignores_buttons() {
        let mut ov = CursorOverlay::new(OverlayConfig {
            click_effect: false,
            trail_ms: 0,
        });
        let at = PointerState {
            visible: true,
            x: 10,
            y: 10,
        };
        ov.observe_with_buttons(at, (0, 0), 0, Buttons::default());
        ov.observe_with_buttons(
            at,
            (0, 0),
            33_000,
            Buttons {
                left: true,
                right: true,
            },
        );
        assert!(ov.ripples.is_empty(), "特效关闭时不该有任何波纹");
    }

    /// 波纹画在预期位置，不画的地方保持原样。
    #[test]
    fn ripple_draws_a_ring_and_leaves_the_rest() {
        let w = 160u32;
        let h = 160u32;
        let mut buf = vec![0x20u8; (w * h * 4) as usize];
        let ripple = RippleShape {
            x: 80,
            y: 80,
            radius: 30.0,
            alpha: 255,
            thickness: 3.0,
        };
        let touched = draw_ripple(
            &mut Canvas::new(&mut buf, (w * 4) as usize, w, h),
            (0, 0),
            &ripple,
        );
        assert!(touched > 0, "圆环应当画出像素");

        let px = |x: u32, y: u32| {
            let off = (y * w * 4 + x * 4) as usize;
            buf[off]
        };
        // 环上（半径 30 处）应当被点亮/压暗，圆心与远处保持原样。
        let ring = px(80 + 30, 80);
        assert_ne!(ring, 0x20, "环上的像素应被改写");
        assert_eq!(px(80, 80), 0x20, "圆心不应被涂");
        assert_eq!(px(80, 10), 0x20, "远离圆环处不应被涂");
    }

    /// 越界的波纹不写任何像素，也不 panic。
    #[test]
    fn ripple_outside_frame_writes_nothing() {
        let w = 32u32;
        let h = 32u32;
        let mut buf = vec![0x11u8; (w * h * 4) as usize];
        let ripple = RippleShape {
            x: 500,
            y: 500,
            radius: 10.0,
            alpha: 255,
            thickness: 2.0,
        };
        assert_eq!(
            draw_ripple(
                &mut Canvas::new(&mut buf, (w * 4) as usize, w, h),
                (0, 0),
                &ripple
            ),
            0
        );
        assert!(buf.iter().all(|b| *b == 0x11));
    }

    /// 指针不可见时，主光标不该出现，但波纹仍要画（点击反馈与指针可见性无关）。
    #[test]
    fn invisible_pointer_still_shows_ripple() {
        let shape = color_shape(8, 8);
        let plan = OverlayPlan {
            shape: Some(&shape),
            trail: Vec::new(),
            ripples: vec![RippleShape {
                x: 50,
                y: 50,
                radius: 10.0,
                alpha: 200,
                thickness: 3.0,
            }],
            main: None,
        };
        assert!(!plan.is_empty());
        let (x, y, w, h) = plan.bounds(320, 240).unwrap();
        assert!(x <= 50 && y <= 50 && x + w >= 51 && y + h >= 51);
    }

    /// 空计划要如实报告为空，采集层据此跳过重编码。
    #[test]
    fn empty_plan_is_detected() {
        let plan = OverlayPlan {
            shape: None,
            trail: Vec::new(),
            ripples: Vec::new(),
            main: None,
        };
        assert!(plan.is_empty());
        assert_eq!(plan.bounds(320, 240), None);
    }

    /// `bounds` 必须是所有元素的并集，且裁到画面内。
    #[test]
    fn bounds_is_union_and_clipped() {
        let shape = color_shape(16, 16);
        let plan = OverlayPlan {
            shape: Some(&shape),
            trail: vec![(10, 10, 100)],
            ripples: vec![RippleShape {
                x: 100,
                y: 100,
                radius: 20.0,
                alpha: 200,
                thickness: 3.0,
            }],
            main: Some((50, 50)),
        };
        let (x, y, w, h) = plan.bounds(320, 240).unwrap();
        assert!(x <= 10 && y <= 10, "应包含最左上角的残影：{x},{y}");
        assert!(x + w >= 120 && y + h >= 120, "应包含波纹：{w}x{h}");

        // 画面外的部分不该扩大包围盒
        let plan = OverlayPlan {
            shape: Some(&shape),
            trail: Vec::new(),
            ripples: Vec::new(),
            main: Some((310, 230)),
        };
        let (x, y, w, h) = plan.bounds(320, 240).unwrap();
        assert!(x + w <= 320 && y + h <= 240, "必须裁到画面内：{x}+{w}");
    }

    /// 流光光轨：能正确在线段周围绘制渐变与发光，且远处不受影响。
    #[test]
    fn light_trail_draws_streamer_and_tapers() {
        let w = 100u32;
        let h = 100u32;
        let mut buf = vec![0x10u8; (w * h * 4) as usize];
        let shape = color_shape(8, 8);
        let plan = OverlayPlan {
            shape: Some(&shape),
            trail: vec![(20, 20, 100), (35, 35, 180)],
            ripples: Vec::new(),
            main: Some((50, 50)),
        };
        let mut canvas = Canvas::new(&mut buf, (w * 4) as usize, w, h);
        let touched = plan.draw_into(&mut canvas, (0, 0));
        assert!(touched > 0, "流光光轨应绘制像素");

        // 中间路径 (35, 35) 附近应当被点亮
        let px = |x: u32, y: u32| {
            let off = (y * w * 4 + x * 4) as usize;
            (buf[off], buf[off + 1], buf[off + 2]) // (B, G, R)
        };
        let mid = px(35, 35);
        assert_ne!(mid, (0x10, 0x10, 0x10), "光轨路径点应被着色");
        // 远离光轨的像素保持原样
        assert_eq!(px(90, 10), (0x10, 0x10, 0x10), "远离光轨处不应被修改");
    }
}
