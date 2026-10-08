import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { NOTICE_AUTO_HIDE_MS } from "../feedback";
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

/** R6:当前可撤销的删除/清空批;`expiresAt` 为 Unix 毫秒,与后端 8 秒窗口同源。 */
interface PendingUndo {
  kind: "delete" | "clear";
  count: number;
  expiresAt: number;
}

interface HistoryListPayload {
  entries: HistoryEntryView[];
  notice: string | null;
  toolsEnabled: boolean;
  pendingUndo: PendingUndo | null;
}

type TimeRange = "all" | "today" | "7d" | "30d";
type ModeFilter = "all" | "region" | "window" | "fullscreen" | "long";

const DAY_MS = 24 * 60 * 60 * 1000;

const STAR_ICON = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linejoin="round" aria-hidden="true"><path d="M12 3.6 14.7 9.1 20.7 9.9 16.3 14.1 17.4 20.1 12 17.2 6.6 20.1 7.7 14.1 3.3 9.9 9.3 9.1Z"/></svg>`;

// 时间格式化器按 locale + 选项缓存为模块级单例:整表重绘每行都要格式化
// 时间,不缓存则每次 render 每行新建 2-3 个 Intl.DateTimeFormat;语言切换
// 时缓存 key 不同,自动重建。
const timeFormatters = new Map<string, Intl.DateTimeFormat>();

function cachedFormatter(tag: string, options: Intl.DateTimeFormatOptions): Intl.DateTimeFormat {
  const key = `${tag}:${JSON.stringify(options)}`;
  let formatter = timeFormatters.get(key);
  if (!formatter) {
    formatter = new Intl.DateTimeFormat(tag, options);
    timeFormatters.set(key, formatter);
  }
  return formatter;
}

function startOfLocalDay(ms: number): number {
  const date = new Date(ms);
  return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
}

/** 行内动作按钮的焦点锚点:条目 id + 动作名,用于整表重建后重新定位焦点。 */
type EntryActionFocus = { id: string; action: string };

function entryActionFocus(element: Element | null): EntryActionFocus | null {
  if (!(element instanceof HTMLButtonElement)) {
    return null;
  }
  const action = element.dataset.entryAction;
  const row = element.closest("[data-entry-id]");
  if (!action || !(row instanceof HTMLElement)) {
    return null;
  }
  const id = row.dataset.entryId;
  return id ? { id, action } : null;
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
        <div class="history-list" data-list></div>
        <p class="history-loading" data-loading role="status"><span class="progress" aria-hidden="true"></span><span data-i18n="history.loading">正在加载历史记录…</span></p>
        <p class="history-empty" data-empty data-i18n="history.empty" hidden>暂无历史记录。截图完成后会自动出现在这里。</p>
        <p class="history-status" data-status role="status" hidden></p>
        <p class="history-undo" data-undo role="status" hidden>
          <span class="history-undo-text" data-undo-text></span>
          <button type="button" class="history-btn history-undo-btn" data-undo-action data-i18n="history.undo.action">撤销</button>
        </p>
        <div class="history-confirm" data-confirm role="alertdialog" data-i18n-aria-label="history.confirm_group" aria-label="确认操作" hidden>
          <p class="history-confirm-text" data-confirm-text></p>
          <div class="history-confirm-actions">
            <button type="button" class="history-btn history-btn-danger history-confirm-accept" data-confirm-accept data-i18n="history.accept">确认</button>
            <button type="button" class="history-btn history-btn-quiet" data-confirm-cancel data-i18n="history.cancel">取消</button>
          </div>
        </div>
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
  const undoEl = root.querySelector("[data-undo]");
  const undoTextEl = root.querySelector("[data-undo-text]");
  const undoBtn = root.querySelector("[data-undo-action]");
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
    !(undoEl instanceof HTMLElement) ||
    !(undoTextEl instanceof HTMLElement) ||
    !(undoBtn instanceof HTMLButtonElement) ||
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
  // R6:撤销条跟随后端 pending-delete 生命周期;隐藏计时按 `expiresAt` 剩余
  // 时长对齐 8 秒窗口,不复用 3600ms 的结果反馈常量(此处窗口即撤销机会本身)。
  let undoTimer: number | undefined;

  const hideUndo = (): void => {
    if (undoTimer !== undefined) {
      window.clearTimeout(undoTimer);
      undoTimer = undefined;
    }
    undoEl.hidden = true;
  };

  const showUndo = (pending: PendingUndo): void => {
    if (undoTimer !== undefined) {
      window.clearTimeout(undoTimer);
    }
    const remaining = pending.expiresAt - Date.now();
    if (remaining <= 0) {
      // 窗口已过期(筛选/语言切换等本地重绘携带的陈旧 pendingUndo):
      // 直接保持隐藏,不先显示再 0ms 隐藏造成单帧闪现。
      undoEl.hidden = true;
      undoTimer = undefined;
      return;
    }
    undoTextEl.textContent = t(
      pending.kind === "clear" ? "history.undo.cleared" : "history.undo.deleted",
      { count: pending.count },
    );
    undoEl.hidden = false;
    undoTimer = window.setTimeout(hideUndo, remaining);
  };

  const confirmMessage = (pending: PendingConfirm): string =>
    pending.kind === "delete" ? t("history.confirm_delete") : t("history.confirm_clear");
  const confirmAcceptLabel = (pending: PendingConfirm): string =>
    pending.kind === "delete" ? t("history.confirm_delete_accept") : t("history.confirm_clear_accept");

  const filtersActive = (): boolean =>
    timeRange !== "all" || modeFilter !== "all" || noteQuery.trim().length > 0;

  // busy 期间操作按钮给禁用可视态;因图像缺失而常驻禁用的按钮保持禁用;
  // 空列表时清空按钮同样禁用,不再弹「清空全部历史记录?」空确认。
  // 按钮被禁用的瞬间浏览器会把落在其上的焦点移走,render 读 activeElement
  // 已拿不到:禁用前记下「条目 id + 动作名」,render 重建后据此还原焦点。
  let disabledFocus: EntryActionFocus | null = null;

  const syncBusy = (): void => {
    disabledFocus = busy ? entryActionFocus(document.activeElement) : null;
    clearEl.disabled = busy || (lastPayload?.entries.length ?? 0) === 0;
    confirmAcceptEl.disabled = busy;
    undoBtn.disabled = busy;
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

  // 结果态(已复制/已收藏)按共享时长自动隐藏(ADR-2);错误常驻直到下一次操作覆盖。
  let statusTimer = 0;
  const scheduleStatusHide = (): void => {
    if (statusTimer) {
      window.clearTimeout(statusTimer);
      statusTimer = 0;
    }
    statusTimer = window.setTimeout(() => {
      statusTimer = 0;
      statusState = { key: null, text: "", isError: false };
      renderStatus();
    }, NOTICE_AUTO_HIDE_MS);
  };

  const setStatusKey = (key: CatalogKey, isError = false): void => {
    statusState = { key, text: "", isError };
    renderStatus();
    if (isError) {
      if (statusTimer) {
        window.clearTimeout(statusTimer);
        statusTimer = 0;
      }
    } else {
      scheduleStatusHide();
    }
  };

  const setStatusText = (text: string, isError = false): void => {
    statusState = { key: null, text, isError };
    renderStatus();
    if (!isError) {
      scheduleStatusHide();
    }
  };

  const errorMessage = (error: unknown): string =>
    error instanceof Error ? error.message : String(error);

  const formatTime = (createdAt: number): { label: string; title: string } => {
    const date = new Date(createdAt);
    if (Number.isNaN(date.getTime())) {
      const unknown = t("history.unknown_time");
      return { label: unknown, title: unknown };
    }
    const tag = localeTag();
    const title = cachedFormatter(tag, {}).format(date);
    const clock = cachedFormatter(tag, {
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
    const label = cachedFormatter(tag, {
      year: sameYear ? undefined : "numeric",
      month: "numeric",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    }).format(date);
    return { label, title };
  };

  // 缩略图按记录 id 缓存 blob URL:筛选输入、备注保存、聚焦刷新都会整表
  // 重绘,不缓存则每次都重取一遍并闪空。容量上限内先进先出回收。上限 200
  // 与设置允许的最大历史保留条数(5-200)对齐:调高保留条数的用户聚焦/筛选
  // 时不再有上百条缩略图反复走 get_history_thumbnail IPC + 解码 + 整批闪白;
  // 200 张缩略图 blob URL 的内存量级在几 MB 内,超出部分仍按先进先出回收。
  const thumbUrls = new Map<string, string>();
  const thumbFailures = new Set<string>();
  const THUMB_CACHE_LIMIT = 200;

  const rememberThumb = (id: string, url: string): void => {
    if (thumbUrls.size >= THUMB_CACHE_LIMIT && !thumbUrls.has(id)) {
      const oldest = thumbUrls.keys().next().value;
      if (oldest !== undefined) {
        URL.revokeObjectURL(thumbUrls.get(oldest)!);
        thumbUrls.delete(oldest);
      }
    }
    thumbUrls.set(id, url);
  };

  const loadThumbnail = (id: string, holder: HTMLElement): void => {
    const attach = (url: string): void => {
      const image = document.createElement("img");
      image.alt = "";
      image.draggable = false;
      image.src = url;
      holder.replaceChildren(image);
    };
    const markMissing = (): void => {
      thumbFailures.add(id);
      holder.classList.add("missing");
      holder.textContent = t("history.thumb_missing");
    };
    const cached = thumbUrls.get(id);
    if (cached !== undefined) {
      attach(cached);
      return;
    }
    if (thumbFailures.has(id)) {
      markMissing();
      return;
    }
    void invoke<ArrayBuffer>("get_history_thumbnail", { id })
      .then((bytes) => {
        if (bytes.byteLength === 0) {
          markMissing();
          return;
        }
        const url = URL.createObjectURL(new Blob([bytes], { type: "image/png" }));
        rememberThumb(id, url);
        // 渲染之间 holder 可能已被换走,只有仍在树中的才写入。
        if (holder.isConnected) {
          attach(url);
        }
      })
      .catch(() => {
        // IPC 失败按暂缺展示但不进失败缓存,下次重绘仍可重试。
        if (holder.isConnected) {
          holder.classList.add("missing");
          holder.textContent = t("history.thumb_missing");
        }
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
      if (!entry.imageMissing) {
        // 双击缩略图即再编辑,提示与按钮同一文案。
        holder.dataset.tooltip = t("history.reedit");
      }
      loadThumbnail(entry.id, holder);
    }

    const meta = document.createElement("div");
    meta.className = "history-meta";
    const time = document.createElement("div");
    time.className = "history-time";
    const formatted = formatTime(entry.createdAt);
    time.textContent = formatted.label;
    time.dataset.tooltip = formatted.title;
    const detail = document.createElement("div");
    detail.className = "history-size";
    const detailParts = [`${entry.width} × ${entry.height}`];
    if (toolsEnabled && entry.mode) {
      const labelKey = modeLabelKey(entry.mode);
      if (labelKey) {
        detailParts.push(t(labelKey));
      }
    }
    detail.textContent = detailParts.join(" · ");
    meta.append(time, detail);
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
      row.append(note);
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

    const main = document.createElement("div");
    main.className = "history-main";
    main.append(meta, actions);
    row.append(holder, main);
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
    const active = document.activeElement;
    const activeNote =
      active instanceof HTMLInputElement && active.dataset.noteId ? active : null;
    const editingId = activeNote?.dataset.noteId ?? null;
    const selectionStart = activeNote?.selectionStart ?? null;
    // 焦点还原从备注输入框推广到行内按钮:重建前记下条目 id + 动作名,
    // 重建后重新定位并聚焦,键盘用户按 Enter 收藏后 Tab 序保持连续。
    // busy 中按钮被禁用导致焦点已被移走时,退回禁用前的快照。
    const activeAction = entryActionFocus(active) ?? (busy ? disabledFocus : null);
    // 滚动锚定:记下视口顶缘的第一行(列表与行同为 static,offsetTop 相对
    // 页面根,故用视口坐标比较),重建后把滚动位置对回该条目;条目被过滤
    // 掉时回退为原滚动位置。聚焦/截新图触发的整表刷新不再把浏览位置打回顶部。
    const prevScrollTop = listEl.scrollTop;
    const listTop = listEl.getBoundingClientRect().top;
    let anchorId: string | null = null;
    for (const child of listEl.children) {
      if (
        child instanceof HTMLElement &&
        child.dataset.entryId !== undefined &&
        child.getBoundingClientRect().top >= listTop
      ) {
        anchorId = child.dataset.entryId;
        break;
      }
    }
    applyingDom = true;
    try {
      listEl.replaceChildren();
      for (const entry of visible) {
        listEl.append(entryRow(entry, toolsEnabled));
      }
    } finally {
      applyingDom = false;
    }
    if (anchorId !== null) {
      const anchorRow = listEl.querySelector<HTMLElement>(`[data-entry-id="${anchorId}"]`);
      if (anchorRow) {
        listEl.scrollTop += anchorRow.getBoundingClientRect().top - listEl.getBoundingClientRect().top;
      } else {
        listEl.scrollTop = prevScrollTop;
      }
    }
    if (editingId) {
      const next = listEl.querySelector<HTMLInputElement>(`[data-note-id="${editingId}"]`);
      if (next) {
        next.focus();
        if (selectionStart !== null) {
          next.setSelectionRange(selectionStart, selectionStart);
        }
      }
    } else if (activeAction) {
      const selector = `[data-entry-id="${activeAction.id}"] [data-entry-action="${activeAction.action}"]`;
      const restoreFocus = (): void => {
        const next = listEl.querySelector<HTMLButtonElement>(selector);
        if (next && !next.disabled) {
          next.focus();
        }
      };
      if (busy) {
        // busy 中重建的按钮暂为禁用(禁用按钮 focus 无效);本轮同步代码
        // (runAction finally 的 setBusy(false))结束后再还原。
        queueMicrotask(restoreFocus);
      } else {
        restoreFocus();
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
    clearEl.disabled = busy || payload.entries.length === 0;
    if (payload.pendingUndo) {
      showUndo(payload.pendingUndo);
    } else {
      hideUndo();
    }
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
    // 删除的终态反馈由撤销条(「已删除 N 条记录。撤销」)承担,状态行清空。
    statusState = { key: null, text: "", isError: false };
    renderStatus();
    try {
      render(await invoke<HistoryListPayload>("delete_history_entry", { id }));
      noteDrafts.delete(id);
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
    } catch (error) {
      setStatusText(errorMessage(error), true);
    } finally {
      setBusy(false);
    }
  };

  const performUndo = async (): Promise<void> => {
    setBusy(true);
    hideUndo();
    try {
      render(await invoke<HistoryListPayload>("undo_history_delete"));
    } catch (error) {
      setStatusText(errorMessage(error), true);
      // 失败后按后端实际状态重绘(标记可能仍在窗口内)。
      await refresh();
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

  // 双击缩略图/时间区 = 再编辑(最高频动作);落在按钮或输入框上的双击不触发。
  listEl.addEventListener("dblclick", (event) => {
    const target = event.target;
    if (!(target instanceof Element)) {
      return;
    }
    if (target.closest("button, input, select, textarea, a")) {
      return;
    }
    const row = target.closest("[data-entry-id]");
    if (!(row instanceof HTMLElement)) {
      return;
    }
    const reedit = row.querySelector<HTMLButtonElement>('[data-entry-action="reedit"]');
    const id = row.dataset.entryId;
    if (id && reedit && !reedit.disabled) {
      void runAction("reedit", id, reedit);
    }
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

  undoBtn.addEventListener("click", () => {
    if (busy) {
      return;
    }
    void performUndo();
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

  // Ctrl+F 聚焦备注搜索(筛选开启时);焦点在任何输入控件内不抢。
  window.addEventListener("keydown", (event) => {
    if (!(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey) {
      return;
    }
    if (event.key.toLowerCase() !== "f" || filtersEl.hidden) {
      return;
    }
    const active = document.activeElement;
    if (active instanceof HTMLInputElement && active !== searchEl) {
      return;
    }
    event.preventDefault();
    searchEl.focus();
    searchEl.select();
  });

  // Esc 分层(R9 与 settings/guide/preview 一致):确认条打开时等同取消,
  // 输入框内先失焦,其余情况关闭历史窗;任何分支都不执行删除。
  window.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || event.defaultPrevented || event.isComposing || event.keyCode === 229) {
      return;
    }
    if (pendingConfirm) {
      if (!busy) {
        event.preventDefault();
        hideConfirm();
      }
      return;
    }
    const active = document.activeElement;
    if (
      active instanceof HTMLInputElement ||
      active instanceof HTMLTextAreaElement ||
      active instanceof HTMLSelectElement
    ) {
      event.preventDefault();
      active.blur();
      return;
    }
    event.preventDefault();
    void getCurrentWindow().close();
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
