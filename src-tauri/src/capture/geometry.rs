#[derive(Debug, Clone, PartialEq)]
pub struct MonitorGeom {
    pub id: String,
    pub logical_x: i32,
    pub logical_y: i32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub physical_x: i32,
    pub physical_y: i32,
    pub physical_width: u32,
    pub physical_height: u32,
    pub scale: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl MonitorGeom {
    #[cfg(test)]
    pub fn from_logical(
        id: impl Into<String>,
        logical_x: i32,
        logical_y: i32,
        logical_width: u32,
        logical_height: u32,
        scale: f64,
    ) -> Self {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        Self {
            id: id.into(),
            logical_x,
            logical_y,
            logical_width,
            logical_height,
            physical_x: scale_i32(logical_x, scale),
            physical_y: scale_i32(logical_y, scale),
            physical_width: scale_u32(logical_width, scale),
            physical_height: scale_u32(logical_height, scale),
            scale,
        }
    }

    pub fn from_physical(
        id: impl Into<String>,
        physical_x: i32,
        physical_y: i32,
        physical_width: u32,
        physical_height: u32,
        scale: f64,
    ) -> Self {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        Self {
            id: id.into(),
            logical_x: unscale_i32(physical_x, scale),
            logical_y: unscale_i32(physical_y, scale),
            logical_width: unscale_u32(physical_width, scale),
            logical_height: unscale_u32(physical_height, scale),
            physical_x,
            physical_y,
            physical_width,
            physical_height,
            scale,
        }
    }

    pub fn contains_physical(&self, x: i32, y: i32) -> bool {
        x >= self.physical_x
            && y >= self.physical_y
            && x < self.physical_x + self.physical_width as i32
            && y < self.physical_y + self.physical_height as i32
    }

    #[cfg(test)]
    pub fn contains_logical(&self, x: i32, y: i32) -> bool {
        x >= self.logical_x
            && y >= self.logical_y
            && x < self.logical_x + self.logical_width as i32
            && y < self.logical_y + self.logical_height as i32
    }
}

pub fn monitor_at_physical(monitors: &[MonitorGeom], x: i32, y: i32) -> Option<&MonitorGeom> {
    monitors
        .iter()
        .find(|monitor| monitor.contains_physical(x, y))
}

#[cfg(test)]
pub fn normalize_logical_rect(x0: f64, y0: f64, x1: f64, y1: f64) -> LogicalRect {
    let x = x0.min(x1);
    let y = y0.min(y1);
    LogicalRect {
        x,
        y,
        width: (x0.max(x1) - x).max(0.0),
        height: (y0.max(y1) - y).max(0.0),
    }
}

/// AppKit 全局坐标(主屏左下原点,Y 向上,逻辑点)→ 引擎物理像素(窗口内容区左上原点,Y 向下).
///
/// `frame_*` 是选区窗的 AppKit `NSWindow.frame`(同样左下原点).点击屏幕底部必须得到
/// 接近 `frame_height * scale` 的引擎 y,而不是 0(顶部);漏掉 Y 翻转时选区会钉在顶边.
#[allow(dead_code)]
pub fn appkit_global_to_physical(
    screen_x: f64,
    screen_y: f64,
    frame_x: f64,
    frame_y: f64,
    frame_height: f64,
    scale: f64,
) -> (i32, i32) {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let x = ((screen_x - frame_x) * scale).round() as i32;
    let y = ((frame_y + frame_height - screen_y) * scale).round() as i32;
    (x, y)
}

/// 视图 backing 像素(左下原点)→ 引擎像素(左上原点)。
/// 抓屏缓冲可能是 1x 或 2x,不能假定 backing 尺寸等于引擎尺寸。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn backing_to_engine(
    backing_x: f64,
    backing_y_from_bottom: f64,
    backing_width: f64,
    backing_height: f64,
    engine_width: f64,
    engine_height: f64,
) -> (i32, i32) {
    let bw = if backing_width.is_finite() && backing_width > 0.0 {
        backing_width
    } else {
        1.0
    };
    let bh = if backing_height.is_finite() && backing_height > 0.0 {
        backing_height
    } else {
        1.0
    };
    let ew = if engine_width.is_finite() && engine_width > 0.0 {
        engine_width
    } else {
        1.0
    };
    let eh = if engine_height.is_finite() && engine_height > 0.0 {
        engine_height
    } else {
        1.0
    };
    let x = (backing_x / bw * ew).round() as i32;
    let y = ((bh - backing_y_from_bottom) / bh * eh).round() as i32;
    (x, y)
}

pub fn crop_from_logical(
    scale: f64,
    rect: LogicalRect,
    frame_w: u32,
    frame_h: u32,
) -> PhysicalRect {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let x = (rect.x * scale).round().max(0.0) as u32;
    let y = (rect.y * scale).round().max(0.0) as u32;
    let width = (rect.width * scale).round().max(0.0) as u32;
    let height = (rect.height * scale).round().max(0.0) as u32;
    PhysicalRect {
        x: x.min(frame_w),
        y: y.min(frame_h),
        width: width.min(frame_w.saturating_sub(x)),
        height: height.min(frame_h.saturating_sub(y)),
    }
}

#[cfg(test)]
fn scale_i32(value: i32, scale: f64) -> i32 {
    (value as f64 * scale).round() as i32
}

#[cfg(test)]
fn scale_u32(value: u32, scale: f64) -> u32 {
    (value as f64 * scale).round() as u32
}

fn unscale_i32(value: i32, scale: f64) -> i32 {
    (value as f64 / scale).round() as i32
}

fn unscale_u32(value: u32, scale: f64) -> u32 {
    (value as f64 / scale).round() as u32
}

/// 拼接画布像素上限(约 384MB RGBA)。超出则明确失败,不分配半成品。
pub const STITCH_MAX_PIXELS: u64 = 96_000_000;

/// 多屏不对齐时的空白填充。不透明深灰,与屏幕内容可区分。
pub const STITCH_BACKGROUND: [u8; 4] = [32, 32, 36, 255];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasFault {
    Empty,
    TooLarge,
}

/// 全部显示器物理矩形的包围盒。原点可以是负的(主屏左侧/上方的屏)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualCanvas {
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
}

/// 托盘项与采集时重新枚举共用的稳定键:身份 + 物理几何。
/// 分辨率或位置变化后旧菜单项不再命中,采集侧明确失败。
pub fn monitor_key(monitor: &MonitorGeom) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        monitor.id,
        monitor.physical_x,
        monitor.physical_y,
        monitor.physical_width,
        monitor.physical_height
    )
}

pub fn virtual_canvas(monitors: &[MonitorGeom]) -> Result<VirtualCanvas, CanvasFault> {
    if monitors.is_empty() {
        return Err(CanvasFault::Empty);
    }
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for monitor in monitors {
        if monitor.physical_width == 0 || monitor.physical_height == 0 {
            return Err(CanvasFault::Empty);
        }
        let x = i64::from(monitor.physical_x);
        let y = i64::from(monitor.physical_y);
        let right = x + i64::from(monitor.physical_width);
        let bottom = y + i64::from(monitor.physical_height);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(right);
        max_y = max_y.max(bottom);
    }
    let width = max_x - min_x;
    let height = max_y - min_y;
    if width <= 0 || height <= 0 || width > i64::from(u32::MAX) || height > i64::from(u32::MAX) {
        return Err(CanvasFault::TooLarge);
    }
    let width = width as u32;
    let height = height as u32;
    if u64::from(width) * u64::from(height) > STITCH_MAX_PIXELS {
        return Err(CanvasFault::TooLarge);
    }
    let origin_x = i32::try_from(min_x).map_err(|_| CanvasFault::TooLarge)?;
    let origin_y = i32::try_from(min_y).map_err(|_| CanvasFault::TooLarge)?;
    Ok(VirtualCanvas {
        origin_x,
        origin_y,
        width,
        height,
    })
}

pub fn monitor_dest(canvas: &VirtualCanvas, monitor: &MonitorGeom) -> (i32, i32) {
    (
        monitor.physical_x.saturating_sub(canvas.origin_x),
        monitor.physical_y.saturating_sub(canvas.origin_y),
    )
}

pub struct RgbaView<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
}

/// 按物理原点把各屏贴进包围盒。尺寸不符或越界返回 None,不写出半张图。
pub fn stitch_views(
    canvas: &VirtualCanvas,
    layers: &[(i32, i32, RgbaView<'_>)],
    background: [u8; 4],
) -> Option<Vec<u8>> {
    let pixels = (canvas.width as usize).checked_mul(canvas.height as usize)?;
    let bytes = pixels.checked_mul(4)?;
    let mut rgba = vec![0u8; bytes];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&background);
    }
    for (dest_x, dest_y, view) in layers {
        blit_opaque(
            canvas.width,
            canvas.height,
            &mut rgba,
            *dest_x,
            *dest_y,
            view,
        )?;
    }
    Some(rgba)
}

fn blit_opaque(
    dst_w: u32,
    dst_h: u32,
    dst: &mut [u8],
    dest_x: i32,
    dest_y: i32,
    src: &RgbaView<'_>,
) -> Option<()> {
    let expected = (src.width as usize)
        .checked_mul(src.height as usize)?
        .checked_mul(4)?;
    if src.rgba.len() != expected || src.width == 0 || src.height == 0 {
        return None;
    }
    if dest_x < 0 || dest_y < 0 {
        return None;
    }
    let dest_x = dest_x as u32;
    let dest_y = dest_y as u32;
    if dest_x.checked_add(src.width)? > dst_w || dest_y.checked_add(src.height)? > dst_h {
        return None;
    }
    for row in 0..src.height as usize {
        let src_off = row * src.width as usize * 4;
        let dst_off = ((dest_y as usize + row) * dst_w as usize + dest_x as usize) * 4;
        let len = src.width as usize * 4;
        dst[dst_off..dst_off + len].copy_from_slice(&src.rgba[src_off..src_off + len]);
    }
    Some(())
}

/// 指针热点是否落在帧的屏幕矩形内(含左上,不含右下)。
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub fn cursor_anchor_inside(
    frame_x: i32,
    frame_y: i32,
    frame_w: u32,
    frame_h: u32,
    hot_x: i32,
    hot_y: i32,
) -> bool {
    let Some(right) = frame_x.checked_add(i32::try_from(frame_w).unwrap_or(i32::MAX)) else {
        return false;
    };
    let Some(bottom) = frame_y.checked_add(i32::try_from(frame_h).unwrap_or(i32::MAX)) else {
        return false;
    };
    hot_x >= frame_x && hot_y >= frame_y && hot_x < right && hot_y < bottom
}

/// 预乘 ARGB(0xAARRGGBB)光标。热点在帧外时不改像素并返回 false。
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub struct CursorBlit<'a> {
    pub hot_x: i32,
    pub hot_y: i32,
    pub sprite_x: i32,
    pub sprite_y: i32,
    pub width: u32,
    pub height: u32,
    pub argb: &'a [u32],
}

/// 把光标贴到帧上。精灵可部分伸出帧外,只画相交部分。
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub fn composite_premul_cursor(
    frame_w: u32,
    frame_h: u32,
    frame: &mut [u8],
    frame_x: i32,
    frame_y: i32,
    cursor: CursorBlit<'_>,
) -> bool {
    if !cursor_anchor_inside(
        frame_x,
        frame_y,
        frame_w,
        frame_h,
        cursor.hot_x,
        cursor.hot_y,
    ) {
        return false;
    }
    let sprite_w = cursor.width;
    let sprite_h = cursor.height;
    let expected = (sprite_w as usize).saturating_mul(sprite_h as usize);
    if sprite_w == 0 || sprite_h == 0 || cursor.argb.len() != expected {
        return false;
    }
    let frame_bytes = (frame_w as usize).saturating_mul(frame_h as usize) * 4;
    if frame.len() != frame_bytes {
        return false;
    }
    let dest_x = cursor.sprite_x - frame_x;
    let dest_y = cursor.sprite_y - frame_y;
    for row in 0..sprite_h as i32 {
        let dy = dest_y + row;
        if dy < 0 || dy >= frame_h as i32 {
            continue;
        }
        for col in 0..sprite_w as i32 {
            let dx = dest_x + col;
            if dx < 0 || dx >= frame_w as i32 {
                continue;
            }
            let pixel = cursor.argb[row as usize * sprite_w as usize + col as usize];
            let alpha = (pixel >> 24) as u8;
            if alpha == 0 {
                continue;
            }
            let offset = (dy as usize * frame_w as usize + dx as usize) * 4;
            let dst = &mut frame[offset..offset + 4];
            if alpha == 255 {
                dst[0] = (pixel >> 16) as u8;
                dst[1] = (pixel >> 8) as u8;
                dst[2] = pixel as u8;
                dst[3] = 255;
                continue;
            }
            let inv = 255 - u32::from(alpha);
            let blend = |src: u8, dst: u8| -> u8 {
                ((u32::from(src) + u32::from(dst) * inv / 255).min(255)) as u8
            };
            dst[0] = blend((pixel >> 16) as u8, dst[0]);
            dst[1] = blend((pixel >> 8) as u8, dst[1]);
            dst[2] = blend(pixel as u8, dst[2]);
            dst[3] = 255;
        }
    }
    true
}

/// SCK/NSScreen 点坐标矩形,用于和 Tauri 逻辑矩形对齐。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayFrame {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 主屏逻辑高度(点)。Tauri 用 CG 左上原点,SCK 帧在部分系统上是 AppKit 左下原点。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn primary_points_height(monitors: &[MonitorGeom]) -> i32 {
    if let Some(primary) = monitors
        .iter()
        .find(|monitor| monitor.logical_x == 0 && monitor.logical_y == 0)
    {
        return primary.logical_height as i32;
    }
    monitors
        .iter()
        .map(|monitor| monitor.logical_y.saturating_add(monitor.logical_height as i32))
        .max()
        .unwrap_or(0)
}

/// 把目标显示器配到一份 SCK 帧列表。先比 CG 逻辑矩形,再比 Y 翻转后的 AppKit 矩形,
/// 仍不唯一时只接受「尺寸唯一」的屏;否则视为目标不可用。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn match_sck_display(
    target: &MonitorGeom,
    displays: &[DisplayFrame],
    primary_points_height: i32,
) -> Option<usize> {
    let logical = DisplayFrame {
        x: target.logical_x,
        y: target.logical_y,
        width: target.logical_width,
        height: target.logical_height,
    };
    if let Some(index) = exact_display(displays, logical) {
        return Some(index);
    }
    let flipped_y = primary_points_height
        .saturating_sub(target.logical_y)
        .saturating_sub(target.logical_height as i32);
    let flipped = DisplayFrame {
        x: target.logical_x,
        y: flipped_y,
        width: target.logical_width,
        height: target.logical_height,
    };
    if let Some(index) = exact_display(displays, flipped) {
        return Some(index);
    }
    let same_size: Vec<usize> = displays
        .iter()
        .enumerate()
        .filter(|(_, display)| {
            display.width == logical.width && display.height == logical.height
        })
        .map(|(index, _)| index)
        .collect();
    if same_size.len() == 1 {
        Some(same_size[0])
    } else {
        None
    }
}

#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn exact_display(displays: &[DisplayFrame], wanted: DisplayFrame) -> Option<usize> {
    let hits: Vec<usize> = displays
        .iter()
        .enumerate()
        .filter(|(_, display)| **display == wanted)
        .map(|(index, _)| index)
        .collect();
    if hits.len() == 1 {
        Some(hits[0])
    } else {
        None
    }
}

/// XFixes GetCursorImage 回复(连接已换成主机字节序)。供 Linux 合成与单测共用。
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XFixesCursorImage {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub x_hot: u16,
    pub y_hot: u16,
    pub argb: Vec<u32>,
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub fn parse_xfixes_cursor_image(bytes: &[u8]) -> Option<(XFixesCursorImage, &[u8])> {
    if bytes.first().copied() != Some(1) || bytes.len() < 32 {
        return None;
    }
    let length = u32::from_ne_bytes(bytes[4..8].try_into().ok()?);
    let x = i16::from_ne_bytes(bytes[8..10].try_into().ok()?);
    let y = i16::from_ne_bytes(bytes[10..12].try_into().ok()?);
    let width = u16::from_ne_bytes(bytes[12..14].try_into().ok()?);
    let height = u16::from_ne_bytes(bytes[14..16].try_into().ok()?);
    let x_hot = u16::from_ne_bytes(bytes[16..18].try_into().ok()?);
    let y_hot = u16::from_ne_bytes(bytes[18..20].try_into().ok()?);
    let pixels = usize::from(width).checked_mul(usize::from(height))?;
    let pixel_bytes = pixels.checked_mul(4)?;
    let body = bytes.get(32..32 + pixel_bytes)?;
    let mut argb = Vec::with_capacity(pixels);
    for chunk in body.chunks_exact(4) {
        argb.push(u32::from_ne_bytes(chunk.try_into().ok()?));
    }
    let consumed = 32usize.saturating_add((length as usize).saturating_mul(4));
    let rest = bytes.get(consumed).map(|_| &bytes[consumed..]).unwrap_or(&[]);
    Some((
        XFixesCursorImage {
            x,
            y,
            width,
            height,
            x_hot,
            y_hot,
            argb,
        },
        rest,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_2x_fullscreen_matches_physical_pixels() {
        let monitor = MonitorGeom::from_logical("s", 0, 0, 1920, 1080, 2.0);
        assert_eq!(monitor.physical_width, 3840);
        assert_eq!(monitor.physical_height, 2160);
    }

    #[test]
    fn mixed_dpi_pointer_selects_that_screen() {
        let one = MonitorGeom::from_physical("1x", 0, 0, 1920, 1080, 1.0);
        let two = MonitorGeom::from_physical("2x", 1920, 0, 2880, 1800, 2.0);
        let monitors = [one, two];
        assert_eq!(monitor_at_physical(&monitors, 100, 100).unwrap().id, "1x");
        assert_eq!(monitor_at_physical(&monitors, 1930, 20).unwrap().id, "2x");
        assert_eq!(monitors[1].logical_width, 1440);
        assert_eq!(monitors[1].physical_width, 2880);
    }

    #[test]
    fn region_logical_drag_maps_to_physical_crop() {
        let rect = normalize_logical_rect(10.0, 20.0, 110.0, 100.0);
        let crop = crop_from_logical(1.5, rect, 3000, 2000);
        assert_eq!(
            crop,
            PhysicalRect {
                x: 15,
                y: 30,
                width: 150,
                height: 120
            }
        );
    }

    #[test]
    fn physical_roundtrip_preserves_2x_geometry() {
        let monitor = MonitorGeom::from_physical("retina", 0, 0, 2560, 1600, 2.0);
        assert_eq!(monitor.logical_width, 1280);
        assert_eq!(monitor.logical_height, 800);
        assert!(monitor.contains_logical(10, 10));
        assert!(!monitor.contains_logical(2000, 10));
    }

    #[test]
    fn appkit_bottom_left_origin_does_not_map_to_engine_top() {
        // 主屏 1440×900 @2x,窗框覆盖整屏.底部点击不得落到引擎 y=0.
        let (x, y) = appkit_global_to_physical(100.0, 0.0, 0.0, 0.0, 900.0, 2.0);
        assert_eq!(x, 200);
        assert_eq!(y, 1800);
        let (x, y) = appkit_global_to_physical(100.0, 900.0, 0.0, 0.0, 900.0, 2.0);
        assert_eq!(x, 200);
        assert_eq!(y, 0);
        let (x, y) = appkit_global_to_physical(720.0, 450.0, 0.0, 0.0, 900.0, 2.0);
        assert_eq!((x, y), (1440, 900));
    }

    #[test]
    fn appkit_secondary_display_uses_window_frame_origin() {
        // 右侧副屏,AppKit 原点仍在主屏左下;窗框 origin=(1440, 0),高度 1080.
        let (x, y) = appkit_global_to_physical(1440.0, 1080.0, 1440.0, 0.0, 1080.0, 1.0);
        assert_eq!((x, y), (0, 0));
        let (x, y) = appkit_global_to_physical(1640.0, 80.0, 1440.0, 0.0, 1080.0, 1.0);
        assert_eq!((x, y), (200, 1000));
    }

    #[test]
    fn backing_1x_engine_keeps_click_in_place() {
        // 1470×956 点窗、1x 抓屏:点 (100, 100 from top) 必须落到引擎 (100, 100)。
        let (x, y) = backing_to_engine(100.0, 856.0, 1470.0, 956.0, 1470.0, 956.0);
        assert_eq!((x, y), (100, 100));
    }

    #[test]
    fn backing_2x_engine_maps_points_to_physical_pixels() {
        let (x, y) = backing_to_engine(200.0, 1712.0, 2940.0, 1912.0, 2940.0, 1912.0);
        assert_eq!((x, y), (200, 200));
        let (x, y) = backing_to_engine(2940.0, 0.0, 2940.0, 1912.0, 2940.0, 1912.0);
        assert_eq!((x, y), (2940, 1912));
    }

    #[test]
    fn backing_2x_does_not_use_engine_1x_scale() {
        // 旧 bug:鼠标按 scale=2 换算,引擎却是 1x 缓冲,选区偏到 2 倍位置。
        let (x, y) = backing_to_engine(200.0, 1712.0, 2940.0, 1912.0, 1470.0, 956.0);
        assert_eq!((x, y), (100, 100));
    }

    #[test]
    fn stitch_places_negative_origin_and_fills_the_gap() {
        let left = MonitorGeom::from_physical("left", -2, 0, 2, 2, 1.0);
        let right = MonitorGeom::from_physical("right", 1, 0, 2, 1, 1.0);
        let canvas = virtual_canvas(&[left.clone(), right.clone()]).unwrap();
        assert_eq!(
            canvas,
            VirtualCanvas {
                origin_x: -2,
                origin_y: 0,
                width: 5,
                height: 2
            }
        );
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        let left_px = red.repeat(4);
        let right_px = blue.repeat(2);
        let rgba = stitch_views(
            &canvas,
            &[
                (
                    monitor_dest(&canvas, &left).0,
                    monitor_dest(&canvas, &left).1,
                    RgbaView {
                        width: 2,
                        height: 2,
                        rgba: &left_px,
                    },
                ),
                (
                    monitor_dest(&canvas, &right).0,
                    monitor_dest(&canvas, &right).1,
                    RgbaView {
                        width: 2,
                        height: 1,
                        rgba: &right_px,
                    },
                ),
            ],
            STITCH_BACKGROUND,
        )
        .unwrap();
        let pixel = |x: usize, y: usize| {
            let i = (y * 5 + x) * 4;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        assert_eq!(pixel(0, 0), red);
        assert_eq!(pixel(2, 0), STITCH_BACKGROUND);
        assert_eq!(pixel(3, 0), blue);
        assert_eq!(pixel(3, 1), STITCH_BACKGROUND);
        assert_eq!(monitor_key(&left), "left|-2|0|2|2");
    }

    #[test]
    fn mixed_dpi_stitch_is_physical_one_to_one() {
        let one = MonitorGeom::from_physical("1x", 0, 0, 2, 2, 1.0);
        let two = MonitorGeom::from_physical("2x", 2, 0, 4, 2, 2.0);
        let canvas = virtual_canvas(&[one.clone(), two.clone()]).unwrap();
        assert_eq!(canvas.width, 6);
        assert_eq!(canvas.height, 2);
        assert_eq!(monitor_dest(&canvas, &two), (2, 0));
        assert_eq!(two.logical_width, 2);
    }

    #[test]
    fn stitch_rejects_empty_and_oversized_canvases() {
        assert_eq!(virtual_canvas(&[]), Err(CanvasFault::Empty));
        let huge = MonitorGeom::from_physical("huge", 0, 0, 20_000, 20_000, 1.0);
        assert_eq!(virtual_canvas(&[huge]), Err(CanvasFault::TooLarge));
    }

    #[test]
    fn stitch_rejects_a_layer_that_does_not_fit() {
        let monitor = MonitorGeom::from_physical("only", 0, 0, 2, 2, 1.0);
        let canvas = virtual_canvas(&[monitor]).unwrap();
        let pixels = [1u8, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 0, 1, 2, 255];
        assert!(stitch_views(
            &canvas,
            &[(
                1,
                0,
                RgbaView {
                    width: 2,
                    height: 2,
                    rgba: &pixels,
                }
            )],
            STITCH_BACKGROUND,
        )
        .is_none());
    }

    #[test]
    fn cursor_is_omitted_when_the_hotspot_is_outside() {
        let mut frame = vec![10u8, 20, 30, 255];
        let sprite = [0xff00_00ffu32];
        let outside = CursorBlit {
            hot_x: 5,
            hot_y: 5,
            sprite_x: 4,
            sprite_y: 4,
            width: 1,
            height: 1,
            argb: &sprite,
        };
        assert!(!composite_premul_cursor(1, 1, &mut frame, 0, 0, outside));
        assert_eq!(frame, vec![10, 20, 30, 255]);
        let inside = CursorBlit {
            hot_x: 0,
            hot_y: 0,
            sprite_x: 0,
            sprite_y: 0,
            width: 1,
            height: 1,
            argb: &sprite,
        };
        assert!(composite_premul_cursor(1, 1, &mut frame, 0, 0, inside));
        assert_eq!(frame, vec![0, 0, 255, 255]);
    }

    #[test]
    fn cursor_clips_to_the_frame_and_uses_premultiplied_alpha() {
        let mut frame = [255u8, 0, 0, 255];
        // 热点在唯一像素内;精灵宽 2,第二列落在帧外。alpha=128 的预乘红。
        let sprite = [0x8080_0000u32, 0xffff_0000];
        assert!(composite_premul_cursor(
            1,
            1,
            &mut frame,
            10,
            20,
            CursorBlit {
                hot_x: 10,
                hot_y: 20,
                sprite_x: 10,
                sprite_y: 20,
                width: 2,
                height: 1,
                argb: &sprite,
            },
        ));
        assert_eq!(frame[0], (0x80 + 255 * 127 / 255) as u8);
        assert_eq!(frame[1], 0);
        assert_eq!(frame[2], 0);
        assert_eq!(frame[3], 255);
    }

    #[test]
    fn sck_display_match_accepts_cg_and_flipped_appkit_frames() {
        let primary = MonitorGeom::from_physical("primary", 0, 0, 200, 100, 1.0);
        let above = MonitorGeom::from_physical("above", 0, -80, 160, 80, 1.0);
        let displays = [
            DisplayFrame {
                x: 0,
                y: 0,
                width: 200,
                height: 100,
            },
            DisplayFrame {
                x: 0,
                y: 100,
                width: 160,
                height: 80,
            },
        ];
        let height = primary_points_height(&[primary.clone(), above.clone()]);
        assert_eq!(height, 100);
        assert_eq!(match_sck_display(&primary, &displays, height), Some(0));
        // CG y=-80 翻成 AppKit y = 100。
        assert_eq!(match_sck_display(&above, &displays, height), Some(1));
        let ambiguous = MonitorGeom::from_physical("other", 9, 9, 160, 80, 1.0);
        let twins = [
            DisplayFrame {
                x: 0,
                y: 0,
                width: 160,
                height: 80,
            },
            DisplayFrame {
                x: 200,
                y: 0,
                width: 160,
                height: 80,
            },
        ];
        assert_eq!(match_sck_display(&ambiguous, &twins, height), None);
    }

    #[test]
    fn xfixes_cursor_image_parses_native_endian_reply() {
        let mut bytes = vec![0u8; 32];
        bytes[0] = 1;
        bytes[4..8].copy_from_slice(&1u32.to_ne_bytes());
        bytes[8..10].copy_from_slice(&4i16.to_ne_bytes());
        bytes[10..12].copy_from_slice(&5i16.to_ne_bytes());
        bytes[12..14].copy_from_slice(&1u16.to_ne_bytes());
        bytes[14..16].copy_from_slice(&1u16.to_ne_bytes());
        bytes[16..18].copy_from_slice(&1u16.to_ne_bytes());
        bytes[18..20].copy_from_slice(&2u16.to_ne_bytes());
        bytes.extend_from_slice(&0xaabbccddu32.to_ne_bytes());
        let (image, rest) = parse_xfixes_cursor_image(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(image.x, 4);
        assert_eq!(image.y, 5);
        assert_eq!(image.x_hot, 1);
        assert_eq!(image.y_hot, 2);
        assert_eq!(image.argb, vec![0xaabbccdd]);
        assert!(parse_xfixes_cursor_image(&[0, 1, 2, 3]).is_none());
    }
}
