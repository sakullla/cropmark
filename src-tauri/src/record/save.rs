//! R3 录制保存:复用现有保存对话框骨架、目录记忆与原子写盘。
//!
//! 录制过程只写临时文件;停止后按当前格式弹出保存对话框,确认后把临时文件
//! 复制到目标目录的临时名再改名(跨盘安全),成功后删除临时文件。取消或失败
//! 都不删除临时文件,由调用方决定重试或丢弃;失败返回的本地化文案说明保留
//! 位置与后续重试方式。暂无重试入口的调用方用 `keep_pending_recording` 登记
//! 待处理产物,后续录制 HUD 可经 `pending_recordings`/`take_pending_recordings`
//! 接入重试与清理。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use tauri::AppHandle;

use crate::export::{default_capture_file_name, partial_path};
use crate::filename_template::unique_path;
use crate::i18n;
use crate::settings;

use super::{RecordFormat, RecordingOutput};

/// 保存失败时登记的待处理产物(按失败顺序):对应的临时文件保留在磁盘上,
/// 由后续录制 HUD 重试保存或显式丢弃;应用运行期内不自动删除。
static PENDING_SAVES: Mutex<Vec<RecordingOutput>> = Mutex::new(Vec::new());

/// 保存失败:登记待处理产物,临时文件保持不删除。同一临时文件重复登记
/// (例如重试再次失败)保持单条,重试入口不会看到重复项。
pub fn keep_pending_recording(output: RecordingOutput) {
    let mut pending = PENDING_SAVES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending
        .iter()
        .any(|item| item.temp_path == output.temp_path)
    {
        return;
    }
    pending.push(output);
}

/// 当前待处理的保存失败产物(按失败顺序,克隆快照);供后续 HUD 展示与重试。
pub fn pending_recordings() -> Vec<RecordingOutput> {
    PENDING_SAVES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 取出全部待处理产物:重试成功或用户放弃后由调用方按需丢弃
/// (见 `discard_recording`),避免临时文件永久残留。
pub fn take_pending_recordings() -> Vec<RecordingOutput> {
    std::mem::take(
        &mut *PENDING_SAVES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

/// 按临时文件路径取出单个待处理产物(HUD 的重试保存/丢弃入口):
/// 重试取消或再次失败时由调用方用 `keep_pending_recording` 放回。
pub fn remove_pending_recording(temp_path: &Path) -> Option<RecordingOutput> {
    let mut pending = PENDING_SAVES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = pending
        .iter()
        .position(|item| item.temp_path == temp_path)?;
    Some(pending.remove(index))
}

/// 保存结果:取消时 `saved=false` 且 `path=None`,临时文件保留。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordSaveResult {
    pub saved: bool,
    pub format: RecordFormat,
    pub path: Option<String>,
}

/// 丢弃录制产物(用户取消保存或应用退出清理):删除临时文件。
pub fn discard_recording(output: &RecordingOutput) {
    let _ = std::fs::remove_file(&output.temp_path);
}

/// 用户输入路径 → 目标路径:扩展名与录制格式一致(大小写不敏感)时保留;
/// 缺失扩展名补全当前格式;其余(未知或其它格式的扩展名)追加规范后缀,
/// 避免写出扩展名与内容不符的文件——录制无法在保存时转码。
pub fn resolve_recording_target(path: PathBuf, format: RecordFormat) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case(format.extension()) {
        return path;
    }
    if extension.is_empty() {
        let mut adjusted = path;
        adjusted.set_extension(format.extension());
        return adjusted;
    }
    let mut adjusted = path;
    let name = adjusted
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cropmark".into());
    adjusted.set_file_name(format!("{name}.{}", format.extension()));
    adjusted
}

/// 原子写盘:复制到同目录临时文件再改名;任一步失败都清理目标目录的半成品
/// 并保留录制临时文件。成功后删除录制临时文件。
pub fn move_output_atomic(output: &RecordingOutput, path: &Path) -> Result<(), String> {
    let partial = partial_path(path);
    if let Err(error) = std::fs::copy(&output.temp_path, &partial) {
        let _ = std::fs::remove_file(&partial);
        log::warn!("record save failed kind=io");
        return Err(save_error(path, &output.temp_path, &error));
    }
    if let Err(error) = std::fs::rename(&partial, path) {
        let _ = std::fs::remove_file(&partial);
        log::warn!("record save failed kind=io");
        return Err(save_error(path, &output.temp_path, &error));
    }
    let _ = std::fs::remove_file(&output.temp_path);
    log::info!(
        "record saved format={} frames={} duration_ms={}",
        output.format.extension(),
        output.frame_count,
        output.duration_ms
    );
    Ok(())
}

fn save_error(path: &Path, temp_path: &Path, error: &std::io::Error) -> String {
    i18n::tp(
        "error.record.save_to_path",
        &[
            ("path", &path.display().to_string()),
            ("temp", &temp_path.display().to_string()),
            ("error", &error.to_string()),
        ],
    )
}

/// 停止录制后的保存对话框:过滤器与默认文件名按录制格式;起始目录复用
/// 导出目录记忆;成功后更新目录记忆。取消返回 `saved=false`。
pub async fn save_recording_with_dialog(
    app: &AppHandle,
    parent: Option<&tauri::WebviewWindow>,
    output: &RecordingOutput,
) -> Result<RecordSaveResult, String> {
    let export = settings::current_export(app);
    let extension = output.format.extension();
    let mut dialog = rfd::AsyncFileDialog::new()
        .add_filter(i18n::t("dialog.recordings_filter"), &[extension])
        .set_file_name(default_capture_file_name(extension))
        .set_title(i18n::t("dialog.save_recording_title"));
    if let Some(directory) = export.existing_directory() {
        dialog = dialog.set_directory(directory);
    }
    if let Some(window) = parent {
        dialog = dialog.set_parent(window);
    }
    let Some(file) = dialog.save_file().await else {
        return Ok(RecordSaveResult {
            saved: false,
            format: output.format,
            path: None,
        });
    };
    let path = unique_path(&resolve_recording_target(
        file.path().to_path_buf(),
        output.format,
    ));
    move_output_atomic(output, &path)?;
    settings::remember_export_directory(app, path.parent());
    Ok(RecordSaveResult {
        saved: true,
        format: output.format,
        path: Some(path.to_string_lossy().into_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_output(dir: &Path, format: RecordFormat) -> RecordingOutput {
        let temp_path = dir.join(format!("recording.{}", format.extension()));
        fs::write(&temp_path, b"recorded-bytes").expect("temp file");
        RecordingOutput {
            format,
            temp_path,
            width: 320,
            height: 200,
            frame_count: 12,
            duration_ms: 1200,
            fps: 15,
            auto_stopped: false,
            interrupted: None,
        }
    }

    #[test]
    fn discard_removes_the_temp_file_and_leaves_the_save_directory_empty() {
        let root =
            std::env::temp_dir().join(format!("cropmark-record-fidelity-{}", std::process::id()));
        let save_dir = root.join("saved");
        let temp_dir = root.join("temp");
        fs::create_dir_all(&save_dir).expect("save dir");
        fs::create_dir_all(&temp_dir).expect("temp dir");
        let output = sample_output(&temp_dir, RecordFormat::Mp4);
        assert!(
            save_dir.read_dir().expect("read save").next().is_none(),
            "save directory must stay empty before an explicit save"
        );
        discard_recording(&output);
        assert!(
            !output.temp_path.exists(),
            "discard must delete the temp file"
        );
        assert!(
            save_dir.read_dir().expect("read save").next().is_none(),
            "discard must not leave a file in the save directory"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_target_keeps_matching_extensions_and_appends_others() {
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip.gif"), RecordFormat::Gif),
            PathBuf::from("clip.gif")
        );
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip.MP4"), RecordFormat::Mp4),
            PathBuf::from("clip.MP4")
        );
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip"), RecordFormat::Webp),
            PathBuf::from("clip.webp")
        );
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip.mov"), RecordFormat::Mp4),
            PathBuf::from("clip.mov.mp4")
        );
        // 其它录制格式的扩展名不能保留:保存时不转码,内容与扩展名必须一致。
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip.gif"), RecordFormat::Mp4),
            PathBuf::from("clip.gif.mp4")
        );
    }

    #[test]
    fn move_output_atomic_writes_target_and_removes_temp() {
        let dir = std::env::temp_dir().join(format!("cropmark-record-save-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let output = sample_output(&dir, RecordFormat::Gif);
        let target = dir.join("kept.gif");
        move_output_atomic(&output, &target).expect("save");
        assert_eq!(fs::read(&target).unwrap(), b"recorded-bytes");
        assert!(!output.temp_path.exists(), "temp file must be removed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_output_atomic_reports_localized_error_and_keeps_temp() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-record-save-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let output = sample_output(&dir, RecordFormat::Mp4);
        let unwritable = dir.join("missing-subdir").join("clip.mp4");
        let error = move_output_atomic(&output, &unwritable).unwrap_err();
        assert!(error.contains("无法保存"), "got {error}");
        assert!(error.contains("missing-subdir"), "got {error}");
        assert!(
            error.contains(&output.temp_path.display().to_string()),
            "error must point to the kept recording: {error}"
        );
        assert!(error.contains("不会自动删除"), "got {error}");
        assert!(
            output.temp_path.exists(),
            "failed save must keep the recording temp file"
        );
        assert!(!partial_path(&unwritable).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discard_recording_removes_temp_file() {
        let dir = std::env::temp_dir().join(format!("cropmark-record-drop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let output = sample_output(&dir, RecordFormat::Webp);
        discard_recording(&output);
        assert!(!output.temp_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 保存失败的产物登记后:临时文件保持不删除、可被列出与取出,只有显式
    /// 丢弃才删除。该接口供后续录制 HUD 提供重试与清理。
    #[test]
    fn pending_recordings_stay_listed_and_on_disk_until_discarded() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-record-pending-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let output = sample_output(&dir, RecordFormat::Gif);
        keep_pending_recording(output.clone());
        keep_pending_recording(output.clone());
        assert_eq!(
            pending_recordings()
                .iter()
                .filter(|item| item.temp_path == output.temp_path)
                .count(),
            1,
            "re-registering the same temp file must keep a single pending entry"
        );
        assert!(output.temp_path.exists(), "kept file must remain on disk");
        let taken = take_pending_recordings();
        assert!(
            taken.iter().any(|item| item.temp_path == output.temp_path),
            "take must hand the kept recording to the retry/cleanup consumer"
        );
        discard_recording(&output);
        assert!(
            !output.temp_path.exists(),
            "explicit discard must remove the kept file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// HUD 的重试入口按临时文件路径取单个待处理项:取到后可重试;
    /// 取消/再次失败时放回、成功后不再出现;未知路径明确返回 None。
    #[test]
    fn remove_pending_recording_takes_by_exact_path_and_returns_it_again() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-record-remove-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let output = sample_output(&dir, RecordFormat::Mp4);
        assert!(
            remove_pending_recording(&output.temp_path).is_none(),
            "unregistered path must miss"
        );
        keep_pending_recording(output.clone());
        let taken = remove_pending_recording(&output.temp_path)
            .expect("registered path must be handed to the retry consumer");
        assert_eq!(taken.temp_path, output.temp_path);
        assert!(
            remove_pending_recording(&output.temp_path).is_none(),
            "a taken entry must not be handed out twice"
        );
        // 重试取消/失败路径放回后仍可再次取出。
        keep_pending_recording(output.clone());
        assert!(remove_pending_recording(&output.temp_path).is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
