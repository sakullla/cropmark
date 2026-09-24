use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSpan {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 整图旋转候选(R11):低置信重试时按此旋转图像,结果坐标用同一变换逆回原图。
/// 变体与 `image::imageops` 的 rotate 函数一一对应,坐标映射必须与像素映射一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Identity,
    Rotate90,
    Rotate270,
    Rotate180,
}

impl Orientation {
    /// ADR-9 重试顺序:原图(逐行角度纠正) → 90° 顺时针 → 90° 逆时针 → 180°。
    pub const RETRY_ORDER: [Self; 4] = [
        Self::Identity,
        Self::Rotate90,
        Self::Rotate270,
        Self::Rotate180,
    ];

    /// 旋转后的图像尺寸;90°/270° 交换宽高。生产路径由 imageops 决定实际尺寸,
    /// 这里供坐标映射测试核对两者一致。
    #[cfg(test)]
    pub fn rotated_size(self, width: u32, height: u32) -> (u32, u32) {
        match self {
            Self::Identity | Self::Rotate180 => (width, height),
            Self::Rotate90 | Self::Rotate270 => (height, width),
        }
    }

    /// 把旋转图上的点(边界坐标,与像素边一致)逆变换回原图坐标。
    pub fn map_point(self, x: f64, y: f64, frame_width: f64, frame_height: f64) -> (f64, f64) {
        match self {
            Self::Identity => (x, y),
            Self::Rotate90 => (y, frame_height - x),
            Self::Rotate270 => (frame_width - y, x),
            Self::Rotate180 => (frame_width - x, frame_height - y),
        }
    }

    /// 把旋转图上的 AABB 逆变换回原图 AABB;90°/270° 时宽高互换。
    pub fn map_bounds(
        self,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        frame_width: f64,
        frame_height: f64,
    ) -> (f64, f64, f64, f64) {
        let (x0, y0) = self.map_point(x, y, frame_width, frame_height);
        let (x1, y1) = self.map_point(x + width, y + height, frame_width, frame_height);
        let left = x0.min(x1);
        let top = y0.min(y1);
        (left, top, (x1 - x0).abs(), (y1 - y0).abs())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    #[cfg(test)]
    pub fn from_points(a: (f64, f64), b: (f64, f64)) -> Self {
        let x = a.0.min(b.0);
        let y = a.1.min(b.1);
        Self {
            x,
            y,
            width: (a.0 - b.0).abs(),
            height: (a.1 - b.1).abs(),
        }
    }

    pub fn normalized(self) -> (f64, f64, f64, f64) {
        let x0 = self.x.min(self.x + self.width);
        let y0 = self.y.min(self.y + self.height);
        let x1 = self.x.max(self.x + self.width);
        let y1 = self.y.max(self.y + self.height);
        (x0, y0, x1, y1)
    }
}

impl TextSpan {
    /// 旋转候选的识别结果:保持文本,坐标按所用旋转逆变换回原图(R11)。
    pub fn mapped_from(
        self,
        orientation: Orientation,
        frame_width: f64,
        frame_height: f64,
    ) -> Self {
        if orientation == Orientation::Identity {
            return self;
        }
        let (x, y, width, height) = orientation.map_bounds(
            self.x,
            self.y,
            self.width,
            self.height,
            frame_width,
            frame_height,
        );
        Self {
            text: self.text,
            x,
            y,
            width,
            height,
        }
    }

    pub fn area(&self) -> f64 {
        self.width.max(0.0) * self.height.max(0.0)
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }

    pub fn intersects(&self, rect: Rect) -> bool {
        let (x0, y0, x1, y1) = rect.normalized();
        self.x < x1 && self.x + self.width > x0 && self.y < y1 && self.y + self.height > y0
    }
}

pub fn aabb(points: &[(f64, f64)]) -> Option<(f64, f64, f64, f64)> {
    let mut iter = points.iter();
    let first = iter.next()?;
    let mut min_x = first.0;
    let mut min_y = first.1;
    let mut max_x = first.0;
    let mut max_y = first.1;
    for &(x, y) in iter {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    Some((
        min_x,
        min_y,
        (max_x - min_x).max(1.0),
        (max_y - min_y).max(1.0),
    ))
}

pub fn expand_for_selection(spans: &[TextSpan]) -> Vec<TextSpan> {
    let mut out = Vec::new();
    for span in spans {
        // R11:90° 旋转截图映射回来的竖排文本块整块可选;按横排拆 token 会与预览位置错位。
        if span.height > span.width {
            out.push(span.clone());
            continue;
        }
        out.extend(tokenize_span(span));
    }
    out
}

pub fn tokenize_span(span: &TextSpan) -> Vec<TextSpan> {
    let tokens = tokenize(&span.text);
    if tokens.len() <= 1 {
        return vec![span.clone()];
    }
    let total: f64 = tokens.iter().map(|token| token_weight(token)).sum();
    if total <= 0.0 {
        return vec![span.clone()];
    }
    let mut x = span.x;
    tokens
        .into_iter()
        .map(|text| {
            let width = (span.width * token_weight(&text) / total).max(1.0);
            let next = TextSpan {
                text,
                x,
                y: span.y,
                width,
                height: span.height,
            };
            x += width;
            next
        })
        .collect()
}

pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut ascii_run: Option<bool> = None;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            ascii_run = None;
            continue;
        }
        let ascii = ch.is_ascii();
        if ascii_run.is_some_and(|prev| prev != ascii) && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
        ascii_run = Some(ascii);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn token_weight(text: &str) -> f64 {
    text.chars()
        .map(|ch| if ch.is_ascii() { 0.55 } else { 1.0 })
        .sum::<f64>()
        .max(0.55)
}

pub fn hit_point(spans: &[TextSpan], x: f64, y: f64) -> Option<usize> {
    spans
        .iter()
        .enumerate()
        .filter(|(_, span)| !span.text.trim().is_empty() && span.contains(x, y))
        .min_by(|a, b| area_cmp(a.1.area(), b.1.area()).then_with(|| a.0.cmp(&b.0)))
        .map(|(index, _)| index)
}

pub fn hit_rect(spans: &[TextSpan], rect: Rect) -> Vec<usize> {
    let mut hits: Vec<usize> = spans
        .iter()
        .enumerate()
        .filter(|(_, span)| !span.text.trim().is_empty() && span.intersects(rect))
        .map(|(index, _)| index)
        .collect();
    hits.sort_by(|&a, &b| reading_order(&spans[a], &spans[b]).then_with(|| a.cmp(&b)));
    hits
}

pub fn all_indices(spans: &[TextSpan]) -> Vec<usize> {
    let mut hits: Vec<usize> = spans
        .iter()
        .enumerate()
        .filter(|(_, span)| !span.text.trim().is_empty())
        .map(|(index, _)| index)
        .collect();
    hits.sort_by(|&a, &b| reading_order(&spans[a], &spans[b]).then_with(|| a.cmp(&b)));
    hits
}

pub fn join_spans(spans: &[TextSpan], indices: &[usize]) -> String {
    if indices.is_empty() {
        return String::new();
    }
    let mut ordered = indices.to_vec();
    ordered.sort_by(|&a, &b| match (spans.get(a), spans.get(b)) {
        (Some(left), Some(right)) => reading_order(left, right).then_with(|| a.cmp(&b)),
        _ => a.cmp(&b),
    });
    ordered.dedup();
    let mut lines: Vec<Vec<usize>> = Vec::new();
    for index in ordered {
        if index >= spans.len() {
            continue;
        }
        if let Some(line) = lines.last_mut() {
            if same_line(&spans[*line.last().expect("line not empty")], &spans[index]) {
                line.push(index);
                continue;
            }
        }
        lines.push(vec![index]);
    }
    lines
        .iter()
        .map(|line| join_line(spans, line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn join_line(spans: &[TextSpan], line: &[usize]) -> String {
    let mut out = String::new();
    for (offset, &index) in line.iter().enumerate() {
        let span = &spans[index];
        if offset > 0 {
            let prev = &spans[line[offset - 1]];
            let gap = span.x - (prev.x + prev.width);
            if needs_space(prev, span, gap) {
                out.push(' ');
            }
        }
        out.push_str(span.text.trim());
    }
    out
}

fn needs_space(prev: &TextSpan, next: &TextSpan, gap: f64) -> bool {
    let prev_ascii = is_ascii_token(&prev.text);
    let next_ascii = is_ascii_token(&next.text);
    if prev_ascii || next_ascii {
        return true;
    }
    gap > prev.height.max(8.0) * 0.35
}

fn is_ascii_token(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty() && trimmed.is_ascii()
}

fn same_line(a: &TextSpan, b: &TextSpan) -> bool {
    let (_, ay) = a.center();
    let (_, by) = b.center();
    (ay - by).abs() <= a.height.min(b.height) * 0.5
}

fn reading_order(a: &TextSpan, b: &TextSpan) -> Ordering {
    if same_line(a, b) {
        a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal)
    } else {
        a.center()
            .1
            .partial_cmp(&b.center().1)
            .unwrap_or(Ordering::Equal)
    }
}

fn area_cmp(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, x: f64, y: f64, width: f64, height: f64) -> TextSpan {
        TextSpan {
            text: text.into(),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn click_hits_smallest_containing_box() {
        let spans = vec![
            span("Hello 中文", 0.0, 0.0, 200.0, 20.0),
            span("Hello", 0.0, 0.0, 80.0, 20.0),
            span("中文", 90.0, 0.0, 60.0, 20.0),
        ];
        assert_eq!(hit_point(&spans, 20.0, 10.0), Some(1));
        assert_eq!(hit_point(&spans, 110.0, 10.0), Some(2));
        assert_eq!(hit_point(&spans, 300.0, 10.0), None);
    }

    #[test]
    fn drag_selects_intersecting_boxes_in_reading_order() {
        let spans = vec![
            span("World", 90.0, 8.0, 50.0, 16.0),
            span("Hello", 10.0, 8.0, 50.0, 16.0),
            span("skip", 10.0, 80.0, 40.0, 16.0),
        ];
        let hits = hit_rect(
            &spans,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 160.0,
                height: 30.0,
            },
        );
        assert_eq!(hits, vec![1, 0]);
        assert_eq!(join_spans(&spans, &hits), "Hello World");
    }

    #[test]
    fn copy_all_joins_lines_without_dropping_cjk() {
        let spans = vec![
            span("Hello", 10.0, 8.0, 40.0, 16.0),
            span("中文", 10.0, 40.0, 48.0, 18.0),
            span("OCR", 62.0, 40.0, 36.0, 18.0),
        ];
        let all = all_indices(&spans);
        assert_eq!(join_spans(&spans, &all), "Hello\n中文 OCR");
    }

    #[test]
    fn empty_or_blank_spans_are_not_hits() {
        let spans = vec![
            span("   ", 0.0, 0.0, 20.0, 10.0),
            span("", 0.0, 20.0, 20.0, 10.0),
        ];
        assert!(hit_point(&spans, 5.0, 5.0).is_none());
        assert!(hit_rect(
            &spans,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 40.0,
                height: 40.0,
            },
        )
        .is_empty());
        assert!(join_spans(&spans, &all_indices(&spans)).is_empty());
    }

    #[test]
    fn tokenize_splits_ascii_words_from_cjk_runs() {
        assert_eq!(tokenize("Hello 中文OCR"), vec!["Hello", "中文", "OCR"]);
        let line = span("Hello 中文", 0.0, 0.0, 120.0, 20.0);
        let tokens = tokenize_span(&line);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].text, "Hello");
        assert_eq!(tokens[1].text, "中文");
        assert!(tokens[1].x >= tokens[0].x + tokens[0].width - 1e-6);
        assert_eq!(hit_point(&tokens, tokens[0].x + 2.0, 10.0), Some(0));
        assert_eq!(hit_point(&tokens, tokens[1].x + 2.0, 10.0), Some(1));
    }

    #[test]
    fn inverted_drag_rect_still_hits() {
        let spans = vec![span("A", 40.0, 40.0, 10.0, 10.0)];
        let hits = hit_rect(&spans, Rect::from_points((60.0, 60.0), (30.0, 30.0)));
        assert_eq!(hits, vec![0]);
    }

    #[test]
    fn rotation_bounds_map_back_to_source_frame() {
        let (w, h) = (100.0, 50.0);
        let (x, y, width, height) = (10.0, 20.0, 5.0, 6.0);
        assert_eq!(
            Orientation::Identity.map_bounds(x, y, width, height, w, h),
            (10.0, 20.0, 5.0, 6.0)
        );
        assert_eq!(
            Orientation::Rotate180.map_bounds(x, y, width, height, w, h),
            (85.0, 24.0, 5.0, 6.0)
        );
        assert_eq!(
            Orientation::Rotate90.map_bounds(x, y, width, height, w, h),
            (20.0, 35.0, 6.0, 5.0)
        );
        assert_eq!(
            Orientation::Rotate270.map_bounds(x, y, width, height, w, h),
            (74.0, 10.0, 6.0, 5.0)
        );
        assert_eq!(Orientation::Rotate90.rotated_size(100, 50), (50, 100));
        assert_eq!(Orientation::Rotate270.rotated_size(100, 50), (50, 100));
        assert_eq!(Orientation::Rotate180.rotated_size(100, 50), (100, 50));
        assert_eq!(Orientation::RETRY_ORDER.len(), 4);
        assert_eq!(Orientation::RETRY_ORDER[0], Orientation::Identity);
    }

    #[test]
    fn rotation_map_matches_imageops_pixel_mapping() {
        use image::{Rgb, RgbImage};

        let mut source = RgbImage::new(3, 2);
        for (x, y, pixel) in source.enumerate_pixels_mut() {
            *pixel = Rgb([(x * 10 + y) as u8, x as u8, y as u8]);
        }
        let cases = [
            (Orientation::Rotate90, image::imageops::rotate90(&source)),
            (Orientation::Rotate270, image::imageops::rotate270(&source)),
            (Orientation::Rotate180, image::imageops::rotate180(&source)),
        ];
        for (orientation, rotated) in cases {
            assert_eq!(
                (rotated.width(), rotated.height()),
                orientation.rotated_size(source.width(), source.height())
            );
            for (x, y, pixel) in rotated.enumerate_pixels() {
                let (sx, sy) = orientation.map_point(x as f64 + 0.5, y as f64 + 0.5, 3.0, 2.0);
                let source_pixel = source.get_pixel(sx.floor() as u32, sy.floor() as u32);
                assert_eq!(source_pixel, pixel, "{orientation:?} at ({x},{y})");
            }
        }
    }

    #[test]
    fn mapped_span_keeps_text_and_moves_bounds() {
        let span = span("中文", 10.0, 20.0, 5.0, 6.0);
        let mapped = span.clone().mapped_from(Orientation::Rotate90, 100.0, 50.0);
        assert_eq!(mapped.text, "中文");
        assert_eq!(
            (mapped.x, mapped.y, mapped.width, mapped.height),
            (20.0, 35.0, 6.0, 5.0)
        );
        assert_eq!(
            span.clone().mapped_from(Orientation::Identity, 100.0, 50.0),
            span
        );
    }

    #[test]
    fn vertical_spans_stay_whole_blocks_for_selection() {
        let spans = vec![span("中文", 10.0, 10.0, 20.0, 80.0)];
        assert_eq!(expand_for_selection(&spans), spans);
        let horizontal = vec![span("Hello 中文", 0.0, 0.0, 120.0, 20.0)];
        assert!(expand_for_selection(&horizontal).len() > 1);
    }
}
