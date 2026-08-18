// warden 应用图标源图生成器(1024×1024 RGBA PNG,零依赖)。
// 设计:盾牌(守护)+ 心电线(进程监护),蓝色纵向渐变 + 白色心电。
// 产物作为 `cargo tauri icon` 的输入,生成 ico/icns/png 全套平台图标:
//   node scripts/gen_icon.mjs scripts/app-icon.png
//   cd desktop && cargo tauri icon ../scripts/app-icon.png
// 改设计只需调本文件的几何常量后重跑上面两条命令。
import { deflateSync } from 'node:zlib';
import { writeFileSync } from 'node:fs';

const SIZE = 1024; // 目标尺寸
const SS = 4;      // 超采样倍数(4×4 → 16 级边缘抗锯齿)
const N = SIZE * SS;

// ── 盾牌轮廓(1024 坐标系):顶点 → 右肩 → 直边 → 贝塞尔收拢到底尖,左侧镜像 ──
function shieldPolygon() {
  const pts = [];
  const P = (x, y) => pts.push([x * SS, y * SS]);
  const bez = (p0, p1, p2, p3) => {
    for (let i = 1; i <= 128; i++) {
      const t = i / 128, u = 1 - t;
      P(
        u * u * u * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t * t * t * p3[0],
        u * u * u * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t * t * t * p3[1]
      );
    }
  };
  P(512, 84);
  P(860, 236); P(860, 496);
  bez([860, 496], [860, 716], [698, 876], [512, 936]);
  bez([512, 936], [326, 876], [164, 716], [164, 496]);
  P(164, 236);
  return pts;
}

// 偶奇规则扫描线填充 → N×N 的 0/1 掩码
function fillPolygon(pts) {
  const mask = new Uint8Array(N * N);
  const edges = [];
  for (let i = 0; i < pts.length; i++) {
    const [x0, y0] = pts[i];
    const [x1, y1] = pts[(i + 1) % pts.length];
    if (y0 === y1) continue;
    edges.push({
      ymin: Math.min(y0, y1), ymax: Math.max(y0, y1),
      x0, y0, k: (x1 - x0) / (y1 - y0),
    });
  }
  const xs = [];
  for (let y = 0; y < N; y++) {
    const yc = y + 0.5;
    xs.length = 0;
    for (const e of edges) {
      if (yc >= e.ymin && yc < e.ymax) xs.push(e.x0 + e.k * (yc - e.y0));
    }
    if (xs.length < 2) continue;
    xs.sort((a, b) => a - b);
    const row = y * N;
    for (let p = 0; p + 1 < xs.length; p += 2) {
      const a = Math.max(0, Math.ceil(xs[p]));
      const b = Math.min(N - 1, Math.floor(xs[p + 1]));
      for (let x = a; x <= b; x++) mask[row + x] = 1;
    }
  }
  return mask;
}

// ── 心电线(白,圆头圆角,线宽 STROKE)─ 点到线段距离 ≤ 半宽即着色 ──
const PULSE = [
  [240, 512], [388, 512], [446, 392], [530, 640], [582, 512], [780, 512],
];
const STROKE = 66;

function markPulse() {
  const half = (STROKE / 2) * SS;
  const white = new Uint8Array(N * N);
  for (let i = 0; i + 1 < PULSE.length; i++) {
    const AX = PULSE[i][0] * SS, AY = PULSE[i][1] * SS;
    const BX = PULSE[i + 1][0] * SS, BY = PULSE[i + 1][1] * SS;
    const minX = Math.max(0, Math.floor(Math.min(AX, BX) - half));
    const maxX = Math.min(N - 1, Math.ceil(Math.max(AX, BX) + half));
    const minY = Math.max(0, Math.floor(Math.min(AY, BY) - half));
    const maxY = Math.min(N - 1, Math.ceil(Math.max(AY, BY) + half));
    const dx = BX - AX, dy = BY - AY;
    const len2 = dx * dx + dy * dy;
    for (let y = minY; y <= maxY; y++) {
      const yc = y + 0.5, row = y * N;
      for (let x = minX; x <= maxX; x++) {
        const xc = x + 0.5;
        let t = len2 ? ((xc - AX) * dx + (yc - AY) * dy) / len2 : 0;
        t = t < 0 ? 0 : t > 1 ? 1 : t;
        const ddx = xc - (AX + t * dx), ddy = yc - (AY + t * dy);
        if (ddx * ddx + ddy * ddy <= half * half) white[row + x] = 1;
      }
    }
  }
  return white;
}

// 纵向渐变 #6aa1ff → #2350c8,取值区间为盾牌顶(84)到底尖(936)
const TOP = [0x6a, 0xa1, 0xff], BOT = [0x23, 0x50, 0xc8];

function render(mask, white) {
  const out = Buffer.alloc(SIZE * SIZE * 4);
  const acc = new Float64Array(SIZE * SIZE * 4); // r,g,b,样本数
  for (let y = 0; y < N; y++) {
    const t = Math.max(0, Math.min(1, (y / SS - 84) / (936 - 84)));
    const r = TOP[0] + (BOT[0] - TOP[0]) * t;
    const g = TOP[1] + (BOT[1] - TOP[1]) * t;
    const b = TOP[2] + (BOT[2] - TOP[2]) * t;
    const ty = (y / SS) | 0;
    for (let x = 0; x < N; x++) {
      const i = y * N + x;
      if (!mask[i]) continue;
      const w = white[i];
      const o = (ty * SIZE + ((x / SS) | 0)) * 4;
      acc[o] += w ? 255 : r;
      acc[o + 1] += w ? 255 : g;
      acc[o + 2] += w ? 255 : b;
      acc[o + 3] += 1;
    }
  }
  for (let p = 0; p < SIZE * SIZE; p++) {
    const n = acc[p * 4 + 3];
    if (!n) continue;
    out[p * 4] = Math.round(acc[p * 4] / n);
    out[p * 4 + 1] = Math.round(acc[p * 4 + 1] / n);
    out[p * 4 + 2] = Math.round(acc[p * 4 + 2] / n);
    out[p * 4 + 3] = Math.round((n / (SS * SS)) * 255);
  }
  return out;
}

// ── PNG 编码(filter 0 + zlib,最小实现)──
function crc32(buf) {
  let c = ~0;
  for (let i = 0; i < buf.length; i++) {
    c ^= buf[i];
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

function encodePNG(rgba, w, h) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type: RGBA
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) {
    raw[y * (w * 4 + 1)] = 0; // filter: none
    rgba.copy(raw, y * (w * 4 + 1) + 1, y * w * 4, (y + 1) * w * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

const out = process.argv[2];
if (!out) {
  console.error('用法:node scripts/gen_icon.mjs <out.png>');
  process.exit(1);
}
const png = encodePNG(render(fillPolygon(shieldPolygon()), markPulse()), SIZE, SIZE);
writeFileSync(out, png);
console.log(`written ${out} (${png.length} bytes, ${SIZE}x${SIZE})`);
