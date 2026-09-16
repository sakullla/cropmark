import { invoke } from "@tauri-apps/api/core";
import "./overlay.css";

interface PreviewFrame {
  pngBase64: string;
  width: number;
  height: number;
}

export function mountPreview(root: HTMLElement): void {
  root.className = "preview-root";
  root.innerHTML = `
    <header data-tauri-drag-region>
      <div class="brand" data-tauri-drag-region>
        <span class="mark" aria-hidden="true"></span>
        <span class="name">Cropmark</span>
      </div>
      <p class="preview-note">未标注图已复制</p>
      <button type="button" class="icon-btn" data-action="close" aria-label="关闭">×</button>
    </header>
    <div class="preview-stage"><img alt="截图预览" /></div>
  `;
  const image = root.querySelector("img");
  const close = root.querySelector("[data-action=close]");
  if (!(image instanceof HTMLImageElement) || !(close instanceof HTMLButtonElement)) {
    return;
  }
  close.addEventListener("click", () => {
    void invoke("close_preview");
  });
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      void invoke("close_preview");
    }
  });
  void invoke<PreviewFrame>("get_preview_frame")
    .then((frame) => {
      image.src = `data:image/png;base64,${frame.pngBase64}`;
    })
    .catch((error) => {
      const note = root.querySelector(".preview-note");
      if (note) {
        note.textContent = error instanceof Error ? error.message : String(error);
      }
    });
}
