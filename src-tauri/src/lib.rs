mod annotate;
mod autostart;
mod beautify;
mod capture;
mod clipboard;
mod export;
mod filename_template;
mod front;
mod history;
mod hotkeys;
mod i18n;
mod logging;
mod ocr;
mod pin;
mod pin_store;
mod settings;
mod single_instance;
mod tray;

use std::sync::{Arc, Mutex, OnceLock};

use hotkeys::CaptureMode;
use tauri::{Emitter, Manager};

pub fn dispatch_capture(app: &tauri::AppHandle, mode: CaptureMode) {
    // 热键与托盘主项读取设置中的延时(R4):0 秒立即截取。
    let delay_ms = settings::current_capture(app).delay_ms();
    dispatch_capture_with_delay(app, mode, delay_ms);
}

pub fn dispatch_capture_with_delay(app: &tauri::AppHandle, mode: CaptureMode, delay_ms: u64) {
    let _ = app.emit("capture-requested", mode);
    capture::begin(app, mode, delay_ms);
}

/// 托盘全屏子菜单(R9)。热键仍走 `dispatch_capture(Fullscreen)`,只抓指针屏。
pub fn dispatch_fullscreen(app: &tauri::AppHandle, target: capture::session::FullscreenTarget) {
    let delay_ms = settings::current_capture(app).delay_ms();
    let _ = app.emit("capture-requested", CaptureMode::Fullscreen);
    capture::begin_fullscreen(app, delay_ms, target);
}

/// 托盘"上次区域"直取(R6):不进入交互选区,按记录区域抓屏裁剪;
/// 延时与热键/托盘主项同源,触发路径与常规截取一致。
/// R19:旧 lastRegion 开关按常开语义移除,有记录即可直取。
pub fn dispatch_last_region(app: &tauri::AppHandle) {
    let delay_ms = settings::current_capture(app).delay_ms();
    let _ = app.emit("capture-requested", CaptureMode::Region);
    capture::begin_last_region(app, delay_ms);
}

pub fn should_prevent_exit(code: Option<i32>) -> bool {
    code.is_none()
}

/// R15:环境变量门控的启动就绪日志。`CROPMARK_STARTUP_TIMING` 设置时在
/// setup 收尾(托盘/降级、热键、预建窗口就绪,即视为可交互)输出一行时序:
/// `elapsed_ms` 从 `run()` 起算,`epoch_ms` 为就绪时刻的 Unix 毫秒(外部工具
/// 可据此换算进程创建到就绪的耗时,与无日志的 v0.2.4 基线同机对比)。未设置
/// 该变量时零输出、零额外开销。
fn log_startup_ready(started: std::time::Instant, tray_ready: bool) {
    if std::env::var_os("CROPMARK_STARTUP_TIMING").is_none() {
        return;
    }
    let elapsed_ms = started.elapsed().as_millis();
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let tray = if tray_ready { "ready" } else { "unavailable" };
    eprintln!("Cropmark: startup ready tray={tray} elapsed_ms={elapsed_ms} epoch_ms={epoch_ms}");
}

/// R16:设置窗口的退出入口(无托盘环境下的兜底退出路径)。复用托盘"退出"的
/// 同一条链:`app.exit(0)` 触发 `RunEvent::Exit`,由既有逻辑收掉全部贴图。
/// 保持模块内私有:`pub` 会让 `#[tauri::command]` 生成的宏与 `#[macro_export]`
/// 在 crate 根同名冲突(crate 根即本模块)。
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/// 在任何线程请求唤出设置窗:非主线程转发到主线程,主线程直接执行。
fn open_settings_on_main(app: &tauri::AppHandle) {
    let app = app.clone();
    let task_app = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(error) = settings::open_settings(&task_app) {
            eprintln!("Cropmark: 无法打开设置窗口:{error}");
        }
    });
}

const DEV_SERVER_PORT: u16 = 1420;
const DEV_HMR_PORT: u16 = 1421;

/// `TAURI_DEV_HOST` 只接受主机名或 IP。拒绝空白、通配符和能截断 CSP 的分隔符。
#[cfg(any(dev, test))]
fn normalize_dev_host(raw: &str) -> Option<String> {
    let host = raw.trim();
    if host.is_empty() || host.len() > 253 {
        return None;
    }
    if !host
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':'))
    {
        return None;
    }
    if host.contains(':') {
        if host.contains(":::") {
            return None;
        }
        return Some(format!("[{host}]"));
    }
    if host.starts_with(['.', '-']) || host.ends_with(['.', '-']) || host.contains("..") {
        return None;
    }
    Some(host.to_string())
}

#[cfg(any(dev, test))]
fn append_csp_source(policy: &str, directive: &str, source: &str) -> String {
    let mut directives = policy
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut found = false;
    for part in &mut directives {
        let mut tokens = part.split_whitespace();
        if tokens.next() != Some(directive) {
            continue;
        }
        found = true;
        if !part.split_whitespace().any(|token| token == source) {
            part.push(' ');
            part.push_str(source);
        }
        break;
    }
    if !found {
        directives.push(format!("{directive} {source}"));
    }
    directives.join("; ")
}

/// 把开发主机派生源追加到 dev CSP：页面与资源走 1420，HMR websocket 同时覆盖
/// 默认端口和 `TAURI_DEV_HOST` 显式配置的 1421。已存在的源不重复追加。
#[cfg(any(dev, test))]
fn extend_dev_csp(policy: &str, host: &str) -> Option<String> {
    let host = normalize_dev_host(host)?;
    let http = format!("http://{host}:{DEV_SERVER_PORT}");
    let ws_server = format!("ws://{host}:{DEV_SERVER_PORT}");
    let ws_hmr = format!("ws://{host}:{DEV_HMR_PORT}");
    let additions = [
        ("script-src", http.as_str()),
        ("style-src", http.as_str()),
        ("img-src", http.as_str()),
        ("connect-src", http.as_str()),
        ("connect-src", ws_server.as_str()),
        ("connect-src", ws_hmr.as_str()),
    ];
    let mut next = policy.to_string();
    for (directive, source) in additions {
        next = append_csp_source(&next, directive, source);
    }
    Some(next)
}

/// 开发构建才改 `devCsp`。生产策略保持配置文件原文，不读 `TAURI_DEV_HOST`。
#[cfg(dev)]
fn apply_dev_host_csp<R: tauri::Runtime>(context: &mut tauri::Context<R>) {
    let Ok(raw) = std::env::var("TAURI_DEV_HOST") else {
        return;
    };
    if raw.trim().is_empty() {
        return;
    }
    let Some(current) = context.config().app.security.dev_csp.clone() else {
        return;
    };
    match extend_dev_csp(&current.to_string(), &raw) {
        Some(next) => {
            context.config_mut().app.security.dev_csp =
                Some(tauri::utils::config::Csp::Policy(next));
        }
        None => {
            eprintln!("Cropmark: 忽略无效的 TAURI_DEV_HOST，开发 CSP 不追加该来源");
        }
    }
}

pub fn run() {
    // R16:panic hook 要在 Tauri 初始化之前装上,setup 再改绑到平台日志目录。
    logging::prepare();
    // R15:启动计时从进程入口附近起算,供门控日志在同机 release 构建中
    // 测量冷启动到可交互;未设置环境变量时该对象无任何可观察开销。
    let startup_started = std::time::Instant::now();
    // R21 呈现诊断:进程 DPI 感知必须在任何 HWND 创建前设置——Windows 上
    // 一旦创建窗口,进程 DPI 感知即不可再变更(旧系统上调用会以
    // ERROR_ACCESS_DENIED 失败),覆盖层会被系统位图拉伸而模糊。单实例
    // 闸门在 Windows 上会创建隐藏消息窗,因此本调用提前到它之前;非 Windows
    // 平台为空实现。
    capture::platform::enable_per_monitor_v2();
    // R1:单实例闸门先于任何 Tauri 初始化。已有实例时本进程在转发启动参数后
    // 直接退出,不创建托盘/窗口,也不注册热键;首实例无响应时同样有界退出。
    let settings_slot: Arc<OnceLock<tauri::AppHandle>> = Arc::new(OnceLock::new());
    let pending_activations: Arc<Mutex<Vec<single_instance::SecondLaunch>>> =
        Arc::new(Mutex::new(Vec::new()));
    let slot = Arc::clone(&settings_slot);
    let pending = Arc::clone(&pending_activations);
    let handler: single_instance::LaunchHandler =
        Arc::new(move |launch: single_instance::SecondLaunch| {
            let Some(app) = slot.get().cloned() else {
                // 首实例尚未完成 setup(Unix 监听线程可能先于窗口就绪收到转发):
                // 先记下,setup 完成后再补开设置窗,避免丢失这次激活。
                pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(launch);
                return;
            };
            open_settings_on_main(&app);
        });
    let _primary = match single_instance::acquire(single_instance::INSTANCE_ID, handler) {
        Ok(single_instance::Acquire::Primary(primary)) => Some(primary),
        Ok(single_instance::Acquire::Forwarded) => std::process::exit(0),
        Err(error) => {
            eprintln!("Cropmark: 单实例检测不可用,按普通启动继续:{error}");
            None
        }
    };

    let context = tauri::generate_context!();
    #[cfg(dev)]
    let context = {
        let mut context = context;
        apply_dev_host_csp(&mut context);
        context
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_log::Builder::new().skip_logger().build())
        .setup(move |app| {
            logging::install(app.handle());
            let _ = settings_slot.set(app.handle().clone());
            let missed = std::mem::take(
                &mut *pending_activations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            );
            if !missed.is_empty() {
                if let Err(error) = settings::open_settings(app.handle()) {
                    eprintln!("Cropmark: 无法打开设置窗口:{error}");
                }
            }

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let stored = settings::load_from_app(app.handle());
            let hotkeys = stored.hotkeys.clone();
            app.manage(settings::SessionState::from_stored(stored));
            // R12:托盘安装前先解析界面语言,保证首帧托盘菜单即为当前语言。
            settings::apply_language(app.handle());
            app.manage(capture::session::CaptureRuntime::default());
            app.manage(ocr::OcrRuntime::default());
            // R16:托盘构建失败(典型为缺少 AppIndicator 的 Linux 桌面)不再
            // 中止启动:`install_guarded` 把构建 Err 与构建期 panic(锁定依赖
            // 在 AppIndicator dlopen 失败时直接 panic)统一记为不可用,继续注册
            // 热键,随后打开设置窗口展示无托盘提示与退出入口;构建成功时行为与
            // 以往一致。
            let tray_ready = match tray::install_guarded(app.handle()) {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("Cropmark: 托盘不可用,继续以无托盘方式运行:{error}");
                    settings::mark_tray_unavailable(app.handle(), tray::unavailable_message());
                    false
                }
            };
            hotkeys::apply_to_app(app.handle(), &hotkeys);
            capture::precreate_windows(app.handle());
            // R2:仅当贴图增强与「重启后恢复」同时开启时恢复上次会话仍存在的
            // 贴图,并按当前显示器可见区域钳制;已关闭的贴图不重现。
            pin::restore_persisted(app.handle());
            // R15:托盘(或降级)、热键与预建窗口就绪,启动路径到此结束;
            // 门控日志只读时钟,不引入启动期同步 IO。
            log::info!(
                "startup ready tray={} elapsed_ms={}",
                if tray_ready { "ready" } else { "unavailable" },
                startup_started.elapsed().as_millis()
            );
            log_startup_ready(startup_started, tray_ready);
            if !tray_ready {
                if let Err(error) = settings::open_settings(app.handle()) {
                    eprintln!("Cropmark: 无法打开设置窗口:{error}");
                }
            } else if settings::onboarding_pending(app.handle()) {
                // 仅托盘安装成功后自动打开一次性引导。打不开也不中止启动:
                // 热键与采集已经注册,设置里仍可手动重开同一内容。
                if let Err(error) = settings::open_guide_window(app.handle()) {
                    log::warn!("guide open failed kind=window");
                    eprintln!("Cropmark: 无法打开首次引导:{error}");
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            settings::get_ui_settings,
            settings::get_language,
            settings::set_language,
            settings::set_hotkey,
            settings::set_pin_clipboard_hotkey,
            settings::set_autostart_enabled,
            settings::set_annotation_defaults,
            settings::set_feature,
            settings::set_annotation_tool,
            annotate::stickers::get_sticker_catalog,
            annotate::stickers::get_sticker_image,
            settings::set_capture_settings,
            settings::set_history_settings,
            settings::set_export_appearance,
            capture::get_overlay_frame,
            capture::get_preview_frame,
            capture::confirm_region,
            capture::confirm_logical_region,
            capture::confirm_window,
            capture::finish_region_with,
            capture::prepare_workspace_save_dialog,
            capture::restore_workspace_after_save_dialog,
            capture::complete_workspace,
            capture::edit_workspace_further,
            capture::fallback_workspace_preview,
            capture::get_toast_message,
            capture::cancel_capture,
            capture::close_preview,
            capture::take_pending_preview_ocr,
            capture::close_capture_error,
            capture::get_delay_state,
            capture::get_capture_error,
            capture::scroll::finish_scroll_capture,
            capture::scroll::cancel_scroll_capture,
            capture::scroll::get_scroll_status,
            export::copy_preview_png,
            export::save_preview_png,
            ocr::recognize_preview,
            ocr::copy_ocr_point,
            ocr::copy_ocr_rect,
            ocr::copy_ocr_all,
            ocr::copy_ocr_fragment,
            ocr::search_ocr_panel,
            pin::pin_current,
            pin::get_pin_image,
            pin::get_pin_state,
            pin::get_pin_options,
            pin::rotate_pin,
            pin::flip_pin,
            pin::set_pin_opacity,
            pin::set_pin_click_through,
            pin::exit_pin_click_through,
            pin::group_all_pins,
            pin::ungroup_pin,
            pin::move_pin,
            pin::zoom_pin,
            pin::reset_pin_zoom,
            pin::copy_pin,
            pin::save_pin,
            pin::begin_pin_edit,
            pin::update_pin_from_preview,
            pin::get_pin_writeback,
            pin::close_pin,
            pin::close_all_pins,
            history::get_history,
            history::get_history_thumbnail,
            history::copy_history_entry,
            history::pin_history_entry,
            history::reedit_history_entry,
            history::delete_history_entry,
            history::set_history_favorite,
            history::set_history_note,
            history::clear_history,
            history::open_history,
            settings::open_guide,
            logging::log_directory,
            logging::open_log_directory,
            #[cfg(debug_assertions)]
            logging::debug_trigger_panic,
            quit_app,
        ])
        .on_window_event(|window, event| {
            // 贴图窗口销毁(手动关闭/显示器断开/退出)即释放标签、源图与存储记录。
            if matches!(event, tauri::WindowEvent::Destroyed) {
                pin::handle_destroyed(window.app_handle(), window.label());
                // 预览窗销毁时收尾可能存在的贴图再标注会话(恢复来源贴图置顶)。
                if window.label() == capture::ui::PREVIEW {
                    capture::close_preview(window.app_handle().clone());
                }
                // R1:长截图控制窗被外部关闭(Alt+F4 等)按取消处理,不产出。
                if window.label() == capture::scroll::WINDOW {
                    capture::scroll::handle_window_destroyed(window.app_handle());
                }
                // 引导中途关闭同样记为已看过,且不拦截销毁,避免挡住托盘与热键。
                if window.label() == front::GUIDE {
                    settings::complete_onboarding(window.app_handle());
                }
                front::demote_if_idle(window.app_handle());
            }
            // R2:DPI/缩放变化后立即把越界贴图拉回可见区域(显示器数量变化
            // 由 pin.rs 的后台监视器兜底)。
            if let tauri::WindowEvent::ScaleFactorChanged { .. } = event {
                if pin::is_pin_label(window.label()) {
                    pin::clamp_pin_label(window.app_handle(), window.label());
                }
            }
        })
        .build(context)
        .expect("Cropmark failed to start")
        .run(|app, event| match event {
            tauri::RunEvent::ExitRequested { api, code, .. } => {
                if should_prevent_exit(code) {
                    api.prevent_exit();
                }
            }
            // 托盘退出(code=Some(0))等真实退出路径:退出前统一收掉贴图。
            tauri::RunEvent::Exit => {
                capture::scroll::shutdown(app);
                pin::close_all(app);
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::{extend_dev_csp, should_prevent_exit};

    #[test]
    fn closing_last_settings_window_keeps_tray_alive() {
        assert!(should_prevent_exit(None));
    }

    #[test]
    fn tray_quit_still_exits_the_process() {
        assert!(!should_prevent_exit(Some(0)));
    }

    const PRODUCTION_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; connect-src 'self' ipc: http://ipc.localhost; font-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'";
    const DEV_CSP: &str = "default-src 'self'; script-src 'self' http://localhost:1420; style-src 'self' http://localhost:1420; img-src 'self' data: blob: http://localhost:1420; connect-src 'self' ipc: http://ipc.localhost http://localhost:1420 ws://localhost:1420 ws://localhost:1421; font-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'";

    const PRODUCTION_URLS: &[&str] = &["http://ipc.localhost"];
    const DEV_URLS: &[&str] = &[
        "http://ipc.localhost",
        "http://localhost:1420",
        "ws://localhost:1420",
        "ws://localhost:1421",
    ];

    fn policy_violations(policy: &str, allowed_urls: &[&str]) -> Vec<String> {
        const KEYWORDS: &[&str] = &["'self'", "'none'"];
        const SCHEMES: &[&str] = &["data:", "blob:", "ipc:"];
        let mut violations = Vec::new();
        for part in policy
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let mut tokens = part.split_whitespace();
            let Some(directive) = tokens.next() else {
                continue;
            };
            if directive.contains('*') {
                violations.push(format!("wildcard directive {directive}"));
            }
            for source in tokens {
                if source.contains('*') {
                    violations.push(format!("{directive} wildcard {source}"));
                    continue;
                }
                if source.contains("://") {
                    if !allowed_urls.contains(&source) {
                        violations.push(format!("{directive} remote {source}"));
                    }
                    continue;
                }
                if !KEYWORDS.contains(&source) && !SCHEMES.contains(&source) {
                    violations.push(format!("{directive} unexpected {source}"));
                }
            }
        }
        violations
    }

    fn security_string(config: &serde_json::Value, key: &str) -> String {
        config["app"]["security"][key]
            .as_str()
            .unwrap_or_else(|| panic!("app.security.{key} must be a string"))
            .to_string()
    }

    #[test]
    fn production_csp_has_no_wildcard_or_remote_source() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{error}"));
        let config: serde_json::Value = serde_json::from_str(&text).expect("parse tauri.conf.json");
        let csp = security_string(&config, "csp");
        assert_eq!(csp, PRODUCTION_CSP);
        assert!(
            policy_violations(&csp, PRODUCTION_URLS).is_empty(),
            "{:?}",
            policy_violations(&csp, PRODUCTION_URLS)
        );
        assert!(!csp.contains("localhost:1420"));
        assert!(!csp.contains("unsafe-inline"));
        assert!(!csp.contains("unsafe-eval"));
    }

    #[test]
    fn dev_csp_only_adds_local_dev_server_and_hmr() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{error}"));
        let config: serde_json::Value = serde_json::from_str(&text).expect("parse tauri.conf.json");
        let dev_csp = security_string(&config, "devCsp");
        assert_eq!(dev_csp, DEV_CSP);
        assert!(
            policy_violations(&dev_csp, DEV_URLS).is_empty(),
            "{:?}",
            policy_violations(&dev_csp, DEV_URLS)
        );
        assert!(dev_csp.contains("http://localhost:1420"));
        assert!(dev_csp.contains("ws://localhost:1421"));
        assert!(!dev_csp.contains("unsafe-inline"));
        assert!(!dev_csp.contains('*'));
    }

    #[test]
    fn dev_csp_appends_tauri_dev_host_without_wildcards() {
        let extended = extend_dev_csp(DEV_CSP, "10.1.2.3").expect("host");
        assert!(extended.contains("http://10.1.2.3:1420"));
        assert!(extended.contains("ws://10.1.2.3:1420"));
        assert!(extended.contains("ws://10.1.2.3:1421"));
        let mut allowed = DEV_URLS.to_vec();
        allowed.extend([
            "http://10.1.2.3:1420",
            "ws://10.1.2.3:1420",
            "ws://10.1.2.3:1421",
        ]);
        assert!(
            policy_violations(&extended, &allowed).is_empty(),
            "{:?}",
            policy_violations(&extended, &allowed)
        );
        assert_eq!(
            extend_dev_csp(DEV_CSP, "localhost").as_deref(),
            Some(DEV_CSP)
        );
        let ipv6 = extend_dev_csp(DEV_CSP, "::1").expect("ipv6");
        assert!(ipv6.contains("http://[::1]:1420"));
        assert!(ipv6.contains("ws://[::1]:1421"));
        assert!(!ipv6.contains('*'));
    }

    #[test]
    fn dev_csp_rejects_injected_host() {
        assert!(extend_dev_csp(DEV_CSP, "*").is_none());
        assert!(extend_dev_csp(DEV_CSP, "evil.example; script-src *").is_none());
        assert!(extend_dev_csp(DEV_CSP, "https://evil.example").is_none());
        assert!(extend_dev_csp(DEV_CSP, "bad host").is_none());
        assert!(extend_dev_csp(DEV_CSP, "").is_none());
        assert!(extend_dev_csp(DEV_CSP, ".").is_none());
    }

    #[test]
    fn frontend_has_no_inline_script_or_style_attribute() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src");
        let mut files = Vec::new();
        collect_frontend_sources(&root, &mut files);
        let mut violations = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{error}"));
            let display = path.display().to_string();
            let lower = text.to_ascii_lowercase();
            if lower.contains("<style") {
                violations.push(format!("{display} has <style"));
            }
            if text.contains("style=\"") || text.contains("style='") {
                violations.push(format!("{display} has a style attribute"));
            }
            if text.contains("setAttribute(\"style\"") || text.contains("setAttribute('style'") {
                violations.push(format!("{display} sets a style attribute"));
            }
            if script_tag_lacks_src(&lower) {
                violations.push(format!("{display} has an inline script"));
            }
        }
        assert!(violations.is_empty(), "{violations:?}");
    }

    fn collect_frontend_sources(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir).unwrap_or_else(|error| panic!("{error}"));
        for entry in entries {
            let path = entry.unwrap_or_else(|error| panic!("{error}")).path();
            if path.is_dir() {
                collect_frontend_sources(&path, files);
                continue;
            }
            let ext = path.extension().and_then(|value| value.to_str());
            if matches!(ext, Some("html" | "ts" | "js")) {
                files.push(path);
            }
        }
    }

    fn script_tag_lacks_src(lower: &str) -> bool {
        let mut rest = lower;
        while let Some(index) = rest.find("<script") {
            let after = &rest[index..];
            let Some(end) = after.find('>') else {
                return true;
            };
            if !after[..=end].contains("src=") {
                return true;
            }
            rest = &after[end + 1..];
        }
        false
    }
}
