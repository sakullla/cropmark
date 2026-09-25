//! R2 贴图持久化(ADR-6):索引与内容存 `app_data_dir/pins/`。
//!
//! - `index.json`:`{ version, pins: [PinRecord] }`,记录几何、旋转/翻转、
//!   透明度与分组;内容为 `<id>.png`(源图,未应用变换,恢复时重建)。
//! - 写入走临时文件 + 替换,避免半成品索引;损坏或更高版本按空索引处理。
//! - 关闭贴图时删除对应 PNG 并重写索引,「已关闭的贴图不因恢复而重现」;
//!   索引是恢复的唯一依据,孤立 PNG 不参与恢复。
//! - 删除与防抖落盘共用 [`io_lock`]:落盘前在锁内重新快照内存状态,
//!   避免「关闭」与后台写盘交错时把已关闭的贴图写回索引。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::capture::buffer::{decode_png, Frame};

pub const PIN_DIR_NAME: &str = "pins";
pub const INDEX_FILE_NAME: &str = "index.json";
pub const INDEX_VERSION: u32 = 1;
/// 透明度下限:0 会让贴图完全不可见且难以再选中。
pub const MIN_PIN_OPACITY: f32 = 0.05;

/// 单张贴图的持久化状态;`id` 同时决定内容文件名 `<id>.png`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinRecord {
    pub id: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
    #[serde(default = "default_scale")]
    pub scale: f64,
    #[serde(default)]
    pub rotation: u32,
    #[serde(default)]
    pub flip_h: bool,
    #[serde(default)]
    pub flip_v: bool,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    #[serde(default)]
    pub group: Option<String>,
}

fn default_scale() -> f64 {
    1.0
}

fn default_opacity() -> f32 {
    1.0
}

impl PinRecord {
    pub fn file_name(&self) -> String {
        format!("{}.png", self.id)
    }

    /// 索引自述字段合法化:非法 id/尺寸的记录不可恢复,直接丢弃;
    /// 旋转归一到 90° 整数倍,透明度钳制下限,分组 id 非法时按未分组。
    pub fn sanitized(mut self) -> Option<Self> {
        if !valid_token(&self.id) {
            return None;
        }
        if !(self.width.is_finite() && self.width > 0.0)
            || !(self.height.is_finite() && self.height > 0.0)
        {
            return None;
        }
        if !self.x.is_finite() || !self.y.is_finite() {
            self.x = 0.0;
            self.y = 0.0;
        }
        if !self.scale.is_finite() || self.scale <= 0.0 {
            self.scale = 1.0;
        }
        self.rotation = (self.rotation / 90 * 90) % 360;
        self.opacity = self.opacity.clamp(MIN_PIN_OPACITY, 1.0);
        self.group = self.group.filter(|group| valid_token(group));
        Some(self)
    }
}

/// id / 分组 id 的安全集合:限制为短 ASCII token,文件路径由 id 派生,
/// 不接受路径分隔符或其它可逃出 `pins/` 的字符。
fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PinIndex {
    version: u32,
    pins: Vec<PinRecord>,
}

impl Default for PinIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            pins: Vec::new(),
        }
    }
}

/// 删除与落盘的串行锁:保证「关闭删除」与后台防抖写入不会交错出旧索引。
pub fn io_lock() -> std::sync::MutexGuard<'static, ()> {
    static IO: Mutex<()> = Mutex::new(());
    IO.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("cropmark"))
        .join(PIN_DIR_NAME)
}

/// 读取索引并逐条合法化;缺文件/损坏/更高版本都按空索引(不恢复)。
pub fn load_from_dir(dir: &Path) -> Vec<PinRecord> {
    let text = match fs::read_to_string(dir.join(INDEX_FILE_NAME)) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let index: PinIndex = match serde_json::from_str(&text) {
        Ok(index) => index,
        Err(_) => return Vec::new(),
    };
    if index.version > INDEX_VERSION {
        return Vec::new();
    }
    sanitize_records(index.pins)
}

/// 记录级合法化:丢弃非法记录,id 去重(保留首条)。
pub fn sanitize_records(records: Vec<PinRecord>) -> Vec<PinRecord> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for record in records {
        let Some(record) = record.sanitized() else {
            continue;
        };
        if !seen.insert(record.id.clone()) {
            continue;
        }
        out.push(record);
    }
    out
}

/// 写内容 PNG + 索引;调用方须持有 [`io_lock`]。
pub fn persist_to_dir(
    dir: &Path,
    records: &[PinRecord],
    contents: &[(String, Vec<u8>)],
) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    for (id, png) in contents {
        if !valid_token(id) {
            continue;
        }
        write_atomic(&dir.join(format!("{id}.png")), png)?;
    }
    write_index(dir, records)
}

/// 只重写索引(关闭/恢复剔除记录时用);调用方须持有 [`io_lock`]。
pub fn write_index_to_dir(dir: &Path, records: &[PinRecord]) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    write_index(dir, records)
}

fn write_index(dir: &Path, records: &[PinRecord]) -> Result<(), String> {
    let index = PinIndex {
        version: INDEX_VERSION,
        pins: records.to_vec(),
    };
    let text = serde_json::to_string_pretty(&index).map_err(|error| error.to_string())?;
    write_atomic(&dir.join(INDEX_FILE_NAME), text.as_bytes())?;
    prune_orphans(dir, records);
    Ok(())
}

/// 清理索引中已不存在的 `<id>.png`(关闭、内容损坏被剔除、空索引覆盖等),
/// 保证孤立内容不会在后续恢复时被误当作可用记录。
fn prune_orphans(dir: &Path, records: &[PinRecord]) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let keep: BTreeSet<String> = records.iter().map(PinRecord::file_name).collect();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.ends_with(".png") && !keep.contains(name) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// 删除单张内容 PNG;索引由调用方按内存状态重写。
pub fn remove_files(dir: &Path, id: &str) {
    if !valid_token(id) {
        return;
    }
    let _ = fs::remove_file(dir.join(format!("{id}.png")));
}

/// 读取并解码某条记录的内容;文件缺失或损坏返回 None(恢复时跳过并剔除)。
pub fn read_frame_from_dir(dir: &Path, record: &PinRecord) -> Option<Frame> {
    let bytes = fs::read(dir.join(record.file_name())).ok()?;
    let mut frame = decode_png(&bytes).ok()?;
    frame.scale = record.scale;
    Some(frame)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("pin")
    ));
    fs::write(&temp, bytes).map_err(|error| error.to_string())?;
    if fs::rename(&temp, path).is_ok() {
        return Ok(());
    }
    // Windows 上目标已存在时 rename 会失败:先移除旧文件再替换。
    let _ = fs::remove_file(path);
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cropmark-pin-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn record(id: &str) -> PinRecord {
        PinRecord {
            id: id.to_string(),
            x: 120.0,
            y: 80.0,
            width: 400.0,
            height: 300.0,
            scale: 2.0,
            rotation: 90,
            flip_h: true,
            flip_v: false,
            opacity: 0.5,
            group: Some("g1".into()),
        }
    }

    #[test]
    fn roundtrip_keeps_geometry_transform_and_group() {
        let dir = temp_dir("roundtrip");
        let frame = Frame {
            width: 4,
            height: 2,
            rgba: vec![9; 32],
            scale: 2.0,
        };
        let png = crate::capture::buffer::encode_png(&frame).unwrap();
        let records = vec![record("p123-1")];
        persist_to_dir(&dir, &records, &[("p123-1".into(), png)]).unwrap();

        let loaded = load_from_dir(&dir);
        assert_eq!(loaded, records);
        let frame = read_frame_from_dir(&dir, &loaded[0]).unwrap();
        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(frame.scale, 2.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_corrupt_index_loads_empty() {
        let dir = temp_dir("corrupt");
        assert!(load_from_dir(&dir).is_empty());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(INDEX_FILE_NAME), "{not json").unwrap();
        assert!(load_from_dir(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn higher_index_version_is_not_loaded() {
        let dir = temp_dir("version");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(INDEX_FILE_NAME),
            r#"{"version":99,"pins":[{"id":"p1","width":10,"height":10}]}"#,
        )
        .unwrap();
        assert!(load_from_dir(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_drops_invalid_and_dedupes_ids() {
        let records = vec![
            record("ok-1"),
            record("ok-1"),
            record("../escape"),
            PinRecord {
                width: 0.0,
                ..record("zero-size")
            },
            PinRecord {
                id: "grouped".into(),
                group: Some("bad group".into()),
                ..record("grouped")
            },
        ];
        let clean = sanitize_records(records);
        assert_eq!(clean.len(), 2);
        assert_eq!(clean[0].id, "ok-1");
        // 非法分组被丢弃,记录本身保留。
        assert_eq!(clean[1].id, "grouped");
        assert_eq!(clean[1].group, None);
    }

    #[test]
    fn sanitize_normalizes_rotation_opacity_and_missing_numbers() {
        let parsed: PinIndex = serde_json::from_str(
            r#"{"version":1,"pins":[{"id":"p1","x":1,"y":2,"width":10,"height":20,"rotation":450,"opacity":0.0}]}"#,
        )
        .unwrap();
        let clean = sanitize_records(parsed.pins);
        assert_eq!(clean.len(), 1);
        assert_eq!(clean[0].rotation, 90);
        assert_eq!(clean[0].opacity, MIN_PIN_OPACITY);
        assert_eq!(clean[0].scale, 1.0);
        assert_eq!(clean[0].file_name(), "p1.png");
    }

    #[test]
    fn remove_files_deletes_content() {
        let dir = temp_dir("clear");
        let frame = Frame {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
            scale: 1.0,
        };
        let png = crate::capture::buffer::encode_png(&frame).unwrap();
        let records = vec![record("p1")];
        persist_to_dir(&dir, &records, &[("p1".into(), png)]).unwrap();
        assert!(dir.join("p1.png").exists());
        remove_files(&dir, "p1");
        assert!(!dir.join("p1.png").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_prunes_orphan_content_files() {
        let dir = temp_dir("prune");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("orphan.png"), b"stale").unwrap();
        persist_to_dir(&dir, &[record("p1")], &[]).unwrap();
        assert!(!dir.join("orphan.png").exists());
        // 索引中的记录内容不受影响。
        assert!(dir.join(INDEX_FILE_NAME).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_index_replaces_previous_records() {
        let dir = temp_dir("replace");
        persist_to_dir(&dir, &[record("p1"), record("p2")], &[]).unwrap();
        write_index_to_dir(&dir, &[record("p2")]).unwrap();
        let loaded = load_from_dir(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "p2");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_id_never_touches_the_filesystem() {
        let dir = temp_dir("invalid-id");
        remove_files(&dir, "../escape");
        assert!(!dir.exists());
    }
}
