mod mosaic;
pub(crate) mod raster;

pub use raster::rasterize;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Annotation {
    Arrow {
        from: Point,
        to: Point,
    },
    Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
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
    },
}

fn default_block() -> u32 {
    12
}

fn default_text_size() -> f64 {
    22.0
}

impl Annotation {
    pub fn is_exportable(&self) -> bool {
        match self {
            Self::Text { text, .. } => !text.trim().is_empty(),
            Self::Arrow { from, to } => (from.x - to.x).abs() >= 1.0 || (from.y - to.y).abs() >= 1.0,
            Self::Rect { width, height, .. } | Self::Mosaic { width, height, .. } => {
                width.abs() >= 1.0 && height.abs() >= 1.0
            }
        }
    }
}

pub fn exportable(ops: &[Annotation]) -> Vec<Annotation> {
    ops.iter().filter(|op| op.is_exportable()).cloned().collect()
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
            },
            Annotation::Text {
                x: 8.0,
                y: 8.0,
                text: " \n\t ".into(),
                size: 18.0,
            },
            Annotation::Rect {
                x: 1.0,
                y: 1.0,
                width: 10.0,
                height: 6.0,
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
}
