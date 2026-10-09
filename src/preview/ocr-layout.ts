import { currentMonitor, getCurrentWindow, PhysicalPosition, PhysicalSize } from "@tauri-apps/api/window";
import { t } from "../i18n";

/** Keep image controls in their own column; only the divider changes the split. */
export function mountOcrLayout(stage: HTMLElement): { sync: (active: boolean) => void } {
  const divider = document.createElement("div");
  divider.className = "preview-ocr-divider";
  divider.hidden = true;
  divider.tabIndex = 0;
  divider.setAttribute("role", "separator");
  divider.setAttribute("aria-orientation", "vertical");
  divider.dataset.i18nAriaLabel = "preview.ocr_panel.resize";
  const resizeLabel = t("preview.ocr_panel.resize");
  divider.setAttribute("aria-label", resizeLabel);
  // 握把的 ::after 已经是竖条，不能再挂自绘气泡。系统提示能说明拖动和双击。
  divider.title = resizeLabel;
  stage.querySelector(".preview-image-pane")!.after(divider);
  let ratio = 0.44;
  let active = false;
  let pointer: number | null = null;
  const update = (): void => {
    const available = Math.max(1, stage.clientWidth - 32);
    const minimum = Math.min(280, available * 0.5);
    const maximum = Math.max(minimum, available - Math.min(240, available * 0.35));
    const width = Math.max(minimum, Math.min(maximum, available * ratio));
    stage.style.setProperty("--ocr-pane-width", `${width}px`);
    divider.setAttribute("aria-valuemin", String(Math.round(minimum / available * 100)));
    divider.setAttribute("aria-valuemax", String(Math.round(maximum / available * 100)));
    divider.setAttribute("aria-valuenow", String(Math.round(width / available * 100)));
  };
  divider.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    pointer = event.pointerId;
    divider.setPointerCapture(pointer);
    stage.classList.add("is-resizing-ocr");
  });
  divider.addEventListener("pointermove", (event) => {
    if (pointer !== event.pointerId) return;
    const bounds = stage.getBoundingClientRect();
    ratio = Math.max(0.1, Math.min(0.9, (bounds.right - 10 - event.clientX) / Math.max(1, stage.clientWidth - 32)));
    update();
  });
  const end = (): void => {
    pointer = null;
    stage.classList.remove("is-resizing-ocr");
  };
  divider.addEventListener("pointerup", end);
  divider.addEventListener("pointercancel", end);
  divider.addEventListener("lostpointercapture", end);
  divider.addEventListener("dblclick", () => { ratio = 0.44; update(); });
  divider.addEventListener("keydown", (event) => {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    event.stopPropagation();
    ratio = Number(divider.getAttribute("aria-valuenow")) / 100;
    ratio = event.key === "Home" ? 0 : event.key === "End" ? 1 : ratio + (event.key === "ArrowLeft" ? 0.03 : -0.03);
    update();
  });
  const observer = new ResizeObserver(update);
  observer.observe(stage);
  return {
    sync(next) {
      divider.hidden = !next;
      stage.classList.toggle("has-ocr", next);
      if (next && !active) void growForOcr(() => active);
      active = next;
      update();
    },
  };
}

/** Small captures create small preview windows. Grow once on entry, within the work area. */
async function growForOcr(isActive: () => boolean): Promise<void> {
  try {
    const win = getCurrentWindow();
    const [monitor, size, position, maximized, fullscreen] = await Promise.all([
      currentMonitor(), win.outerSize(), win.outerPosition(), win.isMaximized(), win.isFullscreen(),
    ]);
    if (!isActive() || !monitor || maximized || fullscreen) return;
    const area = monitor.workArea;
    const width = Math.max(size.width, Math.min(area.size.width, Math.round(960 * monitor.scaleFactor)));
    const height = Math.max(size.height, Math.min(area.size.height, Math.round(640 * monitor.scaleFactor)));
    if (width <= size.width && height <= size.height) return;
    const inner = await win.innerSize();
    if (!isActive()) return;
    await win.setSize(new PhysicalSize(width - (size.width - inner.width), height - (size.height - inner.height)));
    await win.setPosition(new PhysicalPosition(
      Math.max(area.position.x, Math.min(position.x, area.position.x + area.size.width - width)),
      Math.max(area.position.y, Math.min(position.y, area.position.y + area.size.height - height)),
    ));
  } catch (error) {
    console.warn("Could not enlarge OCR preview", error);
  }
}
