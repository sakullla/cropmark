import { mountSettings } from "./settings";

const root = document.querySelector("#app");
if (root instanceof HTMLElement) {
  const view = new URLSearchParams(location.search).get("view") ?? "settings";
  if (view === "settings") {
    mountSettings(root);
  }
}
