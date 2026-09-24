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
}
