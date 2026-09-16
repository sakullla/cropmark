import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./overlay.css";

interface CaptureError {
  kind: string;
  message: string;
  hint: string | null;
}

export function mountCaptureError(root: HTMLElement): void {
  root.className = "error-root";
  root.innerHTML = `
    <div class="error-body">
      <h1>无法截取</h1>
      <p class="message"></p>
      <p class="hint"></p>
    </div>
    <button type="button" data-action="close">关闭</button>
  `;
  const message = root.querySelector(".message");
  const hint = root.querySelector(".hint");
  const close = root.querySelector("[data-action=close]");
  if (
    !(message instanceof HTMLElement) ||
    !(hint instanceof HTMLElement) ||
    !(close instanceof HTMLButtonElement)
  ) {
    return;
  }

  const render = (error: CaptureError | null): void => {
    message.textContent = error?.message || "截取失败。";
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
}
