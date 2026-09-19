import { listen } from "@tauri-apps/api/event";
import { mountHistory } from "./history";
import { mountDelay } from "./overlay/delay";
import { mountCaptureError } from "./overlay/error";
import { mountOverlay } from "./overlay/index";
import { mountPin } from "./pin";
import { mountPreview } from "./preview";
import { mountSettings } from "./settings";
import { mountToast } from "./toast";

const root = document.querySelector("#app");
if (root instanceof HTMLElement) {
  const view = new URLSearchParams(location.search).get("view") ?? "settings";
  if (view === "settings") {
    mountSettings(root);
  } else if (view === "overlay") {
    mountOverlay(root);
  } else if (view === "preview") {
    mountPreview(root);
  } else if (view === "delay") {
    mountDelay(root);
  } else if (view === "error") {
    mountCaptureError(root);
  } else if (view === "toast") {
    mountToast(root);
  } else if (view === "pin") {
    mountPin(root);
  } else if (view === "history") {
    mountHistory(root);
  }
}

void listen("capture-requested", () => {
  // Rust owns hide-wait and capture; this keeps the resident-shell event consumed.
});
