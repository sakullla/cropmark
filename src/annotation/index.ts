import { invoke } from "@tauri-apps/api/core";
import { t, type CatalogKey } from "../i18n";
import "./editor.css";

// R21:预览编辑器与 Wayland Web 覆盖层共用的标注层。
// 坐标契约:全部图元使用「冻帧物理像素」坐标;画布(`paint` 的目标 ctx)必须
// 与冻帧同尺寸,马赛克/高斯模糊直接在该画布上取像素。宿主负责先画底图
// (预览为源画布,覆盖层为冻帧原图),覆盖层再按选区裁剪合成。

export type AnnotationTool =
  | "arrow"
  | "rect"
  | "ellipse"
  | "line"
  | "mosaic"
  | "blur"
  | "highlighter"
  | "pen"
  | "number"
  | "text";

// 拖拽式绘制工具:按下为起点、拖动出范围、松开入栈。
export type DragTool = "arrow" | "rect" | "ellipse" | "line" | "mosaic" | "blur";
// 自由绘制工具:按住移动采集折线点,松开入栈。
export type FreehandTool = "highlighter" | "pen";

export type Point = { x: number; y: number };

export type Bounds = { minX: number; minY: number; maxX: number; maxY: number };

export interface AnnotationFrame {
  width: number;
  height: number;
  scale: number;
}

export type Annotation =
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

export interface AnnotationStyle {
  color: string;
  width: number | null;
  textSize: number | null;
  numberStart: number;
}

/** 工具提示词条(由宿主决定呈现方式:预览写提示条,覆盖层可忽略)。 */
export interface AnnotationHint {
  key: CatalogKey;
  params?: Record<string, string | number>;
}

/** 样式持久化等错误:词条键或宿主错误串。 */
export type AnnotationError = AnnotationHint | string;

export interface AnnotationEditorOptions {
  /** 定位基准(position:relative),文字编辑器与右键菜单挂在这里。 */
  root: HTMLElement;
  /** 交互画布;坐标由 `frame()` 映射,不要求画布等于冻帧尺寸。 */
  canvas: HTMLCanvasElement;
  /** 测量/取色用的 2D 上下文。 */
  ctx: CanvasRenderingContext2D;
  /** 工具条容器;模块填充按钮并加 `annotation-tools` 类。 */
  toolbar: HTMLElement;
  /** 文字编辑器挂载容器(与 root 同为绝对定位基准)。 */
  textHost: HTMLElement;
  frame: () => AnnotationFrame | null;
  /** 宿主重绘:先画底图,再调用 `paint(ctx)`。 */
  redraw: () => void;
  /** 是否接受标注输入;false 时画布事件/快捷键交给宿主(如取字工具)。 */
  isEditable?: () => boolean;
  /** 工具切换提示;null 表示恢复宿主默认提示。 */
  onToolHint?: (hint: AnnotationHint | null) => void;
  /** 宿主接管工具(如取字)时,手动清除工具条高亮。 */
  onToolChange?: (tool: AnnotationTool) => void;
  /** 右键未命中任何图元(无删除菜单可给)时回调;宿主可借此给出替代说明。 */
  onContextMenuMiss?: () => void;
  onError?: (error: AnnotationError) => void;
}

export interface AnnotationEditor {
  /** 在冻帧尺寸的 ctx 上绘制已确认图元 + 草稿 + 选中框。 */
  paint: (ctx: CanvasRenderingContext2D) => void;
  tool: () => AnnotationTool;
  setTool: (tool: AnnotationTool) => void;
  /** 清除工具条高亮(宿主切到自有工具时用),不改内部工具状态。 */
  deactivateTool: () => void;
  annotations: () => Annotation[];
  exportList: () => Annotation[];
  setAnnotations: (list: Annotation[]) => void;
  clear: () => void;
  undo: () => void;
  redo: () => void;
  canUndo: () => boolean;
  deleteSelected: () => void;
  selectedIndex: () => number | null;
  clearSelection: () => void;
  isTextEditing: () => boolean;
  /** 是否存在需要合成的标注内容(已确认图元/草稿/文字编辑):宿主可据此跳过合成。 */
  hasContent: () => boolean;
  commitText: () => void;
  cancelText: () => void;
  style: () => AnnotationStyle;
  setStyle: (next: Partial<AnnotationStyle>) => void;
  refreshLabels: () => void;
}

export const ANNOTATION_TOOLS: AnnotationTool[] = [
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
];

const FALLBACK_STROKE = "#e11d48";
const FALLBACK_SELECT = "#2563eb";
export const TEXT_FONT_STACK = '"Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif';

// 与 Rust annotate::HIGHLIGHTER_ALPHA / MIN_BLUR_SIGMA / MAX_BLUR_SIGMA 对齐。
export const HIGHLIGHTER_ALPHA = 0.38;
const BLUR_SIGMA_MIN = 3;
const BLUR_SIGMA_MAX = 48;
export const MIN_NUMBER_START = 1;
export const MAX_NUMBER_START = 999;
export const MIN_DRAW_SIZE = 3;

// 与 Rust parse_hex_color 对齐:接受 #rgb / #rrggbb / #rrggbbaa,其余形式回退默认色。
export const HEX_COLOR_RE = /^#(?:[0-9a-f]{3}|[0-9a-f]{6}|[0-9a-f]{8})$/i;

// 样式预设:颜色含现行玫红;线宽/字号档位为逻辑值,绘制时乘 scale 并 clamp 2..8(线宽)。
export const STYLE_COLORS = ["#e11d48", "#2563eb", "#f59e0b", "#10b981", "#111827"];
export const STYLE_WIDTHS: Array<{ value: number; labelKey: CatalogKey }> = [
  { value: 2, labelKey: "preview.style.width_thin" },
  { value: 3, labelKey: "preview.style.width_normal" },
  { value: 5, labelKey: "preview.style.width_thick" },
];
export const STYLE_TEXT_SIZES: Array<{ value: number; labelKey: CatalogKey }> = [
  { value: 12, labelKey: "preview.style.text_small" },
  { value: 16, labelKey: "preview.style.text_medium" },
  { value: 22, labelKey: "preview.style.text_large" },
];

const TOOL_TITLE_KEYS: Record<AnnotationTool, CatalogKey> = {
  arrow: "preview.tool.arrow_title",
  rect: "preview.tool.rect_title",
  ellipse: "preview.tool.ellipse_title",
  line: "preview.tool.line_title",
  mosaic: "preview.tool.mosaic_title",
  blur: "preview.tool.blur_title",
  highlighter: "preview.tool.highlighter_title",
  pen: "preview.tool.pen_title",
  number: "preview.tool.number_title",
  text: "preview.tool.text_title",
};

const TOOL_LABEL_KEYS: Record<AnnotationTool, CatalogKey> = {
  arrow: "preview.tool.arrow",
  rect: "preview.tool.rect",
  ellipse: "preview.tool.ellipse",
  line: "preview.tool.line",
  mosaic: "preview.tool.mosaic",
  blur: "preview.tool.blur",
  highlighter: "preview.tool.highlighter",
  pen: "preview.tool.pen",
  number: "preview.tool.number",
  text: "preview.tool.text",
};

const TOOL_HINT_KEYS: Partial<Record<AnnotationTool, CatalogKey>> = {
  text: "preview.note.text_hint",
  number: "preview.note.number_hint",
  pen: "preview.note.pen_hint",
  highlighter: "preview.note.highlighter_hint",
  blur: "preview.note.blur_hint",
};

const ICONS: Record<AnnotationTool | "undo" | "style", string> = {
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

/** 从设置对象读取标注默认样式;非法值忽略(宿主传 `get_ui_settings` 结果)。 */
export function readAnnotationDefaults(settings: unknown): Partial<AnnotationStyle> {
  const defaults = (settings as { annotationDefaults?: Record<string, unknown> } | null)
    ?.annotationDefaults;
  const style: Partial<AnnotationStyle> = {};
  if (!defaults) {
    return style;
  }
  if (typeof defaults.color === "string" && HEX_COLOR_RE.test(defaults.color)) {
    style.color = defaults.color;
  }
  if (typeof defaults.width === "number" && Number.isFinite(defaults.width)) {
    style.width = defaults.width;
  }
  if (typeof defaults.textSize === "number" && Number.isFinite(defaults.textSize)) {
    style.textSize = defaults.textSize;
  }
  if (typeof defaults.numberStart === "number" && Number.isFinite(defaults.numberStart)) {
    style.numberStart = clamp(Math.round(defaults.numberStart), MIN_NUMBER_START, MAX_NUMBER_START);
  }
  return style;
}

/** 读取跨会话标注样式(读取失败静默回退默认)。 */
export async function loadAnnotationDefaults(): Promise<Partial<AnnotationStyle>> {
  try {
    return readAnnotationDefaults(await invoke<unknown>("get_ui_settings"));
  } catch {
    return {};
  }
}

export function isDragTool(tool: AnnotationTool): tool is DragTool {
  return (
    tool === "arrow" ||
    tool === "rect" ||
    tool === "ellipse" ||
    tool === "line" ||
    tool === "mosaic" ||
    tool === "blur"
  );
}

export function isFreehandTool(tool: AnnotationTool): tool is FreehandTool {
  return tool === "pen" || tool === "highlighter";
}

// 默认模糊强度随选区短边自适应;上限保证大区域不会慢到卡住界面。
export function blurSigma(width: number, height: number): number {
  return Math.round(clamp(Math.min(width, height) / 8, BLUR_SIGMA_MIN, BLUR_SIGMA_MAX));
}

export function polylineLength(points: Point[]): number {
  let total = 0;
  for (let i = 1; i < points.length; i += 1) {
    total += Math.hypot(points[i].x - points[i - 1].x, points[i].y - points[i - 1].y);
  }
  return total;
}

export function polylineDistance(point: Point, points: Point[]): number {
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

export function draft(
  tool: AnnotationTool,
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

export function paintAnnotation(
  ctx: CanvasRenderingContext2D,
  op: Annotation,
  stroke: string,
  lineWidth: number,
): void {
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

export function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

export function distToSegment(p: Point, a: Point, b: Point): number {
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  const lengthSq = dx * dx + dy * dy;
  if (lengthSq === 0) {
    return Math.hypot(p.x - a.x, p.y - a.y);
  }
  let segment = ((p.x - a.x) * dx + (p.y - a.y) * dy) / lengthSq;
  segment = clamp(segment, 0, 1);
  return Math.hypot(p.x - (a.x + segment * dx), p.y - (a.y + segment * dy));
}

// 与 paint 的文字绘制同字体度量出的包围盒,供命中与选中高亮共用。
export function annotationBounds(ctx: CanvasRenderingContext2D, op: Annotation): Bounds {
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

export function translateOp(op: Annotation, dx: number, dy: number): Annotation {
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

export function normalizeRect(a: Point, b: Point): { x: number; y: number; width: number; height: number } {
  const x = Math.min(a.x, b.x);
  const y = Math.min(a.y, b.y);
  return { x, y, width: Math.abs(a.x - b.x), height: Math.abs(a.y - b.y) };
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

export function resolveCanvasColor(raw: string, fallback: string): string {
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

export function exportableList(annotations: Annotation[]): Annotation[] {
  return annotations.filter((op) => op.type !== "text" || op.text.trim().length > 0);
}

function toolbarMarkup(): string {
  const buttons = ANNOTATION_TOOLS.map((tool) => {
    const short =
      tool === "text"
        ? `<span data-i18n="preview.tool.text_short">文字</span>`
        : "";
    const className = tool === "text" ? "tool-text" : "";
    return `<button type="button" data-tool="${tool}" class="${className}" data-i18n-title="${TOOL_TITLE_KEYS[tool]}" data-i18n-aria-label="${TOOL_LABEL_KEYS[tool]}" title="${t(TOOL_TITLE_KEYS[tool])}" aria-label="${t(TOOL_LABEL_KEYS[tool])}">${ICONS[tool]}${short}</button>`;
  }).join("");
  return `
    ${buttons}
    <button type="button" data-action="undo" data-i18n-title="preview.tool.undo_title" data-i18n-aria-label="preview.tool.undo" title="撤销 (Ctrl+Z)" aria-label="撤销">${ICONS.undo}</button>
    <div class="annotation-style" data-style-root>
      <button type="button" data-action="style" data-i18n-title="preview.tool.style_title" data-i18n-aria-label="preview.tool.style_title" title="标注样式" aria-label="标注样式" aria-haspopup="true">${ICONS.style}</button>
      <div class="annotation-style-panel" data-style-panel hidden>
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
  `;
}

export function mountAnnotationEditor(options: AnnotationEditorOptions): AnnotationEditor {
  const { root, canvas, ctx, toolbar, textHost } = options;

  toolbar.classList.add("annotation-tools");
  toolbar.innerHTML = toolbarMarkup();

  const editor = document.createElement("textarea");
  editor.className = "annotation-text";
  editor.rows = 2;
  editor.spellcheck = false;
  editor.placeholder = t("preview.text_placeholder");
  editor.dataset.i18nPlaceholder = "preview.text_placeholder";
  editor.classList.remove("is-open");
  textHost.appendChild(editor);

  const contextMenu = document.createElement("div");
  contextMenu.className = "annotation-context";
  contextMenu.hidden = true;
  contextMenu.innerHTML = `<button type="button" data-action="delete-annotation" data-i18n="preview.action.delete_annotation">${t("preview.action.delete_annotation")}</button>`;
  root.appendChild(contextMenu);

  const undoBtn = toolbar.querySelector("[data-action=undo]");
  const stylePanel = toolbar.querySelector("[data-style-panel]");
  const styleBtn = toolbar.querySelector("[data-action=style]");
  const styleRoot = toolbar.querySelector("[data-style-root]");
  const numberStartInput = toolbar.querySelector("[data-style-number-start]");
  if (
    !(undoBtn instanceof HTMLButtonElement) ||
    !(stylePanel instanceof HTMLElement) ||
    !(styleBtn instanceof HTMLButtonElement) ||
    !(styleRoot instanceof HTMLElement) ||
    !(numberStartInput instanceof HTMLInputElement)
  ) {
    return noopEditor();
  }

  const rootStyle = getComputedStyle(root);
  const strokeColor = resolveCanvasColor(rootStyle.getPropertyValue("--stroke"), FALLBACK_STROKE);
  const selectColor = resolveCanvasColor(rootStyle.getPropertyValue("--accent"), FALLBACK_SELECT);

  // 测量文字包围盒用 ctx 与画布尺寸无关;命中/包围盒不改变 canvas 状态。
  const measureCtx = ctx;

  let tool: AnnotationTool = "arrow";
  let annotations: Annotation[] = [];
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
  let composing = false;
  let styleColor = FALLBACK_STROKE;
  let styleWidth: number | null = null;
  let styleTextBase: number | null = null;
  let numberStart = MIN_NUMBER_START;
  // 编辑会话内已放置的序号数:下一次放置 = numberStart + numberPlaced。
  let numberPlaced = 0;

  const editable = (): boolean => options.isEditable?.() !== false;

  const frameScale = (): number => Math.max(options.frame()?.scale ?? 1, 1);

  const mosaicBlock = (): number => Math.max(8, Math.round(12 * frameScale()));

  const textSize = (): number => {
    const frame = options.frame();
    const dpi = Math.max(frame?.scale ?? 1, 1);
    const longestEdge = Math.max(frame?.width ?? 0, frame?.height ?? 0);
    return Math.max(10, Math.round((styleTextBase ?? 16) * Math.max(dpi, longestEdge / 1920)));
  };
  // 与 Rust raster resolve_stroke 同一数值推导:逻辑档位 × scale 后 clamp 2..8。
  const strokeFor = (opWidth: number | null): number =>
    Math.min(8, Math.max(2, (opWidth ?? 3) * frameScale()));
  const colorFor = (opColor: string): string =>
    HEX_COLOR_RE.test(opColor) ? opColor : strokeColor;
  const annotationStyle = (op: Annotation): { color: string; lineWidth: number } => {
    if (op.type === "mosaic" || op.type === "blur") {
      return { color: strokeColor, lineWidth: strokeFor(null) };
    }
    if (op.type === "text" || op.type === "number") {
      return { color: colorFor(op.color), lineWidth: strokeFor(null) };
    }
    return { color: colorFor(op.color), lineWidth: strokeFor(op.strokeWidth) };
  };

  const physicalPoint = (event: MouseEvent): Point => {
    const frame = options.frame();
    const rect = canvas.getBoundingClientRect();
    if (!frame) {
      return { x: 0, y: 0 };
    }
    return {
      x: clamp(((event.clientX - rect.left) / Math.max(rect.width, 1)) * frame.width, 0, frame.width),
      y: clamp(((event.clientY - rect.top) / Math.max(rect.height, 1)) * frame.height, 0, frame.height),
    };
  };

  const cssScale = (): { x: number; y: number } => {
    const frame = options.frame();
    const rect = canvas.getBoundingClientRect();
    return {
      x: rect.width / Math.max(frame?.width ?? canvas.width, 1),
      y: rect.height / Math.max(frame?.height ?? canvas.height, 1),
    };
  };

  const redraw = (): void => {
    options.redraw();
  };

  const syncUndo = (): void => {
    undoBtn.disabled = undoStack.length === 0 && !editorOpen();
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
        const b = annotationBounds(measureCtx, op);
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

  const showEditor = (origin: Point, text: string, baseFontSize: number): void => {
    editorOrigin = origin;
    const scale = cssScale();
    const canvasRect = canvas.getBoundingClientRect();
    const hostRect = textHost.getBoundingClientRect();
    const editorStyle = window.getComputedStyle(editor);
    const insetX = parseFloat(editorStyle.paddingLeft) + parseFloat(editorStyle.borderLeftWidth);
    const insetY = parseFloat(editorStyle.paddingTop) + parseFloat(editorStyle.borderTopWidth);
    const fontSize = baseFontSize * scale.y;
    editor.value = text;
    editor.style.left = `${canvasRect.left - hostRect.left + origin.x * scale.x - insetX}px`;
    editor.style.top = `${canvasRect.top - hostRect.top + origin.y * scale.y - insetY}px`;
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

  const emitToolHint = (): void => {
    const key = TOOL_HINT_KEYS[tool];
    if (key === "preview.note.number_hint") {
      options.onToolHint?.({ key, params: { start: numberStart } });
      return;
    }
    options.onToolHint?.(key ? { key } : null);
  };

  const syncToolUi = (): void => {
    root.dataset.tool = tool;
    toolbar.querySelectorAll<HTMLButtonElement>("[data-tool]").forEach((button) => {
      button.classList.toggle("active", button.dataset.tool === tool);
    });
  };

  const setTool = (next: AnnotationTool): void => {
    commitEditor();
    tool = next;
    syncToolUi();
    selected = null;
    moving = false;
    moveState = null;
    dragging = false;
    start = null;
    current = null;
    freehand = [];
    options.onToolChange?.(next);
    emitToolHint();
    redraw();
  };

  const deactivateTool = (): void => {
    toolbar.querySelectorAll<HTMLButtonElement>("[data-tool]").forEach((button) => {
      button.classList.remove("active");
    });
  };

  const persistStyle = (): void => {
    void invoke<{ notice?: string | null }>("set_annotation_defaults", {
      defaults: {
        color: styleColor,
        width: styleWidth,
        textSize: styleTextBase,
        numberStart,
      },
    })
      .then((result) => {
        if (result?.notice) {
          options.onError?.(result.notice);
        }
      })
      .catch(() => {
        options.onError?.({ key: "preview.note.style_not_saved" });
      });
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

  const setStyle = (next: Partial<AnnotationStyle>): void => {
    if (typeof next.color === "string" && HEX_COLOR_RE.test(next.color)) {
      styleColor = next.color;
    }
    if (next.width !== undefined) {
      styleWidth = typeof next.width === "number" && Number.isFinite(next.width) ? next.width : null;
    }
    if (next.textSize !== undefined) {
      styleTextBase =
        typeof next.textSize === "number" && Number.isFinite(next.textSize) ? next.textSize : null;
    }
    if (typeof next.numberStart === "number" && Number.isFinite(next.numberStart)) {
      numberStart = clamp(Math.round(next.numberStart), MIN_NUMBER_START, MAX_NUMBER_START);
    }
    syncStylePanel();
  };

  const setAnnotations = (list: Annotation[]): void => {
    annotations = list.slice();
    undoStack.length = 0;
    redoStack.length = 0;
    selected = null;
    moving = false;
    moveState = null;
    dragging = false;
    start = null;
    current = null;
    freehand = [];
    // 携带图元(选区即时标注/新帧)并入后,序号继续递增。
    numberPlaced = annotations.filter((op) => op.type === "number").length;
    redraw();
    syncUndo();
  };

  const clear = (): void => {
    setAnnotations([]);
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

  // 起始序号调整后本次会话的连续递增从新起点重算;空/非法输入回退当前值。
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
    if (tool === "number") {
      emitToolHint();
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

  canvas.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || !options.frame() || !editable()) {
      return;
    }
    hideContextMenu();
    const point = physicalPoint(event);
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
    if (event.button !== 0 || !options.frame() || !editable()) {
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
    if (!options.frame() || !editable()) {
      return;
    }
    event.preventDefault();
    hideContextMenu();
    if (!stylePanel.hidden) {
      toggleStylePanel(false);
    }
    // 与 mousedown 同序:先提交编辑器再算命中。编辑器提交若删除清空标注,
    // 数组索引会前移,先命中后提交会把右键菜单指到错误的标注上。
    commitEditor();
    const point = physicalPoint(event);
    const hit = hitAnnotation(point);
    if (hit === -1) {
      options.onContextMenuMiss?.();
      return;
    }
    selected = hit;
    moving = false;
    moveState = null;
    redraw();
    showContextMenu(event.clientX, event.clientY);
  });

  window.addEventListener("mousemove", (event) => {
    if (!editable()) {
      return;
    }
    if (moving && moveState) {
      const frame = options.frame();
      if (!frame) {
        return;
      }
      const point = physicalPoint(event);
      let dx = point.x - moveState.grab.x;
      let dy = point.y - moveState.grab.y;
      const b = annotationBounds(measureCtx, moveState.before);
      // 标注比画布更宽/更高时钳制区间为空(min>max),clamp 会恒取 max 把标注
      // 吸死在右/下缘、抓取点脱节;此时钳到左/上缘并保持抓取点相对偏移。
      const minX = -b.minX;
      const maxX = frame.width - b.maxX;
      const minY = -b.minY;
      const maxY = frame.height - b.maxY;
      dx = minX > maxX ? minX : clamp(dx, minX, maxX);
      dy = minY > maxY ? minY : clamp(dy, minY, maxY);
      if (dx !== 0 || dy !== 0) {
        moveState.moved = true;
      }
      annotations[moveState.index] = translateOp(moveState.before, dx, dy);
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
    if (!editable()) {
      return;
    }
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

  toolbar.addEventListener("click", (event) => {
    const button = event.target instanceof Element ? event.target.closest("button") : null;
    if (button && !button.dataset.tool && !button.dataset.action && button.closest("[data-style-panel]")) {
      // 样式面板内由各自监听处理。
      return;
    }
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    const nextTool = button.dataset.tool;
    if (nextTool && isAnnotationTool(nextTool)) {
      setTool(nextTool);
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
    }
  });

  contextMenu.addEventListener("click", (event) => {
    const button = event.target instanceof Element ? event.target.closest("button") : null;
    if (button instanceof HTMLButtonElement && button.dataset.action === "delete-annotation") {
      hideContextMenu();
      deleteSelected();
    }
  });

  window.addEventListener("keydown", (event) => {
    if (event.defaultPrevented || event.isComposing || composing || event.keyCode === 229) {
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
      return;
    }
    const mod = event.ctrlKey || event.metaKey;
    if (mod) {
      // 宿主接管工具(取字)时仍允许撤销/重做既有标注。
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
      }
      return;
    }
    // 输入框聚焦时不抢工具快捷键/删除键(退格属于输入编辑)。
    if (document.activeElement === editor || document.activeElement === numberStartInput) {
      return;
    }
    if (!editable()) {
      return;
    }
    if (event.altKey || event.shiftKey || editorOpen()) {
      return;
    }
    if (event.key === "Delete" || event.key === "Backspace") {
      if (selected !== null) {
        event.preventDefault();
        deleteSelected();
      }
      return;
    }
    const nextTool = TOOL_KEYS[event.key.toLowerCase()];
    if (nextTool) {
      event.preventDefault();
      setTool(nextTool);
    }
  });

  const refreshLabels = (): void => {
    toolbar.querySelectorAll<HTMLButtonElement>("[data-tool]").forEach((button) => {
      const value = button.dataset.tool;
      if (!value || !isAnnotationTool(value)) {
        return;
      }
      button.title = t(TOOL_TITLE_KEYS[value]);
      button.setAttribute("aria-label", t(TOOL_LABEL_KEYS[value]));
    });
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
  };

  syncToolUi();
  syncStylePanel();
  syncUndo();

  return {
    paint: (target: CanvasRenderingContext2D): void => {
      for (const op of annotations) {
        const style = annotationStyle(op);
        paintAnnotation(target, op, style.color, style.lineWidth);
      }
      if (selected !== null && annotations[selected]) {
        paintSelectionBox(target, annotations[selected], selectColor, frameScale());
      }
      if (dragging && start && current && isDragTool(tool)) {
        const op = draft(tool, start, current, mosaicBlock(), styleColor, styleWidth);
        if (op) {
          const style = annotationStyle(op);
          paintAnnotation(target, op, style.color, style.lineWidth);
        }
      }
      if (dragging && freehand.length >= 2 && isFreehandTool(tool)) {
        const op: Annotation = {
          type: tool,
          points: freehand,
          color: styleColor,
          strokeWidth: styleWidth,
        };
        const style = annotationStyle(op);
        paintAnnotation(target, op, style.color, style.lineWidth);
      }
    },
    tool: () => tool,
    setTool,
    deactivateTool,
    annotations: () => annotations.slice(),
    exportList: () => exportableList(annotations),
    setAnnotations,
    clear,
    undo,
    redo,
    canUndo: () => undoStack.length > 0,
    deleteSelected,
    selectedIndex: () => selected,
    clearSelection: (): void => {
      selected = null;
      moving = false;
      moveState = null;
      redraw();
    },
    isTextEditing: editorOpen,
    hasContent: () =>
      annotations.length > 0 ||
      editorOpen() ||
      (dragging && (start !== null || freehand.length > 0)),
    commitText: commitEditor,
    cancelText: cancelEditor,
    style: () => ({ color: styleColor, width: styleWidth, textSize: styleTextBase, numberStart }),
    setStyle,
    refreshLabels,
  };
}

const TOOL_KEYS: Record<string, AnnotationTool> = {
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

export function isAnnotationTool(value: string): value is AnnotationTool {
  return (ANNOTATION_TOOLS as string[]).includes(value);
}

/** 工具快捷键 → 工具(A/R/E/L/M/B/H/P/N/T);宿主在非标注工具激活时也可用。 */
export function annotationToolForKey(key: string): AnnotationTool | null {
  return TOOL_KEYS[key.toLowerCase()] ?? null;
}

/** 降级编辑器:上下文缺失时宿主可继续工作(不产生标注)。 */
function noopEditor(): AnnotationEditor {
  return {
    paint: () => undefined,
    tool: () => "arrow",
    setTool: () => undefined,
    deactivateTool: () => undefined,
    annotations: () => [],
    exportList: () => [],
    setAnnotations: () => undefined,
    clear: () => undefined,
    undo: () => undefined,
    redo: () => undefined,
    canUndo: () => false,
    deleteSelected: () => undefined,
    selectedIndex: () => null,
    clearSelection: () => undefined,
    isTextEditing: () => false,
    hasContent: () => false,
    commitText: () => undefined,
    cancelText: () => undefined,
    style: () => ({ color: FALLBACK_STROKE, width: null, textSize: null, numberStart: MIN_NUMBER_START }),
    setStyle: () => undefined,
    refreshLabels: () => undefined,
  };
}
