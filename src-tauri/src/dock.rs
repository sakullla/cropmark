//! macOS Dock 右键菜单。
//!
//! Tauri 只提供 Dock 显隐,没有菜单接口。这里在 tao 已注册的
//! `TaoAppDelegateParent` 上补 `applicationDockMenu:`。系统仍会在菜单末尾加上
//! 「选项」和「退出」,所以这里不再放第二个退出项。
//!
//! 系统退出和 Command-Q 都会进 `applicationShouldTerminate:`。此时调用
//! `app.exit(0)`(与托盘「退出」同一条路径),并返回 `NSTerminateCancel`,
//! 让 AppKit 不要自己去关窗口。tao 的 `windowShouldClose:` 恒为 false;
//! 若让系统继续关窗口,隐藏的预建窗口会被拆掉,退出也会停住。
//! 点窗口关闭按钮不经过这个回调,进程继续留在菜单栏。

use crate::tray::{CLIPBOARD_PIN_ID, EXIT_CLICK_THROUGH_ID, LAST_REGION_ID, LONG_CAPTURE_ID};

/// AppKit `NSApplicationTerminateReply`:取消系统这次退出。
#[cfg(any(test, target_os = "macos"))]
const TERMINATE_CANCEL: usize = 0;
/// AppKit `NSApplicationTerminateReply`:交给系统立刻退出。
#[cfg(any(test, target_os = "macos"))]
const TERMINATE_NOW: usize = 1;

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct DockFlags {
    multi_monitor: bool,
    long_capture: bool,
    has_last_region: bool,
    recording_visible: bool,
    recording_active: bool,
    click_through: bool,
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum DockNode {
    Item(&'static str),
    /// 多屏时的全屏子菜单。热键只标在「指针所在屏幕」上。
    Fullscreen,
    Delay,
    Separator,
}

/// 右键时要展示的项。退出留给系统菜单。
#[cfg(any(test, target_os = "macos"))]
fn dock_nodes(flags: &DockFlags) -> Vec<DockNode> {
    let mut nodes = vec![
        DockNode::Item("capture-region"),
        DockNode::Item("capture-window"),
    ];
    if flags.multi_monitor {
        nodes.push(DockNode::Fullscreen);
    } else {
        nodes.push(DockNode::Item("capture-fullscreen"));
    }
    if flags.has_last_region {
        nodes.push(DockNode::Item(LAST_REGION_ID));
    }
    if flags.long_capture {
        nodes.push(DockNode::Item(LONG_CAPTURE_ID));
    }
    nodes.push(DockNode::Delay);
    if flags.recording_visible {
        nodes.push(DockNode::Item(crate::tray::recording_item_id(
            flags.recording_active,
        )));
    }
    nodes.push(DockNode::Separator);
    nodes.push(DockNode::Item("settings"));
    nodes.push(DockNode::Item("history"));
    nodes.push(DockNode::Item(CLIPBOARD_PIN_ID));
    if flags.click_through {
        nodes.push(DockNode::Item(EXIT_CLICK_THROUGH_ID));
    }
    nodes
}

/// 与托盘「延时」子菜单相同的区域截取 id。
#[cfg(any(test, target_os = "macos"))]
fn dock_delay_id(seconds: u64) -> String {
    format!("capture-region-delay-{seconds}")
}

/// 能走 `app.exit(0)` 时取消 AppKit 的退出,避免它接着关窗口。
#[cfg(any(test, target_os = "macos"))]
fn dock_quit_reply(can_exit: bool) -> usize {
    if can_exit {
        TERMINATE_CANCEL
    } else {
        TERMINATE_NOW
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::cell::RefCell;
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, NSObject, Sel};
    use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly, Message};
    use objc2_app_kit::{NSMenu, NSMenuItem};
    use objc2_foundation::NSString;
    use tauri::AppHandle;

    use crate::hotkeys::menu_item_title;
    use crate::i18n;
    use crate::settings;
    use crate::tray::{
        self, CLIPBOARD_PIN_ID, EXIT_CLICK_THROUGH_ID, LAST_REGION_ID, LONG_CAPTURE_ID,
        RECORDING_ID, RECORDING_STOP_ID,
    };

    use super::{dock_delay_id, dock_nodes, dock_quit_reply, DockNode};

    static APP: OnceLock<AppHandle> = OnceLock::new();
    static METHODS_INSTALLED: AtomicBool = AtomicBool::new(false);

    thread_local! {
        static DOCK_TARGET: RefCell<Option<Retained<DockMenuTarget>>> = const { RefCell::new(None) };
        static DOCK_MENU: RefCell<Option<Retained<NSMenu>>> = const { RefCell::new(None) };
    }

    // 菜单项的 target。NSMenuItem.target 是弱引用,实例必须留在 DOCK_TARGET 里。
    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "CropmarkDockMenuTarget"]
        #[thread_kind = MainThreadOnly]
        struct DockMenuTarget;

        impl DockMenuTarget {
            #[unsafe(method(activate:))]
            fn activate(&self, item: &NSMenuItem) {
                let Some(object) = item.representedObject() else {
                    return;
                };
                let Ok(text) = object.downcast::<NSString>() else {
                    return;
                };
                let id = text.to_string();
                if id.is_empty() {
                    return;
                }
                let Some(app) = app_handle() else {
                    return;
                };
                tray::handle_menu_id(app, &id);
            }
        }
    );

    pub(crate) fn install(app: &AppHandle) {
        let _ = APP.set(app.clone());
        if METHODS_INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        add_delegate_method(
            sel!(applicationDockMenu:),
            menu_imp(application_dock_menu),
            c"@@:@",
            "dock-menu",
        );
        add_delegate_method(
            sel!(applicationShouldTerminate:),
            terminate_imp(application_should_terminate),
            c"Q@:@",
            "terminate",
        );
        if let Some(mtm) = MainThreadMarker::new() {
            let _ = menu_target(mtm);
        }
    }

    fn app_handle() -> Option<&'static AppHandle> {
        APP.get()
    }

    fn add_delegate_method(sel: Sel, imp: Imp, types: &CStr, kind: &str) {
        let Some(class) = AnyClass::get(c"TaoAppDelegateParent") else {
            log::warn!("dock menu failed kind=delegate");
            return;
        };
        // SAFETY: 类是 tao 启动时注册的应用委托。sel/imp/types 描述一个尚未
        // 存在的实例方法,不替换 tao 已有实现。
        let added = unsafe {
            objc2::ffi::class_addMethod(
                std::ptr::from_ref(class).cast_mut(),
                sel,
                imp,
                types.as_ptr(),
            )
        };
        if !added.as_bool() {
            log::warn!("dock menu failed kind={kind}");
        }
    }

    fn menu_imp(
        func: unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject) -> *mut AnyObject,
    ) -> Imp {
        // SAFETY: 函数指针与 Imp 同宽。类型串 @@:@ 对应该签名。
        unsafe { std::mem::transmute(func) }
    }

    fn terminate_imp(
        func: unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject) -> usize,
    ) -> Imp {
        // SAFETY: 函数指针与 Imp 同宽。类型串 Q@:@ 对应 NSUInteger 返回值。
        unsafe { std::mem::transmute(func) }
    }

    unsafe extern "C-unwind" fn application_dock_menu(
        _this: &AnyObject,
        _cmd: Sel,
        _sender: &AnyObject,
    ) -> *mut AnyObject {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(build_dock_menu)) {
            Ok(ptr) => ptr,
            Err(_) => {
                log::warn!("dock menu failed kind=build");
                std::ptr::null_mut()
            }
        }
    }

    unsafe extern "C-unwind" fn application_should_terminate(
        _this: &AnyObject,
        _cmd: Sel,
        _sender: &AnyObject,
    ) -> usize {
        let Some(app) = app_handle() else {
            return dock_quit_reply(false);
        };
        app.exit(0);
        dock_quit_reply(true)
    }

    fn build_dock_menu() -> *mut AnyObject {
        let Some(mtm) = MainThreadMarker::new() else {
            log::warn!("dock menu failed kind=thread");
            return std::ptr::null_mut();
        };
        let Some(app) = app_handle() else {
            log::warn!("dock menu failed kind=app");
            return std::ptr::null_mut();
        };
        let target = menu_target(mtm);
        let menu = new_menu(mtm, "Cropmark");
        for node in dock_nodes(&current_flags(app)) {
            match node {
                DockNode::Separator => {
                    let separator = NSMenuItem::separatorItem(mtm);
                    menu.addItem(&separator);
                }
                DockNode::Item(id) => {
                    let title = title_for(app, id);
                    let item = action_item(mtm, &target, &title, id);
                    menu.addItem(&item);
                }
                DockNode::Delay => {
                    let submenu = delay_menu(mtm, &target);
                    let item = submenu_item(mtm, &i18n::t("tray.delay"), &submenu);
                    menu.addItem(&item);
                }
                DockNode::Fullscreen => {
                    let submenu = fullscreen_menu(mtm, app, &target);
                    let item = submenu_item(mtm, &i18n::t("tray.fullscreen"), &submenu);
                    menu.addItem(&item);
                }
            }
        }
        store_menu(menu)
    }

    fn current_flags(app: &AppHandle) -> super::DockFlags {
        let capture = settings::current_capture(app);
        let recording = settings::current_recording(app);
        let active = crate::capture::session::recording_active(app);
        super::DockFlags {
            multi_monitor: tray::fullscreen_uses_submenu(capture.multi_monitor),
            long_capture: capture.long_capture,
            has_last_region: settings::current_last_region(app).is_some(),
            recording_visible: tray::recording_item_visible(recording.enabled, active),
            recording_active: active,
            click_through: crate::pin::has_click_through(),
        }
    }

    fn title_for(app: &AppHandle, id: &str) -> String {
        let hotkeys = settings::current_hotkeys(app);
        match id {
            "capture-region" => menu_item_title(&i18n::t("tray.region"), &hotkeys.region),
            "capture-window" => menu_item_title(&i18n::t("tray.window"), &hotkeys.window),
            "capture-fullscreen" => {
                menu_item_title(&i18n::t("tray.fullscreen"), &hotkeys.fullscreen)
            }
            LAST_REGION_ID => tray::last_region_label(true),
            LONG_CAPTURE_ID => i18n::t("tray.long_capture"),
            RECORDING_ID => tray::recording_item_label(false),
            RECORDING_STOP_ID => tray::recording_item_label(true),
            "settings" => i18n::t("tray.settings"),
            "history" => i18n::t("tray.history"),
            CLIPBOARD_PIN_ID => {
                menu_item_title(&i18n::t("tray.pin_from_clipboard"), &hotkeys.pin_clipboard)
            }
            EXIT_CLICK_THROUGH_ID => i18n::t("tray.exit_click_through"),
            other => other.to_string(),
        }
    }

    fn fullscreen_menu(
        mtm: MainThreadMarker,
        app: &AppHandle,
        target: &DockMenuTarget,
    ) -> Retained<NSMenu> {
        let hotkeys = settings::current_hotkeys(app);
        let menu = new_menu(mtm, &i18n::t("tray.fullscreen"));
        let pointer = action_item(
            mtm,
            target,
            &menu_item_title(&i18n::t("tray.fullscreen_pointer"), &hotkeys.fullscreen),
            "capture-fullscreen",
        );
        menu.addItem(&pointer);
        for (index, monitor) in crate::capture::session::tauri_monitors(app)
            .iter()
            .enumerate()
        {
            let id = tray::fullscreen_monitor_id(&crate::capture::geometry::monitor_key(monitor));
            let title = tray::fullscreen_screen_label(
                index,
                monitor.physical_width,
                monitor.physical_height,
            );
            let item = action_item(mtm, target, &title, &id);
            menu.addItem(&item);
        }
        let all = action_item(
            mtm,
            target,
            &i18n::t("tray.fullscreen_all"),
            tray::FULLSCREEN_ALL_ID,
        );
        menu.addItem(&all);
        menu
    }

    fn delay_menu(mtm: MainThreadMarker, target: &DockMenuTarget) -> Retained<NSMenu> {
        let menu = new_menu(mtm, &i18n::t("tray.delay"));
        for seconds in tray::FIXED_DELAY_SECONDS {
            let id = dock_delay_id(seconds);
            let title = i18n::tp("tray.delay_once", &[("seconds", &seconds.to_string())]);
            let item = action_item(mtm, target, &title, &id);
            menu.addItem(&item);
        }
        menu
    }

    fn new_menu(mtm: MainThreadMarker, title: &str) -> Retained<NSMenu> {
        let menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(title));
        menu.setAutoenablesItems(false);
        menu
    }

    fn action_item(
        mtm: MainThreadMarker,
        target: &DockMenuTarget,
        title: &str,
        id: &str,
    ) -> Retained<NSMenuItem> {
        // 键等效留空。热键文本只写在标题里,避免和全局热键各触发一次。
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(title),
                Some(sel!(activate:)),
                &NSString::from_str(""),
            )
        };
        let represented = NSString::from_str(id);
        // SAFETY: target 是 CropmarkDockMenuTarget,由 DOCK_TARGET 持有;
        // 菜单项对 target 只做弱引用。representedObject 会保留这份 NSString。
        unsafe {
            item.setTarget(Some(as_object(target)));
            item.setRepresentedObject(Some(as_object(&*represented)));
        }
        item.setEnabled(true);
        item
    }

    fn submenu_item(mtm: MainThreadMarker, title: &str, submenu: &NSMenu) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(title),
                None,
                &NSString::from_str(""),
            )
        };
        item.setSubmenu(Some(submenu));
        item
    }

    fn menu_target(mtm: MainThreadMarker) -> Retained<DockMenuTarget> {
        if let Some(existing) = DOCK_TARGET.with(|slot| slot.borrow().clone()) {
            return existing;
        }
        let allocated = mtm.alloc::<DockMenuTarget>().set_ivars(());
        let created: Retained<DockMenuTarget> = unsafe { msg_send![super(allocated), init] };
        DOCK_TARGET.with(|slot| {
            *slot.borrow_mut() = Some(created.clone());
        });
        created
    }

    fn store_menu(menu: Retained<NSMenu>) -> *mut AnyObject {
        // AppKit 不保留 applicationDockMenu: 的返回值。菜单要活到下次右键。
        let ptr = Retained::as_ptr(&menu).cast::<AnyObject>().cast_mut();
        DOCK_MENU.with(|slot| {
            *slot.borrow_mut() = Some(menu);
        });
        ptr
    }

    fn as_object(object: &impl Message) -> &AnyObject {
        // SAFETY: 这是一个 Objective-C 对象,指针指向同一实例。
        unsafe { &*std::ptr::from_ref(object).cast::<AnyObject>() }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use native::install;

#[cfg(test)]
mod tests {
    use super::{
        dock_delay_id, dock_nodes, dock_quit_reply, DockFlags, DockNode, TERMINATE_CANCEL,
        TERMINATE_NOW,
    };
    use crate::hotkeys::CaptureMode;
    use crate::tray::{
        menu_action, DelayChoice, MenuAction, CLIPBOARD_PIN_ID, EXIT_CLICK_THROUGH_ID,
        FIXED_DELAY_SECONDS, LAST_REGION_ID, LONG_CAPTURE_ID, RECORDING_ID, RECORDING_STOP_ID,
    };

    fn item(id: &'static str) -> DockNode {
        DockNode::Item(id)
    }

    #[test]
    fn dock_menu_offers_capture_settings_and_history() {
        assert_eq!(
            dock_nodes(&DockFlags::default()),
            vec![
                item("capture-region"),
                item("capture-window"),
                item("capture-fullscreen"),
                DockNode::Delay,
                DockNode::Separator,
                item("settings"),
                item("history"),
                item(CLIPBOARD_PIN_ID),
            ]
        );
    }

    #[test]
    fn optional_entries_keep_quit_on_the_system_menu() {
        let nodes = dock_nodes(&DockFlags {
            multi_monitor: true,
            long_capture: true,
            has_last_region: true,
            recording_visible: true,
            recording_active: true,
            click_through: true,
        });
        assert!(nodes
            .iter()
            .all(|node| !matches!(node, DockNode::Item("quit"))));
        assert!(nodes.contains(&DockNode::Fullscreen));
        assert!(!nodes.contains(&item("capture-fullscreen")));
        assert!(nodes.contains(&item(LAST_REGION_ID)));
        assert!(nodes.contains(&item(LONG_CAPTURE_ID)));
        assert!(nodes.contains(&item(RECORDING_STOP_ID)));
        assert_eq!(nodes.last(), Some(&item(EXIT_CLICK_THROUGH_ID)));
        let delay = nodes
            .iter()
            .position(|node| *node == DockNode::Delay)
            .unwrap();
        let last_region = nodes
            .iter()
            .position(|node| *node == item(LAST_REGION_ID))
            .unwrap();
        assert!(last_region < delay);
    }

    #[test]
    fn idle_recording_uses_the_start_item() {
        let nodes = dock_nodes(&DockFlags {
            recording_visible: true,
            ..DockFlags::default()
        });
        assert!(nodes.contains(&item(RECORDING_ID)));
        assert!(!nodes.contains(&item(RECORDING_STOP_ID)));
    }

    #[test]
    fn dock_delay_ids_follow_the_tray_region_delay() {
        for seconds in FIXED_DELAY_SECONDS {
            assert_eq!(
                menu_action(&dock_delay_id(seconds)),
                Some(MenuAction {
                    mode: CaptureMode::Region,
                    delay: DelayChoice::Once(seconds * 1000),
                })
            );
        }
    }

    #[test]
    fn dock_quit_cancels_appkit_when_cropmark_can_exit() {
        assert_eq!(dock_quit_reply(true), TERMINATE_CANCEL);
        assert_eq!(TERMINATE_CANCEL, 0);
        assert_eq!(dock_quit_reply(false), TERMINATE_NOW);
        assert_eq!(TERMINATE_NOW, 1);
    }
}
