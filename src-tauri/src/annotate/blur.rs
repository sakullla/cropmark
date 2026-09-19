use image::{imageops, ImageBuffer, Rgba};

/// 单次 `imageops::blur` 直接卷积的代价与区域像素数 × sigma 成正比：
/// 8K 全屏（约 3300 万像素）配 sigma 48 会需要数十秒。超过该像素数的区域
/// 先整数倍降采样、按比例缩小 sigma 模糊后再升采样回原尺寸，把单次处理量
/// 限制在同一量级；视觉结果仍是高斯模糊，遮盖语义（无原始像素残留）不变。
const MAX_BLUR_PIXELS: usize = 512 * 1024;

/// 对 RGBA 缓冲的指定区域做高斯模糊（`imageops::blur`），区域外保持原样。
/// 与马赛克一致：区域先按帧边界裁剪；小于 2px 或 sigma 非正/非有限时
/// 不处理，保证极小选区或空图不产生无效标注。
#[allow(clippy::too_many_arguments)]
pub fn gaussian(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    region_width: u32,
    region_height: u32,
    sigma: f64,
) {
    if width == 0 || height == 0 || region_width == 0 || region_height == 0 {
        return;
    }
    if !sigma.is_finite() || sigma <= 0.0 {
        return;
    }
    let expected = width as usize * height as usize * 4;
    if rgba.len() < expected {
        return;
    }
    let x1 = x.saturating_add(region_width).min(width);
    let y1 = y.saturating_add(region_height).min(height);
    if x >= x1 || y >= y1 {
        return;
    }
    let region_width = x1 - x;
    let region_height = y1 - y;
    // 2px 以下区域模糊后与原始像素无异，直接跳过避免无效处理。
    if region_width < 2 || region_height < 2 {
        return;
    }
    let row_bytes = region_width as usize * 4;
    let mut region = Vec::with_capacity(row_bytes * region_height as usize);
    for py in y..y1 {
        let start = ((py * width + x) * 4) as usize;
        region.extend_from_slice(&rgba[start..start + row_bytes]);
    }
    let Some(region) =
        ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(region_width, region_height, region)
    else {
        return;
    };
    let sigma = sigma.clamp(super::MIN_BLUR_SIGMA, super::MAX_BLUR_SIGMA) as f32;
    let blurred = bounded_blur(&region, sigma);
    for (row, py) in (y..y1).enumerate() {
        let source = &blurred.as_raw()[row * row_bytes..(row + 1) * row_bytes];
        let start = ((py * width + x) * 4) as usize;
        rgba[start..start + row_bytes].copy_from_slice(source);
    }
}

/// 像素预算内直接模糊；超预算时降采样后模糊再升采样，返回与原区域同尺寸的图像。
fn bounded_blur(
    region: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    sigma: f32,
) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    let pixels = region.width() as usize * region.height() as usize;
    if pixels <= MAX_BLUR_PIXELS {
        return imageops::blur(region, sigma);
    }
    let factor = ((pixels as f64 / MAX_BLUR_PIXELS as f64).sqrt()).ceil() as u32;
    let small_width = (region.width() / factor).max(2);
    let small_height = (region.height() / factor).max(2);
    let small = imageops::resize(
        region,
        small_width,
        small_height,
        imageops::FilterType::Triangle,
    );
    let small_sigma = (sigma / factor as f32).max(0.5);
    let blurred = imageops::blur(&small, small_sigma);
    imageops::resize(
        &blurred,
        region.width(),
        region.height(),
        imageops::FilterType::Triangle,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(width: u32, height: u32) -> Vec<u8> {
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                let value = if (x + y) % 2 == 0 { 255 } else { 0 };
                rgba[i..i + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        rgba
    }

    fn luma(rgba: &[u8], width: u32, x: u32, y: u32) -> u8 {
        rgba[((y * width + x) * 4) as usize]
    }

    #[test]
    fn uniform_region_is_unchanged_by_blur() {
        let mut rgba = vec![128u8; 8 * 8 * 4];
        gaussian(&mut rgba, 8, 8, 0, 0, 8, 8, 3.0);
        for px in rgba.chunks_exact(4) {
            for channel in px {
                assert!(
                    (i32::from(*channel) - 128).abs() <= 1,
                    "uniform channel drifted to {channel}"
                );
            }
        }
    }

    #[test]
    fn blur_removes_sharp_checker_contrast_inside_region_only() {
        let width = 16u32;
        let height = 16u32;
        let mut rgba = checker(width, height);
        // 区域外的一对相邻像素保持黑白对比。
        gaussian(&mut rgba, width, height, 4, 4, 8, 8, 2.0);
        assert_eq!(luma(&rgba, width, 0, 0), 255);
        assert_eq!(luma(&rgba, width, 1, 0), 0);
        assert_eq!(luma(&rgba, width, 15, 15), 255);
        let mut max_delta = 0i32;
        for y in 5..11u32 {
            for x in 5..11u32 {
                let delta =
                    i32::from(luma(&rgba, width, x, y)) - i32::from(luma(&rgba, width, x + 1, y));
                max_delta = max_delta.max(delta.abs());
            }
        }
        assert!(
            max_delta < 60,
            "blurred neighbors should lose the 255 contrast, got {max_delta}"
        );
    }

    #[test]
    fn tiny_or_invalid_regions_are_noops() {
        let mut rgba = checker(8, 8);
        let before = rgba.clone();
        gaussian(&mut rgba, 8, 8, 0, 0, 1, 1, 3.0);
        assert_eq!(rgba, before);
        gaussian(&mut rgba, 8, 8, 0, 0, 8, 8, 0.0);
        assert_eq!(rgba, before);
        gaussian(&mut rgba, 8, 8, 0, 0, 8, 8, f64::NAN);
        assert_eq!(rgba, before);
        // 越界区域按帧边界裁剪后仍可用，不改动区域外像素。
        gaussian(&mut rgba, 8, 8, 4, 4, 100, 100, 2.0);
        assert_eq!(luma(&rgba, 8, 0, 0), 255);
    }

    #[test]
    fn empty_buffer_is_ignored() {
        let mut rgba: Vec<u8> = Vec::new();
        gaussian(&mut rgba, 4, 4, 0, 0, 4, 4, 2.0);
        assert!(rgba.is_empty());
    }

    #[test]
    fn large_region_blur_uses_bounded_path_and_removes_contrast() {
        // 超过像素预算的区域走降采样-模糊-升采样:结果仍填满原区域尺寸、
        // 失去棋盘高频对比,证明大区域同样不保留原始像素。
        let width = 1500u32;
        let height = 400u32;
        assert!(width as usize * height as usize > MAX_BLUR_PIXELS);
        let mut rgba = checker(width, height);
        gaussian(&mut rgba, width, height, 0, 0, width, height, 12.0);
        assert!(
            luma(&rgba, width, 0, 0) < 200,
            "corner checker pixel should be averaged away"
        );
        let mut max_delta = 0i32;
        for y in [10u32, 100, 200, 390] {
            for x in 10..width - 11 {
                let delta =
                    i32::from(luma(&rgba, width, x, y)) - i32::from(luma(&rgba, width, x + 1, y));
                max_delta = max_delta.max(delta.abs());
            }
        }
        assert!(
            max_delta < 40,
            "bounded blur should smooth the checker, got {max_delta}"
        );
    }
}
