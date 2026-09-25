import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  annotationToolForKey,
  mountAnnotationEditor,
  readAnnotationDefaults,
  resolveCanvasColor,
  type Annotation,
  type AnnotationEditor,
} from "../annotation";
import { applyTranslations, t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import "./preview.css";

interface PreviewFrame {
  width: number;
  height: number;
  scale: number;
}

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

const FALLBACK_OCR_HL = "#0ea5e9";
const FALLBACK_OCR_HL_STRONG = "#0369a1";

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
    <div class="preview-toolbar">
      <div class="preview-tools" role="toolbar" data-annotation-toolbar data-i18n-aria-label="preview.toolbar_group" aria-label="标注" data-tauri-drag-region="false"></div>
      <div class="preview-actions" data-tauri-drag-region="false">
        <button type="button" class="icon-action" data-tool="ocr" data-i18n-title="preview.action.ocr_title" data-i18n-aria-label="preview.action.ocr" title="取字 (O)" aria-label="取字" data-tauri-drag-region="false">${icons.ocr}</button>
        <button type="button" class="icon-action" data-action="copy-ocr-all" hidden data-i18n-title="preview.action.copy_all" data-i18n-aria-label="preview.action.copy_all" title="复制全部" aria-label="复制全部" data-tauri-drag-region="false">${icons.copy}</button>
        <button type="button" class="icon-action" data-action="pin" data-i18n-title="preview.action.pin_title" data-i18n-aria-label="preview.action.pin" title="贴图" aria-label="贴图" data-tauri-drag-region="false">${icons.pin}</button>
        <button type="button" class="icon-action" data-action="update-pin" data-i18n-title="preview.action.update_pin_title" data-i18n-aria-label="preview.action.update_pin" title="更新贴图：确认后写回来源贴图" aria-label="更新贴图" hidden data-tauri-drag-region="false">${icons.annotate}</button>
        <div class="preview-save" data-save-quality-root>
          <div class="preview-save-split">
            <button type="button" data-action="save" data-i18n-title="preview.action.save_title" title="保存 (Ctrl+S)：扩展名决定格式 PNG/JPEG/WebP" data-tauri-drag-region="false">${icons.save}<span data-i18n="preview.action.save">保存</span></button>
            <button type="button" class="preview-save-caret" data-action="toggle-quality" data-i18n-title="preview.quality.group" data-i18n-aria-label="preview.quality.group" title="保存质量" aria-label="保存质量" aria-haspopup="true" aria-expanded="false" data-tauri-drag-region="false">${icons.chevronDown}</button>
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
        <button type="button" class="primary" data-action="copy" data-i18n-title="preview.action.copy_title" title="复制 (Ctrl+C)" data-tauri-drag-region="false">${icons.copy}<span data-i18n="preview.action.copy">复制</span></button>
      </div>
    </div>
    <div class="preview-stage">
      <div class="preview-frame">
        <canvas></canvas>
      </div>
    </div>
  `;

  const canvas = root.querySelector("canvas");
  const note = root.querySelector(".preview-note");
  const frameEl = root.querySelector(".preview-frame");
  const toolbarEl = root.querySelector("[data-annotation-toolbar]");
  const copyAllBtn = root.querySelector("[data-action=copy-ocr-all]");
  const ocrBtn = root.querySelector("[data-tool=ocr]");
  const pinBtn = root.querySelector("[data-action=pin]");
  const updatePinBtn = root.querySelector("[data-action=update-pin]");
  const saveQualityRoot = root.querySelector("[data-save-quality-root]");
  const saveQualityPanel = root.querySelector("[data-save-quality-panel]");
  const saveQualityToggle = root.querySelector("[data-action=toggle-quality]");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(note instanceof HTMLElement) ||
    !(frameEl instanceof HTMLElement) ||
    !(toolbarEl instanceof HTMLElement) ||
    !(copyAllBtn instanceof HTMLButtonElement) ||
    !(ocrBtn instanceof HTMLButtonElement) ||
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

  const rootStyle = getComputedStyle(root);
  const ocrHl = resolveCanvasColor(rootStyle.getPropertyValue("--ocr-hl"), FALLBACK_OCR_HL);
  const ocrHlStrong = resolveCanvasColor(
    rootStyle.getPropertyValue("--ocr-hl-strong"),
    FALLBACK_OCR_HL_STRONG,
  );
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
  // R21:标注层(共享模块)持有图元/撤销栈/文字编辑;这里只保留取字与导出动作。
  let editor: AnnotationEditor | null = null;
  let ocrActive = false;
  let ocrDoc: OcrDocument | null = null;
  let ocrSelected: number[] = [];
  let ocrDragging = false;
  let ocrStart: Point | null = null;
  let ocrCurrent: Point | null = null;
  let ocrGen = 0;
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
  // 功能入口开关(设置页 features.ocrEntry / features.pinEntry):
  // 关闭时取字按钮隐藏、O 键停用;关闭贴图后隐藏预览工具条贴图按钮。
  let ocrEntryEnabled = true;
  let pinEntryEnabled = true;
  // 贴图再标注(R9):非空表示本会话由贴图进入,确认后写回该 label。
  let writebackLabel: string | null = null;
  let saveQuality: ExportQuality = "high";

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
    if (ocrActive && ocrDoc) {
      const rubber =
        ocrDragging && ocrStart && ocrCurrent ? normalizeRect(ocrStart, ocrCurrent) : null;
      paintOcr(ctx, ocrDoc.spans, ocrSelected, rubber, ocrColors);
    }
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

  const syncCopyAll = (): void => {
    copyAllBtn.hidden = !ocrActive || !ocrDoc || ocrDoc.spans.length === 0;
  };

  const syncWritebackUi = (): void => {
    updatePinBtn.hidden = writebackLabel === null;
    pinBtn.hidden = !pinEntryEnabled || writebackLabel !== null;
  };

  const syncSaveQuality = (): void => {
    saveQualityRoot.querySelectorAll<HTMLButtonElement>("[data-save-quality]").forEach((button) => {
      button.classList.toggle("active", button.dataset.saveQuality === saveQuality);
    });
  };

  const toggleQualityPanel = (open?: boolean): void => {
    const next = open ?? saveQualityPanel.hidden;
    saveQualityPanel.hidden = !next;
    saveQualityToggle.classList.toggle("active", next);
    saveQualityToggle.setAttribute("aria-expanded", next ? "true" : "false");
  };

  editor = mountAnnotationEditor({
    root,
    canvas,
    ctx,
    toolbar: toolbarEl,
    textHost: frameEl,
    frame: () => frame,
    redraw,
    isEditable: () => !ocrActive,
    onToolHint: (hint) => {
      if (hint) {
        setNoteKey(hint.key, hint.params);
      } else if (!note.classList.contains("is-error")) {
        setNoteSource(copiedSource, copiedKind);
      }
    },
    onToolChange: () => {
      ocrBtn.classList.remove("active");
      if (ocrActive) {
        ocrActive = false;
        copyAllBtn.hidden = true;
        redraw();
      }
    },
    // 样式持久化失败等:沿用既有提示条错误呈现,不静默丢失。
    onError: (error) => {
      if (typeof error === "string") {
        setNote(error, "error");
      } else {
        setNoteKey(error.key, error.params, "error");
      }
    },
  });

  const activateOcr = (): void => {
    if (!ocrEntryEnabled) {
      return;
    }
    editor?.commitText();
    editor?.deactivateTool();
    editor?.clearSelection();
    ocrActive = true;
    ocrBtn.classList.add("active");
    root.dataset.tool = "ocr";
    ocrSelected = [];
    ocrCurrent = null;
    ocrStart = null;
    ocrDragging = false;
    syncCopyAll();
    if (!ocrDoc) {
      void runOcr();
    } else if (!note.classList.contains("is-error")) {
      setNoteKey("preview.note.ocr_hint");
    }
    redraw();
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
      return;
    }
    editor?.commitText();
    busy = true;
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
    if (busy || writebackLabel === null) {
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

  canvas.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || !frame || !ocrActive || !ocrDoc) {
      return;
    }
    event.preventDefault();
    const point = physicalPoint(event);
    ocrDragging = true;
    ocrStart = point;
    ocrCurrent = point;
    const hit = indexAtPoint(ocrDoc.spans, point.x, point.y);
    ocrSelected = hit === -1 ? [] : [hit];
    redraw();
  });

  // 取字用右键:共享标注层不接管取字工具,这里兜底阻止浏览器菜单。
  canvas.addEventListener("contextmenu", (event) => {
    event.preventDefault();
  });

  window.addEventListener("mousemove", (event) => {
    if (!ocrDragging || !ocrStart || !ocrDoc) {
      return;
    }
    ocrCurrent = physicalPoint(event);
    const rubber = normalizeRect(ocrStart, ocrCurrent);
    if (Math.hypot(ocrCurrent.x - ocrStart.x, ocrCurrent.y - ocrStart.y) < 4) {
      const hit = indexAtPoint(ocrDoc.spans, ocrCurrent.x, ocrCurrent.y);
      ocrSelected = hit === -1 ? [] : [hit];
    } else {
      ocrSelected = indicesInRect(ocrDoc.spans, rubber);
    }
    redraw();
  });

  window.addEventListener("mouseup", () => {
    if (!ocrDragging || !ocrStart || !ocrCurrent) {
      return;
    }
    ocrDragging = false;
    const from = ocrStart;
    const to = ocrCurrent;
    ocrStart = null;
    ocrCurrent = null;
    redraw();
    void copyOcrSelection(from, to);
  });

  root.addEventListener("click", (event) => {
    const button = event.target instanceof Element ? event.target.closest("button") : null;
    if (!(button instanceof HTMLButtonElement)) {
      return;
    }
    if (button.dataset.tool === "ocr") {
      activateOcr();
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

  document.addEventListener("click", (event) => {
    if (saveQualityPanel.hidden) {
      return;
    }
    if (event.target instanceof Node && saveQualityRoot.contains(event.target)) {
      return;
    }
    toggleQualityPanel(false);
  });

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

  // 标注模块已处理编辑器/菜单/面板/选中与撤销快捷键(Escape/Delete/Ctrl+Z);
  // 这里只保留取字、保存、复制与关闭预览。
  window.addEventListener("keydown", (event) => {
    if (event.defaultPrevented || event.isComposing || event.keyCode === 229) {
      return;
    }
    if (event.key === "Escape") {
      void invoke("close_preview");
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
      if (key === "c") {
        if (document.activeElement instanceof HTMLTextAreaElement) {
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
    if (ocrEntryEnabled && key === "o") {
      event.preventDefault();
      activateOcr();
      return;
    }
    // 取字工具激活时,A/R/E/L/M/B/H/P/N/T 切回标注工具(与既有预览一致)。
    if (ocrActive) {
      const next = annotationToolForKey(key);
      if (next) {
        event.preventDefault();
        editor?.setTool(next);
      }
    }
  });

  // 重读功能入口开关并同步预览 UI:ocrEntryEnabled 同时驱动工具条按钮显隐、
  // O 键映射与当前取字工具的回退;pinEntryEnabled 驱动贴图按钮显隐;
  // 每次 reload 都要重读,不能只在首载做一次。
  const syncFeatureFlags = (settings?: { features?: { ocrEntry?: boolean; pinEntry?: boolean } }): void => {
    ocrEntryEnabled = settings?.features?.ocrEntry !== false;
    ocrBtn.hidden = !ocrEntryEnabled;
    if (!ocrEntryEnabled && ocrActive) {
      ocrActive = false;
      ocrBtn.classList.remove("active");
      editor?.setTool("arrow");
      copyAllBtn.hidden = true;
      redraw();
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
  // 跨会话记忆:加载时读后端保存的上次样式(读写失败均静默回退当前值);
  // 同时读取功能入口开关,关闭取字后隐藏预览工具条按钮并停用 O 键。
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
        editor?.setStyle(readAnnotationDefaults(settings));
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
        editor?.setAnnotations(carried);
        void invoke<boolean>("take_pending_preview_ocr")
          .then((startOcr) => {
            if (generation === previewLoad && startOcr) {
              activateOcr();
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

  void listen("preview-reload", () => {
    // 新帧可能带入选区即时标注(R21):列表随帧在 loadPreview 中恢复,
    // 这里先清空避免旧编辑态残留。
    ocrDoc = null;
    ocrSelected = [];
    editor?.setAnnotations([]);
    editor?.cancelText();
    // 携带说明随新帧重算(image.onload);先清空,避免加载失败时残留旧前缀。
    carriedNoteSource = null;
    // 每次新帧重读功能入口开关:设置页关闭取字后,复用的预览窗口在下一次
    // 截取时也要隐藏按钮/停用 O 键;样式默认只在首次加载,不在 reload 重置。
    reloadFeatureFlags();
    loadPreview();
  });
  loadPreview();

  // 语言切换:静态标签由 main 的 applyTranslations 更新;这里刷新标注模块
  // 组合出的本地化标签,并重渲染来源可解析的提示条。
  const refreshOptionLabels = (): void => {
    editor?.refreshLabels();
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
  };
}

type Point = { x: number; y: number };

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
