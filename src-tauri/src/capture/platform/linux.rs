use std::fs;
use std::path::PathBuf;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as RandrExt;
use x11rb::protocol::xproto::{self, ConnectionExt as XprotoExt, ImageFormat};

use crate::capture::buffer::{
    accept_buffer, crop_desktop_to_monitor, decode_png, Frame, RawBuffer,
};
use crate::capture::error::{classify_platform_failure, CaptureError, PlatformFailure};
use crate::capture::geometry::{
    composite_premul_cursor, cursor_anchor_inside, monitor_at_physical, parse_xfixes_cursor_image,
    CursorBlit, MonitorGeom,
};
use crate::capture::windows_list::{selectable_windows, ListedWindow};
use crate::i18n;

use super::{linux_capture_backend, CursorMode, CursorOutcome, LinuxCaptureBackend};

/// 门户不可用时的统一提示(R13):说明原因并给出替代路径。
const PORTAL_UNAVAILABLE_KEY: &str = "error.linux.portal_missing";
/// 门户/Wayland 无法枚举窗口:点名替代截取方式,不打开空白预览。
const PORTAL_WINDOW_LIST_KEY: &str = "error.linux.portal_window_list";
/// 门户/Wayland 无法截取窗口:同样给出替代截取方式。
const PORTAL_WINDOW_CAPTURE_KEY: &str = "error.linux.portal_window_capture";

pub fn pointer_monitor() -> Result<MonitorGeom, CaptureError> {
    if uses_portal() {
        return Err(wayland_pointer_unavailable());
    }
    if let Ok(monitor) = x11_pointer_monitor() {
        return Ok(monitor);
    }
    Err(CaptureError::unavailable("error.linux.no_monitor"))
}

/// R13:Wayland 指针屏检测失败不再只报"无法从 X11 读取",而是说明失败
/// 原因并给出替代路径,避免用户在区域截取入口看到无响应式报错。
fn wayland_pointer_unavailable() -> CaptureError {
    CaptureError::unavailable("error.linux.wayland_pointer")
}

pub fn capture_monitor(monitor: &MonitorGeom) -> Result<Frame, CaptureError> {
    capture_monitor_with_cursor(monitor, CursorMode::Off).map(|(frame, _)| frame)
}

pub fn capture_monitor_with_cursor(
    monitor: &MonitorGeom,
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    match linux_capture_backend(wayland_display().as_deref()) {
        LinuxCaptureBackend::Portal => {
            let frame = portal_fullscreen()?;
            let cropped = crop_desktop_to_monitor(
                frame,
                monitor,
                monitor.physical_x.min(0),
                monitor.physical_y.min(0),
            )?;
            Ok(portal_cursor(cropped, mode))
        }
        LinuxCaptureBackend::X11 => x11_capture_rect_cursor(
            monitor.physical_x,
            monitor.physical_y,
            monitor.physical_width,
            monitor.physical_height,
            monitor.scale,
            mode,
        ),
    }
}

pub fn capture_display(
    monitor: &MonitorGeom,
    _siblings: &[MonitorGeom],
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    capture_monitor_with_cursor(monitor, mode)
}

pub fn capture_portal_desktop() -> Result<Frame, CaptureError> {
    if !uses_portal() {
        return Err(CaptureError::unavailable(
            "error.capture.stitch_unsupported",
        ));
    }
    portal_fullscreen()
}

pub fn list_windows(self_pid: u32) -> Result<Vec<ListedWindow>, CaptureError> {
    if uses_portal() {
        return Err(CaptureError::unavailable(PORTAL_WINDOW_LIST_KEY));
    }
    x11_list_windows(self_pid)
}

pub fn capture_window(id: &str) -> Result<Frame, CaptureError> {
    capture_window_with_cursor(id, CursorMode::Off).map(|(frame, _)| frame)
}

pub fn capture_window_with_cursor(
    id: &str,
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    if uses_portal() {
        return Err(CaptureError::unavailable(PORTAL_WINDOW_CAPTURE_KEY));
    }
    let (frame, origin_x, origin_y) = x11_capture_window_at(id)?;
    Ok(apply_x11_cursor(frame, origin_x, origin_y, mode))
}

pub fn dismiss_tray_popup() {}

pub fn tray_popup_visible() -> bool {
    false
}

/// R1:仅 X11 会话支持连续抓取;Wayland/portal 只有一次性截图能力。
pub fn scroll_capture_supported() -> bool {
    crate::capture::platform::scroll_capture_supported_for(linux_capture_backend(
        wayland_display().as_deref(),
    ))
}

fn wayland_display() -> Option<String> {
    std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|value| !value.is_empty())
}

fn uses_portal() -> bool {
    linux_capture_backend(wayland_display().as_deref()) == LinuxCaptureBackend::Portal
}

fn portal_fullscreen() -> Result<Frame, CaptureError> {
    pollster::block_on(portal_fullscreen_async())
}

fn portal_cursor(frame: Frame, mode: CursorMode) -> (Frame, CursorOutcome) {
    let outcome = match mode {
        CursorMode::Off => CursorOutcome::NotRequested,
        // 门户帧没有可合成的指针位图;开启开关时降级并保留画面。
        CursorMode::WhenInside => CursorOutcome::Unavailable,
    };
    (frame, outcome)
}

async fn portal_fullscreen_async() -> Result<Frame, CaptureError> {
    let request = ashpd::desktop::screenshot::Screenshot::request()
        .interactive(false)
        .modal(false);
    let response = request
        .send()
        .await
        .map_err(portal_error)?
        .response()
        .map_err(portal_error)?;
    let uri = response.uri().to_string();
    let path = file_uri_to_path(&uri)?;
    let bytes =
        fs::read(&path).map_err(|_| CaptureError::invalid_buffer("error.capture.buffer_empty"))?;
    let _ = fs::remove_file(&path);
    decode_png(&bytes)
}

fn portal_error(error: ashpd::Error) -> CaptureError {
    let text = error.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("denied") || lower.contains("permission") || lower.contains("not allowed") {
        classify_platform_failure(PlatformFailure::PermissionDenied)
    } else if lower.contains("unknown") || lower.contains("not found") || lower.contains("no such")
    {
        CaptureError::unavailable(PORTAL_UNAVAILABLE_KEY)
    } else {
        classify_platform_failure(PlatformFailure::Api(text))
    }
}

fn file_uri_to_path(uri: &str) -> Result<PathBuf, CaptureError> {
    let parsed =
        url::Url::parse(uri).map_err(|_| CaptureError::api("error.capture.portal_path_invalid"))?;
    parsed
        .to_file_path()
        .map_err(|_| CaptureError::api("error.capture.portal_path_invalid"))
}

fn x11_pointer_monitor() -> Result<MonitorGeom, CaptureError> {
    let (conn, screen_num) = x11rb::connect(None).map_err(|_| x11_unavailable())?;
    let screen = &conn.setup().roots[screen_num];
    let pointer = conn
        .query_pointer(screen.root)
        .map_err(|_| x11_unavailable())?
        .reply()
        .map_err(|_| x11_unavailable())?;
    let monitors = x11_monitors(&conn, screen.root)?;
    monitor_at_physical(&monitors, pointer.root_x as i32, pointer.root_y as i32)
        .cloned()
        .or_else(|| monitors.into_iter().next())
        .ok_or_else(x11_unavailable)
}

fn x11_monitors(
    conn: &impl Connection,
    root: xproto::Window,
) -> Result<Vec<MonitorGeom>, CaptureError> {
    let reply = conn
        .randr_get_monitors(root, true)
        .map_err(|_| x11_unavailable())?
        .reply()
        .map_err(|_| x11_unavailable())?;
    let mut monitors = Vec::new();
    for (index, monitor) in reply.monitors.iter().enumerate() {
        let width = monitor.width as u32;
        let height = monitor.height as u32;
        if width == 0 || height == 0 {
            continue;
        }
        monitors.push(MonitorGeom::from_physical(
            format!("x11-{index}"),
            monitor.x as i32,
            monitor.y as i32,
            width,
            height,
            1.0,
        ));
    }
    if monitors.is_empty() {
        Err(x11_unavailable())
    } else {
        Ok(monitors)
    }
}

fn x11_capture_rect(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f64,
) -> Result<Frame, CaptureError> {
    if width == 0 || height == 0 {
        return Err(classify_platform_failure(PlatformFailure::BufferZeroSize));
    }
    let (conn, screen_num) = x11rb::connect(None).map_err(|_| x11_unavailable())?;
    let screen = &conn.setup().roots[screen_num];
    let image = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            screen.root,
            x as i16,
            y as i16,
            width as u16,
            height as u16,
            !0,
        )
        .map_err(|_| classify_platform_failure(PlatformFailure::Api("XGetImage".into())))?
        .reply()
        .map_err(|_| classify_platform_failure(PlatformFailure::Api("XGetImage".into())))?;
    let rgba = zpixmap_to_rgba(&image.data, image.depth, width, height)?;
    let mut frame = accept_buffer(RawBuffer::ready(width, height, rgba))?;
    frame.scale = scale;
    Ok(frame)
}

fn x11_capture_rect_cursor(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f64,
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    let frame = x11_capture_rect(x, y, width, height, scale)?;
    Ok(apply_x11_cursor(frame, x, y, mode))
}

fn apply_x11_cursor(
    mut frame: Frame,
    origin_x: i32,
    origin_y: i32,
    mode: CursorMode,
) -> (Frame, CursorOutcome) {
    if mode == CursorMode::Off {
        return (frame, CursorOutcome::NotRequested);
    }
    let image = match read_xfixes_cursor() {
        Ok(Some(image)) => image,
        Ok(None) => return (frame, CursorOutcome::Outside),
        Err(()) => return (frame, CursorOutcome::Unavailable),
    };
    let hot_x = i32::from(image.x);
    let hot_y = i32::from(image.y);
    if !cursor_anchor_inside(origin_x, origin_y, frame.width, frame.height, hot_x, hot_y) {
        return (frame, CursorOutcome::Outside);
    }
    let drew = composite_premul_cursor(
        frame.width,
        frame.height,
        &mut frame.rgba,
        origin_x,
        origin_y,
        CursorBlit {
            hot_x,
            hot_y,
            sprite_x: hot_x - i32::from(image.x_hot),
            sprite_y: hot_y - i32::from(image.y_hot),
            width: u32::from(image.width),
            height: u32::from(image.height),
            argb: &image.argb,
        },
    );
    if drew {
        (frame, CursorOutcome::Included)
    } else {
        (frame, CursorOutcome::Unavailable)
    }
}

struct XFixesCursorReply(crate::capture::geometry::XFixesCursorImage);

impl x11rb::x11_utils::TryParse for XFixesCursorReply {
    fn try_parse(value: &[u8]) -> Result<(Self, &[u8]), x11rb::errors::ParseError> {
        match parse_xfixes_cursor_image(value) {
            Some((image, rest)) => Ok((Self(image), rest)),
            None => Err(x11rb::errors::ParseError::InsufficientData),
        }
    }
}

/// XFixes QueryVersion 回复(连接字节序即主机序)。
struct XFixesVersionReply {
    major: u32,
}

impl XFixesVersionReply {
    /// GetCursorImage 从主版本 1 起才在服务端放行。
    const fn allows_get_cursor_image(&self) -> bool {
        self.major >= 1
    }
}

impl x11rb::x11_utils::TryParse for XFixesVersionReply {
    fn try_parse(value: &[u8]) -> Result<(Self, &[u8]), x11rb::errors::ParseError> {
        if value.len() < 32 || value[0] != 1 {
            return Err(x11rb::errors::ParseError::InsufficientData);
        }
        let length = u32::from_ne_bytes([value[4], value[5], value[6], value[7]]);
        let major = u32::from_ne_bytes([value[8], value[9], value[10], value[11]]);
        let extra = usize::try_from(length)
            .ok()
            .and_then(|words| words.checked_mul(4))
            .ok_or(x11rb::errors::ParseError::InsufficientData)?;
        let total = 32usize
            .checked_add(extra)
            .ok_or(x11rb::errors::ParseError::InsufficientData)?;
        let rest = value
            .get(total..)
            .ok_or(x11rb::errors::ParseError::InsufficientData)?;
        Ok((Self { major }, rest))
    }
}

const XFIXES_QUERY_VERSION: u8 = 0;
const XFIXES_GET_CURSOR_IMAGE: u8 = 4;

const fn xfixes_query_version_request(major_opcode: u8) -> [u8; 12] {
    let length = 3u16.to_ne_bytes();
    let client_major = 1u32.to_ne_bytes();
    let client_minor = 0u32.to_ne_bytes();
    [
        major_opcode,
        XFIXES_QUERY_VERSION,
        length[0],
        length[1],
        client_major[0],
        client_major[1],
        client_major[2],
        client_major[3],
        client_minor[0],
        client_minor[1],
        client_minor[2],
        client_minor[3],
    ]
}

const fn xfixes_get_cursor_image_request(major_opcode: u8) -> [u8; 4] {
    let length = 1u16.to_ne_bytes();
    [major_opcode, XFIXES_GET_CURSOR_IMAGE, length[0], length[1]]
}

struct XFixesCursorRequests {
    query_version: [u8; 12],
    get_cursor_image: [u8; 4],
}

const fn xfixes_cursor_requests(major_opcode: u8) -> XFixesCursorRequests {
    XFixesCursorRequests {
        query_version: xfixes_query_version_request(major_opcode),
        get_cursor_image: xfixes_get_cursor_image_request(major_opcode),
    }
}

fn xfixes_roundtrip<R>(conn: &impl RequestConnection, request: &[u8]) -> Result<R, ()>
where
    R: x11rb::x11_utils::TryParse,
{
    use std::io::IoSlice;
    conn.send_request_with_reply(
        &[IoSlice::new(request)],
        Vec::<x11rb::utils::RawFdContainer>::new(),
    )
    .map_err(|_| ())?
    .reply()
    .map_err(|_| ())
}

fn read_xfixes_cursor() -> Result<Option<crate::capture::geometry::XFixesCursorImage>, ()> {
    let (conn, _screen) = x11rb::connect(None).map_err(|_| ())?;
    let ext = conn
        .query_extension(b"XFIXES")
        .map_err(|_| ())?
        .reply()
        .map_err(|_| ())?;
    if !ext.present {
        return Err(());
    }
    // 新连接的 major_version 为 0,只接受 QueryVersion。必须在同一条连接上
    // 先协商至少 1.0 并读完回复,服务端才放行 GetCursorImage。
    let requests = xfixes_cursor_requests(ext.major_opcode);
    let version = xfixes_roundtrip::<XFixesVersionReply>(&conn, &requests.query_version)?;
    if !version.allows_get_cursor_image() {
        return Err(());
    }
    let reply = xfixes_roundtrip::<XFixesCursorReply>(&conn, &requests.get_cursor_image)?;
    if reply.0.width == 0 || reply.0.height == 0 {
        Ok(None)
    } else {
        Ok(Some(reply.0))
    }
}

fn x11_list_windows(self_pid: u32) -> Result<Vec<ListedWindow>, CaptureError> {
    let (conn, screen_num) =
        x11rb::connect(None).map_err(|_| CaptureError::unavailable(PORTAL_WINDOW_LIST_KEY))?;
    let screen = &conn.setup().roots[screen_num];
    let atom = intern(&conn, b"_NET_CLIENT_LIST")?;
    let reply = conn
        .get_property(false, screen.root, atom, xproto::AtomEnum::WINDOW, 0, 4096)
        .map_err(|_| CaptureError::api("error.capture.window_list"))?
        .reply()
        .map_err(|_| CaptureError::api("error.capture.window_list"))?;
    let ids: Vec<u32> = reply.value32().into_iter().flatten().collect();
    let mut listed = Vec::new();
    for id in ids {
        if let Some(window) = x11_window_info(&conn, screen.root, id, self_pid) {
            listed.push(window);
        }
    }
    // _NET_CLIENT_LIST 按 stacking 自底向上;反转为契约要求的自顶向下。
    listed.reverse();
    Ok(selectable_windows(&listed, self_pid))
}

fn x11_window_info(
    conn: &impl Connection,
    root: xproto::Window,
    id: u32,
    self_pid: u32,
) -> Option<ListedWindow> {
    let geom = conn.get_geometry(id).ok()?.reply().ok()?;
    let attrs = conn.get_window_attributes(id).ok()?.reply().ok()?;
    if attrs.map_state != xproto::MapState::VIEWABLE {
        return None;
    }
    let translated = conn
        .translate_coordinates(id, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    let (left, right, top, bottom) = frame_extents(conn, id);
    let title = window_title(conn, id).unwrap_or_default();
    let pid = window_pid(conn, id).unwrap_or(0);
    Some(ListedWindow {
        id: id.to_string(),
        title,
        pid,
        x: translated.dst_x as i32 - left,
        y: translated.dst_y as i32 - top,
        width: geom.width as u32 + (left + right) as u32,
        height: geom.height as u32 + (top + bottom) as u32,
        visible: true,
        owner_is_self: pid == self_pid,
    })
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

fn x11_capture_window_at(id: &str) -> Result<(Frame, i32, i32), CaptureError> {
    let window = id
        .parse::<u32>()
        .map_err(|_| CaptureError::api("error.capture.window_unknown"))?;
    let (conn, screen_num) =
        x11rb::connect(None).map_err(|_| CaptureError::unavailable(PORTAL_WINDOW_CAPTURE_KEY))?;
    let geom = conn
        .get_geometry(window)
        .map_err(|_| CaptureError::api("error.capture.window_read"))?
        .reply()
        .map_err(|_| CaptureError::api("error.capture.window_read"))?;
    let screen = &conn.setup().roots[screen_num];
    let translated = conn
        .translate_coordinates(window, screen.root, 0, 0)
        .map_err(|_| CaptureError::api("error.capture.window_rect"))?
        .reply()
        .map_err(|_| CaptureError::api("error.capture.window_rect"))?;
    let origin_x = i32::from(translated.dst_x);
    let origin_y = i32::from(translated.dst_y);
    let image = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            window,
            0,
            0,
            geom.width,
            geom.height,
            !0,
        )
        .map_err(|_| classify_platform_failure(PlatformFailure::Api("XGetImage".into())))?
        .reply()
        .map_err(|_| classify_platform_failure(PlatformFailure::Api("XGetImage".into())))?;
    let rgba = zpixmap_to_rgba(
        &image.data,
        image.depth,
        geom.width as u32,
        geom.height as u32,
    )?;
    let frame = accept_buffer(RawBuffer::ready(
        geom.width as u32,
        geom.height as u32,
        rgba,
    ))?;
    Ok((frame, origin_x, origin_y))
}

fn intern(conn: &impl Connection, name: &[u8]) -> Result<xproto::Atom, CaptureError> {
    Ok(conn
        .intern_atom(false, name)
        .map_err(|_| CaptureError::api("error.capture.window_list"))?
        .reply()
        .map_err(|_| CaptureError::api("error.capture.window_list"))?
        .atom)
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

fn zpixmap_to_rgba(
    data: &[u8],
    depth: u8,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, CaptureError> {
    let pixels = width as usize * height as usize;
    if data.is_empty() {
        return Err(classify_platform_failure(PlatformFailure::BufferEmpty));
    }
    let mut rgba = vec![0u8; pixels * 4];
    match depth {
        24 | 32 if data.len() >= pixels * 4 => {
            for (src, dst) in data.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = 255;
            }
        }
        24 if data.len() >= pixels * 3 => {
            for (src, dst) in data.chunks_exact(3).zip(rgba.chunks_exact_mut(4)) {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = 255;
            }
        }
        _ => {
            return Err(classify_platform_failure(
                PlatformFailure::BufferUninitialized,
            ))
        }
    }
    Ok(rgba)
}

fn x11_unavailable() -> CaptureError {
    CaptureError::unavailable(PORTAL_UNAVAILABLE_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::error::CaptureErrorKind;

    #[test]
    fn wayland_pointer_failure_names_cause_and_alternative() {
        let error = wayland_pointer_unavailable();
        assert_eq!(error.kind, CaptureErrorKind::Unavailable);
        assert!(error.message.contains("Wayland"));
        assert!(error.message.contains("显示器"));
        // 必须给出替代路径,而不是只描述失败。
        assert!(error.message.contains("xdg-desktop-portal"));
        assert!(error.message.contains("X11"));
        assert!(error.user_message().contains("Wayland"));
    }

    #[test]
    fn portal_unavailable_messages_keep_alternatives() {
        assert!(i18n::t(PORTAL_UNAVAILABLE_KEY).contains("xdg-desktop-portal"));
        assert!(i18n::t(PORTAL_UNAVAILABLE_KEY).contains("X11"));
        // 门户不可用时不提供空白窗口列表,而是点名区域/全屏替代。
        assert!(i18n::t(PORTAL_WINDOW_LIST_KEY).contains("区域"));
        assert!(i18n::t(PORTAL_WINDOW_LIST_KEY).contains("全屏"));
        assert!(i18n::t(PORTAL_WINDOW_CAPTURE_KEY).contains("区域"));
        assert!(i18n::t(PORTAL_WINDOW_CAPTURE_KEY).contains("全屏"));
        let list = CaptureError::unavailable(PORTAL_WINDOW_LIST_KEY);
        assert_eq!(list.kind, CaptureErrorKind::Unavailable);
        assert_eq!(x11_unavailable().message, i18n::t(PORTAL_UNAVAILABLE_KEY));
    }

    #[test]
    fn xfixes_cursor_negotiates_version_before_get_cursor_image() {
        let opcode = 0x92;
        let requests = xfixes_cursor_requests(opcode);
        let query = requests.query_version;
        let image = requests.get_cursor_image;
        assert_eq!(query[0], opcode);
        assert_eq!(query[1], XFIXES_QUERY_VERSION);
        assert_eq!(u16::from_ne_bytes([query[2], query[3]]), 3);
        assert_eq!(query.len(), 12);
        let client_major = u32::from_ne_bytes(query[4..8].try_into().unwrap());
        let client_minor = u32::from_ne_bytes(query[8..12].try_into().unwrap());
        assert!(client_major >= 1);
        assert_eq!((client_major, client_minor), (1, 0));
        assert_eq!(image[0], opcode);
        assert_eq!(image[1], XFIXES_GET_CURSOR_IMAGE);
        assert_eq!(u16::from_ne_bytes([image[2], image[3]]), 1);
        assert_eq!(image.len(), 4);
        // 未协商时服务端最高 opcode 是 QueryVersion(0),GetCursorImage 必须排在其后。
        assert!(image[1] > query[1]);
    }

    #[test]
    fn xfixes_version_below_one_blocks_cursor_image() {
        use x11rb::x11_utils::TryParse;
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        bytes[8..12].copy_from_slice(&0u32.to_ne_bytes());
        let (reply, rest) = XFixesVersionReply::try_parse(&bytes).unwrap();
        assert!(rest.is_empty());
        assert!(!reply.allows_get_cursor_image());

        bytes[8..12].copy_from_slice(&1u32.to_ne_bytes());
        let (reply, _) = XFixesVersionReply::try_parse(&bytes).unwrap();
        assert!(reply.allows_get_cursor_image());
        bytes[8..12].copy_from_slice(&4u32.to_ne_bytes());
        let (reply, _) = XFixesVersionReply::try_parse(&bytes).unwrap();
        assert!(reply.allows_get_cursor_image());

        assert!(XFixesVersionReply::try_parse(&[1u8; 16]).is_err());
        bytes[0] = 0;
        assert!(XFixesVersionReply::try_parse(&bytes).is_err());
        let mut truncated = [0u8; 32];
        truncated[0] = 1;
        truncated[4..8].copy_from_slice(&1u32.to_ne_bytes());
        assert!(XFixesVersionReply::try_parse(&truncated).is_err());
    }
}
