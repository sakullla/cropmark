//! macOS 元素级命中 provider 落点(snap-macos 任务实现窗口级与 AX 控件级)。
//!
//! 核心任务(`snap-core`)只注册降级 provider:检测不可用时返回空栈,
//! 区域选区保持自由框选,窗口级选择仍走现有窗口截取模式。后续实现用
//! `CGWindowListCopyWindowInfo` 提供窗口级、Accessibility API(AXUIElement)
//! 提供控件级,`AXIsProcessTrusted` 未授权时降级窗口级并走能力说明;
//! AX 框架链接已在 build.rs 预留(ApplicationServices)。

use super::{SnapCapability, SnapContext, SnapProvider, SnapStack};

/// 平台 provider 单例(无状态)。
pub fn provider() -> &'static dyn SnapProvider {
    &MacosSnapProvider
}

#[derive(Debug)]
struct MacosSnapProvider;

impl SnapProvider for MacosSnapProvider {
    fn capability(&self) -> SnapCapability {
        SnapCapability::Unavailable {
            reason_key: "error.capture.snap_unavailable",
        }
    }

    fn hit(&self, _context: SnapContext, _x: i32, _y: i32) -> SnapStack {
        SnapStack::empty()
    }
}
