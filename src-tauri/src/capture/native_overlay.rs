//! Windows 区域选区壳入口。
//!
//! 自绘选区逻辑已迁移至 `capture/selection/shell/windows.rs`,由平台无关
//! 选区引擎驱动(ADR-001/006);本文件保留原模块路径,把壳实现挂入编译,
//! 避免 `selection/mod.rs`(引擎任务所有)在本任务中改动模块树。
//! 壳职责仅限:Win32 全屏置顶窗、消息泵、事件转发(物理像素坐标)与
//! StretchDIBits 呈现;副作用(剪贴板/toast)经 `ShellHooks` 由会话层注入。

#[path = "selection/shell/windows.rs"]
mod imp;

pub use imp::{pick_region, RegionOutcome, ShellHooks};
