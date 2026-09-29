// R3 录制 HUD:Rust `record::hud` 载荷的前端镜像。
// 字段名与 serde camelCase 对齐;时间统一为毫秒。

export type RecordingPhase = "recording" | "paused" | "finished" | "failed";

export interface RecordingStatus {
  phase: RecordingPhase;
  format: string;
  frameCount: number;
  elapsedMs: number;
  width: number;
  height: number;
  autoStopped: boolean;
  error: string | null;
}

export interface HudRegion {
  width: number;
  height: number;
  /** 显示器缩放系数:标注层 `AnnotationFrame.scale` 用,与录制合成一致。 */
  scale: number;
}

export interface HudCapabilities {
  liveOverlay: boolean;
  cursorPassthrough: boolean;
  captureProtection: boolean;
  /** 降级说明词条键;完整能力时省略。 */
  noticeKey?: string;
}

export interface PendingRecording {
  tempPath: string;
  fileName: string;
  format: string;
  width: number;
  height: number;
  frameCount: number;
  durationMs: number;
  autoStopped: boolean;
  interrupted: string | null;
}

export interface RecordingHudState {
  status: RecordingStatus | null;
  pending: PendingRecording[];
  capabilities: HudCapabilities;
  interactive: boolean;
  hasContext: boolean;
  region: HudRegion | null;
  limitMs: number;
}

export type StopOutcome =
  | { kind: "saved"; name: string }
  | { kind: "discarded" }
  | { kind: "cancelled" }
  | { kind: "failed"; message: string; retryable: boolean }
  | { kind: "empty" };

export interface HudSnapshot {
  jpgBase64: string;
  width: number;
  height: number;
}

/** 毫秒 → `mm:ss`(上限 30 分钟,分钟位不会溢出两位数)。 */
export function formatDuration(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
}
