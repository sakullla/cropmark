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
    pub fn from_logical(
        id: impl Into<String>,
        logical_x: i32,
        logical_y: i32,
        logical_width: u32,
        logical_height: u32,
        scale: f64,
    ) -> Self {
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
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
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
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

    pub fn contains_logical(&self, x: i32, y: i32) -> bool {
        x >= self.logical_x
            && y >= self.logical_y
            && x < self.logical_x + self.logical_width as i32
            && y < self.logical_y + self.logical_height as i32
    }
}

pub fn monitor_at_physical<'a>(
    monitors: &'a [MonitorGeom],
    x: i32,
    y: i32,
) -> Option<&'a MonitorGeom> {
    monitors.iter().find(|monitor| monitor.contains_physical(x, y))
}

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

pub fn crop_from_logical(scale: f64, rect: LogicalRect, frame_w: u32, frame_h: u32) -> PhysicalRect {
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
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

fn scale_i32(value: i32, scale: f64) -> i32 {
    (value as f64 * scale).round() as i32
}

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
}
