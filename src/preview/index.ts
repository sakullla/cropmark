import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./preview.css";

interface PreviewFrame {
  width: number;
  height: number;
  scale: number;
}

type Tool = "arrow" | "rect" | "mosaic" | "text" | "ocr";

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
  | { type: "text"; x: number; y: number; text: string; size: number; color: string };

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

const FALLBACK_STROKE = "#e11d48";
const FALLBACK_OCR_HL = "#0ea5e9";
const FALLBACK_OCR_HL_STRONG = "#0369a1";
const FALLBACK_SELECT = "#2563eb";
const TEXT_FONT_STACK = '"Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif';

// 与 Rust parse_hex_color 对齐：接受 #rgb / #rrggbb / #rrggbbaa，其余形式回退默认色。
const HEX_COLOR_RE = /^#(?:[0-9a-f]{3}|[0-9a-f]{6}|[0-9a-f]{8})$/i;

// 样式预设：颜色含现行玫红；线宽/字号档位为逻辑值，绘制时乘 scale 并 clamp 2..8（线宽）。
const STYLE_COLORS = ["#e11d48", "#2563eb", "#f59e0b", "#10b981", "#111827"];
const STYLE_WIDTHS: Array<{ value: number; label: string }> = [
  { value: 2, label: "细" },
  { value: 3, label: "标准" },
  { value: 5, label: "粗" },
];
const STYLE_TEXT_SIZES: Array<{ value: number; label: string }> = [
  { value: 12, label: "小" },
  { value: 16, label: "中" },
  { value: 22, label: "大" },
];

const ICONS: Record<Exclude<Tool, "ocr"> | "undo" | "style", string> = {
  arrow: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 12.5 12.5 3.5M7 3.5h5.5V9" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  rect: `<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="3" y="4" width="10" height="8" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.7"/></svg>`,
  mosaic: `<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="2" width="5" height="5" fill="currentColor"/><rect x="9" y="2" width="5" height="5" fill="currentColor" opacity="0.45"/><rect x="2" y="9" width="5" height="5" fill="currentColor" opacity="0.65"/><rect x="9" y="9" width="5" height="5" fill="currentColor" opacity="0.28"/></svg>`,
  text: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4.2h8M8 4.2v8.2M5.5 12.4h5" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>`,
  undo: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 7h6.2a3 3 0 1 1 0 6H9" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><path d="M4 7 6.4 4.6M4 7l2.4 2.4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>`,
  style: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 1.8a6.2 6.2 0 0 0 0 12.4c.9 0 1.4-.6 1.4-1.3 0-1.1 1-1.4 2.2-1.4h1.1c.9 0 1.5-.7 1.5-1.9A6.2 6.2 0 0 0 8 1.8Z" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="5.2" cy="6.4" r="1" fill="currentColor"/><circle cx="8.6" cy="4.8" r="1" fill="currentColor"/><circle cx="11.4" cy="7.2" r="1" fill="currentColor"/></svg>`,
};

export function mountPreview(root: HTMLElement): void {
  root.className = "preview-root";
  root.dataset.tool = "arrow";
  root.innerHTML = `
    <header class="preview-titlebar" data-tauri-drag-region>
      <div class="brand" data-tauri-drag-region>
        <span class="mark" aria-hidden="true"></span>
        <span class="name">Cropmark</span>
      </div>
      <p class="preview-note">未标注图已复制</p>
      <button type="button" class="preview-close" data-action="close" aria-label="关闭" data-tauri-drag-region="false">关闭</button>
    </header>
    <div class="preview-toolbar">
      <div class="preview-tools" role="toolbar" aria-label="标注">
        <button type="button" data-tool="arrow" title="箭头 (A)" aria-label="箭头">${ICONS.arrow}</button>
        <button type="button" data-tool="rect" title="框 (R)" aria-label="框">${ICONS.rect}</button>
        <button type="button" data-tool="mosaic" title="马赛克 (M)" aria-label="马赛克">${ICONS.mosaic}</button>
        <button type="button" data-tool="text" title="文字框 (T)" aria-label="文字框" class="tool-text">${ICONS.text}<span>文字</span></button>
        <button type="button" data-action="undo" title="撤销 (Ctrl+Z)" aria-label="撤销">${ICONS.undo}</button>
        <div class="preview-style" data-style-root>
          <button type="button" data-action="style" title="标注样式" aria-label="标注样式" aria-haspopup="true">${ICONS.style}</button>
          <div class="preview-style-panel" data-style-panel hidden>
            <div class="style-group">
              <span class="style-label">颜色</span>
              <div class="style-options" role="group" aria-label="标注颜色">
                ${STYLE_COLORS.map(
                  (color) =>
                    `<button type="button" data-style-color="${color}" style="--swatch:${color}" title="${color}" aria-label="颜色 ${color}"></button>`,
                ).join("")}
              </div>
            </div>
            <div class="style-group">
              <span class="style-label">线宽</span>
              <div class="style-options" role="group" aria-label="线宽">
                ${STYLE_WIDTHS.map(
                  ({ value, label }) =>
                    `<button type="button" data-style-width="${value}" title="${label} (${value})">${label}</button>`,
                ).join("")}
              </div>
            </div>
            <div class="style-group">
              <span class="style-label">字号</span>
              <div class="style-options" role="group" aria-label="文字字号">
                ${STYLE_TEXT_SIZES.map(
                  ({ value, label }) =>
                    `<button type="button" data-style-text-size="${value}" title="${label} (${value})">${label}</button>`,
                ).join("")}
              </div>
            </div>
          </div>
        </div>
      </div>
      <div class="preview-actions">
        <button type="button" data-tool="ocr" title="取字 (O)">取字</button>
        <button type="button" data-action="copy-ocr-all" hidden>复制全部</button>
        <button type="button" data-action="save" title="保存 (Ctrl+S)">保存</button>
        <button type="button" class="primary" data-action="copy" title="复制 (Ctrl+C)">复制</button>
      </div>
    </div>
    <div class="preview-stage">
      <div class="preview-frame">
        <canvas></canvas>
        <textarea class="preview-text" rows="2" spellcheck="false" placeholder="在此输入汉字"></textarea>
      </div>
    </div>
    <div class="preview-context" data-context-menu hidden>
      <button type="button" data-action="delete-annotation">删除标注</button>
    </div>
  `;

  const canvas = root.querySelector("canvas");
  const note = root.querySelector(".preview-note");
  const editor = root.querySelector(".preview-text");
  const undoBtn = root.querySelector("[data-action=undo]");
  const frameEl = root.querySelector(".preview-frame");
  const copyAllBtn = root.querySelector("[data-action=copy-ocr-all]");
  const ocrBtn = root.querySelector("[data-tool=ocr]");
  const styleRoot = root.querySelector("[data-style-root]");
  const stylePanel = root.querySelector("[data-style-panel]");
  const styleBtn = root.querySelector("[data-action=style]");
  const contextMenu = root.querySelector("[data-context-menu]");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(note instanceof HTMLElement) ||
    !(editor instanceof HTMLTextAreaElement) ||
    !(undoBtn instanceof HTMLButtonElement) ||
    !(frameEl instanceof HTMLElement) ||
    !(copyAllBtn instanceof HTMLButtonElement) ||
    !(ocrBtn instanceof HTMLButtonElement) ||
    !(styleRoot instanceof HTMLElement) ||
    !(stylePanel instanceof HTMLElement) ||
    !(styleBtn instanceof HTMLButtonElement) ||
    !(contextMenu instanceof HTMLElement)
  ) {
    return;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return;
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
  const undoStack: EditAction[] = [];
  const redoStack: EditAction[] = [];
  let selected: number | null = null;
  let moving = false;
  let moveState: MoveState | null = null;
  let editTarget: number | null = null;
  let dragging = false;
  let start: Point | null = null;
  let current: Point | null = null;
  let editorOrigin: Point | null = null;
  let busy = false;
  let copied = "未标注图已复制";
  let ocrDoc: OcrDocument | null = null;
  let ocrSelected: number[] = [];
  let ocrDragging = false;
  let ocrStart: Point | null = null;
  let ocrCurrent: Point | null = null;
  let ocrGen = 0;
  let composing = false;
  // 功能入口开关(设置页 features.ocrEntry):关闭时取字按钮隐藏、O 键停用。
  let ocrEntryEnabled = true;
  let styleColor = FALLBACK_STROKE;
  let styleWidth: number | null = null;
  let styleTextBase: number | null = null;
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
  const annotationStyle = (op: Annotation): { color: string; lineWidth: number } => {
    if (op.type === "mosaic") {
      return { color: strokeColor, lineWidth: strokeFor(null) };
    }
    if (op.type === "text") {
      return { color: colorFor(op.color), lineWidth: strokeFor(null) };
    }
    return { color: colorFor(op.color), lineWidth: strokeFor(op.strokeWidth) };
  };

  const setNote = (text: string, kind: NoteKind = "feedback"): void => {
    note.textContent = text;
    note.classList.toggle("is-success", kind === "success");
    note.classList.toggle("is-feedback", kind === "feedback");
    note.classList.toggle("is-error", kind === "error");
  };

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
    ocrSelected = [];
    ocrCurrent = null;
    ocrStart = null;
    ocrDragging = false;
    copyAllBtn.hidden = next !== "ocr" || !ocrDoc || ocrDoc.spans.length === 0;
    if (next === "ocr") {
      if (!ocrDoc) {
        void runOcr();
      } else if (!note.classList.contains("is-error")) {
        setNote("点选或划选文字，也可复制全部。");
      }
    } else if (next === "text") {
      setNote("点在图上放置文字框，然后输入汉字。Enter 确认，Esc 取消。");
    } else if (!note.classList.contains("is-error")) {
      setNote(copied, "success");
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
  };

  const persistStyle = (): void => {
    void invoke<{ notice: string | null }>("set_annotation_defaults", {
      defaults: { color: styleColor, width: styleWidth, textSize: styleTextBase },
    })
      .then((result) => {
        if (result?.notice) {
          setNote(result.notice, "error");
        }
      })
      .catch(() => {
        setNote("标注样式本次可用，但未能记住。", "error");
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

  // 几何命中:从最上层(数组末尾)往下找,箭头按线段距离+箭头端容差,其余按包围盒。
  const hitAnnotation = (point: Point): number => {
    const tol = Math.max(6, Math.round((frame?.scale ?? 1) * 6));
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
    if (dragging && start && current && (tool === "arrow" || tool === "rect" || tool === "mosaic")) {
      const op = draft(tool, start, current, mosaicBlock(), styleColor, styleWidth);
      if (op) {
        const s = annotationStyle(op);
        paint(ctx, op, s.color, s.lineWidth);
      }
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
    setNote("正在识别…");
    redraw();
    try {
      const doc = await invoke<OcrDocument>("recognize_preview");
      if (token !== ocrGen) {
        return;
      }
      ocrDoc = doc;
      syncCopyAll();
      setNote("点选或划选文字，也可复制全部。");
      redraw();
    } catch (error) {
      if (token !== ocrGen) {
        return;
      }
      ocrDoc = null;
      syncCopyAll();
      setNote(invokeError(error, "无法识别图上的文字。"), "error");
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
      setNote(`已复制「${snippet}」`, "success");
    } catch (error) {
      setNote(invokeError(error, "没有选中文字。"), "error");
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
      setNote("已复制全部识别文本。", "success");
    } catch (error) {
      setNote(invokeError(error, "没有识别到文字。"), "error");
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
      copied = annotations.length > 0 ? "已复制当前标注图" : "未标注图已复制";
      setNote(copied, "success");
    } catch (error) {
      setNote(invokeError(error, "无法把截图放入剪贴板。预览仍保留。"), "error");
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
      const result = await invoke<{ saved: boolean }>("save_preview_png", {
        annotations: exportList(),
      });
      if (result.saved) {
        setNote("已保存 PNG。", "success");
      }
    } catch (error) {
      setNote(invokeError(error, "无法保存 PNG。预览仍保留，可继续标注或复制。"), "error");
    } finally {
      busy = false;
    }
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
    dragging = true;
    start = point;
    current = point;
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
    if (!dragging || !start) {
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
    if (!dragging || !start || !current) {
      dragging = false;
      return;
    }
    dragging = false;
    if (tool === "arrow" || tool === "rect" || tool === "mosaic") {
      const op = draft(tool, start, current, mosaicBlock(), styleColor, styleWidth);
      if (op) {
        pushAction({ kind: "add", index: annotations.length, op });
      }
    }
    start = null;
    current = null;
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
    if (nextTool === "arrow" || nextTool === "rect" || nextTool === "mosaic" || nextTool === "text" || nextTool === "ocr") {
      if (nextTool === "ocr" && !ocrEntryEnabled) {
        return;
      }
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
    } else if (button.dataset.action === "copy") {
      void copy();
    } else if (button.dataset.action === "copy-ocr-all") {
      void copyOcrAll();
    } else if (button.dataset.action === "save") {
      void save();
    } else if (button.dataset.action === "close") {
      void invoke("close_preview");
    }
  });

  const titlebar = root.querySelector(".preview-titlebar");
  if (titlebar instanceof HTMLElement) {
    titlebar.addEventListener("mousedown", (event) => {
      if (event.button !== 0) {
        return;
      }
      if (event.target instanceof Element && event.target.closest("button")) {
        return;
      }
      event.preventDefault();
      void getCurrentWindow().startDragging();
    });
  }

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
    if (event.altKey || event.shiftKey || document.activeElement === editor || editorOpen()) {
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
      m: "mosaic",
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

  // 跨会话记忆：加载时读后端保存的上次样式（读写失败均静默回退当前值）；
  // 同时读取功能入口开关，关闭取字后隐藏预览工具条按钮并停用 O 键。
  const loadStyleDefaults = (): void => {
    void invoke<{
      annotationDefaults?: { color?: string; width?: number | null; textSize?: number | null };
      features?: { ocrEntry?: boolean };
    }>("get_ui_settings")
      .then((settings) => {
        ocrEntryEnabled = settings?.features?.ocrEntry !== false;
        ocrBtn.hidden = !ocrEntryEnabled;
        if (!ocrEntryEnabled && tool === "ocr") {
          setTool("arrow");
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
        syncStylePanel();
        redraw();
      })
      .catch(() => undefined);
  };
  loadStyleDefaults();

  let previewLoad = 0;
  const loadPreview = (): void => {
    const generation = ++previewLoad;
    void invoke<ArrayBuffer>("get_preview_frame")
      .then((bytes) => {
        if (generation !== previewLoad) return;
        if (bytes.byteLength <= 20) throw new Error("预览图像数据不完整。");
        const header = new DataView(bytes);
        const clipboardWritten = header.getUint32(16, true) === 1;
        const payload: PreviewFrame = {
          width: header.getUint32(0, true),
          height: header.getUint32(4, true),
          scale: header.getFloat64(8, true),
        };
        frame = payload;
        canvas.width = payload.width;
        canvas.height = payload.height;
        const image = new Image();
        const imageUrl = URL.createObjectURL(new Blob([bytes.slice(20)], { type: "image/png" }));
        image.onload = () => {
          URL.revokeObjectURL(imageUrl);
          if (generation !== previewLoad) return;
          source = document.createElement("canvas");
          source.width = payload.width;
          source.height = payload.height;
          const sourceCtx = source.getContext("2d");
          if (!sourceCtx) {
            setNote("无法显示预览图像。", "error");
            return;
          }
          sourceCtx.drawImage(image, 0, 0, payload.width, payload.height);
          copied = clipboardWritten ? "未标注图已复制" : "自动复制失败，可点击复制重试。";
          setNote(copied, clipboardWritten ? "success" : "error");
          redraw();
        };
        image.onerror = () => {
          URL.revokeObjectURL(imageUrl);
          if (generation === previewLoad) setNote("无法显示预览图像。", "error");
        };
        image.src = imageUrl;
      })
      .catch((error) => {
        if (generation !== previewLoad) return;
        setNote(invokeError(error, "没有可预览的截图。"), "error");
      });
  };

  void listen("preview-reload", () => {
    annotations = [];
    undoStack.length = 0;
    redoStack.length = 0;
    selected = null;
    moving = false;
    moveState = null;
    // 新帧不带旧编辑态:收掉文字编辑器,清掉 editorOrigin,防旧文本误入新帧。
    hideEditor();
    syncUndo();
    hideContextMenu();
    ocrDoc = null;
    ocrSelected = [];
    loadPreview();
  });
  loadPreview();
}

function draft(
  tool: "arrow" | "rect" | "mosaic",
  start: Point,
  end: Point,
  block: number,
  color: string,
  strokeWidth: number | null,
): Annotation | null {
  if (tool === "arrow") {
    if (Math.hypot(end.x - start.x, end.y - start.y) < 3) {
      return null;
    }
    return { type: "arrow", from: start, to: end, color, strokeWidth };
  }
  const x = Math.min(start.x, end.x);
  const y = Math.min(start.y, end.y);
  const width = Math.abs(end.x - start.x);
  const height = Math.abs(end.y - start.y);
  if (width < 3 || height < 3) {
    return null;
  }
  if (tool === "mosaic") {
    return { type: "mosaic", x, y, width, height, block };
  }
  return { type: "rect", x, y, width, height, color, strokeWidth };
}

function paint(ctx: CanvasRenderingContext2D, op: Annotation, stroke: string, lineWidth: number): void {
  if (op.type === "mosaic") {
    paintMosaic(ctx, op);
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
  } else if (op.type === "arrow") {
    paintArrow(ctx, op.from, op.to, lineWidth);
  } else if (op.type === "text") {
    ctx.font = `${op.size}px ${TEXT_FONT_STACK}`;
    ctx.textBaseline = "top";
    const lines = op.text.split("\n");
    lines.forEach((line, index) => {
      ctx.fillText(line, op.x, op.y + index * op.size * 1.25);
    });
  }
  ctx.restore();
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

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
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
  if (op.type === "arrow") {
    return {
      minX: Math.min(op.from.x, op.to.x),
      minY: Math.min(op.from.y, op.to.y),
      maxX: Math.max(op.from.x, op.to.x),
      maxY: Math.max(op.from.y, op.to.y),
    };
  }
  if (op.type === "text") {
    ctx.save();
    ctx.font = `${op.size}px ${TEXT_FONT_STACK}`;
    let width = 0;
    for (const line of op.text.split("\n")) {
      width = Math.max(width, ctx.measureText(line).width);
    }
    ctx.restore();
    const lineCount = op.text.split("\n").length;
    return { minX: op.x, minY: op.y, maxX: op.x + width, maxY: op.y + lineCount * op.size * 1.25 };
  }
  return { minX: op.x, minY: op.y, maxX: op.x + op.width, maxY: op.y + op.height };
}

function translateOp(op: Annotation, dx: number, dy: number): Annotation {
  if (op.type === "arrow") {
    return {
      ...op,
      from: { x: op.from.x + dx, y: op.from.y + dy },
      to: { x: op.to.x + dx, y: op.to.y + dy },
    };
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
