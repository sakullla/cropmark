use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::annotate::{parse_hex_color, DEFAULT_COLOR};
use crate::autostart::{self, AutostartRejection, AutostartState};
use crate::beautify::BeautifyOptions;
use crate::hotkeys::{self, CaptureMode, HotkeyErrors, Hotkeys};
use crate::i18n::{self, Language};

/// 序号工具起始值允许范围；超出时钳制。
pub const MIN_NUMBER_START: u32 = 1;
pub const MAX_NUMBER_START: u32 = 999;

/// 标注样式默认值：color 为 #hex；width/text_size 为 None 时沿用现有自动推导；
/// number_start 为序号工具第一次放置的编号，连续放置自动递增并跨会话保留。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnnotationDefaults {
    pub color: String,
    pub width: Option<f64>,
    pub text_size: Option<f64>,
    pub number_start: u32,
}

impl Default for AnnotationDefaults {
    fn default() -> Self {
        Self {
            color: DEFAULT_COLOR.into(),
            width: None,
            text_size: None,
            number_start: MIN_NUMBER_START,
        }
    }
}

impl AnnotationDefaults {
    /// 非法颜色回退默认色，非正/非有限数值回退 None（自动推导），
    /// 起始序号钳制到 1–999。
    pub fn sanitized(self) -> Self {
        let color = if parse_hex_color(&self.color).is_some() {
            self.color
        } else {
            DEFAULT_COLOR.into()
        };
        let width = self
            .width
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value.min(20.0));
        let text_size = self
            .text_size
            .filter(|value| value.is_finite() && *value >= 8.0)
            .map(|value| value.min(96.0));
        Self {
            color,
            width,
            text_size,
            number_start: self.number_start.clamp(MIN_NUMBER_START, MAX_NUMBER_START),
        }
    }
}

/// 功能开关(R19):每个新增能力一个独立开关,默认按精选表;关闭只停用入口与
/// 新增行为,不删除既有数据。旧 `FeatureSettings` 的 10 项遗留开关(入口/工具栏
/// /光标提示/上次区域/方向纠正/即时标注)已按常开语义移除,其能力保持开启,
/// 旧配置中的 `features` 键不再反序列化(未知字段被忽略)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FeatureToggles {
    pub long_capture: bool,
    pub pin_enhance: bool,
    pub pin_restore: bool,
    pub export_beautify: bool,
    pub capture_cursor: bool,
    pub history_tools: bool,
    pub clipboard_pin: bool,
    pub multi_monitor: bool,
    pub ocr_panel: bool,
    pub onboarding: bool,
    pub filename_template: bool,
}

impl Default for FeatureToggles {
    /// R19 精选:长截图、贴图增强、历史检索与收藏、剪贴板贴图、多屏全屏、
    /// OCR 结果面板、首次引导默认开启;导出美化、捕获光标、贴图重启恢复、
    /// 文件名模板默认关闭。
    fn default() -> Self {
        Self {
            long_capture: true,
            pin_enhance: true,
            pin_restore: false,
            export_beautify: false,
            capture_cursor: false,
            history_tools: true,
            clipboard_pin: true,
            multi_monitor: true,
            ocr_panel: true,
            onboarding: true,
            filename_template: false,
        }
    }
}

impl FeatureToggles {
    /// 按键名设置单个开关(camelCase 优先,兼容 snake_case);未知键返回 None。
    pub fn with_key(self, key: &str, enabled: bool) -> Option<Self> {
        let mut next = self;
        match key {
            "longCapture" | "long_capture" => next.long_capture = enabled,
            "pinEnhance" | "pin_enhance" => next.pin_enhance = enabled,
            "pinRestore" | "pin_restore" => next.pin_restore = enabled,
            "exportBeautify" | "export_beautify" => next.export_beautify = enabled,
            "captureCursor" | "capture_cursor" => next.capture_cursor = enabled,
            "historyTools" | "history_tools" => next.history_tools = enabled,
            "clipboardPin" | "clipboard_pin" => next.clipboard_pin = enabled,
            "multiMonitor" | "multi_monitor" => next.multi_monitor = enabled,
            "ocrPanel" | "ocr_panel" => next.ocr_panel = enabled,
            "onboarding" => next.onboarding = enabled,
            "filenameTemplate" | "filename_template" => next.filename_template = enabled,
            _ => return None,
        }
        Some(next)
    }
}

/// 标注工具 id 白名单(R19):被合并的 line/pen/blur 不单列,其能力由
/// arrow/highlighter/mosaic 的模式提供。
pub const ANNOTATION_TOOL_IDS: [&str; 12] = [
    "rect",
    "ellipse",
    "arrow",
    "text",
    "number",
    "highlighter",
    "mosaic",
    "spotlight",
    "magnifier",
    "bubble",
    "sticker",
    "erase",
];

/// R19 精选默认:矩形、椭圆、箭头、文本、序号、荧光笔、马赛克开启;
/// 聚光灯、放大镜、对话气泡、贴纸、内容擦除关闭。
pub fn default_annotation_tools() -> BTreeMap<String, bool> {
    let enabled = [
        "rect",
        "ellipse",
        "arrow",
        "text",
        "number",
        "highlighter",
        "mosaic",
    ];
    ANNOTATION_TOOL_IDS
        .into_iter()
        .map(|id| (id.to_string(), enabled.contains(&id)))
        .collect()
}

/// 旧配置升级:只保留白名单键的已存值,其余回到精选默认;被合并工具不进入
/// 开关表,未知键忽略。
pub fn sanitize_annotation_tools(stored: BTreeMap<String, bool>) -> BTreeMap<String, bool> {
    let mut tools = default_annotation_tools();
    for (key, value) in stored {
        if let Some(slot) = tools.get_mut(&key) {
            *slot = value;
        }
    }
    tools
}

/// 按键设置单个标注工具开关;未知键返回 None。
pub fn with_annotation_tool(
    mut tools: BTreeMap<String, bool>,
    tool: &str,
    enabled: bool,
) -> Option<BTreeMap<String, bool>> {
    tools.get_mut(tool)?;
    tools.insert(tool.to_string(), enabled);
    Some(tools)
}

/// 延时合法上限(秒)。
pub const MAX_DELAY_SECONDS: u32 = 60;

/// 截取延时。完成后去向由浮层工作区决定,不再保存「完成后动作」或「自动复制」。
/// 旧配置里的 `autoCopy` / `finishAction` 会被 serde 忽略,也不会再写回。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureSettings {
    pub delay_seconds: u32,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self { delay_seconds: 0 }
    }
}

impl CaptureSettings {
    /// 延时钳制到 0–60。
    pub fn sanitized(self) -> Self {
        Self {
            delay_seconds: self.delay_seconds.min(MAX_DELAY_SECONDS),
        }
    }

    pub fn delay_ms(&self) -> u64 {
        u64::from(self.delay_seconds) * 1000
    }
}

/// 历史记录上限允许范围(条):低于 5 会被钳制,高于 200 会被钳制。
pub const MIN_HISTORY_LIMIT: u32 = 5;
pub const MAX_HISTORY_LIMIT: u32 = 200;

/// 本地截图历史(R2):关闭后不新增记录,已有关闭前记录保留;
/// limit 为保留条数上限,超出时按时间淘汰最旧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistorySettings {
    pub enabled: bool,
    pub limit: u32,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            limit: 20,
        }
    }
}

impl HistorySettings {
    /// limit 钳制到 5–200;布尔无非法值。
    pub fn sanitized(self) -> Self {
        Self {
            enabled: self.enabled,
            limit: self.limit.clamp(MIN_HISTORY_LIMIT, MAX_HISTORY_LIMIT),
        }
    }
}

/// 导出格式(R3):保存对话框中以用户输入的扩展名推导;缺失或未知时
/// 回退 `last_format`。PNG 为默认格式且保持无损。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
}

impl ExportFormat {
    /// 规范扩展名:无/未知扩展名时补全,不保留 jpeg 等别名。
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    /// `jpg`/`jpeg` 一律视为 JPEG;大小写不敏感;未知扩展名返回 None。
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Webp => "WebP",
        }
    }
}

/// 导出质量档位(R3):三档,仅 JPEG/WebP 生效,PNG 忽略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportQuality {
    #[default]
    High,
    Medium,
    Low,
}

impl ExportQuality {
    /// 编码器质量值(1–100):高/中/低三档须可观察到文件大小差异。
    pub fn value(self) -> u8 {
        match self {
            Self::High => 90,
            Self::Medium => 75,
            Self::Low => 55,
        }
    }
}

/// 导出记忆(R3/R11):上次格式、目录、质量档位,以及美化参数与文件名模板。
/// 模板默认空;是否套用由 `filename_template` 开关决定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExportSettings {
    pub last_format: ExportFormat,
    pub last_dir: Option<String>,
    pub quality: ExportQuality,
    #[serde(default)]
    pub beautify: BeautifyOptions,
    #[serde(default)]
    pub filename_template: String,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            last_format: ExportFormat::Png,
            last_dir: None,
            quality: ExportQuality::High,
            beautify: BeautifyOptions::default(),
            filename_template: String::new(),
        }
    }
}

impl ExportSettings {
    /// 空白目录按未记录处理;格式/档位由枚举解析保证合法。
    /// 美化参数与模板按各自上限清洗,不改上次格式与目录。
    pub fn sanitized(self) -> Self {
        Self {
            last_format: self.last_format,
            last_dir: self.last_dir.filter(|dir| !dir.trim().is_empty()),
            quality: self.quality,
            beautify: self.beautify.sanitized(),
            filename_template: crate::filename_template::sanitize_template(&self.filename_template),
        }
    }

    /// 对话框起始目录:目录已不存在时忽略记忆,避免弹窗定位失败。
    pub fn existing_directory(&self) -> Option<&std::path::Path> {
        let dir = self.last_dir.as_deref()?;
        let path = std::path::Path::new(dir);
        path.is_dir().then_some(path)
    }
}

/// 上次区域(R6):成功完成区域截图后记住的全局物理像素矩形(多显示器桌面允许负坐标),
/// 托盘的"上次区域"用它直取,不再进入交互选区。分辨率/缩放/显示器变化后使用时
/// 按当前显示器并集钳制,保证裁剪始终落在实际抓取的显示器帧内。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl LastRegion {
    /// 宽高为 0 的记录不是有效区域(其余字段天然合法)。
    pub fn sanitized(self) -> Option<Self> {
        (self.width > 0 && self.height > 0).then_some(self)
    }

    pub fn right(&self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    pub fn bottom(&self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    /// 与另一矩形求交;无交集(或任一矩形为空)返回 None。
    pub fn intersection(&self, other: &LastRegion) -> Option<LastRegion> {
        let left = i64::from(self.x).max(i64::from(other.x));
        let top = i64::from(self.y).max(i64::from(other.y));
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= left || bottom <= top {
            return None;
        }
        Some(LastRegion {
            x: left as i32,
            y: top as i32,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        })
    }

    /// 按当前显示器矩形列表钳制:选重叠面积最大的显示器,把区域裁剪进该显示器。
    /// 返回 `(显示器索引, 钳制后的区域)`;与所有显示器都无交集时返回 None,
    /// 调用方应提示并回到"暂无记录"而不是硬裁/越界。
    /// 显示器分辨率变大时保持原尺寸(不放大),变小或移位时只裁剪重叠部分。
    pub fn clamp_to_monitors(&self, monitors: &[LastRegion]) -> Option<(usize, LastRegion)> {
        let region = self.sanitized()?;
        let mut best: Option<(usize, u64)> = None;
        for (index, monitor) in monitors.iter().enumerate() {
            let Some(overlap) = region.intersection(monitor) else {
                continue;
            };
            let area = u64::from(overlap.width) * u64::from(overlap.height);
            let better = match best {
                Some((_, best_area)) => area > best_area,
                None => true,
            };
            if better {
                best = Some((index, area));
            }
        }
        let (index, _) = best?;
        let overlap = region.intersection(&monitors[index])?;
        Some((index, overlap))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSettings {
    #[serde(default)]
    pub hotkeys: Hotkeys,
    #[serde(default)]
    pub annotation_defaults: AnnotationDefaults,
    /// R19 功能开关;旧 `features` 键不再反序列化(未知字段被忽略)。
    #[serde(default)]
    pub toggles: FeatureToggles,
    /// R19 标注工具逐项开关(工具 id → 是否启用)。
    #[serde(default)]
    pub annotation_tools: BTreeMap<String, bool>,
    #[serde(default)]
    pub capture: CaptureSettings,
    #[serde(default)]
    pub history: HistorySettings,
    #[serde(default)]
    pub export: ExportSettings,
    #[serde(default)]
    pub last_region: Option<LastRegion>,
    /// 界面语言(R12):`system | zh-CN | en`;未知值按 system 处理。
    #[serde(default = "default_language_setting")]
    pub language: String,
    /// 首次引导窗口已关闭。默认 false;关闭引导后写 true,之后不再自动打开。
    /// 旧配置缺少该字段时按未完成处理,开关开启则会自动出现一次。
    #[serde(default)]
    pub onboarding_done: bool,
}

pub fn default_language_setting() -> String {
    i18n::SYSTEM_LANGUAGE.to_string()
}

/// 语言设置值的合法化:只接受三种取值,其余(含空值)回退 system。
pub fn sanitize_language(value: &str) -> String {
    match value.trim() {
        "zh-CN" => "zh-CN".to_string(),
        "en" => "en".to_string(),
        _ => i18n::SYSTEM_LANGUAGE.to_string(),
    }
}

/// 设置值 + 解析结果:前端据此显示选项并作为切换后的重渲染输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageInfo {
    pub language: String,
    pub resolved_language: Language,
}

/// R16:托盘可用性状态。托盘构建失败(Linux 桌面缺少 AppIndicator 等)时应用
/// 继续运行,设置窗口据此展示无托盘提示并提供退出入口;状态不持久化,每次
/// 启动以实际构建结果为准。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrayState {
    pub available: bool,
    pub message: Option<String>,
}

impl TrayState {
    pub fn available() -> Self {
        Self {
            available: true,
            message: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettings {
    pub hotkeys: Hotkeys,
    pub hotkey_errors: HotkeyErrors,
    pub autostart: AutostartState,
    pub notice: Option<String>,
    pub annotation_defaults: AnnotationDefaults,
    /// R19 功能开关与标注工具逐项开关。
    pub toggles: FeatureToggles,
    pub annotation_tools: BTreeMap<String, bool>,
    pub capture: CaptureSettings,
    pub history: HistorySettings,
    pub export: ExportSettings,
    pub tray: TrayState,
    pub language: String,
    pub resolved_language: Language,
}

pub struct SessionState {
    pub hotkeys: Mutex<Hotkeys>,
    pub hotkey_errors: Mutex<HotkeyErrors>,
    pub notice: Mutex<Option<String>>,
    pub autostart_rejection: Mutex<Option<AutostartRejection>>,
    pub annotation_defaults: Mutex<AnnotationDefaults>,
    pub toggles: Mutex<FeatureToggles>,
    pub annotation_tools: Mutex<BTreeMap<String, bool>>,
    pub capture: Mutex<CaptureSettings>,
    pub history: Mutex<HistorySettings>,
    pub export: Mutex<ExportSettings>,
    pub last_region: Mutex<Option<LastRegion>>,
    pub tray: Mutex<TrayState>,
    pub language: Mutex<String>,
    pub onboarding_done: Mutex<bool>,
}

impl SessionState {
    pub fn from_stored(stored: StoredSettings) -> Self {
        Self {
            hotkeys: Mutex::new(stored.hotkeys),
            hotkey_errors: Mutex::new(HotkeyErrors::default()),
            notice: Mutex::new(None),
            autostart_rejection: Mutex::new(None),
            annotation_defaults: Mutex::new(stored.annotation_defaults.sanitized()),
            toggles: Mutex::new(stored.toggles),
            annotation_tools: Mutex::new(sanitize_annotation_tools(stored.annotation_tools)),
            capture: Mutex::new(stored.capture.sanitized()),
            history: Mutex::new(stored.history.sanitized()),
            export: Mutex::new(stored.export.sanitized()),
            last_region: Mutex::new(stored.last_region.and_then(LastRegion::sanitized)),
            tray: Mutex::new(TrayState::available()),
            language: Mutex::new(sanitize_language(&stored.language)),
            onboarding_done: Mutex::new(stored.onboarding_done),
        }
    }
}

/// 供各功能入口读取当前功能开关(R19);内存值即时生效,随设置写盘持久化。
pub fn current_toggles(app: &AppHandle) -> FeatureToggles {
    *lock(&app.state::<SessionState>().toggles)
}

/// 供标注工具入口读取当前逐项开关(R19);关闭只隐藏创建入口,已创建标注
/// 的渲染、编辑与导出不受影响。
pub fn current_annotation_tools(app: &AppHandle) -> BTreeMap<String, bool> {
    lock(&app.state::<SessionState>().annotation_tools).clone()
}

/// 供选区即时标注(R21)读取当前标注样式默认值;只读克隆,不做平台查询,
/// 不进入启动路径。
pub fn current_annotation_defaults(app: &AppHandle) -> AnnotationDefaults {
    lock(&app.state::<SessionState>().annotation_defaults).clone()
}

/// 供截取链路读取延时。设置变更下一次截取即生效,无需重启。
pub fn current_capture(app: &AppHandle) -> CaptureSettings {
    *lock(&app.state::<SessionState>().capture)
}

/// 供完成路径判断是否写入历史记录(内存值即时生效)。
pub fn current_history(app: &AppHandle) -> HistorySettings {
    *lock(&app.state::<SessionState>().history)
}

/// 供导出路径(预览保存/静默保存)读取上次格式、目录与质量档位。
pub fn current_export(app: &AppHandle) -> ExportSettings {
    lock(&app.state::<SessionState>().export).clone()
}

/// 保存成功后更新导出记忆:扩展名推导出的实际格式、目标目录与本次档位,
/// 写盘失败只影响 notice,不影响已完成的文件写入。
pub fn remember_export(
    app: &AppHandle,
    format: ExportFormat,
    quality: ExportQuality,
    directory: Option<&std::path::Path>,
) {
    let mut next = current_export(app);
    next.last_format = format;
    next.last_dir = directory.map(|dir| dir.to_string_lossy().into_owned());
    next.quality = quality;
    let next = next.sanitized();
    *lock(&app.state::<SessionState>().export) = next;
    let applied = i18n::t("notice.export_remembered");
    persist_settings(app, &applied);
}

/// 供托盘读取"上次区域"是否存在:决定菜单项可用状态与标签。
pub fn current_last_region(app: &AppHandle) -> Option<LastRegion> {
    *lock(&app.state::<SessionState>().last_region)
}

/// 区域截图成功后覆盖记录并重建托盘菜单(R6),"上次区域"立即直取同一区域;
/// 写盘失败只影响 notice,不丢内存记录。R19:旧 lastRegion 开关按常开语义
/// 移除,记录行为保持开启。
pub fn remember_last_region(app: &AppHandle, region: LastRegion) {
    let Some(region) = region.sanitized() else {
        return;
    };
    *lock(&app.state::<SessionState>().last_region) = Some(region);
    let applied = i18n::t("notice.last_region_saved");
    persist_settings(app, &applied);
    crate::tray::refresh_menu(app);
}

/// 使用时发现记录与当前显示环境无有效交集:清除记录并重建菜单,
/// 让"上次区域"回到禁用+提示状态(缺记录时调用为无操作)。
pub fn forget_last_region(app: &AppHandle) {
    let state = app.state::<SessionState>();
    if lock(&state.last_region).take().is_none() {
        return;
    }
    let applied = i18n::t("notice.last_region_cleared");
    persist_settings(app, &applied);
    crate::tray::refresh_menu(app);
}

/// R16:托盘构建失败后标记为无托盘运行。设置窗口据此常驻展示提示与退出入口;
/// 热键、截取与预览链路保持原样,后续托盘菜单重建自动跳过。
pub fn mark_tray_unavailable(app: &AppHandle, message: String) {
    *lock(&app.state::<SessionState>().tray) = TrayState {
        available: false,
        message: Some(message),
    };
}

pub fn load_from_app(app: &AppHandle) -> StoredSettings {
    load_from_path(&settings_path(app))
}

pub fn load_from_path(path: &std::path::Path) -> StoredSettings {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => StoredSettings::default(),
    }
}

pub fn save_to_path(path: &std::path::Path, settings: &StoredSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    fs::write(path, text).map_err(|error| error.to_string())
}

pub fn open_settings(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("settings") {
        crate::front::reveal(app, &window);
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        app,
        "settings",
        WebviewUrl::App("index.html?view=settings".into()),
    )
    .title("Cropmark")
    .inner_size(420.0, 560.0)
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

/// 全新配置且「首次使用引导」开启时自动打开一次。关闭窗口后 `done` 为 true。
pub fn should_auto_open_onboarding(enabled: bool, done: bool) -> bool {
    enabled && !done
}

/// 当前会话是否还应在托盘安装成功后自动打开引导。
pub fn onboarding_pending(app: &AppHandle) -> bool {
    let state = app.state::<SessionState>();
    let enabled = lock(&state.toggles).onboarding;
    let done = *lock(&state.onboarding_done);
    should_auto_open_onboarding(enabled, done)
}

/// 打开(或唤出)引导窗口。按需创建,关闭即销毁,不预建、不常驻。
/// 设置里的「使用帮助」与首次自动打开共用这一扇窗口。
pub fn open_guide_window(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(crate::front::GUIDE) {
        crate::front::reveal(app, &window);
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        app,
        crate::front::GUIDE,
        WebviewUrl::App("index.html?view=guide".into()),
    )
    .title("Cropmark")
    .inner_size(440.0, 640.0)
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

/// 从设置页重开引导。async 与 `open_history` 同因:窗口构建要泵平台消息,
/// 同步 command 会占住主线程。
#[tauri::command]
pub async fn open_guide(app: AppHandle) -> Result<(), String> {
    open_guide_window(&app)
}

/// 引导窗口关闭(包括中途关闭)后记为已完成。已完成则不再写盘。
/// 写盘失败时回退内存标记,下次启动仍会自动打开;不清除设置页已有提示。
pub fn complete_onboarding(app: &AppHandle) {
    let state = app.state::<SessionState>();
    {
        let mut done = lock(&state.onboarding_done);
        if *done {
            return;
        }
        *done = true;
    }
    let stored = stored_from_state(&state);
    if let Err(error) = save_to_path(&settings_path(app), &stored) {
        *lock(&state.onboarding_done) = false;
        *lock(&state.notice) = Some(i18n::tp(
            "notice.persist_failed",
            &[
                ("applied", &i18n::t("notice.onboarding_applied")),
                ("error", &error),
            ],
        ));
    }
}

/// 供启动与语言切换调用:读取设置值、解析系统语言并写入进程级当前语言。
pub fn apply_language(app: &AppHandle) -> Language {
    let setting = lock(&app.state::<SessionState>().language).clone();
    let resolved = i18n::resolve_setting(&setting);
    i18n::set_language(resolved);
    resolved
}

/// 语言设置值 + 解析结果;各 webview 启动时据此初始化词条语言。
#[tauri::command]
pub fn get_language(app: AppHandle) -> LanguageInfo {
    let state = app.state::<SessionState>();
    let language = lock(&state.language).clone();
    LanguageInfo {
        resolved_language: i18n::resolve_setting(&language),
        language,
    }
}

/// 切换界面语言(R12):写设置并持久化,更新进程级语言,重建托盘菜单,
/// 广播 `language-changed` 让所有已打开窗口即时重渲染;无需重启。
#[tauri::command]
pub fn set_language(app: AppHandle, language: String) -> UiSettings {
    let sanitized = sanitize_language(&language);
    *lock(&app.state::<SessionState>().language) = sanitized;
    // 先切换进程级语言,持久化提示与托盘菜单再按新语言生成。
    apply_language(&app);
    let applied = i18n::t("notice.language_applied");
    persist_settings(&app, &applied);
    // 无托盘提示按新语言刷新(提示在启动时生成,语言切换后不能残留旧语言)。
    {
        let state = app.state::<SessionState>();
        let mut tray = lock(&state.tray);
        if !tray.available {
            tray.message = Some(crate::tray::unavailable_message());
        }
    }
    crate::tray::refresh_menu(&app);
    let info = get_language(app.clone());
    let _ = tauri::Emitter::emit(&app, "language-changed", info);
    snapshot(&app)
}

pub fn snapshot(app: &AppHandle) -> UiSettings {
    let state = app.state::<SessionState>();
    let hotkeys = lock(&state.hotkeys).clone();
    // 热键错误按当前语言解析(存储为词条键,语言切换后不残留旧语言)。
    let hotkey_errors = lock(&state.hotkey_errors).clone().localized();
    let notice = lock(&state.notice).clone();
    let autostart_rejection = lock(&state.autostart_rejection).clone();
    let annotation_defaults = lock(&state.annotation_defaults).clone();
    // 开关表统一走读取入口,后续功能任务的宿主入口复用同一路径。
    let toggles = current_toggles(app);
    let annotation_tools = current_annotation_tools(app);
    let capture = *lock(&state.capture);
    let history = *lock(&state.history);
    let export = lock(&state.export).clone();
    let tray = lock(&state.tray).clone();
    let language = lock(&state.language).clone();
    let resolved_language = i18n::resolve_setting(&language);
    UiSettings {
        hotkeys,
        hotkey_errors,
        autostart: autostart::merge_autostart_ui(autostart::current_state(), autostart_rejection),
        notice,
        annotation_defaults,
        toggles,
        annotation_tools,
        capture,
        history,
        export,
        tray,
        language,
        resolved_language,
    }
}

#[tauri::command]
pub fn get_ui_settings(app: AppHandle) -> UiSettings {
    snapshot(&app)
}

#[tauri::command]
pub fn set_hotkey(app: AppHandle, mode: CaptureMode, accelerator: String) -> UiSettings {
    let mut hotkeys = lock(&app.state::<SessionState>().hotkeys).clone();
    hotkeys.set(mode, accelerator);
    let applied = i18n::t("notice.hotkey_applied");
    persist_settings(&app, &applied);
    hotkeys::apply_to_app(&app, &hotkeys);
    snapshot(&app)
}

/// R8:设置/清除剪贴板贴图全局快捷键(空串清除绑定)。默认未绑定,未绑定时
/// 不注册也不报错;非法/冲突组合与既有热键一致地保存并展示可见错误。
#[tauri::command]
pub fn set_pin_clipboard_hotkey(app: AppHandle, accelerator: String) -> UiSettings {
    let mut hotkeys = lock(&app.state::<SessionState>().hotkeys).clone();
    hotkeys.set_pin_clipboard(accelerator);
    let applied = i18n::t("notice.hotkey_applied");
    persist_settings(&app, &applied);
    hotkeys::apply_to_app(&app, &hotkeys);
    snapshot(&app)
}

#[tauri::command]
pub fn set_autostart_enabled(app: AppHandle, enabled: bool) -> UiSettings {
    let result = autostart::set_enabled(enabled);
    *lock(&app.state::<SessionState>().autostart_rejection) =
        autostart::remember_autostart_result(&result);
    let mut ui = snapshot(&app);
    ui.autostart = result;
    ui
}

#[tauri::command]
pub fn set_annotation_defaults(app: AppHandle, defaults: AnnotationDefaults) -> UiSettings {
    *lock(&app.state::<SessionState>().annotation_defaults) = defaults.sanitized();
    let applied = i18n::t("notice.style_applied");
    persist_settings(&app, &applied);
    snapshot(&app)
}

/// 设置单个功能开关(R19):只接受新键白名单,未知键返回错误;内存值立即
/// 生效,随 persist_settings 写盘,写盘失败沿 notice 提示。功能入口挂接在
/// 托盘/选区等宿主上,菜单按 id 分发,重建不丢处理器。
#[tauri::command]
pub fn set_feature(app: AppHandle, key: String, enabled: bool) -> Result<UiSettings, String> {
    let next = {
        let state = app.state::<SessionState>();
        let current = *lock(&state.toggles);
        current
            .with_key(&key, enabled)
            .ok_or_else(|| i18n::tp("error.feature.unknown", &[("key", &key)]))?
    };
    *lock(&app.state::<SessionState>().toggles) = next;
    let applied = i18n::t("notice.features_applied");
    persist_settings(&app, &applied);
    // 功能入口挂在托盘/选区等宿主上;菜单事件按 id 分发,重建不丢处理器。
    crate::tray::refresh_menu(&app);
    if matches!(key.as_str(), "exportBeautify" | "export_beautify") {
        emit_export_appearance(&app);
    }
    Ok(snapshot(&app))
}

/// 美化参数与文件名模板(R3/R11)。不改上次格式、目录与质量档位。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportAppearance {
    pub beautify: BeautifyOptions,
    pub filename_template: String,
}

#[tauri::command]
pub fn set_export_appearance(app: AppHandle, appearance: ExportAppearance) -> UiSettings {
    {
        let state = app.state::<SessionState>();
        let mut export = lock(&state.export).clone();
        export.beautify = appearance.beautify;
        export.filename_template = appearance.filename_template;
        *lock(&state.export) = export.sanitized();
    }
    let applied = i18n::t("notice.export_remembered");
    persist_settings(&app, &applied);
    emit_export_appearance(&app);
    snapshot(&app)
}

fn emit_export_appearance(app: &AppHandle) {
    let _ = app.emit("export-appearance-changed", ());
}

/// 设置单个标注工具开关(R19):只控制创建入口;未知工具返回错误。渲染、
/// 复制、保存、贴图、取字与撤销/重做保持常驻,不随开关变化。
#[tauri::command]
pub fn set_annotation_tool(
    app: AppHandle,
    tool: String,
    enabled: bool,
) -> Result<UiSettings, String> {
    let next = {
        let state = app.state::<SessionState>();
        let current = lock(&state.annotation_tools).clone();
        with_annotation_tool(current, &tool, enabled)
            .ok_or_else(|| i18n::tp("error.annotation_tool.unknown", &[("tool", &tool)]))?
    };
    *lock(&app.state::<SessionState>().annotation_tools) = next;
    let applied = i18n::t("notice.annotation_tools_applied");
    persist_settings(&app, &applied);
    Ok(snapshot(&app))
}

/// 设置延时;内存值立即生效(下一次截取起),随 persist_settings 写盘,
/// 并按新延时重建托盘菜单标签。不写回已删除的完成动作与自动复制。
#[tauri::command]
pub fn set_capture_settings(app: AppHandle, settings: CaptureSettings) -> UiSettings {
    *lock(&app.state::<SessionState>().capture) = settings.sanitized();
    let applied = i18n::t("notice.capture_applied");
    persist_settings(&app, &applied);
    crate::tray::refresh_menu(&app);
    snapshot(&app)
}

/// 设置历史开关与上限;上限调低时异步裁剪最旧记录,不阻塞设置窗口。
#[tauri::command]
pub fn set_history_settings(app: AppHandle, settings: HistorySettings) -> UiSettings {
    let next = settings.sanitized();
    *lock(&app.state::<SessionState>().history) = next;
    let applied = i18n::t("notice.history_applied");
    persist_settings(&app, &applied);
    crate::history::prune_async(&app, next.limit);
    snapshot(&app)
}

fn stored_from_state(state: &SessionState) -> StoredSettings {
    StoredSettings {
        hotkeys: lock(&state.hotkeys).clone(),
        annotation_defaults: lock(&state.annotation_defaults).clone(),
        toggles: *lock(&state.toggles),
        annotation_tools: lock(&state.annotation_tools).clone(),
        capture: *lock(&state.capture),
        history: *lock(&state.history),
        export: lock(&state.export).clone(),
        last_region: *lock(&state.last_region),
        language: lock(&state.language).clone(),
        onboarding_done: *lock(&state.onboarding_done),
    }
}

fn persist_settings(app: &AppHandle, applied: &str) {
    let state = app.state::<SessionState>();
    let stored = stored_from_state(&state);
    match save_to_path(&settings_path(app), &stored) {
        Ok(()) => *lock(&state.notice) = None,
        Err(error) => {
            *lock(&state.notice) = Some(i18n::tp(
                "notice.persist_failed",
                &[("applied", applied), ("error", &error)],
            ));
        }
    }
}

fn settings_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("settings.json")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn missing_file_loads_default_hotkeys_and_not_autostart() {
        let dir = std::env::temp_dir().join(format!("cropmark-settings-{}", std::process::id()));
        let path = dir.join("missing.json");
        let _ = fs::remove_file(&path);
        let loaded = load_from_path(&path);
        assert_eq!(loaded.hotkeys, Hotkeys::default());
    }

    #[test]
    fn roundtrip_hotkeys_without_storing_autostart_enabled() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-save-{}", std::process::id()));
        let path = dir.join("settings.json");
        let stored = StoredSettings {
            hotkeys: Hotkeys {
                region: "Ctrl+Alt+R".into(),
                window: "Alt+Shift+W".into(),
                fullscreen: "Alt+Shift+S".into(),
                pin_clipboard: "Ctrl+Alt+P".into(),
            },
            annotation_defaults: AnnotationDefaults {
                color: "#2563eb".into(),
                width: Some(5.0),
                text_size: Some(22.0),
                number_start: 5,
            },
            toggles: FeatureToggles {
                long_capture: false,
                pin_enhance: true,
                pin_restore: true,
                export_beautify: true,
                capture_cursor: true,
                history_tools: false,
                clipboard_pin: true,
                multi_monitor: false,
                ocr_panel: true,
                onboarding: false,
                filename_template: true,
            },
            annotation_tools: BTreeMap::from([
                ("rect".to_string(), false),
                ("spotlight".to_string(), true),
            ]),
            capture: CaptureSettings { delay_seconds: 5 },
            history: HistorySettings {
                enabled: false,
                limit: 50,
            },
            export: ExportSettings {
                last_format: ExportFormat::Jpeg,
                last_dir: Some("C:/shots".into()),
                quality: ExportQuality::Low,
                beautify: BeautifyOptions {
                    preset: "ink".into(),
                    padding: 12,
                    radius: 6,
                    shadow: false,
                },
                filename_template: "shot_{mode}_{seq}".into(),
            },
            last_region: Some(LastRegion {
                x: -640,
                y: 120,
                width: 320,
                height: 200,
            }),
            language: "en".into(),
            onboarding_done: true,
        };
        save_to_path(&path, &stored).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("autostart"));
        assert!(!text.contains("autoCopy"));
        assert!(!text.contains("finishAction"));
        // R19:旧 features 键不再写入;新开关与工具表按新键名持久化。
        assert!(!text.contains("\"features\""));
        assert!(text.contains("\"toggles\""));
        assert!(text.contains("\"annotationTools\""));
        let loaded = load_from_path(&path);
        assert_eq!(loaded.hotkeys.region, "Ctrl+Alt+R");
        assert_eq!(loaded.hotkeys.pin_clipboard, "Ctrl+Alt+P");
        assert_eq!(loaded.annotation_defaults.color, "#2563eb");
        assert_eq!(loaded.annotation_defaults.width, Some(5.0));
        assert_eq!(loaded.annotation_defaults.text_size, Some(22.0));
        assert_eq!(loaded.annotation_defaults.number_start, 5);
        assert!(!loaded.toggles.long_capture);
        assert!(loaded.toggles.pin_enhance);
        assert!(loaded.toggles.pin_restore);
        assert!(loaded.toggles.export_beautify);
        assert!(loaded.toggles.capture_cursor);
        assert!(!loaded.toggles.history_tools);
        assert!(loaded.toggles.clipboard_pin);
        assert!(!loaded.toggles.multi_monitor);
        assert!(loaded.toggles.ocr_panel);
        assert!(!loaded.toggles.onboarding);
        assert!(loaded.toggles.filename_template);
        assert_eq!(loaded.annotation_tools.get("rect"), Some(&false));
        assert_eq!(loaded.annotation_tools.get("spotlight"), Some(&true));
        assert_eq!(loaded.capture.delay_seconds, 5);
        assert!(!loaded.history.enabled);
        assert_eq!(loaded.history.limit, 50);
        assert_eq!(loaded.export.last_format, ExportFormat::Jpeg);
        assert_eq!(loaded.export.last_dir.as_deref(), Some("C:/shots"));
        assert_eq!(loaded.export.quality, ExportQuality::Low);
        assert_eq!(loaded.export.beautify.preset, "ink");
        assert_eq!(loaded.export.beautify.padding, 12);
        assert!(!loaded.export.beautify.shadow);
        assert_eq!(loaded.export.filename_template, "shot_{mode}_{seq}");
        assert_eq!(loaded.language, "en");
        assert!(loaded.onboarding_done);
        assert!(text.contains("\"onboardingDone\""));
        assert_eq!(
            loaded.last_region,
            Some(LastRegion {
                x: -640,
                y: 120,
                width: 320,
                height: 200
            })
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_hotkeys_without_clipboard_pin_keep_the_rest_intact() {
        let parsed: StoredSettings = serde_json::from_str(
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.hotkeys.region, "Ctrl+Alt+R");
        assert!(parsed.hotkeys.pin_clipboard.is_empty());
        assert_eq!(parsed.toggles, FeatureToggles::default());
    }

    #[test]
    fn missing_annotation_defaults_field_loads_current_defaults() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-style-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.annotation_defaults, AnnotationDefaults::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_annotation_defaults_are_sanitized() {
        let dirty = AnnotationDefaults {
            color: "rose".into(),
            width: Some(-3.0),
            text_size: Some(4.0),
            number_start: 0,
        };
        let clean = dirty.sanitized();
        assert_eq!(clean.color, crate::annotate::DEFAULT_COLOR);
        assert_eq!(clean.width, None);
        assert_eq!(clean.text_size, None);
        assert_eq!(clean.number_start, MIN_NUMBER_START);
    }

    #[test]
    fn annotation_defaults_default_number_start_and_clamp_range() {
        assert_eq!(AnnotationDefaults::default().number_start, 1);
        let low = AnnotationDefaults {
            number_start: 0,
            ..AnnotationDefaults::default()
        }
        .sanitized();
        assert_eq!(low.number_start, MIN_NUMBER_START);
        let high = AnnotationDefaults {
            number_start: 100_000,
            ..AnnotationDefaults::default()
        }
        .sanitized();
        assert_eq!(high.number_start, MAX_NUMBER_START);
        let exact = AnnotationDefaults {
            number_start: MAX_NUMBER_START,
            ..AnnotationDefaults::default()
        }
        .sanitized();
        assert_eq!(exact.number_start, MAX_NUMBER_START);
    }

    #[test]
    fn legacy_annotation_defaults_json_defaults_number_start() {
        let parsed: StoredSettings = serde_json::from_str(
            r##"{"annotationDefaults":{"color":"#2563eb","width":5.0,"textSize":22.0}}"##,
        )
        .unwrap();
        assert_eq!(parsed.annotation_defaults.number_start, 1);
        assert_eq!(parsed.annotation_defaults.color, "#2563eb");
        let serialized = serde_json::to_value(parsed.annotation_defaults).unwrap();
        assert_eq!(serialized["numberStart"], 1);
    }

    #[test]
    fn missing_toggles_field_loads_selected_defaults() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-toggles-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.toggles, FeatureToggles::default());
        assert_eq!(loaded.annotation_tools, BTreeMap::new());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn feature_toggle_defaults_follow_the_selected_table() {
        let defaults = FeatureToggles::default();
        assert!(defaults.long_capture);
        assert!(defaults.pin_enhance);
        assert!(!defaults.pin_restore);
        assert!(!defaults.export_beautify);
        assert!(!defaults.capture_cursor);
        assert!(defaults.history_tools);
        assert!(defaults.clipboard_pin);
        assert!(defaults.multi_monitor);
        assert!(defaults.ocr_panel);
        assert!(defaults.onboarding);
        assert!(!defaults.filename_template);
    }

    #[test]
    fn legacy_feature_settings_are_ignored_on_load() {
        let parsed: StoredSettings = serde_json::from_str(
            r#"{"features":{"ocrEntry":false,"pinEntry":false,"magnifier":false,"toolbarCopy":false,"toolbarSave":false,"toolbarPin":false,"cursorHints":false,"lastRegion":false,"ocrOrientation":false,"inlineAnnotation":false}}"#,
        )
        .unwrap();
        assert_eq!(parsed.toggles, FeatureToggles::default());
        assert_eq!(parsed.annotation_tools, BTreeMap::new());
        // 旧的 10 项开关不再进入持久化结构。
        let serialized = serde_json::to_value(&parsed).unwrap();
        assert!(serialized.get("features").is_none());
        assert!(serialized.get("toggles").is_some());
        assert!(serialized.get("annotationTools").is_some());
    }

    #[test]
    fn with_key_applies_known_toggle_keys_and_rejects_unknown() {
        let base = FeatureToggles::default();
        let cases = [
            (
                "longCapture",
                false,
                FeatureToggles {
                    long_capture: false,
                    ..base
                },
            ),
            (
                "pinEnhance",
                false,
                FeatureToggles {
                    pin_enhance: false,
                    ..base
                },
            ),
            (
                "pinRestore",
                true,
                FeatureToggles {
                    pin_restore: true,
                    ..base
                },
            ),
            (
                "exportBeautify",
                true,
                FeatureToggles {
                    export_beautify: true,
                    ..base
                },
            ),
            (
                "captureCursor",
                true,
                FeatureToggles {
                    capture_cursor: true,
                    ..base
                },
            ),
            (
                "historyTools",
                false,
                FeatureToggles {
                    history_tools: false,
                    ..base
                },
            ),
            (
                "clipboardPin",
                false,
                FeatureToggles {
                    clipboard_pin: false,
                    ..base
                },
            ),
            (
                "multiMonitor",
                false,
                FeatureToggles {
                    multi_monitor: false,
                    ..base
                },
            ),
            (
                "ocrPanel",
                false,
                FeatureToggles {
                    ocr_panel: false,
                    ..base
                },
            ),
            (
                "onboarding",
                false,
                FeatureToggles {
                    onboarding: false,
                    ..base
                },
            ),
            (
                "filenameTemplate",
                true,
                FeatureToggles {
                    filename_template: true,
                    ..base
                },
            ),
        ];
        for (key, enabled, expected) in cases {
            assert_eq!(base.with_key(key, enabled), Some(expected), "{key}");
        }
        // snake_case 同样在白名单内(前端 camelCase,测试/脚本兼容)。
        assert!(!base.with_key("long_capture", false).unwrap().long_capture);
        assert!(!base.with_key("ocr_panel", false).unwrap().ocr_panel);
        assert!(
            base.with_key("filename_template", true)
                .unwrap()
                .filename_template
        );
        // 已移除的旧开关不再是合法键。
        assert_eq!(base.with_key("ocrEntry", false), None);
        assert_eq!(base.with_key("toolbarSave", false), None);
        assert_eq!(base.with_key("cursorHints", false), None);
        assert_eq!(base.with_key("lastRegion", false), None);
        assert_eq!(base.with_key("inlineAnnotation", false), None);
        assert_eq!(base.with_key("", true), None);
    }

    #[test]
    fn annotation_tool_defaults_follow_the_selected_table() {
        let tools = default_annotation_tools();
        assert_eq!(tools.len(), ANNOTATION_TOOL_IDS.len());
        for id in [
            "rect",
            "ellipse",
            "arrow",
            "text",
            "number",
            "highlighter",
            "mosaic",
        ] {
            assert_eq!(tools.get(id), Some(&true), "{id}");
        }
        for id in ["spotlight", "magnifier", "bubble", "sticker", "erase"] {
            assert_eq!(tools.get(id), Some(&false), "{id}");
        }
        // 被合并的 line/pen/blur 不单列为开关。
        for id in ["line", "pen", "blur"] {
            assert!(!tools.contains_key(id), "{id}");
        }
    }

    #[test]
    fn sanitize_annotation_tools_keeps_known_overrides_and_drops_unknown() {
        let stored = BTreeMap::from([
            ("rect".to_string(), false),
            ("spotlight".to_string(), true),
            ("line".to_string(), true),
            ("pen".to_string(), false),
            ("blur".to_string(), false),
            ("unknown".to_string(), true),
        ]);
        let tools = sanitize_annotation_tools(stored);
        assert_eq!(tools.get("rect"), Some(&false));
        assert_eq!(tools.get("spotlight"), Some(&true));
        assert_eq!(tools.get("ellipse"), Some(&true));
        for gone in ["line", "pen", "blur", "unknown"] {
            assert!(!tools.contains_key(gone), "{gone}");
        }
        assert_eq!(tools.len(), ANNOTATION_TOOL_IDS.len());
    }

    #[test]
    fn with_annotation_tool_sets_known_tool_and_rejects_unknown() {
        let base = default_annotation_tools();
        let next = with_annotation_tool(base.clone(), "spotlight", true).expect("known tool");
        assert_eq!(next.get("spotlight"), Some(&true));
        assert_eq!(base.get("spotlight"), Some(&false));
        assert_eq!(with_annotation_tool(base.clone(), "line", true), None);
        assert_eq!(with_annotation_tool(base, "", true), None);
    }

    #[test]
    fn from_stored_fills_missing_annotation_tools_with_defaults() {
        let state = SessionState::from_stored(StoredSettings {
            annotation_tools: BTreeMap::from([("rect".to_string(), false)]),
            ..StoredSettings::default()
        });
        let tools = lock(&state.annotation_tools).clone();
        assert_eq!(tools.get("rect"), Some(&false));
        assert_eq!(tools.get("erase"), Some(&false));
        assert_eq!(tools.len(), ANNOTATION_TOOL_IDS.len());
        assert!(lock(&state.toggles).long_capture);
    }

    #[test]
    fn set_autostart_uses_set_enabled_result_not_blank_live_query() {
        let result = autostart::map_platform_status(autostart::PlatformStatus::Denied(
            "access denied".into(),
        ));
        let stored = autostart::remember_autostart_result(&result);
        let live = autostart::map_platform_status(autostart::PlatformStatus::NotRegistered);
        let merged = autostart::merge_autostart_ui(live, stored.clone());
        let mut ui = UiSettings {
            hotkeys: Hotkeys::default(),
            hotkey_errors: HotkeyErrors::default(),
            autostart: merged,
            notice: None,
            annotation_defaults: AnnotationDefaults::default(),
            toggles: FeatureToggles::default(),
            annotation_tools: default_annotation_tools(),
            capture: CaptureSettings::default(),
            history: HistorySettings::default(),
            export: ExportSettings::default(),
            tray: TrayState::available(),
            language: i18n::SYSTEM_LANGUAGE.to_string(),
            resolved_language: Language::ZhCn,
        };
        ui.autostart = result.clone();
        assert!(!ui.autostart.enabled);
        assert_eq!(ui.autostart.message, result.message);
        assert!(ui.autostart.message.as_deref().unwrap().contains("拒绝"));
        // R19:设置页载荷同时携带功能开关与标注工具表。
        let serialized = serde_json::to_value(&ui).unwrap();
        assert_eq!(
            serialized["toggles"]["longCapture"],
            serde_json::json!(true)
        );
        assert_eq!(
            serialized["toggles"]["pinRestore"],
            serde_json::json!(false)
        );
        assert_eq!(
            serialized["annotationTools"]["spotlight"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn capture_defaults_are_immediate() {
        let capture = CaptureSettings::default();
        assert_eq!(capture.delay_seconds, 0);
        assert_eq!(capture.delay_ms(), 0);
    }

    #[test]
    fn capture_sanitize_clamps_delay_and_keeps_valid_seconds() {
        let clamped = CaptureSettings { delay_seconds: 120 }.sanitized();
        assert_eq!(clamped.delay_seconds, MAX_DELAY_SECONDS);
        assert_eq!(clamped.delay_ms(), 60_000);

        let exact = CaptureSettings {
            delay_seconds: MAX_DELAY_SECONDS,
        }
        .sanitized();
        assert_eq!(exact.delay_seconds, 60);
    }

    #[test]
    fn missing_capture_field_loads_defaults() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-capture-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.capture, CaptureSettings::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_settings_ignore_saved_finish_fields() {
        let parsed: StoredSettings = serde_json::from_str(
            r#"{"capture":{"delaySeconds":7,"autoCopy":false,"finishAction":"quiet"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.capture.delay_seconds, 7);
        let serialized = serde_json::to_value(parsed.capture).unwrap();
        assert_eq!(serialized["delaySeconds"], 7);
        assert!(serialized.get("autoCopy").is_none());
        assert!(serialized.get("finishAction").is_none());
    }

    #[test]
    fn history_defaults_to_enabled_with_twenty_records() {
        let history = HistorySettings::default();
        assert!(history.enabled);
        assert_eq!(history.limit, 20);
        assert_eq!(history.sanitized(), history);
    }

    #[test]
    fn history_sanitize_clamps_limit_into_range() {
        let low = HistorySettings {
            enabled: false,
            limit: 1,
        }
        .sanitized();
        assert_eq!(low.limit, MIN_HISTORY_LIMIT);
        assert!(!low.enabled);
        let high = HistorySettings {
            enabled: true,
            limit: 9_999,
        }
        .sanitized();
        assert_eq!(high.limit, MAX_HISTORY_LIMIT);
        let exact = HistorySettings {
            enabled: true,
            limit: MAX_HISTORY_LIMIT,
        }
        .sanitized();
        assert_eq!(exact.limit, MAX_HISTORY_LIMIT);
    }

    #[test]
    fn missing_history_field_loads_defaults() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-history-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.history, HistorySettings::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_settings_deserialize_from_camel_case_json() {
        let parsed: StoredSettings =
            serde_json::from_str(r#"{"history":{"enabled":false,"limit":80}}"#).unwrap();
        assert!(!parsed.history.enabled);
        assert_eq!(parsed.history.limit, 80);
        let serialized = serde_json::to_value(parsed.history).unwrap();
        assert_eq!(serialized["enabled"], false);
        assert_eq!(serialized["limit"], 80);
    }

    #[test]
    fn export_defaults_to_png_lossless_without_directory() {
        let export = ExportSettings::default();
        assert_eq!(export.last_format, ExportFormat::Png);
        assert_eq!(export.last_dir, None);
        assert_eq!(export.quality, ExportQuality::High);
        assert_eq!(export.beautify, BeautifyOptions::default());
        assert!(export.filename_template.is_empty());
        assert_eq!(export.clone().sanitized(), export);
    }

    #[test]
    fn export_format_maps_known_extensions_case_insensitively() {
        assert_eq!(ExportFormat::from_extension("png"), Some(ExportFormat::Png));
        assert_eq!(
            ExportFormat::from_extension("JPG"),
            Some(ExportFormat::Jpeg)
        );
        assert_eq!(
            ExportFormat::from_extension("jpeg"),
            Some(ExportFormat::Jpeg)
        );
        assert_eq!(
            ExportFormat::from_extension("WebP"),
            Some(ExportFormat::Webp)
        );
        assert_eq!(ExportFormat::from_extension("bmp"), None);
        assert_eq!(ExportFormat::Png.extension(), "png");
        assert_eq!(ExportFormat::Jpeg.extension(), "jpg");
        assert_eq!(ExportFormat::Webp.extension(), "webp");
    }

    #[test]
    fn export_quality_tiers_map_to_distinct_encoder_values() {
        let high = ExportQuality::High.value();
        let medium = ExportQuality::Medium.value();
        let low = ExportQuality::Low.value();
        assert!(high > medium && medium > low);
        assert!(low >= 1 && high <= 100);
    }

    #[test]
    fn export_sanitize_drops_blank_directory() {
        let blank = ExportSettings {
            last_format: ExportFormat::Webp,
            last_dir: Some("   ".into()),
            quality: ExportQuality::Medium,
            beautify: BeautifyOptions {
                preset: "missing".into(),
                padding: 9_999,
                radius: 9_999,
                shadow: true,
            },
            filename_template: "  a\nb  ".into(),
        }
        .sanitized();
        assert_eq!(blank.last_dir, None);
        assert_eq!(blank.last_format, ExportFormat::Webp);
        assert_eq!(blank.quality, ExportQuality::Medium);
        assert_eq!(blank.beautify.preset, "paper");
        assert_eq!(blank.beautify.padding, 240);
        assert_eq!(blank.beautify.radius, 160);
        assert_eq!(blank.filename_template, "ab");
        assert!(blank.existing_directory().is_none());
    }

    #[test]
    fn missing_export_field_loads_defaults() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-export-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.export, ExportSettings::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_settings_deserialize_from_camel_case_json() {
        let parsed: StoredSettings = serde_json::from_str(
            r#"{"export":{"lastFormat":"webp","lastDir":"D:/shots","quality":"low"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.export.last_format, ExportFormat::Webp);
        assert_eq!(parsed.export.last_dir.as_deref(), Some("D:/shots"));
        assert_eq!(parsed.export.quality, ExportQuality::Low);
        assert_eq!(parsed.export.beautify, BeautifyOptions::default());
        assert!(parsed.export.filename_template.is_empty());
        let serialized = serde_json::to_value(parsed.export).unwrap();
        assert_eq!(serialized["lastFormat"], "webp");
        assert_eq!(serialized["lastDir"], "D:/shots");
        assert_eq!(serialized["quality"], "low");
    }

    #[test]
    fn export_existing_directory_is_only_used_when_present() {
        let dir = std::env::temp_dir().join(format!(
            "cropmark-settings-export-dir-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let present = ExportSettings {
            last_format: ExportFormat::Png,
            last_dir: Some(dir.to_string_lossy().into_owned()),
            quality: ExportQuality::High,
            ..ExportSettings::default()
        };
        assert_eq!(present.existing_directory(), Some(dir.as_path()));
        let missing = ExportSettings {
            last_format: ExportFormat::Png,
            last_dir: Some(dir.join("gone").to_string_lossy().into_owned()),
            quality: ExportQuality::High,
            ..ExportSettings::default()
        };
        assert!(missing.existing_directory().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_last_region_field_loads_none() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-region-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.last_region, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_region_deserializes_from_camel_case_and_survives_roundtrip() {
        let parsed: StoredSettings =
            serde_json::from_str(r#"{"lastRegion":{"x":-1920,"y":40,"width":640,"height":480}}"#)
                .unwrap();
        assert_eq!(
            parsed.last_region,
            Some(LastRegion {
                x: -1920,
                y: 40,
                width: 640,
                height: 480
            })
        );
        let serialized = serde_json::to_value(parsed.last_region).unwrap();
        assert_eq!(serialized["x"], -1920);
        assert_eq!(serialized["width"], 640);
    }

    #[test]
    fn zero_sized_last_region_is_rejected() {
        let zero = LastRegion {
            x: 0,
            y: 0,
            width: 0,
            height: 400,
        };
        assert_eq!(zero.sanitized(), None);
        assert_eq!(
            zero.clamp_to_monitors(&[LastRegion {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }]),
            None
        );
    }

    #[test]
    fn last_region_inside_monitor_keeps_size_and_picks_that_monitor() {
        let region = LastRegion {
            x: 100,
            y: 200,
            width: 300,
            height: 150,
        };
        let monitors = [
            LastRegion {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
            LastRegion {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
        ];
        assert_eq!(region.clamp_to_monitors(&monitors), Some((1, region)));
    }

    #[test]
    fn last_region_clamped_into_best_overlapping_monitor() {
        // 区域跨屏(左屏 500px + 右屏 300px):选重叠更大的左屏并裁剪。
        let region = LastRegion {
            x: -500,
            y: 100,
            width: 800,
            height: 400,
        };
        let monitors = [
            LastRegion {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
            LastRegion {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
        ];
        assert_eq!(
            region.clamp_to_monitors(&monitors),
            Some((
                0,
                LastRegion {
                    x: -500,
                    y: 100,
                    width: 500,
                    height: 400
                }
            ))
        );
    }

    #[test]
    fn last_region_is_cropped_when_resolution_shrinks() {
        // 上次区域超出右/下边缘:只保留仍可见的重叠部分,不越界。
        let region = LastRegion {
            x: 2000,
            y: 1300,
            width: 800,
            height: 600,
        };
        let monitors = [LastRegion {
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
        }];
        assert_eq!(
            region.clamp_to_monitors(&monitors),
            Some((
                0,
                LastRegion {
                    x: 2000,
                    y: 1300,
                    width: 560,
                    height: 140
                }
            ))
        );
    }

    #[test]
    fn last_region_without_intersection_or_monitors_is_rejected() {
        let region = LastRegion {
            x: 5000,
            y: 5000,
            width: 200,
            height: 200,
        };
        let monitors = [LastRegion {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        assert_eq!(region.clamp_to_monitors(&monitors), None);
        assert_eq!(region.clamp_to_monitors(&[]), None);
        // 显示器缩放到另一位置后原区域同样失效,不得越界。
        let moved = LastRegion {
            x: 4000,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(region.clamp_to_monitors(&[moved]), None);
    }

    #[test]
    fn last_region_negative_origin_is_preserved_when_monitor_unchanged() {
        let region = LastRegion {
            x: -1910,
            y: 20,
            width: 300,
            height: 200,
        };
        let monitors = [LastRegion {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        assert_eq!(region.clamp_to_monitors(&monitors), Some((0, region)));
    }

    #[test]
    fn tray_defaults_to_available_without_message() {
        let tray = TrayState::available();
        assert!(tray.available);
        assert_eq!(tray.message, None);
    }

    #[test]
    fn tray_state_serializes_for_settings_window() {
        let serialized = serde_json::to_value(TrayState {
            available: false,
            message: Some("当前桌面环境未提供托盘".into()),
        })
        .unwrap();
        assert_eq!(serialized["available"], false);
        assert_eq!(serialized["message"], "当前桌面环境未提供托盘");
    }

    #[test]
    fn language_defaults_to_system_and_rejects_unknown_values() {
        assert_eq!(default_language_setting(), "system");
        assert_eq!(sanitize_language("zh-CN"), "zh-CN");
        assert_eq!(sanitize_language(" en "), "en");
        assert_eq!(sanitize_language("system"), "system");
        assert_eq!(sanitize_language("fr-FR"), "system");
        assert_eq!(sanitize_language(""), "system");
    }

    #[test]
    fn missing_language_field_loads_system_default() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-lang-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.language, i18n::SYSTEM_LANGUAGE);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn language_deserializes_from_camel_case_json() {
        let parsed: StoredSettings = serde_json::from_str(r#"{"language":"en"}"#).unwrap();
        assert_eq!(parsed.language, "en");
        let serialized = serde_json::to_value(StoredSettings {
            language: "zh-CN".into(),
            ..StoredSettings::default()
        })
        .unwrap();
        assert_eq!(serialized["language"], "zh-CN");
    }

    #[test]
    fn language_info_reports_setting_and_resolution() {
        let info = LanguageInfo {
            language: "system".into(),
            resolved_language: i18n::resolve_setting("system"),
        };
        let serialized = serde_json::to_value(&info).unwrap();
        assert_eq!(serialized["language"], "system");
        assert!(
            serialized["resolvedLanguage"] == "zh-CN" || serialized["resolvedLanguage"] == "en"
        );
    }

    #[test]
    fn onboarding_auto_opens_once_until_the_window_is_closed() {
        let defaults = StoredSettings::default();
        assert!(!defaults.onboarding_done);
        assert!(defaults.toggles.onboarding);
        assert!(should_auto_open_onboarding(true, false));
        assert!(!should_auto_open_onboarding(true, true));
        assert!(!should_auto_open_onboarding(false, false));
        assert!(!should_auto_open_onboarding(false, true));

        let fresh: StoredSettings = serde_json::from_str("{}").unwrap();
        assert!(!fresh.onboarding_done);
        assert!(should_auto_open_onboarding(
            fresh.toggles.onboarding,
            fresh.onboarding_done
        ));

        // 旧配置没有该字段:视为尚未看过引导,而不是把整份设置判坏。
        let legacy: StoredSettings = serde_json::from_str(
            r#"{"hotkeys":{"region":"Alt+Shift+A","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        assert!(!legacy.onboarding_done);
        assert_eq!(legacy.hotkeys.region, "Alt+Shift+A");

        let done: StoredSettings =
            serde_json::from_str(r#"{"onboardingDone":true,"toggles":{"onboarding":true}}"#)
                .unwrap();
        assert!(done.onboarding_done);
        assert!(!should_auto_open_onboarding(
            done.toggles.onboarding,
            done.onboarding_done
        ));

        let disabled: StoredSettings =
            serde_json::from_str(r#"{"toggles":{"onboarding":false}}"#).unwrap();
        assert!(!disabled.onboarding_done);
        assert!(!should_auto_open_onboarding(
            disabled.toggles.onboarding,
            disabled.onboarding_done
        ));

        let state = SessionState::from_stored(StoredSettings {
            onboarding_done: true,
            ..StoredSettings::default()
        });
        assert!(*lock(&state.onboarding_done));
    }
}
