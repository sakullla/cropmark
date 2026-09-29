import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  annotationToolForKey,
  clampFloatingPanel,
  PRIMARY_TOOLS,
  isAnnotationTool,
  mountAnnotationEditor,
  readAnnotationDefaults,
  resolveCanvasColor,
  type Annotation,
  type AnnotationEditor,
  type AnnotationTool,
} from "../annotation";
import { applyTranslations, t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import { mountOcrModel, type OcrDocument, type OcrModel } from "../ocr";
import { REGION_TOOL_FIELDS, type RegionTools } from "../settings";
import { mountQrModel, type QrModel } from "../qr";
import "./preview.css";

interface PreviewFrame {
  width: number;
  height: number;
  scale: number;
}

/** R6:旋转/裁剪命令返回的重基结果;坐标已由 Rust 按同一变换重映射。 */
interface PreviewTransformResult {
  width: number;
  height: number;
  annotations: Annotation[];
  ocr: OcrDocument | null;
  canUndo: boolean;
  canRedo: boolean;
}

/** R6:裁剪最小边长(物理像素),与 Rust `session::MIN_CROP_EDGE` 一致。 */
const MIN_CROP_EDGE = 8;

/** 设置还没读到时先不放工具，避免关闭项在挂载瞬间闪一下。 */
const PREVIEW_TOOL_DEFAULTS: RegionTools = {
  arrow: true,
  rect: true,
  ellipse: true,
  highlighter: true,
  mosaic: true,
  text: true,
  number: false,
  spotlight: false,
  magnifier: false,
  bubble: false,
  sticker: false,
  erase: false,
  line: false,
  blur: false,
  pin: true,
  ocr: true,
  qr: false,
};
/** 裁剪确认条离画面边缘的间距,与样式里的 12px 悬浮一致。 */
const CROP_BAR_INSET = 12;
const FALLBACK_CROP_EDGE = "#0ea5e9";

type NoteKind = "success" | "feedback" | "error";

// R3 导出:格式由保存对话框返回的扩展名决定,前端只负责质量档位;
// 默认 PNG(后端 lastFormat 默认 png),质量档位仅 JPEG/WebP 生效。
type ExportFormat = "png" | "jpeg" | "webp";
type ExportQuality = "high" | "medium" | "low";

const SAVE_QUALITIES: Array<{
  value: ExportQuality;
  labelKey: CatalogKey;
  titleKey: CatalogKey;
}> = [
  { value: "high", labelKey: "preview.quality.high", titleKey: "preview.quality.high_title" },
  { value: "medium", labelKey: "preview.quality.medium", titleKey: "preview.quality.medium_title" },
  { value: "low", labelKey: "preview.quality.low", titleKey: "preview.quality.low_title" },
];

export function mountPreview(root: HTMLElement): () => void {
  root.className = "preview-root";
  root.dataset.tool = "arrow";
  root.innerHTML = `
    <header class="preview-titlebar">
      <div class="brand" data-drag-handle data-tauri-drag-region>
        <span class="mark" aria-hidden="true"></span>
        <span class="name">Cropmark</span>
      </div>
      <p class="preview-note" data-drag-handle data-tauri-drag-region>${t("preview.copied_clean")}</p>
      <button type="button" class="preview-close icon-btn" data-action="close" data-i18n-aria-label="preview.close" aria-label="关闭">${icons.close}</button>
    </header>
    <div class="preview-toolbar" data-preview-toolbar>
      <div class="preview-toolbar-row" data-preview-row>
        <div class="preview-tools" role="toolbar" data-annotation-toolbar data-preview-group="draw" data-i18n-aria-label="preview.toolbar_group" aria-label="标注" data-tauri-drag-region="false"></div>
        <span class="preview-group-sep" data-preview-sep="draw" aria-hidden="true"></span>
        <div class="preview-action-group" data-preview-group="picture" data-tauri-drag-region="false">
          <button type="button" class="preview-named" data-tool="ocr" hidden data-i18n-title="preview.action.ocr_title" data-i18n-aria-label="preview.action.ocr" aria-label="取字" data-tauri-drag-region="false">${icons.ocr}<span class="tool-label" data-i18n="preview.action.ocr">取字</span></button>
          <button type="button" class="preview-named" data-tool="qr" hidden data-i18n-title="preview.action.qr_title" data-i18n-aria-label="preview.action.qr" aria-label="识别二维码" data-tauri-drag-region="false">${icons.qr}<span class="tool-label" data-i18n="preview.action.qr">识别二维码</span></button>
          <button type="button" class="preview-named" data-action="rotate-left" data-i18n-title="preview.action.rotate_left_title" data-i18n-aria-label="preview.action.rotate_left" aria-label="左旋 90°" data-tauri-drag-region="false">${icons.rotateLeft}<span class="tool-label" data-i18n="preview.action.rotate_left">左旋 90°</span></button>
          <button type="button" class="preview-named" data-action="rotate-right" data-i18n-title="preview.action.rotate_right_title" data-i18n-aria-label="preview.action.rotate_right" aria-label="右旋 90°" data-tauri-drag-region="false">${icons.rotateRight}<span class="tool-label" data-i18n="preview.action.rotate_right">右旋 90°</span></button>
          <button type="button" class="preview-named" data-action="crop" data-i18n-title="preview.action.crop_title" data-i18n-aria-label="preview.action.crop" aria-label="裁剪" data-tauri-drag-region="false">${icons.crop}<span class="tool-label" data-i18n="preview.action.crop">裁剪</span></button>
          <button type="button" class="preview-named" data-action="copy-ocr-all" hidden data-i18n-title="preview.action.copy_all" data-i18n-aria-label="preview.action.copy_all" aria-label="复制全部" data-tauri-drag-region="false">${icons.copy}<span class="tool-label" data-i18n="preview.action.copy_all">复制全部</span></button>
        </div>
        <span class="preview-group-sep" data-preview-sep="picture" aria-hidden="true"></span>
        <div class="preview-action-group" data-preview-group="output" data-tauri-drag-region="false">
          <button type="button" class="preview-named" data-action="pin" data-i18n-title="preview.action.pin_title" data-i18n-aria-label="preview.action.pin" aria-label="贴图" data-tauri-drag-region="false">${icons.pin}<span class="tool-label" data-i18n="preview.action.pin">贴图</span></button>
          <button type="button" class="preview-named" data-action="update-pin" data-i18n-title="preview.action.update_pin_title" data-i18n-aria-label="preview.action.update_pin" aria-label="更新贴图" hidden data-tauri-drag-region="false">${icons.annotate}<span class="tool-label" data-i18n="preview.action.update_pin">更新贴图</span></button>
          <div class="preview-save" data-save-quality-root>
            <div class="preview-save-split">
              <button type="button" class="preview-named" data-action="save" data-i18n-title="preview.action.save_title" data-tauri-drag-region="false">${icons.save}<span class="tool-label" data-i18n="preview.action.save">保存</span></button>
              <button type="button" class="preview-save-caret preview-named" data-action="toggle-quality" data-i18n-title="preview.quality.group" data-i18n-aria-label="preview.quality.group" aria-label="保存质量" aria-haspopup="true" aria-expanded="false" data-tauri-drag-region="false">${icons.chevronDown}</button>
            </div>
            <div class="preview-quality-panel" data-save-quality-panel hidden>
              <span class="style-label" data-i18n="preview.quality.label">质量</span>
              <div class="style-options" role="group" data-i18n-aria-label="preview.quality.group" aria-label="保存质量">
                ${SAVE_QUALITIES.map(
                  ({ value, labelKey, titleKey }) =>
                    `<button type="button" data-save-quality="${value}" data-tooltip="${t(titleKey)}">${t(labelKey)}</button>`,
                ).join("")}
              </div>
            </div>
          </div>
        </div>
      </div>
      <button type="button" class="primary preview-named" data-action="copy" data-i18n-title="preview.action.copy_title" data-tauri-drag-region="false">${icons.copy}<span class="tool-label" data-i18n="preview.action.copy">复制</span></button>
      <div class="preview-overflow" data-preview-overflow hidden>
        <button type="button" class="preview-named" data-action="preview-more" data-i18n-title="preview.tool.more_title" data-i18n-aria-label="preview.tool.more" aria-label="更多" aria-haspopup="menu" aria-expanded="false" data-tauri-drag-region="false">${icons.more}<span class="tool-label" data-i18n="preview.tool.more">更多</span></button>
        <div class="preview-overflow-menu" data-preview-overflow-menu role="menu" hidden></div>
      </div>
    </div>
    <div class="preview-stage">
      <div class="preview-frame">
        <div class="preview-output" data-beautify-output>
          <canvas></canvas>
        </div>
        <div class="preview-crop-bar" data-crop-bar hidden>
          <span class="preview-crop-hint" data-i18n="preview.crop.hint">${t("preview.crop.hint")}</span>
          <button type="button" class="primary" data-crop-action="confirm" data-i18n="preview.crop.confirm">${t("preview.crop.confirm")}</button>
          <button type="button" data-crop-action="cancel" data-i18n="preview.crop.cancel">${t("preview.crop.cancel")}</button>
        </div>
      </div>
    </div>
  `;

  const canvas = root.querySelector("canvas");
  const outputEl = root.querySelector("[data-beautify-output]");
  const note = root.querySelector(".preview-note");
  const frameEl = root.querySelector(".preview-frame");
  const stageEl = root.querySelector(".preview-stage");
  const toolbarEl = root.querySelector("[data-annotation-toolbar]");
  const toolbarRow = root.querySelector("[data-preview-row]");
  const pictureGroup = root.querySelector("[data-preview-group=picture]");
  const outputGroup = root.querySelector("[data-preview-group=output]");
  const drawSep = root.querySelector("[data-preview-sep=draw]");
  const pictureSep = root.querySelector("[data-preview-sep=picture]");
  const overflowRoot = root.querySelector("[data-preview-overflow]");
  const overflowMenu = root.querySelector("[data-preview-overflow-menu]");
  const overflowToggle = root.querySelector("[data-action=preview-more]");
  const copyAllBtn = root.querySelector("[data-action=copy-ocr-all]");
  const ocrBtn = root.querySelector("[data-tool=ocr]");
  const qrBtn = root.querySelector("[data-tool=qr]");
  const rotateLeftBtn = root.querySelector("[data-action=rotate-left]");
  const rotateRightBtn = root.querySelector("[data-action=rotate-right]");
  const cropBtn = root.querySelector("[data-action=crop]");
  const cropBar = root.querySelector("[data-crop-bar]");
  const cropConfirmBtn = root.querySelector("[data-crop-action=confirm]");
  const pinBtn = root.querySelector("[data-action=pin]");
  const updatePinBtn = root.querySelector("[data-action=update-pin]");
  const saveQualityRoot = root.querySelector("[data-save-quality-root]");
  const saveQualityPanel = root.querySelector("[data-save-quality-panel]");
  const saveQualityToggle = root.querySelector("[data-action=toggle-quality]");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(outputEl instanceof HTMLElement) ||
    !(note instanceof HTMLElement) ||
    !(frameEl instanceof HTMLElement) ||
    !(stageEl instanceof HTMLElement) ||
    !(toolbarEl instanceof HTMLElement) ||
    !(toolbarRow instanceof HTMLElement) ||
    !(pictureGroup instanceof HTMLElement) ||
    !(outputGroup instanceof HTMLElement) ||
    !(drawSep instanceof HTMLElement) ||
    !(pictureSep instanceof HTMLElement) ||
    !(overflowRoot instanceof HTMLElement) ||
    !(overflowMenu instanceof HTMLElement) ||
    !(overflowToggle instanceof HTMLButtonElement) ||
    !(copyAllBtn instanceof HTMLButtonElement) ||
    !(ocrBtn instanceof HTMLButtonElement) ||
    !(qrBtn instanceof HTMLButtonElement) ||
    !(rotateLeftBtn instanceof HTMLButtonElement) ||
    !(rotateRightBtn instanceof HTMLButtonElement) ||
    !(cropBtn instanceof HTMLButtonElement) ||
    !(cropBar instanceof HTMLElement) ||
    !(cropConfirmBtn instanceof HTMLButtonElement) ||
    !(pinBtn instanceof HTMLButtonElement) ||
    !(updatePinBtn instanceof HTMLButtonElement) ||
    !(saveQualityRoot instanceof HTMLElement) ||
    !(saveQualityPanel instanceof HTMLElement) ||
    !(saveQualityToggle instanceof HTMLButtonElement)
  ) {
    return () => undefined;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return () => undefined;
  }

  let frame: PreviewFrame | null = null;
  let source: HTMLCanvasElement | null = null;
  // R21:标注层(共享模块)持有图元/撤销栈/文字编辑;R2:取字走共享 OCR 模型。
  let editor: AnnotationEditor | null = null;
  let ocrModel: OcrModel | null = null;
  // R4:二维码识别走共享 QR 模型(与冻结帧工作区同一实现,互斥激活)。
  let qrModel: QrModel | null = null;
  let busy = false;
  // 提示条的来源:词条键可在语言切换后重渲染,不透明文案(宿主错误串)保持原样。
  let noteSource: { key: CatalogKey | null; params?: Record<string, string | number>; text: string } | null =
    null;
  let noteKind: NoteKind = "feedback";
  let copiedSource: { key: CatalogKey | null; params?: Record<string, string | number>; text: string } =
    { key: "preview.copied_clean", text: "" };
  let copiedKind: NoteKind = "success";
  // R21:选区即时标注随帧带入时,提示条以「携带说明 + 复制状态」合并显示;
  // 二者共用同一元素,分两次写入会互相覆盖(review P3)。
  let carriedNoteSource: { key: CatalogKey; params?: Record<string, string | number> } | null = null;
  // 贴图再标注(R9):非空表示本会话由贴图进入,确认后写回该 label。
  let writebackLabel: string | null = null;
  let saveQuality: ExportQuality = "high";
  // R9:美化只包在显示层外:画布位图保持冻帧尺寸,标注坐标不平移。
  let applyBeautify = false;
  let beautifyOptions: BeautifyOptions = { ...DEFAULT_BEAUTIFY };
  // R6:预览旋转/裁剪。裁剪为画布拖选 + 确认/取消;变换由 Rust 按同一仿射
  // 重基帧、标注与取字;撤销/重做在标注编辑器栈为空时回退会话级快照。
  let cropping = false;
  let cropStart: Point | null = null;
  let cropCurrent: Point | null = null;
  let transformUndo = false;
  let transformRedo = false;
  const cropEdgeColor = resolveCanvasColor(
    getComputedStyle(root).getPropertyValue("--accent"),
    FALLBACK_CROP_EDGE,
  );

  // 画布工具标记:裁剪 > 取字 > 二维码 > 标注当前工具(与按钮高亮一致)。
  const syncToolDataset = (): void => {
    root.dataset.tool = cropping
      ? "crop"
      : ocrModel?.active
        ? "ocr"
        : qrModel?.active
          ? "qr"
          : (editor?.tool() ?? "arrow");
  };

  // 裁剪框(Frame 物理像素):拖选两端点四舍五入并钳制在画面内;空拖选返回 null。
  const cropRect = (): { x: number; y: number; width: number; height: number } | null => {
    if (!cropStart || !cropCurrent || canvas.width === 0 || canvas.height === 0) {
      return null;
    }
    const left = Math.round(clamp(Math.min(cropStart.x, cropCurrent.x), 0, canvas.width));
    const top = Math.round(clamp(Math.min(cropStart.y, cropCurrent.y), 0, canvas.height));
    const right = Math.round(clamp(Math.max(cropStart.x, cropCurrent.x), 0, canvas.width));
    const bottom = Math.round(clamp(Math.max(cropStart.y, cropCurrent.y), 0, canvas.height));
    if (right <= left || bottom <= top) {
      return null;
    }
    return { x: left, y: top, width: right - left, height: bottom - top };
  };

  // 裁剪遮罩:未拖选时整幅压暗,拖选后仅保留选区明亮并描边。
  const paintCropOverlay = (): void => {
    if (!cropping) {
      return;
    }
    ctx.save();
    ctx.fillStyle = "rgba(2, 6, 23, 0.55)";
    const rect = cropRect();
    if (!rect) {
      ctx.fillRect(0, 0, canvas.width, canvas.height);
    } else {
      ctx.fillRect(0, 0, canvas.width, rect.y);
      ctx.fillRect(0, rect.y, rect.x, rect.height);
      ctx.fillRect(rect.x + rect.width, rect.y, canvas.width - rect.x - rect.width, rect.height);
      ctx.fillRect(0, rect.y + rect.height, canvas.width, canvas.height - rect.y - rect.height);
      const line = Math.max(1, canvas.width / 900);
      ctx.strokeStyle = cropEdgeColor;
      ctx.lineWidth = line;
      ctx.strokeRect(rect.x + line / 2, rect.y + line / 2, rect.width - line, rect.height - line);
    }
    ctx.restore();
  };

  const noteText = (source: {
    key: CatalogKey | null;
    params?: Record<string, string | number>;
    text: string;
  }): string => (source.key ? t(source.key, source.params) : source.text);

  const renderNote = (): void => {
    if (!noteSource) {
      return;
    }
    // 携带说明只装饰「已复制」状态提示(它是帧级信息,与工具提示无关)。
    const carried = carriedNoteSource;
    const prefix =
      carried !== null && noteSource === copiedSource ? `${t(carried.key, carried.params)} ` : "";
    note.textContent = `${prefix}${noteText(noteSource)}`;
    note.classList.toggle("is-success", noteKind === "success");
    note.classList.toggle("is-feedback", noteKind === "feedback");
    note.classList.toggle("is-error", noteKind === "error");
  };

  const setNoteSource = (
    source: { key: CatalogKey | null; params?: Record<string, string | number>; text: string },
    kind: NoteKind = "feedback",
  ): void => {
    noteSource = source;
    noteKind = kind;
    renderNote();
  };

  const setNote = (text: string, kind: NoteKind = "feedback"): void => {
    setNoteSource({ key: null, text }, kind);
  };

  const setNoteKey = (
    key: CatalogKey,
    params?: Record<string, string | number>,
    kind: NoteKind = "feedback",
  ): void => {
    setNoteSource({ key, params, text: "" }, kind);
  };

  // 「已复制」提示携带来源样式(成功/失败/未自动复制);切换工具重绘时
  // 沿用原 kind,不让 feedback 提示被固定改写成成功色。携带标注说明
  // (`carriedNoteSource`)只跟随复制状态提示,不覆盖其它提示。
  const setCopied = (
    source: { key: CatalogKey | null; params?: Record<string, string | number>; text: string },
    kind: NoteKind,
  ): void => {
    copiedSource = source;
    copiedKind = kind;
    setNoteSource(source, kind);
  };

  // 诊断面:未捕获的脚本错误与 Promise 拒绝直接显现在提示条,避免"按钮点了没反应"无处可查。
  window.addEventListener("error", (event) => {
    setNoteKey("preview.error.ui", { message: String(event.message ?? t("preview.error.unknown")).slice(0, 80) }, "error");
  });
  window.addEventListener("unhandledrejection", (event) => {
    const reason = event.reason instanceof Error ? event.reason.message : String(event.reason);
    setNoteKey("preview.error.action", { message: reason.slice(0, 80) }, "error");
  });

  const redraw = (): void => {
    if (!source) {
      return;
    }
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(source, 0, 0);
    editor?.paint(ctx);
    applyBeautifyChrome();
    ocrModel?.paint(ctx);
    paintCropOverlay();
  };

  const physicalPoint = (event: MouseEvent): Point => {
    const rect = canvas.getBoundingClientRect();
    const scaleX = canvas.width / Math.max(rect.width, 1);
    const scaleY = canvas.height / Math.max(rect.height, 1);
    return {
      x: clamp((event.clientX - rect.left) * scaleX, 0, canvas.width),
      y: clamp((event.clientY - rect.top) * scaleY, 0, canvas.height),
    };
  };

  // 工具显隐和文案宽度变化后重排顶栏。挂载早期还是空操作。
  let requestOverflowLayout = (): void => undefined;
  let closeOverflowMenu = (): void => undefined;
  let enabledAnnotation = new Set<AnnotationTool>();

  // 取字/二维码工具按钮与顶部「复制全部」随共享模型状态同步;退出识别后把
  // 画布工具标记与工具条高亮还给标注编辑器当前工具,并撤掉识别提示。
  let ocrWasActive = false;
  let qrWasActive = false;
  let recognitionNoticeActive = false;
  const syncRecognitionToolbar = (): void => {
    const ocrActive = ocrModel?.active === true;
    const qrActive = qrModel?.active === true;
    ocrBtn.classList.toggle("active", ocrActive);
    qrBtn.classList.toggle("active", qrActive);
    copyAllBtn.hidden = !ocrActive || (ocrModel?.document()?.spans.length ?? 0) === 0;
    if (!ocrActive && !qrActive && (ocrWasActive || qrWasActive)) {
      editor?.setTool(editor.tool());
      if (recognitionNoticeActive) {
        recognitionNoticeActive = false;
        setNoteSource(copiedSource, copiedKind);
      }
    }
    ocrWasActive = ocrActive;
    qrWasActive = qrActive;
    syncToolDataset();
    redraw();
    requestOverflowLayout();
  };

  const syncWritebackUi = (): void => {
    updatePinBtn.hidden = writebackLabel === null;
    pinBtn.hidden = writebackLabel !== null;
    requestOverflowLayout();
  };

  let cropPlaceAttempts = 0;
  // 确认条默认贴画面下缘居中。与当前裁剪矩形相交时改到上缘，避免压住拖选范围。
  const placeCropBar = (): void => {
    if (!cropping) {
      return;
    }
    const frameRect = frameEl.getBoundingClientRect();
    const canvasRect = canvas.getBoundingClientRect();
    const barWidth = cropBar.offsetWidth;
    const barHeight = cropBar.offsetHeight;
    if (
      frameRect.width < 1 ||
      frameRect.height < 1 ||
      canvasRect.width < 1 ||
      canvasRect.height < 1 ||
      barWidth < 1 ||
      barHeight < 1
    ) {
      if (cropPlaceAttempts >= 2) {
        return;
      }
      cropPlaceAttempts += 1;
      requestAnimationFrame(() => {
        if (cropping) {
          placeCropBar();
        }
      });
      return;
    }
    cropPlaceAttempts = 0;
    const maxLeft = Math.max(0, frameRect.width - barWidth);
    const maxTop = Math.max(0, frameRect.height - barHeight);
    const left = Math.round(
      clamp(canvasRect.left - frameRect.left + (canvasRect.width - barWidth) / 2, 0, maxLeft),
    );
    const edgeTop = (preferred: number): number => Math.round(clamp(preferred, 0, maxTop));
    const bottomTop = edgeTop(canvasRect.bottom - frameRect.top - CROP_BAR_INSET - barHeight);
    const topTop = edgeTop(canvasRect.top - frameRect.top + CROP_BAR_INSET);
    const intersectsCrop = (top: number): boolean => {
      const rect = cropRect();
      if (!rect || canvas.width < 1 || canvas.height < 1) {
        return false;
      }
      const scaleX = canvasRect.width / canvas.width;
      const scaleY = canvasRect.height / canvas.height;
      const cropLeft = canvasRect.left + rect.x * scaleX;
      const cropTop = canvasRect.top + rect.y * scaleY;
      const cropRight = cropLeft + rect.width * scaleX;
      const cropBottom = cropTop + rect.height * scaleY;
      const barLeft = frameRect.left + left;
      const barTop = frameRect.top + top;
      return (
        barLeft < cropRight &&
        barLeft + barWidth > cropLeft &&
        barTop < cropBottom &&
        barTop + barHeight > cropTop
      );
    };
    const edge = intersectsCrop(bottomTop) ? "top" : "bottom";
    const top = edge === "top" ? topTop : bottomTop;
    const leftPx = `${left}px`;
    const topPx = `${top}px`;
    if (
      cropBar.style.left !== leftPx ||
      cropBar.style.top !== topPx ||
      cropBar.style.transform !== "none" ||
      cropBar.dataset.cropEdge !== edge
    ) {
      cropBar.style.left = leftPx;
      cropBar.style.top = topPx;
      cropBar.style.right = "auto";
      cropBar.style.bottom = "auto";
      cropBar.style.transform = "none";
      cropBar.dataset.cropEdge = edge;
    }
  };

  const clearCropBarPlacement = (): void => {
    cropPlaceAttempts = 0;
    cropBar.style.left = "";
    cropBar.style.top = "";
    cropBar.style.right = "";
    cropBar.style.bottom = "";
    cropBar.style.transform = "";
    delete cropBar.dataset.cropEdge;
  };

  const applyBeautifyChrome = (): void => {
    if (!applyBeautify || !frame) {
      root.dataset.beautify = "off";
      outputEl.removeAttribute("style");
      canvas.style.width = "";
      canvas.style.height = "";
      canvas.style.borderRadius = "";
      canvas.style.boxShadow = "";
      placeCropBar();
      return;
    }
    const layout = beautifyLayout(frame.width, frame.height, beautifyOptions);
    const availW = Math.max(frameEl.clientWidth, 1);
    const availH = Math.max(frameEl.clientHeight, 1);
    const scale = Math.min(availW / layout.outputWidth, availH / layout.outputHeight);
    if (!Number.isFinite(scale) || scale <= 0) {
      placeCropBar();
      return;
    }
    root.dataset.beautify = "on";
    outputEl.style.boxSizing = "content-box";
    outputEl.style.width = `${frame.width * scale}px`;
    outputEl.style.height = `${frame.height * scale}px`;
    outputEl.style.padding = `${layout.origin * scale}px`;
    outputEl.style.background = beautifyBackground(beautifyOptions.preset);
    canvas.style.width = "100%";
    canvas.style.height = "100%";
    canvas.style.borderRadius = `${layout.radius * scale}px`;
    if (beautifyOptions.shadow && layout.shadowMargin > 0) {
      const blur = Math.max(1, layout.shadowMargin * 0.45) * scale;
      const offsetY = Math.max(1, layout.shadowMargin * 0.2) * scale;
      canvas.style.boxShadow = `0 ${offsetY}px ${blur}px rgba(15, 23, 42, 0.38)`;
    } else {
      canvas.style.boxShadow = "none";
    }
    placeCropBar();
  };

  const syncSaveQuality = (): void => {
    saveQualityRoot.querySelectorAll<HTMLButtonElement>("[data-save-quality]").forEach((button) => {
      button.classList.toggle("active", button.dataset.saveQuality === saveQuality);
    });
  };

  const toggleQualityPanel = (open?: boolean, restoreFocus = false): void => {
    const next = open ?? saveQualityPanel.hidden;
    const panelHadFocus = saveQualityPanel.contains(document.activeElement);
    if (next) {
      // 重开先回 CSS 锚点(右锚)再钳制,与更多/样式面板同一套边界语义。
      saveQualityPanel.style.left = "";
      saveQualityPanel.style.right = "";
    }
    saveQualityPanel.hidden = !next;
    saveQualityToggle.classList.toggle("active", next);
    saveQualityToggle.setAttribute("aria-expanded", next ? "true" : "false");
    if (next) {
      closeOverflowMenu();
      clampFloatingPanel(saveQualityPanel);
      const current = saveQualityPanel.querySelector<HTMLButtonElement>(
        `[data-save-quality="${saveQuality}"]`,
      );
      current?.focus();
    } else if (restoreFocus || panelHadFocus) {
      saveQualityToggle.focus();
    }
  };

  // 质量面板属于 preview 宿主且位于标注层之上。捕获阶段优先消费 Esc，
  // 避免标注编辑器先清选中/取消文字编辑后阻止宿主关闭当前顶层面板。
  window.addEventListener(
    "keydown",
    (event) => {
      if (
        event.key !== "Escape" ||
        event.isComposing ||
        event.keyCode === 229 ||
        saveQualityPanel.hidden
      ) {
        return;
      }
      event.preventDefault();
      event.stopImmediatePropagation();
      toggleQualityPanel(false, true);
    },
    { capture: true },
  );

  editor = mountAnnotationEditor({
    root,
    canvas,
    ctx,
    toolbar: toolbarEl,
    textHost: frameEl,
    inlineTools: true,
    enabledTools: [],
    onToolsChanged: () => requestOverflowLayout(),
    frame: () => frame,
    redraw,
    // R2/R4/R6:取字、二维码与裁剪任一激活时画布输入只走对应宿主分支,
    // 标注编辑暂停。裁剪拖选不得绘制/移动/放置标注,确认与取消保持列表不变。
    isEditable: () =>
      !cropping && ocrModel?.active !== true && qrModel?.active !== true,
    onToolHint: (hint) => {
      if (hint) {
        setNoteKey(hint.key, hint.params);
      } else if (!note.classList.contains("is-error")) {
        setNoteSource(copiedSource, copiedKind);
      }
    },
    onToolChange: () => {
      // 手动切回标注工具即退出取字/二维码识别,结束识别后恢复标注/复制/保存路径。
      ocrModel?.deactivate();
      qrModel?.deactivate();
    },
    // 样式持久化失败等:沿用既有提示条错误呈现,不静默丢失。
    onError: (error) => {
      if (typeof error === "string") {
        setNote(error, "error");
      } else {
        setNoteKey(error.key, error.params, "error");
      }
    },
    // R6:标注栈为空时把 Ctrl+Z/Y 与工具条撤销/重做交给预览变换快照。
    // 新标注编辑使变换重做分支失效(与编辑器 pushAction 清 redo 同一语义):
    // 编辑器仍有未撤销编辑时,不再提供变换重做。
    onUndoFallback: () => (cropping ? false : undoTransform()),
    onRedoFallback: () => (cropping ? false : redoTransform()),
    canUndoFallback: () => !cropping && transformUndo,
    canRedoFallback: () => !cropping && transformRedo && !(editor?.canUndo() ?? false),
  });

  // R2:共享取字模型(结果面板 + 图上三态 + 显式复制)。面板挂在预览舞台,
  // 与标注编辑器的 isEditable 互斥:取字期间画布输入只给取字。
  ocrModel = mountOcrModel({
    host: stageEl,
    notice: (message, kind) => {
      recognitionNoticeActive = true;
      if (kind === "success") {
        setNote(message, "success");
      } else if (kind === "error") {
        setNote(message, "error");
      } else {
        setNote(message);
      }
    },
    onChange: syncRecognitionToolbar,
  });

  // R4:共享二维码识别模型(结果面板 + 显式复制)。与取字互斥激活:
  // 识别只读预览帧,内容展示后由用户点复制写入剪贴板,不自动打开链接。
  qrModel = mountQrModel({
    host: stageEl,
    notice: (message, kind) => {
      recognitionNoticeActive = true;
      if (kind === "success") {
        setNote(message, "success");
      } else if (kind === "error") {
        setNote(message, "error");
      } else {
        setNote(message);
      }
    },
    onChange: syncRecognitionToolbar,
  });

  // 一行放不下时，按优先级把按钮收进带名称的菜单。复制、保存和样式留在主行。
  interface OverflowItem {
    node: HTMLElement;
    kind: "tool" | "tail" | "picture" | "output";
    order: number;
    rank: number;
    keep: boolean;
  }
  const overflowItems: OverflowItem[] = [];
  const registerOverflow = (
    node: HTMLElement | null,
    kind: OverflowItem["kind"],
    rank: number,
    keep = false,
  ): void => {
    if (!(node instanceof HTMLElement)) {
      return;
    }
    overflowItems.push({ node, kind, order: overflowItems.length, rank, keep });
  };
  const primaryToolIds = new Set<string>(PRIMARY_TOOLS);
  const toolRank = (button: HTMLButtonElement): number => {
    const id = button.dataset.tool;
    if (id && isAnnotationTool(id)) {
      return primaryToolIds.has(id) ? 50 : 10;
    }
    return 50;
  };
  toolbarEl.querySelectorAll<HTMLButtonElement>(":scope > button[data-tool]").forEach((button) => {
    registerOverflow(button, "tool", toolRank(button));
  });
  registerOverflow(toolbarEl.querySelector<HTMLElement>("[data-action=undo]"), "tail", 60);
  registerOverflow(toolbarEl.querySelector<HTMLElement>("[data-action=redo]"), "tail", 60);
  registerOverflow(toolbarEl.querySelector<HTMLElement>("[data-action=delete]"), "tail", 20);
  registerOverflow(toolbarEl.querySelector<HTMLElement>("[data-style-root]"), "tail", 65, true);
  pictureGroup.querySelectorAll<HTMLElement>(":scope > button").forEach((button) => {
    registerOverflow(button, "picture", 30);
  });
  registerOverflow(pinBtn, "output", 80);
  registerOverflow(updatePinBtn, "output", 80);
  registerOverflow(saveQualityRoot, "output", 90, true);

  const groupOccupied = (group: HTMLElement): boolean =>
    Array.from(group.children).some((child) => {
      if (!(child instanceof HTMLElement) || child.hidden) {
        return false;
      }
      return !child.classList.contains("toolbar-sep");
    });

  const syncGroupSeps = (): void => {
    const drawOn = groupOccupied(toolbarEl);
    const pictureOn = groupOccupied(pictureGroup);
    const outputOn = groupOccupied(outputGroup);
    drawSep.hidden = !(drawOn && (pictureOn || outputOn));
    pictureSep.hidden = !(pictureOn && outputOn);
    const toolbarSep = toolbarEl.querySelector<HTMLElement>(":scope > .toolbar-sep");
    if (toolbarSep) {
      const toolVisible = Array.from(
        toolbarEl.querySelectorAll<HTMLElement>(":scope > button[data-tool]"),
      ).some((button) => !button.hidden);
      const tailVisible = Array.from(
        toolbarEl.querySelectorAll<HTMLElement>(":scope > button[data-action], :scope > .annotation-style"),
      ).some((node) => !node.hidden);
      toolbarSep.hidden = !(toolVisible && tailVisible);
    }
  };

  const placeOverflowMenu = (): void => {
    overflowMenu.style.position = "fixed";
    overflowMenu.style.right = "auto";
    overflowMenu.style.bottom = "auto";
    overflowMenu.style.left = "0px";
    overflowMenu.style.top = "0px";
    const width = overflowMenu.offsetWidth;
    const height = overflowMenu.offsetHeight;
    const box = overflowToggle.getBoundingClientRect();
    const margin = 8;
    const maxLeft = Math.max(margin, window.innerWidth - margin - width);
    const maxTop = Math.max(margin, window.innerHeight - margin - height);
    let left = box.right - width;
    let top = box.bottom + 6;
    if (top > maxTop) {
      top = Math.max(margin, box.top - 6 - height);
    }
    left = Math.min(Math.max(left, margin), maxLeft);
    top = Math.min(Math.max(top, margin), maxTop);
    overflowMenu.style.left = `${Math.round(left)}px`;
    overflowMenu.style.top = `${Math.round(top)}px`;
    const room = Math.max(120, window.innerHeight - margin * 2);
    overflowMenu.style.maxHeight = `${Math.min(room, 480)}px`;
  };

  closeOverflowMenu = (): void => {
    if (!overflowMenu.hidden) {
      toggleOverflowMenu(false);
    }
  };

  const toggleOverflowMenu = (open?: boolean, restoreFocus = false): void => {
    if (overflowRoot.hidden) {
      overflowMenu.hidden = true;
      overflowToggle.setAttribute("aria-expanded", "false");
      return;
    }
    const next = open ?? overflowMenu.hidden;
    const hadFocus = overflowMenu.contains(document.activeElement);
    overflowMenu.hidden = !next;
    overflowToggle.classList.toggle("active", next);
    overflowToggle.setAttribute("aria-expanded", next ? "true" : "false");
    if (next) {
      placeOverflowMenu();
      const first = overflowMenu.querySelector<HTMLElement>("button:not([hidden])");
      first?.focus();
    } else if (restoreFocus || hadFocus) {
      overflowToggle.focus();
    }
  };

  let layoutEpoch = 0;
  let layoutToken = "";
  let layingOut = false;
  const layoutOverflow = (): void => {
    if (layingOut) {
      return;
    }
    const visibleKey = overflowItems.map((item) => (item.node.hidden ? "0" : "1")).join("");
    const tokenNow = (): string => `${layoutEpoch}|${toolbarRow.clientWidth}|${visibleKey}`;
    if (tokenNow() === layoutToken) {
      return;
    }
    layingOut = true;
    const menuWasOpen = !overflowMenu.hidden;
    const moreHome = toolbarEl.querySelector("[data-more-root]");
    const tools = overflowItems.filter((item) => item.kind === "tool").sort((a, b) => a.order - b.order);
    if (moreHome instanceof HTMLElement) {
      for (const item of [...tools].reverse()) {
        toolbarEl.insertBefore(item.node, moreHome);
      }
    }
    for (const kind of ["tail", "picture", "output"] as const) {
      const home = kind === "tail" ? toolbarEl : kind === "picture" ? pictureGroup : outputGroup;
      for (const item of overflowItems.filter((entry) => entry.kind === kind).sort((a, b) => a.order - b.order)) {
        home.append(item.node);
      }
    }
    overflowRoot.hidden = true;
    overflowMenu.hidden = true;
    overflowMenu.replaceChildren();
    const fits = (): boolean => toolbarRow.scrollWidth <= toolbarRow.clientWidth + 1;
    const moved: OverflowItem[] = [];
    if (!fits()) {
      overflowRoot.hidden = false;
      const movable = overflowItems
        .filter((item) => !item.keep && !item.node.hidden)
        .sort((a, b) => a.rank - b.rank || b.order - a.order);
      for (const item of movable) {
        if (fits()) {
          break;
        }
        item.node.remove();
        moved.push(item);
      }
      moved.sort((a, b) => a.order - b.order);
      overflowMenu.append(...moved.map((item) => item.node));
      if (moved.length === 0) {
        overflowRoot.hidden = true;
      }
    }
    const menuOpen = moved.length > 0 && menuWasOpen;
    overflowMenu.hidden = !menuOpen;
    overflowToggle.classList.toggle("active", menuOpen || overflowMenu.querySelector(".active") !== null);
    overflowToggle.setAttribute("aria-expanded", menuOpen ? "true" : "false");
    if (menuOpen) {
      placeOverflowMenu();
    }
    syncGroupSeps();
    layingOut = false;
    layoutToken = tokenNow();
  };
  requestOverflowLayout = (): void => {
    layoutEpoch += 1;
    layoutOverflow();
  };

  let regionToolsTicket = 0;
  const applyRegionTools = (tools: RegionTools): void => {
    const enabled = REGION_TOOL_FIELDS.flatMap((field) =>
      tools[field.id] && isAnnotationTool(field.id) ? [field.id] : [],
    );
    enabledAnnotation = new Set(enabled);
    editor?.setEnabledTools(enabled);
    ocrBtn.hidden = tools.ocr !== true;
    qrBtn.hidden = tools.qr !== true;
    if (tools.ocr !== true) {
      ocrModel?.deactivate();
    }
    if (tools.qr !== true) {
      qrModel?.deactivate();
    }
    requestOverflowLayout();
  };
  const loadRegionTools = (): void => {
    const ticket = ++regionToolsTicket;
    editor?.setEnabledTools([]);
    enabledAnnotation = new Set();
    ocrBtn.hidden = true;
    qrBtn.hidden = true;
    requestOverflowLayout();
    void invoke<{ regionTools?: RegionTools }>("get_ui_settings")
      .then((settings) => {
        if (ticket !== regionToolsTicket) {
          return;
        }
        applyRegionTools(settings.regionTools ?? PREVIEW_TOOL_DEFAULTS);
      })
      .catch(() => {
        if (ticket !== regionToolsTicket) {
          return;
        }
        applyRegionTools(PREVIEW_TOOL_DEFAULTS);
      });
  };
  const overflowObserver = new ResizeObserver(() => {
    layoutOverflow();
  });
  overflowObserver.observe(toolbarRow);
  loadRegionTools();

  const activateOcr = (): void => {
    if (ocrBtn.hidden || ocrModel?.active) {
      return;
    }
    qrModel?.deactivate();
    editor?.commitText();
    editor?.deactivateTool();
    editor?.clearSelection();
    ocrModel?.activate();
  };

  const activateQr = (): void => {
    if (qrBtn.hidden || qrModel?.active) {
      return;
    }
    ocrModel?.deactivate();
    editor?.commitText();
    editor?.deactivateTool();
    editor?.clearSelection();
    qrModel?.activate();
  };

  // R6:裁剪模式的界面状态(工具条按钮、裁剪条与确认可用性)。
  const syncCropUi = (): void => {
    cropBar.hidden = !cropping;
    cropBtn.classList.toggle("active", cropping);
    rotateLeftBtn.disabled = cropping;
    rotateRightBtn.disabled = cropping;
    cropConfirmBtn.disabled = !cropping || busy || cropRect() === null;
    if (cropping) {
      placeCropBar();
    } else {
      clearCropBarPlacement();
    }
    syncToolDataset();
  };

  const exitCrop = (): void => {
    if (!cropping) {
      return;
    }
    cropping = false;
    cropStart = null;
    cropCurrent = null;
    // 恢复标注工具条高亮(enterCrop 为进入裁剪清掉了)。
    editor?.setTool(editor.tool());
    syncCropUi();
    setNoteSource(copiedSource, copiedKind);
    redraw();
  };

  const enterCrop = (): void => {
    if (cropping || !frame || busy) {
      return;
    }
    cropping = true;
    cropStart = null;
    cropCurrent = null;
    ocrModel?.deactivate();
    qrModel?.deactivate();
    editor?.commitText();
    editor?.deactivateTool();
    editor?.clearSelection();
    syncCropUi();
    setNoteKey("preview.crop.hint");
    redraw();
  };

  const copyBtn = root.querySelector<HTMLButtonElement>("[data-action=copy]");
  let copyReset = 0;
  const flashCopiedButton = (): void => {
    if (!(copyBtn instanceof HTMLButtonElement)) {
      return;
    }
    copyBtn.classList.add("is-copied");
    const label = copyBtn.querySelector("span");
    if (label) {
      label.textContent = t("preview.action.copied");
    }
    window.clearTimeout(copyReset);
    copyReset = window.setTimeout(() => {
      copyBtn.classList.remove("is-copied");
      applyTranslations(copyBtn);
    }, 1600);
  };

  const pulseNote = (): void => {
    note.classList.remove("is-pulse");
    void note.offsetWidth;
    note.classList.add("is-pulse");
  };

  const copy = async (): Promise<void> => {
    if (busy) {
      setNoteKey("preview.note.busy");
      return;
    }
    editor?.commitText();
    busy = true;
    setNoteKey("preview.note.copying");
    try {
      const annotations = editor?.exportList() ?? [];
      await invoke("copy_preview_png", { annotations });
      setCopied(
        annotations.length > 0
          ? { key: "preview.copied_annotated", text: "" }
          : { key: "preview.copied_clean", text: "" },
        "success",
      );
      flashCopiedButton();
      pulseNote();
    } catch (error) {
      setNote(invokeError(error, t("preview.error.copy_fallback")), "error");
    } finally {
      busy = false;
    }
  };

  const save = async (): Promise<void> => {
    if (busy) {
      setNoteKey("preview.note.busy");
      return;
    }
    editor?.commitText();
    busy = true;
    // 进行中提示不自动消失:由成功/取消/失败文案替换(ADR-2)。
    setNoteKey("preview.note.saving");
    try {
      const result = await invoke<{
        saved: boolean;
        format?: ExportFormat;
        path?: string | null;
      }>("save_preview_png", {
        annotations: editor?.exportList() ?? [],
        quality: saveQuality,
      });
      if (result.saved) {
        const format = result.format ?? "png";
        const name = fileNameFromPath(result.path) ?? `cropmark.${format === "jpeg" ? "jpg" : format}`;
        setNoteKey("preview.note.saved", { name }, "success");
      } else {
        // 保存对话框取消(saved=false)不是错误,也要给可见反馈。
        setNoteKey("preview.note.save_canceled");
      }
    } catch (error) {
      setNote(invokeError(error, t("preview.error.save_fallback")), "error");
    } finally {
      busy = false;
    }
  };

  // 贴图:当前标注合成图钉成置顶小窗;预览保持打开,可继续标注/再贴。
  const pin = async (): Promise<void> => {
    if (busy || cropping) {
      if (busy) {
        setNoteKey("preview.note.busy");
      }
      return;
    }
    editor?.commitText();
    busy = true;
    setNoteKey("preview.note.pinning");
    try {
      await invoke("pin_current", { annotations: editor?.exportList() ?? [] });
      setNoteKey("preview.note.pinned", undefined, "success");
    } catch (error) {
      setNote(invokeError(error, t("preview.error.pin_fallback")), "error");
    } finally {
      busy = false;
    }
  };

  // 再标注确认:把当前标注写回来源贴图,Rust 更新源图并通知贴图窗换图;
  // 成功后预览由 Rust 关闭。取消(直接关闭预览)不改动贴图内容。
  const updatePin = async (): Promise<void> => {
    if (busy || cropping || writebackLabel === null) {
      return;
    }
    editor?.commitText();
    busy = true;
    setNoteKey("preview.note.updating_pin");
    try {
      await invoke("update_pin_from_preview", { annotations: editor?.exportList() ?? [] });
    } catch (error) {
      setNote(invokeError(error, t("preview.error.update_pin_fallback")), "error");
    } finally {
      busy = false;
    }
  };

  const closePreview = (): void => {
    void invoke("close_preview").catch((error) => {
      setNote(invokeError(error, t("preview.error.close_fallback")), "error");
    });
  };

  // 裁剪拖选优先;取字/二维码识别期间交给共享模型,画布不接受标注输入。
  canvas.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || !frame) {
      return;
    }
    if (cropping) {
      event.preventDefault();
      cropStart = physicalPoint(event);
      cropCurrent = cropStart;
      syncCropUi();
      redraw();
      return;
    }
    if (ocrModel?.active !== true) {
      return;
    }
    event.preventDefault();
    ocrModel.pointerDown(physicalPoint(event));
  });

  // 取字用右键:共享标注层不接管取字工具,这里兜底阻止浏览器菜单。
  canvas.addEventListener("contextmenu", (event) => {
    event.preventDefault();
  });

  window.addEventListener("mousemove", (event) => {
    if (cropping) {
      if (!cropStart) {
        return;
      }
      cropCurrent = physicalPoint(event);
      syncCropUi();
      redraw();
      return;
    }
    if (ocrModel?.active !== true) {
      return;
    }
    ocrModel.pointerMove(physicalPoint(event));
  });

  window.addEventListener("mouseup", () => {
    if (cropping) {
      syncCropUi();
      return;
    }
    if (ocrModel?.active !== true) {
      return;
    }
    ocrModel.pointerUp();
  });

  cropBar.addEventListener("click", (event) => {
    const button = event.target instanceof Element ? event.target.closest("[data-crop-action]") : null;
    if (!(button instanceof HTMLButtonElement) || button.disabled) {
      return;
    }
    if (button.dataset.cropAction === "confirm") {
      void confirmCrop();
    } else if (button.dataset.cropAction === "cancel") {
      exitCrop();
    }
  });

  window.addEventListener(
    "keydown",
    (event) => {
      if (
        event.key !== "Escape" ||
        event.isComposing ||
        event.keyCode === 229 ||
        overflowMenu.hidden
      ) {
        return;
      }
      event.preventDefault();
      event.stopImmediatePropagation();
      toggleOverflowMenu(false, true);
    },
    { capture: true },
  );

  root.addEventListener("click", (event) => {
    const button = event.target instanceof Element ? event.target.closest("button") : null;
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    if (button.dataset.action === "preview-more") {
      if (!saveQualityPanel.hidden) {
        toggleQualityPanel(false);
      }
      toggleOverflowMenu();
      return;
    }
    if (overflowMenu.contains(button) && button.dataset.action !== "toggle-quality") {
      toggleOverflowMenu(false);
    }
    // 裁剪中只受理裁剪条与关闭:其余动作先退出裁剪再执行会丢失当前选区语义。
    if (cropping && button.dataset.action !== "crop" && button.dataset.action !== "close") {
      return;
    }
    if (button.dataset.tool === "ocr") {
      activateOcr();
      return;
    }
    if (button.dataset.tool === "qr") {
      activateQr();
      return;
    }
    if (button.dataset.action === "rotate-left") {
      void rotate("left");
      return;
    }
    if (button.dataset.action === "rotate-right") {
      void rotate("right");
      return;
    }
    if (button.dataset.action === "crop") {
      if (cropping) {
        exitCrop();
      } else {
        enterCrop();
      }
      return;
    }
    const nextQuality = button.dataset.saveQuality;
    if (nextQuality === "high" || nextQuality === "medium" || nextQuality === "low") {
      saveQuality = nextQuality;
      syncSaveQuality();
      toggleQualityPanel(false);
      return;
    }
    if (button.dataset.action === "toggle-quality") {
      toggleQualityPanel();
      return;
    }
    if (button.dataset.action === "copy") {
      void copy();
    } else if (button.dataset.action === "copy-ocr-all") {
      void ocrModel?.copyAll();
    } else if (button.dataset.action === "save") {
      void save();
    } else if (button.dataset.action === "pin") {
      void pin();
    } else if (button.dataset.action === "update-pin") {
      void updatePin();
    } else if (button.dataset.action === "close") {
      closePreview();
    }
  });

  // 点在按钮文字上时 target 是 Text 节点,不能用 instanceof Element,
  // 否则标题栏会 startDragging,关闭/贴图的 click 被系统拖拽吃掉。
  const eventElement = (event: Event): Element | null => {
    const target = event.target;
    if (target instanceof Element) {
      return target;
    }
    return target instanceof Node ? target.parentElement : null;
  };

  let qualityPanelHadFocusOnPointerDown = false;

  // click 触发前 mousedown 的默认行为可能已把焦点移到 body。提前记住
  // 焦点是否来自面板，外部点击关闭时才能稳定归还到质量开关。
  document.addEventListener(
    "pointerdown",
    (event) => {
      qualityPanelHadFocusOnPointerDown =
        !saveQualityPanel.hidden &&
        saveQualityPanel.contains(document.activeElement) &&
        !(event.target instanceof Node && saveQualityRoot.contains(event.target));
    },
    { capture: true },
  );

  document.addEventListener("click", (event) => {
    if (
      !saveQualityPanel.hidden &&
      !(event.target instanceof Node && saveQualityRoot.contains(event.target))
    ) {
      toggleQualityPanel(false, qualityPanelHadFocusOnPointerDown);
    }
    qualityPanelHadFocusOnPointerDown = false;
    if (
      !overflowMenu.hidden &&
      !(event.target instanceof Node && overflowRoot.contains(event.target))
    ) {
      toggleOverflowMenu(false);
    }
  });

  pinBtn.addEventListener("pointerdown", (event) => event.stopPropagation());
  pinBtn.addEventListener("click", (event) => {
    event.preventDefault();
    event.stopPropagation();
    if (overflowMenu.contains(pinBtn)) {
      toggleOverflowMenu(false);
    }
    void pin();
  });

  const closeBtn = root.querySelector(".preview-close");
  if (closeBtn instanceof HTMLButtonElement) {
    closeBtn.addEventListener("pointerdown", (event) => event.stopPropagation());
    closeBtn.addEventListener("click", (event) => {
      event.preventDefault();
      event.stopPropagation();
      closePreview();
    });
  }

  root.querySelectorAll("[data-drag-handle]").forEach((handle) => {
    handle.addEventListener("mousedown", (event) => {
      if (!(event instanceof MouseEvent) || event.button !== 0) {
        return;
      }
      if (eventElement(event)?.closest("button")) {
        return;
      }
      event.preventDefault();
      void getCurrentWindow().startDragging();
    });
  });

  // 标注模块已处理编辑器/菜单/面板/选中与撤销快捷键(Escape/Delete/Ctrl+Z);
  // 这里保留取字(Esc 退出、Ctrl+A 全选、Ctrl+C 复制所选)、保存、复制与关闭。
  window.addEventListener("keydown", (event) => {
    if (event.defaultPrevented || event.isComposing || event.keyCode === 229) {
      return;
    }
    // R6:裁剪模式只响应 Enter 确认与 Esc 取消,其它快捷键让位给选区调整。
    if (cropping) {
      if (event.key === "Escape") {
        event.preventDefault();
        exitCrop();
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        void confirmCrop();
        return;
      }
      return;
    }
    if (event.key === "Escape") {
      if (!saveQualityPanel.hidden) {
        event.preventDefault();
        toggleQualityPanel(false, true);
        return;
      }
      // R2:取字中先退出取字,再按一次才关预览;退出后恢复标注/复制/保存路径。
      if (ocrModel?.deactivate()) {
        event.preventDefault();
        return;
      }
      // R4:二维码识别中 Esc 同样先退出识别面板。
      if (qrModel?.deactivate()) {
        event.preventDefault();
        return;
      }
      // 走 closePreview 统一入口:invoke 失败有错误提示而不是静默。
      closePreview();
      return;
    }
    const mod = event.ctrlKey || event.metaKey;
    if (mod) {
      const key = event.key.toLowerCase();
      if (key === "s") {
        event.preventDefault();
        void save();
        return;
      }
      // R2:取字中的 Ctrl+C/Ctrl+A 作用于识别文本;搜索框内的原生复制放行。
      if (ocrModel?.active === true && document.activeElement instanceof HTMLInputElement) {
        return;
      }
      if (key === "c" && ocrModel?.active === true) {
        event.preventDefault();
        void ocrModel.copySelected();
        return;
      }
      if (key === "a" && ocrModel?.active === true) {
        event.preventDefault();
        ocrModel.selectAll();
        return;
      }
      // R4:二维码识别中的 Ctrl+C 复制首条内容(面板按钮是主路径)。
      if (key === "c" && qrModel?.active === true) {
        event.preventDefault();
        void qrModel.copy(0);
        return;
      }
      if (key === "c") {
        if (
          document.activeElement instanceof HTMLTextAreaElement ||
          document.activeElement instanceof HTMLInputElement
        ) {
          return;
        }
        const selection = window.getSelection();
        if (selection && !selection.isCollapsed) {
          return;
        }
        event.preventDefault();
        void copy();
      }
      return;
    }
    if (event.altKey || event.shiftKey) {
      return;
    }
    if (
      document.activeElement instanceof HTMLTextAreaElement ||
      document.activeElement instanceof HTMLInputElement
    ) {
      return;
    }
    const key = event.key.toLowerCase();
    if (key === "o") {
      if (ocrBtn.hidden) {
        return;
      }
      event.preventDefault();
      activateOcr();
      return;
    }
    // 取字/二维码识别时，单键切回仍启用的标注工具。关闭的工具不响应。
    if (ocrModel?.active === true || qrModel?.active === true) {
      const next = annotationToolForKey(key);
      if (next && enabledAnnotation.has(next)) {
        event.preventDefault();
        editor?.setTool(next);
      }
    }
  });

  // 跨会话记忆:加载时读后端保存的上次样式、质量档位与「套用美化」普通选项
  // (读写失败均静默回退当前值)。取字是否出现由选区工具开关决定。
  const loadStyleDefaults = (): void => {
    void invoke<{
      annotationDefaults?: {
        color?: string;
        width?: number | null;
        textSize?: number | null;
        numberStart?: number;
      };
      export?: {
        quality?: ExportQuality;
        beautify?: Partial<BeautifyOptions>;
        applyBeautify?: boolean;
      };
    }>("get_ui_settings")
      .then((settings) => {
        const quality = settings?.export?.quality;
        if (quality === "high" || quality === "medium" || quality === "low") {
          saveQuality = quality;
          syncSaveQuality();
        }
        readBeautify(settings);
        editor?.setStyle(readAnnotationDefaults(settings));
        redraw();
      })
      .catch(() => undefined);
  };

  const readBeautify = (settings: {
    export?: { beautify?: Partial<BeautifyOptions>; applyBeautify?: boolean };
  }): void => {
    applyBeautify = settings.export?.applyBeautify === true;
    const stored = settings.export?.beautify;
    beautifyOptions = {
      preset: typeof stored?.preset === "string" && stored.preset.length > 0 ? stored.preset : DEFAULT_BEAUTIFY.preset,
      padding: typeof stored?.padding === "number" ? stored.padding : DEFAULT_BEAUTIFY.padding,
      radius: typeof stored?.radius === "number" ? stored.radius : DEFAULT_BEAUTIFY.radius,
      shadow: stored?.shadow !== false,
    };
  };

  const reloadAppearance = (): void => {
    void invoke<{
      export?: { beautify?: Partial<BeautifyOptions>; applyBeautify?: boolean };
    }>("get_ui_settings")
      .then((settings) => {
        readBeautify(settings);
        applyBeautifyChrome();
      })
      .catch(() => undefined);
  };
  loadStyleDefaults();

  let previewLoad = 0;
  const loadWriteback = async (): Promise<string | null> => {
    try {
      const label = await invoke<string | null>("get_pin_writeback");
      return typeof label === "string" && label.length > 0 ? label : null;
    } catch {
      return null;
    }
  };
  const loadPreview = (): void => {
    const generation = ++previewLoad;
    // R6:新帧对应新坐标系,变换历史与裁剪模式一并复位。
    transformUndo = false;
    transformRedo = false;
    cropping = false;
    cropStart = null;
    cropCurrent = null;
    void (async () => {
      // 再标注模式由会话决定(与帧同源):先取回写目标,再取同一会话的帧,
      // 避免普通截取预览被误判为回写模式。
      const writeback = await loadWriteback();
      if (generation !== previewLoad) return;
      writebackLabel = writeback;
      syncWritebackUi();
      let bytes: ArrayBuffer;
      try {
        bytes = await invoke<ArrayBuffer>("get_preview_frame");
      } catch (error) {
        if (generation === previewLoad) setNote(invokeError(error, t("preview.error.preview_missing")), "error");
        return;
      }
      if (generation !== previewLoad) return;
      if (bytes.byteLength <= 24) {
        setNoteKey("preview.note.image_incomplete", undefined, "error");
        return;
      }
      const activeWriteback = writeback !== null;
      const header = new DataView(bytes);
      // 16..20:0=设置关闭自动复制,1=已复制,2=自动复制失败。
      const copyState = header.getUint32(16, true);
      // 20..24:随帧带入的即时标注 JSON 长度;其后是 JSON,再接 PNG。
      const annotationsLength = header.getUint32(20, true);
      const pngOffset = 24 + annotationsLength;
      if (pngOffset > bytes.byteLength) {
        setNoteKey("preview.note.image_incomplete", undefined, "error");
        return;
      }
      let carried: Annotation[] = [];
      if (annotationsLength > 0) {
        try {
          const parsed: unknown = JSON.parse(
            new TextDecoder().decode(new Uint8Array(bytes, 24, annotationsLength)),
          );
          if (Array.isArray(parsed)) carried = parsed as Annotation[];
        } catch {
          carried = [];
        }
      }
      const payload: PreviewFrame = {
        width: header.getUint32(0, true),
        height: header.getUint32(4, true),
        scale: header.getFloat64(8, true),
      };
      frame = payload;
      canvas.width = payload.width;
      canvas.height = payload.height;
      const image = new Image();
      const imageUrl = URL.createObjectURL(new Blob([bytes.slice(pngOffset)], { type: "image/png" }));
      image.onload = () => {
        URL.revokeObjectURL(imageUrl);
        if (generation !== previewLoad) return;
        source = document.createElement("canvas");
        source.width = payload.width;
        source.height = payload.height;
        const sourceCtx = source.getContext("2d");
        if (!sourceCtx) {
          setNoteKey("preview.note.image_failed", undefined, "error");
          return;
        }
        sourceCtx.drawImage(image, 0, 0, payload.width, payload.height);
        // 选区即时标注并入可编辑列表:撤销栈从零开始,序号继续递增。
        editor?.setAnnotations(carried);
        void invoke<boolean>("take_pending_preview_ocr")
          .then((startOcr) => {
            if (generation === previewLoad && startOcr) {
              activateOcr();
            }
          })
          .catch(() => undefined);
        // R4:壳上的「识别二维码」在预览路径打开后同样自动开始识别。
        void invoke<boolean>("take_pending_preview_qr")
          .then((startQr) => {
            if (generation === previewLoad && startQr) {
              activateQr();
            }
          })
          .catch(() => undefined);
        // 携带说明与复制状态共用同一提示条:先登记携带前缀,再写复制状态,
        // 二者合并可见(review P3:分别写入会互相覆盖)。
        carriedNoteSource =
          carried.length > 0 ? { key: "preview.note.inline_annotations" } : null;
        if (activeWriteback) {
          setCopied({ key: "preview.copied_writeback", text: "" }, "feedback");
        } else if (copyState === 1) {
          setCopied({ key: "preview.copied_clean", text: "" }, "success");
        } else if (copyState === 2) {
          setCopied({ key: "preview.copied_manual_failed", text: "" }, "error");
        } else {
          setCopied({ key: "preview.copied_disabled", text: "" }, "feedback");
        }
        redraw();
      };
      image.onerror = () => {
        URL.revokeObjectURL(imageUrl);
        if (generation === previewLoad) setNoteKey("preview.note.image_failed", undefined, "error");
      };
      image.src = imageUrl;
    })();
  };

  // R6:变换后只重拉像素(不重置标注/取字模型态与提示):帧尺寸已由命令
  // 返回值更新,PNG 头与其一致。generation 与 loadPreview 共用同一序列。
  const loadFramePixels = (generation: number): Promise<boolean> =>
    new Promise((resolve) => {
      void (async () => {
        let bytes: ArrayBuffer;
        try {
          bytes = await invoke<ArrayBuffer>("get_preview_frame");
        } catch (error) {
          if (generation === previewLoad) {
            setNote(invokeError(error, t("preview.error.preview_missing")), "error");
          }
          resolve(false);
          return;
        }
        if (generation !== previewLoad) {
          resolve(false);
          return;
        }
        if (bytes.byteLength <= 24) {
          setNoteKey("preview.note.image_incomplete", undefined, "error");
          resolve(false);
          return;
        }
        const header = new DataView(bytes);
        const annotationsLength = header.getUint32(20, true);
        const pngOffset = 24 + annotationsLength;
        if (pngOffset > bytes.byteLength) {
          setNoteKey("preview.note.image_incomplete", undefined, "error");
          resolve(false);
          return;
        }
        const image = new Image();
        const imageUrl = URL.createObjectURL(
          new Blob([bytes.slice(pngOffset)], { type: "image/png" }),
        );
        image.onload = () => {
          URL.revokeObjectURL(imageUrl);
          if (generation !== previewLoad) {
            resolve(false);
            return;
          }
          source = document.createElement("canvas");
          source.width = canvas.width;
          source.height = canvas.height;
          const sourceCtx = source.getContext("2d");
          if (!sourceCtx) {
            setNoteKey("preview.note.image_failed", undefined, "error");
            resolve(false);
            return;
          }
          sourceCtx.drawImage(image, 0, 0, canvas.width, canvas.height);
          redraw();
          resolve(true);
        };
        image.onerror = () => {
          URL.revokeObjectURL(imageUrl);
          if (generation === previewLoad) {
            setNoteKey("preview.note.image_failed", undefined, "error");
          }
          resolve(false);
        };
        image.src = imageUrl;
      })();
    });

  // R6:应用重基结果:帧尺寸/标注/取字同一仿射变换;标注栈按 setAnnotations
  // 语义清空,撤销/重做可用性随命令返回值更新。
  const applyTransformResult = async (
    result: PreviewTransformResult,
    feedbackKey: CatalogKey,
  ): Promise<void> => {
    const generation = ++previewLoad;
    transformUndo = result.canUndo;
    transformRedo = result.canRedo;
    frame = { width: result.width, height: result.height, scale: frame?.scale ?? 1 };
    canvas.width = result.width;
    canvas.height = result.height;
    cropping = false;
    cropStart = null;
    cropCurrent = null;
    editor?.setAnnotations(result.annotations);
    ocrModel?.setDocument(result.ocr);
    await loadFramePixels(generation);
    if (generation !== previewLoad) {
      return;
    }
    syncCropUi();
    setNoteKey(feedbackKey, undefined, "success");
    redraw();
  };

  const performTransform = async (
    request: () => Promise<PreviewTransformResult>,
    feedbackKey: CatalogKey,
  ): Promise<void> => {
    if (busy) {
      setNoteKey("preview.note.busy");
      return;
    }
    if (!frame) {
      return;
    }
    editor?.commitText();
    busy = true;
    syncCropUi();
    setNoteKey("preview.note.transforming");
    try {
      const result = await request();
      await applyTransformResult(result, feedbackKey);
    } catch (error) {
      setNote(invokeError(error, t("preview.error.transform_fallback")), "error");
    } finally {
      busy = false;
      syncCropUi();
    }
  };

  const rotate = (direction: "left" | "right"): Promise<void> =>
    performTransform(
      () =>
        invoke<PreviewTransformResult>("rotate_preview", {
          direction,
          annotations: editor?.annotations() ?? [],
        }),
      direction === "left" ? "preview.note.rotated_left" : "preview.note.rotated_right",
    );

  // R6:确认裁剪:前端先按同一最小边长提示,越界/过小由 Rust 拒绝并本地化说明;
  // 失败保持裁剪模式与选区,取消(exitCrop)不产生任何副作用。
  const confirmCrop = async (): Promise<void> => {
    if (!cropping || busy) {
      return;
    }
    const rect = cropRect();
    if (!rect) {
      return;
    }
    if (rect.width < MIN_CROP_EDGE || rect.height < MIN_CROP_EDGE) {
      setNoteKey("error.preview.crop_too_small", { detail: MIN_CROP_EDGE }, "error");
      return;
    }
    editor?.commitText();
    busy = true;
    syncCropUi();
    setNoteKey("preview.note.transforming");
    try {
      const result = await invoke<PreviewTransformResult>("crop_preview", {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
        annotations: editor?.annotations() ?? [],
      });
      exitCrop();
      await applyTransformResult(result, "preview.note.cropped");
    } catch (error) {
      setNote(invokeError(error, t("preview.error.transform_fallback")), "error");
    } finally {
      busy = false;
      syncCropUi();
    }
  };

  // R6:撤销/重做回退(标注编辑器栈为空时由 Ctrl+Z/Y 与工具条按钮触发)。
  const undoTransform = (): boolean => {
    if (!transformUndo || busy) {
      return false;
    }
    void performTransform(
      () => invoke<PreviewTransformResult>("undo_preview_transform"),
      "preview.note.transform_undone",
    );
    return true;
  };

  const redoTransform = (): boolean => {
    if (!transformRedo || busy || (editor?.canUndo() ?? false)) {
      return false;
    }
    void performTransform(
      () => invoke<PreviewTransformResult>("redo_preview_transform"),
      "preview.note.transform_redone",
    );
    return true;
  };

  void listen("export-appearance-changed", () => {
    reloadAppearance();
  });

  const chromeObserver = new ResizeObserver(() => {
    applyBeautifyChrome();
  });
  chromeObserver.observe(frameEl);

  void listen("preview-reload", () => {
    // R6:新帧的变换历史与裁剪模式随 loadPreview 复位;先退出裁剪界面态。
    transformUndo = false;
    transformRedo = false;
    cropping = false;
    cropStart = null;
    cropCurrent = null;
    // 新帧可能带入选区即时标注(R21):列表随帧在 loadPreview 中恢复,
    // 这里先清空避免旧编辑态残留。取字/二维码模型也不保留上一帧的结果。
    ocrModel?.reset();
    qrModel?.reset();
    editor?.setAnnotations([]);
    editor?.cancelText();
    syncCropUi();
    // 携带说明随新帧重算(image.onload);先清空,避免加载失败时残留旧前缀。
    carriedNoteSource = null;
    // 样式默认只在首次加载，不在 reload 重置。工具开关每次重载都重读。
    loadRegionTools();
    loadPreview();
  });
  loadPreview();

  // 语言切换:静态标签由 main 的 applyTranslations 更新;这里刷新标注模块与
  // 取字/二维码模型组合出的本地化标签,并重渲染来源可解析的提示条。
  const refreshOptionLabels = (): void => {
    editor?.refreshLabels();
    ocrModel?.refreshLabels();
    qrModel?.refreshLabels();
    saveQualityRoot.querySelectorAll<HTMLButtonElement>("[data-save-quality]").forEach((button) => {
      const option = SAVE_QUALITIES.find((item) => item.value === button.dataset.saveQuality);
      if (option) {
        button.textContent = t(option.labelKey);
        button.dataset.tooltip = t(option.titleKey);
      }
    });
  };

  syncSaveQuality();
  return () => {
    refreshOptionLabels();
    renderNote();
    placeCropBar();
    requestOverflowLayout();
  };
}

type Point = { x: number; y: number };

interface BeautifyOptions {
  preset: string;
  padding: number;
  radius: number;
  shadow: boolean;
}

interface BeautifyLayout {
  padding: number;
  radius: number;
  shadowMargin: number;
  origin: number;
  outputWidth: number;
  outputHeight: number;
}

const BEAUTIFY_MAX_PADDING = 240;
const BEAUTIFY_MAX_RADIUS = 160;

const DEFAULT_BEAUTIFY: BeautifyOptions = {
  preset: "paper",
  padding: 32,
  radius: 16,
  shadow: true,
};

/// 与 `beautify.rs` 的 PRESETS 同色。渐变方向为左上到右下。
const BEAUTIFY_BACKGROUNDS: Record<string, string> = {
  paper: "#f4f1ea",
  slate: "#334155",
  ink: "#0b1220",
  dawn: "linear-gradient(to bottom right, #fde68a, #fb7185)",
  ocean: "linear-gradient(to bottom right, #38bdf8, #1e3a8a)",
  dusk: "linear-gradient(to bottom right, #312e81, #f472b6)",
};

function beautifyBackground(preset: string): string {
  return BEAUTIFY_BACKGROUNDS[preset] ?? BEAUTIFY_BACKGROUNDS.paper;
}

/// 与 Rust `beautify::shadow_margin` / `layout` 同一整数公式。
function beautifyShadowMargin(radius: number, shadow: boolean): number {
  if (!shadow) {
    return 0;
  }
  return Math.min(48, Math.max(12, 12 + Math.floor(radius / 2)));
}

function beautifyLayout(width: number, height: number, options: BeautifyOptions): BeautifyLayout {
  const padding = Math.min(BEAUTIFY_MAX_PADDING, Math.max(0, Math.floor(options.padding)));
  const maxRadius = Math.min(BEAUTIFY_MAX_RADIUS, Math.floor(width / 2), Math.floor(height / 2));
  const radius = Math.min(Math.max(0, maxRadius), Math.max(0, Math.floor(options.radius)));
  const shadowMargin = beautifyShadowMargin(radius, options.shadow);
  const origin = padding + shadowMargin;
  return {
    padding,
    radius,
    shadowMargin,
    origin,
    outputWidth: width + origin * 2,
    outputHeight: height + origin * 2,
  };
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
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
