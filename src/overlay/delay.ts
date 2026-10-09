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
      <div class="count-row">
        <span class="count-ring" aria-hidden="true" hidden></span>
        <div class="count" data-i18n="delay.preparing" aria-hidden="true">准备截取</div>
        <button type="button" data-action="cancel" data-i18n="delay.cancel">取消</button>
      </div>
      <p class="hint" data-i18n="delay.hint">倒计时期间可操作其它应用，到期截取当时屏幕。</p>
      <div class="sr-only" role="status" data-delay-status></div>
    </div>
  `;
  const count = root.querySelector(".count");
  const ring = root.querySelector(".count-ring");
  const cancel = root.querySelector("[data-action=cancel]");
  const status = root.querySelector("[data-delay-status]");
  if (
    !(count instanceof HTMLElement) ||
    !(cancel instanceof HTMLButtonElement) ||
    !(status instanceof HTMLElement)
  ) {
    return () => undefined;
  }

  // 等真实 delayMs 到达(get_delay_state 或 capture-delay 事件)再启动倒计时,
  // 不用硬编码首帧;到达前保持"准备截取"。
  let remaining = 0;
  let tick = 0;
  // R7:环形进度按 delay_ms 与已流逝时间连续收敛;秒数文案仍按整秒播报。
  let totalMs = 0;
  let startedAt = 0;
  let ringRaf = 0;
  const render = (): void => {
    count.textContent = t("delay.countdown", { seconds: remaining });
  };
  const renderRing = (): void => {
    if (!(ring instanceof HTMLElement) || totalMs <= 0) {
      return;
    }
    const elapsed = performance.now() - startedAt;
    const left = Math.min(1, Math.max(0, 1 - elapsed / totalMs));
    ring.style.setProperty("--ring", left.toFixed(4));
  };
  const loopRing = (): void => {
    renderRing();
    ringRaf = requestAnimationFrame(loopRing);
  };
  const stopTick = (): void => {
    if (tick) {
      window.clearInterval(tick);
      tick = 0;
    }
    if (ringRaf) {
      cancelAnimationFrame(ringRaf);
      ringRaf = 0;
    }
    renderRing();
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
    totalMs = Math.max(1000, payload.delayMs);
    startedAt = performance.now();
    remaining = Math.max(1, Math.round(payload.delayMs / 1000));
    if (ring instanceof HTMLElement) {
      ring.hidden = false;
    }
    if (!ringRaf) {
      loopRing();
    }
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
    // 视觉倒数逐秒变化,对屏幕阅读器只播报一次开始状态,不逐秒打扰。
    status.textContent = t("delay.countdown", { seconds: remaining });
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
