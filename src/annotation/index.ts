import { invoke } from "@tauri-apps/api/core";
import { t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import "./editor.css";

// R21:预览编辑器与 Wayland Web 覆盖层共用的标注层。
// 坐标契约:全部图元使用「冻帧物理像素」坐标;画布(`paint` 的目标 ctx)必须
// 与冻帧同尺寸,马赛克/高斯模糊/聚光灯/放大镜/擦除直接在该画布上取像素。
// 宿主负责先画底图(预览为源画布,覆盖层为冻帧原图),覆盖层再按选区裁剪合成。
//
// R5:工具由 `TOOL_REGISTRY` 单一注册表驱动(创建方式/图标/i18n/快捷键/开关 id),
// 工具条、更多面板、样式面板模式与快捷键全部读取它;直线/画笔/模糊作为
// 箭头/荧光笔/马赛克的模式提供,`Annotation` 仍保留全部既有变体用于渲染与
// 历史再编辑。

/** R5:可作为创建入口的标注工具。合并后的 line/pen/blur 不再是工具,只作为模式。 */
export type AnnotationTool =
  | "arrow"
  | "rect"
  | "ellipse"
  | "mosaic"
  | "highlighter"
  | "number"
  | "text"
  | "spotlight"
  | "magnifier"
  | "bubble"
  | "sticker"
  | "erase";

/** R5:合并工具的等效模式:arrow→line、highlighter→pen、mosaic→blur。 */
export type ToolMode = "arrow" | "line" | "highlighter" | "pen" | "mosaic" | "blur";

export type ToolKind = "drag" | "freehand" | "text" | "number" | "sticker";

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
  | { type: "blur"; x: number; y: number; width: number; height: number; sigma: number }
  | { type: "spotlight"; x: number; y: number; width: number; height: number; dim: number }
  | {
      type: "magnifier";
      x: number;
      y: number;
      width: number;
      height: number;
      zoom: number;
      color: string;
    }
  | {
      type: "bubble";
      x: number;
      y: number;
      width: number;
      height: number;
      text: string;
      size: number;
      color: string;
    }
  | { type: "sticker"; x: number; y: number; width: number; height: number; sticker: string }
  | { type: "erase"; x: number; y: number; width: number; height: number; color: string | null };

export interface AnnotationStyle {
  color: string;
  width: number | null;
  textSize: number | null;
  numberStart: number;
}

/** R5:创建拖拽图元所需的当前工具参数。 */
export interface DraftSettings {
  block: number;
  color: string;
  strokeWidth: number | null;
  textSize: number;
  spotlightDim: number;
  magnifierZoom: number;
  eraseFill: string | null;
}

/** 工具提示词条(由宿主决定呈现方式:预览写提示条,覆盖层可忽略)。 */
export interface AnnotationHint {
  key: CatalogKey;
  params?: Record<string, string | number>;
}

/** 样式持久化等错误:词条键或宿主错误串。 */
export type AnnotationError = AnnotationHint | string;

/** R5:逐项工具开关表(工具 id → 是否启用);缺失或非布尔值按启用处理。 */
export interface AnnotationToolToggles {
  [tool: string]: boolean;
}

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
  /** R5:逐项工具开关;null/undefined = 全部可用(设置读取失败时保守回退)。 */
  toolToggles?: AnnotationToolToggles | null;
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
  /** R5:应用逐项工具开关;关闭的工具从工具条与更多面板移除,当前工具被关闭时切换。 */
  setToolToggles: (toggles: AnnotationToolToggles | null) => void;
  refreshLabels: () => void;
}

export interface ToolModeDefinition {
  id: ToolMode;
  labelKey: CatalogKey;
}

export interface ToolDefinition {
  id: AnnotationTool;
  /** 常驻工具条(否则收进「更多」面板)。 */
  primary: boolean;
  kind: ToolKind;
  /** 单键快捷键(与既有 A/R/E/L/M/B/H/P/N/T 语义对齐:L/P/B 走保留工具的模式)。 */
  shortcut: string;
  icon: string;
  titleKey: CatalogKey;
  labelKey: CatalogKey;
  hintKey?: CatalogKey;
  /** 合并工具的等效模式;无模式时为 undefined。 */
  modes?: ToolModeDefinition[];
  defaultMode?: ToolMode;
  /** 模式专属提示覆盖 `hintKey`(如画笔/模糊)。 */
  modeHints?: Partial<Record<ToolMode, CatalogKey>>;
}

const ARROW_MODES: ToolModeDefinition[] = [
  { id: "arrow", labelKey: "preview.tool.arrow" },
  { id: "line", labelKey: "preview.tool.line" },
];

const HIGHLIGHTER_MODES: ToolModeDefinition[] = [
  { id: "highlighter", labelKey: "preview.tool.highlighter" },
  { id: "pen", labelKey: "preview.tool.pen" },
];

const MOSAIC_MODES: ToolModeDefinition[] = [
  { id: "mosaic", labelKey: "preview.tool.mosaic" },
  { id: "blur", labelKey: "preview.tool.blur" },
];

/** R5:标注工具单一注册表;工具条/更多面板/快捷键/模式与开关可见性都读这里。 */
export const TOOL_REGISTRY: ToolDefinition[] = [
  {
    id: "arrow",
    primary: true,
    kind: "drag",
    shortcut: "a",
    icon: icons.arrow,
    titleKey: "preview.tool.arrow_title",
    labelKey: "preview.tool.arrow",
    modes: ARROW_MODES,
    defaultMode: "arrow",
  },
  {
    id: "rect",
    primary: true,
    kind: "drag",
    shortcut: "r",
    icon: icons.rect,
    titleKey: "preview.tool.rect_title",
    labelKey: "preview.tool.rect",
  },
  {
    id: "ellipse",
    primary: true,
    kind: "drag",
    shortcut: "e",
    icon: icons.ellipse,
    titleKey: "preview.tool.ellipse_title",
    labelKey: "preview.tool.ellipse",
  },
  {
    id: "highlighter",
    primary: true,
    kind: "freehand",
    shortcut: "h",
    icon: icons.highlighter,
    titleKey: "preview.tool.highlighter_title",
    labelKey: "preview.tool.highlighter",
    hintKey: "preview.note.highlighter_hint",
    modes: HIGHLIGHTER_MODES,
    defaultMode: "highlighter",
    modeHints: { pen: "preview.note.pen_hint" },
  },
  {
    id: "mosaic",
    primary: true,
    kind: "drag",
    shortcut: "m",
    icon: icons.mosaic,
    titleKey: "preview.tool.mosaic_title",
    labelKey: "preview.tool.mosaic",
    modes: MOSAIC_MODES,
    defaultMode: "mosaic",
    modeHints: { blur: "preview.note.blur_hint" },
  },
  {
    id: "text",
    primary: true,
    kind: "text",
    shortcut: "t",
    icon: icons.text,
    titleKey: "preview.tool.text_title",
    labelKey: "preview.tool.text",
    hintKey: "preview.note.text_hint",
  },
  {
    id: "number",
    primary: false,
    kind: "number",
    shortcut: "n",
    icon: icons.number,
    titleKey: "preview.tool.number_title",
    labelKey: "preview.tool.number",
    hintKey: "preview.note.number_hint",
  },
  {
    id: "spotlight",
    primary: false,
    kind: "drag",
    shortcut: "s",
    icon: icons.spotlight,
    titleKey: "preview.tool.spotlight_title",
    labelKey: "preview.tool.spotlight",
    hintKey: "preview.note.spotlight_hint",
  },
  {
    id: "magnifier",
    primary: false,
    kind: "drag",
    shortcut: "g",
    icon: icons.magnifier,
    titleKey: "preview.tool.magnifier_title",
    labelKey: "preview.tool.magnifier",
    hintKey: "preview.note.magnifier_hint",
  },
  {
    id: "bubble",
    primary: false,
    kind: "drag",
    shortcut: "c",
    icon: icons.bubble,
    titleKey: "preview.tool.bubble_title",
    labelKey: "preview.tool.bubble",
    hintKey: "preview.note.bubble_hint",
  },
  {
    id: "sticker",
    primary: false,
    kind: "sticker",
    shortcut: "k",
    icon: icons.sticker,
    titleKey: "preview.tool.sticker_title",
    labelKey: "preview.tool.sticker",
    hintKey: "preview.note.sticker_hint",
  },
  {
    id: "erase",
    primary: false,
    kind: "drag",
    shortcut: "x",
    icon: icons.erase,
    titleKey: "preview.tool.erase_title",
    labelKey: "preview.tool.erase",
    hintKey: "preview.note.erase_hint",
  },
];

const TOOL_BY_ID = new Map<AnnotationTool, ToolDefinition>(
  TOOL_REGISTRY.map((definition) => [definition.id, definition]),
);

/** 合并工具的既有快捷键:L/P/B 切到保留工具并同时选中对应模式。 */
const MODE_SHORTCUTS: Record<string, { tool: AnnotationTool; mode: ToolMode }> = {
  l: { tool: "arrow", mode: "line" },
  p: { tool: "highlighter", mode: "pen" },
  b: { tool: "mosaic", mode: "blur" },
};

export const ANNOTATION_TOOLS: AnnotationTool[] = TOOL_REGISTRY.map(
  (definition) => definition.id,
);

/** 预览/覆盖层常驻工具:对齐 Snipaste 一类主栏,少用工具收进「更多」。 */
export const PRIMARY_TOOLS: AnnotationTool[] = TOOL_REGISTRY.filter(
  (definition) => definition.primary,
).map((definition) => definition.id);

export const MORE_TOOLS: AnnotationTool[] = TOOL_REGISTRY.filter(
  (definition) => !definition.primary,
).map((definition) => definition.id);

/** 全部模式定义(含所属工具),样式面板渲染与可见性同步共用。 */
export const TOOL_MODE_ENTRIES: Array<{ tool: AnnotationTool; mode: ToolModeDefinition }> =
  TOOL_REGISTRY.flatMap((definition) =>
    (definition.modes ?? []).map((mode) => ({ tool: definition.id, mode })),
  );

const FALLBACK_STROKE = "#e11d48";
const FALLBACK_SELECT = "#1d4ed8";
export const TEXT_FONT_STACK = '"Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif';

// 与 Rust annotate::HIGHLIGHTER_ALPHA / MIN_BLUR_SIGMA / MAX_BLUR_SIGMA 对齐。
export const HIGHLIGHTER_ALPHA = 0.38;
const BLUR_SIGMA_MIN = 3;
const BLUR_SIGMA_MAX = 48;
export const MIN_NUMBER_START = 1;
export const MAX_NUMBER_START = 999;
export const MIN_DRAW_SIZE = 3;
// R5:气泡最小可创建尺寸(物理像素),小于该尺寸的拖拽不产生图元。
export const MIN_BUBBLE_WIDTH = 40;
export const MIN_BUBBLE_HEIGHT = 28;
export const DEFAULT_SPOTLIGHT_DIM = 0.55;
export const DEFAULT_MAGNIFIER_ZOOM = 3;
// 与 Rust annotate::MIN/MAX_MAGNIFIER_ZOOM 对齐;倍率上限避免放大内容失真与采样过载。
export const MIN_MAGNIFIER_ZOOM = 1.2;
export const MAX_MAGNIFIER_ZOOM = 8;

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
// 聚光灯暗度/放大镜倍率档位(与 Rust 端 clamp 上限一致)。
export const SPOTLIGHT_DIM_LEVELS = [0.35, 0.55, 0.75];
export const MAGNIFIER_ZOOM_LEVELS = [2, 3, 4, 6];
// 贴纸尺寸档位(逻辑像素,放置时乘冻帧 scale;上限受 Rust 端无限制,只影响创建入口)。
export const STICKER_SIZES: Array<{ value: number; labelKey: CatalogKey }> = [
  { value: 64, labelKey: "preview.style.text_small" },
  { value: 96, labelKey: "preview.style.text_medium" },
  { value: 144, labelKey: "preview.style.text_large" },
];

// R5:随包素材 id → 词条键;Rust `annotate::stickers` 的清单是权威来源,
// 未登记 id 回退通用「贴纸」文案。
const STICKER_LABEL_KEYS: Record<string, CatalogKey> = {
  star: "preview.sticker.star",
  heart: "preview.sticker.heart",
  check: "preview.sticker.check",
  cross: "preview.sticker.cross",
  exclaim: "preview.sticker.exclaim",
  smile: "preview.sticker.smile",
};

interface StickerInfo {
  id: string;
  width: number;
  height: number;
}

// 贴纸素材模块级缓存:多个编辑器实例(预览/覆盖层)共用同一批解码结果。
const stickerImages = new Map<string, HTMLImageElement>();
const stickerOrder: string[] = [];
let stickerLoad: Promise<void> | null = null;

function ensureStickerCatalog(): Promise<void> {
  if (stickerLoad) {
    return stickerLoad;
  }
  // 素材经后端命令取回(与 Rust 栅格化同一批 include_bytes 资产),
  // 断网可用;失败时贴纸入口保持隐藏,其它工具不受影响。
  stickerLoad = (async () => {
    const catalog = await invoke<StickerInfo[]>("get_sticker_catalog");
    for (const info of catalog) {
      if (stickerImages.has(info.id)) {
        continue;
      }
      const bytes = await invoke<ArrayBuffer>("get_sticker_image", { id: info.id });
      const url = URL.createObjectURL(new Blob([bytes], { type: "image/png" }));
      const image = new Image();
      image.src = url;
      await new Promise<void>((resolve, reject) => {
        image.onload = () => resolve();
        image.onerror = () => reject(new Error("sticker decode failed"));
      });
      stickerImages.set(info.id, image);
      stickerOrder.push(info.id);
    }
  })().catch(() => undefined);
  return stickerLoad;
}

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

/** R5:从设置对象读取逐项工具开关;缺失/非法结构返回 null(全部可用)。 */
export function readAnnotationToolToggles(settings: unknown): AnnotationToolToggles | null {
  const tools = (settings as { annotationTools?: Record<string, unknown> } | null)
    ?.annotationTools;
  if (!tools || typeof tools !== "object") {
    return null;
  }
  const toggles: AnnotationToolToggles = {};
  for (const [key, value] of Object.entries(tools)) {
    if (typeof value === "boolean") {
      toggles[key] = value;
    }
  }
  return Object.keys(toggles).length > 0 ? toggles : null;
}

/** 读取跨会话标注样式(读取失败静默回退默认)。 */
export async function loadAnnotationDefaults(): Promise<Partial<AnnotationStyle>> {
  try {
    return readAnnotationDefaults(await invoke<unknown>("get_ui_settings"));
  } catch {
    return {};
  }
}

/** R5:一次读取样式默认与逐项工具开关(读取失败静默回退:默认样式 + 全部工具)。 */
export async function loadAnnotationSettings(): Promise<{
  style: Partial<AnnotationStyle>;
  tools: AnnotationToolToggles | null;
}> {
  try {
    const settings = await invoke<unknown>("get_ui_settings");
    return {
      style: readAnnotationDefaults(settings),
      tools: readAnnotationToolToggles(settings),
    };
  } catch {
    return { style: {}, tools: null };
  }
}

export function isDragTool(tool: AnnotationTool): boolean {
  return TOOL_BY_ID.get(tool)?.kind === "drag";
}

export function isFreehandTool(tool: AnnotationTool): boolean {
  return TOOL_BY_ID.get(tool)?.kind === "freehand";
}

/** 合并工具的模式选择:模式 id 决定创建哪种既有图元。 */
export function modeForTool(tool: AnnotationTool): ToolMode | null {
  const definition = TOOL_BY_ID.get(tool);
  if (!definition?.modes?.length) {
    return null;
  }
  return definition.defaultMode ?? definition.modes[0].id;
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

/**
 * R5:由工具 + 模式 + 当前参数构造拖拽图元草稿。直线/画笔/模糊并入保留工具后,
 * 模式决定生成 `line`/`pen`/`blur` 既有图元(渲染与导出语义不变)。
 */
export function draft(
  tool: AnnotationTool,
  mode: ToolMode | null,
  start: Point,
  end: Point,
  settings: DraftSettings,
): Annotation | null {
  if (tool === "arrow") {
    if (Math.hypot(end.x - start.x, end.y - start.y) < MIN_DRAW_SIZE) {
      return null;
    }
    return mode === "line"
      ? {
          type: "line",
          from: start,
          to: end,
          color: settings.color,
          strokeWidth: settings.strokeWidth,
        }
      : {
          type: "arrow",
          from: start,
          to: end,
          color: settings.color,
          strokeWidth: settings.strokeWidth,
        };
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
    return mode === "blur"
      ? { type: "blur", x, y, width, height, sigma: blurSigma(width, height) }
      : { type: "mosaic", x, y, width, height, block: settings.block };
  }
  if (tool === "ellipse") {
    return { type: "ellipse", x, y, width, height, color: settings.color, strokeWidth: settings.strokeWidth };
  }
  if (tool === "rect") {
    return { type: "rect", x, y, width, height, color: settings.color, strokeWidth: settings.strokeWidth };
  }
  if (tool === "spotlight") {
    return { type: "spotlight", x, y, width, height, dim: settings.spotlightDim };
  }
  if (tool === "magnifier") {
    return {
      type: "magnifier",
      x,
      y,
      width,
      height,
      zoom: settings.magnifierZoom,
      color: settings.color,
    };
  }
  if (tool === "erase") {
    return { type: "erase", x, y, width, height, color: settings.eraseFill };
  }
  if (tool === "bubble") {
    if (width < MIN_BUBBLE_WIDTH || height < MIN_BUBBLE_HEIGHT) {
      return null;
    }
    return {
      type: "bubble",
      x,
      y,
      width,
      height,
      text: "",
      size: settings.textSize,
      color: settings.color,
    };
  }
  return null;
}

/** R5:自由绘制图元按模式落为 pen(不透明)或 highlighter(半透明)。 */
export function freehandDraft(
  tool: AnnotationTool,
  mode: ToolMode | null,
  points: Point[],
  color: string,
  strokeWidth: number | null,
): Annotation | null {
  if (!isFreehandTool(tool)) {
    return null;
  }
  if (tool === "highlighter" && mode === "pen") {
    return { type: "pen", points, color, strokeWidth };
  }
  return { type: "highlighter", points, color, strokeWidth };
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
  if (op.type === "spotlight") {
    paintSpotlight(ctx, op);
    return;
  }
  if (op.type === "magnifier") {
    paintMagnifier(ctx, op, stroke, lineWidth);
    return;
  }
  if (op.type === "sticker") {
    paintSticker(ctx, op);
    return;
  }
  if (op.type === "erase") {
    paintErase(ctx, op);
    return;
  }
  if (op.type === "bubble") {
    paintBubble(ctx, op, lineWidth);
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

// R5:聚光灯草稿与成品同一实现;圈外按 dim 压暗,区域本身不动。
function paintSpotlight(
  ctx: CanvasRenderingContext2D,
  op: Extract<Annotation, { type: "spotlight" }>,
): void {
  const dim = clamp(op.dim, 0, 1);
  if (dim <= 0) {
    return;
  }
  ctx.save();
  ctx.fillStyle = `rgba(0, 0, 0, ${dim})`;
  ctx.beginPath();
  ctx.rect(0, 0, ctx.canvas.width, ctx.canvas.height);
  ctx.rect(op.x, op.y, op.width, op.height);
  ctx.fill("evenodd");
  ctx.restore();
}

// R5:放大镜预览:把区域内容按倍率围绕中心放大并裁剪回区域,再画边框。
// 与 Rust magnify 的就近采样语义一致(不插值),复制/保存结果与画布一致。
function paintMagnifier(
  ctx: CanvasRenderingContext2D,
  op: Extract<Annotation, { type: "magnifier" }>,
  stroke: string,
  lineWidth: number,
): void {
  const x = clamp(Math.round(op.x), 0, ctx.canvas.width);
  const y = clamp(Math.round(op.y), 0, ctx.canvas.height);
  const w = Math.min(Math.round(op.width), ctx.canvas.width - x);
  const h = Math.min(Math.round(op.height), ctx.canvas.height - y);
  if (w < 2 || h < 2) {
    return;
  }
  const zoom = clamp(op.zoom, MIN_MAGNIFIER_ZOOM, MAX_MAGNIFIER_ZOOM);
  ctx.save();
  ctx.beginPath();
  ctx.rect(x, y, w, h);
  ctx.clip();
  const smoothing = ctx.imageSmoothingEnabled;
  ctx.imageSmoothingEnabled = false;
  const dw = w * zoom;
  const dh = h * zoom;
  ctx.drawImage(ctx.canvas, x, y, w, h, x + w / 2 - dw / 2, y + h / 2 - dh / 2, dw, dh);
  ctx.imageSmoothingEnabled = smoothing;
  ctx.restore();
  ctx.save();
  ctx.strokeStyle = stroke;
  ctx.lineWidth = lineWidth;
  ctx.lineJoin = "round";
  ctx.strokeRect(x + 0.5, y + 0.5, w - 1, h - 1);
  ctx.restore();
}

function paintSticker(
  ctx: CanvasRenderingContext2D,
  op: Extract<Annotation, { type: "sticker" }>,
): void {
  const image = stickerImages.get(op.sticker);
  if (!image) {
    return;
  }
  // 与 Rust draw_sticker 的就近缩放一致(不插值),复制/保存结果贴近画布。
  const smoothing = ctx.imageSmoothingEnabled;
  ctx.imageSmoothingEnabled = false;
  ctx.drawImage(image, op.x, op.y, op.width, op.height);
  ctx.imageSmoothingEnabled = smoothing;
}

// R5:内容擦除预览:选定颜色直接填充;未选颜色时取区域周边像素均值,
// 与 Rust erase_region 同语义。擦除只改像素,不影响其它图元。
function paintErase(ctx: CanvasRenderingContext2D, op: Extract<Annotation, { type: "erase" }>): void {
  const x = clamp(Math.round(op.x), 0, ctx.canvas.width);
  const y = clamp(Math.round(op.y), 0, ctx.canvas.height);
  const w = Math.min(Math.round(op.width), ctx.canvas.width - x);
  const h = Math.min(Math.round(op.height), ctx.canvas.height - y);
  if (w <= 0 || h <= 0) {
    return;
  }
  const explicitColor =
    typeof op.color === "string" && HEX_COLOR_RE.test(op.color) ? op.color : null;
  ctx.save();
  ctx.fillStyle = explicitColor ?? averageSurrounding(ctx, x, y, w, h);
  ctx.fillRect(x, y, w, h);
  ctx.restore();
}

/** 区域周边 2px 环带均值;整幅图都被覆盖时退化为区域自身均值。 */
function averageSurrounding(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
): string {
  const ring = 2;
  const sx = Math.max(0, x - ring);
  const sy = Math.max(0, y - ring);
  const ex = Math.min(ctx.canvas.width, x + w + ring);
  const ey = Math.min(ctx.canvas.height, y + h + ring);
  const width = ex - sx;
  const height = ey - sy;
  if (width <= 0 || height <= 0) {
    return "#ffffff";
  }
  const data = ctx.getImageData(sx, sy, width, height).data;
  let r = 0;
  let g = 0;
  let b = 0;
  let n = 0;
  for (let py = 0; py < height; py += 1) {
    for (let px = 0; px < width; px += 1) {
      const absX = sx + px;
      const absY = sy + py;
      if (absX >= x && absX < x + w && absY >= y && absY < y + h) {
        continue;
      }
      const i = (py * width + px) * 4;
      r += data[i];
      g += data[i + 1];
      b += data[i + 2];
      n += 1;
    }
  }
  if (n === 0) {
    const region = ctx.getImageData(x, y, w, h).data;
    const count = region.length / 4;
    for (let i = 0; i < region.length; i += 4) {
      r += region[i];
      g += region[i + 1];
      b += region[i + 2];
    }
    n = count;
  }
  if (n === 0) {
    return "#ffffff";
  }
  return `rgb(${Math.round(r / n)}, ${Math.round(g / n)}, ${Math.round(b / n)})`;
}

function roundedRectPath(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  radius: number,
): void {
  const r = Math.min(radius, w / 2, h / 2);
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}

/** 按框宽逐字符换行;与 Rust wrap_text 同规则(显式换行符保留)。 */
function wrapCanvasText(
  ctx: CanvasRenderingContext2D,
  text: string,
  maxWidth: number,
): string[] {
  const lines: string[] = [];
  for (const raw of text.split("\n")) {
    if (raw.length === 0) {
      lines.push("");
      continue;
    }
    let line = "";
    for (const ch of Array.from(raw)) {
      if (line.length > 0 && ctx.measureText(line + ch).width > maxWidth) {
        lines.push(line);
        line = ch;
      } else {
        line += ch;
      }
    }
    lines.push(line);
  }
  return lines;
}

// R5:对话气泡:圆角矩形主体 + 左下指向尾 + 框内自动换行文本。
function paintBubble(
  ctx: CanvasRenderingContext2D,
  op: Extract<Annotation, { type: "bubble" }>,
  lineWidth: number,
): void {
  const { x, y, width: w, height: h } = op;
  if (w < 2 || h < 2) {
    return;
  }
  const radius = clamp(Math.min(w, h) * 0.22, 4, 18);
  const tail = clamp(h * 0.45, 10, 40);
  const baseLeft = { x: x + w * 0.14, y: y + h };
  const baseRight = { x: x + w * 0.42, y: y + h };
  const apex = { x: x - tail * 0.35, y: y + h + tail };
  ctx.save();
  ctx.fillStyle = "#ffffff";
  ctx.beginPath();
  ctx.moveTo(apex.x, apex.y);
  ctx.lineTo(baseLeft.x, baseLeft.y);
  ctx.lineTo(baseRight.x, baseRight.y);
  ctx.closePath();
  ctx.fill();
  roundedRectPath(ctx, x, y, w, h, radius);
  ctx.fill();
  ctx.strokeStyle = op.color;
  ctx.lineWidth = lineWidth;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  ctx.beginPath();
  ctx.moveTo(baseLeft.x, baseLeft.y);
  ctx.lineTo(apex.x, apex.y);
  ctx.lineTo(baseRight.x, baseRight.y);
  ctx.stroke();
  if (op.text.trim().length > 0) {
    const size = Math.max(op.size, 10);
    const padding = Math.max(8, size * 0.6);
    ctx.save();
    roundedRectPath(ctx, x, y, w, h, radius);
    ctx.clip();
    ctx.font = `${size}px ${TEXT_FONT_STACK}`;
    ctx.textBaseline = "top";
    ctx.fillStyle = op.color;
    const lines = wrapCanvasText(ctx, op.text, Math.max(8, w - padding * 2));
    lines.forEach((line, index) => {
      ctx.fillText(line, x + padding, y + padding + index * size * 1.25);
    });
    ctx.restore();
  }
  roundedRectPath(ctx, x, y, w, h, radius);
  ctx.stroke();
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
  if (op.type === "bubble") {
    // 气泡包含左下指向尾,命中范围比主体略大。
    const tail = clamp(op.height * 0.45, 10, 40);
    return {
      minX: Math.min(op.x, op.x - tail * 0.35),
      minY: op.y,
      maxX: op.x + op.width,
      maxY: op.y + op.height + tail,
    };
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

function toolButton(definition: ToolDefinition): string {
  return `<button type="button" data-tool="${definition.id}" data-i18n-title="${definition.titleKey}" data-i18n-aria-label="${definition.labelKey}" data-tooltip="${t(definition.titleKey)}" aria-label="${t(definition.labelKey)}">${definition.icon}</button>`;
}

function modeButton(entry: { tool: AnnotationTool; mode: ToolModeDefinition }): string {
  return `<button type="button" data-mode-tool="${entry.tool}" data-tool-mode="${entry.mode.id}" data-i18n="${entry.mode.labelKey}" data-tooltip="${t(entry.mode.labelKey)}">${t(entry.mode.labelKey)}</button>`;
}

function stickerSizeButton(option: { value: number; labelKey: CatalogKey }): string {
  return `<button type="button" data-sticker-size="${option.value}" data-tooltip="${t("preview.style.option_title", { label: t("preview.style.sticker_size"), value: option.value })}">${t(option.labelKey)}</button>`;
}

function toolbarMarkup(): string {
  const primary = TOOL_REGISTRY.filter((definition) => definition.primary)
    .map(toolButton)
    .join("");
  const extra = TOOL_REGISTRY.filter((definition) => !definition.primary)
    .map(toolButton)
    .join("");
  return `
    ${primary}
    <div class="annotation-more" data-more-root>
      <button type="button" data-action="more" data-i18n-title="preview.tool.more_title" data-i18n-aria-label="preview.tool.more" data-tooltip="${t("preview.tool.more_title")}" aria-label="${t("preview.tool.more")}" aria-haspopup="true">${icons.more}</button>
      <div class="annotation-more-panel" data-more-panel hidden>${extra}</div>
    </div>
    <span class="toolbar-sep" aria-hidden="true"></span>
    <button type="button" data-action="undo" data-i18n-title="preview.tool.undo_title" data-i18n-aria-label="preview.tool.undo" data-tooltip="${t("preview.tool.undo_title")}" aria-label="${t("preview.tool.undo")}">${icons.undo}</button>
    <div class="annotation-style" data-style-root>
      <button type="button" data-action="style" data-i18n-title="preview.tool.style_title" data-i18n-aria-label="preview.tool.style_title" data-tooltip="${t("preview.tool.style_title")}" aria-label="${t("preview.tool.style_title")}" aria-haspopup="true">${icons.style}</button>
      <div class="annotation-style-panel" data-style-panel hidden>
        <div class="style-group" data-style-group="mode" hidden>
          <span class="style-label" data-i18n="preview.style.mode">模式</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.style.mode_group" aria-label="工具模式">
            ${TOOL_MODE_ENTRIES.map(modeButton).join("")}
          </div>
        </div>
        <div class="style-group">
          <span class="style-label" data-i18n="preview.style.color">颜色</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.style.color_group" aria-label="标注颜色">
            ${STYLE_COLORS.map(
              (color) =>
                `<button type="button" data-style-color="${color}" style="--swatch:${color}" data-tooltip="${color}" aria-label="${t("preview.style.color_aria", { color })}"></button>`,
            ).join("")}
          </div>
        </div>
        <div class="style-group">
          <span class="style-label" data-i18n="preview.style.width">线宽</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.style.width" aria-label="线宽">
            ${STYLE_WIDTHS.map(
              ({ value, labelKey }) =>
                `<button type="button" data-style-width="${value}" data-tooltip="${t("preview.style.option_title", { label: t(labelKey), value })}">${t(labelKey)}</button>`,
            ).join("")}
          </div>
        </div>
        <div class="style-group">
          <span class="style-label" data-i18n="preview.style.text_size">字号</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.style.text_size_group" aria-label="文字字号">
            ${STYLE_TEXT_SIZES.map(
              ({ value, labelKey }) =>
                `<button type="button" data-style-text-size="${value}" data-tooltip="${t("preview.style.option_title", { label: t(labelKey), value })}">${t(labelKey)}</button>`,
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
        <div class="style-group" data-style-group="zoom" hidden>
          <span class="style-label" data-i18n="preview.style.zoom">倍率</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.style.zoom" aria-label="放大倍率">
            ${MAGNIFIER_ZOOM_LEVELS.map(
              (value) =>
                `<button type="button" data-zoom="${value}" data-tooltip="${t("preview.style.option_title", { label: t("preview.style.zoom"), value: `${value}×` })}">${value}×</button>`,
            ).join("")}
          </div>
        </div>
        <div class="style-group" data-style-group="dim" hidden>
          <span class="style-label" data-i18n="preview.style.dim">暗度</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.style.dim" aria-label="聚光灯暗度">
            ${SPOTLIGHT_DIM_LEVELS.map(
              (value) =>
                `<button type="button" data-dim="${value}" data-tooltip="${t("preview.style.option_title", { label: t("preview.style.dim"), value: `${Math.round(value * 100)}%` })}">${Math.round(value * 100)}%</button>`,
            ).join("")}
          </div>
        </div>
        <div class="style-group" data-style-group="erase" hidden>
          <span class="style-label" data-i18n="preview.style.erase_fill">填充</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.style.erase_fill" aria-label="擦除填充">
            <button type="button" data-erase-fill="auto" data-i18n-title="preview.style.erase_auto_title" data-tooltip="${t("preview.style.erase_auto_title")}" aria-label="${t("preview.style.erase_auto_title")}">${t("preview.style.erase_auto")}</button>
            ${STYLE_COLORS.map(
              (color) =>
                `<button type="button" data-erase-fill="${color}" style="--swatch:${color}" data-tooltip="${t("preview.style.color_aria", { color })}" aria-label="${t("preview.style.color_aria", { color })}"></button>`,
            ).join("")}
          </div>
        </div>
        <div class="style-group" data-style-group="sticker" hidden>
          <span class="style-label" data-i18n="preview.style.sticker">素材</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.style.sticker" aria-label="贴纸素材" data-sticker-options></div>
        </div>
        <div class="style-group" data-style-group="sticker-size" hidden>
          <span class="style-label" data-i18n="preview.style.sticker_size">大小</span>
          <div class="style-options" role="group" data-i18n-aria-label="preview.style.sticker_size" aria-label="贴纸大小">
            ${STICKER_SIZES.map(stickerSizeButton).join("")}
          </div>
        </div>
      </div>
    </div>
  `;
}

function snapDevicePx(value: number): number {
  const dpr = window.devicePixelRatio || 1;
  return Math.round(value * dpr) / dpr;
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
  const morePanel = toolbar.querySelector("[data-more-panel]");
  const moreBtn = toolbar.querySelector("[data-action=more]");
  const moreRoot = toolbar.querySelector("[data-more-root]");
  const numberStartInput = toolbar.querySelector("[data-style-number-start]");
  const modeGroup = toolbar.querySelector("[data-style-group=mode]");
  const zoomGroup = toolbar.querySelector("[data-style-group=zoom]");
  const dimGroup = toolbar.querySelector("[data-style-group=dim]");
  const eraseGroup = toolbar.querySelector("[data-style-group=erase]");
  const stickerGroup = toolbar.querySelector("[data-style-group=sticker]");
  const stickerSizeGroup = toolbar.querySelector("[data-style-group=sticker-size]");
  const stickerOptions = toolbar.querySelector("[data-sticker-options]");
  if (
    !(undoBtn instanceof HTMLButtonElement) ||
    !(stylePanel instanceof HTMLElement) ||
    !(styleBtn instanceof HTMLButtonElement) ||
    !(styleRoot instanceof HTMLElement) ||
    !(morePanel instanceof HTMLElement) ||
    !(moreBtn instanceof HTMLButtonElement) ||
    !(moreRoot instanceof HTMLElement) ||
    !(numberStartInput instanceof HTMLInputElement) ||
    !(modeGroup instanceof HTMLElement) ||
    !(zoomGroup instanceof HTMLElement) ||
    !(dimGroup instanceof HTMLElement) ||
    !(eraseGroup instanceof HTMLElement) ||
    !(stickerGroup instanceof HTMLElement) ||
    !(stickerSizeGroup instanceof HTMLElement) ||
    !(stickerOptions instanceof HTMLElement)
  ) {
    return noopEditor();
  }

  const rootStyle = getComputedStyle(root);
  const strokeColor = resolveCanvasColor(rootStyle.getPropertyValue("--stroke"), FALLBACK_STROKE);
  const selectColor = resolveCanvasColor(rootStyle.getPropertyValue("--accent"), FALLBACK_SELECT);

  // 测量文字包围盒用 ctx 与画布尺寸无关;命中/包围盒不改变 canvas 状态。
  const measureCtx = ctx;

  let tool: AnnotationTool = "arrow";
  let toolToggles: AnnotationToolToggles | null = options.toolToggles ?? null;
  const activeModes: Partial<Record<AnnotationTool, ToolMode>> = {};
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
  let spotlightDim = DEFAULT_SPOTLIGHT_DIM;
  let magnifierZoom = DEFAULT_MAGNIFIER_ZOOM;
  let eraseFill: string | null = null;
  let stickerSizeBase: number | null = null;
  let activeSticker: string | null = null;

  const editable = (): boolean => options.isEditable?.() !== false;

  const frameScale = (): number => Math.max(options.frame()?.scale ?? 1, 1);

  const mosaicBlock = (): number => Math.max(8, Math.round(12 * frameScale()));

  const sizeBase = (base: number | null, fallback: number, min: number): number => {
    const frame = options.frame();
    const dpi = Math.max(frame?.scale ?? 1, 1);
    const longestEdge = Math.max(frame?.width ?? 0, frame?.height ?? 0);
    return Math.max(min, Math.round((base ?? fallback) * Math.max(dpi, longestEdge / 1920)));
  };

  const textSize = (): number => sizeBase(styleTextBase, 28, 22);

  const stickerSize = (): number => sizeBase(stickerSizeBase, 96, 24);

  // R5:贴纸入口只在素材实际可用时出现;开关关闭的其它工具一律隐藏。
  const isToolEnabled = (id: AnnotationTool): boolean => {
    if (id === "sticker" && stickerImages.size === 0) {
      return false;
    }
    return toolToggles === null || toolToggles[id] !== false;
  };

  const modeFor = (id: AnnotationTool): ToolMode | null => {
    const definition = TOOL_BY_ID.get(id);
    if (!definition?.modes?.length) {
      return null;
    }
    return activeModes[id] ?? definition.defaultMode ?? definition.modes[0].id;
  };

  const toolSettings = (): DraftSettings => ({
    block: mosaicBlock(),
    color: styleColor,
    strokeWidth: styleWidth,
    textSize: textSize(),
    spotlightDim,
    magnifierZoom,
    eraseFill,
  });

  // 与 Rust raster resolve_stroke 同一数值推导:逻辑档位 × scale 后 clamp 2..8。
  const strokeFor = (opWidth: number | null): number =>
    Math.min(8, Math.max(2, (opWidth ?? 3) * frameScale()));
  const colorFor = (opColor: string): string =>
    HEX_COLOR_RE.test(opColor) ? opColor : strokeColor;
  const annotationStyle = (op: Annotation): { color: string; lineWidth: number } => {
    if (op.type === "mosaic" || op.type === "blur" || op.type === "spotlight" || op.type === "sticker" || op.type === "erase") {
      return { color: strokeColor, lineWidth: strokeFor(null) };
    }
    if (op.type === "text" || op.type === "number" || op.type === "bubble" || op.type === "magnifier") {
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

  const syncToolVisibility = (): void => {
    toolbar.querySelectorAll<HTMLButtonElement>("[data-tool]").forEach((button) => {
      const value = button.dataset.tool;
      button.hidden = !value || !isAnnotationTool(value) || !isToolEnabled(value);
    });
    const moreVisible = MORE_TOOLS.some((id) => isToolEnabled(id));
    moreRoot.hidden = !moreVisible;
    if (!moreVisible) {
      toggleMorePanel(false);
    }
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
    contextMenu.style.left = `${snapDevicePx(left)}px`;
    contextMenu.style.top = `${snapDevicePx(top)}px`;
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
    const targetOp = target !== null ? annotations[target] : undefined;
    if (targetOp && (targetOp.type === "text" || targetOp.type === "bubble")) {
      if (text.trim().length === 0) {
        pushAction({ kind: "remove", index: target as number, op: targetOp });
      } else if (text !== targetOp.text) {
        pushAction({
          kind: "replace",
          index: target as number,
          before: targetOp,
          after: { ...targetOp, text },
        });
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
    editor.style.left = `${snapDevicePx(canvasRect.left - hostRect.left + origin.x * scale.x - insetX)}px`;
    editor.style.top = `${snapDevicePx(canvasRect.top - hostRect.top + origin.y * scale.y - insetY)}px`;
    editor.style.fontSize = `${snapDevicePx(fontSize)}px`;
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

  // 双击文字/气泡原位重编辑:载入原文本与原字号,提交时走 replace 动作。
  const openTextEditor = (index: number): void => {
    if (composing) {
      return;
    }
    const op = annotations[index];
    if (!op) {
      return;
    }
    let origin: Point;
    if (op.type === "text") {
      origin = { x: op.x, y: op.y };
    } else if (op.type === "bubble") {
      const padding = Math.max(8, op.size * 0.6);
      origin = { x: op.x + padding, y: op.y + padding };
    } else {
      return;
    }
    commitEditor();
    editTarget = index;
    selected = null;
    moving = false;
    moveState = null;
    showEditor(origin, op.text, op.size);
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
    const definition = TOOL_BY_ID.get(tool);
    const mode = modeFor(tool);
    const key =
      (mode ? definition?.modeHints?.[mode] : undefined) ?? definition?.hintKey ?? null;
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
    moreBtn.classList.toggle("active", MORE_TOOLS.includes(tool));
  };

  const setTool = (next: AnnotationTool): void => {
    commitEditor();
    tool = next;
    syncToolUi();
    syncStylePanel();
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

  /** 合并工具快捷键:切到保留工具并选中等效模式。 */
  const setToolMode = (nextTool: AnnotationTool, mode: ToolMode): void => {
    if (tool !== nextTool) {
      setTool(nextTool);
    }
    activeModes[nextTool] = mode;
    syncToolUi();
    syncStylePanel();
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

  const replaceChildren = (host: Element, children: Element[]): void => {
    host.replaceChildren(...children);
  };

  const renderStickerOptions = (): void => {
    const buttons: HTMLElement[] = [];
    for (const id of stickerOrder) {
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.stickerId = id;
      const labelKey = STICKER_LABEL_KEYS[id];
      const label = labelKey ? t(labelKey) : t("preview.tool.sticker");
      button.dataset.tooltip = label;
      button.setAttribute("aria-label", label);
      const image = document.createElement("img");
      image.src = stickerImages.get(id)?.src ?? "";
      image.alt = "";
      button.appendChild(image);
      buttons.push(button);
    }
    replaceChildren(stickerOptions, buttons);
    if (!activeSticker || !stickerImages.has(activeSticker)) {
      activeSticker = stickerOrder[0] ?? null;
    }
    syncStickerSelection();
  };

  const setGroupActive = (
    host: Element,
    attribute: string,
    active: string,
  ): void => {
    host.querySelectorAll<HTMLButtonElement>(`[${attribute}]`).forEach((button) => {
      button.classList.toggle("active", button.getAttribute(attribute) === active);
    });
  };

  const syncStickerSelection = (): void => {
    setGroupActive(stickerOptions, "data-sticker-id", activeSticker ?? "");
  };

  const syncStylePanel = (): void => {
    const definition = TOOL_BY_ID.get(tool);
    const modes = definition?.modes ?? [];
    modeGroup.hidden = modes.length === 0;
    if (modes.length > 0) {
      const active = modeFor(tool);
      modeGroup.querySelectorAll<HTMLButtonElement>("[data-mode-tool]").forEach((button) => {
        button.hidden = button.dataset.modeTool !== tool;
        button.classList.toggle("active", button.dataset.toolMode === active);
      });
    }
    zoomGroup.hidden = tool !== "magnifier";
    if (tool === "magnifier") {
      setGroupActive(zoomGroup, "data-zoom", String(magnifierZoom));
    }
    dimGroup.hidden = tool !== "spotlight";
    if (tool === "spotlight") {
      setGroupActive(dimGroup, "data-dim", String(spotlightDim));
    }
    eraseGroup.hidden = tool !== "erase";
    if (tool === "erase") {
      setGroupActive(eraseGroup, "data-erase-fill", eraseFill ?? "auto");
    }
    stickerGroup.hidden = tool !== "sticker";
    stickerSizeGroup.hidden = tool !== "sticker";
    if (tool === "sticker") {
      setGroupActive(stickerSizeGroup, "data-sticker-size", String(stickerSizeBase ?? 96));
      syncStickerSelection();
    }
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

  const toggleMorePanel = (open?: boolean): void => {
    const next = open ?? morePanel.hidden;
    morePanel.hidden = !next;
    moreBtn.setAttribute("aria-expanded", next ? "true" : "false");
    if (next) {
      toggleStylePanel(false);
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
      toggleMorePanel(false);
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

  const setToolToggles = (next: AnnotationToolToggles | null): void => {
    toolToggles = next ?? null;
    syncToolVisibility();
    if (!isToolEnabled(tool)) {
      // 当前工具被关闭:切到第一个可用工具;全部关闭时保留内部工具但不再创建。
      const fallback = ANNOTATION_TOOLS.find((id) => isToolEnabled(id));
      if (fallback) {
        setTool(fallback);
        return;
      }
    }
    syncToolUi();
    syncStylePanel();
    redraw();
  };

  stylePanel.addEventListener("click", (event) => {
    const button = event.target instanceof Element ? event.target.closest("button") : null;
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    const nextColor = button.dataset.styleColor;
    const nextWidth = button.dataset.styleWidth;
    const nextTextSize = button.dataset.styleTextSize;
    const nextMode = button.dataset.toolMode;
    const nextZoom = button.dataset.zoom;
    const nextDim = button.dataset.dim;
    const nextEraseFill = button.dataset.eraseFill;
    const nextStickerId = button.dataset.stickerId;
    const nextStickerSize = button.dataset.stickerSize;
    if (nextColor) {
      styleColor = nextColor;
    } else if (nextWidth !== undefined) {
      styleWidth = Number(nextWidth);
    } else if (nextTextSize !== undefined) {
      styleTextBase = Number(nextTextSize);
    } else if (nextMode && isAnnotationTool(tool)) {
      activeModes[tool] = nextMode as ToolMode;
      emitToolHint();
    } else if (nextZoom !== undefined) {
      magnifierZoom = clamp(Number(nextZoom), MIN_MAGNIFIER_ZOOM, MAX_MAGNIFIER_ZOOM);
    } else if (nextDim !== undefined) {
      spotlightDim = clamp(Number(nextDim), 0, 1);
    } else if (nextEraseFill !== undefined) {
      eraseFill = nextEraseFill === "auto" ? null : nextEraseFill;
    } else if (nextStickerId) {
      activeSticker = nextStickerId;
    } else if (nextStickerSize !== undefined) {
      stickerSizeBase = Number(nextStickerSize);
    } else {
      return;
    }
    syncStylePanel();
    if (nextColor || nextWidth !== undefined || nextTextSize !== undefined) {
      persistStyle();
    }
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
    if (!stylePanel.hidden && !(event.target instanceof Node && styleRoot.contains(event.target))) {
      toggleStylePanel(false);
    }
    if (!morePanel.hidden && !(event.target instanceof Node && moreRoot.contains(event.target))) {
      toggleMorePanel(false);
    }
  });

  const placeSticker = (point: Point): void => {
    const frame = options.frame();
    const id = activeSticker;
    if (!frame || !id || !stickerImages.has(id)) {
      return;
    }
    const size = stickerSize();
    const x = clamp(point.x - size / 2, 0, Math.max(0, frame.width - size));
    const y = clamp(point.y - size / 2, 0, Math.max(0, frame.height - size));
    pushAction({
      kind: "add",
      index: annotations.length,
      op: { type: "sticker", x, y, width: size, height: size, sticker: id },
    });
    redraw();
    syncUndo();
  };

  canvas.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || !options.frame() || !editable()) {
      return;
    }
    hideContextMenu();
    const point = physicalPoint(event);
    const toolActive = isToolEnabled(tool);
    if (toolActive && tool === "text") {
      placeEditor(point);
      return;
    }
    commitEditor();
    // 绘制工具下先做命中:命中已放标注则进入选中+拖移,否则清空选中并回到绘制起笔。
    // 工具被逐项开关关闭时仍允许选中/移动/删除已有标注(编辑操作常驻)。
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
    if (!toolActive) {
      return;
    }
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
    if (tool === "sticker") {
      placeSticker(point);
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
    const op = hit !== -1 ? annotations[hit] : undefined;
    if (op && (op.type === "text" || op.type === "bubble")) {
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
    if (!morePanel.hidden) {
      toggleMorePanel(false);
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
      const op = freehandDraft(tool, modeFor(tool), points, styleColor, styleWidth);
      if (op && polylineLength(points) >= MIN_DRAW_SIZE) {
        pushAction({ kind: "add", index: annotations.length, op });
      }
    } else if (isDragTool(tool) && start && current) {
      const op = draft(tool, modeFor(tool), start, current, toolSettings());
      if (op) {
        pushAction({ kind: "add", index: annotations.length, op });
        // 气泡创建后直接进入文本编辑:拖出形状即输入内容。
        if (op.type === "bubble") {
          openTextEditor(annotations.length - 1);
        }
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
      if (MORE_TOOLS.includes(nextTool)) {
        toggleMorePanel(false);
      }
      return;
    }
    if (button.dataset.action === "more") {
      toggleMorePanel();
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
      if (!morePanel.hidden) {
        event.preventDefault();
        toggleMorePanel(false);
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
    const lower = event.key.toLowerCase();
    const modeShortcut = MODE_SHORTCUTS[lower];
    if (modeShortcut) {
      if (isToolEnabled(modeShortcut.tool)) {
        event.preventDefault();
        setToolMode(modeShortcut.tool, modeShortcut.mode);
      }
      return;
    }
    const nextTool = annotationToolForKey(lower);
    if (nextTool && isToolEnabled(nextTool)) {
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
      const definition = TOOL_BY_ID.get(value);
      if (!definition) {
        return;
      }
      button.dataset.tooltip = t(definition.titleKey);
      button.setAttribute("aria-label", t(definition.labelKey));
    });
    toolbar.querySelectorAll<HTMLButtonElement>("[data-tool-mode]").forEach((button) => {
      const entry = TOOL_MODE_ENTRIES.find(
        (item) => item.tool === button.dataset.modeTool && item.mode.id === button.dataset.toolMode,
      );
      if (entry) {
        button.textContent = t(entry.mode.labelKey);
        button.dataset.tooltip = t(entry.mode.labelKey);
      }
    });
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-style-width]").forEach((button) => {
      const option = STYLE_WIDTHS.find((item) => String(item.value) === button.dataset.styleWidth);
      if (option) {
        button.textContent = t(option.labelKey);
        button.dataset.tooltip = t("preview.style.option_title", {
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
        button.dataset.tooltip = t("preview.style.option_title", {
          label: t(option.labelKey),
          value: option.value,
        });
      }
    });
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-sticker-size]").forEach((button) => {
      const option = STICKER_SIZES.find(
        (item) => String(item.value) === button.dataset.stickerSize,
      );
      if (option) {
        button.textContent = t(option.labelKey);
        button.dataset.tooltip = t("preview.style.option_title", {
          label: t("preview.style.sticker_size"),
          value: option.value,
        });
      }
    });
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-zoom]").forEach((button) => {
      const value = button.dataset.zoom ?? "";
      button.dataset.tooltip = t("preview.style.option_title", {
        label: t("preview.style.zoom"),
        value: `${value}×`,
      });
    });
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-dim]").forEach((button) => {
      const value = Number(button.dataset.dim ?? 0);
      button.dataset.tooltip = t("preview.style.option_title", {
        label: t("preview.style.dim"),
        value: `${Math.round(value * 100)}%`,
      });
    });
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-style-color]").forEach((button) => {
      const color = button.dataset.styleColor ?? "";
      button.dataset.tooltip = color;
      button.setAttribute("aria-label", t("preview.style.color_aria", { color }));
    });
    stylePanel.querySelectorAll<HTMLButtonElement>("[data-erase-fill]").forEach((button) => {
      const value = button.dataset.eraseFill ?? "";
      if (value === "auto") {
        button.dataset.tooltip = t("preview.style.erase_auto_title");
      } else {
        button.dataset.tooltip = t("preview.style.color_aria", { color: value });
      }
    });
    stickerOptions.querySelectorAll<HTMLButtonElement>("[data-sticker-id]").forEach((button) => {
      const id = button.dataset.stickerId ?? "";
      const labelKey = STICKER_LABEL_KEYS[id];
      const label = labelKey ? t(labelKey) : t("preview.tool.sticker");
      button.dataset.tooltip = label;
      button.setAttribute("aria-label", label);
    });
  };

  syncToolVisibility();
  syncToolUi();
  syncStylePanel();
  syncUndo();
  // 素材异步就绪后刷新入口可用性/缩略图;失败时贴纸入口保持隐藏。
  void ensureStickerCatalog().then(() => {
    renderStickerOptions();
    syncToolVisibility();
    syncStylePanel();
    redraw();
  });

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
        const op = draft(tool, modeFor(tool), start, current, toolSettings());
        if (op) {
          const style = annotationStyle(op);
          paintAnnotation(target, op, style.color, style.lineWidth);
        }
      }
      if (dragging && freehand.length >= 2 && isFreehandTool(tool)) {
        const op = freehandDraft(tool, modeFor(tool), freehand, styleColor, styleWidth);
        if (op) {
          const style = annotationStyle(op);
          paintAnnotation(target, op, style.color, style.lineWidth);
        }
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
    setToolToggles,
    refreshLabels,
  };
}

const SHORTCUTS: Record<string, AnnotationTool> = Object.fromEntries(
  TOOL_REGISTRY.map((definition) => [definition.shortcut, definition.id]),
) as Record<string, AnnotationTool>;

export function isAnnotationTool(value: string): value is AnnotationTool {
  return (ANNOTATION_TOOLS as string[]).includes(value);
}

/** 工具快捷键 → 工具(A/R/E/M/H/N/T/S/G/C/K/X);L/P/B 返回保留工具(模式见编辑器内)。 */
export function annotationToolForKey(key: string): AnnotationTool | null {
  const lower = key.toLowerCase();
  const modeShortcut = MODE_SHORTCUTS[lower];
  if (modeShortcut) {
    return modeShortcut.tool;
  }
  return SHORTCUTS[lower] ?? null;
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
    setToolToggles: () => undefined,
    refreshLabels: () => undefined,
  };
}
