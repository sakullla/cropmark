"""Export the approved GPT Image 2.5 artwork; requires Pillow."""

from pathlib import Path
from shutil import copyfile

from PIL import Image


ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "output" / "imagegen"
ICONS = ROOT / "src-tauri" / "icons" / "v2"


def main() -> None:
    source = Image.open(OUTPUT / "cropmark-icon-v2-gpt-image-2.5-original.png").convert("RGBA")
    # Discard almost transparent generation noise outside the rounded tile.
    alpha = source.getchannel("A").point(lambda value: 0 if value < 16 else value)
    source.putalpha(alpha)
    bounds = alpha.point(lambda value: 255 if value >= 128 else 0).getbbox()
    if bounds is None:
        raise ValueError("The source icon is empty")
    tile = source.crop(bounds)
    tile.thumbnail((896, 896), Image.Resampling.LANCZOS)
    master = Image.new("RGBA", (1024, 1024))
    master.alpha_composite(tile, ((1024 - tile.width) // 2, (1024 - tile.height) // 2))
    master.save(OUTPUT / "cropmark-icon-v2.png")
    copyfile(OUTPUT / "cropmark-icon-v2.png", ROOT / "assets" / "brand" / "cropmark-icon-v2.png")

    ICONS.mkdir(parents=True, exist_ok=True)
    for filename, size in [("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256), ("icon.png", 512)]:
        master.resize((size, size), Image.Resampling.LANCZOS).save(ICONS / filename)
    master.save(ICONS / "icon.ico", sizes=[(n, n) for n in (16, 24, 32, 48, 64, 128, 256)])
    master.save(ICONS / "icon.icns")

    # macOS template icons use alpha only: extract the bright mint symbol.
    symbol = Image.new("RGBA", source.size)
    symbol.putdata([
        (0, 0, 0, round(a * max(0, min(1, (min(r, g) - 65) / 45))))
        for r, g, b, a in source.getdata()
    ])
    symbol = symbol.crop(symbol.getbbox())
    symbol.thumbnail((28, 28), Image.Resampling.LANCZOS)
    tray = Image.new("RGBA", (32, 32))
    tray.alpha_composite(symbol, ((32 - symbol.width) // 2, (32 - symbol.height) // 2))
    tray.save(ICONS / "tray-template.png")
    print("Exported Cropmark v2 PNG, ICO, ICNS and macOS tray template.")


if __name__ == "__main__":
    main()
