import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { localeTag, t, type CatalogKey } from "../i18n";
import "./history.css";

interface HistoryEntryView {
  id: string;
  createdAt: number;
  width: number;
  height: number;
  thumbMissing: boolean;
  imageMissing: boolean;
}

interface HistoryListPayload {
  entries: HistoryEntryView[];
  notice: string | null;
}

// 历史视图:按时间展示本机保存的截图缩略图,每条可重新复制、重新贴图或
// 删除,也可清空全部。缩略图经 Rust 命令拉取为二进制,转 blob URL 显示;
// 索引损坏或缩略图/原图缺失时给出可理解状态,列表仍可用。
export function mountHistory(root: HTMLElement): () => void {
  root.className = "history-root";
  root.innerHTML = `
    <div class="shell">
      <header class="titlebar" data-tauri-drag-region>
        <div class="brand" data-tauri-drag-region>
          <span class="mark" aria-hidden="true"></span>
          <span class="name" data-i18n="history.title">历史记录</span>
        </div>
        <div class="history-toolbar">
          <button type="button" class="choice" data-action="clear" data-i18n="history.clear">清空全部</button>
          <button type="button" class="icon-btn" data-action="close" data-i18n-aria-label="history.close" aria-label="关闭">×</button>
        </div>
      </header>
      <main class="content history-content">
        <p class="notice" data-notice role="alert" hidden></p>
        <p class="history-status" data-status role="status" hidden></p>
        <div class="history-confirm" data-confirm role="alertdialog" data-i18n-aria-label="history.confirm_group" aria-label="确认操作" hidden>
          <p class="history-confirm-text" data-confirm-text></p>
          <div class="history-confirm-actions">
            <button type="button" class="choice history-confirm-accept" data-confirm-accept data-i18n="history.accept">确认</button>
            <button type="button" class="choice" data-confirm-cancel data-i18n="history.cancel">取消</button>
          </div>
        </div>
        <div class="history-list" data-list></div>
        <p class="history-empty" data-empty hidden data-i18n="history.empty">暂无历史记录。截图完成后会自动出现在这里。</p>
      </main>
    </div>
  `;

  const noticeEl = root.querySelector("[data-notice]");
  const statusEl = root.querySelector("[data-status]");
  const confirmEl = root.querySelector("[data-confirm]");
  const confirmTextEl = root.querySelector("[data-confirm-text]");
  const confirmAcceptEl = root.querySelector("[data-confirm-accept]");
  const confirmCancelEl = root.querySelector("[data-confirm-cancel]");
  const listEl = root.querySelector("[data-list]");
  const emptyEl = root.querySelector("[data-empty]");
  const clearEl = root.querySelector("[data-action=clear]");
  const closeEl = root.querySelector("[data-action=close]");
  if (
    !(noticeEl instanceof HTMLElement) ||
    !(statusEl instanceof HTMLElement) ||
    !(confirmEl instanceof HTMLElement) ||
    !(confirmTextEl instanceof HTMLElement) ||
    !(confirmAcceptEl instanceof HTMLButtonElement) ||
    !(confirmCancelEl instanceof HTMLButtonElement) ||
    !(listEl instanceof HTMLElement) ||
    !(emptyEl instanceof HTMLElement) ||
    !(clearEl instanceof HTMLButtonElement) ||
    !(closeEl instanceof HTMLButtonElement)
  ) {
    return () => undefined;
  }

  let busy = false;
  let lastPayload: HistoryListPayload | null = null;
  let statusState: { key: CatalogKey | null; text: string; isError: boolean } = {
    key: null,
    text: "",
    isError: false,
  };
  // macOS 的 WKWebView 不提供 window.confirm(wry 未实现该面板,恒按取消处理),
  // 确认一律走窗口内确认条,三平台行为一致,也不新增插件与权限依赖。
  type PendingConfirm = { kind: "delete"; id: string } | { kind: "clear" };
  let pendingConfirm: PendingConfirm | null = null;

  const confirmMessage = (pending: PendingConfirm): string =>
    pending.kind === "delete" ? t("history.confirm_delete") : t("history.confirm_clear");
  const confirmAcceptLabel = (pending: PendingConfirm): string =>
    pending.kind === "delete" ? t("history.confirm_delete_accept") : t("history.confirm_clear_accept");

  const hideConfirm = (): void => {
    pendingConfirm = null;
    confirmEl.hidden = true;
    confirmTextEl.textContent = "";
    confirmAcceptEl.textContent = t("history.accept");
  };

  const showConfirm = (pending: PendingConfirm): void => {
    pendingConfirm = pending;
    confirmTextEl.textContent = confirmMessage(pending);
    confirmAcceptEl.textContent = confirmAcceptLabel(pending);
    confirmEl.hidden = false;
    // 初始焦点落在取消,键盘 Enter 不会直接触发破坏性操作。
    confirmCancelEl.focus();
  };

  const renderStatus = (): void => {
    const message = statusState.key ? t(statusState.key) : statusState.text;
    statusEl.hidden = message.length === 0;
    statusEl.textContent = message;
    statusEl.classList.toggle("is-error", statusState.isError);
  };

  const setStatusKey = (key: CatalogKey, isError = false): void => {
    statusState = { key, text: "", isError };
    renderStatus();
  };

  const setStatusText = (text: string, isError = false): void => {
    statusState = { key: null, text, isError };
    renderStatus();
  };

  const errorMessage = (error: unknown): string =>
    error instanceof Error ? error.message : String(error);

  const formatTime = (createdAt: number): string => {
    const date = new Date(createdAt);
    if (Number.isNaN(date.getTime())) {
      return t("history.unknown_time");
    }
    return date.toLocaleString(localeTag());
  };

  const loadThumbnail = (id: string, holder: HTMLElement): void => {
    void invoke<ArrayBuffer>("get_history_thumbnail", { id })
      .then((bytes) => {
        if (bytes.byteLength === 0) {
          throw new Error("empty");
        }
        const url = URL.createObjectURL(new Blob([bytes], { type: "image/png" }));
        const image = document.createElement("img");
        image.alt = "";
        image.draggable = false;
        image.onload = () => URL.revokeObjectURL(url);
        image.onerror = () => URL.revokeObjectURL(url);
        image.src = url;
        holder.replaceChildren(image);
      })
      .catch(() => {
        holder.classList.add("missing");
        holder.textContent = t("history.thumb_missing");
      });
  };

  const entryRow = (entry: HistoryEntryView): HTMLElement => {
    const row = document.createElement("article");
    row.className = "history-item";
    row.dataset.entryId = entry.id;

    const holder = document.createElement("div");
    holder.className = "history-thumb";
    if (entry.thumbMissing) {
      holder.classList.add("missing");
      holder.textContent = t("history.thumb_missing");
    } else {
      loadThumbnail(entry.id, holder);
    }

    const meta = document.createElement("div");
    meta.className = "history-meta";
    const time = document.createElement("div");
    time.className = "history-time";
    time.textContent = formatTime(entry.createdAt);
    const size = document.createElement("div");
    size.className = "history-size";
    size.textContent = `${entry.width} × ${entry.height}`;
    meta.append(time, size);
    if (entry.imageMissing) {
      const missing = document.createElement("div");
      missing.className = "history-missing-note";
      missing.textContent = t("history.image_missing");
      meta.append(missing);
    }

    const actions = document.createElement("div");
    actions.className = "history-actions";
    const actionLabels: Array<[string, CatalogKey]> = [
      ["copy", "history.copy"],
      ["pin", "history.pin"],
      ["delete", "history.delete"],
    ];
    for (const [action, labelKey] of actionLabels) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = action === "delete" ? "choice history-delete" : "choice";
      button.dataset.entryAction = action;
      button.textContent = t(labelKey);
      if (entry.imageMissing && action !== "delete") {
        button.disabled = true;
        button.title = t("history.image_missing_title");
      }
      actions.append(button);
    }

    row.append(holder, meta, actions);
    return row;
  };

  const render = (payload: HistoryListPayload): void => {
    lastPayload = payload;
    noticeEl.hidden = !payload.notice;
    noticeEl.textContent = payload.notice ?? "";
    listEl.replaceChildren();
    for (const entry of payload.entries) {
      listEl.append(entryRow(entry));
    }
    emptyEl.hidden = payload.entries.length > 0;
  };

  const refresh = async (): Promise<void> => {
    try {
      render(await invoke<HistoryListPayload>("get_history"));
    } catch (error) {
      setStatusText(errorMessage(error), true);
    }
  };

  const performDelete = async (id: string): Promise<void> => {
    busy = true;
    try {
      render(await invoke<HistoryListPayload>("delete_history_entry", { id }));
      setStatusKey("history.deleted");
    } catch (error) {
      setStatusText(errorMessage(error), true);
    } finally {
      busy = false;
    }
  };

  const performClear = async (): Promise<void> => {
    busy = true;
    statusState = { key: null, text: "", isError: false };
    renderStatus();
    try {
      render(await invoke<HistoryListPayload>("clear_history"));
      setStatusKey("history.cleared");
    } catch (error) {
      setStatusText(errorMessage(error), true);
    } finally {
      busy = false;
    }
  };

  const runAction = async (action: string, id: string): Promise<void> => {
    if (busy) {
      return;
    }
    if (action === "delete") {
      // 删除需二次确认:显示窗口内确认条,确认后再执行,不依赖 WebView 对话框。
      showConfirm({ kind: "delete", id });
      return;
    }
    hideConfirm();
    busy = true;
    try {
      if (action === "copy") {
        await invoke("copy_history_entry", { id });
        setStatusKey("history.copied");
      } else if (action === "pin") {
        await invoke("pin_history_entry", { id });
        setStatusKey("history.pinned");
      }
    } catch (error) {
      setStatusText(errorMessage(error), true);
    } finally {
      busy = false;
    }
  };

  listEl.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element)) {
      return;
    }
    const button = target.closest("[data-entry-action]");
    if (!(button instanceof HTMLButtonElement) || button.disabled) {
      return;
    }
    const row = button.closest("[data-entry-id]");
    const id = row instanceof HTMLElement ? row.dataset.entryId : undefined;
    const action = button.dataset.entryAction;
    if (!id || !action) {
      return;
    }
    void runAction(action, id);
  });

  clearEl.addEventListener("click", () => {
    if (busy) {
      return;
    }
    showConfirm({ kind: "clear" });
  });

  confirmAcceptEl.addEventListener("click", () => {
    if (busy || !pendingConfirm) {
      return;
    }
    const pending = pendingConfirm;
    hideConfirm();
    if (pending.kind === "delete") {
      void performDelete(pending.id);
    } else {
      void performClear();
    }
  });

  confirmCancelEl.addEventListener("click", () => {
    hideConfirm();
  });

  // Esc 等同取消,不执行删除。
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && pendingConfirm && !busy) {
      hideConfirm();
    }
  });

  closeEl.addEventListener("click", () => {
    void getCurrentWindow().close();
  });

  // 窗口被截取流程隐藏后再次打开时,Rust 发来刷新信号,列表不会停留在旧数据。
  void listen("history-refresh", () => {
    void refresh();
  });

  void refresh();

  // 语言切换:重建动态文案(状态、确认条、列表时间/动作),静态标签由 main 应用。
  return () => {
    renderStatus();
    if (lastPayload) {
      render(lastPayload);
    }
    if (pendingConfirm) {
      confirmTextEl.textContent = confirmMessage(pendingConfirm);
      confirmAcceptEl.textContent = confirmAcceptLabel(pendingConfirm);
    }
  };
}
