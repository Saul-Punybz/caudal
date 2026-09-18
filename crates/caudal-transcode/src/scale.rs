//! A small pure-Rust bilinear scaler for planar 4:2:0 frames (the
//! `RustyH264` engine's `scale`). Fixed-point, 8 fractional bits. When
//! shrinking by more than 2x it averages a 2x2 box first to limit aliasing.

use rusty_h264::YuvFrame;

/// Output size for `height`, keeping the aspect ratio, both even.
pub(crate) fn fit(src_w: usize, src_h: usize, height: usize) -> (usize, usize) {
    let h = (height & !1).max(2);
    let w = ((src_w * h + src_h / 2) / src_h.max(1)).max(2);
    (w & !1, h)
}

fn scale_plane(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    // Pre-shrink by 2x while the ratio is over 2, so bilinear never skips
    // whole source pixels.
    if sw > dw * 2 && sh > dh * 2 {
        let (hw, hh) = (sw / 2, sh / 2);
        let mut half = vec![0u8; hw * hh];
        for y in 0..hh {
            let (r0, r1) = (&src[2 * y * sw..], &src[(2 * y + 1) * sw..]);
            for x in 0..hw {
                let s =
                    u32::from(r0[2 * x]) + u32::from(r0[2 * x + 1]) + u32::from(r1[2 * x]) + u32::from(r1[2 * x + 1]);
                half[y * hw + x] = ((s + 2) / 4) as u8;
            }
        }
        return scale_plane(&half, hw, hh, dw, dh);
    }
    let map = |d: usize, s: usize, n: usize| -> (usize, usize, u32) {
        // Centre-aligned source coordinate in 1/256 pixels.
        let pos = (((2 * d + 1) * s * 256) / (2 * n)).saturating_sub(128);
        let i0 = (pos / 256).min(s - 1);
        (i0, (i0 + 1).min(s - 1), (pos % 256) as u32)
    };
    let xs: Vec<_> = (0..dw).map(|x| map(x, sw, dw)).collect();
    let mut out = vec![0u8; dw * dh];
    for y in 0..dh {
        let (y0, y1, fy) = map(y, sh, dh);
        let (r0, r1) = (&src[y0 * sw..y0 * sw + sw], &src[y1 * sw..y1 * sw + sw]);
        let row = &mut out[y * dw..(y + 1) * dw];
        for (o, &(x0, x1, fx)) in row.iter_mut().zip(&xs) {
            let top = u32::from(r0[x0]) * (256 - fx) + u32::from(r0[x1]) * fx;
            let bot = u32::from(r1[x0]) * (256 - fx) + u32::from(r1[x1]) * fx;
            *o = ((top * (256 - fy) + bot * fy + (1 << 15)) >> 16) as u8;
        }
    }
    out
}

/// `src` resized to `w`x`h` (both even).
pub(crate) fn scale(src: &YuvFrame, w: usize, h: usize) -> YuvFrame {
    let (sw, sh) = (src.width, src.height);
    let (scw, sch) = (sw.div_ceil(2), sh.div_ceil(2));
    YuvFrame {
        width: w,
        height: h,
        y: scale_plane(&src.y, sw, sh, w, h),
        u: scale_plane(&src.u, scw, sch, w / 2, h / 2),
        v: scale_plane(&src.v, scw, sch, w / 2, h / 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_keep_aspect_and_parity() {
        assert_eq!(fit(1280, 720, 360), (640, 360));
        assert_eq!(fit(640, 360, 240), (426, 240));
        assert_eq!(fit(256, 144, 96), (170, 96));
    }

    #[test]
    fn flat_stays_flat_and_gradients_survive() {
        let mut f = YuvFrame::black(64, 32);
        for (i, p) in f.y.iter_mut().enumerate() {
            *p = ((i % 64) * 4) as u8;
        }
        let s = scale(&f, 16, 8);
        assert_eq!((s.width, s.height, s.y.len(), s.u.len()), (16, 8, 128, 32));
        assert!(s.u.iter().all(|&c| c == 128));
        let row = &s.y[..16];
        assert!(row.windows(2).all(|w| w[1] > w[0]), "{row:?}");
        assert!(row[0] < 16 && row[15] > 235, "{row:?}");
    }
}
