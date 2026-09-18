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

export interface UiSettings {
  hotkeys: Hotkeys;
  hotkeyErrors: HotkeyErrors;
  autostart: AutostartState;
  notice: string | null;
  features: FeatureSettings;
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
  if (
    !(noticeEl instanceof HTMLElement) ||
    !(hotkeyRoot instanceof HTMLElement) ||
    !(featureRoot instanceof HTMLElement) ||
    !(helpEl instanceof HTMLElement) ||
    !(switchEl instanceof HTMLButtonElement) ||
    !(closeEl instanceof HTMLButtonElement)
  ) {
    return;
  }

  let recording: CaptureMode | null = null;
  let applying = false;

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
