export function autostartHelp(
  enabled: boolean,
  message: string | null | undefined,
): string {
  if (message) {
    return message;
  }
  if (enabled) {
    return "下次登录只会在托盘出现 Cropmark，不会打开主窗口。";
  }
  return "新安装默认关闭。打开后由系统在登录时拉起托盘。";
}

export function hotkeyErrorText(error: string | null | undefined): string {
  return error ?? "";
}
