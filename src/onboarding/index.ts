import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { t } from "../i18n";
import { icons } from "../icons";
import { displayAccelerator, type Hotkeys } from "../settings";
import "./onboarding.css";

type CaptureSlot = "region" | "window" | "fullscreen";

const SLOTS: CaptureSlot[] = ["region", "window", "fullscreen"];

/** 与 Rust `Hotkeys::default` 一致;读不到设置时仍能展示三种采集热键。 */
const FALLBACK: Record<CaptureSlot, string> = {
  region: "Alt+Shift+A",
  window: "Alt+Shift+W",
  fullscreen: "Alt+Shift+S",
};

/**
 * 首次引导与设置「使用帮助」共用的说明页。关闭窗口由 Rust 记为已完成,
 * 这里不拦截关闭,也不承担托盘或热键逻辑。
 */
export function mountOnboarding(root: HTMLElement): () => void {
  root.innerHTML = `
    <div class="shell guide-root">
      <header class="titlebar" data-tauri-drag-region>
        <div class="brand" data-tauri-drag-region>
          <span class="mark" aria-hidden="true"></span>
          <span class="name" data-i18n="guide.title">使用帮助</span>
        </div>
        <button type="button" class="icon-btn" data-action="close" data-i18n-aria-label="guide.close" aria-label="关闭引导">${icons.close}</button>
      </header>
      <main class="content">
        <p class="hint" data-i18n="guide.intro">Cropmark 启动后留在系统托盘，不占用主窗口。关闭本页不会退出应用，热键和之后的截图都不受影响。</p>
        <section class="card guide-section" aria-labelledby="guide-tray-title">
          <h2 id="guide-tray-title" data-i18n="guide.tray.title">托盘常驻</h2>
          <p class="hint" data-i18n="guide.tray.body">从托盘图标打开区域、窗口或全屏截图，也可以进入历史和设置。退出请使用托盘菜单或设置中的退出。</p>
        </section>
        <section class="card guide-section" aria-labelledby="guide-hotkeys-title">
          <h2 id="guide-hotkeys-title" data-i18n="guide.hotkeys.title">三种采集热键</h2>
          <p class="hint" data-i18n="guide.hotkeys.body">下面是当前生效的组合，可在设置里修改。</p>
          <ul class="guide-hotkeys">
            <li class="guide-hotkey">
              <span data-i18n="settings.mode.region">区域</span>
              <kbd data-hotkey="region">Alt+Shift+A</kbd>
            </li>
            <li class="guide-hotkey">
              <span data-i18n="settings.mode.window">窗口</span>
              <kbd data-hotkey="window">Alt+Shift+W</kbd>
            </li>
            <li class="guide-hotkey">
              <span data-i18n="settings.mode.fullscreen">全屏</span>
              <kbd data-hotkey="fullscreen">Alt+Shift+S</kbd>
            </li>
          </ul>
        </section>
        <section class="card guide-section" aria-labelledby="guide-pin-title">
          <h2 id="guide-pin-title" data-i18n="guide.pin.title">贴图</h2>
          <p class="hint" data-i18n="guide.pin.body">截图后可以贴在屏幕上对照查看。关掉贴图不会退出托盘中的 Cropmark。</p>
        </section>
        <section class="card guide-section" aria-labelledby="guide-ocr-title">
          <h2 id="guide-ocr-title" data-i18n="guide.ocr.title">文字识别</h2>
          <p class="hint" data-i18n="guide.ocr.body">在预览中识别画面文字，可按点击位置、框选或全部复制。开启结果面板后还能搜索并复制一段文字。识别使用本机模型，无需联网。</p>
        </section>
        <section class="card guide-section" aria-labelledby="guide-history-title">
          <h2 id="guide-history-title" data-i18n="guide.history.title">历史</h2>
          <p class="hint" data-i18n="guide.history.body">完成的截图会进入历史，可再次复制、贴图或删除。设置里可以打开历史并调整保留数量。</p>
        </section>
        <section class="card guide-section" aria-labelledby="guide-logs-title">
          <h2 id="guide-logs-title" data-i18n="guide.logs.title">日志位置</h2>
          <p class="hint" data-i18n="guide.logs.body">诊断日志保存在本机，不含截图、剪贴板或识别文字。下面是日志文件的位置，也可以在设置的通用分组里再次打开。</p>
          <p class="hint" data-log-path>—</p>
          <p class="hint" data-log-error hidden></p>
          <button type="button" class="choice" data-action="open-logs" data-i18n="guide.logs.button">打开日志文件夹</button>
        </section>
        <button type="button" class="guide-done" data-action="close" data-i18n="guide.done">知道了</button>
      </main>
    </div>
  `;

  let hotkeys: Record<CaptureSlot, string> = { ...FALLBACK };

  const renderHotkeys = (): void => {
    for (const slot of SLOTS) {
      const element = root.querySelector(`[data-hotkey="${slot}"]`);
      if (!(element instanceof HTMLElement)) {
        continue;
      }
      const value = hotkeys[slot].trim() || FALLBACK[slot];
      element.textContent = displayAccelerator(value);
    }
  };

  const close = (): void => {
    void getCurrentWindow().close();
  };

  root.querySelectorAll("[data-action=close]").forEach((button) => {
    button.addEventListener("click", () => {
      close();
    });
  });

  window.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") {
      return;
    }
    event.preventDefault();
    close();
  });

  const logPathEl = root.querySelector("[data-log-path]");
  const logErrorEl = root.querySelector("[data-log-error]");
  let logDirectory = "";
  let logOpenFailed = false;
  if (logPathEl instanceof HTMLElement) {
    logPathEl.style.overflowWrap = "anywhere";
    logPathEl.style.userSelect = "text";
  }
  const renderLogPath = (): void => {
    if (!(logPathEl instanceof HTMLElement)) {
      return;
    }
    logPathEl.textContent = logDirectory.trim() ? logDirectory : t("settings.logs.unavailable");
    if (logErrorEl instanceof HTMLElement) {
      logErrorEl.hidden = !logOpenFailed;
      logErrorEl.textContent = logOpenFailed ? t("settings.logs.open_failed") : "";
    }
  };
  const render = (): void => {
    renderHotkeys();
    renderLogPath();
  };

  render();
  void invoke<string>("log_directory")
    .then((path) => {
      logDirectory = path;
      renderLogPath();
    })
    .catch(() => {
      logDirectory = "";
      renderLogPath();
    });
  root.querySelector("[data-action=open-logs]")?.addEventListener("click", () => {
    logOpenFailed = false;
    renderLogPath();
    void invoke("open_log_directory").catch(() => {
      logOpenFailed = true;
      renderLogPath();
    });
  });

  void invoke<{ hotkeys?: Hotkeys }>("get_ui_settings")
    .then((settings) => {
      const loaded = settings?.hotkeys;
      if (!loaded) {
        return;
      }
      hotkeys = {
        region: loaded.region,
        window: loaded.window,
        fullscreen: loaded.fullscreen,
      };
      renderHotkeys();
    })
    .catch(() => {
      // 无宿主或命令失败时保留默认热键,说明文字仍可读。
    });

  return render;
}
