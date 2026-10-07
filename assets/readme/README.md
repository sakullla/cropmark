# README 宣传图片

这些图片由 `render.py` 使用虚构文案绘制，复用仓库现有 Cropmark 图标与工具栏图标；不包含桌面截图、真实文件、账号或设备信息。图中的界面与识别结果是功能示意，不代表实机截图或 OCR 测试结果。

- `hero.png`：品牌与核心功能，1600 × 880。
- `annotation.png`：标注、美化与分享示意，1600 × 960。
- `offline-ocr.png`：中文 / English 离线取字示意，1600 × 850。

在已有 Python 3、Pillow 和中文字体的环境中，从仓库根目录运行：

```sh
python assets/readme/render.py
```

Windows 默认使用本机 Microsoft YaHei 字体；其他系统通过 `CROPMARK_ART_FONT` 和 `CROPMARK_ART_FONT_BOLD` 指定中文常规与粗体字体文件。字体不会打包进仓库。生成过程只读取图标和字体，并覆盖本目录中的三张 PNG；不会启动应用、访问网络、截屏、读取剪贴板或更改系统设置。输出不附带 EXIF 或文本元数据。
