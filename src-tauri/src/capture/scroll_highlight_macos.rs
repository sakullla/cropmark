//! macOS 长截图进行时留在屏幕上的范围框。
//!
//! 选区壳关闭后如果没有可见的框,用户不知道正在截哪一块。这层面板点击穿透、
//! 不成为键盘焦点,失活也不隐藏。抓取侧已经排除本进程,框和压暗不会进拼接结果。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSScreen, NSStringDrawing, NSView,
    NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_core_foundation::{kCFRunLoopCommonModes, CFRunLoop, CFType, CGPoint};
use objc2_core_graphics::kCGScreenSaverWindowLevel;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use super::CaptureAxis;
use crate::capture::geometry::MonitorGeom;
use crate::capture::session::RegionSelection;

const DIM_ALPHA: f64 = 72.0 / 255.0;
const BORDER: f64 = 3.0;

#[derive(Clone, Copy)]
struct ViewRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Clone)]
struct PaintState {
    id: u64,
    view_w: f64,
    view_h: f64,
    region: ViewRect,
    region_w: u32,
    region_h: u32,
    base: u32,
    stitched: u32,
}

static PAINT: Mutex<Option<PaintState>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Guard {
    panel: usize,
    id: u64,
    closed: Arc<AtomicBool>,
}

struct OpenRequest {
    logical_x: i32,
    logical_y: i32,
    logical_width: u32,
    logical_height: u32,
    scale: f64,
    region_x: u32,
    region_y: u32,
    region_w: u32,
    region_h: u32,
    base: u32,
}

impl Guard {
    pub(crate) fn open(
        monitor: &MonitorGeom,
        region: &RegionSelection,
        axis: CaptureAxis,
    ) -> Option<Self> {
        let request = OpenRequest {
            logical_x: monitor.logical_x,
            logical_y: monitor.logical_y,
            logical_width: monitor.logical_width,
            logical_height: monitor.logical_height,
            scale: monitor.scale,
            region_x: region.x,
            region_y: region.y,
            region_w: region.width,
            region_h: region.height,
            base: match axis {
                CaptureAxis::Vertical => region.height,
                CaptureAxis::Horizontal => region.width,
            },
        };
        if MainThreadMarker::new().is_some() {
            return open_on_main(request);
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let queued = enqueue(move || {
            let _ = tx.send(open_on_main(request));
        });
        if !queued {
            return None;
        }
        rx.recv_timeout(Duration::from_secs(2)).ok().flatten()
    }

    pub(crate) fn set_axis(&self, axis: CaptureAxis) {
        let base_is_height = matches!(axis, CaptureAxis::Vertical);
        self.retouch(move |paint| {
            paint.base = if base_is_height {
                paint.region_h
            } else {
                paint.region_w
            };
        });
    }

    pub(crate) fn update(&self, stitched_length: u32) {
        self.retouch(move |paint| {
            paint.stitched = stitched_length;
        });
    }

    fn retouch(&self, edit: impl FnOnce(&mut PaintState) + Send + 'static) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let id = self.id;
        let panel = self.panel;
        let closed = Arc::clone(&self.closed);
        let _ = enqueue(move || {
            if closed.load(Ordering::SeqCst) {
                return;
            }
            {
                let mut slot = PAINT
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(paint) = slot.as_mut() {
                    if paint.id == id {
                        edit(paint);
                    }
                }
            }
            redraw(panel, &closed);
        });
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        let panel = self.panel;
        let id = self.id;
        let _ = enqueue(move || {
            {
                let mut slot = PAINT
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if slot.as_ref().is_some_and(|paint| paint.id == id) {
                    *slot = None;
                }
            }
            let ptr = panel as *mut HighlightPanel;
            if let Some(window) = unsafe { Retained::from_raw(ptr) } {
                window.orderOut(None);
            }
        });
    }
}

fn open_on_main(request: OpenRequest) -> Option<Guard> {
    let mtm = MainThreadMarker::new()?;
    let frame = screen_frame(mtm, &request);
    let view_w = frame.size.width.max(1.0);
    let view_h = frame.size.height.max(1.0);
    let region = region_view_rect(
        request.region_x,
        request.region_y,
        request.region_w,
        request.region_h,
        view_w,
        view_h,
        request.scale,
    );
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    {
        let mut slot = PAINT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(PaintState {
            id,
            view_w,
            view_h,
            region,
            region_w: request.region_w,
            region_h: request.region_h,
            base: request.base,
            stitched: request.base,
        });
    }
    let view = unsafe { create_view(mtm, frame.size) };
    let panel = unsafe { create_panel(mtm, frame) };
    let content: &NSView = &view;
    panel.setContentView(Some(content));
    panel.setFrame_display(frame, true);
    panel.orderFrontRegardless();
    view.setNeedsDisplay(true);
    let closed = Arc::new(AtomicBool::new(false));
    Some(Guard {
        panel: Retained::into_raw(panel) as usize,
        id,
        closed,
    })
}

fn redraw(panel: usize, closed: &AtomicBool) {
    if closed.load(Ordering::SeqCst) || panel == 0 {
        return;
    }
    let panel = unsafe { &*(panel as *const HighlightPanel) };
    if let Some(view) = panel.contentView() {
        view.setNeedsDisplay(true);
    }
}

fn enqueue(work: impl FnOnce() + Send + 'static) -> bool {
    let Some(rl) = CFRunLoop::main() else {
        return false;
    };
    let work = Mutex::new(Some(work));
    let block = RcBlock::new(move || {
        if let Some(work) = work
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            work();
        }
    });
    let block: &DynBlock<dyn Fn()> = &block;
    let mode = unsafe { kCFRunLoopCommonModes }.map(|mode| mode as &CFType);
    unsafe { rl.perform_block(mode, Some(block)) };
    rl.wake_up();
    true
}

fn screen_frame(mtm: MainThreadMarker, request: &OpenRequest) -> NSRect {
    let fallback = NSRect {
        origin: NSPoint {
            x: request.logical_x as f64,
            y: request.logical_y as f64,
        },
        size: NSSize {
            width: request.logical_width.max(1) as f64,
            height: request.logical_height.max(1) as f64,
        },
    };
    for screen in NSScreen::screens(mtm).to_vec() {
        let frame = screen.frame();
        let matched = frame.origin.x.round() as i32 == request.logical_x
            && frame.origin.y.round() as i32 == request.logical_y
            && frame.size.width.round() as u32 == request.logical_width
            && frame.size.height.round() as u32 == request.logical_height;
        if matched {
            return frame;
        }
    }
    fallback
}

/// 选区是显示器左上原点的物理像素。视图是 AppKit 左下原点的点。
fn region_view_rect(
    region_x: u32,
    region_y: u32,
    region_w: u32,
    region_h: u32,
    view_w: f64,
    view_h: f64,
    scale: f64,
) -> ViewRect {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let w = (region_w as f64 / scale).max(1.0);
    let h = (region_h as f64 / scale).max(1.0);
    let x = (region_x as f64 / scale).clamp(0.0, view_w.max(0.0));
    let top = region_y as f64 / scale;
    let y = (view_h - top - h).clamp(0.0, view_h.max(0.0));
    ViewRect {
        x,
        y,
        w: w.min((view_w - x).max(1.0)),
        h: h.min((view_h - y).max(1.0)),
    }
}

fn overlay_label(width: u32, height: u32, base: u32, stitched: u32) -> String {
    if stitched > base {
        format!("{width}x{height} +{}", stitched - base)
    } else {
        format!("{width}x{height}")
    }
}

/// 标签优先放在选区上沿之外;贴顶放不下时改放到框内上沿。
fn label_frame(view_w: f64, view_h: f64, region: ViewRect, text_chars: usize) -> ViewRect {
    let label_h = 22.0;
    let label_w = ((text_chars as f64) * 7.5 + 16.0).clamp(48.0, view_w.max(48.0));
    let gap = 8.0;
    let above = region.y + region.h + gap;
    let y = if above + label_h <= view_h {
        above
    } else {
        (region.y + region.h - label_h - gap)
            .max(region.y)
            .min((view_h - label_h).max(0.0))
    };
    let x = region.x.clamp(0.0, (view_w - label_w).max(0.0));
    ViewRect {
        x,
        y,
        w: label_w.min(view_w.max(1.0)),
        h: label_h.min(view_h.max(1.0)),
    }
}

fn draw_highlight() {
    let paint = {
        let slot = PAINT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.clone()
    };
    let Some(paint) = paint else {
        return;
    };
    let dim = NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, DIM_ALPHA);
    dim.setFill();
    fill_outside(paint.view_w, paint.view_h, paint.region);
    let border = NSColor::colorWithSRGBRed_green_blue_alpha(
        0x1D as f64 / 255.0,
        0x4E as f64 / 255.0,
        0xD8 as f64 / 255.0,
        1.0,
    );
    border.setStroke();
    let stroke = NSRect {
        origin: NSPoint {
            x: paint.region.x - BORDER / 2.0,
            y: paint.region.y - BORDER / 2.0,
        },
        size: NSSize {
            width: paint.region.w + BORDER,
            height: paint.region.h + BORDER,
        },
    };
    let path = NSBezierPath::bezierPathWithRect(stroke);
    path.setLineWidth(BORDER);
    path.stroke();
    let label = overlay_label(paint.region_w, paint.region_h, paint.base, paint.stitched);
    let chip = label_frame(
        paint.view_w,
        paint.view_h,
        paint.region,
        label.chars().count(),
    );
    let chip_rect = NSRect {
        origin: NSPoint {
            x: chip.x,
            y: chip.y,
        },
        size: NSSize {
            width: chip.w,
            height: chip.h,
        },
    };
    NSColor::colorWithSRGBRed_green_blue_alpha(0.95, 0.97, 0.99, 0.94).setFill();
    NSBezierPath::fillRect(chip_rect);
    let text = NSString::from_str(&label);
    unsafe {
        text.drawAtPoint_withAttributes(
            CGPoint {
                x: chip.x + 8.0,
                y: chip.y + 4.0,
            },
            None,
        );
    }
}

fn fill_outside(view_w: f64, view_h: f64, hole: ViewRect) {
    if hole.y > 0.0 {
        NSBezierPath::fillRect(NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: view_w,
                height: hole.y,
            },
        });
    }
    let top = hole.y + hole.h;
    if top < view_h {
        NSBezierPath::fillRect(NSRect {
            origin: NSPoint { x: 0.0, y: top },
            size: NSSize {
                width: view_w,
                height: view_h - top,
            },
        });
    }
    if hole.x > 0.0 {
        NSBezierPath::fillRect(NSRect {
            origin: NSPoint { x: 0.0, y: hole.y },
            size: NSSize {
                width: hole.x,
                height: hole.h,
            },
        });
    }
    let right = hole.x + hole.w;
    if right < view_w {
        NSBezierPath::fillRect(NSRect {
            origin: NSPoint {
                x: right,
                y: hole.y,
            },
            size: NSSize {
                width: view_w - right,
                height: hole.h,
            },
        });
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[name = "CropmarkScrollHighlightView"]
    #[thread_kind = MainThreadOnly]
    struct HighlightView;

    impl HighlightView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            draw_highlight();
        }
    }
);

define_class!(
    #[unsafe(super(objc2_app_kit::NSPanel))]
    #[name = "CropmarkScrollHighlightPanel"]
    #[thread_kind = MainThreadOnly]
    struct HighlightPanel;

    impl HighlightPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            false
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }

        #[unsafe(method(constrainFrameRect:toScreen:))]
        fn constrain_frame_rect_to_screen(
            &self,
            frame_rect: NSRect,
            _screen: Option<&NSScreen>,
        ) -> NSRect {
            frame_rect
        }
    }
);

unsafe fn create_view(mtm: MainThreadMarker, size: NSSize) -> Retained<HighlightView> {
    let allocated = mtm.alloc::<HighlightView>().set_ivars(());
    let frame = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size,
    };
    msg_send![super(allocated), initWithFrame: frame]
}

unsafe fn create_panel(mtm: MainThreadMarker, content_rect: NSRect) -> Retained<HighlightPanel> {
    let allocated = mtm.alloc::<HighlightPanel>().set_ivars(());
    let style = NSWindowStyleMask::Borderless.union(NSWindowStyleMask::NonactivatingPanel);
    let panel: Retained<HighlightPanel> = msg_send![super(allocated),
        initWithContentRect: content_rect,
        styleMask: style,
        backing: NSBackingStoreType::Buffered,
        defer: false];
    panel.setIgnoresMouseEvents(true);
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setHidesOnDeactivate(false);
    panel.setMovable(false);
    panel.setSharingType(NSWindowSharingType::None);
    panel.setReleasedWhenClosed(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            .union(NSWindowCollectionBehavior::FullScreenAuxiliary),
    );
    panel.setLevel(kCGScreenSaverWindowLevel as isize);
    panel
}

#[cfg(test)]
mod tests {
    use super::{label_frame, overlay_label, region_view_rect, ViewRect};

    #[test]
    fn retina_region_maps_to_bottom_left_view_points() {
        // 物理 200×100,起点距屏幕左上 (100, 50),2x,视图 1000×800 点。
        let rect = region_view_rect(100, 50, 200, 100, 1000.0, 800.0, 2.0);
        assert_eq!((rect.x, rect.y, rect.w, rect.h), (50.0, 725.0, 100.0, 50.0));
    }

    #[test]
    fn region_touching_the_top_sits_at_the_top_of_the_view() {
        let rect = region_view_rect(0, 0, 100, 100, 800.0, 600.0, 2.0);
        assert_eq!((rect.x, rect.y, rect.w, rect.h), (0.0, 550.0, 50.0, 50.0));
    }

    #[test]
    fn label_sits_above_the_region_until_it_would_leave_the_view() {
        let region = ViewRect {
            x: 40.0,
            y: 100.0,
            w: 200.0,
            h: 80.0,
        };
        let above = label_frame(800.0, 600.0, region, 9);
        assert!(above.y >= region.y + region.h);
        let topped = ViewRect {
            x: 40.0,
            y: 560.0,
            w: 200.0,
            h: 30.0,
        };
        let inside = label_frame(800.0, 600.0, topped, 9);
        assert!(inside.y + inside.h <= 600.0);
        assert!(inside.y < topped.y + topped.h);
    }

    #[test]
    fn label_shows_growth_only_after_the_base_length() {
        assert_eq!(overlay_label(320, 200, 200, 200), "320x200");
        assert_eq!(overlay_label(320, 200, 200, 480), "320x200 +280");
    }
}
