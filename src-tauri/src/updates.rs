//! 检查更新:查询 GitHub Releases 最新发布,与当前版本做语义化比对。
//! 只做「检查 + 打开发布页」,不做应用内自更新——后者需要更新签名密钥、
//! 新增发布工件(latest.json、.app.tar.gz)并改发布流程,先保持发布链不变。
//!
//! - 托盘「检查更新」在后台线程查询,结果回主线程用原生对话框反馈;
//! - 设置页「关于」里的检查走 `check_for_updates` command,结果内联展示;
//! - 启动静默检查由「启动时检查更新」开关(默认开)门控,失败只记日志;
//! - 发现新版本时把版本与下载页记入 `UpdateState` 并重建托盘菜单,
//!   下载入口出现;打开的一律是 GitHub 发布页,不经任何自有服务器。

use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

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

/// 查询 GitHub 最新发布。GitHub API 要求 User-Agent;无 token,走
/// 匿名限额(按 IP 60 次/小时),启动 + 手动检查的量级远够。
pub fn fetch_latest_release() -> Result<ReleaseInfo, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .new_agent();
    let mut response = agent
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

/// 比对最新发布与当前版本。tag 无法解析按失败处理(不提示、不误报);
/// 预发布 tag 不会被 `/releases/latest` 返回,无需单独处理。
pub fn evaluate_release(release: ReleaseInfo) -> Result<UpdateStatus, String> {
    let latest = parse_release_tag(&release.tag_name)
        .ok_or_else(|| format!("unrecognized tag: {}", release.tag_name))?;
    let current = semver::Version::parse(current_version()).expect("app version is semver");
    if latest > current {
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

pub fn known_update(app: &AppHandle) -> Option<KnownUpdate> {
    lock(&app.state::<UpdateState>().0).clone()
}

/// 记录新版本并重建托盘菜单(下载入口出现)。菜单构建走平台主线程。
pub fn remember_update(app: &AppHandle, update: KnownUpdate) {
    *lock(&app.state::<UpdateState>().0) = Some(update);
    let task = app.clone();
    let _ = app.run_on_main_thread(move || crate::tray::refresh_menu(&task));
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
    if let UpdateStatus::Available { latest, url, .. } = &status {
        remember_update(
            &app,
            KnownUpdate {
                version: latest.clone(),
                url: url.clone(),
            },
        );
    }
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
            Ok(UpdateStatus::Available { latest, url, .. }) => {
                log::info!("update available latest={latest}");
                remember_update(
                    &app,
                    KnownUpdate {
                        version: latest,
                        url,
                    },
                );
            }
            Ok(UpdateStatus::UpToDate { .. }) => log::info!("update check up to date"),
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
            Ok(UpdateStatus::Available { latest, url, .. }) => {
                remember_update(
                    &task,
                    KnownUpdate {
                        version: latest.clone(),
                        url: url.clone(),
                    },
                );
                let prompt = i18n::tp("updates.open_prompt", &[("version", &latest)]);
                if dialog(&prompt, rfd::MessageLevel::Info, rfd::MessageButtons::YesNo)
                    == rfd::MessageDialogResult::Yes
                {
                    if let Err(error) = open_release_page(task.clone()) {
                        log::warn!("open release page failed: {error}");
                    }
                }
            }
            Ok(UpdateStatus::UpToDate { current }) => {
                let text = i18n::tp("updates.uptodate", &[("version", &current)]);
                dialog(&text, rfd::MessageLevel::Info, rfd::MessageButtons::Ok);
            }
            Err(error) => {
                let text = i18n::tp("updates.check_failed", &[("error", &error)]);
                dialog(&text, rfd::MessageLevel::Warning, rfd::MessageButtons::Ok);
            }
        });
    });
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
    use super::*;

    fn release(tag: &str, url: &str) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: tag.into(),
            html_url: url.into(),
        }
    }

    const PAGE: &str = "https://github.com/sakullla/cropmark/releases/tag/v9.9.9";

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
        // 预发布版本号本身大于旧 final(0.3.36 → 0.3.37-rc.1)按可用提示;
        // 反向(rc 领先于线上 final)则提示 final 可用,rc 用户应转正。
        let prerelease_next = format!(
            "{}.{}.{}-rc.1",
            current.major,
            current.minor,
            current.patch + 1
        );
        assert!(matches!(
            evaluate_release(release(&prerelease_next, PAGE)),
            Ok(UpdateStatus::Available { .. })
        ));
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
}
