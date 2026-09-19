import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalPosition, LogicalSize } from "@tauri-apps/api/window";
import "./pin.css";

const MIN_ZOOM = 0.2;
const MAX_ZOOM = 5;
// 透明度档位以 1 → 0.75 → 0.5 → 0.25 循环;与 Rust 侧 alpha 乘算一致。
const OPACITY_STEPS = [1, 0.75, 0.5, 0.25];

const ICONS = {
  copy: `<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.4" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M10.5 5.5v-2a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2" fill="none" stroke="currentColor" stroke-width="1.5"/></svg>`,
  save: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 2.5h7L13 5v8.5h-9.5z" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/><path d="M5.5 2.5v3.5h5V2.5M5.5 10h5" fill="none" stroke="currentColor" stroke-width="1.5"/></svg>`,
  rotate: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M12.5 8a4.5 4.5 0 1 1-1.4-3.25" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="M12.8 1.8v3.4H9.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  annotate: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2.8 13.2 3.6 10l7-7a1.2 1.2 0 0 1 1.7 0l.7.7a1.2 1.2 0 0 1 0 1.7l-7 7z" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/><path d="M9.6 4.2l2.2 2.2" fill="none" stroke="currentColor" stroke-width="1.5"/></svg>`,
};

// 贴图视图:Rust 建好窗口并把源图留在 PinStore 后,前端首载拉取一次;
// 滚轮以光标为中心缩放(调整窗口尺寸+画布铺满,显示比例与原始分辨率
// 解耦),整窗拖移;悬停工具条/右键菜单提供复制、保存、旋转 90°、
// 透明度、再标注与关闭。旋转与透明度是窗口局部状态,关闭重开恢复默认。
export function mountPin(root: HTMLElement): void {
  root.className = "pin-root";
  root.innerHTML = `
    <div class="pin-stage" data-tauri-drag-region>
      <canvas class="pin-canvas"></canvas>
    </div>
    <div class="pin-toolbar">
      <button type="button" data-action="copy" title="复制图片（无损 PNG）" aria-label="复制">${ICONS.copy}</button>
      <button type="button" data-action="save" title="保存 PNG" aria-label="保存">${ICONS.save}</button>
      <button type="button" data-action="rotate" title="顺时针旋转 90°" aria-label="旋转 90°">${ICONS.rotate}</button>
      <button type="button" data-action="opacity" class="pin-opacity" title="调整透明度" aria-label="透明度">100%</button>
      <button type="button" data-action="annotate" title="再标注（确认后更新贴图）" aria-label="再标注">${ICONS.annotate}</button>
      <button type="button" data-action="close" class="pin-close" title="关闭贴图" aria-label="关闭贴图">×</button>
    </div>
    <div class="pin-menu" data-menu hidden>
      <button type="button" data-menu-action="copy">复制图片</button>
      <button type="button" data-menu-action="save">保存 PNG…</button>
      <button type="button" data-menu-action="rotate">顺时针旋转 90°</button>
      <div class="pin-menu-row">
        <span>透明度</span>
        <div class="pin-menu-opacity">
          ${OPACITY_STEPS.map(
            (value) =>
              `<button type="button" data-menu-opacity="${value}" title="透明度 ${Math.round(value * 100)}%">${Math.round(value * 100)}%</button>`,
          ).join("")}
        </div>
      </div>
      <button type="button" data-menu-action="annotate">再标注…</button>
      <button type="button" data-menu-action="reset">缩放重置</button>
      <button type="button" data-menu-action="close">关闭贴图</button>
    </div>
    <div class="pin-note" data-note hidden></div>
  `;

  const stage = root.querySelector(".pin-stage");
  const canvas = root.querySelector(".pin-canvas");
  const menu = root.querySelector("[data-menu]");
  const note = root.querySelector("[data-note]");
  const opacityBtn = root.querySelector("[data-action=opacity]");
  if (
    !(stage instanceof HTMLElement) ||
    !(canvas instanceof HTMLCanvasElement) ||
    !(menu instanceof HTMLElement) ||
    !(note instanceof HTMLElement) ||
    !(opacityBtn instanceof HTMLButtonElement)
  ) {
    return;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return;
  }

  const win = getCurrentWindow();
  // 缩放基准:当前旋转取向下 1x 的窗口逻辑尺寸(= 源图逻辑尺寸,
  // 旋转 90°/270° 时宽高互换);窗口尺寸 = base * zoom。
  let base = { w: 0, h: 0 };
  let zoom = 1;
  let rotation = 0;
  let opacity = 1;
  let image: HTMLImageElement | null = null;
  let busy = false;
  let noteTimer = 0;

  const clampZoom = (value: number): number => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, value));

  const showNote = (text: string, isError = false): void => {
    note.textContent = text;
    note.classList.toggle("is-error", isError);
    note.hidden = false;
    window.clearTimeout(noteTimer);
    noteTimer = window.setTimeout(() => {
      note.hidden = true;
    }, 2200);
  };

  const syncUi = (): void => {
    opacityBtn.textContent = `${Math.round(opacity * 100)}%`;
    menu.querySelectorAll<HTMLButtonElement>("[data-menu-opacity]").forEach((button) => {
      button.classList.toggle("active", Number(button.dataset.menuOpacity) === opacity);
    });
  };

  // 把源图按当前旋转角画进画布:内部像素尺寸随旋转换向,CSS 拉伸铺满
  // 窗口(窗口按同一取向设置尺寸,不产生变形)。
  const render = (): void => {
    if (!image) {
      return;
    }
    const turns = ((rotation / 90) % 4 + 4) % 4;
    const sourceW = Math.max(1, image.naturalWidth);
    const sourceH = Math.max(1, image.naturalHeight);
    const swapped = turns % 2 === 1;
    canvas.width = swapped ? sourceH : sourceW;
    canvas.height = swapped ? sourceW : sourceH;
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.save();
    ctx.globalAlpha = opacity;
    if (turns === 1) {
      ctx.translate(canvas.width, 0);
      ctx.rotate(Math.PI / 2);
    } else if (turns === 2) {
      ctx.translate(canvas.width, canvas.height);
      ctx.rotate(Math.PI);
    } else if (turns === 3) {
      ctx.translate(0, canvas.height);
      ctx.rotate(-Math.PI / 2);
    }
    ctx.drawImage(image, 0, 0);
    ctx.restore();
  };

  const close = (): void => {
    void invoke("close_pin", { label: win.label }).catch(() => win.close());
  };

  // 以 (anchorX, anchorY)(窗口内 CSS 坐标)为不动点缩放:图像点
  // u = client / zoom 缩放后仍落在同一屏幕位置 → 窗口原点平移
  // origin + client * (1 - next/zoom)。窗口尺寸 = base * zoom。
  const setZoom = async (next: number, anchorX?: number, anchorY?: number): Promise<void> => {
    next = clampZoom(next);
    if (next === zoom || base.w <= 0 || base.h <= 0) {
      return;
    }
    const factor = await win.scaleFactor();
    const position = await win.innerPosition();
    const originX = position.x / factor;
    const originY = position.y / factor;
    const ax = anchorX ?? window.innerWidth / 2;
    const ay = anchorY ?? window.innerHeight / 2;
    const ratio = next / zoom;
    const x = originX + ax * (1 - ratio);
    const y = originY + ay * (1 - ratio);
    zoom = next;
    await win.setPosition(new LogicalPosition(x, y));
    await win.setSize(new LogicalSize(base.w * zoom, base.h * zoom));
  };

  const resetZoom = async (): Promise<void> => {
    if (zoom === 1 || base.w <= 0 || base.h <= 0) {
      return;
    }
    const factor = await win.scaleFactor();
    const position = await win.innerPosition();
    zoom = 1;
    await win.setPosition(new LogicalPosition(position.x / factor, position.y / factor));
    await win.setSize(new LogicalSize(base.w, base.h));
  };

  // 顺时针旋转 90°:基准换向、窗口以中心为不动点换向,重绘即时可见。
  const rotate = async (): Promise<void> => {
    if (!image || busy) {
      return;
    }
    busy = true;
    try {
      const factor = await win.scaleFactor();
      const position = await win.innerPosition();
      const oldW = base.w * zoom;
      const oldH = base.h * zoom;
      rotation = (rotation + 90) % 360;
      base = { w: base.h, h: base.w };
      const nextW = base.w * zoom;
      const nextH = base.h * zoom;
      await win.setPosition(
        new LogicalPosition(
          position.x / factor + (oldW - nextW) / 2,
          position.y / factor + (oldH - nextH) / 2,
        ),
      );
      await win.setSize(new LogicalSize(nextW, nextH));
      render();
      syncUi();
    } finally {
      busy = false;
    }
  };

  const setOpacity = (next: number): void => {
    if (!OPACITY_STEPS.includes(next)) {
      return;
    }
    opacity = next;
    render();
    syncUi();
  };

  const cycleOpacity = (): void => {
    const index = OPACITY_STEPS.indexOf(opacity);
    setOpacity(OPACITY_STEPS[(index + 1) % OPACITY_STEPS.length]);
  };

  const copy = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      await invoke("copy_pin", { label: win.label, rotation, opacity });
      showNote("已复制当前内容（无损 PNG）。");
    } catch (error) {
      showNote(invokeError(error, "无法复制贴图。"), true);
    } finally {
      busy = false;
    }
  };

  const save = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      const result = await invoke<{ saved: boolean; path?: string | null }>("save_pin", {
        label: win.label,
        rotation,
        opacity,
      });
      if (result?.saved) {
        showNote(`已保存 ${fileNameFromPath(result.path) ?? "cropmark-pin.png"}。`);
      }
    } catch (error) {
      showNote(invokeError(error, "无法保存贴图。"), true);
    } finally {
      busy = false;
    }
  };

  // 再标注:当前显示内容载入预览编辑器;确认后贴图更新,取消不改动。
  const annotate = async (): Promise<void> => {
    if (busy || !image) {
      return;
    }
    busy = true;
    try {
      await invoke("begin_pin_edit", { label: win.label, rotation, opacity });
    } catch (error) {
      showNote(invokeError(error, "无法进入再标注。"), true);
    } finally {
      busy = false;
    }
  };

  const hideMenu = (): void => {
    menu.hidden = true;
  };

  const runAction = (action: string): void => {
    if (action === "copy") {
      void copy();
    } else if (action === "save") {
      void save();
    } else if (action === "rotate") {
      void rotate();
    } else if (action === "opacity") {
      cycleOpacity();
    } else if (action === "annotate") {
      void annotate();
    } else if (action === "reset") {
      void resetZoom();
    } else if (action === "close") {
      close();
    }
  };

  stage.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    const rect = root.getBoundingClientRect();
    menu.hidden = false;
    const menuRect = menu.getBoundingClientRect();
    menu.style.left = `${Math.max(4, Math.min(event.clientX - rect.left, rect.width - menuRect.width - 4))}px`;
    menu.style.top = `${Math.max(4, Math.min(event.clientY - rect.top, rect.height - menuRect.height - 4))}px`;
  });

  document.addEventListener("click", (event) => {
    if (!menu.hidden && !(event.target instanceof Node && menu.contains(event.target))) {
      hideMenu();
    }
  });

  root.addEventListener("click", (event) => {
    const button =
      event.target instanceof Element ? event.target.closest("button") : null;
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    const menuOpacity = button.dataset.menuOpacity;
    if (menuOpacity !== undefined) {
      setOpacity(Number(menuOpacity));
      hideMenu();
      return;
    }
    const menuAction = button.dataset.menuAction;
    if (menuAction) {
      hideMenu();
      runAction(menuAction);
      return;
    }
    const action = button.dataset.action;
    if (action) {
      runAction(action);
    }
  });

  stage.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      // 每格滚轮 ±10%,以光标为不动点。
      void setZoom(zoom * (event.deltaY < 0 ? 1.1 : 1 / 1.1), event.clientX, event.clientY);
    },
    { passive: false },
  );

  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      if (!menu.hidden) {
        hideMenu();
        return;
      }
      close();
    }
  });

  const loadImage = async (): Promise<boolean> => {
    try {
      const bytes = await invoke<ArrayBuffer>("get_pin_image", { label: win.label });
      if (!bytes || bytes.byteLength === 0) {
        return false;
      }
      const url = URL.createObjectURL(new Blob([bytes], { type: "image/png" }));
      try {
        const loaded = await new Promise<HTMLImageElement>((resolve, reject) => {
          const next = new Image();
          next.onload = () => resolve(next);
          next.onerror = () => reject(new Error("decode"));
          next.src = url;
        });
        image = loaded;
        return true;
      } finally {
        URL.revokeObjectURL(url);
      }
    } catch {
      return false;
    }
  };

  // 再标注确认后 Rust 已把新内容写入源图并广播 pin-reload:
  // 重新拉图,旋转/透明度已被烘焙进内容,窗口局部状态归零。
  const reload = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      const ok = await loadImage();
      if (!ok) {
        showNote("贴图更新失败，请重新贴图。", true);
        return;
      }
      rotation = 0;
      opacity = 1;
      render();
      syncUi();
      showNote("贴图已更新。");
    } finally {
      busy = false;
    }
  };

  void listen("pin-reload", () => {
    void reload();
  });

  base = { w: Math.max(1, window.innerWidth), h: Math.max(1, window.innerHeight) };
  void (async () => {
    if (!(await loadImage())) {
      root.classList.add("is-broken");
      return;
    }
    render();
    syncUi();
  })();
}

function fileNameFromPath(path: string | null | undefined): string | null {
  if (!path) {
    return null;
  }
  const parts = path.split(/[\\/]/);
  const name = parts[parts.length - 1];
  return name.length > 0 ? name : null;
}

function invokeError(error: unknown, fallback: string): string {
  if (typeof error === "string" && error.trim()) {
    return error;
  }
  if (error && typeof error === "object" && "message" in error) {
    const message = String((error as { message: unknown }).message);
    if (message.trim()) {
      return message;
    }
  }
  return fallback;
}
