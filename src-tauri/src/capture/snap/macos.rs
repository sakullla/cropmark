//! macOS 元素级命中 provider:窗口级 `CGWindowListCopyWindowInfo`,控件级
//! Accessibility API(AXUIElement)。native 取值由 `macos_sck.m` 的
//! `cropmark_snap_*` 完成,本模块只做坐标换算、过滤与命中栈组装(纯逻辑
//! 可单测)。
//!
//! 坐标契约:壳传入 `SnapContext` 与帧像素查询点。`SnapContext.origin` 是
//! `NSScreen.frame` 原点 × backingScale(AppKit 底原点、y 向上),帧内 y
//! 向下;这里先把帧像素按 `scale` 还原为显示器内逻辑点,再翻成 CG 全局点
//! 坐标(y 向下,主屏左上为原点)查询,返回矩形换算回 `SnapContext.origin`
//! 同基的屏幕物理像素,由 `FrameSnap` 平移裁进帧内。
//!
//! 控件级需要辅助功能授权:`capability` 用 `AXIsProcessTrusted` 预检,未授权
//! 返回 `WindowOnly`(说明词条沿用
//! `error.capture.snap_control_unavailable`);进入选区后首次实际命中时经
//! `AXIsProcessTrustedWithOptions` 给出一次系统授权提示(系统本地化文案),
//! 随后自动降级窗口级。窗口级依赖屏幕录制权限
//! (`CGPreflightScreenCaptureAccess`,截取流程已具备),不具备时返回
//! `Unavailable`,沿用自由框选说明。
//!
//! 命中栈自最深控件到顶层窗口:AX 链取指针处最深元素并逐级向上收集,
//! 顶层窗口由 CGWindowList 的前后序提供;同坐标重叠的窗口自顶向下依次入栈
//! 供滚轮切层级。AX 消息超时压到 250ms、无响应应用不拖住选区壳;任何 native
//! 调用失败都按本次无命中(空栈)降级,不 panic、不阻断选区交互。

use std::os::raw::c_char;

use super::{SnapCapability, SnapContext, SnapHit, SnapKind, SnapProvider, SnapRect, SnapStack};

/// 控件级降级说明词条(现有能力说明机制,catalog.json 已本地化)。
const CONTROL_FALLBACK_KEY: &str = "error.capture.snap_control_unavailable";
/// 窗口级/检测不可用说明词条(自由框选回退)。
const UNAVAILABLE_KEY: &str = "error.capture.snap_unavailable";

/// AX 链入栈上限(不含顶层窗口)。
const MAX_CONTROL_HITS: usize = 16;
/// 同坐标重叠窗口入栈上限(CGWindowList 前后序自顶向下)。
const MAX_WINDOW_HITS: usize = 8;
/// 标题/标签缓冲长度(与 `macos_sck.m` 结构体一致)。
const TITLE_CAP: usize = 256;
/// AXRole 缓冲长度(与 `macos_sck.m` 结构体一致)。
const ROLE_CAP: usize = 64;

/// 显示器几何(CG/AX 全局点坐标 ↔ 帧物理像素的换算基准)。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CropmarkSnapDisplay {
    logical_x: i32,
    logical_y: i32,
    logical_w: u32,
    logical_h: u32,
    /// 主屏高度(点):CG 全局点坐标 y 向下与 AppKit 底原点的翻转基准。
    primary_h: i32,
}

/// 指针处的一个在屏窗口(CG 全局点坐标)。
#[repr(C)]
#[derive(Clone, Copy)]
struct CropmarkSnapWindow {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    pid: i32,
    title: [c_char; TITLE_CAP],
}

/// AX 链中的一个元素(CG 全局点坐标,自最深到浅层)。
#[repr(C)]
#[derive(Clone, Copy)]
struct CropmarkSnapAxItem {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    role: [c_char; ROLE_CAP],
    label: [c_char; TITLE_CAP],
}

extern "C" {
    /// 屏幕录制权限预检:窗口级 CGWindowList 是否可用。
    fn cropmark_snap_window_level_available() -> i32;
    /// 辅助功能授权预检(不弹窗)。
    fn cropmark_snap_ax_trusted() -> i32;
    /// 未授权时每进程给出一次系统授权提示;返回当前信任状态。
    fn cropmark_snap_ax_prompt_if_needed() -> i32;
    /// 按逻辑原点匹配显示器;0 成功。
    fn cropmark_snap_display(logical_x: i32, logical_y: i32, out: *mut CropmarkSnapDisplay) -> i32;
    /// 指针处自顶向下的在屏窗口列表(CG 全局点);0 成功,count 可为 0。
    fn cropmark_snap_windows_at(
        x: f64,
        y: f64,
        out: *mut CropmarkSnapWindow,
        cap: i32,
        count: *mut i32,
    ) -> i32;
    /// 指定进程指针处自最深到窗口的 AX 链(CG 全局点);0 成功。
    fn cropmark_snap_ax_chain(
        pid: i32,
        x: f64,
        y: f64,
        out: *mut CropmarkSnapAxItem,
        cap: i32,
        count: *mut i32,
    ) -> i32;
}

/// 平台 provider 单例(无状态)。
pub fn provider() -> &'static dyn SnapProvider {
    &MacosSnapProvider
}

#[derive(Debug)]
struct MacosSnapProvider;

impl SnapProvider for MacosSnapProvider {
    fn capability(&self) -> SnapCapability {
        capability_for(
            unsafe { cropmark_snap_window_level_available() } != 0,
            unsafe { cropmark_snap_ax_trusted() } != 0,
        )
    }

    fn hit(&self, context: SnapContext, x: i32, y: i32) -> SnapStack {
        let Some(display) = display_geometry(context) else {
            return SnapStack::empty();
        };
        let (cg_x, cg_y) = cg_point(context, display, x, y);
        let windows = windows_at(cg_x, cg_y);
        let Some(top) = windows.first() else {
            // 桌面/本进程窗口之下没有可吸附目标:保持自由框选。
            return SnapStack::empty();
        };
        // 控件级需要辅助功能授权:首次实际命中给出一次系统授权提示,
        // 未授权时返回空控件链、只保留窗口级。
        let controls = if top.pid > 0 && unsafe { cropmark_snap_ax_prompt_if_needed() } != 0 {
            ax_chain(top.pid, cg_x, cg_y)
        } else {
            Vec::new()
        };
        assemble_stack(context, display, &controls, &windows)
    }
}

/// 能力判定(纯逻辑,便于单测):屏幕录制权限缺失时窗口级也不可用,走
/// `Unavailable` 与自由框选说明;辅助功能未授权时明确回退窗口级,不伪造
/// 控件级。
fn capability_for(window_level_available: bool, ax_trusted: bool) -> SnapCapability {
    if !window_level_available {
        return SnapCapability::Unavailable {
            reason_key: UNAVAILABLE_KEY,
        };
    }
    if ax_trusted {
        SnapCapability::Full
    } else {
        SnapCapability::WindowOnly {
            reason_key: CONTROL_FALLBACK_KEY,
        }
    }
}

/// 按 `SnapContext.logical_origin` 取当前显示器几何;显示器几何异常时按
/// 本次无命中处理(不做错误坐标换算)。
fn display_geometry(context: SnapContext) -> Option<CropmarkSnapDisplay> {
    let mut display = CropmarkSnapDisplay {
        logical_x: 0,
        logical_y: 0,
        logical_w: 0,
        logical_h: 0,
        primary_h: 0,
    };
    let status = unsafe {
        cropmark_snap_display(
            context.logical_origin.0,
            context.logical_origin.1,
            &mut display,
        )
    };
    if status != 0 || display.logical_w == 0 || display.logical_h == 0 || display.primary_h <= 0 {
        return None;
    }
    Some(display)
}

fn windows_at(cg_x: f64, cg_y: f64) -> Vec<CropmarkSnapWindow> {
    let mut raw = vec![empty_window(); MAX_WINDOW_HITS];
    let mut count = 0i32;
    let status = unsafe {
        cropmark_snap_windows_at(cg_x, cg_y, raw.as_mut_ptr(), raw.len() as i32, &mut count)
    };
    if status != 0 {
        return Vec::new();
    }
    raw.truncate(count.max(0) as usize);
    raw
}

fn ax_chain(pid: i32, cg_x: f64, cg_y: f64) -> Vec<CropmarkSnapAxItem> {
    let mut raw = vec![empty_ax_item(); MAX_CONTROL_HITS];
    let mut count = 0i32;
    let status = unsafe {
        cropmark_snap_ax_chain(
            pid,
            cg_x,
            cg_y,
            raw.as_mut_ptr(),
            raw.len() as i32,
            &mut count,
        )
    };
    if status != 0 {
        return Vec::new();
    }
    raw.truncate(count.max(0) as usize);
    raw
}

fn empty_window() -> CropmarkSnapWindow {
    CropmarkSnapWindow {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
        pid: 0,
        title: [0; TITLE_CAP],
    }
}

fn empty_ax_item() -> CropmarkSnapAxItem {
    CropmarkSnapAxItem {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
        role: [0; ROLE_CAP],
        label: [0; TITLE_CAP],
    }
}

/// CG 全局点坐标系矩形(y 向下,主屏左上为原点)。
#[derive(Debug, Clone, Copy, PartialEq)]
struct CgRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl CgRect {
    /// 有限且至少 1×1;异常 native 值不进入命中栈。
    fn usable(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 1.0
            && self.height >= 1.0
    }
}

/// 帧像素 → CG 全局点:显示器内按 scale 还原逻辑点,再由帧向下翻回 CG 的
/// y 向下(主屏左上原点)。
fn cg_point(context: SnapContext, display: CropmarkSnapDisplay, x: i32, y: i32) -> (f64, f64) {
    let scale = context.scale;
    let local_x = f64::from(x) / scale;
    let local_y = f64::from(y) / scale;
    let display_top = f64::from(display.logical_y) + f64::from(display.logical_h);
    let cg_x = f64::from(display.logical_x) + local_x;
    let cg_y = f64::from(display.primary_h) - display_top + local_y;
    (cg_x, cg_y)
}

/// CG 全局点矩形 → 屏幕物理像素(与 `SnapContext.origin` 同基:AppKit 底
/// 原点 × scale,帧内 y 向下)。
fn physical_rect(context: SnapContext, display: CropmarkSnapDisplay, rect: CgRect) -> SnapRect {
    let scale = context.scale;
    let cg_min_y =
        f64::from(display.primary_h) - f64::from(display.logical_y) - f64::from(display.logical_h);
    let x = (rect.x - f64::from(display.logical_x)) * scale;
    let y = (rect.y - cg_min_y) * scale;
    SnapRect::new(
        context.origin.0.saturating_add(x.round() as i32),
        context.origin.1.saturating_add(y.round() as i32),
        (rect.width * scale).round().max(0.0) as u32,
        (rect.height * scale).round().max(0.0) as u32,
    )
}

/// 组装命中栈(纯逻辑):控件自最深到浅层在前,顶层窗口及其下重叠窗口按
/// CGWindowList 前后序在后;AXWindow 及以上不重复入栈,零尺寸与连续同矩形
/// 的包裹层被过滤。
fn assemble_stack(
    context: SnapContext,
    display: CropmarkSnapDisplay,
    controls: &[CropmarkSnapAxItem],
    windows: &[CropmarkSnapWindow],
) -> SnapStack {
    let mut hits = Vec::new();
    let mut previous: Option<SnapRect> = None;
    for item in controls {
        if role_text(&item.role) == "AXWindow" {
            break;
        }
        let rect = CgRect {
            x: item.x,
            y: item.y,
            width: item.width,
            height: item.height,
        };
        if !rect.usable() {
            continue;
        }
        let frame = physical_rect(context, display, rect);
        if frame.width == 0 || frame.height == 0 || previous == Some(frame) {
            continue;
        }
        previous = Some(frame);
        hits.push(SnapHit {
            kind: SnapKind::Control,
            rect: frame,
            label: text(&item.label),
        });
    }
    for window in windows {
        let rect = CgRect {
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        };
        if !rect.usable() {
            continue;
        }
        let frame = physical_rect(context, display, rect);
        if frame.width == 0 || frame.height == 0 {
            continue;
        }
        hits.push(SnapHit {
            kind: SnapKind::Window,
            rect: frame,
            label: text(&window.title),
        });
    }
    SnapStack::new(hits)
}

/// NUL 结尾的 C 字符串缓冲 → Option<String>(空串视为缺失)。不要求缓冲
/// 一定以 NUL 结尾,避免异常 native 写入越界。
fn text(buffer: &[c_char]) -> Option<String> {
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    if end == 0 {
        return None;
    }
    let bytes: Vec<u8> = buffer[..end].iter().map(|&byte| byte as u8).collect();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn role_text(buffer: &[c_char]) -> String {
    text(buffer).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::snap::SnapStep;

    fn primary_display() -> CropmarkSnapDisplay {
        CropmarkSnapDisplay {
            logical_x: 0,
            logical_y: 0,
            logical_w: 1920,
            logical_h: 1080,
            primary_h: 1080,
        }
    }

    /// 主屏右侧的副屏(AppKit 全局原点在 x 方向右移,y 仍与主屏底对齐)。
    fn right_display() -> CropmarkSnapDisplay {
        CropmarkSnapDisplay {
            logical_x: 1920,
            logical_y: 0,
            logical_w: 1280,
            logical_h: 720,
            primary_h: 1080,
        }
    }

    /// 主屏上方的副屏(AppKit y 为正,CG y 为负)。
    fn above_display() -> CropmarkSnapDisplay {
        CropmarkSnapDisplay {
            logical_x: 0,
            logical_y: 1080,
            logical_w: 1920,
            logical_h: 1080,
            primary_h: 1080,
        }
    }

    fn c_string(buffer: &mut [c_char], text: &str) {
        for (slot, byte) in buffer.iter_mut().zip(text.as_bytes()) {
            *slot = *byte as c_char;
        }
    }

    fn window(x: f64, y: f64, width: f64, height: f64, title: &str) -> CropmarkSnapWindow {
        let mut title_buffer = [0; TITLE_CAP];
        c_string(&mut title_buffer, title);
        CropmarkSnapWindow {
            x,
            y,
            width,
            height,
            pid: 42,
            title: title_buffer,
        }
    }

    fn ax_item(
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        role: &str,
        label: &str,
    ) -> CropmarkSnapAxItem {
        let mut role_buffer = [0; ROLE_CAP];
        let mut label_buffer = [0; TITLE_CAP];
        c_string(&mut role_buffer, role);
        c_string(&mut label_buffer, label);
        CropmarkSnapAxItem {
            x,
            y,
            width,
            height,
            role: role_buffer,
            label: label_buffer,
        }
    }

    #[test]
    fn capability_maps_trust_and_window_level_to_full_window_only_or_unavailable() {
        assert_eq!(capability_for(true, true), SnapCapability::Full);

        // 未授权但窗口级可用:仅窗口级回退并给出本地化说明词条。
        let window_only = capability_for(true, false);
        assert!(window_only.window_level());
        assert!(!window_only.control_level());
        assert_eq!(window_only.reason_key(), Some(CONTROL_FALLBACK_KEY));

        // 屏幕录制权限缺失:窗口级也不可用,沿用自由框选说明。
        for (window_level, trusted) in [(false, true), (false, false)] {
            let unavailable = capability_for(window_level, trusted);
            assert!(!unavailable.window_level());
            assert_eq!(unavailable.reason_key(), Some(UNAVAILABLE_KEY));
        }
    }

    #[test]
    fn cg_point_flips_frame_y_into_cg_global_points() {
        // 主屏:帧左上角就是 CG 原点;帧 (400,200) 在 2x 下是 CG (200,100)。
        let context = SnapContext::new((0, 0), (0, 0), 2.0);
        assert_eq!(cg_point(context, primary_display(), 0, 0), (0.0, 0.0));
        assert_eq!(
            cg_point(context, primary_display(), 400, 200),
            (200.0, 100.0)
        );

        // 右侧副屏:帧 (10,20) 仍是该屏左上角 + (10,20) 的 CG 点。
        let context = SnapContext::new((1920, 0), (1920, 0), 1.0);
        assert_eq!(cg_point(context, right_display(), 10, 20), (1930.0, 380.0));

        // 上方副屏:CG y 为负,帧顶 = primary_h - (logical_y + logical_h)。
        let context = SnapContext::new((0, 1080), (0, 1080), 1.0);
        assert_eq!(cg_point(context, above_display(), 0, 0), (0.0, -1080.0));
        assert_eq!(cg_point(context, above_display(), 12, 34), (12.0, -1046.0));
    }

    #[test]
    fn physical_rect_rebases_cg_rects_onto_frame_origin_and_clamps() {
        // 主屏 2x:CG (100,50,800×600) → 帧物理 (200,100,1600×1200)。
        let context = SnapContext::new((0, 0), (0, 0), 2.0);
        assert_eq!(
            physical_rect(
                context,
                primary_display(),
                CgRect {
                    x: 100.0,
                    y: 50.0,
                    width: 800.0,
                    height: 600.0,
                },
            ),
            SnapRect::new(200, 100, 1600, 1200)
        );

        // 上方副屏:返回矩形与 SnapContext.origin 同基,clamp 后回到帧内。
        let context = SnapContext::new((0, 1080), (0, 1080), 1.0);
        let screen = physical_rect(
            context,
            above_display(),
            CgRect {
                x: 100.0,
                y: -1080.0,
                width: 800.0,
                height: 600.0,
            },
        );
        assert_eq!(screen, SnapRect::new(100, 1080, 800, 600));
        let clamped = SnapStack::new(vec![SnapHit {
            kind: SnapKind::Window,
            rect: screen,
            label: Some("Notes".into()),
        }])
        .clamped_to_frame(context.origin, 1920, 1080);
        assert_eq!(clamped.hits()[0].rect, SnapRect::new(100, 0, 800, 600));
    }

    #[test]
    fn hit_stack_orders_deepest_control_first_and_windows_front_to_back() {
        let context = SnapContext::new((0, 0), (0, 0), 2.0);
        let controls = vec![
            ax_item(100.0, 80.0, 30.0, 24.0, "AXButton", "OK"),
            // 与最深控件同矩形的包裹层去重,滚轮不停在无意义的重复层级。
            ax_item(100.0, 80.0, 30.0, 24.0, "AXGroup", ""),
            ax_item(50.0, 40.0, 400.0, 240.0, "AXGroup", ""),
            // AXWindow 及以上由窗口级条目承担,不再重复入栈。
            ax_item(50.0, 40.0, 400.0, 240.0, "AXWindow", "Notes"),
            ax_item(0.0, 0.0, 800.0, 600.0, "AXGroup", "ignored"),
        ];
        let windows = vec![
            window(50.0, 40.0, 400.0, 240.0, "Notes"),
            window(0.0, 0.0, 1920.0, 1080.0, "Wallpaper"),
        ];
        let stack = assemble_stack(context, primary_display(), &controls, &windows);
        assert_eq!(stack.len(), 4);
        assert_eq!(stack.hits()[0].kind, SnapKind::Control);
        assert_eq!(stack.hits()[0].label.as_deref(), Some("OK"));
        assert_eq!(stack.hits()[0].rect, SnapRect::new(200, 160, 60, 48));
        assert_eq!(stack.hits()[1].kind, SnapKind::Control);
        assert_eq!(stack.hits()[1].label, None);
        assert_eq!(stack.hits()[1].rect, SnapRect::new(100, 80, 800, 480));
        assert_eq!(stack.hits()[2].kind, SnapKind::Window);
        assert_eq!(stack.hits()[2].label.as_deref(), Some("Notes"));
        assert_eq!(stack.hits()[2].rect, SnapRect::new(100, 80, 800, 480));
        assert_eq!(stack.hits()[3].kind, SnapKind::Window);
        assert_eq!(stack.hits()[3].rect, SnapRect::new(0, 0, 3840, 2160));
        // 默认高亮最深控件;滚轮 Parent 逐级进入外层控件与窗口。
        assert_eq!(stack.default_index(), Some(0));
        assert_eq!(stack.step(0, SnapStep::Parent), 1);
    }

    #[test]
    fn hit_stack_drops_invalid_rects_and_keeps_window_only_fallback() {
        let context = SnapContext::new((0, 0), (0, 0), 1.0);
        let controls = vec![
            ax_item(10.0, 10.0, 0.0, 20.0, "", ""),
            ax_item(f64::NAN, 10.0, 20.0, 20.0, "AXImage", ""),
            ax_item(10.0, 10.0, 20.0, 20.0, "", "value"),
        ];
        let windows = vec![
            window(0.0, 0.0, 0.0, 100.0, "degenerate"),
            window(0.0, 0.0, 200.0, 100.0, "Notes"),
        ];
        let stack = assemble_stack(context, primary_display(), &controls, &windows);
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.hits()[0].kind, SnapKind::Control);
        assert_eq!(stack.hits()[0].label.as_deref(), Some("value"));
        assert_eq!(stack.hits()[0].rect, SnapRect::new(10, 10, 20, 20));
        assert_eq!(stack.hits()[1].kind, SnapKind::Window);
        assert_eq!(stack.hits()[1].rect, SnapRect::new(0, 0, 200, 100));
        // 未授权/链为空时只剩窗口级条目,窗口级高亮仍可用。
        let window_only = assemble_stack(context, primary_display(), &[], &windows);
        assert_eq!(window_only.len(), 1);
        assert_eq!(window_only.hits()[0].kind, SnapKind::Window);
    }

    #[test]
    fn text_parses_nul_terminated_utf8_and_truncated_buffers() {
        let mut buffer = [0; 8];
        c_string(&mut buffer, "按");
        assert_eq!(text(&buffer).as_deref(), Some("按"));
        assert_eq!(text(&[0; 4]), None);
        // 无 NUL 的截断缓冲按已写内容解析,不越界。
        let full = [1; 4];
        assert_eq!(
            text(&full).as_deref(),
            Some("\u{1}\u{1}\u{1}\u{1}"),
            "full buffer without terminator"
        );
        assert_eq!(role_text(&[0; 4]), "");
    }
}
