//! 本地截图历史(R2,ADR-2)。
//!
//! 截图完成后在 `app_data_dir/history/` 保留最近记录:
//! - `index.json`:schemaVersion + 条目列表(新→旧),原子写(临时文件+rename);
//! - `<id>.png`:原始帧 PNG;
//! - `<id>.thumb.png`:最长边约 320px 的缩略图。
//!
//! 写入与缩略图生成在 `spawn_blocking` 中执行,不阻塞完成路径;按设置上限
//! 淘汰最旧记录。索引损坏或缩略图缺失时列表降级可用并给出可理解状态,
//! 数据只保存在本机,不上传。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::capture::buffer::{decode_png, encode_png, fit_display, resize_rgba, Frame};

/// 历史窗口标签:与 capabilities 的 windows 清单和隐藏清单保持同源。
pub const HISTORY_WINDOW: &str = crate::capture::ui::HISTORY;
/// 索引 schema 版本:不匹配时按降级状态展示,不猜测旧结构。
pub const SCHEMA_VERSION: u32 = 1;
/// 缩略图最长边(px)。
pub const THUMB_MAX_EDGE: u32 = 320;

const INDEX_FILE: &str = "index.json";
const INDEX_TMP_FILE: &str = "index.json.tmp";
const THUMB_SUFFIX: &str = ".thumb.png";

/// 索引读改写互斥:写入、删除、清空与裁剪都在此锁内完成。
static STORE_LOCK: Mutex<()> = Mutex::new(());
/// 记录 id 的同毫秒序号,避免同一毫秒内多条记录互相覆盖。
static ID_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub created_at: u64,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub file_name: String,
    pub thumb_name: String,
}

/// 索引加载状态:`Missing` 属正常空状态,其余情况在列表顶部给出说明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    Ready,
    Missing,
    Corrupted,
    Unsupported,
}

/// 列表条目视图:预计算文件缺失标记,前端无需再探测文件系统。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntryView {
    pub id: String,
    pub created_at: u64,
    pub width: u32,
    pub height: u32,
    pub thumb_missing: bool,
    pub image_missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryListPayload {
    pub entries: Vec<HistoryEntryView>,
    pub notice: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredIndex {
    schema_version: u32,
    entries: Vec<HistoryEntry>,
}

fn lock_store() -> std::sync::MutexGuard<'static, ()> {
    STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn history_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("cropmark"))
        .join("history")
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// 新条目排前:时间倒序,同毫秒保持插入顺序。
fn sort_entries(entries: &mut [HistoryEntry]) {
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
}

fn load_index(dir: &Path) -> (Vec<HistoryEntry>, IndexState) {
    match fs::read_to_string(dir.join(INDEX_FILE)) {
        Err(_) => (Vec::new(), IndexState::Missing),
        Ok(text) => match serde_json::from_str::<StoredIndex>(&text) {
            Ok(index) if index.schema_version == SCHEMA_VERSION => {
                let mut entries = index.entries;
                sort_entries(&mut entries);
                (entries, IndexState::Ready)
            }
            Ok(_) => (Vec::new(), IndexState::Unsupported),
            Err(_) => (Vec::new(), IndexState::Corrupted),
        },
    }
}

fn write_index(dir: &Path, entries: &[HistoryEntry]) -> Result<(), String> {
    let index = StoredIndex {
        schema_version: SCHEMA_VERSION,
        entries: entries.to_vec(),
    };
    let text = serde_json::to_string_pretty(&index).map_err(|error| error.to_string())?;
    let tmp = dir.join(INDEX_TMP_FILE);
    fs::write(&tmp, text).map_err(|error| format!("无法写入历史索引：{error}"))?;
    fs::rename(&tmp, dir.join(INDEX_FILE)).map_err(|error| format!("无法更新历史索引：{error}"))
}

fn index_notice(state: IndexState) -> Option<String> {
    match state {
        IndexState::Ready | IndexState::Missing => None,
        IndexState::Corrupted => {
            Some("历史索引已损坏，旧记录暂不可用；新的截图仍会继续记录。".into())
        }
        IndexState::Unsupported => Some("历史索引版本不受支持，当前无法读取旧记录。".into()),
    }
}

fn thumbnail_png(frame: &Frame) -> Result<Vec<u8>, String> {
    let (width, height) = fit_display(frame.width, frame.height, THUMB_MAX_EDGE);
    let resized = resize_rgba(frame, width, height).map_err(|error| error.user_message())?;
    encode_png(&resized).map_err(|error| error.user_message())
}

fn next_entry_id(dir: &Path, now_millis: u64) -> String {
    loop {
        let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
        let id = format!("{now_millis}-{seq}");
        if !dir.join(format!("{id}.png")).exists()
            && !dir.join(format!("{id}{THUMB_SUFFIX}")).exists()
        {
            return id;
        }
    }
}

/// 条目已按新→旧排列,超出上限的尾部即最旧记录。
fn trim_entries(entries: &mut Vec<HistoryEntry>, limit: u32) -> Vec<HistoryEntry> {
    let limit = limit.max(1) as usize;
    if entries.len() <= limit {
        return Vec::new();
    }
    entries.split_off(limit)
}

fn remove_entry_files(dir: &Path, entry: &HistoryEntry) {
    let _ = fs::remove_file(dir.join(&entry.file_name));
    let _ = fs::remove_file(dir.join(&entry.thumb_name));
}

/// 写入一条记录:PNG 与缩略图都编码完成后才进入索引临界区;索引写入失败
/// 时回滚新文件,淘汰的旧文件在索引更新成功后删除,避免索引引用缺失文件。
pub fn record_frame(
    dir: &Path,
    frame: &Frame,
    limit: u32,
    created_at: u64,
) -> Result<HistoryEntry, String> {
    let png = encode_png(frame).map_err(|error| error.user_message())?;
    let thumb = thumbnail_png(frame)?;
    let _guard = lock_store();
    fs::create_dir_all(dir).map_err(|error| format!("无法创建历史目录：{error}"))?;
    let (mut entries, _) = load_index(dir);
    let id = next_entry_id(dir, created_at);
    let entry = HistoryEntry {
        id: id.clone(),
        created_at,
        width: frame.width,
        height: frame.height,
        scale: frame.scale,
        file_name: format!("{id}.png"),
        thumb_name: format!("{id}{THUMB_SUFFIX}"),
    };
    fs::write(dir.join(&entry.file_name), &png)
        .map_err(|error| format!("无法写入历史图片：{error}"))?;
    fs::write(dir.join(&entry.thumb_name), &thumb)
        .map_err(|error| format!("无法写入历史缩略图：{error}"))?;
    entries.insert(0, entry.clone());
    // 系统时间回拨也不破坏"淘汰最旧"语义。
    sort_entries(&mut entries);
    let removed = trim_entries(&mut entries, limit);
    if let Err(error) = write_index(dir, &entries) {
        remove_entry_files(dir, &entry);
        return Err(error);
    }
    for old in &removed {
        remove_entry_files(dir, old);
    }
    Ok(entry)
}

/// 上限调低后立即裁剪最旧记录;索引更新成功才删文件。
pub fn prune_to_limit(dir: &Path, limit: u32) -> Result<(), String> {
    let _guard = lock_store();
    let (mut entries, _) = load_index(dir);
    let removed = trim_entries(&mut entries, limit);
    if removed.is_empty() {
        return Ok(());
    }
    write_index(dir, &entries)?;
    for entry in &removed {
        remove_entry_files(dir, entry);
    }
    Ok(())
}

pub fn list_views(dir: &Path) -> (Vec<HistoryEntryView>, Option<String>) {
    let _guard = lock_store();
    let (entries, state) = load_index(dir);
    let views = entries
        .iter()
        .map(|entry| HistoryEntryView {
            id: entry.id.clone(),
            created_at: entry.created_at,
            width: entry.width,
            height: entry.height,
            thumb_missing: !dir.join(&entry.thumb_name).is_file(),
            image_missing: !dir.join(&entry.file_name).is_file(),
        })
        .collect();
    (views, index_notice(state))
}

fn find_entry(dir: &Path, id: &str) -> Result<HistoryEntry, String> {
    let (entries, _) = load_index(dir);
    entries
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| "历史记录不存在或已被删除。".to_string())
}

/// 读取一条记录的原图(复制/贴图共用);文件缺失时报可理解错误。
pub fn read_entry(dir: &Path, id: &str) -> Result<(HistoryEntry, Vec<u8>), String> {
    let _guard = lock_store();
    let entry = find_entry(dir, id)?;
    let png = fs::read(dir.join(&entry.file_name))
        .map_err(|_| "历史图片文件缺失，无法复制或贴图。".to_string())?;
    Ok((entry, png))
}

pub fn read_thumbnail(dir: &Path, id: &str) -> Result<Vec<u8>, String> {
    let _guard = lock_store();
    let entry = find_entry(dir, id)?;
    fs::read(dir.join(&entry.thumb_name)).map_err(|_| "历史缩略图缺失。".to_string())
}

/// 删除单条;记录已不存在时视作成功(幂等),便于重复点击。
pub fn delete_entry(dir: &Path, id: &str) -> Result<(), String> {
    let _guard = lock_store();
    let (mut entries, _) = load_index(dir);
    let Some(position) = entries.iter().position(|entry| entry.id == id) else {
        return Ok(());
    };
    let entry = entries.remove(position);
    write_index(dir, &entries)?;
    remove_entry_files(dir, &entry);
    Ok(())
}

/// 清空全部:删除索引与目录内全部文件(含索引损坏后遗留的孤儿文件)。
pub fn clear_entries(dir: &Path) -> Result<(), String> {
    let _guard = lock_store();
    match fs::read_dir(dir) {
        Ok(read) => {
            for item in read.flatten() {
                let path = item.path();
                if path.is_file() {
                    let _ = fs::remove_file(path);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("无法清空历史记录：{error}")),
    }
    Ok(())
}

/// 完成路径调用:仅当 history.enabled 时把最终帧交给后台线程落盘,
/// 编码与缩略图生成都在 spawn_blocking 内,不阻塞完成路径。
pub fn record_capture(app: &AppHandle, frame: Frame) {
    let settings = crate::settings::current_history(app);
    if !settings.enabled {
        return;
    }
    let dir = history_dir(app);
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) = record_frame(&dir, &frame, settings.limit, now_millis()) {
            eprintln!("Cropmark: 无法写入截图历史：{error}");
        }
    });
}

/// 设置页调低上限后异步裁剪,不阻塞设置窗口。
pub fn prune_async(app: &AppHandle, limit: u32) {
    let dir = history_dir(app);
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) = prune_to_limit(&dir, limit) {
            eprintln!("Cropmark: 无法裁剪历史记录：{error}");
        }
    });
}

fn list_payload(app: &AppHandle) -> HistoryListPayload {
    let (entries, notice) = list_views(&history_dir(app));
    HistoryListPayload { entries, notice }
}

fn friendly(error: crate::capture::error::CaptureError) -> String {
    let message = error.user_message();
    if message.is_empty() {
        "无法完成历史操作。".into()
    } else {
        message
    }
}

#[tauri::command]
pub fn get_history(app: AppHandle) -> HistoryListPayload {
    list_payload(&app)
}

#[tauri::command]
pub fn get_history_thumbnail(app: AppHandle, id: String) -> Result<tauri::ipc::Response, String> {
    read_thumbnail(&history_dir(&app), &id).map(tauri::ipc::Response::new)
}

#[tauri::command]
pub fn copy_history_entry(app: AppHandle, id: String) -> Result<(), String> {
    let (_, png) = read_entry(&history_dir(&app), &id)?;
    let frame = decode_png(&png).map_err(friendly)?;
    crate::clipboard::copy_frame_with_png(&frame, &png).map_err(friendly)
}

#[tauri::command]
pub async fn pin_history_entry(app: AppHandle, id: String) -> Result<(), String> {
    let (entry, png) = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || read_entry(&history_dir(&app), &id)
    })
    .await
    .map_err(|_| "贴图线程失败。".to_string())??;
    crate::pin::open_pin_from_frame(&app, png, entry.width, entry.height, entry.scale)
}

#[tauri::command]
pub fn delete_history_entry(app: AppHandle, id: String) -> Result<HistoryListPayload, String> {
    delete_entry(&history_dir(&app), &id)?;
    Ok(list_payload(&app))
}

#[tauri::command]
pub fn clear_history(app: AppHandle) -> Result<HistoryListPayload, String> {
    clear_entries(&history_dir(&app))?;
    Ok(list_payload(&app))
}

/// 打开(或唤出)历史窗口;托盘与设置页入口共用。窗口按需创建,
/// 不在启动路径做任何历史 IO(R15)。已存在的窗口在显示后收到
/// `history-refresh`,避免截取期间被隐藏后列表停留在旧数据。
pub fn open_window(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(HISTORY_WINDOW) {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        let _ = window.emit("history-refresh", ());
        return Ok(());
    }
    WebviewWindowBuilder::new(
        app,
        HISTORY_WINDOW,
        WebviewUrl::App("index.html?view=history".into()),
    )
    .title("Cropmark")
    .inner_size(640.0, 680.0)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .skip_taskbar(true)
    .always_on_top(false)
    .visible(true)
    .center()
    .build()
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// 从设置页打开历史窗口。async 与 `pin_current` 同因:窗口构建需要泵
/// 平台消息,同步 command 会占住主线程。
#[tauri::command]
pub async fn open_history(app: AppHandle) -> Result<(), String> {
    open_window(&app)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cropmark-history-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Frame {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            rgba.extend_from_slice(&color);
        }
        Frame {
            width,
            height,
            rgba,
            scale: 1.0,
        }
    }

    #[test]
    fn record_writes_png_thumbnail_and_index_newest_first() {
        let dir = temp_dir("record");
        let first = record_frame(&dir, &solid(800, 200, [255, 0, 0, 255]), 20, 1_000).unwrap();
        let second = record_frame(&dir, &solid(40, 30, [0, 255, 0, 255]), 20, 2_000).unwrap();

        assert!(dir.join(&first.file_name).is_file());
        assert!(dir.join(&first.thumb_name).is_file());
        assert!(dir.join(&second.file_name).is_file());

        let (entries, state) = load_index(&dir);
        assert_eq!(state, IndexState::Ready);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, second.id);
        assert_eq!(entries[1].id, first.id);
        assert_eq!(entries[0].width, 40);
        assert_eq!(entries[0].height, 30);

        let thumb = decode_png(&fs::read(dir.join(&first.thumb_name)).unwrap()).unwrap();
        assert_eq!((thumb.width, thumb.height), (320, 80));

        let (views, notice) = list_views(&dir);
        assert!(notice.is_none());
        assert_eq!(views.len(), 2);
        assert!(!views[0].thumb_missing && !views[0].image_missing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_keeps_frame_scale_metadata() {
        let dir = temp_dir("scale");
        let mut frame = solid(16, 8, [1, 2, 3, 255]);
        frame.scale = 2.0;
        let entry = record_frame(&dir, &frame, 20, 10).unwrap();
        assert_eq!(entry.scale, 2.0);
        let (entries, _) = load_index(&dir);
        assert_eq!(entries[0].scale, 2.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_evicts_oldest_over_limit_and_removes_files() {
        let dir = temp_dir("evict");
        let mut recorded = Vec::new();
        for index in 0..5_u64 {
            recorded.push(
                record_frame(
                    &dir,
                    &solid(8, 8, [index as u8, 0, 0, 255]),
                    3,
                    1_000 + index,
                )
                .unwrap(),
            );
        }
        let (entries, _) = load_index(&dir);
        assert_eq!(entries.len(), 3);
        assert!(!dir.join(&recorded[0].file_name).exists());
        assert!(!dir.join(&recorded[1].thumb_name).exists());
        assert!(dir.join(&recorded[2].file_name).exists());
        assert_eq!(entries[0].id, recorded[4].id);
        assert_eq!(entries[2].id, recorded[2].id);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_millisecond_records_get_unique_ids_and_files() {
        let dir = temp_dir("ids");
        let first = record_frame(&dir, &solid(4, 4, [1, 2, 3, 255]), 20, 777).unwrap();
        let second = record_frame(&dir, &solid(4, 4, [3, 2, 1, 255]), 20, 777).unwrap();
        assert_ne!(first.id, second.id);
        assert!(dir.join(&first.file_name).is_file());
        assert!(dir.join(&second.file_name).is_file());
        assert_eq!(load_index(&dir).0.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_index_is_empty_and_not_degraded() {
        let dir = temp_dir("missing-index");
        fs::create_dir_all(&dir).unwrap();
        let (entries, state) = load_index(&dir);
        assert!(entries.is_empty());
        assert_eq!(state, IndexState::Missing);
        let (views, notice) = list_views(&dir);
        assert!(views.is_empty());
        assert!(notice.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupted_index_degrades_then_next_record_rebuilds() {
        let dir = temp_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(INDEX_FILE), b"{not json").unwrap();
        let (views, notice) = list_views(&dir);
        assert!(views.is_empty());
        assert!(notice.as_deref().unwrap_or_default().contains("损坏"));

        record_frame(&dir, &solid(6, 6, [9, 9, 9, 255]), 20, 42).unwrap();
        let (views, notice) = list_views(&dir);
        assert_eq!(views.len(), 1);
        assert!(notice.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_schema_is_reported_without_panicking() {
        let dir = temp_dir("unsupported");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(INDEX_FILE), r#"{"schemaVersion":99,"entries":[]}"#).unwrap();
        let (views, notice) = list_views(&dir);
        assert!(views.is_empty());
        assert!(notice.as_deref().unwrap_or_default().contains("版本"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_thumbnail_and_image_are_flagged_while_entry_stays_listed() {
        let dir = temp_dir("degrade");
        let entry = record_frame(&dir, &solid(8, 8, [0, 0, 255, 255]), 20, 5).unwrap();
        fs::remove_file(dir.join(&entry.thumb_name)).unwrap();
        let (views, _) = list_views(&dir);
        assert_eq!(views.len(), 1);
        assert!(views[0].thumb_missing);
        assert!(!views[0].image_missing);

        fs::remove_file(dir.join(&entry.file_name)).unwrap();
        let (views, _) = list_views(&dir);
        assert!(views[0].image_missing);
        assert!(read_entry(&dir, &entry.id).is_err());
        assert!(read_thumbnail(&dir, &entry.id).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_entry_files_and_is_idempotent() {
        let dir = temp_dir("delete");
        let entry = record_frame(&dir, &solid(8, 8, [1, 1, 1, 255]), 20, 1).unwrap();
        let keeper = record_frame(&dir, &solid(8, 8, [2, 2, 2, 255]), 20, 2).unwrap();

        delete_entry(&dir, &entry.id).unwrap();
        assert!(!dir.join(&entry.file_name).exists());
        assert!(!dir.join(&entry.thumb_name).exists());
        assert!(dir.join(&keeper.file_name).exists());
        assert_eq!(load_index(&dir).0.len(), 1);

        delete_entry(&dir, &entry.id).unwrap();
        assert_eq!(load_index(&dir).0.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_removes_index_and_every_file() {
        let dir = temp_dir("clear");
        record_frame(&dir, &solid(8, 8, [1, 1, 1, 255]), 20, 1).unwrap();
        record_frame(&dir, &solid(8, 8, [2, 2, 2, 255]), 20, 2).unwrap();
        clear_entries(&dir).unwrap();
        assert!(load_index(&dir).0.is_empty());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_to_limit_keeps_newest_and_removes_files() {
        let dir = temp_dir("prune");
        let mut recorded = Vec::new();
        for index in 0..4_u64 {
            recorded.push(
                record_frame(
                    &dir,
                    &solid(8, 8, [index as u8, 0, 0, 255]),
                    20,
                    100 + index,
                )
                .unwrap(),
            );
        }
        prune_to_limit(&dir, 2).unwrap();
        let (entries, _) = load_index(&dir);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, recorded[3].id);
        assert!(!dir.join(&recorded[0].file_name).exists());
        assert!(!dir.join(&recorded[1].thumb_name).exists());
        // 已在限内时裁剪是空操作。
        prune_to_limit(&dir, 2).unwrap();
        assert_eq!(load_index(&dir).0.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnail_caps_long_edge_and_keeps_small_images() {
        let dir = temp_dir("thumb");
        let big = thumbnail_png(&solid(800, 200, [10, 20, 30, 255])).unwrap();
        let decoded = decode_png(&big).unwrap();
        assert_eq!((decoded.width, decoded.height), (320, 80));
        let small = thumbnail_png(&solid(100, 50, [10, 20, 30, 255])).unwrap();
        let decoded = decode_png(&small).unwrap();
        assert_eq!((decoded.width, decoded.height), (100, 50));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_entry_returns_stored_png_bytes() {
        let dir = temp_dir("read");
        let frame = solid(12, 6, [4, 5, 6, 255]);
        let entry = record_frame(&dir, &frame, 20, 1).unwrap();
        let (stored, png) = read_entry(&dir, &entry.id).unwrap();
        assert_eq!(stored.id, entry.id);
        let decoded = decode_png(&png).unwrap();
        assert_eq!((decoded.width, decoded.height), (12, 6));
        assert_eq!(decoded.rgba, frame.rgba);
        assert!(read_entry(&dir, "missing-id").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
