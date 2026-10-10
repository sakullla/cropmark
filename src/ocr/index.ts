import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { resolveCanvasColor } from "../annotation";
import { currentLanguage, t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import { NOTICE_AUTO_HIDE_MS } from "../feedback";
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

export type OcrNoticeKind = "progress" | "hint" | "success" | "error" | "empty";

/** R7:Rust `ocr-progress` 事件载荷(裸阶段名);与 Rust `OcrStage` serde 表示一致。 */
export type OcrStage = "preparing" | "recognizing" | "retrying" | "post_processing";

const OCR_STAGE_KEYS: Record<OcrStage, CatalogKey> = {
  preparing: "preview.note.ocr_stage.preparing",
  recognizing: "preview.note.ocr_stage.recognizing",
  retrying: "preview.note.ocr_stage.retrying",
  post_processing: "preview.note.ocr_stage.post_processing",
};

/** 未知/丢失的载荷返回 null:调用方保持「识别中」常驻兜底,不报错不阻塞。 */
function ocrStageKey(payload: string): CatalogKey | null {
  return Object.prototype.hasOwnProperty.call(OCR_STAGE_KEYS, payload)
    ? OCR_STAGE_KEYS[payload as OcrStage]
    : null;
}

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
  /**
   * R6:预览旋转/裁剪后由宿主替换识别结果(坐标已按同一变换重映射);传入
   * null/空文本清空。进行中的识别作废,旧选择清除,面板按新坐标重绘。
   */
  setDocument: (doc: OcrDocument | null) => void;
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
const SEARCH_DEBOUNCE_MS = 150;

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
      <div><h2 data-i18n="preview.ocr_panel.title">${t("preview.ocr_panel.title")}</h2><p class="ocr-panel-subtitle" data-i18n="preview.ocr_panel.subtitle">${t("preview.ocr_panel.subtitle")}</p></div>
      <button type="button" class="icon-btn" data-ocr-action="close-panel" data-i18n-aria-label="preview.ocr_panel.close" aria-label="${t("preview.ocr_panel.close")}">${icons.close}</button>
    </div>
    <div class="ocr-panel-search">
      <input type="search" class="ocr-panel-query" data-ocr-search data-i18n-placeholder="preview.ocr_panel.search" data-i18n-aria-label="preview.ocr_panel.search" placeholder="${t("preview.ocr_panel.search")}" aria-label="${t("preview.ocr_panel.search")}" autocomplete="off" spellcheck="false" />
      <div class="ocr-panel-nav">
        <span class="ocr-panel-count" data-ocr-search-count aria-live="polite"></span>
        <button type="button" data-ocr-action="prev" data-i18n-aria-label="preview.ocr_panel.prev" aria-label="${t("preview.ocr_panel.prev")}" disabled>↑</button>
        <button type="button" data-ocr-action="next" data-i18n-aria-label="preview.ocr_panel.next" aria-label="${t("preview.ocr_panel.next")}" disabled>↓</button>
      </div>
    </div>
    <div class="ocr-panel-state" data-ocr-state hidden>
      <p data-ocr-state-title></p>
      <p class="ocr-panel-state-hint" data-i18n="preview.ocr_panel.empty_hint">${t("preview.ocr_panel.empty_hint")}</p>
      <button type="button" data-ocr-action="retry" data-i18n="preview.ocr_panel.retry">${t("preview.ocr_panel.retry")}</button>
    </div>
    <div class="ocr-panel-text" data-ocr-text tabindex="0"></div>
    <footer class="ocr-panel-footer">
      <p class="ocr-panel-summary" data-ocr-summary></p>
      <div class="ocr-panel-actions" data-ocr-actions hidden>
        <button type="button" data-ocr-action="copy-selected" data-i18n="preview.ocr_panel.copy_selected">${t("preview.ocr_panel.copy_selected")}</button>
        <button type="button" class="primary" data-ocr-action="copy-all" data-i18n="preview.ocr_panel.copy_all">${t("preview.ocr_panel.copy_all")}</button>
      </div>
    </footer>
  `;
  host.append(panel);

  // R7:识别进行中的阶段化进度呈现(共享 .progress 旋转圈 + 阶段词条)。
  // 纯视觉:词条同时走宿主 notice 通道(role=status 由宿主提示条承担播报),
  // 这里不重复播报;宿主提示条本身不动。
  const progressNote = document.createElement("p");
  progressNote.className = "ocr-progress";
  progressNote.setAttribute("aria-hidden", "true");
  progressNote.hidden = true;
  progressNote.innerHTML = '<span class="progress"></span><span class="ocr-progress-text"></span>';
  panel.querySelector("[data-ocr-state]")!.prepend(progressNote);
  const progressText = progressNote.querySelector(".ocr-progress-text") as HTMLElement;

  const stateView = panel.querySelector("[data-ocr-state]") as HTMLElement;
  const stateTitle = panel.querySelector("[data-ocr-state-title]") as HTMLElement;
  const stateHint = panel.querySelector(".ocr-panel-state-hint") as HTMLElement;
  const retryBtn = panel.querySelector("[data-ocr-action=retry]") as HTMLButtonElement;
  const summary = panel.querySelector("[data-ocr-summary]") as HTMLElement;
  const searchRow = panel.querySelector(".ocr-panel-search") as HTMLElement;
  const searchInput = panel.querySelector("[data-ocr-search]") as HTMLInputElement;
  const searchCount = panel.querySelector("[data-ocr-search-count]") as HTMLElement;
  const panelText = panel.querySelector("[data-ocr-text]") as HTMLElement;
  const prevBtn = panel.querySelector("[data-ocr-action=prev]") as HTMLButtonElement;
  const nextBtn = panel.querySelector("[data-ocr-action=next]") as HTMLButtonElement;
  const actionsRow = panel.querySelector("[data-ocr-actions]") as HTMLElement;
  const copySelectedBtn = panel.querySelector("[data-ocr-action=copy-selected]") as HTMLButtonElement;
  const copyAllBtn = panel.querySelector("[data-ocr-action=copy-all]") as HTMLButtonElement;
  let copyFlashTimer = 0;
  let copyFlashBtn: HTMLButtonElement | null = null;
  const restoreCopyFlash = (): void => {
    if (copyFlashTimer) {
      window.clearTimeout(copyFlashTimer);
      copyFlashTimer = 0;
    }
    const button = copyFlashBtn;
    copyFlashBtn = null;
    if (!button) {
      return;
    }
    const key =
      button.dataset.ocrAction === "copy-all"
        ? "preview.ocr_panel.copy_all"
        : "preview.ocr_panel.copy_selected";
    button.dataset.i18n = key;
    button.textContent = t(key);
    button.classList.remove("is-copied");
  };
  // 文案宽度按语言+类名缓存:图上拖橡皮筋时 renderPanel→syncActions 每个鼠标
  // 事件都会走到,克隆探针逐次 append+测量会在事件频率上强制整页布局。
  // 与 progressWidthCache/searchCountWidthCache 同一套思路。
  const buttonTextWidthCache = new Map<string, string>();
  const reserveButtonTextWidth = (button: HTMLButtonElement, texts: readonly string[]): void => {
    if (!button.isConnected) {
      return;
    }
    const cacheKey = `${currentLanguage()}|${button.className}|${texts.join("\u0000")}`;
    const cached = buttonTextWidthCache.get(cacheKey);
    if (cached !== undefined) {
      if (button.style.minWidth !== cached) {
        button.style.minWidth = cached;
      }
      return;
    }
    const probe = button.cloneNode(false);
    if (!(probe instanceof HTMLButtonElement)) {
      return;
    }
    probe.className = button.className;
    probe.style.position = "absolute";
    probe.style.visibility = "hidden";
    probe.style.pointerEvents = "none";
    probe.style.width = "auto";
    probe.style.minWidth = "0";
    probe.style.left = "0";
    probe.style.top = "0";
    panel.append(probe);
    let widest = 0;
    for (const text of texts) {
      probe.textContent = text;
      widest = Math.max(widest, probe.getBoundingClientRect().width);
    }
    probe.remove();
    if (widest > 0) {
      const minWidth = `${Math.ceil(widest)}px`;
      buttonTextWidthCache.set(cacheKey, minWidth);
      button.style.minWidth = minWidth;
    }
  };
  const reserveCopyWidths = (): void => {
    const copied = t("preview.action.copied");
    reserveButtonTextWidth(copySelectedBtn, [t("preview.ocr_panel.copy_selected"), copied]);
    reserveButtonTextWidth(copyAllBtn, [t("preview.ocr_panel.copy_all"), copied]);
  };

  const flashCopyButton = (button: HTMLButtonElement): void => {
    reserveCopyWidths();
    restoreCopyFlash();
    copyFlashBtn = button;
    button.classList.add("is-copied");
    button.dataset.i18n = "preview.action.copied";
    button.textContent = t("preview.action.copied");
    copyFlashTimer = window.setTimeout(() => {
      copyFlashTimer = 0;
      restoreCopyFlash();
    }, NOTICE_AUTO_HIDE_MS);
  };
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
  let recognitionError: string | null = null;
  let generation = 0;
  let searchGen = 0;
  let panelPaintFrame = 0;
  let searchDebounce = 0;
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
  // R7:当前呈现的阶段词条(语言切换时用它重渲染进度行)。
  let stageKey: CatalogKey = "preview.note.ocr_running";

  const emitChange = (): void => {
    options.onChange?.();
  };

  // 阶段文案长短不一。转圈旁边的字先按最长那句留宽，换阶段时转圈不再左右晃。
  const progressWidthCache = new Map<string, string>();
  const reserveProgressWidth = (): void => {
    const parent = progressText.parentElement;
    if (!parent || parent.getClientRects().length === 0) {
      return;
    }
    const key = currentLanguage();
    let width = progressWidthCache.get(key);
    if (!width) {
      const probe = progressText.cloneNode(false);
      if (!(probe instanceof HTMLElement)) {
        return;
      }
      probe.className = progressText.className;
      probe.style.position = "absolute";
      probe.style.visibility = "hidden";
      probe.style.pointerEvents = "none";
      probe.style.width = "auto";
      probe.style.minWidth = "0";
      probe.style.whiteSpace = "nowrap";
      parent.append(probe);
      let widest = 0;
      const labels: CatalogKey[] = [
        "preview.note.ocr_running",
        ...Object.values(OCR_STAGE_KEYS),
      ];
      for (const label of labels) {
        probe.textContent = t(label);
        widest = Math.max(widest, probe.getBoundingClientRect().width);
      }
      probe.remove();
      if (widest <= 0) {
        return;
      }
      width = `${Math.ceil(widest)}px`;
      progressWidthCache.set(key, width);
    }
    if (progressText.style.minWidth !== width) {
      progressText.style.minWidth = width;
    }
  };

  const showProgressStage = (key: CatalogKey): void => {
    stageKey = key;
    progressText.textContent = t(key);
    progressNote.hidden = false;
    reserveProgressWidth();
  };

  const hideProgressStage = (): void => {
    progressNote.hidden = true;
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
    cancelScheduledPaint();
    searchInput.value = "";
    matches = [];
    matchIndex = 0;
    panelText.replaceChildren();
    searchCount.textContent = "";
    prevBtn.disabled = true;
    nextBtn.disabled = true;
    panel.hidden = true;
  };

  const hasDocument = (): boolean => doc !== null && doc.fullText.trim().length > 0;

  const panelSelectionText = (): string | null => {
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || selection.rangeCount === 0) {
      return null;
    }
    const node = selection.anchorNode;
    if (!node || !panelText.contains(node) || !selection.focusNode || !panelText.contains(selection.focusNode)) {
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
      reserveCopyWidths();
      const canCopySelection = hasCopySource();
      const needsSelection = !canCopySelection && !busy;
      copySelectedBtn.disabled = busy;
      // toggleAttribute 会写成空字符串，对不上 [aria-disabled="true"]，按钮看起来能点，点下去却是「没有选区」。
      if (needsSelection) {
        copySelectedBtn.setAttribute("aria-disabled", "true");
      } else {
        copySelectedBtn.removeAttribute("aria-disabled");
      }
      if (needsSelection) {
        copySelectedBtn.dataset.i18nTitle = "preview.ocr_panel.selection_hint";
        copySelectedBtn.dataset.tooltip = t("preview.ocr_panel.selection_hint");
        copySelectedBtn.removeAttribute("title");
      } else {
        delete copySelectedBtn.dataset.i18nTitle;
        delete copySelectedBtn.dataset.tooltip;
      }
      copyAllBtn.disabled = busy;
      // 已经有选区或当前命中时，复制所选才是这次要做的事。没有选区时，复制全部仍是主按钮。
      copySelectedBtn.classList.toggle("primary", canCopySelection);
      copyAllBtn.classList.toggle("primary", !canCopySelection);
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
    if (first) {
      const item = first.getBoundingClientRect();
      const viewport = panelText.getBoundingClientRect();
      if (item.top < viewport.top) panelText.scrollTop += item.top - viewport.top - 8;
      else if (item.bottom > viewport.bottom) panelText.scrollTop += item.bottom - viewport.bottom + 8;
    }
  };

  // 面板文本重绘按帧合并:拖选/步进每次事件触发的是「调度」而非立即重建,
  // 同一帧内多次事件(图上拖橡皮筋的 pointerMove)只重建一次整份文本 DOM,
  // 面板滚动跳变随多次叠加的 scrollTop 修正一起消失。
  const cancelScheduledPaint = (): void => {
    if (panelPaintFrame !== 0) {
      cancelAnimationFrame(panelPaintFrame);
      panelPaintFrame = 0;
    }
  };

  const schedulePaint = (): void => {
    if (panelPaintFrame !== 0) {
      return;
    }
    panelPaintFrame = requestAnimationFrame(() => {
      panelPaintFrame = 0;
      // 回调执行先于本帧渲染,不会露出旧内容;面板已隐藏(退出/重置)则跳过。
      if (!panel.hidden) {
        paintPanel();
      }
    });
  };

  // 「无匹配」比「1/3」宽。先按较宽的那句留宽，搜到或搜不到时输入框不再被挤来挤去。
  const searchCountWidthCache = new Map<string, number>();
  const measureSearchCount = (text: string): number => {
    const parent = searchCount.parentElement;
    if (!parent || parent.getClientRects().length === 0) {
      return 0;
    }
    const probe = searchCount.cloneNode(false);
    if (!(probe instanceof HTMLElement)) {
      return 0;
    }
    probe.hidden = false;
    probe.className = "ocr-panel-count";
    probe.style.position = "absolute";
    probe.style.visibility = "hidden";
    probe.style.pointerEvents = "none";
    probe.style.width = "auto";
    probe.style.minWidth = "0";
    probe.style.whiteSpace = "nowrap";
    probe.textContent = text;
    parent.append(probe);
    const width = probe.getBoundingClientRect().width;
    probe.remove();
    return width;
  };
  const reserveSearchCountWidth = (label: string): void => {
    if (!label) {
      if (searchCount.style.minWidth) {
        searchCount.style.minWidth = "";
      }
      return;
    }
    const key = currentLanguage();
    let noMatch = searchCountWidthCache.get(key);
    if (noMatch === undefined) {
      noMatch = measureSearchCount(t("preview.ocr_panel.no_match"));
      if (noMatch > 0) {
        searchCountWidthCache.set(key, noMatch);
      }
    }
    const noMatchLabel = t("preview.ocr_panel.no_match");
    const labelWidth = label === noMatchLabel ? (noMatch ?? 0) : measureSearchCount(label);
    const widest = Math.max(noMatch ?? 0, labelWidth);
    if (widest <= 0) {
      return;
    }
    const next = `${Math.ceil(widest)}px`;
    if (searchCount.style.minWidth !== next) {
      searchCount.style.minWidth = next;
    }
  };

  const syncSearchStatus = (): void => {
    const query = searchInput.value.trim();
    let label = "";
    if (!query) {
      searchCount.textContent = "";
    } else if (matches.length === 0) {
      label = t("preview.ocr_panel.no_match");
      searchCount.textContent = label;
    } else {
      label = t("preview.ocr_panel.match_count", {
        current: matchIndex + 1,
        total: matches.length,
      });
      searchCount.textContent = label;
    }
    // 「无匹配」和「1/3」以前同一套正文色，搜不到时不容易看出来。
    searchCount.classList.toggle("is-empty", query.length > 0 && matches.length === 0);
    searchCount.hidden = !query;
    reserveSearchCountWidth(label);
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
    const visible = active && !panelDismissed;
    if (!visible) {
      panel.hidden = true;
      syncActions();
      return;
    }
    panel.hidden = false;
    const ready = hasDocument() && !running;
    panelText.hidden = !ready;
    searchRow.hidden = !ready;
    stateView.hidden = ready;
    progressNote.hidden = !running;
    stateTitle.hidden = running;
    stateHint.hidden = running;
    retryBtn.hidden = running;
    // 同值不再重建文本节点:renderPanel 随拖橡皮筋逐事件执行,文本节点重建
    // 会在事件频率上弄脏布局。
    const stateTitleText = recognitionError ?? t("preview.error.no_text");
    if (stateTitle.textContent !== stateTitleText) {
      stateTitle.textContent = stateTitleText;
    }
    // 引擎失败和「图里没有字」共用这一块。失败用危险色，空结果仍是正文。
    stateTitle.classList.toggle("is-error", recognitionError !== null);
    panel.setAttribute("aria-busy", String(running));
    if (ready && doc) {
      const summaryText = t("preview.ocr_panel.count", {
        count: Array.from(doc.fullText).length,
        lines: doc.fullText.split("\n").length,
      });
      if (summary.textContent !== summaryText) {
        summary.textContent = summaryText;
      }
      // 字数本身不是操作说明。有正文时悬停才提示怎么选。
      summary.dataset.tooltip = t("preview.ocr_panel.selection_hint");
    } else {
      if (summary.textContent !== t("preview.ocr_panel.subtitle")) {
        summary.textContent = t("preview.ocr_panel.subtitle");
      }
      delete summary.dataset.tooltip;
    }
    if (ready && !panelHasDomSelection()) {
      schedulePaint();
    }
    syncActions();
  };

  const closePanel = (): void => {
    deactivate();
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
      emitChange();
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
      emitChange();
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
    schedulePaint();
    syncSearchStatus();
    emitChange();
  };

  const runRecognition = async (): Promise<void> => {
    const token = ++generation;
    running = true;
    recognitionError = null;
    doc = null;
    spanOffsets = [];
    panelDismissed = false;
    clearSelection();
    clearDrag();
    clearPanelView();
    setNoticeKey("preview.note.ocr_running", "progress");
    showProgressStage("preview.note.ocr_running");
    renderPanel();
    emitChange();
    // R7:识别期间订阅后端阶段事件(准备/识别/重试/后处理)映射词条;
    // 订阅失败或事件丢失时保持上面的「识别中」常驻兜底,不阻塞识别。
    const unlistenStage = await listen<string>("ocr-progress", (event) => {
      if (token !== generation || !running) {
        return;
      }
      const key = ocrStageKey(String(event.payload));
      if (!key) {
        return;
      }
      showProgressStage(key);
      if (active) {
        setNoticeKey(key, "progress");
      }
    }).catch(() => null);
    try {
      const result = await invoke<OcrDocument>("recognize_preview");
      if (token !== generation) {
        return;
      }
      if (!result.fullText.trim()) {
        doc = null;
        spanOffsets = [];
        if (active) {
          setNoticeKey("preview.error.no_text", "empty");
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
      recognitionError = invokeError(error, t("preview.error.ocr_fallback"));
      clearPanelView();
      if (active) {
        setNoticeText(recognitionError, "error");
      }
    } finally {
      unlistenStage?.();
      if (token === generation) {
        running = false;
        hideProgressStage();
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
    panelDismissed = false;
    if (running) {
      showProgressStage(stageKey);
      renderPanel();
    } else if (!doc) {
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
    cancelScheduledPaint();
    // 退出取字即撤下阶段进度呈现;词条通道也随之静默(结果不再展示)。
    hideProgressStage();
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
    hideProgressStage();
    clearSelection();
    clearDrag();
    clearPanelView();
    syncActions();
    emitChange();
  };

  // R6:用宿主重映射后的识别结果替换当前文档。进行中的识别作废(token 失效);
  // 只清选择与拖选,面板保持原开关状态,搜索字符下标仍指向同一全文。
  const setDocument = (next: OcrDocument | null): void => {
    generation += 1;
    running = false;
    recognitionError = null;
    // 文档更替:旧文档的待绘制帧作废(空文档路径由 clearPanelView 再兜底)。
    cancelScheduledPaint();
    clearSelection();
    clearDrag();
    hideProgressStage();
    if (!next || !next.fullText.trim() || next.spans.length === 0) {
      doc = null;
      spanOffsets = [];
      clearPanelView();
      renderPanel();
      emitChange();
      return;
    }
    doc = next;
    spanOffsets = mapSpanOffsets(next);
    renderPanel();
    syncActions();
    emitChange();
  };

  const selectAll = (): void => {
    if (!active || !doc || doc.spans.length === 0) {
      return;
    }
    window.getSelection()?.removeAllRanges();
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
    let copied = false;
    busy = true;
    syncActions();
    try {
      let copiedText: string;
      if (source.kind === "point") {
        copiedText = await invoke<string>("copy_ocr_point", {
          x: source.point.x,
          y: source.point.y,
        });
      } else if (source.kind === "rect") {
        copiedText = await invoke<string>("copy_ocr_rect", {
          x: source.rect.x,
          y: source.rect.y,
          width: source.rect.width,
          height: source.rect.height,
        });
      } else if (source.kind === "all") {
        copiedText = await invoke<string>("copy_ocr_all");
      } else {
        copiedText = await invoke<string>("copy_ocr_fragment", { text: source.text });
      }
      setNoticeKey("preview.note.ocr_copied", "success", { snippet: snippetOf(copiedText) });
      copied = true;
    } catch (error) {
      setNoticeText(invokeError(error, t("preview.error.no_selection")), "error");
    } finally {
      busy = false;
      syncActions();
    }
    if (copied) {
      flashCopyButton(source.kind === "all" ? copyAllBtn : copySelectedBtn);
    }
  };

  const copyAll = async (): Promise<void> => {
    if (busy) {
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
    let copied = false;
    busy = true;
    syncActions();
    try {
      await invoke<string>("copy_ocr_all");
      setNoticeKey("preview.note.ocr_all_copied", "success");
      copied = true;
    } catch (error) {
      setNoticeText(invokeError(error, t("preview.error.no_text")), "error");
    } finally {
      busy = false;
      syncActions();
    }
    if (copied) {
      flashCopyButton(copyAllBtn);
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
    // 宿主可能把帧缩放后绘制(工作区覆盖层画布是帧的等比显示框):按当前
    // 变换还原帧空间宽度,线宽与虚线随帧比例变化,预览与覆盖层视觉一致。
    const transform = typeof ctx.getTransform === "function" ? ctx.getTransform() : null;
    const unit = transform ? Math.max(Math.hypot(transform.a, transform.b), Number.EPSILON) : 1;
    const lineWidth = Math.max(1, ctx.canvas.width / unit / 900);
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
    if (
      !(button instanceof HTMLButtonElement) ||
      button.hidden ||
      button.disabled ||
      button.getAttribute("aria-disabled") === "true"
    ) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const action = button.dataset.ocrAction;
    if (action === "retry") {
      if (!running) void runRecognition();
    } else if (action === "close-panel") {
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
    // 连续击键合并为一次搜索:防抖期间 searchGen 已保证过期结果被丢弃;
    // Enter 步进(stepMatch)不走防抖,立即响应。
    if (searchDebounce !== 0) {
      window.clearTimeout(searchDebounce);
    }
    searchDebounce = window.setTimeout(() => {
      searchDebounce = 0;
      void applySearch();
    }, SEARCH_DEBOUNCE_MS);
  });
  searchInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.isComposing) {
      event.preventDefault();
      stepMatch(event.shiftKey ? -1 : 1);
      return;
    }
    // 搜索框内 Esc 先清关键词(浏览器搜索框肌肉记忆):词已为空则不拦截,
    // 放行给宿主的 Esc 退出取字路径。stopPropagation 避免宿主同帧误触发。
    if (event.key === "Escape" && !event.isComposing && searchInput.value !== "") {
      searchInput.value = "";
      if (searchDebounce !== 0) {
        window.clearTimeout(searchDebounce);
        searchDebounce = 0;
      }
      void applySearch();
      event.preventDefault();
      event.stopPropagation();
    }
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
    } else {
      selectionSource = text ? { kind: "fragment", text } : null;
      syncActions();
    }
  });

  void document.fonts.ready.then(() => {
    if (panel.isConnected) {
      searchCountWidthCache.clear();
      progressWidthCache.clear();
      reserveCopyWidths();
      syncSearchStatus();
      reserveProgressWidth();
    }
  });

  const refreshLabels = (): void => {
    searchCountWidthCache.clear();
    progressWidthCache.clear();
    reserveCopyWidths();
    renderPanel();
    syncSearchStatus();
    if (lastNotice?.key) {
      notice(t(lastNotice.key, lastNotice.params), lastNotice.kind);
    }
    if (!progressNote.hidden) {
      progressText.textContent = t(stageKey);
      reserveProgressWidth();
    }
  };

  return {
    get active() {
      return active;
    },
    document: () => doc,
    isRunning: () => running,
    setDocument,
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
