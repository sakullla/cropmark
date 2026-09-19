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

export function hotkeyErrorText(error: string | null | undefined): string {
  return error ?? "";
}
