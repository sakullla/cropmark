import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { autostartHelp, hotkeyErrorText } from "../errors";

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

export interface UiSettings {
  hotkeys: Hotkeys;
  hotkeyErrors: HotkeyErrors;
  autostart: AutostartState;
  notice: string | null;
  features: FeatureSettings;
  capture: CaptureSettings;
  history: HistorySettings;
}

type FeatureKey = keyof FeatureSettings;

const FEATURE_ITEMS: Array<{ key: FeatureKey; label: string; hint: string }> = [
  { key: "ocrEntry", label: "取字", hint: "关闭后选区菜单与预览工具条不再显示取字，O 键停用。" },
  { key: "pinEntry", label: "贴图", hint: "关闭后选区菜单不再显示贴图入口。" },
  { key: "magnifier", label: "放大镜", hint: "选区时跟随指针的像素放大镜；关闭后选区内 C 键取色同时停用。" },
  { key: "toolbarCopy", label: "操作条·复制", hint: "选区操作条上的复制按钮。" },
  { key: "toolbarSave", label: "操作条·保存", hint: "选区操作条上的保存按钮。" },
  { key: "toolbarPin", label: "操作条·贴图", hint: "选区操作条上的贴图按钮。" },
];

const MODE_LABEL: Record<CaptureMode, string> = {
  region: "区域截取",
  window: "窗口截取",
  fullscreen: "全屏截取",
};

const MODES: CaptureMode[] = ["region", "window", "fullscreen"];

export function mountSettings(root: HTMLElement): void {
  root.innerHTML = `
    <div class="shell">
      <header class="titlebar" data-tauri-drag-region>
        <div class="brand" data-tauri-drag-region>
          <span class="mark" aria-hidden="true"></span>
          <span class="name">Cropmark</span>
        </div>
        <button type="button" class="icon-btn" data-action="close" aria-label="关闭">×</button>
      </header>
      <main class="content">
        <p class="notice" role="alert" hidden></p>
        <section class="card" aria-labelledby="hotkeys-title">
          <h1 id="hotkeys-title">热键</h1>
          <p class="hint">点击热键按钮后按下新组合，Esc 取消；改动立即生效。</p>
          <div class="rows" data-hotkeys></div>
        </section>
        <section class="card" aria-labelledby="capture-title">
          <h1 id="capture-title">截图</h1>
          <div class="setting-row">
            <div>
              <div class="label" id="delay-label">延时秒数</div>
              <p class="hint">0–60 秒，热键与托盘截取按此倒计时；0 为立即截取。</p>
            </div>
            <input type="number" class="number-input" data-capture="delay" min="0" max="60" step="1" inputmode="numeric" aria-labelledby="delay-label" />
          </div>
          <p class="error" data-capture-error role="alert" hidden></p>
          <div class="setting-row">
            <div>
              <div class="label" id="autocopy-label">完成后自动复制</div>
              <p class="hint">关闭后截图完成不写入剪贴板；预览内手动复制不受影响。</p>
            </div>
            <button type="button" class="switch" data-capture="auto-copy" role="switch" aria-checked="true" aria-labelledby="autocopy-label">
              <span class="knob"></span>
            </button>
          </div>
          <div class="setting-row">
            <div>
              <div class="label" id="finish-label">完成后动作</div>
              <p class="hint">静默完成会直接复制并给出提示，不打开预览；需自动复制开启。</p>
            </div>
            <div class="choices" data-capture="finish" role="radiogroup" aria-labelledby="finish-label">
              <button type="button" class="choice" role="radio" data-finish-action="preview" aria-checked="true">预览</button>
              <button type="button" class="choice" role="radio" data-finish-action="quiet" aria-checked="false">静默完成</button>
            </div>
          </div>
        </section>
        <section class="card" aria-labelledby="history-title">
          <h1 id="history-title">历史记录</h1>
          <p class="hint">截图完成后在本机保留最近记录，可重新复制、贴图或删除；数据只保存在本机。</p>
          <div class="setting-row">
            <div>
              <div class="label" id="history-enabled-label">保留截图历史</div>
              <p class="hint">关闭后不再新增记录；已有记录保留，可在历史窗口清空。</p>
            </div>
            <button type="button" class="switch" data-history="enabled" role="switch" aria-checked="true" aria-labelledby="history-enabled-label">
              <span class="knob"></span>
            </button>
          </div>
          <div class="setting-row">
            <div>
              <div class="label" id="history-limit-label">记录上限</div>
              <p class="hint">5–200 条，超出上限时自动淘汰最旧记录。</p>
            </div>
            <input type="number" class="number-input" data-history="limit" min="5" max="200" step="1" inputmode="numeric" aria-labelledby="history-limit-label" />
          </div>
          <p class="error" data-history-error role="alert" hidden></p>
          <div class="setting-row">
            <div>
              <div class="label" id="history-open-label">浏览历史</div>
              <p class="hint">打开历史窗口，按时间查看缩略图并重新复制、贴图或删除。</p>
            </div>
            <button type="button" class="choice" data-action="open-history" aria-labelledby="history-open-label">打开历史记录</button>
          </div>
        </section>
        <section class="card" aria-labelledby="autostart-title">
          <h1 id="autostart-title">开机启动</h1>
          <div class="autostart-row">
            <div>
              <div class="label" id="autostart-label">登录时运行</div>
              <p class="hint autostart-help"></p>
            </div>
            <button type="button" class="switch" data-action="autostart" role="switch" aria-checked="false" aria-labelledby="autostart-label">
              <span class="knob"></span>
            </button>
          </div>
        </section>
        <section class="card" aria-labelledby="features-title">
          <h1 id="features-title">功能入口</h1>
          <p class="hint">关闭的入口即刻生效，从下一次截取起消失；截取热键与复制/保存能力始终保留。</p>
          <div class="rows feature-rows" data-features></div>
        </section>
        <section class="card about" aria-labelledby="about-title">
          <h1 id="about-title">关于</h1>
          <p class="about-name">Cropmark</p>
          <p class="hint">独立系统截图工具，界面与托盘只使用 Cropmark 名称与图标。</p>
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
    !(historyOpenEl instanceof HTMLButtonElement)
  ) {
    return;
  }

  let recording: CaptureMode | null = null;
  let applying = false;
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
      button.title = button.disabled ? "需先开启完成后自动复制" : "";
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

  const render = (settings: UiSettings): void => {
    if (settings.notice) {
      noticeEl.hidden = false;
      noticeEl.textContent = settings.notice;
    } else {
      noticeEl.hidden = true;
      noticeEl.textContent = "";
    }

    hotkeyRoot.replaceChildren();
    for (const mode of MODES) {
      const row = document.createElement("div");
      row.className = "hotkey-row";
      const errorText = hotkeyErrorText(settings.hotkeyErrors[mode]);
      if (errorText) {
        row.classList.add("has-error");
      }

      const label = document.createElement("div");
      label.className = "label";
      label.textContent = MODE_LABEL[mode];

      const button = document.createElement("button");
      button.type = "button";
      button.className = "hotkey-btn";
      button.dataset.mode = mode;
      button.title = "点击后按下新组合，Esc 取消";
      button.setAttribute(
        "aria-label",
        recording === mode
          ? `${MODE_LABEL[mode]}热键：正在录制，请按下新组合，Esc 取消`
          : `${MODE_LABEL[mode]}热键：当前为 ${displayAccelerator(settings.hotkeys[mode])}，点击修改`,
      );
      button.textContent =
        recording === mode ? "按下新热键…" : displayAccelerator(settings.hotkeys[mode]);
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
      label.textContent = item.label;
      const hint = document.createElement("p");
      hint.className = "hint";
      hint.textContent = item.hint;
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

  const commitDelay = (): void => {
    const raw = delayEl.value.trim();
    if (!/^\d+$/.test(raw)) {
      showDelayError("延时需为 0–60 之间的整数秒。");
      return;
    }
    const seconds = Number(raw);
    if (!Number.isSafeInteger(seconds) || seconds < 0 || seconds > 60) {
      showDelayError("延时需为 0–60 之间的整数秒。");
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
      showHistoryError("记录上限需为 5–200 之间的整数。");
      return;
    }
    const limit = Number(raw);
    if (!Number.isSafeInteger(limit) || limit < 5 || limit > 200) {
      showHistoryError("记录上限需为 5–200 之间的整数。");
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
