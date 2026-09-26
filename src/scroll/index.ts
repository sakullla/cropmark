import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t, type CatalogKey } from "../i18n";
import "./scroll.css";

/** Rust 侧 `ScrollStatus` 的镜像;`state` 映射到 `scroll.status.*` 词条。 */
interface ScrollStatus {
  state: string;
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

/**
 * 长截图控制窗(R1):置顶非模态、位于框选区域旁;承载状态提示与
 * 「完成/取消」。窗口本身不参与截图内容,状态由 Rust 周期推送。
 */
export function mountScroll(root: HTMLElement): () => void {
  root.className = "scroll-root";
  root.innerHTML = `
    <div class="scroll-card">
      <div class="scroll-head">
        <span class="scroll-title" data-i18n="scroll.title"></span>
        <span class="scroll-size"></span>
      </div>
      <p class="scroll-status" role="status"></p>
      <p class="scroll-hint" data-i18n="scroll.hint"></p>
      <div class="scroll-actions">
        <button type="button" class="scroll-finish" data-i18n="scroll.finish"></button>
        <button type="button" class="scroll-cancel" data-i18n="scroll.cancel"></button>
      </div>
    </div>`;
  const status = root.querySelector(".scroll-status");
  const size = root.querySelector(".scroll-size");
  const finish = root.querySelector(".scroll-finish");
  const cancel = root.querySelector(".scroll-cancel");
  if (
    !(status instanceof HTMLElement) ||
    !(size instanceof HTMLElement) ||
    !(finish instanceof HTMLButtonElement) ||
    !(cancel instanceof HTMLButtonElement)
  ) {
    return () => undefined;
  }

  let last: ScrollStatus | null = null;
  // 取消/完成请求进行中:期间两按钮禁用,进行中文案持续显示直到结果替换(ADR-2)。
  let actionBusy = false;

  const render = (): void => {
    if (!last) {
      status.classList.remove("is-error");
      status.textContent = t("scroll.status.running");
      size.textContent = "";
      return;
    }
    status.classList.toggle("is-error", last.state === "failed");
    status.textContent = t(STATUS_KEYS[last.state] ?? "scroll.status.running");
    // 已拼接条数与高度一起展示,滚动中能看到进展。
    const parts: string[] = [];
    if (last.appended > 0) {
      parts.push(t("scroll.appended", { count: last.appended }));
    }
    if (last.height > 0) {
      parts.push(t("scroll.height", { height: last.height }));
    }
    size.textContent = parts.join(" · ");
    const busy = actionBusy || last.state === "finishing";
    finish.disabled = busy;
    cancel.disabled = busy;
  };

  const apply = (payload: ScrollStatus | null): void => {
    last = payload;
    render();
  };

  const reset = (): void => {
    actionBusy = false;
    finish.disabled = false;
    cancel.disabled = false;
    apply(null);
    void invoke<ScrollStatus | null>("get_scroll_status").then(apply);
  };

  const showActionError = (key: CatalogKey): void => {
    actionBusy = false;
    finish.disabled = false;
    cancel.disabled = false;
    status.textContent = t(key);
    status.classList.add("is-error");
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

  void invoke<ScrollStatus | null>("get_scroll_status").then(apply);
  void listen<ScrollStatus>("scroll-status", (event) => apply(event.payload));
  // 控制窗被下一次会话复用时,Rust 发 reload 让本视图清掉上一次的状态。
  void listen("scroll-reload", reset);
  render();

  return render;
}
