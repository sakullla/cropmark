# Cropmark icon v2

Generated through the current Codex provider (`sub1`) using the imagegen fallback CLI with the explicitly requested `gpt-image-2.5` model, high quality, on 2026-09-17. No credentials are stored in this directory.

- `cropmark-icon-v2.prompt.txt`: complete generation prompt.
- `cropmark-icon-v2-gpt-image-2.5-original.png`: unmodified provider output (1254 × 1254 RGBA, despite a 1024 × 1024 request).
- `cropmark-icon-v2.png`: normalized 1024 × 1024 transparent master.
- `cropmark-icon-v2-preview.png`: small-size previews on light and dark backgrounds.

The design uses a deep teal rounded tile, mint crop brackets and a captured-screen rectangle. Transparency was present in the returned image; export removes faint alpha noise and normalizes size and padding.

The application uses `src-tauri/icons/v2/` for PNG, Windows ICO and macOS ICNS assets. The macOS menu bar uses a separate alpha-only template extracted from the mint symbol. The original v1 assets remain available.

To re-export the saved artwork without another API call, install Pillow and run:

```powershell
python assets/brand/export_generated_icon.py
```
