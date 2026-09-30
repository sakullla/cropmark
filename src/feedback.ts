/// R1:结果类提示(toast/note)统一自动隐藏时长,毫秒。
/// 仅用于「已复制/已保存」这类终态反馈;进行中提示(正在复制/识别中)常驻,
/// 直到被终态替换(ADR-2 旧约束)。Rust 侧原生 toast 时长不属于本常量管辖。
export const NOTICE_AUTO_HIDE_MS = 3600;
