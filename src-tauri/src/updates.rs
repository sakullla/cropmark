//! 检查更新:查询 GitHub Releases 最新稳定版,与当前版本做语义化比对。
//! 确认前不下载。可识别的 Windows NSIS、位于 .app 内的 macOS、以及带
//! APPIMAGE 的 Linux,确认后用已配置的 updater 公钥验签安装;其余形态
//! 只打开发布页。采集进行中不下载、不安装、不退出。
//!
//! - 托盘「检查更新」在后台线程查询,结果回主线程用原生对话框反馈;
//! - 设置页「关于」里的检查走 `check_for_updates` command,结果内联展示;
//! - 启动静默检查由「启动时检查更新」开关(默认开)门控,失败只记日志;
//! - 发现新版本时把版本与下载页记入 `UpdateState` 并重建托盘菜单。
//!   托盘下载项与关于页按钮走同一次确认,不无条件打开发布页。

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::i18n;
use crate::settings;

const API_URL: &str = "https://api.github.com/repos/sakullla/cropmark/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/sakullla/cropmark/releases/latest";
/// 响应里的发布页必须仍指向本仓库:防止被篡改的响应把浏览器带去任意地址。
const RELEASE_URL_PREFIX: &str = "https://github.com/sakullla/cropmark/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 正常 latest 响应只有几十 KB;异常超大响应按读满上限截断后解析失败处理。
const RESPONSE_LIMIT_BYTES: u64 = 512 * 1024;

/// 已发现的新版本(版本号 + 发布页);随会话存续,不持久化——重启后重新检查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownUpdate {
    pub version: String,
    pub url: String,
}

#[derive(Debug, Default)]
pub struct UpdateState(pub Mutex<Option<KnownUpdate>>);

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// GitHub `/releases/latest` 响应里用到的字段。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub html_url: String,
}

/// 发布 tag → 语义化版本:去掉单个 v/V 前缀;无法解析返回 None。
pub fn parse_release_tag(tag: &str) -> Option<semver::Version> {
    let trimmed = tag.trim();
    let stripped = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    semver::Version::parse(stripped).ok()
}

pub fn is_release_url(url: &str) -> bool {
    url.starts_with(RELEASE_URL_PREFIX)
}

/// ureq 的 TLS provider 默认是 Rustls,与启用的 cargo feature 无关,未选用时
/// 首个 https 请求会直接 panic,必须显式选 NativeTls;根证书走系统信任库。
fn http_agent() -> ureq::Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .tls_config(tls)
        .build()
        .new_agent()
}

/// 查询 GitHub 最新发布。GitHub API 要求 User-Agent;无 token,走
/// 匿名限额(按 IP 60 次/小时),启动 + 手动检查的量级远够。
pub fn fetch_latest_release() -> Result<ReleaseInfo, String> {
    let mut response = http_agent()
        .get(API_URL)
        .header(
            "User-Agent",
            concat!("Cropmark/", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|error| format!("GitHub API: {error}"))?;
    let text = response
        .body_mut()
        .with_config()
        .limit(RESPONSE_LIMIT_BYTES)
        .read_to_string()
        .map_err(|error| format!("GitHub API body: {error}"))?;
    let release: ReleaseInfo =
        serde_json::from_str(&text).map_err(|error| format!("GitHub API JSON: {error}"))?;
    if !is_release_url(&release.html_url) {
        return Err(format!("unexpected release url: {}", release.html_url));
    }
    Ok(release)
}

/// 检查结果:前端按 `status` 判别并内联展示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum UpdateStatus {
    UpToDate {
        current: String,
    },
    Available {
        current: String,
        latest: String,
        url: String,
    },
}

/// 比对最新发布与当前版本。tag 无法解析按失败处理(不提示、不误报)。
/// 稳定版不把预发布 tag 当作可安装更新;预发布安装仍可升到更高的正式版。
pub fn evaluate_release(release: ReleaseInfo) -> Result<UpdateStatus, String> {
    let latest = parse_release_tag(&release.tag_name)
        .ok_or_else(|| format!("unrecognized tag: {}", release.tag_name))?;
    let current = semver::Version::parse(current_version()).expect("app version is semver");
    if release_offered(&current, &latest) {
        Ok(UpdateStatus::Available {
            current: current.to_string(),
            latest: latest.to_string(),
            url: release.html_url,
        })
    } else {
        Ok(UpdateStatus::UpToDate {
            current: current.to_string(),
        })
    }
}

/// 稳定版安装不接收预发布标记;其余仍按语义化版本「更高才提供」。
fn release_offered(current: &semver::Version, latest: &semver::Version) -> bool {
    if !latest.pre.is_empty() && current.pre.is_empty() {
        return false;
    }
    latest > current
}

/// 运行中的安装形态。只由操作系统、是否调试构建、可执行文件路径和
/// `APPIMAGE` 决定,不新增用户设置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallForm {
    WindowsNsis,
    MacOsApp,
    LinuxAppImage,
    Unrecognized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostOs {
    Windows,
    MacOs,
    Linux,
    Other,
}

/// 用户确认之后的动作。拒绝确认时是 `None`:不安装、不打开发布页、不退出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmedUpdateAction {
    Install,
    OpenReleasePage,
}

/// 采集占用。三项都只读;任一为真就拒绝下载和安装。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UpdateBlockers {
    capture_session: bool,
    recording_active: bool,
    scroll_started: bool,
}

fn current_host_os() -> HostOs {
    if cfg!(windows) {
        HostOs::Windows
    } else if cfg!(target_os = "macos") {
        HostOs::MacOs
    } else if cfg!(target_os = "linux") {
        HostOs::Linux
    } else {
        HostOs::Other
    }
}

fn app_bundle_root(executable: &Path) -> Option<&Path> {
    executable.ancestors().skip(1).find(|ancestor| {
        ancestor.file_name().is_some_and(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        })
    })
}

fn classify_install_form(
    os: HostOs,
    debug_build: bool,
    executable: &Path,
    appimage: Option<&OsStr>,
) -> InstallForm {
    match os {
        HostOs::Windows if !debug_build => InstallForm::WindowsNsis,
        HostOs::MacOs if app_bundle_root(executable).is_some() => InstallForm::MacOsApp,
        HostOs::Linux if appimage.is_some_and(|value| !value.is_empty()) => {
            InstallForm::LinuxAppImage
        }
        _ => InstallForm::Unrecognized,
    }
}

fn current_install_form() -> InstallForm {
    let executable = std::env::current_exe().unwrap_or_default();
    classify_install_form(
        current_host_os(),
        cfg!(debug_assertions),
        &executable,
        std::env::var_os("APPIMAGE").as_deref(),
    )
}

fn confirm_message_key(form: InstallForm, os: HostOs) -> &'static str {
    match form {
        InstallForm::WindowsNsis => "updates.confirm_windows",
        InstallForm::MacOsApp | InstallForm::LinuxAppImage => "updates.confirm_install",
        InstallForm::Unrecognized if os == HostOs::Linux => "updates.open_linux",
        InstallForm::Unrecognized => "updates.open_prompt",
    }
}

fn action_after_confirm(form: InstallForm, accepted: bool) -> Option<ConfirmedUpdateAction> {
    if !accepted {
        return None;
    }
    match form {
        InstallForm::Unrecognized => Some(ConfirmedUpdateAction::OpenReleasePage),
        InstallForm::WindowsNsis | InstallForm::MacOsApp | InstallForm::LinuxAppImage => {
            Some(ConfirmedUpdateAction::Install)
        }
    }
}

fn update_install_blocked(blockers: UpdateBlockers) -> bool {
    blockers.capture_session || blockers.recording_active || blockers.scroll_started
}

fn current_update_blockers(app: &AppHandle) -> UpdateBlockers {
    UpdateBlockers {
        capture_session: crate::capture::session::capture_session_present(app),
        recording_active: crate::capture::session::recording_active(app),
        scroll_started: crate::capture::scroll::scroll_session_started(),
    }
}

pub fn known_update(app: &AppHandle) -> Option<KnownUpdate> {
    lock(&app.state::<UpdateState>().0).clone()
}

/// 检查结果写回会话内的已知更新。已是最新时清掉,关于页和托盘不再提供安装。
fn known_after_check(status: &UpdateStatus) -> Option<KnownUpdate> {
    match status {
        UpdateStatus::Available { latest, url, .. } => Some(KnownUpdate {
            version: latest.clone(),
            url: url.clone(),
        }),
        UpdateStatus::UpToDate { .. } => None,
    }
}

/// 关于页展示的可安装版本;没有已知更新时不出现安装入口。
fn about_install_version(known: Option<&KnownUpdate>) -> Option<&str> {
    known.map(|known| known.version.as_str())
}

#[derive(Clone, serde::Serialize)]
pub struct KnownUpdateNotice {
    version: Option<String>,
}

fn set_known_update(app: &AppHandle, update: Option<KnownUpdate>) {
    let state = app.state::<UpdateState>();
    let mut slot = lock(&state.0);
    if *slot == update {
        return;
    }
    *slot = update;
    drop(slot);
    let version = about_install_version(known_update(app).as_ref()).map(str::to_string);
    let _ = app.emit("update-known", KnownUpdateNotice { version });
    let task = app.clone();
    let _ = app.run_on_main_thread(move || crate::tray::refresh_menu(&task));
}

/// 设置页打开时读取启动检查已经记下的版本,确认前不下载。
#[tauri::command]
pub fn known_available_update(app: AppHandle) -> Option<KnownUpdateNotice> {
    let known = known_update(&app);
    let version = about_install_version(known.as_ref())?.to_string();
    Some(KnownUpdateNotice {
        version: Some(version),
    })
}

/// 设置页与托盘共用的检查 command:阻塞 HTTP 放到阻塞线程池,
/// 不占主线程与异步 worker;发现新版本时顺带更新托盘入口。
#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> Result<UpdateStatus, String> {
    let release = tauri::async_runtime::handle()
        .spawn_blocking(fetch_latest_release)
        .await
        .map_err(|error| error.to_string())??;
    let status = evaluate_release(release)?;
    set_known_update(&app, known_after_check(&status));
    Ok(status)
}

/// 打开发布页:有已知新版本时开对应发布页,否则开 releases/latest。
#[tauri::command]
pub fn open_release_page(app: AppHandle) -> Result<(), String> {
    let url = known_update(&app)
        .map(|known| known.url)
        .unwrap_or_else(|| RELEASES_PAGE.to_string());
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|error| error.to_string())
}

/// 启动静默检查:等启动路径结束再联网;失败只记日志,不打扰。
/// 发现新版本时记录并重建托盘菜单,不弹窗、不自动打开浏览器。
pub fn spawn_startup_check(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(4));
        if !settings::current_update(&app).check_on_startup {
            return;
        }
        match fetch_latest_release().and_then(evaluate_release) {
            Ok(status) => {
                match &status {
                    UpdateStatus::Available { latest, .. } => {
                        log::info!("update available latest={latest}")
                    }
                    UpdateStatus::UpToDate { .. } => log::info!("update check up to date"),
                }
                set_known_update(&app, known_after_check(&status));
            }
            Err(error) => log::warn!("update check failed: {error}"),
        }
    });
}

/// 托盘「检查更新」:后台线程查询,结果回主线程用原生对话框反馈。
/// macOS 上 rfd 对话框要求主线程,run_on_main_thread 里的模态属常规阻塞。
pub fn run_manual_check(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let outcome = fetch_latest_release().and_then(evaluate_release);
        let task = app.clone();
        let _ = app.run_on_main_thread(move || match outcome {
            Ok(status) => {
                set_known_update(&task, known_after_check(&status));
                match status {
                    UpdateStatus::Available { .. } => begin_update_action(task),
                    UpdateStatus::UpToDate { current } => {
                        let text = i18n::tp("updates.uptodate", &[("version", &current)]);
                        dialog(&text, rfd::MessageLevel::Info, rfd::MessageButtons::Ok);
                    }
                }
            }
            Err(error) => {
                let text = i18n::tp("updates.check_failed", &[("error", &error)]);
                dialog(&text, rfd::MessageLevel::Warning, rfd::MessageButtons::Ok);
            }
        });
    });
}

static UPDATE_ACTION_RUNNING: AtomicBool = AtomicBool::new(false);

/// 托盘下载项与关于页按钮的入口:确认对话框在主线程。
pub fn start_update_action(app: &AppHandle) {
    let app = app.clone();
    let _ = app
        .clone()
        .run_on_main_thread(move || begin_update_action(app));
}

/// 关于页「下载并安装」:与托盘下载项同一条确认路径。
#[tauri::command]
pub fn confirm_available_update(app: AppHandle) -> Result<(), String> {
    start_update_action(&app);
    Ok(())
}

fn begin_update_action(app: AppHandle) {
    if UPDATE_ACTION_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    run_update_action(app);
}

fn run_update_action(app: AppHandle) {
    let Some(known) = known_update(&app) else {
        UPDATE_ACTION_RUNNING.store(false, Ordering::SeqCst);
        return;
    };
    let form = current_install_form();
    let prompt = i18n::tp(
        confirm_message_key(form, current_host_os()),
        &[("version", &known.version)],
    );
    let accepted = dialog(&prompt, rfd::MessageLevel::Info, rfd::MessageButtons::YesNo)
        == rfd::MessageDialogResult::Yes;
    match action_after_confirm(form, accepted) {
        None => UPDATE_ACTION_RUNNING.store(false, Ordering::SeqCst),
        Some(ConfirmedUpdateAction::OpenReleasePage) => {
            if let Err(error) = open_release_page(app) {
                log::warn!("open release page failed: {error}");
            }
            UPDATE_ACTION_RUNNING.store(false, Ordering::SeqCst);
        }
        Some(ConfirmedUpdateAction::Install) => {
            let task = app.clone();
            tauri::async_runtime::spawn(async move {
                let notice = install_signed_update(&task).await.err();
                let text = notice.as_ref().map(install_notice_text);
                let level = notice.as_ref().map(install_notice_level);
                let posted = task.clone().run_on_main_thread(move || {
                    if let (Some(text), Some(level)) = (text, level) {
                        dialog(&text, level, rfd::MessageButtons::Ok);
                    }
                    UPDATE_ACTION_RUNNING.store(false, Ordering::SeqCst);
                });
                if posted.is_err() {
                    if let Some(text) = notice.as_ref().map(install_notice_text) {
                        log::warn!("update notice: {text}");
                    }
                    UPDATE_ACTION_RUNNING.store(false, Ordering::SeqCst);
                }
            });
        }
    }
}

enum InstallNotice {
    Busy,
    Unavailable,
    Failed(String),
    #[cfg(not(windows))]
    RelaunchFailed,
}

fn install_notice_text(notice: &InstallNotice) -> String {
    match notice {
        InstallNotice::Busy => i18n::t("updates.busy"),
        InstallNotice::Unavailable => i18n::t("updates.install_unavailable"),
        InstallNotice::Failed(error) => i18n::tp("updates.install_failed", &[("error", error)]),
        #[cfg(not(windows))]
        InstallNotice::RelaunchFailed => relaunch_failed_text(),
    }
}

fn install_notice_level(notice: &InstallNotice) -> rfd::MessageLevel {
    match notice {
        InstallNotice::Busy => rfd::MessageLevel::Info,
        InstallNotice::Unavailable | InstallNotice::Failed(_) => rfd::MessageLevel::Warning,
        #[cfg(not(windows))]
        InstallNotice::RelaunchFailed => rfd::MessageLevel::Warning,
    }
}

#[cfg(not(windows))]
fn relaunch_failed_text() -> String {
    let base = i18n::t("updates.relaunch_failed");
    if cfg!(target_os = "macos") {
        format!("{base} {}", i18n::t("updates.macos_allow_open"))
    } else {
        base
    }
}

async fn install_signed_update(app: &AppHandle) -> Result<(), InstallNotice> {
    if update_install_blocked(current_update_blockers(app)) {
        return Err(InstallNotice::Busy);
    }
    let updater = app
        .updater_builder()
        .version_comparator(|current, release| release_offered(&current, &release.version))
        .restart_after_install(false)
        .build()
        .map_err(|error| InstallNotice::Failed(error.to_string()))?;
    let update = updater
        .check()
        .await
        .map_err(|error| InstallNotice::Failed(error.to_string()))?;
    let Some(update) = update else {
        return Err(InstallNotice::Unavailable);
    };
    let announced = semver::Version::parse(&update.version)
        .map_err(|error| InstallNotice::Failed(error.to_string()))?;
    let current = semver::Version::parse(current_version()).expect("app version is semver");
    if !release_offered(&current, &announced) {
        return Err(InstallNotice::Unavailable);
    }
    if update_install_blocked(current_update_blockers(app)) {
        return Err(InstallNotice::Busy);
    }
    let bytes = update
        .download(|_: usize, _: Option<u64>| {}, || {})
        .await
        .map_err(|error| InstallNotice::Failed(error.to_string()))?;
    if update_install_blocked(current_update_blockers(app)) {
        return Err(InstallNotice::Busy);
    }
    update
        .restart_after_install(false)
        .install(bytes)
        .map_err(|error| InstallNotice::Failed(error.to_string()))?;
    // Windows 安装器启动后进程已退出,不会走到这里。macOS 与 Linux 安装
    // 成功后重新打开;打不开则留下已安装的文件,请用户自己再打开。
    #[cfg(not(windows))]
    if let Err(error) = relaunch_after_install(app) {
        log::warn!("relaunch after install failed: {error}");
        return Err(InstallNotice::RelaunchFailed);
    }
    Ok(())
}

/// 新进程必须在本进程放开单实例锁之后再启动,并观察它有没有留下来。
/// `open` 自己会马上退出,不能用来判断应用是否还在。
#[cfg(not(windows))]
const SUCCESSOR_SETTLE: Duration = Duration::from_millis(750);

#[cfg(any(test, not(windows)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuccessorWatch {
    ExitThisProcess,
    RelaunchFailed,
}

#[cfg(any(test, not(windows)))]
fn successor_may_start(lock_released: bool) -> bool {
    lock_released
}

#[cfg(any(test, not(windows)))]
fn successor_watch(spawned: bool, still_running: bool) -> SuccessorWatch {
    if spawned && still_running {
        SuccessorWatch::ExitThisProcess
    } else {
        SuccessorWatch::RelaunchFailed
    }
}

#[cfg(not(windows))]
fn relaunch_after_install(app: &AppHandle) -> Result<(), String> {
    let env = app.env();
    let path = tauri::process::current_binary(&env).map_err(|error| error.to_string())?;
    let args: Vec<_> = env.args_os.iter().skip(1).cloned().collect();
    let lock_released = crate::release_single_instance_for_relaunch();
    if !successor_may_start(lock_released) {
        return Err("single-instance lock still held".into());
    }
    let spawned = spawn_successor(&path, &args);
    let (spawned_ok, mut child) = match spawned {
        Ok(child) => (true, Some(child)),
        Err(error) => {
            log::warn!("successor spawn failed: {error}");
            (false, None)
        }
    };
    if spawned_ok {
        std::thread::sleep(SUCCESSOR_SETTLE);
    }
    let still_running = match child.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(None)),
        None => false,
    };
    match successor_watch(spawned_ok, still_running) {
        SuccessorWatch::ExitThisProcess => {
            app.exit(0);
            Ok(())
        }
        SuccessorWatch::RelaunchFailed => {
            crate::reacquire_single_instance(app);
            Err("successor did not stay running".into())
        }
    }
}

#[cfg(not(windows))]
fn spawn_successor(path: &Path, args: &[std::ffi::OsString]) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(path);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    command.spawn().map_err(|error| error.to_string())
}

fn dialog(
    text: &str,
    level: rfd::MessageLevel,
    buttons: rfd::MessageButtons,
) -> rfd::MessageDialogResult {
    rfd::MessageDialog::new()
        .set_title("Cropmark")
        .set_level(level)
        .set_buttons(buttons)
        .set_description(text)
        .show()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;

    use super::*;

    fn release(tag: &str, url: &str) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: tag.into(),
            html_url: url.into(),
        }
    }

    const PAGE: &str = "https://github.com/sakullla/cropmark/releases/tag/v9.9.9";

    /// 回归:本地端口接受后立即断开,https 请求应走到 TLS 握手并返回错误;
    /// TLS provider 未配置时这里会 panic。
    #[test]
    fn https_requests_use_an_enabled_tls_provider() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || drop(listener.accept()));
        let result = http_agent()
            .get(format!("https://127.0.0.1:{port}/"))
            .call();
        server.join().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn release_tags_parse_with_optional_v_prefix() {
        assert_eq!(
            parse_release_tag("v0.3.36"),
            Some(semver::Version::new(0, 3, 36))
        );
        assert_eq!(
            parse_release_tag("0.4.0"),
            Some(semver::Version::new(0, 4, 0))
        );
        assert_eq!(
            parse_release_tag("V1.0.0-rc.1"),
            semver::Version::parse("1.0.0-rc.1").ok()
        );
        assert_eq!(
            parse_release_tag(" v0.3.36 "),
            Some(semver::Version::new(0, 3, 36))
        );
        assert_eq!(parse_release_tag("release-1"), None);
        assert_eq!(parse_release_tag(""), None);
        assert_eq!(parse_release_tag("vv1.0.0"), None);
    }

    #[test]
    fn newer_release_is_available_and_equal_or_older_is_up_to_date() {
        let current = semver::Version::parse(current_version()).expect("app version is semver");
        let newer = current.major + 1;
        assert!(matches!(
            evaluate_release(release(&format!("v{newer}.0.0"), PAGE)),
            Ok(UpdateStatus::Available { .. })
        ));
        // 同版本、更旧版本(用户本地领先于线上)都不提示。
        assert!(matches!(
            evaluate_release(release(&format!("v{current}"), PAGE)),
            Ok(UpdateStatus::UpToDate { .. })
        ));
        let older = semver::Version::new(current.major, current.minor.saturating_sub(1), 0);
        assert!(matches!(
            evaluate_release(release(&format!("v{older}"), PAGE)),
            Ok(UpdateStatus::UpToDate { .. })
        ));
        // 稳定版不把更高的预发布 tag 当作更新。
        let prerelease_next = format!(
            "{}.{}.{}-rc.1",
            current.major,
            current.minor,
            current.patch + 1
        );
        assert!(matches!(
            evaluate_release(release(&prerelease_next, PAGE)),
            Ok(UpdateStatus::UpToDate { .. })
        ));
    }

    #[test]
    fn stable_install_does_not_offer_a_prerelease_tag() {
        let stable = semver::Version::new(1, 2, 0);
        let newer_stable = semver::Version::new(1, 3, 0);
        let older_stable = semver::Version::new(1, 1, 0);
        let newer_pre = semver::Version::parse("1.3.0-rc.1").unwrap();
        let current_pre = semver::Version::parse("1.2.0-rc.2").unwrap();
        let final_of_pre = semver::Version::new(1, 2, 0);

        assert!(!release_offered(&stable, &newer_pre));
        assert!(!release_offered(&stable, &stable));
        assert!(!release_offered(&stable, &older_stable));
        assert!(release_offered(&stable, &newer_stable));
        assert!(release_offered(&current_pre, &final_of_pre));
        assert!(release_offered(
            &current_pre,
            &semver::Version::parse("1.2.0-rc.3").unwrap()
        ));
        assert!(!release_offered(&current_pre, &older_stable));
    }

    #[test]
    fn unrecognized_tags_fail_instead_of_reporting() {
        assert!(evaluate_release(release("release-1", PAGE)).is_err());
        assert!(evaluate_release(release("", PAGE)).is_err());
    }

    #[test]
    fn release_urls_must_stay_inside_the_repository() {
        assert!(is_release_url(PAGE));
        assert!(is_release_url(
            "https://github.com/sakullla/cropmark/releases/latest"
        ));
        assert!(!is_release_url("https://evil.example/download"));
        assert!(!is_release_url(
            "http://github.com/sakullla/cropmark/releases"
        ));
        assert!(!is_release_url(""));
    }

    #[test]
    fn github_response_fields_deserialize() {
        let parsed: ReleaseInfo = serde_json::from_str(
            r#"{"tag_name":"v0.3.37","html_url":"https://github.com/sakullla/cropmark/releases/tag/v0.3.37","body":"notes","published_at":"2026-10-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(parsed.tag_name, "v0.3.37");
        assert!(parsed.html_url.ends_with("v0.3.37"));
        assert!(serde_json::from_str::<ReleaseInfo>(r#"{"html_url":"https://x"}"#).is_err());
    }

    #[test]
    fn status_serializes_camel_case_with_discriminator() {
        let up_to_date = UpdateStatus::UpToDate {
            current: "0.3.36".into(),
        };
        let value = serde_json::to_value(&up_to_date).unwrap();
        assert_eq!(value["status"], "upToDate");
        assert_eq!(value["current"], "0.3.36");
        let available = UpdateStatus::Available {
            current: "0.3.36".into(),
            latest: "0.3.37".into(),
            url: PAGE.into(),
        };
        let value = serde_json::to_value(&available).unwrap();
        assert_eq!(value["status"], "available");
        assert_eq!(value["latest"], "0.3.37");
        assert_eq!(value["url"], PAGE);
    }

    #[test]
    fn install_form_follows_the_running_package() {
        let bundled = Path::new("/Applications/Cropmark.app/Contents/MacOS/cropmark");
        let loose = Path::new("/tmp/cropmark");
        let bundle_itself = Path::new("/Applications/Cropmark.app");
        let appimage = Some(OsStr::new("/home/me/Cropmark.AppImage"));

        assert_eq!(
            classify_install_form(HostOs::Windows, false, loose, None),
            InstallForm::WindowsNsis
        );
        assert_eq!(
            classify_install_form(HostOs::Windows, true, loose, None),
            InstallForm::Unrecognized
        );
        assert_eq!(
            classify_install_form(HostOs::MacOs, false, bundled, None),
            InstallForm::MacOsApp
        );
        assert_eq!(
            classify_install_form(HostOs::MacOs, true, bundled, None),
            InstallForm::MacOsApp
        );
        assert_eq!(
            classify_install_form(HostOs::MacOs, false, loose, None),
            InstallForm::Unrecognized
        );
        assert_eq!(
            classify_install_form(HostOs::MacOs, false, bundle_itself, None),
            InstallForm::Unrecognized
        );
        assert_eq!(
            app_bundle_root(bundled)
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .as_deref(),
            Some("Cropmark.app")
        );
        assert_eq!(
            classify_install_form(HostOs::Linux, false, loose, appimage),
            InstallForm::LinuxAppImage
        );
        assert_eq!(
            classify_install_form(HostOs::Linux, true, loose, appimage),
            InstallForm::LinuxAppImage
        );
        assert_eq!(
            classify_install_form(HostOs::Linux, false, loose, None),
            InstallForm::Unrecognized
        );
        assert_eq!(
            classify_install_form(HostOs::Linux, false, loose, Some(OsStr::new(""))),
            InstallForm::Unrecognized
        );
        assert_eq!(
            classify_install_form(HostOs::Other, false, bundled, appimage),
            InstallForm::Unrecognized
        );
    }

    #[test]
    fn declining_confirm_does_not_install_or_open_a_page() {
        for form in [
            InstallForm::WindowsNsis,
            InstallForm::MacOsApp,
            InstallForm::LinuxAppImage,
            InstallForm::Unrecognized,
        ] {
            assert_eq!(action_after_confirm(form, false), None);
        }
        assert_eq!(
            action_after_confirm(InstallForm::WindowsNsis, true),
            Some(ConfirmedUpdateAction::Install)
        );
        assert_eq!(
            action_after_confirm(InstallForm::MacOsApp, true),
            Some(ConfirmedUpdateAction::Install)
        );
        assert_eq!(
            action_after_confirm(InstallForm::LinuxAppImage, true),
            Some(ConfirmedUpdateAction::Install)
        );
        assert_eq!(
            action_after_confirm(InstallForm::Unrecognized, true),
            Some(ConfirmedUpdateAction::OpenReleasePage)
        );
    }

    #[test]
    fn windows_confirm_text_says_the_app_will_exit() {
        assert_eq!(
            confirm_message_key(InstallForm::WindowsNsis, HostOs::Windows),
            "updates.confirm_windows"
        );
        let zh = i18n::tr(i18n::Language::ZhCn, "updates.confirm_windows");
        let en = i18n::tr(i18n::Language::En, "updates.confirm_windows");
        assert!(zh.contains("退出"), "{zh}");
        assert!(en.to_ascii_lowercase().contains("exit"), "{en}");
        let reopen = i18n::tr(i18n::Language::En, "updates.relaunch_failed");
        assert!(reopen.to_ascii_lowercase().contains("open"), "{reopen}");
    }

    #[test]
    fn install_is_refused_when_any_capture_is_busy() {
        let clear = UpdateBlockers {
            capture_session: false,
            recording_active: false,
            scroll_started: false,
        };
        assert!(!update_install_blocked(clear));
        assert!(update_install_blocked(UpdateBlockers {
            capture_session: true,
            ..clear
        }));
        assert!(update_install_blocked(UpdateBlockers {
            recording_active: true,
            ..clear
        }));
        assert!(update_install_blocked(UpdateBlockers {
            scroll_started: true,
            ..clear
        }));
    }

    #[test]
    fn known_update_stays_visible_until_a_later_check_is_current() {
        let available = UpdateStatus::Available {
            current: "0.3.42".into(),
            latest: "0.3.43".into(),
            url: PAGE.into(),
        };
        let known = known_after_check(&available);
        assert_eq!(about_install_version(known.as_ref()), Some("0.3.43"));
        let current = UpdateStatus::UpToDate {
            current: "0.3.43".into(),
        };
        let known = known_after_check(&current);
        assert_eq!(about_install_version(known.as_ref()), None);
    }

    #[test]
    fn successor_starts_only_after_the_lock_is_released_and_must_stay_up() {
        assert!(!successor_may_start(false));
        assert!(successor_may_start(true));
        assert_eq!(
            successor_watch(true, false),
            SuccessorWatch::RelaunchFailed
        );
        assert_eq!(
            successor_watch(false, false),
            SuccessorWatch::RelaunchFailed
        );
        assert_eq!(
            successor_watch(true, true),
            SuccessorWatch::ExitThisProcess
        );
    }
}
