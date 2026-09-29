//! Windows 元素级命中 provider(R7):窗口级 `WindowFromPoint` + 控件级 UI
//! Automation。
//!
//! - 窗口级:命中点的顶层窗口套用 `platform::selectable_window`(与窗口列表
//!   过滤同源:可见、非最小化、非工具窗、无属主、非 DWM cloaked、有标题、非
//!   本进程)。冻结帧覆盖层铺满显示器且必然先被命中,此时按 z 序自顶向下枚举
//!   顶层窗口,取第一个覆盖该点的可截取窗口。
//! - 控件级:UI Automation 从目标窗口元素起逐层下探包含查询点的最深子元素,
//!   自最深控件到顶层窗口进栈,滚轮可切换层级。COM 初始化失败或 UIA 不可用时
//!   只返回窗口级(`SnapCapability::WindowOnly`),壳仍可自由框选。
//! - 坐标:进程启动即启用 Per-Monitor V2 DPI 感知,Win32 与 UIA 都返回屏幕
//!   物理像素;`SnapContext::to_screen` 把帧像素换算为屏幕像素,返回矩形不再
//!   加显示器偏移。

use std::cell::RefCell;
use std::mem::size_of;
use std::rc::Rc;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationCondition,
    IUIAutomationElement, TreeScope_Children, UIA_BoundingRectanglePropertyId,
    UIA_IsOffscreenPropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetDesktopWindow, GetWindowLongW, GetWindowRect, WindowFromPoint,
    GA_ROOT, GWL_EXSTYLE, WS_EX_TRANSPARENT,
};

use super::{SnapCapability, SnapContext, SnapHit, SnapKind, SnapProvider, SnapRect, SnapStack};
use crate::capture::platform::selectable_window;
use crate::capture::windows_list::ListedWindow;

/// 控件级不可用(仅窗口级)说明词条。
const CONTROL_FALLBACK_KEY: &str = "error.capture.snap_control_unavailable";
/// 检测不可用(自由框选)说明词条。
const UNAVAILABLE_KEY: &str = "error.capture.snap_unavailable";
/// 命中栈上限:最深控件 + 中间容器 + 顶层窗口。
const MAX_STACK_HITS: usize = 10;
/// UIA 自顶层窗口元素向下下探的最大层数。
const MAX_CONTROL_DEPTH: usize = 8;

/// 平台 provider 单例(无状态;UIA 客户端按线程缓存)。
pub fn provider() -> &'static dyn SnapProvider {
    &WindowsSnapProvider
}

#[derive(Debug)]
struct WindowsSnapProvider;

impl SnapProvider for WindowsSnapProvider {
    fn capability(&self) -> SnapCapability {
        capability_for(uia().is_some(), window_probe_available())
    }

    fn hit(&self, context: SnapContext, x: i32, y: i32) -> SnapStack {
        if !self.capability().window_level() {
            return SnapStack::empty();
        }
        let (screen_x, screen_y) = context.to_screen(x, y);
        let point = POINT {
            x: screen_x,
            y: screen_y,
        };
        // 探测失败(无窗口/属性读取失败)按"本次无命中"处理:壳保持自由框选,
        // 不 panic、不阻断交互。
        let Some((hwnd, window)) = hovered_window(point) else {
            return SnapStack::empty();
        };
        let window_hit = window_snap_hit(hwnd, &window);
        let controls = uia()
            .map(|uia| control_hits(&uia, hwnd, &window_hit.rect, point))
            .unwrap_or_default();
        build_stack(window_hit, controls)
    }
}

/// 能力判定(纯逻辑,便于单测):桌面不可探测时完全不可用走自由框选;
/// UIA 不可用时明确回退窗口级,不伪造控件级。
fn capability_for(uia_available: bool, window_probe_available: bool) -> SnapCapability {
    if !window_probe_available {
        return SnapCapability::Unavailable {
            reason_key: UNAVAILABLE_KEY,
        };
    }
    if uia_available {
        SnapCapability::Full
    } else {
        SnapCapability::WindowOnly {
            reason_key: CONTROL_FALLBACK_KEY,
        }
    }
}

/// 线程内复用的 UIA 客户端:COM 套间是线程属性,不能跨线程共享;首次探测
/// 失败后固定为 None,不在每次悬停重复初始化。
struct UiaContext {
    automation: IUIAutomation,
    children_cache: IUIAutomationCacheRequest,
    true_condition: IUIAutomationCondition,
}

thread_local! {
    static UIA: RefCell<Option<Option<Rc<UiaContext>>>> = const { RefCell::new(None) };
}

fn uia() -> Option<Rc<UiaContext>> {
    UIA.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(init_uia().map(Rc::new));
        }
        slot.as_ref().and_then(Option::clone)
    })
}

fn init_uia() -> Option<UiaContext> {
    unsafe {
        // S_FALSE = 本线程已初始化;RPC_E_CHANGED_MODE = 已按其它套间初始化,
        // 两者都继续用当前套间创建 UIA 客户端,不做 CoUninitialize 配对。
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if initialized.is_err() && initialized != RPC_E_CHANGED_MODE {
            return None;
        }
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
        let children_cache = automation.CreateCacheRequest().ok()?;
        // 缓存请求的 TreeScope 保持默认(Element):改成 Children 会让
        // FindAllBuildCache 返回的子元素成为"仅缓存"元素,连请求过的
        // 边界矩形都读不到(实测报"所需属性不在 CacheRequest 中")。
        children_cache
            .AddProperty(UIA_BoundingRectanglePropertyId)
            .ok()?;
        children_cache.AddProperty(UIA_IsOffscreenPropertyId).ok()?;
        let true_condition = automation.CreateTrueCondition().ok()?;
        Some(UiaContext {
            automation,
            children_cache,
            true_condition,
        })
    }
}

/// 窗口级探测的基本前提:进程附着在可枚举的交互桌面上。正常情况下恒真,
/// 仅会话隔离/服务等异常环境为假,能力退到 `Unavailable` 走自由框选。
fn window_probe_available() -> bool {
    unsafe {
        let desktop = GetDesktopWindow();
        if desktop.0.is_null() {
            return false;
        }
        let mut rect = RECT::default();
        GetWindowRect(desktop, &mut rect).is_ok() && rect_non_empty(&rect)
    }
}

/// z 序枚举的查询状态:查询点与首个命中(供枚举回调提前停止)。
struct ZOrderProbe {
    point: POINT,
    found: Option<(HWND, ListedWindow)>,
}

/// 悬停点下的可截取顶层窗口。`WindowFromPoint` 通常先命中本进程的冻结帧
/// 覆盖层(它铺满显示器),此时按 z 序自顶向下枚举全部顶层窗口,取第一个
/// 覆盖该点的可截取窗口;不能沿 `GW_HWNDNEXT` 逐窗下探——z 序里夹着大量
/// 不可见/工具窗(IME、弹窗、托盘等),到内容窗的距离不受控。
fn hovered_window(point: POINT) -> Option<(HWND, ListedWindow)> {
    unsafe {
        let hit = WindowFromPoint(point);
        if !hit.0.is_null() {
            let root = root_window(hit);
            if !root.0.is_null() {
                if let Some(window) = selectable_at(root, point) {
                    return Some((root, window));
                }
            }
        }
        let mut probe = ZOrderProbe { point, found: None };
        let param = LPARAM(&mut probe as *mut ZOrderProbe as isize);
        let _ = EnumWindows(Some(enum_underlay_window), param);
        probe.found
    }
}

/// 枚举回调:自顶向下取首个覆盖查询点、可截取、非本进程的顶层窗口后停止。
unsafe extern "system" fn enum_underlay_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let probe = &mut *(lparam.0 as *mut ZOrderProbe);
    if probe.found.is_some() {
        return BOOL::from(false);
    }
    if !covers_point(hwnd, probe.point) || is_click_through(hwnd) {
        return BOOL::from(true);
    }
    if let Some(window) = selectable_window(hwnd) {
        if !window.owner_is_self {
            probe.found = Some((hwnd, window));
            return BOOL::from(false);
        }
    }
    BOOL::from(true)
}

unsafe fn root_window(hwnd: HWND) -> HWND {
    let root = GetAncestor(hwnd, GA_ROOT);
    if root.0.is_null() {
        hwnd
    } else {
        root
    }
}

/// 窗口是否可作为吸附目标:覆盖查询点、通过列表同源过滤、且不是本进程窗口。
unsafe fn selectable_at(hwnd: HWND, point: POINT) -> Option<ListedWindow> {
    if !covers_point(hwnd, point) {
        return None;
    }
    let window = selectable_window(hwnd)?;
    if window.owner_is_self {
        return None;
    }
    Some(window)
}

unsafe fn covers_point(hwnd: HWND, point: POINT) -> bool {
    let mut rect = RECT::default();
    GetWindowRect(hwnd, &mut rect).is_ok() && rect_contains_point(&rect, point)
}

/// `WS_EX_TRANSPARENT` 窗口(区域框/HUD 等点击穿透覆盖层)不接收鼠标点击,
/// 也不作为吸附目标;命中窗口本身不做此过滤(Chromium 页面子窗带该风格)。
unsafe fn is_click_through(hwnd: HWND) -> bool {
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    ex & WS_EX_TRANSPARENT.0 != 0
}

/// 窗口层命中项:优先 DWM 扩展边框(与窗口截取所用区域一致),读取失败回退
/// `GetWindowRect`;标签用窗口标题。
fn window_snap_hit(hwnd: HWND, window: &ListedWindow) -> SnapHit {
    let mut rect = RECT::default();
    let extended = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
    };
    if extended.is_err() || !rect_non_empty(&rect) {
        rect = listed_window_rect(window);
    }
    SnapHit {
        kind: SnapKind::Window,
        rect: snap_rect_from_win32(rect),
        label: Some(window.title.clone()),
    }
}

fn listed_window_rect(window: &ListedWindow) -> RECT {
    RECT {
        left: window.x,
        top: window.y,
        right: window
            .x
            .saturating_add(window.width.min(i32::MAX as u32) as i32),
        bottom: window
            .y
            .saturating_add(window.height.min(i32::MAX as u32) as i32),
    }
}

/// 控件级命中(自最深到窗口方向):从目标窗口元素起,每层取第一个包含查询
/// 点的子元素继续下探,经过的层级全部进栈。UIA 元素坐标在 Per-Monitor V2
/// 下即屏幕物理像素,与帧坐标契约一致。
fn control_hits(context: &UiaContext, hwnd: HWND, window: &SnapRect, point: POINT) -> Vec<SnapHit> {
    unsafe {
        let Ok(root) = context.automation.ElementFromHandle(hwnd) else {
            return Vec::new();
        };
        let window_rect = rect_from_snap(window);
        let mut deepest_first: Vec<SnapHit> = Vec::new();
        let mut current = root;
        for _ in 0..MAX_CONTROL_DEPTH {
            let Some((child, rect)) = child_at(context, &current, point, &window_rect) else {
                break;
            };
            deepest_first.push(SnapHit {
                kind: SnapKind::Control,
                rect: snap_rect_from_win32(rect),
                label: element_label(&child),
            });
            current = child;
        }
        deepest_first.reverse();
        deepest_first
    }
}

/// 目标窗口元素下第一个包含查询点且与窗口相交的子元素。属性走请求过的
/// 缓存(一次跨进程调用取回整层),provider 不支持缓存时回退实时读取。
unsafe fn child_at(
    context: &UiaContext,
    parent: &IUIAutomationElement,
    point: POINT,
    window: &RECT,
) -> Option<(IUIAutomationElement, RECT)> {
    let children = parent
        .FindAllBuildCache(
            TreeScope_Children,
            &context.true_condition,
            &context.children_cache,
        )
        .ok()?;
    let count = children.Length().ok()?;
    for index in 0..count {
        let Ok(child) = children.GetElement(index) else {
            continue;
        };
        let Some(rect) = element_rect(&child) else {
            continue;
        };
        if !rect_contains_point(&rect, point) || is_offscreen(&child) {
            continue;
        }
        // 越出目标窗口的子元素不属于本次悬停(跨屏工具窗等)。
        if !rect_intersects(&rect, window) {
            continue;
        }
        return Some((child, rect));
    }
    None
}

unsafe fn element_rect(element: &IUIAutomationElement) -> Option<RECT> {
    element
        .CachedBoundingRectangle()
        .ok()
        .filter(rect_non_empty)
        .or_else(|| {
            element
                .CurrentBoundingRectangle()
                .ok()
                .filter(rect_non_empty)
        })
}

unsafe fn is_offscreen(element: &IUIAutomationElement) -> bool {
    element
        .CachedIsOffscreen()
        .or_else(|_| element.CurrentIsOffscreen())
        .map(|value| value.as_bool())
        .unwrap_or(false)
}

unsafe fn element_label(element: &IUIAutomationElement) -> Option<String> {
    element
        .CurrentName()
        .ok()
        .map(|name| name.to_string())
        .filter(|name| !name.trim().is_empty())
}

/// 组装命中栈(纯逻辑,便于单测):控件层自最深到外层,裁进窗口层并去重、
/// 限深,顶层窗口恒为栈尾。无窗口命中时 provider 直接返回空栈(自由框选),
/// 栈非空则至少含窗口层。
fn build_stack(window: SnapHit, controls: Vec<SnapHit>) -> SnapStack {
    let mut hits: Vec<SnapHit> = Vec::new();
    for control in controls {
        let Some(rect) = clip_snap_rect(&control.rect, &window.rect) else {
            continue;
        };
        if rect == window.rect || hits.iter().any(|hit| hit.rect == rect) {
            continue;
        }
        hits.push(SnapHit {
            kind: control.kind,
            rect,
            label: control.label,
        });
        if hits.len() + 1 >= MAX_STACK_HITS {
            break;
        }
    }
    hits.push(window);
    SnapStack::new(hits)
}

/// `inner` 裁进 `outer`;完全越界或裁剪后不足 1×1 返回 None。
fn clip_snap_rect(inner: &SnapRect, outer: &SnapRect) -> Option<SnapRect> {
    let left = i64::from(inner.x).max(i64::from(outer.x));
    let top = i64::from(inner.y).max(i64::from(outer.y));
    let right = (i64::from(inner.x) + i64::from(inner.width))
        .min(i64::from(outer.x) + i64::from(outer.width));
    let bottom = (i64::from(inner.y) + i64::from(inner.height))
        .min(i64::from(outer.y) + i64::from(outer.height));
    if right - left < 1 || bottom - top < 1 {
        return None;
    }
    Some(SnapRect::new(
        left as i32,
        top as i32,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

fn snap_rect_from_win32(rect: RECT) -> SnapRect {
    let width = (i64::from(rect.right) - i64::from(rect.left)).clamp(0, i64::from(u32::MAX)) as u32;
    let height =
        (i64::from(rect.bottom) - i64::from(rect.top)).clamp(0, i64::from(u32::MAX)) as u32;
    SnapRect::new(rect.left, rect.top, width, height)
}

fn rect_from_snap(rect: &SnapRect) -> RECT {
    RECT {
        left: rect.x,
        top: rect.y,
        right: rect
            .x
            .saturating_add(rect.width.min(i32::MAX as u32) as i32),
        bottom: rect
            .y
            .saturating_add(rect.height.min(i32::MAX as u32) as i32),
    }
}

fn rect_non_empty(rect: &RECT) -> bool {
    rect.right > rect.left && rect.bottom > rect.top
}

/// 左闭右开包含判定,与命中测试惯例一致。
fn rect_contains_point(rect: &RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

fn rect_intersects(left: &RECT, right: &RECT) -> bool {
    left.left.max(right.left) < left.right.min(right.right)
        && left.top.max(right.top) < left.bottom.min(right.bottom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::snap::SnapStep;

    fn hit(kind: SnapKind, x: i32, y: i32, width: u32, height: u32, label: &str) -> SnapHit {
        SnapHit {
            kind,
            rect: SnapRect::new(x, y, width, height),
            label: Some(label.into()),
        }
    }

    fn window_hit(x: i32, y: i32, width: u32, height: u32) -> SnapHit {
        hit(SnapKind::Window, x, y, width, height, "Notes")
    }

    #[test]
    fn stack_orders_deepest_control_first_then_ancestors_then_window() {
        let window = window_hit(100, 100, 400, 300);
        let stack = build_stack(
            window.clone(),
            vec![
                hit(SnapKind::Control, 160, 160, 40, 24, "ok-button"),
                hit(SnapKind::Control, 120, 120, 200, 160, "dialog"),
            ],
        );
        let labels: Vec<_> = stack.hits().iter().map(|h| h.label.as_deref()).collect();
        assert_eq!(labels, [Some("ok-button"), Some("dialog"), Some("Notes")]);
        assert_eq!(stack.hits()[2].kind, SnapKind::Window);
        // 默认高亮最深的控件;滚轮 Parent 逐层切到外层直到顶层窗口。
        assert_eq!(stack.default_index(), Some(0));
        assert_eq!(stack.step(0, SnapStep::Parent), 1);
        assert_eq!(stack.step(1, SnapStep::Parent), 2);
    }

    #[test]
    fn stack_clips_controls_to_window_and_drops_duplicates() {
        let window = window_hit(100, 100, 200, 100);
        let stack = build_stack(
            window.clone(),
            vec![
                // 裁剪后与窗口层同矩形,重复层级不再进栈。
                hit(SnapKind::Control, 90, 90, 300, 200, "window-sized"),
                hit(SnapKind::Control, 150, 150, 30, 20, "button"),
                // 包裹同一区域的嵌套容器:只保留最深层。
                hit(SnapKind::Control, 150, 150, 30, 20, "button-wrapper"),
                // 零尺寸与完全越界都不进栈。
                hit(SnapKind::Control, 150, 150, 0, 20, "zero"),
                hit(SnapKind::Control, 500, 500, 10, 10, "outside"),
                // 部分越界:裁进窗口内。
                hit(SnapKind::Control, 280, 180, 100, 100, "clipped"),
            ],
        );
        let labels: Vec<_> = stack.hits().iter().map(|h| h.label.as_deref()).collect();
        assert_eq!(labels, [Some("button"), Some("clipped"), Some("Notes")]);
        assert_eq!(stack.hits()[1].rect, SnapRect::new(280, 180, 20, 20));
    }

    #[test]
    fn stack_caps_control_levels_and_keeps_window_last() {
        let controls = (0..32)
            .map(|index| hit(SnapKind::Control, index * 10, 0, 8, 8, "level"))
            .collect();
        let stack = build_stack(window_hit(0, 0, 1000, 1000), controls);
        assert_eq!(stack.len(), MAX_STACK_HITS);
        assert_eq!(stack.hits()[0].kind, SnapKind::Control);
        assert_eq!(stack.hits()[MAX_STACK_HITS - 1].kind, SnapKind::Window);
    }

    #[test]
    fn uia_unavailable_degrades_to_window_level_stack() {
        // UIA 不可用(无控件层):栈只剩窗口级,默认高亮与点击确认仍对窗口生效。
        let window = window_hit(-100, -50, 800, 600);
        let stack = build_stack(window.clone(), Vec::new());
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.hits()[0], window);
        assert_eq!(stack.default_index(), Some(0));
    }

    #[test]
    fn capability_falls_back_from_control_to_window_to_free_selection() {
        assert_eq!(capability_for(true, true), SnapCapability::Full);

        let window_only = capability_for(false, true);
        assert!(window_only.window_level());
        assert!(!window_only.control_level());
        assert_eq!(window_only.reason_key(), Some(CONTROL_FALLBACK_KEY));

        // 桌面不可探测时连窗口级都不宣称,壳走自由框选说明。
        let unavailable = capability_for(true, false);
        assert!(!unavailable.window_level());
        assert!(!unavailable.control_level());
        assert_eq!(unavailable.reason_key(), Some(UNAVAILABLE_KEY));
    }

    #[test]
    fn clipping_handles_negative_monitor_origins() {
        let outer = SnapRect::new(-1920, 0, 1920, 1080);
        let inner = SnapRect::new(-1930, -10, 40, 40);
        assert_eq!(
            clip_snap_rect(&inner, &outer),
            Some(SnapRect::new(-1920, 0, 30, 30))
        );
        assert_eq!(
            clip_snap_rect(&SnapRect::new(-2500, 0, 100, 100), &outer),
            None
        );
    }
}
