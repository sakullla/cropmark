//! 托盘菜单三平台共用同一份定义(区域/窗口/全屏/延时区域/设置/历史/退出),
//! 不支持的平台能力(托盘弹窗检测等)由 capture/platform 静默降级,不影响
//! 此处入口。构建失败(Err 或构建期 panic)不在本模块处理,由 `install_guarded`
//! 归一后交给 `lib.rs` 记录降级并继续启动(R16)。

use tauri::image::Image;
use tauri::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::AppHandle;

use crate::hotkeys::CaptureMode;
use crate::i18n;
use crate::settings;

pub const TRAY_ID: &str = "cropmark-tray";

/// 托盘"上次区域"直取(R6)的菜单 id。
pub const LAST_REGION_ID: &str = "capture-last-region";

/// R1 托盘长截图入口的菜单 id(功能开关开启时出现)。
pub const LONG_CAPTURE_ID: &str = "capture-long";

/// R2 托盘「退出贴图穿透」的菜单 id:任一贴图处于穿透时出现,
/// 是穿透状态必达的全局退出路径。
pub const EXIT_CLICK_THROUGH_ID: &str = "pin-exit-click-through";

/// 一次性延时档位(秒):托盘「延时」子菜单,点击即按该秒数做区域截取。
/// 窗口/全屏延时走设置里的延时秒数,避免 delay×mode 的嵌套菜单。
pub const FIXED_DELAY_SECONDS: [u64; 3] = [3, 5, 10];

/// 无记录时菜单项禁用且标签即提示(禁用项无法点击,提示只能靠文案承载)。
pub fn last_region_label(has_region: bool) -> String {
    i18n::t(if has_region {
        "tray.last_region"
    } else {
        "tray.last_region_empty"
    })
}

/// "上次区域"项可用 = 有记录。R19:旧 lastRegion 开关按常开语义移除,
/// 无记录时只禁用菜单项并以标签提示,记录保留不受影响。
fn last_region_enabled(has_region: bool) -> bool {
    has_region
}

/// R1:长截图入口可见 = 功能开关开启;关闭时不构建菜单项(入口不出现)。
fn long_capture_enabled(enabled: bool) -> bool {
    enabled
}

/// R2:只要仍有贴图处于穿透,托盘就必须保留退出项——穿透会拦截窗口自身
/// 的鼠标事件,托盘是唯一可达的恢复入口;安全出口优先于开关隐藏规则。
fn exit_click_through_visible(any_click_through: bool) -> bool {
    any_click_through
}

/// R16:托盘构建失败(Err 或构建期 panic)时的用户可见提示。以安装路径的
/// 实际结果为唯一判据,不做 AppIndicator/StatusNotifier 探测(DE 图标不可见
/// 属不可检测场景);提示需给出仍可用能力与替代入口。
pub fn unavailable_message() -> String {
    i18n::t("tray.unavailable")
}

/// R16:托盘安装入口,把构建期 panic 与 `Err` 一视同仁地归一为错误。
///
/// Linux 缺少 AppIndicator 动态库时锁定依赖不会返回 `Err`:libappindicator-sys
/// 的 static LIB 在 libayatana-appindicator3.so.1 与 libappindicator3.so.1 均
/// dlopen 失败时直接 `panic!`,而该 panic 位于 `TrayIconBuilder::build` 调用链内,
/// 若沿 setup 向外传播会跳过热键注册与设置窗口打开,使无托盘环境整体无法启动。
/// 这里用 catch_unwind 捕获该 panic,与 `Err` 走同一条降级链。
///
/// 不采用安装前 dlopen 预检:预检只能覆盖"库文件不存在"这一已知形态,无法覆盖
/// 库存在但符号缺失等其它构建期 panic;catch_unwind 覆盖安装路径的全部 panic。
/// 已确认该 panic 链路为纯 Rust 栈帧(不穿过 C 帧),且发生在托盘注册与任何 GTK
/// 调用之前,捕获后 Tauri/GTK 状态不受影响;捕获后不再重试构建,后续
/// `refresh_menu` 因 `tray_by_id` 找不到托盘而自动跳过。
pub fn install_guarded(app: &AppHandle) -> Result<(), String> {
    guard_install(|| install(app))
}

/// panic 归一化的可测试包装:错误转为文本,panic payload 提取为可读文本。
fn guard_install<F>(install: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), Box<dyn std::error::Error>>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(install)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(payload) => Err(panic_message(payload.as_ref())),
    }
}

/// panic payload 的可读文本,仅用于日志;用户可见提示固定为
/// `unavailable_message()`。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "托盘构建过程发生 panic".into()
    }
}

fn install(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let menu = build_menu(app)?;
    let icon = tray_icon()?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("Cropmark")
        .on_menu_event(handle_menu_event)
        .build(app)?;
    Ok(())
}

/// "上次区域"记录或功能开关变化后重建托盘菜单,标签与可用状态与当前状态一致。
/// 菜单事件按 id 分发,重建不丢处理器。
pub fn refresh_menu(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match build_menu(app) {
        Ok(menu) => {
            let _ = tray.set_menu(Some(menu));
        }
        Err(error) => eprintln!("Cropmark: 无法更新托盘菜单:{error}"),
    }
}

/// 托盘一次性延时子菜单 id 与档位的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayChoice {
    /// 使用设置中的延时(热键同源)。
    Configured,
    /// 本次截取覆盖为指定毫秒数。
    Once(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuAction {
    pub mode: CaptureMode,
    pub delay: DelayChoice,
}

/// 截取菜单 id → 动作;"设置"/"退出"等非截取项返回 None。
pub fn menu_action(id: &str) -> Option<MenuAction> {
    let rest = id.strip_prefix("capture-")?;
    let (mode_token, delay_token) = match rest.rsplit_once("-delay-") {
        Some((mode, delay)) => (mode, Some(delay)),
        None => (rest, None),
    };
    let mode = match mode_token {
        "region" => CaptureMode::Region,
        "window" => CaptureMode::Window,
        "fullscreen" => CaptureMode::Fullscreen,
        "long" => CaptureMode::LongCapture,
        _ => return None,
    };
    let delay = match delay_token {
        None | Some("setting") => DelayChoice::Configured,
        Some(token) => DelayChoice::Once(token.parse::<u64>().ok()? * 1000),
    };
    Some(MenuAction { mode, delay })
}

fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id.as_ref() {
        "settings" => {
            let _ = settings::open_settings(app);
        }
        "history" => {
            let _ = crate::history::open_window(app);
        }
        LAST_REGION_ID => crate::dispatch_last_region(app),
        EXIT_CLICK_THROUGH_ID => crate::pin::exit_pin_click_through(app.clone()),
        "quit" => app.exit(0),
        id => {
            if let Some(action) = menu_action(id) {
                match action.delay {
                    DelayChoice::Configured => crate::dispatch_capture(app, action.mode),
                    DelayChoice::Once(delay_ms) => {
                        crate::dispatch_capture_with_delay(app, action.mode, delay_ms)
                    }
                }
            }
        }
    }
}

fn delay_once_label(seconds: u64) -> String {
    i18n::tp("tray.delay_once", &[("seconds", &seconds.to_string())])
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let region = MenuItem::with_id(
        app,
        "capture-region",
        i18n::t("tray.region"),
        true,
        None::<&str>,
    )?;
    let has_last_region = crate::settings::current_last_region(app).is_some();
    let last_region = MenuItem::with_id(
        app,
        LAST_REGION_ID,
        last_region_label(has_last_region),
        last_region_enabled(has_last_region),
        None::<&str>,
    )?;
    let window = MenuItem::with_id(
        app,
        "capture-window",
        i18n::t("tray.window"),
        true,
        None::<&str>,
    )?;
    let fullscreen = MenuItem::with_id(
        app,
        "capture-fullscreen",
        i18n::t("tray.fullscreen"),
        true,
        None::<&str>,
    )?;
    // R1:长截图入口仅在功能开关开启时进入截取子菜单。
    let long_capture_visible = long_capture_enabled(settings::current_toggles(app).long_capture);
    let long_capture = MenuItem::with_id(
        app,
        LONG_CAPTURE_ID,
        i18n::t("tray.long_capture"),
        long_capture_visible,
        None::<&str>,
    )?;
    let delay = delay_submenu(app)?;
    let mut capture_items: Vec<&dyn IsMenuItem<tauri::Wry>> =
        vec![&region, &last_region, &window, &fullscreen];
    if long_capture_visible {
        capture_items.push(&long_capture);
    }
    capture_items.push(&delay);
    let capture = Submenu::with_items(app, i18n::t("tray.capture"), true, &capture_items)?;
    let settings_item = MenuItem::with_id(
        app,
        "settings",
        i18n::t("tray.settings"),
        true,
        None::<&str>,
    )?;
    let history_item =
        MenuItem::with_id(app, "history", i18n::t("tray.history"), true, None::<&str>)?;
    // R2:穿透中的贴图无法接收鼠标事件,托盘项是唯一退出路径。
    let exit_click_through = MenuItem::with_id(
        app,
        EXIT_CLICK_THROUGH_ID,
        i18n::t("tray.exit_click_through"),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", i18n::t("tray.quit"), true, None::<&str>)?;
    let first_separator = PredefinedMenuItem::separator(app)?;
    let second_separator = PredefinedMenuItem::separator(app)?;
    let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> =
        vec![&capture, &first_separator, &settings_item, &history_item];
    let click_through_visible = exit_click_through_visible(crate::pin::has_click_through());
    if click_through_visible {
        items.push(&exit_click_through);
    }
    items.push(&second_separator);
    items.push(&quit);
    Menu::with_items(app, &items)
}

/// 一次性延时:只挂区域截取。窗口/全屏用设置页的延时,避免每个档位再拆三种模式。
fn delay_submenu(app: &AppHandle) -> tauri::Result<Submenu<tauri::Wry>> {
    let items = FIXED_DELAY_SECONDS
        .into_iter()
        .map(|seconds| {
            MenuItem::with_id(
                app,
                format!("capture-region-delay-{seconds}"),
                delay_once_label(seconds),
                true,
                None::<&str>,
            )
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let refs: Vec<&dyn IsMenuItem<tauri::Wry>> = items
        .iter()
        .map(|item| item as &dyn IsMenuItem<tauri::Wry>)
        .collect();
    Submenu::with_items(app, i18n::t("tray.delay"), true, &refs)
}

fn tray_icon() -> Result<Image<'static>, Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    let bytes = include_bytes!("../icons/v2/tray-template.png").as_slice();
    #[cfg(not(target_os = "macos"))]
    let bytes = include_bytes!("../icons/v2/32x32.png").as_slice();
    Ok(Image::from_bytes(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(mode: CaptureMode, delay: DelayChoice) -> MenuAction {
        MenuAction { mode, delay }
    }

    #[test]
    fn main_items_use_configured_delay() {
        assert_eq!(
            menu_action("capture-region"),
            Some(action(CaptureMode::Region, DelayChoice::Configured))
        );
        assert_eq!(
            menu_action("capture-window"),
            Some(action(CaptureMode::Window, DelayChoice::Configured))
        );
        assert_eq!(
            menu_action("capture-fullscreen"),
            Some(action(CaptureMode::Fullscreen, DelayChoice::Configured))
        );
    }

    #[test]
    fn fixed_delay_items_map_to_seconds() {
        for seconds in FIXED_DELAY_SECONDS {
            for (mode, token) in [
                (CaptureMode::Region, "region"),
                (CaptureMode::Window, "window"),
                (CaptureMode::Fullscreen, "fullscreen"),
            ] {
                let id = format!("capture-{token}-delay-{seconds}");
                assert_eq!(
                    menu_action(&id),
                    Some(action(mode, DelayChoice::Once(seconds * 1000))),
                    "{id}"
                );
            }
        }
    }

    #[test]
    fn configured_delay_items_use_configured_choice() {
        assert_eq!(
            menu_action("capture-region-delay-setting"),
            Some(action(CaptureMode::Region, DelayChoice::Configured))
        );
        assert_eq!(
            menu_action("capture-fullscreen-delay-setting"),
            Some(action(CaptureMode::Fullscreen, DelayChoice::Configured))
        );
    }

    #[test]
    fn tray_delay_menu_uses_region_once_ids() {
        for seconds in FIXED_DELAY_SECONDS {
            assert_eq!(
                menu_action(&format!("capture-region-delay-{seconds}")),
                Some(action(
                    CaptureMode::Region,
                    DelayChoice::Once(seconds * 1000)
                )),
            );
        }
    }

    #[test]
    fn non_capture_and_malformed_ids_are_ignored() {
        assert_eq!(menu_action("settings"), None);
        assert_eq!(menu_action("quit"), None);
        assert_eq!(menu_action("capture-delay"), None);
        assert_eq!(menu_action("capture-unknown-delay-3"), None);
        assert_eq!(menu_action("capture-region-delay-x"), None);
        assert_eq!(menu_action("capture-region-delay-"), None);
    }

    #[test]
    fn delay_once_label_is_just_the_duration() {
        assert_eq!(delay_once_label(3), "3 秒");
        assert_eq!(delay_once_label(5), "5 秒");
        assert_eq!(delay_once_label(10), "10 秒");
    }

    #[test]
    fn last_region_label_hints_when_no_record() {
        assert_eq!(last_region_label(true), "上次区域");
        assert_eq!(last_region_label(false), "上次区域（暂无记录）");
        assert!(last_region_label(false).contains("暂无记录"));
    }

    #[test]
    fn last_region_item_requires_a_record_only() {
        // 有记录:可用。
        assert!(last_region_enabled(true));
        // 无记录:禁用(标签提示暂无记录)。
        assert!(!last_region_enabled(false));
    }

    #[test]
    fn long_capture_entry_maps_to_long_capture_mode() {
        assert_eq!(
            menu_action(LONG_CAPTURE_ID),
            Some(action(CaptureMode::LongCapture, DelayChoice::Configured))
        );
        // 长截图没有一次性延时子菜单项。
        assert_eq!(
            menu_action("capture-long-delay-3"),
            Some(action(CaptureMode::LongCapture, DelayChoice::Once(3000)))
        );
    }

    #[test]
    fn long_capture_entry_only_when_the_toggle_is_on() {
        assert!(long_capture_enabled(true));
        assert!(!long_capture_enabled(false));
    }

    #[test]
    fn exit_click_through_only_when_a_pin_is_click_through() {
        assert!(exit_click_through_visible(true));
        assert!(!exit_click_through_visible(false));
    }

    #[test]
    fn exit_click_through_id_is_not_parsed_as_a_capture_mode() {
        assert_eq!(menu_action(EXIT_CLICK_THROUGH_ID), None);
    }

    #[test]
    fn last_region_id_is_not_parsed_as_a_capture_mode() {
        assert_eq!(menu_action(LAST_REGION_ID), None);
        assert_eq!(menu_action("capture-last-region-delay-3"), None);
    }

    #[test]
    fn unavailable_message_explains_hotkeys_and_exit_path() {
        let message = unavailable_message();
        assert!(message.contains("托盘"));
        assert!(message.contains("热键"));
        assert!(message.contains("退出"));
        assert!(!message.contains("错误"));
    }

    #[test]
    fn guard_install_passes_success_and_normalizes_errors() {
        assert!(guard_install(|| Ok(())).is_ok());
        assert_eq!(
            guard_install(|| Err("缺少 AppIndicator".into())),
            Err("缺少 AppIndicator".to_string())
        );
    }

    #[test]
    fn guard_install_treats_panics_as_unavailable_like_errors() {
        // Linux 缺少 AppIndicator 时构建直接 panic 而非返回 Err:
        // 归一化后必须与 Err 一样返回可读文本,setup 才能继续降级启动。
        assert_eq!(
            guard_install(|| panic!("Failed to load appindicator3")),
            Err("Failed to load appindicator3".to_string())
        );
        assert_eq!(
            guard_install(|| std::panic::panic_any(String::from("动态库加载失败"))),
            Err("动态库加载失败".to_string())
        );
        assert_eq!(
            guard_install(|| std::panic::panic_any(42_u8)),
            Err("托盘构建过程发生 panic".to_string())
        );
    }
}
