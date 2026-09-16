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
    let capture = Submenu::with_id_and_items(
        app,
        "capture",
        "截取",
        true,
        &[&region, &window, &fullscreen],
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
