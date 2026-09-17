mod mosaic;
pub(crate) mod raster;

pub(crate) use raster::parse_hex_color;
pub use raster::rasterize;

use serde::{Deserialize, Serialize};

/// 默认标注色（玫红），与 v0.1.3 前的 STROKE 像素值一致。
pub const DEFAULT_COLOR: &str = "#e11d48";

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
}

fn default_block() -> u32 {
    12
}

fn default_text_size() -> f64 {
    22.0
}

fn default_color() -> String {
    DEFAULT_COLOR.into()
}

impl Annotation {
    pub fn is_exportable(&self) -> bool {
        match self {
            Self::Text { text, .. } => !text.trim().is_empty(),
            Self::Arrow { from, to, .. } => {
                (from.x - to.x).abs() >= 1.0 || (from.y - to.y).abs() >= 1.0
            }
            Self::Rect { width, height, .. } | Self::Mosaic { width, height, .. } => {
                width.abs() >= 1.0 && height.abs() >= 1.0
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
}
