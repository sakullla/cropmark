import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
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
    <div class="overlay-chrome">
      <div class="overlay-hint"></div>
      <button type="button" class="overlay-cancel">取消 Esc</button>
    </div>
    <div class="size-badge" hidden></div>
    <div class="window-list" hidden></div>
  `;
  const canvas = root.querySelector("canvas");
  const hint = root.querySelector(".overlay-hint");
  const badge = root.querySelector(".size-badge");
  const list = root.querySelector(".window-list");
  const cancelBtn = root.querySelector(".overlay-cancel");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(hint instanceof HTMLElement) ||
    !(badge instanceof HTMLElement) ||
    !(list instanceof HTMLElement) ||
    !(cancelBtn instanceof HTMLButtonElement)
  ) {
    return;
  }

  const ctx = canvas.getContext("2d", { alpha: false });
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
  let raf = 0;

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
    if (!selection || selection.width < 1 || selection.height < 1) {
      badge.hidden = true;
      return;
    }
    const mapped = toCanvas(selection.x, selection.y, selection.width, selection.height);
    ctx.save();
    ctx.globalCompositeOperation = "destination-out";
    ctx.fillRect(mapped.x, mapped.y, mapped.width, mapped.height);
    ctx.restore();
    ctx.drawImage(
      image,
      (selection.x / frame.width) * image.width,
      (selection.y / frame.height) * image.height,
      (selection.width / frame.width) * image.width,
      (selection.height / frame.height) * image.height,
      mapped.x,
      mapped.y,
      mapped.width,
      mapped.height,
    );
    ctx.strokeStyle = "#2dd4bf";
    ctx.lineWidth = 2;
    ctx.strokeRect(mapped.x + 1, mapped.y + 1, mapped.width - 2, mapped.height - 2);
    badge.hidden = false;
    badge.textContent = `${Math.round(selection.width)} × ${Math.round(selection.height)}`;
    const rect = canvas.getBoundingClientRect();
    const cssX = (selection.x / frame.width) * rect.width;
    const cssY = (selection.y / frame.height) * rect.height;
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
      hint.textContent =
        frame.mode === "window" ? "点击要截取的窗口" : "拖出矩形截取区域";
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
      const message = invokeError(error);
      if (message.includes("没有正在进行")) {
        return;
      }
      hint.textContent = message;
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

  // 窗口模式下右键=取消(Esc 失焦卡住时的兜底);区域模式右键留给动作菜单。
  canvas.addEventListener("contextmenu", (event) => {
    if (!frame || frame.mode !== "window") {
      return;
    }
    event.preventDefault();
    cancel();
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
    },
    true,
  );

  void listen("overlay-reload", () => {
    void load();
  });
  void load();
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

function invokeError(error: unknown): string {
  if (typeof error === "string") {
    return error;
  }
  if (error && typeof error === "object" && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return "截取失败。";
}
