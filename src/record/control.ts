import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import {
  isAnnotationTool,
  STYLE_COLORS,
  STYLE_TEXT_SIZES,
  STYLE_WIDTHS,
  type AnnotationTool,
} from "../annotation";
import { REGION_TOOL_FIELDS, type RegionTools } from "../settings";
import { currentLanguage, t, type CatalogKey } from "../i18n";
import {
  formatDuration,
  type PendingRecording,
  type RecordingHudState,
  type RecordingPreview,
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
  ready: "record.status.ready",
  countdown: "record.status.countdown",
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
          <button type="button" class="record-btn record-discard" data-i18n="record.pending.discard" hidden></button>
          <button type="button" class="record-btn record-draw" data-i18n="record.bar.draw" hidden></button>
        </div>
      </div>
      <div class="record-preview" hidden>
        <img class="record-preview-image" alt="" hidden />
        <video class="record-preview-video" muted autoplay loop playsinline hidden></video>
        <p class="record-preview-meta"></p>
      </div>
      <div class="record-annotate" data-record-annotate hidden></div>
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
  const discard = root.querySelector(".record-discard");
  const draw = root.querySelector(".record-draw");
  const annotateHost = root.querySelector("[data-record-annotate]");
  const previewBox = root.querySelector(".record-preview");
  const previewImage = root.querySelector(".record-preview-image");
  const previewVideo = root.querySelector(".record-preview-video");
  const previewMeta = root.querySelector(".record-preview-meta");
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
    !(discard instanceof HTMLButtonElement) ||
    !(draw instanceof HTMLButtonElement) ||
    !(annotateHost instanceof HTMLElement) ||
    !(previewBox instanceof HTMLElement) ||
    !(previewImage instanceof HTMLImageElement) ||
    !(previewVideo instanceof HTMLVideoElement) ||
    !(previewMeta instanceof HTMLElement) ||
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
  let previewError = "";
  let previewKey = "";
  let previewUrl = "";
  let overlayNotice = "";
  let appliedHeight = -1;
  let timer: number | null = null;
  let annotateToken = 0;
  let activeTool: AnnotationTool | null = null;
  let styleOpen = false;
  const strokeTools = new Set<AnnotationTool>(["arrow", "rect", "ellipse", "highlighter"]);
  const textTools = new Set<AnnotationTool>(["text", "number", "bubble"]);
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
      // 工具行带横向滚动时,被压扁后 offsetHeight 会变成 0,要用内容高度把窗口撑开。
      let height = node.offsetHeight;
      if (
        node.classList.contains("record-annotate") ||
        node.classList.contains("record-row") ||
        node.classList.contains("record-notice")
      ) {
        height = Math.max(height, node.scrollHeight);
      }
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
    } else if (previewError) {
      text = previewError;
      error = true;
    } else if (status?.behind && (status.phase === "recording" || status.phase === "paused")) {
      text = t("record.hud.behind");
    } else if (status?.autoStopped || state?.preview?.autoStopped) {
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

  const releasePreview = (): void => {
    if (previewUrl) {
      URL.revokeObjectURL(previewUrl);
      previewUrl = "";
    }
    previewImage.hidden = true;
    previewVideo.hidden = true;
    previewImage.removeAttribute("src");
    previewVideo.removeAttribute("src");
  };

  const loadPreview = async (item: RecordingPreview): Promise<void> => {
    if (item.tempPath === previewKey) {
      return;
    }
    previewKey = item.tempPath;
    previewError = "";
    releasePreview();
    try {
      const parts: Uint8Array[] = [];
      let offset = 0;
      for (;;) {
        const chunk = await invoke<string | null>("read_recording_preview_chunk", {
          tempPath: item.tempPath,
          offset,
        });
        if (!chunk) {
          break;
        }
        const binary = atob(chunk);
        const bytes = new Uint8Array(binary.length);
        for (let index = 0; index < binary.length; index += 1) {
          bytes[index] = binary.charCodeAt(index);
        }
        parts.push(bytes);
        offset += bytes.length;
        if (previewKey !== item.tempPath) {
          return;
        }
      }
      const type =
        item.format === "mp4" ? "video/mp4" : item.format === "webp" ? "image/webp" : "image/gif";
      const total = parts.reduce((sum, part) => sum + part.length, 0);
      const merged = new Uint8Array(total);
      let cursor = 0;
      for (const part of parts) {
        merged.set(part, cursor);
        cursor += part.length;
      }
      const blob = new Blob([merged.buffer], { type });
      previewUrl = URL.createObjectURL(blob);
      if (item.format === "mp4") {
        previewVideo.src = previewUrl;
        previewVideo.hidden = false;
        void previewVideo.play().catch(() => {
          previewError = t("record.preview.failed");
          notice.hidden = false;
          notice.textContent = previewError;
          notice.classList.add("is-error");
        });
      } else {
        previewImage.src = previewUrl;
        previewImage.hidden = false;
      }
    } catch {
      if (previewKey === item.tempPath) {
        previewError = t("record.preview.failed");
        render();
      }
    }
  };

  const markAnnotateTool = (): void => {
    annotateHost.querySelectorAll<HTMLButtonElement>("[data-tool]").forEach((button) => {
      button.classList.toggle("is-active", button.dataset.tool === activeTool);
    });
    const styleButton = annotateHost.querySelector("[data-annotate-style]");
    if (styleButton instanceof HTMLButtonElement) {
      styleButton.classList.toggle("is-active", styleOpen);
    }
  };

  const syncAnnotateStyle = (): void => {
    const showStyle = styleOpen;
    annotateHost.querySelectorAll<HTMLElement>("[data-tool], [data-annotate-action]").forEach((node) => {
      node.hidden = showStyle;
    });
    annotateHost.querySelectorAll<HTMLElement>("[data-style-color]").forEach((node) => {
      node.hidden = !showStyle;
    });
    const stroke = activeTool !== null && strokeTools.has(activeTool);
    const text = activeTool !== null && textTools.has(activeTool);
    annotateHost.querySelectorAll<HTMLElement>("[data-style-width]").forEach((node) => {
      node.hidden = !showStyle || !stroke;
    });
    annotateHost.querySelectorAll<HTMLElement>("[data-style-text]").forEach((node) => {
      node.hidden = !showStyle || !text;
    });
    markAnnotateTool();
  };

  const fillAnnotateTools = async (): Promise<void> => {
    const token = ++annotateToken;
    const settings = await invoke<{ regionTools?: RegionTools }>("get_ui_settings").catch(() => null);
    if (token !== annotateToken || state?.interactive !== true) {
      return;
    }
    const chosen = settings?.regionTools;
    const enabled = (id: (typeof REGION_TOOL_FIELDS)[number]["id"]): boolean => {
      if (chosen) {
        return chosen[id];
      }
      return (
        id === "arrow" ||
        id === "rect" ||
        id === "ellipse" ||
        id === "highlighter" ||
        id === "mosaic" ||
        id === "text"
      );
    };
    annotateHost.replaceChildren();
    const append = (button: HTMLButtonElement): void => {
      annotateHost.append(button);
    };
    for (const field of REGION_TOOL_FIELDS) {
      const toolId = field.id;
      if (!enabled(toolId) || !isAnnotationTool(toolId)) {
        continue;
      }
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.tool = toolId;
      button.textContent = t(field.labelKey);
      button.addEventListener("click", () => {
        activeTool = toolId;
        styleOpen = false;
        markAnnotateTool();
        syncAnnotateStyle();
        void emit("record-annotate-tool", { tool: toolId });
      });
      append(button);
    }
    for (const action of ["undo", "redo"] as const) {
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.annotateAction = action;
      button.textContent = t(action === "undo" ? "preview.tool.undo" : "preview.tool.redo");
      button.addEventListener("click", () => {
        void emit("record-annotate-action", { action });
      });
      append(button);
    }
    const styleButton = document.createElement("button");
    styleButton.type = "button";
    styleButton.dataset.annotateStyle = "true";
    styleButton.textContent = t("preview.tool.style_title");
    styleButton.addEventListener("click", () => {
      styleOpen = !styleOpen;
      syncAnnotateStyle();
    });
    append(styleButton);
    for (const color of STYLE_COLORS) {
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.styleColor = color;
      button.style.setProperty("--swatch", color);
      button.hidden = true;
      button.addEventListener("click", () => {
        void emit("record-annotate-style", { color });
      });
      append(button);
    }
    for (const width of STYLE_WIDTHS) {
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.styleWidth = String(width.value);
      button.textContent = t(width.labelKey);
      button.hidden = true;
      button.addEventListener("click", () => {
        void emit("record-annotate-style", { width: width.value });
      });
      append(button);
    }
    for (const size of STYLE_TEXT_SIZES) {
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.styleText = String(size.value);
      button.textContent = t(size.labelKey);
      button.hidden = true;
      button.addEventListener("click", () => {
        void emit("record-annotate-style", { textSize: size.value });
      });
      append(button);
    }
    if (activeTool === null) {
      const first = annotateHost.querySelector<HTMLButtonElement>("[data-tool]");
      if (first?.dataset.tool && isAnnotationTool(first.dataset.tool)) {
        activeTool = first.dataset.tool;
      }
    }
    syncAnnotateStyle();
  };

  const render = (): void => {
    const status = state?.status ?? null;
    const preview = state?.preview ?? null;
    const reviewing = preview !== null || status?.phase === "finished";
    const fpsLabel = t("record.bar.fps", {
      fps: String(preview?.fps ?? status?.fps ?? state?.fps ?? 0),
    });
    const formatLabel = (status?.format ?? preview?.format ?? state?.format ?? "").toUpperCase();
    const phaseIsReady = status?.phase === "ready" || status?.phase === "countdown";
    time.textContent = status
      ? phaseIsReady
        ? // 就绪/倒计时带上尺寸:R4/R5 拖框改宽高时 HUD 实时反映最终框
          // (状态 width/height 是捕获矩形;确认框尺寸在 state.region)。
          `${formatLabel} · ${fpsLabel} · ${state?.region?.width ?? status.width}×${state?.region?.height ?? status.height}`
        : `${formatDuration(status.elapsedMs)} / ${formatDuration(state?.limitMs ?? 0)} · ${fpsLabel}`
      : preview
        ? `${formatDuration(preview.durationMs)} · ${fpsLabel}`
        : state?.fps
          ? `${formatLabel} · ${fpsLabel}`
          : "--:--";
    // 倒计时时阶段行带上剩余秒数:降级平台(overlay 隐藏)也能看到倒数。
    const countdownLeft =
      status?.phase === "countdown" && typeof status.countdownMs === "number"
        ? Math.max(1, Math.ceil(status.countdownMs / 1000))
        : 0;
    phase.textContent = status
      ? countdownLeft > 0
        ? `${t(PHASE_KEYS[status.phase])} ${countdownLeft}`
        : t(PHASE_KEYS[status.phase])
      : "";
    root.dataset.phase = status?.phase ?? (preview ? "finished" : "idle");
    root.classList.toggle("is-paused", status?.phase === "paused");
    root.classList.toggle("is-stopped", reviewing);
    root.classList.toggle("is-failed", status?.phase === "failed" || lastError.length > 0);

    previewBox.hidden = preview === null;
    if (preview) {
      previewMeta.textContent = `${formatDuration(preview.durationMs)} · ${fpsLabel} · ${preview.width}×${preview.height}`;
      void loadPreview(preview);
    } else if (previewKey) {
      previewKey = "";
      previewError = "";
      releasePreview();
    }

    toggle.hidden = !status || reviewing || phaseIsReady;
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

    // 就绪/倒计时态:停止按钮变成「取消」——stop_recording_from_hud 此时走取消收尾。
    const cancelling = status?.phase === "ready" || status?.phase === "countdown";
    stop.hidden = !status && !reviewing;
    stop.disabled = busy;
    stop.dataset.action = reviewing ? "save" : "stop";
    stop.textContent = cancelling
      ? t("record.bar.cancel")
      : reviewing
        ? t("record.bar.save")
        : status?.phase === "failed"
          ? t("record.bar.close")
          : t("record.bar.stop");
    discard.hidden = !reviewing;
    discard.disabled = busy;

    draw.hidden = !status || reviewing || phaseIsReady;
    const drawCapable = status?.phase === "recording" || status?.phase === "paused";
    draw.disabled = busy || !drawCapable;
    draw.textContent = state?.interactive
      ? t("record.bar.draw_done")
      : t("record.bar.draw");
    draw.classList.toggle("is-active", state?.interactive === true);
    annotateHost.hidden = state?.interactive !== true;
    if (state?.interactive === true && annotateHost.childElementCount === 0) {
      void fillAnnotateTools();
    }
    if (state?.interactive !== true) {
      annotateToken += 1;
      styleOpen = false;
      activeTool = null;
      annotateHost.replaceChildren();
    }

    const noticeInfo = noticeText();
    notice.hidden = noticeInfo.text.length === 0;
    notice.textContent = noticeInfo.text;
    notice.classList.toggle("is-error", noticeInfo.error);

    // 就绪态「开始」为独立按钮;倒计时已开始则不重复显示,取消仍可用。
    const showReadyStart = status?.phase === "ready";
    const showRestart = status === null && state?.preview == null && state?.hasContext === true;
    start.hidden = !(showReadyStart || showRestart);
    start.disabled = busy;
    start.textContent = showReadyStart ? t("record.bar.start_ready") : t("record.bar.start");
    close.hidden = status !== null || state?.preview != null;
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
    if (!busy && next.status === null && next.pending.length === 0 && !next.preview) {
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

  const savePreview = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    lastError = "";
    render();
    try {
      const outcome = await invoke<StopOutcome>("save_recording_preview");
      if (outcome.kind === "failed") {
        lastError = outcome.message;
      }
    } catch (error) {
      lastError = messageOf(error);
    }
    busy = false;
    await refresh();
  };

  const discardPreview = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    lastError = "";
    previewKey = "";
    releasePreview();
    render();
    try {
      await invoke<boolean>("discard_recording_preview");
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
  stop.addEventListener("click", () => {
    if (stop.dataset.action === "save") {
      void savePreview();
    } else {
      void stopRecording();
    }
  });
  window.addEventListener("keydown", (event) => {
    const phaseNow = state?.status?.phase;
    if (phaseNow === "ready" || phaseNow === "countdown") {
      if (event.key === " " || event.code === "Space") {
        event.preventDefault();
        void runControl("start");
      } else if (event.key === "Escape") {
        event.preventDefault();
        void stopRecording();
      }
    }
  });
  discard.addEventListener("click", () => void discardPreview());
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
  void listen<{ tool?: string }>("record-annotate-active", (event) => {
    const tool = event.payload.tool;
    if (!tool || !isAnnotationTool(tool)) {
      return;
    }
    activeTool = tool;
    syncAnnotateStyle();
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
  resizeObserver.observe(previewBox);
  void listen<RecordingHudState>("record-hud-state", (event) => {
    apply(event.payload);
  });
  // 预创建窗口可能错过打开事件(视图重载等):挂载即刷新一次。
  void refresh();

  return render;
}
