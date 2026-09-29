//! R7:元素级吸附抽象。命中栈自最深控件到顶层窗口,平台实现见
//! `windows` / `macos` / `linux` 子模块(平台任务填充;本任务只注册降级
//! provider 并接通壳与覆盖层)。
//!
//! 坐标契约:provider 收到 `SnapContext`(显示器物理/逻辑原点与缩放)与
//! **帧像素**查询点,返回矩形用**屏幕物理像素**;壳侧 `FrameSnap` 统一
//! 平移到帧坐标并裁进帧内,引擎只消费帧坐标。

// 命中栈与能力枚举是 snap-windows/snap-macos/snap-linux 的公共落点;核心
// 任务只走到降级分支,先整体豁免 dead_code,平台实现接入后移除。
#![allow(dead_code)]

use std::fmt::Debug;

use super::geometry::PhysicalRect;

/// 命中项层级:控件(更深)或窗口(顶层)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapKind {
    Control,
    Window,
}

/// 屏幕坐标矩形(物理像素,原点可为负:副屏可位于主屏左侧/上方)。
/// 平移到帧坐标后由 `clamped_to_frame` 转为非负的 `PhysicalRect`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl SnapRect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn right(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    fn bottom(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }
}

/// 命中栈中的一项(矩形为屏幕物理像素,可为负)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapHit {
    pub kind: SnapKind,
    pub rect: SnapRect,
    /// 可读名称(窗口标题/控件名);缺失时为 None。
    pub label: Option<String>,
}

/// 平台查询上下文:显示器几何与缩放,平台 provider 用它把帧像素坐标
/// 换算到系统坐标系(macOS 的 CGWindowList/AX 用逻辑点;Windows/X11 用物理像素)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapContext {
    /// 显示器物理原点(帧左上角的屏幕物理像素坐标,可为负)。
    pub origin: (i32, i32),
    /// 显示器逻辑原点(系统点坐标系;与 `scale` 一起还原物理坐标)。
    pub logical_origin: (i32, i32),
    /// 帧 DPI 缩放(逻辑 → 物理)。
    pub scale: f64,
}

impl SnapContext {
    pub fn new(origin: (i32, i32), logical_origin: (i32, i32), scale: f64) -> Self {
        Self {
            origin,
            logical_origin,
            scale: if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                1.0
            },
        }
    }

    /// 帧像素点 → 屏幕物理像素。
    pub fn to_screen(self, x: i32, y: i32) -> (i32, i32) {
        (
            x.saturating_add(self.origin.0),
            y.saturating_add(self.origin.1),
        )
    }

    /// 帧像素点 → 显示器逻辑点(系统点坐标)。
    pub fn to_logical(self, x: i32, y: i32) -> (i32, i32) {
        (
            self.logical_origin.0 + (f64::from(x) / self.scale).round() as i32,
            self.logical_origin.1 + (f64::from(y) / self.scale).round() as i32,
        )
    }
}

/// 层级切换方向:父子层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapStep {
    /// 父级方向:向顶层窗口移动(命中栈索引 +1)。
    Parent,
    /// 子级方向:向更深的控件移动(命中栈索引 -1)。
    Child,
}

/// 命中栈:自最深控件到顶层窗口。同一坐标重叠的窗口/嵌套控件都进栈。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapStack {
    hits: Vec<SnapHit>,
}

impl SnapStack {
    pub fn new(hits: Vec<SnapHit>) -> Self {
        Self { hits }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn hits(&self) -> &[SnapHit] {
        &self.hits
    }

    pub fn len(&self) -> usize {
        self.hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&SnapHit> {
        self.hits.get(index)
    }

    /// 默认高亮层级:最深控件(栈首)。空栈返回 None。
    pub fn default_index(&self) -> Option<usize> {
        if self.hits.is_empty() {
            None
        } else {
            Some(0)
        }
    }

    /// 层级切换(端点钳制,不循环)。
    pub fn step(&self, index: usize, step: SnapStep) -> usize {
        match step {
            SnapStep::Parent => (index + 1).min(self.hits.len().saturating_sub(1)),
            SnapStep::Child => index.saturating_sub(1),
        }
    }

    /// 平移到冻结帧坐标系(`origin` 为显示器物理原点,可为负),并裁掉
    /// 完全越界或裁剪后不足 1×1 的项;结果矩形均为非负帧坐标。
    pub fn clamped_to_frame(self, origin: (i32, i32), frame_width: u32, frame_height: u32) -> Self {
        let mut hits = Vec::with_capacity(self.hits.len());
        for mut hit in self.hits {
            let left = (i64::from(hit.rect.x) - i64::from(origin.0)).max(0);
            let top = (i64::from(hit.rect.y) - i64::from(origin.1)).max(0);
            let right = hit.rect.right() - i64::from(origin.0);
            let bottom = hit.rect.bottom() - i64::from(origin.1);
            let right = right.min(i64::from(frame_width));
            let bottom = bottom.min(i64::from(frame_height));
            if right - left < 1 || bottom - top < 1 {
                continue;
            }
            hit.rect = SnapRect {
                x: left as i32,
                y: top as i32,
                width: (right - left) as u32,
                height: (bottom - top) as u32,
            };
            hits.push(hit);
        }
        Self { hits }
    }

    /// 当前高亮项在帧坐标下的非负矩形(引擎/合成器消费)。
    pub fn frame_rect(&self, index: usize) -> Option<PhysicalRect> {
        let hit = self.hits.get(index)?;
        if hit.rect.x < 0 || hit.rect.y < 0 {
            return None;
        }
        Some(PhysicalRect {
            x: hit.rect.x as u32,
            y: hit.rect.y as u32,
            width: hit.rect.width,
            height: hit.rect.height,
        })
    }
}

/// 平台元素检测能力。检测不可用时壳仍保留自由框选;窗口级回退由窗口选择
/// 模式承担,控件级降级用 `reason_key` 走现有能力说明机制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapCapability {
    /// 控件级与窗口级命中都可用。
    Full,
    /// 仅窗口级可用(控件级不可用/未授权);`reason_key` 为本地化说明词条。
    WindowOnly { reason_key: &'static str },
    /// 检测不可用,仅自由框选;`reason_key` 为本地化说明词条。
    Unavailable { reason_key: &'static str },
}

impl SnapCapability {
    pub fn window_level(self) -> bool {
        !matches!(self, Self::Unavailable { .. })
    }

    pub fn control_level(self) -> bool {
        matches!(self, Self::Full)
    }

    /// 降级说明词条;完整能力时为 None。
    pub fn reason_key(self) -> Option<&'static str> {
        match self {
            Self::Full => None,
            Self::WindowOnly { reason_key } | Self::Unavailable { reason_key } => Some(reason_key),
        }
    }
}

/// 平台命中 provider。实现不得 panic;检测不可用时返回 `Unavailable` 与空栈。
pub trait SnapProvider: Debug + Send + Sync {
    fn capability(&self) -> SnapCapability;

    /// 查询帧像素 (x, y) 处的命中栈,自最深控件到顶层窗口;返回矩形为
    /// 屏幕物理像素(`SnapContext::to_screen` 可换算查询点)。
    /// 不可用时返回空栈。
    fn hit(&self, context: SnapContext, x: i32, y: i32) -> SnapStack;
}

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

/// 平台 provider 注册;每平台唯一入口,后续平台任务替换各自子模块实现。
#[cfg(windows)]
pub fn platform_provider() -> &'static dyn SnapProvider {
    windows::provider()
}

#[cfg(target_os = "macos")]
pub fn platform_provider() -> &'static dyn SnapProvider {
    macos::provider()
}

#[cfg(target_os = "linux")]
pub fn platform_provider() -> &'static dyn SnapProvider {
    linux::provider()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn platform_provider() -> &'static dyn SnapProvider {
    &NoPlatformProvider
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
#[derive(Debug)]
struct NoPlatformProvider;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
impl SnapProvider for NoPlatformProvider {
    fn capability(&self) -> SnapCapability {
        SnapCapability::Unavailable {
            reason_key: "error.capture.snap_unavailable",
        }
    }

    fn hit(&self, _context: SnapContext, _x: i32, _y: i32) -> SnapStack {
        SnapStack::empty()
    }
}

/// 当前平台元素检测能力(会话层/能力说明消费)。
pub fn platform_capability() -> SnapCapability {
    platform_provider().capability()
}

/// 冻结帧坐标适配器:壳注入 provider 与显示器几何,引擎按帧坐标查询。
#[derive(Debug, Clone, Copy)]
pub struct FrameSnap {
    provider: &'static dyn SnapProvider,
    context: SnapContext,
    frame: (u32, u32),
}

impl FrameSnap {
    pub fn new(
        provider: &'static dyn SnapProvider,
        context: SnapContext,
        frame: (u32, u32),
    ) -> Self {
        Self {
            provider,
            context,
            frame,
        }
    }

    pub fn capability(self) -> SnapCapability {
        self.provider.capability()
    }

    /// 帧坐标入参;返回已平移到帧坐标并裁进帧内的命中栈。
    pub fn hit(self, x: i32, y: i32) -> SnapStack {
        if !self.capability().window_level() {
            return SnapStack::empty();
        }
        self.provider.hit(self.context, x, y).clamped_to_frame(
            self.context.origin,
            self.frame.0,
            self.frame.1,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: SnapKind, x: i32, y: i32, width: u32, height: u32, label: &str) -> SnapHit {
        SnapHit {
            kind,
            rect: SnapRect::new(x, y, width, height),
            label: Some(label.into()),
        }
    }

    /// 命中栈:自最深控件到顶层窗口。
    fn sample_stack() -> SnapStack {
        SnapStack::new(vec![
            hit(SnapKind::Control, 120, 80, 30, 24, "ok-button"),
            hit(SnapKind::Control, 100, 60, 200, 120, "dialog"),
            hit(SnapKind::Window, 0, 0, 800, 600, "Notes"),
        ])
    }

    #[test]
    fn stack_keeps_deepest_control_first_and_defaults_to_it() {
        let stack = sample_stack();
        assert_eq!(stack.len(), 3);
        assert_eq!(stack.hits()[0].kind, SnapKind::Control);
        assert_eq!(stack.hits()[2].kind, SnapKind::Window);
        assert_eq!(stack.hits()[2].label.as_deref(), Some("Notes"));
        // 悬停按钮/输入框时高亮控件本身,默认层级是最深项。
        assert_eq!(stack.default_index(), Some(0));
        assert_eq!(
            stack
                .get(stack.default_index().unwrap())
                .unwrap()
                .label
                .as_deref(),
            Some("ok-button")
        );
        assert_eq!(SnapStack::empty().default_index(), None);
    }

    #[test]
    fn wheel_switches_between_parent_and_child_levels_with_clamping() {
        let stack = sample_stack();
        // 默认最深控件 → 父级(对话框)→ 顶层窗口;两端钳制不越界。
        let dialog = stack.step(0, SnapStep::Parent);
        assert_eq!(dialog, 1);
        let window = stack.step(dialog, SnapStep::Parent);
        assert_eq!(window, 2);
        assert_eq!(stack.step(window, SnapStep::Parent), 2);
        // 子级方向逐步回到最深控件。
        assert_eq!(stack.step(window, SnapStep::Child), 1);
        assert_eq!(stack.step(1, SnapStep::Child), 0);
        assert_eq!(stack.step(0, SnapStep::Child), 0);
    }

    #[test]
    fn clamping_rebases_to_frame_and_drops_offscreen_hits() {
        // 显示器原点为负(副屏在主屏左侧):屏幕坐标需平移到帧坐标。
        let stack = SnapStack::new(vec![
            hit(SnapKind::Window, -100, -50, 900, 700, "Notes"),
            hit(SnapKind::Window, -200, 0, 100, 100, "off-screen"),
            hit(SnapKind::Control, 10, 10, 20, 20, "button"),
        ]);
        let clamped = stack.clamped_to_frame((-100, -50), 800, 600);
        assert_eq!(clamped.len(), 2);
        assert_eq!(clamped.hits()[0].rect, SnapRect::new(0, 0, 800, 600));
        assert_eq!(clamped.hits()[1].rect, SnapRect::new(110, 60, 20, 20));
        // 帧矩形为非负物理像素,供引擎/合成器消费。
        assert_eq!(
            clamped.frame_rect(0),
            Some(PhysicalRect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            })
        );
        assert_eq!(
            clamped.frame_rect(1),
            Some(PhysicalRect {
                x: 110,
                y: 60,
                width: 20,
                height: 20,
            })
        );
        assert_eq!(clamped.frame_rect(2), None);
    }

    #[test]
    fn frame_snap_queries_screen_coordinates_and_reports_frame_rects() {
        static PROVIDER: TestProvider = TestProvider;

        #[derive(Debug)]
        struct TestProvider;

        impl SnapProvider for TestProvider {
            fn capability(&self) -> SnapCapability {
                SnapCapability::Full
            }

            fn hit(&self, context: SnapContext, x: i32, y: i32) -> SnapStack {
                // 查询点按帧坐标传入,provider 自行换算到屏幕坐标。
                assert_eq!(context.to_screen(x, y), (150, 90));
                SnapStack::new(vec![hit(SnapKind::Window, 120, 60, 200, 100, "Notes")])
            }
        }

        let context = SnapContext::new((100, 50), (80, 40), 1.0);
        let snap = FrameSnap::new(&PROVIDER, context, (400, 300));
        assert_eq!(snap.capability(), SnapCapability::Full);
        let stack = snap.hit(50, 40);
        // 返回矩形是屏幕物理像素(120,60),适配后平移到帧坐标 (20,10)。
        assert_eq!(stack.hits()[0].rect, SnapRect::new(20, 10, 200, 100));
    }

    #[test]
    fn snap_context_maps_frame_points_to_screen_and_logical_points() {
        let context = SnapContext::new((-200, -100), (-100, -50), 2.0);
        assert_eq!(context.to_screen(10, 20), (-190, -80));
        assert_eq!(context.to_logical(20, 40), (-90, -30));
        // 非法缩放回退 1.0,不产生除零/NaN。
        assert_eq!(SnapContext::new((0, 0), (0, 0), 0.0).scale, 1.0);
    }

    #[test]
    fn unavailable_capability_reports_window_only_or_free_selection_fallback() {
        assert!(SnapCapability::Full.window_level());
        assert!(SnapCapability::Full.control_level());
        assert_eq!(SnapCapability::Full.reason_key(), None);

        let window_only = SnapCapability::WindowOnly {
            reason_key: "error.capture.snap_control_unavailable",
        };
        assert!(window_only.window_level());
        assert!(!window_only.control_level());
        assert_eq!(
            window_only.reason_key(),
            Some("error.capture.snap_control_unavailable")
        );

        let unavailable = SnapCapability::Unavailable {
            reason_key: "error.capture.snap_unavailable",
        };
        assert!(!unavailable.window_level());
        assert!(!unavailable.control_level());
        assert_eq!(
            unavailable.reason_key(),
            Some("error.capture.snap_unavailable")
        );
    }

    #[test]
    fn unavailable_provider_returns_an_empty_stack_for_free_selection() {
        static PROVIDER: UnavailableProvider = UnavailableProvider;

        #[derive(Debug)]
        struct UnavailableProvider;

        impl SnapProvider for UnavailableProvider {
            fn capability(&self) -> SnapCapability {
                SnapCapability::Unavailable {
                    reason_key: "error.capture.snap_unavailable",
                }
            }

            fn hit(&self, _context: SnapContext, _x: i32, _y: i32) -> SnapStack {
                SnapStack::empty()
            }
        }

        let context = SnapContext::new((0, 0), (0, 0), 1.0);
        let snap = FrameSnap::new(&PROVIDER, context, (800, 600));
        assert!(!snap.capability().window_level());
        assert!(snap.hit(10, 10).is_empty());
    }
}
