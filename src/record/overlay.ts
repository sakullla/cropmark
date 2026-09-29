import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import {
  mountAnnotationEditor,
  type Annotation,
  type AnnotationEditor,
} from "../annotation";
import { t } from "../i18n";
import type { HudCapabilities, HudRegion, HudSnapshot, RecordingHudState } from "./types";
import "./record.css";

// R3 录制标注层:覆盖录制区域的透明画布 + 标注工具条。
//
// 坐标契约:画布位图 = 录制区域物理像素,与 Rust `rasterize_lenient` 逐帧合成
// 使用的坐标系一致;因此 `exportList()` 的图元可实时同步给录制引擎。
//
// 平台差异:Windows/macOS 是透明实时层,默认鼠标穿透,进入标注模式后接收输入;
// Linux 等降级环境只作为「显式绘制模式」使用:进入时抓取定格快照作底图,
// 绘制期间由 Rust 暂停录制,退出后隐藏(避免不透明层进入录制画面)。

const POLL_MS = 500;
const SYNC_MS = 200;
/** 底部提示与工具条、完成条或控制条之间至少留出的间距;再近就把提示交给控制卡片。 */
const NOTICE_GAP = 8;
const OVERLAY_NOTICE_EVENT = "record-overlay-notice";

/** 与 `HudControlFrame` 一致:相对标注层左上角的 CSS 像素。 */
interface ControlFrame {
  x: number;
  y: number;
  width: number;
  height: number;
}

function readControlFrame(state: RecordingHudState): ControlFrame | null {
  const frame = (state as RecordingHudState & { controlFrame?: unknown }).controlFrame;
  if (typeof frame !== "object" || frame === null) {
    return null;
  }
  const raw = frame as Record<string, unknown>;
  const { x, y, width, height } = raw;
  if (
    typeof x !== "number" ||
    typeof y !== "number" ||
    typeof width !== "number" ||
    typeof height !== "number" ||
    !Number.isFinite(x) ||
    !Number.isFinite(y) ||
    !Number.isFinite(width) ||
    !Number.isFinite(height)
  ) {
    return null;
  }
  return { x, y, width, height };
}

/** 与 `hud.rs` 全屏几何测试同一规则:间距内也算挡住控制条。 */
function conflictsWithControl(notice: DOMRect, root: DOMRect, frame: ControlFrame): boolean {
  const left = notice.left - root.left;
  const top = notice.top - root.top;
  const right = notice.right - root.left;
  const bottom = notice.bottom - root.top;
  return (
    left < frame.x + frame.width + NOTICE_GAP &&
    right > frame.x - NOTICE_GAP &&
    top < frame.y + frame.height + NOTICE_GAP &&
    bottom > frame.y - NOTICE_GAP
  );
}

function messageOf(error: unknown): string {
  return typeof error === "string" && error.trim().length > 0 ? error : String(error);
}

export function mountRecordOverlay(root: HTMLElement): () => void {
  root.className = "record-overlay-root";
  root.innerHTML = `
    <canvas class="record-canvas"></canvas>
    <div class="record-draw" hidden>
      <div class="record-tools annotation-tools" role="toolbar" data-i18n-aria-label="preview.toolbar_group" aria-label="标注"></div>
      <div class="record-draw-actions">
        <span class="record-draw-hint" data-i18n="record.overlay.hint"></span>
        <button type="button" class="record-btn record-draw-done" data-i18n="record.bar.draw_done"></button>
      </div>
    </div>
    <p class="record-overlay-notice" role="status" hidden></p>`;
  const canvas = root.querySelector(".record-canvas");
  const drawBar = root.querySelector(".record-draw");
  const tools = root.querySelector(".record-tools");
  const done = root.querySelector(".record-draw-done");
  const notice = root.querySelector(".record-overlay-notice");
  if (
    !(canvas instanceof HTMLCanvasElement) ||
    !(drawBar instanceof HTMLElement) ||
    !(tools instanceof HTMLElement) ||
    !(done instanceof HTMLButtonElement) ||
    !(notice instanceof HTMLElement)
  ) {
    return () => undefined;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return () => undefined;
  }

  let editor: AnnotationEditor | null = null;
  let region: HudRegion | null = null;
  let capabilities: HudCapabilities | null = null;
  let interactive = false;
  let initialized = false;
  let snapshot: HTMLImageElement | null = null;
  let raf = 0;
  let timer: number | null = null;
  let syncTimer: number | null = null;
  let synced = "[]";
  let noticeMessage = "";
  let publishedNotice = "";
  let controlFrame: ControlFrame | null = null;

  const fitCanvas = (): void => {
    if (!region) {
      return;
    }
    const width = Math.max(1, Math.round(region.width));
    const height = Math.max(1, Math.round(region.height));
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
  };

  /** 马赛克工具(含其模糊模式)在画布上直接取像素:需要定格底图避免预览读出透明像素。 */
  const needsBase = (): boolean => editor?.tool() === "mosaic";

  /**
   * 降级(非 live)平台的不透明层只有快照能充当底图:快照就绪后始终绘制,
   * 否则透明背景上的标注在录制中不可见。实时平台仍只在取像素工具下绘制。
   */
  const shouldDrawSnapshot = (): boolean =>
    snapshot !== null &&
    snapshot.complete &&
    snapshot.naturalWidth > 0 &&
    (capabilities?.liveOverlay !== true || needsBase());

  const draw = (): void => {
    fitCanvas();
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    if (shouldDrawSnapshot() && snapshot) {
      ctx.drawImage(snapshot, 0, 0, canvas.width, canvas.height);
    }
    editor?.paint(ctx);
  };

  const scheduleDraw = (): void => {
    if (raf) {
      return;
    }
    raf = requestAnimationFrame(() => {
      raf = 0;
      draw();
    });
  };

  const publishNotice = (text: string): void => {
    // 空说明只发一次;仍显示的说明重复发送,控制条晚挂载时也能接到。
    if (text.length === 0 && text === publishedNotice) {
      return;
    }
    publishedNotice = text;
    void emit(OVERLAY_NOTICE_EVENT, text);
  };

  /**
   * 提示贴在区域底部。碰上工具条、完成标注条、控制条或画面中心时不留在区域上,
   * 改送到控制卡片已有的说明行。全屏时控制条在区域内侧底边,必须计入它的实际矩形。
   * 文案在测量前写入元素,空文案则清空。测量在同一次布局里完成,避免闪烁。
   */
  const layoutNotice = (): void => {
    const text = noticeMessage;
    notice.textContent = text;
    if (!text) {
      notice.hidden = true;
      notice.style.visibility = "";
      publishNotice("");
      return;
    }
    notice.hidden = false;
    notice.style.visibility = "hidden";
    const noticeBox = notice.getBoundingClientRect();
    const rootBox = root.getBoundingClientRect();
    const centerY = rootBox.top + rootBox.height / 2;
    const occupiedBottom = drawBar.hidden ? 0 : drawBar.getBoundingClientRect().bottom;
    const hitsTools = occupiedBottom > 0 && noticeBox.top < occupiedBottom + NOTICE_GAP;
    const coversCenter = noticeBox.top <= centerY && noticeBox.bottom >= centerY;
    const hitsControl =
      controlFrame !== null && conflictsWithControl(noticeBox, rootBox, controlFrame);
    const collides = hitsTools || coversCenter || hitsControl;
    notice.style.visibility = "";
    if (collides) {
      notice.hidden = true;
      publishNotice(text);
      return;
    }
    publishNotice("");
  };

  const showNotice = (text: string): void => {
    noticeMessage = text;
    layoutNotice();
  };

  const syncAnnotations = (): void => {
    if (!initialized) {
      return;
    }
    const list = editor?.exportList() ?? [];
    const json = JSON.stringify(list);
    if (json === synced) {
      return;
    }
    synced = json;
    void invoke("set_recording_annotations", { annotations: list });
  };

  /** 新录制初始化:取引擎当前标注(含选区壳已确认项)作为编辑起点。 */
  const initialize = async (): Promise<void> => {
    let annotations: Annotation[] = [];
    try {
      annotations = await invoke<Annotation[]>("get_recording_annotations");
    } catch {
      annotations = [];
    }
    if (!initialized) {
      return;
    }
    editor?.setAnnotations(annotations);
    synced = JSON.stringify(annotations);
    scheduleDraw();
  };

  const loadSnapshot = async (): Promise<boolean> => {
    try {
      const shot = await invoke<HudSnapshot | null>("get_recording_hud_snapshot");
      if (!shot) {
        return false;
      }
      const image = new Image();
      const loaded = await new Promise<boolean>((resolve) => {
        image.onload = () => resolve(true);
        image.onerror = () => resolve(false);
        image.src = `data:image/jpeg;base64,${shot.jpgBase64}`;
      });
      if (!loaded) {
        return false;
      }
      snapshot = image;
      scheduleDraw();
      return true;
    } catch {
      return false;
    }
  };

  /** 标注模式切换:进入时准备底图,降级平台等底图就绪后再显示绘制层。 */
  const applyInteractive = async (next: boolean): Promise<void> => {
    interactive = next;
    drawBar.hidden = !next;
    if (next) {
      const live = capabilities?.liveOverlay === true;
      const loaded = await loadSnapshot();
      if (!live) {
        if (loaded) {
          void invoke("set_recording_hud_overlay_visible", { visible: true });
        } else {
          interactive = false;
          drawBar.hidden = true;
          showNotice(t("record.overlay.snapshot_failed"));
          void invoke("set_recording_hud_interactive", { interactive: false });
        }
      }
    } else {
      snapshot = null;
      showNotice("");
      scheduleDraw();
      if (capabilities?.liveOverlay !== true) {
        // 降级平台绘制层只在显式绘制期间可见:退出后立即隐藏,避免不透明层
        // 继续盖住屏幕(会话中途结束的路径同样走这里)。
        void invoke("set_recording_hud_overlay_visible", { visible: false });
      }
    }
    layoutNotice();
  };

  const applyState = (state: RecordingHudState): void => {
    capabilities = state.capabilities;
    controlFrame = readControlFrame(state);
    if (state.region) {
      region = state.region;
    }
    if (state.status !== null) {
      // 打开事件可能早于视图挂载(极早期录制/开发重载):见到会话就持续同步。
      startTimers();
    }
    if (!initialized) {
      initialized = true;
      void initialize();
      void applyInteractive(state.interactive);
    } else if (state.interactive !== interactive) {
      void applyInteractive(state.interactive);
    } else {
      scheduleDraw();
      layoutNotice();
    }
  };

  const refresh = async (): Promise<void> => {
    try {
      applyState(await invoke<RecordingHudState>("get_recording_hud_state"));
    } catch {
      // 宿主暂不可用:下一次轮询重试。
    }
  };

  const startTimers = (): void => {
    if (timer === null) {
      timer = window.setInterval(() => void refresh(), POLL_MS);
    }
    if (syncTimer === null) {
      syncTimer = window.setInterval(syncAnnotations, SYNC_MS);
    }
  };

  const stopTimers = (): void => {
    if (timer !== null) {
      window.clearInterval(timer);
      timer = null;
    }
    if (syncTimer !== null) {
      window.clearInterval(syncTimer);
      syncTimer = null;
    }
  };

  const exitInteractive = async (): Promise<void> => {
    interactive = false;
    drawBar.hidden = true;
    snapshot = null;
    showNotice("");
    scheduleDraw();
    try {
      applyState(
        await invoke<RecordingHudState>("set_recording_hud_interactive", { interactive: false }),
      );
    } catch (error) {
      showNotice(messageOf(error));
    }
  };

  editor = mountAnnotationEditor({
    root,
    canvas,
    ctx,
    toolbar: tools,
    textHost: root,
    // 坐标空间 = 录制区域物理像素(与 Rust 逐帧合成一致);scale 取显示器
    // 缩放系数,线宽/字号等推导尺寸与导出及截图路径保持一致。
    frame: () => (region ? { width: region.width, height: region.height, scale: region.scale } : null),
    redraw: () => scheduleDraw(),
    isEditable: () => interactive,
    onToolChange: () => scheduleDraw(),
    onError: (error) => {
      showNotice(typeof error === "string" ? error : t(error.key, error.params));
    },
  });

  /** 清空上一会话的绘制状态:复位后由下一次状态刷新按引擎标注重新初始化。 */
  const resetView = (): void => {
    initialized = false;
    interactive = false;
    snapshot = null;
    drawBar.hidden = true;
    showNotice("");
    editor?.clear();
    synced = "[]";
    ctx.clearRect(0, 0, canvas.width, canvas.height);
  };

  done.addEventListener("click", () => void exitInteractive());
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && interactive && editor?.isTextEditing() !== true) {
      event.preventDefault();
      void exitInteractive();
    }
  });

  void listen("record-hud-open", () => {
    region = null;
    resetView();
    startTimers();
    void refresh();
  });
  void listen("record-hud-close", () => {
    stopTimers();
    resetView();
  });
  void listen("record-hud-reset", () => {
    // 会话结束/重新开始:清空旧标注后立即按当前引擎标注重新初始化。
    resetView();
    void refresh();
  });
  void listen<RecordingHudState>("record-hud-state", (event) => {
    applyState(event.payload);
  });
  // 预创建窗口可能错过打开事件(视图重载等):挂载即刷新一次。
  void refresh();

  return () => {
    layoutNotice();
    scheduleDraw();
  };
}
