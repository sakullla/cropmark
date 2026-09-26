import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t } from "../i18n";
import "./overlay.css";

interface DelayPayload {
  delayMs: number;
  mode: string;
}

export function mountDelay(root: HTMLElement): () => void {
  root.className = "delay-root";
  root.innerHTML = `
    <div>
      <div class="count" data-i18n="delay.preparing">准备截取</div>
      <p class="hint" data-i18n="delay.hint">倒计时期间可操作其它应用，到期截取当时屏幕。</p>
    </div>
    <button type="button" data-action="cancel" data-i18n="delay.cancel">取消</button>
  `;
  const count = root.querySelector(".count");
  const cancel = root.querySelector("[data-action=cancel]");
  if (!(count instanceof HTMLElement) || !(cancel instanceof HTMLButtonElement)) {
    return () => undefined;
  }

  // 等真实 delayMs 到达(get_delay_state 或 capture-delay 事件)再启动倒计时,
  // 不用硬编码首帧;到达前保持"准备截取"。
  let remaining = 0;
  let tick = 0;
  const render = (): void => {
    count.textContent = t("delay.countdown", { seconds: remaining });
  };
  const stopTick = (): void => {
    if (tick) {
      window.clearInterval(tick);
      tick = 0;
    }
  };

  cancel.addEventListener("click", () => {
    stopTick();
    void invoke("cancel_capture");
  });
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      stopTick();
      void invoke("cancel_capture");
    }
  });

  const apply = (payload: DelayPayload): void => {
    remaining = Math.max(1, Math.round(payload.delayMs / 1000));
    if (!tick) {
      tick = window.setInterval(() => {
        remaining = Math.max(0, remaining - 1);
        render();
        if (remaining === 0) {
          stopTick();
        }
      }, 1000);
    }
    render();
  };

  void invoke<DelayPayload>("get_delay_state").then(apply);
  void listen<DelayPayload>("capture-delay", (event) => apply(event.payload));

  // 语言切换:倒计时进行中重渲染秒数文案;未启动时"准备截取"由
  // applyTranslations 的 data-i18n 处理。
  return () => {
    if (tick) {
      render();
    }
  };
}
