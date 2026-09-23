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
//! 光标提示(ADR-5/ADR-1):引擎 `cursor_for` 的提示经 "cursor" 字体字形
//! (`XCreateFontCursor` 语义:source=glyph、mask=glyph+1)映射为 fleur/resize
//! 箭头/默认指针;Crosshair 使用 pixmap 双色十字(浅芯深描边、热点镂空),不把
//! 单色 `XC_CROSSHAIR` 当作浅色底上的唯一指示。输入事件后按提示变化切换
//! (窗口属性等价 XDefineCursor;活动抓取另经 `ChangeActivePointerGrab` 立即
//! 换光标)。字体或单个字形不可用时该提示退回服务器默认指针;Crosshair
//! pixmap 失败则用更小的双色位图,仍不回退单色字形。不阻断选择/确认/取消。
//!
//! 键盘:keycode→keysym 用核心协议 GetKeyboardMapping 的第 0 列(无修饰
//! 键位),方向键/Enter/Esc/C 与布局无关;Shift 状态取事件 state 的
//! KeyButMask::SHIFT 位,与 windows.rs 的 VK_SHIFT 跟踪同义。
//!
//! 文本输入(R21):Xlib 侧独立连接(链接 libX11,CI/构建依赖已含 libx11-dev)
//! 上开 XIM(`XOpenIM`/`XCreateIC`,XIMPreeditNothing|XIMStatusNothing),按键经
//! `XFilterEvent` 转发给 IM、IM 回放/提交经 `XInternalConnectionNumbers` +
//! `XProcessInternalConnection` 取回后用 `Xutf8LookupString` 读出;
//! `XOpenDisplay`/`XOpenIM` 失败时只失去 IME,ASCII 直输仍走核心键盘映射的
//! keysym→字符回退,选择/确认/取消不受影响。
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

use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shm::{self, ConnectionExt as ShmExt};
use x11rb::protocol::xproto::{
    self, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as XprotoExt, CreateGCAux,
    CreateWindowAux, EventMask, GrabMode, GrabStatus, ImageFormat, ImageOrder, KeyButMask, Screen,
    Setup, Visualtype, WindowClass,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE, NO_SYMBOL};

use crate::annotate::Annotation;
use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;
use crate::capture::geometry::{MonitorGeom, PhysicalRect};
use crate::capture::selection::composer::{self, Composer};
use crate::capture::selection::{
    AnnotationOptions, AnnotationTool, CursorHint, EngineOutcome, FeatureFlags, InputEvent,
    LogicalKey, SelectionAction, SelectionEngine,
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
const XK_Y_LOWER: u32 = 0x79;
const XK_Y_UPPER: u32 = 0x59;
const XK_Z_LOWER: u32 = 0x7a;
const XK_Z_UPPER: u32 = 0x5a;
const XK_BACKSPACE: u32 = 0xff08;
const XK_DELETE: u32 = 0xffff;
// 标注工具快捷键(与预览编辑器一致:A/R/E/L/M/B/H/P/N/T)。
const XK_A_LOWER: u32 = 0x61;
const XK_A_UPPER: u32 = 0x41;
const XK_B_LOWER: u32 = 0x62;
const XK_B_UPPER: u32 = 0x42;
const XK_E_LOWER: u32 = 0x65;
const XK_E_UPPER: u32 = 0x45;
const XK_H_LOWER: u32 = 0x68;
const XK_H_UPPER: u32 = 0x48;
const XK_L_LOWER: u32 = 0x6c;
const XK_L_UPPER: u32 = 0x4c;
const XK_M_LOWER: u32 = 0x6d;
const XK_M_UPPER: u32 = 0x4d;
const XK_N_LOWER: u32 = 0x6e;
const XK_N_UPPER: u32 = 0x4e;
const XK_P_LOWER: u32 = 0x70;
const XK_P_UPPER: u32 = 0x50;
const XK_R_LOWER: u32 = 0x72;
const XK_R_UPPER: u32 = 0x52;
const XK_T_LOWER: u32 = 0x74;
const XK_T_UPPER: u32 = 0x54;

// "cursor" 字体中的标准字形(X11/cursorfont.h 的 XC_* 常量)。每个光标占两个
// 字符码:source=glyph、mask=glyph+1(XCreateFontCursor 语义)。
#[cfg(test)]
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
/// (放大镜面板)在 X11 下都使用默认箭头。Crosshair 走 pixmap 双色十字,
/// 不映射 XC_CROSSHAIR(ADR-1)。
fn cursor_glyph(hint: CursorHint) -> Option<u16> {
    match hint {
        CursorHint::Crosshair => None,
        CursorHint::Move => Some(XC_FLEUR),
        CursorHint::ResizeNS => Some(XC_SB_V_DOUBLE_ARROW),
        CursorHint::ResizeEW => Some(XC_SB_H_DOUBLE_ARROW),
        CursorHint::ResizeNWSE => Some(XC_TOP_LEFT_CORNER),
        CursorHint::ResizeNESW => Some(XC_TOP_RIGHT_CORNER),
        CursorHint::Pointer | CursorHint::Arrow => Some(XC_LEFT_PTR),
    }
}

/// 双色十字像素:浅色线芯 + 深色描边;Empty 为透明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrosshairPixel {
    Empty,
    Core,
    Outline,
}

/// 运行时十字规格(奇数边长,热点在交叉点)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CrosshairSprite {
    size: usize,
    arm: usize,
}

impl CrosshairSprite {
    /// 1x 基准臂长(物理像素);实际臂长按 frame.scale 缩放。
    const BASE_ARM: usize = 10;

    /// 最小双色位图(光标创建失败时的兜底规格)。
    const fn fallback() -> Self {
        Self { size: 9, arm: 3 }
    }

    /// 按冻结帧 DPI 缩放生成规格:臂长随 frame.scale 缩放(200% 时
    /// 翻倍),线芯恒 1 物理像素;边长保持奇数,热点在交叉点。
    fn for_scale(scale: f64) -> Self {
        let arm = ((Self::BASE_ARM as f64) * scale.max(0.1)).round().max(3.0) as usize;
        Self {
            size: arm * 2 + 5,
            arm,
        }
    }

    fn hotspot(self) -> (usize, usize) {
        let center = self.size / 2;
        (center, center)
    }

    /// 线芯:横竖臂上距中心 1..=arm 的像素。中心热点像素镂空,
    /// 指针像素经镂空处直接可见;芯宽恒 1 物理像素。
    fn is_core(self, x: usize, y: usize) -> bool {
        let center = self.size / 2;
        let dist = if x == center {
            y.abs_diff(center)
        } else if y == center {
            x.abs_diff(center)
        } else {
            return false;
        };
        (1..=self.arm).contains(&dist)
    }

    fn pixel(self, x: usize, y: usize) -> CrosshairPixel {
        if x >= self.size || y >= self.size {
            return CrosshairPixel::Empty;
        }
        let center = self.size / 2;
        // 中心热点像素镂空:热点仍对准指针像素,但该像素透明。
        if x == center && y == center {
            return CrosshairPixel::Empty;
        }
        if self.is_core(x, y) {
            return CrosshairPixel::Core;
        }
        let x0 = x.saturating_sub(1);
        let y0 = y.saturating_sub(1);
        let x1 = (x + 1).min(self.size - 1);
        let y1 = (y + 1).min(self.size - 1);
        for nx in x0..=x1 {
            for ny in y0..=y1 {
                if (nx != x || ny != y) && self.is_core(nx, ny) {
                    return CrosshairPixel::Outline;
                }
            }
        }
        CrosshairPixel::Empty
    }
}

/// 1-bit XYBitmap 打包:按 setup 的 bit order 与 scanline pad。
fn pack_bitmap(
    size: usize,
    mut set: impl FnMut(usize, usize) -> bool,
    lsb_first: bool,
    scanline_pad_bits: u8,
) -> Vec<u8> {
    let pad = usize::from(scanline_pad_bits.max(8));
    let stride_bits = size.div_ceil(pad) * pad;
    let stride = stride_bits / 8;
    let mut data = vec![0u8; stride * size];
    for y in 0..size {
        for x in 0..size {
            if !set(x, y) {
                continue;
            }
            let bit = if lsb_first { x % 8 } else { 7 - (x % 8) };
            data[y * stride + x / 8] |= 1 << bit;
        }
    }
    data
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

/// 当前活动壳窗口与取消原子(0 = 无)。会话层 stale 重置时从任意线程经
/// `request_shell_close` 发一条 ClientMessage 唤醒 X11 事件泵,泵退出后的
/// 旧结果按会话代际丢弃(ADR-16)。
static ACTIVE_SHELL_WINDOW: AtomicU32 = AtomicU32::new(0);
static ACTIVE_SHELL_CANCEL_ATOM: AtomicU32 = AtomicU32::new(0);

/// 请求关闭当前选区壳(线程安全,可从任意线程调用):向壳窗口发送取消
/// ClientMessage;无活动壳时为 no-op。壳结果由会话层代际校验丢弃(ADR-16)。
pub fn shell_is_active() -> bool {
    ACTIVE_SHELL_WINDOW.load(Ordering::SeqCst) != 0
}

pub fn request_shell_close() {
    let window = ACTIVE_SHELL_WINDOW.load(Ordering::SeqCst);
    let atom = ACTIVE_SHELL_CANCEL_ATOM.load(Ordering::SeqCst);
    if window == 0 || atom == 0 {
        return;
    }
    let Ok((conn, _screen_num)) = x11rb::connect(None) else {
        return;
    };
    // event_mask=0:事件直接投递给创建窗口的客户端(override-redirect 无 WM)。
    let event = ClientMessageEvent::new(32, window, atom, [0u8; 20]);
    let _ = conn.send_event(false, window, EventMask::NO_EVENT, event);
    let _ = conn.flush();
}

/// 壳的最终结果:会话层据此选择完成路径(与 Windows 壳同构)。
/// R21 起携带即时标注图元;X11 文本输入经 XIM(不可用时 Xlib 直输回退)
/// 接入,`pick_region` 强制 `AnnotationOptions::text_input`,工具条含文字工具。
#[derive(Debug, Clone, PartialEq)]
pub enum RegionOutcome {
    /// Enter 确认:rect 走普通完成路径(按 finishAction 预览或静默)。
    Preview(PhysicalRect, Vec<Annotation>),
    /// 操作条/菜单的「标注」动作:rect 强制走预览编辑器,不受静默完成配置影响。
    Annotate(PhysicalRect, Vec<Annotation>),
    /// 操作条/菜单的 copy/save/pin/ocr 动作:rect 走 Quiet 完成路径并执行动作。
    Quiet(PhysicalRect, QuietAction, Vec<Annotation>),
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
    /// R21 文本输入通道(XIM);None = Xlib 不可用,直输走核心键盘映射。
    xim: Option<XimSession>,
}

/// 一次会话预建的光标集。Crosshair 为 pixmap 双色十字;其余为字体字形。
/// 字体或单个字形创建失败时对应项为 NONE(显示服务器默认指针);
/// Crosshair 创建失败回退更小的双色 pixmap,不使用 XC_CROSSHAIR。
/// 任何失败都不阻断选择/确认/取消。
struct CursorSet {
    font: xproto::Font,
    font_open: bool,
    cursors: [xproto::Cursor; CURSOR_SLOT_COUNT],
}

impl CursorSet {
    /// 预建 Crosshair pixmap(臂长按 frame.scale),再打开 "cursor" 字体
    /// 建其余字形;每一步失败都只退化对应槽位。
    fn build(conn: &RustConnection, scale: f64) -> Self {
        let mut set = Self {
            font: NONE,
            font_open: false,
            cursors: [NONE; CURSOR_SLOT_COUNT],
        };
        if let Ok(cursor) = conn.generate_id() {
            if build_pixmap_crosshair(conn, cursor, scale) {
                set.cursors[cursor_slot(CursorHint::Crosshair)] = cursor;
            }
        }
        let Ok(font) = conn.generate_id() else {
            return set;
        };
        if conn.open_font(font, b"cursor").is_err() {
            return set;
        }
        set.font = font;
        set.font_open = true;
        for hint in [
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
            let Some(glyph) = cursor_glyph(hint) else {
                continue;
            };
            let Ok(cursor) = conn.generate_id() else {
                continue;
            };
            if build_font_cursor(conn, font, cursor, glyph) {
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

/// 建 Crosshair pixmap 光标:臂长按 frame.scale,主规格失败则用最小双色
/// 位图,不使用 XC_CROSSHAIR。
fn build_pixmap_crosshair(conn: &RustConnection, cursor: xproto::Cursor, scale: f64) -> bool {
    build_pixmap_crosshair_sized(conn, cursor, CrosshairSprite::for_scale(scale))
        || build_pixmap_crosshair_sized(conn, cursor, CrosshairSprite::fallback())
}

fn build_pixmap_crosshair_sized(
    conn: &RustConnection,
    cursor: xproto::Cursor,
    sprite: CrosshairSprite,
) -> bool {
    let Some(root) = conn.setup().roots.first().map(|screen| screen.root) else {
        return false;
    };
    let Ok(source) = conn.generate_id() else {
        return false;
    };
    let Ok(mask) = conn.generate_id() else {
        return false;
    };
    let Ok(gc) = conn.generate_id() else {
        return false;
    };
    let size = sprite.size as u16;
    let drawable = xproto::Drawable::from(root);
    let created = (|| {
        conn.create_pixmap(1, source, drawable, size, size)
            .ok()?
            .check()
            .ok()?;
        conn.create_pixmap(1, mask, drawable, size, size)
            .ok()?
            .check()
            .ok()?;
        conn.create_gc(gc, source, &CreateGCAux::new().foreground(1).background(0))
            .ok()?
            .check()
            .ok()?;
        let lsb_first = conn.setup().bitmap_format_bit_order == ImageOrder::LSB_FIRST;
        let pad = conn.setup().bitmap_format_scanline_pad;
        let source_bits = pack_bitmap(
            sprite.size,
            |x, y| sprite.pixel(x, y) == CrosshairPixel::Core,
            lsb_first,
            pad,
        );
        let mask_bits = pack_bitmap(
            sprite.size,
            |x, y| sprite.pixel(x, y) != CrosshairPixel::Empty,
            lsb_first,
            pad,
        );
        conn.put_image(
            ImageFormat::XY_BITMAP,
            source,
            gc,
            size,
            size,
            0,
            0,
            0,
            1,
            &source_bits,
        )
        .ok()?
        .check()
        .ok()?;
        conn.put_image(
            ImageFormat::XY_BITMAP,
            mask,
            gc,
            size,
            size,
            0,
            0,
            0,
            1,
            &mask_bits,
        )
        .ok()?
        .check()
        .ok()?;
        let (hot_x, hot_y) = sprite.hotspot();
        // 前景=浅色线芯,背景=深色描边(source 置位处显示前景)。
        conn.create_cursor(
            cursor,
            source,
            mask,
            0xf7f7,
            0xf7f7,
            0xf7f7,
            0x1414,
            0x1414,
            0x1414,
            hot_x as u16,
            hot_y as u16,
        )
        .ok()?
        .check()
        .ok()?;
        Some(())
    })();
    let _ = conn.free_gc(gc);
    let _ = conn.free_pixmap(source);
    let _ = conn.free_pixmap(mask);
    created.is_some()
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

    /// 按 Shift 列取 keysym(文本直输用);第 0 列为 NoSymbol 时回退首个有效列。
    fn keysym_for(&self, keycode: u8, shift: bool) -> Option<u32> {
        if self.per_keycode == 0 {
            return None;
        }
        let index = keycode.checked_sub(self.min_keycode)? as usize;
        let base = index.checked_mul(self.per_keycode)?;
        let list = self.keysyms.get(base..base + self.per_keycode)?;
        let primary = list.get(usize::from(shift)).copied().unwrap_or(NO_SYMBOL);
        if primary != NO_SYMBOL {
            return Some(primary);
        }
        list.iter().copied().find(|sym| *sym != NO_SYMBOL)
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
            // 文本编辑的退格/删除(非编辑态下引擎忽略)。
            XK_BACKSPACE | XK_DELETE => Some(LogicalKey::Delete),
            // R21 修订:工具快捷键(A/R/E/L/M/B/H/P/N/T)进入标注模式并选工具。
            XK_R_LOWER | XK_R_UPPER => Some(LogicalKey::Tool(AnnotationTool::Rect)),
            XK_E_LOWER | XK_E_UPPER => Some(LogicalKey::Tool(AnnotationTool::Ellipse)),
            XK_L_LOWER | XK_L_UPPER => Some(LogicalKey::Tool(AnnotationTool::Line)),
            XK_A_LOWER | XK_A_UPPER => Some(LogicalKey::Tool(AnnotationTool::Arrow)),
            XK_N_LOWER | XK_N_UPPER => Some(LogicalKey::Tool(AnnotationTool::Number)),
            XK_T_LOWER | XK_T_UPPER => Some(LogicalKey::Tool(AnnotationTool::Text)),
            XK_P_LOWER | XK_P_UPPER => Some(LogicalKey::Tool(AnnotationTool::Pen)),
            XK_H_LOWER | XK_H_UPPER => Some(LogicalKey::Tool(AnnotationTool::Highlighter)),
            XK_M_LOWER | XK_M_UPPER => Some(LogicalKey::Tool(AnnotationTool::Mosaic)),
            XK_B_LOWER | XK_B_UPPER => Some(LogicalKey::Tool(AnnotationTool::Blur)),
            _ => None,
        }
    }

    /// 撤销/重做快捷键:与 Windows 壳一致用 Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y。
    fn shortcut_key(&self, keycode: u8, state: u16) -> Option<LogicalKey> {
        if state & u16::from(KeyButMask::CONTROL) == 0 {
            return None;
        }
        let shift = state & u16::from(KeyButMask::SHIFT) != 0;
        match self.plain_keysym(keycode)? {
            XK_Z_LOWER | XK_Z_UPPER => Some(if shift {
                LogicalKey::Redo
            } else {
                LogicalKey::Undo
            }),
            XK_Y_LOWER | XK_Y_UPPER => Some(LogicalKey::Redo),
            _ => None,
        }
    }
}

// ---- R21 文本输入:Xlib XIM(基础路径) ----

/// Xlib 不透明句柄(仅作为 FFI 指针类型;语义由 Xlib 管理)。
#[repr(C)]
struct XDisplay {
    _private: [u8; 0],
}
#[repr(C)]
struct XimOpaque {
    _private: [u8; 0],
}
#[repr(C)]
struct XicOpaque {
    _private: [u8; 0],
}

/// XKeyEvent 的 C 布局(Bool 即 int;60 字节字段 + 对齐,见 Xlib.h)。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct XKeyEvent {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    window: c_ulong,
    root: c_ulong,
    subwindow: c_ulong,
    time: c_ulong,
    x: c_int,
    y: c_int,
    x_root: c_int,
    y_root: c_int,
    state: c_uint,
    keycode: c_uint,
    same_screen: c_int,
}

/// XEvent 联合体按 64 位平台 24 个 long 定容;Xlib 可能按整只 XEvent
/// 复制/回塞事件,缓冲必须足量。
#[repr(C)]
#[derive(Clone, Copy)]
struct XEventBuf {
    _pad: [c_ulong; 24],
}

impl XEventBuf {
    fn zeroed() -> Self {
        Self { _pad: [0; 24] }
    }

    fn from_key(key: &XKeyEvent) -> Self {
        let mut buf = Self::zeroed();
        unsafe { ptr::write(buf._pad.as_mut_ptr().cast::<XKeyEvent>(), *key) };
        buf
    }

    fn key(&self) -> XKeyEvent {
        unsafe { ptr::read(self._pad.as_ptr().cast::<XKeyEvent>()) }
    }
}

// X11 常量(Xlib.h):事件类型、XIM 样式、查找状态。
const X_KEY_PRESS: c_int = 2;
const X_KEY_RELEASE: c_int = 3;
/// XIMPreeditNothing | XIMStatusNothing:IM 自绘预编辑/状态,不要求客户端窗口。
const XIM_STYLE_NO_CLIENT_DRAW: c_ulong = (1 << 3) | (1 << 18);
const X_BUFFER_OVERFLOW: c_int = -1;
/// 转发按键后等待 IM 回放/提交的上限(ms);IM 无响应时只多这一次轮询。
const XIM_KEY_DRAIN_MS: c_int = 15;
/// 单次 drain 收集上限(防御 IM 持续灌数据导致泵饥饿)。
const XIM_DRAIN_MAX_EVENTS: usize = 128;

#[link(name = "X11")]
extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut XDisplay;
    fn XCloseDisplay(dpy: *mut XDisplay) -> c_int;
    fn XSetLocaleModifiers(mods: *const c_char) -> *mut c_char;
    fn XOpenIM(
        dpy: *mut XDisplay,
        rdb: *mut c_void,
        res_name: *const c_char,
        res_class: *const c_char,
    ) -> *mut XimOpaque;
    fn XCloseIM(im: *mut XimOpaque) -> c_int;
    fn XCreateIC(im: *mut XimOpaque, ...) -> *mut XicOpaque;
    fn XDestroyIC(ic: *mut XicOpaque);
    fn XSetICFocus(ic: *mut XicOpaque);
    fn XUnsetICFocus(ic: *mut XicOpaque);
    fn XFilterEvent(ev: *mut XEventBuf, window: c_ulong) -> c_int;
    fn Xutf8LookupString(
        ic: *mut XicOpaque,
        ev: *mut XKeyEvent,
        buffer: *mut c_char,
        bytes: c_int,
        keysym: *mut c_ulong,
        status: *mut c_int,
    ) -> c_int;
    fn XLookupString(
        ev: *mut XKeyEvent,
        buffer: *mut c_char,
        bytes: c_int,
        keysym: *mut c_ulong,
        status: *mut c_void,
    ) -> c_int;
    fn XInternalConnectionNumbers(
        dpy: *mut XDisplay,
        fd_list: *mut *mut c_int,
        count: *mut c_int,
    ) -> c_int;
    fn XProcessInternalConnection(dpy: *mut XDisplay, fd: c_int);
    fn XFree(data: *mut c_void) -> c_int;
    fn XPending(dpy: *mut XDisplay) -> c_int;
    fn XNextEvent(dpy: *mut XDisplay, ev: *mut XEventBuf) -> c_int;
}

/// IM 回放/提交的输出;文本进引擎,`Key` 回放经 `apply_xim_output` 仅分派
/// 按下事件(释放回放只用于 IM 按键追踪,见 `replayed_key_is_press`)。
enum XimOutput {
    Text(String),
    Key(XKeyEvent),
}

/// 回放按键是否有引擎语义:仅按下事件。IM 的 `IMFilterEventMask` 含
/// KeyReleaseMask,未被 IM 消费的释放事件也会经回放送回客户端;释放只在
/// IM 内用于按键状态追踪,按按下派发会二次触发文本与快捷键(退格/删除/
/// 撤销双执行,Enter/Esc 会意外结束编辑或选区会话)。
fn replayed_key_is_press(key: &XKeyEvent) -> bool {
    key.kind == X_KEY_PRESS
}

/// 打开的 XIM 文本输入通道:Xlib 独立连接 + IM/IC + 传输 fd。
/// `XOpenDisplay` 失败返回 None,壳回退核心键盘映射直输。
struct XimSession {
    dpy: *mut XDisplay,
    im: *mut XimOpaque,
    ic: *mut XicOpaque,
    /// IM 传输的内部连接 fd;就绪后交 `XProcessInternalConnection`。
    fds: Vec<c_int>,
}

impl Drop for XimSession {
    fn drop(&mut self) {
        unsafe {
            if !self.ic.is_null() {
                XUnsetICFocus(self.ic);
                XDestroyIC(self.ic);
            }
            if !self.im.is_null() {
                XCloseIM(self.im);
            }
            if !self.dpy.is_null() {
                XCloseDisplay(self.dpy);
            }
        }
    }
}

impl XimSession {
    /// 在独立 Xlib 连接上打开 XIM 并绑定选区窗;任一步失败只退回直输。
    fn open(window: xproto::Window) -> Option<Self> {
        unsafe {
            let dpy = XOpenDisplay(ptr::null());
            if dpy.is_null() {
                return None;
            }
            // 使用当前 locale 的 IM 修饰键;locale 由 GTK 初始化设置。
            XSetLocaleModifiers(c"".as_ptr());
            let mut session = Self {
                dpy,
                im: ptr::null_mut(),
                ic: ptr::null_mut(),
                fds: Vec::new(),
            };
            let im = XOpenIM(dpy, ptr::null_mut(), ptr::null(), ptr::null());
            if !im.is_null() {
                session.im = im;
                session.create_ic(window);
            }
            session.refresh_fds();
            Some(session)
        }
    }

    /// XCreateIC(style=Nothing|Nothing + client/focus window);失败保持 ic=NULL。
    unsafe fn create_ic(&mut self, window: xproto::Window) {
        const XN_INPUT_STYLE: &[u8] = b"inputStyle\0";
        const XN_CLIENT_WINDOW: &[u8] = b"clientWindow\0";
        const XN_FOCUS_WINDOW: &[u8] = b"focusWindow\0";
        let ic = XCreateIC(
            self.im,
            XN_INPUT_STYLE.as_ptr().cast::<c_char>(),
            XIM_STYLE_NO_CLIENT_DRAW,
            XN_CLIENT_WINDOW.as_ptr().cast::<c_char>(),
            window as c_ulong,
            XN_FOCUS_WINDOW.as_ptr().cast::<c_char>(),
            window as c_ulong,
            ptr::null::<c_char>(),
        );
        if !ic.is_null() {
            self.ic = ic;
        }
    }

    /// 缓存 IM 传输 fd(Xlib 内部连接;数据由 XIM 协议层直读 socket)。
    unsafe fn refresh_fds(&mut self) {
        let mut list: *mut c_int = ptr::null_mut();
        let mut count: c_int = 0;
        if XInternalConnectionNumbers(self.dpy, &mut list, &mut count) != 0
            && !list.is_null()
            && count > 0
        {
            self.fds = std::slice::from_raw_parts(list, count as usize).to_vec();
        }
        if !list.is_null() {
            XFree(list.cast::<c_void>());
        }
    }

    /// 告知 IM 选区窗持有输入焦点。
    fn focus(&self) {
        if !self.ic.is_null() {
            unsafe { XSetICFocus(self.ic) };
        }
    }

    /// 把按键转发给 IM(XFilterEvent);返回 true 表示事件已被 IM 消费。
    /// 未被消费时事件可能被本地 compose 改写(keycode=0),调用方按改写后处理。
    fn forward_key(&mut self, key: &mut XKeyEvent, window: xproto::Window) -> bool {
        if self.ic.is_null() {
            return false;
        }
        key.display = self.dpy;
        let mut buf = XEventBuf::from_key(key);
        let filtered = unsafe { XFilterEvent(&mut buf, window as c_ulong) } != 0;
        if !filtered {
            *key = buf.key();
        }
        filtered
    }

    /// 轮询 IM 传输与 Xlib 队列:回放按键/提交文本以输出形式返回。
    /// `wait_ms` 仅作用第一轮(转发后等回放);其后只做非阻塞轮询。
    fn drain(&mut self, wait_ms: c_int) -> Vec<XimOutput> {
        let mut out = Vec::new();
        let mut first = true;
        loop {
            let timeout = if first { wait_ms } else { 0 };
            first = false;
            if !self.service_internal(timeout) {
                break;
            }
            self.collect(&mut out);
            if out.len() >= XIM_DRAIN_MAX_EVENTS {
                break;
            }
        }
        self.collect(&mut out);
        out
    }

    /// select/poll 语义:IM fd 就绪时交 Xlib 生成内部事件(回塞队列)。
    fn service_internal(&mut self, timeout_ms: c_int) -> bool {
        if self.fds.is_empty() {
            return false;
        }
        let mut pfds: Vec<libc::pollfd> = self
            .fds
            .iter()
            .map(|fd| libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let ready =
            unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
        if ready <= 0 {
            return false;
        }
        for pfd in &pfds {
            if pfd.revents & libc::POLLIN != 0 {
                unsafe { XProcessInternalConnection(self.dpy, pfd.fd) };
            }
        }
        true
    }

    /// 清空 Xlib 队列:协议事件先过 `XFilterEvent`,回放/提交转输出。
    fn collect(&mut self, out: &mut Vec<XimOutput>) {
        loop {
            if out.len() >= XIM_DRAIN_MAX_EVENTS {
                return;
            }
            if unsafe { XPending(self.dpy) } <= 0 {
                return;
            }
            let mut buf = XEventBuf::zeroed();
            unsafe { XNextEvent(self.dpy, &mut buf) };
            let key = buf.key();
            let filtered = unsafe { XFilterEvent(&mut buf, 0) } != 0;
            if key.kind == X_KEY_PRESS && key.keycode == 0 {
                // 提交串:keycode=0 的合成事件,查 lookup 取文本。
                if let Some(text) = self.lookup_utf8(&buf.key()) {
                    out.push(XimOutput::Text(text));
                }
            } else if !filtered && (key.kind == X_KEY_PRESS || key.kind == X_KEY_RELEASE) {
                // 释放回放同样收集,但分派时按 kind 忽略(见 replayed_key_is_press)。
                out.push(XimOutput::Key(key));
            }
        }
    }

    /// 文本输入:XIM 打开时走 Xutf8LookupString(含组合/提交),否则回退
    /// XLookupString 的 keysym 直映射。IME 不可用不影响其余交互。
    fn text_for(&self, key: &XKeyEvent) -> Option<String> {
        if !self.ic.is_null() {
            if let Some(text) = self.lookup_utf8(key) {
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
        self.lookup_direct(key)
    }

    /// Xutf8LookupString:提交串与直输文本统一 UTF-8;缓冲区不足时扩容量试。
    fn lookup_utf8(&self, key: &XKeyEvent) -> Option<String> {
        if self.ic.is_null() {
            return None;
        }
        let mut event = *key;
        event.display = self.dpy;
        let mut size = 64usize;
        loop {
            let mut bytes = vec![0u8; size];
            let mut keysym: c_ulong = 0;
            let mut status: c_int = 0;
            let n = unsafe {
                Xutf8LookupString(
                    self.ic,
                    &mut event,
                    bytes.as_mut_ptr().cast::<c_char>(),
                    bytes.len() as c_int,
                    &mut keysym,
                    &mut status,
                )
            };
            if status == X_BUFFER_OVERFLOW && size < 4096 {
                size *= 4;
                continue;
            }
            if n <= 0 {
                return None;
            }
            let len = (n as usize).min(bytes.len());
            return Some(String::from_utf8_lossy(&bytes[..len]).into_owned());
        }
    }

    /// 无 IC 时的直输:XLookupString 取 keysym,再用 keysym→字符映射,
    /// 避免依赖 locale 编码。
    fn lookup_direct(&self, key: &XKeyEvent) -> Option<String> {
        let mut event = *key;
        event.display = self.dpy;
        let mut buf = [0 as c_char; 8];
        let mut keysym: c_ulong = 0;
        let n = unsafe {
            XLookupString(
                &mut event,
                buf.as_mut_ptr(),
                buf.len() as c_int,
                &mut keysym,
                ptr::null_mut(),
            )
        };
        if let Some(ch) = keysym_to_char(keysym as u32) {
            return Some(ch.to_string());
        }
        if n > 0 {
            let bytes = unsafe {
                std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), (n as usize).min(buf.len()))
            };
            if let Ok(text) = std::str::from_utf8(bytes) {
                let text = text.trim_end_matches('\0');
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
        }
        None
    }
}

/// keysym → 可提交字符:ASCII/拉丁文与 Unicode 段(0x01000000+码点);
/// 功能键/方向键等 XK_* 段不产生文本。
fn keysym_to_char(keysym: u32) -> Option<char> {
    let code = if keysym >= 0x0100_0000 {
        keysym & 0x00ff_ffff
    } else {
        keysym
    };
    let printable = keysym >= 0x0100_0000 || matches!(code, 0x20..=0x7e | 0xa0..=0xff);
    if !printable {
        return None;
    }
    char::from_u32(code).filter(|ch| !ch.is_control())
}

/// 按当前 Shift 状态从核心键盘表取 keysym 并映射为直输字符。
fn direct_text_from_map(keyboard: &KeyboardMap, key: &XKeyEvent) -> Option<String> {
    let shift = key.state & u32::from(KeyButMask::SHIFT) != 0;
    keyboard
        .keysym_for(key.keycode as u8, shift)
        .and_then(keysym_to_char)
        .map(|ch| ch.to_string())
}

/// 驱动一次区域选区:创建 override-redirect 全屏窗并泵事件直到引擎终态。
pub fn pick_region(
    frame: &Frame,
    monitor: &MonitorGeom,
    flags: FeatureFlags,
    annotation_options: AnnotationOptions,
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
        .ok_or_else(|| CaptureError::api("error.capture.x11_pixel_format"))?;
    if format.bits_per_pixel != 32 || format.scanline_pad != 32 {
        return Err(CaptureError::api(
            "error.capture.x11_pixel_format_unsupported",
        ));
    }
    let layout = root_visual_layout(conn.setup(), screen)
        .ok_or_else(|| CaptureError::api("error.capture.x11_visual_format"))?;
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
                    | EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE,
            ),
    )
    .map_err(|_| window_failed())?
    .check()
    .map_err(|_| window_failed())?;
    conn.create_gc(gc, window, &CreateGCAux::new())
        .map_err(|_| window_failed())?
        .check()
        .map_err(|_| window_failed())?;

    // 光标集:Crosshair 为 pixmap 双色十字,其余为字体字形;
    // 字体/字形缺失时对应提示退回服务器默认指针(不阻断交互)。
    let cursors = CursorSet::build(&conn, frame.scale);
    let grab_cursor = cursors.cursor(CursorHint::Crosshair);

    // R21 文本输入:独立 Xlib 连接上开 XIM(XOpenIM/XCreateIC);失败只失去
    // IME,直输仍回退核心键盘映射。文字工具在 X11 原生壳始终可用。
    let xim = XimSession::open(window);
    let mut annotation_options = annotation_options;
    annotation_options.text_input = true;

    let mut state = ShellState {
        hooks,
        canvas: Canvas {
            // 注入冻结帧 DPI 缩放:chrome(放大镜面板)光标命中需要。
            engine: SelectionEngine::new(width as u32, height as u32, flags)
                .with_scale(frame.scale)
                .with_annotation_options(annotation_options),
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
        xim,
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
    // 映射成功后注册活动壳:取消原子经服务器全局 intern,关闭请求从任意
    // 连接发送;窗口在收尾处销毁后才清除注册。
    let cancel_atom = conn
        .intern_atom(false, b"CROPMARK_SHELL_CANCEL")
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.atom)
        .unwrap_or(0);
    ACTIVE_SHELL_WINDOW.store(window, Ordering::SeqCst);
    ACTIVE_SHELL_CANCEL_ATOM.store(cancel_atom, Ordering::SeqCst);
    grab_inputs(&conn, window, grab_cursor);
    // XIM 焦点跟随键盘抓取:无 WM 的 override-redirect 窗没有 FocusIn,
    // 抓取成功即视为 IC 获得输入焦点。
    if let Some(xim) = state.xim.as_ref() {
        xim.focus();
    }
    let _ = conn.flush();

    pump_until_done(&mut state, &surface, &keyboard);

    // 收尾:解除抓取、销毁资源;呈现段的 server 侧 attach 在此释放。
    let _ = conn.ungrab_keyboard(CURRENT_TIME);
    let _ = conn.ungrab_pointer(CURRENT_TIME);
    let _ = conn.destroy_window(window);
    ACTIVE_SHELL_WINDOW.store(0, Ordering::SeqCst);
    ACTIVE_SHELL_CANCEL_ATOM.store(0, Ordering::SeqCst);
    let _ = conn.free_gc(gc);
    state.cursors.free(&conn);
    if let PresentBuffer::Shm(segment) = &state.canvas.present {
        let _ = conn.shm_detach(segment.seg);
    }
    let _ = conn.flush();
    Ok(state.outcome.take().unwrap_or(RegionOutcome::Cancelled))
}

fn shell_unavailable() -> CaptureError {
    CaptureError::unavailable("error.capture.x11_connect")
}

fn id_failed() -> CaptureError {
    CaptureError::api("error.capture.x11_alloc")
}

fn window_failed() -> CaptureError {
    CaptureError::api("error.capture.window_open")
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
        .grab_keyboard(
            false,
            window,
            CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.status == GrabStatus::SUCCESS);
    if std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some() {
        eprintln!("Cropmark X11 grabs: pointer={pointer:?}, keyboard={keyboard:?}");
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
            XEvent::KeyPress(e) => handle_key_press(state, surface, keyboard, &e),
            XEvent::KeyRelease(e) => handle_key_release(state, surface, keyboard, &e),
            // 无效验请求的错误回执会以事件形式送达;选区窗的绘制/抓取
            // 请求均为尽力而为,忽略错误继续泵。
            XEvent::Error(_) => false,
            // stale 重置的关闭请求:取消本壳,结果由会话层代际校验丢弃。
            XEvent::ClientMessage(e) => {
                if e.type_ == ACTIVE_SHELL_CANCEL_ATOM.load(Ordering::SeqCst) {
                    state.outcome = Some(RegionOutcome::Cancelled);
                    true
                } else {
                    false
                }
            }
            // Expose:直接呈现缓存位图,不重合成(ADR-007)。
            XEvent::Expose(_) => {
                put_frame(surface, &state.canvas);
                false
            }
            _ => false,
        };
        // IM 的异步回放/提交(与当前事件无直接对应)在阻塞等待前收掉。
        if done || flush_xim(state, surface, keyboard) {
            return;
        }
    }
}

/// 构造 IM 用的按键事件(display 由 XimSession 填入;坐标为物理像素)。
#[allow(clippy::too_many_arguments)]
fn key_event_for(
    surface: &Surface<'_>,
    kind: c_int,
    detail: u8,
    time: u32,
    event_x: i16,
    event_y: i16,
    root_x: i16,
    root_y: i16,
    state_mask: u16,
    root: xproto::Window,
) -> XKeyEvent {
    XKeyEvent {
        kind,
        serial: 0,
        send_event: 0,
        display: ptr::null_mut(),
        window: surface.window as c_ulong,
        root: root as c_ulong,
        subwindow: 0,
        time: time as c_ulong,
        x: event_x as c_int,
        y: event_y as c_int,
        x_root: root_x as c_int,
        y_root: root_y as c_int,
        state: state_mask as c_uint,
        keycode: detail as c_uint,
        same_screen: 1,
    }
}

/// 按键:先交 IM 转发(被消费的按键由回放/提交输出继续处理),
/// 未被消费时按壳内映射处理(Enter/Esc/方向/Ctrl+Z 与文本直输)。
fn handle_key_press(
    state: &mut ShellState,
    surface: &Surface<'_>,
    keyboard: &KeyboardMap,
    event: &xproto::KeyPressEvent,
) -> bool {
    let mut key = key_event_for(
        surface,
        X_KEY_PRESS,
        event.detail,
        event.time,
        event.event_x,
        event.event_y,
        event.root_x,
        event.root_y,
        u16::from(event.state),
        event.root,
    );
    let (forwarded, outputs) = match state.xim.as_mut() {
        Some(xim) => {
            let forwarded = xim.forward_key(&mut key, surface.window);
            (forwarded, xim.drain(XIM_KEY_DRAIN_MS))
        }
        None => (false, Vec::new()),
    };
    let mut done = false;
    for output in outputs {
        done |= apply_xim_output(state, surface, keyboard, output);
    }
    if forwarded {
        return done;
    }
    done || handle_key_event(state, surface, keyboard, &key)
}

/// 释放事件只用于 IM 追踪按键状态;其回放(含 IM 回显)不产生引擎动作。
fn handle_key_release(
    state: &mut ShellState,
    surface: &Surface<'_>,
    keyboard: &KeyboardMap,
    event: &xproto::KeyReleaseEvent,
) -> bool {
    let outputs = match state.xim.as_mut() {
        Some(xim) => {
            let mut key = key_event_for(
                surface,
                X_KEY_RELEASE,
                event.detail,
                event.time,
                event.event_x,
                event.event_y,
                event.root_x,
                event.root_y,
                u16::from(event.state),
                event.root,
            );
            let _ = xim.forward_key(&mut key, surface.window);
            xim.drain(0)
        }
        None => Vec::new(),
    };
    let mut done = false;
    for output in outputs {
        done |= apply_xim_output(state, surface, keyboard, output);
    }
    done
}

/// 应用 IM 输出:提交串进引擎(仅文本编辑态);回放按键只有按下事件进入
/// 壳内映射,释放事件(IM 按键追踪回放)直接忽略。
fn apply_xim_output(
    state: &mut ShellState,
    surface: &Surface<'_>,
    keyboard: &KeyboardMap,
    output: XimOutput,
) -> bool {
    match output {
        XimOutput::Text(text) => feed_text(state, surface, text),
        XimOutput::Key(key) => {
            if replayed_key_is_press(&key) {
                handle_key_event(state, surface, keyboard, &key)
            } else {
                false
            }
        }
    }
}

/// 非阻塞处理 IM 传输数据(与按键无直接对应的异步回放/提交)。
fn flush_xim(state: &mut ShellState, surface: &Surface<'_>, keyboard: &KeyboardMap) -> bool {
    let outputs = match state.xim.as_mut() {
        Some(xim) => xim.drain(0),
        None => Vec::new(),
    };
    let mut done = false;
    for output in outputs {
        done |= apply_xim_output(state, surface, keyboard, output);
    }
    done
}

/// 单次按键的壳内语义:逻辑键(含 Ctrl+Z/Y)与文本直输;控制键不插入文本。
fn handle_key_event(
    state: &mut ShellState,
    surface: &Surface<'_>,
    keyboard: &KeyboardMap,
    key: &XKeyEvent,
) -> bool {
    let shift = key.state & u32::from(KeyButMask::SHIFT) != 0;
    let ctrl = key.state & u32::from(KeyButMask::CONTROL) != 0;
    let keycode = key.keycode as u8;
    let typing = state.canvas.engine.text_edit().is_some();
    let logical = keyboard
        .shortcut_key(keycode, key.state as u16)
        .or_else(|| {
            let mapped = keyboard.logical_key(keycode);
            // 文本编辑中字母键属于输入内容;Ctrl 组合也不作为工具快捷键
            // (与 Windows 壳一致,避免 Ctrl+R 等误切工具)。
            if matches!(mapped, Some(LogicalKey::Tool(_))) && (typing || ctrl) {
                None
            } else {
                mapped
            }
        });
    if let Some(logical) = logical {
        if feed_event(
            state,
            surface,
            InputEvent::Key {
                key: logical,
                shift,
            },
        ) {
            return true;
        }
    }
    // 控制键不进入文本输入(与 Windows 壳丢弃 WM_CHAR 控制字符一致);
    // 修饰组合(Ctrl/Alt/Super)同样不产生文本;取色键 C 仍允许插入字符。
    if key.state
        & (u32::from(KeyButMask::CONTROL)
            | u32::from(KeyButMask::MOD1)
            | u32::from(KeyButMask::MOD4))
        != 0
    {
        return false;
    }
    if let Some(logical) = logical {
        if logical != LogicalKey::CopyColor {
            return false;
        }
    }
    // 无编辑会话时文本没有落点:直接跳过,避免无意义重合成。
    if state.canvas.engine.text_edit().is_none() {
        return false;
    }
    let text = match state.xim.as_ref() {
        Some(xim) => xim.text_for(key),
        None => direct_text_from_map(keyboard, key),
    };
    match text {
        Some(text) => feed_text(state, surface, text),
        None => false,
    }
}

/// 文本进入引擎(仅文本编辑态;空串丢弃)。
fn feed_text(state: &mut ShellState, surface: &Surface<'_>, text: String) -> bool {
    if text.is_empty() || state.canvas.engine.text_edit().is_none() {
        return false;
    }
    feed_event(state, surface, InputEvent::Text(text))
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
            state.outcome = Some(RegionOutcome::Preview(
                rect,
                state.canvas.engine.annotations().to_vec(),
            ));
            true
        }
        EngineOutcome::Cancelled => {
            state.outcome = Some(RegionOutcome::Cancelled);
            true
        }
        EngineOutcome::Action(action) => match action {
            SelectionAction::Annotate => {
                if let Some(outcome) = annotate_outcome(&state.canvas.engine) {
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
            // 标注工具条动作由引擎内部消费,不会到达这里;防御性忽略。
            SelectionAction::Tool(_)
            | SelectionAction::Undo
            | SelectionAction::Redo
            | SelectionAction::Delete
            | SelectionAction::More => false,
            quiet => {
                if let (Some(rect), Some(action)) =
                    (state.canvas.engine.selection(), quiet_action_for(quiet))
                {
                    state.outcome = Some(RegionOutcome::Quiet(
                        rect,
                        action,
                        state.canvas.engine.annotations().to_vec(),
                    ));
                    return true;
                }
                false
            }
        },
    }
}

/// 「标注」动作到壳结果的映射:引擎尚无选区时返回 None,会话继续等待。
fn annotate_outcome(engine: &SelectionEngine) -> Option<RegionOutcome> {
    engine
        .selection()
        .map(|rect| RegionOutcome::Annotate(rect, engine.annotations().to_vec()))
}

/// 操作条/菜单动作到静默完成动作的映射;标注/取消/复制色值与标注工具条
/// 动作不在此列。
fn quiet_action_for(action: SelectionAction) -> Option<QuietAction> {
    match action {
        SelectionAction::Copy => Some(QuietAction::Copy),
        SelectionAction::Save => Some(QuietAction::Save),
        SelectionAction::Pin => Some(QuietAction::Pin),
        SelectionAction::Ocr => Some(QuietAction::Ocr),
        SelectionAction::Annotate
        | SelectionAction::Cancel
        | SelectionAction::CopyColor
        | SelectionAction::Tool(_)
        | SelectionAction::Undo
        | SelectionAction::Redo
        | SelectionAction::Delete
        | SelectionAction::More => None,
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
        let overlay = canvas.engine.annotation_overlay();
        canvas
            .composer
            .compose_into_with_overlay(&scene, &overlay, &mut canvas.scratch);
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

    const XK_D_LOWER: u32 = 0x64;
    const XK_Q_LOWER: u32 = 0x71;

    /// min_keycode=8、每键 2 列的小键盘表:8=Return,9=Esc,10/11/12/13=方向,
    /// 14=c,17-21=Keypad Enter/方向,22='Q';shift 列(奇数索引)填 NoSymbol
    /// 验证回退,15 的第 0 列无效、16/22 不在引擎语义内。
    /// 'A'/'B' 已是工具快捷键,未映射字母改用 'D'/'Q'。
    fn test_keyboard_map() -> KeyboardMap {
        KeyboardMap {
            min_keycode: 8,
            per_keycode: 2,
            keysyms: vec![
                XK_RETURN,
                NO_SYMBOL,
                XK_ESCAPE,
                NO_SYMBOL,
                XK_LEFT,
                NO_SYMBOL,
                XK_UP,
                NO_SYMBOL,
                XK_RIGHT,
                NO_SYMBOL,
                XK_DOWN,
                NO_SYMBOL,
                XK_C_LOWER,
                NO_SYMBOL,
                NO_SYMBOL,
                XK_C_UPPER,
                NO_SYMBOL,
                XK_D_LOWER,
                XK_KP_ENTER,
                NO_SYMBOL,
                XK_KP_LEFT,
                NO_SYMBOL,
                XK_KP_UP,
                NO_SYMBOL,
                XK_KP_RIGHT,
                NO_SYMBOL,
                XK_KP_DOWN,
                NO_SYMBOL,
                XK_Q_LOWER,
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
        // 16:'D' 不在引擎语义内;17-21:数字小键盘的 Enter/方向键与主键盘同义。
        assert_eq!(map.logical_key(16), None);
        assert_eq!(map.logical_key(17), Some(LogicalKey::Enter));
        assert_eq!(map.logical_key(18), Some(LogicalKey::ArrowLeft));
        assert_eq!(map.logical_key(19), Some(LogicalKey::ArrowUp));
        assert_eq!(map.logical_key(20), Some(LogicalKey::ArrowRight));
        assert_eq!(map.logical_key(21), Some(LogicalKey::ArrowDown));
        // 22:'Q' 不在引擎语义内;23:超出表尾。
        assert_eq!(map.logical_key(22), None);
        assert_eq!(map.logical_key(23), None);
        // 超出键盘表范围。
        assert_eq!(map.logical_key(7), None);
        assert_eq!(map.logical_key(100), None);
    }

    #[test]
    fn tool_keysyms_map_to_annotation_tools() {
        let map = KeyboardMap {
            min_keycode: 8,
            per_keycode: 1,
            keysyms: vec![
                XK_R_LOWER, XK_E_LOWER, XK_L_LOWER, XK_A_LOWER, XK_N_LOWER, XK_T_LOWER, XK_P_LOWER,
                XK_H_LOWER, XK_M_LOWER, XK_B_LOWER, XK_A_UPPER, XK_B_UPPER, XK_D_LOWER,
            ],
        };
        assert_eq!(
            map.logical_key(8),
            Some(LogicalKey::Tool(AnnotationTool::Rect))
        );
        assert_eq!(
            map.logical_key(9),
            Some(LogicalKey::Tool(AnnotationTool::Ellipse))
        );
        assert_eq!(
            map.logical_key(10),
            Some(LogicalKey::Tool(AnnotationTool::Line))
        );
        assert_eq!(
            map.logical_key(11),
            Some(LogicalKey::Tool(AnnotationTool::Arrow))
        );
        assert_eq!(
            map.logical_key(12),
            Some(LogicalKey::Tool(AnnotationTool::Number))
        );
        assert_eq!(
            map.logical_key(13),
            Some(LogicalKey::Tool(AnnotationTool::Text))
        );
        assert_eq!(
            map.logical_key(14),
            Some(LogicalKey::Tool(AnnotationTool::Pen))
        );
        assert_eq!(
            map.logical_key(15),
            Some(LogicalKey::Tool(AnnotationTool::Highlighter))
        );
        assert_eq!(
            map.logical_key(16),
            Some(LogicalKey::Tool(AnnotationTool::Mosaic))
        );
        assert_eq!(
            map.logical_key(17),
            Some(LogicalKey::Tool(AnnotationTool::Blur))
        );
        assert_eq!(
            map.logical_key(18),
            Some(LogicalKey::Tool(AnnotationTool::Arrow))
        );
        assert_eq!(
            map.logical_key(19),
            Some(LogicalKey::Tool(AnnotationTool::Blur))
        );
        assert_eq!(map.logical_key(20), None);
    }

    #[test]
    fn backspace_and_delete_map_to_engine_delete() {
        let map = KeyboardMap {
            min_keycode: 8,
            per_keycode: 2,
            keysyms: vec![
                XK_BACKSPACE,
                NO_SYMBOL,
                XK_DELETE,
                NO_SYMBOL,
                XK_RETURN,
                NO_SYMBOL,
            ],
        };
        assert_eq!(map.logical_key(8), Some(LogicalKey::Delete));
        assert_eq!(map.logical_key(9), Some(LogicalKey::Delete));
        assert_eq!(map.logical_key(10), Some(LogicalKey::Enter));
    }

    #[test]
    fn shortcut_keys_require_ctrl_and_shift_selects_redo() {
        let map = KeyboardMap {
            min_keycode: 8,
            per_keycode: 2,
            keysyms: vec![XK_Z_LOWER, NO_SYMBOL, XK_Y_LOWER, NO_SYMBOL],
        };
        let ctrl = u16::from(KeyButMask::CONTROL);
        let shift = u16::from(KeyButMask::SHIFT);
        assert_eq!(map.shortcut_key(8, 0), None);
        assert_eq!(map.shortcut_key(8, ctrl), Some(LogicalKey::Undo));
        assert_eq!(map.shortcut_key(8, ctrl | shift), Some(LogicalKey::Redo));
        assert_eq!(map.shortcut_key(9, ctrl), Some(LogicalKey::Redo));
        assert_eq!(map.shortcut_key(9, ctrl | shift), Some(LogicalKey::Redo));
    }

    #[test]
    fn shift_column_keysym_falls_back_when_unshifted_is_no_symbol() {
        let map = KeyboardMap {
            min_keycode: 8,
            per_keycode: 2,
            keysyms: vec![0x61, 0x41, NO_SYMBOL, 0x42, NO_SYMBOL, NO_SYMBOL],
        };
        assert_eq!(map.keysym_for(8, false), Some(0x61));
        assert_eq!(map.keysym_for(8, true), Some(0x41));
        // 第 0 列 NoSymbol → 回退第 1 列;两列都空 → None。
        assert_eq!(map.keysym_for(9, false), Some(0x42));
        assert_eq!(map.keysym_for(10, false), None);
    }

    #[test]
    fn keysym_to_char_accepts_printable_and_unicode_only() {
        assert_eq!(keysym_to_char(0x61), Some('a'));
        assert_eq!(keysym_to_char(0x7e), Some('~'));
        assert_eq!(keysym_to_char(0xe9), Some('é'));
        // 方向键/回车/功能键等 XK_* 段不产生文本。
        assert_eq!(keysym_to_char(XK_RETURN), None);
        assert_eq!(keysym_to_char(XK_LEFT), None);
        assert_eq!(keysym_to_char(0xffbe), None);
        assert_eq!(keysym_to_char(XK_BACKSPACE), None);
        // 0x01000000|码点 段(Unicode keysym)。
        assert_eq!(keysym_to_char(0x0100_4e2d), Some('中'));
        // 空格与拉丁文补段。
        assert_eq!(keysym_to_char(0x20), Some(' '));
    }

    /// 构造壳内按键事件的测试样本(display 由 XIM 转发时填充)。
    fn test_key_event(kind: c_int, keycode: u32) -> XKeyEvent {
        XKeyEvent {
            kind,
            serial: 0,
            send_event: 0,
            display: ptr::null_mut(),
            window: 0x1234,
            root: 0,
            subwindow: 0,
            time: 0,
            x: 0,
            y: 0,
            x_root: 0,
            y_root: 0,
            state: 0,
            keycode,
            same_screen: 1,
        }
    }

    /// 回归:IM 也会回放未消费的 KeyRelease(ibus 的 IMFilterEventMask 含
    /// KeyReleaseMask);释放回放没有引擎语义,若进入 handle_key_event 会
    /// 二次触发文本与快捷键(Enter/Esc 意外结束编辑或选区会话)。
    #[test]
    fn xim_replayed_release_keys_have_no_engine_semantics() {
        // Enter/Esc/退格/字母等代表性键:按下可派发,释放回放全部拦下。
        for keycode in [8u32, 9, 22, 38] {
            assert!(replayed_key_is_press(&test_key_event(X_KEY_PRESS, keycode)));
            assert!(!replayed_key_is_press(&test_key_event(
                X_KEY_RELEASE,
                keycode
            )));
        }
    }

    #[test]
    fn x_event_buf_roundtrips_key_layout() {
        let key = XKeyEvent {
            kind: X_KEY_PRESS,
            serial: 7,
            send_event: 0,
            display: ptr::null_mut(),
            window: 0x1234,
            root: 0x99,
            subwindow: 0,
            time: 42,
            x: 3,
            y: 4,
            x_root: 5,
            y_root: 6,
            state: u32::from(KeyButMask::SHIFT),
            keycode: 38,
            same_screen: 1,
        };
        let buf = XEventBuf::from_key(&key);
        let read = buf.key();
        assert_eq!(read.window, key.window);
        assert_eq!(read.keycode, key.keycode);
        assert_eq!(read.state, key.state);
        assert_eq!(read.kind, X_KEY_PRESS);
        assert!(std::mem::size_of::<XEventBuf>() >= std::mem::size_of::<XKeyEvent>());
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
        assert_eq!(cursor_glyph(CursorHint::Crosshair), None);
        assert_ne!(cursor_glyph(CursorHint::Crosshair), Some(XC_CROSSHAIR));
        assert_eq!(cursor_glyph(CursorHint::Move), Some(XC_FLEUR));
        assert_eq!(
            cursor_glyph(CursorHint::ResizeNS),
            Some(XC_SB_V_DOUBLE_ARROW)
        );
        assert_eq!(
            cursor_glyph(CursorHint::ResizeEW),
            Some(XC_SB_H_DOUBLE_ARROW)
        );
        assert_eq!(
            cursor_glyph(CursorHint::ResizeNWSE),
            Some(XC_TOP_LEFT_CORNER)
        );
        assert_eq!(
            cursor_glyph(CursorHint::ResizeNESW),
            Some(XC_TOP_RIGHT_CORNER)
        );
        // 可点击 chrome 与放大镜面板都用默认箭头。
        assert_eq!(cursor_glyph(CursorHint::Pointer), Some(XC_LEFT_PTR));
        assert_eq!(cursor_glyph(CursorHint::Arrow), Some(XC_LEFT_PTR));
    }

    fn assert_dual_color_crosshair(sprite: CrosshairSprite) {
        let (cx, cy) = sprite.hotspot();
        assert_eq!(sprite.size % 2, 1);
        assert_eq!((cx, cy), (sprite.size / 2, sprite.size / 2));
        // 中心热点像素镂空:热点仍对准指针像素,但该像素透明。
        assert_eq!(sprite.pixel(cx, cy), CrosshairPixel::Empty);
        // 芯从距中心 1 像素处开始,恒 1 物理像素宽。
        assert_eq!(sprite.pixel(cx + 1, cy), CrosshairPixel::Core);
        assert_eq!(sprite.pixel(cx, cy + 1), CrosshairPixel::Core);
        assert_eq!(sprite.pixel(cx + 1, cy + 1), CrosshairPixel::Outline);
        assert_eq!(
            sprite.pixel(cx, cy.saturating_sub(sprite.arm + 1)),
            CrosshairPixel::Outline
        );
        assert_eq!(sprite.pixel(0, 0), CrosshairPixel::Empty);
    }

    #[test]
    fn dual_color_crosshair_has_light_core_and_dark_outline() {
        assert_dual_color_crosshair(CrosshairSprite::for_scale(1.0));
        assert_dual_color_crosshair(CrosshairSprite::fallback());
        let sprite = CrosshairSprite::fallback();
        let source = pack_bitmap(
            sprite.size,
            |x, y| sprite.pixel(x, y) == CrosshairPixel::Core,
            true,
            32,
        );
        let mask = pack_bitmap(
            sprite.size,
            |x, y| sprite.pixel(x, y) != CrosshairPixel::Empty,
            true,
            32,
        );
        assert!(source.iter().any(|byte| *byte != 0));
        assert!(mask.iter().any(|byte| *byte != 0));
        assert_ne!(source, mask);
    }

    #[test]
    fn crosshair_sprite_scales_arm_with_frame_scale() {
        let base = CrosshairSprite::for_scale(1.0);
        assert_eq!(base.arm, CrosshairSprite::BASE_ARM);
        // 200% 时臂长翻倍,边长仍为奇数,芯恒 1 物理像素。
        let scaled = CrosshairSprite::for_scale(2.0);
        assert_eq!(scaled.arm, base.arm * 2);
        assert_eq!(scaled.size % 2, 1);
        assert_dual_color_crosshair(scaled);
        let (cx, cy) = scaled.hotspot();
        assert_eq!(scaled.pixel(cx, cy + 2), CrosshairPixel::Core);
        assert_eq!(scaled.pixel(cx + 1, cy + 2), CrosshairPixel::Outline);
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
        let mut engine = SelectionEngine::new(320, 200, FeatureFlags::default());
        assert_eq!(annotate_outcome(&engine), None);
        engine.handle_event(InputEvent::LeftDown { x: 5, y: 6 });
        engine.handle_event(InputEvent::PointerMove { x: 35, y: 46 });
        engine.handle_event(InputEvent::LeftUp { x: 35, y: 46 });
        let rect = engine.selection().unwrap();
        assert_eq!(
            annotate_outcome(&engine),
            Some(RegionOutcome::Annotate(rect, Vec::new()))
        );
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

        // 主行复制(非取消:取消走 Cancelled),验证松开才产出 Action。
        let toolbar = engine.unified_toolbar().expect("toolbar");
        let (expected, rect) = toolbar
            .buttons
            .into_iter()
            .find(|(action, _)| *action == SelectionAction::Copy)
            .expect("copy button");
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
