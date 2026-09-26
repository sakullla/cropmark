import { listen } from "@tauri-apps/api/event";
import { mountHistory } from "./history";
import { applyTranslations, initI18n, onLanguageChanged } from "./i18n";
import { mountOnboarding } from "./onboarding";
import { mountDelay } from "./overlay/delay";
import { mountCaptureError } from "./overlay/error";
import { mountOverlay } from "./overlay/index";
import { mountPin } from "./pin";
import { mountPreview } from "./preview";
import { mountScroll } from "./scroll";
import { mountSettings } from "./settings";
import { mountToast } from "./toast";

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
  const view = requestedView ?? "settings";
  let applyLanguage: () => void = () => undefined;
  if (view === "settings") {
    applyLanguage = mountSettings(root);
  } else if (view === "overlay") {
    applyLanguage = mountOverlay(root);
  } else if (view === "preview") {
    applyLanguage = mountPreview(root);
  } else if (view === "delay") {
    applyLanguage = mountDelay(root);
  } else if (view === "error") {
    applyLanguage = mountCaptureError(root);
  } else if (view === "toast") {
    applyLanguage = mountToast(root);
  } else if (view === "pin") {
    applyLanguage = mountPin(root);
  } else if (view === "history") {
    applyLanguage = mountHistory(root);
  } else if (view === "scroll") {
    applyLanguage = mountScroll(root);
  } else if (view === "guide") {
    applyLanguage = mountOnboarding(root);
  }
  const renderLanguage = (): void => {
    applyTranslations(root);
    applyLanguage();
  };
  renderLanguage();
  onLanguageChanged(renderLanguage);
})();
