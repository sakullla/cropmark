import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { handleRadioGroupKeydown } from "../a11y";
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
  /** 回退段栈非空:栈空时禁用回退按钮(首帧不可回退)。 */
  canUndo: boolean;
  /** 拼接结果总段数(初始区域计 1 段);finishing 的「正在拼接 · N 段」用它。 */
  segmentCount: number;
}

const STATUS_KEYS: Record<string, CatalogKey> = {
  ready: "scroll.status.ready",
  running: "scroll.status.running",
  unchanged: "scroll.status.unchanged",
  fast: "scroll.status.fast",
  no_match: "scroll.status.no_match",
  limit: "scroll.status.limit",
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
function messageOf(error: unknown, fallback: CatalogKey = "scroll.action.axis_failed"): string {
  if (typeof error === "string" && error.trim().length > 0) {
    return error;
  }
  if (error && typeof error === "object" && "message" in error) {
    const message = (error as { message?: unknown }).message;
    if (typeof message === "string" && message.trim().length > 0) {
      return message;
    }
  }
  return t(fallback);
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
        <div class="scroll-axis" role="radiogroup" data-i18n-aria-label="scroll.axis.label">
          <button type="button" class="scroll-axis-option" role="radio" data-axis="vertical" data-i18n="scroll.axis.vertical" aria-checked="true" tabindex="0"></button>
          <button type="button" class="scroll-axis-option" role="radio" data-axis="horizontal" data-i18n="scroll.axis.horizontal" aria-checked="false" tabindex="-1"></button>
        </div>
        <p class="scroll-lock" hidden></p>
        <p class="scroll-status" role="status"><span class="scroll-status-text"></span><span class="progress" aria-hidden="true" hidden></span></p>
        <p class="scroll-hint"></p>
      </div>
      <div class="scroll-actions">
        <button type="button" class="scroll-start" data-i18n="scroll.start"></button>
        <button type="button" class="scroll-undo" data-i18n="scroll.undo"></button>
        <button type="button" class="scroll-finish" data-i18n="scroll.finish"></button>
        <button type="button" class="scroll-cancel" data-i18n="scroll.cancel"></button>
      </div>
    </div>`;
  const status = root.querySelector(".scroll-status");
  const statusText = root.querySelector(".scroll-status-text");
  const statusSpinner = root.querySelector(".scroll-status .progress");
  const size = root.querySelector(".scroll-size");
  const hint = root.querySelector(".scroll-hint");
  const lock = root.querySelector(".scroll-lock");
  const finish = root.querySelector(".scroll-finish");
  const start = root.querySelector(".scroll-start");
  const undo = root.querySelector(".scroll-undo");
  const cancel = root.querySelector(".scroll-cancel");
  const axisGroup = root.querySelector(".scroll-axis");
  const axisButtons = Array.from(root.querySelectorAll(".scroll-axis-option")).filter(
    (button): button is HTMLButtonElement => button instanceof HTMLButtonElement,
  );
  if (
    !(status instanceof HTMLElement) ||
    !(statusText instanceof HTMLElement) ||
    !(statusSpinner instanceof HTMLElement) ||
    !(size instanceof HTMLElement) ||
    !(hint instanceof HTMLElement) ||
    !(lock instanceof HTMLElement) ||
    !(finish instanceof HTMLButtonElement) ||
    !(start instanceof HTMLButtonElement) ||
    !(undo instanceof HTMLButtonElement) ||
    !(cancel instanceof HTMLButtonElement) ||
    !(axisGroup instanceof HTMLElement) ||
    axisButtons.length !== 2
  ) {
    return () => undefined;
  }

  let last: ScrollStatus | null = null;
  // 取消/完成/开始请求进行中:期间按钮禁用,进行中文案持续显示直到结果替换(ADR-2)。
  let actionBusy = false;
  // 方向切换请求进行中:期间方向按钮禁用,避免连点覆盖。
  let axisBusy = false;
  // 控制窗就绪信号每次会话只发一次;Rust 侧据此才开始 Windows 自动滚动计时,
  // 保证首个自动 nudge 不会在方向按钮可交互前制造首个内容变化(横向被锁死)。
  let readySent = false;

  const axisOf = (payload: ScrollStatus | null): CaptureAxis => payload?.axis ?? "vertical";
  /** 显式开始后方向锁定(与 Rust `axis_switch_allowed` 同口径)。 */
  const axisLocked = (payload: ScrollStatus | null): boolean =>
    payload !== null && payload.state !== "ready";

  const render = (): void => {
    const axis = axisOf(last);
    const locked = axisLocked(last);
    const ready = last !== null && last.state === "ready";
    const axisDisabled =
      last === null || locked || actionBusy || axisBusy || last.state === "finishing";
    for (const button of axisButtons) {
      const active = axisFromDataset(button.dataset.axis) === axis;
      button.classList.toggle("is-active", active);
      button.setAttribute("aria-checked", active ? "true" : "false");
      // radiogroup 漫游 tab 序:仅当前方向可 Tab 到达,其余靠方向键/Home/End。
      button.tabIndex = active ? 0 : -1;
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

    // R7:finishing 进行中呈现「正在拼接 · N 段」+ 共享旋转圈,常驻到被
    // 结果替换(成功收窗/失败走 failed 文案,ADR-2);其余状态不转圈。
    const finishing = last?.state === "finishing";
    statusSpinner.hidden = !finishing;
    if (!last) {
      status.classList.remove("is-error");
      statusText.textContent = t(statusKey("ready", axis));
      size.textContent = "";
    } else {
      status.classList.toggle("is-error", last.state === "failed");
      statusText.textContent = finishing
        ? t("scroll.status.finishing_segments", { count: last.segmentCount })
        : t(statusKey(last.state, last.axis));
      // 尺寸一直显示：这就是正在截的那一块，滚动后沿轴变长。
      const parts = [t("scroll.watching", { width: last.width, height: last.height })];
      if (last.appended > 0) {
        const appendedKey: CatalogKey =
          last.axis === "horizontal" ? "scroll.appended.horizontal" : "scroll.appended";
        parts.push(t(appendedKey, { count: last.appended }));
      }
      size.textContent = parts.join(" · ");
    }
    // 开始按钮只在就绪态出现;开始后被开始信号接管,不保留入口。
    start.hidden = !ready;
    start.disabled = actionBusy;
    // 回退:栈空(只剩首帧/未开始)、finishing 或动作进行中禁用并附原因。
    const undoDisabled =
      !last || !last.canUndo || actionBusy || last.state === "finishing";
    undo.disabled = undoDisabled;
    if (last && !last.canUndo) {
      undo.dataset.tooltip = t("scroll.undo.empty");
    } else {
      delete undo.dataset.tooltip;
    }
    const busy = actionBusy || last?.state === "finishing";
    finish.disabled = busy || !last;
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
    start.disabled = false;
    undo.disabled = false;
    cancel.disabled = false;
    apply(null);
    void invoke<ScrollStatus | null>("get_scroll_status").then(apply);
  };

  const showError = (message: string): void => {
    actionBusy = false;
    finish.disabled = false;
    start.disabled = false;
    undo.disabled = false;
    cancel.disabled = false;
    statusText.textContent = message;
    statusSpinner.hidden = true;
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
    undo.disabled = true;
    cancel.disabled = true;
    status.classList.remove("is-error");
    // 乐观呈现段数(以最近状态为准),拼接线程的 finishing 事件随即对齐。
    statusText.textContent = t("scroll.status.finishing_segments", {
      count: last?.segmentCount ?? 1,
    });
    statusSpinner.hidden = false;
    void invoke("finish_scroll_capture").catch(() => {
      showActionError("scroll.action.finish_failed");
    });
  });

  const undoSegment = (): void => {
    if (actionBusy || !last || !last.canUndo || last.state === "finishing") {
      return;
    }
    undo.disabled = true;
    void invoke("undo_scroll_segment")
      .then(() => {
        // 回退结果由下一次 scroll-status 事件带回(尺寸/已追加长度回拨)。
        void invoke<ScrollStatus | null>("get_scroll_status")
          .then(apply)
          .catch(() => undefined);
      })
      .catch((error) => {
        showError(messageOf(error, "scroll.action.undo_failed"));
      })
      .finally(() => {
        render();
      });
  };
  undo.addEventListener("click", undoSegment);

  const startCapture = (): void => {
    if (actionBusy || last?.state !== "ready") {
      return;
    }
    actionBusy = true;
    start.disabled = true;
    void invoke("start_scroll_capture")
      .then(() => {
        actionBusy = false;
      })
      .catch(() => {
        showActionError("scroll.action.start_failed");
      });
  };
  start.addEventListener("click", startCapture);

  const cancelCapture = (): void => {
    if (actionBusy) {
      return;
    }
    actionBusy = true;
    finish.disabled = true;
    start.disabled = true;
    undo.disabled = true;
    cancel.disabled = true;
    status.classList.remove("is-error");
    statusText.textContent = t("scroll.action.canceling");
    statusSpinner.hidden = true;
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
  // R2:方向选择 radiogroup 化:方向键/Home/End 漫游并互斥选中(共享 a11y 实现)。
  axisGroup.addEventListener("keydown", (event) => {
    handleRadioGroupKeydown(event, axisGroup, ".scroll-axis-option");
  });

  void invoke<ScrollStatus | null>("get_scroll_status").then(apply);
  void listen<ScrollStatus>("scroll-status", (event) => apply(event.payload));
  // 控制窗被下一次会话复用时,Rust 发 reload 让本视图清掉上一次的状态。
  void listen("scroll-reload", reset);
  render();

  return render;
}
