import { invoke } from "@tauri-apps/api/core";
import { t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import "./qr.css";

// R4:本地二维码识别结果模型。预览与冻结帧工作区覆盖层共用同一实现:
// 识别只读当前冻结/预览帧,内容展示在结果面板,复制必须由用户显式点击;
// 无码或失败只给本地化说明,不写剪贴板,也从不打开任何链接。

export interface QrPayload {
  contents: string[];
}

export type QrNoticeKind = "progress" | "hint" | "success" | "error";

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
    <div class="qr-panel-list" data-qr-list></div>
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
  let running = false;
  let generation = 0;
  let busy = false;
  let panelDismissed = true;
  let lastNotice: {
    key: CatalogKey | null;
    params?: Record<string, string | number>;
    kind: QrNoticeKind;
  } | null = null;

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

  const renderPanel = (): void => {
    const visible = active && !panelDismissed && contents !== null && contents.length > 0;
    panel.hidden = !visible;
    if (!visible) {
      return;
    }
    list.replaceChildren();
    contents?.forEach((content, index) => {
      const item = document.createElement("div");
      item.className = "qr-item";
      const text = document.createElement("div");
      text.className = "qr-item-text";
      text.textContent = content;
      const copy = document.createElement("button");
      copy.type = "button";
      copy.className = "qr-item-copy";
      copy.dataset.qrCopy = String(index);
      copy.textContent = t("preview.qr_panel.copy");
      copy.disabled = busy;
      item.append(text, copy);
      list.append(item);
    });
  };

  const runRecognition = async (): Promise<void> => {
    const token = ++generation;
    running = true;
    contents = null;
    panelDismissed = false;
    renderPanel();
    setNoticeKey("preview.note.qr_running", "progress");
    emitChange();
    try {
      const result = await invoke<QrPayload>("recognize_qr_preview");
      if (token !== generation) {
        return;
      }
      if (Array.isArray(result?.contents) && result.contents.length > 0) {
        contents = result.contents;
        renderPanel();
        if (active) {
          setNoticeKey("preview.note.qr_hint", "hint");
        }
      } else {
        contents = null;
        panelDismissed = true;
        renderPanel();
        if (active) {
          setNoticeKey("preview.error.qr_fallback", "error");
        }
      }
    } catch (error) {
      if (token !== generation) {
        return;
      }
      contents = null;
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
    if (contents !== null && !panelDismissed) {
      renderPanel();
      setNoticeKey("preview.note.qr_hint", "hint");
      emitChange();
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
    busy = false;
    panelDismissed = true;
    lastNotice = null;
    renderPanel();
    emitChange();
  };

  const closePanel = (): void => {
    panelDismissed = true;
    renderPanel();
    emitChange();
  };

  const copy = async (index: number): Promise<void> => {
    if (busy || running) {
      setNoticeKey("preview.note.busy", "hint");
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

  const refreshLabels = (): void => {
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
    isRunning: () => running,
    activate,
    deactivate,
    reset,
    closePanel,
    copy,
    refreshLabels,
  };
}
