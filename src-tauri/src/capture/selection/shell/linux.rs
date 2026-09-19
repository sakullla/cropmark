//! Linux 区域选区壳(X11):override-redirect 全屏窗 + MIT-SHM/XPutImage 呈现。
//!
//! 交互语义全部由平台无关选区引擎(`capture::selection`)决定;本壳只把
//! 鼠标(左/右/移动)与键盘(方向键/Shift/Enter/Esc/C)事件以物理像素坐标
//! 喂给引擎,并把引擎的合成位图 present 到屏幕(ADR-001/006)。右键=菜单、
//! Esc=取消;Enter 确认;操作条/菜单动作经 `RegionOutcome` 交回会话层分发,
//! 与 `shell/windows.rs` 同构(ShellHooks 零 tauri 依赖模式)。
//!
//! 呈现:优先 MIT-SHM 共享内存段(探测扩展 + `ShmAttach` 校验,失败静默
//! 回退),否则按行分带的 XPutImage 经 socket 传输(每带 ≤1MiB,适配保守的
//! max-request-length)。像素打包按根视觉的 R/G/B 掩码与服务器字节序生成,
//! 常见 LSBFirst + {R<<16,G<<8,B} 布局等价于 Windows 壳的 BGRA 呈现缓冲。
//!
//! 光标提示(ADR-5):引擎 `cursor_for` 的提示经 "cursor" 字体字形
//! (`XCreateFontCursor` 语义:source=glyph、mask=glyph+1)映射为 fleur/resize
//! 箭头/默认指针/十字,输入事件后按提示变化切换(窗口属性等价 XDefineCursor;
//! 活动抓取另经 `ChangeActivePointerGrab` 立即换光标)。字体或单个字形不可用时
//! 该提示退回服务器默认指针,不阻断选择/确认/取消。
//!
//! 键盘:keycode→keysym 用核心协议 GetKeyboardMapping 的第 0 列(无修饰
//! 键位),方向键/Enter/Esc/C 与布局无关;Shift 状态取事件 state 的
//! KeyButMask::SHIFT 位,与 windows.rs 的 VK_SHIFT 跟踪同义。
//!
//! 接线说明:经 `capture/native_overlay.rs` 的 `#[path]` 分发挂入编译
//! (ADR-008);session.rs 的 Linux `capture_region` 按会话类型分派——
//! 非 Wayland 且 `$DISPLAY` 可用(含 XWayland)时调 `pick_region`,
//! Wayland 维持 Web 覆盖层路径(portal 抓屏不变)。
//!
//! Wayland 处置:纯 Rust 无工具链建原生窗成本高,按 ADR-008 的执行期
//! 修订定为文档化平台行为,Wayland 区域选择沿用 Web 覆盖层,原生窗列为
//! 后续独立工作。注意:本模块只能在 Linux 编译;Windows/macOS 主机上的
//! 离线核对以 windows.rs 逐块对照 + probe crate 交叉 cargo check 为准。

use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shm::{self, ConnectionExt as ShmExt};
use x11rb::protocol::xproto::{
    self, ChangeWindowAttributesAux, ConnectionExt as XprotoExt, CreateGCAux, CreateWindowAux,
    EventMask, GrabMode, GrabStatus, ImageFormat, ImageOrder, KeyButMask, Screen, Setup,
    Visualtype, WindowClass,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE, NO_SYMBOL};

use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;
use crate::capture::geometry::{MonitorGeom, PhysicalRect};
use crate::capture::selection::composer::{self, Composer};
use crate::capture::selection::{
    CursorHint, EngineOutcome, FeatureFlags, InputEvent, LogicalKey, SelectionAction,
    SelectionEngine,
};
use crate::capture::session::QuietAction;

// keysym 常量(X11/keysymdef.h;与键盘布局无关)。
const XK_RETURN: u32 = 0xff0d;
const XK_ESCAPE: u32 = 0xff1b;
const XK_LEFT: u32 = 0xff51;
const XK_UP: u32 = 0xff52;
const XK_RIGHT: u32 = 0xff53;
const XK_DOWN: u32 = 0xff54;
// 数字小键盘(Keypad)变体:主键盘区与 Keypad 的 Enter/方向键同一语义。
const XK_KP_ENTER: u32 = 0xff8b;
const XK_KP_LEFT: u32 = 0xff96;
const XK_KP_UP: u32 = 0xff97;
const XK_KP_RIGHT: u32 = 0xff98;
const XK_KP_DOWN: u32 = 0xff99;
const XK_C_LOWER: u32 = 0x63;
const XK_C_UPPER: u32 = 0x43;

// "cursor" 字体中的标准字形(X11/cursorfont.h 的 XC_* 常量)。每个光标占两个
// 字符码:source=glyph、mask=glyph+1(XCreateFontCursor 语义)。
const XC_CROSSHAIR: u16 = 34;
const XC_FLEUR: u16 = 52;
const XC_LEFT_PTR: u16 = 68;
const XC_SB_H_DOUBLE_ARROW: u16 = 108;
const XC_SB_V_DOUBLE_ARROW: u16 = 116;
const XC_TOP_LEFT_CORNER: u16 = 134;
const XC_TOP_RIGHT_CORNER: u16 = 136;

/// 光标槽位数:每个不同字形一槽;Pointer 与 Arrow 共用默认箭头槽。
const CURSOR_SLOT_COUNT: usize = 7;

/// 引擎光标提示 → "cursor" 字体字形。Pointer(可点击 chrome)与 Arrow
/// (放大镜面板)在 X11 下都使用默认箭头。
fn cursor_glyph(hint: CursorHint) -> u16 {
    match hint {
        CursorHint::Crosshair => XC_CROSSHAIR,
        CursorHint::Move => XC_FLEUR,
        CursorHint::ResizeNS => XC_SB_V_DOUBLE_ARROW,
        CursorHint::ResizeEW => XC_SB_H_DOUBLE_ARROW,
        CursorHint::ResizeNWSE => XC_TOP_LEFT_CORNER,
        CursorHint::ResizeNESW => XC_TOP_RIGHT_CORNER,
        CursorHint::Pointer | CursorHint::Arrow => XC_LEFT_PTR,
    }
}

fn cursor_slot(hint: CursorHint) -> usize {
    match hint {
        CursorHint::Crosshair => 0,
        CursorHint::Move => 1,
        CursorHint::ResizeNS => 2,
        CursorHint::ResizeEW => 3,
        CursorHint::ResizeNWSE => 4,
        CursorHint::ResizeNESW => 5,
        CursorHint::Pointer | CursorHint::Arrow => 6,
    }
}

/// XPutImage 回退路径的每请求上限:分带发送,避开保守的 max-request-length。
const PUT_IMAGE_BAND_LIMIT: usize = 1 << 20;

/// 壳的最终结果:会话层据此选择完成路径(与 Windows 壳同构)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionOutcome {
    /// Enter 确认:rect 走普通完成路径(按 finishAction 预览或静默)。
    Preview(PhysicalRect),
    /// 操作条/菜单的「标注」动作:rect 强制走预览编辑器,不受静默完成配置影响。
    Annotate(PhysicalRect),
    /// 操作条/菜单的 copy/save/pin/ocr 动作:rect 走 Quiet 完成路径并执行动作。
    Quiet(PhysicalRect, QuietAction),
    /// Esc 或菜单「取消」:整个会话取消。
    Cancelled,
}

/// 壳侧回调:色值复制等副作用由会话层注入,壳不直接触碰 tauri 运行时
/// (与 Windows 壳同因:避免把 dialog 模块链入测试二进制)。
#[derive(Debug, Clone, Copy)]
pub struct ShellHooks {
    /// 复制色值文本并给出反馈;参数为完整文本与 HEX 简写。
    pub copy_color: fn(text: &str, hex: &str),
}

/// 单个颜色通道的掩码打包参数(shift + 有效位数)。
#[derive(Debug, Clone, Copy)]
struct ChannelPack {
    shift: u32,
    bits: u32,
}

impl ChannelPack {
    fn new(mask: u32) -> Self {
        Self {
            shift: mask.trailing_zeros(),
            bits: mask.count_ones(),
        }
    }

    fn pack(&self, value: u8) -> u32 {
        if self.bits == 0 {
            // 掩码缺失的通道(如灰度视觉):恒为 0,同时避免移位溢出。
            return 0;
        }
        if self.bits >= 8 {
            // 8 位直通;>8 位的深色视觉取高位。
            ((value as u32) >> (self.bits - 8).min(24)) << self.shift
        } else {
            // <8 位按量程缩放,四舍五入。
            ((value as u32).wrapping_mul((1 << self.bits) - 1) + 127) / 255 << self.shift
        }
    }
}

/// 根视觉的像素打包布局(R/G/B 掩码 + 服务器字节序)。
#[derive(Debug, Clone, Copy)]
struct VisualLayout {
    red: ChannelPack,
    green: ChannelPack,
    blue: ChannelPack,
    msb_first: bool,
}

impl VisualLayout {
    /// 常见 TrueColor 布局(LSBFirst + R<<16/G<<8/B):打包结果等价于 BGRA。
    #[cfg(test)]
    fn typical() -> Self {
        Self {
            red: ChannelPack::new(0xff_0000),
            green: ChannelPack::new(0x00_ff00),
            blue: ChannelPack::new(0x0000_ff),
            msb_first: false,
        }
    }
}

/// 引擎 + 合成/呈现缓冲。独立成结构使呈现路径可脱离 AppHandle 测量。
struct Canvas {
    engine: SelectionEngine,
    composer: Composer,
    /// 引擎输出的 RGBA 合成帧(长度与冻结帧一致)。
    scratch: Vec<u8>,
    /// 呈现缓冲:X 像素格式(按根视觉掩码/字节序打包,4 字节/像素)。
    present: PresentBuffer,
    width: i32,
    height: i32,
    layout: VisualLayout,
}

/// MIT-SHM 共享内存段;Drop 时仅解除本进程映射,X server 侧的 attach 由
/// 会话收尾的 `shm_detach` 释放,段在两侧都 detach 后自动回收(IPC_RMID
/// 已在创建时标记)。
struct ShmSegment {
    seg: shm::Seg,
    addr: *mut u8,
    len: usize,
}

// 显式声明:段只在创建它的线程内使用,不跨线程移动。
impl Drop for ShmSegment {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::shmdt(self.addr.cast());
        }
    }
}

/// 呈现目标:优先共享内存,回退 socket 分带上传。
enum PresentBuffer {
    Shm(ShmSegment),
    Socket(Vec<u8>),
}

impl PresentBuffer {
    fn len(&self) -> usize {
        match self {
            PresentBuffer::Shm(segment) => segment.len,
            PresentBuffer::Socket(buf) => buf.len(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            PresentBuffer::Shm(segment) => unsafe {
                std::slice::from_raw_parts_mut(segment.addr, segment.len)
            },
            PresentBuffer::Socket(buf) => buf.as_mut_slice(),
        }
    }
}

struct ShellState {
    hooks: ShellHooks,
    canvas: Canvas,
    /// 预建的光标集;抓取与窗口属性都从这里取当前提示的光标。
    cursors: CursorSet,
    /// 最近一次应用的引擎提示;未变化时不重复切换请求。
    cursor_hint: Option<CursorHint>,
    outcome: Option<RegionOutcome>,
    timing: bool,
}

/// 一次会话预建的字体光标集。字体或单个字形创建失败时对应项为 NONE
/// (显示服务器默认指针),任何失败都不阻断选择/确认/取消。
struct CursorSet {
    font: xproto::Font,
    font_open: bool,
    cursors: [xproto::Cursor; CURSOR_SLOT_COUNT],
}

impl CursorSet {
    /// 打开 "cursor" 字体并预建全部字形;每一步失败都只退化对应槽位。
    fn build(conn: &RustConnection) -> Self {
        let mut set = Self {
            font: NONE,
            font_open: false,
            cursors: [NONE; CURSOR_SLOT_COUNT],
        };
        let Ok(font) = conn.generate_id() else {
            return set;
        };
        if conn.open_font(font, b"cursor").is_err() {
            return set;
        }
        set.font = font;
        set.font_open = true;
        for hint in [
            CursorHint::Crosshair,
            CursorHint::Move,
            CursorHint::ResizeNS,
            CursorHint::ResizeEW,
            CursorHint::ResizeNWSE,
            CursorHint::ResizeNESW,
            CursorHint::Pointer,
        ] {
            let slot = cursor_slot(hint);
            if set.cursors[slot] != NONE {
                continue; // Pointer/Arrow 共槽,已建。
            }
            let Ok(cursor) = conn.generate_id() else {
                continue;
            };
            if build_font_cursor(conn, font, cursor, cursor_glyph(hint)) {
                set.cursors[slot] = cursor;
            }
        }
        set
    }

    fn cursor(&self, hint: CursorHint) -> xproto::Cursor {
        self.cursors[cursor_slot(hint)]
    }

    /// 释放字形光标与字体;失败仅忽略(连接关闭后由服务器回收)。
    fn free(&self, conn: &RustConnection) {
        for &cursor in &self.cursors {
            if cursor != NONE {
                let _ = conn.free_cursor(cursor);
            }
        }
        if self.font_open {
            let _ = conn.close_font(self.font);
        }
    }
}

/// 建单个字体光标:`XCreateFontCursor` 语义(source=glyph、mask=glyph+1,
/// 前景黑/背景白)。掩码字形缺失时退回同字形,再失败放弃该提示。
fn build_font_cursor(
    conn: &RustConnection,
    font: xproto::Font,
    cursor: xproto::Cursor,
    glyph: u16,
) -> bool {
    create_glyph_cursor_checked(conn, font, cursor, glyph, glyph.saturating_add(1))
        || create_glyph_cursor_checked(conn, font, cursor, glyph, glyph)
}

/// 同步校验的字形光标创建:字形未定义时服务器回 BadValue,`check` 捕获后
/// 调用方可安全重试或退回默认指针。
fn create_glyph_cursor_checked(
    conn: &RustConnection,
    font: xproto::Font,
    cursor: xproto::Cursor,
    source_char: u16,
    mask_char: u16,
) -> bool {
    conn.create_glyph_cursor(
        cursor,
        font,
        font,
        source_char,
        mask_char,
        0,
        0,
        0,
        0xffff,
        0xffff,
        0xffff,
    )
    .ok()
    .and_then(|cookie| cookie.check().ok())
    .is_some()
}

/// 一次选区会话的 X 资源与连接引用。
struct Surface<'a> {
    conn: &'a RustConnection,
    window: xproto::Window,
    gc: xproto::Gcontext,
    depth: u8,
}

/// 核心协议键盘表:keycode→keysym(第 0 列,无修饰键位)。
struct KeyboardMap {
    min_keycode: u8,
    per_keycode: usize,
    keysyms: Vec<u32>,
}

impl KeyboardMap {
    fn new(conn: &RustConnection) -> Result<Self, CaptureError> {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let count = setup.max_keycode.saturating_sub(min).saturating_add(1);
        let reply = conn
            .get_keyboard_mapping(min, count)
            .map_err(|_| shell_unavailable())?
            .reply()
            .map_err(|_| shell_unavailable())?;
        Ok(Self {
            min_keycode: min,
            per_keycode: reply.keysyms_per_keycode as usize,
            keysyms: reply.keysyms,
        })
    }

    /// 无修饰列的 keysym(Shift 语义由事件 state 提供,不参与映射)。
    fn plain_keysym(&self, keycode: u8) -> Option<u32> {
        if self.per_keycode == 0 {
            return None;
        }
        let index = keycode.checked_sub(self.min_keycode)? as usize;
        let base = index.checked_mul(self.per_keycode)?;
        let list = self.keysyms.get(base..base + self.per_keycode)?;
        let sym = list.first().copied().unwrap_or(NO_SYMBOL);
        // 部分布局第 0 列为 NoSymbol 而第 1 列有效,做一次回退。
        if sym == NO_SYMBOL {
            let shifted = list.get(1).copied().unwrap_or(NO_SYMBOL);
            return (shifted != NO_SYMBOL).then_some(shifted);
        }
        Some(sym)
    }

    fn logical_key(&self, keycode: u8) -> Option<LogicalKey> {
        match self.plain_keysym(keycode)? {
            XK_RETURN | XK_KP_ENTER => Some(LogicalKey::Enter),
            XK_ESCAPE => Some(LogicalKey::Escape),
            XK_LEFT | XK_KP_LEFT => Some(LogicalKey::ArrowLeft),
            XK_UP | XK_KP_UP => Some(LogicalKey::ArrowUp),
            XK_RIGHT | XK_KP_RIGHT => Some(LogicalKey::ArrowRight),
            XK_DOWN | XK_KP_DOWN => Some(LogicalKey::ArrowDown),
            XK_C_LOWER | XK_C_UPPER => Some(LogicalKey::CopyColor),
            _ => None,
        }
    }
}

/// 驱动一次区域选区:创建 override-redirect 全屏窗并泵事件直到引擎终态。
pub fn pick_region(
    frame: &Frame,
    monitor: &MonitorGeom,
    flags: FeatureFlags,
    hooks: ShellHooks,
) -> Result<RegionOutcome, CaptureError> {
    let composer = Composer::new(frame)?;
    let (frame_w, frame_h) = composer.size();
    let width = frame_w.min(monitor.physical_width).max(1) as i32;
    let height = frame_h.min(monitor.physical_height).max(1) as i32;
    let bytes = frame.rgba.len();

    let (conn, screen_num) = x11rb::connect(None).map_err(|_| shell_unavailable())?;
    let screen = &conn.setup().roots[screen_num];
    let depth = screen.root_depth;
    // 呈现打包假设 32bpp/32bit 扫描对齐;其它格式显式报错而非输出错图。
    let format = conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .ok_or_else(|| CaptureError::api("无法识别当前显示的像素格式。"))?;
    if format.bits_per_pixel != 32 || format.scanline_pad != 32 {
        return Err(CaptureError::api(
            "暂不支持的显示像素格式(需要 32bpp/32bit 对齐)。",
        ));
    }
    let layout = root_visual_layout(conn.setup(), screen)
        .ok_or_else(|| CaptureError::api("无法识别当前显示的视觉格式。"))?;
    let keyboard = KeyboardMap::new(&conn)?;

    let window = conn.generate_id().map_err(|_| id_failed())?;
    let gc = conn.generate_id().map_err(|_| id_failed())?;
    let surface = Surface {
        conn: &conn,
        window,
        gc,
        depth,
    };

    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        screen.root,
        monitor.physical_x as i16,
        monitor.physical_y as i16,
        width as u16,
        height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        COPY_FROM_PARENT,
        &CreateWindowAux::new()
            .override_redirect(1)
            .save_under(1)
            .background_pixmap(NONE)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            ),
    )
    .map_err(|_| window_failed())?
    .check()
    .map_err(|_| window_failed())?;
    conn.create_gc(gc, window, &CreateGCAux::new())
        .map_err(|_| window_failed())?
        .check()
        .map_err(|_| window_failed())?;

    // 光标集:按引擎提示预建字形光标,抓取期间与窗口属性都使用它;
    // 字体/字形缺失时对应提示退回服务器默认指针(不阻断交互)。
    let cursors = CursorSet::build(&conn);
    let grab_cursor = cursors.cursor(CursorHint::Crosshair);

    let mut state = ShellState {
        hooks,
        canvas: Canvas {
            // 注入冻结帧 DPI 缩放:chrome(放大镜面板)光标命中需要。
            engine: SelectionEngine::new(width as u32, height as u32, flags)
                .with_scale(frame.scale),
            composer,
            scratch: vec![0; bytes],
            present: create_present_buffer(&conn, bytes),
            width,
            height,
            layout,
        },
        cursors,
        cursor_hint: Some(CursorHint::Crosshair),
        outcome: None,
        timing: std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some(),
    };

    // 窗口光标兜底:抓取失败时窗口属性仍能显示十字(与抓取光标一致)。
    if grab_cursor != NONE {
        let _ = conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().cursor(grab_cursor),
        );
    }

    // 首帧先合成并写入窗口再映射,避免映射瞬间闪黑(ADR-007:合成只在
    // 引擎 Redraw 时发生,Expose 直接呈现缓存)。
    present(&mut state, &surface);
    conn.map_window(window).map_err(|_| window_failed())?;
    let _ = conn.flush();
    grab_inputs(&conn, window, grab_cursor);
    let _ = conn.flush();

    pump_until_done(&mut state, &surface, &keyboard);

    // 收尾:解除抓取、销毁资源;呈现段的 server 侧 attach 在此释放。
    let _ = conn.ungrab_keyboard(CURRENT_TIME);
    let _ = conn.ungrab_pointer(CURRENT_TIME);
    let _ = conn.destroy_window(window);
    let _ = conn.free_gc(gc);
    state.cursors.free(&conn);
    if let PresentBuffer::Shm(segment) = &state.canvas.present {
        let _ = conn.shm_detach(segment.seg);
    }
    let _ = conn.flush();
    Ok(state.outcome.take().unwrap_or(RegionOutcome::Cancelled))
}

fn shell_unavailable() -> CaptureError {
    CaptureError::unavailable("当前会话无法连接 X11 服务器,无法打开原生选区窗。")
}

fn id_failed() -> CaptureError {
    CaptureError::api("无法分配 X11 资源。")
}

fn window_failed() -> CaptureError {
    CaptureError::api("无法打开截取窗。")
}

/// 从 allowed_depths 找根视觉的掩码,生成像素打包布局。
fn root_visual_layout(setup: &Setup, screen: &Screen) -> Option<VisualLayout> {
    let visual = screen
        .allowed_depths
        .iter()
        .find(|depth| depth.depth == screen.root_depth)?
        .visuals
        .iter()
        .find(|visual: &&Visualtype| visual.visual_id == screen.root_visual)?;
    if visual.red_mask | visual.green_mask | visual.blue_mask == 0 {
        return None;
    }
    Some(VisualLayout {
        red: ChannelPack::new(visual.red_mask),
        green: ChannelPack::new(visual.green_mask),
        blue: ChannelPack::new(visual.blue_mask),
        msb_first: setup.image_byte_order == ImageOrder::MSB_FIRST,
    })
}

/// 抓取指针与键盘,保证选区期间独占输入;失败仅记录(鼠标路径仍可用)。
fn grab_inputs(conn: &RustConnection, window: xproto::Window, cursor: xproto::Cursor) {
    let pointer = conn
        .grab_pointer(
            false,
            window,
            EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            window,
            cursor,
            CURRENT_TIME,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.status == GrabStatus::SUCCESS);
    let keyboard = conn
        .grab_keyboard(false, window, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.status == GrabStatus::SUCCESS);
    if std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some() {
        eprintln!(
            "Cropmark X11 grabs: pointer={pointer:?}, keyboard={keyboard:?}"
        );
    }
}

/// MIT-SHM 探测:扩展存在 + 段 attach 成功才启用;否则回退 socket 缓冲。
fn create_present_buffer(conn: &RustConnection, len: usize) -> PresentBuffer {
    match create_shm_segment(conn, len) {
        Some(segment) => PresentBuffer::Shm(segment),
        None => PresentBuffer::Socket(vec![0; len]),
    }
}

/// 建共享内存段并 attach 到 X server(checked:attach 失败立即回退)。
fn create_shm_segment(conn: &RustConnection, len: usize) -> Option<ShmSegment> {
    let present = conn
        .extension_information(shm::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_some();
    if !present || len == 0 {
        return None;
    }
    unsafe {
        let shmid = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
        if shmid < 0 {
            return None;
        }
        let addr = libc::shmat(shmid, std::ptr::null(), 0) as *mut u8;
        if addr as isize == -1 || addr.is_null() {
            let _ = libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            return None;
        }
        // 标记删除:两侧 detach 后自动回收,进程崩溃也不残留。
        let _ = libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
        // shmat 之后的任何失败都必须解除本进程映射,否则段标记了 IPC_RMID
        // 也要等到进程退出才回收(本函数可能每帧重试,泄漏会累积)。
        let Ok(seg) = conn.generate_id() else {
            let _ = libc::shmdt(addr.cast());
            return None;
        };
        let attach_failed = match conn.shm_attach(seg, shmid as u32, false) {
            Ok(cookie) => cookie.check().is_err(),
            Err(_) => true,
        };
        if attach_failed {
            let _ = libc::shmdt(addr.cast());
            return None;
        }
        Some(ShmSegment { seg, addr, len })
    }
}

/// 手动泵事件直到引擎给出终态;连接错误按取消处理(会话层恢复原状)。
fn pump_until_done(state: &mut ShellState, surface: &Surface<'_>, keyboard: &KeyboardMap) {
    loop {
        let event = match surface.conn.wait_for_event() {
            Ok(event) => event,
            Err(_) => return,
        };
        let done = match event {
            XEvent::ButtonPress(e) => match e.detail {
                1 => feed_event(
                    state,
                    surface,
                    InputEvent::LeftDown {
                        x: e.event_x as i32,
                        y: e.event_y as i32,
                    },
                ),
                3 => feed_event(
                    state,
                    surface,
                    InputEvent::RightDown {
                        x: e.event_x as i32,
                        y: e.event_y as i32,
                    },
                ),
                _ => false,
            },
            XEvent::ButtonRelease(e) if e.detail == 1 => feed_event(
                state,
                surface,
                InputEvent::LeftUp {
                    x: e.event_x as i32,
                    y: e.event_y as i32,
                },
            ),
            XEvent::MotionNotify(e) => feed_event(
                state,
                surface,
                InputEvent::PointerMove {
                    x: e.event_x as i32,
                    y: e.event_y as i32,
                },
            ),
            XEvent::KeyPress(e) => {
                let shift = u16::from(e.state) & u16::from(KeyButMask::SHIFT) != 0;
                keyboard
                    .logical_key(e.detail)
                    .map(|key| feed_event(state, surface, InputEvent::Key { key, shift }))
                    .unwrap_or(false)
            }
            // 无效验请求的错误回执会以事件形式送达;选区窗的绘制/抓取
            // 请求均为尽力而为,忽略错误继续泵。
            XEvent::Error(_) => false,
            // Expose:直接呈现缓存位图,不重合成(ADR-007)。
            XEvent::Expose(_) => {
                put_frame(surface, &state.canvas);
                false
            }
            _ => false,
        };
        if done {
            return;
        }
    }
}

/// 依引擎当前提示切换光标(X11 光标提示,ADR-5);提示未变时跳过。切换
/// 全部尽力而为:字体缺失、抓取不存在(BadGrab)等错误只会显示默认指针,
/// 不影响选择/确认/取消。
fn sync_cursor(state: &mut ShellState, surface: &Surface<'_>) {
    let (x, y) = state.canvas.engine.cursor();
    let hint = state.canvas.engine.cursor_for(x, y);
    if state.cursor_hint == Some(hint) {
        return;
    }
    state.cursor_hint = Some(hint);
    let cursor = state.cursors.cursor(hint);
    if cursor == NONE {
        return;
    }
    // 窗口属性覆盖"未抓取/抓取失败"时的指针;活动抓取(存在时)另经
    // ChangeActivePointerGrab 立即换光标,否则服务器回 BadGrab 错误事件,
    // 由泵内忽略。
    let _ = surface.conn.change_window_attributes(
        surface.window,
        &ChangeWindowAttributesAux::new().cursor(cursor),
    );
    let _ = surface.conn.change_active_pointer_grab(
        cursor,
        CURRENT_TIME,
        EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
    );
}

/// 与 Windows 壳同构的 EngineOutcome 处理:Redraw→重呈现、
/// Confirmed→Preview、Cancelled→取消、Action→会话侧完成或复制色值。
fn feed_event(state: &mut ShellState, surface: &Surface<'_>, event: InputEvent) -> bool {
    let outcome = state.canvas.engine.handle_event(event);
    sync_cursor(state, surface);
    match outcome {
        EngineOutcome::Redraw => {
            present(state, surface);
            false
        }
        EngineOutcome::Confirmed(rect) => {
            state.outcome = Some(RegionOutcome::Preview(rect));
            true
        }
        EngineOutcome::Cancelled => {
            state.outcome = Some(RegionOutcome::Cancelled);
            true
        }
        EngineOutcome::Action(action) => match action {
            SelectionAction::Annotate => {
                if let Some(outcome) = annotate_outcome(state.canvas.engine.selection()) {
                    state.outcome = Some(outcome);
                    return true;
                }
                false
            }
            SelectionAction::Cancel => {
                // 引擎在菜单路径已把「取消」译为 Cancelled;此支仅为防御。
                state.outcome = Some(RegionOutcome::Cancelled);
                true
            }
            SelectionAction::CopyColor => {
                copy_color_value(state);
                false
            }
            quiet => {
                if let (Some(rect), Some(action)) =
                    (state.canvas.engine.selection(), quiet_action_for(quiet))
                {
                    state.outcome = Some(RegionOutcome::Quiet(rect, action));
                    return true;
                }
                false
            }
        },
    }
}

/// 「标注」动作到壳结果的映射:引擎尚无选区时返回 None,会话继续等待。
fn annotate_outcome(selection: Option<PhysicalRect>) -> Option<RegionOutcome> {
    selection.map(RegionOutcome::Annotate)
}

/// 操作条/菜单动作到静默完成动作的映射;标注/取消/复制色值不在此列。
fn quiet_action_for(action: SelectionAction) -> Option<QuietAction> {
    match action {
        SelectionAction::Copy => Some(QuietAction::Copy),
        SelectionAction::Save => Some(QuietAction::Save),
        SelectionAction::Pin => Some(QuietAction::Pin),
        SelectionAction::Ocr => Some(QuietAction::Ocr),
        SelectionAction::Annotate | SelectionAction::Cancel | SelectionAction::CopyColor => None,
    }
}

/// C 键取色:按引擎光标从冻结帧采样,经回调复制 HEX+RGB 文本并反馈。
fn copy_color_value(state: &mut ShellState) {
    let cursor = state.canvas.engine.cursor();
    let pixel = state.canvas.composer.sample(cursor.0, cursor.1);
    let hex = composer::hex_readout(pixel);
    let text = format!("{} {}", hex, composer::rgb_readout(pixel));
    (state.hooks.copy_color)(&text, &hex);
}

/// 单次合成耗时(仅 Redraw 路径调用;ADR-007 护栏的可观察基线)。
fn compose_canvas(canvas: &mut Canvas) -> Option<Duration> {
    let started = Instant::now();
    let (w, h) = canvas.composer.size();
    let expected = w as usize * h as usize * 4;
    // 壳侧防御:compose_into 要求 out 长度与冻结帧严格一致,越界会 panic;
    // 长度不符时跳过本次合成而非崩溃。
    if canvas.scratch.len() == expected && canvas.present.len() == expected {
        let scene = canvas.engine.scene();
        canvas.composer.compose_into(&scene, &mut canvas.scratch);
        let layout = canvas.layout;
        let present = canvas.present.as_mut_slice();
        pack_rgba_to_x(&canvas.scratch, present, &layout);
        return Some(started.elapsed());
    }
    None
}

/// RGBA→X 像素打包:按根视觉掩码组像素值,再按服务器字节序写 4 字节单元。
/// 长度不齐时,剩余尾部不参与交换。
fn pack_rgba_to_x(src: &[u8], dst: &mut [u8], layout: &VisualLayout) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let pixel = layout.red.pack(s[0]) | layout.green.pack(s[1]) | layout.blue.pack(s[2]);
        if layout.msb_first {
            d[0] = (pixel >> 24) as u8;
            d[1] = (pixel >> 16) as u8;
            d[2] = (pixel >> 8) as u8;
            d[3] = pixel as u8;
        } else {
            d[0] = pixel as u8;
            d[1] = (pixel >> 8) as u8;
            d[2] = (pixel >> 16) as u8;
            d[3] = (pixel >> 24) as u8;
        }
    }
}

/// 仅在引擎要求 Redraw 时重合成并上屏;Expose 路径直接呈现缓存位图。
fn present(state: &mut ShellState, surface: &Surface<'_>) {
    let started = if state.timing {
        Some(Instant::now())
    } else {
        None
    };
    let compose_at = compose_canvas(&mut state.canvas);
    put_frame(surface, &state.canvas);
    if let Some(started) = started {
        eprintln!(
            "Cropmark overlay {}x{} present: compose+pack={:?}, total={:?}",
            state.canvas.width,
            state.canvas.height,
            compose_at,
            started.elapsed()
        );
    }
}

/// 把呈现缓冲写入窗口:MIT-SHM 单请求直传,回退路径按行分带 XPutImage。
fn put_frame(surface: &Surface<'_>, canvas: &Canvas) {
    match &canvas.present {
        PresentBuffer::Shm(segment) => {
            let _ = surface.conn.shm_put_image(
                surface.window,
                surface.gc,
                canvas.width as u16,
                canvas.height as u16,
                0,
                0,
                canvas.width as u16,
                canvas.height as u16,
                0,
                0,
                surface.depth,
                u8::from(ImageFormat::Z_PIXMAP),
                false,
                segment.seg,
                0,
            );
        }
        PresentBuffer::Socket(buf) => {
            put_image_banded(
                surface.conn,
                surface.window,
                surface.gc,
                surface.depth,
                canvas.width,
                canvas.height,
                buf,
            );
        }
    }
    let _ = surface.conn.flush();
}

/// XPutImage 分带上传:每带 ≤ `PUT_IMAGE_BAND_LIMIT`,适配保守的
/// max-request-length(4K 全帧 33MiB 会超过部分服务器的单请求上限)。
fn put_image_banded(
    conn: &RustConnection,
    window: xproto::Window,
    gc: xproto::Gcontext,
    depth: u8,
    width: i32,
    height: i32,
    data: &[u8],
) {
    let stride = (width as usize).max(1) * 4;
    if data.len() < stride * height as usize {
        return;
    }
    let band_rows = (PUT_IMAGE_BAND_LIMIT / stride).max(1);
    let mut y = 0usize;
    while y < height as usize {
        let rows = band_rows.min(height as usize - y);
        let start = y * stride;
        let end = (y + rows) * stride;
        let _ = conn.put_image(
            ImageFormat::Z_PIXMAP,
            window,
            gc,
            width as u16,
            rows as u16,
            0,
            y as i16,
            0,
            depth,
            &data[start..end],
        );
        y += rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::{accept_buffer, RawBuffer};
    use crate::capture::selection::{CursorHint, EngineState};

    fn test_frame(width: u32, height: u32, scale: f64) -> Frame {
        let mut frame = accept_buffer(RawBuffer::ready(
            width,
            height,
            vec![80u8; (width * height * 4) as usize],
        ))
        .unwrap();
        frame.scale = scale;
        frame
    }

    fn engine_from_frame(frame: &Frame) -> SelectionEngine {
        SelectionEngine::new(frame.width, frame.height, FeatureFlags::default())
            .with_scale(frame.scale)
    }

    /// min_keycode=8、每键 2 列的小键盘表:8=Return,9=Esc,10/11/12/13=方向,
    /// 14=c,17-21=Keypad Enter/方向,22='B';shift 列(奇数索引)填 NoSymbol
    /// 验证回退,15 的第 0 列无效、16/22 不在引擎语义内。
    fn test_keyboard_map() -> KeyboardMap {
        KeyboardMap {
            min_keycode: 8,
            per_keycode: 2,
            keysyms: vec![
                XK_RETURN, NO_SYMBOL, XK_ESCAPE, NO_SYMBOL, XK_LEFT, NO_SYMBOL, XK_UP, NO_SYMBOL,
                XK_RIGHT, NO_SYMBOL, XK_DOWN, NO_SYMBOL, XK_C_LOWER, NO_SYMBOL, NO_SYMBOL,
                XK_C_UPPER, NO_SYMBOL, 0x0041, XK_KP_ENTER, NO_SYMBOL, XK_KP_LEFT, NO_SYMBOL,
                XK_KP_UP, NO_SYMBOL, XK_KP_RIGHT, NO_SYMBOL, XK_KP_DOWN, NO_SYMBOL, 0x0042,
                NO_SYMBOL,
            ],
        }
    }

    #[test]
    fn keysyms_map_to_engine_logical_keys() {
        let map = test_keyboard_map();
        assert_eq!(map.logical_key(8), Some(LogicalKey::Enter));
        assert_eq!(map.logical_key(9), Some(LogicalKey::Escape));
        assert_eq!(map.logical_key(10), Some(LogicalKey::ArrowLeft));
        assert_eq!(map.logical_key(11), Some(LogicalKey::ArrowUp));
        assert_eq!(map.logical_key(12), Some(LogicalKey::ArrowRight));
        assert_eq!(map.logical_key(13), Some(LogicalKey::ArrowDown));
        assert_eq!(map.logical_key(14), Some(LogicalKey::CopyColor));
        // 15:第 0 列 NoSymbol 回退到第 1 列(大写 C 同样是取字键)。
        assert_eq!(map.logical_key(15), Some(LogicalKey::CopyColor));
        // 16:'A' 不在引擎语义内;17-21:数字小键盘的 Enter/方向键与主键盘同义。
        assert_eq!(map.logical_key(16), None);
        assert_eq!(map.logical_key(17), Some(LogicalKey::Enter));
        assert_eq!(map.logical_key(18), Some(LogicalKey::ArrowLeft));
        assert_eq!(map.logical_key(19), Some(LogicalKey::ArrowUp));
        assert_eq!(map.logical_key(20), Some(LogicalKey::ArrowRight));
        assert_eq!(map.logical_key(21), Some(LogicalKey::ArrowDown));
        // 22:'B' 不在引擎语义内;23:超出表尾。
        assert_eq!(map.logical_key(22), None);
        assert_eq!(map.logical_key(23), None);
        // 超出键盘表范围。
        assert_eq!(map.logical_key(7), None);
        assert_eq!(map.logical_key(100), None);
    }

    #[test]
    fn empty_keyboard_table_maps_nothing() {
        let map = KeyboardMap {
            min_keycode: 8,
            per_keycode: 0,
            keysyms: Vec::new(),
        };
        assert_eq!(map.logical_key(8), None);
    }

    #[test]
    fn cursor_glyphs_map_hints_to_cursor_font_shapes() {
        assert_eq!(cursor_glyph(CursorHint::Crosshair), XC_CROSSHAIR);
        assert_eq!(cursor_glyph(CursorHint::Move), XC_FLEUR);
        assert_eq!(cursor_glyph(CursorHint::ResizeNS), XC_SB_V_DOUBLE_ARROW);
        assert_eq!(cursor_glyph(CursorHint::ResizeEW), XC_SB_H_DOUBLE_ARROW);
        assert_eq!(cursor_glyph(CursorHint::ResizeNWSE), XC_TOP_LEFT_CORNER);
        assert_eq!(cursor_glyph(CursorHint::ResizeNESW), XC_TOP_RIGHT_CORNER);
        // 可点击 chrome 与放大镜面板都用默认箭头。
        assert_eq!(cursor_glyph(CursorHint::Pointer), XC_LEFT_PTR);
        assert_eq!(cursor_glyph(CursorHint::Arrow), XC_LEFT_PTR);
    }

    #[test]
    fn cursor_slots_are_distinct_except_pointer_and_arrow() {
        let slots = [
            cursor_slot(CursorHint::Crosshair),
            cursor_slot(CursorHint::Move),
            cursor_slot(CursorHint::ResizeNS),
            cursor_slot(CursorHint::ResizeEW),
            cursor_slot(CursorHint::ResizeNWSE),
            cursor_slot(CursorHint::ResizeNESW),
        ];
        assert_eq!(slots, [0, 1, 2, 3, 4, 5]);
        assert_eq!(
            cursor_slot(CursorHint::Pointer),
            cursor_slot(CursorHint::Arrow)
        );
        assert!(!slots.contains(&cursor_slot(CursorHint::Pointer)));
        for hint in [
            CursorHint::Crosshair,
            CursorHint::Move,
            CursorHint::ResizeNS,
            CursorHint::ResizeEW,
            CursorHint::ResizeNWSE,
            CursorHint::ResizeNESW,
            CursorHint::Pointer,
            CursorHint::Arrow,
        ] {
            assert!(cursor_slot(hint) < CURSOR_SLOT_COUNT);
        }
    }

    /// 字体/字形不可用的槽位解析为 NONE:光标切换失败只退回默认指针,
    /// 不产生错误路径,也不阻断交互。
    #[test]
    fn unavailable_cursor_slots_resolve_to_none() {
        let mut set = CursorSet {
            font: NONE,
            font_open: false,
            cursors: [NONE; CURSOR_SLOT_COUNT],
        };
        assert_eq!(set.cursor(CursorHint::Crosshair), NONE);
        assert_eq!(set.cursor(CursorHint::ResizeNWSE), NONE);
        // 部分槽位构建成功后,其余提示仍解析为 NONE。
        set.cursors[cursor_slot(CursorHint::ResizeEW)] = 42;
        assert_eq!(set.cursor(CursorHint::ResizeEW), 42);
        assert_eq!(set.cursor(CursorHint::ResizeNS), NONE);
        // Pointer/Arrow 共用槽:一个可用则两者都可用。
        set.cursors[cursor_slot(CursorHint::Pointer)] = 43;
        assert_eq!(set.cursor(CursorHint::Arrow), 43);
    }

    #[test]
    fn quiet_actions_map_and_exclude_annotate_cancel_color() {
        assert_eq!(
            quiet_action_for(SelectionAction::Copy),
            Some(QuietAction::Copy)
        );
        assert_eq!(
            quiet_action_for(SelectionAction::Save),
            Some(QuietAction::Save)
        );
        assert_eq!(
            quiet_action_for(SelectionAction::Pin),
            Some(QuietAction::Pin)
        );
        assert_eq!(
            quiet_action_for(SelectionAction::Ocr),
            Some(QuietAction::Ocr)
        );
        assert_eq!(quiet_action_for(SelectionAction::Annotate), None);
        assert_eq!(quiet_action_for(SelectionAction::Cancel), None);
        assert_eq!(quiet_action_for(SelectionAction::CopyColor), None);
    }

    #[test]
    fn annotate_outcome_needs_a_selection_and_keeps_rect() {
        let rect = PhysicalRect {
            x: 5,
            y: 6,
            width: 30,
            height: 40,
        };
        assert_eq!(
            annotate_outcome(Some(rect)),
            Some(RegionOutcome::Annotate(rect))
        );
        assert_eq!(annotate_outcome(None), None);
    }

    #[test]
    fn pack_rgba_to_x_matches_typical_bgra_layout() {
        let src = [0x2D, 0xD4, 0xBF, 255, 1, 2, 3, 4];
        let mut dst = vec![0u8; 8];
        pack_rgba_to_x(&src, &mut dst, &VisualLayout::typical());
        // 像素 0x2DD4BF,LSBFirst 字节序:B,G,R,pad。
        assert_eq!(dst, vec![0xBF, 0xD4, 0x2D, 0, 3, 2, 1, 0]);
        // 长度不齐时,剩余尾部不参与交换。
        let src = [0x2D, 0xD4, 0xBF, 255, 9];
        let mut dst = vec![0u8; 5];
        pack_rgba_to_x(&src, &mut dst, &VisualLayout::typical());
        assert_eq!(dst, vec![0xBF, 0xD4, 0x2D, 0, 0]);
    }

    #[test]
    fn pack_rgba_to_x_honors_msb_first_order() {
        let src = [0x2D, 0xD4, 0xBF, 255];
        let mut dst = vec![0u8; 4];
        let mut layout = VisualLayout::typical();
        layout.msb_first = true;
        pack_rgba_to_x(&src, &mut dst, &layout);
        assert_eq!(dst, vec![0, 0x2D, 0xD4, 0xBF]);
    }

    #[test]
    fn channel_pack_scales_sub_byte_masks() {
        // 5 位量程:255 → 31,0 → 0,128 → 16(四舍五入)。
        let five_bit = ChannelPack::new(0x001f);
        assert_eq!(five_bit.pack(255), 31);
        assert_eq!(five_bit.pack(0), 0);
        assert_eq!(five_bit.pack(128), 16);
        // 掩码位移:0x03e0 → shift=5。
        assert_eq!(ChannelPack::new(0x03e0).shift, 5);
        // 掩码缺失的通道恒为 0(不移位,防溢出)。
        assert_eq!(ChannelPack::new(0).pack(255), 0);
    }

    #[test]
    fn color_text_is_hex_plus_rgb() {
        let pixel = [0x2D, 0xD4, 0xBF, 255];
        let text = format!(
            "{} {}",
            composer::hex_readout(pixel),
            composer::rgb_readout(pixel)
        );
        assert_eq!(text, "#2DD4BF R 45 G 212 B 191");
    }

    #[test]
    fn compose_skips_on_mismatched_buffer_lengths() {
        let frame = accept_buffer(RawBuffer::ready(4, 4, vec![9u8; 64])).unwrap();
        let composer = Composer::new(&frame).unwrap();
        let mut canvas = Canvas {
            engine: engine_from_frame(&frame),
            composer,
            scratch: vec![0; 64],
            present: PresentBuffer::Socket(vec![0; 8]), // 故意错误长度:防御路径不 panic。
            width: 4,
            height: 4,
            layout: VisualLayout::typical(),
        };
        assert!(compose_canvas(&mut canvas).is_none());
    }

    #[test]
    fn selection_engine_uses_frame_scale_for_magnifier_hit() {
        let cursor = (40, 40);
        let scaled_frame = test_frame(800, 800, 2.0);
        let mut scaled = engine_from_frame(&scaled_frame);
        scaled.handle_event(InputEvent::PointerMove {
            x: cursor.0,
            y: cursor.1,
        });
        let size = scaled.size();
        let unscaled_panel = composer::magnifier_rect(cursor, size, 1.0);
        let scaled_panel = composer::magnifier_rect(cursor, size, 2.0);
        let mut probe = None;
        for y in scaled_panel.y..scaled_panel.bottom() {
            for x in scaled_panel.x..scaled_panel.right() {
                if !unscaled_panel.contains(x, y) {
                    probe = Some((x, y));
                    break;
                }
            }
            if probe.is_some() {
                break;
            }
        }
        let (x, y) = probe.expect("scale 2.0 magnifier should extend past 1.0");
        assert_eq!(scaled.cursor_for(x, y), CursorHint::Arrow);

        let unscaled_frame = test_frame(800, 800, 1.0);
        let mut unscaled = engine_from_frame(&unscaled_frame);
        unscaled.handle_event(InputEvent::PointerMove {
            x: cursor.0,
            y: cursor.1,
        });
        assert_eq!(unscaled.cursor_for(x, y), CursorHint::Crosshair);
    }

    #[test]
    fn toolbar_action_fires_on_left_up_not_press() {
        let frame = test_frame(320, 200, 1.5);
        let mut engine = engine_from_frame(&frame);
        engine.handle_event(InputEvent::LeftDown { x: 40, y: 30 });
        engine.handle_event(InputEvent::PointerMove { x: 200, y: 120 });
        engine.handle_event(InputEvent::LeftUp { x: 200, y: 120 });
        assert!(engine.scene().toolbar_visible);

        let buttons = composer::toolbar_buttons(engine.flags());
        let panel =
            composer::toolbar_panel(engine.selection().unwrap(), engine.size(), &buttons).unwrap();
        let (expected, rect) = composer::toolbar_button_rects(panel, &buttons)
            .last()
            .copied()
            .unwrap();
        let (cx, cy) = rect.center();
        assert_eq!(
            engine.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
            EngineOutcome::Redraw
        );
        assert!(matches!(
            engine.state(),
            EngineState::PressingChrome { action } if *action == expected
        ));
        assert!(engine.scene().toolbar_visible);
        assert_eq!(
            engine.handle_event(InputEvent::LeftUp { x: cx, y: cy }),
            EngineOutcome::Action(expected)
        );
    }

    #[test]
    fn put_image_banded_splits_by_request_limit() {
        // 宽 256(步长 1KiB):每带 1024 行;总高 2048 → 两带。
        let width = 256;
        let height = 2048;
        let data = vec![7u8; width as usize * 4 * height as usize];
        // 仅验证分带数学:band_rows 与循环推进可通过边界值断言。
        let stride = width as usize * 4;
        let band_rows = (PUT_IMAGE_BAND_LIMIT / stride).max(1);
        assert_eq!(band_rows, 1024);
        assert_eq!((height as usize).div_ceil(band_rows), 2);
        // 分带覆盖全部行且不越界。
        let mut covered = 0usize;
        let mut y = 0usize;
        while y < height as usize {
            let rows = band_rows.min(height as usize - y);
            assert!(rows > 0 && y + rows <= height as usize);
            covered += rows;
            y += rows;
        }
        assert_eq!(covered, height as usize);
        assert!(data.len() >= stride * height as usize);
    }

    /// ADR-007 预算护栏:4K 冻结帧下选区+操作条+放大镜场景的 compose+pack
    /// 耗时基线(无 X 连接也可测量;呈现在 X 实机上人工复核)。
    /// 手动运行:`cargo test --release shell::linux -- --ignored --nocapture`。
    #[test]
    #[ignore = "耗时基线,手动 --release --nocapture 运行"]
    fn four_k_compose_pack_budget() {
        let (w, h) = (3840u32, 2160u32);
        let bytes = vec![80u8; (w * h * 4) as usize];
        let frame = accept_buffer(RawBuffer::ready(w, h, bytes)).unwrap();
        let mut canvas = Canvas {
            engine: engine_from_frame(&frame),
            composer: Composer::new(&frame).unwrap(),
            scratch: vec![0; frame.rgba.len()],
            present: PresentBuffer::Socket(vec![0; frame.rgba.len()]),
            width: w as i32,
            height: h as i32,
            layout: VisualLayout::typical(),
        };
        canvas
            .engine
            .handle_event(InputEvent::LeftDown { x: 600, y: 400 });
        canvas
            .engine
            .handle_event(InputEvent::PointerMove { x: 2600, y: 1800 });
        canvas
            .engine
            .handle_event(InputEvent::LeftUp { x: 2600, y: 1800 });
        let scene = canvas.engine.scene();
        assert!(scene.selection.is_some() && scene.toolbar_visible);
        assert!(compose_canvas(&mut canvas).is_some());
        let mut worst = Duration::ZERO;
        let mut total = Duration::ZERO;
        let rounds = 60;
        for _ in 0..rounds {
            let elapsed = compose_canvas(&mut canvas).expect("lengths match");
            worst = worst.max(elapsed);
            total += elapsed;
        }
        eprintln!(
            "4K compose+pack: rounds={rounds}, avg={:?}, worst={:?}",
            total / rounds,
            worst
        );
    }
}
