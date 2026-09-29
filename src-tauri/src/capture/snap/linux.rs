//! Linux 元素级命中 provider:X11 窗口级命中,控件级明确回退。
//!
//! 窗口级命中栈与窗口枚举(`capture::platform::linux`)同源:优先
//! `_NET_CLIENT_LIST_STACKING`(EWMH 规定的窗口 z 序,自底向上),该属性缺失
//! 时回退 `_NET_CLIENT_LIST`(EWMH 只保证它是初始映射顺序,不保证 stacking),
//! 列数探测与命中栈共用同一来源;`get_geometry` +
//! `translate_coordinates` + `_NET_FRAME_EXTENTS` 叠加窗口装饰得到屏幕物理
//! 像素矩形;标题取 `_NET_WM_NAME`/`WM_NAME`,`_NET_WM_PID` 用于排除本进程
//! 窗口(贴图等;选区壳自身是 override-redirect,不进受管列表)。
//! 坐标契约:查询点经 `SnapContext::to_screen` 换算为屏幕物理像素,X11 全链
//! 物理像素(缩放恒 1.0),无需逻辑点换算。
//!
//! Linux 没有跨工具包可用的控件级检测(ADR-06),能力恒为 `WindowOnly`:
//! 命中栈只含窗口项,悬停高亮窗口边界,滚轮在重叠窗口间切换层级。Wayland
//! 会话或拿不到窗口列表(无 X 连接/无 EWMH WM/列表为空)时返回 `Unavailable`
//! 与空栈,不宣称窗口级可用——与 snap-core 的 Web 能力位语义一致:没有窗口
//! 可悬停/吸附时按自由框选回退。

use std::sync::OnceLock;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as XprotoExt};

use super::{SnapCapability, SnapContext, SnapHit, SnapKind, SnapProvider, SnapRect, SnapStack};

/// 控件级不可用(仅窗口级)说明词条。
const CONTROL_FALLBACK_KEY: &str = "error.capture.snap_control_unavailable";
/// 检测不可用(自由框选)说明词条。
const UNAVAILABLE_KEY: &str = "error.capture.snap_unavailable";
/// EWMH 窗口 z 序属性(自底向上);命中栈按它排序。
const STACKING_LIST_ATOM: &[u8] = b"_NET_CLIENT_LIST_STACKING";
/// 回退属性:WM 不维护 stacking 时使用,但 EWMH 只保证它是初始映射顺序。
const CLIENT_LIST_ATOM: &[u8] = b"_NET_CLIENT_LIST";

/// 平台 provider 单例。窗口级一旦探测可用就缓存(WM 能力在进程生命周期内
/// 稳定);负向结果(无 X/无窗口列表)不缓存,避免一次空桌面把降级状态粘死。
pub fn provider() -> &'static dyn SnapProvider {
    static PROVIDER: LinuxSnapProvider = LinuxSnapProvider {
        capability: OnceLock::new(),
    };
    &PROVIDER
}

#[derive(Debug)]
struct LinuxSnapProvider {
    capability: OnceLock<SnapCapability>,
}

impl SnapProvider for LinuxSnapProvider {
    fn capability(&self) -> SnapCapability {
        if let Some(capability) = self.capability.get() {
            return *capability;
        }
        let capability = probe_capability();
        if capability.window_level() {
            let _ = self.capability.set(capability);
        }
        capability
    }

    fn hit(&self, context: SnapContext, x: i32, y: i32) -> SnapStack {
        if !self.capability().window_level() {
            return SnapStack::empty();
        }
        let (screen_x, screen_y) = context.to_screen(x, y);
        // 连接/属性/几何任一读取失败都按"本次无命中"处理:壳保持自由框选,
        // 不 panic、不阻断交互。
        x11_hit_stack(screen_x, screen_y).unwrap_or_default()
    }
}

fn probe_capability() -> SnapCapability {
    capability_for(wayland_session(), listed_window_count())
}

/// 能力判定(纯逻辑,便于单测):Wayland 或窗口列表为空都返回 `Unavailable`,
/// 与 Web 覆盖层的能力位语义一致——没有窗口可悬停/吸附时不宣称窗口级可用;
/// 仅窗口级(控件级不可用)以 `WindowOnly` 显式回退。
fn capability_for(wayland: bool, window_count: usize) -> SnapCapability {
    if !wayland && window_count > 0 {
        SnapCapability::WindowOnly {
            reason_key: CONTROL_FALLBACK_KEY,
        }
    } else {
        SnapCapability::Unavailable {
            reason_key: UNAVAILABLE_KEY,
        }
    }
}

/// 与 `platform/linux.rs` 的 backend 判定同源:WAYLAND_DISPLAY 非空即
/// Wayland 会话,原生 X11 选区壳不启用,窗口级命中一律不可用。
fn wayland_session() -> bool {
    std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|value| !value.is_empty())
        .is_some()
}

/// 当前受管窗口列表长度;X11 连接或属性读取失败按 0(不可用)处理——
/// "列表不可读"与"列表为空"都不构成窗口级可用的依据。与命中栈共用
/// `client_list` 的来源选择,能力位与栈不会出现"来源分裂"。
fn listed_window_count() -> usize {
    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return 0;
    };
    let root = conn.setup().roots[screen_num].root;
    client_list(&conn, root).map_or(0, |ids| ids.len())
}

/// X11 窗口级命中栈:受管窗口按 z 序自底向上枚举(见 `client_list`),
/// 过滤未映射/零尺寸/本进程窗口,返回屏幕物理像素矩形(由 `FrameSnap`
/// 平移到帧坐标并裁进帧内)。
fn x11_hit_stack(screen_x: i32, screen_y: i32) -> Result<SnapStack, ()> {
    let (conn, screen_num) = x11rb::connect(None).map_err(|_| ())?;
    let root = conn.setup().roots[screen_num].root;
    let ids = client_list(&conn, root)?;
    let self_pid = std::process::id();
    let mut windows = Vec::new();
    for id in ids {
        let Some(rect) = window_rect(&conn, root, id) else {
            continue;
        };
        // 先做局部包含判定,避免为无关窗口读取标题/pid。
        if !rect_contains(rect, screen_x, screen_y) {
            continue;
        }
        // 本进程窗口(贴图等)不进命中栈:窗口级吸附只面向其它应用。
        if window_pid(&conn, id) == Some(self_pid) {
            continue;
        }
        windows.push((rect, window_title(&conn, id)));
    }
    Ok(stack_from_windows_bottom_up(windows))
}

/// 组装窗口级命中栈(纯逻辑):入参按 stacking z 序自底向上
/// (`_NET_CLIENT_LIST_STACKING`;回退 `_NET_CLIENT_LIST` 时该顺序只是近似),
/// 输出自上层到下层的命中——栈首是最上层(指针所在)窗口,默认高亮它,
/// 滚轮 Parent 逐步切到被它遮住的窗口。
fn stack_from_windows_bottom_up(windows: Vec<(SnapRect, Option<String>)>) -> SnapStack {
    SnapStack::new(
        windows
            .into_iter()
            .rev()
            .map(|(rect, label)| SnapHit {
                kind: SnapKind::Window,
                rect,
                label,
            })
            .collect(),
    )
}

/// 受管窗口 → 屏幕矩形:仅接受已映射(VIEWABLE)窗口;装饰边距叠加方式与
/// `windows_list` 的 X11 枚举一致(左上角减装饰,宽高加装饰)。
fn window_rect(conn: &impl Connection, root: xproto::Window, id: u32) -> Option<SnapRect> {
    let attrs = conn.get_window_attributes(id).ok()?.reply().ok()?;
    if attrs.map_state != xproto::MapState::VIEWABLE {
        return None;
    }
    let geom = conn.get_geometry(id).ok()?.reply().ok()?;
    let translated = conn
        .translate_coordinates(id, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    decorated_rect(
        translated.dst_x,
        translated.dst_y,
        geom.width,
        geom.height,
        frame_extents(conn, id),
    )
}

/// 客户端矩形 + `_NET_FRAME_EXTENTS`(左/右/上/下,物理像素)→ 屏幕矩形;
/// 零尺寸无效,负装饰按 0 处理,防止 u32 回绕。
fn decorated_rect(
    dst_x: i16,
    dst_y: i16,
    width: u16,
    height: u16,
    extents: (i32, i32, i32, i32),
) -> Option<SnapRect> {
    if width == 0 || height == 0 {
        return None;
    }
    let (left, right, top, bottom) = extents;
    let left = left.max(0);
    let right = right.max(0);
    let top = top.max(0);
    let bottom = bottom.max(0);
    let width = i64::from(width) + i64::from(left) + i64::from(right);
    let height = i64::from(height) + i64::from(top) + i64::from(bottom);
    Some(SnapRect::new(
        i32::from(dst_x) - left,
        i32::from(dst_y) - top,
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    ))
}

/// 屏幕矩形包含判定(左闭右开,与命中测试惯例一致);中间量用 i64,
/// 避免负原点或超大尺寸溢出。
fn rect_contains(rect: SnapRect, x: i32, y: i32) -> bool {
    let left = i64::from(rect.x);
    let top = i64::from(rect.y);
    let right = left + i64::from(rect.width);
    let bottom = top + i64::from(rect.height);
    let x = i64::from(x);
    let y = i64::from(y);
    x >= left && x < right && y >= top && y < bottom
}

/// 受管窗口列表,命中栈与能力探测(列数)共用,保证两者来源一致:
/// 优先 `_NET_CLIENT_LIST_STACKING`(EWMH 规定的 z 序,自底向上);
/// 该属性缺失(WM 不维护)时回退 `_NET_CLIENT_LIST`——EWMH 只保证后者是
/// 初始映射顺序,窗口重新置顶后不更新,回退顺序只是近似。两者都缺失时
/// 按不可读处理;连接/请求失败由 `Err` 上抛。
fn client_list(conn: &impl Connection, root: xproto::Window) -> Result<Vec<u32>, ()> {
    let stacking_atom = intern(conn, STACKING_LIST_ATOM)?;
    let stacking = window_list_property(conn, root, stacking_atom)?;
    let fallback = if stacking.is_none() {
        let client_atom = intern(conn, CLIENT_LIST_ATOM)?;
        window_list_property(conn, root, client_atom)?
    } else {
        None
    };
    resolve_client_list(stacking, fallback)
}

/// 来源选择(纯逻辑,便于单测):stacking 属性存在(即使为空列表)即采用,
/// 不混入另一来源;缺失才回退 client list;两者都缺失按不可读处理。
/// 属性存在但为空表示 WM 当前没有受管窗口,不能拿旧来源拼凑命中栈。
fn resolve_client_list(
    stacking: Option<Vec<u32>>,
    client_list: Option<Vec<u32>>,
) -> Result<Vec<u32>, ()> {
    stacking.or(client_list).ok_or(())
}

/// 读取 WINDOW 列表属性:属性缺失(type NONE)返回 `None`,存在时返回其值
/// (可能为空列表),请求失败返回 `Err`。
fn window_list_property(
    conn: &impl Connection,
    root: xproto::Window,
    atom: xproto::Atom,
) -> Result<Option<Vec<u32>>, ()> {
    let reply = conn
        .get_property(false, root, atom, xproto::AtomEnum::WINDOW, 0, 4096)
        .map_err(|_| ())?
        .reply()
        .map_err(|_| ())?;
    if reply.type_ == xproto::AtomEnum::NONE.into() {
        return Ok(None);
    }
    Ok(Some(reply.value32().into_iter().flatten().collect()))
}

fn frame_extents(conn: &impl Connection, id: u32) -> (i32, i32, i32, i32) {
    let Ok(atom) = intern(conn, b"_NET_FRAME_EXTENTS") else {
        return (0, 0, 0, 0);
    };
    let Ok(cookie) = conn.get_property(false, id, atom, xproto::AtomEnum::CARDINAL, 0, 4) else {
        return (0, 0, 0, 0);
    };
    let Ok(reply) = cookie.reply() else {
        return (0, 0, 0, 0);
    };
    let mut values = reply.value32().into_iter().flatten();
    (
        values.next().unwrap_or(0) as i32,
        values.next().unwrap_or(0) as i32,
        values.next().unwrap_or(0) as i32,
        values.next().unwrap_or(0) as i32,
    )
}

fn window_title(conn: &impl Connection, id: u32) -> Option<String> {
    let net_name = intern(conn, b"_NET_WM_NAME").ok()?;
    let utf8 = intern(conn, b"UTF8_STRING").ok()?;
    if let Ok(cookie) = conn.get_property(false, id, net_name, utf8, 0, 1024) {
        if let Ok(reply) = cookie.reply() {
            if !reply.value.is_empty() {
                return Some(String::from_utf8_lossy(&reply.value).into_owned());
            }
        }
    }
    let cookie = conn
        .get_property(
            false,
            id,
            xproto::AtomEnum::WM_NAME,
            xproto::AtomEnum::STRING,
            0,
            1024,
        )
        .ok()?;
    let reply = cookie.reply().ok()?;
    Some(String::from_utf8_lossy(&reply.value).into_owned())
}

fn window_pid(conn: &impl Connection, id: u32) -> Option<u32> {
    let atom = intern(conn, b"_NET_WM_PID").ok()?;
    let reply = conn
        .get_property(false, id, atom, xproto::AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let mut values = reply.value32()?;
    values.next()
}

fn intern(conn: &impl Connection, name: &[u8]) -> Result<xproto::Atom, ()> {
    Ok(conn
        .intern_atom(false, name)
        .map_err(|_| ())?
        .reply()
        .map_err(|_| ())?
        .atom)
}

#[cfg(test)]
mod tests {
    use crate::capture::snap::SnapStep;

    use super::*;

    #[test]
    fn capability_falls_back_to_window_level_or_unavailable_never_full() {
        // X11 有受管窗口 → 仅窗口级,控件级明确回退。
        let window_only = capability_for(false, 3);
        assert!(window_only.window_level());
        assert!(!window_only.control_level());
        assert_eq!(window_only.reason_key(), Some(CONTROL_FALLBACK_KEY));

        // Wayland 或窗口列表为空都不宣称窗口级可用。
        for (wayland, count) in [(true, 0), (true, 5), (false, 0)] {
            let unavailable = capability_for(wayland, count);
            assert!(
                !unavailable.window_level(),
                "wayland={wayland} count={count}"
            );
            assert!(!unavailable.control_level());
            assert_eq!(unavailable.reason_key(), Some(UNAVAILABLE_KEY));
        }
    }

    #[test]
    fn decorated_rect_adds_frame_extents_and_rejects_zero_size() {
        let rect = decorated_rect(120, 80, 800, 600, (2, 2, 24, 2)).expect("valid window");
        assert_eq!(rect, SnapRect::new(118, 56, 804, 626));
        assert_eq!(decorated_rect(0, 0, 0, 600, (0, 0, 0, 0)), None);
        assert_eq!(decorated_rect(0, 0, 800, 0, (0, 0, 0, 0)), None);
        // 异常负装饰不参与尺寸也不造成回绕。
        assert_eq!(
            decorated_rect(10, 20, 300, 200, (-5, -5, -3, -3)).expect("valid window"),
            SnapRect::new(10, 20, 300, 200)
        );
    }

    #[test]
    fn rect_contains_uses_half_open_bounds_and_handles_large_extents() {
        let rect = SnapRect::new(-100, -50, 900, 700);
        assert!(rect_contains(rect, -100, -50));
        assert!(rect_contains(rect, 799, 649));
        assert!(!rect_contains(rect, 800, 650));
        assert!(!rect_contains(rect, -101, 0));
        // 负原点 + 超大尺寸:i64 中间量不溢出。
        let huge = SnapRect::new(i32::MIN / 2, i32::MIN / 2, u32::MAX, u32::MAX);
        assert!(rect_contains(huge, 0, 0));
    }

    #[test]
    fn client_list_source_prefers_stacking_and_falls_back_only_when_missing() {
        // 来源属性名与 EWMH 一致:z 序在 stacking 属性,回退属性只保证初始映射顺序。
        assert_eq!(STACKING_LIST_ATOM, b"_NET_CLIENT_LIST_STACKING".as_slice());
        assert_eq!(CLIENT_LIST_ATOM, b"_NET_CLIENT_LIST".as_slice());

        // stacking 属性存在(即使与 client list 不同或为空)一律采用,不混用来源。
        assert_eq!(
            resolve_client_list(Some(vec![7, 3]), Some(vec![7, 9])),
            Ok(vec![7, 3])
        );
        assert_eq!(resolve_client_list(Some(vec![]), Some(vec![7])), Ok(vec![]));
        // stacking 缺失(WM 不维护)才回退 client list。
        assert_eq!(resolve_client_list(None, Some(vec![7, 9])), Ok(vec![7, 9]));
        // 两者都缺失:不可读,能力探测按不可用处理。
        assert_eq!(resolve_client_list(None, None), Err(()));
    }

    #[test]
    fn stack_orders_topmost_window_first_and_defaults_to_it() {
        let bottom = SnapRect::new(0, 0, 800, 600);
        let top = SnapRect::new(100, 100, 200, 150);
        let stack = stack_from_windows_bottom_up(vec![
            (bottom, Some("desktop".into())),
            (top, Some("dialog".into())),
        ]);
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.hits()[0].kind, SnapKind::Window);
        assert_eq!(stack.hits()[0].rect, top);
        assert_eq!(stack.hits()[0].label.as_deref(), Some("dialog"));
        assert_eq!(stack.hits()[1].rect, bottom);
        // 默认高亮栈首(指针所在的最上层窗口);滚轮 Parent 切到下层窗口。
        assert_eq!(stack.default_index(), Some(0));
        assert_eq!(stack.step(0, SnapStep::Parent), 1);
    }
}
