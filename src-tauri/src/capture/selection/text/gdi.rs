//! Windows 选区 chrome 文字:用 GDI 灰度抗锯齿光栅化。
//!
//! ab_glyph 不执行 TrueType hinting。雅黑在 125%/150% 下的 17–20px 汉字横竖画
//! 会落在半像素上，铺成两行浅灰，工具条菜单、徽标和提示看起来发虚。GDI 走字体
//! 自带的 hinting，笔画对齐像素网格，和系统其它界面一样清楚。
//!
//! 字号语义与 ab_glyph 的 `PxScale` 保持一致：正的 `lfHeight` 是字符单元高度
//! (ascent + descent)，所以版面尺寸和垂直居中不变。只用于 chrome；标注文字仍走
//! ab_glyph，和导出栅格化保持一致。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, GdiFlush,
    GetTextExtentPoint32W, GetTextMetricsW, SelectObject, SetBkMode, SetTextColor, TextOutW,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DIB_RGB_COLORS, FW_NORMAL, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_TT_PRECIS,
    TEXTMETRICW, TRANSPARENT,
};

/// 抗锯齿边缘可能越出字符单元，四周各留出这么多像素。
const PAD: i32 = 2;
/// 单次文字位图的边长上限，防止异常长串申请巨大 DIB。
const MAX_EDGE: i32 = 4096;

/// 一段已光栅化的文字：单通道覆盖度，`(0, 0)` 对应字符单元左上角减去 `PAD`。
pub(super) struct Run {
    pub width: i32,
    pub height: i32,
    pub coverage: Vec<u8>,
}

struct Surface {
    bitmap: HBITMAP,
    bits: *mut u8,
    width: i32,
    height: i32,
}

struct Gdi {
    dc: HDC,
    fonts: HashMap<i32, HFONT>,
    surface: Option<Surface>,
    initial_bitmap: Option<HGDIOBJ>,
    initial_font: Option<HGDIOBJ>,
}

impl Gdi {
    fn new() -> Option<Self> {
        // SAFETY: 创建与屏幕兼容的内存 DC，失败时返回空句柄。
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return None;
        }
        // SAFETY: dc 有效，设置透明背景只影响本 DC。
        unsafe {
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
        }
        Some(Self {
            dc,
            fonts: HashMap::new(),
            surface: None,
            initial_bitmap: None,
            initial_font: None,
        })
    }

    /// 选入指定像素高度的字体；同一高度复用。
    fn select_font(&mut self, height: i32) -> bool {
        let font = match self.fonts.get(&height) {
            Some(font) => *font,
            None => {
                // SAFETY: 参数均为常量或有效像素高度。字体族缺失时 GDI 自动映射。
                let font = unsafe {
                    CreateFontW(
                        height,
                        0,
                        0,
                        0,
                        FW_NORMAL.0 as i32,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET,
                        OUT_TT_PRECIS,
                        CLIP_DEFAULT_PRECIS,
                        ANTIALIASED_QUALITY,
                        0,
                        w!("Microsoft YaHei"),
                    )
                };
                if font.is_invalid() {
                    return false;
                }
                self.fonts.insert(height, font);
                font
            }
        };
        // SAFETY: dc 与 font 都有效。
        let previous = unsafe { SelectObject(self.dc, font.into()) };
        if self.initial_font.is_none() {
            self.initial_font = Some(previous);
        }
        true
    }

    /// 保证内存位图至少有 `width × height`，按需扩容。
    fn ensure_surface(&mut self, width: i32, height: i32) -> bool {
        if let Some(surface) = &self.surface {
            if surface.width >= width && surface.height >= height {
                return true;
            }
        }
        let (old_w, old_h) = self
            .surface
            .as_ref()
            .map(|surface| (surface.width, surface.height))
            .unwrap_or((0, 0));
        let new_w = width.max(old_w).max(64);
        let new_h = height.max(old_h).max(32);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: new_w,
                biHeight: -new_h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: info 描述 32bpp 自顶向下 DIB；bits 由 GDI 填写。
        let Ok(bitmap) =
            (unsafe { CreateDIBSection(Some(self.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) })
        else {
            return false;
        };
        if bits.is_null() {
            // SAFETY: bitmap 是刚创建、尚未选入的对象。
            unsafe {
                let _ = DeleteObject(bitmap.into());
            }
            return false;
        }
        // SAFETY: dc 与 bitmap 有效。旧位图在选出后才释放。
        let previous = unsafe { SelectObject(self.dc, bitmap.into()) };
        if self.initial_bitmap.is_none() {
            self.initial_bitmap = Some(previous);
        }
        if let Some(old) = self.surface.take() {
            // SAFETY: 旧位图已被新位图替换出 DC。
            unsafe {
                let _ = DeleteObject(old.bitmap.into());
            }
        }
        self.surface = Some(Surface {
            bitmap,
            bits: bits.cast::<u8>(),
            width: new_w,
            height: new_h,
        });
        true
    }

    fn extent(&self, wide: &[u16]) -> Option<SIZE> {
        if wide.is_empty() {
            return Some(SIZE { cx: 0, cy: 0 });
        }
        let mut size = SIZE::default();
        // SAFETY: dc 有效，字体已选入。
        unsafe { GetTextExtentPoint32W(self.dc, wide, &mut size) }
            .as_bool()
            .then_some(size)
    }

    fn cell_height(&self) -> Option<i32> {
        let mut metrics = TEXTMETRICW::default();
        // SAFETY: dc 有效，字体已选入。
        unsafe { GetTextMetricsW(self.dc, &mut metrics) }
            .as_bool()
            .then_some(metrics.tmHeight)
    }

    fn render(&mut self, wide: &[u16]) -> Option<Run> {
        let extent = self.extent(wide)?;
        let cell = self.cell_height()?.max(extent.cy);
        let width = extent.cx + PAD * 2;
        let height = cell + PAD * 2;
        if width > MAX_EDGE || height > MAX_EDGE || !self.ensure_surface(width, height) {
            return None;
        }
        let surface = self.surface.as_ref()?;
        let stride = surface.width as usize * 4;
        // SAFETY: bits 指向 surface.width × surface.height 的 BGRA 像素，DC 只在本线程使用。
        let pixels = unsafe {
            std::slice::from_raw_parts_mut(surface.bits, stride * surface.height as usize)
        };
        for row in pixels.chunks_exact_mut(stride).take(height as usize) {
            row[..width as usize * 4].fill(0);
        }
        // SAFETY: 先 flush 确保清零对 GDI 可见，再画白字并等绘制完成后读取。
        unsafe {
            let _ = GdiFlush();
            if !TextOutW(self.dc, PAD, PAD, wide).as_bool() {
                return None;
            }
            let _ = GdiFlush();
        }
        let mut coverage = Vec::with_capacity((width * height) as usize);
        for row in pixels.chunks_exact(stride).take(height as usize) {
            for px in row[..width as usize * 4].chunks_exact(4) {
                coverage.push(px[0].max(px[1]).max(px[2]));
            }
        }
        Some(Run {
            width,
            height,
            coverage,
        })
    }
}

impl Drop for Gdi {
    fn drop(&mut self) {
        // SAFETY: 先把初始对象选回，再释放本模块创建的 GDI 对象。
        unsafe {
            if let Some(bitmap) = self.initial_bitmap {
                SelectObject(self.dc, bitmap);
            }
            if let Some(font) = self.initial_font {
                SelectObject(self.dc, font);
            }
            if let Some(surface) = self.surface.take() {
                let _ = DeleteObject(surface.bitmap.into());
            }
            for (_, font) in self.fonts.drain() {
                let _ = DeleteObject(font.into());
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

thread_local! {
    static GDI: RefCell<Option<Gdi>> = RefCell::new(Gdi::new());
}

fn px_height(size: f32) -> i32 {
    (size.round() as i32).clamp(1, MAX_EDGE / 2)
}

fn encode(text: &str) -> Vec<u16> {
    text.chars()
        .filter(|ch| *ch != '\r' && *ch != '\n')
        .collect::<String>()
        .encode_utf16()
        .collect()
}

fn with_font<T>(size: f32, f: impl FnOnce(&mut Gdi) -> Option<T>) -> Option<T> {
    GDI.with(|cell| {
        let mut gdi = cell.try_borrow_mut().ok()?;
        let gdi = gdi.as_mut()?;
        if !gdi.select_font(px_height(size)) {
            return None;
        }
        f(gdi)
    })
}

/// 单行文字宽度(物理像素)。
pub(super) fn measure_width(text: &str, size: f32) -> Option<f32> {
    let wide = encode(text);
    with_font(size, |gdi| gdi.extent(&wide).map(|extent| extent.cx as f32))
}

/// 逐字符宽度；GDI 默认不做字距调整，逐字相加等于整串宽度。
pub(super) fn char_widths(text: &str, size: f32) -> Option<Vec<(char, f32)>> {
    with_font(size, |gdi| {
        let mut out = Vec::new();
        let mut buf = [0u16; 2];
        for ch in text.chars().filter(|ch| *ch != '\r' && *ch != '\n') {
            let extent = gdi.extent(ch.encode_utf16(&mut buf))?;
            out.push((ch, extent.cx as f32));
        }
        Some(out)
    })
}

/// 光栅化单行文字。返回的覆盖度位图左上角对应字符单元左上角向外 `PAD` 像素。
pub(super) fn render(text: &str, size: f32) -> Option<(Run, i32)> {
    let wide = encode(text);
    with_font(size, |gdi| gdi.render(&wide)).map(|run| (run, PAD))
}
