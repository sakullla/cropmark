import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  loadAnnotationDefaults,
  mountAnnotationEditor,
  resolveCanvasColor,
  type Annotation,
  type AnnotationEditor,
} from "../annotation";
import { t, type CatalogKey } from "../i18n";
import { mountOcrModel, type OcrModel } from "../ocr";
import { mountQrModel, type QrModel } from "../qr";
import { canvasGeometry } from "./geometry";
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
  workspaceActions?: boolean;
  copy?: boolean;
  save?: boolean;
  pin?: boolean;
  ocr?: boolean;
  qr?: boolean;
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
  /** R7:窗口级元素吸附可用(平台窗口级检测或本会话可枚举窗口)。 */
  snapWindowLevel?: boolean;
  /** R7:控件级元素吸附可用。 */
  snapControlLevel?: boolean;
  windows: ListedWindow[];
  fixed?: boolean;
  annotations?: Annotation[];
  pendingOcr?: boolean;
  pendingQr?: boolean;
  /** 录屏 MP4:确认前把宽高向下收成偶数,尺寸与将要录下的矩形一致。 */
  recordEven?: boolean;
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

const FALLBACK_SELECTION = "#1d4ed8";
const FALLBACK_SELECTION_HALO = "#ffffff";
const FALLBACK_IDLE_STROKE = "#1c2128";

/// 触发不可用能力时的即时说明:同一事实在面板与按键反馈里保持一致。
const TOOLBAR_NOTICE_KEY: CatalogKey = "overlay.notice.toolbar";
const COLOR_NOTICE_KEY: CatalogKey = "overlay.notice.color";
const NUDGE_NOTICE_KEY: CatalogKey = "overlay.notice.nudge";

function canvasToken(root: HTMLElement, name: string, fallback: string): string {
  return resolveCanvasColor(getComputedStyle(root).getPropertyValue(name), fallback);
}

// 选区描边读令牌。晕边与描边等宽并外移一个线宽，两条边刚好相接，对比不靠壁纸像素。
function strokeWithHalo(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  width: number,
  height: number,
  lineWidth: number,
  stroke: string,
  selectionHalo: string,
): void {
  ctx.lineWidth = lineWidth;
  ctx.strokeStyle = selectionHalo;
  ctx.strokeRect(x - lineWidth, y - lineWidth, width + lineWidth * 2, height + lineWidth * 2);
  ctx.strokeStyle = stroke;
  ctx.strokeRect(x, y, width, height);
}

export function mountOverlay(root: HTMLElement): () => void {
  root.className = "overlay-root";
  root.innerHTML = `
    <canvas></canvas>
    <div class="overlay-chrome">
      <div class="overlay-hint"></div>
      <button type="button" class="overlay-retry" data-i18n="overlay.retry" hidden>重试</button>
      <div class="overlay-actions" hidden>
        <button type="button" data-workspace="ocr" data-i18n="overlay.action.ocr">取字</button>
        <button type="button" data-workspace="qr" data-i18n="overlay.action.qr">识别二维码</button>
        <button type="button" data-workspace="pin" data-i18n="overlay.action.pin">贴图</button>
        <button type="button" data-workspace="save" data-i18n="overlay.action.save">保存</button>
        <button type="button" class="primary" data-workspace="copy" data-i18n="overlay.action.copy">复制</button>
        <button type="button" data-workspace="edit" data-i18n="overlay.action.edit">进一步编辑</button>
      </div>
      <button type="button" class="overlay-capabilities" aria-haspopup="dialog" aria-controls="overlay-capability-panel" aria-expanded="false" data-i18n="overlay.capabilities" hidden>能力说明</button>
      <button type="button" class="overlay-cancel" data-i18n="overlay.cancel">取消 Esc</button>
    </div>
    <div class="overlay-tools annotation-tools" role="toolbar" data-i18n-aria-label="preview.toolbar_group" aria-label="标注" hidden></div>
    <aside id="overlay-capability-panel" class="capability-panel" role="dialog" tabindex="-1" data-i18n-aria-label="overlay.panel.title" aria-label="能力说明" hidden></aside>
    <div class="overlay-notice" role="status" hidden></div>
    <div class="size-badge" hidden></div>
    <div class="window-list" hidden></div>
  `;
  const canvas = root.querySelector("canvas");
  const hint = root.querySelector(".overlay-hint");
  const retryBtn = root.querySelector(".overlay-retry");
  const badge = root.querySelector(".size-badge");
  const list = root.querySelector(".window-list");
  const cancelBtn = root.querySelector(".overlay-cancel");
  const actionsEl = root.querySelector(".overlay-actions");
  const toolsEl = root.querySelector(".overlay-tools");
  const capabilityToggle = root.querySelector(".overlay-capabilities");
  const capabilityPanel = root.querySelector(".capability-panel");
  const notice = root.querySelector(".overlay-notice");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(hint instanceof HTMLElement) ||
    !(retryBtn instanceof HTMLButtonElement) ||
    !(badge instanceof HTMLElement) ||
    !(list instanceof HTMLElement) ||
    !(cancelBtn instanceof HTMLButtonElement) ||
    !(actionsEl instanceof HTMLElement) ||
    !(toolsEl instanceof HTMLElement) ||
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
  /// R7:命中栈(光标下重叠窗口,自顶层到底层)与当前高亮层级。
  let snapStack: ListedWindow[] = [];
  let snapLevel = 0;
  /// R7:当前高亮窗口在帧坐标的矩形(悬停高亮/点击吸附/尺寸徽标同源)。
  let snapRect: Selection | null = null;
  /// R7:区域模式点击吸附:按下时记录,未拖动则松开即确认高亮窗口。
  let snapPressPoint: { x: number; y: number } | null = null;
  let snapClickPending = false;
  /// R7:点击吸附与自由拖选的判别阈值(物理像素,容忍按下抖动)。
  const SNAP_DRAG_THRESHOLD = 3;
  /// 最近一次指针在宿主内的 CSS 坐标。尺寸徽标用它避开光标热点。
  let pointer: { x: number; y: number } | null = null;
  /// 当前徽标对应的帧矩形;为空则不显示。
  let badgeTarget: Selection | null = null;
  /// R21:即时标注会话阶段;"select" 拖选区,"annotate" 选区固定后可标注。
  let phase: "select" | "annotate" = "select";
  /// R24:关闭 inlineAnnotation 后覆盖层不提供标注层,保持原有的松开即完成。
  let inlineEnabled = false;
  let editor: AnnotationEditor | null = null;
  /// R2:冻结帧工作区的共享取字模型(与预览同一实现)。
  let ocrModel: OcrModel | null = null;
  /// R4:冻结帧工作区的共享二维码识别模型(与预览同一实现)。
  let qrModel: QrModel | null = null;
  /// 标注合成层:与冻帧同物理尺寸的底图 + 图元(马赛克/模糊需要整帧像素)。
  let annotationLayer: HTMLCanvasElement | null = null;
  /// 底图缓存:冻帧位图按物理尺寸只缩放一次,避免逐帧重采样。
  let annotationBase: HTMLCanvasElement | null = null;

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
    let width = right - left;
    let height = bottom - top;
    if (frame.recordEven) {
      width -= width % 2;
      height -= height % 2;
    }
    return { x: left, y: top, width, height };
  };

  const annotationActive = (): boolean =>
    inlineEnabled && frame !== null && (frame.fixed === true || phase === "annotate");

  const resetAnnotationSession = (): void => {
    const wasAnnotating = phase === "annotate";
    phase = "select";
    toolsEl.hidden = true;
    actionsEl.hidden = true;
    root.classList.remove("has-tools");
    editor?.cancelText();
    editor?.setAnnotations([]);
    if (wasAnnotating) {
      renderHint();
    }
  };

  const ensureAnnotationLayer = (target: OverlayFrame): HTMLCanvasElement => {
    if (
      !annotationLayer ||
      annotationLayer.width !== Math.max(1, target.width) ||
      annotationLayer.height !== Math.max(1, target.height)
    ) {
      annotationLayer = document.createElement("canvas");
      annotationLayer.width = Math.max(1, target.width);
      annotationLayer.height = Math.max(1, target.height);
      annotationBase = null;
    }
    return annotationLayer;
  };

  /// 与冻帧同物理尺寸的底图:标注绘制(马赛克/模糊取像素)以它为基准,
  /// 保证覆盖层所见与 Rust 对完整冻帧裁剪 + rasterize 的结果一致。
  const ensureAnnotationBase = (target: OverlayFrame): HTMLCanvasElement | null => {
    if (!image || image.naturalWidth === 0) {
      return null;
    }
    if (
      !annotationBase ||
      annotationBase.width !== Math.max(1, target.width) ||
      annotationBase.height !== Math.max(1, target.height)
    ) {
      const base = document.createElement("canvas");
      base.width = Math.max(1, target.width);
      base.height = Math.max(1, target.height);
      const baseCtx = base.getContext("2d");
      if (!baseCtx) {
        return null;
      }
      baseCtx.drawImage(image, 0, 0, base.width, base.height);
      annotationBase = base;
    }
    return annotationBase;
  };

  // ADR-2:进行中提示(persistent)不自动消失,直到完成/失败文案替换;
  // 瞬态结果提示仍可 3.6s 自动隐藏。
  const showNotice = (message: string, persistent = false): void => {
    notice.textContent = message;
    notice.hidden = false;
    if (noticeTimer) {
      window.clearTimeout(noticeTimer);
      noticeTimer = 0;
    }
    if (!persistent) {
      noticeTimer = window.setTimeout(() => {
        noticeTimer = 0;
        notice.hidden = true;
      }, 3600);
    }
  };

  // 隐藏提示必须同时清掉自动隐藏计时器,否则旧计时器会提前藏掉新提示。
  const hideNotice = (): void => {
    if (noticeTimer) {
      window.clearTimeout(noticeTimer);
      noticeTimer = 0;
    }
    notice.hidden = true;
  };

  // finishing 期间动作按钮给共享 disabled 可视态(app.css button:disabled),
  // 不再只是静默忽略重复点击。
  const setFinishing = (value: boolean): void => {
    finishing = value;
    actionsEl.querySelectorAll("button").forEach((el) => {
      if (el instanceof HTMLButtonElement) {
        el.disabled = value;
      }
    });
  };

  // 加载/确认失败除改写 hint 外提供重试入口;成功路径经 renderHint/load 收起。
  let retryAction: (() => void) | null = null;
  const showFailure = (message: string, retry: () => void): void => {
    hint.textContent = message;
    retryAction = retry;
    retryBtn.hidden = false;
  };

  const setCapabilityPanel = (
    open: boolean,
    options: { focusPanel?: boolean; restoreFocus?: boolean } = {},
  ): void => {
    const focusWasInside = capabilityPanel.contains(document.activeElement);
    capabilityPanel.hidden = !open;
    capabilityToggle.setAttribute("aria-expanded", open ? "true" : "false");
    capabilityToggle.textContent = t(
      open ? "overlay.capabilities_collapse" : "overlay.capabilities",
    );
    if (open && options.focusPanel) {
      capabilityPanel.focus();
    } else if (
      !open &&
      (options.restoreFocus || focusWasInside) &&
      !capabilityToggle.hidden
    ) {
      capabilityToggle.focus();
    }
  };

  const renderCapabilityPanel = (): void => {
    const title = document.createElement("h2");
    title.textContent = t("overlay.panel.title");
    const available = document.createElement("p");
    available.className = "available";
    // R21:能力说明随 inlineAnnotation 开关给出标注可用性与替代路径。
    available.textContent = `${t("overlay.panel.available")} ${
      inlineEnabled
        ? t("overlay.panel.annotate_available")
        : t("overlay.panel.annotate_unavailable")
    }`;
    const listEl = document.createElement("dl");
    const items: Array<{ nameKey: CatalogKey; detailKey: CatalogKey }> = [
      ...REDUCED_CAPABILITIES,
    ];
    // R7:控件级吸附不可用时,用同一能力说明机制给出降级说明(窗口级可用
    // 时说明窗口级路径;完全不可用时给自由框选路径)。
    if (frame?.snapControlLevel !== true) {
      items.push({
        nameKey: "overlay.caps.snap_name",
        detailKey:
          frame?.snapWindowLevel === true
            ? "overlay.caps.snap_detail_window"
            : "overlay.caps.snap_detail_none",
      });
    }
    for (const item of items) {
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
    // 恢复正常提示即收起失败重试入口。
    retryBtn.hidden = true;
    retryAction = null;
    const reduced = frame.reducedCapabilities === true;
    if (frame.fixed) {
      // R2/R4:取字/二维码识别中提示退出方式;退出后恢复工作区/标注提示。
      if (ocrModel?.active === true) {
        hint.textContent = t("overlay.hint.ocr");
        return;
      }
      if (qrModel?.active === true) {
        hint.textContent = t("overlay.hint.qr");
        return;
      }
      hint.textContent = t(
        annotationActive()
          ? reduced
            ? "overlay.hint.reduced_annotate"
            : "overlay.hint.annotate"
          : "overlay.hint.workspace",
      );
      return;
    }
    hint.textContent =
      frame.mode === "window"
        ? t("overlay.hint.window")
        : frame.snapWindowLevel === true
          ? t("overlay.hint.snap")
          : reduced
            ? t("overlay.hint.reduced")
            : t("overlay.hint.region");
  };

  // 画布几何由 `canvasGeometry` 唯一决定:fixed 工作区是帧的等比显示框,
  // 其余模式铺满窗口。图像、标注层与取字三态共用这一张画布,帧→画布比例
  // 恒为 canvas.width/frame.width,不再假定帧等于窗口或显示器物理尺寸。
  const fitCanvas = (): void => {
    const host = root.getBoundingClientRect();
    const geometry = canvasGeometry(
      { width: frame?.width ?? 0, height: frame?.height ?? 0, fixed: frame?.fixed === true },
      { width: host.width, height: host.height },
      window.devicePixelRatio || 1,
    );
    if (canvas.width !== geometry.width || canvas.height !== geometry.height) {
      canvas.width = geometry.width;
      canvas.height = geometry.height;
    }
    canvas.style.width = `${geometry.cssWidth}px`;
    canvas.style.height = `${geometry.cssHeight}px`;
    canvas.style.left = `${geometry.left}px`;
    canvas.style.top = `${geometry.top}px`;
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

  // ---- R7:元素级吸附(Web 覆盖层只枚举窗口级:命中栈即光标下重叠窗口)。----

  /// 悬停吸附可用:窗口模式恒有(现有行为);区域模式由后端下发窗口级能力。
  const snapHoverEnabled = (): boolean =>
    frame !== null &&
    frame.fixed !== true &&
    (frame.mode === "window" || frame.snapWindowLevel === true);

  /// 光标下重叠窗口,自顶层(Z 序最上)到底层;首个命中即用户实际看到的最上层窗口。
  const snapStackAt = (x: number, y: number): ListedWindow[] => {
    if (!frame) {
      return [];
    }
    const view = frame;
    return view.windows.filter((item) => {
      const rect = windowRectOnFrame(item, view);
      return (
        rect !== null &&
        x >= rect.x &&
        y >= rect.y &&
        x <= rect.x + rect.width &&
        y <= rect.y + rect.height
      );
    });
  };

  const selectedSnapWindow = (): ListedWindow | null => snapStack[snapLevel] ?? null;

  const refreshSnapRect = (): void => {
    const picked = selectedSnapWindow();
    snapRect = frame && picked ? windowRectOnFrame(picked, frame) : null;
  };

  const sameRect = (a: Selection | null, b: Selection | null): boolean =>
    a === b ||
    (a !== null &&
      b !== null &&
      a.x === b.x &&
      a.y === b.y &&
      a.width === b.width &&
      a.height === b.height);

  /// 刷新悬停命中栈(层级回到最顶层窗口);窗口模式同步窗口列表高亮。
  /// 返回高亮是否变化,供调用方决定是否重绘。
  const updateSnapHover = (x: number, y: number): boolean => {
    if (!snapHoverEnabled()) {
      const changed = snapStack.length > 0 || snapRect !== null;
      snapStack = [];
      snapLevel = 0;
      snapRect = null;
      return changed;
    }
    const beforeId = selectedSnapWindow()?.id ?? null;
    const beforeRect = snapRect;
    const next = snapStackAt(x, y);
    const same =
      next.length === snapStack.length && next.every((item, index) => item.id === snapStack[index].id);
    if (!same) {
      snapStack = next;
      snapLevel = 0;
    }
    refreshSnapRect();
    let changed = !sameRect(beforeRect, snapRect);
    if (frame?.mode === "window") {
      const id = selectedSnapWindow()?.id ?? null;
      if (id !== hoverId) {
        hoverId = id;
        markActiveWindow(list, hoverId);
        changed = changed || id !== beforeId;
      }
    }
    return changed;
  };

  /// 滚轮在命中栈层级间切换:向下滚进入更底层重叠窗口,向上滚回到更上层
  /// (与原生壳的父子层级方向一致)。栈内没有更多层级时不消费事件。
  const cycleSnap = (delta: number): boolean => {
    if (!snapHoverEnabled() || snapStack.length <= 1) {
      return false;
    }
    const level = clamp(snapLevel + (delta > 0 ? 1 : -1), 0, snapStack.length - 1);
    if (level === snapLevel) {
      return false;
    }
    snapLevel = level;
    refreshSnapRect();
    if (frame?.mode === "window") {
      hoverId = selectedSnapWindow()?.id ?? null;
      markActiveWindow(list, hoverId);
    }
    scheduleDraw();
    return true;
  };

  const clearSnap = (): void => {
    snapStack = [];
    snapLevel = 0;
    snapRect = null;
    snapClickPending = false;
    snapPressPoint = null;
  };

  // R2:取字三态由共享模型按帧物理坐标绘制。工作区画布是帧的等比显示框
  // (长图会小于帧),绘制前把画布变换到帧坐标空间,与图像、标注层共用同一
  // frame→画布映射;共享模型按当前变换还原帧空间线宽,视觉比例与预览一致。
  const paintOcr = (): void => {
    if (ocrModel?.active !== true || !frame) {
      return;
    }
    ctx.save();
    ctx.setTransform(
      canvas.width / Math.max(frame.width, 1),
      0,
      0,
      canvas.height / Math.max(frame.height, 1),
      0,
      0,
    );
    ocrModel.paint(ctx);
    ctx.restore();
  };

  /// 帧矩形映射到宿主 CSS 坐标(画布在工作区是居中的显示框)。
  const frameBoxToHost = (box: Selection): Selection | null => {
    if (!frame || frame.width < 1 || frame.height < 1) {
      return null;
    }
    const host = root.getBoundingClientRect();
    const rect = canvas.getBoundingClientRect();
    return {
      x: rect.left - host.left + (box.x / frame.width) * rect.width,
      y: rect.top - host.top + (box.y / frame.height) * rect.height,
      width: (box.width / frame.width) * rect.width,
      height: (box.height / frame.height) * rect.height,
    };
  };

  const clearBadge = (): void => {
    badgeTarget = null;
    badge.hidden = true;
  };

  /// 尺寸徽标放在离光标热点最远、且仍在画面内的角外侧。没有光标时按选区中心平局,留在左上。
  const layoutBadge = (): void => {
    if (!frame || !badgeTarget) {
      badge.hidden = true;
      return;
    }
    const target = frameBoxToHost(badgeTarget);
    if (!target || target.width < 1 || target.height < 1) {
      badge.hidden = true;
      return;
    }
    badge.hidden = false;
    const label = t("overlay.size_format", {
      width: badgeTarget.width,
      height: badgeTarget.height,
    });
    if (badge.textContent !== label) {
      badge.textContent = label;
    }
    const host = root.getBoundingClientRect();
    const badgeWidth = badge.offsetWidth;
    const badgeHeight = badge.offsetHeight;
    if (badgeWidth < 1 || badgeHeight < 1 || host.width < 1 || host.height < 1) {
      return;
    }
    const cursor = pointer ?? {
      x: target.x + target.width / 2,
      y: target.y + target.height / 2,
    };
    const placed = placeSizeBadge(target, cursor, host.width, host.height, badgeWidth, badgeHeight, 6);
    badge.style.left = `${placed.x}px`;
    badge.style.top = `${placed.y}px`;
  };

  const placeBadge = (crop: Selection): void => {
    badgeTarget = crop;
    layoutBadge();
  };

  // 结果面板左右槽:顶距与边距跟 overlay.css 一致。没有正面积矩形时留在右侧。
  const RESULT_PANEL_TOP = 64;
  const RESULT_PANEL_MARGIN = 18;
  const placeResultPanel = (panel: HTMLElement, regions: Selection[]): void => {
    if (panel.hidden) {
      delete panel.dataset.slot;
      return;
    }
    const host = root.getBoundingClientRect();
    const width = panel.offsetWidth;
    const height = panel.offsetHeight;
    const usable = regions.filter((region) => region.width > 0 && region.height > 0);
    if (width < 1 || height < 1 || host.width < 1 || usable.length === 0) {
      delete panel.dataset.slot;
      return;
    }
    const left = { x: RESULT_PANEL_MARGIN, y: RESULT_PANEL_TOP, width, height };
    const right = {
      x: Math.max(RESULT_PANEL_MARGIN, host.width - RESULT_PANEL_MARGIN - width),
      y: RESULT_PANEL_TOP,
      width,
      height,
    };
    const area = (slot: Selection): number =>
      usable.reduce((sum, region) => sum + intersectionArea(slot, region), 0);
    if (area(left) < area(right)) {
      panel.dataset.slot = "left";
    } else {
      delete panel.dataset.slot;
    }
  };

  const placeResultPanels = (): void => {
    const ocrPanel = root.querySelector(":scope > .ocr-panel");
    const qrPanel = root.querySelector(":scope > .qr-panel");
    if (ocrPanel instanceof HTMLElement) {
      const regions: Selection[] = [];
      for (const span of ocrModel?.document()?.spans ?? []) {
        if (!span.text.trim() || span.width <= 0 || span.height <= 0) {
          continue;
        }
        const box = frameBoxToHost(span);
        if (box) {
          regions.push(box);
        }
      }
      placeResultPanel(ocrPanel, regions);
    }
    if (qrPanel instanceof HTMLElement) {
      const regions: Selection[] = [];
      for (const region of qrModel?.regions() ?? []) {
        if (region.width <= 0 || region.height <= 0) {
          continue;
        }
        const box = frameBoxToHost(region);
        if (box) {
          regions.push(box);
        }
      }
      placeResultPanel(qrPanel, regions);
    }
  };

  const draw = (): void => {
    if (!image || !frame || image.naturalWidth === 0) {
      return;
    }
    fitCanvas();
    placeResultPanels();
    ctx.drawImage(image, 0, 0, canvas.width, canvas.height);
    if (frame.fixed !== true) {
      // 选择阶段整帧压暗;工作区帧已定,压暗只会让工作区图像发灰。
      ctx.fillStyle = "rgba(12, 10, 9, 0.48)";
      ctx.fillRect(0, 0, canvas.width, canvas.height);
    }
    const selectionStroke = canvasToken(root, "--selection-stroke", FALLBACK_SELECTION);
    const selectionHalo = canvasToken(root, "--selection-halo", FALLBACK_SELECTION_HALO);
    const idleStroke = canvasToken(root, "--ink", FALLBACK_IDLE_STROKE);
    if (frame.mode === "window" && frame.fixed !== true) {
      // 窗口模式不挖洞(Snipaste 惯例):整帧均匀压暗,冻结帧中缺失的
      // 后开窗口不再呈现黑洞;悬停窗口画令牌描边、紧贴晕边和轻微填充,
      // 其余候选窗口用正文色描边加同一条晕边。
      for (const listed of frame.windows) {
        const rect = windowRectOnFrame(listed, frame);
        if (!rect) {
          continue;
        }
        const mapped = toCanvas(rect.x, rect.y, rect.width, rect.height);
        const active = hoverId === listed.id;
        if (active) {
          ctx.save();
          ctx.globalAlpha = 0.16;
          ctx.fillStyle = selectionStroke;
          ctx.fillRect(mapped.x, mapped.y, mapped.width, mapped.height);
          ctx.restore();
          strokeWithHalo(
            ctx,
            mapped.x + 1.25,
            mapped.y + 1.25,
            mapped.width - 2.5,
            mapped.height - 2.5,
            2.5,
            selectionStroke,
            selectionHalo,
          );
        } else {
          strokeWithHalo(
            ctx,
            mapped.x + 0.5,
            mapped.y + 0.5,
            mapped.width - 1,
            mapped.height - 1,
            1,
            idleStroke,
            selectionHalo,
          );
        }
      }
      // R7:窗口模式下尺寸显示随悬停高亮更新(点击/滚轮选中的窗口)。
      const hoveredWin = selectedSnapWindow();
      const hoveredRect = hoveredWin ? windowRectOnFrame(hoveredWin, frame) : null;
      if (hoveredRect) {
        placeBadge(hoveredRect);
      } else {
        clearBadge();
      }
      return;
    }
    const crop = roundedRect();
    if (!crop || crop.width < 1 || crop.height < 1) {
      // R7:区域模式尚无选区时绘制悬停吸附高亮(窗口级)与尺寸徽标。
      const snap = snapHoverEnabled() ? snapRect : null;
      if (snap) {
        const snapMapped = toCanvas(snap.x, snap.y, snap.width, snap.height);
        ctx.save();
        ctx.globalAlpha = 0.16;
        ctx.fillStyle = selectionStroke;
        ctx.fillRect(snapMapped.x, snapMapped.y, snapMapped.width, snapMapped.height);
        ctx.restore();
        strokeWithHalo(
          ctx,
          snapMapped.x + 1.25,
          snapMapped.y + 1.25,
          snapMapped.width - 2.5,
          snapMapped.height - 2.5,
          2.5,
          selectionStroke,
          selectionHalo,
        );
        placeBadge(snap);
      } else {
        clearBadge();
      }
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
    // R21:标注层在冻帧物理像素上渲染(马赛克/模糊取冻帧底图像素),
    // 再按选区裁剪合成,保证所见与 Rust 裁剪 + rasterize 的最终输出一致。
    // 无标注内容时不合成,保持选区视图与既有性能特征。
    if (annotationActive() && editor?.hasContent()) {
      const layer = ensureAnnotationLayer(frame);
      const base = ensureAnnotationBase(frame);
      const layerCtx = layer.getContext("2d");
      if (base && layerCtx) {
        layerCtx.clearRect(0, 0, layer.width, layer.height);
        layerCtx.drawImage(base, 0, 0);
        editor.paint(layerCtx);
        ctx.drawImage(
          layer,
          crop.x,
          crop.y,
          crop.width,
          crop.height,
          mapped.x,
          mapped.y,
          mapped.width,
          mapped.height,
        );
      }
    }
    strokeWithHalo(
      ctx,
      mapped.x + 1,
      mapped.y + 1,
      mapped.width - 2,
      mapped.height - 2,
      2,
      selectionStroke,
      selectionHalo,
    );
    placeBadge(crop);
    // R2:取字三态画在最上层(已选 > 当前命中 > 搜索命中)。
    paintOcr();
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
      // R2/R4:新会话先清掉上一帧的取字全文、二维码结果与选择。
      ocrModel?.reset();
      qrModel?.reset();
      fitCanvas();
      root.classList.toggle("mode-window", frame.mode === "window");
      root.classList.toggle("mode-region", frame.mode !== "window");
      // R24:能力子集缺失(旧后端)视为可用;关闭后不出现标注入口,
      // 区域确认保持原有的松开即完成行为。
      inlineEnabled = frame.capabilities?.inlineAnnotation !== false;
      selection = frame.fixed
        ? { x: 0, y: 0, width: frame.width, height: frame.height }
        : null;
      hoverId = null;
      dragging = false;
      // R7:上一会话的命中栈/高亮/点击吸附状态不跨会话。
      clearSnap();
      clearBadge();
      pointer = null;
      setFinishing(false);
      // 旧帧位图先摘除,避免重置标注会话触发的重绘读到未加载的新图。
      image = null;
      annotationBase = null;
      resetAnnotationSession();
      const reduced = frame.reducedCapabilities === true;
      capabilityToggle.hidden = !reduced;
      hideNotice();
      setCapabilityPanel(false);
      if (reduced) {
        renderCapabilityPanel();
      }
      renderHint();
      void getCurrentWindow().setFocus();
      document.body.tabIndex = -1;
      document.body.focus();
      // 工作区(fixed)不再列窗:窗口已定,列表没有可选对象。
      list.hidden = frame.mode !== "window" || frame.fixed === true;
      if (frame.mode === "window" && frame.fixed !== true) {
        renderWindowList(list, frame.windows, hoverId, (id) => {
          void finishWindow(id);
        });
      }
      image = new Image();
      image.onload = () => scheduleDraw();
      image.src = `data:image/jpeg;base64,${frame.pngBase64}`;
      // 跨会话样式(R8):每次会话重读设置页保存的颜色/线宽/字号/起始序号;
      // 会话已重置,不存在覆盖本次编辑的问题。
      if (frame.fixed) {
        phase = "annotate";
        const hosted = frame.capabilities?.workspaceActions !== false;
        if (!hosted) {
          actionsEl.hidden = true;
          toolsEl.hidden = true;
          showNotice(t("overlay.notice.workspace_unavailable"));
          void invoke("fallback_workspace_preview", { annotations: frame.annotations ?? [] });
        } else {
          showWorkspaceActions(frame);
          toolsEl.hidden = !inlineEnabled;
          root.classList.toggle("has-tools", inlineEnabled);
          editor?.setAnnotations(frame.annotations ?? []);
          // 壳上的取字动作:工作区打开后自动进入取字,不写剪贴板。
          if (frame.pendingOcr) {
            activateWorkspaceOcr();
          }
          // 壳上的识别二维码动作:工作区打开后自动开始识别,不写剪贴板。
          if (frame.pendingQr) {
            activateWorkspaceQr();
          }
        }
      }
      // R9:标注工具去门控常开,这里只恢复跨会话样式。
      void loadAnnotationDefaults().then((style) => {
        editor?.setStyle(style);
      });
    } catch (error) {
      // 无进行中的会话(cancelled)是预创建/隐藏时的正常路径,静默返回。
      if (isCancelledError(error)) {
        return;
      }
      showFailure(invokeError(error, t("overlay.error.capture_failed")), () => {
        void load();
      });
    }
  };

  const finishRegion = async (): Promise<void> => {
    if (finishing || !frame || frame.mode !== "region") {
      return;
    }
    const crop = roundedRect();
    if (!crop || crop.width < 2 || crop.height < 2) {
      // 不静默吞掉确认:给出下次能成功的具体做法(R13)。单击未拖动得到的
      // 0×0 选区同样按"太小"提示,而不是无反馈。
      showNotice(t(selection ? "overlay.notice.too_small" : "overlay.notice.select_first"));
      return;
    }
    editor?.commitText();
    const annotations = editor?.exportList() ?? [];
    setFinishing(true);
    try {
      await invoke("confirm_region", {
        x: crop.x,
        y: crop.y,
        width: crop.width,
        height: crop.height,
        annotations,
      });
    } catch (error) {
      setFinishing(false);
      showFailure(invokeError(error, t("overlay.error.capture_failed")), () => {
        void finishRegion();
      });
    }
  };

  const finishWindow = async (windowId: string): Promise<void> => {
    if (finishing) {
      return;
    }
    setFinishing(true);
    try {
      await invoke("confirm_window", { windowId });
    } catch (error) {
      setFinishing(false);
      showFailure(invokeError(error, t("overlay.error.capture_failed")), () => {
        void finishWindow(windowId);
      });
    }
  };

  const showWorkspaceActions = (current: OverlayFrame): void => {
    actionsEl.hidden = false;
    const caps = current.capabilities;
    const button = (name: string): HTMLButtonElement | null => {
      const found = actionsEl.querySelector(`[data-workspace=${name}]`);
      return found instanceof HTMLButtonElement ? found : null;
    };
    const ocr = button("ocr");
    const qr = button("qr");
    const pin = button("pin");
    const save = button("save");
    const copy = button("copy");
    if (ocr) {
      ocr.hidden = caps?.ocr === false;
    }
    if (qr) {
      qr.hidden = caps?.qr === false;
    }
    if (pin) {
      pin.hidden = caps?.pin === false;
    }
    if (save) {
      save.hidden = caps?.save === false;
    }
    if (copy) {
      copy.hidden = caps?.copy === false;
    }
  };

  const currentAnnotations = (): Annotation[] => editor?.exportList() ?? [];

  // R2/R4:取字或二维码识别激活期间隐藏工作区动作与标注工具条,退出后恢复;
  // 提示随状态切换。两种识别模型互斥激活,共用同一份 chrome 状态。
  let ocrWasActive = false;
  let qrWasActive = false;
  let recognitionNoticeActive = false;
  const syncRecognitionChrome = (): void => {
    const ocrActive = ocrModel?.active === true;
    const qrActive = qrModel?.active === true;
    const active = ocrActive || qrActive;
    if (frame?.fixed && frame.capabilities?.workspaceActions !== false) {
      actionsEl.hidden = active;
      toolsEl.hidden = active || !inlineEnabled;
      root.classList.toggle("has-tools", !active && inlineEnabled);
    }
    if (!active && (ocrWasActive || qrWasActive)) {
      // 退出识别:恢复标注工具条高亮并撤掉识别提示。
      editor?.setTool(editor.tool());
      if (recognitionNoticeActive) {
        recognitionNoticeActive = false;
        hideNotice();
      }
    }
    ocrWasActive = ocrActive;
    qrWasActive = qrActive;
    renderHint();
    placeResultPanels();
    scheduleDraw();
  };

  const activateWorkspaceOcr = (): void => {
    if (finishing || ocrModel?.active) {
      return;
    }
    editor?.commitText();
    editor?.deactivateTool();
    editor?.clearSelection();
    qrModel?.deactivate();
    setFinishing(true);
    void invoke("preview_workspace_ocr", { annotations: currentAnnotations() }).catch((error) => {
      setFinishing(false);
      showNotice(invokeError(error, t("overlay.error.capture_failed")));
    });
  };

  const activateWorkspaceQr = (): void => {
    if (finishing || qrModel?.active) {
      return;
    }
    // 二维码不留在全屏冻结层。按图片大小打开预览,预览里自动开始识别。
    editor?.commitText();
    editor?.deactivateTool();
    editor?.clearSelection();
    ocrModel?.deactivate();
    setFinishing(true);
    void invoke("preview_workspace_qr", { annotations: currentAnnotations() }).catch((error) => {
      setFinishing(false);
      showNotice(invokeError(error, t("overlay.error.capture_failed")));
    });
  };

  ocrModel = mountOcrModel({
    host: root,
    closable: false,
    notice: (message, kind) => {
      recognitionNoticeActive = true;
      showNotice(message, kind === "progress" || kind === "hint");
    },
    onChange: syncRecognitionChrome,
  });

  qrModel = mountQrModel({
    host: root,
    closable: false,
    notice: (message, kind) => {
      recognitionNoticeActive = true;
      showNotice(message, kind === "progress" || kind === "hint");
    },
    onChange: syncRecognitionChrome,
  });

  const afterExport = async (kind: string, name?: string): Promise<void> => {
    await invoke("complete_workspace", { kind, name: name ?? null });
  };

  const copyWorkspace = async (): Promise<void> => {
    if (finishing) {
      return;
    }
    editor?.commitText();
    setFinishing(true);
    try {
      await invoke("copy_preview_png", { annotations: currentAnnotations() });
      await afterExport("copy");
    } catch (error) {
      setFinishing(false);
      showNotice(invokeError(error, t("preview.error.copy_fallback")));
    }
  };

  const saveWorkspace = async (): Promise<void> => {
    if (finishing) {
      return;
    }
    editor?.commitText();
    setFinishing(true);
    let yielded = false;
    try {
      // 对话框挂在隐藏的预览窗上。先摘掉浮层置顶并让预览窗取得焦点,
      // 否则对话框停在全屏浮层后面,saving 标志一直为真。
      await invoke("prepare_workspace_save_dialog");
      yielded = true;
      const result = await invoke<{ saved: boolean; path?: string | null }>("save_preview_png", {
        annotations: currentAnnotations(),
      });
      if (!result.saved) {
        await invoke("restore_workspace_after_save_dialog");
        yielded = false;
        setFinishing(false);
        showNotice(t("overlay.notice.save_cancelled"));
        return;
      }
      const name = fileNameFromPath(result.path) ?? "cropmark";
      await afterExport("save", name);
    } catch (error) {
      if (yielded) {
        try {
          await invoke("restore_workspace_after_save_dialog");
        } catch {
          // 工作区恢复失败不能再静默:用户需要知道浮层状态可能异常。
          setFinishing(false);
          showNotice(t("overlay.notice.restore_failed"));
          return;
        }
      }
      setFinishing(false);
      showNotice(invokeError(error, t("overlay.error.save_fallback")));
    }
  };

  const pinWorkspace = async (): Promise<void> => {
    if (finishing) {
      return;
    }
    editor?.commitText();
    setFinishing(true);
    try {
      await invoke("pin_current", { annotations: currentAnnotations() });
      await afterExport("pin");
    } catch (error) {
      setFinishing(false);
      showNotice(invokeError(error, t("preview.error.pin_fallback")));
    }
  };

  const editFurther = async (): Promise<void> => {
    if (finishing) {
      return;
    }
    editor?.commitText();
    setFinishing(true);
    try {
      await invoke("edit_workspace_further", { annotations: currentAnnotations() });
    } catch (error) {
      setFinishing(false);
      showNotice(invokeError(error, t("overlay.error.capture_failed")));
    }
  };

  canvas.addEventListener("mousedown", (event) => {
    if (!frame) {
      return;
    }
    // R2:取字激活时画布输入全部交给共享取字模型(点选/划选)。
    if (ocrModel?.active === true) {
      if (event.button !== 0) {
        return;
      }
      event.preventDefault();
      ocrModel.pointerDown(physicalPoint(event));
      return;
    }
    // R4:二维码识别中画布不接收输入(结果面板只读,复制走面板按钮)。
    if (qrModel?.active === true) {
      return;
    }
    if (frame.fixed || frame.mode !== "region" || event.button !== 0) {
      return;
    }
    if (annotationActive()) {
      const crop = roundedRect();
      const point = physicalPoint(event);
      if (
        crop &&
        point.x >= crop.x &&
        point.x <= crop.x + crop.width &&
        point.y >= crop.y &&
        point.y <= crop.y + crop.height
      ) {
        // 选区内部交给共享标注层处理(绘制/选中/文字)。
        return;
      }
      // 选区外按下=重新拖选:废弃旧选区与标注,回到选择阶段。
      resetAnnotationSession();
    }
    const point = physicalPoint(event);
    // R7:悬停高亮时点击确认该高亮区域(松开前未拖动即采纳;越过阈值转为
    // 自由框选,见 mousemove)。
    if (snapRect && snapHoverEnabled()) {
      snapClickPending = true;
      snapPressPoint = point;
      selection = { ...snapRect };
      hideNotice();
      scheduleDraw();
      return;
    }
    // 自由拖选优先:清掉悬停高亮与点击吸附状态。
    clearSnap();
    dragging = true;
    startX = point.x;
    startY = point.y;
    selection = { x: point.x, y: point.y, width: 0, height: 0 };
    hideNotice();
    scheduleDraw();
  });

  window.addEventListener("mousemove", (event) => {
    const host = root.getBoundingClientRect();
    pointer = { x: event.clientX - host.left, y: event.clientY - host.top };
    if (badgeTarget && !dragging) {
      layoutBadge();
    }
    if (!frame) {
      return;
    }
    const point = physicalPoint(event);
    if (ocrModel?.active === true) {
      ocrModel.pointerMove(point);
      return;
    }
    if (qrModel?.active === true) {
      return;
    }
    // R7:悬停命中栈(窗口模式恒有;区域模式需窗口级吸附能力,无能力时
    // 只清掉残留高亮)。
    if (!dragging && !snapClickPending && updateSnapHover(point.x, point.y)) {
      scheduleDraw();
    }
    if (frame.mode === "window") {
      return;
    }
    // R7:点击吸附中越过阈值 → 从按压点开始自由框选。
    if (snapClickPending && snapPressPoint) {
      const moved =
        Math.abs(point.x - snapPressPoint.x) > SNAP_DRAG_THRESHOLD ||
        Math.abs(point.y - snapPressPoint.y) > SNAP_DRAG_THRESHOLD;
      if (moved) {
        const anchor = snapPressPoint;
        clearSnap();
        dragging = true;
        startX = anchor.x;
        startY = anchor.y;
        selection = { x: anchor.x, y: anchor.y, width: 0, height: 0 };
      } else {
        return;
      }
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
    if (ocrModel?.active === true) {
      ocrModel.pointerUp();
      return;
    }
    if (qrModel?.active === true) {
      return;
    }
    // R7:点击吸附——选区已由高亮矩形填充,松开即确认。
    if (snapClickPending) {
      clearSnap();
      dragging = false;
      void finishRegion();
      return;
    }
    if (!dragging || frame?.fixed) {
      dragging = false;
      return;
    }
    dragging = false;
    void finishRegion();
  });

  // R7:滚轮在命中栈层级间切换高亮;只监听画布(面板/列表上的滚动不被劫持),
  // 栈内无更多层级时不消费事件。
  canvas.addEventListener(
    "wheel",
    (event) => {
      if (!frame || frame.fixed || dragging || snapClickPending) {
        return;
      }
      if (ocrModel?.active === true || qrModel?.active === true) {
        return;
      }
      if (event.deltaY !== 0 && cycleSnap(event.deltaY)) {
        event.preventDefault();
      }
    },
    { passive: false },
  );

  canvas.addEventListener("click", (event) => {
    if (
      !frame ||
      frame.fixed ||
      frame.mode !== "window" ||
      ocrModel?.active === true ||
      qrModel?.active === true
    ) {
      return;
    }
    const point = physicalPoint(event);
    // R7:确认当前高亮窗口(滚轮可能已切到更底层重叠窗口)。
    const id = hoverId ?? hitWindow(frame, point.x, point.y);
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
  // 标注阶段右键交给共享标注层(命中图元时给出删除菜单)。
  canvas.addEventListener("contextmenu", (event) => {
    if (!frame || ocrModel?.active === true || qrModel?.active === true) {
      if (ocrModel?.active === true || qrModel?.active === true) {
        event.preventDefault();
      }
      return;
    }
    if (annotationActive()) {
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
    const opening = capabilityPanel.hidden;
    setCapabilityPanel(opening, { focusPanel: opening, restoreFocus: !opening });
  });

  let capabilityPanelHadFocusOnPointerDown = false;

  // 外部 click 到来前焦点可能已经由 mousedown 默认行为移出面板；在捕获阶段
  // 记录来源，点击 canvas 等不可聚焦区域关闭时仍归还到能力说明开关。
  document.addEventListener(
    "pointerdown",
    (event) => {
      const target = event.target;
      capabilityPanelHadFocusOnPointerDown =
        !capabilityPanel.hidden &&
        capabilityPanel.contains(document.activeElement) &&
        target instanceof Node &&
        !capabilityPanel.contains(target) &&
        !capabilityToggle.contains(target);
    },
    { capture: true },
  );

  document.addEventListener("click", (event) => {
    if (
      !capabilityPanel.hidden &&
      event.target instanceof Node &&
      !capabilityPanel.contains(event.target) &&
      !capabilityToggle.contains(event.target)
    ) {
      setCapabilityPanel(false, { restoreFocus: capabilityPanelHadFocusOnPointerDown });
    }
    capabilityPanelHadFocusOnPointerDown = false;
  });

  capabilityPanel.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    setCapabilityPanel(false, { restoreFocus: true });
  });

  cancelBtn.addEventListener("click", (event) => {
    event.preventDefault();
    event.stopPropagation();
    cancel();
  });

  retryBtn.addEventListener("click", (event) => {
    event.preventDefault();
    event.stopPropagation();
    const action = retryAction;
    retryBtn.hidden = true;
    retryAction = null;
    action?.();
  });

  actionsEl.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element)) {
      return;
    }
    const button = target.closest("[data-workspace]");
    if (!(button instanceof HTMLButtonElement) || button.hidden) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const action = button.dataset.workspace;
    if (action === "copy") {
      void copyWorkspace();
    } else if (action === "save") {
      void saveWorkspace();
    } else if (action === "pin") {
      void pinWorkspace();
    } else if (action === "ocr") {
      activateWorkspaceOcr();
    } else if (action === "qr") {
      activateWorkspaceQr();
    } else if (action === "edit") {
      void editFurther();
    }
  });

  // R21:内嵌与预览同源的标注层;坐标映射到冻帧物理像素,图元由 Rust
  // 裁剪平移后 rasterize,复制/保存输出与所见一致。
  editor = mountAnnotationEditor({
    root,
    canvas,
    ctx,
    toolbar: toolsEl,
    textHost: root,
    frame: () => (frame ? { width: frame.width, height: frame.height, scale: frame.scale } : null),
    redraw: () => scheduleDraw(),
    // R2/R4:取字或二维码识别激活时画布输入只给识别层,标注编辑暂时禁用。
    isEditable: () =>
      annotationActive() && ocrModel?.active !== true && qrModel?.active !== true,
    // 标注阶段右键未命中图元:与 R13 一致地说明操作条缺失与替代路径,
    // 而不是静默无响应。
    onContextMenuMiss: () => {
      if (frame?.reducedCapabilities) {
        showNotice(t(TOOLBAR_NOTICE_KEY));
      }
    },
    onError: (error) => {
      showNotice(typeof error === "string" ? error : t(error.key, error.params));
    },
  });

  // 标注层已在缺失能力说明里给出替代路径(R13);键盘只处理选区确认/取消与
  // 取字/二维码识别(Esc 退出、Ctrl+A 全选、Ctrl+C 复制所选),标注层先消费其它按键。
  window.addEventListener("keydown", (event) => {
    if (event.defaultPrevented || editor?.isTextEditing()) {
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      if (!capabilityPanel.hidden) {
        setCapabilityPanel(false, { restoreFocus: true });
        return;
      }
      // R2:取字中 Esc 先退出取字(恢复工作区动作),再按一次才取消会话。
      if (ocrModel?.deactivate()) {
        return;
      }
      // R4:二维码识别中 Esc 同样先退出识别面板。
      if (qrModel?.deactivate()) {
        return;
      }
      cancel();
      return;
    }
    if (
      document.activeElement instanceof HTMLTextAreaElement ||
      document.activeElement instanceof HTMLInputElement
    ) {
      return;
    }
    // R2:取字中的 Ctrl+A 全选识别文本,Ctrl+C 复制所选(搜索框内走原生)。
    // R4:二维码识别中的 Ctrl+C 复制首条内容。
    if (event.ctrlKey || event.metaKey) {
      const key = event.key.toLowerCase();
      if (ocrModel?.active === true) {
        if (key === "a") {
          event.preventDefault();
          ocrModel.selectAll();
          return;
        }
        if (key === "c") {
          event.preventDefault();
          void ocrModel.copySelected();
          return;
        }
      }
      if (qrModel?.active === true && key === "c") {
        event.preventDefault();
        void qrModel.copy(0);
        return;
      }
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      // Enter 等同鼠标确认路径:区域模式确认当前选区(与松开确认同一
      // finishRegion,无选区/选区太小会得到提示),窗口模式确认悬停窗口;
      // fixed 工作区由动作按钮完成导出,Enter 无动作。
      if (!frame || frame.fixed) {
        return;
      }
      if (frame.mode === "window") {
        if (hoverId) {
          void finishWindow(hoverId);
        }
        return;
      }
      void finishRegion();
      return;
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
  });

  // 预创建浮层先被停放,窗口尺寸在会话开始后才落定:尺寸变化后按新几何重排。
  window.addEventListener("resize", () => {
    scheduleDraw();
  });

  void listen("overlay-reload", () => {
    void load();
  });
  void load();

  // 语言切换:提示条、能力面板、取字/二维码面板与开关文案即时更新;静态标签由 main 应用。
  return () => {
    renderHint();
    editor?.refreshLabels();
    ocrModel?.refreshLabels();
    qrModel?.refreshLabels();
    placeResultPanels();
    setCapabilityPanel(!capabilityPanel.hidden);
    if (!capabilityPanel.hidden) {
      renderCapabilityPanel();
    }
    // 窗口列表空态与尺寸文案随语言重渲染。
    if (frame?.mode === "window") {
      renderWindowList(list, frame.windows, hoverId, (id) => {
        void finishWindow(id);
      });
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
  // 无可列窗口时给出空态,而不是留下一块空白列表。
  if (windows.length === 0) {
    const empty = document.createElement("div");
    empty.className = "window-empty";
    empty.textContent = t("overlay.window_list.empty");
    root.append(empty);
    return;
  }
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
      meta.textContent = t("overlay.size_format", { width: item.width, height: item.height });
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

function fileNameFromPath(path: string | null | undefined): string | null {
  if (!path) {
    return null;
  }
  const parts = path.split(/[/\\]/);
  return parts[parts.length - 1] || null;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function intersectionArea(a: Selection, b: Selection): number {
  const width = Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x);
  const height = Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y);
  if (width <= 0 || height <= 0) {
    return 0;
  }
  return width * height;
}

/// 与原生 `place_size_badge` 同一规则:四角外侧离光标最远且整块在画面内。
/// 平局优先左上。四角都放不下时,把最远角的候选钳进画面。
function placeSizeBadge(
  target: Selection,
  cursor: { x: number; y: number },
  screenWidth: number,
  screenHeight: number,
  width: number,
  height: number,
  margin: number,
): Selection {
  const right = target.x + target.width;
  const bottom = target.y + target.height;
  const anchors: Array<[{ x: number; y: number }, Selection]> = [
    [
      { x: target.x, y: target.y },
      { x: target.x, y: target.y - height - margin, width, height },
    ],
    [
      { x: right, y: target.y },
      { x: right - width, y: target.y - height - margin, width, height },
    ],
    [
      { x: target.x, y: bottom },
      { x: target.x, y: bottom + margin, width, height },
    ],
    [
      { x: right, y: bottom },
      { x: right - width, y: bottom + margin, width, height },
    ],
  ];
  const distance = (point: { x: number; y: number }): number => {
    const dx = point.x - cursor.x;
    const dy = point.y - cursor.y;
    return dx * dx + dy * dy;
  };
  const inside = (panel: Selection): boolean =>
    panel.width > 0 &&
    panel.height > 0 &&
    panel.x >= 0 &&
    panel.y >= 0 &&
    panel.x + panel.width <= screenWidth &&
    panel.y + panel.height <= screenHeight;
  const contains = (panel: Selection, point: { x: number; y: number }): boolean =>
    point.x >= panel.x &&
    point.y >= panel.y &&
    point.x < panel.x + panel.width &&
    point.y < panel.y + panel.height;
  const pick = (avoidCursor: boolean): Selection | null => {
    let bestDist = -1;
    let bestRank = anchors.length;
    let bestPanel: Selection | null = null;
    for (let rank = 0; rank < anchors.length; rank += 1) {
      const [point, panel] = anchors[rank];
      if (!inside(panel) || (avoidCursor && contains(panel, cursor))) {
        continue;
      }
      const dist = distance(point);
      if (!bestPanel || dist > bestDist || (dist === bestDist && rank < bestRank)) {
        bestDist = dist;
        bestRank = rank;
        bestPanel = panel;
      }
    }
    return bestPanel;
  };
  const chosen = pick(true) ?? pick(false);
  if (chosen) {
    return chosen;
  }
  let farthest = 0;
  let bestDist = -1;
  for (let rank = 0; rank < anchors.length; rank += 1) {
    const dist = distance(anchors[rank][0]);
    if (dist > bestDist) {
      bestDist = dist;
      farthest = rank;
    }
  }
  return clampBadge(anchors[farthest][1], screenWidth, screenHeight);
}

function clampBadge(panel: Selection, screenWidth: number, screenHeight: number): Selection {
  if (screenWidth <= 0 || screenHeight <= 0) {
    return { x: 0, y: 0, width: 0, height: 0 };
  }
  const width = Math.max(0, Math.min(panel.width, screenWidth));
  const height = Math.max(0, Math.min(panel.height, screenHeight));
  return {
    x: clamp(panel.x, 0, screenWidth - width),
    y: clamp(panel.y, 0, screenHeight - height),
    width,
    height,
  };
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
