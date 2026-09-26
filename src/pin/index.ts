import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import "./pin.css";

// 透明度档位以 1 → 0.75 → 0.5 → 0.25 循环;与 Rust 侧 alpha 乘算一致。
const OPACITY_STEPS = [1, 0.75, 0.5, 0.25];

// 结果提示的自动隐藏时长(ADR-2:只有结果提示自动消失,进行中型提示不用它)。
const NOTE_AUTO_HIDE_MS = 2200;

const ICONS = {
  copy: icons.copy,
  save: icons.save,
  rotate: icons.rotate,
  annotate: icons.annotate,
  close: icons.close,
};

// R2 贴图增强:旋转/翻转/透明度/几何/编组/穿透状态都以后端为准,前端只做
// 显示同步——变换后经 `pin-reload` 重拉「已应用变换」的图像,因此画面、复制
// 与保存天然一致;拖动与滚轮只上报几何,组内联动由后端应用到所有成员。
interface PinState {
  label: string;
  rotation: number;
  flipH: boolean;
  flipV: boolean;
  opacity: number;
  grouped: boolean;
  groupSize: number;
  clickThrough: boolean;
  // R8:文本贴图复制回原始文本,其余复制图像;文案与提示随之切换。
  copyKind: "image" | "text";
  logicalWidth: number;
  logicalHeight: number;
  windowWidth: number;
  windowHeight: number;
}

interface PinOptions {
  enhance: boolean;
  restore: boolean;
  clickThroughSupported: boolean;
  clickThroughReason: string | null;
  trayAvailable: boolean;
}

// 贴图视图:Rust 建好窗口并把源图与状态留在 STORE 后,前端首载拉取一次;
// 滚轮以光标为中心缩放(后端调整整组窗口尺寸),整窗拖移并同步组内成员;
// 悬停工具条/右键菜单提供复制、保存、旋转、翻转、透明度、穿透、编组、
// 再标注与关闭。旋转与透明度等状态在后端持久化,重启由设置决定是否恢复。
export function mountPin(root: HTMLElement): () => void {
  root.className = "pin-root";
  root.innerHTML = `
    <div class="pin-stage" data-tauri-drag-region tabindex="0" data-i18n-aria-label="pin.stage_label" aria-label="贴图">
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
    <div class="pin-menu" data-menu data-i18n-aria-label="pin.menu_label" aria-label="贴图菜单" hidden>
      <button type="button" data-menu-action="copy" data-i18n="pin.menu.copy">复制图片</button>
      <button type="button" data-menu-action="save" data-i18n="pin.menu.save">保存 PNG…</button>
      <button type="button" data-menu-action="rotate" data-i18n="pin.menu.rotate">顺时针旋转 90°</button>
      <button type="button" data-menu-action="flip-h" data-enhance data-i18n="pin.menu.flip_h">水平翻转</button>
      <button type="button" data-menu-action="flip-v" data-enhance data-i18n="pin.menu.flip_v">垂直翻转</button>
      <div class="pin-menu-row">
        <span data-i18n="pin.menu.opacity">透明度</span>
        <div class="pin-menu-opacity">
          ${OPACITY_STEPS.map(
            (value) =>
              `<button type="button" data-menu-opacity="${value}" data-i18n-title="pin.menu.opacity_option_title" data-i18n-title-params='{"percent":${Math.round(value * 100)}}' title="透明度 ${Math.round(value * 100)}%">${Math.round(value * 100)}%</button>`,
          ).join("")}
        </div>
      </div>
      <button type="button" data-menu-action="click-through" data-enhance data-i18n="pin.menu.click_through">开启点击穿透</button>
      <button type="button" data-menu-action="group" data-enhance data-i18n="pin.menu.group">编组全部贴图</button>
      <button type="button" data-menu-action="ungroup" data-enhance data-i18n="pin.menu.ungroup">从编组中解组</button>
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
  const closeToolbarBtn = root.querySelector("[data-action=close]");
  const copyToolbarBtn = root.querySelector("[data-action=copy]");
  const copyMenuBtn = root.querySelector("[data-menu-action=copy]");
  const clickThroughBtn = root.querySelector("[data-menu-action=click-through]");
  const groupBtn = root.querySelector("[data-menu-action=group]");
  const ungroupBtn = root.querySelector("[data-menu-action=ungroup]");
  const closeMenuBtn = root.querySelector("[data-menu-action=close]");
  if (
    !(stage instanceof HTMLElement) ||
    !(canvas instanceof HTMLCanvasElement) ||
    !(menu instanceof HTMLElement) ||
    !(note instanceof HTMLElement) ||
    !(opacityBtn instanceof HTMLButtonElement) ||
    !(closeToolbarBtn instanceof HTMLButtonElement) ||
    !(copyToolbarBtn instanceof HTMLButtonElement) ||
    !(copyMenuBtn instanceof HTMLButtonElement) ||
    !(clickThroughBtn instanceof HTMLButtonElement) ||
    !(groupBtn instanceof HTMLButtonElement) ||
    !(ungroupBtn instanceof HTMLButtonElement) ||
    !(closeMenuBtn instanceof HTMLButtonElement)
  ) {
    return () => undefined;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return () => undefined;
  }

  const win = getCurrentWindow();
  let state: PinState | null = null;
  let options: PinOptions = {
    enhance: false,
    restore: false,
    clickThroughSupported: true,
    clickThroughReason: null,
    trayAvailable: true,
  };
  let image: HTMLImageElement | null = null;
  let busy = false;
  let noteTimer = 0;
  let noteState: {
    key: CatalogKey | null;
    params?: Record<string, string | number>;
    text: string;
    isError: boolean;
  } | null = null;

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

  const showNoteSource = (stateToShow: NonNullable<typeof noteState>): void => {
    noteState = stateToShow;
    note.textContent = noteText();
    note.classList.toggle("is-error", stateToShow.isError);
    note.hidden = false;
    window.clearTimeout(noteTimer);
    noteTimer = window.setTimeout(() => {
      note.hidden = true;
    }, NOTE_AUTO_HIDE_MS);
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

  // 菜单可用性/状态文案由后端状态与开关驱动;增强项在功能关闭时整体隐藏。
  const syncUi = (): void => {
    const opacity = state?.opacity ?? 1;
    opacityBtn.textContent = `${Math.round(opacity * 100)}%`;
    menu.querySelectorAll<HTMLButtonElement>("[data-menu-opacity]").forEach((button) => {
      button.classList.toggle(
        "active",
        Math.abs(Number(button.dataset.menuOpacity) - opacity) < 0.001,
      );
    });

    const enhance = options.enhance;
    menu.querySelectorAll<HTMLElement>("[data-enhance]").forEach((element) => {
      element.hidden = !enhance;
    });
    const clickThroughBlocked =
      !enhance || !options.clickThroughSupported || !options.trayAvailable;
    clickThroughBtn.classList.toggle("is-disabled", clickThroughBlocked);
    clickThroughBtn.setAttribute("aria-disabled", String(clickThroughBlocked));
    if (options.clickThroughReason) {
      clickThroughBtn.dataset.tooltip = options.clickThroughReason;
    } else {
      delete clickThroughBtn.dataset.tooltip;
    }
    clickThroughBtn.textContent = t(
      state?.clickThrough ? "pin.menu.click_through_off" : "pin.menu.click_through",
    );

    const grouped = state?.grouped ?? false;
    groupBtn.hidden = !enhance || grouped;
    ungroupBtn.hidden = !enhance || !grouped;
    closeMenuBtn.textContent = t(grouped ? "pin.menu.close_group" : "pin.menu.close");
    closeToolbarBtn.dataset.tooltip = t(
      grouped ? "pin.toolbar.close_group_title" : "pin.toolbar.close_title",
    );

    // R8:文本贴图的复制目标是原始文本而非渲染图像,标题与菜单同步。
    const copyText = state?.copyKind === "text";
    copyToolbarBtn.dataset.tooltip = t(
      copyText ? "pin.toolbar.copy_text_title" : "pin.toolbar.copy_title",
    );
    copyMenuBtn.textContent = t(copyText ? "pin.menu.copy_text" : "pin.menu.copy");
  };

  // 后端返回的 PNG 已应用旋转/翻转/透明度,画布只按窗口尺寸拉伸显示。
  const render = (): void => {
    if (!image) {
      return;
    }
    canvas.width = Math.max(1, image.naturalWidth);
    canvas.height = Math.max(1, image.naturalHeight);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(image, 0, 0);
  };

  const hideMenu = (): void => {
    // 焦点在菜单内时,关闭后返回 stage,键盘用户不丢失上下文。
    const focusInside = menu.contains(document.activeElement);
    menu.hidden = true;
    if (focusInside) {
      stage.focus();
    }
  };

  const close = (): void => {
    void invoke("close_pin", { label: win.label });
  };

  const decodeImage = (bytes: ArrayBuffer): Promise<HTMLImageElement> => {
    const url = URL.createObjectURL(new Blob([bytes], { type: "image/png" }));
    return new Promise<HTMLImageElement>((resolve, reject) => {
      const next = new Image();
      next.onload = () => resolve(next);
      next.onerror = () => reject(new Error("decode"));
      next.src = url;
    }).finally(() => {
      URL.revokeObjectURL(url);
    });
  };

  const loadState = async (): Promise<PinState | null> => {
    try {
      return await invoke<PinState>("get_pin_state", { label: win.label });
    } catch {
      return null;
    }
  };

  const loadOptions = async (): Promise<PinOptions> => {
    try {
      const next = await invoke<PinOptions>("get_pin_options");
      return { ...options, ...next };
    } catch {
      return options;
    }
  };

  // 重拉图像与状态:变换后的画面、复制与保存共用同一后端变换。
  const refresh = async (updatedNote: boolean): Promise<void> => {
    const hadImage = image !== null;
    try {
      const bytes = await invoke<ArrayBuffer>("get_pin_image", { label: win.label });
      if (!bytes || bytes.byteLength === 0) {
        throw new Error("empty");
      }
      const nextState = await loadState();
      image = await decodeImage(bytes);
      if (nextState) {
        state = nextState;
      }
      root.classList.remove("is-broken");
      render();
      syncUi();
      if (updatedNote && hadImage) {
        showNoteKey("pin.note.updated");
      }
    } catch {
      if (hadImage) {
        showNoteKey("pin.note.reload_failed", undefined, true);
      }
    }
  };

  const applyState = (next: PinState | null, allowExitNote = true): void => {
    const wasClickThrough = state?.clickThrough ?? false;
    state = next;
    syncUi();
    if (allowExitNote && wasClickThrough && next && !next.clickThrough) {
      showNoteKey("pin.note.click_through_off");
    }
  };

  const copy = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      await invoke("copy_pin", { label: win.label });
      showNoteKey(state?.copyKind === "text" ? "pin.note.copied_text" : "pin.note.copied");
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
      await invoke("begin_pin_edit", { label: win.label });
    } catch (error) {
      showNote(invokeError(error, t("pin.error.annotate")), true);
    } finally {
      busy = false;
    }
  };

  const rotate = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      applyState(await invoke<PinState>("rotate_pin", { label: win.label }), false);
    } catch (error) {
      showNote(invokeError(error, t("pin.error.rotate")), true);
    } finally {
      busy = false;
    }
  };

  const flip = async (axis: "horizontal" | "vertical"): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      await invoke("flip_pin", { label: win.label, axis });
      showNoteKey("pin.note.flipped");
    } catch (error) {
      showNote(invokeError(error, t("pin.error.flip")), true);
    } finally {
      busy = false;
    }
  };

  const setOpacity = async (next: number): Promise<void> => {
    if (!OPACITY_STEPS.includes(next) || busy) {
      return;
    }
    busy = true;
    try {
      applyState(await invoke<PinState>("set_pin_opacity", { label: win.label, opacity: next }), false);
    } catch (error) {
      showNote(invokeError(error, t("pin.error.opacity")), true);
    } finally {
      busy = false;
    }
  };

  const cycleOpacity = (): void => {
    const index = OPACITY_STEPS.indexOf(state?.opacity ?? 1);
    void setOpacity(OPACITY_STEPS[(index + 1) % OPACITY_STEPS.length]);
  };

  const toggleClickThrough = async (): Promise<void> => {
    if (busy) {
      return;
    }
    const blocked = !options.enhance || !options.clickThroughSupported || !options.trayAvailable;
    if (blocked) {
      showNote(options.clickThroughReason ?? t("pin.error.click_through"), true);
      return;
    }
    busy = true;
    try {
      const next = await invoke<PinState>("set_pin_click_through", {
        label: win.label,
        enabled: !(state?.clickThrough ?? false),
      });
      applyState(next, false);
      showNoteKey(next.clickThrough ? "pin.note.click_through_on" : "pin.note.click_through_off");
    } catch (error) {
      showNote(invokeError(error, t("pin.error.click_through")), true);
    } finally {
      busy = false;
    }
  };

  const groupAll = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      const count = await invoke<number>("group_all_pins");
      applyState(await loadState(), false);
      showNoteKey("pin.note.grouped", { count });
    } catch (error) {
      showNote(invokeError(error, t("pin.error.group")), true);
    } finally {
      busy = false;
    }
  };

  const ungroup = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      applyState(await invoke<PinState>("ungroup_pin", { label: win.label }), false);
      showNoteKey("pin.note.ungrouped");
    } catch (error) {
      showNote(invokeError(error, t("pin.error.ungroup")), true);
    } finally {
      busy = false;
    }
  };

  const resetZoom = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      applyState(await invoke<PinState>("reset_pin_zoom", { label: win.label }), false);
    } catch (error) {
      showNote(invokeError(error, t("pin.error.reset")), true);
    } finally {
      busy = false;
    }
  };

  // 滚轮缩放:锚点为光标所在屏幕的逻辑坐标;后端按当前缩放钳制并联动整组。
  let zoomInFlight = false;
  let zoomPending: { zoomIn: boolean; clientX: number; clientY: number } | null = null;
  const zoom = async (zoomIn: boolean, clientX: number, clientY: number): Promise<void> => {
    if (zoomInFlight) {
      zoomPending = { zoomIn, clientX, clientY };
      return;
    }
    zoomInFlight = true;
    try {
      const factor = await win.scaleFactor();
      const position = await win.innerPosition();
      const next = await invoke<PinState>("zoom_pin", {
        label: win.label,
        zoomIn,
        anchorX: position.x / factor + clientX,
        anchorY: position.y / factor + clientY,
      });
      applyState(next, false);
    } catch (error) {
      showNote(invokeError(error, t("pin.error.zoom")), true);
    } finally {
      zoomInFlight = false;
      if (zoomPending) {
        const pending = zoomPending;
        zoomPending = null;
        void zoom(pending.zoomIn, pending.clientX, pending.clientY);
      }
    }
  };

  // 窗口移动回执:后端把增量应用到整组;串行化避免回执堆积。
  let moveInFlight = false;
  let movePending: { x: number; y: number } | null = null;
  const reportMove = async (physicalX: number, physicalY: number): Promise<void> => {
    if (!state) {
      return;
    }
    if (moveInFlight) {
      movePending = { x: physicalX, y: physicalY };
      return;
    }
    moveInFlight = true;
    try {
      const factor = window.devicePixelRatio || 1;
      await invoke("move_pin", {
        label: win.label,
        x: physicalX / factor,
        y: physicalY / factor,
      });
    } catch {
      // 移动回执失败不打断拖动;下次事件或退出落盘仍会同步几何。
    } finally {
      moveInFlight = false;
      if (movePending) {
        const pending = movePending;
        movePending = null;
        void reportMove(pending.x, pending.y);
      }
    }
  };

  const runAction = (action: string): void => {
    if (action === "copy") {
      void copy();
    } else if (action === "save") {
      void save();
    } else if (action === "rotate") {
      void rotate();
    } else if (action === "flip-h") {
      void flip("horizontal");
    } else if (action === "flip-v") {
      void flip("vertical");
    } else if (action === "opacity") {
      cycleOpacity();
    } else if (action === "click-through") {
      void toggleClickThrough();
    } else if (action === "group") {
      void groupAll();
    } else if (action === "ungroup") {
      void ungroup();
    } else if (action === "annotate") {
      void annotate();
    } else if (action === "reset") {
      void resetZoom();
    } else if (action === "close") {
      close();
    }
  };

  // 打开菜单后焦点移入第一个可见项,Esc/点选/外部点击关闭后由 hideMenu 送回 stage。
  const focusMenu = (): void => {
    const first = Array.from(menu.querySelectorAll<HTMLButtonElement>("button")).find(
      (button) => !button.hidden,
    );
    first?.focus();
  };

  const openMenuAt = async (clientX: number, clientY: number): Promise<void> => {
    if (!state) {
      return;
    }
    options = await loadOptions();
    syncUi();
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
    const x = Math.min(Math.max(4, clientX), Math.max(4, root.clientWidth - mw - 4));
    const y = Math.min(Math.max(4, clientY), Math.max(4, root.clientHeight - mh - 4));
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;
    focusMenu();
  };

  stage.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    void openMenuAt(event.clientX, event.clientY);
  });

  // 键盘打开菜单:聚焦 stage 后按菜单键/Shift+F10/Enter,菜单出现在窗口中心。
  stage.addEventListener("keydown", (event) => {
    const menuKey =
      event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey);
    if (menuKey || event.key === "Enter") {
      event.preventDefault();
      void openMenuAt(Math.round(root.clientWidth / 2), Math.round(root.clientHeight / 2));
    }
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
    const menuOpacity = button.dataset.menuOpacity;
    if (menuOpacity !== undefined) {
      hideMenu();
      void setOpacity(Number(menuOpacity));
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
      void zoom(event.deltaY < 0, event.clientX, event.clientY);
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

  // 再标注确认/复用池窗口重新展示时 Rust 广播 pin-reload;写回带 "writeback"
  // 载荷,用于区分「贴图已更新」提示。
  void listen<string | null>("pin-reload", (event) => {
    void refresh(event.payload === "writeback");
  });
  // 编组/穿透/几何变化只需要同步状态,不重拉图像。
  void listen<PinState>("pin-state", (event) => {
    const payload = event.payload;
    if (!payload || payload.label !== win.label) {
      return;
    }
    applyState(payload);
  });
  // 关闭(整组)后清空显示;窗口可能被池复用为下一张贴图。
  void listen("pin-cleared", () => {
    image = null;
    state = null;
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    syncUi();
  });

  void win.onMoved(({ payload }) => {
    void reportMove(payload.x, payload.y);
  });

  void (async () => {
    options = await loadOptions();
    syncUi();
    await refresh(false);
  })();

  // 语言切换:重渲染进行中的提示文案与状态菜单;静态标签由 main 应用。
  return () => {
    renderNote();
    syncUi();
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
