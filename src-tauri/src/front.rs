//! 把产品窗口抬到其它应用前面。
//!
//! Cropmark 以 macOS Accessory 常驻托盘。Accessory 下 `show`/`set_focus`
//! 只在本进程内聚焦,窗口留在当前前台应用后面:设置/历史要点两次才到前面,
//! 截取预览或错误窗也会被挡住。先切到 Regular 再激活;没有产品界面时再回到
//! Accessory,保持托盘常驻、不占 Dock。
//!
//! 从 Dock、启动台或访达打开时显示设置。应用已在菜单栏运行时,再点 Dock
//! 图标会收到 `applicationShouldHandleReopen`,同样打开或前置窗口。登录项启动不弹窗。
//! 右键菜单由 `dock` 模块提供。

use tauri::{AppHandle, Manager, WebviewWindow};

use crate::capture::ui::{ERROR, HISTORY, PREVIEW, SETTINGS};

/// 首次引导 / 快捷键帮助。按需创建,关闭即销毁,不在启动路径预建。
pub use crate::capture::ui::GUIDE;

/// 显示并前置窗口。macOS 会先把进程从 Accessory 提升为 Regular。
pub fn reveal(app: &AppHandle, window: &WebviewWindow) {
    #[cfg(not(target_os = "macos"))]
    let _ = app;
    #[cfg(target_os = "macos")]
    promote(app);
    if window.show().is_err() {
        log::warn!("window reveal failed label={} kind=show", window.label());
    } else {
        log::debug!("window reveal label={}", window.label());
    }
    let _ = window.unminimize();
    let _ = window.set_focus();
    #[cfg(target_os = "macos")]
    order_front(window);
}

/// 没有设置/历史/预览/错误/引导窗、也没有原生选区壳时,退回 Accessory。
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
/// Dock 重新打开也用同一条规则:空闲才打开设置,截图或已有窗口时不打断。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn should_demote(product_ui_visible: bool, shell_active: bool) -> bool {
    !product_ui_visible && !shell_active
}

/// macOS 上用户从 Dock / 启动台 / 访达启动时打开设置。登录项启动保持只进托盘。
pub fn should_open_settings_on_launch(macos: bool, launched_at_login: bool) -> bool {
    macos && !launched_at_login
}

/// `kAEOpenApplication` 带 `keyAELaunchedAsLogInItem` 时是登录项启动。
pub fn is_login_launch(event_id: Option<u32>, login_flag_present: bool) -> bool {
    const OPEN_APPLICATION: u32 = u32::from_be_bytes(*b"oapp");
    event_id == Some(OPEN_APPLICATION) && login_flag_present
}

/// 当前进程是否由登录项拉起。非 macOS 恒为否。读不到事件时按用户启动处理。
pub fn launched_at_login() -> bool {
    #[cfg(target_os = "macos")]
    {
        launched_at_login_macos()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// 点 Dock 图标。已有设置/预览/历史/引导/错误窗时把它抬到前面;
/// 正在截图时不打断;否则打开设置。
#[cfg(target_os = "macos")]
pub fn handle_reopen(app: &AppHandle) {
    if shell_is_active() {
        return;
    }
    if let Some(window) = foremost_product_window(app) {
        reveal(app, &window);
        return;
    }
    if let Err(error) = crate::settings::open_settings(app) {
        log::warn!("dock reopen failed kind=settings");
        eprintln!("Cropmark: 无法从 Dock 打开设置:{error}");
    }
}

#[cfg(target_os = "macos")]
fn foremost_product_window(app: &AppHandle) -> Option<WebviewWindow> {
    // 预览正在编辑时优先把它抬回来,其次才是设置。
    for label in [PREVIEW, SETTINGS, HISTORY, GUIDE, ERROR] {
        let Some(window) = app.get_webview_window(label) else {
            continue;
        };
        if window.is_visible().unwrap_or(false) {
            return Some(window);
        }
    }
    None
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn product_ui_visible(app: &AppHandle) -> bool {
    [SETTINGS, HISTORY, PREVIEW, ERROR, GUIDE]
        .into_iter()
        .any(|label| {
            app.get_webview_window(label)
                .and_then(|window| window.is_visible().ok())
                .unwrap_or(false)
        })
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn shell_is_active() -> bool {
    crate::capture::native_overlay::shell_is_active()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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

/// 登录项标志。常量来自 `AERegistry.h`:`kAEOpenApplication` = 'oapp',
/// `keyAEPropData` = 'prdt',`keyAELaunchedAsLogInItem` = 'lgit'。
#[cfg(target_os = "macos")]
fn launched_at_login_macos() -> bool {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    const OPEN_APPLICATION: u32 = u32::from_be_bytes(*b"oapp");
    const PROP_DATA: u32 = u32::from_be_bytes(*b"prdt");
    const LOGIN_ITEM: u32 = u32::from_be_bytes(*b"lgit");

    // SAFETY: 只在主线程启动路径读取当前 Apple Event,立即使用返回的描述符,
    // 不跨线程保存。选择子是 NSAppleEventManager / NSAppleEventDescriptor 的公开方法。
    unsafe {
        let Some(cls) = AnyClass::get(c"NSAppleEventManager") else {
            return false;
        };
        let manager: *mut AnyObject = msg_send![cls, sharedAppleEventManager];
        if manager.is_null() {
            return false;
        }
        let event: *mut AnyObject = msg_send![manager, currentAppleEvent];
        if event.is_null() {
            return false;
        }
        let event_id: u32 = msg_send![event, eventID];
        if event_id != OPEN_APPLICATION {
            return false;
        }
        let direct: *mut AnyObject = msg_send![event, paramDescriptorForKeyword: LOGIN_ITEM];
        if !direct.is_null() {
            return true;
        }
        let prop: *mut AnyObject = msg_send![event, paramDescriptorForKeyword: PROP_DATA];
        if prop.is_null() {
            return false;
        }
        let flag: *mut AnyObject = msg_send![prop, paramDescriptorForKeyword: LOGIN_ITEM];
        is_login_launch(Some(event_id), !flag.is_null())
    }
}

#[cfg(test)]
mod tests {
    use super::{is_login_launch, should_demote, should_open_settings_on_launch};

    #[test]
    fn demote_only_when_no_product_ui_and_no_shell() {
        assert!(should_demote(false, false));
        assert!(!should_demote(true, false));
        assert!(!should_demote(false, true));
        assert!(!should_demote(true, true));
    }

    #[test]
    fn dock_launch_opens_settings_except_for_login_items() {
        assert!(should_open_settings_on_launch(true, false));
        assert!(!should_open_settings_on_launch(true, true));
        assert!(!should_open_settings_on_launch(false, false));
        assert!(!should_open_settings_on_launch(false, true));
    }

    #[test]
    fn login_launch_requires_open_application_and_the_login_flag() {
        let open_application = u32::from_be_bytes(*b"oapp");
        let open_document = u32::from_be_bytes(*b"odoc");
        assert!(is_login_launch(Some(open_application), true));
        assert!(!is_login_launch(Some(open_application), false));
        assert!(!is_login_launch(Some(open_document), true));
        assert!(!is_login_launch(None, true));
    }
}
