import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
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
export function mountHistory(root: HTMLElement): void {
  root.className = "history-root";
  root.innerHTML = `
    <div class="shell">
      <header class="titlebar" data-tauri-drag-region>
        <div class="brand" data-tauri-drag-region>
          <span class="mark" aria-hidden="true"></span>
          <span class="name">历史记录</span>
        </div>
        <div class="history-toolbar">
          <button type="button" class="choice" data-action="clear">清空全部</button>
          <button type="button" class="icon-btn" data-action="close" aria-label="关闭">×</button>
        </div>
      </header>
      <main class="content history-content">
        <p class="notice" data-notice role="alert" hidden></p>
        <p class="history-status" data-status role="status" hidden></p>
        <div class="history-list" data-list></div>
        <p class="history-empty" data-empty hidden>暂无历史记录。截图完成后会自动出现在这里。</p>
      </main>
    </div>
  `;

  const noticeEl = root.querySelector("[data-notice]");
  const statusEl = root.querySelector("[data-status]");
  const listEl = root.querySelector("[data-list]");
  const emptyEl = root.querySelector("[data-empty]");
  const clearEl = root.querySelector("[data-action=clear]");
  const closeEl = root.querySelector("[data-action=close]");
  if (
    !(noticeEl instanceof HTMLElement) ||
    !(statusEl instanceof HTMLElement) ||
    !(listEl instanceof HTMLElement) ||
    !(emptyEl instanceof HTMLElement) ||
    !(clearEl instanceof HTMLButtonElement) ||
    !(closeEl instanceof HTMLButtonElement)
  ) {
    return;
  }

  let busy = false;

  const setStatus = (message: string, isError = false): void => {
    statusEl.hidden = message.length === 0;
    statusEl.textContent = message;
    statusEl.classList.toggle("is-error", isError);
  };

  const errorMessage = (error: unknown): string =>
    error instanceof Error ? error.message : String(error);

  const formatTime = (createdAt: number): string => {
    const date = new Date(createdAt);
    if (Number.isNaN(date.getTime())) {
      return "未知时间";
    }
    return date.toLocaleString();
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
        holder.textContent = "缩略图缺失";
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
      holder.textContent = "缩略图缺失";
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
      missing.textContent = "原始图片缺失，无法复制或贴图";
      meta.append(missing);
    }

    const actions = document.createElement("div");
    actions.className = "history-actions";
    for (const [action, label] of [
      ["copy", "复制"],
      ["pin", "贴图"],
      ["delete", "删除"],
    ] as const) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = action === "delete" ? "choice history-delete" : "choice";
      button.dataset.entryAction = action;
      button.textContent = label;
      if (entry.imageMissing && action !== "delete") {
        button.disabled = true;
        button.title = "原始图片缺失";
      }
      actions.append(button);
    }

    row.append(holder, meta, actions);
    return row;
  };

  const render = (payload: HistoryListPayload): void => {
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
      setStatus(errorMessage(error), true);
    }
  };

  const runAction = async (action: string, id: string): Promise<void> => {
    if (busy) {
      return;
    }
    if (action === "delete" && !window.confirm("删除这条历史记录？")) {
      return;
    }
    busy = true;
    try {
      if (action === "copy") {
        await invoke("copy_history_entry", { id });
        setStatus("已复制到剪贴板。");
      } else if (action === "pin") {
        await invoke("pin_history_entry", { id });
        setStatus("已贴图。");
      } else if (action === "delete") {
        render(await invoke<HistoryListPayload>("delete_history_entry", { id }));
        setStatus("已删除。");
      }
    } catch (error) {
      setStatus(errorMessage(error), true);
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
    if (busy || !window.confirm("清空全部历史记录？此操作不可撤销。")) {
      return;
    }
    busy = true;
    setStatus("");
    void invoke<HistoryListPayload>("clear_history")
      .then((payload) => {
        render(payload);
        setStatus("已清空。");
      })
      .catch((error) => setStatus(errorMessage(error), true))
      .finally(() => {
        busy = false;
      });
  });

  closeEl.addEventListener("click", () => {
    void getCurrentWindow().close();
  });

  // 窗口被截取流程隐藏后再次打开时,Rust 发来刷新信号,列表不会停留在旧数据。
  void listen("history-refresh", () => {
    void refresh();
  });

  void refresh();
}
