//! BGRA → I420 色彩空间转换（CPU 路径，软件编码器的输入）。
//!
//! BT.709 studio-swing（16–235），与 GPU 路径（MF 侧显式标注 BT.709）及 MP4
//! `colr` 盒保持一致：
//! ```text
//! Y = (( 47R + 157G +  16B + 128) >> 8) + 16
//! U = ((-26R -  87G + 112B + 128) >> 8) + 128
//! V = ((112R - 102G -  10B + 128) >> 8) + 128
//! ```
//! 早期用的是 BT.601 系数，导致同一功能两条编码路径出片偏色不同——高清屏幕内容
//! 应按 BT.709 处理。整数近似会带来 ±1 LSB 误差（例如纯灰的 U 落在 127 而非 128）。
//!
//! 4:2:0 色度抽样：每个 2×2 块平均。
//!
//! 注意：DXGI 桌面复制输出的 BGRA 的行距（stride）通常大于 width*4
//! （纹理对齐），必须按 stride 寻址；裁剪到 region 在本模块之前完成。

/// 单帧转换结果（拥有数据的 I420 平面缓冲）。
pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    /// Y 平面，width*height 字节
    pub y: Vec<u8>,
    /// U 平面，(width/2)*(height/2) 字节
    pub u: Vec<u8>,
    /// V 平面，同上
    pub v: Vec<u8>,
}

/// BGRA（8:8:8:8，B 在最低字节）转 I420。
///
/// - `bgra`：`stride * height` 字节，行距为 `stride`
/// - 宽高为奇数时，最后一行/一列在色度平面被丢弃（宏块对齐由调用方保证）
pub fn bgra_to_i420(bgra: &[u8], stride: u32, width: u32, height: u32) -> I420Frame {
    assert!(stride >= width * 4, "stride 小于 width*4");
    assert!(
        bgra.len() >= (stride * height) as usize,
        "BGRA 缓冲区长度不足"
    );

    let w = width as usize;
    let h = height as usize;
    let st = stride as usize;

    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; (w / 2) * (h / 2)];
    let mut v = vec![0u8; (w / 2) * (h / 2)];

    for row in 0..h {
        let src = &bgra[row * st..row * st + w * 4];
        let dst = &mut y[row * w..(row + 1) * w];
        for x in 0..w {
            let b = src[x * 4] as i32;
            let g = src[x * 4 + 1] as i32;
            let r = src[x * 4 + 2] as i32;
            dst[x] = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
        }
    }

    let hw = w / 2;
    let hh = h / 2;
    for cy in 0..hh {
        for cx in 0..hw {
            // 2x2 块内的 4 个像素取平均后转色度。
            let mut sr = 0i32;
            let mut sg = 0i32;
            let mut sb = 0i32;
            for dy in 0..2 {
                let row = (cy * 2 + dy) * st;
                for dx in 0..2 {
                    let off = row + (cx * 2 + dx) * 4;
                    sb += bgra[off] as i32;
                    sg += bgra[off + 1] as i32;
                    sr += bgra[off + 2] as i32;
                }
            }
            sr >>= 2;
            sg >>= 2;
            sb >>= 2;
            let idx = cy * hw + cx;
            u[idx] = (((-26 * sr - 87 * sg + 112 * sb + 128) >> 8) + 128) as u8;
            v[idx] = (((112 * sr - 102 * sg - 10 * sb + 128) >> 8) + 128) as u8;
        }
    }

    I420Frame {
        width: w as u32,
        height: h as u32,
        y,
        u,
        v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bgra(w: u32, h: u32, fill: [u8; 4]) -> (Vec<u8>, u32) {
        let stride = w * 4;
        let mut buf = vec![0u8; (stride * h) as usize];
        for i in 0..(w * h) as usize {
            buf[i * 4..i * 4 + 4].copy_from_slice(&fill);
        }
        (buf, stride)
    }

    #[test]
    fn white_black_and_gray_are_stable() {
        // 纯白（255,255,255）BT.709 → Y=235；U 因整数系数和不为 0 落在 127。
        let (buf, st) = make_bgra(64, 64, [255, 255, 255, 255]);
        let f = bgra_to_i420(&buf, st, 64, 64);
        assert!(f.y.iter().all(|&v| v == 235), "Y 白场应为 235");
        assert!(
            f.u.iter().all(|&v| (127..=128).contains(&v)),
            "U 应为中性 127/128"
        );
        assert!(f.v.iter().all(|&v| v == 128));

        // 纯黑 → Y=16
        let (buf, st) = make_bgra(64, 64, [0, 0, 0, 255]);
        let f = bgra_to_i420(&buf, st, 64, 64);
        assert!(f.y.iter().all(|&v| v == 16), "Y 黑场应为 16");
    }

    #[test]
    fn primary_colors_hit_expected_ranges() {
        // 纯红（255,0,0）→ BT.709：Y≈63, V≈240（强正向）
        let (buf, st) = make_bgra(32, 32, [0, 0, 255, 255]);
        let f = bgra_to_i420(&buf, st, 32, 32);
        assert!((62..=64).contains(&f.y[0]), "R Y={}", f.y[0]);
        assert!(f.v[0] > 230, "R V={}", f.v[0]);

        // 纯蓝（0,0,255）→ U 强正向
        let (buf, st) = make_bgra(32, 32, [255, 0, 0, 255]);
        let f = bgra_to_i420(&buf, st, 32, 32);
        assert!(f.u[0] > 230, "B U={}", f.u[0]);
    }

    #[test]
    fn stride_larger_than_width_is_handled() {
        // 宽 4 高 4，stride 32（>16）：像素内容在行内左对齐，行间有填充。
        let w = 4u32;
        let h = 4u32;
        let stride = 32u32;
        let mut buf = vec![0u8; (stride * h) as usize];
        for row in 0..h as usize {
            for col in 0..w as usize {
                let off = row * stride as usize + col * 4;
                buf[off] = 255; // 全白
                buf[off + 1] = 255;
                buf[off + 2] = 255;
                buf[off + 3] = 255;
            }
        }
        let f = bgra_to_i420(&buf, stride, w, h);
        assert_eq!(f.y.len(), 16);
        assert!(f.y.iter().all(|&v| v == 235));
        assert_eq!(f.u.len(), 4);
    }

    #[test]
    fn odd_dimensions_do_not_panic() {
        let (buf, st) = make_bgra(5, 3, [128, 64, 200, 255]);
        let f = bgra_to_i420(&buf, st, 5, 3);
        assert_eq!(f.y.len(), 15);
        assert_eq!(f.u.len(), 2); // 5/2=2, 3/2=1
    }
}
