import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t } from "../i18n";
import "./overlay.css";

interface CaptureError {
  kind: string;
  message: string;
  hint: string | null;
}

export function mountCaptureError(root: HTMLElement): () => void {
  root.className = "error-root";
  root.innerHTML = `
    <div class="error-body">
      <h1 data-i18n="error.title">无法截取</h1>
      <p class="message"></p>
      <p class="hint"></p>
    </div>
    <button type="button" data-action="close" data-i18n="error.close">关闭</button>
  `;
  const message = root.querySelector(".message");
  const hint = root.querySelector(".hint");
  const close = root.querySelector("[data-action=close]");
  if (
    !(message instanceof HTMLElement) ||
    !(hint instanceof HTMLElement) ||
    !(close instanceof HTMLButtonElement)
  ) {
    return () => undefined;
  }

  const render = (error: CaptureError | null): void => {
    message.textContent = error?.message || t("error.fallback");
    hint.textContent = error?.hint || "";
    hint.hidden = !error?.hint;
  };

  close.addEventListener("click", () => {
    void invoke("close_capture_error");
  });
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      void invoke("close_capture_error");
    }
  });

  void invoke<CaptureError | null>("get_capture_error").then(render);
  void listen<CaptureError>("capture-error", (event) => render(event.payload));

  // 语言切换:Rust 侧按词条键重新解析当前错误,前端重新拉取而不是复用旧文案。
  return () => {
    void invoke<CaptureError | null>("get_capture_error").then(render);
  };
}
