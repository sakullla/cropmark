/// R2:radio 组键盘漫游共享实现,自 settings 闭包原样提取,行为不变。
/// 方向键/Home/End 移动焦点并选中目标项;选中走既有点击委托,
/// 与指针点击路径完全一致。settings 与 scroll 方向选择共同消费。
export function handleRadioGroupKeydown(
  event: KeyboardEvent,
  container: HTMLElement,
  selector: string,
): void {
  const keys = ["ArrowLeft", "ArrowUp", "ArrowRight", "ArrowDown", "Home", "End"];
  if (!keys.includes(event.key)) {
    return;
  }
  const buttons = Array.from(
    container.querySelectorAll<HTMLButtonElement>(selector),
  ).filter((button) => !button.disabled);
  if (buttons.length === 0) {
    return;
  }
  event.preventDefault();
  const currentIndex = buttons.indexOf(document.activeElement as HTMLButtonElement);
  let nextIndex: number;
  if (event.key === "Home") {
    nextIndex = 0;
  } else if (event.key === "End") {
    nextIndex = buttons.length - 1;
  } else {
    const delta = event.key === "ArrowLeft" || event.key === "ArrowUp" ? -1 : 1;
    nextIndex =
      currentIndex < 0
        ? delta > 0
          ? 0
          : buttons.length - 1
        : (currentIndex + delta + buttons.length) % buttons.length;
  }
  const target = buttons[nextIndex];
  target.focus();
  target.click();
}
