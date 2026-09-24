// Export toolbar icon SVGs to 24px / 48px PNGs.
//
// Zero-dependency rasterizer: parses the restricted path subset used by the
// SVGs in this directory (M L H V C A Z, absolute coordinates only, optional
// per-path stroke-width / fill), flattens curves, then strokes by distance
// field (round caps/joins) and fills by even-odd scan. Writes RGBA PNGs via
// zlib into ../../src-tauri/icons/toolbar/.
//
// Usage: node assets/toolbar/export.mjs
import { readdirSync, readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import zlib from "node:zlib";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "..", "..", "src-tauri", "icons", "toolbar");
const GRID = 24;
const SIZES = [24, 48];

// ---------- path parsing ----------
function tokenizePath(d) {
  const tokens = d.match(/[MLHVCAZ]|-?\d*\.?\d+(?:e[-+]?\d+)?/gi);
  if (!tokens) return [];
  return tokens.map((t) => (/^[MLHVCAZ]$/i.test(t) ? t.toUpperCase() : Number(t)));
}

function parsePath(d) {
  const t = tokenizePath(d);
  const subpaths = [];
  let i = 0;
  let cur = [0, 0];
  let start = [0, 0];
  let pts = null;
  const num = () => {
    if (i >= t.length || typeof t[i] !== "number") throw new Error(`bad path data: ${d}`);
    return t[i++];
  };
  while (i < t.length) {
    const cmd = t[i++];
    if (typeof cmd === "number") throw new Error(`missing command in: ${d}`);
    switch (cmd) {
      case "M": {
        cur = [num(), num()];
        start = cur;
        if (pts && pts.length) subpaths.push(pts);
        pts = [cur];
        // subsequent implicit pairs are L
        while (typeof t[i] === "number") {
          cur = [num(), num()];
          pts.push(cur);
        }
        break;
      }
      case "L":
        while (typeof t[i] === "number") {
          cur = [num(), num()];
          pts.push(cur);
        }
        break;
      case "H":
        while (typeof t[i] === "number") {
          cur = [num(), cur[1]];
          pts.push(cur);
        }
        break;
      case "V":
        while (typeof t[i] === "number") {
          cur = [cur[0], num()];
          pts.push(cur);
        }
        break;
      case "C": {
        while (typeof t[i] === "number") {
          const c1 = [num(), num()];
          const c2 = [num(), num()];
          const end = [num(), num()];
          const p0 = cur;
          for (let s = 1; s <= 20; s++) {
            const u = s / 20;
            const a = (1 - u) ** 3;
            const b = 3 * (1 - u) ** 2 * u;
            const c = 3 * (1 - u) * u * u;
            const e = u ** 3;
            pts.push([
              a * p0[0] + b * c1[0] + c * c2[0] + e * end[0],
              a * p0[1] + b * c1[1] + c * c2[1] + e * end[1],
            ]);
          }
          cur = end;
        }
        break;
      }
      case "A": {
        while (typeof t[i] === "number") {
          const rx = num();
          const ry = num();
          const rot = (num() * Math.PI) / 180;
          const laf = num();
          const sf = num();
          const end = [num(), num()];
          for (const p of arcPoints(cur, end, rx, ry, rot, laf, sf)) pts.push(p);
          cur = end;
        }
        break;
      }
      case "Z": {
        if (pts && pts.length && (pts[pts.length - 1][0] !== start[0] || pts[pts.length - 1][1] !== start[1])) {
          pts.push([start[0], start[1]]);
        }
        break;
      }
      default:
        throw new Error(`unsupported command ${cmd} in: ${d}`);
    }
  }
  if (pts && pts.length) subpaths.push(pts);
  return subpaths;
}

// SVG endpoint-arc-parameterization -> polyline (F.6.5)
function arcPoints(p1, p2, rx, ry, phi, laf, sf) {
  rx = Math.abs(rx);
  ry = Math.abs(ry);
  if (rx === 0 || ry === 0) return [p2];
  const [x1, y1] = p1;
  const [x2, y2] = p2;
  const dx = (x1 - x2) / 2;
  const dy = (y1 - y2) / 2;
  const x1p = Math.cos(phi) * dx + Math.sin(phi) * dy;
  const y1p = -Math.sin(phi) * dx + Math.cos(phi) * dy;
  let lam = x1p ** 2 / rx ** 2 + y1p ** 2 / ry ** 2;
  if (lam > 1) {
    const s = Math.sqrt(lam);
    rx *= s;
    ry *= s;
  }
  const num = rx ** 2 * ry ** 2 - rx ** 2 * y1p ** 2 - ry ** 2 * x1p ** 2;
  const den = rx ** 2 * y1p ** 2 + ry ** 2 * x1p ** 2;
  const co = Math.sqrt(Math.max(0, num / den)) * (laf === sf ? -1 : 1);
  const cxp = (co * rx * y1p) / ry;
  const cyp = (-co * ry * x1p) / rx;
  const cx = Math.cos(phi) * cxp - Math.sin(phi) * cyp + (x1 + x2) / 2;
  const cy = Math.sin(phi) * cxp + Math.cos(phi) * cyp + (y1 + y2) / 2;
  const ang = (ux, uy, vx, vy) => {
    const dot = ux * vx + uy * vy;
    const len = Math.hypot(ux, uy) * Math.hypot(vx, vy);
    let a = Math.acos(Math.min(1, Math.max(-1, dot / len)));
    if (ux * vy - uy * vx < 0) a = -a;
    return a;
  };
  const th1 = ang(1, 0, (x1p - cxp) / rx, (y1p - cyp) / ry);
  let dth = ang((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
  if (!sf && dth > 0) dth -= 2 * Math.PI;
  if (sf && dth < 0) dth += 2 * Math.PI;
  const steps = Math.max(6, Math.ceil(Math.abs(dth) / (Math.PI / 18)));
  const out = [];
  for (let s = 1; s <= steps; s++) {
    const t = th1 + (dth * s) / steps;
    const xp = rx * Math.cos(t);
    const yp = ry * Math.sin(t);
    out.push([Math.cos(phi) * xp - Math.sin(phi) * yp + cx, Math.sin(phi) * xp + Math.cos(phi) * yp + cy]);
  }
  return out;
}

// ---------- SVG loading ----------
function loadSvg(file) {
  const text = readFileSync(join(here, file), "utf8");
  const svgSw = /<svg\b[^>]*\bstroke-width="([^"]+)"/.exec(text)?.[1] ?? "2";
  const paths = [];
  const re = /<path\b[^>]*\/?>(?:<\/path>)?/g;
  let m;
  while ((m = re.exec(text))) {
    const tag = m[0];
    const d = /d="([^"]+)"/.exec(tag)?.[1];
    if (!d) continue;
    const fill = /fill="([^"]+)"/.exec(tag)?.[1] ?? "none";
    const stroke = /stroke="([^"]+)"/.exec(tag)?.[1] ?? "#000";
    const sw = Number(/stroke-width="([^"]+)"/.exec(tag)?.[1] ?? svgSw);
    paths.push({
      subpaths: parsePath(d),
      fill: fill !== "none",
      stroke: stroke !== "none",
      sw,
    });
  }
  if (!paths.length) throw new Error(`no paths in ${file}`);
  return paths;
}

// ---------- rasterization ----------
// 平头：投影落在线段外就不算覆盖。圆头会在端点外再铺一圈灰边。
function distButt(px, py, ax, ay, bx, by) {
  const abx = bx - ax;
  const aby = by - ay;
  const len2 = abx * abx + aby * aby;
  if (len2 === 0) return Infinity;
  const t = ((px - ax) * abx + (py - ay) * aby) / len2;
  if (t < 0 || t > 1) return Infinity;
  const dx = px - (ax + t * abx);
  const dy = py - (ay + t * aby);
  return Math.hypot(dx, dy);
}

function inFill(px, py, subpaths) {
  // even-odd rule across all subpaths
  let inside = false;
  for (const pts of subpaths) {
    for (let i = 0, j = pts.length - 1; i < pts.length; j = i++) {
      const [xi, yi] = pts[i];
      const [xj, yj] = pts[j];
      if (yi > py !== yj > py && px < ((xj - xi) * (py - yi)) / (yj - yi) + xi) inside = !inside;
    }
  }
  return inside;
}

function coverage(px, py, path) {
  if (path.fill && inFill(px, py, path.subpaths)) return 1;
  if (!path.stroke) return 0;
  const r = path.sw / 2;
  const r2 = r * r;
  for (const pts of path.subpaths) {
    for (let i = 0; i + 1 < pts.length; i++) {
      if (distButt(px, py, pts[i][0], pts[i][1], pts[i + 1][0], pts[i + 1][1]) < r) return 1;
    }
    if (pts.length < 2) continue;
    const closed =
      pts[0][0] === pts[pts.length - 1][0] && pts[0][1] === pts[pts.length - 1][1];
    const from = closed ? 0 : 1;
    const to = pts.length - 1;
    for (let i = from; i < to; i++) {
      const dx = px - pts[i][0];
      const dy = py - pts[i][1];
      if (dx * dx + dy * dy < r2) return 1;
    }
  }
  return 0;
}

function render(paths, size) {
  const scale = size / GRID;
  const rgba = Buffer.alloc(size * size * 4);
  const SS = 8;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let acc = 0;
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const px = (x + (sx + 0.5) / SS) / scale;
          const py = (y + (sy + 0.5) / SS) / scale;
          let c = 0;
          for (const p of paths) c = Math.max(c, coverage(px, py, p));
          acc += c;
        }
      }
      const a = Math.round((acc / (SS * SS)) * 255);
      const o = (y * size + x) * 4;
      rgba[o] = 0;
      rgba[o + 1] = 0;
      rgba[o + 2] = 0;
      rgba[o + 3] = a;
    }
  }
  return rgba;
}

// ---------- PNG encoding ----------
const crcTable = (() => {
  const t = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c;
  }
  return t;
})();

function crc32(buf) {
  let c = -1;
  for (let i = 0; i < buf.length; i++) c = crcTable[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

function chunk(type, data) {
  const out = Buffer.alloc(8 + data.length + 4);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4);
  data.copy(out, 8);
  out.writeUInt32BE(crc32(Buffer.concat([Buffer.from(type), data])), 8 + data.length);
  return out;
}

function encodePng(rgba, size) {
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    rgba.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  const idat = zlib.deflateSync(raw, { level: 9 });
  return Buffer.concat([sig, chunk("IHDR", ihdr), chunk("IDAT", idat), chunk("IEND", Buffer.alloc(0))]);
}

// ---------- main ----------
mkdirSync(outDir, { recursive: true });
const svgs = readdirSync(here).filter((f) => f.endsWith(".svg")).sort();
if (svgs.length !== 20) throw new Error(`expected 20 SVGs, found ${svgs.length}`);
for (const file of svgs) {
  const name = file.replace(/\.svg$/, "");
  const paths = loadSvg(file);
  for (const size of SIZES) {
    const png = encodePng(render(paths, size), size);
    const out = join(outDir, `${name}-${size}.png`);
    writeFileSync(out, png);
  }
  console.log(`${file} -> ${name}-{24,48}.png`);
}
console.log(`done: ${svgs.length} icons x ${SIZES.length} sizes -> ${outDir}`);
