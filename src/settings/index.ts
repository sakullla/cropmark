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

export interface UiSettings {
  hotkeys: Hotkeys;
  hotkeyErrors: HotkeyErrors;
  autostart: AutostartState;
  notice: string | null;
}

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
  const helpEl = root.querySelector(".autostart-help");
  const switchEl = root.querySelector("[data-action=autostart]");
  const closeEl = root.querySelector("[data-action=close]");
  if (
    !(noticeEl instanceof HTMLElement) ||
    !(hotkeyRoot instanceof HTMLElement) ||
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
      button.setAttribute("aria-live", "polite");
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
      if (errorText) {
        button.setAttribute("aria-invalid", "true");
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
