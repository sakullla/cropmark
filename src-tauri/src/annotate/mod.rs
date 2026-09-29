pub(crate) mod blur;
mod mosaic;
pub(crate) mod raster;
pub(crate) mod stickers;

pub(crate) use raster::parse_hex_color;
pub use raster::rasterize;
pub use raster::rasterize_lenient;

use serde::{Deserialize, Serialize};

/// 默认标注色（玫红），与 v0.1.3 前的 STROKE 像素值一致。
pub const DEFAULT_COLOR: &str = "#e11d48";

/// 荧光笔固定半透明阿尔法（0–1），与前端 paintPolyline 的荧光笔分支一致；
/// 用户选择的颜色只决定色相，透明度由工具语义固定。
pub const HIGHLIGHTER_ALPHA: f32 = 0.38;

/// 高斯模糊 sigma 允许范围（物理像素）；默认 sigma 由前端按区域自适应。
pub const MIN_BLUR_SIGMA: f64 = 0.5;
pub const MAX_BLUR_SIGMA: f64 = 128.0;

/// 遮盖类工具（马赛克/高斯模糊）区域最小边长（物理像素）：
/// 更小的选区不产生可见遮盖效果，直接视为无效标注。与马赛克既有
/// `is_exportable` 的 1.0 相比，模糊需要至少 2px 才能改变像素。
pub const MIN_MASK_EDGE: f64 = 2.0;

/// R5:放大镜默认倍率与允许范围（倍率上限用于避免超大缩放采样）。
pub const DEFAULT_MAGNIFIER_ZOOM: f64 = 3.0;
pub const MIN_MAGNIFIER_ZOOM: f64 = 1.2;
pub const MAX_MAGNIFIER_ZOOM: f64 = 8.0;

/// R5:聚光灯默认暗度（0–1，越大圈外越暗）。
pub const DEFAULT_SPOTLIGHT_DIM: f64 = 0.55;

/// R5:擦除周边取样的采样环带宽（物理像素）。
pub const ERASE_SAMPLE_RING: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Annotation {
    Arrow {
        from: Point,
        to: Point,
        #[serde(default = "default_color")]
        color: String,
        /// 线宽档位（逻辑像素）；None = 沿用既有 3×scale 推导。
        #[serde(default)]
        stroke_width: Option<f64>,
    },
    Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default = "default_color")]
        color: String,
        #[serde(default)]
        stroke_width: Option<f64>,
    },
    Mosaic {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default = "default_block")]
        block: u32,
    },
    Text {
        x: f64,
        y: f64,
        text: String,
        #[serde(default = "default_text_size")]
        size: f64,
        #[serde(default = "default_color")]
        color: String,
    },
    Ellipse {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default = "default_color")]
        color: String,
        #[serde(default)]
        stroke_width: Option<f64>,
    },
    Line {
        from: Point,
        to: Point,
        #[serde(default = "default_color")]
        color: String,
        #[serde(default)]
        stroke_width: Option<f64>,
    },
    Number {
        x: f64,
        y: f64,
        value: u32,
        #[serde(default = "default_text_size")]
        size: f64,
        #[serde(default = "default_color")]
        color: String,
    },
    Highlighter {
        points: Vec<Point>,
        #[serde(default = "default_color")]
        color: String,
        #[serde(default)]
        stroke_width: Option<f64>,
    },
    Pen {
        points: Vec<Point>,
        #[serde(default = "default_color")]
        color: String,
        #[serde(default)]
        stroke_width: Option<f64>,
    },
    Blur {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default = "default_sigma")]
        sigma: f64,
    },
    /// R5:聚光灯。区域保持原样，其余按 `dim` 变暗。
    Spotlight {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default = "default_dim")]
        dim: f64,
    },
    /// R5:放大镜。区域中心按 `zoom` 倍率就地放大并加边框。
    Magnifier {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default = "default_zoom")]
        zoom: f64,
        #[serde(default = "default_color")]
        color: String,
    },
    /// R5:对话气泡。圆角矩形 + 左下指向尾，文本按框宽自动换行。
    Bubble {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default)]
        text: String,
        #[serde(default = "default_text_size")]
        size: f64,
        #[serde(default = "default_color")]
        color: String,
    },
    /// R5:贴纸。`sticker` 为随包素材 id（`annotate::stickers`）。
    Sticker {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        sticker: String,
    },
    /// R5:内容擦除。`color` 为 None 时用区域周边像素均值填充。
    Erase {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default)]
        color: Option<String>,
    },
}

fn default_block() -> u32 {
    12
}

fn default_text_size() -> f64 {
    22.0
}

fn default_sigma() -> f64 {
    12.0
}

fn default_dim() -> f64 {
    DEFAULT_SPOTLIGHT_DIM
}

fn default_zoom() -> f64 {
    DEFAULT_MAGNIFIER_ZOOM
}

fn default_color() -> String {
    DEFAULT_COLOR.into()
}

/// 折线总长度；用于过滤单点/零位移的自由绘制图元。
fn polyline_length(points: &[Point]) -> f64 {
    points
        .windows(2)
        .map(|pair| (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y))
        .sum()
}

impl Annotation {
    pub fn is_exportable(&self) -> bool {
        match self {
            Self::Text { text, .. } => !text.trim().is_empty(),
            Self::Arrow { from, to, .. } | Self::Line { from, to, .. } => {
                (from.x - to.x).abs() >= 1.0 || (from.y - to.y).abs() >= 1.0
            }
            Self::Rect { width, height, .. }
            | Self::Mosaic { width, height, .. }
            | Self::Ellipse { width, height, .. } => width.abs() >= 1.0 && height.abs() >= 1.0,
            Self::Number { value, .. } => *value > 0,
            Self::Highlighter { points, .. } | Self::Pen { points, .. } => {
                points.len() >= 2 && polyline_length(points) >= 1.0
            }
            Self::Blur {
                width,
                height,
                sigma,
                ..
            } => {
                width.abs() >= MIN_MASK_EDGE
                    && height.abs() >= MIN_MASK_EDGE
                    && sigma.is_finite()
                    && *sigma > 0.0
            }
            Self::Spotlight {
                width, height, dim, ..
            } => {
                width.abs() >= MIN_MASK_EDGE
                    && height.abs() >= MIN_MASK_EDGE
                    && dim.is_finite()
                    && *dim > 0.0
            }
            Self::Magnifier {
                width,
                height,
                zoom,
                ..
            } => {
                width.abs() >= MIN_MASK_EDGE
                    && height.abs() >= MIN_MASK_EDGE
                    && zoom.is_finite()
                    && *zoom > 1.0
            }
            Self::Bubble { width, height, .. } | Self::Erase { width, height, .. } => {
                width.abs() >= MIN_MASK_EDGE && height.abs() >= MIN_MASK_EDGE
            }
            Self::Sticker {
                width,
                height,
                sticker,
                ..
            } => {
                width.abs() >= MIN_MASK_EDGE
                    && height.abs() >= MIN_MASK_EDGE
                    && !sticker.trim().is_empty()
            }
        }
    }
}

pub fn exportable(ops: &[Annotation]) -> Vec<Annotation> {
    ops.iter()
        .filter(|op| op.is_exportable())
        .cloned()
        .collect()
}

/// 平移单个图元(dx/dy 加到全部坐标);选区即时标注把整屏坐标映射到
/// 裁剪坐标系时与 `translated_all` 共用。
pub fn translated(op: &Annotation, dx: f64, dy: f64) -> Annotation {
    let point = |point: &Point| Point {
        x: point.x + dx,
        y: point.y + dy,
    };
    let points = |points: &[Point]| points.iter().map(point).collect::<Vec<_>>();
    match op {
        Annotation::Arrow {
            from,
            to,
            color,
            stroke_width,
        } => Annotation::Arrow {
            from: point(from),
            to: point(to),
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Rect {
            x,
            y,
            width,
            height,
            color,
            stroke_width,
        } => Annotation::Rect {
            x: x + dx,
            y: y + dy,
            width: *width,
            height: *height,
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Mosaic {
            x,
            y,
            width,
            height,
            block,
        } => Annotation::Mosaic {
            x: x + dx,
            y: y + dy,
            width: *width,
            height: *height,
            block: *block,
        },
        Annotation::Text {
            x,
            y,
            text,
            size,
            color,
        } => Annotation::Text {
            x: x + dx,
            y: y + dy,
            text: text.clone(),
            size: *size,
            color: color.clone(),
        },
        Annotation::Ellipse {
            x,
            y,
            width,
            height,
            color,
            stroke_width,
        } => Annotation::Ellipse {
            x: x + dx,
            y: y + dy,
            width: *width,
            height: *height,
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Line {
            from,
            to,
            color,
            stroke_width,
        } => Annotation::Line {
            from: point(from),
            to: point(to),
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Number {
            x,
            y,
            value,
            size,
            color,
        } => Annotation::Number {
            x: x + dx,
            y: y + dy,
            value: *value,
            size: *size,
            color: color.clone(),
        },
        Annotation::Highlighter {
            points: polyline,
            color,
            stroke_width,
        } => Annotation::Highlighter {
            points: points(polyline),
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Pen {
            points: polyline,
            color,
            stroke_width,
        } => Annotation::Pen {
            points: points(polyline),
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Blur {
            x,
            y,
            width,
            height,
            sigma,
        } => Annotation::Blur {
            x: x + dx,
            y: y + dy,
            width: *width,
            height: *height,
            sigma: *sigma,
        },
        Annotation::Spotlight {
            x,
            y,
            width,
            height,
            dim,
        } => Annotation::Spotlight {
            x: x + dx,
            y: y + dy,
            width: *width,
            height: *height,
            dim: *dim,
        },
        Annotation::Magnifier {
            x,
            y,
            width,
            height,
            zoom,
            color,
        } => Annotation::Magnifier {
            x: x + dx,
            y: y + dy,
            width: *width,
            height: *height,
            zoom: *zoom,
            color: color.clone(),
        },
        Annotation::Bubble {
            x,
            y,
            width,
            height,
            text,
            size,
            color,
        } => Annotation::Bubble {
            x: x + dx,
            y: y + dy,
            width: *width,
            height: *height,
            text: text.clone(),
            size: *size,
            color: color.clone(),
        },
        Annotation::Sticker {
            x,
            y,
            width,
            height,
            sticker,
        } => Annotation::Sticker {
            x: x + dx,
            y: y + dy,
            width: *width,
            height: *height,
            sticker: sticker.clone(),
        },
        Annotation::Erase {
            x,
            y,
            width,
            height,
            color,
        } => Annotation::Erase {
            x: x + dx,
            y: y + dy,
            width: *width,
            height: *height,
            color: color.clone(),
        },
    }
}

pub fn translated_all(ops: &[Annotation], dx: f64, dy: f64) -> Vec<Annotation> {
    ops.iter().map(|op| translated(op, dx, dy)).collect()
}

/// R6:预览旋转/裁剪的坐标映射,与 `capture::buffer` 的像素变换共用同一几何:
/// 顺时针 (x, y) → (h - y, x),逆时针 (x, y) → (y, w - x),裁剪平移 (-x0, -y0)。
/// 旋转按边界坐标计算,90° 时轴对齐矩形宽高互换;帧尺寸为变换前的值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameTransform {
    RotateCw { height: f64 },
    RotateCcw { width: f64 },
    Crop { dx: f64, dy: f64 },
}

impl FrameTransform {
    pub fn map_point(self, x: f64, y: f64) -> (f64, f64) {
        match self {
            Self::RotateCw { height } => (height - y, x),
            Self::RotateCcw { width } => (y, width - x),
            Self::Crop { dx, dy } => (x + dx, y + dy),
        }
    }

    /// 轴对齐矩形的重映射:取两角映射后的包围盒(90° 旋转宽高互换)。
    pub fn map_bounds(self, x: f64, y: f64, width: f64, height: f64) -> (f64, f64, f64, f64) {
        let (x0, y0) = self.map_point(x, y);
        let (x1, y1) = self.map_point(x + width, y + height);
        let left = x0.min(x1);
        let top = y0.min(y1);
        (left, top, (x1 - x0).abs(), (y1 - y0).abs())
    }
}

/// 单个图元的重映射:包围盒类图元按 `map_bounds` 换位,折线/箭头按点映射,
/// 文字与序号等定点图元映射锚点(不引入旋转属性,保持既有坐标 schema)。
pub fn transformed(op: &Annotation, transform: FrameTransform) -> Annotation {
    let point = |point: &Point| {
        let (x, y) = transform.map_point(point.x, point.y);
        Point { x, y }
    };
    let points = |points: &[Point]| points.iter().map(point).collect::<Vec<_>>();
    let bounds =
        |x: f64, y: f64, width: f64, height: f64| transform.map_bounds(x, y, width, height);
    match op {
        Annotation::Arrow {
            from,
            to,
            color,
            stroke_width,
        } => Annotation::Arrow {
            from: point(from),
            to: point(to),
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Rect {
            x,
            y,
            width,
            height,
            color,
            stroke_width,
        } => {
            let (x, y, width, height) = bounds(*x, *y, *width, *height);
            Annotation::Rect {
                x,
                y,
                width,
                height,
                color: color.clone(),
                stroke_width: *stroke_width,
            }
        }
        Annotation::Mosaic {
            x,
            y,
            width,
            height,
            block,
        } => {
            let (x, y, width, height) = bounds(*x, *y, *width, *height);
            Annotation::Mosaic {
                x,
                y,
                width,
                height,
                block: *block,
            }
        }
        Annotation::Text {
            x,
            y,
            text,
            size,
            color,
        } => {
            let (x, y) = transform.map_point(*x, *y);
            Annotation::Text {
                x,
                y,
                text: text.clone(),
                size: *size,
                color: color.clone(),
            }
        }
        Annotation::Ellipse {
            x,
            y,
            width,
            height,
            color,
            stroke_width,
        } => {
            let (x, y, width, height) = bounds(*x, *y, *width, *height);
            Annotation::Ellipse {
                x,
                y,
                width,
                height,
                color: color.clone(),
                stroke_width: *stroke_width,
            }
        }
        Annotation::Line {
            from,
            to,
            color,
            stroke_width,
        } => Annotation::Line {
            from: point(from),
            to: point(to),
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Number {
            x,
            y,
            value,
            size,
            color,
        } => {
            let (x, y) = transform.map_point(*x, *y);
            Annotation::Number {
                x,
                y,
                value: *value,
                size: *size,
                color: color.clone(),
            }
        }
        Annotation::Highlighter {
            points: polyline,
            color,
            stroke_width,
        } => Annotation::Highlighter {
            points: points(polyline),
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Pen {
            points: polyline,
            color,
            stroke_width,
        } => Annotation::Pen {
            points: points(polyline),
            color: color.clone(),
            stroke_width: *stroke_width,
        },
        Annotation::Blur {
            x,
            y,
            width,
            height,
            sigma,
        } => {
            let (x, y, width, height) = bounds(*x, *y, *width, *height);
            Annotation::Blur {
                x,
                y,
                width,
                height,
                sigma: *sigma,
            }
        }
        Annotation::Spotlight {
            x,
            y,
            width,
            height,
            dim,
        } => {
            let (x, y, width, height) = bounds(*x, *y, *width, *height);
            Annotation::Spotlight {
                x,
                y,
                width,
                height,
                dim: *dim,
            }
        }
        Annotation::Magnifier {
            x,
            y,
            width,
            height,
            zoom,
            color,
        } => {
            let (x, y, width, height) = bounds(*x, *y, *width, *height);
            Annotation::Magnifier {
                x,
                y,
                width,
                height,
                zoom: *zoom,
                color: color.clone(),
            }
        }
        Annotation::Bubble {
            x,
            y,
            width,
            height,
            text,
            size,
            color,
        } => {
            let (x, y, width, height) = bounds(*x, *y, *width, *height);
            Annotation::Bubble {
                x,
                y,
                width,
                height,
                text: text.clone(),
                size: *size,
                color: color.clone(),
            }
        }
        Annotation::Sticker {
            x,
            y,
            width,
            height,
            sticker,
        } => {
            let (x, y, width, height) = bounds(*x, *y, *width, *height);
            Annotation::Sticker {
                x,
                y,
                width,
                height,
                sticker: sticker.clone(),
            }
        }
        Annotation::Erase {
            x,
            y,
            width,
            height,
            color,
        } => {
            let (x, y, width, height) = bounds(*x, *y, *width, *height);
            Annotation::Erase {
                x,
                y,
                width,
                height,
                color: color.clone(),
            }
        }
    }
}

pub fn transformed_all(ops: &[Annotation], transform: FrameTransform) -> Vec<Annotation> {
    ops.iter().map(|op| transformed(op, transform)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_whitespace_text_is_not_exported() {
        let ops = vec![
            Annotation::Text {
                x: 4.0,
                y: 4.0,
                text: String::new(),
                size: 18.0,
                color: default_color(),
            },
            Annotation::Text {
                x: 8.0,
                y: 8.0,
                text: " \n\t ".into(),
                size: 18.0,
                color: default_color(),
            },
            Annotation::Rect {
                x: 1.0,
                y: 1.0,
                width: 10.0,
                height: 6.0,
                color: default_color(),
                stroke_width: None,
            },
        ];
        let kept = exportable(&ops);
        assert_eq!(kept.len(), 1);
        assert!(matches!(kept[0], Annotation::Rect { .. }));
    }

    #[test]
    fn cancelled_text_never_enters_the_list() {
        let ops: Vec<Annotation> = Vec::new();
        assert!(exportable(&ops).is_empty());
    }

    #[test]
    fn legacy_arrow_json_without_style_fields_defaults() {
        let op: Annotation =
            serde_json::from_str(r#"{"type":"arrow","from":{"x":1,"y":2},"to":{"x":9,"y":8}}"#)
                .unwrap();
        let Annotation::Arrow {
            color,
            stroke_width,
            ..
        } = op
        else {
            panic!("expected arrow");
        };
        assert_eq!(color, DEFAULT_COLOR);
        assert_eq!(stroke_width, None);
    }

    #[test]
    fn legacy_rect_and_text_json_without_style_fields_defaults() {
        let rect: Annotation =
            serde_json::from_str(r#"{"type":"rect","x":0,"y":0,"width":10,"height":4}"#).unwrap();
        assert!(matches!(
            &rect,
            Annotation::Rect {
                color,
                stroke_width: None,
                ..
            } if color == DEFAULT_COLOR
        ));
        let text: Annotation =
            serde_json::from_str(r#"{"type":"text","x":2,"y":2,"text":"hi"}"#).unwrap();
        assert!(matches!(
            &text,
            Annotation::Text { color, size, .. } if color == DEFAULT_COLOR && *size == 22.0
        ));
    }

    #[test]
    fn mosaic_json_with_style_fields_is_still_accepted() {
        let op: Annotation = serde_json::from_str(
            r##"{"type":"mosaic","x":0,"y":0,"width":8,"height":8,"color":"#2563eb"}"##,
        )
        .unwrap();
        assert!(matches!(op, Annotation::Mosaic { block: 12, .. }));
    }

    #[test]
    fn style_fields_serialize_camel_case_and_roundtrip() {
        let op = Annotation::Arrow {
            from: Point { x: 1.0, y: 1.0 },
            to: Point { x: 5.0, y: 5.0 },
            color: "#2563eb".into(),
            stroke_width: Some(5.0),
        };
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"strokeWidth\":5.0"), "got {json}");
        assert!(json.contains("\"color\":\"#2563eb\""), "got {json}");
        let back: Annotation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, op);
    }

    #[test]
    fn new_variants_roundtrip_camel_case_json() {
        let ops = vec![
            Annotation::Ellipse {
                x: 1.0,
                y: 2.0,
                width: 10.0,
                height: 6.0,
                color: "#2563eb".into(),
                stroke_width: Some(5.0),
            },
            Annotation::Line {
                from: Point { x: 1.0, y: 2.0 },
                to: Point { x: 9.0, y: 8.0 },
                color: "#10b981".into(),
                stroke_width: None,
            },
            Annotation::Number {
                x: 3.0,
                y: 4.0,
                value: 7,
                size: 22.0,
                color: DEFAULT_COLOR.into(),
            },
            Annotation::Highlighter {
                points: vec![Point { x: 0.0, y: 0.0 }, Point { x: 4.0, y: 6.0 }],
                color: "#f59e0b".into(),
                stroke_width: Some(3.0),
            },
            Annotation::Pen {
                points: vec![Point { x: 0.0, y: 0.0 }, Point { x: 4.0, y: 6.0 }],
                color: DEFAULT_COLOR.into(),
                stroke_width: None,
            },
            Annotation::Blur {
                x: 0.0,
                y: 0.0,
                width: 20.0,
                height: 12.0,
                sigma: 8.0,
            },
        ];
        for op in ops {
            let json = serde_json::to_string(&op).unwrap();
            assert!(json.contains("\"type\""), "got {json}");
            let back: Annotation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, op, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn new_variants_default_style_fields_when_absent() {
        let line: Annotation =
            serde_json::from_str(r#"{"type":"line","from":{"x":0,"y":0},"to":{"x":3,"y":3}}"#)
                .unwrap();
        assert!(matches!(
            &line,
            Annotation::Line {
                color,
                stroke_width: None,
                ..
            } if color == DEFAULT_COLOR
        ));
        let ellipse: Annotation =
            serde_json::from_str(r#"{"type":"ellipse","x":0,"y":0,"width":8,"height":6}"#).unwrap();
        assert!(matches!(
            &ellipse,
            Annotation::Ellipse {
                color,
                stroke_width: None,
                ..
            } if color == DEFAULT_COLOR
        ));
        let number: Annotation =
            serde_json::from_str(r#"{"type":"number","x":2,"y":2,"value":3}"#).unwrap();
        assert!(matches!(
            &number,
            Annotation::Number { color, size, .. } if color == DEFAULT_COLOR && *size == 22.0
        ));
        let blur: Annotation =
            serde_json::from_str(r#"{"type":"blur","x":0,"y":0,"width":8,"height":8}"#).unwrap();
        assert!(matches!(&blur, Annotation::Blur { sigma, .. } if *sigma > 0.0));
    }

    #[test]
    fn legacy_four_tools_still_parse_with_new_variants_present() {
        // 旧版图元 JSON 必须继续解析；新枚举不得改变既有 tag 与默认值。
        for raw in [
            r#"{"type":"arrow","from":{"x":1,"y":2},"to":{"x":9,"y":8}}"#,
            r#"{"type":"rect","x":0,"y":0,"width":10,"height":4}"#,
            r#"{"type":"mosaic","x":0,"y":0,"width":8,"height":8}"#,
            r#"{"type":"text","x":2,"y":2,"text":"hi"}"#,
        ] {
            let op: Annotation = serde_json::from_str(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert!(op.is_exportable() || matches!(op, Annotation::Text { .. }));
        }
    }

    #[test]
    fn degenerate_new_annotations_are_not_exportable() {
        assert!(!Annotation::Line {
            from: Point { x: 5.0, y: 5.0 },
            to: Point { x: 5.0, y: 5.0 },
            color: default_color(),
            stroke_width: None,
        }
        .is_exportable());
        assert!(!Annotation::Ellipse {
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 10.0,
            color: default_color(),
            stroke_width: None,
        }
        .is_exportable());
        assert!(!Annotation::Number {
            x: 1.0,
            y: 1.0,
            value: 0,
            size: 22.0,
            color: default_color(),
        }
        .is_exportable());
        assert!(!Annotation::Pen {
            points: Vec::new(),
            color: default_color(),
            stroke_width: None,
        }
        .is_exportable());
        assert!(!Annotation::Highlighter {
            points: vec![Point { x: 1.0, y: 1.0 }],
            color: default_color(),
            stroke_width: None,
        }
        .is_exportable());
        // 极小模糊选区不产生无效标注。
        assert!(!Annotation::Blur {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            sigma: 8.0,
        }
        .is_exportable());
        assert!(!Annotation::Blur {
            x: 0.0,
            y: 0.0,
            width: 8.0,
            height: 8.0,
            sigma: 0.0,
        }
        .is_exportable());
    }

    #[test]
    fn valid_new_annotations_are_exportable() {
        assert!(Annotation::Highlighter {
            points: vec![Point { x: 0.0, y: 0.0 }, Point { x: 4.0, y: 0.0 }],
            color: default_color(),
            stroke_width: None,
        }
        .is_exportable());
        assert!(Annotation::Blur {
            x: 0.0,
            y: 0.0,
            width: 12.0,
            height: 8.0,
            sigma: 6.0,
        }
        .is_exportable());
        assert!(Annotation::Number {
            x: 1.0,
            y: 1.0,
            value: 1,
            size: 22.0,
            color: default_color(),
        }
        .is_exportable());
    }

    #[test]
    fn r5_variants_roundtrip_camel_case_json() {
        let ops = vec![
            Annotation::Spotlight {
                x: 4.0,
                y: 4.0,
                width: 40.0,
                height: 24.0,
                dim: 0.4,
            },
            Annotation::Magnifier {
                x: 4.0,
                y: 4.0,
                width: 40.0,
                height: 40.0,
                zoom: 4.0,
                color: "#2563eb".into(),
            },
            Annotation::Bubble {
                x: 2.0,
                y: 2.0,
                width: 60.0,
                height: 36.0,
                text: "你好\nworld".into(),
                size: 18.0,
                color: "#10b981".into(),
            },
            Annotation::Sticker {
                x: 8.0,
                y: 8.0,
                width: 32.0,
                height: 32.0,
                sticker: "star".into(),
            },
            Annotation::Erase {
                x: 1.0,
                y: 2.0,
                width: 20.0,
                height: 10.0,
                color: Some("#111827".into()),
            },
            Annotation::Erase {
                x: 1.0,
                y: 2.0,
                width: 20.0,
                height: 10.0,
                color: None,
            },
        ];
        for op in ops {
            let json = serde_json::to_string(&op).unwrap();
            assert!(json.contains("\"type\""), "got {json}");
            let back: Annotation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, op, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn r5_variants_default_fields_when_absent() {
        let spotlight: Annotation =
            serde_json::from_str(r#"{"type":"spotlight","x":0,"y":0,"width":20,"height":10}"#)
                .unwrap();
        assert!(matches!(
            &spotlight,
            Annotation::Spotlight { dim, .. } if (*dim - DEFAULT_SPOTLIGHT_DIM).abs() < f64::EPSILON
        ));
        let magnifier: Annotation =
            serde_json::from_str(r#"{"type":"magnifier","x":0,"y":0,"width":20,"height":20}"#)
                .unwrap();
        assert!(matches!(
            &magnifier,
            Annotation::Magnifier { zoom, color, .. }
                if (*zoom - DEFAULT_MAGNIFIER_ZOOM).abs() < f64::EPSILON && color == DEFAULT_COLOR
        ));
        let bubble: Annotation =
            serde_json::from_str(r#"{"type":"bubble","x":0,"y":0,"width":40,"height":30}"#)
                .unwrap();
        assert!(matches!(
            &bubble,
            Annotation::Bubble { text, size, color, .. }
                if text.is_empty() && *size == 22.0 && color == DEFAULT_COLOR
        ));
        let erase: Annotation =
            serde_json::from_str(r#"{"type":"erase","x":0,"y":0,"width":10,"height":10}"#).unwrap();
        assert!(matches!(&erase, Annotation::Erase { color: None, .. }));
    }

    #[test]
    fn r5_degenerate_annotations_are_not_exportable() {
        assert!(!Annotation::Spotlight {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 10.0,
            dim: 0.5,
        }
        .is_exportable());
        assert!(!Annotation::Spotlight {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            dim: 0.0,
        }
        .is_exportable());
        assert!(!Annotation::Magnifier {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            zoom: 1.0,
            color: default_color(),
        }
        .is_exportable());
        assert!(!Annotation::Sticker {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            sticker: "  ".into(),
        }
        .is_exportable());
        assert!(!Annotation::Bubble {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            text: "hi".into(),
            size: 22.0,
            color: default_color(),
        }
        .is_exportable());
        assert!(!Annotation::Erase {
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 10.0,
            color: None,
        }
        .is_exportable());
    }

    #[test]
    fn r5_translated_shifts_new_variants_without_touching_size() {
        let ops = vec![
            Annotation::Spotlight {
                x: 1.0,
                y: 2.0,
                width: 8.0,
                height: 6.0,
                dim: 0.5,
            },
            Annotation::Magnifier {
                x: 1.0,
                y: 2.0,
                width: 8.0,
                height: 6.0,
                zoom: 2.0,
                color: "#2563eb".into(),
            },
            Annotation::Bubble {
                x: 1.0,
                y: 2.0,
                width: 8.0,
                height: 6.0,
                text: "hi".into(),
                size: 22.0,
                color: "#2563eb".into(),
            },
            Annotation::Sticker {
                x: 1.0,
                y: 2.0,
                width: 8.0,
                height: 8.0,
                sticker: "star".into(),
            },
            Annotation::Erase {
                x: 1.0,
                y: 2.0,
                width: 8.0,
                height: 6.0,
                color: None,
            },
        ];
        for op in &ops {
            let moved = translated(op, 5.0, -3.0);
            match (&op, &moved) {
                (
                    Annotation::Spotlight { x, y, dim, .. },
                    Annotation::Spotlight {
                        x: mx,
                        y: my,
                        dim: mdim,
                        ..
                    },
                ) => {
                    assert_eq!((*mx, *my, *mdim), (x + 5.0, y - 3.0, *dim));
                }
                (
                    Annotation::Magnifier { x, y, zoom, .. },
                    Annotation::Magnifier {
                        x: mx,
                        y: my,
                        zoom: mzoom,
                        ..
                    },
                ) => {
                    assert_eq!((*mx, *my, *mzoom), (x + 5.0, y - 3.0, *zoom));
                }
                (
                    Annotation::Bubble { x, y, text, .. },
                    Annotation::Bubble {
                        x: mx,
                        y: my,
                        text: mtext,
                        ..
                    },
                ) => {
                    assert_eq!((*mx, *my, mtext), (x + 5.0, y - 3.0, text));
                }
                (
                    Annotation::Sticker { x, y, sticker, .. },
                    Annotation::Sticker {
                        x: mx,
                        y: my,
                        sticker: ms,
                        ..
                    },
                ) => {
                    assert_eq!((*mx, *my, ms), (x + 5.0, y - 3.0, sticker));
                }
                (Annotation::Erase { x, y, .. }, Annotation::Erase { x: mx, y: my, .. }) => {
                    assert_eq!((*mx, *my), (x + 5.0, y - 3.0));
                }
                _ => panic!("variant changed shape after translation"),
            }
        }
    }

    #[test]
    fn frame_transform_rotates_rect_bounds_and_swaps_axes() {
        // 源帧 10x4:矩形 (2,1,3,2) 顺时针后 x = h - y - height = 1,y = x = 2,
        // 宽高互换(边界坐标,与像素旋转 (h-1-y, x) 对齐)。
        let cw = FrameTransform::RotateCw { height: 4.0 };
        let (x, y, width, height) = cw.map_bounds(2.0, 1.0, 3.0, 2.0);
        assert_eq!((x, y, width, height), (1.0, 2.0, 2.0, 3.0));

        // 逆时针:矩形 (2,1,3,2) → x = y = 1,y = w - x - width = 5。
        let ccw = FrameTransform::RotateCcw { width: 10.0 };
        let (x, y, width, height) = ccw.map_bounds(2.0, 1.0, 3.0, 2.0);
        assert_eq!((x, y, width, height), (1.0, 5.0, 2.0, 3.0));

        // 裁剪只平移,不改变宽高。
        let crop = FrameTransform::Crop { dx: -4.0, dy: -6.0 };
        let (x, y, width, height) = crop.map_bounds(4.0, 6.0, 5.0, 7.0);
        assert_eq!((x, y, width, height), (0.0, 0.0, 5.0, 7.0));
    }

    #[test]
    fn frame_transform_maps_every_annotation_variant_consistently() {
        let transform = FrameTransform::RotateCw { height: 20.0 };
        let rect = Annotation::Rect {
            x: 2.0,
            y: 3.0,
            width: 4.0,
            height: 5.0,
            color: default_color(),
            stroke_width: None,
        };
        match transformed(&rect, transform) {
            Annotation::Rect {
                x,
                y,
                width,
                height,
                ..
            } => assert_eq!((x, y, width, height), (20.0 - 3.0 - 5.0, 2.0, 5.0, 4.0)),
            other => panic!("rect changed variant: {other:?}"),
        }

        let arrow = Annotation::Arrow {
            from: Point { x: 1.0, y: 2.0 },
            to: Point { x: 3.0, y: 4.0 },
            color: default_color(),
            stroke_width: Some(2.0),
        };
        match transformed(&arrow, transform) {
            Annotation::Arrow { from, to, .. } => {
                assert_eq!((from.x, from.y), (18.0, 1.0));
                assert_eq!((to.x, to.y), (16.0, 3.0));
            }
            other => panic!("arrow changed variant: {other:?}"),
        }

        let polyline = Annotation::Pen {
            points: vec![Point { x: 0.0, y: 0.0 }, Point { x: 20.0, y: 20.0 }],
            color: default_color(),
            stroke_width: None,
        };
        match transformed(&polyline, transform) {
            Annotation::Pen { points, .. } => {
                assert_eq!((points[0].x, points[0].y), (20.0, 0.0));
                assert_eq!((points[1].x, points[1].y), (0.0, 20.0));
            }
            other => panic!("pen changed variant: {other:?}"),
        }

        // 文字锚点按同一仿射映射(不引入旋转属性,保持既有坐标 schema)。
        let text = Annotation::Text {
            x: 5.0,
            y: 6.0,
            text: "字".into(),
            size: 20.0,
            color: default_color(),
        };
        match transformed(&text, transform) {
            Annotation::Text { x, y, text, .. } => {
                assert_eq!((x, y, text), (14.0, 5.0, "字".to_string()));
            }
            other => panic!("text changed variant: {other:?}"),
        }
    }

    #[test]
    fn frame_transform_crop_round_trips_with_translation() {
        let ops = vec![
            Annotation::Mosaic {
                x: 3.0,
                y: 4.0,
                width: 5.0,
                height: 6.0,
                block: 12,
            },
            Annotation::Number {
                x: 7.0,
                y: 8.0,
                value: 2,
                size: 22.0,
                color: default_color(),
            },
        ];
        let crop = FrameTransform::Crop { dx: -3.0, dy: -4.0 };
        let moved = transformed_all(&ops, crop);
        let restored = transformed_all(&moved, FrameTransform::Crop { dx: 3.0, dy: 4.0 });
        assert_eq!(restored, ops);
    }
}
