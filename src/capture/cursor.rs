//! 鼠标指针解析与合成（DXGI 桌面复制的「指针通道」）。
//!
//! ## 为什么必须自己合成
//!
//! `IDXGIOutputDuplication` 交付的桌面图像里**没有鼠标指针**。指针走的是独立的
//! 旁路通道：`DXGI_OUTDUPL_FRAME_INFO` 里带 `PointerPosition`（位置 + 可见性）和
//! `PointerShapeBufferSize`（形状是否变化），形状本身要再调
//! `GetFramePointerShape` 现取。想让录制里有指针，就得自己把形状画到画面上。
//!
//! ## 五个容易踩空的契约（每条都实测/查证过）
//!
//! 1. **`Position` 是光标图像的左上角，不是热点。** 官方文档对此有明确的一句：
//!    「应用程序在 `Position` 指定的位置绘制光标形状的左上角；**不使用**热点」。
//!    按常识减掉 `HotSpot` 会让指针整体偏移，且偏移量随光标样式变化（箭头是 (0,0)，
//!    文本 I 型是 (4,8) 之类），看起来像「偶发错位」。
//! 2. **不是每帧都带指针更新。** 只有 `LastMouseUpdateTime != 0` 的帧里
//!    `PointerPosition` 才有效；其余帧该字段是 0（读出来就是 `(0,0)` + 不可见）。
//!    若每帧都直接采信，鼠标一停下指针就会跳到屏幕左上角并消失。必须**持久化**。
//! 3. **`PointerShapeBufferSize > 0` 才表示形状变了。** 形状没变时它为 0，
//!    这时不该再调 `GetFramePointerShape`（而且该方法是 `DXGI_ERROR_INVALID_CALL`
//!    的高发点——必须在持有帧期间调用）。
//! 4. **单色光标是 1bpp 双掩码，不是 BGRA。** 缓冲区布局是「AND 掩码
//!    `Height` 行，紧接 XOR 掩码 `Height` 行」，行距都是 `Pitch`。把它当 32bpp
//!    解读会得到一片花屏。
//! 5. **`DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR` 的枚举值是 4，不是 3。**
//!    DXGI 的枚举在这里跳过了 3，凭记忆写 `3` 会静默落到「未知类型」分支。
//!
//! ## 三种形状的合成规则
//!
//! | 类型 | 数据 | 合成 |
//! |---|---|---|
//! | `MONOCHROME` | 1bpp AND 掩码 + 1bpp XOR 掩码 | `out = (screen & AND) ^ XOR`，逐通道按 0x00/0xFF 展开 |
//! | `COLOR` | 32bpp BGRA（带 alpha） | 直通 alpha 混合 `out = src*a + screen*(1-a)` |
//! | `MASKED_COLOR` | 32bpp BGRA（alpha 当掩码） | `a < 0x80` → 直接用光标色；`a >= 0x80` → `screen ^ 光标色` |
//!
//! 单色掩码的四种组合正好对应 `AND/XOR` 的语义：`(1,0)` 透出桌面、`(1,1)` 反色、
//! `(0,0)` 纯黑、`(0,1)` 纯白。反色分支是**唯一必须读到桌面像素才能算**的情况，
//! 这也是 GPU 路径不能只贴一张「光标纹理」的原因（见 `mf_sink` 的包围盒回读方案）。

use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

/// 指针形状的像素载荷（已从 DXGI 的三种格式归一化）。
#[derive(Debug, Clone)]
enum CursorPixels {
    /// 1bpp，两个掩码各 `height` 行、行距 `pitch` 字节
    Mono {
        and_mask: Vec<u8>,
        xor_mask: Vec<u8>,
        pitch: usize,
    },
    /// 32bpp BGRA，alpha 参与混合
    Color { bgra: Vec<u8>, pitch: usize },
    /// 32bpp BGRA，alpha 当掩码用
    MaskedColor { bgra: Vec<u8>, pitch: usize },
}

/// 一次解析好的指针形状。形状变化时才重新解析，之后每帧复用。
#[derive(Debug, Clone)]
pub struct CursorShape {
    width: u32,
    height: u32,
    /// 热点（相对左上角）。**仅作信息透出**，合成时不用——见模块注释第 1 条。
    hotspot_x: i32,
    hotspot_y: i32,
    pixels: CursorPixels,
}

impl CursorShape {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn hotspot(&self) -> (i32, i32) {
        (self.hotspot_x, self.hotspot_y)
    }

    /// 从 `GetFramePointerShape` 的原始返回解析。
    ///
    /// `shape_type` 是 `DXGI_OUTDUPL_POINTER_SHAPE_INFO.Type`（`u32`）；
    /// `data` 是形状缓冲区，长度即 `PointerShapeBufferSize`。
    ///
    /// 任何自洽性检查不过（尺寸为 0、行距装不下一行、缓冲区比声明的矮）都返回
    /// `None` 而不是 panic：坏形状顶多让这一帧没有指针，不该拖垮整段录制。
    pub fn parse(
        shape_type: u32,
        info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        data: &[u8],
    ) -> Option<Self> {
        let width = info.Width;
        let declared_h = info.Height as usize;
        if width == 0 || declared_h == 0 || data.is_empty() {
            return None;
        }
        let (hotspot_x, hotspot_y) = (info.HotSpot.x, info.HotSpot.y);

        if shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
            let pitch = info.Pitch as usize;
            // 单行至少要 1 字节，且必须装得下 width 个 bit。
            if pitch == 0 || pitch * 8 < width as usize {
                return None;
            }
            let rows = data.len() / pitch;
            // 理论上 rows == 2 * declared_h；个别驱动会把 Height 直接给成两倍，
            // 这里以「缓冲区实际够高」为准，两种都能吃下。
            let height = if declared_h * 2 <= rows {
                declared_h
            } else {
                rows / 2
            };
            if height == 0 {
                return None;
            }
            let need = pitch * height;
            if data.len() < need * 2 {
                return None;
            }
            Some(Self {
                width,
                height: height as u32,
                hotspot_x,
                hotspot_y,
                pixels: CursorPixels::Mono {
                    and_mask: data[..need].to_vec(),
                    xor_mask: data[need..need * 2].to_vec(),
                    pitch,
                },
            })
        } else if shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32
            || shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32
        {
            let row_bytes = width as usize * 4;
            let pitch = if info.Pitch == 0 {
                row_bytes
            } else {
                info.Pitch as usize
            };
            if pitch < row_bytes || data.len() < pitch * declared_h {
                return None;
            }
            let bytes = data[..pitch * declared_h].to_vec();
            let masked = shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
            Some(Self {
                width,
                height: declared_h as u32,
                hotspot_x,
                hotspot_y,
                pixels: if masked {
                    CursorPixels::MaskedColor { bgra: bytes, pitch }
                } else {
                    CursorPixels::Color { bgra: bytes, pitch }
                },
            })
        } else {
            // 未知类型（未来新增 / 驱动异常）：当作没有指针。
            None
        }
    }

    /// 把光标合成到 BGRA 画面上的 `(dst_x, dst_y)` 处。
    ///
    /// `dst` 是 `dst_h` 行、每行 `dst_stride` 字节的区域画面（紧凑行距时
    /// `dst_stride == dst_w * 4`）。`dst_x/dst_y` 是**光标左上角**在 `dst` 坐标系里的
    /// 位置，允许为负或越界——落在画面外的部分会被裁掉，不做任何假设。
    ///
    /// 返回实际参与合成的像素数（0 表示完全在画面外），便于上层判断「这帧是否
    /// 真的需要重编码」。
    pub fn blend_into(
        &self,
        dst: &mut [u8],
        dst_stride: usize,
        dst_w: u32,
        dst_h: u32,
        dst_x: i32,
        dst_y: i32,
    ) -> usize {
        if dst_stride < dst_w as usize * 4 {
            return 0;
        }
        let reachable_h = dst.len() / dst_stride;
        let rows = (dst_h as usize).min(reachable_h);
        if rows == 0 {
            return 0;
        }

        // 与画面求交，得到在光标局部坐标里的取样范围。
        let x0 = dst_x.max(0);
        let y0 = dst_y.max(0);
        let x1 = (dst_x + self.width as i32).min(dst_w as i32);
        let y1 = (dst_y + self.height as i32).min(rows as i32);
        if x1 <= x0 || y1 <= y0 {
            return 0;
        }

        let mut touched = 0usize;
        for ty in y0..y1 {
            let sy = (ty - dst_y) as usize;
            let row_off = ty as usize * dst_stride;
            for tx in x0..x1 {
                let sx = (tx - dst_x) as usize;
                let off = row_off + tx as usize * 4;
                // BGRA = 4 字节，缓冲区按行构造，越界一律跳过而不是 panic。
                if off + 3 >= dst.len() {
                    continue;
                }
                let px = [dst[off], dst[off + 1], dst[off + 2]];
                let Some(out) = self.sample(sx, sy, px) else {
                    continue;
                };
                dst[off] = out[0];
                dst[off + 1] = out[1];
                dst[off + 2] = out[2];
                // 目标是桌面图像的 BGRA，alpha 恒为不透明；光标不改变它。
                dst[off + 3] = 0xFF;
                touched += 1;
            }
        }
        touched
    }

    /// 计算单个像素的合成结果。`screen` 是目标处原有的 BGR（不含 A）。
    /// 返回 `None` 表示该像素完全透明、保持桌面原样。
    fn sample(&self, sx: usize, sy: usize, screen: [u8; 3]) -> Option<[u8; 3]> {
        match &self.pixels {
            CursorPixels::Mono {
                and_mask,
                xor_mask,
                pitch,
            } => {
                // 位序是 MSB 优先（DDB 位图的惯例），行按 pitch 对齐。
                let byte = sy * pitch + (sx >> 3);
                let bit = 7 - (sx & 7);
                let and = (and_mask.get(byte)? >> bit) & 1;
                let xor = (xor_mask.get(byte)? >> bit) & 1;
                // AND=1 保留桌面、XOR=1 取反；两者组合出四种标准语义。
                let and_m = if and == 1 { 0xFFu8 } else { 0x00 };
                let xor_m = if xor == 1 { 0xFFu8 } else { 0x00 };
                Some([
                    (screen[0] & and_m) ^ xor_m,
                    (screen[1] & and_m) ^ xor_m,
                    (screen[2] & and_m) ^ xor_m,
                ])
            }
            CursorPixels::Color { bgra, pitch } => {
                let off = sy * pitch + sx * 4;
                let (b, g, r, a) = (
                    *bgra.get(off)? as u32,
                    *bgra.get(off + 1)? as u32,
                    *bgra.get(off + 2)? as u32,
                    *bgra.get(off + 3)? as u32,
                );
                if a == 0 {
                    return None;
                }
                if a == 255 {
                    return Some([b as u8, g as u8, r as u8]);
                }
                // 直通 alpha 混合（非预乘）：现代抗锯齿光标边缘靠它才不生硬。
                let inv = 255 - a;
                let mix = |s: u32, d: u8| ((s * a + d as u32 * inv) / 255) as u8;
                Some([mix(b, screen[0]), mix(g, screen[1]), mix(r, screen[2])])
            }
            CursorPixels::MaskedColor { bgra, pitch } => {
                let off = sy * pitch + sx * 4;
                let (b, g, r, a) = (
                    bgra.get(off).copied().unwrap_or(0),
                    bgra.get(off + 1).copied().unwrap_or(0),
                    bgra.get(off + 2).copied().unwrap_or(0),
                    bgra.get(off + 3).copied().unwrap_or(0),
                );
                if a < 0x80 {
                    // 掩码位为 0：直接盖上光标颜色（不透明）。
                    Some([b, g, r])
                } else {
                    // 掩码位为 1：与桌面异或，等价于单色掩码的 (1,1) 反色语义。
                    Some([screen[0] ^ b, screen[1] ^ g, screen[2] ^ r])
                }
            }
        }
    }
}

/// 指针当下的位置与可见性（**已被换算到录制区域坐标系**）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerState {
    pub visible: bool,
    /// 光标图像左上角，相对录制区域左上角的像素偏移；可为负（部分在区域外）
    pub x: i32,
    pub y: i32,
}

/// 交给下游合成的一帧指针信息。
///
/// `shape` 为 `None` 表示「还没收到过任何形状」——此时不该画任何东西，
/// 而不是画一个占位方块。
#[derive(Clone, Copy)]
pub struct PointerFrame<'a> {
    pub state: PointerState,
    pub shape: Option<&'a CursorShape>,
}

impl PointerFrame<'_> {
    /// 是否真的会画出像素（可见 + 有形状）。
    pub fn is_drawable(&self) -> bool {
        self.state.visible && self.shape.is_some()
    }
}

/// 光标在画面内实际覆盖的矩形 `(x, y, w, h)`；完全落在画面外时返回 `None`。
///
/// 单独抽出来是因为 GPU 路径要拿它当「回读包围盒」用：只需要把这一小块从显存
/// 读回 CPU 合成，而不是整帧。CPU 路径的 [`CursorShape::blend_into`] 自己会裁剪，
/// 两条路由此共享同一套边界判断，不会一处写「部分可见」另一处当「完全不可见」。
pub fn overlay_rect(
    state: PointerState,
    shape: &CursorShape,
    frame_w: u32,
    frame_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    let left = state.x.max(0);
    let top = state.y.max(0);
    let right = (state.x + shape.width() as i32).min(frame_w as i32);
    let bottom = (state.y + shape.height() as i32).min(frame_h as i32);
    if right <= left || bottom <= top {
        return None;
    }
    Some((
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(
        ty: u32,
        w: u32,
        h: u32,
        pitch: u32,
        hx: i32,
        hy: i32,
    ) -> DXGI_OUTDUPL_POINTER_SHAPE_INFO {
        DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: ty,
            Width: w,
            Height: h,
            Pitch: pitch,
            HotSpot: windows::Win32::Foundation::POINT { x: hx, y: hy },
        }
    }

    fn mono_type() -> u32 {
        DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32
    }

    /// 造一个 8x1 的单色光标：`and_bits` / `xor_bits` 是每列一个 bit（MSB 优先）。
    fn mono_1row(and_bits: &[u8; 8], xor_bits: &[u8; 8]) -> CursorShape {
        let mut data = Vec::new();
        let mut a = 0u8;
        let mut x = 0u8;
        for i in 0..8 {
            a |= and_bits[i] << (7 - i);
            x |= xor_bits[i] << (7 - i);
        }
        data.push(a);
        data.push(x);
        CursorShape::parse(mono_type(), &info(mono_type(), 8, 1, 1, 0, 0), &data).unwrap()
    }

    /// 单色掩码的四种组合必须各自落在「桌面 / 反色 / 黑 / 白」上。
    ///
    /// 这是指针合成里唯一真正容易写错的地方：位序（MSB 优先）、行距、四种组合的
    /// 语义，任何一项搞反都会得到一个「形状对但颜色错」的光标。
    #[test]
    fn mono_mask_implements_all_four_and_xor_cases() {
        // 列 0..7 依次为：透出桌面、反色、黑、白，循环两遍
        let and_bits = [1, 1, 0, 0, 1, 1, 0, 0];
        let xor_bits = [0, 1, 0, 1, 0, 1, 0, 1];
        let shape = mono_1row(&and_bits, &xor_bits);

        let desktop = [0x10u8, 0x20, 0x30];
        let mut dst = Vec::new();
        for _ in 0..8 {
            dst.extend_from_slice(&[desktop[0], desktop[1], desktop[2], 0xFF]);
        }
        let touched = shape.blend_into(&mut dst, 8 * 4, 8, 1, 0, 0);
        assert_eq!(touched, 8, "8 列应全部参与合成");

        let px = |i: usize| (dst[i * 4], dst[i * 4 + 1], dst[i * 4 + 2]);
        assert_eq!(px(0), (0x10, 0x20, 0x30), "(AND=1,XOR=0) 应透出桌面");
        assert_eq!(px(1), (0xEF, 0xDF, 0xCF), "(AND=1,XOR=1) 应对桌面反色");
        assert_eq!(px(2), (0x00, 0x00, 0x00), "(AND=0,XOR=0) 应为纯黑");
        assert_eq!(px(3), (0xFF, 0xFF, 0xFF), "(AND=0,XOR=1) 应为纯白");
        // 第二组循环，验证不是只对了前四个像素
        assert_eq!(px(4), (0x10, 0x20, 0x30));
        assert_eq!(px(5), (0xEF, 0xDF, 0xCF));
        assert_eq!(px(6), (0x00, 0x00, 0x00));
        assert_eq!(px(7), (0xFF, 0xFF, 0xFF));
    }

    /// 单色掩码的 AND 与 XOR 必须是**两段独立数据**，不能读成「行交错」。
    #[test]
    fn mono_masks_are_two_stacked_blocks_not_interleaved() {
        // 高 2、行距 1：AND 两行 = 0xFF,0x00；XOR 两行 = 0x00,0xFF
        let data = vec![0xFF, 0x00, 0x00, 0xFF];
        let shape = CursorShape::parse(mono_type(), &info(mono_type(), 8, 2, 1, 0, 0), &data)
            .expect("2 行双掩码应能解析");
        assert_eq!(shape.height(), 2);

        let mut dst = vec![0x40u8; 8 * 2 * 4];
        shape.blend_into(&mut dst, 8 * 4, 8, 2, 0, 0);
        // 第 0 行：AND=0xFF（保留桌面）、XOR=0x00 → 桌面原样
        assert_eq!(&dst[0..4], &[0x40, 0x40, 0x40, 0xFF]);
        // 第 1 行：AND=0x00（清黑）、XOR=0xFF → 纯白
        let row1 = 8 * 4;
        assert_eq!(&dst[row1..row1 + 4], &[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    /// 形状缓冲区比声明的矮时必须拒绝，而不是越界读。
    #[test]
    fn mono_rejects_truncated_buffer() {
        let short = vec![0u8; 1]; // 高 2 的 mono 至少要 4 字节
        assert!(
            CursorShape::parse(mono_type(), &info(mono_type(), 8, 2, 1, 0, 0), &short).is_none(),
            "缓冲区不足时必须返回 None"
        );
    }

    /// 行距装不下一整行时拒绝：
    /// `pitch * 8 < width` 意味着这一行根本没写完，继续读就是错位。
    #[test]
    fn mono_rejects_pitch_too_small_for_width() {
        let data = vec![0u8; 16];
        assert!(
            CursorShape::parse(mono_type(), &info(mono_type(), 32, 2, 1, 0, 0), &data).is_none(),
            "pitch=1 装不下 32 像素宽"
        );
    }

    fn color_shape(w: u32, h: u32, pixels: &[[u8; 4]]) -> CursorShape {
        let pitch = w as usize * 4;
        let mut data = Vec::with_capacity(pitch * h as usize);
        for p in pixels {
            data.extend_from_slice(p);
        }
        let ty = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32;
        CursorShape::parse(ty, &info(ty, w, h, pitch as u32, 1, 2), &data).unwrap()
    }

    /// 半透明像素要做真正的混合，而不是「过阈值就算不透明」。
    #[test]
    fn color_blends_half_transparent_alpha() {
        // 纯红、alpha=128，压在纯蓝桌面上
        let shape = color_shape(1, 1, &[[0, 0, 255, 128]]);
        let mut dst = vec![255u8, 0, 0, 0xFF]; // BGRA: B=255 蓝
        shape.blend_into(&mut dst, 4, 1, 1, 0, 0);
        // B = (0*128 + 255*127)/255 = 127；R = (255*128 + 0*127)/255 = 128
        assert_eq!(dst[0], 127, "蓝通道应按 alpha 衰减一半");
        assert_eq!(dst[2], 128, "红通道应按 alpha 混入一半");
    }

    /// alpha=0 完全透明，alpha=255 完全覆盖，两者都必须是精确值。
    #[test]
    fn color_handles_fully_transparent_and_opaque() {
        let shape = color_shape(2, 1, &[[9, 9, 9, 0], [7, 8, 10, 255]]);
        let mut dst = vec![0x40u8; 2 * 4];
        shape.blend_into(&mut dst, 2 * 4, 2, 1, 0, 0);
        assert_eq!(
            &dst[0..4],
            &[0x40, 0x40, 0x40, 0x40],
            "透明像素不应改动画面"
        );
        assert_eq!(&dst[4..8], &[7, 8, 10, 0xFF], "不透明像素应原样覆盖");
    }

    /// MASKED_COLOR：alpha 当掩码，`<0x80` 盖上光标色，`>=0x80` 与桌面异或。
    #[test]
    fn masked_color_uses_alpha_as_mask() {
        let ty = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
        // 像素 0：掩码 0（盖色）；像素 1：掩码 0xFF（异或）
        let data = vec![0x11, 0x22, 0x33, 0x00, 0x0F, 0xF0, 0x00, 0xFF];
        let shape =
            CursorShape::parse(ty, &info(ty, 2, 1, 8, 0, 0), &data).expect("masked color 应能解析");
        let mut dst = vec![0xAAu8; 2 * 4];
        shape.blend_into(&mut dst, 2 * 4, 2, 1, 0, 0);
        assert_eq!(&dst[0..3], &[0x11, 0x22, 0x33], "掩码为 0 时应直接用光标色");
        assert_eq!(&dst[4..7], &[0xA5, 0x5A, 0xAA], "掩码为 1 时应与桌面异或");
    }

    /// 光标只有一部分落在画面内时，只画能看见的那部分，且位置要对。
    #[test]
    fn clipping_keeps_in_frame_part_and_skips_the_rest() {
        // 4x1 的不透明绿光标，放到 x = -2
        let shape = color_shape(4, 1, &[[0, 255, 0, 255]; 4]);
        let mut dst = vec![0u8; 4 * 4];
        let touched = shape.blend_into(&mut dst, 4 * 4, 4, 1, -2, 0);
        assert_eq!(touched, 2, "左边两列在画面外，只应合成两列");
        assert_eq!(&dst[0..4], &[0, 255, 0, 0xFF], "第一列应是光标第 3 个像素");
        assert_eq!(&dst[4..8], &[0, 255, 0, 0xFF]);
        assert_eq!(&dst[8..12], &[0, 0, 0, 0], "画面外的部分不得写入");
    }

    /// 完全在画面外时不写任何像素（返回值让上层可以跳过重编码）。
    #[test]
    fn fully_outside_returns_zero_and_writes_nothing() {
        let shape = color_shape(2, 2, &[[1, 2, 3, 255]; 4]);
        let mut dst = vec![0x55u8; 2 * 2 * 4];
        let touched = shape.blend_into(&mut dst, 2 * 4, 2, 2, 100, 100);
        assert_eq!(touched, 0);
        assert!(dst.iter().all(|b| *b == 0x55), "越界时画面必须保持不变");
    }

    /// `Position` 是左上角，热点不参与绘制——即使热点非零也必须从 (0,0) 对齐。
    #[test]
    fn hotspot_does_not_shift_placement() {
        let (hx, hy) = (5, 7);
        let ty = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32;
        let data = vec![0xFFu8; 4 * 4];
        let shape = CursorShape::parse(ty, &info(ty, 4, 1, 16, hx, hy), &data).unwrap();
        assert_eq!(shape.hotspot(), (5, 7), "热点应被保留下来供上层参考");

        let mut dst = vec![0u8; 8 * 4];
        shape.blend_into(&mut dst, 8 * 4, 8, 1, 3, 0);
        // 若错误地减掉热点，会从 x=3-5=-2 开始画，第一个可见像素会落在 x=0；
        // 正确实现应从 x=3 开始。
        assert_eq!(&dst[3 * 4..3 * 4 + 4], &[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&dst[0..4], &[0, 0, 0, 0], "x=0 不应被写入");
    }

    /// 未知形状类型不能崩，也不能画。
    #[test]
    fn unknown_shape_type_is_ignored() {
        let data = vec![0u8; 64];
        assert!(CursorShape::parse(3, &info(3, 4, 4, 16, 0, 0), &data).is_none());
        assert!(CursorShape::parse(99, &info(99, 4, 4, 16, 0, 0), &data).is_none());
    }

    /// 指针状态与「能不能画」的组合语义。
    #[test]
    fn pointer_frame_is_drawable_requires_visible_and_shape() {
        let shape = color_shape(1, 1, &[[0, 0, 0, 255]]);
        let visible = PointerState {
            visible: true,
            x: 1,
            y: 2,
        };
        assert!(
            !PointerFrame {
                state: visible,
                shape: None
            }
            .is_drawable(),
            "没有形状时不能画占位方块"
        );
        assert!(
            !PointerFrame {
                state: PointerState::default(),
                shape: Some(&shape)
            }
            .is_drawable(),
            "不可见时不该画"
        );
        assert!(
            PointerFrame {
                state: visible,
                shape: Some(&shape)
            }
            .is_drawable()
        );
    }

    /// 宽高为 0 的退化形状直接拒绝，避免下游除零/空循环。
    #[test]
    fn zero_sized_shape_rejected() {
        let data = vec![0u8; 16];
        assert!(
            CursorShape::parse(mono_type(), &info(mono_type(), 0, 1, 1, 0, 0), &data).is_none()
        );
        assert!(
            CursorShape::parse(mono_type(), &info(mono_type(), 8, 0, 1, 0, 0), &data).is_none()
        );
        assert!(
            CursorShape::parse(
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
                &info(2, 4, 0, 16, 0, 0),
                &data
            )
            .is_none()
        );
    }

    /// GPU 路径的回读包围盒：完全在内 / 部分越界 / 完全在外三种情形。
    ///
    /// 这块算错的表现很隐蔽——指针会「少一截」，或者贴边时整块不画，而画面其余
    /// 部分完全正常，很难联想到是包围盒而不是合成公式的问题。
    #[test]
    fn overlay_rect_clips_to_frame() {
        let shape = color_shape(10, 8, &[[0, 0, 0, 255]; 80]);

        // 完全在画面内
        let inside = PointerState {
            visible: true,
            x: 20,
            y: 30,
        };
        assert_eq!(
            overlay_rect(inside, &shape, 100, 100),
            Some((20, 30, 10, 8))
        );

        // 左上越界：起点夹到 0，尺寸相应缩小
        let top_left = PointerState {
            visible: true,
            x: -4,
            y: -3,
        };
        assert_eq!(overlay_rect(top_left, &shape, 100, 100), Some((0, 0, 6, 5)));

        // 右下越界
        let bottom_right = PointerState {
            visible: true,
            x: 95,
            y: 96,
        };
        assert_eq!(
            overlay_rect(bottom_right, &shape, 100, 100),
            Some((95, 96, 5, 4))
        );

        // 完全在外
        let outside = PointerState {
            visible: true,
            x: 200,
            y: 0,
        };
        assert_eq!(overlay_rect(outside, &shape, 100, 100), None);
        let outside_neg = PointerState {
            visible: true,
            x: -50,
            y: 10,
        };
        assert_eq!(overlay_rect(outside_neg, &shape, 100, 100), None);
    }
}
