import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./overlay.css";

interface DelayPayload {
  delayMs: number;
  mode: string;
}

export function mountDelay(root: HTMLElement): void {
  root.className = "delay-root";
  root.innerHTML = `
    <div>
      <div class="count">准备截取</div>
      <p class="hint">倒计时期间可操作其它应用，到期截取当时屏幕。</p>
    </div>
    <button type="button" data-action="cancel">取消</button>
  `;
  const count = root.querySelector(".count");
  const cancel = root.querySelector("[data-action=cancel]");
  if (!(count instanceof HTMLElement) || !(cancel instanceof HTMLButtonElement)) {
    return;
  }

  let remaining = 3;
  const render = (): void => {
    count.textContent = `${remaining} 秒后截取`;
  };

  const tick = window.setInterval(() => {
    remaining = Math.max(0, remaining - 1);
    render();
    if (remaining === 0) {
      window.clearInterval(tick);
    }
  }, 1000);

  cancel.addEventListener("click", () => {
    window.clearInterval(tick);
    void invoke("cancel_capture");
  });
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      window.clearInterval(tick);
      void invoke("cancel_capture");
    }
  });

  const apply = (payload: DelayPayload): void => {
    remaining = Math.max(1, Math.round(payload.delayMs / 1000));
    render();
  };

  void invoke<DelayPayload>("get_delay_state").then(apply);
  void listen<DelayPayload>("capture-delay", (event) => apply(event.payload));
  render();
}
