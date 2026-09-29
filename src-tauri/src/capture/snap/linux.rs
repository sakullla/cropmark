//! Linux 元素级命中 provider 落点(snap-linux 任务实现 X11 窗口级命中)。
//!
//! 核心任务(`snap-core`)只注册降级 provider:检测不可用时返回空栈,
//! 区域选区保持自由框选,窗口级选择仍走现有窗口截取模式。后续实现用
//! X11 `_NET_CLIENT_LIST` 提供窗口级命中,Wayland 明确回退:控件级不可用时
//! 给出说明,Web 覆盖层只承担窗口级(无窗口列表时仅自由框选)。

use super::{SnapCapability, SnapContext, SnapProvider, SnapStack};

/// 平台 provider 单例(无状态)。
pub fn provider() -> &'static dyn SnapProvider {
    &LinuxSnapProvider
}

#[derive(Debug)]
struct LinuxSnapProvider;

impl SnapProvider for LinuxSnapProvider {
    fn capability(&self) -> SnapCapability {
        SnapCapability::Unavailable {
            reason_key: "error.capture.snap_unavailable",
        }
    }

    fn hit(&self, _context: SnapContext, _x: i32, _y: i32) -> SnapStack {
        SnapStack::empty()
    }
}
