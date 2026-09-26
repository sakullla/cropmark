import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { localeTag, t, type CatalogKey } from "../i18n";
import { icons } from "../icons";
import "./history.css";

interface HistoryEntryView {
  id: string;
  createdAt: number;
  width: number;
  height: number;
  thumbMissing: boolean;
  imageMissing: boolean;
  mode: string | null;
  favorite: boolean;
  note: string;
}

interface HistoryListPayload {
  entries: HistoryEntryView[];
  notice: string | null;
  toolsEnabled: boolean;
}

type TimeRange = "all" | "today" | "7d" | "30d";
type ModeFilter = "all" | "region" | "window" | "fullscreen" | "long";

const DAY_MS = 24 * 60 * 60 * 1000;

const STAR_ICON = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linejoin="round" aria-hidden="true"><path d="M12 3.6 14.7 9.1 20.7 9.9 16.3 14.1 17.4 20.1 12 17.2 6.6 20.1 7.7 14.1 3.3 9.9 9.3 9.1Z"/></svg>`;

function startOfLocalDay(ms: number): number {
  const date = new Date(ms);
  return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
}

/** 今天按本地日历日;最近 7/30 天按滚动窗口,含当前时刻。 */
function matchesTime(createdAt: number, range: TimeRange, now: number): boolean {
  if (range === "all") {
    return true;
  }
  if (!Number.isFinite(createdAt)) {
    return false;
  }
  if (range === "today") {
    return startOfLocalDay(createdAt) === startOfLocalDay(now);
  }
  const days = range === "7d" ? 7 : 30;
  return createdAt >= now - days * DAY_MS;
}

/** 无模式的旧记录只在「全部模式」下出现。 */
function matchesMode(mode: string | null, filter: ModeFilter): boolean {
  return filter === "all" || mode === filter;
}

/** 关键词只匹配备注,大小写不敏感;空白关键词不筛选。 */
function matchesNote(note: string, query: string): boolean {
  const needle = query.trim().toLocaleLowerCase();
  return needle.length === 0 || note.toLocaleLowerCase().includes(needle);
}

function visibleEntries(
  entries: HistoryEntryView[],
  toolsEnabled: boolean,
  timeRange: TimeRange,
  modeFilter: ModeFilter,
  noteQuery: string,
  now = Date.now(),
): HistoryEntryView[] {
  if (!toolsEnabled) {
    return entries.slice();
  }
  const matched = entries.filter(
    (entry) =>
      matchesTime(entry.createdAt, timeRange, now) &&
      matchesMode(entry.mode, modeFilter) &&
      matchesNote(entry.note, noteQuery),
  );
  matched.sort((left, right) => {
    if (left.favorite !== right.favorite) {
      return left.favorite ? -1 : 1;
    }
    return right.createdAt - left.createdAt;
  });
  return matched;
}

function modeLabelKey(mode: string): CatalogKey | null {
  switch (mode) {
    case "region":
      return "history.mode.region";
    case "window":
      return "history.mode.window";
    case "fullscreen":
      return "history.mode.fullscreen";
    case "long":
      return "history.mode.long";
    default:
      return null;
  }
}

function asTimeRange(value: string): TimeRange {
  if (value === "today" || value === "7d" || value === "30d") {
    return value;
  }
  return "all";
}

function asModeFilter(value: string): ModeFilter {
  if (value === "region" || value === "window" || value === "fullscreen" || value === "long") {
    return value;
  }
  return "all";
}

// 历史视图:按时间展示本机保存的截图缩略图,每条可再编辑、复制、贴图或
// 删除,也可清空全部。开启「历史检索与收藏」后可按时间/模式筛选、收藏置顶
// 并检索备注;关闭开关只恢复原来的时间列表,索引中的模式、收藏和备注仍保留。
export function mountHistory(root: HTMLElement): () => void {
  root.className = "history-root";
  root.innerHTML = `
    <div class="shell">
      <header class="titlebar" data-tauri-drag-region>
        <div class="brand" data-tauri-drag-region>
          <span class="mark" aria-hidden="true"></span>
          <span class="name" data-i18n="history.title">历史记录</span>
          <span class="history-count" data-count hidden></span>
        </div>
        <div class="history-toolbar">
          <button type="button" class="history-clear" data-action="clear" data-i18n="history.clear">清空全部</button>
          <button type="button" class="icon-btn" data-action="close" data-i18n-aria-label="history.close" aria-label="关闭">${icons.close}</button>
        </div>
      </header>
      <main class="content history-content">
        <div class="history-filters" data-filters role="group" data-i18n-aria-label="history.filter.group" aria-label="筛选历史" hidden>
          <select class="history-select" data-filter="time" data-i18n-aria-label="history.filter.time_label" aria-label="时间范围">
            <option value="all" data-i18n="history.filter.time_all">全部时间</option>
            <option value="today" data-i18n="history.filter.time_today">今天</option>
            <option value="7d" data-i18n="history.filter.time_7d">最近 7 天</option>
            <option value="30d" data-i18n="history.filter.time_30d">最近 30 天</option>
          </select>
          <select class="history-select" data-filter="mode" data-i18n-aria-label="history.filter.mode_label" aria-label="采集模式">
            <option value="all" data-i18n="history.filter.mode_all">全部模式</option>
            <option value="region" data-i18n="history.filter.mode_region">区域</option>
            <option value="window" data-i18n="history.filter.mode_window">窗口</option>
            <option value="fullscreen" data-i18n="history.filter.mode_fullscreen">全屏</option>
            <option value="long" data-i18n="history.filter.mode_long">长截图</option>
          </select>
          <input class="history-search" data-filter="note" type="search" autocomplete="off" data-i18n-aria-label="history.filter.note_label" data-i18n-placeholder="history.filter.note_placeholder" aria-label="搜索备注" placeholder="搜索备注" />
          <button type="button" class="history-filter-clear" data-action="clear-filters" data-i18n="history.filter.clear" disabled>清除筛选</button>
        </div>
        <p class="notice" data-notice role="alert" hidden></p>
        <p class="history-status" data-status role="status" hidden></p>
        <div class="history-confirm" data-confirm role="alertdialog" data-i18n-aria-label="history.confirm_group" aria-label="确认操作" hidden>
          <p class="history-confirm-text" data-confirm-text></p>
          <div class="history-confirm-actions">
            <button type="button" class="history-btn history-btn-danger history-confirm-accept" data-confirm-accept data-i18n="history.accept">确认</button>
            <button type="button" class="history-btn history-btn-quiet" data-confirm-cancel data-i18n="history.cancel">取消</button>
          </div>
        </div>
        <div class="history-list" data-list></div>
        <p class="history-loading" data-loading role="status"><span class="progress" aria-hidden="true"></span><span data-i18n="history.loading">正在加载历史记录…</span></p>
        <p class="history-empty" data-empty data-i18n="history.empty" hidden>暂无历史记录。截图完成后会自动出现在这里。</p>
      </main>
    </div>
  `;

  const filtersEl = root.querySelector("[data-filters]");
  const timeEl = root.querySelector("[data-filter=time]");
  const modeEl = root.querySelector("[data-filter=mode]");
  const searchEl = root.querySelector("[data-filter=note]");
  const clearFiltersEl = root.querySelector("[data-action=clear-filters]");
  const noticeEl = root.querySelector("[data-notice]");
  const statusEl = root.querySelector("[data-status]");
  const confirmEl = root.querySelector("[data-confirm]");
  const confirmTextEl = root.querySelector("[data-confirm-text]");
  const confirmAcceptEl = root.querySelector("[data-confirm-accept]");
  const confirmCancelEl = root.querySelector("[data-confirm-cancel]");
  const listEl = root.querySelector("[data-list]");
  const countEl = root.querySelector("[data-count]");
  const emptyEl = root.querySelector("[data-empty]");
  const loadingEl = root.querySelector("[data-loading]");
  const clearEl = root.querySelector("[data-action=clear]");
  const closeEl = root.querySelector("[data-action=close]");
  if (
    !(filtersEl instanceof HTMLElement) ||
    !(timeEl instanceof HTMLSelectElement) ||
    !(modeEl instanceof HTMLSelectElement) ||
    !(searchEl instanceof HTMLInputElement) ||
    !(clearFiltersEl instanceof HTMLButtonElement) ||
    !(noticeEl instanceof HTMLElement) ||
    !(statusEl instanceof HTMLElement) ||
    !(confirmEl instanceof HTMLElement) ||
    !(confirmTextEl instanceof HTMLElement) ||
    !(confirmAcceptEl instanceof HTMLButtonElement) ||
    !(confirmCancelEl instanceof HTMLButtonElement) ||
    !(listEl instanceof HTMLElement) ||
    !(countEl instanceof HTMLElement) ||
    !(emptyEl instanceof HTMLElement) ||
    !(loadingEl instanceof HTMLElement) ||
    !(clearEl instanceof HTMLButtonElement) ||
    !(closeEl instanceof HTMLButtonElement)
  ) {
    return () => undefined;
  }

  let busy = false;
  let applyingDom = false;
  let lastPayload: HistoryListPayload | null = null;
  let timeRange: TimeRange = "all";
  let modeFilter: ModeFilter = "all";
  let noteQuery = "";
  const noteDrafts = new Map<string, string>();
  let statusState: { key: CatalogKey | null; text: string; isError: boolean } = {
    key: null,
    text: "",
    isError: false,
  };
  // macOS 的 WKWebView 不提供 window.confirm(wry 未实现该面板,恒按取消处理),
  // 确认一律走窗口内确认条,三平台行为一致,也不新增插件与权限依赖。
  type PendingConfirm = { kind: "delete"; id: string } | { kind: "clear" };
  let pendingConfirm: PendingConfirm | null = null;
  // 打开确认条的触发按钮:确认条关闭后焦点还原到它(键盘用户不丢上下文)。
  let confirmTrigger: HTMLElement | null = null;

  const confirmMessage = (pending: PendingConfirm): string =>
    pending.kind === "delete" ? t("history.confirm_delete") : t("history.confirm_clear");
  const confirmAcceptLabel = (pending: PendingConfirm): string =>
    pending.kind === "delete" ? t("history.confirm_delete_accept") : t("history.confirm_clear_accept");

  const filtersActive = (): boolean =>
    timeRange !== "all" || modeFilter !== "all" || noteQuery.trim().length > 0;

  // busy 期间操作按钮给禁用可视态;因图像缺失而常驻禁用的按钮保持禁用。
  const syncBusy = (): void => {
    clearEl.disabled = busy;
    confirmAcceptEl.disabled = busy;
    listEl
      .querySelectorAll<HTMLButtonElement>("[data-entry-action]")
      .forEach((button) => {
        button.disabled = busy || button.dataset.missingDisabled === "true";
      });
  };

  const setBusy = (value: boolean): void => {
    busy = value;
    syncBusy();
  };

  const hideConfirm = (): void => {
    pendingConfirm = null;
    confirmEl.hidden = true;
    confirmTextEl.textContent = "";
    confirmAcceptEl.textContent = t("history.accept");
    const trigger = confirmTrigger;
    confirmTrigger = null;
    // 仅当焦点还在确认条内时才还原,不打断用户已移走的焦点。
    if (trigger && trigger.isConnected && confirmEl.contains(document.activeElement)) {
      trigger.focus();
    }
  };

  const showConfirm = (pending: PendingConfirm, trigger?: HTMLElement): void => {
    pendingConfirm = pending;
    confirmTrigger =
      trigger ??
      (document.activeElement instanceof HTMLElement ? document.activeElement : null);
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

  const formatTime = (createdAt: number): { label: string; title: string } => {
    const date = new Date(createdAt);
    if (Number.isNaN(date.getTime())) {
      const unknown = t("history.unknown_time");
      return { label: unknown, title: unknown };
    }
    const title = date.toLocaleString(localeTag());
    const clock = new Intl.DateTimeFormat(localeTag(), {
      hour: "2-digit",
      minute: "2-digit",
    }).format(date);
    const dayStart = (value: Date): number =>
      new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime();
    const day = dayStart(date);
    const now = new Date();
    if (day === dayStart(now)) {
      return { label: t("history.today", { time: clock }), title };
    }
    const yesterday = new Date(now);
    yesterday.setDate(now.getDate() - 1);
    if (day === dayStart(yesterday)) {
      return { label: t("history.yesterday", { time: clock }), title };
    }
    const sameYear = date.getFullYear() === now.getFullYear();
    const label = new Intl.DateTimeFormat(localeTag(), {
      year: sameYear ? undefined : "numeric",
      month: "numeric",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    }).format(date);
    return { label, title };
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

  const savedNote = (id: string): string =>
    lastPayload?.entries.find((entry) => entry.id === id)?.note ?? "";

  const persistNote = async (id: string, draft: string): Promise<void> => {
    try {
      const payload = await invoke<HistoryListPayload>("set_history_note", { id, note: draft });
      const serverNote = payload.entries.find((entry) => entry.id === id)?.note ?? "";
      const pending = noteDrafts.get(id);
      if (pending === undefined || pending === serverNote) {
        noteDrafts.delete(id);
        render(payload);
        setStatusKey("history.note_saved");
        return;
      }
      render(payload);
    } catch (error) {
      noteDrafts.set(id, draft);
      setStatusText(errorMessage(error), true);
      if (lastPayload) {
        render(lastPayload);
      }
    }
  };

  const entryRow = (entry: HistoryEntryView, toolsEnabled: boolean): HTMLElement => {
    const row = document.createElement("article");
    row.className = "history-item";
    if (entry.favorite && toolsEnabled) {
      row.classList.add("is-favorite");
    }
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
    const formatted = formatTime(entry.createdAt);
    time.textContent = formatted.label;
    time.dataset.tooltip = formatted.title;
    const size = document.createElement("div");
    size.className = "history-size";
    size.textContent = `${entry.width} × ${entry.height}`;
    meta.append(time, size);
    if (toolsEnabled && entry.mode) {
      const labelKey = modeLabelKey(entry.mode);
      if (labelKey) {
        const mode = document.createElement("div");
        mode.className = "history-mode";
        mode.textContent = t(labelKey);
        meta.append(mode);
      }
    }
    if (entry.imageMissing) {
      const missing = document.createElement("div");
      missing.className = "history-missing-note";
      missing.textContent = t("history.image_missing");
      meta.append(missing);
    }
    if (toolsEnabled) {
      const note = document.createElement("label");
      note.className = "history-note";
      const input = document.createElement("input");
      input.type = "text";
      input.className = "history-note-input";
      input.autocomplete = "off";
      input.maxLength = 500;
      input.dataset.noteId = entry.id;
      input.placeholder = t("history.note_placeholder");
      input.setAttribute("aria-label", t("history.note_label"));
      input.value = noteDrafts.get(entry.id) ?? entry.note;
      input.addEventListener("input", () => {
        noteDrafts.set(entry.id, input.value);
      });
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter" && !event.isComposing) {
          event.preventDefault();
          input.blur();
        }
      });
      input.addEventListener("blur", () => {
        if (applyingDom) {
          return;
        }
        const draft = input.value;
        noteDrafts.delete(entry.id);
        if (draft === savedNote(entry.id)) {
          return;
        }
        void persistNote(entry.id, draft);
      });
      note.append(input);
      meta.append(note);
    }

    const actions = document.createElement("div");
    actions.className = "history-actions";
    if (toolsEnabled) {
      const favorite = document.createElement("button");
      favorite.type = "button";
      favorite.className = entry.favorite
        ? "history-btn history-fav is-on"
        : "history-btn history-fav";
      favorite.dataset.entryAction = "favorite";
      favorite.setAttribute("aria-pressed", entry.favorite ? "true" : "false");
      favorite.disabled = busy;
      const favoriteLabel = t(entry.favorite ? "history.unfavorite" : "history.favorite");
      favorite.setAttribute("aria-label", favoriteLabel);
      favorite.dataset.tooltip = favoriteLabel;
      favorite.innerHTML = STAR_ICON;
      actions.append(favorite);
    }
    const actionLabels: Array<[string, CatalogKey, string]> = [
      ["reedit", "history.reedit", "history-btn history-btn-edit"],
      ["copy", "history.copy", "history-btn history-btn-quiet"],
      ["pin", "history.pin", "history-btn history-btn-quiet"],
      ["delete", "history.delete", "history-btn history-btn-danger"],
    ];
    for (const [action, labelKey, className] of actionLabels) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = className;
      button.dataset.entryAction = action;
      button.textContent = t(labelKey);
      if (entry.imageMissing && action !== "delete") {
        button.disabled = true;
        button.dataset.missingDisabled = "true";
        // 禁用控件不响应自绘提示的悬停;原因提示保留原生 title(R2 允许例外)。
        button.title = t("history.image_missing_title");
      } else if (busy) {
        button.disabled = true;
      }
      if (action === "delete") {
        const separator = document.createElement("span");
        separator.className = "history-action-sep";
        separator.setAttribute("aria-hidden", "true");
        actions.append(separator);
      }
      actions.append(button);
    }

    row.append(holder, meta, actions);
    return row;
  };

  const render = (payload: HistoryListPayload): void => {
    lastPayload = payload;
    const toolsEnabled = payload.toolsEnabled;
    filtersEl.hidden = !toolsEnabled;
    clearFiltersEl.disabled = !filtersActive();
    noticeEl.hidden = !payload.notice;
    noticeEl.textContent = payload.notice ?? "";
    const visible = visibleEntries(
      payload.entries,
      toolsEnabled,
      timeRange,
      modeFilter,
      noteQuery,
    );
    const activeNote =
      document.activeElement instanceof HTMLInputElement && document.activeElement.dataset.noteId
        ? document.activeElement
        : null;
    const editingId = activeNote?.dataset.noteId ?? null;
    const selectionStart = activeNote?.selectionStart ?? null;
    applyingDom = true;
    try {
      listEl.replaceChildren();
      for (const entry of visible) {
        listEl.append(entryRow(entry, toolsEnabled));
      }
    } finally {
      applyingDom = false;
    }
    if (editingId) {
      const next = listEl.querySelector<HTMLInputElement>(`[data-note-id="${editingId}"]`);
      if (next) {
        next.focus();
        if (selectionStart !== null) {
          next.setSelectionRange(selectionStart, selectionStart);
        }
      }
    }
    const noHistory = payload.entries.length === 0;
    const noMatch = !noHistory && visible.length === 0;
    emptyEl.hidden = !(noHistory || noMatch);
    emptyEl.textContent = noMatch ? t("history.filter.empty") : t("history.empty");
    countEl.hidden = payload.entries.length === 0;
    countEl.textContent =
      toolsEnabled && filtersActive()
        ? t("history.count_filtered", { count: visible.length, total: payload.entries.length })
        : t("history.count", { count: payload.entries.length });
  };

  const refresh = async (): Promise<void> => {
    try {
      render(await invoke<HistoryListPayload>("get_history"));
    } catch (error) {
      setStatusText(errorMessage(error), true);
    } finally {
      // 初始加载态只出现一次;后续静默刷新(聚焦/刷新信号)不再闪现。
      loadingEl.hidden = true;
    }
  };

  const performDelete = async (id: string): Promise<void> => {
    setBusy(true);
    try {
      render(await invoke<HistoryListPayload>("delete_history_entry", { id }));
      noteDrafts.delete(id);
      setStatusKey("history.deleted");
    } catch (error) {
      setStatusText(errorMessage(error), true);
    } finally {
      setBusy(false);
    }
  };

  const performClear = async (): Promise<void> => {
    setBusy(true);
    statusState = { key: null, text: "", isError: false };
    renderStatus();
    try {
      render(await invoke<HistoryListPayload>("clear_history"));
      noteDrafts.clear();
      setStatusKey("history.cleared");
    } catch (error) {
      setStatusText(errorMessage(error), true);
    } finally {
      setBusy(false);
    }
  };

  const runAction = async (action: string, id: string, trigger?: HTMLElement): Promise<void> => {
    if (busy) {
      return;
    }
    if (action === "delete") {
      // 删除需二次确认:显示窗口内确认条,确认后再执行,不依赖 WebView 对话框。
      showConfirm({ kind: "delete", id }, trigger);
      return;
    }
    hideConfirm();
    setBusy(true);
    try {
      if (action === "copy") {
        await invoke("copy_history_entry", { id });
        setStatusKey("history.copied");
      } else if (action === "pin") {
        await invoke("pin_history_entry", { id });
        setStatusKey("history.pinned");
      } else if (action === "reedit") {
        await invoke("reedit_history_entry", { id });
      } else if (action === "favorite") {
        const entry = lastPayload?.entries.find((item) => item.id === id);
        const favorite = !(entry?.favorite ?? false);
        render(await invoke<HistoryListPayload>("set_history_favorite", { id, favorite }));
        setStatusKey(favorite ? "history.favorited" : "history.unfavorited");
      }
    } catch (error) {
      setStatusText(errorMessage(error), true);
    } finally {
      setBusy(false);
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
    void runAction(action, id, button);
  });

  const resetFilters = (): void => {
    timeRange = "all";
    modeFilter = "all";
    noteQuery = "";
    timeEl.value = "all";
    modeEl.value = "all";
    searchEl.value = "";
    if (lastPayload) {
      render(lastPayload);
    }
  };

  timeEl.addEventListener("change", () => {
    timeRange = asTimeRange(timeEl.value);
    if (lastPayload) {
      render(lastPayload);
    }
  });

  modeEl.addEventListener("change", () => {
    modeFilter = asModeFilter(modeEl.value);
    if (lastPayload) {
      render(lastPayload);
    }
  });

  searchEl.addEventListener("input", () => {
    noteQuery = searchEl.value;
    if (lastPayload) {
      render(lastPayload);
    }
  });

  clearFiltersEl.addEventListener("click", () => {
    resetFilters();
  });

  clearEl.addEventListener("click", () => {
    if (busy) {
      return;
    }
    showConfirm({ kind: "clear" }, clearEl);
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
  // 设置里开关历史检索后,回到本窗口即按最新开关重绘,不必重启。
  void getCurrentWindow().onFocusChanged(({ payload: focused }) => {
    if (focused) {
      void refresh();
    }
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
