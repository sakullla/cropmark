import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./toast.css";

interface ToastPayload {
  message: string;
}

export function mountToast(root: HTMLElement): () => void {
  root.className = "toast-root";
  // 纯提示窗：消息由 Rust 定位到工作区右下角并 always-on-top；窗口本身无交互。
  // role=status + aria-live 让屏幕阅读器按状态通告读出结果,与就地提示同一层级。
  root.innerHTML = '<p class="toast-message" role="status" aria-live="polite"></p>';
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
