import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalPosition, LogicalSize } from "@tauri-apps/api/window";
import "./pin.css";

const MIN_ZOOM = 0.2;
const MAX_ZOOM = 5;

// 贴图视图:Rust 建好窗口后前端拉取 PNG 独占持有;滚轮以光标为中心
// 缩放(调整窗口尺寸+图像铺满,显示比例与原始分辨率解耦),整窗拖移,
// 悬停显示关闭按钮,Esc 关闭,右键菜单提供关闭/缩放重置。
export function mountPin(root: HTMLElement): void {
  root.className = "pin-root";
  root.innerHTML = `
    <div class="pin-stage" data-tauri-drag-region>
      <img class="pin-image" alt="" draggable="false">
    </div>
    <button type="button" class="pin-close" data-action="close" aria-label="关闭贴图"></button>
    <div class="pin-menu" data-menu hidden>
      <button type="button" data-menu-action="reset">缩放重置</button>
      <button type="button" data-menu-action="close">关闭贴图</button>
    </div>
  `;

  const stage = root.querySelector(".pin-stage");
  const image = root.querySelector(".pin-image");
  const menu = root.querySelector("[data-menu]");
  if (
    !(stage instanceof HTMLElement) ||
    !(image instanceof HTMLImageElement) ||
    !(menu instanceof HTMLElement)
  ) {
    return;
  }

  const win = getCurrentWindow();
  // 缩放基准:窗口创建时的逻辑尺寸(= 图像逻辑尺寸,zoom 1x)。
  let base = { w: 0, h: 0 };
  let zoom = 1;

  const clampZoom = (value: number): number => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, value));

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

  const hideMenu = (): void => {
    menu.hidden = true;
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
    const button = event.target instanceof Element ? event.target.closest("button") : null;
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    if (button.dataset.action === "close") {
      close();
      return;
    }
    const action = button.dataset.menuAction;
    if (action === "close") {
      hideMenu();
      close();
    } else if (action === "reset") {
      hideMenu();
      void resetZoom();
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

  base = { w: Math.max(1, window.innerWidth), h: Math.max(1, window.innerHeight) };
  const imageUrl = (url: string): void => {
    image.src = url;
  };
  void invoke<ArrayBuffer>("get_pin_image", { label: win.label })
    .then((bytes) => {
      if (bytes.byteLength === 0) {
        throw new Error("empty");
      }
      const url = URL.createObjectURL(new Blob([bytes], { type: "image/png" }));
      image.onload = () => URL.revokeObjectURL(url);
      imageUrl(url);
    })
    .catch(() => {
      root.classList.add("is-broken");
    });
}
