# Offline PP-OCR models

Cropmark loads these files from disk at runtime. They are not downloaded and OCR does not open a network connection.

- `ch_PP-OCRv3_det_infer.onnx` — text detection
- `ch_PP-OCRv3_rec_infer.onnx` — printed Chinese and English recognition
- `ch_ppocr_mobile_v2.0_cls_infer.onnx` — orientation net (loaded, unused for upright screenshots)
