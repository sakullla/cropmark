import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t, type CatalogKey } from "../i18n";
import "./scroll.css";

/** R5:滚动方向;与 Rust 侧 `CaptureAxis` 的 serde 小写表示一致。 */
type CaptureAxis = "vertical" | "horizontal";

/** Rust 侧 `ScrollStatus` 的镜像;`state` 映射到 `scroll.status.*` 词条。 */
interface ScrollStatus {
  state: string;
  axis: CaptureAxis;
  width: number;
  height: number;
  appended: number;
}

const STATUS_KEYS: Record<string, CatalogKey> = {
  running: "scroll.status.running",
  unchanged: "scroll.status.unchanged",
  fast: "scroll.status.fast",
  no_match: "scroll.status.no_match",
  limit: "scroll.status.limit",
  finishing: "scroll.status.finishing",
  failed: "scroll.status.failed",
};

/** 横向滚动时方向相关的状态词条;其余状态文案与方向无关。 */
const HORIZONTAL_STATUS_KEYS: Record<string, CatalogKey> = {
  running: "scroll.status.running.horizontal",
  unchanged: "scroll.status.unchanged.horizontal",
  limit: "scroll.status.limit.horizontal",
};

function statusKey(state: string, axis: CaptureAxis): CatalogKey {
  if (axis === "horizontal" && HORIZONTAL_STATUS_KEYS[state]) {
    return HORIZONTAL_STATUS_KEYS[state];
  }
  return STATUS_KEYS[state] ?? "scroll.status.running";
}

function axisFromDataset(value: string | undefined): CaptureAxis {
  return value === "horizontal" ? "horizontal" : "vertical";
}

/** 命令失败时优先展示服务端本地化文案,否则用通用失败文案。 */
function messageOf(error: unknown): string {
  if (typeof error === "string" && error.trim().length > 0) {
    return error;
  }
  if (error && typeof error === "object" && "message" in error) {
    const message = (error as { message?: unknown }).message;
    if (typeof message === "string" && message.trim().length > 0) {
      return message;
    }
  }
  return t("scroll.action.axis_failed");
}

/**
 * 长截图控制窗:置顶非模态、位于框选区域旁;承载状态提示、
 * 方向选择与「完成/取消」。窗口本身不参与截图内容,状态由 Rust 周期推送;
 * 方向在首个内容变化前可切换,之后锁定(以 Rust 状态里的 axis 为准)。
 * 锁定原因写在卡片里;完成和取消在滚动区外,进度变长时仍留在底部。
 */
export function mountScroll(root: HTMLElement): () => void {
  root.className = "scroll-root";
  root.innerHTML = `
    <div class="scroll-card">
      <div class="scroll-body">
        <div class="scroll-head">
          <span class="scroll-title" data-i18n="scroll.title"></span>
          <span class="scroll-size"></span>
        </div>
        <div class="scroll-axis" role="group" data-i18n-aria-label="scroll.axis.label">
          <button type="button" class="scroll-axis-option" data-axis="vertical" data-i18n="scroll.axis.vertical" aria-pressed="true"></button>
          <button type="button" class="scroll-axis-option" data-axis="horizontal" data-i18n="scroll.axis.horizontal" aria-pressed="false"></button>
        </div>
        <p class="scroll-lock" hidden></p>
        <p class="scroll-status" role="status"></p>
        <p class="scroll-hint"></p>
      </div>
      <div class="scroll-actions">
        <button type="button" class="scroll-finish" data-i18n="scroll.finish"></button>
        <button type="button" class="scroll-cancel" data-i18n="scroll.cancel"></button>
      </div>
    </div>`;
  const status = root.querySelector(".scroll-status");
  const size = root.querySelector(".scroll-size");
  const hint = root.querySelector(".scroll-hint");
  const lock = root.querySelector(".scroll-lock");
  const finish = root.querySelector(".scroll-finish");
  const cancel = root.querySelector(".scroll-cancel");
  const axisButtons = Array.from(root.querySelectorAll(".scroll-axis-option")).filter(
    (button): button is HTMLButtonElement => button instanceof HTMLButtonElement,
  );
  if (
    !(status instanceof HTMLElement) ||
    !(size instanceof HTMLElement) ||
    !(hint instanceof HTMLElement) ||
    !(lock instanceof HTMLElement) ||
    !(finish instanceof HTMLButtonElement) ||
    !(cancel instanceof HTMLButtonElement) ||
    axisButtons.length !== 2
  ) {
    return () => undefined;
  }

  let last: ScrollStatus | null = null;
  // 取消/完成请求进行中:期间按钮禁用,进行中文案持续显示直到结果替换(ADR-2)。
  let actionBusy = false;
  // 方向切换请求进行中:期间方向按钮禁用,避免连点覆盖。
  let axisBusy = false;
  // 控制窗就绪信号每次会话只发一次;Rust 侧据此才开始 Windows 自动滚动计时,
  // 保证首个自动 nudge 不会在方向按钮可交互前制造首个内容变化(横向被锁死)。
  let readySent = false;

  const axisOf = (payload: ScrollStatus | null): CaptureAxis => payload?.axis ?? "vertical";
  /** 首个内容变化后方向锁定(Rust 侧同样拒绝切换)。 */
  const axisLocked = (payload: ScrollStatus | null): boolean =>
    payload !== null && payload.appended > 0;

  const render = (): void => {
    const axis = axisOf(last);
    const locked = axisLocked(last);
    const axisDisabled =
      last === null || locked || actionBusy || axisBusy || last.state === "finishing";
    for (const button of axisButtons) {
      const active = axisFromDataset(button.dataset.axis) === axis;
      button.classList.toggle("is-active", active);
      button.setAttribute("aria-pressed", active ? "true" : "false");
      button.disabled = axisDisabled;
      if (locked) {
        button.dataset.tooltip = t("scroll.axis.locked");
      } else {
        delete button.dataset.tooltip;
      }
    }
    // 锁定原因常驻在卡片里,不依赖悬停;未锁定时不占位。
    if (locked) {
      lock.hidden = false;
      lock.textContent = t("scroll.axis.locked");
    } else {
      lock.hidden = true;
      lock.textContent = "";
    }
    hint.textContent = axis === "horizontal" ? t("scroll.hint.horizontal") : t("scroll.hint");

    if (!last) {
      status.classList.remove("is-error");
      status.textContent = t(statusKey("running", axis));
      size.textContent = "";
      return;
    }
    status.classList.toggle("is-error", last.state === "failed");
    status.textContent = t(statusKey(last.state, last.axis));
    // 尺寸一直显示：这就是正在截的那一块，滚动后沿轴变长。
    const parts = [t("scroll.watching", { width: last.width, height: last.height })];
    if (last.appended > 0) {
      const appendedKey: CatalogKey =
        last.axis === "horizontal" ? "scroll.appended.horizontal" : "scroll.appended";
      parts.push(t(appendedKey, { count: last.appended }));
    }
    size.textContent = parts.join(" · ");
    const busy = actionBusy || last.state === "finishing";
    finish.disabled = busy;
    cancel.disabled = busy;
  };

  /** 已有状态可渲染即视为可交互:方向按钮此时才启用,通知 Rust 开始自动滚动计时。 */
  const signalReady = (): void => {
    if (readySent || last === null) {
      return;
    }
    readySent = true;
    void invoke("scroll_control_ready").catch(() => undefined);
  };

  const apply = (payload: ScrollStatus | null): void => {
    last = payload;
    render();
    signalReady();
  };

  const reset = (): void => {
    actionBusy = false;
    axisBusy = false;
    readySent = false;
    finish.disabled = false;
    cancel.disabled = false;
    apply(null);
    void invoke<ScrollStatus | null>("get_scroll_status").then(apply);
  };

  const showError = (message: string): void => {
    actionBusy = false;
    finish.disabled = false;
    cancel.disabled = false;
    status.textContent = message;
    status.classList.add("is-error");
  };

  const showActionError = (key: CatalogKey): void => {
    showError(t(key));
  };

  const selectAxis = (axis: CaptureAxis): void => {
    if (
      !last ||
      axisBusy ||
      actionBusy ||
      axisLocked(last) ||
      last.state === "finishing" ||
      last.axis === axis
    ) {
      return;
    }
    axisBusy = true;
    const previous = last.axis;
    last = { ...last, axis };
    render();
    void invoke("set_scroll_axis", { axis })
      .catch((error) => {
        // 会话已结束或已开始拼接:回正方向并以服务端状态为准。
        if (last) {
          last = { ...last, axis: previous };
        }
        showError(messageOf(error));
        void invoke<ScrollStatus | null>("get_scroll_status")
          .then(apply)
          .catch(() => undefined);
      })
      .finally(() => {
        axisBusy = false;
      });
  };

  finish.addEventListener("click", () => {
    if (actionBusy) {
      return;
    }
    actionBusy = true;
    finish.disabled = true;
    cancel.disabled = true;
    status.classList.remove("is-error");
    status.textContent = t("scroll.status.finishing");
    void invoke("finish_scroll_capture").catch(() => {
      showActionError("scroll.action.finish_failed");
    });
  });

  const cancelCapture = (): void => {
    if (actionBusy) {
      return;
    }
    actionBusy = true;
    finish.disabled = true;
    cancel.disabled = true;
    status.classList.remove("is-error");
    status.textContent = t("scroll.action.canceling");
    void invoke("cancel_scroll_capture").catch(() => {
      showActionError("scroll.action.cancel_failed");
    });
  };
  cancel.addEventListener("click", cancelCapture);
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      cancelCapture();
    }
  });
  for (const button of axisButtons) {
    button.addEventListener("click", () => {
      selectAxis(axisFromDataset(button.dataset.axis));
    });
  }

  void invoke<ScrollStatus | null>("get_scroll_status").then(apply);
  void listen<ScrollStatus>("scroll-status", (event) => apply(event.payload));
  // 控制窗被下一次会话复用时,Rust 发 reload 让本视图清掉上一次的状态。
  void listen("scroll-reload", reset);
  render();

  return render;
}
