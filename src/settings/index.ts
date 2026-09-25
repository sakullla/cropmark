import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { autostartHelp, hotkeyErrorText } from "../errors";
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

/// R19:功能开关(与 Rust `FeatureToggles` 字段一一对应);默认值由后端给出,
/// 前端只渲染并回写,不自行决定默认。
export interface FeatureToggles {
  longCapture: boolean;
  pinEnhance: boolean;
  pinRestore: boolean;
  exportBeautify: boolean;
  captureCursor: boolean;
  historyTools: boolean;
  clipboardPin: boolean;
  multiMonitor: boolean;
  ocrPanel: boolean;
  onboarding: boolean;
  filenameTemplate: boolean;
}

/// R19:标注工具逐项开关(工具 id → 是否启用);被合并工具不在表中。
export type AnnotationToolToggles = Record<string, boolean>;

export interface CaptureSettings {
  delaySeconds: number;
}

export interface HistorySettings {
  enabled: boolean;
  limit: number;
}

export interface TrayState {
  available: boolean;
  message: string | null;
}

export type LanguageSetting = "system" | "zh-CN" | "en";

export interface UiSettings {
  hotkeys: Hotkeys;
  hotkeyErrors: HotkeyErrors;
  autostart: AutostartState;
  notice: string | null;
  toggles: FeatureToggles;
  annotationTools: AnnotationToolToggles;
  capture: CaptureSettings;
  history: HistorySettings;
  tray: TrayState;
  language: string;
  resolvedLanguage: string;
}

type ToggleKey = keyof FeatureToggles;

const TOGGLE_ITEMS: Record<ToggleKey, { labelKey: CatalogKey; hintKey: CatalogKey }> = {
  longCapture: {
    labelKey: "settings.toggle.long_capture_label",
    hintKey: "settings.toggle.long_capture_hint",
  },
  pinEnhance: {
    labelKey: "settings.toggle.pin_enhance_label",
    hintKey: "settings.toggle.pin_enhance_hint",
  },
  pinRestore: {
    labelKey: "settings.toggle.pin_restore_label",
    hintKey: "settings.toggle.pin_restore_hint",
  },
  exportBeautify: {
    labelKey: "settings.toggle.export_beautify_label",
    hintKey: "settings.toggle.export_beautify_hint",
  },
  captureCursor: {
    labelKey: "settings.toggle.capture_cursor_label",
    hintKey: "settings.toggle.capture_cursor_hint",
  },
  historyTools: {
    labelKey: "settings.toggle.history_tools_label",
    hintKey: "settings.toggle.history_tools_hint",
  },
  clipboardPin: {
    labelKey: "settings.toggle.clipboard_pin_label",
    hintKey: "settings.toggle.clipboard_pin_hint",
  },
  multiMonitor: {
    labelKey: "settings.toggle.multi_monitor_label",
    hintKey: "settings.toggle.multi_monitor_hint",
  },
  ocrPanel: {
    labelKey: "settings.toggle.ocr_panel_label",
    hintKey: "settings.toggle.ocr_panel_hint",
  },
  onboarding: {
    labelKey: "settings.toggle.onboarding_label",
    hintKey: "settings.toggle.onboarding_hint",
  },
  filenameTemplate: {
    labelKey: "settings.toggle.filename_template_label",
    hintKey: "settings.toggle.filename_template_hint",
  },
};

// R19:新增功能开关集中在「通用 → 功能开关 → 功能」;采集组只放包含鼠标指针,
// 记录与输出组只放保存文件名模板(与 R19 的归属一致)。
const GENERAL_TOGGLES: ToggleKey[] = [
  "longCapture",
  "pinEnhance",
  "pinRestore",
  "exportBeautify",
  "historyTools",
  "clipboardPin",
  "multiMonitor",
  "ocrPanel",
  "onboarding",
];

const CAPTURE_TOGGLES: ToggleKey[] = ["captureCursor"];
const OUTPUT_TOGGLES: ToggleKey[] = ["filenameTemplate"];

// R19:标注工具展示顺序;后端只回传开关表,顺序在这里固定。
const TOOL_ITEMS: Array<{ id: string; labelKey: CatalogKey }> = [
  { id: "rect", labelKey: "settings.tool.rect" },
  { id: "ellipse", labelKey: "settings.tool.ellipse" },
  { id: "arrow", labelKey: "settings.tool.arrow" },
  { id: "text", labelKey: "settings.tool.text" },
  { id: "number", labelKey: "settings.tool.number" },
  { id: "highlighter", labelKey: "settings.tool.highlighter" },
  { id: "mosaic", labelKey: "settings.tool.mosaic" },
  { id: "spotlight", labelKey: "settings.tool.spotlight" },
  { id: "magnifier", labelKey: "settings.tool.magnifier" },
  { id: "bubble", labelKey: "settings.tool.bubble" },
  { id: "sticker", labelKey: "settings.tool.sticker" },
  { id: "erase", labelKey: "settings.tool.erase" },
];

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

function hotkeyValue(hotkeys: Hotkeys, slot: HotkeySlot): string {
  return slot === "clipboardpin" ? hotkeys.pinClipboard : hotkeys[slot];
}

function hotkeyError(errors: HotkeyErrors, slot: HotkeySlot): string | null {
  return slot === "clipboardpin" ? errors.pinClipboard : errors[slot];
}

export function mountSettings(root: HTMLElement): () => void {
  root.innerHTML = `
    <div class="shell">
      <header class="titlebar" data-tauri-drag-region>
        <div class="brand" data-tauri-drag-region>
          <span class="mark" aria-hidden="true"></span>
          <span class="name">Cropmark</span>
        </div>
        <button type="button" class="icon-btn" data-action="close" data-i18n-aria-label="settings.close" aria-label="关闭">${icons.close}</button>
      </header>
      <main class="content">
        <p class="notice" role="alert" hidden></p>
        <p class="notice tray-notice" data-tray-notice role="status" hidden></p>
        <section class="card group" aria-labelledby="group-capture-title">
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
            <div class="rows feature-rows" data-toggles="capture"></div>
          </section>
        </section>
        <section class="card group" aria-labelledby="group-output-title">
          <h1 id="group-output-title" data-i18n="settings.group.output">记录与输出</h1>
          <section class="block" aria-labelledby="history-title">
            <h2 id="history-title" data-i18n="settings.history.title">历史记录</h2>
            <p class="hint" data-i18n="settings.history.hint">截图完成后在本机保留最近记录，可重新复制、贴图或删除；数据只保存在本机。</p>
            <div class="setting-row">
              <div>
                <div class="label" id="history-enabled-label" data-i18n="settings.history.enabled_label">保留截图历史</div>
                <p class="hint" data-i18n="settings.history.enabled_hint">关闭后不再新增记录；已有记录保留，可在历史窗口清空。</p>
              </div>
              <button type="button" class="switch" data-history="enabled" role="switch" aria-checked="true" aria-labelledby="history-enabled-label">
                <span class="knob"></span>
              </button>
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
            <div class="rows feature-rows" data-toggles="output"></div>
          </section>
        </section>
        <section class="card group" aria-labelledby="group-general-title">
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
          <section class="block" aria-labelledby="toggles-title">
            <h2 id="toggles-title" data-i18n="settings.section.toggles">功能开关</h2>
            <p class="hint" data-i18n="settings.section.toggles_hint">逐项控制功能入口；关闭只停用入口与新增行为，已有数据与已创建标注保留，关闭状态跨重启保持。</p>
            <h3 class="subheading" data-i18n="settings.section.features">功能</h3>
            <div class="rows feature-rows" data-toggles="general"></div>
            <h3 class="subheading" data-i18n="settings.section.tools">标注工具</h3>
            <p class="hint" data-i18n="settings.section.tools_hint">只控制工具栏与更多面板中的创建入口；撤销/重做、删除、复制/保存/贴图/取字与已创建标注始终可用。</p>
            <div class="tool-grid" data-tools></div>
          </section>
        </section>
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
      </main>
    </div>
  `;

  const noticeEl = root.querySelector(".notice");
  const hotkeyRoot = root.querySelector("[data-hotkeys]");
  const captureToggleRoot = root.querySelector("[data-toggles=capture]");
  const outputToggleRoot = root.querySelector("[data-toggles=output]");
  const generalToggleRoot = root.querySelector("[data-toggles=general]");
  const toolRoot = root.querySelector("[data-tools]");
  const helpEl = root.querySelector(".autostart-help");
  const switchEl = root.querySelector("[data-action=autostart]");
  const closeEl = root.querySelector("[data-action=close]");
  const delayEl = root.querySelector("[data-capture=delay]");
  const delayErrorEl = root.querySelector("[data-capture-error]");
  const historyEnabledEl = root.querySelector("[data-history=enabled]");
  const historyLimitEl = root.querySelector("[data-history=limit]");
  const historyErrorEl = root.querySelector("[data-history-error]");
  const historyOpenEl = root.querySelector("[data-action=open-history]");
  const trayNoticeEl = root.querySelector("[data-tray-notice]");
  const quitEl = root.querySelector("[data-action=quit]");
  const languageRoot = root.querySelector("[data-language]");
  const versionEl = root.querySelector("[data-version]");
  if (
    !(noticeEl instanceof HTMLElement) ||
    !(hotkeyRoot instanceof HTMLElement) ||
    !(captureToggleRoot instanceof HTMLElement) ||
    !(outputToggleRoot instanceof HTMLElement) ||
    !(generalToggleRoot instanceof HTMLElement) ||
    !(toolRoot instanceof HTMLElement) ||
    !(helpEl instanceof HTMLElement) ||
    !(switchEl instanceof HTMLButtonElement) ||
    !(closeEl instanceof HTMLButtonElement) ||
    !(delayEl instanceof HTMLInputElement) ||
    !(delayErrorEl instanceof HTMLElement) ||
    !(historyEnabledEl instanceof HTMLButtonElement) ||
    !(historyLimitEl instanceof HTMLInputElement) ||
    !(historyErrorEl instanceof HTMLElement) ||
    !(historyOpenEl instanceof HTMLButtonElement) ||
    !(trayNoticeEl instanceof HTMLElement) ||
    !(quitEl instanceof HTMLButtonElement) ||
    !(languageRoot instanceof HTMLElement)
  ) {
    return () => undefined;
  }

  let recording: HotkeySlot | null = null;
  let applying = false;
  let lastSettings: UiSettings | null = null;
  let captureSettings: CaptureSettings = {
    delaySeconds: 0,
  };
  let historySettings: HistorySettings = {
    enabled: true,
    limit: 20,
  };

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
    historyEnabledEl.setAttribute("aria-checked", history.enabled ? "true" : "false");
    historyEnabledEl.classList.toggle("on", history.enabled);
  };

  const renderLanguage = (language: string): void => {
    languageRoot.querySelectorAll<HTMLButtonElement>("[data-language-value]").forEach((button) => {
      const selected = button.dataset.languageValue === language;
      button.setAttribute("aria-checked", selected ? "true" : "false");
      button.classList.toggle("selected", selected);
    });
  };

  const makeToggleButton = (
    datasetKey: "toggleFeature" | "toggleTool",
    value: string,
    labelId: string,
    enabled: boolean,
  ): HTMLButtonElement => {
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "switch";
    toggle.dataset[datasetKey] = value;
    toggle.setAttribute("role", "switch");
    toggle.setAttribute("aria-checked", enabled ? "true" : "false");
    toggle.setAttribute("aria-labelledby", labelId);
    const knob = document.createElement("span");
    knob.className = "knob";
    toggle.appendChild(knob);
    if (enabled) {
      toggle.classList.add("on");
    }
    return toggle;
  };

  const renderFeatureToggles = (
    container: HTMLElement,
    keys: ToggleKey[],
    toggles: FeatureToggles,
  ): void => {
    container.replaceChildren();
    for (const key of keys) {
      const meta = TOGGLE_ITEMS[key];
      const enabled = toggles[key];
      const row = document.createElement("div");
      row.className = "feature-row";

      const text = document.createElement("div");
      const label = document.createElement("div");
      label.className = "label";
      label.id = `feature-label-${key}`;
      label.textContent = t(meta.labelKey);
      const hint = document.createElement("p");
      hint.className = "hint";
      hint.textContent = t(meta.hintKey);
      text.append(label, hint);

      row.append(text, makeToggleButton("toggleFeature", key, label.id, enabled));
      container.append(row);
    }
  };

  const renderToolToggles = (
    container: HTMLElement,
    tools: AnnotationToolToggles,
  ): void => {
    container.replaceChildren();
    for (const item of TOOL_ITEMS) {
      const enabled = tools[item.id] === true;
      const row = document.createElement("div");
      row.className = "tool-row";

      const label = document.createElement("div");
      label.className = "label";
      label.id = `tool-label-${item.id}`;
      label.textContent = t(item.labelKey);

      row.append(label, makeToggleButton("toggleTool", item.id, label.id, enabled));
      container.append(row);
    }
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

    renderFeatureToggles(captureToggleRoot, CAPTURE_TOGGLES, settings.toggles);
    renderFeatureToggles(outputToggleRoot, OUTPUT_TOGGLES, settings.toggles);
    renderFeatureToggles(generalToggleRoot, GENERAL_TOGGLES, settings.toggles);
    renderToolToggles(toolRoot, settings.annotationTools);

    switchEl.setAttribute("aria-checked", settings.autostart.enabled ? "true" : "false");
    switchEl.classList.toggle("on", settings.autostart.enabled);
    helpEl.textContent = autostartHelp(
      settings.autostart.enabled,
      settings.autostart.message,
    );
    helpEl.classList.toggle("error-text", Boolean(settings.autostart.message));

    renderCapture(settings.capture);
    renderHistory(settings.history);
  };

  const showInvokeError = (error: unknown): void => {
    noticeEl.hidden = false;
    noticeEl.textContent = error instanceof Error ? error.message : String(error);
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
    applying = true;
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
      applying = false;
    }
  };

  const applyCapture = async (next: CaptureSettings): Promise<void> => {
    applying = true;
    try {
      const settings = await invoke<UiSettings>("set_capture_settings", {
        settings: next,
      });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      applying = false;
    }
  };

  const applyHistory = async (next: HistorySettings): Promise<void> => {
    applying = true;
    try {
      const settings = await invoke<UiSettings>("set_history_settings", {
        settings: next,
      });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      applying = false;
    }
  };

  const applyLanguageSetting = async (language: LanguageSetting): Promise<void> => {
    applying = true;
    try {
      const settings = await invoke<UiSettings>("set_language", { language });
      render(settings);
    } catch (error) {
      showInvokeError(error);
    } finally {
      applying = false;
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

  switchEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = switchEl.getAttribute("aria-checked") !== "true";
    applying = true;
    void invoke<UiSettings>("set_autostart_enabled", { enabled: next })
      .then(render)
      .catch(showInvokeError)
      .finally(() => {
        applying = false;
      });
  });

  // R19:功能开关与标注工具逐项开关共用一套点击委托;写回后按后端返回的
  // 新设置整页重渲染,保证开关状态与实际持久化值一致。
  root.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element) || applying) {
      return;
    }
    const featureButton = target.closest("[data-toggle-feature]");
    if (featureButton instanceof HTMLButtonElement && featureButton.dataset.toggleFeature) {
      const key = featureButton.dataset.toggleFeature as ToggleKey;
      const next = featureButton.getAttribute("aria-checked") !== "true";
      applying = true;
      void invoke<UiSettings>("set_feature", { key, enabled: next })
        .then(render)
        .catch(showInvokeError)
        .finally(() => {
          applying = false;
        });
      return;
    }
    const toolButton = target.closest("[data-toggle-tool]");
    if (toolButton instanceof HTMLButtonElement && toolButton.dataset.toggleTool) {
      const tool = toolButton.dataset.toggleTool;
      const next = toolButton.getAttribute("aria-checked") !== "true";
      applying = true;
      void invoke<UiSettings>("set_annotation_tool", { tool, enabled: next })
        .then(render)
        .catch(showInvokeError)
        .finally(() => {
          applying = false;
        });
    }
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
  // 重渲染动态行(热键/开关/工具/历史/语言选项),再从后端重取一次
  // (热键错误/开机启动/无托盘提示由后端按新语言重新解析)。
  return () => {
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
