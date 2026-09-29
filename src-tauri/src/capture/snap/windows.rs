//! Windows 元素级命中 provider 落点(snap-windows 任务实现窗口级与控件级)。
//!
//! 核心任务(`snap-core`)只注册降级 provider:检测不可用时返回空栈,
//! 区域选区保持自由框选,窗口级选择仍走现有窗口截取模式。控件级将使用
//! `WindowFromPoint` + `EnumChildWindows` 兜底与 UI Automation
//! (`windows` crate 的 `Win32_UI_Accessibility` feature,已在 Cargo.toml 预留)。

use super::{SnapCapability, SnapContext, SnapProvider, SnapStack};

/// 平台 provider 单例(无状态)。
pub fn provider() -> &'static dyn SnapProvider {
    &WindowsSnapProvider
}

#[derive(Debug)]
struct WindowsSnapProvider;

impl SnapProvider for WindowsSnapProvider {
    fn capability(&self) -> SnapCapability {
        SnapCapability::Unavailable {
            reason_key: "error.capture.snap_unavailable",
        }
    }

    fn hit(&self, _context: SnapContext, _x: i32, _y: i32) -> SnapStack {
        SnapStack::empty()
    }
}
