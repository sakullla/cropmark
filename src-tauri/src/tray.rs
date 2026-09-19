use tauri::image::Image;
use tauri::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::AppHandle;

use crate::hotkeys::CaptureMode;
use crate::settings;

pub const TRAY_ID: &str = "cropmark-tray";

/// 托盘一次性延时的档位(秒):保留 3/5/10,另加"使用设置的延时"。
pub const FIXED_DELAY_SECONDS: [u64; 3] = [3, 5, 10];

pub fn install(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
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

/// 延时设置变化后重建托盘菜单:裁剪主项与"使用设置的延时(N 秒)"标签
/// 始终与当前配置一致。菜单事件按 id 分发,重建不丢处理器。
pub fn refresh_delay_menu(app: &AppHandle) {
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
    let window = MenuItem::with_id(app, "capture-window", "窗口", true, None::<&str>)?;
    let fullscreen = MenuItem::with_id(app, "capture-fullscreen", "全屏", true, None::<&str>)?;

    let mut fixed_delays = Vec::new();
    for seconds in FIXED_DELAY_SECONDS {
        fixed_delays.push(fixed_delay_submenu(app, seconds)?);
    }
    let delay_configured = configured_delay_submenu(app, configured_seconds)?;

    let mut capture_items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&region, &window, &fullscreen];
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
}
