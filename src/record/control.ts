import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { currentLanguage, t, type CatalogKey } from "../i18n";
import {
  formatDuration,
  type PendingRecording,
  type RecordingHudState,
  type RecordingStatus,
  type StopOutcome,
} from "./types";
import "./record.css";

// R3 录制控制条:实时时长 + 暂停/继续/停止 + 标注模式开关。
// 保存失败保留的待处理录制在这里重试保存或丢弃;没有活动会话但有待处理产物或
// 区域上下文时提供「重新录制」与「关闭」。停止/保存成功或全部丢弃后自动收起。

const POLL_MS = 400;
/** 与 `hud.rs` 的紧凑态 / 展开上限一致(逻辑像素)。 */
const CONTROL_HEIGHT = 64;
const CONTROL_HEIGHT_EXPANDED = 248;
/** 区域太矮时,标注层把提示改写进控制卡片的说明行。 */
const OVERLAY_NOTICE_EVENT = "record-overlay-notice";

const PHASE_KEYS: Record<RecordingStatus["phase"], CatalogKey> = {
  recording: "record.status.recording",
  paused: "record.status.paused",
  finished: "record.status.finished",
  failed: "record.status.failed",
};

function messageOf(error: unknown): string {
  if (typeof error === "string" && error.trim().length > 0) {
    // Rust 命令错误已是本地化用户文案,原样展示。
    return error;
  }
  return t("record.error.action", { detail: String(error) });
}

export function mountRecordControl(root: HTMLElement): () => void {
  root.className = "record-root";
  root.innerHTML = `
    <div class="record-card">
      <div class="record-row">
        <span class="record-pulse" aria-hidden="true"></span>
        <span class="record-time" data-i18n-title="record.bar.time_title"></span>
        <span class="record-phase"></span>
        <div class="record-actions">
          <button type="button" class="record-btn record-toggle" hidden></button>
          <button type="button" class="record-btn record-primary record-stop" hidden></button>
          <button type="button" class="record-btn record-draw" data-i18n="record.bar.draw" hidden></button>
        </div>
      </div>
      <p class="record-notice" role="status" hidden></p>
      <div class="record-pending" hidden>
        <div class="record-pending-head">
          <span class="record-pending-title"></span>
          <button type="button" class="record-btn record-start" data-i18n="record.bar.start" hidden></button>
          <button type="button" class="record-btn record-close" data-i18n="record.bar.close" hidden></button>
        </div>
        <ul class="record-pending-list"></ul>
      </div>
    </div>`;
  const card = root.querySelector(".record-card");
  const row = root.querySelector(".record-row");
  const time = root.querySelector(".record-time");
  const phase = root.querySelector(".record-phase");
  const toggle = root.querySelector(".record-toggle");
  const stop = root.querySelector(".record-stop");
  const draw = root.querySelector(".record-draw");
  const notice = root.querySelector(".record-notice");
  const pending = root.querySelector(".record-pending");
  const pendingTitle = root.querySelector(".record-pending-title");
  const pendingList = root.querySelector(".record-pending-list");
  const start = root.querySelector(".record-start");
  const close = root.querySelector(".record-close");
  if (
    !(card instanceof HTMLElement) ||
    !(row instanceof HTMLElement) ||
    !(time instanceof HTMLElement) ||
    !(phase instanceof HTMLElement) ||
    !(toggle instanceof HTMLButtonElement) ||
    !(stop instanceof HTMLButtonElement) ||
    !(draw instanceof HTMLButtonElement) ||
    !(notice instanceof HTMLElement) ||
    !(pending instanceof HTMLElement) ||
    !(pendingTitle instanceof HTMLElement) ||
    !(pendingList instanceof HTMLElement) ||
    !(start instanceof HTMLButtonElement) ||
    !(close instanceof HTMLButtonElement)
  ) {
    return () => undefined;
  }

  let state: RecordingHudState | null = null;
  let busy = false;
  let lastError = "";
  let overlayNotice = "";
  let appliedHeight = -1;
  let timer: number | null = null;
  const pendingView = { signature: "" };

  const readPx = (value: string): number => {
    const parsed = Number.parseFloat(value);
    return Number.isFinite(parsed) ? parsed : 0;
  };

  /** 卡片内容的逻辑像素高度(含根内边距)。待保存列表按完整内容计,窗口上限由宿主夹紧。 */
  const measureWindowHeight = (): number => {
    const cardStyle = getComputedStyle(card);
    const gap = readPx(cardStyle.rowGap);
    let content = 0;
    let visible = 0;
    for (const node of card.children) {
      if (!(node instanceof HTMLElement) || node.hidden) {
        continue;
      }
      let height = node.offsetHeight;
      if (node.classList.contains("record-pending")) {
        const list = node.querySelector(".record-pending-list");
        if (list instanceof HTMLElement) {
          height += Math.max(0, list.scrollHeight - list.clientHeight);
        }
      }
      content += height;
      visible += 1;
    }
    if (visible > 1) {
      content += gap * (visible - 1);
    }
    const cardChrome =
      readPx(cardStyle.paddingTop) +
      readPx(cardStyle.paddingBottom) +
      readPx(cardStyle.borderTopWidth) +
      readPx(cardStyle.borderBottomWidth);
    const rootStyle = getComputedStyle(root);
    const rootChrome = readPx(rootStyle.paddingTop) + readPx(rootStyle.paddingBottom);
    return Math.ceil(content + cardChrome + rootChrome);
  };

  const syncWindow = (): void => {
    if (!state?.region && (state?.pending.length ?? 0) === 0) {
      return;
    }
    // 待保存列表沿用展开上限,避免列表被压扁后又按剩余高度来回改窗口。
    const height =
      (state?.pending.length ?? 0) > 0 ? CONTROL_HEIGHT_EXPANDED : measureWindowHeight();
    if (appliedHeight >= 0 && Math.abs(height - appliedHeight) <= 1) {
      return;
    }
    appliedHeight = height;
    void invoke("set_recording_hud_expanded", {
      expanded: height > CONTROL_HEIGHT,
      contentHeight: height,
    });
  };

  const noticeText = (): { text: string; error: boolean } => {
    const status = state?.status ?? null;
    let text = "";
    let error = false;
    if (status?.error) {
      text = status.error;
      error = true;
    } else if (lastError) {
      text = lastError;
      error = true;
    } else if (status && (status.autoStopped || status.phase === "finished")) {
      text = t("toast.recording_auto_stopped");
    } else if (!status && (state?.pending.length ?? 0) > 0) {
      text = t("record.pending.kept");
    } else if (state?.capabilities.noticeKey) {
      text = t(state.capabilities.noticeKey as CatalogKey);
    }
    if (overlayNotice && !text.includes(overlayNotice)) {
      text = text ? `${text}\n${overlayNotice}` : overlayNotice;
      error = true;
    }
    return { text, error };
  };

  const renderPending = (): void => {
    const items = state?.pending ?? [];
    // 列表每秒轮询多次:签名不变时保留现有按钮,避免点击中途被替换;
    // 语言切换会改变按钮文案,因此签名带上当前语言。
    const signature = `${currentLanguage()}#${items.map((item) => item.tempPath).join("|")}#${busy}`;
    pending.hidden = items.length === 0;
    if (signature === pendingView.signature) {
      return;
    }
    pendingView.signature = signature;
    pendingTitle.textContent = t("record.pending.title", { count: items.length });
    pendingList.replaceChildren(
      ...items.map((item) => {
        const row = document.createElement("li");
        row.className = "record-pending-item";
        const meta = document.createElement("div");
        meta.className = "record-pending-meta";
        const name = document.createElement("span");
        name.className = "record-pending-name";
        name.textContent = item.fileName;
        name.dataset.tooltip = item.tempPath;
        const detail = document.createElement("span");
        detail.className = "record-pending-detail";
        detail.textContent = `${formatDuration(item.durationMs)} · ${item.width}×${item.height} · ${item.format.toUpperCase()}`;
        meta.append(name, detail);
        const retry = document.createElement("button");
        retry.type = "button";
        retry.className = "record-btn";
        retry.textContent = t("record.pending.retry");
        retry.disabled = busy;
        retry.addEventListener("click", () => void retrySave(item));
        const discard = document.createElement("button");
        discard.type = "button";
        discard.className = "record-btn";
        discard.textContent = t("record.pending.discard");
        discard.disabled = busy;
        discard.addEventListener("click", () => void discardPending(item));
        row.append(meta, retry, discard);
        return row;
      }),
    );
  };

  const render = (): void => {
    const status = state?.status ?? null;
    time.textContent = status
      ? `${formatDuration(status.elapsedMs)} / ${formatDuration(state?.limitMs ?? 0)}`
      : "--:--";
    phase.textContent = status ? t(PHASE_KEYS[status.phase]) : "";
    root.dataset.phase = status?.phase ?? "idle";
    root.classList.toggle("is-paused", status?.phase === "paused");
    root.classList.toggle("is-stopped", status?.phase === "finished");
    root.classList.toggle("is-failed", status?.phase === "failed" || lastError.length > 0);

    toggle.hidden = !status;
    // 标注模式期间暂停/继续不可用:先「完成标注」退出绘制层再控制录制
    // (降级平台的不透明绘制层会在恢复录制后入画)。
    toggle.disabled = busy || state?.interactive === true;
    if (status?.phase === "recording") {
      toggle.textContent = t("record.bar.pause");
      toggle.dataset.action = "pause";
    } else if (status?.phase === "paused") {
      toggle.textContent = t("record.bar.resume");
      toggle.dataset.action = "resume";
    } else {
      toggle.hidden = true;
    }

    stop.hidden = !status;
    stop.disabled = busy;
    stop.textContent =
      status?.phase === "finished"
        ? t("record.bar.save")
        : status?.phase === "failed"
          ? t("record.bar.close")
          : t("record.bar.stop");

    draw.hidden = !status;
    const drawCapable = status?.phase === "recording" || status?.phase === "paused";
    draw.disabled = busy || !drawCapable;
    draw.textContent = state?.interactive
      ? t("record.bar.draw_done")
      : t("record.bar.draw");
    draw.classList.toggle("is-active", state?.interactive === true);

    const noticeInfo = noticeText();
    notice.hidden = noticeInfo.text.length === 0;
    notice.textContent = noticeInfo.text;
    notice.classList.toggle("is-error", noticeInfo.error);

    start.hidden = status !== null || !state?.hasContext;
    start.disabled = busy;
    close.hidden = status !== null;
    close.disabled = busy;
    renderPending();
    syncWindow();
  };

  const apply = (next: RecordingHudState): void => {
    state = next;
    render();
    if (next.status !== null) {
      // 打开事件可能早于视图挂载(极早期录制/开发重载):见到会话就持续轮询。
      startPolling();
    }
    if (!busy && next.status === null && next.pending.length === 0) {
      stopPolling();
      void invoke("close_recording_hud");
    }
  };

  const refresh = async (): Promise<void> => {
    try {
      apply(await invoke<RecordingHudState>("get_recording_hud_state"));
    } catch {
      // 宿主暂不可用:下一次轮询重试,不把瞬时 IPC 失败当成业务错误。
    }
  };

  const startPolling = (): void => {
    if (timer === null) {
      timer = window.setInterval(() => void refresh(), POLL_MS);
    }
  };

  const stopPolling = (): void => {
    if (timer !== null) {
      window.clearInterval(timer);
      timer = null;
    }
  };

  const runControl = async (action: "pause" | "resume" | "start"): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    lastError = "";
    render();
    try {
      await invoke<RecordingStatus>("recording_control", { action });
    } catch (error) {
      lastError = messageOf(error);
    }
    busy = false;
    await refresh();
  };

  const stopRecording = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    lastError = "";
    render();
    try {
      const outcome = await invoke<StopOutcome>("stop_recording_from_hud");
      if (outcome.kind === "failed") {
        lastError = outcome.message;
      }
    } catch (error) {
      lastError = messageOf(error);
    }
    busy = false;
    await refresh();
  };

  const toggleDraw = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    lastError = "";
    render();
    try {
      apply(
        await invoke<RecordingHudState>("set_recording_hud_interactive", {
          interactive: !(state?.interactive === true),
        }),
      );
    } catch (error) {
      lastError = messageOf(error);
    }
    busy = false;
    render();
  };

  const retrySave = async (item: PendingRecording): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    lastError = "";
    render();
    try {
      const outcome = await invoke<StopOutcome>("retry_recording_save", {
        tempPath: item.tempPath,
      });
      if (outcome.kind === "failed") {
        lastError = outcome.message;
      }
    } catch (error) {
      lastError = messageOf(error);
    }
    busy = false;
    await refresh();
  };

  const discardPending = async (item: PendingRecording): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    lastError = "";
    render();
    try {
      await invoke<boolean>("discard_pending_recording", {
        tempPath: item.tempPath,
      });
    } catch (error) {
      lastError = messageOf(error);
    }
    busy = false;
    await refresh();
  };

  toggle.addEventListener("click", () => {
    const action = toggle.dataset.action;
    if (action === "pause" || action === "resume") {
      void runControl(action);
    }
  });
  stop.addEventListener("click", () => void stopRecording());
  draw.addEventListener("click", () => void toggleDraw());
  start.addEventListener("click", () => void runControl("start"));
  close.addEventListener("click", () => {
    stopPolling();
    void invoke("close_recording_hud");
  });

  void listen("record-hud-open", () => {
    lastError = "";
    appliedHeight = -1;
    startPolling();
    void refresh();
  });
  void listen("record-hud-close", () => {
    stopPolling();
    overlayNotice = "";
    appliedHeight = -1;
  });
  void listen<string>(OVERLAY_NOTICE_EVENT, (event) => {
    const text = typeof event.payload === "string" ? event.payload : "";
    if (text === overlayNotice) {
      return;
    }
    overlayNotice = text;
    render();
  });
  const resizeObserver = new ResizeObserver(() => {
    syncWindow();
  });
  resizeObserver.observe(row);
  resizeObserver.observe(notice);
  void listen<RecordingHudState>("record-hud-state", (event) => {
    apply(event.payload);
  });
  // 预创建窗口可能错过打开事件(视图重载等):挂载即刷新一次。
  void refresh();

  return render;
}
