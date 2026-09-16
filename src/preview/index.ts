import { invoke } from "@tauri-apps/api/core";
import "../overlay/overlay.css";
import "./preview.css";

interface PreviewFrame {
  pngBase64: string;
  width: number;
  height: number;
  scale: number;
}

type Tool = "arrow" | "rect" | "mosaic" | "text" | "ocr";

type Point = { x: number; y: number };

type Annotation =
  | { type: "arrow"; from: Point; to: Point }
  | { type: "rect"; x: number; y: number; width: number; height: number }
  | { type: "mosaic"; x: number; y: number; width: number; height: number; block: number }
  | { type: "text"; x: number; y: number; text: string; size: number };

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

const STROKE = "#E11D48";

const ICONS: Record<Exclude<Tool, "ocr"> | "undo", string> = {
  arrow: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 12.5 12.5 3.5M7 3.5h5.5V9" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  rect: `<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="3" y="4" width="10" height="8" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.7"/></svg>`,
  mosaic: `<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="2" width="5" height="5" fill="currentColor"/><rect x="9" y="2" width="5" height="5" fill="currentColor" opacity="0.45"/><rect x="2" y="9" width="5" height="5" fill="currentColor" opacity="0.65"/><rect x="9" y="9" width="5" height="5" fill="currentColor" opacity="0.28"/></svg>`,
  text: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4.2h8M8 4.2v8.2M5.5 12.4h5" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>`,
  undo: `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 7h6.2a3 3 0 1 1 0 6H9" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/><path d="M4 7 6.4 4.6M4 7l2.4 2.4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>`,
};

export function mountPreview(root: HTMLElement): void {
  root.className = "preview-root";
  root.dataset.tool = "arrow";
  root.innerHTML = `
    <header data-tauri-drag-region>
      <div class="brand" data-tauri-drag-region>
        <span class="mark" aria-hidden="true"></span>
        <span class="name">Cropmark</span>
      </div>
      <p class="preview-note">未标注图已复制</p>
      <button type="button" class="icon-btn" data-action="close" aria-label="关闭">×</button>
    </header>
    <div class="preview-toolbar">
      <div class="preview-tools" role="toolbar" aria-label="标注">
        <button type="button" data-tool="arrow" title="箭头" aria-label="箭头">${ICONS.arrow}</button>
        <button type="button" data-tool="rect" title="框" aria-label="框">${ICONS.rect}</button>
        <button type="button" data-tool="mosaic" title="马赛克" aria-label="马赛克">${ICONS.mosaic}</button>
        <button type="button" data-tool="text" title="文字框" aria-label="文字框">${ICONS.text}</button>
        <button type="button" data-action="undo" title="撤销" aria-label="撤销">${ICONS.undo}</button>
      </div>
      <div class="preview-actions">
        <button type="button" data-tool="ocr" title="取字">取字</button>
        <button type="button" data-action="copy-ocr-all" hidden>复制全部</button>
        <button type="button" data-action="save">保存</button>
        <button type="button" class="primary" data-action="copy">复制</button>
      </div>
    </div>
    <div class="preview-stage">
      <div class="preview-frame">
        <canvas></canvas>
        <textarea class="preview-text" hidden rows="1" spellcheck="false"></textarea>
      </div>
    </div>
  `;

  const canvas = root.querySelector("canvas");
  const note = root.querySelector(".preview-note");
  const editor = root.querySelector(".preview-text");
  const undoBtn = root.querySelector("[data-action=undo]");
  const frameEl = root.querySelector(".preview-frame");
  const copyAllBtn = root.querySelector("[data-action=copy-ocr-all]");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(note instanceof HTMLElement) ||
    !(editor instanceof HTMLTextAreaElement) ||
    !(undoBtn instanceof HTMLButtonElement) ||
    !(frameEl instanceof HTMLElement) ||
    !(copyAllBtn instanceof HTMLButtonElement)
  ) {
    return;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return;
  }

  let frame: PreviewFrame | null = null;
  let source: HTMLCanvasElement | null = null;
  let tool: Tool = "arrow";
  let annotations: Annotation[] = [];
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

  const mosaicBlock = (): number => Math.max(8, Math.round(12 * Math.max(frame?.scale ?? 1, 1)));
  const textSize = (): number => Math.max(16, Math.round(14 * Math.max(frame?.scale ?? 1, 1)));
  const strokeWidth = (): number => Math.min(8, Math.max(2, 3 * Math.max(frame?.scale ?? 1, 1)));

  const setNote = (text: string, isError = false): void => {
    note.textContent = text;
    note.classList.toggle("is-error", isError);
  };

  const setTool = (next: Tool): void => {
    commitEditor();
    tool = next;
    root.dataset.tool = next;
    root.querySelectorAll("[data-tool]").forEach((button) => {
      button.classList.toggle("active", button.getAttribute("data-tool") === next);
    });
    ocrSelected = [];
    ocrCurrent = null;
    ocrStart = null;
    ocrDragging = false;
    copyAllBtn.hidden = next !== "ocr" || !ocrDoc || ocrDoc.spans.length === 0;
    if (next === "ocr") {
      if (!ocrDoc) {
        void runOcr();
      } else if (!note.classList.contains("is-error")) {
        setNote("点选或划选文字，也可复制全部。", false);
      }
    } else if (!note.classList.contains("is-error")) {
      setNote(copied, false);
    }
    redraw();
  };

  const syncUndo = (): void => {
    undoBtn.disabled = annotations.length === 0 && editor.hidden;
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

  const cssScale = (): { x: number; y: number } => {
    const rect = canvas.getBoundingClientRect();
    return {
      x: rect.width / Math.max(canvas.width, 1),
      y: rect.height / Math.max(canvas.height, 1),
    };
  };

  const redraw = (): void => {
    if (!source) {
      return;
    }
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(source, 0, 0);
    for (const op of annotations) {
      paint(ctx, op, strokeWidth());
    }
    if (dragging && start && current && (tool === "arrow" || tool === "rect" || tool === "mosaic")) {
      const op = draft(tool, start, current, mosaicBlock());
      if (op) {
        paint(ctx, op, strokeWidth());
      }
    }
    if (tool === "ocr" && ocrDoc) {
      const rubber =
        ocrDragging && ocrStart && ocrCurrent ? normalizeRect(ocrStart, ocrCurrent) : null;
      paintOcr(ctx, ocrDoc.spans, ocrSelected, rubber);
    }
  };

  const placeEditor = (point: Point): void => {
    commitEditor();
    editorOrigin = point;
    const scale = cssScale();
    const canvasRect = canvas.getBoundingClientRect();
    const frameRect = frameEl.getBoundingClientRect();
    editor.hidden = false;
    editor.value = "";
    editor.style.left = `${canvasRect.left - frameRect.left + point.x * scale.x}px`;
    editor.style.top = `${canvasRect.top - frameRect.top + point.y * scale.y}px`;
    editor.style.fontSize = `${textSize() * scale.y}px`;
    editor.style.width = `${Math.max(96, 160 * scale.x)}px`;
    editor.focus();
    syncUndo();
  };

  const hideEditor = (): void => {
    editor.hidden = true;
    editor.value = "";
    editorOrigin = null;
  };

  const commitEditor = (): void => {
    if (editor.hidden || !editorOrigin) {
      return;
    }
    const text = editor.value;
    const origin = editorOrigin;
    hideEditor();
    if (text.trim().length > 0) {
      annotations.push({ type: "text", x: origin.x, y: origin.y, text, size: textSize() });
      redraw();
    }
    syncUndo();
  };

  const cancelEditor = (): void => {
    hideEditor();
    syncUndo();
    redraw();
  };

  const undo = (): void => {
    if (!editor.hidden) {
      cancelEditor();
      return;
    }
    annotations.pop();
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
    setNote("正在识别…", false);
    redraw();
    try {
      const doc = await invoke<OcrDocument>("recognize_preview");
      if (token !== ocrGen) {
        return;
      }
      ocrDoc = doc;
      syncCopyAll();
      setNote("点选或划选文字，也可复制全部。", false);
      redraw();
    } catch (error) {
      if (token !== ocrGen) {
        return;
      }
      ocrDoc = null;
      syncCopyAll();
      setNote(invokeError(error, "无法识别图上的文字。"), true);
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
      setNote(`已复制「${snippet}」`, false);
    } catch (error) {
      setNote(invokeError(error, "没有选中文字。"), true);
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
      setNote("已复制全部识别文本。", false);
    } catch (error) {
      setNote(invokeError(error, "没有识别到文字。"), true);
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
      setNote(copied, false);
    } catch (error) {
      setNote(invokeError(error, "无法把截图放入剪贴板。预览仍保留。"), true);
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
        setNote("已保存 PNG。", false);
      }
    } catch (error) {
      setNote(invokeError(error, "无法保存 PNG。预览仍保留，可继续标注或复制。"), true);
    } finally {
      busy = false;
    }
  };

  canvas.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || !frame) {
      return;
    }
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
    dragging = true;
    start = point;
    current = point;
  });

  window.addEventListener("mousemove", (event) => {
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
      const op = draft(tool, start, current, mosaicBlock());
      if (op) {
        annotations.push(op);
      }
    }
    start = null;
    current = null;
    redraw();
    syncUndo();
  });

  let skipUndoClick = false;
  undoBtn.addEventListener("pointerdown", (event) => {
    if (!editor.hidden) {
      event.preventDefault();
      cancelEditor();
      skipUndoClick = true;
    }
  });
  editor.addEventListener("mousedown", (event) => event.stopPropagation());
  editor.addEventListener("keydown", (event) => {
    if (event.isComposing || event.key === "Process" || event.keyCode === 229) {
      return;
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      commitEditor();
    } else if (event.key === "Escape") {
      event.preventDefault();
      cancelEditor();
    }
  });
  editor.addEventListener("blur", () => {
    commitEditor();
  });

  root.addEventListener("click", (event) => {
    const button = event.target instanceof Element ? event.target.closest("button") : null;
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    const nextTool = button.dataset.tool;
    if (nextTool === "arrow" || nextTool === "rect" || nextTool === "mosaic" || nextTool === "text" || nextTool === "ocr") {
      setTool(nextTool);
      return;
    }
    if (button.dataset.action === "undo") {
      if (skipUndoClick) {
        skipUndoClick = false;
        return;
      }
      undo();
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

  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      if (!editor.hidden) {
        event.preventDefault();
        cancelEditor();
        return;
      }
      void invoke("close_preview");
      return;
    }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "z" && editor.hidden) {
      event.preventDefault();
      undo();
    }
  });

  setTool("arrow");
  syncUndo();

  void invoke<PreviewFrame>("get_preview_frame")
    .then((payload) => {
      frame = payload;
      canvas.width = payload.width;
      canvas.height = payload.height;
      const image = new Image();
      image.onload = () => {
        source = document.createElement("canvas");
        source.width = payload.width;
        source.height = payload.height;
        const sourceCtx = source.getContext("2d");
        if (!sourceCtx) {
          setNote("无法显示预览图像。", true);
          return;
        }
        sourceCtx.drawImage(image, 0, 0);
        redraw();
      };
      image.src = `data:image/png;base64,${payload.pngBase64}`;
    })
    .catch((error) => {
      setNote(invokeError(error, "没有可预览的截图。"), true);
    });
}

function draft(tool: "arrow" | "rect" | "mosaic", start: Point, end: Point, block: number): Annotation | null {
  if (tool === "arrow") {
    if (Math.hypot(end.x - start.x, end.y - start.y) < 3) {
      return null;
    }
    return { type: "arrow", from: start, to: end };
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
  return { type: "rect", x, y, width, height };
}

function paint(ctx: CanvasRenderingContext2D, op: Annotation, lineWidth: number): void {
  if (op.type === "mosaic") {
    paintMosaic(ctx, op);
    return;
  }
  ctx.save();
  ctx.strokeStyle = STROKE;
  ctx.fillStyle = STROKE;
  ctx.lineWidth = lineWidth;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  if (op.type === "rect") {
    ctx.strokeRect(op.x + 0.5, op.y + 0.5, op.width, op.height);
  } else if (op.type === "arrow") {
    paintArrow(ctx, op.from, op.to, lineWidth);
  } else if (op.type === "text") {
    ctx.font = `${op.size}px "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif`;
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

function paintOcr(
  ctx: CanvasRenderingContext2D,
  spans: TextSpan[],
  selected: number[],
  rubber: { x: number; y: number; width: number; height: number } | null,
): void {
  ctx.save();
  const selectedSet = new Set(selected);
  for (let i = 0; i < spans.length; i += 1) {
    const span = spans[i];
    const on = selectedSet.has(i);
    ctx.fillStyle = on ? "rgba(14, 165, 233, 0.38)" : "rgba(14, 165, 233, 0.12)";
    ctx.strokeStyle = on ? "rgba(3, 105, 161, 0.95)" : "rgba(14, 165, 233, 0.55)";
    ctx.lineWidth = Math.max(1, ctx.canvas.width / 900);
    ctx.beginPath();
    ctx.rect(span.x, span.y, span.width, span.height);
    ctx.fill();
    ctx.stroke();
  }
  if (rubber && (rubber.width > 3 || rubber.height > 3)) {
    ctx.fillStyle = "rgba(14, 165, 233, 0.08)";
    ctx.strokeStyle = "rgba(3, 105, 161, 0.9)";
    ctx.setLineDash([6, 4]);
    ctx.fillRect(rubber.x, rubber.y, rubber.width, rubber.height);
    ctx.strokeRect(rubber.x, rubber.y, rubber.width, rubber.height);
  }
  ctx.restore();
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
