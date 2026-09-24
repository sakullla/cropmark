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
  fixed?: boolean;
  annotations?: Annotation[];
  pendingOcr?: boolean;
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

const FALLBACK_SELECTION = "#0e6d66";
const FALLBACK_SELECTION_HALO = "#ffffff";
const FALLBACK_IDLE_STROKE = "#1c1917";

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
      <div class="overlay-actions" hidden>
        <button type="button" data-workspace="ocr" data-i18n="overlay.action.ocr">取字</button>
        <button type="button" data-workspace="pin" data-i18n="overlay.action.pin">贴图</button>
        <button type="button" data-workspace="save" data-i18n="overlay.action.save">保存</button>
        <button type="button" data-workspace="copy" data-i18n="overlay.action.copy">复制</button>
        <button type="button" data-workspace="edit" data-i18n="overlay.action.edit">进一步编辑</button>
      </div>
      <button type="button" class="overlay-capabilities" aria-expanded="false" data-i18n="overlay.capabilities" hidden>能力说明</button>
      <button type="button" class="overlay-cancel" data-i18n="overlay.cancel">取消 Esc</button>
    </div>
    <div class="overlay-tools annotation-tools" role="toolbar" data-i18n-aria-label="preview.toolbar_group" aria-label="标注" hidden></div>
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
  const actionsEl = root.querySelector(".overlay-actions");
  const toolsEl = root.querySelector(".overlay-tools");
  const capabilityToggle = root.querySelector(".overlay-capabilities");
  const capabilityPanel = root.querySelector(".capability-panel");
  const notice = root.querySelector(".overlay-notice");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(hint instanceof HTMLElement) ||
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
  /// R21:即时标注会话阶段;"select" 拖选区,"annotate" 选区固定后可标注。
  let phase: "select" | "annotate" = "select";
  /// R24:关闭 inlineAnnotation 后覆盖层不提供标注层,保持原有的松开即完成。
  let inlineEnabled = false;
  let editor: AnnotationEditor | null = null;
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
    return { x: left, y: top, width: right - left, height: bottom - top };
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
    // R21:能力说明随 inlineAnnotation 开关给出标注可用性与替代路径。
    available.textContent = `${t("overlay.panel.available")} ${
      inlineEnabled
        ? t("overlay.panel.annotate_available")
        : t("overlay.panel.annotate_unavailable")
    }`;
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
    if (frame.fixed) {
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
    if (!image || !frame || image.naturalWidth === 0) {
      return;
    }
    fitCanvas();
    ctx.drawImage(image, 0, 0, canvas.width, canvas.height);
    ctx.fillStyle = "rgba(12, 10, 9, 0.48)";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    const selectionStroke = canvasToken(root, "--selection-stroke", FALLBACK_SELECTION);
    const selectionHalo = canvasToken(root, "--selection-halo", FALLBACK_SELECTION_HALO);
    const idleStroke = canvasToken(root, "--ink", FALLBACK_IDLE_STROKE);
    if (frame.mode === "window") {
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
      // R24:能力子集缺失(旧后端)视为可用;关闭后不出现标注入口,
      // 区域确认保持原有的松开即完成行为。
      inlineEnabled = frame.capabilities?.inlineAnnotation !== false;
      selection = frame.fixed
        ? { x: 0, y: 0, width: frame.width, height: frame.height }
        : null;
      hoverId = null;
      dragging = false;
      finishing = false;
      // 旧帧位图先摘除,避免重置标注会话触发的重绘读到未加载的新图。
      image = null;
      annotationBase = null;
      resetAnnotationSession();
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
          if (frame.pendingOcr) {
            void runOcr();
          }
        }
      }
      void loadAnnotationDefaults().then((style) => {
        editor?.setStyle(style);
      });
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
    editor?.commitText();
    const annotations = editor?.exportList() ?? [];
    finishing = true;
    try {
      await invoke("confirm_region", {
        x: crop.x,
        y: crop.y,
        width: crop.width,
        height: crop.height,
        annotations,
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

  const showWorkspaceActions = (current: OverlayFrame): void => {
    actionsEl.hidden = false;
    const caps = current.capabilities;
    const button = (name: string): HTMLButtonElement | null => {
      const found = actionsEl.querySelector(`[data-workspace=${name}]`);
      return found instanceof HTMLButtonElement ? found : null;
    };
    const ocr = button("ocr");
    const pin = button("pin");
    const save = button("save");
    const copy = button("copy");
    if (ocr) {
      ocr.hidden = caps?.ocr === false;
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

  const runOcr = async (): Promise<void> => {
    if (finishing) {
      return;
    }
    finishing = true;
    showNotice(t("toast.ocr_progress"));
    try {
      await invoke("recognize_preview");
      const text = await invoke<string>("copy_ocr_all");
      const chars = Array.from(text).length;
      showNotice(t("toast.ocr_copied", { chars: String(chars) }));
    } catch (error) {
      showNotice(invokeError(error, t("preview.error.ocr_fallback")));
    } finally {
      finishing = false;
    }
  };

  const afterExport = async (kind: string, name?: string): Promise<void> => {
    await invoke("complete_workspace", { kind, name: name ?? null });
  };

  const copyWorkspace = async (): Promise<void> => {
    if (finishing) {
      return;
    }
    editor?.commitText();
    finishing = true;
    try {
      await invoke("copy_preview_png", { annotations: currentAnnotations() });
      await afterExport("copy");
    } catch (error) {
      finishing = false;
      showNotice(invokeError(error, t("preview.error.copy_fallback")));
    }
  };

  const saveWorkspace = async (): Promise<void> => {
    if (finishing) {
      return;
    }
    editor?.commitText();
    finishing = true;
    try {
      const result = await invoke<{ saved: boolean; path?: string | null }>("save_preview_png", {
        annotations: currentAnnotations(),
      });
      if (!result.saved) {
        finishing = false;
        return;
      }
      const name = fileNameFromPath(result.path) ?? "cropmark";
      await afterExport("save", name);
    } catch (error) {
      finishing = false;
      showNotice(invokeError(error, t("preview.error.save_fallback")));
    }
  };

  const pinWorkspace = async (): Promise<void> => {
    if (finishing) {
      return;
    }
    editor?.commitText();
    finishing = true;
    try {
      await invoke("pin_current", { annotations: currentAnnotations() });
      await afterExport("pin");
    } catch (error) {
      finishing = false;
      showNotice(invokeError(error, t("preview.error.pin_fallback")));
    }
  };

  const editFurther = async (): Promise<void> => {
    if (finishing) {
      return;
    }
    editor?.commitText();
    finishing = true;
    try {
      await invoke("edit_workspace_further", { annotations: currentAnnotations() });
    } catch (error) {
      finishing = false;
      showNotice(invokeError(error, t("overlay.error.capture_failed")));
    }
  };

  canvas.addEventListener("mousedown", (event) => {
    if (!frame || frame.fixed || frame.mode !== "region" || event.button !== 0) {
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
    if (!dragging || frame?.fixed) {
      dragging = false;
      return;
    }
    dragging = false;
    void finishRegion();
  });

  canvas.addEventListener("click", (event) => {
    if (!frame || frame.fixed || frame.mode !== "window") {
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
  // 标注阶段右键交给共享标注层(命中图元时给出删除菜单)。
  canvas.addEventListener("contextmenu", (event) => {
    if (!frame || annotationActive()) {
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
      void runOcr();
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
    isEditable: () => annotationActive(),
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

  // 标注层已在缺失能力说明里给出替代路径(R13);键盘只处理选区确认/取消,
  // 标注层先消费 Escape/工具/撤销等按键,未消费时才回到这里。
  window.addEventListener("keydown", (event) => {
    if (event.defaultPrevented || editor?.isTextEditing()) {
      return;
    }
    if (
      document.activeElement instanceof HTMLTextAreaElement ||
      document.activeElement instanceof HTMLInputElement
    ) {
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      cancel();
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
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

  void listen("overlay-reload", () => {
    void load();
  });
  void load();

  // 语言切换:提示条、能力面板与开关文案即时更新;静态标签由 main 应用。
  return () => {
    renderHint();
    editor?.refreshLabels();
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
