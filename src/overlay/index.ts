import { invoke } from "@tauri-apps/api/core";
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

interface OverlayFrame {
  mode: CaptureMode;
  pngBase64: string;
  width: number;
  height: number;
  scale: number;
  logicalWidth: number;
  logicalHeight: number;
  windows: ListedWindow[];
}

interface Selection {
  x: number;
  y: number;
  width: number;
  height: number;
}

export function mountOverlay(root: HTMLElement): void {
  root.className = "overlay-root";
  root.innerHTML = `
    <canvas></canvas>
    <div class="overlay-hint"></div>
    <div class="size-badge" hidden></div>
    <div class="window-list" hidden></div>
  `;
  const canvas = root.querySelector("canvas");
  const hint = root.querySelector(".overlay-hint");
  const badge = root.querySelector(".size-badge");
  const list = root.querySelector(".window-list");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(hint instanceof HTMLElement) ||
    !(badge instanceof HTMLElement) ||
    !(list instanceof HTMLElement)
  ) {
    return;
  }

  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return;
  }

  let frame: OverlayFrame | null = null;
  let image: HTMLImageElement | null = null;
  let dragging = false;
  let startX = 0;
  let startY = 0;
  let selection: Selection | null = null;
  let hoverId: string | null = null;
  let finishing = false;

  const physicalPoint = (event: MouseEvent): { x: number; y: number } => {
    const rect = canvas.getBoundingClientRect();
    const scaleX = canvas.width / Math.max(rect.width, 1);
    const scaleY = canvas.height / Math.max(rect.height, 1);
    return {
      x: clamp((event.clientX - rect.left) * scaleX, 0, canvas.width),
      y: clamp((event.clientY - rect.top) * scaleY, 0, canvas.height),
    };
  };

  const draw = (): void => {
    if (!image || !frame) {
      return;
    }
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(image, 0, 0, canvas.width, canvas.height);
    ctx.fillStyle = "rgba(12, 10, 9, 0.48)";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    if (frame.mode === "window") {
      for (const window of frame.windows) {
        const rect = windowRectOnFrame(window, frame);
        if (!rect) {
          continue;
        }
        const active = hoverId === window.id;
        ctx.save();
        ctx.globalCompositeOperation = "destination-out";
        ctx.fillStyle = active ? "rgba(0,0,0,0.85)" : "rgba(0,0,0,0.35)";
        ctx.fillRect(rect.x, rect.y, rect.width, rect.height);
        ctx.restore();
        ctx.strokeStyle = active ? "#2dd4bf" : "rgba(245, 240, 232, 0.55)";
        ctx.lineWidth = active ? 3 : 1.5;
        ctx.strokeRect(rect.x + 0.5, rect.y + 0.5, rect.width - 1, rect.height - 1);
      }
      return;
    }
    if (!selection || selection.width < 1 || selection.height < 1) {
      return;
    }
    ctx.save();
    ctx.globalCompositeOperation = "destination-out";
    ctx.fillRect(selection.x, selection.y, selection.width, selection.height);
    ctx.restore();
    ctx.drawImage(
      image,
      selection.x,
      selection.y,
      selection.width,
      selection.height,
      selection.x,
      selection.y,
      selection.width,
      selection.height,
    );
    ctx.strokeStyle = "#2dd4bf";
    ctx.lineWidth = 2;
    ctx.strokeRect(selection.x + 1, selection.y + 1, selection.width - 2, selection.height - 2);
    badge.hidden = false;
    badge.textContent = `${Math.round(selection.width)} × ${Math.round(selection.height)}`;
    const rect = canvas.getBoundingClientRect();
    const cssX = (selection.x / canvas.width) * rect.width;
    const cssY = (selection.y / canvas.height) * rect.height;
    badge.style.left = `${Math.min(cssX + 8, rect.width - 88)}px`;
    badge.style.top = `${Math.max(cssY - 28, 12)}px`;
  };

  const load = async (): Promise<void> => {
    try {
      frame = await invoke<OverlayFrame>("get_overlay_frame");
      canvas.width = frame.width;
      canvas.height = frame.height;
      hint.textContent =
        frame.mode === "window" ? "点击窗口 · Esc 取消" : "拖选区域 · Esc 取消";
      list.hidden = frame.mode !== "window";
      if (frame.mode === "window") {
        renderWindowList(list, frame.windows, hoverId, (id) => {
          void finishWindow(id);
        });
      }
      image = new Image();
      image.onload = () => draw();
      image.src = `data:image/png;base64,${frame.pngBase64}`;
    } catch (error) {
      hint.textContent = invokeError(error);
    }
  };

  const finishRegion = async (): Promise<void> => {
    if (!selection || finishing || selection.width < 2 || selection.height < 2) {
      return;
    }
    finishing = true;
    try {
      await invoke("confirm_region", {
        x: Math.round(selection.x),
        y: Math.round(selection.y),
        width: Math.round(selection.width),
        height: Math.round(selection.height),
      });
    } catch (error) {
      finishing = false;
      hint.textContent = invokeError(error);
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
      hint.textContent = invokeError(error);
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
    draw();
  });

  window.addEventListener("mousemove", (event) => {
    if (!frame) {
      return;
    }
    const point = physicalPoint(event);
    if (frame.mode === "window") {
      hoverId = hitWindow(frame, point.x, point.y);
      renderWindowList(list, frame.windows, hoverId, (id) => {
        void finishWindow(id);
      });
      draw();
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
    draw();
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

  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      void invoke("cancel_capture");
    }
    if (event.key === "Enter") {
      event.preventDefault();
      void finishRegion();
    }
  });

  void load();
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
  for (const window of [...frame.windows].reverse()) {
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

function invokeError(error: unknown): string {
  if (typeof error === "string") {
    return error;
  }
  if (error && typeof error === "object" && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return "截取失败。";
}
