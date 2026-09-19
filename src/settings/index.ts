import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { autostartHelp, hotkeyErrorText } from "../errors";
import { t, type CatalogKey } from "../i18n";

export type CaptureMode = "region" | "window" | "fullscreen";

export interface Hotkeys {
  region: string;
  window: string;
  fullscreen: string;
}

export interface HotkeyErrors {
  region: string | null;
  window: string | null;
  fullscreen: string | null;
}

export interface AutostartState {
  enabled: boolean;
  message: string | null;
}

export interface FeatureSettings {
  ocrEntry: boolean;
  pinEntry: boolean;
  magnifier: boolean;
  toolbarCopy: boolean;
  toolbarSave: boolean;
  toolbarPin: boolean;
  cursorHints: boolean;
  lastRegion: boolean;
  ocrOrientation: boolean;
  inlineAnnotation: boolean;
}

export type FinishAction = "preview" | "quiet";

export interface CaptureSettings {
  delaySeconds: number;
  autoCopy: boolean;
  finishAction: FinishAction;
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
  features: FeatureSettings;
  capture: CaptureSettings;
  history: HistorySettings;
  tray: TrayState;
  language: string;
  resolvedLanguage: string;
}

type FeatureKey = keyof FeatureSettings;

const FEATURE_ITEMS: Array<{ key: FeatureKey; labelKey: CatalogKey; hintKey: CatalogKey }> = [
  {
    key: "ocrEntry",
    labelKey: "settings.feature.ocr_label",
    hintKey: "settings.feature.ocr_hint",
  },
  {
    key: "pinEntry",
    labelKey: "settings.feature.pin_label",
    hintKey: "settings.feature.pin_hint",
  },
  {
    key: "magnifier",
    labelKey: "settings.feature.magnifier_label",
    hintKey: "settings.feature.magnifier_hint",
  },
  {
    key: "toolbarCopy",
    labelKey: "settings.feature.toolbar_copy_label",
    hintKey: "settings.feature.toolbar_copy_hint",
  },
  {
    key: "toolbarSave",
    labelKey: "settings.feature.toolbar_save_label",
    hintKey: "settings.feature.toolbar_save_hint",
  },
  {
    key: "toolbarPin",
    labelKey: "settings.feature.toolbar_pin_label",
    hintKey: "settings.feature.toolbar_pin_hint",
  },
  {
    key: "cursorHints",
    labelKey: "settings.feature.cursor_hints_label",
    hintKey: "settings.feature.cursor_hints_hint",
  },
  {
    key: "lastRegion",
    labelKey: "settings.feature.last_region_label",
    hintKey: "settings.feature.last_region_hint",
  },
  {
    key: "ocrOrientation",
    labelKey: "settings.feature.ocr_orientation_label",
    hintKey: "settings.feature.ocr_orientation_hint",
  },
  {
    key: "inlineAnnotation",
    labelKey: "settings.feature.inline_annotation_label",
    hintKey: "settings.feature.inline_annotation_hint",
  },
];

const MODE_LABEL_KEY: Record<CaptureMode, CatalogKey> = {
  region: "settings.mode.region",
  window: "settings.mode.window",
  fullscreen: "settings.mode.fullscreen",
};

const LANGUAGE_OPTIONS: Array<{ value: LanguageSetting; labelKey: CatalogKey }> = [
  { value: "system", labelKey: "language.system" },
  { value: "zh-CN", labelKey: "language.zh_cn" },
  { value: "en", labelKey: "language.en" },
];

const MODES: CaptureMode[] = ["region", "window", "fullscreen"];

export function mountSettings(root: HTMLElement): () => void {
  root.innerHTML = `
    <div class="shell">
      <header class="titlebar" data-tauri-drag-region>
        <div class="brand" data-tauri-drag-region>
          <span class="mark" aria-hidden="true"></span>
          <span class="name">Cropmark</span>
        </div>
        <button type="button" class="icon-btn" data-action="close" data-i18n-aria-label="settings.close" aria-label="关闭">×</button>
      </header>
      <main class="content">
        <p class="notice" role="alert" hidden></p>
        <p class="notice tray-notice" data-tray-notice role="status" hidden></p>
        <section class="card" aria-labelledby="hotkeys-title">
          <h1 id="hotkeys-title" data-i18n="settings.hotkeys.title">热键</h1>
          <p class="hint" data-i18n="settings.hotkeys.hint">点击热键按钮后按下新组合，Esc 取消；改动立即生效。</p>
          <div class="rows" data-hotkeys></div>
        </section>
        <section class="card" aria-labelledby="capture-title">
          <h1 id="capture-title" data-i18n="settings.capture.title">截图</h1>
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
              <div class="label" id="autocopy-label" data-i18n="settings.capture.autocopy_label">完成后自动复制</div>
              <p class="hint" data-i18n="settings.capture.autocopy_hint">关闭后截图完成不写入剪贴板；预览内手动复制不受影响。</p>
            </div>
            <button type="button" class="switch" data-capture="auto-copy" role="switch" aria-checked="true" aria-labelledby="autocopy-label">
              <span class="knob"></span>
            </button>
          </div>
          <div class="setting-row">
            <div>
              <div class="label" id="finish-label" data-i18n="settings.capture.finish_label">完成后动作</div>
              <p class="hint" data-i18n="settings.capture.finish_hint">静默完成会直接复制并给出提示，不打开预览；需自动复制开启。</p>
            </div>
            <div class="choices" data-capture="finish" role="radiogroup" aria-labelledby="finish-label">
              <button type="button" class="choice" role="radio" data-finish-action="preview" aria-checked="true" data-i18n="settings.capture.finish_preview">预览</button>
              <button type="button" class="choice" role="radio" data-finish-action="quiet" aria-checked="false" data-i18n="settings.capture.finish_quiet">静默完成</button>
            </div>
          </div>
        </section>
        <section class="card" aria-labelledby="history-title">
          <h1 id="history-title" data-i18n="settings.history.title">历史记录</h1>
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
        <section class="card" aria-labelledby="autostart-title">
          <h1 id="autostart-title" data-i18n="settings.autostart.title">开机启动</h1>
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
        <section class="card" aria-labelledby="features-title">
          <h1 id="features-title" data-i18n="settings.features.title">功能入口</h1>
          <p class="hint" data-i18n="settings.features.hint">关闭的入口即刻生效，从下一次截取起消失；截取热键与复制/保存能力始终保留。</p>
          <div class="rows feature-rows" data-features></div>
        </section>
        <section class="card" aria-labelledby="language-title">
          <h1 id="language-title" data-i18n="settings.language.title">语言</h1>
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
        <section class="card about" aria-labelledby="about-title">
          <h1 id="about-title" data-i18n="settings.about.title">关于</h1>
          <p class="about-name">Cropmark</p>
          <p class="hint" data-i18n="settings.about.hint">独立系统截图工具，界面与托盘只使用 Cropmark 名称与图标。</p>
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
  const featureRoot = root.querySelector("[data-features]");
  const helpEl = root.querySelector(".autostart-help");
  const switchEl = root.querySelector("[data-action=autostart]");
  const closeEl = root.querySelector("[data-action=close]");
  const delayEl = root.querySelector("[data-capture=delay]");
  const delayErrorEl = root.querySelector("[data-capture-error]");
  const autoCopyEl = root.querySelector("[data-capture=auto-copy]");
  const finishRoot = root.querySelector("[data-capture=finish]");
  const historyEnabledEl = root.querySelector("[data-history=enabled]");
  const historyLimitEl = root.querySelector("[data-history=limit]");
  const historyErrorEl = root.querySelector("[data-history-error]");
  const historyOpenEl = root.querySelector("[data-action=open-history]");
  const trayNoticeEl = root.querySelector("[data-tray-notice]");
  const quitEl = root.querySelector("[data-action=quit]");
  const languageRoot = root.querySelector("[data-language]");
  if (
    !(noticeEl instanceof HTMLElement) ||
    !(hotkeyRoot instanceof HTMLElement) ||
    !(featureRoot instanceof HTMLElement) ||
    !(helpEl instanceof HTMLElement) ||
    !(switchEl instanceof HTMLButtonElement) ||
    !(closeEl instanceof HTMLButtonElement) ||
    !(delayEl instanceof HTMLInputElement) ||
    !(delayErrorEl instanceof HTMLElement) ||
    !(autoCopyEl instanceof HTMLButtonElement) ||
    !(finishRoot instanceof HTMLElement) ||
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

  let recording: CaptureMode | null = null;
  let applying = false;
  let lastSettings: UiSettings | null = null;
  let captureSettings: CaptureSettings = {
    delaySeconds: 0,
    autoCopy: true,
    finishAction: "preview",
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
    autoCopyEl.setAttribute("aria-checked", capture.autoCopy ? "true" : "false");
    autoCopyEl.classList.toggle("on", capture.autoCopy);
    const finishButtons = finishRoot.querySelectorAll("[data-finish-action]");
    for (const button of finishButtons) {
      if (!(button instanceof HTMLButtonElement)) {
        continue;
      }
      const quiet = button.dataset.finishAction === "quiet";
      const selected = button.dataset.finishAction === capture.finishAction;
      button.setAttribute("aria-checked", selected ? "true" : "false");
      button.classList.toggle("selected", selected);
      button.disabled = quiet && !capture.autoCopy;
      button.title = button.disabled ? t("settings.capture.quiet_locked") : "";
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
    for (const mode of MODES) {
      const modeLabel = t(MODE_LABEL_KEY[mode]);
      const row = document.createElement("div");
      row.className = "hotkey-row";
      const errorText = hotkeyErrorText(settings.hotkeyErrors[mode]);
      if (errorText) {
        row.classList.add("has-error");
      }

      const label = document.createElement("div");
      label.className = "label";
      label.textContent = modeLabel;

      const button = document.createElement("button");
      button.type = "button";
      button.className = "hotkey-btn";
      button.dataset.mode = mode;
      button.title = t("settings.hotkey.title");
      button.setAttribute(
        "aria-label",
        recording === mode
          ? t("settings.hotkey.aria_recording", { mode: modeLabel })
          : t("settings.hotkey.aria_current", {
              mode: modeLabel,
              accelerator: displayAccelerator(settings.hotkeys[mode]),
            }),
      );
      button.textContent =
        recording === mode ? t("settings.hotkey.recording") : displayAccelerator(settings.hotkeys[mode]);
      if (recording === mode) {
        button.classList.add("recording");
      }

      const error = document.createElement("p");
      error.className = "error";
      error.id = `hotkey-error-${mode}`;
      error.setAttribute("role", "alert");
      error.textContent = errorText;
      error.hidden = !errorText;
      if (errorText) {
        button.setAttribute("aria-describedby", error.id);
      }

      row.append(label, button, error);
      hotkeyRoot.append(row);
    }

    featureRoot.replaceChildren();
    for (const item of FEATURE_ITEMS) {
      const enabled = settings.features[item.key];
      const row = document.createElement("div");
      row.className = "feature-row";

      const text = document.createElement("div");
      const label = document.createElement("div");
      label.className = "label";
      label.id = `feature-label-${item.key}`;
      label.textContent = t(item.labelKey);
      const hint = document.createElement("p");
      hint.className = "hint";
      hint.textContent = t(item.hintKey);
      text.append(label, hint);

      const toggle = document.createElement("button");
      toggle.type = "button";
      toggle.className = "switch";
      toggle.dataset.feature = item.key;
      toggle.setAttribute("role", "switch");
      toggle.setAttribute("aria-checked", enabled ? "true" : "false");
      toggle.setAttribute("aria-labelledby", label.id);
      const knob = document.createElement("span");
      knob.className = "knob";
      toggle.appendChild(knob);
      if (enabled) {
        toggle.classList.add("on");
      }

      row.append(text, toggle);
      featureRoot.append(row);
    }

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

  const applyHotkey = async (mode: CaptureMode, accelerator: string): Promise<void> => {
    applying = true;
    try {
      const settings = await invoke<UiSettings>("set_hotkey", { mode, accelerator });
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

  autoCopyEl.addEventListener("click", () => {
    if (applying) {
      return;
    }
    const next = autoCopyEl.getAttribute("aria-checked") !== "true";
    void applyCapture({ ...captureSettings, autoCopy: next });
  });

  finishRoot.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element) || applying) {
      return;
    }
    const button = target.closest("[data-finish-action]");
    if (!(button instanceof HTMLButtonElement) || button.disabled) {
      return;
    }
    const action = button.dataset.finishAction as FinishAction | undefined;
    if (!action || action === captureSettings.finishAction) {
      return;
    }
    void applyCapture({ ...captureSettings, finishAction: action });
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

  featureRoot.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element)) {
      return;
    }
    const button = target.closest("[data-feature]");
    if (!(button instanceof HTMLButtonElement) || !button.dataset.feature) {
      return;
    }
    const key = button.dataset.feature as FeatureKey;
    const next = button.getAttribute("aria-checked") !== "true";
    applying = true;
    void invoke<UiSettings>("set_feature", { key, enabled: next })
      .then(render)
      .catch(showInvokeError)
      .finally(() => {
        applying = false;
      });
  });

  hotkeyRoot.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof HTMLButtonElement) || !target.dataset.mode) {
      return;
    }
    recording = target.dataset.mode as CaptureMode;
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

  void refresh();

  // 语言切换:静态标签由 main 的 applyTranslations 更新;这里先按当前状态
  // 重渲染动态行,再从后端重取一次(热键错误/开机启动/无托盘提示由后端按
  // 新语言重新解析)。
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
