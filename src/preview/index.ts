import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { t, type CatalogKey } from "../i18n";
import "./preview.css";

interface PreviewFrame {
  width: number;
  height: number;
  scale: number;
}

type Tool =
  | "arrow"
  | "rect"
  | "ellipse"
  | "line"
  | "mosaic"
  | "blur"
  | "highlighter"
  | "pen"
  | "number"
  | "text"
  | "ocr";

// 拖拽式绘制工具：按下为起点、拖动出范围、松开入栈。
type DragTool = "arrow" | "rect" | "ellipse" | "line" | "mosaic" | "blur";
// 自由绘制工具：按住移动采集折线点，松开入栈。
type FreehandTool = "highlighter" | "pen";

type Point = { x: number; y: number };

type Bounds = { minX: number; minY: number; maxX: number; maxY: number };

// add/remove/replace(move、retext)三类动作构成 undo/redo 栈;replace 存前后值快照。
type EditAction =
  | { kind: "add"; index: number; op: Annotation }
  | { kind: "remove"; index: number; op: Annotation }
  | { kind: "replace"; index: number; before: Annotation; after: Annotation };

interface MoveState {
  index: number;
  before: Annotation;
  grab: Point;
  moved: boolean;
}

type Annotation =
  | {
      type: "arrow";
      from: Point;
      to: Point;
      color: string;
      strokeWidth: number | null;
    }
  | {
      type: "rect";
      x: number;
      y: number;
      width: number;
      height: number;
      color: string;
      strokeWidth: number | null;
    }
  | { type: "mosaic"; x: number; y: number; width: number; height: number; block: number }
  | { type: "text"; x: number; y: number; text: string; size: number; color: string }
  | {
      type: "ellipse";
      x: number;
      y: number;
      width: number;
      height: number;
      color: string;
      strokeWidth: number | null;
    }
  | { type: "line"; from: Point; to: Point; color: string; strokeWidth: number | null }
  | { type: "number"; x: number; y: number; value: number; size: number; color: string }
  | { type: "highlighter"; points: Point[]; color: string; strokeWidth: number | null }
  | { type: "pen"; points: Point[]; color: string; strokeWidth: number | null }
  | { type: "blur"; x: number; y: number; width: number; height: number; sigma: number };

interface TextSpan {
  text: string;
  x: number;
  y: number;
  width: number;
  height: number;
}

interface OcrDocument {
  spans: TextSpan[];
  fullText: string;
}

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

const FALLBACK_STROKE = "#e11d48";
const FALLBACK_OCR_HL = "#0ea5e9";
const FALLBACK_OCR_HL_STRONG = "#0369a1";
const FALLBACK_SELECT = "#2563eb";
const TEXT_FONT_STACK = '"Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif';

// 与 Rust annotate::HIGHLIGHTER_ALPHA / MIN_BLUR_SIGMA / MAX_BLUR_SIGMA 对齐。
const HIGHLIGHTER_ALPHA = 0.38;
const BLUR_SIGMA_MIN = 3;
const BLUR_SIGMA_MAX = 48;
const MIN_NUMBER_START = 1;
const MAX_NUMBER_START = 999;
const MIN_DRAW_SIZE = 3;

// 与 Rust parse_hex_color 对齐：接受 #rgb / #rrggbb / #rrggbbaa，其余形式回退默认色。
const HEX_COLOR_RE = /^#(?:[0-9a-f]{3}|[0-9a-f]{6}|[0-9a-f]{8})$/i;

// 样式预设：颜色含现行玫红；线宽/字号档位为逻辑值，绘制时乘 scale 并 clamp 2..8（线宽）。
const STYLE_COLORS = ["#e11d48", "#2563eb", "#f59e0b", "#10b981", "#111827"];
const STYLE_WIDTHS: Array<{ value: number; labelKey: CatalogKey }> = [
  { value: 2, labelKey: "preview.style.width_thin" },
  { value: 3, labelKey: "preview.style.width_normal" },
  { value: 5, labelKey: "preview.style.width_thick" },
];
const STYLE_TEXT_SIZES: Array<{ value: number; labelKey: CatalogKey }> = [
  { value: 12, labelKey: "preview.style.text_small" },
  { value: 16, labelKey: "preview.style.text_medium" },
  { value: 22, labelKey: "preview.style.text_large" },
];

const ICONS: Record<Exclude<Tool, "ocr"> | "undo" | "style", string> = {
  arrow: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 12.5 12.5 3.5M7 3.5h5.5V9" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  rect: `<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="3" y="4" width="10" height="8" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.7"/></svg>`,
  ellipse: `<svg viewBox="0 0 16 16" aria-hidden="true"><ellipse cx="8" cy="8" rx="5.4" ry="4" fill="none" stroke="currentColor" stroke-width="1.7"/></svg>`,
  line: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.8 12.2 12.2 3.8" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><circle cx="3.8" cy="12.2" r="1.3" fill="currentColor"/><circle cx="12.2" cy="3.8" r="1.3" fill="currentColor"/></svg>`,
  mosaic: `<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="2" width="5" height="5" fill="currentColor"/><rect x="9" y="2" width="5" height="5" fill="currentColor" opacity="0.45"/><rect x="2" y="9" width="5" height="5" fill="currentColor" opacity="0.65"/><rect x="9" y="9" width="5" height="5" fill="currentColor" opacity="0.28"/></svg>`,
  blur: `<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="5.6" fill="currentColor" opacity="0.18"/><circle cx="8" cy="8" r="3.6" fill="currentColor" opacity="0.32"/><circle cx="8" cy="8" r="1.8" fill="currentColor" opacity="0.6"/></svg>`,
  highlighter: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 2.6h4.6a1 1 0 0 1 1 1V9H6z" fill="currentColor" opacity="0.55"/><path d="M5.2 9h7.2v1.6a1 1 0 0 1-1 1H6.2a1 1 0 0 1-1-1z" fill="currentColor" opacity="0.85"/><path d="M6 12.4h7" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/><path d="M4.5 2.6h1.5v6.4H4.5z" fill="currentColor" opacity="0.4"/></svg>`,
  pen: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 13c2.2-1.2 2.6-3 3.8-4.9C8 6 9.4 4.2 12.8 2.4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><path d="M12.8 2.4c-2 1-3.4 2.4-4.5 4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>`,
  number: `<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="5.6" fill="none" stroke="currentColor" stroke-width="1.6"/><text x="8" y="11.4" text-anchor="middle" font-size="8.4" font-weight="700" fill="currentColor" font-family="sans-serif">1</text></svg>`,
  text: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4.2h8M8 4.2v8.2M5.5 12.4h5" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>`,
  undo: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 7h6.2a3 3 0 1 1 0 6H9" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><path d="M4 7 6.4 4.6M4 7l2.4 2.4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>`,
  style: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 1.8a6.2 6.2 0 0 0 0 12.4c.9 0 1.4-.6 1.4-1.3 0-1.1 1-1.4 2.2-1.4h1.1c.9 0 1.5-.7 1.5-1.9A6.2 6.2 0 0 0 8 1.8Z" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="5.2" cy="6.4" r="1" fill="currentColor"/><circle cx="8.6" cy="4.8" r="1" fill="currentColor"/><circle cx="11.4" cy="7.2" r="1" fill="currentColor"/></svg>`,
};

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
      <button type="button" class="preview-close" data-action="close" data-i18n-aria-label="preview.close" aria-label="关闭">关闭</button>
    </header>
    <div class="preview-toolbar">
      <div class="preview-tools" role="toolbar" data-i18n-aria-label="preview.toolbar_group" aria-label="标注" data-tauri-drag-region="false">
        <button type="button" data-tool="arrow" data-i18n-title="preview.tool.arrow_title" data-i18n-aria-label="preview.tool.arrow" title="箭头 (A)" aria-label="箭头">${ICONS.arrow}</button>
        <button type="button" data-tool="rect" data-i18n-title="preview.tool.rect_title" data-i18n-aria-label="preview.tool.rect" title="框 (R)" aria-label="框">${ICONS.rect}</button>
        <button type="button" data-tool="ellipse" data-i18n-title="preview.tool.ellipse_title" data-i18n-aria-label="preview.tool.ellipse" title="椭圆 (E)" aria-label="椭圆">${ICONS.ellipse}</button>
        <button type="button" data-tool="line" data-i18n-title="preview.tool.line_title" data-i18n-aria-label="preview.tool.line" title="直线 (L)" aria-label="直线">${ICONS.line}</button>
        <button type="button" data-tool="mosaic" data-i18n-title="preview.tool.mosaic_title" data-i18n-aria-label="preview.tool.mosaic" title="马赛克 (M)" aria-label="马赛克">${ICONS.mosaic}</button>
        <button type="button" data-tool="blur" data-i18n-title="preview.tool.blur_title" data-i18n-aria-label="preview.tool.blur" title="高斯模糊 (B)" aria-label="高斯模糊">${ICONS.blur}</button>
        <button type="button" data-tool="highlighter" data-i18n-title="preview.tool.highlighter_title" data-i18n-aria-label="preview.tool.highlighter" title="荧光笔 (H)" aria-label="荧光笔">${ICONS.highlighter}</button>
        <button type="button" data-tool="pen" data-i18n-title="preview.tool.pen_title" data-i18n-aria-label="preview.tool.pen" title="画笔 (P)" aria-label="画笔">${ICONS.pen}</button>
        <button type="button" data-tool="number" data-i18n-title="preview.tool.number_title" data-i18n-aria-label="preview.tool.number" title="序号 (N)" aria-label="序号">${ICONS.number}</button>
        <button type="button" data-tool="text" data-i18n-title="preview.tool.text_title" data-i18n-aria-label="preview.tool.text" title="文字框 (T)" aria-label="文字框" class="tool-text">${ICONS.text}<span data-i18n="preview.tool.text_short">文字</span></button>
        <button type="button" data-action="undo" data-i18n-title="preview.tool.undo_title" data-i18n-aria-label="preview.tool.undo" title="撤销 (Ctrl+Z)" aria-label="撤销">${ICONS.undo}</button>
        <div class="preview-style" data-style-root>
          <button type="button" data-action="style" data-i18n-title="preview.tool.style_title" data-i18n-aria-label="preview.tool.style_title" title="标注样式" aria-label="标注样式" aria-haspopup="true">${ICONS.style}</button>
          <div class="preview-style-panel" data-style-panel hidden>
            <div class="style-group">
              <span class="style-label" data-i18n="preview.style.color">颜色</span>
              <div class="style-options" role="group" data-i18n-aria-label="preview.style.color_group" aria-label="标注颜色">
                ${STYLE_COLORS.map(
                  (color) =>
                    `<button type="button" data-style-color="${color}" style="--swatch:${color}" title="${color}" aria-label="${t("preview.style.color_aria", { color })}"></button>`,
                ).join("")}
              </div>
            </div>
            <div class="style-group">
              <span class="style-label" data-i18n="preview.style.width">线宽</span>
              <div class="style-options" role="group" data-i18n-aria-label="preview.style.width" aria-label="线宽">
                ${STYLE_WIDTHS.map(
                  ({ value, labelKey }) =>
                    `<button type="button" data-style-width="${value}" title="${t("preview.style.option_title", { label: t(labelKey), value })}">${t(labelKey)}</button>`,
                ).join("")}
              </div>
            </div>
            <div class="style-group">
              <span class="style-label" data-i18n="preview.style.text_size">字号</span>
              <div class="style-options" role="group" data-i18n-aria-label="preview.style.text_size_group" aria-label="文字字号">
                ${STYLE_TEXT_SIZES.map(
                  ({ value, labelKey }) =>
                    `<button type="button" data-style-text-size="${value}" title="${t("preview.style.option_title", { label: t(labelKey), value })}">${t(labelKey)}</button>`,
                ).join("")}
              </div>
            </div>
            <div class="style-group">
              <span class="style-label" data-i18n="preview.style.number">序号</span>
              <div class="style-options" role="group" data-i18n-aria-label="preview.style.number_group" aria-label="序号起始值">
                <input
                  type="number"
                  class="style-number-start"
                  data-style-number-start
                  min="${MIN_NUMBER_START}"
                  max="${MAX_NUMBER_START}"
                  step="1"
                  value="${MIN_NUMBER_START}"
                  data-i18n-aria-label="preview.style.number_group"
                  aria-label="序号起始值"
                  data-i18n-title="preview.style.number_title"
                  title="序号起始值 (1–999)"
                />
              </div>
            </div>
          </div>
        </div>
      </div>
      <div class="preview-actions" data-tauri-drag-region="false">
        <button type="button" data-tool="ocr" data-i18n-title="preview.action.ocr_title" data-i18n="preview.action.ocr" title="取字 (O)" data-tauri-drag-region="false">取字</button>
        <button type="button" data-action="copy-ocr-all" hidden data-tauri-drag-region="false" data-i18n="preview.action.copy_all">复制全部</button>
        <button type="button" data-action="pin" data-i18n-title="preview.action.pin_title" data-i18n="preview.action.pin" title="贴图" data-tauri-drag-region="false">贴图</button>
        <button type="button" data-action="update-pin" data-i18n-title="preview.action.update_pin_title" data-i18n="preview.action.update_pin" title="更新贴图：确认后写回来源贴图" hidden data-tauri-drag-region="false">更新贴图</button>
        <div class="style-group" data-save-quality-root>
          <span class="style-label" data-i18n="preview.quality.label">质量</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.quality.group" aria-label="保存质量">
            ${SAVE_QUALITIES.map(
              ({ value, labelKey, titleKey }) =>
                `<button type="button" data-save-quality="${value}" title="${t(titleKey)}">${t(labelKey)}</button>`,
            ).join("")}
          </div>
        </div>
        <button type="button" data-action="save" data-i18n-title="preview.action.save_title" data-i18n="preview.action.save" title="保存 (Ctrl+S)：扩展名决定格式 PNG/JPEG/WebP" data-tauri-drag-region="false">保存</button>
        <button type="button" class="primary" data-action="copy" data-i18n-title="preview.action.copy_title" data-i18n="preview.action.copy" title="复制 (Ctrl+C)" data-tauri-drag-region="false">复制</button>
      </div>
    </div>
    <div class="preview-stage">
      <div class="preview-frame">
        <canvas></canvas>
        <textarea class="preview-text" rows="2" spellcheck="false" data-i18n-placeholder="preview.text_placeholder" placeholder="在此输入汉字"></textarea>
      </div>
    </div>
    <div class="preview-context" data-context-menu hidden>
      <button type="button" data-action="delete-annotation" data-i18n="preview.action.delete_annotation">删除标注</button>
    </div>
  `;

  const canvas = root.querySelector("canvas");
  const note = root.querySelector(".preview-note");
  const editor = root.querySelector(".preview-text");
  const undoBtn = root.querySelector("[data-action=undo]");
  const frameEl = root.querySelector(".preview-frame");
  const copyAllBtn = root.querySelector("[data-action=copy-ocr-all]");
  const ocrBtn = root.querySelector("[data-tool=ocr]");
  const pinBtn = root.querySelector("[data-action=pin]");
  const updatePinBtn = root.querySelector("[data-action=update-pin]");
  const saveQualityRoot = root.querySelector("[data-save-quality-root]");
  const styleRoot = root.querySelector("[data-style-root]");
  const stylePanel = root.querySelector("[data-style-panel]");
  const styleBtn = root.querySelector("[data-action=style]");
  const contextMenu = root.querySelector("[data-context-menu]");
  const numberStartInput = root.querySelector("[data-style-number-start]");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(note instanceof HTMLElement) ||
    !(editor instanceof HTMLTextAreaElement) ||
    !(undoBtn instanceof HTMLButtonElement) ||
    !(frameEl instanceof HTMLElement) ||
    !(copyAllBtn instanceof HTMLButtonElement) ||
    !(ocrBtn instanceof HTMLButtonElement) ||
    !(pinBtn instanceof HTMLButtonElement) ||
    !(updatePinBtn instanceof HTMLButtonElement) ||
    !(saveQualityRoot instanceof HTMLElement) ||
    !(styleRoot instanceof HTMLElement) ||
    !(stylePanel instanceof HTMLElement) ||
    !(styleBtn instanceof HTMLButtonElement) ||
    !(contextMenu instanceof HTMLElement) ||
    !(numberStartInput instanceof HTMLInputElement)
  ) {
    return () => undefined;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return () => undefined;
  }

  const rootStyle = getComputedStyle(root);
  const strokeColor = resolveCanvasColor(rootStyle.getPropertyValue("--stroke"), FALLBACK_STROKE);
  const selectColor = resolveCanvasColor(rootStyle.getPropertyValue("--accent"), FALLBACK_SELECT);
  const ocrHl = resolveCanvasColor(rootStyle.getPropertyValue("--ocr-hl"), FALLBACK_OCR_HL);
  const ocrHlStrong = resolveCanvasColor(rootStyle.getPropertyValue("--ocr-hl-strong"), FALLBACK_OCR_HL_STRONG);
  const ocrColors = {
    hl: ocrHl,
    hlStrong: ocrHlStrong,
    fillWeak: withAlpha(ocrHl, 0.12),
    fillStrong: withAlpha(ocrHl, 0.38),
    fillRubber: withAlpha(ocrHl, 0.08),
    strokeWeak: withAlpha(ocrHl, 0.55),
    strokeStrong: withAlpha(ocrHlStrong, 0.95),
    strokeRubber: withAlpha(ocrHlStrong, 0.9),
  };

  let frame: PreviewFrame | null = null;
  let source: HTMLCanvasElement | null = null;
  let tool: Tool = "arrow";
  let annotations: Annotation[] = [];
  // R21:选区即时标注随帧带入的初始图元;新帧到达时恢复到该列表(可继续编辑)。
  let carriedAnnotations: Annotation[] = [];
  const undoStack: EditAction[] = [];
  const redoStack: EditAction[] = [];
  let selected: number | null = null;
  let moving = false;
  let moveState: MoveState | null = null;
  let editTarget: number | null = null;
  let dragging = false;
  let start: Point | null = null;
  let current: Point | null = null;
  let freehand: Point[] = [];
  let editorOrigin: Point | null = null;
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
  let ocrDoc: OcrDocument | null = null;
  let ocrSelected: number[] = [];
  let ocrDragging = false;
  let ocrStart: Point | null = null;
  let ocrCurrent: Point | null = null;
  let ocrGen = 0;
  let composing = false;
  // 功能入口开关(设置页 features.ocrEntry / features.pinEntry):
  // 关闭时取字按钮隐藏、O 键停用;关闭贴图后隐藏预览工具条贴图按钮。
  let ocrEntryEnabled = true;
  let pinEntryEnabled = true;
  // 贴图再标注(R9):非空表示本会话由贴图进入,确认后写回该 label。
  let writebackLabel: string | null = null;
  let styleColor = FALLBACK_STROKE;
  let styleWidth: number | null = null;
  let styleTextBase: number | null = null;
  let numberStart = MIN_NUMBER_START;
  // 编辑会话内已放置的序号数：下一次放置 = numberStart + numberPlaced。
  let numberPlaced = 0;
  let saveQuality: ExportQuality = "high";
  editor.classList.remove("is-open");

  const mosaicBlock = (): number => Math.max(8, Math.round(12 * Math.max(frame?.scale ?? 1, 1)));
  const textSize = (): number => {
    const dpi = Math.max(frame?.scale ?? 1, 1);
    const longestEdge = Math.max(frame?.width ?? 0, frame?.height ?? 0);
    return Math.max(10, Math.round((styleTextBase ?? 16) * Math.max(dpi, longestEdge / 1920)));
  };
  // 与 Rust raster resolve_stroke 同一数值推导：逻辑档位 × scale 后 clamp 2..8。
  const strokeFor = (opWidth: number | null): number =>
    Math.min(8, Math.max(2, (opWidth ?? 3) * Math.max(frame?.scale ?? 1, 1)));
  const colorFor = (opColor: string): string =>
    HEX_COLOR_RE.test(opColor) ? opColor : strokeColor;
  const frameScale = (): number => Math.max(frame?.scale ?? 1, 1);
  const annotationStyle = (op: Annotation): { color: string; lineWidth: number } => {
    if (op.type === "mosaic" || op.type === "blur") {
      return { color: strokeColor, lineWidth: strokeFor(null) };
    }
    if (op.type === "text" || op.type === "number") {
      return { color: colorFor(op.color), lineWidth: strokeFor(null) };
    }
    return { color: colorFor(op.color), lineWidth: strokeFor(op.strokeWidth) };
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

  const setTool = (next: Tool): void => {
    commitEditor();
    tool = next;
    root.dataset.tool = next;
    root.querySelectorAll("[data-tool]").forEach((button) => {
      button.classList.toggle("active", button.getAttribute("data-tool") === next);
    });
    selected = null;
    moving = false;
    moveState = null;
    dragging = false;
    start = null;
    current = null;
    freehand = [];
    ocrSelected = [];
    ocrCurrent = null;
    ocrStart = null;
    ocrDragging = false;
    copyAllBtn.hidden = next !== "ocr" || !ocrDoc || ocrDoc.spans.length === 0;
    if (next === "ocr") {
      if (!ocrDoc) {
        void runOcr();
      } else if (!note.classList.contains("is-error")) {
        setNoteKey("preview.note.ocr_hint");
      }
    } else if (next === "text") {
      setNoteKey("preview.note.text_hint");
    } else if (next === "number") {
      setNoteKey("preview.note.number_hint", { start: numberStart });
    } else if (next === "pen") {
      setNoteKey("preview.note.pen_hint");
    } else if (next === "highlighter") {
      setNoteKey("preview.note.highlighter_hint");
    } else if (next === "blur") {
      setNoteKey("preview.note.blur_hint");
    } else if (!note.classList.contains("is-error")) {
      setNoteSource(copiedSource, copiedKind);
    }
    redraw();
  };

  const syncUndo = (): void => {
    undoBtn.disabled = undoStack.length === 0 && !editorOpen();
  };

  const syncStylePanel = (): void => {
    const activeColor = styleColor.toLowerCase();
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-style-color]").forEach((button) => {
      button.classList.toggle("active", (button.dataset.styleColor ?? "").toLowerCase() === activeColor);
    });
    const activeWidth = String(styleWidth ?? 3);
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-style-width]").forEach((button) => {
      button.classList.toggle("active", button.dataset.styleWidth === activeWidth);
    });
    const activeTextSize = String(styleTextBase ?? 16);
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-style-text-size]").forEach((button) => {
      button.classList.toggle("active", button.dataset.styleTextSize === activeTextSize);
    });
    if (numberStartInput.value !== String(numberStart)) {
      numberStartInput.value = String(numberStart);
    }
  };

  const syncSaveQuality = (): void => {
    saveQualityRoot.querySelectorAll<HTMLButtonElement>("[data-save-quality]").forEach((button) => {
      button.classList.toggle("active", button.dataset.saveQuality === saveQuality);
    });
  };

  // 再标注模式:隐藏「贴图」(避免从编辑内容再开新贴图),显示「更新贴图」。
  const syncWritebackUi = (): void => {
    updatePinBtn.hidden = writebackLabel === null;
    pinBtn.hidden = !pinEntryEnabled || writebackLabel !== null;
  };

  const persistStyle = (): void => {
    void invoke<{ notice: string | null }>("set_annotation_defaults", {
      defaults: {
        color: styleColor,
        width: styleWidth,
        textSize: styleTextBase,
        numberStart,
      },
    })
      .then((result) => {
        if (result?.notice) {
          setNote(result.notice, "error");
        }
      })
      .catch(() => {
        setNoteKey("preview.note.style_not_saved", undefined, "error");
      });
  };

  const toggleStylePanel = (open?: boolean): void => {
    const next = open ?? stylePanel.hidden;
    if (next) {
      // 开样式面板前先提交编辑器并取消编辑选中,避免两套编辑态互相干扰。
      commitEditor();
      selected = null;
      moving = false;
      moveState = null;
      redraw();
    }
    stylePanel.hidden = !next;
    styleBtn.classList.toggle("active", next);
    if (next) {
      syncStylePanel();
    }
  };

  stylePanel.addEventListener("click", (event) => {
    const button = event.target instanceof Element ? event.target.closest("button") : null;
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    const nextColor = button.dataset.styleColor;
    const nextWidth = button.dataset.styleWidth;
    const nextTextSize = button.dataset.styleTextSize;
    if (nextColor) {
      styleColor = nextColor;
    } else if (nextWidth !== undefined) {
      styleWidth = Number(nextWidth);
    } else if (nextTextSize !== undefined) {
      styleTextBase = Number(nextTextSize);
    } else {
      return;
    }
    syncStylePanel();
    persistStyle();
    redraw();
  });

  // 起始序号调整后本次会话的连续递增从新起点重算；空/非法输入回退当前值。
  numberStartInput.addEventListener("change", () => {
    const parsed = Number(numberStartInput.value);
    if (!Number.isFinite(parsed)) {
      numberStartInput.value = String(numberStart);
      return;
    }
    const next = clamp(Math.round(parsed), MIN_NUMBER_START, MAX_NUMBER_START);
    numberStartInput.value = String(next);
    if (next === numberStart) {
      return;
    }
    numberStart = next;
    // 起始值变化后从新起点重算:已有序号(含选区带入)中不小于新起点的
    // 数量决定下一个值,避免与既有编号重复。
    numberPlaced = annotations.filter(
      (op) => op.type === "number" && op.value >= numberStart,
    ).length;
    persistStyle();
    if (tool === "number" && !note.classList.contains("is-error")) {
      setNoteKey("preview.note.number_hint", { start: numberStart });
    }
  });

  document.addEventListener("click", (event) => {
    if (!contextMenu.hidden && !(event.target instanceof Node && contextMenu.contains(event.target))) {
      hideContextMenu();
    }
    if (stylePanel.hidden) {
      return;
    }
    if (event.target instanceof Node && styleRoot.contains(event.target)) {
      return;
    }
    toggleStylePanel(false);
  });

  const physicalPoint = (event: MouseEvent): Point => {
    const rect = canvas.getBoundingClientRect();
    const scaleX = canvas.width / Math.max(rect.width, 1);
    const scaleY = canvas.height / Math.max(rect.height, 1);
    return {
      x: clamp((event.clientX - rect.left) * scaleX, 0, canvas.width),
      y: clamp((event.clientY - rect.top) * scaleY, 0, canvas.height),
    };
  };

  const cssScale = (): { x: number; y: number } => {
    const rect = canvas.getBoundingClientRect();
    return {
      x: rect.width / Math.max(canvas.width, 1),
      y: rect.height / Math.max(canvas.height, 1),
    };
  };

  // 几何命中:从最上层(数组末尾)往下找,箭头/直线/自由绘制按线段距离,
  // 其余按包围盒;容差随帧缩放。
  const hitAnnotation = (point: Point): number => {
    const tol = Math.max(6, Math.round(frameScale() * 6));
    for (let i = annotations.length - 1; i >= 0; i -= 1) {
      const op = annotations[i];
      if (op.type === "arrow") {
        const lineWidth = annotationStyle(op).lineWidth;
        if (
          distToSegment(point, op.from, op.to) <= tol + lineWidth / 2 ||
          Math.hypot(point.x - op.to.x, point.y - op.to.y) <= tol + 8
        ) {
          return i;
        }
      } else if (op.type === "line") {
        const lineWidth = annotationStyle(op).lineWidth;
        if (distToSegment(point, op.from, op.to) <= tol + lineWidth / 2) {
          return i;
        }
      } else if (op.type === "pen" || op.type === "highlighter") {
        const lineWidth = annotationStyle(op).lineWidth;
        if (polylineDistance(point, op.points) <= tol + lineWidth / 2) {
          return i;
        }
      } else {
        const b = annotationBounds(ctx, op);
        if (
          point.x >= b.minX - tol &&
          point.x <= b.maxX + tol &&
          point.y >= b.minY - tol &&
          point.y <= b.maxY + tol
        ) {
          return i;
        }
      }
    }
    return -1;
  };

  const hideContextMenu = (): void => {
    contextMenu.hidden = true;
  };

  const showContextMenu = (clientX: number, clientY: number): void => {
    const rootRect = root.getBoundingClientRect();
    contextMenu.hidden = false;
    const menuRect = contextMenu.getBoundingClientRect();
    const left = clamp(clientX - rootRect.left, 4, Math.max(4, rootRect.width - menuRect.width - 4));
    const top = clamp(clientY - rootRect.top, 4, Math.max(4, rootRect.height - menuRect.height - 4));
    contextMenu.style.left = `${left}px`;
    contextMenu.style.top = `${top}px`;
  };

  const deleteSelected = (): void => {
    if (editorOpen() || selected === null) {
      return;
    }
    const index = selected;
    const op = annotations[index];
    selected = null;
    moving = false;
    moveState = null;
    if (!op) {
      redraw();
      return;
    }
    pushAction({ kind: "remove", index, op });
    redraw();
    syncUndo();
  };

  const redraw = (): void => {
    if (!source) {
      return;
    }
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(source, 0, 0);
    for (const op of annotations) {
      const s = annotationStyle(op);
      paint(ctx, op, s.color, s.lineWidth);
    }
    if (selected !== null && annotations[selected]) {
      paintSelectionBox(ctx, annotations[selected], selectColor, Math.max(frame?.scale ?? 1, 1));
    }
    if (dragging && start && current && isDragTool(tool)) {
      const op = draft(tool, start, current, mosaicBlock(), styleColor, styleWidth);
      if (op) {
        const s = annotationStyle(op);
        paint(ctx, op, s.color, s.lineWidth);
      }
    }
    if (dragging && freehand.length >= 2 && isFreehandTool(tool)) {
      const op: Annotation = {
        type: tool,
        points: freehand,
        color: styleColor,
        strokeWidth: styleWidth,
      };
      const s = annotationStyle(op);
      paint(ctx, op, s.color, s.lineWidth);
    }
    if (tool === "ocr" && ocrDoc) {
      const rubber =
        ocrDragging && ocrStart && ocrCurrent ? normalizeRect(ocrStart, ocrCurrent) : null;
      paintOcr(ctx, ocrDoc.spans, ocrSelected, rubber, ocrColors);
    }
  };

  const showEditor = (origin: Point, text: string, baseFontSize: number): void => {
    editorOrigin = origin;
    const scale = cssScale();
    const canvasRect = canvas.getBoundingClientRect();
    const frameRect = frameEl.getBoundingClientRect();
    const editorStyle = window.getComputedStyle(editor);
    const insetX = parseFloat(editorStyle.paddingLeft) + parseFloat(editorStyle.borderLeftWidth);
    const insetY = parseFloat(editorStyle.paddingTop) + parseFloat(editorStyle.borderTopWidth);
    const fontSize = baseFontSize * scale.y;
    editor.value = text;
    editor.style.left = `${canvasRect.left - frameRect.left + origin.x * scale.x - insetX}px`;
    editor.style.top = `${canvasRect.top - frameRect.top + origin.y * scale.y - insetY}px`;
    editor.style.fontSize = `${fontSize}px`;
    editor.style.width = `${Math.max(160, fontSize * 12)}px`;
    editor.classList.add("is-open");
    syncUndo();
    window.setTimeout(() => {
      editor.focus();
    }, 0);
  };

  const placeEditor = (point: Point): void => {
    if (composing) {
      return;
    }
    commitEditor();
    editTarget = null;
    selected = null;
    showEditor(point, "", textSize());
  };

  // 双击文字原位重编辑:载入原文本与原字号,提交时走 retext 动作。
  const openTextEditor = (index: number): void => {
    if (composing) {
      return;
    }
    const op = annotations[index];
    if (!op || op.type !== "text") {
      return;
    }
    commitEditor();
    editTarget = index;
    selected = null;
    moving = false;
    moveState = null;
    showEditor({ x: op.x, y: op.y }, op.text, op.size);
  };

  const hideEditor = (): void => {
    editor.classList.remove("is-open");
    editor.value = "";
    editorOrigin = null;
    editTarget = null;
    // 失焦收口:隐藏后仍持有焦点会吞掉 A/R/M/T 等工具快捷键。
    if (document.activeElement === editor) {
      editor.blur();
    }
  };

  const editorOpen = (): boolean => editor.classList.contains("is-open");

  const runAction = (action: EditAction, undoIt: boolean): void => {
    if (action.kind === "add") {
      if (undoIt) {
        annotations.splice(action.index, 1);
      } else {
        annotations.splice(action.index, 0, action.op);
      }
    } else if (action.kind === "remove") {
      if (undoIt) {
        annotations.splice(action.index, 0, action.op);
      } else {
        annotations.splice(action.index, 1);
      }
    } else {
      annotations[action.index] = undoIt ? action.before : action.after;
    }
  };

  const pushAction = (action: EditAction): void => {
    runAction(action, false);
    undoStack.push(action);
    redoStack.length = 0;
  };

  const commitEditor = (): void => {
    if (!editorOpen() || !editorOrigin || composing) {
      return;
    }
    const text = editor.value;
    const origin = editorOrigin;
    const target = editTarget;
    hideEditor();
    if (target !== null && annotations[target]?.type === "text") {
      const before = annotations[target];
      if (text.trim().length === 0) {
        pushAction({ kind: "remove", index: target, op: before });
      } else if (text !== before.text) {
        pushAction({ kind: "replace", index: target, before, after: { ...before, text } });
      }
    } else if (text.trim().length > 0) {
      pushAction({
        kind: "add",
        index: annotations.length,
        op: { type: "text", x: origin.x, y: origin.y, text, size: textSize(), color: styleColor },
      });
    }
    redraw();
    syncUndo();
  };

  const cancelEditor = (): void => {
    hideEditor();
    syncUndo();
    redraw();
  };

  // 拖移中触发 undo/redo 时,拖移位移只被 mousemove 原地写入、尚未入栈。
  // undo 前先把已产生的位移补推成 replace,使本次 Ctrl+Z 先回退拖移,
  // 且位移进入 redo 栈,动作链保持可回溯。
  const settleMove = (): void => {
    if (!moving || !moveState) {
      return;
    }
    const { index, before, moved } = moveState;
    moving = false;
    moveState = null;
    if (moved) {
      pushAction({ kind: "replace", index, before, after: annotations[index] });
      syncUndo();
    }
  };

  const undo = (): void => {
    if (editorOpen()) {
      cancelEditor();
      return;
    }
    settleMove();
    const action = undoStack.pop();
    if (!action) {
      return;
    }
    runAction(action, true);
    // 撤销"放置序号"时回退会话计数,使重放/继续放置仍连续递增不跳号。
    if (action.kind === "add" && action.op.type === "number") {
      numberPlaced = Math.max(0, numberPlaced - 1);
    }
    selected = null;
    moving = false;
    moveState = null;
    redoStack.push(action);
    redraw();
    syncUndo();
  };

  const redo = (): void => {
    if (editorOpen()) {
      return;
    }
    if (moving && moveState) {
      // 重做前丢弃未入栈的拖移位移(此处不能补推动作,否则会清空 redo 栈)。
      annotations[moveState.index] = moveState.before;
      moving = false;
      moveState = null;
    }
    const action = redoStack.pop();
    if (!action) {
      return;
    }
    runAction(action, false);
    if (action.kind === "add" && action.op.type === "number") {
      numberPlaced += 1;
    }
    selected = null;
    moving = false;
    moveState = null;
    undoStack.push(action);
    redraw();
    syncUndo();
  };

  const exportList = (): Annotation[] => annotations.filter((op) => op.type !== "text" || op.text.trim().length > 0);

  const syncCopyAll = (): void => {
    copyAllBtn.hidden = tool !== "ocr" || !ocrDoc || ocrDoc.spans.length === 0;
  };

  const runOcr = async (): Promise<void> => {
    if (busy) {
      return;
    }
    const token = ++ocrGen;
    busy = true;
    ocrDoc = null;
    ocrSelected = [];
    syncCopyAll();
    setNoteKey("preview.note.ocr_running");
    redraw();
    try {
      const doc = await invoke<OcrDocument>("recognize_preview");
      if (token !== ocrGen) {
        return;
      }
      ocrDoc = doc;
      syncCopyAll();
      setNoteKey("preview.note.ocr_hint");
      redraw();
    } catch (error) {
      if (token !== ocrGen) {
        return;
      }
      ocrDoc = null;
      syncCopyAll();
      setNote(invokeError(error, t("preview.error.ocr_fallback")), "error");
      redraw();
    } finally {
      if (token === ocrGen) {
        busy = false;
      }
    }
  };

  const copyOcrSelection = async (startPoint: Point, endPoint: Point): Promise<void> => {
    if (busy || !ocrDoc) {
      return;
    }
    busy = true;
    try {
      const drag = Math.hypot(endPoint.x - startPoint.x, endPoint.y - startPoint.y);
      const copiedText =
        drag < 4
          ? await invoke<string>("copy_ocr_point", { x: endPoint.x, y: endPoint.y })
          : await invoke<string>("copy_ocr_rect", normalizeRect(startPoint, endPoint));
      const snippet = copiedText.length > 24 ? `${copiedText.slice(0, 24)}…` : copiedText;
      setNoteKey("preview.note.ocr_copied", { snippet }, "success");
    } catch (error) {
      setNote(invokeError(error, t("preview.error.no_selection")), "error");
    } finally {
      busy = false;
    }
  };

  const copyOcrAll = async (): Promise<void> => {
    if (busy) {
      return;
    }
    busy = true;
    try {
      await invoke<string>("copy_ocr_all");
      setNoteKey("preview.note.ocr_all_copied", undefined, "success");
    } catch (error) {
      setNote(invokeError(error, t("preview.error.no_text")), "error");
    } finally {
      busy = false;
    }
  };

  const copy = async (): Promise<void> => {
    if (busy) {
      return;
    }
    commitEditor();
    busy = true;
    try {
      await invoke("copy_preview_png", { annotations: exportList() });
      setCopied(
        annotations.length > 0
          ? { key: "preview.copied_annotated", text: "" }
          : { key: "preview.copied_clean", text: "" },
        "success",
      );
    } catch (error) {
      setNote(invokeError(error, t("preview.error.copy_fallback")), "error");
    } finally {
      busy = false;
    }
  };

  const save = async (): Promise<void> => {
    if (busy) {
      return;
    }
    commitEditor();
    busy = true;
    try {
      const result = await invoke<{
        saved: boolean;
        format?: ExportFormat;
        path?: string | null;
      }>("save_preview_png", {
        annotations: exportList(),
        quality: saveQuality,
      });
      if (result.saved) {
        const format = result.format ?? "png";
        const name = fileNameFromPath(result.path) ?? `cropmark.${format === "jpeg" ? "jpg" : format}`;
        setNoteKey("preview.note.saved", { name }, "success");
      }
    } catch (error) {
      setNote(invokeError(error, t("preview.error.save_fallback")), "error");
    } finally {
      busy = false;
    }
  };

  // 贴图:当前标注合成图钉成置顶小窗;预览保持打开,可继续标注/再贴。
  const pin = async (): Promise<void> => {
    if (busy) {
      setNoteKey("preview.note.busy");
      return;
    }
    commitEditor();
    busy = true;
    setNoteKey("preview.note.pinning");
    try {
      await invoke("pin_current", { annotations: exportList() });
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
    if (busy || writebackLabel === null) {
      return;
    }
    commitEditor();
    busy = true;
    setNoteKey("preview.note.updating_pin");
    try {
      await invoke("update_pin_from_preview", { annotations: exportList() });
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

  canvas.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || !frame) {
      return;
    }
    hideContextMenu();
    const point = physicalPoint(event);
    if (tool === "ocr") {
      if (!ocrDoc) {
        return;
      }
      event.preventDefault();
      ocrDragging = true;
      ocrStart = point;
      ocrCurrent = point;
      const hit = indexAtPoint(ocrDoc.spans, point.x, point.y);
      ocrSelected = hit === -1 ? [] : [hit];
      redraw();
      return;
    }
    if (tool === "text") {
      placeEditor(point);
      return;
    }
    commitEditor();
    // 绘制工具下先做命中:命中已放标注则进入选中+拖移,否则清空选中并回到绘制起笔。
    const hit = hitAnnotation(point);
    if (hit !== -1) {
      event.preventDefault();
      selected = hit;
      moving = true;
      moveState = { index: hit, before: annotations[hit], grab: point, moved: false };
      redraw();
      return;
    }
    selected = null;
    if (tool === "number") {
      // 序号工具点击即放置:起始值 + 本次会话已放置数,并立即入栈可撤销。
      const value = numberStart + numberPlaced;
      numberPlaced += 1;
      pushAction({
        kind: "add",
        index: annotations.length,
        op: { type: "number", x: point.x, y: point.y, value, size: textSize(), color: styleColor },
      });
      redraw();
      syncUndo();
      return;
    }
    dragging = true;
    start = point;
    current = point;
    freehand = isFreehandTool(tool) ? [point] : [];
  });

  canvas.addEventListener("dblclick", (event) => {
    if (event.button !== 0 || !frame || tool === "ocr") {
      return;
    }
    const point = physicalPoint(event);
    const hit = hitAnnotation(point);
    if (hit !== -1 && annotations[hit]?.type === "text") {
      event.preventDefault();
      openTextEditor(hit);
    }
  });

  canvas.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    hideContextMenu();
    if (!stylePanel.hidden) {
      toggleStylePanel(false);
    }
    if (!frame) {
      return;
    }
    // 与 mousedown 同序:先提交编辑器再算命中。编辑器提交若删除清空标注,
    // 数组索引会前移,先命中后提交会把右键菜单指到错误的标注上。
    commitEditor();
    const point = physicalPoint(event);
    const hit = hitAnnotation(point);
    if (hit === -1) {
      return;
    }
    selected = hit;
    moving = false;
    moveState = null;
    redraw();
    showContextMenu(event.clientX, event.clientY);
  });

  window.addEventListener("mousemove", (event) => {
    if (moving && moveState) {
      const point = physicalPoint(event);
      let dx = point.x - moveState.grab.x;
      let dy = point.y - moveState.grab.y;
      const b = annotationBounds(ctx, moveState.before);
      // 标注比画布更宽/更高时钳制区间为空(min>max),clamp 会恒取 max 把标注
      // 吸死在右/下缘、抓取点脱节;此时钳到左/上缘并保持抓取点相对偏移。
      const minX = -b.minX;
      const maxX = canvas.width - b.maxX;
      const minY = -b.minY;
      const maxY = canvas.height - b.maxY;
      dx = minX > maxX ? minX : clamp(dx, minX, maxX);
      dy = minY > maxY ? minY : clamp(dy, minY, maxY);
      if (dx !== 0 || dy !== 0) {
        moveState.moved = true;
      }
      annotations[moveState.index] = translateOp(moveState.before, dx, dy);
      redraw();
      return;
    }
    if (ocrDragging && ocrStart && ocrDoc) {
      ocrCurrent = physicalPoint(event);
      const rubber = normalizeRect(ocrStart, ocrCurrent);
      if (Math.hypot(ocrCurrent.x - ocrStart.x, ocrCurrent.y - ocrStart.y) < 4) {
        const hit = indexAtPoint(ocrDoc.spans, ocrCurrent.x, ocrCurrent.y);
        ocrSelected = hit === -1 ? [] : [hit];
      } else {
        ocrSelected = indicesInRect(ocrDoc.spans, rubber);
      }
      redraw();
      return;
    }
    if (!dragging) {
      return;
    }
    if (isFreehandTool(tool)) {
      const point = physicalPoint(event);
      const last = freehand[freehand.length - 1];
      // 1.5px 抽稀:只保留有位移的采样点,控制撤销栈内存。
      if (!last || Math.hypot(point.x - last.x, point.y - last.y) >= 1.5) {
        freehand.push(point);
      }
      current = point;
      redraw();
      return;
    }
    if (!start) {
      return;
    }
    current = physicalPoint(event);
    redraw();
  });

  window.addEventListener("mouseup", () => {
    if (moving && moveState) {
      moving = false;
      const { index, before, moved } = moveState;
      moveState = null;
      if (moved) {
        const after = annotations[index];
        pushAction({ kind: "replace", index, before, after });
        syncUndo();
      }
      redraw();
      return;
    }
    if (ocrDragging && ocrStart && ocrCurrent) {
      ocrDragging = false;
      const from = ocrStart;
      const to = ocrCurrent;
      ocrStart = null;
      ocrCurrent = null;
      redraw();
      void copyOcrSelection(from, to);
      return;
    }
    if (!dragging) {
      return;
    }
    dragging = false;
    if (isFreehandTool(tool)) {
      const points = freehand;
      freehand = [];
      if (points.length >= 2 && polylineLength(points) >= MIN_DRAW_SIZE) {
        pushAction({
          kind: "add",
          index: annotations.length,
          op: { type: tool, points, color: styleColor, strokeWidth: styleWidth },
        });
      }
    } else if (isDragTool(tool) && start && current) {
      const op = draft(tool, start, current, mosaicBlock(), styleColor, styleWidth);
      if (op) {
        pushAction({ kind: "add", index: annotations.length, op });
      }
    }
    start = null;
    current = null;
    freehand = [];
    redraw();
    syncUndo();
  });

  let skipUndoClick = false;
  undoBtn.addEventListener("pointerdown", (event) => {
    if (editorOpen()) {
      event.preventDefault();
      cancelEditor();
      skipUndoClick = true;
    }
  });
  editor.addEventListener("mousedown", (event) => event.stopPropagation());
  editor.addEventListener("pointerdown", (event) => event.stopPropagation());
  editor.addEventListener("compositionstart", () => {
    composing = true;
  });
  editor.addEventListener("compositionend", () => {
    composing = false;
  });
  editor.addEventListener("keydown", (event) => {
    if (event.isComposing || composing || event.key === "Process" || event.keyCode === 229) {
      return;
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      event.stopPropagation();
      commitEditor();
    } else if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      cancelEditor();
    }
  });
  editor.addEventListener("blur", () => {
    window.setTimeout(() => {
      if (composing || document.activeElement === editor) {
        return;
      }
      commitEditor();
    }, 200);
  });

  root.addEventListener("click", (event) => {
    const button = event.target instanceof Element ? event.target.closest("button") : null;
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    const nextTool = button.dataset.tool;
    if (nextTool && isTool(nextTool)) {
      if (nextTool === "ocr" && !ocrEntryEnabled) {
        return;
      }
      setTool(nextTool);
      return;
    }
    const nextQuality = button.dataset.saveQuality;
    if (nextQuality === "high" || nextQuality === "medium" || nextQuality === "low") {
      saveQuality = nextQuality;
      syncSaveQuality();
      return;
    }
    if (button.dataset.action === "undo") {
      if (skipUndoClick) {
        skipUndoClick = false;
        return;
      }
      undo();
    } else if (button.dataset.action === "style") {
      toggleStylePanel();
    } else if (button.dataset.action === "delete-annotation") {
      hideContextMenu();
      deleteSelected();
    } else if (button.dataset.action === "copy") {
      void copy();
    } else if (button.dataset.action === "copy-ocr-all") {
      void copyOcrAll();
    } else if (button.dataset.action === "save") {
      void save();
    } else if (button.dataset.action === "pin") {
      if (pinEntryEnabled) {
        void pin();
      }
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

  pinBtn.addEventListener("pointerdown", (event) => event.stopPropagation());
  pinBtn.addEventListener("click", (event) => {
    event.preventDefault();
    event.stopPropagation();
    if (pinEntryEnabled) {
      void pin();
    }
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

  window.addEventListener("keydown", (event) => {
    if (event.isComposing || composing || event.keyCode === 229) {
      return;
    }
    if (event.key === "Escape") {
      if (!contextMenu.hidden) {
        event.preventDefault();
        hideContextMenu();
        return;
      }
      if (!stylePanel.hidden) {
        event.preventDefault();
        toggleStylePanel(false);
        return;
      }
      if (editorOpen()) {
        event.preventDefault();
        cancelEditor();
        return;
      }
      if (selected !== null) {
        event.preventDefault();
        selected = null;
        redraw();
        return;
      }
      void invoke("close_preview");
      return;
    }
    const mod = event.ctrlKey || event.metaKey;
    if (mod) {
      const key = event.key.toLowerCase();
      if (key === "z" && !editorOpen()) {
        event.preventDefault();
        if (event.shiftKey) {
          redo();
        } else {
          undo();
        }
        return;
      }
      if (key === "y" && !editorOpen()) {
        event.preventDefault();
        redo();
        return;
      }
      if (key === "s") {
        event.preventDefault();
        void save();
        return;
      }
      if (key === "c" && document.activeElement !== editor) {
        const selection = window.getSelection();
        if (selection && !selection.isCollapsed) {
          return;
        }
        event.preventDefault();
        void copy();
      }
      return;
    }
    // 序号输入框聚焦时不吞按键(退格/Delete 属于输入编辑),也不触发工具切换。
    if (
      event.altKey ||
      event.shiftKey ||
      document.activeElement === editor ||
      document.activeElement === numberStartInput ||
      editorOpen()
    ) {
      return;
    }
    if (event.key === "Delete" || event.key === "Backspace") {
      if (selected !== null) {
        event.preventDefault();
        deleteSelected();
      }
      return;
    }
    const toolKeys: Record<string, Tool> = {
      a: "arrow",
      r: "rect",
      e: "ellipse",
      l: "line",
      m: "mosaic",
      b: "blur",
      h: "highlighter",
      p: "pen",
      n: "number",
      t: "text",
    };
    if (ocrEntryEnabled) {
      toolKeys.o = "ocr";
    }
    const nextTool = toolKeys[event.key.toLowerCase()];
    if (nextTool) {
      event.preventDefault();
      setTool(nextTool);
    }
  });

  setTool("arrow");
  syncUndo();
  syncSaveQuality();
  syncStylePanel();

  // 重读功能入口开关并同步预览 UI:ocrEntryEnabled 同时驱动工具条按钮显隐、
  // O 键映射与当前 ocr 工具的回退;pinEntryEnabled 驱动贴图按钮显隐;
  // 每次 reload 都要重读,不能只在首载做一次。
  const syncFeatureFlags = (settings?: { features?: { ocrEntry?: boolean; pinEntry?: boolean } }): void => {
    ocrEntryEnabled = settings?.features?.ocrEntry !== false;
    ocrBtn.hidden = !ocrEntryEnabled;
    if (!ocrEntryEnabled && tool === "ocr") {
      setTool("arrow");
    }
    pinEntryEnabled = settings?.features?.pinEntry !== false;
    syncWritebackUi();
  };
  const reloadFeatureFlags = (): void => {
    void invoke<{ features?: { ocrEntry?: boolean; pinEntry?: boolean } }>("get_ui_settings")
      .then((settings) => {
        syncFeatureFlags(settings);
      })
      .catch(() => undefined);
  };
  // 跨会话记忆：加载时读后端保存的上次样式（读写失败均静默回退当前值）；
  // 同时读取功能入口开关，关闭取字后隐藏预览工具条按钮并停用 O 键。
  const loadStyleDefaults = (): void => {
    void invoke<{
      annotationDefaults?: {
        color?: string;
        width?: number | null;
        textSize?: number | null;
        numberStart?: number;
      };
      features?: { ocrEntry?: boolean; pinEntry?: boolean };
      export?: { quality?: ExportQuality };
    }>("get_ui_settings")
      .then((settings) => {
        syncFeatureFlags(settings);
        const quality = settings?.export?.quality;
        if (quality === "high" || quality === "medium" || quality === "low") {
          saveQuality = quality;
          syncSaveQuality();
        }
        const defaults = settings?.annotationDefaults;
        if (!defaults) {
          return;
        }
        if (typeof defaults.color === "string" && HEX_COLOR_RE.test(defaults.color)) {
          styleColor = defaults.color;
        }
        styleWidth = typeof defaults.width === "number" && Number.isFinite(defaults.width) ? defaults.width : null;
        styleTextBase =
          typeof defaults.textSize === "number" && Number.isFinite(defaults.textSize) ? defaults.textSize : null;
        if (typeof defaults.numberStart === "number" && Number.isFinite(defaults.numberStart)) {
          numberStart = clamp(Math.round(defaults.numberStart), MIN_NUMBER_START, MAX_NUMBER_START);
        }
        syncStylePanel();
        redraw();
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
        carriedAnnotations = carried;
        annotations = carriedAnnotations.slice();
        undoStack.length = 0;
        redoStack.length = 0;
        selected = null;
        numberPlaced = annotations.filter((op) => op.type === "number").length;
        // 携带说明与复制状态共用同一提示条:先登记携带前缀,再写复制状态,
        // 二者合并可见(review P3:分别写入会互相覆盖)。
        carriedNoteSource =
          annotations.length > 0 ? { key: "preview.note.inline_annotations" } : null;
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

  void listen("preview-reload", () => {
    // 新帧可能带入选区即时标注(R21):列表随帧在 loadPreview 中恢复,
    // 这里先清空避免旧编辑态残留。
    annotations = [];
    undoStack.length = 0;
    redoStack.length = 0;
    selected = null;
    moving = false;
    moveState = null;
    dragging = false;
    start = null;
    current = null;
    freehand = [];
    // 新帧的序号从携带图元之后继续递增(image.onload 中重算)。
    numberPlaced = 0;
    // 携带说明随新帧重算(image.onload);先清空,避免加载失败时残留旧前缀。
    carriedNoteSource = null;
    // 新帧不带旧编辑态:收掉文字编辑器,清掉 editorOrigin,防旧文本误入新帧。
    hideEditor();
    syncUndo();
    hideContextMenu();
    ocrDoc = null;
    ocrSelected = [];
    // 每次新帧重读功能入口开关:设置页关闭取字后,复用的预览窗口在下一次
    // 截取时也要隐藏按钮/停用 O 键;样式默认只在首次加载,不在 reload 重置。
    reloadFeatureFlags();
    loadPreview();
  });
  loadPreview();

  // 语言切换:静态标签由 main 的 applyTranslations 更新;这里刷新组合了本地化
  // 文本的样式/质量选项 label+title,并重渲染来源可解析的提示条。
  const refreshOptionLabels = (): void => {
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-style-width]").forEach((button) => {
      const option = STYLE_WIDTHS.find((item) => String(item.value) === button.dataset.styleWidth);
      if (option) {
        button.textContent = t(option.labelKey);
        button.title = t("preview.style.option_title", {
          label: t(option.labelKey),
          value: option.value,
        });
      }
    });
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-style-text-size]").forEach((button) => {
      const option = STYLE_TEXT_SIZES.find(
        (item) => String(item.value) === button.dataset.styleTextSize,
      );
      if (option) {
        button.textContent = t(option.labelKey);
        button.title = t("preview.style.option_title", {
          label: t(option.labelKey),
          value: option.value,
        });
      }
    });
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-style-color]").forEach((button) => {
      const color = button.dataset.styleColor ?? "";
      button.setAttribute("aria-label", t("preview.style.color_aria", { color }));
    });
    saveQualityRoot.querySelectorAll<HTMLButtonElement>("[data-save-quality]").forEach((button) => {
      const option = SAVE_QUALITIES.find((item) => item.value === button.dataset.saveQuality);
      if (option) {
        button.textContent = t(option.labelKey);
        button.title = t(option.titleKey);
      }
    });
  };

  return () => {
    refreshOptionLabels();
    renderNote();
  };
}

const ALL_TOOLS: Tool[] = [
  "arrow",
  "rect",
  "ellipse",
  "line",
  "mosaic",
  "blur",
  "highlighter",
  "pen",
  "number",
  "text",
  "ocr",
];

function isTool(value: string): value is Tool {
  return (ALL_TOOLS as string[]).includes(value);
}

function isDragTool(tool: Tool): tool is DragTool {
  return (
    tool === "arrow" ||
    tool === "rect" ||
    tool === "ellipse" ||
    tool === "line" ||
    tool === "mosaic" ||
    tool === "blur"
  );
}

function isFreehandTool(tool: Tool): tool is FreehandTool {
  return tool === "pen" || tool === "highlighter";
}

// 默认模糊强度随选区短边自适应;上限保证大区域不会慢到卡住界面。
function blurSigma(width: number, height: number): number {
  return Math.round(clamp(Math.min(width, height) / 8, BLUR_SIGMA_MIN, BLUR_SIGMA_MAX));
}

function polylineLength(points: Point[]): number {
  let total = 0;
  for (let i = 1; i < points.length; i += 1) {
    total += Math.hypot(points[i].x - points[i - 1].x, points[i].y - points[i - 1].y);
  }
  return total;
}

function polylineDistance(point: Point, points: Point[]): number {
  if (points.length === 0) {
    return Number.POSITIVE_INFINITY;
  }
  if (points.length === 1) {
    return Math.hypot(point.x - points[0].x, point.y - points[0].y);
  }
  let best = Number.POSITIVE_INFINITY;
  for (let i = 1; i < points.length; i += 1) {
    best = Math.min(best, distToSegment(point, points[i - 1], points[i]));
  }
  return best;
}

function draft(
  tool: Tool,
  start: Point,
  end: Point,
  block: number,
  color: string,
  strokeWidth: number | null,
): Annotation | null {
  if (tool === "arrow" || tool === "line") {
    if (Math.hypot(end.x - start.x, end.y - start.y) < MIN_DRAW_SIZE) {
      return null;
    }
    return tool === "arrow"
      ? { type: "arrow", from: start, to: end, color, strokeWidth }
      : { type: "line", from: start, to: end, color, strokeWidth };
  }
  const x = Math.min(start.x, end.x);
  const y = Math.min(start.y, end.y);
  const width = Math.abs(end.x - start.x);
  const height = Math.abs(end.y - start.y);
  // 极小选区不产生无效标注:短边小于 3px 的区域类工具直接丢弃。
  if (width < MIN_DRAW_SIZE || height < MIN_DRAW_SIZE) {
    return null;
  }
  if (tool === "mosaic") {
    return { type: "mosaic", x, y, width, height, block };
  }
  if (tool === "blur") {
    return { type: "blur", x, y, width, height, sigma: blurSigma(width, height) };
  }
  if (tool === "ellipse") {
    return { type: "ellipse", x, y, width, height, color, strokeWidth };
  }
  if (tool === "rect") {
    return { type: "rect", x, y, width, height, color, strokeWidth };
  }
  return null;
}

function paint(ctx: CanvasRenderingContext2D, op: Annotation, stroke: string, lineWidth: number): void {
  if (op.type === "mosaic") {
    paintMosaic(ctx, op);
    return;
  }
  if (op.type === "blur") {
    paintBlur(ctx, op);
    return;
  }
  ctx.save();
  ctx.strokeStyle = stroke;
  ctx.fillStyle = stroke;
  ctx.lineWidth = lineWidth;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  if (op.type === "rect") {
    ctx.strokeRect(op.x + 0.5, op.y + 0.5, op.width, op.height);
  } else if (op.type === "ellipse") {
    ctx.beginPath();
    ctx.ellipse(op.x + op.width / 2, op.y + op.height / 2, op.width / 2, op.height / 2, 0, 0, Math.PI * 2);
    ctx.stroke();
  } else if (op.type === "line") {
    ctx.beginPath();
    ctx.moveTo(op.from.x, op.from.y);
    ctx.lineTo(op.to.x, op.to.y);
    ctx.stroke();
  } else if (op.type === "arrow") {
    paintArrow(ctx, op.from, op.to, lineWidth);
  } else if (op.type === "text") {
    ctx.font = `${op.size}px ${TEXT_FONT_STACK}`;
    ctx.textBaseline = "top";
    const lines = op.text.split("\n");
    lines.forEach((line, index) => {
      ctx.fillText(line, op.x, op.y + index * op.size * 1.25);
    });
  } else if (op.type === "number") {
    ctx.font = `600 ${op.size}px ${TEXT_FONT_STACK}`;
    ctx.textBaseline = "top";
    ctx.fillText(String(op.value), op.x, op.y);
  } else if (op.type === "pen") {
    paintPolyline(ctx, op.points, 1);
  } else if (op.type === "highlighter") {
    paintPolyline(ctx, op.points, HIGHLIGHTER_ALPHA);
  }
  ctx.restore();
}

function paintPolyline(ctx: CanvasRenderingContext2D, points: Point[], alpha: number): void {
  if (points.length < 2) {
    return;
  }
  const previous = ctx.globalAlpha;
  ctx.globalAlpha = previous * alpha;
  ctx.beginPath();
  ctx.moveTo(points[0].x, points[0].y);
  for (let i = 1; i < points.length; i += 1) {
    ctx.lineTo(points[i].x, points[i].y);
  }
  ctx.stroke();
  ctx.globalAlpha = previous;
}

function paintArrow(ctx: CanvasRenderingContext2D, from: Point, to: Point, lineWidth: number): void {
  const dx = to.x - from.x;
  const dy = to.y - from.y;
  const len = Math.hypot(dx, dy);
  if (len < 2) {
    return;
  }
  const ux = dx / len;
  const uy = dy / len;
  const head = Math.min(28, Math.max(10, 14 * (lineWidth / 3)));
  const backX = to.x - ux * head;
  const backY = to.y - uy * head;
  const px = -uy;
  const py = ux;
  const spread = head * 0.42;
  ctx.beginPath();
  ctx.moveTo(from.x, from.y);
  ctx.lineTo(backX + ux * (lineWidth * 0.5), backY + uy * (lineWidth * 0.5));
  ctx.stroke();
  ctx.beginPath();
  ctx.moveTo(to.x, to.y);
  ctx.lineTo(backX + px * spread, backY + py * spread);
  ctx.lineTo(backX - px * spread, backY - py * spread);
  ctx.closePath();
  ctx.fill();
}

function paintMosaic(
  ctx: CanvasRenderingContext2D,
  op: Extract<Annotation, { type: "mosaic" }>,
): void {
  const x = Math.max(0, Math.round(op.x));
  const y = Math.max(0, Math.round(op.y));
  const w = Math.max(1, Math.round(op.width));
  const h = Math.max(1, Math.round(op.height));
  const block = Math.max(2, op.block);
  const maxW = Math.min(w, ctx.canvas.width - x);
  const maxH = Math.min(h, ctx.canvas.height - y);
  if (maxW <= 0 || maxH <= 0) {
    return;
  }
  const data = ctx.getImageData(x, y, maxW, maxH);
  const px = data.data;
  for (let by = 0; by < maxH; by += block) {
    const bh = Math.min(block, maxH - by);
    for (let bx = 0; bx < maxW; bx += block) {
      const bw = Math.min(block, maxW - bx);
      let r = 0;
      let g = 0;
      let b = 0;
      let n = 0;
      for (let pyy = 0; pyy < bh; pyy += 1) {
        for (let pxx = 0; pxx < bw; pxx += 1) {
          const i = ((by + pyy) * maxW + (bx + pxx)) * 4;
          r += px[i];
          g += px[i + 1];
          b += px[i + 2];
          n += 1;
        }
      }
      if (n === 0) {
        continue;
      }
      r = Math.round(r / n);
      g = Math.round(g / n);
      b = Math.round(b / n);
      for (let pyy = 0; pyy < bh; pyy += 1) {
        for (let pxx = 0; pxx < bw; pxx += 1) {
          const i = ((by + pyy) * maxW + (bx + pxx)) * 4;
          px[i] = r;
          px[i + 1] = g;
          px[i + 2] = b;
          px[i + 3] = 255;
        }
      }
    }
  }
  ctx.putImageData(data, x, y);
}

// ctx.filter 支持探测只需一次;不支持时(旧版 WebKit)退回降采样近似。
let canvasFilterSupport: boolean | null = null;

function supportsCanvasFilter(ctx: CanvasRenderingContext2D): boolean {
  if (canvasFilterSupport !== null) {
    return canvasFilterSupport;
  }
  canvasFilterSupport = false;
  if ("filter" in ctx) {
    const previous = ctx.filter;
    ctx.filter = "blur(1px)";
    canvasFilterSupport = ctx.filter !== "none" && ctx.filter !== "";
    ctx.filter = previous;
  }
  return canvasFilterSupport;
}

// 高斯模糊预览:对当前已绘制内容做区域模糊,与 Rust 侧对合成帧做 imageops::blur
// 的语义一致(遮盖类只要求视觉不可还原,不要求逐像素一致)。
function paintBlur(ctx: CanvasRenderingContext2D, op: Extract<Annotation, { type: "blur" }>): void {
  const x = clamp(Math.round(op.x), 0, ctx.canvas.width);
  const y = clamp(Math.round(op.y), 0, ctx.canvas.height);
  const w = Math.min(Math.round(op.width), ctx.canvas.width - x);
  const h = Math.min(Math.round(op.height), ctx.canvas.height - y);
  if (w < 2 || h < 2) {
    return;
  }
  const sigma = clamp(op.sigma, 1, 200);
  ctx.save();
  ctx.beginPath();
  ctx.rect(x, y, w, h);
  ctx.clip();
  if (supportsCanvasFilter(ctx)) {
    ctx.filter = `blur(${sigma}px)`;
    // 外扩边距作为采样余量,避免区域边缘混入画布外的透明像素。
    const pad = Math.max(2, Math.round(sigma * 2));
    const sx = Math.max(0, x - pad);
    const sy = Math.max(0, y - pad);
    const ex = Math.min(ctx.canvas.width, x + w + pad);
    const ey = Math.min(ctx.canvas.height, y + h + pad);
    ctx.drawImage(ctx.canvas, sx, sy, ex - sx, ey - sy, sx, sy, ex - sx, ey - sy);
  } else {
    const factor = Math.max(2, Math.round(sigma));
    const small = document.createElement("canvas");
    small.width = Math.max(1, Math.round(w / factor));
    small.height = Math.max(1, Math.round(h / factor));
    const smallCtx = small.getContext("2d");
    if (smallCtx) {
      smallCtx.drawImage(ctx.canvas, x, y, w, h, 0, 0, small.width, small.height);
      ctx.imageSmoothingEnabled = true;
      ctx.drawImage(small, 0, 0, small.width, small.height, x, y, w, h);
    }
  }
  ctx.restore();
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

function distToSegment(p: Point, a: Point, b: Point): number {
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  const lengthSq = dx * dx + dy * dy;
  if (lengthSq === 0) {
    return Math.hypot(p.x - a.x, p.y - a.y);
  }
  let t = ((p.x - a.x) * dx + (p.y - a.y) * dy) / lengthSq;
  t = clamp(t, 0, 1);
  return Math.hypot(p.x - (a.x + t * dx), p.y - (a.y + t * dy));
}

// 与 paint 的文字绘制同字体度量出的包围盒,供命中与选中高亮共用。
function annotationBounds(ctx: CanvasRenderingContext2D, op: Annotation): Bounds {
  if (op.type === "arrow" || op.type === "line") {
    return {
      minX: Math.min(op.from.x, op.to.x),
      minY: Math.min(op.from.y, op.to.y),
      maxX: Math.max(op.from.x, op.to.x),
      maxY: Math.max(op.from.y, op.to.y),
    };
  }
  if (op.type === "text" || op.type === "number") {
    const text = op.type === "text" ? op.text : String(op.value);
    ctx.save();
    ctx.font =
      op.type === "text" ? `${op.size}px ${TEXT_FONT_STACK}` : `600 ${op.size}px ${TEXT_FONT_STACK}`;
    let width = 0;
    for (const line of text.split("\n")) {
      width = Math.max(width, ctx.measureText(line).width);
    }
    ctx.restore();
    const lineCount = text.split("\n").length;
    return { minX: op.x, minY: op.y, maxX: op.x + width, maxY: op.y + lineCount * op.size * 1.25 };
  }
  if (op.type === "pen" || op.type === "highlighter") {
    if (op.points.length === 0) {
      return { minX: 0, minY: 0, maxX: 0, maxY: 0 };
    }
    let minX = op.points[0].x;
    let minY = op.points[0].y;
    let maxX = minX;
    let maxY = minY;
    for (const point of op.points) {
      minX = Math.min(minX, point.x);
      minY = Math.min(minY, point.y);
      maxX = Math.max(maxX, point.x);
      maxY = Math.max(maxY, point.y);
    }
    return { minX, minY, maxX, maxY };
  }
  return { minX: op.x, minY: op.y, maxX: op.x + op.width, maxY: op.y + op.height };
}

function translateOp(op: Annotation, dx: number, dy: number): Annotation {
  if (op.type === "arrow" || op.type === "line") {
    return {
      ...op,
      from: { x: op.from.x + dx, y: op.from.y + dy },
      to: { x: op.to.x + dx, y: op.to.y + dy },
    };
  }
  if (op.type === "pen" || op.type === "highlighter") {
    return { ...op, points: op.points.map((point) => ({ x: point.x + dx, y: point.y + dy })) };
  }
  return { ...op, x: op.x + dx, y: op.y + dy };
}

function paintSelectionBox(
  ctx: CanvasRenderingContext2D,
  op: Annotation,
  color: string,
  scale: number,
): void {
  const b = annotationBounds(ctx, op);
  const pad = Math.max(4, Math.round(scale * 4));
  ctx.save();
  ctx.strokeStyle = color;
  ctx.lineWidth = Math.max(1, Math.round(scale));
  ctx.setLineDash([6, 4]);
  ctx.strokeRect(
    b.minX - pad,
    b.minY - pad,
    b.maxX - b.minX + pad * 2,
    b.maxY - b.minY + pad * 2,
  );
  ctx.restore();
}

function normalizeRect(a: Point, b: Point): { x: number; y: number; width: number; height: number } {
  const x = Math.min(a.x, b.x);
  const y = Math.min(a.y, b.y);
  return { x, y, width: Math.abs(a.x - b.x), height: Math.abs(a.y - b.y) };
}

function indexAtPoint(spans: TextSpan[], x: number, y: number): number {
  let best = -1;
  let area = Number.POSITIVE_INFINITY;
  for (let i = 0; i < spans.length; i += 1) {
    const span = spans[i];
    if (x < span.x || x > span.x + span.width || y < span.y || y > span.y + span.height) {
      continue;
    }
    const nextArea = span.width * span.height;
    if (nextArea < area) {
      area = nextArea;
      best = i;
    }
  }
  return best;
}

function indicesInRect(
  spans: TextSpan[],
  rect: { x: number; y: number; width: number; height: number },
): number[] {
  const x1 = rect.x + rect.width;
  const y1 = rect.y + rect.height;
  return spans
    .map((span, index) => ({ span, index }))
    .filter(
      ({ span }) => span.x < x1 && span.x + span.width > rect.x && span.y < y1 && span.y + span.height > rect.y,
    )
    .map(({ index }) => index);
}

interface OcrColors {
  hl: string;
  hlStrong: string;
  fillWeak: string;
  fillStrong: string;
  fillRubber: string;
  strokeWeak: string;
  strokeStrong: string;
  strokeRubber: string;
}

function paintOcr(
  ctx: CanvasRenderingContext2D,
  spans: TextSpan[],
  selected: number[],
  rubber: { x: number; y: number; width: number; height: number } | null,
  colors: OcrColors,
): void {
  ctx.save();
  const selectedSet = new Set(selected);
  for (let i = 0; i < spans.length; i += 1) {
    const span = spans[i];
    const on = selectedSet.has(i);
    ctx.fillStyle = on ? colors.fillStrong : colors.fillWeak;
    ctx.strokeStyle = on ? colors.strokeStrong : colors.strokeWeak;
    ctx.lineWidth = Math.max(1, ctx.canvas.width / 900);
    ctx.beginPath();
    ctx.rect(span.x, span.y, span.width, span.height);
    ctx.fill();
    ctx.stroke();
  }
  if (rubber && (rubber.width > 3 || rubber.height > 3)) {
    ctx.fillStyle = colors.fillRubber;
    ctx.strokeStyle = colors.strokeRubber;
    ctx.setLineDash([6, 4]);
    ctx.fillRect(rubber.x, rubber.y, rubber.width, rubber.height);
    ctx.strokeRect(rubber.x, rubber.y, rubber.width, rubber.height);
  }
  ctx.restore();
}

function resolveCanvasColor(raw: string, fallback: string): string {
  const value = raw.trim();
  if (!value) {
    return fallback;
  }
  if (/^#([0-9a-f]{3,8})$/i.test(value) || /^rgba?\(/.test(value)) {
    return value;
  }
  const probe = document.createElement("span");
  probe.style.color = value;
  probe.style.display = "none";
  document.body.appendChild(probe);
  const resolved = getComputedStyle(probe).color;
  probe.remove();
  return resolved || fallback;
}

function withAlpha(color: string, alpha: number): string {
  let value = color.trim();
  if (!/^rgba?\(/.test(value) && !/^#[0-9a-f]{3,8}$/i.test(value)) {
    // 非 hex/rgb 形式(如 color-mix() 计算结果)先规范化为 rgb()
    value = resolveCanvasColor(value, "");
  }
  const rgb = /^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/.exec(value);
  if (rgb) {
    return `rgba(${rgb[1]}, ${rgb[2]}, ${rgb[3]}, ${alpha})`;
  }
  const hex = /^#([0-9a-f]{3}|[0-9a-f]{6})(?:[0-9a-f]{2})?$/i.exec(value);
  if (hex) {
    let body = hex[1];
    if (body.length === 3) {
      body = body
        .split("")
        .map((ch) => ch + ch)
        .join("");
    }
    const r = parseInt(body.slice(0, 2), 16);
    const g = parseInt(body.slice(2, 4), 16);
    const b = parseInt(body.slice(4, 6), 16);
    return `rgba(${r}, ${g}, ${b}, ${alpha})`;
  }
  // 解析失败回退固定 OCR 高亮蓝,保证高亮始终半透明
  return `rgba(14, 165, 233, ${alpha})`;
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
