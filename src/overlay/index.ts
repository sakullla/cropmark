import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { t, type CatalogKey } from "../i18n";
import "./overlay.css";

type CaptureMode = "region" | "window" | "fullscreen";

interface ListedWindow {
  id: string;
  title: string;
  pid: number;
  x: number;
  y: number;
  width: number;
  height: number;
  visible: boolean;
  ownerIsSelf: boolean;
}

/** R24:覆盖层能力子集;旧后端缺失时按默认可用处理,未知字段天然忽略。 */
interface OverlayCapabilities {
  inlineAnnotation?: boolean;
}

interface OverlayFrame {
  mode: CaptureMode;
  pngBase64: string;
  width: number;
  height: number;
  scale: number;
  logicalWidth: number;
  logicalHeight: number;
  reducedCapabilities: boolean;
  capabilities?: OverlayCapabilities;
  windows: ListedWindow[];
}

interface Selection {
  x: number;
  y: number;
  width: number;
  height: number;
}

/// R13:Wayland Web 覆盖层缺少的原生能力说明(不伪造不可用功能)。
const REDUCED_CAPABILITIES: Array<{ nameKey: CatalogKey; detailKey: CatalogKey }> = [
  {
    nameKey: "overlay.caps.toolbar_name",
    detailKey: "overlay.caps.toolbar_detail",
  },
  { nameKey: "overlay.caps.magnifier_name", detailKey: "overlay.caps.magnifier_detail" },
  { nameKey: "overlay.caps.color_name", detailKey: "overlay.caps.color_detail" },
  { nameKey: "overlay.caps.nudge_name", detailKey: "overlay.caps.nudge_detail" },
];

/// 触发不可用能力时的即时说明:同一事实在面板与按键反馈里保持一致。
const TOOLBAR_NOTICE_KEY: CatalogKey = "overlay.notice.toolbar";
const COLOR_NOTICE_KEY: CatalogKey = "overlay.notice.color";
const NUDGE_NOTICE_KEY: CatalogKey = "overlay.notice.nudge";

export function mountOverlay(root: HTMLElement): () => void {
  root.className = "overlay-root";
  root.innerHTML = `
    <canvas></canvas>
    <div class="overlay-chrome">
      <div class="overlay-hint"></div>
      <button type="button" class="overlay-capabilities" aria-expanded="false" data-i18n="overlay.capabilities" hidden>能力说明</button>
      <button type="button" class="overlay-cancel" data-i18n="overlay.cancel">取消 Esc</button>
    </div>
    <aside class="capability-panel" hidden></aside>
    <div class="overlay-notice" role="status" hidden></div>
    <div class="size-badge" hidden></div>
    <div class="window-list" hidden></div>
  `;
  const canvas = root.querySelector("canvas");
  const hint = root.querySelector(".overlay-hint");
  const badge = root.querySelector(".size-badge");
  const list = root.querySelector(".window-list");
  const cancelBtn = root.querySelector(".overlay-cancel");
  const capabilityToggle = root.querySelector(".overlay-capabilities");
  const capabilityPanel = root.querySelector(".capability-panel");
  const notice = root.querySelector(".overlay-notice");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(hint instanceof HTMLElement) ||
    !(badge instanceof HTMLElement) ||
    !(list instanceof HTMLElement) ||
    !(cancelBtn instanceof HTMLButtonElement) ||
    !(capabilityToggle instanceof HTMLButtonElement) ||
    !(capabilityPanel instanceof HTMLElement) ||
    !(notice instanceof HTMLElement)
  ) {
    return () => undefined;
  }

  const ctx = canvas.getContext("2d", { alpha: false });
  if (!ctx) {
    return () => undefined;
  }

  let frame: OverlayFrame | null = null;
  let image: HTMLImageElement | null = null;
  let dragging = false;
  let startX = 0;
  let startY = 0;
  let selection: Selection | null = null;
  let hoverId: string | null = null;
  let finishing = false;
  let raf = 0;
  let noticeTimer = 0;

  /// 与 `confirm_region` 发送的整数裁剪矩形完全一致:徽标数值、挖洞区域
  /// 与实际裁剪结果同源,且保证 x+width/y+height 不越出冻结帧(R13)。
  const roundedRect = (): Selection | null => {
    if (!frame || !selection) {
      return null;
    }
    const left = clamp(Math.round(selection.x), 0, frame.width);
    const top = clamp(Math.round(selection.y), 0, frame.height);
    const right = clamp(Math.round(selection.x + selection.width), 0, frame.width);
    const bottom = clamp(Math.round(selection.y + selection.height), 0, frame.height);
    return { x: left, y: top, width: right - left, height: bottom - top };
  };

  const showNotice = (message: string): void => {
    notice.textContent = message;
    notice.hidden = false;
    if (noticeTimer) {
      window.clearTimeout(noticeTimer);
    }
    noticeTimer = window.setTimeout(() => {
      noticeTimer = 0;
      notice.hidden = true;
    }, 3600);
  };

  const setCapabilityPanel = (open: boolean): void => {
    capabilityPanel.hidden = !open;
    capabilityToggle.setAttribute("aria-expanded", open ? "true" : "false");
    capabilityToggle.textContent = t(
      open ? "overlay.capabilities_collapse" : "overlay.capabilities",
    );
  };

  const renderCapabilityPanel = (): void => {
    const title = document.createElement("h2");
    title.textContent = t("overlay.panel.title");
    const available = document.createElement("p");
    available.className = "available";
    available.textContent = t("overlay.panel.available");
    const listEl = document.createElement("dl");
    for (const item of REDUCED_CAPABILITIES) {
      const name = document.createElement("dt");
      name.textContent = t(item.nameKey);
      const detail = document.createElement("dd");
      detail.textContent = t(item.detailKey);
      listEl.append(name, detail);
    }
    capabilityPanel.replaceChildren(title, available, listEl);
  };

  const renderHint = (): void => {
    if (!frame) {
      return;
    }
    const reduced = frame.reducedCapabilities === true;
    hint.textContent =
      frame.mode === "window"
        ? t("overlay.hint.window")
        : reduced
          ? t("overlay.hint.reduced")
          : t("overlay.hint.region");
  };

  const fitCanvas = (): void => {
    const rect = canvas.getBoundingClientRect();
    const dpr = Math.min(window.devicePixelRatio || 1, 1.5);
    const width = Math.max(1, Math.round(rect.width * dpr));
    const height = Math.max(1, Math.round(rect.height * dpr));
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
  };

  const physicalPoint = (event: MouseEvent): { x: number; y: number } => {
    if (!frame) {
      return { x: 0, y: 0 };
    }
    const rect = canvas.getBoundingClientRect();
    return {
      x: clamp(((event.clientX - rect.left) / Math.max(rect.width, 1)) * frame.width, 0, frame.width),
      y: clamp(((event.clientY - rect.top) / Math.max(rect.height, 1)) * frame.height, 0, frame.height),
    };
  };

  const toCanvas = (x: number, y: number, width = 0, height = 0): Selection => {
    if (!frame) {
      return { x, y, width, height };
    }
    return {
      x: (x / frame.width) * canvas.width,
      y: (y / frame.height) * canvas.height,
      width: (width / frame.width) * canvas.width,
      height: (height / frame.height) * canvas.height,
    };
  };

  const draw = (): void => {
    if (!image || !frame) {
      return;
    }
    fitCanvas();
    ctx.drawImage(image, 0, 0, canvas.width, canvas.height);
    ctx.fillStyle = "rgba(12, 10, 9, 0.48)";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    if (frame.mode === "window") {
      // 窗口模式不挖洞(Snipaste 惯例):整帧均匀压暗,冻结帧中缺失的
      // 后开窗口不再呈现黑洞;悬停窗口画 accent 描边 + 轻微 accent 填充,
      // 其余候选窗口 1px 浅色描边。
      for (const listed of frame.windows) {
        const rect = windowRectOnFrame(listed, frame);
        if (!rect) {
          continue;
        }
        const mapped = toCanvas(rect.x, rect.y, rect.width, rect.height);
        const active = hoverId === listed.id;
        if (active) {
          ctx.fillStyle = "rgba(45, 212, 191, 0.16)";
          ctx.fillRect(mapped.x, mapped.y, mapped.width, mapped.height);
          ctx.strokeStyle = "#2dd4bf";
          ctx.lineWidth = 2.5;
          ctx.strokeRect(
            mapped.x + 1.25,
            mapped.y + 1.25,
            mapped.width - 2.5,
            mapped.height - 2.5,
          );
        } else {
          ctx.strokeStyle = "rgba(245, 240, 232, 0.55)";
          ctx.lineWidth = 1;
          ctx.strokeRect(mapped.x + 0.5, mapped.y + 0.5, mapped.width - 1, mapped.height - 1);
        }
      }
      return;
    }
    const crop = roundedRect();
    if (!crop || crop.width < 1 || crop.height < 1) {
      badge.hidden = true;
      return;
    }
    const mapped = toCanvas(crop.x, crop.y, crop.width, crop.height);
    ctx.save();
    ctx.globalCompositeOperation = "destination-out";
    ctx.fillRect(mapped.x, mapped.y, mapped.width, mapped.height);
    ctx.restore();
    ctx.drawImage(
      image,
      (crop.x / frame.width) * image.width,
      (crop.y / frame.height) * image.height,
      (crop.width / frame.width) * image.width,
      (crop.height / frame.height) * image.height,
      mapped.x,
      mapped.y,
      mapped.width,
      mapped.height,
    );
    ctx.strokeStyle = "#2dd4bf";
    ctx.lineWidth = 2;
    ctx.strokeRect(mapped.x + 1, mapped.y + 1, mapped.width - 2, mapped.height - 2);
    badge.hidden = false;
    badge.textContent = `${crop.width} × ${crop.height}`;
    const rect = canvas.getBoundingClientRect();
    const cssX = (crop.x / frame.width) * rect.width;
    const cssY = (crop.y / frame.height) * rect.height;
    badge.style.left = `${Math.min(cssX + 8, rect.width - 88)}px`;
    badge.style.top = `${Math.max(cssY - 28, 12)}px`;
  };

  const scheduleDraw = (): void => {
    if (raf) {
      return;
    }
    raf = requestAnimationFrame(() => {
      raf = 0;
      draw();
    });
  };

  const load = async (): Promise<void> => {
    try {
      frame = await invoke<OverlayFrame>("get_overlay_frame");
      fitCanvas();
      root.classList.toggle("mode-window", frame.mode === "window");
      root.classList.toggle("mode-region", frame.mode !== "window");
      const reduced = frame.reducedCapabilities === true;
      capabilityToggle.hidden = !reduced;
      notice.hidden = true;
      setCapabilityPanel(false);
      if (reduced) {
        renderCapabilityPanel();
      }
      renderHint();
      void getCurrentWindow().setFocus();
      document.body.tabIndex = -1;
      document.body.focus();
      list.hidden = frame.mode !== "window";
      if (frame.mode === "window") {
        renderWindowList(list, frame.windows, hoverId, (id) => {
          void finishWindow(id);
        });
      }
      image = new Image();
      image.onload = () => scheduleDraw();
      image.src = `data:image/jpeg;base64,${frame.pngBase64}`;
    } catch (error) {
      // 无进行中的会话(cancelled)是预创建/隐藏时的正常路径,静默返回。
      if (isCancelledError(error)) {
        return;
      }
      hint.textContent = invokeError(error, t("overlay.error.capture_failed"));
    }
  };

  const finishRegion = async (): Promise<void> => {
    if (finishing || !frame || frame.mode !== "region") {
      return;
    }
    const crop = roundedRect();
    if (!crop || crop.width < 2 || crop.height < 2) {
      // 不静默吞掉确认:给出下次能成功的具体做法(R13)。
      if (!selection) {
        showNotice(t("overlay.notice.select_first"));
      } else if (selection.width >= 1 || selection.height >= 1) {
        showNotice(t("overlay.notice.too_small"));
      }
      return;
    }
    finishing = true;
    try {
      await invoke("confirm_region", {
        x: crop.x,
        y: crop.y,
        width: crop.width,
        height: crop.height,
      });
    } catch (error) {
      finishing = false;
      hint.textContent = invokeError(error, t("overlay.error.capture_failed"));
    }
  };

  const finishWindow = async (windowId: string): Promise<void> => {
    if (finishing) {
      return;
    }
    finishing = true;
    try {
      await invoke("confirm_window", { windowId });
    } catch (error) {
      finishing = false;
      hint.textContent = invokeError(error, t("overlay.error.capture_failed"));
    }
  };

  canvas.addEventListener("mousedown", (event) => {
    if (!frame || frame.mode !== "region") {
      return;
    }
    const point = physicalPoint(event);
    dragging = true;
    startX = point.x;
    startY = point.y;
    selection = { x: point.x, y: point.y, width: 0, height: 0 };
    notice.hidden = true;
    scheduleDraw();
  });

  window.addEventListener("mousemove", (event) => {
    if (!frame) {
      return;
    }
    const point = physicalPoint(event);
    if (frame.mode === "window") {
      const nextHover = hitWindow(frame, point.x, point.y);
      if (nextHover !== hoverId) {
        hoverId = nextHover;
        markActiveWindow(list, hoverId);
        scheduleDraw();
      }
      return;
    }
    if (!dragging) {
      return;
    }
    const x = Math.min(startX, point.x);
    const y = Math.min(startY, point.y);
    selection = {
      x,
      y,
      width: Math.abs(point.x - startX),
      height: Math.abs(point.y - startY),
    };
    scheduleDraw();
  });

  window.addEventListener("mouseup", () => {
    if (!dragging) {
      return;
    }
    dragging = false;
    void finishRegion();
  });

  canvas.addEventListener("click", (event) => {
    if (!frame || frame.mode !== "window") {
      return;
    }
    const point = physicalPoint(event);
    const id = hitWindow(frame, point.x, point.y);
    if (id) {
      void finishWindow(id);
    }
  });

  const cancel = (): void => {
    dragging = false;
    void invoke("cancel_capture");
  };

  // 窗口模式下右键=取消(Esc 失焦卡住时的兜底);区域模式右键=动作菜单,
  // Wayland 覆盖层没有该菜单,必须说明而不是静默无响应(R13)。
  canvas.addEventListener("contextmenu", (event) => {
    if (!frame) {
      return;
    }
    if (frame.mode === "window") {
      event.preventDefault();
      cancel();
      return;
    }
    if (frame.reducedCapabilities) {
      event.preventDefault();
      showNotice(t(TOOLBAR_NOTICE_KEY));
    }
  });

  capabilityToggle.addEventListener("click", (event) => {
    event.preventDefault();
    event.stopPropagation();
    setCapabilityPanel(capabilityPanel.hidden);
  });

  cancelBtn.addEventListener("click", (event) => {
    event.preventDefault();
    event.stopPropagation();
    cancel();
  });

  document.addEventListener(
    "keydown",
    (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        cancel();
      }
      if (event.key === "Enter") {
        event.preventDefault();
        void finishRegion();
      }
      if (!frame || !frame.reducedCapabilities || frame.mode !== "region") {
        return;
      }
      // 原生壳的可用快捷键在 Wayland 覆盖层缺失:触发时给出说明与替代。
      if (event.key === "c" || event.key === "C") {
        event.preventDefault();
        showNotice(t(COLOR_NOTICE_KEY));
        return;
      }
      if (event.key.startsWith("Arrow") && selection) {
        event.preventDefault();
        showNotice(t(NUDGE_NOTICE_KEY));
      }
    },
    true,
  );

  void listen("overlay-reload", () => {
    void load();
  });
  void load();

  // 语言切换:提示条、能力面板与开关文案即时更新;静态标签由 main 应用。
  return () => {
    renderHint();
    setCapabilityPanel(!capabilityPanel.hidden);
    if (!capabilityPanel.hidden) {
      renderCapabilityPanel();
    }
  };
}

function markActiveWindow(root: HTMLElement, activeId: string | null): void {
  root.querySelectorAll(".window-item").forEach((item) => {
    const button = item as HTMLElement;
    button.classList.toggle("active", button.dataset.windowId === activeId);
  });
}

function renderWindowList(
  root: HTMLElement,
  windows: ListedWindow[],
  activeId: string | null,
  onPick: (id: string) => void,
): void {
  root.replaceChildren();
  for (const item of windows) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "window-item";
    button.dataset.windowId = item.id;
    if (item.id === activeId) {
      button.classList.add("active");
    }
    button.innerHTML = `<div class="title"></div><div class="meta"></div>`;
    const title = button.querySelector(".title");
    const meta = button.querySelector(".meta");
    if (title) {
      title.textContent = item.title;
    }
    if (meta) {
      meta.textContent = `${item.width} × ${item.height}`;
    }
    button.addEventListener("click", (event) => {
      event.stopPropagation();
      onPick(item.id);
    });
    root.append(button);
  }
}

function windowRectOnFrame(window: ListedWindow, frame: OverlayFrame): Selection | null {
  const x = Math.max(0, window.x);
  const y = Math.max(0, window.y);
  const right = Math.min(frame.width, window.x + window.width);
  const bottom = Math.min(frame.height, window.y + window.height);
  if (right - x < 2 || bottom - y < 2) {
    return null;
  }
  return {
    x,
    y,
    width: right - x,
    height: bottom - y,
  };
}

function hitWindow(frame: OverlayFrame, x: number, y: number): string | null {
  // frame.windows 按 z 序自顶向下(平台层契约:Windows EnumWindows /
  // macOS CGWindowList 天然自顶向下,X11 _NET_CLIENT_LIST 已在后端反转)。
  // 首个命中即用户实际看到的最上层窗口;不可再 reverse,否则最底层
  // 窗口(常常是桌面)会吞掉所有点击。
  for (const window of frame.windows) {
    const rect = windowRectOnFrame(window, frame);
    if (!rect) {
      continue;
    }
    if (x >= rect.x && y >= rect.y && x <= rect.x + rect.width && y <= rect.y + rect.height) {
      return window.id;
    }
  }
  return null;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function isCancelledError(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    (error as { kind?: unknown }).kind === "cancelled"
  );
}

function invokeError(error: unknown, fallback: string): string {
  if (typeof error === "string") {
    return error;
  }
  if (error && typeof error === "object" && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return fallback;
}
