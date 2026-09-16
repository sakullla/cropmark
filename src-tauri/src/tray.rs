use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::AppHandle;

use crate::hotkeys::CaptureMode;
use crate::settings;

pub fn install(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let region = MenuItem::with_id(app, "capture-region", "区域", true, None::<&str>)?;
    let window = MenuItem::with_id(app, "capture-window", "窗口", true, None::<&str>)?;
    let fullscreen = MenuItem::with_id(app, "capture-fullscreen", "全屏", true, None::<&str>)?;
    let delay_region = MenuItem::with_id(app, "capture-region-delay-3", "区域", true, None::<&str>)?;
    let delay_window = MenuItem::with_id(app, "capture-window-delay-3", "窗口", true, None::<&str>)?;
    let delay_fullscreen =
        MenuItem::with_id(app, "capture-fullscreen-delay-3", "全屏", true, None::<&str>)?;
    let delay = Submenu::with_id_and_items(
        app,
        "capture-delay",
        "延时 3 秒",
        true,
        &[&delay_region, &delay_window, &delay_fullscreen],
    )?;
    let capture = Submenu::with_id_and_items(
        app,
        "capture",
        "截取",
        true,
        &[&region, &window, &fullscreen, &delay],
    )?;
    let settings_item = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &capture,
            &PredefinedMenuItem::separator(app)?,
            &settings_item,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let icon = tray_icon()?;
    TrayIconBuilder::with_id("cropmark-tray")
        .icon(icon)
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("Cropmark")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "capture-region" => crate::dispatch_capture(app, CaptureMode::Region),
            "capture-window" => crate::dispatch_capture(app, CaptureMode::Window),
            "capture-fullscreen" => crate::dispatch_capture(app, CaptureMode::Fullscreen),
            "capture-region-delay-3" => {
                crate::dispatch_capture_with_delay(app, CaptureMode::Region, 3000)
            }
            "capture-window-delay-3" => {
                crate::dispatch_capture_with_delay(app, CaptureMode::Window, 3000)
            }
            "capture-fullscreen-delay-3" => {
                crate::dispatch_capture_with_delay(app, CaptureMode::Fullscreen, 3000)
            }
            "settings" => {
                let _ = settings::open_settings(app);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn tray_icon() -> Result<Image<'static>, Box<dyn std::error::Error>> {
    Ok(Image::from_bytes(include_bytes!("../icons/32x32.png"))?)
}
