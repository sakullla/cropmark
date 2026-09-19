pub(crate) mod blur;
mod mosaic;
pub(crate) mod raster;

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
    }
}

pub fn translated_all(ops: &[Annotation], dx: f64, dy: f64) -> Vec<Annotation> {
    ops.iter().map(|op| translated(op, dx, dy)).collect()
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
}
