import { emit, listen } from "@tauri-apps/api/event";

// 界面主题：跟随系统 / 浅色 / 深色。选择写入 localStorage，并通过事件
// 同步到其它已打开的窗口；所有视图在挂载前调用 initTheme 以避免首帧闪变。
export type ThemeSetting = "system" | "light" | "dark";

const STORAGE_KEY = "cropmark.theme";
const THEME_EVENT = "theme-changed";

let current: ThemeSetting = "system";
let started = false;

function isTheme(value: unknown): value is ThemeSetting {
  return value === "system" || value === "light" || value === "dark";
}

function storedTheme(): ThemeSetting {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    return isTheme(value) ? value : "system";
  } catch {
    return "system";
  }
}

export function getTheme(): ThemeSetting {
  return current;
}

export function applyTheme(theme: ThemeSetting): void {
  current = theme;
  const root = document.documentElement;
  if (theme === "system") {
    delete root.dataset.theme;
  } else {
    root.dataset.theme = theme;
  }
}

export async function initTheme(): Promise<void> {
  applyTheme(storedTheme());
  if (started) {
    return;
  }
  started = true;
  try {
    await listen<ThemeSetting>(THEME_EVENT, (event) => {
      if (isTheme(event.payload) && event.payload !== current) {
        applyTheme(event.payload);
      }
    });
  } catch {
    // 浏览器直开或宿主未就绪时仅使用本地状态。
  }
  window.addEventListener("storage", (event) => {
    if (event.key !== STORAGE_KEY) {
      return;
    }
    applyTheme(isTheme(event.newValue) ? event.newValue : "system");
  });
}

export function setTheme(theme: ThemeSetting): void {
  if (theme === current) {
    return;
  }
  applyTheme(theme);
  try {
    localStorage.setItem(STORAGE_KEY, theme);
  } catch {
    // 存储不可用时仍立即生效，只是不跨会话保留。
  }
  void emit(THEME_EVENT, theme).catch(() => undefined);
}
