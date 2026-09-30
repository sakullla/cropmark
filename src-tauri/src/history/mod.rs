//! 本地截图历史(R2,ADR-2;R7,ADR-8)。
//!
//! 截图完成后在 `app_data_dir/history/` 保留最近记录:
//! - `index.json`:schemaVersion + 条目列表(新→旧),原子写(临时文件+rename);
//! - `<id>.png`:原始帧 PNG;
//! - `<id>.thumb.png`:最长边约 320px 的缩略图;
//! - `pending-delete.json`:R6 限时撤销标记。删除/清空先把条目移出索引、
//!   文件原地保留并落标记(含完整条目元数据与截止时间),8 秒内可整体撤销;
//!   到期线程、退出与启动兜底统一走 `sweep_pending` 终结标记并删除文件。
//!   后到操作为准:新删除/清空会先终结既有标记,旧批不可再撤销。
//!
//! 索引 v2 为每条增加 `mode`(region/window/fullscreen/long)、`favorite`、`note`。
//! 读取更低版本时按默认值在内存中升级(无模式、未收藏、空备注),下次写入才落成 v2;
//! 高于当前版本仍走不支持提示,不猜测结构。筛选、收藏置顶与备注检索只改变展示,
//! 不改变删除、清空与按时间淘汰的语义。
//!
//! 写入与缩略图生成在 `spawn_blocking` 中执行,不阻塞完成路径;按设置上限
//! 淘汰最旧记录。索引损坏或缩略图缺失时列表降级可用并给出可理解状态,
//! 数据只保存在本机,不上传。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::capture::buffer::{decode_png, encode_png, fit_display, resize_rgba, Frame};
use crate::i18n;

/// 历史窗口标签:与 capabilities 的 windows 清单和隐藏清单保持同源。
pub const HISTORY_WINDOW: &str = crate::capture::ui::HISTORY;
/// 索引 schema 版本。低于此版本可读并在下次写入时升级;高于此版本不支持。
pub const SCHEMA_VERSION: u32 = 2;
/// 缩略图最长边(px)。
pub const THUMB_MAX_EDGE: u32 = 320;
/// 备注最大字符数(按 Unicode 标量值,不是字节)。
pub const NOTE_MAX_CHARS: usize = 500;
/// R6:删除/清空后的限时撤销窗口(毫秒),窗口内条目文件原地保留。
pub const UNDO_WINDOW_MS: u64 = 8_000;
/// 可筛选的采集模式 token,与文件名模板 `{mode}` 保持同一套稳定英文值。
const MODE_TOKENS: [&str; 4] = ["region", "window", "fullscreen", "long"];

const INDEX_FILE: &str = "index.json";
const INDEX_TMP_FILE: &str = "index.json.tmp";
const PENDING_FILE: &str = "pending-delete.json";
const PENDING_TMP_FILE: &str = "pending-delete.json.tmp";
const THUMB_SUFFIX: &str = ".thumb.png";

/// 索引读改写互斥:写入、删除、清空与裁剪都在此锁内完成。
static STORE_LOCK: Mutex<()> = Mutex::new(());
/// 记录 id 的同毫秒序号,避免同一毫秒内多条记录互相覆盖。
static ID_SEQ: AtomicU64 = AtomicU64::new(0);
/// pending-delete 批 id 的同毫秒序号:到期线程据批 id 识别过时定时器。
static PENDING_SEQ: AtomicU64 = AtomicU64::new(0);

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
    /// 采集模式 token。旧索引没有该字段,仅在「全部模式」下展示。
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub note: String,
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
    /// 未知或缺失模式为 `None`,前端仅在「全部模式」中显示。
    pub mode: Option<String>,
    pub favorite: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryListPayload {
    pub entries: Vec<HistoryEntryView>,
    pub notice: Option<String>,
    /// R9:历史检索与收藏/备注去门控常开,固定为 true;字段保留供前端沿用
    /// 同一列表布局契约。
    pub tools_enabled: bool,
    /// R6:当前可撤销的删除/清空批;无标记或已过窗口时为 `None`。
    pub pending_undo: Option<PendingUndoPayload>,
}

/// 前端撤销条视图:操作种类、条数与截止时间(毫秒),用于文案与剩余时长。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingUndoPayload {
    /// `delete`(单条)或 `clear`(清空批量)。
    pub kind: String,
    pub count: usize,
    pub expires_at: u64,
}

/// R6:限时撤销标记。条目元数据整体入批,撤销时原样回索引(含收藏/备注);
/// `deadline` 为 Unix 毫秒,过期后只允许终结不允许恢复。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingDelete {
    id: String,
    kind: String,
    deadline: u64,
    entries: Vec<HistoryEntry>,
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

/// 新条目排前:时间倒序,同毫秒保持插入顺序。收藏置顶只在展示层处理。
fn sort_entries(entries: &mut [HistoryEntry]) {
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
}

/// 只接受稳定模式 token;空白、未知值与缺失都视为无模式。
fn normalize_mode(mode: Option<&str>) -> Option<String> {
    let token = mode?.trim();
    MODE_TOKENS
        .iter()
        .copied()
        .find(|known| *known == token)
        .map(str::to_string)
}

/// 备注按单行保存:换行与制表符折成空格,去掉其他控制字符,并截断到上限。
fn sanitize_note(note: &str) -> String {
    let mut cleaned = String::new();
    let mut count = 0usize;
    for ch in note.chars() {
        if count >= NOTE_MAX_CHARS {
            break;
        }
        let mapped = match ch {
            '\n' | '\r' | '\t' => ' ',
            ch if ch.is_control() => continue,
            ch => ch,
        };
        cleaned.push(mapped);
        count += 1;
    }
    cleaned.trim().to_string()
}

fn normalize_entry(mut entry: HistoryEntry) -> HistoryEntry {
    entry.mode = normalize_mode(entry.mode.as_deref());
    entry.note = sanitize_note(&entry.note);
    entry
}

fn load_index(dir: &Path) -> (Vec<HistoryEntry>, IndexState) {
    match fs::read_to_string(dir.join(INDEX_FILE)) {
        Err(_) => (Vec::new(), IndexState::Missing),
        Ok(text) => match serde_json::from_str::<StoredIndex>(&text) {
            // 当前版本与更低版本都能读。低版本缺的字段由 serde 默认值补上,
            // 本次读取不回写;下一次 record/delete/prune/收藏/备注写入才落成 v2。
            Ok(index) if index.schema_version <= SCHEMA_VERSION => {
                let mut entries: Vec<HistoryEntry> =
                    index.entries.into_iter().map(normalize_entry).collect();
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
    fs::write(&tmp, text).map_err(|error| {
        i18n::tp(
            "error.history.write_index",
            &[("error", &error.to_string())],
        )
    })?;
    fs::rename(&tmp, dir.join(INDEX_FILE)).map_err(|error| {
        i18n::tp(
            "error.history.rename_index",
            &[("error", &error.to_string())],
        )
    })
}

fn index_notice(state: IndexState) -> Option<String> {
    match state {
        IndexState::Ready | IndexState::Missing => None,
        IndexState::Corrupted => Some(i18n::t("error.history.corrupted")),
        IndexState::Unsupported => Some(i18n::t("error.history.unsupported")),
    }
}

// --- R6:限时撤销(pending-delete)-----------------------------------------

fn pending_path(dir: &Path) -> PathBuf {
    dir.join(PENDING_FILE)
}

/// 读取标记;缺失或结构损坏都视作无待撤销批(损坏标记由 sweep 兜底清掉)。
fn load_pending(dir: &Path) -> Option<PendingDelete> {
    let text = fs::read_to_string(pending_path(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 落一批待删除条目:文件原地保留,标记原子写(临时文件+rename)。
/// 失败时调用方负责回退到立即删除语义,不留索引外文件。
fn stage_pending(
    dir: &Path,
    kind: &str,
    entries: Vec<HistoryEntry>,
) -> Result<PendingDelete, String> {
    let pending = PendingDelete {
        id: format!(
            "pending-{}-{}",
            now_millis(),
            PENDING_SEQ.fetch_add(1, Ordering::Relaxed)
        ),
        kind: kind.to_string(),
        deadline: now_millis().saturating_add(UNDO_WINDOW_MS),
        entries,
    };
    let text = serde_json::to_string_pretty(&pending).map_err(|error| error.to_string())?;
    let tmp = dir.join(PENDING_TMP_FILE);
    fs::write(&tmp, text).map_err(|error| {
        i18n::tp(
            "error.history.write_index",
            &[("error", &error.to_string())],
        )
    })?;
    fs::rename(&tmp, pending_path(dir)).map_err(|error| {
        i18n::tp(
            "error.history.rename_index",
            &[("error", &error.to_string())],
        )
    })?;
    Ok(pending)
}

/// 终结当前标记:删除批内条目文件与标记本身。调用方必须已持有 STORE_LOCK。
fn finalize_pending_locked(dir: &Path) {
    if let Some(pending) = load_pending(dir) {
        for entry in &pending.entries {
            remove_entry_files(dir, entry);
        }
    }
    let _ = fs::remove_file(pending_path(dir));
    let _ = fs::remove_file(dir.join(PENDING_TMP_FILE));
}

/// 到期/退出/启动共用的兜底清理:终结遗留标记并删除其条目文件。
/// 到期线程在锁内按批 id 过期时间判定,过时定时器(已被新操作取代)不动作。
fn schedule_pending_expiry(dir: PathBuf, batch_id: String, deadline: u64) {
    std::thread::spawn(move || {
        let now = now_millis();
        if deadline > now {
            std::thread::sleep(std::time::Duration::from_millis(deadline - now));
        }
        let _guard = lock_store();
        if let Some(pending) = load_pending(&dir) {
            if pending.id == batch_id && now_millis() >= pending.deadline {
                finalize_pending_locked(&dir);
            }
        }
    });
}

/// 启动/退出兜底(R6):终结异常退出遗留的 pending-delete,条目与文件最终
/// 删除,重启后不复活。启动侧由后台线程调用,不在启动路径同步做历史 IO。
pub fn sweep_pending(dir: &Path) {
    let _guard = lock_store();
    finalize_pending_locked(dir);
}

/// 撤销最近一次删除/清空:窗口内把批内条目(含收藏/备注等元数据)原样
/// 回索引,文件未动即完整回原位。无标记或已过期返回 `false`(过期时顺带
/// 终结标记)。同一 id 已被新记录占位时跳过该条,不覆盖新数据。
pub fn undo_pending_delete(dir: &Path) -> Result<bool, String> {
    let _guard = lock_store();
    let Some(pending) = load_pending(dir) else {
        return Ok(false);
    };
    if now_millis() >= pending.deadline {
        finalize_pending_locked(dir);
        return Ok(false);
    }
    let (mut entries, state) = load_index(dir);
    match state {
        IndexState::Corrupted => return Err(i18n::t("error.history.corrupted")),
        IndexState::Unsupported => return Err(i18n::t("error.history.unsupported")),
        IndexState::Ready | IndexState::Missing => {}
    }
    for entry in pending.entries {
        if !entries.iter().any(|existing| existing.id == entry.id) {
            entries.push(entry);
        }
    }
    sort_entries(&mut entries);
    write_index(dir, &entries)?;
    let _ = fs::remove_file(pending_path(dir));
    let _ = fs::remove_file(dir.join(PENDING_TMP_FILE));
    Ok(true)
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
    mode: Option<&str>,
) -> Result<HistoryEntry, String> {
    let png = encode_png(frame).map_err(|error| error.user_message())?;
    let thumb = thumbnail_png(frame)?;
    let _guard = lock_store();
    fs::create_dir_all(dir)
        .map_err(|error| i18n::tp("error.history.create_dir", &[("error", &error.to_string())]))?;
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
        mode: normalize_mode(mode),
        favorite: false,
        note: String::new(),
    };
    fs::write(dir.join(&entry.file_name), &png).map_err(|error| {
        i18n::tp(
            "error.history.write_image",
            &[("error", &error.to_string())],
        )
    })?;
    fs::write(dir.join(&entry.thumb_name), &thumb).map_err(|error| {
        i18n::tp(
            "error.history.write_thumb",
            &[("error", &error.to_string())],
        )
    })?;
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
    log::info!(
        "history wrote id={} bytes={} size={}x{} mode={}",
        entry.id,
        png.len(),
        entry.width,
        entry.height,
        entry.mode.as_deref().unwrap_or("-")
    );
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
            mode: entry.mode.clone(),
            favorite: entry.favorite,
            note: entry.note.clone(),
        })
        .collect();
    (views, index_notice(state))
}

fn find_entry(dir: &Path, id: &str) -> Result<HistoryEntry, String> {
    let (entries, _) = load_index(dir);
    entries
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| i18n::t("error.history.missing"))
}

/// 再编辑用的原图帧:只读,不改索引或文件名。尺寸以 PNG 为准,缩放沿用索引。
pub fn load_entry_frame(dir: &Path, id: &str) -> Result<Frame, String> {
    let (entry, png) = read_entry(dir, id)?;
    let mut frame = decode_png(&png).map_err(friendly)?;
    frame.scale = entry.scale;
    Ok(frame)
}

/// 读取一条记录的原图(复制/贴图共用);文件缺失时报可理解错误。
pub fn read_entry(dir: &Path, id: &str) -> Result<(HistoryEntry, Vec<u8>), String> {
    let _guard = lock_store();
    let entry = find_entry(dir, id)?;
    let png =
        fs::read(dir.join(&entry.file_name)).map_err(|_| i18n::t("error.history.image_missing"))?;
    Ok((entry, png))
}

pub fn read_thumbnail(dir: &Path, id: &str) -> Result<Vec<u8>, String> {
    let _guard = lock_store();
    let entry = find_entry(dir, id)?;
    fs::read(dir.join(&entry.thumb_name)).map_err(|_| i18n::t("error.history.thumb_missing"))
}

/// 删除单条(R6):条目立即移出索引,文件原地保留并入 pending-delete 批,
/// 8 秒内可撤销;到期线程/退出/启动兜底负责最终删除。后到操作为准:既有
/// 待撤销批先被终结(文件删除)。记录已不存在时视作成功(幂等)。
pub fn delete_entry(dir: &Path, id: &str) -> Result<(), String> {
    let _guard = lock_store();
    let (mut entries, _) = load_index(dir);
    let Some(position) = entries.iter().position(|entry| entry.id == id) else {
        return Ok(());
    };
    let entry = entries.remove(position);
    finalize_pending_locked(dir);
    write_index(dir, &entries)?;
    match stage_pending(dir, "delete", vec![entry.clone()]) {
        Ok(pending) => {
            schedule_pending_expiry(dir.to_path_buf(), pending.id, pending.deadline);
            Ok(())
        }
        // 置标失败退回旧的立即删除语义:索引已不含该条,不能留索引外文件。
        Err(error) => {
            remove_entry_files(dir, &entry);
            Err(error)
        }
    }
}

/// 改一条的收藏或备注。不支持/损坏的索引不覆盖;成功写入即把低版本落成 v2。
fn update_entry(
    dir: &Path,
    id: &str,
    mutate: impl FnOnce(&mut HistoryEntry),
) -> Result<(), String> {
    let _guard = lock_store();
    let (mut entries, state) = load_index(dir);
    match state {
        IndexState::Corrupted => return Err(i18n::t("error.history.corrupted")),
        IndexState::Unsupported => return Err(i18n::t("error.history.unsupported")),
        IndexState::Ready | IndexState::Missing => {}
    }
    let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
        return Err(i18n::t("error.history.missing"));
    };
    mutate(entry);
    write_index(dir, &entries)
}

fn set_favorite(dir: &Path, id: &str, favorite: bool) -> Result<(), String> {
    update_entry(dir, id, |entry| entry.favorite = favorite)
}

fn set_note(dir: &Path, id: &str, note: &str) -> Result<(), String> {
    let note = sanitize_note(note);
    update_entry(dir, id, move |entry| entry.note = note)
}

/// 清空全部(R6):索引条目整体进入 pending-delete 批,文件原地保留待
/// 撤销;孤儿文件(索引损坏遗留等)仍立即删除;既有待撤销批先被终结
/// (后到操作为准)。索引移除成功、标记写入失败时退回旧的立即删除语义。
pub fn clear_entries(dir: &Path) -> Result<(), String> {
    let _guard = lock_store();
    finalize_pending_locked(dir);
    let (entries, _) = load_index(dir);
    let kept: HashSet<String> = entries
        .iter()
        .flat_map(|entry| [entry.file_name.clone(), entry.thumb_name.clone()])
        .collect();
    // 索引先移除:此窗口内崩溃只会留下孤儿文件,不会出现索引引用已删文件的
    // 降级视图;随后标记落盘,撤销依据才生效。
    match fs::remove_file(dir.join(INDEX_FILE)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(i18n::tp(
                "error.history.clear",
                &[("error", &error.to_string())],
            ))
        }
    }
    match fs::read_dir(dir) {
        Ok(read) => {
            for item in read.flatten() {
                let path = item.path();
                if path.is_file()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| !kept.contains(name))
                {
                    let _ = fs::remove_file(path);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(i18n::tp(
                "error.history.clear",
                &[("error", &error.to_string())],
            ))
        }
    }
    if entries.is_empty() {
        return Ok(());
    }
    match stage_pending(dir, "clear", entries.clone()) {
        Ok(pending) => {
            schedule_pending_expiry(dir.to_path_buf(), pending.id, pending.deadline);
            Ok(())
        }
        Err(error) => {
            for entry in &entries {
                remove_entry_files(dir, entry);
            }
            Err(error)
        }
    }
}

/// 完成路径调用:仅当 history.enabled 时把最终帧交给后台线程落盘,
/// 编码与缩略图生成都在 spawn_blocking 内,不阻塞完成路径。
/// `mode` 由调用方在会话仍在时取好,避免后台线程读到已清空的会话。
pub fn record_capture(app: &AppHandle, frame: Frame, mode: Option<crate::hotkeys::CaptureMode>) {
    let settings = crate::settings::current_history(app);
    if !settings.enabled {
        log::debug!("history skipped kind=disabled");
        return;
    }
    let mode = mode
        .map(crate::export::capture_mode_token)
        .map(str::to_string);
    let dir = history_dir(app);
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) =
            record_frame(&dir, &frame, settings.limit, now_millis(), mode.as_deref())
        {
            log::warn!("history write failed kind=io");
            eprintln!("Cropmark: 无法写入截图历史：{error}");
            crate::capture::ui::show_toast_key(&app, "toast.history_write_failed");
        }
    });
}

/// 设置页调低上限后异步裁剪,不阻塞设置窗口。
pub fn prune_async(app: &AppHandle, limit: u32) {
    let dir = history_dir(app);
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) = prune_to_limit(&dir, limit) {
            log::warn!("history prune failed kind=io");
            eprintln!("Cropmark: 无法裁剪历史记录：{error}");
        }
    });
}

fn list_payload(app: &AppHandle) -> HistoryListPayload {
    let dir = history_dir(app);
    let (entries, notice) = list_views(&dir);
    let pending_undo = {
        let _guard = lock_store();
        load_pending(&dir)
            .filter(|pending| now_millis() < pending.deadline)
            .map(|pending| PendingUndoPayload {
                kind: pending.kind,
                count: pending.entries.len(),
                expires_at: pending.deadline,
            })
    };
    HistoryListPayload {
        entries,
        notice,
        tools_enabled: true,
        pending_undo,
    }
}

fn friendly(error: crate::capture::error::CaptureError) -> String {
    let message = error.user_message();
    if message.is_empty() {
        i18n::t("error.history.failed")
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

/// 再编辑:把该条历史图像装入现有预览,不写回贴图,也不改历史目录。
#[tauri::command]
pub async fn reedit_history_entry(app: AppHandle, id: String) -> Result<(), String> {
    let frame = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || load_entry_frame(&history_dir(&app), &id)
    })
    .await
    .map_err(|_| i18n::t("error.history.failed"))??;
    crate::pin::finish_pin_edit(&app);
    crate::capture::session::adopt_history_frame(&app, frame.clone()).map_err(friendly)?;
    crate::capture::ui::open_preview(&app, &frame)
        .map_err(friendly)
        .map(|_| ())
}

#[tauri::command]
pub async fn pin_history_entry(app: AppHandle, id: String) -> Result<(), String> {
    let (entry, png) = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || read_entry(&history_dir(&app), &id)
    })
    .await
    .map_err(|_| i18n::t("error.pin.thread_pin"))??;
    crate::pin::open_pin_from_frame(&app, png, entry.width, entry.height, entry.scale)
}

#[tauri::command]
pub fn set_history_favorite(
    app: AppHandle,
    id: String,
    favorite: bool,
) -> Result<HistoryListPayload, String> {
    set_favorite(&history_dir(&app), &id, favorite)?;
    Ok(list_payload(&app))
}

#[tauri::command]
pub fn set_history_note(
    app: AppHandle,
    id: String,
    note: String,
) -> Result<HistoryListPayload, String> {
    set_note(&history_dir(&app), &id, &note)?;
    Ok(list_payload(&app))
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

/// R6:撤销最近一次删除/清空。窗口已过或无待撤销批时为无害空操作,
/// 返回的列表如实反映当前状态。
#[tauri::command]
pub fn undo_history_delete(app: AppHandle) -> Result<HistoryListPayload, String> {
    undo_pending_delete(&history_dir(&app))?;
    Ok(list_payload(&app))
}

/// R6 启动兜底:后台线程清理异常退出遗留的 pending-delete,不在启动路径
/// 同步做历史 IO(R15)。
pub fn sweep_on_startup(app: &AppHandle) {
    let dir = history_dir(app);
    tauri::async_runtime::spawn_blocking(move || sweep_pending(&dir));
}

/// R6 退出兜底:窗口期内退出时条目与文件最终删除,重启不复活。
pub fn sweep_on_exit(app: &AppHandle) {
    sweep_pending(&history_dir(app));
}

/// 打开(或唤出)历史窗口;托盘与设置页入口共用。窗口按需创建,
/// 不在启动路径做任何历史 IO(R15)。已存在的窗口在显示后收到
/// `history-refresh`,避免截取期间被隐藏后列表停留在旧数据。
pub fn open_window(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(HISTORY_WINDOW) {
        crate::front::reveal(app, &window);
        let _ = window.emit("history-refresh", ());
        return Ok(());
    }
    let window = WebviewWindowBuilder::new(
        app,
        HISTORY_WINDOW,
        WebviewUrl::App("index.html?view=history".into()),
    )
    .title("Cropmark")
    .inner_size(640.0, 760.0)
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
    crate::front::reveal(app, &window);
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

    fn record(
        dir: &Path,
        frame: &Frame,
        limit: u32,
        created_at: u64,
    ) -> Result<HistoryEntry, String> {
        record_frame(dir, frame, limit, created_at, None)
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
        let first = record(&dir, &solid(800, 200, [255, 0, 0, 255]), 20, 1_000).unwrap();
        let second = record(&dir, &solid(40, 30, [0, 255, 0, 255]), 20, 2_000).unwrap();

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
        let entry = record(&dir, &frame, 20, 10).unwrap();
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
                record(
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
        let first = record(&dir, &solid(4, 4, [1, 2, 3, 255]), 20, 777).unwrap();
        let second = record(&dir, &solid(4, 4, [3, 2, 1, 255]), 20, 777).unwrap();
        assert_ne!(first.id, second.id);
        assert!(dir.join(&first.file_name).is_file());
        assert!(dir.join(&second.file_name).is_file());
        assert_eq!(load_index(&dir).0.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_into_unwritable_dir_returns_error_without_writing() {
        // 目标路径的父级是已存在文件:create_dir_all 失败,错误向上传播
        // (record_capture 据此在日志之外弹 toast),不落任何文件。
        let dir = temp_dir("io-error");
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocked");
        fs::write(&blocker, b"file").unwrap();
        let target = blocker.join("history");
        let result = record(&target, &solid(8, 8, [0, 0, 0, 255]), 20, 1);
        assert!(result.is_err());
        assert!(!target.exists());
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

        record(&dir, &solid(6, 6, [9, 9, 9, 255]), 20, 42).unwrap();
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
        let entry = record(&dir, &solid(8, 8, [0, 0, 255, 255]), 20, 5).unwrap();
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
    fn delete_stages_pending_then_sweep_removes_files_idempotently() {
        let dir = temp_dir("delete");
        let entry = record(&dir, &solid(8, 8, [1, 1, 1, 255]), 20, 1).unwrap();
        let keeper = record(&dir, &solid(8, 8, [2, 2, 2, 255]), 20, 2).unwrap();

        delete_entry(&dir, &entry.id).unwrap();
        // R6:条目立即出索引,文件与标记原地保留等待撤销窗口。
        assert_eq!(load_index(&dir).0.len(), 1);
        assert!(dir.join(&entry.file_name).is_file());
        assert!(dir.join(&entry.thumb_name).is_file());
        assert!(dir.join(PENDING_FILE).is_file());
        assert!(dir.join(&keeper.file_name).exists());

        // 到期/退出/启动共用的兜底路径最终删除文件与标记。
        sweep_pending(&dir);
        assert!(!dir.join(&entry.file_name).exists());
        assert!(!dir.join(&entry.thumb_name).exists());
        assert!(!dir.join(PENDING_FILE).exists());
        assert!(dir.join(&keeper.file_name).exists());
        assert_eq!(load_index(&dir).0.len(), 1);

        // 记录不存在时删除幂等,不影响现存条目。
        delete_entry(&dir, &entry.id).unwrap();
        assert_eq!(load_index(&dir).0.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_undo_restores_entry_with_metadata_and_is_once_only() {
        let dir = temp_dir("undo-delete");
        let entry = record_frame(&dir, &solid(8, 8, [1, 1, 1, 255]), 20, 1, Some("long")).unwrap();
        set_favorite(&dir, &entry.id, true).unwrap();
        set_note(&dir, &entry.id, "keep me").unwrap();
        let image = fs::read(dir.join(&entry.file_name)).unwrap();
        let thumb = fs::read(dir.join(&entry.thumb_name)).unwrap();

        delete_entry(&dir, &entry.id).unwrap();
        assert!(read_entry(&dir, &entry.id).is_err());
        // 撤销窗口外的新记录与撤销共存:按时间重排,互不覆盖。
        let fresh = record(&dir, &solid(4, 4, [9, 9, 9, 255]), 20, 100).unwrap();

        assert!(undo_pending_delete(&dir).unwrap());
        let (entries, _) = load_index(&dir);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, fresh.id);
        let restored = entries.iter().find(|e| e.id == entry.id).unwrap();
        assert!(restored.favorite);
        assert_eq!(restored.note, "keep me");
        assert_eq!(restored.mode.as_deref(), Some("long"));
        assert_eq!(fs::read(dir.join(&entry.file_name)).unwrap(), image);
        assert_eq!(fs::read(dir.join(&entry.thumb_name)).unwrap(), thumb);
        assert!(!dir.join(PENDING_FILE).exists());

        // 标记已消费:再次撤销是无害空操作。
        assert!(!undo_pending_delete(&dir).unwrap());
        assert_eq!(load_index(&dir).0.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_after_deadline_finalizes_files_instead_of_restoring() {
        let dir = temp_dir("undo-expired");
        let entry = record(&dir, &solid(8, 8, [1, 1, 1, 255]), 20, 1).unwrap();
        delete_entry(&dir, &entry.id).unwrap();
        // 把标记截止时间改到过去,模拟用户在 8 秒窗口之后才点撤销。
        let mut pending = load_pending(&dir).unwrap();
        pending.deadline = now_millis().saturating_sub(1);
        fs::write(
            pending_path(&dir),
            serde_json::to_string_pretty(&pending).unwrap(),
        )
        .unwrap();

        assert!(!undo_pending_delete(&dir).unwrap());
        assert!(load_index(&dir).0.is_empty());
        assert!(!dir.join(&entry.file_name).exists());
        assert!(!dir.join(&entry.thumb_name).exists());
        assert!(!dir.join(PENDING_FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_sweep_cleans_crash_leftover_and_nothing_resurrects() {
        let dir = temp_dir("undo-sweep");
        let entry = record(&dir, &solid(8, 8, [1, 1, 1, 255]), 20, 1).unwrap();
        delete_entry(&dir, &entry.id).unwrap();
        // 异常退出:到期线程与退出兜底都没跑,标记与文件遗留。
        assert!(dir.join(PENDING_FILE).is_file());
        assert!(dir.join(&entry.file_name).is_file());

        // 下次启动的 sweep 终结遗留:条目与文件最终删除,重启不复活。
        sweep_pending(&dir);
        assert!(!dir.join(&entry.file_name).exists());
        assert!(!dir.join(&entry.thumb_name).exists());
        assert!(!dir.join(PENDING_FILE).exists());
        assert!(load_index(&dir).0.is_empty());
        // 撤销入口随标记一起消失。
        assert!(!undo_pending_delete(&dir).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_batches_all_entries_then_sweep_removes_every_file() {
        let dir = temp_dir("clear");
        let first = record(&dir, &solid(8, 8, [1, 1, 1, 255]), 20, 1).unwrap();
        let second = record(&dir, &solid(8, 8, [2, 2, 2, 255]), 20, 2).unwrap();
        // 索引未引用的孤儿文件(索引损坏遗留等)清空时立即删除,不进撤销批。
        fs::write(dir.join("orphan.png"), b"stale").unwrap();

        clear_entries(&dir).unwrap();
        let (entries, state) = load_index(&dir);
        assert!(entries.is_empty());
        assert_eq!(state, IndexState::Missing);
        assert!(!dir.join(INDEX_FILE).exists());
        assert!(!dir.join("orphan.png").exists());
        // 批内文件与标记在窗口内保留。
        assert!(dir.join(&first.file_name).is_file());
        assert!(dir.join(&second.thumb_name).is_file());
        assert!(dir.join(PENDING_FILE).is_file());

        sweep_pending(&dir);
        assert!(load_index(&dir).0.is_empty());
        assert!(!dir.join(&first.file_name).exists());
        assert!(!dir.join(&second.thumb_name).exists());
        assert!(!dir.join(PENDING_FILE).exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_undo_restores_whole_batch_with_metadata() {
        let dir = temp_dir("undo-clear");
        let first =
            record_frame(&dir, &solid(8, 8, [1, 1, 1, 255]), 20, 1, Some("region")).unwrap();
        let second =
            record_frame(&dir, &solid(6, 6, [2, 2, 2, 255]), 20, 2, Some("window")).unwrap();
        set_favorite(&dir, &first.id, true).unwrap();
        set_note(&dir, &second.id, "batch note").unwrap();
        assert!(load_pending(&dir).is_none());

        clear_entries(&dir).unwrap();
        let pending = load_pending(&dir).unwrap();
        assert_eq!(pending.kind, "clear");
        assert_eq!(pending.entries.len(), 2);

        assert!(undo_pending_delete(&dir).unwrap());
        let (entries, state) = load_index(&dir);
        assert_eq!(state, IndexState::Ready);
        assert_eq!(entries.len(), 2);
        let restored_first = entries.iter().find(|e| e.id == first.id).unwrap();
        let restored_second = entries.iter().find(|e| e.id == second.id).unwrap();
        assert!(restored_first.favorite);
        assert_eq!(restored_first.mode.as_deref(), Some("region"));
        assert_eq!(restored_second.note, "batch note");
        assert_eq!(restored_second.mode.as_deref(), Some("window"));
        assert!(dir.join(&first.file_name).is_file());
        assert!(dir.join(&second.thumb_name).is_file());
        assert!(!dir.join(PENDING_FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn later_operation_supersedes_earlier_pending_batch() {
        let dir = temp_dir("undo-last-op");
        let first = record(&dir, &solid(8, 8, [1, 1, 1, 255]), 20, 1).unwrap();
        let second = record(&dir, &solid(6, 6, [2, 2, 2, 255]), 20, 2).unwrap();

        // 先删 first:进入待撤销批。
        delete_entry(&dir, &first.id).unwrap();
        // 再清空:既有批被终结(first 的文件最终删除),清空批成为唯一可撤销项。
        clear_entries(&dir).unwrap();
        assert!(!dir.join(&first.file_name).exists());
        assert!(!dir.join(&first.thumb_name).exists());
        let pending = load_pending(&dir).unwrap();
        assert_eq!(pending.kind, "clear");
        assert_eq!(pending.entries.len(), 1);
        assert_eq!(pending.entries[0].id, second.id);

        // 撤销只恢复后到的清空批,first 不复活。
        assert!(undo_pending_delete(&dir).unwrap());
        let (entries, _) = load_index(&dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, second.id);
        assert!(dir.join(&second.file_name).is_file());
        assert!(!dir.join(&first.file_name).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_to_limit_keeps_newest_and_removes_files() {
        let dir = temp_dir("prune");
        let mut recorded = Vec::new();
        for index in 0..4_u64 {
            recorded.push(
                record(
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
    fn reedit_load_keeps_directory_format_and_blocks_when_image_is_missing() {
        let dir = temp_dir("reedit");
        let mut frame = solid(10, 6, [7, 8, 9, 255]);
        frame.scale = 1.5;
        let entry = record(&dir, &frame, 20, 9).unwrap();
        let index_before = fs::read(dir.join(INDEX_FILE)).unwrap();
        let image_before = fs::read(dir.join(&entry.file_name)).unwrap();
        let thumb_before = fs::read(dir.join(&entry.thumb_name)).unwrap();

        let loaded = load_entry_frame(&dir, &entry.id).unwrap();
        assert_eq!((loaded.width, loaded.height), (10, 6));
        assert_eq!(loaded.scale, 1.5);
        assert_eq!(loaded.rgba, frame.rgba);
        assert_eq!(fs::read(dir.join(INDEX_FILE)).unwrap(), index_before);
        assert_eq!(fs::read(dir.join(&entry.file_name)).unwrap(), image_before);
        assert_eq!(fs::read(dir.join(&entry.thumb_name)).unwrap(), thumb_before);
        assert_eq!(entry.file_name, format!("{}.png", entry.id));
        assert_eq!(entry.thumb_name, format!("{}{THUMB_SUFFIX}", entry.id));

        fs::remove_file(dir.join(&entry.file_name)).unwrap();
        let missing = load_entry_frame(&dir, &entry.id).unwrap_err();
        assert!(missing.contains("再编辑") || missing.contains("re-edit"));
        delete_entry(&dir, &entry.id).unwrap();
        assert!(load_index(&dir).0.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_entry_returns_stored_png_bytes() {
        let dir = temp_dir("read");
        let frame = solid(12, 6, [4, 5, 6, 255]);
        let entry = record(&dir, &frame, 20, 1).unwrap();
        let (stored, png) = read_entry(&dir, &entry.id).unwrap();
        assert_eq!(stored.id, entry.id);
        let decoded = decode_png(&png).unwrap();
        assert_eq!((decoded.width, decoded.height), (12, 6));
        assert_eq!(decoded.rgba, frame.rgba);
        assert!(read_entry(&dir, "missing-id").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    fn index_json(dir: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(dir.join(INDEX_FILE)).unwrap()).unwrap()
    }

    fn write_v1_index(dir: &Path, id: &str) -> String {
        fs::create_dir_all(dir).unwrap();
        let text = format!(
            r#"{{"schemaVersion":1,"entries":[{{"id":"{id}","createdAt":10,"width":4,"height":2,"scale":1.0,"fileName":"{id}.png","thumbName":"{id}.thumb.png"}}]}}"#
        );
        fs::write(dir.join(INDEX_FILE), &text).unwrap();
        text
    }

    #[test]
    fn v1_index_reads_with_defaults_and_upgrades_on_next_write() {
        let dir = temp_dir("upgrade-v1");
        let original = write_v1_index(&dir, "old");
        let (entries, state) = load_index(&dir);
        assert_eq!(state, IndexState::Ready);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "old");
        assert_eq!(entries[0].mode, None);
        assert!(!entries[0].favorite);
        assert_eq!(entries[0].note, "");
        assert_eq!(fs::read_to_string(dir.join(INDEX_FILE)).unwrap(), original);

        set_favorite(&dir, "old", true).unwrap();
        let stored = index_json(&dir);
        assert_eq!(stored["schemaVersion"], 2);
        assert_eq!(stored["entries"][0]["favorite"], true);
        assert_eq!(stored["entries"][0]["note"], "");
        assert!(stored["entries"][0]["mode"].is_null());

        let (reloaded, state) = load_index(&dir);
        assert_eq!(state, IndexState::Ready);
        assert!(reloaded[0].favorite);
        assert_eq!(reloaded[0].mode, None);

        let added =
            record_frame(&dir, &solid(4, 4, [1, 2, 3, 255]), 20, 50, Some("window")).unwrap();
        let (entries, _) = load_index(&dir);
        assert_eq!(entries[0].id, added.id);
        assert_eq!(entries[0].mode.as_deref(), Some("window"));
        assert!(!entries[0].favorite);
        assert_eq!(entries[1].id, "old");
        assert!(entries[1].favorite);
        assert_eq!(index_json(&dir)["schemaVersion"], 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mode_favorite_and_note_roundtrip_and_unknown_mode_is_dropped() {
        let dir = temp_dir("meta");
        let long = record_frame(&dir, &solid(4, 4, [9, 9, 9, 255]), 20, 3, Some(" long ")).unwrap();
        assert_eq!(long.mode.as_deref(), Some("long"));
        let unknown = record_frame(
            &dir,
            &solid(4, 4, [8, 8, 8, 255]),
            20,
            4,
            Some("FULLSCREEN"),
        )
        .unwrap();
        assert_eq!(unknown.mode, None);

        set_favorite(&dir, &long.id, true).unwrap();
        set_note(&dir, &long.id, "  hello\nworld\t备注  ").unwrap();
        let (entries, _) = load_index(&dir);
        let stored = entries.iter().find(|entry| entry.id == long.id).unwrap();
        assert!(stored.favorite);
        assert_eq!(stored.note, "hello world 备注");
        assert_eq!(stored.mode.as_deref(), Some("long"));

        let (views, notice) = list_views(&dir);
        assert!(notice.is_none());
        let view = views.iter().find(|entry| entry.id == long.id).unwrap();
        assert!(view.favorite);
        assert_eq!(view.note, "hello world 备注");
        assert_eq!(view.mode.as_deref(), Some("long"));

        set_note(&dir, &long.id, "   \n\t  ").unwrap();
        assert_eq!(
            load_index(&dir)
                .0
                .iter()
                .find(|entry| entry.id == long.id)
                .unwrap()
                .note,
            ""
        );
        let too_long = format!("测{}", "画".repeat(NOTE_MAX_CHARS));
        set_note(&dir, &long.id, &too_long).unwrap();
        let note = load_index(&dir)
            .0
            .into_iter()
            .find(|entry| entry.id == long.id)
            .unwrap()
            .note;
        assert_eq!(note.chars().count(), NOTE_MAX_CHARS);
        assert!(note.starts_with('测'));
        assert!(!note.contains('\n'));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn favorite_does_not_reorder_storage_or_survive_eviction_and_clear() {
        let dir = temp_dir("fav-policy");
        let oldest = record(&dir, &solid(4, 4, [1, 0, 0, 255]), 20, 1).unwrap();
        let middle = record(&dir, &solid(4, 4, [0, 1, 0, 255]), 20, 2).unwrap();
        let newest = record(&dir, &solid(4, 4, [0, 0, 1, 255]), 20, 3).unwrap();
        set_favorite(&dir, &oldest.id, true).unwrap();
        set_note(&dir, &middle.id, "kept").unwrap();
        let (entries, _) = load_index(&dir);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec![newest.id.as_str(), middle.id.as_str(), oldest.id.as_str()]
        );
        assert!(entries[2].favorite);

        delete_entry(&dir, &oldest.id).unwrap();
        let (entries, _) = load_index(&dir);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| !entry.favorite));
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id == middle.id)
                .unwrap()
                .note,
            "kept"
        );

        set_favorite(&dir, &middle.id, true).unwrap();
        prune_to_limit(&dir, 1).unwrap();
        let (entries, _) = load_index(&dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, newest.id);
        assert!(!dir.join(&middle.file_name).exists());

        set_favorite(&dir, &newest.id, true).unwrap();
        set_note(&dir, &newest.id, "gone").unwrap();
        clear_entries(&dir).unwrap();
        let (entries, state) = load_index(&dir);
        assert!(entries.is_empty());
        assert_eq!(state, IndexState::Missing);
        assert!(!dir.join(INDEX_FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn note_and_favorite_do_not_overwrite_unsupported_or_corrupt_indexes() {
        let dir = temp_dir("protect-index");
        fs::create_dir_all(&dir).unwrap();
        let unsupported = r#"{"schemaVersion":99,"entries":[{"id":"x","createdAt":1,"width":1,"height":1,"scale":1.0,"fileName":"x.png","thumbName":"x.thumb.png","favorite":true}]}"#;
        fs::write(dir.join(INDEX_FILE), unsupported).unwrap();
        assert!(set_favorite(&dir, "x", false).unwrap_err().contains("版本"));
        assert!(set_note(&dir, "x", "nope").unwrap_err().contains("版本"));
        assert_eq!(
            fs::read_to_string(dir.join(INDEX_FILE)).unwrap(),
            unsupported
        );

        fs::write(dir.join(INDEX_FILE), b"{not json").unwrap();
        assert!(set_note(&dir, "x", "nope").unwrap_err().contains("损坏"));
        assert_eq!(
            fs::read_to_string(dir.join(INDEX_FILE)).unwrap(),
            "{not json"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
