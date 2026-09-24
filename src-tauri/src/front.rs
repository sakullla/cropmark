//! 把产品窗口抬到其它应用前面。
//!
//! Cropmark 以 macOS Accessory 常驻托盘。Accessory 下 `show`/`set_focus`
//! 只在本进程内聚焦,窗口留在当前前台应用后面:设置/历史要点两次才到前面,
//! 截取预览或错误窗也会被挡住。先切到 Regular 再激活;没有产品界面时再回到
//! Accessory,保持托盘常驻、不占 Dock。

use tauri::{AppHandle, Manager, WebviewWindow};

use crate::capture::ui::{ERROR, HISTORY, PREVIEW, SETTINGS};

/// 显示并前置窗口。macOS 会先把进程从 Accessory 提升为 Regular。
pub fn reveal(app: &AppHandle, window: &WebviewWindow) {
    #[cfg(target_os = "macos")]
    promote(app);
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    #[cfg(target_os = "macos")]
    order_front(window);
}

/// 没有设置/历史/预览/错误窗、也没有原生选区壳时,退回 Accessory。
pub fn demote_if_idle(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        if !should_demote(product_ui_visible(app), shell_is_active()) {
            return;
        }
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
    }
}

/// 仅在既没有产品窗口、也没有选区壳时才应退回托盘常驻。
pub fn should_demote(product_ui_visible: bool, shell_active: bool) -> bool {
    !product_ui_visible && !shell_active
}

fn product_ui_visible(app: &AppHandle) -> bool {
    [SETTINGS, HISTORY, PREVIEW, ERROR]
        .into_iter()
        .any(|label| {
            app.get_webview_window(label)
                .and_then(|window| window.is_visible().ok())
                .unwrap_or(false)
        })
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn shell_is_active() -> bool {
    crate::capture::native_overlay::shell_is_active()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn shell_is_active() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn promote(app: &AppHandle) {
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    activate_now();
}

#[cfg(target_os = "macos")]
fn activate_now() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.activate();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
}

#[cfg(target_os = "macos")]
fn order_front(window: &WebviewWindow) {
    use objc2::MainThreadMarker;

    if MainThreadMarker::new().is_none() {
        let window = window.clone();
        let _ = window
            .clone()
            .run_on_main_thread(move || order_front_ns(&window));
        return;
    }
    order_front_ns(window);
}

#[cfg(target_os = "macos")]
fn order_front_ns(window: &WebviewWindow) {
    use objc2_app_kit::NSWindow;

    let Ok(ptr) = window.ns_window() else {
        return;
    };
    if ptr.is_null() {
        return;
    }
    // SAFETY: Tauri 的 ns_window 是该 Webview 当前的 NSWindow*。
    let ns_window = unsafe { &*ptr.cast::<NSWindow>() };
    ns_window.makeKeyAndOrderFront(None);
    ns_window.orderFrontRegardless();
}

#[cfg(test)]
mod tests {
    use super::should_demote;

    #[test]
    fn demote_only_when_no_product_ui_and_no_shell() {
        assert!(should_demote(false, false));
        assert!(!should_demote(true, false));
        assert!(!should_demote(false, true));
        assert!(!should_demote(true, true));
    }
}
