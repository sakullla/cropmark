//! 平台区域选区壳的分发入口(ADR-008 三平台接线)。
//!
//! 各平台壳实现位于 `capture/selection/shell/`,`#[path]` 挂入本模块参与
//! 编译,`selection/mod.rs`(引擎所有)不感知平台模块树。三份壳实现保持
//! 同一导出契约:`pick_region(frame, monitor, flags, hooks)` 返回
//! `RegionOutcome`,副作用(色值复制)经 `ShellHooks` 由会话层注入。
//! 壳职责:置顶全屏窗、事件泵、事件转发(物理像素坐标)与合成位图呈现
//! (Windows: Win32+StretchDIBits;macOS: NSWindow+CG;Linux: X11
//! override-redirect + MIT-SHM/XPutImage)。Wayland 会话的区域路径不经
//! 本模块,由 session 层回退 Web 覆盖层(ADR-008)。

#[cfg(windows)]
#[path = "selection/shell/windows.rs"]
mod imp;
#[cfg(target_os = "macos")]
#[path = "selection/shell/macos.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "selection/shell/linux.rs"]
mod imp;

pub use imp::{pick_region, RegionOutcome, ShellHooks};
