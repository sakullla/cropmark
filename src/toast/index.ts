import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./toast.css";

interface ToastPayload {
  message: string;
}

export function mountToast(root: HTMLElement): () => void {
  root.className = "toast-root";
  root.innerHTML = '<p class="toast-message"></p>';
  const message = root.querySelector(".toast-message");
  if (!(message instanceof HTMLElement)) {
    return () => undefined;
  }

  const render = (payload: ToastPayload | null): void => {
    message.textContent = payload?.message ?? "";
  };

  void invoke<ToastPayload | null>("get_toast_message").then(render);
  void listen<ToastPayload>("capture-toast", (event) => render(event.payload));

  // 语言切换:Rust 侧 toast 按词条重新解析,重新拉取最新文案。
  return () => {
    void invoke<ToastPayload | null>("get_toast_message").then(render);
  };
}
