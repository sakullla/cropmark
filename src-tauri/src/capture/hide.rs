use std::time::{Duration, Instant};

use super::error::CaptureError;

#[allow(dead_code)]
pub const PRODUCT_SURFACES: [&str; 3] = ["preview", "settings", "tray-popup"];
#[allow(dead_code)]
pub const SESSION_SURFACES: [&str; 3] = ["overlay", "capture-delay", "capture-error"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Preview,
    Settings,
    TrayPopup,
    Overlay,
    Delay,
    Error,
}

impl SurfaceKind {
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Settings => "settings",
            Self::TrayPopup => "tray-popup",
            Self::Overlay => "overlay",
            Self::Delay => "capture-delay",
            Self::Error => "capture-error",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "preview" => Some(Self::Preview),
            "settings" => Some(Self::Settings),
            "tray-popup" => Some(Self::TrayPopup),
            "overlay" => Some(Self::Overlay),
            "capture-delay" => Some(Self::Delay),
            "capture-error" => Some(Self::Error),
            _ => None,
        }
    }

    pub fn restore_on_cancel(self) -> bool {
        matches!(self, Self::Preview | Self::Settings | Self::TrayPopup)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedSurface {
    pub label: String,
    pub kind: SurfaceKind,
    pub was_visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HideWait {
    pub recorded: Vec<RecordedSurface>,
    hide_requested: bool,
    hide_presented: bool,
    still_visible: bool,
}

impl HideWait {
    pub fn record(surfaces: Vec<RecordedSurface>) -> Self {
        Self {
            recorded: surfaces,
            hide_requested: false,
            hide_presented: false,
            still_visible: true,
        }
    }

    pub fn request_hide(&mut self) {
        self.hide_requested = true;
        self.hide_presented = false;
        self.still_visible = self.recorded.iter().any(|surface| surface.was_visible);
    }

    pub fn mark_unmapped(&mut self) {
        self.still_visible = false;
    }

    pub fn mark_presented(&mut self) {
        if self.hide_requested && !self.still_visible {
            self.hide_presented = true;
        }
    }

    #[cfg(test)]
    pub fn set_still_visible(&mut self, visible: bool) {
        self.still_visible = visible;
        if visible {
            self.hide_presented = false;
        }
    }

    pub fn can_capture(&self) -> bool {
        self.hide_requested && self.hide_presented && !self.still_visible
    }

    pub fn commit_presented(&mut self, surfaces_hidden: bool, tray_gone: bool) -> Result<(), CaptureError> {
        if !surfaces_hidden || !tray_gone {
            return Err(hide_not_presented_error());
        }
        self.mark_unmapped();
        self.mark_presented();
        grab_allowed(self)
    }

    pub fn restore_on_cancel(&self) -> Vec<RecordedSurface> {
        self.recorded
            .iter()
            .filter(|surface| surface.was_visible && surface.kind.restore_on_cancel())
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureAt {
    Now,
    OnExpiry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelayPlan {
    pub delay_ms: u64,
    pub hide_before_delay: bool,
    pub overlay_during_delay: bool,
    pub capture_at: CaptureAt,
}

pub fn plan_delay(delay_ms: u64) -> DelayPlan {
    DelayPlan {
        delay_ms,
        hide_before_delay: true,
        overlay_during_delay: false,
        capture_at: if delay_ms == 0 {
            CaptureAt::Now
        } else {
            CaptureAt::OnExpiry
        },
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStep {
    RecordSurfaces,
    Hide,
    WaitPresented,
    DelayWithoutOverlay,
    CapturePixels,
    ShowOverlayOnFreeze,
    OpenPreview,
}

#[cfg(test)]
pub fn session_steps(region: bool, delay_ms: u64) -> Vec<SessionStep> {
    let mut steps = vec![
        SessionStep::RecordSurfaces,
        SessionStep::Hide,
        SessionStep::WaitPresented,
    ];
    if delay_ms > 0 {
        steps.push(SessionStep::DelayWithoutOverlay);
        steps.push(SessionStep::WaitPresented);
    }
    steps.push(SessionStep::CapturePixels);
    if region {
        steps.push(SessionStep::ShowOverlayOnFreeze);
    } else {
        steps.push(SessionStep::OpenPreview);
    }
    steps
}

pub fn hide_not_presented_error() -> CaptureError {
    CaptureError::api("界面尚未隐藏完成，未能截取。")
}

pub fn grab_allowed(wait: &HideWait) -> Result<(), CaptureError> {
    if wait.can_capture() {
        Ok(())
    } else {
        Err(hide_not_presented_error())
    }
}

pub fn wait_until_hidden<F>(mut is_visible: F, timeout: Duration) -> bool
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    loop {
        if !is_visible() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
}

pub fn wait_compositor_presented() {
    #[cfg(windows)]
    unsafe {
        let _ = windows::Win32::Graphics::Dwm::DwmFlush();
    }
    std::thread::sleep(Duration::from_millis(8));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(label: &str) -> RecordedSurface {
        RecordedSurface {
            label: label.to_string(),
            kind: SurfaceKind::from_label(label).unwrap(),
            was_visible: true,
        }
    }

    #[test]
    fn capture_blocked_until_hide_is_presented() {
        let mut wait = HideWait::record(vec![visible("preview"), visible("settings")]);
        assert!(!wait.can_capture());
        wait.request_hide();
        assert!(!wait.can_capture());
        wait.mark_unmapped();
        assert!(!wait.can_capture());
        wait.mark_presented();
        assert!(wait.can_capture());
    }

    #[test]
    fn still_visible_surface_blocks_capture() {
        let mut wait = HideWait::record(vec![visible("tray-popup")]);
        wait.request_hide();
        wait.mark_unmapped();
        wait.mark_presented();
        wait.set_still_visible(true);
        assert!(!wait.can_capture());
    }

    #[test]
    fn cancel_restores_recorded_product_surfaces_only() {
        let mut wait = HideWait::record(vec![
            visible("preview"),
            visible("settings"),
            RecordedSurface {
                label: "overlay".into(),
                kind: SurfaceKind::Overlay,
                was_visible: true,
            },
        ]);
        wait.request_hide();
        let restored = wait.restore_on_cancel();
        let labels: Vec<_> = restored.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["preview", "settings"]);
    }

    #[test]
    fn delay_hides_without_showing_overlay() {
        let plan = plan_delay(3000);
        assert!(plan.hide_before_delay);
        assert!(!plan.overlay_during_delay);
        assert_eq!(plan.capture_at, CaptureAt::OnExpiry);
        let steps = session_steps(true, 3000);
        assert_eq!(steps[3], SessionStep::DelayWithoutOverlay);
        assert_eq!(steps[5], SessionStep::CapturePixels);
        assert_eq!(steps[6], SessionStep::ShowOverlayOnFreeze);
        assert!(steps.iter().position(|&s| s == SessionStep::DelayWithoutOverlay).unwrap()
            < steps.iter().position(|&s| s == SessionStep::CapturePixels).unwrap());
    }

    #[test]
    fn tray_delay_hides_before_pixels_for_overlay_and_fullscreen() {
        for uses_overlay in [true, false] {
            let plan = plan_delay(3000);
            assert!(plan.hide_before_delay);
            assert!(!plan.overlay_during_delay);
            let steps = session_steps(uses_overlay, 3000);
            assert_eq!(
                &steps[..3],
                &[
                    SessionStep::RecordSurfaces,
                    SessionStep::Hide,
                    SessionStep::WaitPresented,
                ]
            );
            let delay = steps
                .iter()
                .position(|&step| step == SessionStep::DelayWithoutOverlay)
                .unwrap();
            let pixels = steps
                .iter()
                .position(|&step| step == SessionStep::CapturePixels)
                .unwrap();
            assert!(delay < pixels);
            if uses_overlay {
                assert_eq!(steps.last().copied(), Some(SessionStep::ShowOverlayOnFreeze));
            } else {
                assert_eq!(steps.last().copied(), Some(SessionStep::OpenPreview));
            }
        }
    }

    #[test]
    fn overlay_is_drawn_after_freeze() {
        let steps = session_steps(true, 0);
        let capture = steps.iter().position(|&s| s == SessionStep::CapturePixels).unwrap();
        let overlay = steps.iter().position(|&s| s == SessionStep::ShowOverlayOnFreeze).unwrap();
        assert!(capture < overlay);
    }

    #[test]
    fn hide_timeout_does_not_mark_presented_or_allow_grab() {
        let mut wait = HideWait::record(vec![visible("preview")]);
        wait.request_hide();
        let error = wait.commit_presented(false, true).unwrap_err();
        assert!(error.message.contains("隐藏"));
        assert!(!wait.can_capture());
        assert!(grab_allowed(&wait).is_err());
    }

    #[test]
    fn tray_still_open_does_not_allow_capture() {
        let mut wait = HideWait::record(vec![visible("tray-popup")]);
        wait.request_hide();
        assert!(wait.commit_presented(true, false).is_err());
        assert!(!wait.can_capture());
    }

    #[test]
    fn grab_requires_hide_presented() {
        let mut wait = HideWait::record(vec![visible("settings")]);
        wait.request_hide();
        assert!(grab_allowed(&wait).is_err());
        wait.mark_unmapped();
        wait.mark_presented();
        assert!(grab_allowed(&wait).is_ok());
    }

    #[test]
    fn wait_until_hidden_timeout_returns_false() {
        let hidden = wait_until_hidden(|| true, Duration::from_millis(24));
        assert!(!hidden);
    }

    #[test]
    fn wait_until_hidden_observes_unmap() {
        let mut remaining = 2;
        let hidden = wait_until_hidden(
            || {
                if remaining == 0 {
                    false
                } else {
                    remaining -= 1;
                    true
                }
            },
            Duration::from_millis(200),
        );
        assert!(hidden);
        assert_eq!(remaining, 0);
    }
}
