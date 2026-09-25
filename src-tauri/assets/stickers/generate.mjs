// 贴纸素材生成器(R5):随包自有素材,不引入第三方版权图。
// 运行:`node src-tauri/assets/stickers/generate.mjs`
// 输出与脚本同目录的 128×128 RGBA PNG(白色描边 + 纯色主体),供
// `annotate/stickers.rs`(include_bytes! 编译期嵌入)与前端标注画布共用。
//
// 仅用 Node 内置模块:zlib 压缩 + 手写 PNG chunk/CRC32,无外部依赖。

import { deflateSync } from "node:zlib";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const SIZE = 128;
const SAMPLES = 4; // 每像素每轴子采样:抗锯齿质量与生成速度的折中。

// ---------- PNG 编码 ----------

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

// ---------- 几何 ----------

function circle(cx, cy, r) {
  return (x, y) => Math.hypot(x - cx, y - cy) <= r;
}

function polygon(points) {
  return (x, y) => {
    let inside = false;
    for (let i = 0, j = points.length - 1; i < points.length; j = i, i += 1) {
      const [xi, yi] = points[i];
      const [xj, yj] = points[j];
      if (yi > y !== yj > y && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi) {
        inside = !inside;
      }
    }
    return inside;
  };
}

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

function arc(cx, cy, r, width, start, end) {
  return (x, y) => {
    const dx = x - cx;
    const dy = y - cy;
    if (Math.abs(Math.hypot(dx, dy) - r) > width / 2) {
      return false;
    }
    let angle = Math.atan2(dy, dx);
    if (angle < 0) {
      angle += Math.PI * 2;
    }
    return angle >= start && angle <= end;
  };
}

function starPoints(cx, cy, outer, inner, points, rotation = -Math.PI / 2) {
  const list = [];
  for (let i = 0; i < points * 2; i += 1) {
    const radius = i % 2 === 0 ? outer : inner;
    const angle = rotation + (Math.PI * i) / points;
    list.push([cx + Math.cos(angle) * radius, cy + Math.sin(angle) * radius]);
  }
  return list;
}

function heartPoints(cx, cy, size) {
  const points = [];
  for (let i = 0; i < 64; i += 1) {
    const t = (i / 64) * Math.PI * 2;
    const x = 16 * Math.sin(t) ** 3;
    const y = 13 * Math.cos(t) - 5 * Math.cos(2 * t) - 2 * Math.cos(3 * t) - Math.cos(4 * t);
    points.push([cx + (x / 18) * size, cy - (y / 18) * size]);
  }
  return points;
}

// ---------- 绘制 ----------

const WHITE = [255, 255, 255, 255];

function shape(color, test) {
  return { color, test };
}

function blend(dst, offset, color, coverage) {
  if (coverage <= 0) {
    return;
  }
  const src = (color[3] / 255) * coverage;
  if (src <= 0) {
    return;
  }
  for (let c = 0; c < 3; c += 1) {
    dst[offset + c] = Math.round(dst[offset + c] * (1 - src) + color[c] * src);
  }
  const outA = src + (dst[offset + 3] / 255) * (1 - src);
  dst[offset + 3] = Math.round(outA * 255);
}

function render(shapes) {
  const rgba = Buffer.alloc(SIZE * SIZE * 4);
  const steps = [];
  for (let i = 0; i < SAMPLES; i += 1) {
    steps.push((i + 0.5) / SAMPLES);
  }
  for (let y = 0; y < SIZE; y += 1) {
    for (let x = 0; x < SIZE; x += 1) {
      const offset = (y * SIZE + x) * 4;
      for (const { color, test } of shapes) {
        let hits = 0;
        for (const sy of steps) {
          for (const sx of steps) {
            if (test(x + sx - 0.5, y + sy - 0.5)) {
              hits += 1;
            }
          }
        }
        blend(rgba, offset, color, hits / (SAMPLES * SAMPLES));
      }
    }
  }
  return rgba;
}

const AMBER = [245, 158, 11, 255];
const ROSE = [225, 29, 72, 255];
const EMERALD = [16, 185, 129, 255];
const SKY = [37, 99, 235, 255];
const ORANGE = [249, 115, 22, 255];
const YELLOW = [250, 204, 21, 255];
const INK = [17, 24, 39, 255];

const stickers = {
  // 星星:白色描边 + 琥珀主体。
  star: render([
    shape(WHITE, polygon(starPoints(64, 64, 54, 26, 5))),
    shape(AMBER, polygon(starPoints(64, 64, 50, 22, 5))),
  ]),
  // 爱心。
  heart: render([
    shape(WHITE, polygon(heartPoints(64, 62, 52))),
    shape(ROSE, polygon(heartPoints(64, 62, 46))),
  ]),
  // 对勾徽章。
  check: render([
    shape(WHITE, circle(64, 64, 56)),
    shape(EMERALD, circle(64, 64, 50)),
    shape(WHITE, segment(40, 66, 56, 82, 14)),
    shape(WHITE, segment(56, 82, 90, 44, 14)),
  ]),
  // 叉号徽章。
  cross: render([
    shape(WHITE, circle(64, 64, 56)),
    shape(ROSE, circle(64, 64, 50)),
    shape(WHITE, segment(44, 44, 84, 84, 14)),
    shape(WHITE, segment(84, 44, 44, 84, 14)),
  ]),
  // 感叹号徽章。
  exclaim: render([
    shape(WHITE, circle(64, 64, 56)),
    shape(ORANGE, circle(64, 64, 50)),
    shape(WHITE, segment(64, 34, 64, 70, 16)),
    shape(WHITE, circle(64, 88, 9)),
  ]),
  // 笑脸徽章。
  smile: render([
    shape(WHITE, circle(64, 64, 56)),
    shape(YELLOW, circle(64, 64, 50)),
    shape(INK, circle(47, 52, 7)),
    shape(INK, circle(81, 52, 7)),
    // 下半弧:π*0.15 到 π*0.85,即从左下到右下的笑弧。
    shape(INK, arc(64, 64, 30, 7, Math.PI * 0.15, Math.PI * 0.85)),
  ]),
};

const here = dirname(fileURLToPath(import.meta.url));
for (const [id, pixels] of Object.entries(stickers)) {
  const file = join(here, `${id}.png`);
  writeFileSync(file, encodePng(pixels, SIZE, SIZE));
  process.stdout.write(`sticker ${id}: ${file}\n`);
}
