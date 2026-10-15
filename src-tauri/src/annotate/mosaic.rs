#[allow(clippy::too_many_arguments)]
pub fn pixelate(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    region_width: u32,
    region_height: u32,
    block: u32,
) {
    if width == 0 || height == 0 || region_width == 0 || region_height == 0 {
        return;
    }
    let expected = width as usize * height as usize * 4;
    if rgba.len() < expected {
        return;
    }
    let block = block.max(2);
    let x1 = x.saturating_add(region_width).min(width);
    let y1 = y.saturating_add(region_height).min(height);
    if x >= x1 || y >= y1 {
        return;
    }
    let mut by = y;
    while by < y1 {
        let bh = block.min(y1 - by);
        let mut bx = x;
        while bx < x1 {
            let bw = block.min(x1 - bx);
            let mut sum = [0u64; 3];
            let mut count = 0u64;
            for py in by..by + bh {
                for px in bx..bx + bw {
                    let i = ((py * width + px) * 4) as usize;
                    sum[0] += u64::from(rgba[i]);
                    sum[1] += u64::from(rgba[i + 1]);
                    sum[2] += u64::from(rgba[i + 2]);
                    count += 1;
                }
            }
            if count > 0 {
                let avg = [
                    sum[0].checked_div(count).unwrap_or_default() as u8,
                    sum[1].checked_div(count).unwrap_or_default() as u8,
                    sum[2].checked_div(count).unwrap_or_default() as u8,
                ];
                for py in by..by + bh {
                    for px in bx..bx + bw {
                        let i = ((py * width + px) * 4) as usize;
                        rgba[i] = avg[0];
                        rgba[i + 1] = avg[1];
                        rgba[i + 2] = avg[2];
                        rgba[i + 3] = 255;
                    }
                }
            }
            bx += block;
        }
        by += block;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn mosaic_keeps_uniform_source_mean_not_a_tint_overlay() {
        let mut rgba = vec![0u8; 8 * 8 * 4];
        for px in rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&[255, 0, 0, 255]);
        }
        pixelate(&mut rgba, 8, 8, 0, 0, 8, 8, 4);
        for px in rgba.chunks_exact(4) {
            assert_eq!(px, [255, 0, 0, 255]);
        }
    }

    #[test]
    fn mosaic_scatters_contrasting_source_pixels_into_block_means() {
        let mut rgba = vec![0u8; 8 * 8 * 4];
        for y in 0..8u32 {
            for x in 0..8u32 {
                let i = ((y * 8 + x) * 4) as usize;
                if (x + y) % 2 == 0 {
                    rgba[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                } else {
                    rgba[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        }
        pixelate(&mut rgba, 8, 8, 0, 0, 8, 8, 4);
        for by in [0u32, 4] {
            for bx in [0u32, 4] {
                let mean = pixel(&rgba, 8, bx, by);
                assert!(
                    mean[0] > 100 && mean[0] < 160,
                    "block mean should average checker"
                );
                assert_eq!(mean[3], 255);
                for y in by..by + 4 {
                    for x in bx..bx + 4 {
                        assert_eq!(pixel(&rgba, 8, x, y), mean);
                    }
                }
            }
        }
        assert_eq!(pixel(&rgba, 8, 0, 0), pixel(&rgba, 8, 1, 0));
    }

    /// 确定性的非均匀样例:每个像素与坐标相关,含 alpha 0(马赛克必须
    /// 把它变成不透明,隐私区域不能保留原始透明信息)。
    fn gradient(width: u32, height: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[
                    (x * 40) as u8,
                    (y * 60) as u8,
                    ((x + y) * 20) as u8,
                    if (x + y) % 3 == 0 { 0 } else { 255 },
                ]);
            }
        }
        rgba
    }

    /// 块边长下限:0/1/2 输出一致(内部按 2 处理),不 panic 也不退化成
    /// "逐像素不变"(那等于没有马赛克)。
    #[test]
    fn mosaic_treats_block_smaller_than_two_as_two() {
        let source = gradient(6, 4);
        let mut zero = source.clone();
        let mut one = source.clone();
        let mut two = source.clone();
        pixelate(&mut zero, 6, 4, 0, 0, 6, 4, 0);
        pixelate(&mut one, 6, 4, 0, 0, 6, 4, 1);
        pixelate(&mut two, 6, 4, 0, 0, 6, 4, 2);
        assert_eq!(zero, two);
        assert_eq!(one, two);
        assert_ne!(two, source, "clamped block must still change the pixels");
    }

    /// 区域裁剪与部分块:区域外一个字节都不动;边缘不足一块的块只平均
    /// 区域内像素,输出一律不透明。
    #[test]
    fn mosaic_clips_to_the_region_and_forces_opaque_inside_it() {
        let (width, height) = (7u32, 5u32);
        let source = gradient(width, height);
        let mut rgba = source.clone();
        // 区域 x∈[1,5)、y∈[1,4):右缘 x=4 是不足一块的单列块。
        pixelate(&mut rgba, width, height, 1, 1, 4, 3, 3);

        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                let inside = (1..5).contains(&x) && (1..4).contains(&y);
                if !inside {
                    assert_eq!(&rgba[i..i + 4], &source[i..i + 4], "outside ({x},{y})");
                    continue;
                }
                assert_eq!(rgba[i + 3], 255, "inside ({x},{y}) must be opaque");
            }
        }

        // 单列块只平均该列在区域内的像素:(x=4, y=1..4) → RGB 均值 160/120/120,
        // 源里 y=2 的 alpha 0 被强制为 255。
        assert_eq!(pixel(&rgba, width, 4, 1), [160, 120, 120, 255]);
        assert_eq!(pixel(&rgba, width, 4, 3), [160, 120, 120, 255]);
        // 3×3 块内的像素互相一致。
        let mean = pixel(&rgba, width, 1, 1);
        for y in 1..4 {
            for x in 1..4 {
                assert_eq!(pixel(&rgba, width, x, y), mean);
            }
        }
    }

    /// 区域越界被裁剪到帧内:不 panic、不环绕写回,完全在帧外的调用无操作。
    #[test]
    fn mosaic_clips_regions_that_overflow_the_frame() {
        let (width, height) = (4u32, 3u32);
        let source = gradient(width, height);
        let mut rgba = source.clone();
        pixelate(&mut rgba, width, height, 2, 1, 10, 10, 2);

        assert_eq!(pixel(&rgba, width, 2, 1)[3], 255, "in-frame part applies");
        assert_eq!(pixel(&rgba, width, 0, 0), pixel(&source, width, 0, 0));
        assert_eq!(pixel(&rgba, width, 3, 0), pixel(&source, width, 3, 0));

        let mut untouched = source.clone();
        pixelate(&mut untouched, width, height, 8, 8, 4, 4, 3);
        assert_eq!(untouched, source, "fully out-of-frame region is a no-op");
    }
}
