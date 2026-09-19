//! 托盘菜单三平台共用同一份定义(区域/窗口/全屏/延时/设置/历史/退出),
//! 不支持的平台能力(托盘弹窗检测等)由 capture/platform 静默降级,不影响
//! 此处入口。构建失败(Err 或构建期 panic)不在本模块处理,由 `install_guarded`
//! 归一后交给 `lib.rs` 记录降级并继续启动(R16)。

use tauri::image::Image;
use tauri::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::AppHandle;

use crate::hotkeys::CaptureMode;
use crate::settings;

pub const TRAY_ID: &str = "cropmark-tray";

/// 托盘"上次区域"直取(R6)的菜单 id。
pub const LAST_REGION_ID: &str = "capture-last-region";

/// 延时一次性延时的档位(秒):保留 3/5/10,另加"使用设置的延时"。
pub const FIXED_DELAY_SECONDS: [u64; 3] = [3, 5, 10];

/// 无记录时菜单项禁用且标签即提示(禁用项无法点击,提示只能靠文案承载)。
pub fn last_region_label(has_region: bool) -> &'static str {
    if has_region {
        "上次区域"
    } else {
        "上次区域（暂无记录）"
    }
}

/// R16:托盘构建失败(Err 或构建期 panic)时的用户可见提示。以安装路径的
/// 实际结果为唯一判据,不做 AppIndicator/StatusNotifier 探测(DE 图标不可见
/// 属不可检测场景);提示需给出仍可用能力与替代入口。
pub fn unavailable_message() -> String {
    "当前桌面环境未提供托盘，热键仍可用；可再次启动 Cropmark 打开设置，或在此退出应用。".into()
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
    let seconds = settings::current_capture(app).delay_seconds;
    let menu = build_menu(app, seconds)?;
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

/// 延时设置变化或"上次区域"记录更新后重建托盘菜单:裁剪主项、"使用设置的
/// 延时(N 秒)"标签与"上次区域"可用状态/标签始终与当前状态一致。
/// 菜单事件按 id 分发,重建不丢处理器。
pub fn refresh_menu(app: &AppHandle) {
    let seconds = settings::current_capture(app).delay_seconds;
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match build_menu(app, seconds) {
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

pub fn configured_delay_label(seconds: u32) -> String {
    format!("使用设置的延时（{seconds} 秒）")
}

fn build_menu(app: &AppHandle, configured_seconds: u32) -> tauri::Result<Menu<tauri::Wry>> {
    let region = MenuItem::with_id(app, "capture-region", "区域", true, None::<&str>)?;
    let has_last_region = crate::settings::current_last_region(app).is_some();
    let last_region = MenuItem::with_id(
        app,
        LAST_REGION_ID,
        last_region_label(has_last_region),
        has_last_region,
        None::<&str>,
    )?;
    let window = MenuItem::with_id(app, "capture-window", "窗口", true, None::<&str>)?;
    let fullscreen = MenuItem::with_id(app, "capture-fullscreen", "全屏", true, None::<&str>)?;

    let mut fixed_delays = Vec::new();
    for seconds in FIXED_DELAY_SECONDS {
        fixed_delays.push(fixed_delay_submenu(app, seconds)?);
    }
    let delay_configured = configured_delay_submenu(app, configured_seconds)?;

    let mut capture_items: Vec<&dyn IsMenuItem<tauri::Wry>> =
        vec![&region, &last_region, &window, &fullscreen];
    for submenu in &fixed_delays {
        capture_items.push(submenu);
    }
    capture_items.push(&delay_configured);
    let capture = Submenu::with_items(app, "截取", true, &capture_items)?;
    let settings_item = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
    let history_item = MenuItem::with_id(app, "history", "历史记录", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    Menu::with_items(
        app,
        &[
            &capture,
            &PredefinedMenuItem::separator(app)?,
            &settings_item,
            &history_item,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )
}

fn fixed_delay_submenu(app: &AppHandle, seconds: u64) -> tauri::Result<Submenu<tauri::Wry>> {
    let region = MenuItem::with_id(
        app,
        format!("capture-region-delay-{seconds}"),
        "区域",
        true,
        None::<&str>,
    )?;
    let window = MenuItem::with_id(
        app,
        format!("capture-window-delay-{seconds}"),
        "窗口",
        true,
        None::<&str>,
    )?;
    let fullscreen = MenuItem::with_id(
        app,
        format!("capture-fullscreen-delay-{seconds}"),
        "全屏",
        true,
        None::<&str>,
    )?;
    Submenu::with_items(
        app,
        format!("延时 {seconds} 秒"),
        true,
        &[&region, &window, &fullscreen],
    )
}

fn configured_delay_submenu(app: &AppHandle, seconds: u32) -> tauri::Result<Submenu<tauri::Wry>> {
    let region = MenuItem::with_id(
        app,
        "capture-region-delay-setting",
        "区域",
        true,
        None::<&str>,
    )?;
    let window = MenuItem::with_id(
        app,
        "capture-window-delay-setting",
        "窗口",
        true,
        None::<&str>,
    )?;
    let fullscreen = MenuItem::with_id(
        app,
        "capture-fullscreen-delay-setting",
        "全屏",
        true,
        None::<&str>,
    )?;
    Submenu::with_items(
        app,
        configured_delay_label(seconds),
        true,
        &[&region, &window, &fullscreen],
    )
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
    fn non_capture_and_malformed_ids_are_ignored() {
        assert_eq!(menu_action("settings"), None);
        assert_eq!(menu_action("quit"), None);
        assert_eq!(menu_action("capture-delay"), None);
        assert_eq!(menu_action("capture-unknown-delay-3"), None);
        assert_eq!(menu_action("capture-region-delay-x"), None);
        assert_eq!(menu_action("capture-region-delay-"), None);
    }

    #[test]
    fn configured_delay_label_shows_seconds_and_tracks_changes() {
        assert_eq!(configured_delay_label(0), "使用设置的延时（0 秒）");
        assert_eq!(configured_delay_label(5), "使用设置的延时（5 秒）");
        assert_ne!(configured_delay_label(5), configured_delay_label(10));
    }

    #[test]
    fn last_region_label_hints_when_no_record() {
        assert_eq!(last_region_label(true), "上次区域");
        assert_eq!(last_region_label(false), "上次区域（暂无记录）");
        assert!(last_region_label(false).contains("暂无记录"));
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
