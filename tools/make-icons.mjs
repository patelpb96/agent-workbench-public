// Draws the app icons (a dark shale-gray tile with a blocky "AW") and writes them into src-tauri/icons:
//   icon.ico / icon.png      the normal build
//   icon-dev.ico / icon-dev.png   the development (debug) build: the same tile with amber letters and an amber frame,
//                                  so the two can be told apart in the taskbar and the Alt+Tab list.
// No dependencies: the shapes are polygons rasterised here with 4x4 supersampling at every size, and the files are
// written as PNG-compressed ICO entries. Run `node tools/make-icons.mjs`; `--preview DIR` also writes a 256 px PNG of each
// into DIR. The geometry is shared with the in-app brand mark (frontend/src/brand.ts), keep them in step.
import { deflateSync, crc32 } from 'node:zlib';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const SIZES = [16, 24, 32, 48, 64, 128, 256];

/** The letters, in a 256-unit tile. Each letter is a list of convex polygons (their union is the letter). */
export function logoPolygons(boost = 1) {
  const t = 21 * boost; // stroke thickness
  const h = 108;
  const top = (256 - h) / 2;
  // A: an inverted V with a flat apex and a crossbar.
  const aw = 84;
  const gap = 14;
  const ww = 108;
  const total = aw + gap + ww;
  const x0 = (256 - total) / 2;
  const A = [
    [[0, h], [t, h], [aw / 2 + t / 2, 0], [aw / 2 - t / 2, 0]],
    [[aw, h], [aw - t, h], [aw / 2 - t / 2, 0], [aw / 2 + t / 2, 0]],
    [[aw * 0.27, h * 0.56], [aw * 0.73, h * 0.56], [aw * 0.73, h * 0.56 + t * 0.78], [aw * 0.27, h * 0.56 + t * 0.78]],
  ];
  // W: four slanted strokes of constant horizontal thickness, the middle peak lower than the outer tops.
  const P = [[t / 2, 0], [ww / 4, h], [ww / 2, h * 0.3], [(3 * ww) / 4, h], [ww - t / 2, 0]];
  const W = [];
  for (let i = 0; i < 4; i++) {
    const [a, b] = [P[i], P[i + 1]];
    W.push([[a[0] - t / 2, a[1]], [a[0] + t / 2, a[1]], [b[0] + t / 2, b[1]], [b[0] - t / 2, b[1]]]);
  }
  const place = (polys, dx) => polys.map(poly => poly.map(([x, y]) => [x + dx, y + top]));
  return [...place(A, x0), ...place(W, x0 + aw + gap)];
}

function inside(poly, x, y) {
  // Convex polygon: all cross products share a sign (either winding).
  let sign = 0;
  for (let i = 0; i < poly.length; i++) {
    const [ax, ay] = poly[i];
    const [bx, by] = poly[(i + 1) % poly.length];
    const c = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
    if (c === 0) continue;
    const s = c > 0 ? 1 : -1;
    if (sign === 0) sign = s;
    else if (s !== sign) return false;
  }
  return true;
}

const hex = h => [parseInt(h.slice(1, 3), 16), parseInt(h.slice(3, 5), 16), parseInt(h.slice(5, 7), 16)];

const THEMES = {
  normal: { bg: hex('#2b3036'), edge: hex('#414952'), ink: hex('#e9ecef') },
  dev: { bg: hex('#2b3036'), edge: hex('#e8a33d'), ink: hex('#e8a33d') },
};

function render(size, theme) {
  const { bg, edge, ink } = THEMES[theme];
  const boost = size <= 24 ? 1.4 : size <= 32 ? 1.15 : 1;
  const polys = logoPolygons(boost);
  const scale = 256 / size;
  const border = Math.max(1, Math.round(size * 0.04)); // frame thickness in pixels
  const SS = 4;
  const rgba = Buffer.alloc(size * size * 4);
  for (let py = 0; py < size; py++) {
    for (let px = 0; px < size; px++) {
      let r = 0, g = 0, b = 0;
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const fx = px + (sx + 0.5) / SS;
          const fy = py + (sy + 0.5) / SS;
          const onFrame = fx < border || fy < border || fx > size - border || fy > size - border;
          const ux = fx * scale;
          const uy = fy * scale;
          const onLetter = polys.some(poly => inside(poly, ux, uy));
          const c = onLetter ? ink : onFrame ? edge : bg;
          r += c[0]; g += c[1]; b += c[2];
        }
      }
      const n = SS * SS;
      const o = (py * size + px) * 4;
      rgba[o] = Math.round(r / n); rgba[o + 1] = Math.round(g / n); rgba[o + 2] = Math.round(b / n); rgba[o + 3] = 255;
    }
  }
  return rgba;
}

function png(size, rgba) {
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    rgba.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  const chunk = (type, data) => {
    const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
    const out = Buffer.alloc(8 + data.length + 4);
    out.writeUInt32BE(data.length, 0);
    body.copy(out, 4);
    out.writeUInt32BE(crc32(body), 8 + data.length);
    return out;
  };
  const head = Buffer.alloc(13);
  head.writeUInt32BE(size, 0); head.writeUInt32BE(size, 4);
  head[8] = 8; head[9] = 6; // 8-bit RGBA
  return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk('IHDR', head), chunk('IDAT', deflateSync(raw, { level: 9 })), chunk('IEND', Buffer.alloc(0))]);
}

function ico(images) {
  const head = Buffer.alloc(6 + 16 * images.length);
  head.writeUInt16LE(0, 0); head.writeUInt16LE(1, 2); head.writeUInt16LE(images.length, 4);
  let offset = head.length;
  images.forEach(({ size, data }, i) => {
    const e = 6 + 16 * i;
    head[e] = size === 256 ? 0 : size; head[e + 1] = size === 256 ? 0 : size;
    head[e + 2] = 0; head[e + 3] = 0;
    head.writeUInt16LE(1, e + 4); head.writeUInt16LE(32, e + 6);
    head.writeUInt32LE(data.length, e + 8); head.writeUInt32LE(offset, e + 12);
    offset += data.length;
  });
  return Buffer.concat([head, ...images.map(i => i.data)]);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const dir = join(root, 'src-tauri', 'icons');
  mkdirSync(dir, { recursive: true });
  const previewAt = process.argv.indexOf('--preview');
  const preview = previewAt > 0 ? process.argv[previewAt + 1] : null;
  for (const [theme, name] of [['normal', 'icon'], ['dev', 'icon-dev']]) {
    const images = SIZES.map(size => ({ size, data: png(size, render(size, theme)) }));
    writeFileSync(join(dir, `${name}.ico`), ico(images));
    writeFileSync(join(dir, `${name}.png`), images.at(-1).data);
    if (preview) {
      mkdirSync(preview, { recursive: true });
      for (const size of [16, 32, 256]) writeFileSync(join(preview, `${name}-${size}.png`), png(size, render(size, theme)));
    }
    console.log(`wrote ${name}.ico (${SIZES.join(', ')}) and ${name}.png`);
  }
}
