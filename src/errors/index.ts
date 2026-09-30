import { t } from "../i18n";

export function autostartHelp(
  enabled: boolean,
  message: string | null | undefined,
): string {
  if (message) {
    return message;
  }
  if (enabled) {
    return t("autostart.help_enabled");
  }
  return t("autostart.help_disabled");
}

/// R3:热键错误呈现为「前端主词条 + 后端已本地化细节」。
/// 后端 `HotkeyErrors` 已按当前语言解析为完整句子,这里只加前端主词条;
/// 语言切换后设置视图重渲染会重新拉取并重新组合,两种语言均正确。
export function hotkeyErrorText(error: string | null | undefined): string {
  if (!error) {
    return "";
  }
  return t("settings.hotkey.error_prefix", { detail: error });
}
