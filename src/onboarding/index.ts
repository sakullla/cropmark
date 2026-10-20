import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { t } from "../i18n";
import { icons } from "../icons";
import { appendKeycaps, type Hotkeys } from "../settings";
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
      <header class="guide-topbar" data-tauri-drag-region>
        <div class="guide-brand" data-tauri-drag-region>
          <span class="mark" aria-hidden="true"></span>
          <span class="guide-brand-text">
            <span class="guide-brand-name">Cropmark</span>
            <span class="guide-brand-sub" data-i18n="guide.title">使用帮助</span>
          </span>
        </div>
        <button type="button" class="icon-btn" data-action="close" data-i18n-aria-label="guide.close" aria-label="关闭引导">${icons.close}</button>
      </header>
      <main class="guide-content">
        <section class="guide-hero">
          <div class="guide-hero-copy">
            <span class="guide-eyebrow" data-i18n="guide.eyebrow">GET STARTED</span>
            <h1 data-i18n="guide.headline">截图，本该如此顺手。</h1>
            <p class="guide-lead" data-i18n="guide.lead">Cropmark 常驻托盘，按一下热键，截取、标注、取字、贴图一步到位。关闭本页不会退出应用。</p>
            <div class="guide-hero-actions">
              <button type="button" class="guide-cta" data-action="close" data-i18n="guide.start">开始使用</button>
              <span class="guide-offline"><span class="guide-offline-dot" aria-hidden="true"></span><span data-i18n="guide.offline_badge">100% 本地 · 无账号 · 不上传</span></span>
            </div>
          </div>
          <div class="guide-hero-art" aria-hidden="true">
            <div class="guide-art-window">
              <div class="guide-art-bar"><span></span><span></span><span></span></div>
              <div class="guide-art-shot">
                <span class="guide-art-line is-title"></span>
                <span class="guide-art-line"></span>
                <span class="guide-art-line is-short"></span>
                <span class="guide-art-block"></span>
                <span class="guide-art-chip"></span>
              </div>
              <div class="guide-art-tools">
                <span></span><span></span><span></span><span></span><span></span>
              </div>
            </div>
          </div>
        </section>
        <section class="guide-quickstart" aria-labelledby="guide-quickstart-title">
          <div class="guide-section-head">
            <div>
              <h2 id="guide-quickstart-title" data-i18n="guide.quickstart.title">三步开始</h2>
              <p class="hint" data-i18n="guide.hotkeys.body">下面是当前生效的组合，可在设置里修改。</p>
            </div>
            <span class="guide-section-badge" data-i18n="guide.hotkeys.title">三种采集热键</span>
          </div>
          <div class="guide-steps">
            <article class="guide-step">
              <span class="guide-step-no">01</span>
              <div class="guide-step-copy">
                <h3 data-i18n="guide.quickstart.step1.title">按下热键</h3>
                <p data-i18n="guide.quickstart.step1.body">区域、窗口或全屏，随取随截。</p>
              </div>
              <div class="guide-step-keys">
                <div class="guide-step-key"><span data-i18n="settings.mode.region">区域</span><span class="guide-keys" data-hotkey="region">Alt+Shift+A</span></div>
                <div class="guide-step-key"><span data-i18n="settings.mode.window">窗口</span><span class="guide-keys" data-hotkey="window">Alt+Shift+W</span></div>
                <div class="guide-step-key"><span data-i18n="settings.mode.fullscreen">全屏</span><span class="guide-keys" data-hotkey="fullscreen">Alt+Shift+S</span></div>
              </div>
            </article>
            <article class="guide-step">
              <span class="guide-step-no">02</span>
              <div class="guide-step-copy">
                <h3 data-i18n="guide.quickstart.step2.title">标注与取字</h3>
                <p data-i18n="guide.quickstart.step2.body">箭头、高亮、马赛克，或直接识别文字。</p>
              </div>
              <div class="guide-step-visual is-annotate" aria-hidden="true"><span></span><span></span><span></span></div>
            </article>
            <article class="guide-step">
              <span class="guide-step-no">03</span>
              <div class="guide-step-copy">
                <h3 data-i18n="guide.quickstart.step3.title">复制或贴图</h3>
                <p data-i18n="guide.quickstart.step3.body">一键复制，或把截图钉在屏幕上对照。</p>
              </div>
              <div class="guide-step-visual is-pin" aria-hidden="true"><span></span><span></span></div>
            </article>
          </div>
        </section>
        <section class="guide-features" aria-labelledby="guide-features-title">
          <div class="guide-section-head">
            <div>
              <h2 id="guide-features-title" data-i18n="guide.feature.title">更多能力</h2>
            </div>
          </div>
          <div class="guide-feature-grid">
            <article class="guide-feature">
              <span class="guide-icon" aria-hidden="true">${icons.tray}</span>
              <div><h3 data-i18n="guide.tray.title">托盘常驻</h3><p data-i18n="guide.tray.body">从托盘图标打开区域、窗口或全屏截图，也可以进入历史和设置。退出请使用托盘菜单或设置中的退出。</p></div>
            </article>
            <article class="guide-feature">
              <span class="guide-icon" aria-hidden="true">${icons.pin}</span>
              <div><h3 data-i18n="guide.pin.title">贴图</h3><p data-i18n="guide.pin.body">截图后可以贴在屏幕上对照查看。关掉贴图不会退出托盘中的 Cropmark。</p></div>
            </article>
            <article class="guide-feature">
              <span class="guide-icon" aria-hidden="true">${icons.ocr}</span>
              <div><h3 data-i18n="guide.ocr.title">文字识别</h3><p data-i18n="guide.ocr.body">在预览中识别画面文字，可按点击位置、框选或全部复制。开启结果面板后还能搜索并复制一段文字。识别使用本机模型，无需联网。</p></div>
            </article>
            <article class="guide-feature">
              <span class="guide-icon" aria-hidden="true">${icons.history}</span>
              <div><h3 data-i18n="guide.history.title">历史</h3><p data-i18n="guide.history.body">完成的截图会进入历史，可再次复制、贴图或删除。设置里可以打开历史并调整保留数量。</p></div>
            </article>
          </div>
        </section>
      </main>
      <footer class="guide-footer">
        <div class="guide-log-meta">
          <span class="guide-log-label" data-i18n="guide.logs.title">日志位置</span>
          <span class="guide-log-path" data-log-path>—</span>
          <span class="guide-log-error" data-log-error role="alert" hidden></span>
        </div>
        <button type="button" class="guide-open-logs" data-action="open-logs" data-i18n="guide.open_logs">打开</button>
        <button type="button" class="guide-done" data-action="close" data-i18n="guide.done">知道了</button>
      </footer>
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
      appendKeycaps(element, value);
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
    logPathEl.textContent = logDirectory.trim() ? logDirectory : t("guide.logs.unavailable");
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
