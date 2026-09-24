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
}
