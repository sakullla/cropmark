import { invoke } from "@tauri-apps/api/core";
import { resolveCanvasColor } from "../annotation";
import { t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import "./ocr.css";

// R2:取字模型(图上文本层 + 结果面板 + 显式复制)。预览与冻结帧工作区覆盖层
// 共用同一实现:任何入口在用户触发复制前都不写剪贴板;识别为空/失败只给
// 本地化说明,不显示可用的复制动作。

export interface OcrTextSpan {
  text: string;
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface OcrDocument {
  spans: OcrTextSpan[];
  fullText: string;
}

export interface OcrPanelMatch {
  start: number;
  end: number;
  fragment: string;
}

export interface OcrPoint {
  x: number;
  y: number;
}

export interface OcrRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export type OcrNoticeKind = "progress" | "hint" | "success" | "error";

export interface OcrModelOptions {
  /** 结果面板挂载容器(预览舞台 / 覆盖层根)。 */
  host: HTMLElement;
  /** 宿主提示通道:progress/hint 常驻到被替换,success/error 为结果反馈。 */
  notice: (message: string, kind: OcrNoticeKind) => void;
  /** 激活态、文档或选择变化时通知宿主(同步 chrome 并重绘)。 */
  onChange?: () => void;
  /** 面板是否提供关闭按钮(覆盖层用 Esc 退出,不提供单独关闭)。 */
  closable?: boolean;
}

export interface OcrModel {
  readonly active: boolean;
  document: () => OcrDocument | null;
  isRunning: () => boolean;
  activate: () => void;
  deactivate: () => boolean;
  reset: () => void;
  paint: (ctx: CanvasRenderingContext2D) => void;
  pointerDown: (point: OcrPoint) => boolean;
  pointerMove: (point: OcrPoint) => boolean;
  pointerUp: () => boolean;
  selectAll: () => void;
  copySelected: () => Promise<void>;
  copyAll: () => Promise<void>;
  closePanel: () => void;
  refreshLabels: () => void;
}

const FALLBACK_OCR_HL = "#0ea5e9";
const FALLBACK_OCR_HL_STRONG = "#0369a1";
const DRAG_THRESHOLD = 4;
const MIN_RUBBER = 3;

type SelectionSource =
  | { kind: "point"; point: OcrPoint }
  | { kind: "rect"; rect: OcrRect }
  | { kind: "fragment"; text: string }
  | { kind: "all" };

interface CharRange {
  start: number;
  end: number;
}

interface OcrColors {
  hl: string;
  hlStrong: string;
}

function normalizeRect(a: OcrPoint, b: OcrPoint): OcrRect {
  const x = Math.min(a.x, b.x);
  const y = Math.min(a.y, b.y);
  return { x, y, width: Math.abs(a.x - b.x), height: Math.abs(a.y - b.y) };
}

function withAlpha(color: string, alpha: number): string {
  let value = color.trim();
  if (!/^rgba?\(/.test(value) && !/^#[0-9a-f]{3,8}$/i.test(value)) {
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
  return `rgba(14, 165, 233, ${alpha})`;
}

function snippetOf(text: string): string {
  return text.length > 24 ? `${text.slice(0, 24)}…` : text;
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

/** 点在 span 上的最小非空白命中;与 Rust `hit_point` 同语义。 */
function indexAtPoint(spans: OcrTextSpan[], x: number, y: number): number {
  let best = -1;
  let area = Number.POSITIVE_INFINITY;
  for (let i = 0; i < spans.length; i += 1) {
    const span = spans[i];
    if (!span.text.trim()) {
      continue;
    }
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

/** 与 Rust `hit_rect` 同语义:相交的识别段按原顺序返回。 */
function indicesInRect(spans: OcrTextSpan[], rect: OcrRect): number[] {
  const x1 = rect.x + rect.width;
  const y1 = rect.y + rect.height;
  return spans
    .map((span, index) => ({ span, index }))
    .filter(
      ({ span }) =>
        span.text.trim().length > 0 &&
        span.x < x1 &&
        span.x + span.width > rect.x &&
        span.y < y1 &&
        span.y + span.height > rect.y,
    )
    .map(({ index }) => index);
}

function spanCenter(span: OcrTextSpan): OcrPoint {
  return { x: span.x + span.width / 2, y: span.y + span.height / 2 };
}

function sameIndices(a: number[], b: number[]): boolean {
  return a.length === b.length && a.every((value, index) => value === b[index]);
}

/**
 * 把识别段文本映射回面板全文的字符区间(Unicode 码点,与 Rust 面板搜索的
 * 字符下标一致)。引擎的 `fullText` 与 spans 同源但可能插入行内空格,这里
 * 按阅读顺序顺序查找,允许跳过插入的空白。
 */
function findCharSequence(chars: string[], needle: string, from: number): number {
  const target = Array.from(needle);
  if (target.length === 0) {
    return -1;
  }
  for (let start = Math.max(0, from); start + target.length <= chars.length; start += 1) {
    let match = true;
    for (let offset = 0; offset < target.length; offset += 1) {
      if (chars[start + offset] !== target[offset]) {
        match = false;
        break;
      }
    }
    if (match) {
      return start;
    }
  }
  return -1;
}

function mapSpanOffsets(doc: OcrDocument): Array<CharRange | null> {
  const chars = Array.from(doc.fullText ?? "");
  const offsets: Array<CharRange | null> = doc.spans.map(() => null);
  let cursor = 0;
  for (let i = 0; i < doc.spans.length; i += 1) {
    const needle = doc.spans[i].text.trim();
    if (!needle) {
      continue;
    }
    const length = Array.from(needle).length;
    let at = findCharSequence(chars, needle, cursor);
    if (at < 0) {
      at = findCharSequence(chars, needle, 0);
    }
    if (at < 0) {
      continue;
    }
    offsets[i] = { start: at, end: at + length };
    cursor = at + length;
  }
  return offsets;
}

export function mountOcrModel(options: OcrModelOptions): OcrModel {
  const { host, notice } = options;
  const panel = document.createElement("aside");
  panel.className = "ocr-panel";
  panel.hidden = true;
  panel.dataset.ocrPanel = "";
  panel.setAttribute("role", "complementary");
  panel.setAttribute("data-i18n-aria-label", "preview.ocr_panel.text");
  panel.setAttribute("aria-label", t("preview.ocr_panel.text"));
  panel.innerHTML = `
    <div class="ocr-panel-bar">
      <h2 data-i18n="preview.ocr_panel.title">${t("preview.ocr_panel.title")}</h2>
      <button type="button" class="icon-btn" data-ocr-action="close-panel" data-i18n-aria-label="preview.ocr_panel.close" aria-label="${t("preview.ocr_panel.close")}">${icons.close}</button>
    </div>
    <div class="ocr-panel-search">
      <input type="search" class="ocr-panel-query" data-ocr-search data-i18n-placeholder="preview.ocr_panel.search" data-i18n-aria-label="preview.ocr_panel.search" placeholder="${t("preview.ocr_panel.search")}" aria-label="${t("preview.ocr_panel.search")}" autocomplete="off" spellcheck="false" />
      <div class="ocr-panel-nav">
        <span class="ocr-panel-count" data-ocr-search-count aria-live="polite"></span>
        <button type="button" data-ocr-action="prev" data-i18n="preview.ocr_panel.prev" disabled>${t("preview.ocr_panel.prev")}</button>
        <button type="button" data-ocr-action="next" data-i18n="preview.ocr_panel.next" disabled>${t("preview.ocr_panel.next")}</button>
      </div>
    </div>
    <div class="ocr-panel-text" data-ocr-text tabindex="0"></div>
    <div class="ocr-panel-actions" data-ocr-actions hidden>
      <button type="button" data-ocr-action="copy-selected" data-i18n="preview.ocr_panel.copy_selected">${t("preview.ocr_panel.copy_selected")}</button>
      <button type="button" data-ocr-action="copy-all" data-i18n="preview.ocr_panel.copy_all">${t("preview.ocr_panel.copy_all")}</button>
    </div>
  `;
  host.append(panel);

  const searchInput = panel.querySelector("[data-ocr-search]") as HTMLInputElement;
  const searchCount = panel.querySelector("[data-ocr-search-count]") as HTMLElement;
  const panelText = panel.querySelector("[data-ocr-text]") as HTMLElement;
  const prevBtn = panel.querySelector("[data-ocr-action=prev]") as HTMLButtonElement;
  const nextBtn = panel.querySelector("[data-ocr-action=next]") as HTMLButtonElement;
  const actionsRow = panel.querySelector("[data-ocr-actions]") as HTMLElement;
  const copySelectedBtn = panel.querySelector("[data-ocr-action=copy-selected]") as HTMLButtonElement;
  const copyAllBtn = panel.querySelector("[data-ocr-action=copy-all]") as HTMLButtonElement;
  const closeBtn = panel.querySelector("[data-ocr-action=close-panel]") as HTMLButtonElement;
  if (options.closable === false) {
    closeBtn.hidden = true;
  }

  const rootStyle = getComputedStyle(host);
  const colors: OcrColors = {
    hl: resolveCanvasColor(rootStyle.getPropertyValue("--ocr-hl"), FALLBACK_OCR_HL),
    hlStrong: resolveCanvasColor(
      rootStyle.getPropertyValue("--ocr-hl-strong"),
      FALLBACK_OCR_HL_STRONG,
    ),
  };

  let active = false;
  let doc: OcrDocument | null = null;
  let running = false;
  let generation = 0;
  let searchGen = 0;
  let selected: number[] = [];
  let selectionSource: SelectionSource | null = null;
  let spanOffsets: Array<CharRange | null> = [];
  let matches: OcrPanelMatch[] = [];
  let matchIndex = 0;
  let panelDismissed = true;
  let dragging = false;
  let dragStart: OcrPoint | null = null;
  let dragCurrent: OcrPoint | null = null;
  let busy = false;
  let lastNotice: {
    key: CatalogKey | null;
    params?: Record<string, string | number>;
    kind: OcrNoticeKind;
  } | null = null;

  const emitChange = (): void => {
    options.onChange?.();
  };

  const setNoticeKey = (
    key: CatalogKey,
    kind: OcrNoticeKind,
    params?: Record<string, string | number>,
  ): void => {
    lastNotice = { key, kind, params };
    notice(t(key, params), kind);
  };

  const setNoticeText = (text: string, kind: OcrNoticeKind): void => {
    lastNotice = { key: null, kind };
    notice(text, kind);
  };

  const clearDrag = (): void => {
    dragging = false;
    dragStart = null;
    dragCurrent = null;
  };

  const clearSelection = (): void => {
    selected = [];
    selectionSource = null;
  };

  const clearPanelView = (): void => {
    searchGen += 1;
    searchInput.value = "";
    matches = [];
    matchIndex = 0;
    panelText.replaceChildren();
    searchCount.textContent = "";
    prevBtn.disabled = true;
    nextBtn.disabled = true;
    panel.hidden = true;
  };

  const hasDocument = (): boolean => doc !== null && doc.spans.length > 0;

  const panelSelectionText = (): string | null => {
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || selection.rangeCount === 0) {
      return null;
    }
    const node = selection.anchorNode;
    if (!node || !panelText.contains(node)) {
      return null;
    }
    const text = selection.toString().replace(/\r\n/g, "\n");
    return text.trim() ? text : null;
  };

  const hasCopySource = (): boolean =>
    selected.length > 0 || panelSelectionText() !== null || matches[matchIndex] !== undefined;

  const syncActions = (): void => {
    const visible = active && !panelDismissed && hasDocument();
    actionsRow.hidden = !visible;
    if (visible) {
      copySelectedBtn.disabled = !hasCopySource() || busy;
      copyAllBtn.disabled = busy;
    }
  };

  const charStates = (): Uint8Array => {
    const text = doc?.fullText ?? "";
    const length = Array.from(text).length;
    const states = new Uint8Array(length);
    const paintRange = (range: CharRange, state: number): void => {
      for (let i = Math.max(0, range.start); i < Math.min(length, range.end); i += 1) {
        states[i] = state;
      }
    };
    for (const match of matches) {
      paintRange({ start: match.start, end: match.end }, 1);
    }
    const current = matches[matchIndex];
    if (current) {
      paintRange({ start: current.start, end: current.end }, 2);
    }
    for (const index of selected) {
      const range = spanOffsets[index];
      if (range) {
        paintRange(range, 3);
      }
    }
    return states;
  };

  const paintPanel = (): void => {
    const text = doc?.fullText ?? "";
    const chars = Array.from(text);
    const states = charStates();
    panelText.replaceChildren();
    let index = 0;
    while (index < chars.length) {
      const state = states[index];
      let end = index + 1;
      while (end < chars.length && states[end] === state) {
        end += 1;
      }
      const chunk = chars.slice(index, end).join("");
      if (state === 0) {
        panelText.append(document.createTextNode(chunk));
      } else {
        const mark = document.createElement("mark");
        mark.className = state === 3 ? "is-selected" : state === 2 ? "is-current" : "is-search";
        mark.textContent = chunk;
        panelText.append(mark);
      }
      index = end;
    }
    const first = panelText.querySelector("mark.is-selected") ?? panelText.querySelector("mark.is-current");
    first?.scrollIntoView({ block: "nearest" });
  };

  const syncSearchStatus = (): void => {
    const query = searchInput.value.trim();
    if (!query) {
      searchCount.textContent = "";
    } else if (matches.length === 0) {
      searchCount.textContent = t("preview.ocr_panel.no_match");
    } else {
      searchCount.textContent = t("preview.ocr_panel.match_count", {
        current: matchIndex + 1,
        total: matches.length,
      });
    }
    const canStep = matches.length > 0;
    prevBtn.disabled = !canStep;
    nextBtn.disabled = !canStep;
  };

  const panelHasDomSelection = (): boolean => {
    const selection = window.getSelection();
    return (
      selection !== null &&
      !selection.isCollapsed &&
      selection.anchorNode !== null &&
      panelText.contains(selection.anchorNode)
    );
  };

  const renderPanel = (): void => {
    const visible = active && !panelDismissed && hasDocument();
    if (!visible) {
      panel.hidden = true;
      syncActions();
      return;
    }
    panel.hidden = false;
    if (!panelHasDomSelection()) {
      paintPanel();
    }
    syncActions();
  };

  const closePanel = (): void => {
    panelDismissed = true;
    clearPanelView();
    window.getSelection()?.removeAllRanges();
    syncActions();
  };

  const applySearch = async (): Promise<void> => {
    const token = ++searchGen;
    const text = doc?.fullText ?? "";
    const query = searchInput.value;
    if (panelDismissed || !text.trim() || !query.trim()) {
      matches = [];
      matchIndex = 0;
      paintPanel();
      syncSearchStatus();
      syncActions();
      return;
    }
    try {
      const found = await invoke<OcrPanelMatch[]>("search_ocr_panel", { query });
      if (token !== searchGen || doc?.fullText !== text) {
        return;
      }
      matches = found;
      matchIndex = 0;
      paintPanel();
      syncSearchStatus();
      syncActions();
    } catch (error) {
      if (token !== searchGen) {
        return;
      }
      setNoticeText(invokeError(error, t("preview.error.ocr_fallback")), "error");
    }
  };

  const stepMatch = (delta: number): void => {
    if (matches.length === 0) {
      return;
    }
    matchIndex = (matchIndex + delta + matches.length) % matches.length;
    paintPanel();
    syncSearchStatus();
    emitChange();
  };

  const runRecognition = async (): Promise<void> => {
    const token = ++generation;
    running = true;
    doc = null;
    spanOffsets = [];
    panelDismissed = true;
    clearSelection();
    clearDrag();
    clearPanelView();
    setNoticeKey("preview.note.ocr_running", "progress");
    emitChange();
    try {
      const result = await invoke<OcrDocument>("recognize_preview");
      if (token !== generation) {
        return;
      }
      if (!result.fullText.trim()) {
        doc = null;
        spanOffsets = [];
        if (active) {
          setNoticeKey("preview.error.no_text", "error");
        }
      } else {
        doc = result;
        spanOffsets = mapSpanOffsets(result);
        panelDismissed = false;
        if (active) {
          renderPanel();
          setNoticeKey("preview.note.ocr_hint", "hint");
        }
      }
    } catch (error) {
      if (token !== generation) {
        return;
      }
      doc = null;
      spanOffsets = [];
      panelDismissed = true;
      clearPanelView();
      if (active) {
        setNoticeText(invokeError(error, t("preview.error.ocr_fallback")), "error");
      }
    } finally {
      if (token === generation) {
        running = false;
        renderPanel();
        emitChange();
      }
    }
  };

  const activate = (): void => {
    if (active) {
      return;
    }
    active = true;
    clearSelection();
    clearDrag();
    if (!doc || panelDismissed) {
      void runRecognition();
    } else {
      renderPanel();
      setNoticeKey("preview.note.ocr_hint", "hint");
    }
    emitChange();
  };

  const deactivate = (): boolean => {
    if (!active) {
      return false;
    }
    active = false;
    clearSelection();
    clearDrag();
    panel.hidden = true;
    syncActions();
    if (panelHasDomSelection()) {
      window.getSelection()?.removeAllRanges();
    }
    emitChange();
    return true;
  };

  const reset = (): void => {
    generation += 1;
    running = false;
    active = false;
    doc = null;
    spanOffsets = [];
    panelDismissed = true;
    lastNotice = null;
    clearSelection();
    clearDrag();
    clearPanelView();
    syncActions();
    emitChange();
  };

  const selectAll = (): void => {
    if (!active || !doc || doc.spans.length === 0) {
      return;
    }
    selected = doc.spans
      .map((span, index) => ({ span, index }))
      .filter(({ span }) => span.text.trim().length > 0)
      .map(({ index }) => index);
    selectionSource = { kind: "all" };
    renderPanel();
    emitChange();
  };

  const copySelected = async (): Promise<void> => {
    if (busy) {
      setNoticeKey("preview.note.busy", "hint");
      return;
    }
    if (running) {
      setNoticeKey("preview.note.ocr_running", "progress");
      return;
    }
    if (!doc) {
      setNoticeKey("preview.error.no_text", "error");
      return;
    }
    const fragment = panelSelectionText();
    const current = matches[matchIndex];
    const source: SelectionSource | null = fragment
      ? { kind: "fragment", text: fragment }
      : selectionSource ?? (current ? { kind: "fragment", text: current.fragment } : null);
    if (!source) {
      setNoticeKey("preview.error.no_selection", "error");
      return;
    }
    busy = true;
    syncActions();
    try {
      let copied: string;
      if (source.kind === "point") {
        copied = await invoke<string>("copy_ocr_point", {
          x: source.point.x,
          y: source.point.y,
        });
      } else if (source.kind === "rect") {
        copied = await invoke<string>("copy_ocr_rect", {
          x: source.rect.x,
          y: source.rect.y,
          width: source.rect.width,
          height: source.rect.height,
        });
      } else if (source.kind === "all") {
        copied = await invoke<string>("copy_ocr_all");
      } else {
        copied = await invoke<string>("copy_ocr_fragment", { text: source.text });
      }
      setNoticeKey("preview.note.ocr_copied", "success", { snippet: snippetOf(copied) });
    } catch (error) {
      setNoticeText(invokeError(error, t("preview.error.no_selection")), "error");
    } finally {
      busy = false;
      syncActions();
    }
  };

  const copyAll = async (): Promise<void> => {
    if (busy) {
      setNoticeKey("preview.note.busy", "hint");
      return;
    }
    if (running) {
      setNoticeKey("preview.note.ocr_running", "progress");
      return;
    }
    if (!doc) {
      setNoticeKey("preview.error.no_text", "error");
      return;
    }
    busy = true;
    syncActions();
    try {
      await invoke<string>("copy_ocr_all");
      setNoticeKey("preview.note.ocr_all_copied", "success");
    } catch (error) {
      setNoticeText(invokeError(error, t("preview.error.no_text")), "error");
    } finally {
      busy = false;
      syncActions();
    }
  };

  const applySelection = (next: number[], source: SelectionSource | null): void => {
    selected = next;
    selectionSource = source;
    renderPanel();
    emitChange();
  };

  const pointerDown = (point: OcrPoint): boolean => {
    if (!active || !doc || running) {
      return false;
    }
    dragging = true;
    dragStart = point;
    dragCurrent = point;
    const hit = indexAtPoint(doc.spans, point.x, point.y);
    if (hit < 0) {
      applySelection([], null);
    } else {
      applySelection([hit], { kind: "point", point: spanCenter(doc.spans[hit]) });
    }
    return true;
  };

  const pointerMove = (point: OcrPoint): boolean => {
    if (!dragging || !doc) {
      return false;
    }
    dragCurrent = point;
    const drag = Math.hypot(point.x - (dragStart?.x ?? point.x), point.y - (dragStart?.y ?? point.y));
    if (drag < DRAG_THRESHOLD) {
      const hit = indexAtPoint(doc.spans, point.x, point.y);
      if (hit < 0) {
        applySelection([], null);
      } else {
        applySelection([hit], { kind: "point", point: spanCenter(doc.spans[hit]) });
      }
      return true;
    }
    const rect = normalizeRect(dragStart ?? point, point);
    applySelection(indicesInRect(doc.spans, rect), { kind: "rect", rect });
    return true;
  };

  const pointerUp = (): boolean => {
    if (!dragging) {
      return false;
    }
    // 选择到此为止;复制必须由用户显式触发(Ctrl+C / 面板按钮)。
    clearDrag();
    emitChange();
    return true;
  };

  const domOffset = (node: Node, offset: number): number => {
    const walker = document.createTreeWalker(panelText, NodeFilter.SHOW_TEXT);
    let total = 0;
    while (walker.nextNode()) {
      const text = walker.currentNode.textContent ?? "";
      if (walker.currentNode === node) {
        return total + Array.from(text.slice(0, offset)).length;
      }
      total += Array.from(text).length;
    }
    return total;
  };

  const selectedPanelRange = (): CharRange | null => {
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || selection.rangeCount === 0) {
      return null;
    }
    const range = selection.getRangeAt(0);
    if (!panelText.contains(range.startContainer) || !panelText.contains(range.endContainer)) {
      return null;
    }
    return {
      start: domOffset(range.startContainer, range.startOffset),
      end: domOffset(range.endContainer, range.endOffset),
    };
  };

  const spansInRange = (range: CharRange): number[] => {
    if (!doc) {
      return [];
    }
    const hits: number[] = [];
    for (let i = 0; i < doc.spans.length; i += 1) {
      const offsets = spanOffsets[i];
      if (!offsets) {
        continue;
      }
      if (offsets.start < range.end && offsets.end > range.start) {
        hits.push(i);
      }
    }
    return hits;
  };

  const indexAtCharOffset = (offset: number): number => {
    if (!doc) {
      return -1;
    }
    for (let i = 0; i < doc.spans.length; i += 1) {
      const range = spanOffsets[i];
      if (range && offset >= range.start && offset < range.end) {
        return i;
      }
    }
    return -1;
  };

  const caretRangeAt = (x: number, y: number): Range | null => {
    const documentWithCaret = document as Document & {
      caretRangeFromPoint?: (x: number, y: number) => Range | null;
      caretPositionFromPoint?: (x: number, y: number) => { offsetNode: Node; offset: number } | null;
    };
    if (typeof documentWithCaret.caretRangeFromPoint === "function") {
      return documentWithCaret.caretRangeFromPoint(x, y);
    }
    const position = documentWithCaret.caretPositionFromPoint?.(x, y);
    if (!position) {
      return null;
    }
    const range = document.createRange();
    range.setStart(position.offsetNode, position.offset);
    range.collapse(true);
    return range;
  };

  const paint = (ctx: CanvasRenderingContext2D): void => {
    if (!active || !doc) {
      return;
    }
    const lineWidth = Math.max(1, ctx.canvas.width / 900);
    const selectedSet = new Set(selected);
    const plain: number[] = [];
    const searchHits: number[] = [];
    const currentHits: number[] = [];
    const currentMatch = matches[matchIndex];
    for (let i = 0; i < doc.spans.length; i += 1) {
      if (!doc.spans[i].text.trim() || selectedSet.has(i)) {
        continue;
      }
      const offsets = spanOffsets[i];
      if (!offsets) {
        plain.push(i);
        continue;
      }
      const isCurrent =
        currentMatch !== undefined &&
        currentMatch.start < offsets.end &&
        currentMatch.end > offsets.start;
      if (isCurrent) {
        currentHits.push(i);
        continue;
      }
      if (matches.some((match) => match.start < offsets.end && match.end > offsets.start)) {
        searchHits.push(i);
        continue;
      }
      plain.push(i);
    }
    const strokeSpan = (index: number, fill: string, stroke: string, width: number): void => {
      const span = doc?.spans[index];
      if (!span) {
        return;
      }
      ctx.fillStyle = fill;
      ctx.strokeStyle = stroke;
      ctx.lineWidth = width;
      ctx.beginPath();
      ctx.rect(span.x, span.y, span.width, span.height);
      ctx.fill();
      ctx.stroke();
    };
    ctx.save();
    // 识别完成即高亮全部文字区域(弱);搜索命中/当前命中/已选逐级增强,
    // 三态在图上与面板一致。
    for (const index of plain) {
      strokeSpan(index, withAlpha(colors.hl, 0.08), withAlpha(colors.hl, 0.32), lineWidth);
    }
    for (const index of searchHits) {
      strokeSpan(index, withAlpha(colors.hl, 0.16), withAlpha(colors.hl, 0.6), lineWidth);
    }
    for (const index of currentHits) {
      strokeSpan(
        index,
        withAlpha(colors.hl, 0.24),
        withAlpha(colors.hlStrong, 0.85),
        lineWidth * 1.4,
      );
    }
    for (const index of selectedSet) {
      strokeSpan(
        index,
        withAlpha(colors.hl, 0.38),
        withAlpha(colors.hlStrong, 0.95),
        lineWidth * 1.6,
      );
    }
    const rubber = dragging && dragStart && dragCurrent ? normalizeRect(dragStart, dragCurrent) : null;
    if (rubber && (rubber.width > MIN_RUBBER || rubber.height > MIN_RUBBER)) {
      ctx.fillStyle = withAlpha(colors.hl, 0.08);
      ctx.strokeStyle = withAlpha(colors.hlStrong, 0.9);
      ctx.lineWidth = lineWidth;
      ctx.setLineDash([6, 4]);
      ctx.fillRect(rubber.x, rubber.y, rubber.width, rubber.height);
      ctx.strokeRect(rubber.x, rubber.y, rubber.width, rubber.height);
    }
    ctx.restore();
  };

  panel.addEventListener("click", (event) => {
    const target = event.target;
    const button = target instanceof Element ? target.closest("[data-ocr-action]") : null;
    if (!(button instanceof HTMLButtonElement) || button.hidden) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const action = button.dataset.ocrAction;
    if (action === "close-panel") {
      closePanel();
    } else if (action === "copy-selected") {
      void copySelected();
    } else if (action === "copy-all") {
      void copyAll();
    } else if (action === "prev") {
      stepMatch(-1);
    } else if (action === "next") {
      stepMatch(1);
    }
  });

  searchInput.addEventListener("input", () => {
    void applySearch();
  });
  searchInput.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" || event.isComposing) {
      return;
    }
    event.preventDefault();
    stepMatch(event.shiftKey ? -1 : 1);
  });

  panelText.addEventListener("mouseup", (event) => {
    if (!active || !doc) {
      return;
    }
    const selection = window.getSelection();
    if (selection && !selection.isCollapsed) {
      // 拖选已由 selectionchange 联动到图上,不再重绘面板破坏选区。
      return;
    }
    const range = caretRangeAt(event.clientX, event.clientY);
    if (!range) {
      return;
    }
    const index = indexAtCharOffset(domOffset(range.startContainer, range.startOffset));
    if (index < 0) {
      applySelection([], null);
    } else {
      applySelection([index], { kind: "point", point: spanCenter(doc.spans[index]) });
    }
  });

  // 面板拖选 → 图上选中:双向联动的面板一侧。
  document.addEventListener("selectionchange", () => {
    if (!active || panel.hidden) {
      return;
    }
    const range = selectedPanelRange();
    if (!range) {
      return;
    }
    const text = panelSelectionText();
    const next = spansInRange(range);
    if (!sameIndices(next, selected)) {
      applySelection(next, text ? { kind: "fragment", text } : null);
    }
  });

  const refreshLabels = (): void => {
    syncSearchStatus();
    if (lastNotice?.key) {
      notice(t(lastNotice.key, lastNotice.params), lastNotice.kind);
    }
  };

  return {
    get active() {
      return active;
    },
    document: () => doc,
    isRunning: () => running,
    activate,
    deactivate,
    reset,
    paint,
    pointerDown,
    pointerMove,
    pointerUp,
    selectAll,
    copySelected,
    copyAll,
    closePanel,
    refreshLabels,
  };
}
