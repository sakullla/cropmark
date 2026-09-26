//! 结构化日志与崩溃记录（R16）。
//!
//! 运行日志交给 `tauri-plugin-log`：调试构建同时写标准输出，级别为 Debug，
//! 发布构建只写文件且级别为 Info。文件在平台日志目录，超过 [`MAX_LOG_BYTES`]
//! 后轮转并只保留上一份归档。目录不可写时不挂文件目标，启动与采集继续。
//!
//! 崩溃记录不走日志门面（panic 时门面锁可能已被占用），同步追加
//! `<log_dir>/crash.log`，超限则截断后再写。内容只有时间、级别、位置和消息。

use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Manager};
use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};
use tauri_plugin_opener::OpenerExt;

const APP_IDENTIFIER: &str = "app.cropmark.desktop";
const LOG_FILE_STEM: &str = "cropmark";
const LOG_FILE_NAME: &str = "cropmark.log";
const CRASH_FILE_NAME: &str = "crash.log";

/// 运行日志单文件上限。`KeepSome(1)` 另留一份归档，总量约两倍。
const MAX_LOG_BYTES: u64 = 1_048_576;
/// 崩溃记录上限。超限时丢掉旧内容，只保留还能写下的最新一条。
const MAX_CRASH_BYTES: u64 = 256 * 1024;
const MAX_PANIC_MESSAGE: usize = 512;

static CRASH_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static PANIC_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// 进程入口尽早装上 panic hook，并尽量绑到与 Tauri 相同的日志目录。
pub fn prepare() {
    if let Some(dir) = probed_log_dir() {
        if fs::create_dir_all(&dir).is_ok() && dir_is_writable(&dir) {
            set_crash_dir(Some(dir));
        }
    }
    install_panic_hook();
}

/// 在 Tauri setup 里挂上文件日志。失败只丢失日志，不让启动失败。
pub fn install(app: &AppHandle) {
    let writable = match app.path().app_log_dir() {
        Ok(dir) if fs::create_dir_all(&dir).is_ok() && dir_is_writable(&dir) => {
            set_crash_dir(Some(dir));
            true
        }
        Ok(_) => {
            // 平台目录已知但不可写：不要再往探测路径写，避免和界面显示的位置不一致。
            set_crash_dir(None);
            false
        }
        Err(_) => false,
    };
    if !attach_logger(app, writable) && writable {
        let _ = attach_logger(app, false);
    }
    log::info!("logger file={}", if writable { "on" } else { "off" });
}

#[tauri::command]
pub fn log_directory(app: AppHandle) -> String {
    app.path()
        .app_log_dir()
        .map(|dir| dir.join(LOG_FILE_NAME).to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[tauri::command]
pub fn open_log_directory(app: AppHandle) -> Result<(), String> {
    let dir = app
        .path()
        .app_log_dir()
        .map_err(|_| "unavailable".to_string())?;
    let _ = fs::create_dir_all(&dir);
    app.opener()
        .open_path(dir.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|_| "unavailable".to_string())
}

/// 调试构建里手动触发一次未捕获 panic，用来核对崩溃记录。发布构建不包含该命令。
#[cfg(debug_assertions)]
#[tauri::command]
pub fn debug_trigger_panic() {
    panic!("cropmark debug panic probe");
}

fn attach_logger(app: &AppHandle, file: bool) -> bool {
    let level = if cfg!(debug_assertions) {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    let mut builder = tauri_plugin_log::Builder::new()
        .clear_targets()
        .level(level)
        .max_file_size(u128::from(MAX_LOG_BYTES))
        .rotation_strategy(RotationStrategy::KeepSome(1))
        .timezone_strategy(TimezoneStrategy::UseLocal);
    if cfg!(debug_assertions) {
        builder = builder.target(Target::new(TargetKind::Stdout));
    }
    if file {
        builder = builder.target(Target::new(TargetKind::LogDir {
            file_name: Some(LOG_FILE_STEM.to_string()),
        }));
    }
    match builder.split(app) {
        Ok((_, max_level, logger)) => tauri_plugin_log::attach_logger(max_level, logger).is_ok(),
        Err(_) => false,
    }
}

fn set_crash_dir(dir: Option<PathBuf>) {
    *CRASH_DIR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = dir;
}

fn crash_dir() -> Option<PathBuf> {
    CRASH_DIR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn install_panic_hook() {
    if PANIC_HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_panic(info.location(), info.payload());
        previous(info);
    }));
}

fn record_panic(location: Option<&std::panic::Location<'_>>, payload: &(dyn std::any::Any + Send)) {
    let Some(dir) = crash_dir() else {
        return;
    };
    let place = location
        .map(|location| format!("{}:{}", location.file(), location.line()))
        .unwrap_or_else(|| "unknown".to_string());
    let line = format_crash_line(&local_timestamp(), &place, &payload_text(payload));
    let _ = append_crash_record(&dir, &line, MAX_CRASH_BYTES);
}

fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panic".to_string()
    }
}

fn format_crash_line(timestamp: &str, location: &str, message: &str) -> String {
    let message = flatten_message(message);
    format!("{timestamp} [ERROR] panic at {location}: {message}")
}

fn flatten_message(message: &str) -> String {
    message
        .chars()
        .filter(|ch| *ch != '\n' && *ch != '\r')
        .take(MAX_PANIC_MESSAGE)
        .collect()
}

/// 追加一条崩溃记录。超限时先把文件截成空，再写最新一条；单条仍超限则截断正文。
fn append_crash_record(dir: &Path, record: &str, max_bytes: u64) -> std::io::Result<()> {
    if max_bytes == 0 {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let line = bounded_line(record, max_bytes);
    let path = dir.join(CRASH_FILE_NAME);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    let len = file.metadata()?.len();
    let add = line.len() as u64;
    if len > 0 && len.saturating_add(add) > max_bytes {
        file.set_len(0)?;
    }
    file.seek(SeekFrom::End(0))?;
    file.write_all(line.as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn bounded_line(record: &str, max_bytes: u64) -> String {
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let body_max = max.saturating_sub(1);
    let mut text = flatten_message(record);
    if text.len() > body_max {
        let mut end = body_max;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text.push('\n');
    text
}

fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(".cropmark-write-probe");
    match fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 与 `tauri::path::PathResolver::app_log_dir` 相同的目录，供 setup 之前的 panic 使用。
fn probed_log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Logs")
                .join(APP_IDENTIFIER)
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        local_data_dir().map(|dir| dir.join(APP_IDENTIFIER).join("logs"))
    }
}

#[cfg(windows)]
fn local_data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|profile| PathBuf::from(profile).join("AppData").join("Local"))
        })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn local_data_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
}

#[cfg(windows)]
fn local_timestamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let now = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    )
}

#[cfg(not(windows))]
fn local_timestamp() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut broken = std::mem::zeroed();
        if libc::localtime_r(&now, &mut broken).is_null() {
            return "0000-00-00 00:00:00".to_string();
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            broken.tm_year + 1900,
            broken.tm_mon + 1,
            broken.tm_mday,
            broken.tm_hour,
            broken.tm_min,
            broken.tm_sec
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_case(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "cropmark-log-{}-{}-{name}",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn crash_record_writes_time_level_and_location() {
        let dir = temp_case("write");
        let line = format_crash_line(
            "2026-01-02 03:04:05",
            "src/logging.rs:42",
            "cropmark debug panic probe",
        );
        append_crash_record(&dir, &line, 4096).unwrap();
        let text = fs::read_to_string(dir.join(CRASH_FILE_NAME)).unwrap();
        assert!(text.contains("2026-01-02 03:04:05"));
        assert!(text.contains("[ERROR]"));
        assert!(text.contains("src/logging.rs:42"));
        assert!(text.contains("cropmark debug panic probe"));
        assert!(!text.contains("iVBORw0KGgo"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_record_truncates_when_the_next_line_would_exceed_the_limit() {
        let dir = temp_case("truncate");
        let max = 80u64;
        append_crash_record(&dir, &"A".repeat(50), max).unwrap();
        append_crash_record(&dir, &"B".repeat(50), max).unwrap();
        let bytes = fs::read(dir.join(CRASH_FILE_NAME)).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(bytes.len() as u64 <= max);
        assert!(text.contains('B'));
        assert!(!text.contains('A'));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_record_truncates_a_single_oversized_line_on_a_char_boundary() {
        let dir = temp_case("utf8");
        let record = format!("HEAD{}", "测".repeat(40));
        append_crash_record(&dir, &record, 16).unwrap();
        let text = fs::read_to_string(dir.join(CRASH_FILE_NAME)).unwrap();
        assert!(text.len() <= 16);
        assert!(text.ends_with('\n'));
        assert!(text.chars().filter(|ch| *ch == '测').count() < 40);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_record_unwritable_directory_returns_error_without_panicking() {
        let blocker = temp_case("blocker");
        fs::write(&blocker, b"x").unwrap();
        let error = append_crash_record(&blocker, "panic at src/logging.rs:1: probe", 128);
        assert!(error.is_err());
        let message: &str = "ignored";
        record_panic(None, &message);
        let _ = fs::remove_file(&blocker);
    }

    #[test]
    fn panic_message_flattens_newlines_and_caps_length() {
        let message = flatten_message(&format!("alpha\nbeta\r{}", "x".repeat(MAX_PANIC_MESSAGE)));
        assert!(!message.contains('\n'));
        assert!(!message.contains('\r'));
        assert!(message.starts_with("alphabeta"));
        assert!(message.len() <= MAX_PANIC_MESSAGE);
    }
}
