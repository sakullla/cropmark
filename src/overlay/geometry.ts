// 工作区显示几何:冻结帧到覆盖层画布的唯一映射来源(R2)。
//
// fixed 工作区的画布位图是帧的等比显示框:等比缩放到窗口内、居中,且不超过
// 1:1 物理像素(不放大、不压扁)。其余模式画布铺满窗口,整屏冻结帧随之铺满。
// 图像、标注层与取字三态都画在同一张画布上,`canvas.width / frame.width`
// 就是帧→画布比例,因此非显示器尺寸定帧(选区裁剪帧、窗口、上次区域、长截图)
// 不再被非等比拉伸,也不依赖"显示器逻辑尺寸×devicePixelRatio 等于帧尺寸"的假设。

export interface FrameGeometry {
  width: number;
  height: number;
  /** 画面已定:按帧等比显示;否则整屏铺满。 */
  fixed: boolean;
}

export interface WindowGeometry {
  width: number;
  height: number;
}

export interface CanvasGeometry {
  /** 画布位图尺寸(物理像素)。 */
  width: number;
  height: number;
  /** 画布 CSS 尺寸与相对宿主的左上角偏移(CSS 像素)。 */
  cssWidth: number;
  cssHeight: number;
  left: number;
  top: number;
}

function positive(value: number, fallback: number): number {
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

export function canvasGeometry(
  frame: FrameGeometry,
  host: WindowGeometry,
  dpr: number,
): CanvasGeometry {
  const ratio = positive(dpr, 1);
  const hostWidth = positive(host.width, 1);
  const hostHeight = positive(host.height, 1);
  const windowWidth = Math.max(1, Math.round(hostWidth * ratio));
  const windowHeight = Math.max(1, Math.round(hostHeight * ratio));
  let width = windowWidth;
  let height = windowHeight;
  if (frame.fixed) {
    const frameWidth = positive(frame.width, 1);
    const frameHeight = positive(frame.height, 1);
    const scale = Math.min(1, windowWidth / frameWidth, windowHeight / frameHeight);
    width = Math.max(1, Math.round(frameWidth * scale));
    height = Math.max(1, Math.round(frameHeight * scale));
  }
  const cssWidth = width / ratio;
  const cssHeight = height / ratio;
  return {
    width,
    height,
    cssWidth,
    cssHeight,
    left: Math.max(0, (hostWidth - cssWidth) / 2),
    top: Math.max(0, (hostHeight - cssHeight) / 2),
  };
}
