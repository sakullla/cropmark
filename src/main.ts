import { listen } from "@tauri-apps/api/event";
import { applyTranslations, initI18n, onLanguageChanged } from "./i18n";

// 视图标记必须在挂载前同步写上。覆盖层背景等选择器依赖 data-view，
// 且不能留在 HTML 内联脚本里，否则会被 script-src 'self' 拦住。
const requestedView = new URLSearchParams(location.search).get("view");
if (requestedView) {
  document.documentElement.dataset.view = requestedView;
}

void listen("capture-requested", () => {
  // Rust owns hide-wait and capture; this keeps the resident-shell event consumed.
});

void (async () => {
  // R12:先解析当前界面语言再挂载,首帧即为正确语言。
  await initI18n();
  const root = document.querySelector("#app");
  if (!(root instanceof HTMLElement)) {
    return;
  }
  // 每个原生窗口只加载自己的视图,避免预创建浮层同时解析设置/历史/录屏界面。
  const view = requestedView ?? "settings";
  let applyLanguage: () => void = () => undefined;
  if (view === "settings") {
    applyLanguage = (await import("./settings")).mountSettings(root);
  } else if (view === "overlay") {
    applyLanguage = (await import("./overlay/index")).mountOverlay(root);
  } else if (view === "preview") {
    applyLanguage = (await import("./preview")).mountPreview(root);
  } else if (view === "delay") {
    applyLanguage = (await import("./overlay/delay")).mountDelay(root);
  } else if (view === "error") {
    applyLanguage = (await import("./overlay/error")).mountCaptureError(root);
  } else if (view === "toast") {
    applyLanguage = (await import("./toast")).mountToast(root);
  } else if (view === "pin") {
    applyLanguage = (await import("./pin")).mountPin(root);
  } else if (view === "history") {
    applyLanguage = (await import("./history")).mountHistory(root);
  } else if (view === "scroll") {
    applyLanguage = (await import("./scroll")).mountScroll(root);
  } else if (view === "record-control") {
    applyLanguage = (await import("./record/control")).mountRecordControl(root);
  } else if (view === "record-overlay") {
    applyLanguage = (await import("./record/overlay")).mountRecordOverlay(root);
  } else if (view === "guide") {
    applyLanguage = (await import("./onboarding")).mountOnboarding(root);
  }
  const renderLanguage = (): void => {
    applyTranslations(root);
    applyLanguage();
  };
  renderLanguage();
  onLanguageChanged(renderLanguage);
})();
