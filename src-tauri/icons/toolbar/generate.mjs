// 工具条图标生成器(R5/T2):为注册表新增的 5 个标注工具生成 24/48 两档
// 黑线透明底 PNG,与既有 `icons/toolbar/*.png` 同风格(2px 描边、24px 网格)。
// 运行:`node src-tauri/icons/toolbar/generate.mjs`
//
// 仅用 Node 内置模块:zlib 压缩 + 手写 PNG chunk/CRC32,无外部依赖。
// 图标语义与前端 `src/icons.ts` 中同名 SVG 保持一致(聚光灯/放大镜/
// 对话气泡/贴纸/内容擦除)。

import { deflateSync } from "node:zlib";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const SAMPLES = 4; // 每像素每轴子采样:抗锯齿质量与生成速度的折中。

// ---------- PNG 编码(与 assets/stickers/generate.mjs 同构) ----------

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(bytes) {
  let c = 0xffffffff;
  for (const byte of bytes) {
    c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  }
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const head = Buffer.alloc(4);
  head.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const tail = Buffer.alloc(4);
  tail.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([head, body, tail]);
}

function encodePng(rgba, width, height) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc((width * 4 + 1) * height);
  for (let y = 0; y < height; y += 1) {
    raw[y * (width * 4 + 1)] = 0; // filter: none
    rgba.copy(raw, y * (width * 4 + 1) + 1, y * width * 4, (y + 1) * width * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ---------- 几何(黑线稿:返回 coverage 的形状测试) ----------

function segment(ax, ay, bx, by, width) {
  const half = width / 2;
  const dx = bx - ax;
  const dy = by - ay;
  const lengthSq = dx * dx + dy * dy || 1;
  return (x, y) => {
    let t = ((x - ax) * dx + (y - ay) * dy) / lengthSq;
    t = Math.max(0, Math.min(1, t));
    return Math.hypot(x - (ax + t * dx), y - (ay + t * dy)) <= half;
  };
}

function circleStroke(cx, cy, r, width) {
  const half = width / 2;
  return (x, y) => Math.abs(Math.hypot(x - cx, y - cy) - r) <= half;
}

function distToSegment(x, y, ax, ay, bx, by) {
  const dx = bx - ax;
  const dy = by - ay;
  const lengthSq = dx * dx + dy * dy || 1;
  let t = ((x - ax) * dx + (y - ay) * dy) / lengthSq;
  t = Math.max(0, Math.min(1, t));
  return Math.hypot(x - (ax + t * dx), y - (ay + t * dy));
}

/** 多边形边线描边(不填充):到任一边的距离 ≤ 半宽。 */
function polygonStroke(points, width) {
  const half = width / 2;
  return (x, y) => {
    for (let i = 0, j = points.length - 1; i < points.length; j = i, i += 1) {
      const [xi, yi] = points[i];
      const [xj, yj] = points[j];
      if (distToSegment(x, y, xi, yi, xj, yj) <= half) {
        return true;
      }
    }
    return false;
  };
}

function union(...tests) {
  return (x, y) => tests.some((test) => test(x, y));
}

// ---------- 24px 网格图标定义(48 = 同形放大 2 倍) ----------

/** 聚光灯:中心圆 + 八向射线。 */
function spotlight(w) {
  const tests = [circleStroke(12, 12, 4.6, w)];
  for (let i = 0; i < 8; i += 1) {
    const angle = (Math.PI / 4) * i;
    const inner = 6.6;
    const outer = 9.2;
    tests.push(
      segment(
        12 + Math.cos(angle) * inner,
        12 + Math.sin(angle) * inner,
        12 + Math.cos(angle) * outer,
        12 + Math.sin(angle) * outer,
        w,
      ),
    );
  }
  return union(...tests);
}

/** 放大镜:圆 + 斜柄。 */
function magnifier(w) {
  return union(
    circleStroke(10.2, 10.2, 5.7, w),
    segment(14.4, 14.4, 19.4, 19.4, w + 0.4),
  );
}

/** 对话气泡:圆角矩形 + 左下尾。 */
function bubble(w) {
  const x0 = 4.6;
  const y0 = 5.6;
  const x1 = 19.4;
  const y1 = 16.4;
  const r = 2.2;
  // 圆角矩形边:四段直线 + 四个 90° 圆弧。
  const tests = [
    segment(x0 + r, y0, x1 - r, y0, w),
    segment(x1, y0 + r, x1, y1 - r, w),
    segment(x0 + r, y1, x1 - r, y1, w),
    segment(x0, y0 + r, x0, y1 - r, w),
    circleStroke(x0 + r, y0 + r, r, w),
    circleStroke(x1 - r, y0 + r, r, w),
    circleStroke(x1 - r, y1 - r, r, w),
    circleStroke(x0 + r, y1 - r, r, w),
    // 尾巴:从底边伸出的小三角两边线(第三边贴住气泡底边,不重复画)。
    segment(x0 + 3.2, y1 - 0.4, x0 + 2.0, 20.2, w),
    segment(x0 + 2.0, 20.2, x0 + 6.6, y1 - 0.4, w),
  ];
  return union(...tests);
}

/** 贴纸:折角方形 + 折线。 */
function sticker(w) {
  return union(
    polygonStroke(
      [
        [4.6, 5.4],
        [19.4, 5.4],
        [19.4, 13.6],
        [14.0, 19.0],
        [4.6, 19.0],
      ],
      w,
    ),
    segment(19.4, 13.6, 14.0, 13.6, w),
    segment(14.0, 13.6, 14.0, 19.0, w),
  );
}

/** 内容擦除:斜置擦头 + 底线。 */
function erase(w) {
  return union(
    polygonStroke(
      [
        [5.2, 14.8],
        [13.0, 7.0],
        [16.4, 7.0],
        [19.8, 10.4],
        [12.0, 18.2],
        [8.6, 18.2],
      ],
      w,
    ),
    segment(8.4, 20.6, 19.6, 20.6, w),
  );
}

const ICONS = { spotlight, magnifier, bubble, sticker, erase };
const STROKE = 2.0;

// ---------- 渲染 ----------

function render(test, size) {
  const scale = size / 24;
  const rgba = Buffer.alloc(size * size * 4);
  const steps = [];
  for (let i = 0; i < SAMPLES; i += 1) {
    steps.push((i + 0.5) / SAMPLES);
  }
  const black = [0, 0, 0];
  for (let y = 0; y < size; y += 1) {
    for (let x = 0; x < size; x += 1) {
      let hits = 0;
      for (const sy of steps) {
        for (const sx of steps) {
          if (test((x + sx - 0.5) / scale, (y + sy - 0.5) / scale)) {
            hits += 1;
          }
        }
      }
      const coverage = hits / (SAMPLES * SAMPLES);
      if (coverage > 0) {
        const offset = (y * size + x) * 4;
        rgba[offset] = black[0];
        rgba[offset + 1] = black[1];
        rgba[offset + 2] = black[2];
        rgba[offset + 3] = Math.round(coverage * 255);
      }
    }
  }
  return rgba;
}

const here = dirname(fileURLToPath(import.meta.url));
for (const [id, shape] of Object.entries(ICONS)) {
  const test = shape(STROKE);
  for (const size of [24, 48]) {
    const file = join(here, `${id}-${size}.png`);
    writeFileSync(file, encodePng(render(test, size), size, size));
    process.stdout.write(`icon ${id}@${size}: ${file}\n`);
  }
}
