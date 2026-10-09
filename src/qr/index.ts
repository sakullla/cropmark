import { invoke } from "@tauri-apps/api/core";
import { currentLanguage, t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import "./qr.css";

// R4:本地二维码识别结果模型。预览与冻结帧工作区覆盖层共用同一实现:
// 识别只读当前冻结/预览帧,内容展示在结果面板,复制必须由用户显式点击;
// 无码或失败只给本地化说明,不写剪贴板,也从不打开任何链接。

export interface QrRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface QrCode {
  text: string;
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface QrPayload {
  contents: string[];
  /** 与内容对齐的帧内矩形。缺失或宽高非正时,面板当作没有位置。 */
  codes?: QrCode[];
}

export type QrNoticeKind = "progress" | "hint" | "success" | "error" | "empty";

export interface QrModelOptions {
  /** 结果面板挂载容器(预览舞台 / 覆盖层根)。 */
  host: HTMLElement;
  /** 宿主提示通道:progress/hint 常驻到被替换,success/error 为结果反馈。 */
  notice: (message: string, kind: QrNoticeKind) => void;
  /** 激活态、结果或复制状态变化时通知宿主(同步 chrome 并重绘)。 */
  onChange?: () => void;
  /** 面板是否提供关闭按钮(覆盖层用 Esc 退出,不提供单独关闭)。 */
  closable?: boolean;
}

export interface QrModel {
  readonly active: boolean;
  contents: () => string[] | null;
  /** 正面积的码身矩形。没有位置时为空,面板留在右侧。 */
  regions: () => QrRect[];
  isRunning: () => boolean;
  activate: () => void;
  deactivate: () => boolean;
  reset: () => void;
  closePanel: () => void;
  copy: (index: number) => Promise<void>;
  refreshLabels: () => void;
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

export function mountQrModel(options: QrModelOptions): QrModel {
  const { host, notice } = options;
  const panel = document.createElement("aside");
  panel.className = "qr-panel";
  panel.hidden = true;
  panel.dataset.qrPanel = "";
  panel.setAttribute("role", "complementary");
  panel.setAttribute("data-i18n-aria-label", "preview.qr_panel.title");
  panel.setAttribute("aria-label", t("preview.qr_panel.title"));
  panel.innerHTML = `
    <div class="qr-panel-bar">
      <h2 data-i18n="preview.qr_panel.title">${t("preview.qr_panel.title")}</h2>
      <button type="button" class="icon-btn" data-qr-action="close-panel" data-i18n-aria-label="preview.qr_panel.close" aria-label="${t("preview.qr_panel.close")}">${icons.close}</button>
    </div>
    <div class="qr-panel-list" data-qr-list role="list"></div>
  `;
  host.append(panel);

  const closeBtn = panel.querySelector("[data-qr-action=close-panel]");
  const list = panel.querySelector("[data-qr-list]");
  if (!(closeBtn instanceof HTMLButtonElement) || !(list instanceof HTMLElement)) {
    return {
      get active() {
        return false;
      },
      contents: () => null,
      regions: () => [],
      isRunning: () => false,
      activate: () => undefined,
      deactivate: () => false,
      reset: () => undefined,
      closePanel: () => undefined,
      copy: async () => undefined,
      refreshLabels: () => undefined,
    };
  }
  if (options.closable === false) {
    closeBtn.hidden = true;
  }

  let active = false;
  let contents: string[] | null = null;
  let codes: QrCode[] = [];
  let running = false;
  let generation = 0;
  let busy = false;
  let panelDismissed = true;
  // 复制按钮被 renderPanel 重建时的待恢复焦点序号(见 renderPanel 内说明)。
  let copyFocusIndex: number | null = null;
  let copiedIndex: number | null = null;
  let copiedTimer = 0;
  let lastNotice: {
    key: CatalogKey | null;
    params?: Record<string, string | number>;
    kind: QrNoticeKind;
  } | null = null;

  const clearCopied = (): void => {
    copiedIndex = null;
    if (copiedTimer !== 0) {
      window.clearTimeout(copiedTimer);
      copiedTimer = 0;
    }
  };

  const emitChange = (): void => {
    options.onChange?.();
  };

  const setNoticeKey = (
    key: CatalogKey,
    kind: QrNoticeKind,
    params?: Record<string, string | number>,
  ): void => {
    lastNotice = { key, kind, params };
    notice(t(key, params), kind);
  };

  const setNoticeText = (text: string, kind: QrNoticeKind): void => {
    lastNotice = { key: null, kind };
    notice(text, kind);
  };

  const readCodes = (result: QrPayload): QrCode[] => {
    if (!Array.isArray(result.codes)) {
      return [];
    }
    const parsed: QrCode[] = [];
    for (const code of result.codes) {
      if (!code || typeof code.text !== "string") {
        continue;
      }
      const x = Number(code.x);
      const y = Number(code.y);
      const width = Number(code.width);
      const height = Number(code.height);
      if (![x, y, width, height].every((value) => Number.isFinite(value))) {
        continue;
      }
      parsed.push({ text: code.text, x, y, width, height });
    }
    return parsed;
  };

  // 「复制」比「已复制」窄，变绿时字还会加粗。先按较宽的那句留宽，成功时按钮不再变宽。
  const qrCopyWidthCache = new Map<string, string>();
  const reserveQrCopyWidth = (button: HTMLButtonElement): void => {
    const parent = button.parentElement;
    if (!parent || parent.getClientRects().length === 0) {
      return;
    }
    const primary = button.classList.contains("primary");
    const key = `${currentLanguage()}:${primary ? "primary" : "quiet"}`;
    let width = qrCopyWidthCache.get(key);
    if (!width) {
      const probe = button.cloneNode(false);
      if (!(probe instanceof HTMLButtonElement)) {
        return;
      }
      probe.className = primary ? "qr-item-copy primary" : "qr-item-copy";
      probe.style.position = "absolute";
      probe.style.visibility = "hidden";
      probe.style.pointerEvents = "none";
      probe.style.width = "auto";
      probe.style.minWidth = "0";
      probe.style.whiteSpace = "nowrap";
      parent.append(probe);
      let widest = 0;
      probe.textContent = t("preview.qr_panel.copy");
      widest = Math.max(widest, probe.getBoundingClientRect().width);
      probe.classList.add("is-copied");
      probe.textContent = t("preview.qr_panel.copied");
      widest = Math.max(widest, probe.getBoundingClientRect().width);
      probe.remove();
      if (widest <= 0) {
        return;
      }
      width = `${Math.ceil(widest)}px`;
      qrCopyWidthCache.set(key, width);
    }
    if (button.style.minWidth !== width) {
      button.style.minWidth = width;
    }
  };

  const renderPanel = (): void => {
    const visible = active && !panelDismissed && contents !== null && contents.length > 0;
    panel.hidden = !visible;
    if (!visible) {
      copyFocusIndex = null;
      return;
    }
    // 重建前记下键盘焦点所在的复制按钮;焦点已移到面板外其他控件(或 body 之外的
    // 任何元素)时不争夺。busy 置灰的按钮不可聚焦,保留序号待下次重建恢复——
    // copy() 结束的重建会重新启用按钮,把焦点放回原处。
    const activeElement = document.activeElement;
    if (activeElement instanceof HTMLButtonElement) {
      const index = Number(activeElement.dataset.qrCopy);
      copyFocusIndex = Number.isInteger(index) ? index : null;
    } else if (copyFocusIndex !== null && activeElement !== document.body) {
      copyFocusIndex = null;
    }
    list.replaceChildren();
    const items = contents ?? [];
    const sole = items.length === 1;
    items.forEach((content, index) => {
      const item = document.createElement("div");
      item.className = "qr-item";
      // R2:list/listitem 语义:辅助技术可逐条枚举结果并报出总数。
      item.setAttribute("role", "listitem");
      const text = document.createElement("div");
      text.className = "qr-item-text";
      text.textContent = content;
      const copy = document.createElement("button");
      copy.type = "button";
      const copied = copiedIndex === index;
      copy.className = sole ? "qr-item-copy primary" : "qr-item-copy";
      if (copied) {
        copy.classList.add("is-copied");
      }
      copy.dataset.qrCopy = String(index);
      copy.textContent = copied ? t("preview.qr_panel.copied") : t("preview.qr_panel.copy");
      // 可见文本同为「复制」,读屏靠带序号的 aria-label 区分对应哪条内容。
      copy.setAttribute(
        "aria-label",
        `${copied ? t("preview.qr_panel.copied") : t("preview.qr_panel.copy")} ${index + 1}`,
      );
      copy.disabled = busy;
      item.append(text, copy);
      list.append(item);
      reserveQrCopyWidth(copy);
    });
    if (copyFocusIndex !== null) {
      const target = list.querySelector<HTMLButtonElement>(`[data-qr-copy="${copyFocusIndex}"]`);
      if (!target) {
        copyFocusIndex = null;
      } else if (!target.disabled) {
        target.focus();
        copyFocusIndex = null;
      }
    }
  };

  const runRecognition = async (): Promise<void> => {
    const token = ++generation;
    running = true;
    contents = null;
    codes = [];
    panelDismissed = false;
    clearCopied();
    renderPanel();
    setNoticeKey("preview.note.qr_running", "progress");
    emitChange();
    try {
      const result = await invoke<QrPayload>("recognize_qr_preview");
      if (token !== generation) {
        return;
      }
      const texts = Array.isArray(result?.contents)
        ? result.contents.filter(
            (item): item is string => typeof item === "string" && item.length > 0,
          )
        : [];
      if (texts.length > 0) {
        contents = texts;
        codes = readCodes(result);
        renderPanel();
        if (active) {
          setNoticeKey("preview.note.qr_hint", "hint");
        }
      } else {
        contents = null;
        codes = [];
        panelDismissed = true;
        renderPanel();
        if (active) {
          setNoticeKey("preview.note.qr_none", "empty");
        }
      }
    } catch (error) {
      if (token !== generation) {
        return;
      }
      contents = null;
      codes = [];
      panelDismissed = true;
      renderPanel();
      if (active) {
        setNoticeText(invokeError(error, t("preview.error.qr_fallback")), "error");
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
    // 缓存重开(与取字模型同语义):帧是冻结的,已识别内容不会过期,
    // 带 `contents` 直接重开面板,不整段重新识别;无缓存才发起识别。
    if (contents !== null) {
      panelDismissed = false;
      renderPanel();
      setNoticeKey("preview.note.qr_hint", "hint");
    } else {
      void runRecognition();
    }
    emitChange();
  };

  const deactivate = (): boolean => {
    if (!active) {
      return false;
    }
    active = false;
    renderPanel();
    emitChange();
    return true;
  };

  const reset = (): void => {
    generation += 1;
    running = false;
    active = false;
    contents = null;
    codes = [];
    busy = false;
    panelDismissed = true;
    lastNotice = null;
    clearCopied();
    renderPanel();
    emitChange();
  };

  // 关闭面板即退出激活态(与取字模型一致):宿主工具条高亮同步熄灭,
  // 再点按钮走 activate 的缓存重开,不留「亮着却点不动」的死按钮。
  const closePanel = (): void => {
    deactivate();
  };

  const copy = async (index: number): Promise<void> => {
    if (busy || running) {
      return;
    }
    const content = contents?.[index];
    if (typeof content !== "string") {
      setNoticeKey("preview.error.qr_fallback", "error");
      return;
    }
    busy = true;
    renderPanel();
    try {
      await invoke<string>("copy_qr_content", { text: content });
      setNoticeKey("preview.note.qr_copied", "success");
      copiedIndex = index;
      if (copiedTimer !== 0) {
        window.clearTimeout(copiedTimer);
      }
      copiedTimer = window.setTimeout(() => {
        copiedTimer = 0;
        if (copiedIndex === index) {
          copiedIndex = null;
          renderPanel();
        }
      }, 1600);
    } catch (error) {
      setNoticeText(invokeError(error, t("preview.error.qr_fallback")), "error");
    } finally {
      busy = false;
      renderPanel();
      emitChange();
    }
  };

  panel.addEventListener("click", (event) => {
    const target = event.target;
    const button = target instanceof Element ? target.closest("[data-qr-action], [data-qr-copy]") : null;
    if (!(button instanceof HTMLButtonElement) || button.hidden) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    if (button.dataset.qrAction === "close-panel") {
      closePanel();
      return;
    }
    const index = Number(button.dataset.qrCopy);
    if (Number.isInteger(index)) {
      void copy(index);
    }
  });

  void document.fonts.ready.then(() => {
    if (!panel.isConnected) {
      return;
    }
    qrCopyWidthCache.clear();
    renderPanel();
  });

  const refreshLabels = (): void => {
    qrCopyWidthCache.clear();
    panel.setAttribute("aria-label", t("preview.qr_panel.title"));
    renderPanel();
    if (lastNotice?.key) {
      notice(t(lastNotice.key, lastNotice.params), lastNotice.kind);
    }
  };

  return {
    get active() {
      return active;
    },
    contents: () => contents,
    regions: () =>
      codes
        .filter((code) => code.width > 0 && code.height > 0)
        .map((code) => ({ x: code.x, y: code.y, width: code.width, height: code.height })),
    isRunning: () => running,
    activate,
    deactivate,
    reset,
    closePanel,
    copy,
    refreshLabels,
  };
}
