import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import catalog from "./catalog.json";

// 界面语言(R12):词条表与 Rust 侧共用 `catalog.json`;Rust 负责解析
// `system | zh-CN | en` 与系统检测,前端只消费解析结果并即时重渲染。
// 缺失词条回退另一种语言的完整文案,不允许空白或键名直出。

export type Language = "zh-CN" | "en";

export type CatalogKey = keyof typeof catalog;

interface LanguageInfo {
  language?: string;
  resolvedLanguage?: string;
}

const table = catalog as Record<string, Record<Language, string>>;

let current: Language = "zh-CN";
const listeners = new Set<() => void>();

export function currentLanguage(): Language {
  return current;
}

/** `toLocaleString` 等本地化格式使用的语言标签。 */
export function localeTag(): string {
  return current;
}

function normalizeLanguage(tag: string | null | undefined): Language | null {
  if (!tag) {
    return null;
  }
  const normalized = tag.trim().toLowerCase().replace("_", "-");
  if (normalized.startsWith("zh")) {
    return "zh-CN";
  }
  if (normalized.startsWith("en")) {
    return "en";
  }
  return null;
}

function applyLanguage(tag: string | null | undefined): boolean {
  const next = normalizeLanguage(tag);
  if (!next || next === current) {
    return false;
  }
  current = next;
  document.documentElement.lang = next;
  return true;
}

/** 切换语言并通知已注册的重渲染回调;返回是否发生切换。 */
export function setLanguage(tag: string | null | undefined): boolean {
  if (!applyLanguage(tag)) {
    return false;
  }
  for (const listener of listeners) {
    listener();
  }
  return true;
}

/** 注册"语言已切换"回调(视图重渲染入口);返回取消注册函数。 */
export function onLanguageChanged(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/**
 * 从 Rust 读取解析后的界面语言,并订阅后续切换事件。
 * 在各视图 mount 之前调用一次;失败时保持默认中文。
 */
export async function initI18n(): Promise<Language> {
  try {
    const info = await invoke<LanguageInfo>("get_language");
    applyLanguage(info?.resolvedLanguage ?? info?.language);
  } catch {
    // 单测/浏览器直开等无宿主环境:保持默认语言。
  }
  void listen<LanguageInfo>("language-changed", (event) => {
    const payload = event.payload;
    setLanguage(payload?.resolvedLanguage ?? payload?.language);
  });
  return current;
}

function lookup(language: Language, key: string): string {
  const entry = table[key];
  if (!entry) {
    return key;
  }
  const primary = entry[language];
  if (typeof primary === "string" && primary.trim().length > 0) {
    return primary;
  }
  const fallback = entry[language === "zh-CN" ? "en" : "zh-CN"];
  if (typeof fallback === "string" && fallback.trim().length > 0) {
    return fallback;
  }
  return key;
}

/** 取当前语言的文案;`params` 替换 `{name}` 占位符。 */
export function t(
  key: CatalogKey,
  params?: Record<string, string | number>,
): string {
  const template = lookup(current, key);
  if (!params) {
    return template;
  }
  return template.replace(/\{(\w+)\}/g, (match, name: string) =>
    Object.prototype.hasOwnProperty.call(params, name) ? String(params[name]) : match,
  );
}

function readParams(element: HTMLElement, datasetKey: string): Record<string, string> | undefined {
  const raw = element.dataset[datasetKey];
  if (!raw) {
    return undefined;
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      const params: Record<string, string> = {};
      for (const [name, value] of Object.entries(parsed as Record<string, unknown>)) {
        params[name] = String(value);
      }
      return params;
    }
  } catch {
    // 非法参数视为无参数:保留原占位符而不是抛错。
  }
  return undefined;
}

/**
 * 静态标签重渲染:按 `data-i18n` / `data-i18n-title` / `data-i18n-aria-label`
 * 更新文本与属性,`-params` 后缀的 data 属性提供 JSON 占位符参数。
 * 视图 mount 时调用一次,并在 `onLanguageChanged` 回调中再次调用。
 */
export function applyTranslations(root: ParentNode = document): void {
  root.querySelectorAll<HTMLElement>("[data-i18n]").forEach((element) => {
    element.textContent = t(
      element.dataset.i18n as CatalogKey,
      readParams(element, "i18nParams"),
    );
  });
  root.querySelectorAll<HTMLElement>("[data-i18n-title]").forEach((element) => {
    element.title = t(
      element.dataset.i18nTitle as CatalogKey,
      readParams(element, "i18nTitleParams"),
    );
  });
  root.querySelectorAll<HTMLElement>("[data-i18n-aria-label]").forEach((element) => {
    element.setAttribute(
      "aria-label",
      t(
        element.dataset.i18nAriaLabel as CatalogKey,
        readParams(element, "i18nAriaLabelParams"),
      ),
    );
  });
  root.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("[data-i18n-placeholder]").forEach(
    (element) => {
      element.placeholder = t(element.dataset.i18nPlaceholder as CatalogKey);
    },
  );
}
