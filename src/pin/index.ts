import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalPosition, LogicalSize } from "@tauri-apps/api/window";
import { t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import "./pin.css";

const MIN_ZOOM = 0.2;
const MAX_ZOOM = 5;
// 透明度档位以 1 → 0.75 → 0.5 → 0.25 循环;与 Rust 侧 alpha 乘算一致。
const OPACITY_STEPS = [1, 0.75, 0.5, 0.25];

const ICONS = {
  copy: icons.copy,
  save: icons.save,
  rotate: icons.rotate,
  annotate: icons.annotate,
  close: icons.close,
};

// 贴图视图:Rust 建好窗口并把源图留在 PinStore 后,前端首载拉取一次;
// 滚轮以光标为中心缩放(调整窗口尺寸+画布铺满,显示比例与原始分辨率
// 解耦),整窗拖移;悬停工具条/右键菜单提供复制、保存、旋转 90°、
// 透明度、再标注与关闭。旋转与透明度是窗口局部状态,关闭重开恢复默认。
export function mountPin(root: HTMLElement): () => void {
  root.className = "pin-root";
  root.innerHTML = `
    <div class="pin-stage" data-tauri-drag-region>
      <canvas class="pin-canvas"></canvas>
    </div>
    <div class="pin-toolbar">
      <button type="button" data-action="copy" data-i18n-title="pin.toolbar.copy_title" data-i18n-aria-label="pin.toolbar.copy" title="复制图片（无损 PNG）" aria-label="复制">${ICONS.copy}</button>
      <button type="button" data-action="save" data-i18n-title="pin.toolbar.save_title" data-i18n-aria-label="pin.toolbar.save" title="保存 PNG" aria-label="保存">${ICONS.save}</button>
      <button type="button" data-action="rotate" data-i18n-title="pin.toolbar.rotate_title" data-i18n-aria-label="pin.toolbar.rotate" title="顺时针旋转 90°" aria-label="旋转 90°">${ICONS.rotate}</button>
      <button type="button" data-action="opacity" class="pin-opacity" data-i18n-title="pin.toolbar.opacity_title" data-i18n-aria-label="pin.toolbar.opacity" title="调整透明度" aria-label="透明度">100%</button>
      <button type="button" data-action="annotate" data-i18n-title="pin.toolbar.annotate_title" data-i18n-aria-label="pin.toolbar.annotate" title="再标注（确认后更新贴图）" aria-label="再标注">${ICONS.annotate}</button>
      <button type="button" data-action="close" class="pin-close" data-i18n-title="pin.toolbar.close_title" data-i18n-aria-label="pin.toolbar.close" title="关闭贴图" aria-label="关闭贴图">${ICONS.close}</button>
    </div>
    <div class="pin-menu" data-menu hidden>
      <button type="button" data-menu-action="copy" data-i18n="pin.menu.copy">复制图片</button>
      <button type="button" data-menu-action="save" data-i18n="pin.menu.save">保存 PNG…</button>
      <button type="button" data-menu-action="rotate" data-i18n="pin.menu.rotate">顺时针旋转 90°</button>
      <div class="pin-menu-row">
        <span data-i18n="pin.menu.opacity">透明度</span>
        <div class="pin-menu-opacity">
          ${OPACITY_STEPS.map(
            (value) =>
              `<button type="button" data-menu-opacity="${value}" data-i18n-title="pin.menu.opacity_option_title" data-i18n-title-params='{"percent":${Math.round(value * 100)}}' title="透明度 ${Math.round(value * 100)}%">${Math.round(value * 100)}%</button>`,
          ).join("")}
        </div>
      </div>
      <button type="button" data-menu-action="annotate" data-i18n="pin.menu.annotate">再标注…</button>
      <button type="button" data-menu-action="reset" data-i18n="pin.menu.reset">缩放重置</button>
      <button type="button" data-menu-action="close" data-i18n="pin.menu.close">关闭贴图</button>
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
    return () => undefined;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return () => undefined;
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
  let noteState: {
    key: CatalogKey | null;
    params?: Record<string, string | number>;
    text: string;
    isError: boolean;
  } | null = null;

  const clampZoom = (value: number): number => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, value));

  const noteText = (): string =>
    noteState
      ? noteState.key
        ? t(noteState.key, noteState.params)
        : noteState.text
      : "";

  const renderNote = (): void => {
    if (!noteState || note.hidden) {
      return;
    }
    note.textContent = noteText();
    note.classList.toggle("is-error", noteState.isError);
  };

  const showNoteSource = (state: NonNullable<typeof noteState>): void => {
    noteState = state;
    note.textContent = noteText();
    note.classList.toggle("is-error", state.isError);
    note.hidden = false;
    window.clearTimeout(noteTimer);
    noteTimer = window.setTimeout(() => {
      note.hidden = true;
    }, 2200);
  };

  const showNote = (text: string, isError = false): void => {
    showNoteSource({ key: null, text, isError });
  };

  const showNoteKey = (
    key: CatalogKey,
    params?: Record<string, string | number>,
    isError = false,
  ): void => {
    showNoteSource({ key, params, text: "", isError });
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
    void invoke("close_pin", { label: win.label });
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
      showNoteKey("pin.note.copied");
    } catch (error) {
      showNote(invokeError(error, t("pin.error.copy")), true);
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
        showNoteKey("pin.note.saved", {
          name: fileNameFromPath(result.path) ?? "cropmark-pin.png",
        });
      }
    } catch (error) {
      showNote(invokeError(error, t("pin.error.save")), true);
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
      showNote(invokeError(error, t("pin.error.annotate")), true);
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
    menu.style.maxHeight = "";
    menu.style.overflowY = "";
    menu.hidden = false;
    const limitW = Math.max(8, root.clientWidth - 8);
    const limitH = Math.max(8, root.clientHeight - 8);
    menu.style.maxWidth = `${limitW}px`;
    if (menu.offsetHeight > limitH) {
      menu.style.maxHeight = `${limitH}px`;
      menu.style.overflowY = "auto";
    }
    const mw = menu.offsetWidth;
    const mh = menu.offsetHeight;
    const x = Math.min(Math.max(4, event.clientX), Math.max(4, root.clientWidth - mw - 4));
    const y = Math.min(Math.max(4, event.clientY), Math.max(4, root.clientHeight - mh - 4));
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;
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

  // 再标注确认或复用池窗口重新展示时 Rust 广播 pin-reload:
  // 重新拉图,旋转/透明度/缩放置零。首次从空闲池打开不提示「已更新」。
  const reload = async (): Promise<void> => {
    if (busy) {
      return;
    }
    const hadImage = image !== null;
    busy = true;
    image = null;
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    try {
      const ok = await loadImage();
      if (!ok) {
        if (hadImage) {
          showNoteKey("pin.note.reload_failed", undefined, true);
        }
        return;
      }
      rotation = 0;
      opacity = 1;
      zoom = 1;
      base = { w: Math.max(1, window.innerWidth), h: Math.max(1, window.innerHeight) };
      root.classList.remove("is-broken");
      render();
      syncUi();
      if (hadImage) {
        showNoteKey("pin.note.updated");
      }
    } finally {
      busy = false;
    }
  };

  void listen("pin-reload", () => {
    void reload();
  });

  base = { w: Math.max(1, window.innerWidth), h: Math.max(1, window.innerHeight) };
  void (async () => {
    if (await loadImage()) {
      render();
      syncUi();
    }
  })();

  // 语言切换:重渲染进行中的提示文案;静态标签由 main 应用。
  return () => {
    renderNote();
  };
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
