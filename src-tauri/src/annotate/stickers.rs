//! R5:随包贴纸素材。编译期嵌入 `src-tauri/assets/stickers/*.png`,断网可用、
//! 不引入第三方素材;导出时由 `raster` 按 id 解码绘制,前端素材选择与
//! `Annotation::Sticker.sticker` 共用同一批 id。
//!
//! 素材由 `assets/stickers/generate.mjs` 自绘生成,修改后重新运行该脚本即可。

use std::collections::HashMap;
use std::sync::OnceLock;

use image::{ImageFormat, RgbaImage};
use serde::Serialize;

/// 素材清单。id 是稳定契约:前端标签键 `preview.sticker.<id>` 与
/// 历史/导出数据都引用它,新增素材只能追加,不能改名。
pub const STICKERS: [(&str, &[u8]); 6] = [
    ("star", include_bytes!("../../assets/stickers/star.png")),
    ("heart", include_bytes!("../../assets/stickers/heart.png")),
    ("check", include_bytes!("../../assets/stickers/check.png")),
    ("cross", include_bytes!("../../assets/stickers/cross.png")),
    (
        "exclaim",
        include_bytes!("../../assets/stickers/exclaim.png"),
    ),
    ("smile", include_bytes!("../../assets/stickers/smile.png")),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StickerInfo {
    pub id: String,
    pub width: u32,
    pub height: u32,
}

fn decoded() -> &'static HashMap<&'static str, RgbaImage> {
    static DECODED: OnceLock<HashMap<&'static str, RgbaImage>> = OnceLock::new();
    DECODED.get_or_init(|| {
        let mut images = HashMap::with_capacity(STICKERS.len());
        for (id, bytes) in STICKERS {
            let Ok(image) = image::load_from_memory_with_format(bytes, ImageFormat::Png) else {
                continue;
            };
            images.insert(id, image.to_rgba8());
        }
        images
    })
}

/// 按 id 取解码后的 RGBA 素材;未知 id 或素材损坏时返回 None(绘制端跳过)。
pub fn image(id: &str) -> Option<&'static RgbaImage> {
    decoded().get(id)
}

/// 已成功解码的素材清单(保持 `STICKERS` 声明顺序)。
pub fn catalog() -> Vec<StickerInfo> {
    let images = decoded();
    STICKERS
        .iter()
        .filter_map(|(id, _)| {
            images.get(id).map(|image| StickerInfo {
                id: (*id).to_string(),
                width: image.width(),
                height: image.height(),
            })
        })
        .collect()
}

/// 前端素材选择列表:只回传 id 与尺寸,图像字节按需另取。
#[tauri::command]
pub fn get_sticker_catalog() -> Vec<StickerInfo> {
    catalog()
}

/// 前端画布用素材原图(PNG 字节)。前端解码后缓存为图片对象,
/// 与 Rust 栅格化共用同一份编译期嵌入素材。
#[tauri::command]
pub fn get_sticker_image(id: String) -> Result<tauri::ipc::Response, String> {
    let bytes = STICKERS
        .iter()
        .find(|(sticker_id, _)| *sticker_id == id)
        .map(|(_, bytes)| *bytes)
        .ok_or_else(|| crate::i18n::tp("error.capture.sticker_missing", &[("id", &id)]))?;
    Ok(tauri::ipc::Response::new(bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_sticker_decodes_to_non_empty_rgba() {
        let images = decoded();
        assert_eq!(images.len(), STICKERS.len());
        for (id, _) in STICKERS {
            let image = images.get(id).unwrap_or_else(|| panic!("{id} must decode"));
            assert!(
                image.width() >= 32 && image.height() >= 32,
                "{id} too small"
            );
            assert!(
                image.pixels().any(|px| px[3] > 0),
                "{id} must not be fully transparent"
            );
            assert!(
                image.pixels().any(|px| px[3] == 0),
                "{id} should keep transparent margins"
            );
        }
    }

    #[test]
    fn catalog_preserves_declared_order_and_ids() {
        let catalog = catalog();
        assert_eq!(catalog.len(), STICKERS.len());
        for (info, (id, _)) in catalog.iter().zip(STICKERS.iter()) {
            assert_eq!(info.id, *id);
            assert!(info.width > 0 && info.height > 0);
        }
    }

    #[test]
    fn unknown_sticker_id_has_no_image() {
        assert!(image("not-a-sticker").is_none());
        assert!(image("").is_none());
    }
}
