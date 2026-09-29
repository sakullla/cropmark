import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { autostartHelp, hotkeyErrorText } from "../errors";
import type { AnnotationTool } from "../annotation";
import { t, type CatalogKey } from "../i18n";
import { icons } from "../icons";

export type CaptureMode = "region" | "window" | "fullscreen";

/// R8:热键行槽位 = 三种采集模式 + 可选的剪贴板贴图(默认未绑定)。
export type HotkeySlot = CaptureMode | "clipboardpin";

export interface Hotkeys {
  region: string;
  window: string;
  fullscreen: string;
  pinClipboard: string;
}

export interface HotkeyErrors {
  region: string | null;
  window: string | null;
  fullscreen: string | null;
  pinClipboard: string | null;
}

export interface AutostartState {
  enabled: boolean;
  message: string | null;
}

/// R9:采集普通选项(延时 + 包含鼠标指针 + 多屏全屏采集);默认值由后端给出,
/// 前端只渲染并回写,不自行决定默认。
export interface CaptureSettings {
  delaySeconds: number;
  captureCursor: boolean;
  multiMonitor: boolean;
  longCapture: boolean;
}

export interface HistorySettings {
  enabled: boolean;
  limit: number;
}

/// 与 Rust `BeautifyOptions` 一致;颜色在预览与 `beautify.rs` 各有一份。
export interface BeautifyOptions {
  preset: string;
  padding: number;
  radius: number;
  shadow: boolean;
}

/// R9:导出记忆与普通选项;`applyBeautify` / `useFilenameTemplate` 默认关闭,
/// 与精简前同名开关一致。
export interface ExportSettings {
  lastFormat: "png" | "jpeg" | "webp";
  lastDir: string | null;
  quality: "high" | "medium" | "low";
  beautify: BeautifyOptions;
  filenameTemplate: string;
  applyBeautify: boolean;
  useFilenameTemplate: boolean;
}

/// R9:贴图普通选项;默认关闭,与精简前同名开关一致。
export interface PinSettings {
  restore: boolean;
}

/// R3:录屏开关与录制格式。开关默认关闭;格式默认 GIF。
export type RecordingFormat = "gif" | "webp" | "mp4";

export interface RecordingSettings {
  enabled: boolean;
  format: RecordingFormat;
}

/// 选区工具开关。关闭只是不放进工具条，工具本身还在。
export interface RegionTools {
  arrow: boolean;
  rect: boolean;
  ellipse: boolean;
  highlighter: boolean;
  mosaic: boolean;
  text: boolean;
  number: boolean;
  spotlight: boolean;
  magnifier: boolean;
  bubble: boolean;
  sticker: boolean;
  erase: boolean;
  line: boolean;
  blur: boolean;
  pin: boolean;
  ocr: boolean;
  qr: boolean;
}

export type RegionToolId = AnnotationTool | "line" | "blur" | "pin" | "ocr" | "qr";

export const REGION_TOOL_FIELDS: { id: RegionToolId; labelKey: CatalogKey }[] = [
  { id: "arrow", labelKey: "selection.tool.arrow" },
  { id: "rect", labelKey: "selection.tool.rect" },
  { id: "ellipse", labelKey: "selection.tool.ellipse" },
  { id: "highlighter", labelKey: "selection.tool.highlighter" },
  { id: "mosaic", labelKey: "selection.tool.mosaic" },
  { id: "text", labelKey: "selection.tool.text" },
  { id: "number", labelKey: "selection.tool.number" },
  { id: "spotlight", labelKey: "selection.tool.spotlight" },
  { id: "magnifier", labelKey: "selection.tool.magnifier" },
  { id: "bubble", labelKey: "selection.tool.bubble" },
  { id: "sticker", labelKey: "selection.tool.sticker" },
  { id: "erase", labelKey: "selection.tool.erase" },
  { id: "line", labelKey: "selection.tool.line" },
  { id: "blur", labelKey: "selection.tool.blur" },
  { id: "pin", labelKey: "selection.action.pin" },
  { id: "ocr", labelKey: "selection.action.ocr" },
  { id: "qr", labelKey: "selection.action.qr" },
];

export interface TrayState {
  available: boolean;
  message: string | null;
}

export type LanguageSetting = "system" | "zh-CN" | "en";

/// R9:设置页载荷不再包含功能开关与标注工具表;普通选项随所属结构返回。
export interface UiSettings {
  hotkeys: Hotkeys;
  hotkeyErrors: HotkeyErrors;
  autostart: AutostartState;
  notice: string | null;
  capture: CaptureSettings;
  history: HistorySettings;
  export: ExportSettings;
  pin: PinSettings;
  recording: RecordingSettings;
  regionTools: RegionTools;
  tray: TrayState;
  language: string;
  resolvedLanguage: string;
}

/// R8:左侧导航的四分类;每个分类对应右侧一个独立面板。
type SettingsSection = "capture" | "output" | "general" | "about";

const SECTIONS: SettingsSection[] = ["capture", "output", "general", "about"];

const SECTION_LABEL_KEY: Record<SettingsSection, CatalogKey> = {
  capture: "settings.group.capture",
  output: "settings.group.output",
  general: "settings.group.general",
  about: "settings.about.title",
};

const HOTKEY_LABEL_KEY: Record<HotkeySlot, CatalogKey> = {
  region: "settings.mode.region",
  window: "settings.mode.window",
  fullscreen: "settings.mode.fullscreen",
  clipboardpin: "settings.hotkeys.clipboard_pin_label",
};

const LANGUAGE_OPTIONS: Array<{ value: LanguageSetting; labelKey: CatalogKey }> = [
  { value: "system", labelKey: "language.system" },
  { value: "zh-CN", labelKey: "language.zh_cn" },
  { value: "en", labelKey: "language.en" },
];

const HOTKEY_SLOTS: HotkeySlot[] = ["region", "window", "fullscreen", "clipboardpin"];

/// R3:录制格式选项;value 与 Rust `RecordFormat` 的 lowercase serde 值一致。
const RECORDING_FORMATS: Array<{ value: RecordingFormat; labelKey: CatalogKey }> = [
  { value: "gif", labelKey: "settings.recording.format.gif" },
  { value: "webp", labelKey: "settings.recording.format.webp" },
  { value: "mp4", labelKey: "settings.recording.format.mp4" },
];

const DEFAULT_RECORDING: RecordingSettings = { enabled: false, format: "gif" };

const DEFAULT_BEAUTIFY: BeautifyOptions = {
  preset: "paper",
  padding: 32,
  radius: 16,
  shadow: true,
};

/// 与 `src-tauri/src/beautify.rs` 的 PRESETS 保持同序、同色。
const BEAUTIFY_PRESETS: Array<{ id: string; labelKey: CatalogKey; swatch: string }> = [
  { id: "paper", labelKey: "settings.export.preset.paper", swatch: "#f4f1ea" },
  { id: "slate", labelKey: "settings.export.preset.slate", swatch: "#334155" },
  { id: "ink", labelKey: "settings.export.preset.ink", swatch: "#0b1220" },
  { id: "dawn", labelKey: "settings.export.preset.dawn", swatch: "#fde68a" },
  { id: "ocean", labelKey: "settings.export.preset.ocean", swatch: "#38bdf8" },
  { id: "dusk", labelKey: "settings.export.preset.dusk", swatch: "#312e81" },
];

interface ExportAppearance {
  beautify: BeautifyOptions;
  filenameTemplate: string;
  applyBeautify: boolean;
  useFilenameTemplate: boolean;
}

function clampInt(value: number, min: number, max: number, fallback: number): number {
  if (!Number.isFinite(value)) {
    return fallback;
  }
  return Math.min(max, Math.max(min, Math.round(value)));
}

function hotkeyValue(hotkeys: Hotkeys, slot: HotkeySlot): string {
  return slot === "clipboardpin" ? hotkeys.pinClipboard : hotkeys[slot];
}

function hotkeyError(errors: HotkeyErrors, slot: HotkeySlot): string | null {
  return slot === "clipboardpin" ? errors.pinClipboard : errors[slot];
}

function switchMarkup(dataset: string, value: string, labelId: string, enabled: boolean): string {
  return `<button type="button" class="switch" data-${dataset}="${value}" role="switch" aria-checked="${enabled ? "true" : "false"}" aria-labelledby="${labelId}"><span class="knob"></span></button>`;
}

export function mountSettings(root: HTMLElement): () => void {
  // R8:宽版两栏布局;导航固定,右侧面板独立滚动。所有设置项与默认值保留,
  // R9 移除的功能/工具开关不再出现,五项普通选项以开关行呈现。
  root.innerHTML = `
    <div class="shell">
      <header class="titlebar" data-tauri-drag-region>
        <div class="brand" data-tauri-drag-region>
          <span class="mark" aria-hidden="true"></span>
          <span class="name">Cropmark</span>
        </div>
        <button type="button" class="icon-btn" data-action="close" data-i18n-aria-label="settings.close" aria-label="关闭">${icons.close}</button>
      </header>
      <main class="settings-body">
        <nav class="settings-nav" role="tablist" aria-orientation="vertical" data-settings-nav data-i18n-aria-label="settings.nav.aria_label" aria-label="设置分类">
          ${SECTIONS.map(
            (section, index) => `
            <button type="button" class="settings-nav-item" role="tab" id="settings-tab-${section}" aria-controls="settings-panel-${section}" aria-selected="${index === 0 ? "true" : "false"}" tabindex="${index === 0 ? 0 : -1}" data-section="${section}" data-i18n="${SECTION_LABEL_KEY[section]}">${t(SECTION_LABEL_KEY[section])}</button>`,
          ).join("")}
        </nav>
        <div class="settings-main">
          <p class="notice" role="alert" hidden></p>
          <p class="notice tray-notice" data-tray-notice role="status" hidden></p>
          <div class="settings-panels" data-settings-panels>
            <section class="settings-panel" role="tabpanel" id="settings-panel-capture" aria-labelledby="settings-tab-capture" data-panel="capture">
              <section class="card" aria-labelledby="group-capture-title">
                <h1 id="group-capture-title" data-i18n="settings.group.capture">采集</h1>
                <section class="block" aria-labelledby="hotkeys-title">
                  <h2 id="hotkeys-title" data-i18n="settings.hotkeys.title">热键</h2>
                  <p class="hint" data-i18n="settings.hotkeys.hint">点击热键按钮后按下新组合，Esc 取消；改动立即生效。</p>
                  <div class="rows" data-hotkeys></div>
                </section>
                <section class="block" aria-labelledby="capture-title">
                  <h2 id="capture-title" data-i18n="settings.capture.title">截图</h2>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="delay-label" data-i18n="settings.capture.delay_label">延时秒数</div>
                      <p class="hint" data-i18n="settings.capture.delay_hint">0–60 秒，热键与托盘截取按此倒计时；0 为立即截取。</p>
                    </div>
                    <input type="number" class="number-input" data-capture="delay" min="0" max="60" step="1" inputmode="numeric" aria-labelledby="delay-label" />
                  </div>
                  <p class="error" data-capture-error role="alert" hidden></p>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="capture-cursor-label" data-i18n="settings.capture.cursor_label">包含鼠标指针</div>
                      <p class="hint" data-i18n="settings.capture.cursor_hint">采集瞬间把系统鼠标指针绘制进结果，仅当指针位于采集范围内；默认关闭。</p>
                    </div>
                    ${switchMarkup("capture-option", "captureCursor", "capture-cursor-label", false)}
                  </div>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="multi-monitor-label" data-i18n="settings.capture.multi_monitor_label">多屏全屏采集</div>
                      <p class="hint" data-i18n="settings.capture.multi_monitor_hint">全屏采集可选择指定显示器或全部显示器拼接；关闭后只抓指针所在屏。</p>
                    </div>
                    ${switchMarkup("capture-option", "multiMonitor", "multi-monitor-label", true)}
                  </div>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="long-capture-label" data-i18n="settings.capture.long_capture_label">长截图</div>
                      <p class="hint" data-i18n="settings.capture.long_capture_hint">开启后托盘和选区出现长截图入口。</p>
                    </div>
                    ${switchMarkup("capture-option", "longCapture", "long-capture-label", false)}
                  </div>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="region-tools-title" data-i18n="settings.region_tools.title">选区工具</div>
                      <p class="hint" data-i18n="settings.region_tools.hint">决定区域截图工具条和「更多」里显示哪些项。</p>
                    </div>
                  </div>
                  <div class="choices" data-region-tools role="group" aria-labelledby="region-tools-title"></div>
                </section>
                <section class="block" aria-labelledby="recording-title">
                  <h2 id="recording-title" data-i18n="settings.recording.title">录屏</h2>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="recording-enabled-label" data-i18n="settings.recording.enabled_label">启用录屏</div>
                      <p class="hint" data-i18n="settings.recording.enabled_hint">开启后托盘与选区出现录屏入口，经延时与区域选择开始录制；关闭时没有入口，也不产生录制行为。录制文件不会进入截图历史。</p>
                    </div>
                    ${switchMarkup("recording-option", "enabled", "recording-enabled-label", false)}
                  </div>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="recording-format-label" data-i18n="settings.recording.format_label">录制格式</div>
                      <p class="hint" data-i18n="settings.recording.format_hint">录制文件的保存格式；GIF 通用、WebP 体积更小、MP4 适合较长内容。</p>
                    </div>
                  </div>
                  <div class="choices" data-recording-formats role="radiogroup" aria-labelledby="recording-format-label"></div>
                </section>
              </section>
            </section>
            <section class="settings-panel" role="tabpanel" id="settings-panel-output" aria-labelledby="settings-tab-output" data-panel="output" hidden>
              <section class="card" aria-labelledby="group-output-title">
                <h1 id="group-output-title" data-i18n="settings.group.output">记录与输出</h1>
                <section class="block" aria-labelledby="history-title">
                  <h2 id="history-title" data-i18n="settings.history.title">历史记录</h2>
                  <p class="hint" data-i18n="settings.history.hint">截图完成后在本机保留最近记录，可重新复制、贴图或删除；数据只保存在本机。</p>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="history-enabled-label" data-i18n="settings.history.enabled_label">保留截图历史</div>
                      <p class="hint" data-i18n="settings.history.enabled_hint">关闭后不再新增记录；已有记录保留，可在历史窗口清空。</p>
                    </div>
                    ${switchMarkup("history", "enabled", "history-enabled-label", true)}
                  </div>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="history-limit-label" data-i18n="settings.history.limit_label">记录上限</div>
                      <p class="hint" data-i18n="settings.history.limit_hint">5–200 条，超出上限时自动淘汰最旧记录。</p>
                    </div>
                    <input type="number" class="number-input" data-history="limit" min="5" max="200" step="1" inputmode="numeric" aria-labelledby="history-limit-label" />
                  </div>
                  <p class="error" data-history-error role="alert" hidden></p>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="history-open-label" data-i18n="settings.history.open_label">浏览历史</div>
                      <p class="hint" data-i18n="settings.history.open_hint">打开历史窗口，按时间查看缩略图并重新复制、贴图或删除。</p>
                    </div>
                    <button type="button" class="choice" data-action="open-history" aria-labelledby="history-open-label" data-i18n="settings.history.open_button">打开历史记录</button>
                  </div>
                </section>
                <section class="block" aria-labelledby="naming-title">
                  <h2 id="naming-title" data-i18n="settings.section.naming">保存与命名</h2>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="apply-template-label" data-i18n="settings.export.apply_template_label">套用文件名模板</div>
                      <p class="hint" data-i18n="settings.export.apply_template_hint">开启后保存对话框默认文件名按模板生成，非法字符自动替换，空模板回退 Cropmark 时间戳；关闭后使用现有默认命名。同名文件会另存为不覆盖的新路径。</p>
                    </div>
                    ${switchMarkup("export-option", "useFilenameTemplate", "apply-template-label", false)}
                  </div>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="template-label" data-i18n="settings.export.template_label">文件名模板</div>
                      <p class="hint" data-i18n="settings.export.template_hint">占位符：{date}、{time}、{datetime}、{mode}、{seq}。只影响本地保存的默认文件名。</p>
                    </div>
                  </div>
                  <input type="text" class="number-input" data-filename-template maxlength="180" spellcheck="false" aria-labelledby="template-label" data-i18n-placeholder="settings.export.template_placeholder" placeholder="例如 shot_{date}_{mode}_{seq}" />
                </section>
                <section class="block" aria-labelledby="beautify-title">
                  <h2 id="beautify-title" data-i18n="settings.export.beautify_title">导出美化</h2>
                  <p class="hint" data-i18n="settings.export.beautify_hint">开启「套用美化」后，预览、复制和保存使用下面的背景、留白、圆角与阴影；标注仍画在原始画面上。</p>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="apply-beautify-label" data-i18n="settings.export.apply_label">套用美化</div>
                    </div>
                    ${switchMarkup("export-option", "applyBeautify", "apply-beautify-label", false)}
                  </div>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="preset-label" data-i18n="settings.export.preset_label">背景</div>
                    </div>
                  </div>
                  <div class="choices" data-beautify-presets role="radiogroup" aria-labelledby="preset-label"></div>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="padding-label" data-i18n="settings.export.padding_label">留白</div>
                      <p class="hint" data-i18n="settings.export.padding_hint">0–240 像素，输出四周各加这么多留白。</p>
                    </div>
                    <input type="number" class="number-input" data-beautify-padding min="0" max="240" step="1" inputmode="numeric" aria-labelledby="padding-label" />
                  </div>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="radius-label" data-i18n="settings.export.radius_label">圆角</div>
                      <p class="hint" data-i18n="settings.export.radius_hint">0–160 像素。阴影边距由圆角推导。</p>
                    </div>
                    <input type="number" class="number-input" data-beautify-radius min="0" max="160" step="1" inputmode="numeric" aria-labelledby="radius-label" />
                  </div>
                  <p class="error" data-export-error role="alert" hidden></p>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="shadow-label" data-i18n="settings.export.shadow_label">阴影</div>
                      <p class="hint" data-i18n="settings.export.shadow_hint">在圆角外侧加一圈阴影。</p>
                    </div>
                    ${switchMarkup("beautify-shadow", "shadow", "shadow-label", true)}
                  </div>
                </section>
              </section>
            </section>
            <section class="settings-panel" role="tabpanel" id="settings-panel-general" aria-labelledby="settings-tab-general" data-panel="general" hidden>
              <section class="card" aria-labelledby="group-general-title">
                <h1 id="group-general-title" data-i18n="settings.group.general">通用</h1>
                <section class="block" aria-labelledby="language-title">
                  <h2 id="language-title" data-i18n="settings.language.title">语言</h2>
                  <p class="hint" data-i18n="settings.language.hint">切换后界面立即更新，无需重启；选择会跨会话保留。</p>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="language-label" data-i18n="settings.language.label">界面语言</div>
                    </div>
                    <div class="choices" data-language role="radiogroup" aria-labelledby="language-label">
                      ${LANGUAGE_OPTIONS.map(
                        ({ value, labelKey }) =>
                          `<button type="button" class="choice" role="radio" data-language-value="${value}" aria-checked="false" data-i18n="${labelKey}">${t(labelKey)}</button>`,
                      ).join("")}
                    </div>
                  </div>
                </section>
                <section class="block" aria-labelledby="autostart-title">
                  <h2 id="autostart-title" data-i18n="settings.autostart.title">开机启动</h2>
                  <div class="autostart-row">
                    <div>
                      <div class="label" id="autostart-label" data-i18n="settings.autostart.label">登录时运行</div>
                      <p class="hint autostart-help"></p>
                    </div>
                    <button type="button" class="switch" data-action="autostart" role="switch" aria-checked="false" aria-labelledby="autostart-label">
                      <span class="knob"></span>
                    </button>
                  </div>
                </section>
                <section class="block" aria-labelledby="pin-title">
                  <h2 id="pin-title" data-i18n="settings.pin.title">贴图</h2>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="pin-restore-label" data-i18n="settings.pin.restore_label">重启后恢复贴图</div>
                      <p class="hint" data-i18n="settings.pin.restore_hint">重启后恢复仍存在的贴图及其位置、尺寸与变换；已关闭的贴图不会重现。</p>
                    </div>
                    ${switchMarkup("pin-option", "restore", "pin-restore-label", false)}
                  </div>
                </section>
                <section class="block" aria-labelledby="help-title">
                  <h2 id="help-title" data-i18n="settings.help.title">使用帮助</h2>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="help-label" data-i18n="settings.help.label">快捷键与功能</div>
                      <p class="hint" data-i18n="settings.help.hint">查看与首次引导相同的说明，可随时再次打开。</p>
                    </div>
                    <button type="button" class="choice" data-action="open-guide" aria-labelledby="help-label" data-i18n="settings.help.button">打开</button>
                  </div>
                </section>
                <section class="block" aria-labelledby="logs-title">
                  <h2 id="logs-title" data-i18n="settings.logs.title">日志位置</h2>
                  <div class="setting-row">
                    <div>
                      <div class="label" id="logs-label" data-i18n="settings.logs.label">日志文件</div>
                      <p class="hint" data-i18n="settings.logs.hint">诊断记录写在本机该文件；同目录的 crash.log 只在发生 panic 时追加。日志不含图像、剪贴板内容或识别文字。</p>
                      <p class="hint" data-log-path>—</p>
                    </div>
                    <button type="button" class="choice" data-action="open-logs" aria-labelledby="logs-label" data-i18n="settings.logs.button">打开</button>
                  </div>
                </section>
              </section>
            </section>
            <section class="settings-panel" role="tabpanel" id="settings-panel-about" aria-labelledby="settings-tab-about" data-panel="about" hidden>
              <section class="card about" aria-labelledby="about-title">
                <h1 id="about-title" data-i18n="settings.about.title">关于</h1>
                <span class="mark" aria-hidden="true"></span>
                <p class="about-name">Cropmark</p>
                <p class="hint" data-i18n="settings.about.hint">独立系统截图工具，界面与托盘只使用 Cropmark 名称与图标。</p>
                <div class="setting-row">
                  <div>
                    <div class="label" id="about-version-label" data-i18n="settings.about.version_label">版本</div>
                  </div>
                  <span class="about-version" data-version>—</span>
                </div>
                <div class="setting-row">
                  <div>
                    <div class="label" id="quit-label" data-i18n="settings.about.quit_label">退出 Cropmark</div>
                    <p class="hint" data-i18n="settings.about.quit_hint">结束应用并停止热键；有托盘时也可从托盘菜单退出。</p>
                  </div>
                  <button type="button" class="choice danger" data-action="quit" aria-labelledby="quit-label" data-i18n="settings.about.quit_button">退出</button>
                </div>
              </section>
            </section>
          </div>
        </div>
      </main>
    </div>
  `;

  const noticeEl = root.querySelector(".notice");
  const trayNoticeEl = root.querySelector("[data-tray-notice]");
  const navRoot = root.querySelector("[data-settings-nav]");
  const panelsRoot = root.querySelector("[data-settings-panels]");
  const hotkeyRoot = root.querySelector("[data-hotkeys]");
  const helpEl = root.querySelector(".autostart-help");
  const switchEl = root.querySelector("[data-action=autostart]");
  const closeEl = root.querySelector("[data-action=close]");
  const delayEl = root.querySelector("[data-capture=delay]");
  const delayErrorEl = root.querySelector("[data-capture-error]");
  const captureCursorEl = root.querySelector("[data-capture-option=captureCursor]");
  const multiMonitorEl = root.querySelector("[data-capture-option=multiMonitor]");
  const longCaptureEl = root.querySelector("[data-capture-option=longCapture]");
  const historyEnabledEl = root.querySelector("[data-history=enabled]");
  const historyLimitEl = root.querySelector("[data-history=limit]");
  const historyErrorEl = root.querySelector("[data-history-error]");
  const historyOpenEl = root.querySelector("[data-action=open-history]");
  const useFilenameTemplateEl = root.querySelector("[data-export-option=useFilenameTemplate]");
  const templateEl = root.querySelector("[data-filename-template]");
  const applyBeautifyEl = root.querySelector("[data-export-option=applyBeautify]");
  const presetRoot = root.querySelector("[data-beautify-presets]");
  const paddingEl = root.querySelector("[data-beautify-padding]");
  const radiusEl = root.querySelector("[data-beautify-radius]");
  const exportErrorEl = root.querySelector("[data-export-error]");
  const shadowEl = root.querySelector("[data-beautify-shadow=shadow]");
  const pinRestoreEl = root.querySelector("[data-pin-option=restore]");
  const recordingEnabledEl = root.querySelector("[data-recording-option=enabled]");
  const recordingFormatsEl = root.querySelector("[data-recording-formats]");
  const regionToolsEl = root.querySelector("[data-region-tools]");
  const openGuideEl = root.querySelector("[data-action=open-guide]");
  const logPathEl = root.querySelector("[data-log-path]");
  const openLogsEl = root.querySelector("[data-action=open-logs]");
  const quitEl = root.querySelector("[data-action=quit]");
  const languageRoot = root.querySelector("[data-language]");
  const versionEl = root.querySelector("[data-version]");
  if (
    !(noticeEl instanceof HTMLElement) ||
    !(trayNoticeEl instanceof HTMLElement) ||
    !(navRoot instanceof HTMLElement) ||
    !(panelsRoot instanceof HTMLElement) ||
    !(hotkeyRoot instanceof HTMLElement) ||
    !(helpEl instanceof HTMLElement) ||
    !(switchEl instanceof HTMLButtonElement) ||
    !(closeEl instanceof HTMLButtonElement) ||
    !(delayEl instanceof HTMLInputElement) ||
    !(delayErrorEl instanceof HTMLElement) ||
    !(captureCursorEl instanceof HTMLButtonElement) ||
    !(multiMonitorEl instanceof HTMLButtonElement) ||
    !(longCaptureEl instanceof HTMLButtonElement) ||
    !(historyEnabledEl instanceof HTMLButtonElement) ||
    !(historyLimitEl instanceof HTMLInputElement) ||
    !(historyErrorEl instanceof HTMLElement) ||
    !(historyOpenEl instanceof HTMLButtonElement) ||
    !(useFilenameTemplateEl instanceof HTMLButtonElement) ||
    !(templateEl instanceof HTMLInputElement) ||
    !(applyBeautifyEl instanceof HTMLButtonElement) ||
    !(presetRoot instanceof HTMLElement) ||
    !(paddingEl instanceof HTMLInputElement) ||
    !(radiusEl instanceof HTMLInputElement) ||
    !(exportErrorEl instanceof HTMLElement) ||
    !(shadowEl instanceof HTMLButtonElement) ||
    !(pinRestoreEl instanceof HTMLButtonElement) ||
    !(recordingEnabledEl instanceof HTMLButtonElement) ||
    !(recordingFormatsEl instanceof HTMLElement) ||
    !(regionToolsEl instanceof HTMLElement) ||
    !(openGuideEl instanceof HTMLButtonElement) ||
    !(logPathEl instanceof HTMLElement) ||
    !(openLogsEl instanceof HTMLButtonElement) ||
    !(quitEl instanceof HTMLButtonElement) ||
    !(languageRoot instanceof HTMLElement)
  ) {
    return () => undefined;
  }

  let recording: HotkeySlot | null = null;
  let applying = false;
  // 切换写回进行中:开关给共享 busy 可视态(app.css button[aria-busy])并阻止点击。
  const setApplying = (value: boolean): void => {
    applying = value;
    root.querySelectorAll("button.switch").forEach((button) => {
      if (value) {
        button.setAttribute("aria-busy", "true");
      } else {
        button.removeAttribute("aria-busy");
      }
    });
  };
  let lastSettings: UiSettings | null = null;
  let section: SettingsSection = "capture";
  let captureSettings: CaptureSettings = {
    delaySeconds: 0,
    captureCursor: false,
    multiMonitor: true,
    longCapture: false,
  };
  let historySettings: HistorySettings = {
    enabled: true,
    limit: 20,
  };
  let exportAppearance: ExportAppearance = {
    beautify: { ...DEFAULT_BEAUTIFY },
    filenameTemplate: "",
    applyBeautify: false,
    useFilenameTemplate: false,
  };
  let pinSettings: PinSettings = { restore: false };
  let recordingSettings: RecordingSettings = { ...DEFAULT_RECORDING };
  let regionTools: RegionTools = {
    arrow: true,
    rect: true,
    ellipse: true,
    highlighter: true,
    mosaic: true,
    text: true,
    number: true,
    spotlight: true,
    magnifier: true,
    bubble: true,
    sticker: true,
    erase: true,
    line: true,
    blur: true,
    pin: true,
    ocr: true,
    qr: true,
  };

  const syncSwitch = (button: HTMLButtonElement, on: boolean): void => {
    button.setAttribute("aria-checked", on ? "true" : "false");
    button.classList.toggle("on", on);
  };

  // R8:导航与面板一一对应;切换只改选中态与 hidden,导航自身不随内容滚动。
  const selectSection = (next: SettingsSection, moveFocus = false): void => {
    section = next;
    navRoot.querySelectorAll<HTMLButtonElement>("[data-section]").forEach((button) => {
      const selected = button.dataset.section === next;
      button.setAttribute("aria-selected", selected ? "true" : "false");
      button.tabIndex = selected ? 0 : -1;
      if (selected && moveFocus) {
        button.focus();
      }
    });
    panelsRoot.querySelectorAll<HTMLElement>("[data-panel]").forEach((panel) => {
      panel.hidden = panel.dataset.panel !== next;
    });
    panelsRoot.scrollTop = 0;
  };

  navRoot.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element)) {
      return;
    }
    const button = target.closest("[data-section]");
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    const next = button.dataset.section as SettingsSection | undefined;
    if (next && SECTIONS.includes(next) && next !== section) {
      selectSection(next);
    }
  });
  navRoot.addEventListener("keydown", (event) => {
    const keys = ["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End"];
    if (!keys.includes(event.key)) {
      return;
    }
    event.preventDefault();
    const currentIndex = SECTIONS.indexOf(section);
    let nextIndex: number;
    if (event.key === "Home") {
      nextIndex = 0;
    } else if (event.key === "End") {
      nextIndex = SECTIONS.length - 1;
    } else {
      const delta = event.key === "ArrowUp" || event.key === "ArrowLeft" ? -1 : 1;
      nextIndex = (currentIndex + delta + SECTIONS.length) % SECTIONS.length;
    }
    selectSection(SECTIONS[nextIndex], true);
  });

  const showDelayError = (message: string): void => {
    delayErrorEl.hidden = false;
    delayErrorEl.textContent = message;
    delayEl.setAttribute("aria-invalid", "true");
  };

  const clearDelayError = (): void => {
    delayErrorEl.hidden = true;
    delayErrorEl.textContent = "";
    delayEl.removeAttribute("aria-invalid");
  };

  const renderCapture = (capture: CaptureSettings): void => {
    captureSettings = capture;
    delayEl.value = String(capture.delaySeconds);
    clearDelayError();
    syncSwitch(captureCursorEl, capture.captureCursor);
    syncSwitch(multiMonitorEl, capture.multiMonitor);
    syncSwitch(longCaptureEl, capture.longCapture);
  };

  const renderRegionTools = (tools: RegionTools): void => {
    regionTools = tools;
    const active =
      document.activeElement instanceof HTMLButtonElement
        ? (document.activeElement.dataset.regionTool ?? null)
        : null;
    regionToolsEl.replaceChildren();
    for (const field of REGION_TOOL_FIELDS) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "choice";
      button.dataset.regionTool = field.id;
      button.textContent = t(field.labelKey);
      const on = tools[field.id];
      button.setAttribute("aria-pressed", on ? "true" : "false");
      button.classList.toggle("selected", on);
      regionToolsEl.append(button);
    }
    if (active) {
      regionToolsEl.querySelector<HTMLButtonElement>(`[data-region-tool="${active}"]`)?.focus();
    }
  };

  const showHistoryError = (message: string): void => {
    historyErrorEl.hidden = false;
    historyErrorEl.textContent = message;
    historyLimitEl.setAttribute("aria-invalid", "true");
  };

  const clearHistoryError = (): void => {
    historyErrorEl.hidden = true;
    historyErrorEl.textContent = "";
    historyLimitEl.removeAttribute("aria-invalid");
  };

  const renderHistory = (history: HistorySettings): void => {
    historySettings = history;
    historyLimitEl.value = String(history.limit);
    clearHistoryError();
    syncSwitch(historyEnabledEl, history.enabled);
  };

  const renderPin = (pin: PinSettings): void => {
    pinSettings = pin;
    syncSwitch(pinRestoreEl, pin.restore);
  };

  // R3:录制格式按钮每次重建(与美化预设一致),方向键漫游后还原焦点。
  const renderRecording = (settings: RecordingSettings): void => {
    recordingSettings = settings;
    syncSwitch(recordingEnabledEl, settings.enabled);
    const activeFormat =
      document.activeElement instanceof HTMLButtonElement &&
      recordingFormatsEl.contains(document.activeElement)
        ? (document.activeElement.dataset.recordingFormat ?? null)
        : null;
    recordingFormatsEl.replaceChildren();
    for (const item of RECORDING_FORMATS) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "choice";
      button.dataset.recordingFormat = item.value;
      button.setAttribute("role", "radio");
      button.textContent = t(item.labelKey);
      const selected = item.value === settings.format;
      button.setAttribute("aria-checked", selected ? "true" : "false");
      button.classList.toggle("selected", selected);
      // 开关关闭时格式仍可见；disabled 后点击与方向键都不改写格式。
      button.disabled = !settings.enabled;
      // 漫游 tabindex:与语言/预设组一致的 radio 组键盘行为。
      button.tabIndex = selected ? 0 : -1;
      recordingFormatsEl.append(button);
    }
    if (activeFormat) {
      recordingFormatsEl
        .querySelector<HTMLButtonElement>(`[data-recording-format="${activeFormat}"]`)
        ?.focus();
    }
  };

  const renderLanguage = (language: string): void => {
    languageRoot.querySelectorAll<HTMLButtonElement>("[data-language-value]").forEach((button) => {
      const selected = button.dataset.languageValue === language;
      button.setAttribute("aria-checked", selected ? "true" : "false");
      button.classList.toggle("selected", selected);
      // 漫游 tabindex:整组一个 Tab 停靠点,方向键在组内移动并选中。
      button.tabIndex = selected ? 0 : -1;
    });
  };

  const render = (settings: UiSettings): void => {
    lastSettings = settings;
    if (settings.notice) {
      noticeEl.hidden = false;
      noticeEl.textContent = settings.notice;
    } else {
      noticeEl.hidden = true;
      noticeEl.textContent = "";
    }

    const tray = settings.tray;
    if (tray && !tray.available) {
      trayNoticeEl.hidden = false;
      trayNoticeEl.textContent =
        tray.message ?? t("settings.tray.unavailable_fallback");
    } else {
      trayNoticeEl.hidden = true;
      trayNoticeEl.textContent = "";
    }

    renderLanguage(settings.language);

    hotkeyRoot.replaceChildren();
    for (const slot of HOTKEY_SLOTS) {
      const slotLabel = t(HOTKEY_LABEL_KEY[slot]);
      const optional = slot === "clipboardpin";
      const accelerator = hotkeyValue(settings.hotkeys, slot);
      const display =
        displayAccelerator(accelerator) || (optional ? t("settings.hotkeys.unbound") : "");
      const row = document.createElement("div");
      row.className = "hotkey-row";
      const errorText = hotkeyErrorText(hotkeyError(settings.hotkeyErrors, slot));
      if (errorText) {
        row.classList.add("has-error");
      }

      const label = document.createElement("div");
      label.className = "label";
      label.textContent = slotLabel;

      const button = document.createElement("button");
      button.type = "button";
      button.className = "hotkey-btn";
      button.dataset.mode = slot;
      button.dataset.tooltip = t("settings.hotkey.title");
      button.setAttribute(
        "aria-label",
        recording === slot
          ? t("settings.hotkey.aria_recording", { mode: slotLabel })
          : accelerator
            ? t("settings.hotkey.aria_current", {
                mode: slotLabel,
                accelerator: display,
              })
            : t("settings.hotkeys.unbound"),
      );
      button.textContent =
        recording === slot ? t("settings.hotkey.recording") : display;
      if (recording === slot) {
        button.classList.add("recording");
      }

      const error = document.createElement("p");
      error.className = "error";
      error.id = `hotkey-error-${slot}`;
      error.setAttribute("role", "alert");
      error.textContent = errorText;
      error.hidden = !errorText;
      if (errorText) {
        button.setAttribute("aria-describedby", error.id);
      }

      row.append(label);
      // 可选绑定的行在热键按钮旁提供清除按钮;CSSOM 写样式不受未来 CSP 的
      // style-src 限制,同时保持既有两列网格不变。
      const controls = document.createElement("div");
      controls.style.display = "flex";
      controls.style.alignItems = "center";
      controls.style.gap = "6px";
      controls.style.gridArea = "btn";
      controls.append(button);
      // R8:可选绑定支持显式清除;录制中不显示清除按钮,避免误触丢输入。
      if (optional && accelerator && recording !== slot) {
        const clear = document.createElement("button");
        clear.type = "button";
        clear.className = "choice";
        clear.dataset.hotkeyClear = slot;
        clear.dataset.tooltip = t("settings.hotkeys.clear_title");
        clear.textContent = t("settings.hotkeys.clear");
        controls.append(clear);
      }
      row.append(controls, error);
      hotkeyRoot.append(row);
    }

    switchEl.setAttribute("aria-checked", settings.autostart.enabled ? "true" : "false");
    switchEl.classList.toggle("on", settings.autostart.enabled);
    helpEl.textContent = autostartHelp(
      settings.autostart.enabled,
      settings.autostart.message,
    );
    helpEl.classList.toggle("error-text", Boolean(settings.autostart.message));

    renderCapture(settings.capture);
    renderRegionTools(settings.regionTools);
    renderHistory(settings.history);
    renderExport(settings);
    renderPin(settings.pin);
    renderRecording(settings.recording ?? { ...DEFAULT_RECORDING });
  };

  const showExportError = (message: string): void => {
    exportErrorEl.hidden = false;
    exportErrorEl.textContent = message;
  };

  const clearExportError = (): void => {
    exportErrorEl.hidden = true;
    exportErrorEl.textContent = "";
    paddingEl.removeAttribute("aria-invalid");
    radiusEl.removeAttribute("aria-invalid");
  };

  const renderExport = (settings: UiSettings): void => {
    const stored = settings.export?.beautify ?? DEFAULT_BEAUTIFY;
    const preset = BEAUTIFY_PRESETS.some((item) => item.id === stored.preset)
      ? stored.preset
      : DEFAULT_BEAUTIFY.preset;
    exportAppearance = {
      beautify: {
        preset,
        padding: clampInt(stored.padding, 0, 240, DEFAULT_BEAUTIFY.padding),
        radius: clampInt(stored.radius, 0, 160, DEFAULT_BEAUTIFY.radius),
        shadow: stored.shadow !== false,
      },
      filenameTemplate: settings.export?.filenameTemplate ?? "",
      applyBeautify: settings.export?.applyBeautify === true,
      useFilenameTemplate: settings.export?.useFilenameTemplate === true,
    };
    syncSwitch(applyBeautifyEl, exportAppearance.applyBeautify);
    syncSwitch(useFilenameTemplateEl, exportAppearance.useFilenameTemplate);
    if (document.activeElement !== templateEl) {
      templateEl.value = exportAppearance.filenameTemplate;
    }
    templateEl.disabled = !exportAppearance.useFilenameTemplate;
    if (document.activeElement !== paddingEl) {
      paddingEl.value = String(exportAppearance.beautify.padding);
    }
    if (document.activeElement !== radiusEl) {
      radiusEl.value = String(exportAppearance.beautify.radius);
    }
    paddingEl.disabled = !exportAppearance.applyBeautify;
    radiusEl.disabled = !exportAppearance.applyBeautify;
    shadowEl.disabled = !exportAppearance.applyBeautify;
    syncSwitch(shadowEl, exportAppearance.beautify.shadow);
    // 预设按钮每次重建:方向键漫游后焦点落在被替换节点上,这里记住并还原。
    const activePreset =
      document.activeElement instanceof HTMLButtonElement &&
      presetRoot.contains(document.activeElement)
        ? (document.activeElement.dataset.beautifyPreset ?? null)
        : null;
    presetRoot.replaceChildren();
    for (const item of BEAUTIFY_PRESETS) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "choice";
      button.dataset.beautifyPreset = item.id;
      button.setAttribute("role", "radio");
      button.textContent = t(item.labelKey);
      button.disabled = !exportAppearance.applyBeautify;
      const selected = item.id === exportAppearance.beautify.preset;
      button.setAttribute("aria-checked", selected ? "true" : "false");
      button.classList.toggle("selected", selected);
      // 漫游 tabindex:与语言组一致的 radio 组键盘行为。
      button.tabIndex = selected ? 0 : -1;
      const chip = document.createElement("span");
      chip.setAttribute("aria-hidden", "true");
      chip.style.display = "inline-block";
      chip.style.width = "10px";
      chip.style.height = "10px";
      chip.style.marginRight = "6px";
      chip.style.borderRadius = "999px";
      chip.style.verticalAlign = "-1px";
      chip.style.background = item.swatch;
      chip.style.boxShadow = "inset 0 0 0 1px rgba(0,0,0,0.18)";
      button.prepend(chip);
      presetRoot.append(button);
    }
    if (activePreset) {
      presetRoot
        .querySelector<HTMLButtonElement>(`[data-beautify-preset="${activePreset}"]`)
        ?.focus();
    }
  };

  const applyAppearance = async (next: ExportAppearance): Promise<void> => {
    setApplying(true);
    try {
      const settings = await invoke<UiSettings>("set_export_appearance", { appearance: next });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      setApplying(false);
    }
  };

  // invoke 失败给本地化提示,后接后端细节(ADR-5);不直出英文原文。
  const showInvokeError = (error: unknown): void => {
    const detail = error instanceof Error ? error.message : String(error);
    noticeEl.hidden = false;
    noticeEl.textContent = t("settings.error.invoke_failed", { detail });
  };

  const refresh = async (): Promise<void> => {
    try {
      const settings = await invoke<UiSettings>("get_ui_settings");
      render(settings);
    } catch (error) {
      showInvokeError(error);
    }
  };

  const applyHotkey = async (slot: HotkeySlot, accelerator: string): Promise<void> => {
    setApplying(true);
    try {
      // R8:剪贴板贴图热键走独立字段与命令,空串表示清除绑定。
      const settings =
        slot === "clipboardpin"
          ? await invoke<UiSettings>("set_pin_clipboard_hotkey", { accelerator })
          : await invoke<UiSettings>("set_hotkey", { mode: slot, accelerator });
      recording = null;
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      setApplying(false);
    }
  };

  const applyRegionTools = async (next: RegionTools): Promise<void> => {
    setApplying(true);
    try {
      const settings = await invoke<UiSettings>("set_region_tools", { tools: next });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      setApplying(false);
    }
  };

  const applyCapture = async (next: CaptureSettings): Promise<void> => {
    setApplying(true);
    try {
      const settings = await invoke<UiSettings>("set_capture_settings", {
        settings: next,
      });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      setApplying(false);
    }
  };

  const applyHistory = async (next: HistorySettings): Promise<void> => {
    setApplying(true);
    try {
      const settings = await invoke<UiSettings>("set_history_settings", {
        settings: next,
      });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      setApplying(false);
    }
  };

  const applyPin = async (next: PinSettings): Promise<void> => {
    setApplying(true);
    try {
      const settings = await invoke<UiSettings>("set_pin_settings", { settings: next });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      setApplying(false);
    }
  };

  // R3:录屏开关与录制格式;托盘入口按新值立即重建,选区入口打开时读取。
  const applyRecording = async (next: RecordingSettings): Promise<void> => {
    setApplying(true);
    try {
      const settings = await invoke<UiSettings>("set_recording_settings", { settings: next });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      setApplying(false);
    }
  };

  const applyLanguageSetting = async (language: LanguageSetting): Promise<void> => {
    setApplying(true);
    try {
      const settings = await invoke<UiSettings>("set_language", { language });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      setApplying(false);
    }
  };

  const commitDelay = (): void => {
    const raw = delayEl.value.trim();
    if (!/^\d+$/.test(raw)) {
      showDelayError(t("settings.capture.delay_error"));
      return;
    }
    const seconds = Number(raw);
    if (!Number.isSafeInteger(seconds) || seconds < 0 || seconds > 60) {
      showDelayError(t("settings.capture.delay_error"));
      return;
    }
    clearDelayError();
    if (seconds === captureSettings.delaySeconds) {
      return;
    }
    void applyCapture({ ...captureSettings, delaySeconds: seconds });
  };

  delayEl.addEventListener("change", () => {
    if (!applying) {
      commitDelay();
    }
  });
  delayEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      delayEl.blur();
    }
  });

  captureCursorEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = captureCursorEl.getAttribute("aria-checked") !== "true";
    void applyCapture({ ...captureSettings, captureCursor: next });
  });

  multiMonitorEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = multiMonitorEl.getAttribute("aria-checked") !== "true";
    void applyCapture({ ...captureSettings, multiMonitor: next });
  });

  longCaptureEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = longCaptureEl.getAttribute("aria-checked") !== "true";
    void applyCapture({ ...captureSettings, longCapture: next });
  });

  regionToolsEl.addEventListener("click", (event) => {
    if (applying) {
      return;
    }
    const target = event.target;
    if (!(target instanceof Element)) {
      return;
    }
    const button = target.closest("[data-region-tool]");
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    const id = button.dataset.regionTool as keyof RegionTools | undefined;
    if (!id || !(id in regionTools)) {
      return;
    }
    void applyRegionTools({ ...regionTools, [id]: !regionTools[id] });
  });

  const commitHistoryLimit = (): void => {
    const raw = historyLimitEl.value.trim();
    if (!/^\d+$/.test(raw)) {
      showHistoryError(t("settings.history.limit_error"));
      return;
    }
    const limit = Number(raw);
    if (!Number.isSafeInteger(limit) || limit < 5 || limit > 200) {
      showHistoryError(t("settings.history.limit_error"));
      return;
    }
    clearHistoryError();
    if (limit === historySettings.limit) {
      return;
    }
    void applyHistory({ ...historySettings, limit });
  };

  historyLimitEl.addEventListener("change", () => {
    if (!applying) {
      commitHistoryLimit();
    }
  });
  historyLimitEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      historyLimitEl.blur();
    }
  });

  historyEnabledEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = historyEnabledEl.getAttribute("aria-checked") !== "true";
    void applyHistory({ ...historySettings, enabled: next });
  });

  historyOpenEl.addEventListener("click", () => {
    void invoke("open_history").catch(showInvokeError);
  });

  openGuideEl.addEventListener("click", () => {
    void invoke("open_guide").catch(showInvokeError);
  });

  pinRestoreEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = pinRestoreEl.getAttribute("aria-checked") !== "true";
    void applyPin({ ...pinSettings, restore: next });
  });

  recordingEnabledEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = recordingEnabledEl.getAttribute("aria-checked") !== "true";
    void applyRecording({ ...recordingSettings, enabled: next });
  });

  recordingFormatsEl.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element) || applying || !recordingSettings.enabled) {
      return;
    }
    const button = target.closest("[data-recording-format]");
    if (!(button instanceof HTMLButtonElement) || button.disabled) {
      return;
    }
    const format = button.dataset.recordingFormat as RecordingFormat | undefined;
    if (!format || format === recordingSettings.format) {
      return;
    }
    void applyRecording({ ...recordingSettings, format });
  });

  let logDirectory = "";
  logPathEl.style.overflowWrap = "anywhere";
  logPathEl.style.userSelect = "text";
  const renderLogPath = (): void => {
    logPathEl.textContent = logDirectory.trim() ? logDirectory : t("settings.logs.unavailable");
  };
  const loadLogDirectory = (): void => {
    void invoke<string>("log_directory")
      .then((path) => {
        logDirectory = path;
        renderLogPath();
      })
      .catch(() => {
        logDirectory = "";
        renderLogPath();
      });
  };
  loadLogDirectory();
  openLogsEl.addEventListener("click", () => {
    void invoke("open_log_directory").catch(() => {
      noticeEl.hidden = false;
      noticeEl.textContent = t("settings.logs.open_failed");
    });
  });

  applyBeautifyEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = applyBeautifyEl.getAttribute("aria-checked") !== "true";
    void applyAppearance({ ...exportAppearance, applyBeautify: next });
  });

  useFilenameTemplateEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = useFilenameTemplateEl.getAttribute("aria-checked") !== "true";
    void applyAppearance({ ...exportAppearance, useFilenameTemplate: next });
  });

  const commitTemplate = (): void => {
    if (!exportAppearance.useFilenameTemplate) {
      return;
    }
    const next = templateEl.value;
    if (next === exportAppearance.filenameTemplate) {
      return;
    }
    void applyAppearance({ ...exportAppearance, filenameTemplate: next });
  };

  templateEl.addEventListener("change", () => {
    if (!applying) {
      commitTemplate();
    }
  });

  const commitBeautifyNumber = (
    input: HTMLInputElement,
    field: "padding" | "radius",
    max: number,
    errorKey: "settings.export.padding_error" | "settings.export.radius_error",
  ): void => {
    if (!exportAppearance.applyBeautify) {
      return;
    }
    const raw = input.value.trim();
    if (!/^\d+$/.test(raw)) {
      input.setAttribute("aria-invalid", "true");
      showExportError(t(errorKey));
      return;
    }
    const value = Number(raw);
    if (!Number.isSafeInteger(value) || value < 0 || value > max) {
      input.setAttribute("aria-invalid", "true");
      showExportError(t(errorKey));
      return;
    }
    clearExportError();
    if (value === exportAppearance.beautify[field]) {
      return;
    }
    void applyAppearance({
      ...exportAppearance,
      beautify: { ...exportAppearance.beautify, [field]: value },
    });
  };

  paddingEl.addEventListener("change", () => {
    if (!applying) {
      commitBeautifyNumber(paddingEl, "padding", 240, "settings.export.padding_error");
    }
  });
  radiusEl.addEventListener("change", () => {
    if (!applying) {
      commitBeautifyNumber(radiusEl, "radius", 160, "settings.export.radius_error");
    }
  });
  for (const input of [paddingEl, radiusEl, templateEl]) {
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        input.blur();
      }
    });
  }

  shadowEl.addEventListener("click", () => {
    if (applying || shadowEl.disabled) {
      return;
    }
    void applyAppearance({
      ...exportAppearance,
      beautify: { ...exportAppearance.beautify, shadow: shadowEl.getAttribute("aria-checked") !== "true" },
    });
  });

  presetRoot.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element) || applying) {
      return;
    }
    const button = target.closest("[data-beautify-preset]");
    if (!(button instanceof HTMLButtonElement) || button.disabled) {
      return;
    }
    const preset = button.dataset.beautifyPreset;
    if (!preset || preset === exportAppearance.beautify.preset) {
      return;
    }
    clearExportError();
    void applyAppearance({
      ...exportAppearance,
      beautify: { ...exportAppearance.beautify, preset },
    });
  });

  closeEl.addEventListener("click", () => {
    void getCurrentWindow().close();
  });

  quitEl.addEventListener("click", () => {
    void invoke("quit_app").catch(showInvokeError);
  });

  languageRoot.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element) || applying) {
      return;
    }
    const button = target.closest("[data-language-value]");
    if (!(button instanceof HTMLButtonElement) || button.disabled) {
      return;
    }
    const value = button.dataset.languageValue as LanguageSetting | undefined;
    if (!value || value === lastSettings?.language) {
      return;
    }
    void applyLanguageSetting(value);
  });

  // radio 组方向键漫游(R6):方向键/Home/End 移动焦点并选中目标项,
  // 选中走既有点击委托,行为与指针完全一致。
  const handleRadioGroupKeydown = (
    event: KeyboardEvent,
    container: HTMLElement,
    selector: string,
  ): void => {
    const keys = ["ArrowLeft", "ArrowUp", "ArrowRight", "ArrowDown", "Home", "End"];
    if (!keys.includes(event.key)) {
      return;
    }
    const buttons = Array.from(
      container.querySelectorAll<HTMLButtonElement>(selector),
    ).filter((button) => !button.disabled);
    if (buttons.length === 0) {
      return;
    }
    event.preventDefault();
    const currentIndex = buttons.indexOf(document.activeElement as HTMLButtonElement);
    let nextIndex: number;
    if (event.key === "Home") {
      nextIndex = 0;
    } else if (event.key === "End") {
      nextIndex = buttons.length - 1;
    } else {
      const delta = event.key === "ArrowLeft" || event.key === "ArrowUp" ? -1 : 1;
      nextIndex =
        currentIndex < 0
          ? delta > 0
            ? 0
            : buttons.length - 1
          : (currentIndex + delta + buttons.length) % buttons.length;
    }
    const target = buttons[nextIndex];
    target.focus();
    target.click();
  };

  languageRoot.addEventListener("keydown", (event) => {
    handleRadioGroupKeydown(event, languageRoot, "[data-language-value]");
  });
  presetRoot.addEventListener("keydown", (event) => {
    handleRadioGroupKeydown(event, presetRoot, "[data-beautify-preset]");
  });
  recordingFormatsEl.addEventListener("keydown", (event) => {
    if (!recordingSettings.enabled) {
      return;
    }
    handleRadioGroupKeydown(event, recordingFormatsEl, "[data-recording-format]");
  });

  switchEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = switchEl.getAttribute("aria-checked") !== "true";
    setApplying(true);
    void invoke<UiSettings>("set_autostart_enabled", { enabled: next })
      .then(render)
      .catch(showInvokeError)
      .finally(() => {
        setApplying(false);
      });
  });

  hotkeyRoot.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element) || applying) {
      return;
    }
    const clearButton = target.closest("[data-hotkey-clear]");
    if (clearButton instanceof HTMLButtonElement && clearButton.dataset.hotkeyClear) {
      void applyHotkey(clearButton.dataset.hotkeyClear as HotkeySlot, "");
      return;
    }
    const button = target.closest("[data-mode]");
    if (!(button instanceof HTMLButtonElement) || !button.dataset.mode) {
      return;
    }
    recording = button.dataset.mode as HotkeySlot;
    void refresh();
  });

  window.addEventListener("keydown", (event) => {
    if (!recording) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    if (event.repeat) {
      return;
    }
    if (event.key === "Escape") {
      recording = null;
      void refresh();
      return;
    }
    const accelerator = acceleratorFromEvent(event);
    if (!accelerator) {
      return;
    }
    void applyHotkey(recording, accelerator);
  });

  // R19:关于组显示当前版本;读取失败时保持占位符,不阻塞其余设置。
  if (versionEl instanceof HTMLElement) {
    void getVersion()
      .then((version) => {
        versionEl.textContent = version;
      })
      .catch(() => {
        versionEl.textContent = "—";
      });
  }

  void refresh();

  // 语言切换:静态标签由 main 的 applyTranslations 更新;这里先按当前状态
  // 重渲染动态行(热键/开关/历史/语言选项/美化预设),再从后端重取一次
  // (热键错误/开机启动/无托盘提示由后端按新语言重新解析)。
  return () => {
    renderLogPath();
    if (lastSettings) {
      render(lastSettings);
    }
    void refresh();
  };
}

export function acceleratorFromEvent(event: KeyboardEvent): string | null {
  if (["Shift", "Control", "Alt", "Meta"].includes(event.key)) {
    return null;
  }
  const key = codeToKey(event.code);
  if (!key) {
    return null;
  }
  const parts: string[] = [];
  if (event.ctrlKey) {
    parts.push("Ctrl");
  }
  if (event.altKey) {
    parts.push("Alt");
  }
  if (event.shiftKey) {
    parts.push("Shift");
  }
  if (event.metaKey) {
    parts.push("Super");
  }
  parts.push(key);
  return parts.join("+");
}

export function displayAccelerator(accelerator: string): string {
  const platform = navigator.platform;
  const superLabel = platform.includes("Mac")
    ? "Cmd"
    : platform.includes("Win")
      ? "Win"
      : "Super";
  return accelerator.replaceAll("Super", superLabel).replaceAll("Meta", superLabel);
}

function codeToKey(code: string): string | null {
  if (/^Key[A-Z]$/.test(code)) {
    return code.slice(3);
  }
  if (/^Digit[0-9]$/.test(code)) {
    return code.slice(5);
  }
  if (/^F([1-9]|1[0-2])$/.test(code)) {
    return code;
  }
  const extra: Record<string, string> = {
    PrintScreen: "PrintScreen",
    Space: "Space",
    Minus: "-",
    Equal: "=",
    BracketLeft: "[",
    BracketRight: "]",
    Backslash: "\\",
    Semicolon: ";",
    Quote: "'",
    Comma: ",",
    Period: ".",
    Slash: "/",
    Backquote: "`",
    ArrowUp: "Up",
    ArrowDown: "Down",
    ArrowLeft: "Left",
    ArrowRight: "Right",
    Escape: "Esc",
    Tab: "Tab",
    Enter: "Enter",
    Backspace: "Backspace",
    Delete: "Delete",
    Home: "Home",
    End: "End",
    PageUp: "PageUp",
    PageDown: "PageDown",
  };
  return extra[code] ?? null;
}
