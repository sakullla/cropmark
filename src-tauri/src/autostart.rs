use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::i18n;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformStatus {
    Enabled,
    NotRegistered,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    RequiresApproval,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    NotFound,
    Denied(String),
}

/// 拒绝原因:保存原因而不是成品文案,语言切换后消息按新语言重解析。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartRejection {
    RequiresApproval,
    NotFound,
    Denied(String),
    Rejected,
}

impl AutostartRejection {
    pub fn message(&self) -> String {
        match self {
            Self::RequiresApproval => i18n::t("autostart.requires_approval"),
            Self::NotFound => i18n::t("autostart.not_found"),
            Self::Denied(reason) => i18n::tp("autostart.denied", &[("reason", reason)]),
            Self::Rejected => i18n::t("autostart.rejected"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutostartState {
    pub enabled: bool,
    pub message: Option<String>,
    /// 拒绝原因(不序列化):设置页据此在语言切换后重解析消息。
    #[serde(skip)]
    pub rejection: Option<AutostartRejection>,
}

pub fn current_state() -> AutostartState {
    map_platform_status(platform_status())
}

pub fn set_enabled(enabled: bool) -> AutostartState {
    let state = if let Err(status) = apply_enabled(enabled) {
        map_platform_status(status)
    } else {
        let state = map_platform_status(platform_status());
        if enabled && !state.enabled && state.message.is_none() {
            let rejection = AutostartRejection::Rejected;
            AutostartState {
                enabled: false,
                message: Some(rejection.message()),
                rejection: Some(rejection),
            }
        } else {
            state
        }
    };
    log::info!(
        "autostart enabled={} kind={}",
        state.enabled,
        autostart_kind(&state)
    );
    state
}

fn autostart_kind(state: &AutostartState) -> &'static str {
    match &state.rejection {
        Some(AutostartRejection::RequiresApproval) => "approval",
        Some(AutostartRejection::NotFound) => "not_found",
        Some(AutostartRejection::Denied(_)) => "denied",
        Some(AutostartRejection::Rejected) => "rejected",
        None if state.enabled => "enabled",
        None => "off",
    }
}

pub fn merge_autostart_ui(
    live: AutostartState,
    last_rejection: Option<AutostartRejection>,
) -> AutostartState {
    if live.enabled {
        return AutostartState {
            enabled: true,
            message: None,
            rejection: None,
        };
    }
    if live.message.is_some() {
        return live;
    }
    AutostartState {
        enabled: false,
        message: last_rejection.as_ref().map(AutostartRejection::message),
        rejection: last_rejection,
    }
}

pub fn remember_autostart_result(result: &AutostartState) -> Option<AutostartRejection> {
    if result.enabled {
        None
    } else {
        result.rejection.clone()
    }
}

pub fn map_platform_status(status: PlatformStatus) -> AutostartState {
    let rejection = rejection_for(&status);
    let message = rejection.as_ref().map(AutostartRejection::message);
    match status {
        PlatformStatus::Enabled => AutostartState {
            enabled: true,
            message: None,
            rejection: None,
        },
        PlatformStatus::NotRegistered => AutostartState {
            enabled: false,
            message: None,
            rejection: None,
        },
        PlatformStatus::RequiresApproval | PlatformStatus::NotFound | PlatformStatus::Denied(_) => {
            AutostartState {
                enabled: false,
                message,
                rejection,
            }
        }
    }
}

fn rejection_for(status: &PlatformStatus) -> Option<AutostartRejection> {
    match status {
        PlatformStatus::RequiresApproval => Some(AutostartRejection::RequiresApproval),
        PlatformStatus::NotFound => Some(AutostartRejection::NotFound),
        PlatformStatus::Denied(reason) => Some(AutostartRejection::Denied(reason.clone())),
        PlatformStatus::Enabled | PlatformStatus::NotRegistered => None,
    }
}

fn platform_status() -> PlatformStatus {
    match current_exe() {
        Ok(exe) => backend_status(&exe),
        Err(message) => PlatformStatus::Denied(message),
    }
}

fn apply_enabled(enabled: bool) -> Result<(), PlatformStatus> {
    let exe = current_exe().map_err(PlatformStatus::Denied)?;
    backend_set(&exe, enabled)
}

fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| error.to_string())
}

#[cfg(windows)]
fn backend_status(exe: &Path) -> PlatformStatus {
    windows::status(exe)
}

#[cfg(windows)]
fn backend_set(exe: &Path, enabled: bool) -> Result<(), PlatformStatus> {
    windows::set(exe, enabled)
}

#[cfg(target_os = "macos")]
fn backend_status(_exe: &Path) -> PlatformStatus {
    macos::status()
}

#[cfg(target_os = "macos")]
fn backend_set(_exe: &Path, enabled: bool) -> Result<(), PlatformStatus> {
    macos::set(enabled)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn backend_status(exe: &Path) -> PlatformStatus {
    linux::status(exe)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn backend_set(exe: &Path, enabled: bool) -> Result<(), PlatformStatus> {
    linux::set(exe, enabled)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    pub const VALUE_NAME: &str = "Cropmark";

    pub fn run_command_for_exe(exe: &Path) -> String {
        format!("\"{}\"", exe.display())
    }

    pub fn run_command_matches(stored: &str, exe: &Path) -> bool {
        normalize_windows_command(stored) == normalize_windows_command(&run_command_for_exe(exe))
            || normalize_windows_command(stored) == normalize_windows_path(exe)
    }

    pub fn status(exe: &Path) -> PlatformStatus {
        match read_run_value() {
            Ok(Some(value)) if run_command_matches(&value, exe) => PlatformStatus::Enabled,
            Ok(_) => PlatformStatus::NotRegistered,
            Err(message) => PlatformStatus::Denied(message),
        }
    }

    pub fn set(exe: &Path, enabled: bool) -> Result<(), PlatformStatus> {
        if enabled {
            write_run_value(&run_command_for_exe(exe)).map_err(PlatformStatus::Denied)
        } else {
            delete_run_value().map_err(PlatformStatus::Denied)
        }
    }

    fn run_key(write: bool) -> Result<RegKey, String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if write {
            hkcu.create_subkey(RUN_SUBKEY)
                .map(|(key, _)| key)
                .map_err(|error| error.to_string())
        } else {
            hkcu.open_subkey(RUN_SUBKEY)
                .map_err(|error| error.to_string())
        }
    }

    fn read_run_value() -> Result<Option<String>, String> {
        let key = match run_key(false) {
            Ok(key) => key,
            Err(_) => return Ok(None),
        };
        match key.get_value::<String, _>(VALUE_NAME) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn write_run_value(command: &str) -> Result<(), String> {
        let key = run_key(true)?;
        key.set_value(VALUE_NAME, &command.to_string())
            .map_err(|error| error.to_string())
    }

    fn delete_run_value() -> Result<(), String> {
        let key = match run_key(true) {
            Ok(key) => key,
            Err(_) => return Ok(()),
        };
        match key.delete_value(VALUE_NAME) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn normalize_windows_command(value: &str) -> String {
        normalize_windows_path_str(value.trim().trim_matches('"'))
    }

    fn normalize_windows_path(path: &Path) -> String {
        normalize_windows_path_str(&path.display().to_string())
    }

    fn normalize_windows_path_str(value: &str) -> String {
        value.replace('/', "\\").to_ascii_lowercase()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::PlatformStatus;
    use smappservice_rs::{AppService, ServiceManagementError, ServiceStatus, ServiceType};

    fn service() -> AppService {
        AppService::new(ServiceType::MainApp)
    }

    pub fn status() -> PlatformStatus {
        map_service_status(service().status())
    }

    pub fn set(enabled: bool) -> Result<(), PlatformStatus> {
        let service = service();
        let result = if enabled {
            service.register()
        } else {
            service.unregister()
        };
        match result {
            Ok(()) => Ok(()),
            Err(ServiceManagementError::AlreadyRegistered) if enabled => Ok(()),
            Err(ServiceManagementError::JobNotFound) if !enabled => Ok(()),
            Err(ServiceManagementError::LaunchDeniedByUser) => {
                Err(PlatformStatus::RequiresApproval)
            }
            Err(error) => Err(PlatformStatus::Denied(error.to_string())),
        }
    }

    pub fn map_service_status(status: ServiceStatus) -> PlatformStatus {
        match status {
            ServiceStatus::Enabled => PlatformStatus::Enabled,
            ServiceStatus::NotRegistered => PlatformStatus::NotRegistered,
            ServiceStatus::RequiresApproval => PlatformStatus::RequiresApproval,
            ServiceStatus::NotFound => PlatformStatus::NotFound,
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use super::*;
    use std::fs;

    pub const DESKTOP_FILE_NAME: &str = "cropmark.desktop";

    pub fn desktop_entry(exe: &Path) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Version=1.0\n\
             Name=Cropmark\n\
             Comment=Cropmark\n\
             Exec=\"{}\"\n\
             Icon=cropmark\n\
             Terminal=false\n\
             Categories=Graphics;Utility;\n\
             X-GNOME-Autostart-enabled=true\n\
             Hidden=false\n",
            exe.display()
        )
    }

    pub fn desktop_path() -> Result<PathBuf, String> {
        let config = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                PathBuf::from(xdg)
            } else {
                home_config()?
            }
        } else {
            home_config()?
        };
        Ok(config.join("autostart").join(DESKTOP_FILE_NAME))
    }

    fn home_config() -> Result<PathBuf, String> {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home).join(".config"))
    }

    pub fn status(exe: &Path) -> PlatformStatus {
        let path = match desktop_path() {
            Ok(path) => path,
            Err(message) => return PlatformStatus::Denied(message),
        };
        match fs::read_to_string(&path) {
            Ok(body) if desktop_enabled(&body, exe) => PlatformStatus::Enabled,
            Ok(_) | Err(_) if !path.exists() => PlatformStatus::NotRegistered,
            Ok(_) => PlatformStatus::NotRegistered,
            Err(error) => PlatformStatus::Denied(error.to_string()),
        }
    }

    pub fn set(exe: &Path, enabled: bool) -> Result<(), PlatformStatus> {
        let path = desktop_path().map_err(PlatformStatus::Denied)?;
        if enabled {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)
                    .map_err(|error| PlatformStatus::Denied(error.to_string()))?;
            }
            fs::write(&path, desktop_entry(exe))
                .map_err(|error| PlatformStatus::Denied(error.to_string()))
        } else {
            match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(PlatformStatus::Denied(error.to_string())),
            }
        }
    }

    pub fn desktop_enabled(body: &str, exe: &Path) -> bool {
        let hidden = body.lines().any(|line| {
            let line = line.trim();
            line.eq_ignore_ascii_case("Hidden=true")
                || line.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
        });
        if hidden {
            return false;
        }
        body.lines().any(|line| {
            line.trim()
                .strip_prefix("Exec=")
                .map(|value| {
                    let value = value.trim().trim_matches('"');
                    Path::new(value) == exe
                })
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
pub fn linux_desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=Cropmark\n\
         Comment=Cropmark\n\
         Exec=\"{}\"\n\
         Icon=cropmark\n\
         Terminal=false\n\
         Categories=Graphics;Utility;\n\
         X-GNOME-Autostart-enabled=true\n\
         Hidden=false\n",
        exe.display()
    )
}

#[cfg(test)]
pub fn linux_desktop_enabled(body: &str, exe: &Path) -> bool {
    let hidden = body.lines().any(|line| {
        let line = line.trim();
        line.eq_ignore_ascii_case("Hidden=true")
            || line.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
    });
    if hidden {
        return false;
    }
    body.lines().any(|line| {
        line.trim()
            .strip_prefix("Exec=")
            .map(|value| {
                let value = value.trim().trim_matches('"');
                Path::new(value) == exe
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
pub fn run_command_for_exe(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

#[cfg(test)]
pub fn run_command_matches(stored: &str, exe: &Path) -> bool {
    normalize_command(stored) == normalize_command(&run_command_for_exe(exe))
        || normalize_command(stored) == normalize_path(exe)
}

#[cfg(test)]
fn normalize_command(value: &str) -> String {
    normalize_path_str(value.trim().trim_matches('"'))
}

#[cfg(test)]
fn normalize_path(path: &Path) -> String {
    normalize_path_str(&path.display().to_string())
}

#[cfg(test)]
fn normalize_path_str(value: &str) -> String {
    value.replace('/', "\\").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_maps_on_without_message() {
        let state = map_platform_status(PlatformStatus::Enabled);
        assert!(state.enabled);
        assert_eq!(state.message, None);
    }

    #[test]
    fn not_registered_maps_off_without_message() {
        let state = map_platform_status(PlatformStatus::NotRegistered);
        assert!(!state.enabled);
        assert_eq!(state.message, None);
    }

    #[test]
    fn requires_approval_maps_off_with_product_message() {
        let state = map_platform_status(PlatformStatus::RequiresApproval);
        assert!(!state.enabled);
        let message = state.message.expect("approval needs an explanation");
        assert!(message.contains("批准"));
        assert!(message.contains("关闭"));
    }

    #[test]
    fn not_found_and_denied_map_off_with_message() {
        let missing = map_platform_status(PlatformStatus::NotFound);
        assert!(!missing.enabled);
        assert!(missing.message.unwrap().contains("关闭"));

        let denied = map_platform_status(PlatformStatus::Denied("access denied".into()));
        assert!(!denied.enabled);
        assert!(denied.message.unwrap().contains("拒绝"));
    }

    #[test]
    fn windows_run_command_matches_quoted_and_plain_paths() {
        let exe = Path::new(r"C:\Program Files\Cropmark\cropmark.exe");
        assert!(run_command_matches(
            r#""C:\Program Files\Cropmark\cropmark.exe""#,
            exe
        ));
        assert!(run_command_matches(
            r"C:\Program Files\Cropmark\cropmark.exe",
            exe
        ));
        assert!(!run_command_matches(r#""C:\Other\cropmark.exe""#, exe));
        assert!(run_command_for_exe(exe).starts_with('"'));
        assert_eq!(windows_run_value_name(), "Cropmark");
    }

    #[test]
    fn linux_desktop_is_cropmark_and_respects_hidden() {
        let exe = Path::new("/opt/Cropmark/cropmark");
        let body = linux_desktop_entry(exe);
        assert!(body.contains("Name=Cropmark"));
        assert!(body.contains("Exec=\"/opt/Cropmark/cropmark\""));
        assert!(!body.to_ascii_lowercase().contains("cleanshot"));
        assert!(!body.to_ascii_lowercase().contains("flameshot"));
        assert!(linux_desktop_enabled(&body, exe));
        assert!(!linux_desktop_enabled(
            "Hidden=true\nExec=/opt/Cropmark/cropmark\n",
            exe
        ));
        assert!(!linux_desktop_enabled(&body, Path::new("/tmp/other")));
    }

    fn windows_run_value_name() -> &'static str {
        "Cropmark"
    }

    #[test]
    fn denied_write_message_survives_live_not_registered_query() {
        let result = map_platform_status(PlatformStatus::Denied("access denied".into()));
        let stored = remember_autostart_result(&result);
        let live = map_platform_status(PlatformStatus::NotRegistered);
        let ui = merge_autostart_ui(live, stored);
        assert!(!ui.enabled);
        assert!(ui.message.as_deref().unwrap().contains("拒绝"));
    }

    #[test]
    fn successful_toggle_clears_stored_rejection() {
        let enabled = map_platform_status(PlatformStatus::Enabled);
        assert_eq!(remember_autostart_result(&enabled), None);
        let ui = merge_autostart_ui(enabled, Some(AutostartRejection::Rejected));
        assert!(ui.enabled);
        assert_eq!(ui.message, None);

        let off = map_platform_status(PlatformStatus::NotRegistered);
        assert_eq!(remember_autostart_result(&off), None);
        let ui = merge_autostart_ui(off, None);
        assert!(!ui.enabled);
        assert_eq!(ui.message, None);
    }
}
