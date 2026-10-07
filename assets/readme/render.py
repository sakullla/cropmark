"""Render README artwork from synthetic content only; no screen or app access.

Requires Python 3 and Pillow. Run: python assets/readme/render.py
Uses Microsoft YaHei on Windows; elsewhere set CROPMARK_ART_FONT and
CROPMARK_ART_FONT_BOLD to local Chinese-capable font files. No downloads.
"""

from functools import lru_cache
import math
import os
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont


HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SCALE = 2
INK = "#123D3A"
MUTED = "#54716E"
TEAL = "#087F73"
MINT = "#62EAD1"
FONT = os.environ.get("CROPMARK_ART_FONT", "C:/Windows/Fonts/msyh.ttc")
BOLD = os.environ.get("CROPMARK_ART_FONT_BOLD", "C:/Windows/Fonts/msyhbd.ttc")


@lru_cache(maxsize=80)
def font(size, bold=False):
    return ImageFont.truetype(BOLD if bold else FONT, size * SCALE)


class Art:
    def __init__(self, height, dark=False):
        self.height = height
        self.im = Image.new("RGB", (1600 * SCALE, height * SCALE))
        self.d = ImageDraw.Draw(self.im)
        start, end = ((8, 43, 42), (20, 79, 72)) if dark else ((240, 248, 246), (225, 241, 237))
        for y in range(height * SCALE):
            t = y / (height * SCALE)
            color = tuple(round(a + (b - a) * t) for a, b in zip(start, end))
            self.d.line((0, y, 1600 * SCALE, y), fill=color)

    def box(self, xy, fill, radius=18, outline=None, width=1):
        self.d.rounded_rectangle(tuple(round(v * SCALE) for v in xy), radius * SCALE,
                                 fill=fill, outline=outline, width=width * SCALE)

    def text(self, x, y, value, size=24, color=INK, bold=False):
        self.d.text((x * SCALE, y * SCALE), value, font=font(size, bold), fill=color, anchor="lt")

    def line(self, points, color, width=2):
        self.d.line([(round(x * SCALE), round(y * SCALE)) for x, y in points],
                    fill=color, width=width * SCALE, joint="curve")

    def arrow(self, start, end, color=TEAL, width=4):
        self.line([start, end], color, width)
        angle = math.atan2(end[1] - start[1], end[0] - start[0])
        for delta in (-0.55, 0.55):
            self.line([end, (end[0] - 19 * math.cos(angle + delta),
                             end[1] - 19 * math.sin(angle + delta))], color, width)

    def shadow(self, xy, radius=24):
        layer = Image.new("RGBA", self.im.size)
        ImageDraw.Draw(layer).rounded_rectangle(
            tuple((v + (10 if i % 2 else 0)) * SCALE for i, v in enumerate(xy)),
            radius * SCALE, fill=(4, 44, 40, 35))
        self.im.paste(layer.filter(ImageFilter.GaussianBlur(18 * SCALE)), (0, 0),
                      layer.filter(ImageFilter.GaussianBlur(18 * SCALE)))

    def icon(self, x, y, size=64):
        with Image.open(ROOT / "src-tauri/icons/v2/icon.png") as source:
            icon = source.convert("RGBA").resize((size * SCALE, size * SCALE), Image.Resampling.LANCZOS)
        self.im.paste(icon, (x * SCALE, y * SCALE), icon)

    def pill(self, x, y, label, dark=False, size=21):
        width = round(self.d.textlength(label, font=font(size)) / SCALE) + 36
        self.box((x, y, x + width, y + 44), "#205B53" if dark else "#DAF2EA", radius=22)
        self.text(x + 18, y + 10, label, size, "#BDF8E8" if dark else TEAL)
        return width

    def footer(self, dark=False):
        color = "#A5CCC2" if dark else MUTED
        self.text(64, self.height - 45, "CROPMARK  /  开源 · 离线 · 跨平台", 18, color)
        self.text(1120, self.height - 45, "功能示意 · 虚构内容 · 非实机截图", 18, color)

    def save(self, name):
        self.im.resize((1600, self.height), Image.Resampling.LANCZOS).save(HERE / name, optimize=True)


def note(a, xy, compact=False):
    x, y, right, bottom = xy
    a.shadow(xy)
    a.box(xy, "#FFFFFF", 20)
    a.pill(x + 28, y + 26, "DEMO / 示例内容", size=17)
    a.text(x + 28, y + 95, "把想法，清楚地分享。", 31 if compact else 39, bold=True)
    a.text(x + 28, y + 158, "Capture ideas. Keep them local.", 21 if compact else 25, MUTED)
    for i, length in enumerate((0.83, 0.65, 0.74)):
        yy = y + 226 + i * 32
        if yy + 12 < bottom - 22:
            a.box((x + 28, yy, x + 28 + (right - x - 56) * length, yy + 10), "#E4EFEB", 5)


def hero():
    a = Art(880, dark=True)
    a.icon(62, 60, 74)
    a.text(154, 77, "Cropmark", 39, "#F2FFF9", True)
    a.pill(64, 178, "OPEN SOURCE  /  100% OFFLINE", True, 18)
    a.text(64, 266, "截图之后，", 66, "#F3FFF9", True)
    a.text(64, 357, "还有无限可能。", 66, MINT, True)
    a.text(68, 477, "截图 · 标注 · 贴图 · 长截图 · 录屏", 25, "#D4EDE5")
    a.text(68, 525, "中文 / English OCR，全程离线。", 25, "#D4EDE5")
    a.text(68, 642, "Windows  /  macOS  /  Linux", 24, "#F3FFF9", True)
    a.text(68, 691, "无账号   ·   无云端   ·   开源免费", 22, "#A5CCC2")

    a.box((824, 162, 1518, 659), "#286A5E", 32)
    note(a, (876, 208, 1466, 595), True)
    a.box((897, 291, 1271, 348), None, 8, TEAL, 3)
    a.arrow((1399, 401), (1285, 332), TEAL)
    for x, y, dx, dy in [(847, 183, 1, 1), (1495, 183, -1, 1),
                          (847, 636, 1, -1), (1495, 636, -1, -1)]:
        a.line([(x, y + dy * 30), (x, y), (x + dx * 30, y)], MINT, 5)
    a.shadow((947, 626, 1474, 741))
    a.box((947, 626, 1474, 741), "#F2FFF9", 20)
    a.text(973, 647, "从截取到分享", 25, bold=True)
    a.text(973, 691, "标注重点   →   提取文字   →   导出", 21, MUTED)
    a.footer(True)
    a.save("hero.png")


def annotate():
    a = Art(960)
    a.text(64, 49, "01 / CAPTURE → ANNOTATE → SHARE", 18, TEAL, True)
    a.text(64, 97, "截取灵感，标出重点。", 49, bold=True)
    a.text(66, 176, "用箭头、文字与高亮讲清楚，再给分享加一点设计感。", 24, MUTED)
    a.shadow((64, 249, 1110, 858))
    a.box((64, 249, 1110, 858), "#FAFCFB", 22)
    a.icon(82, 262, 34)
    a.text(125, 268, "Cropmark", 22, bold=True)
    a.text(825, 270, "预览与标注 · 功能示意", 18, MUTED)
    a.line([(64, 310), (1110, 310)], "#DCE8E3")
    names = ["rect", "ellipse", "arrow", "pen", "highlighter", "text", "number", "mosaic"]
    for i, name in enumerate(names):
        x = 83 + i * 57
        if name == "arrow":
            a.box((x - 4, 324, x + 40, 368), "#D7F2E9", 9)
        with Image.open(ROOT / f"src-tauri/icons/toolbar/{name}-48.png") as source:
            icon = source.convert("RGBA").resize((30 * SCALE, 30 * SCALE), Image.Resampling.LANCZOS)
        a.im.paste(icon, (x * SCALE, 331 * SCALE), icon)
    for x, label in [(590, "取字"), (680, "贴图"), (770, "美化"), (860, "保存")]:
        a.text(x, 337, label, 21, MUTED)
    a.box((966, 324, 1088, 368), TEAL, 10)
    a.text(1002, 334, "复制", 21, "#FFFFFF")
    a.box((88, 389, 1086, 832), "#CDE8E0", 16)
    note(a, (147, 424, 1024, 795))
    a.box((171, 510, 643, 574), None, 8, TEAL, 3)
    a.box((176, 580, 638, 617), "#FBF0B1", 4)
    a.text(175, 582, "Capture ideas. Keep them local.", 25, INK)
    a.arrow((870, 666), (667, 549), "#E17951", 5)
    a.pill(792, 684, "分享这个想法", size=20)
    features = [(281, "01", "标出重点", "箭头 / 高亮 / 文字"),
                (455, "02", "一键美化", "留白 / 圆角 / 阴影"),
                (629, "03", "随手分享", "复制 / PNG / JPEG / WebP")]
    for y, num, title, subtitle in features:
        a.pill(1154, y, num, size=18)
        a.text(1156, y + 61, title, 30, bold=True)
        a.text(1156, y + 109, subtitle, 21, MUTED)
    a.footer()
    a.save("annotation.png")


def ocr():
    a = Art(850)
    a.text(64, 49, "02 / IMAGE → TEXT", 18, TEAL, True)
    a.text(64, 97, "图片里的文字，选中就能带走。", 49, bold=True)
    a.text(66, 177, "内置中文与英文 OCR 模型，断网也能把截图变成可复制的文字。", 24, MUTED)
    for xy in [(64, 262, 730, 653), (870, 262, 1536, 653)]:
        a.shadow(xy)
        a.box(xy, "#FFFFFF", 22)
    a.pill(95, 291, "截图中的文字", size=19)
    a.text(96, 375, "灵感便签", 32, bold=True)
    a.box((92, 446, 537, 489), "#D9F2E9", 5, "#80CBB7")
    a.text(102, 454, "让每一次分享都清晰一点。", 28)
    a.box((92, 509, 642, 552), "#D9F2E9", 5, "#80CBB7")
    a.text(102, 518, "Good ideas deserve a clear picture.", 25)
    a.text(96, 594, "按坐标选择文字 · 示例选区", 19, MUTED)
    a.arrow((765, 456), (833, 456), TEAL, 4)
    a.pill(903, 291, "可复制文本 · 示意", size=19)
    a.text(906, 380, "让每一次分享都清晰一点。", 28)
    a.text(906, 442, "Good ideas deserve a clear picture.", 25)
    a.line([(905, 526), (1495, 526)], "#E0EBE6")
    a.text(906, 572, "识别与复制都在本机完成", 24, TEAL, True)
    x = 64
    for label in ["中文 + English", "模型随安装包提供", "无需账号", "无需上传图片"]:
        x += a.pill(x, 707, label, size=22) + 18
    a.footer()
    a.save("offline-ocr.png")


if __name__ == "__main__":
    hero()
    annotate()
    ocr()
