import { useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { v4 as uuidv4 } from 'uuid';
import { useEditorStore } from '../store/useEditorStore';
import { useLibraryStore } from '../store/useLibraryStore';
import { useSettingsStore } from '../store/useSettingsStore';
import { useUIStore } from '../store/useUIStore';
import { ImageFile, ImageFlag, Invokes } from '../components/ui/AppProperties';
import {
  Adjustments,
  COLOR_LABELS,
  INITIAL_ADJUSTMENTS,
  INITIAL_MASK_ADJUSTMENTS,
  INITIAL_MASK_CONTAINER,
  MaskContainer,
  PasteMode,
  normalizeLoadedAdjustments,
} from '../utils/adjustments';
import { createSubMask } from '../utils/maskUtils';
import { FACE_PARTS, Mask, SubMask } from '../components/panel/right/Masks';
import { computeSortedLibrary } from './useSortedLibrary';
import { debouncedSave, debouncedSetHistory, getOriginalAdjustments, useEditorActions, withRotation } from './useEditorActions';
import { calculateCenteredCrop } from '../utils/cropUtils';
import { aiFailures, getTransformAdjustments, useAiMasking } from './useAiMasking';
import { Grade, Knob, Look, Mix, Targets, coherent, describeLook, lookNumbers, lookOf, spreadOf, targetFor } from './mcpLook';
import { adviceFor, describePalette, paletteNumbers, paletteOf } from './mcpPalette';

export interface McpHandlers {
  handleImageSelect: (path: string, openInEditor?: boolean) => Promise<void> | void;
  handleSelectSubfolder: (path: string | null, isNewRoot?: boolean) => Promise<void> | void;
  executeDelete: (paths: string[], options?: { includeAssociated: boolean }) => Promise<void>;
  handleCopyAdjustments: (path?: string) => Promise<void>;
  handlePasteAdjustments: (paths?: string[]) => void;
  handleResetAdjustments: (paths?: string[]) => void;
  handleZoomChange: (zoom: number, fit?: boolean) => void;
}

type Args = Record<string, any>;
type Request = { id: string; op: string; call?: string; args: Args };

// What a running request can do beyond returning: say how far it got (with the results so far) and
// notice that the agent asked it to stop.
type JobCtx = {
  report: (progress: Record<string, unknown>, partial?: unknown) => void;
  stopped: () => boolean;
};

// Written to %APPDATA%\RapidRAW-MCP\bridge.log next to the server's own lines.
const note = (text: string) => {
  void invoke('mcp_log', { text }).catch(() => {});
};

// A timer that fires much later than asked means the window is being throttled (hidden or minimized).
const sleep = (ms: number) =>
  new Promise<void>((resolve) => {
    const started = performance.now();
    setTimeout(() => {
      const late = performance.now() - started - ms;
      if (late > 1500) note(`timer ${ms} ms fired ${Math.round(late)} ms late (window ${document.visibilityState})`);
      resolve();
    }, ms);
  });
const norm = (p: string) => p.replace(/\//g, '\\').toLowerCase();
const MAX_PAGE = 500;
const MAX_REPLY_BASE64 = 2_900_000;
const FORBIDDEN_PATCH_KEYS = new Set(['masks', 'aiPatches']);
// Default mask names of earlier versions: a rerun still replaces masks made under the old name.
const LEGACY_MASK_NAMES: Record<string, string> = { 'Wedding dress': 'Vestido novia', 'Light skin': 'Pieles claras' };

const fail = (message: string): never => {
  throw new Error(message);
};

let ROOTS: string[] = [];

// Relative to the first root when inside it, otherwise absolute; always forward slashes.
const trimSlash = (p: string) => norm(p).replace(/\\+$/, '');
const relPath = (root: string, p: string) => {
  const r = trimSlash(root);
  const n = trimSlash(p);
  if (n === r) return '.';
  if (n.startsWith(r + '\\')) return p.slice(r.length + 1).replace(/\\/g, '/');
  return p.replace(/\\/g, '/');
};
const isRoot = (p: string) => ROOTS.some((r) => trimSlash(r) === trimSlash(p));

const colorOf = (image: ImageFile) => image.tags?.find((t) => t.startsWith('color:'))?.substring(6) ?? null;

const revisionOf = (adjustments: unknown) => {
  const text = JSON.stringify(adjustments);
  let hash = 5381;
  for (let i = 0; i < text.length; i++) hash = ((hash * 33) ^ text.charCodeAt(i)) >>> 0;
  return hash.toString(16).padStart(8, '0');
};

const isPlainObject = (v: unknown): v is Record<string, any> => !!v && typeof v === 'object' && !Array.isArray(v);

type Saved = { saved: boolean; save_problem?: string };

// Editor slider ranges; set_adjustments keeps values inside them and says when it had to.
const ADJUSTMENT_RANGES: Record<string, [number, number]> = {
  exposure: [-5, 5], brightness: [-100, 100], contrast: [-100, 100], highlights: [-100, 100], shadows: [-100, 100],
  whites: [-100, 100], blacks: [-100, 100], saturation: [-100, 100], vibrance: [-100, 100], clarity: [-100, 100],
  structure: [-100, 100], dehaze: [-100, 100], temperature: [-100, 100], tint: [-100, 100], hue: [-100, 100],
  sharpness: [0, 100], lumaNoiseReduction: [0, 100], colorNoiseReduction: [0, 100], grainAmount: [0, 100],
  vignetteAmount: [-100, 100], rotation: [-45, 45],
};

const sameIds = (a: Array<{ id?: string }>, b: Array<{ id?: string }>) =>
  a.length === b.length && a.every((m, i) => m.id === b[i].id);

const deepMerge = (base: any, patch: any): any => {
  if (!isPlainObject(base) || !isPlainObject(patch)) return patch;
  const out: Record<string, any> = { ...base };
  for (const [k, v] of Object.entries(patch)) out[k] = isPlainObject(v) && isPlainObject(base[k]) ? deepMerge(base[k], v) : v;
  return out;
};

const isSubset = (patch: any, actual: any): boolean => {
  if (isPlainObject(patch)) return isPlainObject(actual) && Object.entries(patch).every(([k, v]) => isSubset(v, actual[k]));
  if (typeof patch === 'number' && typeof actual === 'number') return Math.abs(patch - actual) < 1e-6;
  return JSON.stringify(patch) === JSON.stringify(actual);
};

// A painted mask can hold thousands of points; by default report one line per stroke instead.
const describeLines = (lines: any[]) =>
  lines.map((line, index) => {
    const points: Array<{ x: number; y: number }> = Array.isArray(line?.points) ? line.points : [];
    const xs = points.map((p) => p.x);
    const ys = points.map((p) => p.y);
    return {
      index,
      tool: line?.tool,
      brush_size: line?.brushSize,
      feather: line?.feather,
      points: points.length,
      bbox: points.length
        ? { x1: Math.round(Math.min(...xs)), y1: Math.round(Math.min(...ys)), x2: Math.round(Math.max(...xs)), y2: Math.round(Math.max(...ys)) }
        : null,
    };
  });

// Drop AI bitmaps so mask descriptions stay small and are never echoed back as edits.
const describeSubMask = (sm: SubMask, full = false) => {
  const { maskDataBase64, ...params } = sm.parameters || {};
  const parameters: any = !full && Array.isArray(params.lines) ? { ...params, lines: describeLines(params.lines) } : params;
  return {
    id: sm.id,
    type: sm.type,
    mode: sm.mode,
    invert: sm.invert,
    opacity: sm.opacity,
    visible: sm.visible,
    parameters,
    has_mask_data: !!maskDataBase64,
  };
};

const describeMask = (c: MaskContainer, full = false) => {
  const changed = Object.keys(c.adjustments).filter(
    (k) => k !== 'activeTools' && JSON.stringify(c.adjustments[k]) !== JSON.stringify((INITIAL_MASK_ADJUSTMENTS as any)[k]),
  );
  return {
    id: c.id,
    name: c.name,
    visible: c.visible,
    invert: c.invert,
    opacity: c.opacity,
    sub_masks: c.subMasks.map((sm) => describeSubMask(sm, full)),
    adjustments: Object.fromEntries(changed.map((k) => [k, c.adjustments[k]])),
  };
};

const blobToBase64 = (blob: Blob) =>
  new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result).split(',')[1] ?? '');
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(blob);
  });

const bytesToBitmap = (bytes: ArrayBuffer, type: string) => createImageBitmap(new Blob([bytes], { type }));

const canvasJpeg = async (canvas: HTMLCanvasElement) => {
  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, 'image/jpeg', 0.9));
  if (!blob) fail('Could not encode the preview');
  const data = await blobToBase64(blob!);
  if (data.length > MAX_REPLY_BASE64) fail('Image exceeds the bridge limit; request a smaller max_dimension or region');
  return data;
};

const canvasFrom = (width: number, height: number) => {
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.round(width));
  canvas.height = Math.max(1, Math.round(height));
  const ctx = canvas.getContext('2d');
  if (!ctx) fail('Canvas unavailable');
  return { canvas, ctx: ctx! };
};

type Stats = {
  mean_rgb: number[];
  luminance: { mean: number; p5: number; p50: number; p95: number };
  clipped_shadows_pct: number;
  clipped_highlights_pct: number;
  saturation_pct: number;
  warm_minus_cool: number;
  note: string;
};

// Numeric description of what an image looks like, for agents that cannot see pictures.
// `weights` (0..1 per pixel) restricts the statistics to a mask; omitted means the whole frame.
const statsOf = (canvas: HTMLCanvasElement, weights?: Float32Array | null, invert = false): Stats | null => {
  const ctx = canvas.getContext('2d');
  if (!ctx) return null;
  const { data } = ctx.getImageData(0, 0, canvas.width, canvas.height);
  const hist = new Float64Array(256);
  let total = 0;
  let r = 0, g = 0, b = 0, sat = 0;
  for (let i = 0, px = 0; i < data.length; i += 4, px++) {
    const w = weights ? (invert ? 1 - weights[px] : weights[px]) : 1;
    if (w <= 0.01) continue;
    const R = data[i], G = data[i + 1], B = data[i + 2];
    const y = Math.round(0.2126 * R + 0.7152 * G + 0.0722 * B);
    hist[y] += w;
    total += w;
    r += R * w; g += G * w; b += B * w;
    const mx = Math.max(R, G, B);
    sat += (mx === 0 ? 0 : (mx - Math.min(R, G, B)) / mx) * w;
  }
  if (total === 0) return null;
  const quantile = (q: number) => {
    let acc = 0;
    for (let k = 0; k < 256; k++) {
      acc += hist[k];
      if (acc >= q * total) return k;
    }
    return 255;
  };
  let lum = 0, dark = 0, bright = 0;
  for (let k = 0; k < 256; k++) {
    lum += k * hist[k];
    if (k <= 3) dark += hist[k];
    if (k >= 252) bright += hist[k];
  }
  const mean = [r / total, g / total, b / total].map((v) => Math.round(v * 10) / 10);
  const warm = Math.round((mean[0] - mean[2]) * 10) / 10;
  const cast = Math.abs(warm) < 6 ? 'neutral' : warm > 0 ? 'warm (red above blue)' : 'cool (blue above red)';
  return {
    mean_rgb: mean,
    luminance: { mean: Math.round(lum / total), p5: quantile(0.05), p50: quantile(0.5), p95: quantile(0.95) },
    clipped_shadows_pct: Math.round((dark / total) * 1000) / 10,
    clipped_highlights_pct: Math.round((bright / total) * 1000) / 10,
    saturation_pct: Math.round((sat / total) * 1000) / 10,
    warm_minus_cool: warm,
    note: `color balance ${cast}`,
  };
};

const smallCanvas = (source: CanvasImageSource & { width: number; height: number }, sx = 0, sy = 0, sw = source.width, sh = source.height, side = 480) => {
  const k = Math.min(1, side / Math.max(sw, sh));
  const { canvas, ctx } = canvasFrom(sw * k, sh * k);
  ctx.drawImage(source, sx, sy, sw, sh, 0, 0, canvas.width, canvas.height);
  return canvas;
};

// A render that did not finish leaves a pure white block along the right or bottom edge while the rest of the
// frame is normal. Seen once on a DNG right after a long job; a repeat of the same call renders it whole.
const looksIncomplete = (bitmap: ImageBitmap) => {
  const small = smallCanvas(bitmap, 0, 0, bitmap.width, bitmap.height, 64);
  const w = small.width;
  const h = small.height;
  const data = (small.getContext('2d') as CanvasRenderingContext2D).getImageData(0, 0, w, h).data;
  const white = (x: number, y: number) => {
    const i = (y * w + x) * 4;
    return data[i] >= 253 && data[i + 1] >= 253 && data[i + 2] >= 253;
  };
  const kx = Math.max(2, Math.floor(w * 0.2));
  const ky = Math.max(2, Math.floor(h * 0.2));
  const share = (x0: number, x1: number, y0: number, y1: number) => {
    let n = 0;
    for (let y = y0; y < y1; y++) for (let x = x0; x < x1; x++) if (white(x, y)) n++;
    return n / Math.max(1, (x1 - x0) * (y1 - y0));
  };
  // The strip is blank and the rest of the frame is not: a plain white photo is not a failed render.
  if (share(w - kx, w, 0, h) === 1 && share(0, w - kx, 0, h) < 0.5) return true;
  return share(0, w, h - ky, h) === 1 && share(0, w, 0, h - ky) < 0.5;
};

// Mask coverage figures from the overlay bitmap (its alpha is half the mask intensity).
const coverageOf = (overlay: ImageBitmap | null, width: number, height: number) => {
  const { canvas, ctx } = canvasFrom(width, height);
  if (overlay) ctx.drawImage(overlay, 0, 0, width, height);
  const { data } = ctx.getImageData(0, 0, canvas.width, canvas.height);
  const weights = new Float32Array(canvas.width * canvas.height);
  let sum = 0, cx = 0, cy = 0, x0 = 1, y0 = 1, x1 = 0, y1 = 0;
  for (let px = 0; px < weights.length; px++) {
    const w = Math.min(1, (data[px * 4 + 3] * 2) / 255);
    weights[px] = w;
    sum += w;
    if (w > 0.5) {
      const x = (px % canvas.width) / canvas.width, y = Math.floor(px / canvas.width) / canvas.height;
      cx += x; cy += y;
      x0 = Math.min(x0, x); y0 = Math.min(y0, y); x1 = Math.max(x1, x); y1 = Math.max(y1, y);
    }
  }
  const hard = weights.reduce((n, w) => n + (w > 0.5 ? 1 : 0), 0);
  const round = (v: number) => Math.round(v * 1000) / 1000;
  return {
    weights,
    size: { width: canvas.width, height: canvas.height },
    summary: {
      coverage_pct: Math.round((sum / weights.length) * 1000) / 10,
      empty: hard === 0,
      bbox: hard ? { x: round(x0), y: round(y0), width: round(x1 - x0), height: round(y1 - y0) } : null,
      centroid: hard ? { x: round(cx / hard), y: round(cy / hard) } : null,
    },
  };
};

// ---------------------------------------------------------------------------------------------
// Dress selection. The AI silhouette often takes more than the dress (the bride's face, hair and
// arms, a bridesmaid's gown, the groom's shirt). It is cleaned with the photo's own pixels so that
// only the dress stays: colours that differ from the dress, dark hair, stray islands and anything
// the caller excluded are dropped, and the cleaned bitmap replaces the AI one.
// ---------------------------------------------------------------------------------------------
const DRESS_PROMPT = 'white wedding dress of the bride';
const DRESS_FALLBACK_PROMPTS = ['wedding dress', "bride's long white gown skirt", 'white gown'];
const DRESS_WORK_SIDE = 800;

type Box = { x1: number; y1: number; x2: number; y2: number };

const LINEAR_RGB = (() => {
  const table = new Float32Array(256);
  for (let i = 0; i < 256; i++) {
    const c = i / 255;
    table[i] = c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
  }
  return table;
})();

const labPixel = (r: number, g: number, b: number, out: Float32Array, o: number) => {
  const R = LINEAR_RGB[r], G = LINEAR_RGB[g], B = LINEAR_RGB[b];
  const f = (t: number) => (t > 0.008856 ? Math.cbrt(t) : 7.787 * t + 16 / 116);
  const fx = f((0.4124 * R + 0.3576 * G + 0.1805 * B) / 0.95047);
  const fy = f(0.2126 * R + 0.7152 * G + 0.0722 * B);
  const fz = f((0.0193 * R + 0.1192 * G + 0.9505 * B) / 1.08883);
  out[o] = 116 * fy - 16;
  out[o + 1] = 500 * (fx - fy);
  out[o + 2] = 200 * (fy - fz);
};

const percentileOf = (values: ArrayLike<number>, p: number) => {
  const sorted = Float32Array.from(values as ArrayLike<number>).sort();
  return sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))] : 0;
};

// 3x3 grow or shrink; neighbours outside the frame are ignored so a dress touching the edge keeps its edge.
const morph3 = (src: Uint8Array, W: number, H: number, grow: boolean) => {
  const out = new Uint8Array(src.length);
  for (let y = 0; y < H; y++) {
    for (let x = 0; x < W; x++) {
      let v = grow ? 0 : 1;
      for (let dy = -1; dy <= 1 && (grow ? !v : v); dy++) {
        const yy = y + dy;
        if (yy < 0 || yy >= H) continue;
        for (let dx = -1; dx <= 1; dx++) {
          const xx = x + dx;
          if (xx < 0 || xx >= W) continue;
          const s = src[yy * W + xx];
          if (grow && s) { v = 1; break; }
          if (!grow && !s) { v = 0; break; }
        }
      }
      out[y * W + x] = v;
    }
  }
  return out;
};

const labelComponents = (bin: Uint8Array, W: number, H: number) => {
  const labels = new Int32Array(bin.length);
  const areas: number[] = [0];
  const boxes: Box[] = [{ x1: 0, y1: 0, x2: 0, y2: 0 }];
  const stack = new Int32Array(bin.length);
  let n = 0;
  for (let start = 0; start < bin.length; start++) {
    if (!bin[start] || labels[start]) continue;
    n++;
    let sp = 0, area = 0, x1 = W, y1 = H, x2 = 0, y2 = 0;
    stack[sp++] = start;
    labels[start] = n;
    while (sp) {
      const p = stack[--sp];
      const x = p % W, y = (p / W) | 0;
      area++;
      if (x < x1) x1 = x; if (x > x2) x2 = x; if (y < y1) y1 = y; if (y > y2) y2 = y;
      if (x > 0 && bin[p - 1] && !labels[p - 1]) { labels[p - 1] = n; stack[sp++] = p - 1; }
      if (x < W - 1 && bin[p + 1] && !labels[p + 1]) { labels[p + 1] = n; stack[sp++] = p + 1; }
      if (y > 0 && bin[p - W] && !labels[p - W]) { labels[p - W] = n; stack[sp++] = p - W; }
      if (y < H - 1 && bin[p + W] && !labels[p + W]) { labels[p + W] = n; stack[sp++] = p + W; }
    }
    areas.push(area);
    boxes.push({ x1: x1 / W, y1: y1 / H, x2: (x2 + 1) / W, y2: (y2 + 1) / H });
  }
  return { labels, areas, boxes, count: n };
};

const insideBox = (x: number, y: number, b: Box) => x >= b.x1 && x <= b.x2 && y >= b.y1 && y <= b.y2;

type DressCleanOptions = { region?: Box; exclude?: Box[]; chroma_scale?: number; dark_ratio?: number };

// gray: AI silhouette (0-255), rgba: the photo at the same size. Returns a soft 0-255 "keep" map.
const cleanDress = (gray: Uint8Array, rgba: Uint8ClampedArray, W: number, H: number, opts: DressCleanOptions) => {
  const N = W * H;
  const picked = new Uint8Array(N);
  let total = 0;
  for (let i = 0; i < N; i++) if (gray[i] > 127) { picked[i] = 1; total++; }
  if (total === 0) return { keep: new Uint8Array(N), report: { silhouette_px: 0, empty: true } as Record<string, unknown> };

  const lab = new Float32Array(N * 3);
  const idx: number[] = [];
  for (let i = 0; i < N; i++) {
    if (!picked[i]) continue;
    labPixel(rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2], lab, i * 3);
    idx.push(i);
  }

  const scale = opts.chroma_scale ?? 1;
  const chroma = (i: number) => Math.hypot(lab[i * 3 + 1], lab[i * 3 + 2]);
  // The dress colour is read from the calm, lit half of a set of pixels: robust to skin and hair inside it.
  const colourOf = (pixels: number[]) => {
    const lightFloor = percentileOf(pixels.map((i) => lab[i * 3]), 0.25);
    const lit = pixels.filter((i) => lab[i * 3] >= lightFloor);
    const chromaMid = percentileOf(lit.map(chroma), 0.5);
    const calm = lit.filter((i) => chroma(i) <= chromaMid);
    const ref = calm.length ? calm : lit;
    const a = percentileOf(ref.map((i) => lab[i * 3 + 1]), 0.5);
    const b = percentileOf(ref.map((i) => lab[i * 3 + 2]), 0.5);
    const l = percentileOf(ref.map((i) => lab[i * 3]), 0.5);
    const spread = percentileOf(ref.map((i) => Math.hypot(lab[i * 3 + 1] - a, lab[i * 3 + 2] - b)), 0.9);
    return { a, b, l, spread };
  };
  const exclude = opts.exclude ?? [];
  let byColor = 0, byDark = 0, bySkin = 0, byBox = 0;
  const pass = (c: { a: number; b: number; l: number; spread: number }) => {
    const keep = new Uint8Array(N);
    // Lighting changes across a dress (warm on one side, cool on the other), so colour is only used to
    // drop real outliers. Skin is judged by its own hue, not by distance from the dress.
    const threshold = Math.max(22, 3 * c.spread) * scale;
    const darkLimit = (opts.dark_ratio ?? 0.4) * c.l;
    byColor = byDark = bySkin = byBox = 0;
    for (const i of idx) {
      const x = (i % W) / W, y = ((i / W) | 0) / H;
      if ((opts.region && !insideBox(x, y, opts.region)) || exclude.some((b) => insideBox(x, y, b))) { byBox++; continue; }
      const L = lab[i * 3], a = lab[i * 3 + 1], b = lab[i * 3 + 2];
      if (L < darkLimit) { byDark++; continue; }
      const hue = (Math.atan2(b, a) * 180) / Math.PI;
      if (hue >= 28 && hue <= 80 && Math.hypot(a, b) >= 13 * scale && L <= 78) { bySkin++; continue; }
      // A blue cast on the dress is what the caller wants to treat, so bluish pixels are never "outliers".
      if (Math.hypot(a - c.a, b - c.b) > threshold && b > -2) { byColor++; continue; }
      keep[i] = 1;
    }
    return { keep, threshold };
  };
  const openAndLabel = (keep: Uint8Array) => {
    // Cut hair strands and the thin bridges made by hands and arms before looking for pieces.
    let opened = keep;
    for (let k = 0; k < 2; k++) opened = morph3(opened, W, H, false);
    for (let k = 0; k < 2; k++) opened = morph3(opened, W, H, true);
    return { opened, comps: labelComponents(opened, W, H) };
  };

  // Pass 1 finds the biggest piece (the dress); pass 2 re-reads the colour from that piece alone.
  const first = pass(colourOf(idx));
  const firstPieces = openAndLabel(first.keep);
  let anchor = 0;
  for (let c = 1; c <= firstPieces.comps.count; c++) if (!anchor || firstPieces.comps.areas[c] > firstPieces.comps.areas[anchor]) anchor = c;
  const anchorPixels = anchor ? idx.filter((i) => firstPieces.comps.labels[i] === anchor) : idx;
  const { keep: keep0, threshold } = pass(colourOf(anchorPixels.length > 200 ? anchorPixels : idx));

  const { comps } = openAndLabel(keep0);
  const biggest = comps.areas.slice(1).reduce((m, a) => Math.max(m, a), 0);
  const minArea = Math.max(biggest * 0.02, N * 0.0003);
  let main = 0;
  for (let c = 1; c <= comps.count; c++) if (!main || comps.areas[c] > comps.areas[main]) main = c;
  // Pieces of the same dress are small and sit next to the main piece (the bodice above the bouquet,
  // lace on the train). A big separate piece is another person's gown, or something else entirely.
  // "Belongs to the dress" = centred inside the main piece's width and not hanging below it.
  const touches = (b: Box) => {
    if (!main) return false;
    const m = comps.boxes[main];
    const cx = (b.x1 + b.x2) / 2;
    return cx >= m.x1 - 0.02 && cx <= m.x2 + 0.02 && b.y2 <= m.y2 + 0.02;
  };
  const keepLabel = new Uint8Array(comps.count + 1);
  let kept = 0, droppedByShade = 0;
  for (let c = 1; c <= comps.count; c++) {
    if (comps.areas[c] < minArea) continue;
    if (c !== main && (comps.areas[c] > comps.areas[main] * 0.4 || !touches(comps.boxes[c]))) { droppedByShade++; continue; }
    keepLabel[c] = 1;
    kept++;
  }
  const body = new Uint8Array(N);
  for (let i = 0; i < N; i++) if (keepLabel[comps.labels[i]]) body[i] = 1;
  const final = morph3(body, W, H, true);
  for (let i = 0; i < N; i++) if (!keep0[i] && picked[i] && !body[i]) final[i] = 0;

  // Soften the edge by one pixel so the cut is not a hard staircase.
  const soft = new Uint8Array(N);
  for (let y = 0; y < H; y++) {
    for (let x = 0; x < W; x++) {
      let sum = 0, cnt = 0;
      for (let dy = -1; dy <= 1; dy++) {
        const yy = y + dy;
        if (yy < 0 || yy >= H) continue;
        for (let dx = -1; dx <= 1; dx++) {
          const xx = x + dx;
          if (xx < 0 || xx >= W) continue;
          sum += final[yy * W + xx]; cnt++;
        }
      }
      soft[y * W + x] = Math.round((sum / cnt) * 255);
    }
  }
  let keptPx = 0;
  for (let i = 0; i < N; i++) if (final[i] && picked[i]) keptPx++;
  const pct = (v: number) => Math.round((v / total) * 1000) / 10;
  return {
    keep: soft,
    report: {
      silhouette_px: total,
      kept_pct: pct(keptPx),
      removed_by_color_pct: pct(byColor),
      removed_by_skin_pct: pct(bySkin),
      removed_by_dark_pct: pct(byDark),
      removed_by_box_pct: pct(byBox),
      pieces_kept: kept,
      pieces_dropped: comps.count - kept,
      pieces_dropped_by_shade: droppedByShade,
      color_threshold: Math.round(threshold * 10) / 10,
      empty: keptPx === 0,
    } as Record<string, unknown>,
  };
};

const toBox = (v: any): Box | undefined => {
  if (!v || typeof v !== 'object') return undefined;
  const nums = [v.x1, v.y1, v.x2, v.y2];
  if (nums.some((n) => typeof n !== 'number' || n < 0 || n > 1)) fail('region/exclude boxes need x1,y1,x2,y2 as fractions 0-1');
  return { x1: Math.min(v.x1, v.x2), y1: Math.min(v.y1, v.y2), x2: Math.max(v.x1, v.x2), y2: Math.max(v.y1, v.y2) };
};

// The AI bitmap must keep its own size (the renderer maps it with the full image scale), so the small
// "keep" map is stretched over it and multiplied in: black removes, white leaves the AI value.
const applyKeepToMask = async (maskBitmap: ImageBitmap, keep: Uint8Array, W: number, H: number) => {
  const { canvas, ctx } = canvasFrom(maskBitmap.width, maskBitmap.height);
  ctx.drawImage(maskBitmap, 0, 0);
  const small = canvasFrom(W, H);
  const img = small.ctx.createImageData(W, H);
  for (let i = 0; i < W * H; i++) {
    const v = keep[i];
    img.data[i * 4] = v;
    img.data[i * 4 + 1] = v;
    img.data[i * 4 + 2] = v;
    img.data[i * 4 + 3] = 255;
  }
  small.ctx.putImageData(img, 0, 0);
  ctx.globalCompositeOperation = 'multiply';
  ctx.imageSmoothingEnabled = true;
  ctx.imageSmoothingQuality = 'high';
  ctx.drawImage(small.canvas, 0, 0, canvas.width, canvas.height);
  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, 'image/png'));
  if (!blob) fail('Could not encode the cleaned mask');
  return `data:image/png;base64,${await blobToBase64(blob!)}`;
};

const GRID = 3;
// Mean luminance per cell of a 3x3 grid: a coarse map of where the light is.
const lumaGrid = (canvas: HTMLCanvasElement) => {
  const ctx = canvas.getContext('2d');
  if (!ctx) return null;
  const rows: number[][] = [];
  for (let gy = 0; gy < GRID; gy++) {
    const row: number[] = [];
    for (let gx = 0; gx < GRID; gx++) {
      const w = Math.max(1, Math.floor(canvas.width / GRID)), h = Math.max(1, Math.floor(canvas.height / GRID));
      const { data } = ctx.getImageData(gx * w, gy * h, w, h);
      let sum = 0;
      for (let i = 0; i < data.length; i += 4) sum += 0.2126 * data[i] + 0.7152 * data[i + 1] + 0.0722 * data[i + 2];
      row.push(Math.round(sum / (data.length / 4)));
    }
    rows.push(row);
  }
  return rows;
};

// Photo quality measurement (assess_photos). Sharpness is the mean squared Laplacian of the luminance of
// a 1200 px frame: the more fine detail, the higher. The values only compare photos measured the same way.
// When faces are measured the frame is 2000 px, so a small face still has enough pixels to tell sharp from soft.
// match_style measures renders of this size, and tries at most this many times per photo to reach its targets.
const MATCH_SIDE = 480;
const MATCH_ROUNDS = 7;
// match_style never lets the lightest 5% of a photo go above this while brightening it.
const HEADROOM = 238;
const ASSESS_SIDE = 1200;
const ASSESS_SIDE_FACES = 2000;
// A camera (told apart by file type here) needs this many photos before it is ranked against its own kind;
// RAW files of different sensors render with very different amounts of fine detail at the same size.
const ASSESS_MIN_GROUP = 8;
const FACE_REGION_PARTS = ['faceSkin', 'eyebrows', 'eyeWhites', 'iris', 'lips', 'teeth', 'nose'];

type AssessRow = {
  path: string;
  ok: boolean;
  error?: string;
  note?: string;
  group?: string;
  faces?: number;
  face_area_pct?: number | null;
  sharp_face?: number | null;
  face_vs_frame?: number | null;
  noise_floor?: number;
  sharp_region?: number;
  sharp_global?: number;
  luma_mean?: number;
  clipped_shadows_pct?: number;
  clipped_highlights_pct?: number;
  reference?: number;
  z?: number;
  quality?: number;
  stars?: number;
  suspect_blurry?: boolean;
  noisy?: boolean;
  reason?: string;
};

const lumaPlane = (rgba: Uint8ClampedArray, pixels: number) => {
  const out = new Float32Array(pixels);
  for (let i = 0; i < pixels; i++) out[i] = 0.2126 * rgba[i * 4] + 0.7152 * rgba[i * 4 + 1] + 0.0722 * rgba[i * 4 + 2];
  return out;
};

// Energy over a region, or only over the pixels set in `keep`.
const laplacianEnergy = (luma: Float32Array, W: number, H: number, keep: Uint8Array | null, x0 = 0, y0 = 0, x1 = W, y1 = H) => {
  let sum = 0;
  let count = 0;
  for (let y = Math.max(1, y0); y < Math.min(H - 1, y1); y++) {
    for (let x = Math.max(1, x0); x < Math.min(W - 1, x1); x++) {
      const i = y * W + x;
      if (keep && !keep[i]) continue;
      const l = 4 * luma[i] - luma[i - 1] - luma[i + 1] - luma[i - W] - luma[i + W];
      sum += l * l;
      count++;
    }
  }
  return { energy: count ? sum / count : 0, count };
};

// Energy of each tile of a 6x6 grid, ascending. The flattest tiles show how much of the energy is noise; the
// sharpest ones show the detail of the subject.
const tileEnergies = (luma: Float32Array, W: number, H: number) => {
  const G = 6;
  const energies: number[] = [];
  for (let gy = 0; gy < G; gy++) {
    for (let gx = 0; gx < G; gx++) {
      const { energy, count } = laplacianEnergy(luma, W, H, null, Math.floor((gx * W) / G), Math.floor((gy * H) / G), Math.floor(((gx + 1) * W) / G), Math.floor(((gy + 1) * H) / G));
      if (count > 0) energies.push(energy);
    }
  }
  return energies.sort((a, b) => a - b);
};

// Sharpness of each face in a face mask (white = face), largest first. The mask is shrunk a few pixels so the
// border against the background does not pass for detail.
const faceSharpness = (gray: Uint8Array, luma: Float32Array, W: number, H: number) => {
  let bin = new Uint8Array(W * H);
  for (let i = 0; i < bin.length; i++) if (gray[i] > 127) bin[i] = 1;
  const shrink = Math.max(3, Math.round((3 * Math.max(W, H)) / ASSESS_SIDE));
  for (let k = 0; k < shrink; k++) bin = morph3(bin, W, H, false);
  const { labels, areas, count } = labelComponents(bin, W, H);
  const faces: Array<{ area_pct: number; sharp: number }> = [];
  for (let c = 1; c <= count; c++) {
    if (areas[c] < W * H * 0.0015) continue;
    const keep = new Uint8Array(W * H);
    for (let i = 0; i < keep.length; i++) if (labels[i] === c) keep[i] = 1;
    faces.push({ area_pct: Math.round((areas[c] / (W * H)) * 1000) / 10, sharp: laplacianEnergy(luma, W, H, keep).energy });
  }
  return faces.sort((a, b) => b.area_pct - a.area_pct);
};

const quantileOf = (sorted: number[], q: number) => (sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * q))] : 0);
const round1 = (v: number) => Math.round(v * 10) / 10;

const groupOf = (path: string) => (/\.([^.\/]+)$/.exec(path)?.[1] ?? '').toLowerCase();

// Rank a set of measured photos. Sharpness decides most of the score; exposure and overall detail the rest.
// A photo is a blur suspect when its reference sharpness (main face, else sharpest region) is far below that of its
// own kind: photos of one file type (one camera) are compared with each other, so a sensor that renders less fine
// detail is not flagged for being itself. A type with too few photos is compared with the whole set.
// `side` is the frame size the photos were measured at; the absolute noise limit grows with it.
const rankAssessments = (rows: AssessRow[], side = ASSESS_SIDE) => {
  const ok = rows.filter((r) => r.ok);
  for (const r of ok) {
    r.reference = r.sharp_face ?? r.sharp_region ?? 0;
    r.group = groupOf(r.path);
  }
  const sortedLogs = (set: AssessRow[]) => set.map((r) => Math.log1p(r.reference!)).sort((a, b) => a - b);
  const groupStats = (set: AssessRow[]) => {
    const sorted = sortedLogs(set);
    const median = quantileOf(sorted, 0.5);
    return {
      sorted,
      median,
      spread: quantileOf(sorted.map((v) => Math.abs(v - median)).sort((a, b) => a - b), 0.5) * 1.4826 || 0.1,
      globals: set.map((r) => r.sharp_global ?? 0),
      medianFloor: quantileOf(set.map((r) => r.noise_floor ?? 0).sort((a, b) => a - b), 0.5),
    };
  };
  const overall = groupStats(ok);
  const groups = new Map<string, ReturnType<typeof groupStats>>();
  for (const g of new Set(ok.map((r) => r.group!))) {
    const members = ok.filter((r) => r.group === g);
    if (members.length >= ASSESS_MIN_GROUP && members.length < ok.length) groups.set(g, groupStats(members));
  }
  const below = (values: number[], v: number) => values.filter((x) => x < v).length / Math.max(1, values.length - 1);
  const enough = ok.length >= 8;
  const noiseLimit = 60 * (side / ASSESS_SIDE);
  for (const r of ok) {
    const st = groups.get(r.group!) ?? overall;
    r.noisy = enough && (r.noise_floor ?? 0) > Math.max(noiseLimit, 4 * st.medianFloor);
    if (r.noisy) r.reason = 'unusually noisy or patterned: look at it before trusting any figure';
    r.z = Math.round(((Math.log1p(r.reference!) - st.median) / st.spread) * 100) / 100;
    const clipped = (r.clipped_shadows_pct ?? 0) + (r.clipped_highlights_pct ?? 0);
    let exposure = 1 - Math.min(1, clipped / 30);
    if ((r.luma_mean ?? 128) < 35) exposure *= 0.6;
    if ((r.luma_mean ?? 128) > 215) exposure *= 0.7;
    r.quality = Math.round(100 * (0.65 * below(st.sorted, Math.log1p(r.reference!)) + 0.15 * below(st.globals, r.sharp_global ?? 0) + 0.2 * exposure));
    // A face far softer than the frame is only a suspect when it is also soft for its kind: a chandelier or a
    // lamp makes the frame's sharpest region huge and any ordinary face look soft next to it.
    const softFace = (r.faces ?? 0) > 0 && r.face_vs_frame != null && r.face_vs_frame < 0.12 && r.z < -1;
    // Inside one kind roughly one photo in ten sits 1.2 spreads below the middle without being blurry, so a plain
    // z limit is not enough: the photo must also hold under half of the typical sharpness of its kind.
    const farBelow = r.z < -2 && r.reference! < 0.5 * Math.expm1(st.median);
    r.suspect_blurry = enough && (farBelow || softFace);
    if (r.suspect_blurry && !r.noisy) r.reason = softFace ? 'the face is far softer than the rest of the frame' : 'far softer than the rest of its kind';
  }
  const order = [...ok].sort((a, b) => b.quality! - a.quality!);
  order.forEach((r, i) => {
    const f = i / order.length;
    r.stars = f < 0.08 ? 5 : f < 0.25 ? 4 : f < 0.6 ? 3 : f < 0.85 ? 2 : 1;
    if (r.suspect_blurry) r.stars = 1;
    // A photo whose flattest areas are far noisier than the rest of the set is not a sharp photo, however much
    // "detail" it seems to have: cap it and ask for a look.
    if (r.noisy) r.stars = Math.min(r.stars, 2);
  });
  const reference = (st: ReturnType<typeof groupStats>) => ({ median: round1(Math.expm1(st.median)), p10: round1(Math.expm1(quantileOf(st.sorted, 0.1))), p90: round1(Math.expm1(quantileOf(st.sorted, 0.9))) });
  return {
    ok,
    reference: reference(overall),
    by_group: Object.fromEntries([...groups].map(([g, st]) => [g, { photos: st.sorted.length, ...reference(st) }])),
    enough,
  };
};

export function useMcpBridge(handlers: McpHandlers) {
  const handlersRef = useRef(handlers);
  handlersRef.current = handlers;
  const { setAdjustments, selectCompareMode, handleAutoAdjustments } = useEditorActions();
  const ai = useAiMasking();
  const toolsRef = useRef({ setAdjustments, selectCompareMode, handleAutoAdjustments, ai });
  toolsRef.current = { setAdjustments, selectCompareMode, handleAutoAdjustments, ai };

  useEffect(() => {
    let disposed = false;

    const library = () => useLibraryStore.getState();
    const editor = () => useEditorStore.getState();
    const visibleList = () => computeSortedLibrary(library(), useSettingsStore.getState());

    const entryFor = (path: string): ImageFile => {
      const key = norm(path);
      return (
        library().imageList.find((img) => norm(img.path) === key) ??
        fail(`Photo is not in the open library view; call open_folder first: ${path}`)
      );
    };

    const summary = (root: string, image: ImageFile) => ({
      path: relPath(root, image.path),
      rating: library().imageRatings[image.path] ?? image.rating ?? 0,
      flag: image.flag ?? null,
      color: colorOf(image),
      edited: image.is_edited,
      raw: image.is_raw,
    });

    const waitReady = async (path: string, timeoutMs = 30000) => {
      const deadline = Date.now() + timeoutMs;
      for (;;) {
        const s = editor().selectedImage;
        if (s && norm(s.path) === norm(path) && s.isReady) return s;
        if (Date.now() >= deadline || disposed) fail('Photo did not finish loading in the editor');
        await sleep(80);
      }
    };

    const activeReady = (path: string) => {
      const s = editor().selectedImage;
      if (!s || norm(s.path) !== norm(path) || !s.isReady) fail('Photo is not the active, loaded photo; call open_photo first');
      return s!;
    };

    const savedAdjustments = async (path: string): Promise<Adjustments> => {
      const meta: any = await invoke(Invokes.LoadMetadata, { path });
      return meta?.adjustments && !meta.adjustments.is_null
        ? normalizeLoadedAdjustments(meta.adjustments)
        : { ...INITIAL_ADJUSTMENTS };
    };

    const refreshEdited = (path: string, saved: Adjustments) => {
      const edited = revisionOf(saved) !== revisionOf(INITIAL_ADJUSTMENTS);
      library().setLibrary((state) => ({
        imageList: state.imageList.map((img) => (norm(img.path) === norm(path) ? { ...img, is_edited: edited } : img)),
      }));
    };

    // Write what the editor holds to the sidecar now, wait for that write, and read it back.
    // `saved` means the sidecar on disk holds this edit, not that an autosave timer is pending.
    // AI bitmaps are stored apart from the sidecar, so masks are compared by id, not by content.
    const persist = async (path: string, expectedPatch?: any): Promise<Saved> => {
      debouncedSetHistory.flush();
      debouncedSave.cancel();
      const s = editor().selectedImage;
      if (!s || norm(s.path) !== norm(path) || !s.isReady) return { saved: false, save_problem: 'the photo is no longer the active one' };
      const sent = editor().adjustments;
      try {
        await invoke(Invokes.SaveMetadataAndUpdateThumbnail, { path: s.path, adjustments: sent });
      } catch (e) {
        note(`persist ${relPath(ROOTS[0] ?? '', s.path)} write failed: ${String(e)}`);
        return { saved: false, save_problem: `the sidecar could not be written: ${String(e)}` };
      }
      let saved: Adjustments;
      try {
        saved = await savedAdjustments(s.path);
      } catch (e) {
        return { saved: false, save_problem: `the sidecar could not be read back: ${String(e)}` };
      }
      refreshEdited(s.path, saved);
      const mine = sent.masks || [];
      const theirs = saved.masks || [];
      if (!sameIds(mine, theirs)) {
        return { saved: false, save_problem: `the sidecar holds ${theirs.length} mask(s) but the editor has ${mine.length}` };
      }
      if (expectedPatch !== undefined && !isSubset(expectedPatch, saved)) {
        return { saved: false, save_problem: 'the saved values differ from the requested ones' };
      }
      return { saved: true };
    };

    // The editor may report a photo as loaded while still showing adjustments from its cache; the
    // sidecar is what was saved, so wait until both agree on the masks before anyone edits.
    const waitInSync = async (path: string, timeoutMs = 6000) => {
      const deadline = Date.now() + timeoutMs;
      for (;;) {
        const saved = await savedAdjustments(path);
        const current = editor().adjustments;
        if (sameIds(current.masks || [], saved.masks || [])) return null;
        if (Date.now() >= deadline || disposed) {
          return `the editor shows ${(current.masks || []).length} mask(s) but the saved sidecar has ${(saved.masks || []).length}`;
        }
        await sleep(150);
      }
    };

    // Batch jobs (paste, reset) return before their sidecars are written: poll each target until the
    // saved file satisfies `check`, and report per photo.
    const confirmSaved = async (paths: string[], check: (saved: Adjustments) => boolean, timeoutMs = 10000) => {
      const deadline = Date.now() + timeoutMs;
      const pending = new Set(paths);
      while (pending.size > 0) {
        for (const p of [...pending]) {
          if (check(await savedAdjustments(p))) pending.delete(p);
        }
        if (pending.size === 0 || Date.now() >= deadline || disposed) break;
        await sleep(200);
      }
      return paths.map((p) => ({ path: relPath(ROOTS[0] ?? '', p), ok: !pending.has(p), ...(pending.has(p) ? { error: 'the saved sidecar does not show the change yet' } : {}) }));
    };

    const settle = async () => {
      await sleep(60);
    };

    const adjustmentsFor = async (path: string): Promise<Adjustments> => {
      const s = editor().selectedImage;
      if (s && norm(s.path) === norm(path) && s.isReady) return editor().adjustments;
      const meta: any = await invoke(Invokes.LoadMetadata, { path });
      return meta?.adjustments && !meta.adjustments.is_null
        ? normalizeLoadedAdjustments(meta.adjustments)
        : INITIAL_ADJUSTMENTS;
    };

    const render = async (path: string, adjustments: Adjustments, maxDimension: number) => {
      for (let attempt = 0; ; attempt++) {
        const buffer = await invoke<ArrayBuffer>('generate_preview_for_path', {
          path,
          jsAdjustments: adjustments,
          maxDimension,
        });
        const bitmap = await bytesToBitmap(buffer, 'image/jpeg');
        if (attempt >= 2 || !looksIncomplete(bitmap)) return bitmap;
        note(`render looks incomplete (blank edge), asking again (${attempt + 1})`);
        bitmap.close();
        await sleep(1500);
      }
    };

    // Coordinates are fractions of the oriented, uncropped image; masks are stored in image pixels.
    const imageSpace = () => {
      const s = editor().selectedImage!;
      const steps = editor().adjustments.orientationSteps || 0;
      const swap = steps % 2 === 1;
      const width = swap ? s.height : s.width;
      const height = swap ? s.width : s.height;
      return { width, height, long: Math.max(width, height) };
    };

    const toPixelParams = (type: string, params: Args, dim: { width: number; height: number; long: number }) => {
      const p = { ...params };
      const px = (key: string, axis: 'width' | 'height') => {
        if (typeof p[key] === 'number') p[key] = p[key] * dim[axis];
      };
      if (type === Mask.Linear) {
        px('startX', 'width');
        px('startY', 'height');
        px('endX', 'width');
        px('endY', 'height');
      } else if (type === Mask.Radial) {
        px('centerX', 'width');
        px('centerY', 'height');
        px('radiusX', 'width');
        px('radiusY', 'width');
      }
      return p;
    };

    const toLines = (lines: any[], dim: { width: number; height: number; long: number }) =>
      lines.map((line) => {
        if (!Array.isArray(line.points) || line.points.length === 0) fail('Brush strokes need at least one point');
        const flow = typeof line.flow === 'number' ? { flow: line.flow } : {};
        return {
          tool: line.tool === 'eraser' ? 'eraser' : 'brush',
          brushSize: Math.max(1, (line.size ?? 0.03) * dim.long),
          feather: line.feather ?? 0.5,
          ...flow,
          points: line.points.map((pt: any) => ({ x: pt.x * dim.width, y: pt.y * dim.height })),
        };
      });

    const setMasks = (mutate: (masks: MaskContainer[]) => MaskContainer[]) =>
      toolsRef.current.setAdjustments((prev: Adjustments) => ({ ...prev, masks: mutate(prev.masks || []) }));

    const findMask = (id: string) =>
      (editor().adjustments.masks || []).find((m: MaskContainer) => m.id === id) ?? fail(`Unknown mask id: ${id}`);

    const metadataOp = async (
      root: string,
      args: Args,
      command: string,
      payload: (paths: string[]) => Args,
      check: (meta: any) => boolean,
    ) => {
      if (!Array.isArray(args.paths) || args.paths.length === 0) fail('paths is required');
      const targets = args.paths.map((p: string) => entryFor(p));
      const paths = targets.map((t: ImageFile) => t.path);
      let batchError: string | null = null;
      try {
        await invoke(command, payload(paths));
      } catch (e) {
        batchError = String(e);
      }
      const files = [];
      for (const image of targets) {
        try {
          const meta: any = await invoke(Invokes.LoadMetadata, { path: image.path });
          library().setLibrary((state) => ({
            imageRatings: { ...state.imageRatings, [image.path]: meta.rating },
            imageList: state.imageList.map((img) =>
              img.path === image.path ? { ...img, flag: meta.flag ?? null, tags: meta.tags ?? null } : img,
            ),
          }));
          const ok = check(meta);
          files.push({
            path: relPath(root, image.path),
            ok,
            rating: meta.rating,
            flag: meta.flag ?? null,
            color: (meta.tags as string[] | null)?.find((t) => t.startsWith('color:'))?.substring(6) ?? null,
            ...(ok ? {} : { error: batchError ?? 'Persisted value differs from requested value' }),
          });
        } catch (e) {
          files.push({ path: relPath(root, image.path), ok: false, error: String(e) });
        }
      }
      const failed = files.filter((f) => !f.ok);
      if (files.length > 20 && args.detail !== 'full') {
        return { ok: failed.length === 0, count: files.length, failed, note: 'Every photo was checked against its saved metadata. Pass detail "full" for a row per photo.' };
      }
      return { ok: failed.length === 0, files, ...(failed.length ? { failed } : {}) };
    };

    const overlayParts = async (path: string, maskId: string, maxDimension: number) => {
      const s = activeReady(path);
      const adjustments = editor().adjustments;
      const container = findMask(maskId);
      const base = await render(s.path, adjustments, maxDimension);
      const steps = adjustments.orientationSteps || 0;
      const orientedWidth = steps % 2 === 1 ? s.height : s.width;
      const cropWidth = adjustments.crop?.width || orientedWidth;
      const scale = base.width / cropWidth;
      const def = structuredClone(container);
      def.subMasks.forEach((sm: any) => {
        if (sm.parameters?.maskDataBase64 !== undefined && editor().patchesSentToBackend.has(sm.id)) {
          sm.parameters.maskDataBase64 = null;
        }
      });
      const buffer = await invoke<ArrayBuffer>(Invokes.GenerateMaskOverlay, {
        cropOffset: [adjustments.crop?.x || 0, adjustments.crop?.y || 0],
        height: base.height,
        width: base.width,
        maskDef: def,
        scale,
        jsAdjustments: adjustments,
      });
      const overlay = buffer && buffer.byteLength > 0 ? await bytesToBitmap(buffer, 'image/png') : null;
      return { base, overlay };
    };

    // Where the mask lands and how the pixels inside and outside it look, as numbers.
    const maskReport = (base: ImageBitmap, overlay: ImageBitmap | null) => {
      const cov = coverageOf(overlay, base.width, base.height);
      const baseCanvas = smallCanvas(base, 0, 0, base.width, base.height, base.width);
      return {
        ...cov.summary,
        inside_mask: statsOf(baseCanvas, cov.weights),
        outside_mask: statsOf(baseCanvas, cov.weights, true),
      };
    };

    const overlayImage = async (path: string, maskId: string, maxDimension: number) => {
      const { base, overlay } = await overlayParts(path, maskId, maxDimension);
      const { canvas, ctx } = canvasFrom(base.width, base.height);
      ctx.drawImage(base, 0, 0);
      if (overlay) ctx.drawImage(overlay, 0, 0, base.width, base.height);
      const report = maskReport(base, overlay);
      const result = { canvas, width: base.width, height: base.height, empty: !overlay, report };
      base.close();
      overlay?.close();
      return result;
    };

    // AI silhouette of the dress, then cleaned with the photo's own pixels (see cleanDress).
    const dressSilhouette = async (path: string, containerId: string, subMaskId: string, userPrompt: string | undefined, options: DressCleanOptions) => {
      const ai = toolsRef.current.ai;
      const readData = () =>
        (findMask(containerId).subMasks.find((sm: SubMask) => sm.id === subMaskId)?.parameters?.maskDataBase64 as string | null | undefined) ?? null;
      const prompts = userPrompt ? [userPrompt] : [DRESS_PROMPT, ...DRESS_FALLBACK_PROMPTS];
      let data: string | null = null;
      let used = '';
      let hardError: string | null = null;
      for (const prompt of prompts) {
        let attempts = 0;
        for (;;) {
          aiFailures.delete(subMaskId);
          await ai.handleGenerateAiTextMask(subMaskId, prompt);
          data = readData();
          const failure = aiFailures.get(subMaskId);
          // Too little free memory is transient: wait a moment and ask again before giving up on the photo.
          if (data || !failure || !/memor|8007000E|bad alloc|No original image loaded/i.test(failure) || attempts >= 2) break;
          attempts++;
          note(`dress segmentation hit a memory error, retry ${attempts}`);
          await sleep(4000);
        }
        if (data) {
          used = prompt;
          break;
        }
        // A real failure (model missing, out of memory) is not fixed by asking with other words.
        const why = aiFailures.get(subMaskId);
        if (why?.startsWith('error')) {
          hardError = why;
          break;
        }
      }
      if (!data) {
        return {
          found: false,
          prompts_tried: prompts,
          ...(hardError ? { failure: hardError } : {}),
          warning: hardError
            ? `The segmentation failed, so the dress could not be looked for (${hardError})`
            : 'No dress found by the segmentation; try another prompt, or paint it with brush strokes',
        };
      }
      const s = activeReady(path);
      // Original colours (no user grading), uncropped, so the photo lines up with the full-frame silhouette.
      const plain = { ...getOriginalAdjustments(editor().adjustments), crop: null, aspectRatio: INITIAL_ADJUSTMENTS.aspectRatio } as Adjustments;
      const photo = await render(s.path, plain, DRESS_WORK_SIDE);
      const maskBitmap = await createImageBitmap(await (await fetch(data)).blob());
      const W = photo.width, H = photo.height;
      let cleaned = data;
      let report: Record<string, unknown>;
      if (Math.abs(maskBitmap.width / maskBitmap.height - W / H) > 0.03) {
        report = { skipped: 'photo and silhouette do not line up (rotated or odd aspect); only the AI silhouette was kept' };
      } else {
        const grayCanvas = canvasFrom(W, H);
        grayCanvas.ctx.drawImage(maskBitmap, 0, 0, W, H);
        const g = grayCanvas.ctx.getImageData(0, 0, W, H).data;
        const gray = new Uint8Array(W * H);
        for (let i = 0; i < gray.length; i++) gray[i] = g[i * 4];
        const photoCanvas = canvasFrom(W, H);
        photoCanvas.ctx.drawImage(photo, 0, 0);
        const rgba = photoCanvas.ctx.getImageData(0, 0, W, H).data;
        const out = cleanDress(gray, rgba, W, H, options);
        if (out.report.empty) {
          report = { ...out.report, warning: 'Cleaning removed everything, so the AI silhouette was kept. Try chroma_scale above 1, or drop region/exclude.' };
        } else {
          cleaned = await applyKeepToMask(maskBitmap, out.keep, W, H);
          report = out.report;
        }
      }
      photo.close();
      maskBitmap.close();
      setMasks((masks) =>
        masks.map((m) =>
          m.id !== containerId
            ? m
            : { ...m, subMasks: m.subMasks.map((sm) => (sm.id === subMaskId ? { ...sm, parameters: { ...sm.parameters, prompt: used, maskDataBase64: cleaned } } : sm)) },
        ),
      );
      editor().patchesSentToBackend.delete(subMaskId);
      await settle();
      return { found: true, prompt: used, ...report };
    };

    // Render at a small size and measure the look of what it shows (match_style).
    const lookAt = async (path: string, adjustments: Adjustments, side: number): Promise<Look> => {
      const bitmap = await render(path, adjustments, side);
      const small = smallCanvas(bitmap, 0, 0, bitmap.width, bitmap.height, side);
      bitmap.close();
      const data = (small.getContext('2d') as CanvasRenderingContext2D).getImageData(0, 0, small.width, small.height).data;
      return lookOf(data, small.width * small.height);
    };

    // A render with its measurements: the look (tone and colour casts) and the palette (whites, blacks, colour families).
    const readAt = async (path: string, adjustments: Adjustments, side: number) => {
      const bitmap = await render(path, adjustments, side);
      const canvas = smallCanvas(bitmap, 0, 0, bitmap.width, bitmap.height, side);
      bitmap.close();
      const data = (canvas.getContext('2d') as CanvasRenderingContext2D).getImageData(0, 0, canvas.width, canvas.height).data;
      const pixels = canvas.width * canvas.height;
      return { canvas, look: lookOf(data, pixels), palette: paletteOf(data, pixels) };
    };

    // The sliders that make up a look; style_photo reset clears these and leaves masks, crop and detail alone.
    const lookResetPatch = () => {
      const keys = ['exposure', 'brightness', 'contrast', 'highlights', 'shadows', 'whites', 'blacks', 'temperature', 'tint', 'saturation', 'vibrance'];
      const patch: Record<string, any> = {};
      for (const k of keys) patch[k] = (INITIAL_ADJUSTMENTS as any)[k];
      patch.hsl = JSON.parse(JSON.stringify(INITIAL_ADJUSTMENTS.hsl));
      patch.colorGrading = JSON.parse(JSON.stringify(INITIAL_ADJUSTMENTS.colorGrading));
      patch.curves = JSON.parse(JSON.stringify(INITIAL_ADJUSTMENTS.curves));
      patch.pointCurves = JSON.parse(JSON.stringify(INITIAL_ADJUSTMENTS.pointCurves));
      return patch;
    };

    // Photos laid out on one dark sheet with a short label each; the tools return it as one picture.
    const contactSheet = async (items: Array<{ label: string; canvas: HTMLCanvasElement }>, bigCell?: number) => {
      const cols = Math.min(4, items.length);
      const cell = bigCell ?? (items.length <= 4 ? 360 : 300);
      const rows = Math.ceil(items.length / cols);
      const { canvas, ctx } = canvasFrom(cols * cell, rows * cell);
      ctx.fillStyle = '#111';
      ctx.fillRect(0, 0, canvas.width, canvas.height);
      items.forEach((item, i) => {
        const k = Math.min(cell / item.canvas.width, cell / item.canvas.height);
        const w = item.canvas.width * k, h = item.canvas.height * k;
        const x = (i % cols) * cell + (cell - w) / 2, y = Math.floor(i / cols) * cell + (cell - h) / 2;
        ctx.drawImage(item.canvas, x, y, w, h);
        ctx.fillStyle = 'rgba(0,0,0,0.6)';
        ctx.fillRect(x, y, 46, 16);
        ctx.fillStyle = '#fff';
        ctx.font = '12px sans-serif';
        ctx.fillText(item.label.slice(0, 4), x + 4, y + 12);
      });
      return canvasJpeg(canvas);
    };

    // The sliders match_style owns, per family. A photo is measured with them at zero, so what the match produces
    // does not depend on what a previous run left there and running it twice gives the same result.
    const STYLE_FAMILIES = ['tone', 'color', 'grading', 'saturation'];
    const withoutStyle = (adjustments: Adjustments, groups: Set<string>): Adjustments => {
      const out: any = { ...adjustments };
      const zero = (keys: string[]) => keys.forEach((k) => (out[k] = (INITIAL_ADJUSTMENTS as any)[k]));
      if (groups.has('tone')) zero(['exposure', 'contrast', 'highlights', 'blacks']);
      if (groups.has('color')) zero(['temperature', 'tint']);
      if (groups.has('saturation')) zero(['saturation', 'vibrance']);
      if (groups.has('grading')) {
        const grading: any = adjustments.colorGrading ?? INITIAL_ADJUSTMENTS.colorGrading;
        out.colorGrading = {
          ...grading,
          shadows: { ...grading.shadows, hue: 0, saturation: 0 },
          highlights: { ...grading.highlights, hue: 0, saturation: 0 },
        };
      }
      return out;
    };

    const brief = (look: Look | Targets) => {
      const t = 'luma' in look
        ? { p5: look.luma.p5, p50: look.luma.p50, p95: look.luma.p95, warm: look.mids[0], green: look.mids[1], saturation: look.saturation }
        : { p5: look.p5, p50: look.p50, p95: look.p95, warm: look.mids[0], green: look.mids[1], saturation: look.saturation };
      return {
        shadows_p5: Math.round(t.p5),
        brightness_p50: Math.round(t.p50),
        highlights_p95: Math.round(t.p95),
        warm: round1(t.warm),
        green: round1(t.green),
        saturation_pct: round1(t.saturation),
      };
    };

    // Move one photo's sliders until its render reaches the targets; nothing is written to the photo here.
    // The sliders respond to each other (brightness moves the highlights, contrast moves the blacks), so the work is
    // staged: first exposure, contrast, colour and saturation, then the ends of the tonal range and the shadow and
    // highlight colours. The best candidate of all rounds is kept, never just the last one.
    const solveStyle = async (image: ImageFile, neutral: Adjustments, own: Look, target: Targets, groups: Set<string>) => {
      const knobs = {
        exposure: new Knob(0, -1.5, 1.5, 35, 0.5),
        contrast: new Knob(0, -50, 50, 0.35, 12),
        highlights: new Knob(0, -100, 30, 0.35, 20),
        blacks: new Knob(0, -50, 50, 0.25, 12),
        temperature: new Knob(0, -60, 60, 1.2, 10),
        tint: new Knob(0, -60, 60, -0.8, 10),
        saturation: new Knob(0, -60, 60, 0.25, 12),
      };
      const shadows = new Grade(0.08, 20);
      const highlights = new Grade(0.15, 20);
      const patchOf = () => {
        const patch: Record<string, any> = {};
        if (groups.has('tone')) {
          patch.exposure = Math.round(knobs.exposure.value * 100) / 100;
          patch.contrast = Math.round(knobs.contrast.value);
          patch.highlights = Math.round(knobs.highlights.value);
          patch.blacks = Math.round(knobs.blacks.value);
        }
        if (groups.has('color')) {
          patch.temperature = Math.round(knobs.temperature.value);
          patch.tint = Math.round(knobs.tint.value);
        }
        if (groups.has('saturation')) {
          patch.saturation = Math.round(knobs.saturation.value);
          patch.vibrance = Math.round(knobs.saturation.value * 0.5);
        }
        if (groups.has('grading')) {
          patch.colorGrading = {
            shadows: { hue: shadows.hue, saturation: shadows.saturation },
            highlights: { hue: highlights.hue, saturation: highlights.saturation },
          };
        }
        return patch;
      };
      // Each measured figure with the tolerance inside which it counts as reached. `below` means lower is fine too.
      const errors = (look: Look) => {
        const e: Array<{ name: string; got: number; want: number; tol: number; below?: boolean }> = [];
        const add = (name: string, got: number, want: number, tol: number, below = false) => e.push({ name, got, want, tol, below });
        if (groups.has('tone')) {
          add('brightness', look.luma.p50, target.p50, 4);
          add('contrast', look.luma.p75 - look.luma.p25, target.p75 - target.p25, 8);
          // The ends only count when they are worse than the target: a highlight below it, or blacks deeper than it, is fine.
          add('highlights', look.luma.p95, target.p95, 6, true);
          add('blacks', look.luma.p5, target.p5, 6, true);
        }
        if (groups.has('color')) {
          add('warmth', look.mids[0], target.mids[0], 3);
          add('green-magenta', look.mids[1], target.mids[1], 2.5);
        }
        if (groups.has('saturation')) add('saturation', look.saturation, target.saturation, 3);
        if (groups.has('grading')) {
          add('shadow tint warm', look.shadows[0] - look.mids[0], target.dShadows[0], 4);
          add('shadow tint green', look.shadows[1] - look.mids[1], target.dShadows[1], 4);
          add('highlight tint warm', look.highlights[0] - look.mids[0], target.dHighlights[0], 4);
          add('highlight tint green', look.highlights[1] - look.mids[1], target.dHighlights[1], 4);
        }
        return e;
      };
      const miss = (x: { got: number; want: number; below?: boolean }) => {
        const d = x.got - x.want;
        return x.below ? Math.max(0, d) : Math.abs(d);
      };
      let look = own;
      let best: { patch: Record<string, any>; look: Look; score: number } | null = null;
      let rounds = 0;
      for (;;) {
        const patch = patchOf();
        look = await lookAt(image.path, deepMerge(neutral, patch), MATCH_SIDE);
        rounds++;
        const e = errors(look);
        const score = e.reduce((sum, x) => sum + Math.min(3, miss(x) / x.tol), 0);
        if (!best || score < best.score) best = { patch, look, score };
        if (e.every((x) => miss(x) <= x.tol) || rounds >= MATCH_ROUNDS) break;
        // Exposure first. When the highlights are about to clip, the middle gives way: a flash photo of a dark room is
        // brightened only as far as its highlights allow. The other sliders only learn how they respond while
        // exposure is barely moving, because a big exposure step changes every figure at once.
        const late = rounds >= 3;
        const before = knobs.exposure.value;
        if (groups.has('tone')) {
          const over = look.luma.p95 - HEADROOM;
          knobs.exposure.step(look.luma.p50, over > 0 ? Math.min(target.p50, look.luma.p50 - over * 0.8) : target.p50);
        }
        const calm = Math.abs(knobs.exposure.value - before) < 0.15;
        if (groups.has('tone')) {
          knobs.contrast.step(look.luma.p75 - look.luma.p25, target.p75 - target.p25, 0.5, calm);
          if (late) {
            if (look.luma.p95 > target.p95 + 6) knobs.highlights.step(look.luma.p95, target.p95, 0.6, calm);
            if (look.luma.p5 > target.p5 + 6) knobs.blacks.step(look.luma.p5, target.p5, 0.5, calm);
          }
        }
        if (groups.has('color')) {
          knobs.temperature.step(look.mids[0], target.mids[0], 0.65, calm);
          knobs.tint.step(look.mids[1], target.mids[1], 0.65, calm);
        }
        if (groups.has('saturation')) knobs.saturation.step(look.saturation, target.saturation, 0.65, calm);
        if (groups.has('grading') && late) {
          shadows.step([look.shadows[0] - look.mids[0], look.shadows[1] - look.mids[1]], target.dShadows);
          highlights.step([look.highlights[0] - look.mids[0], look.highlights[1] - look.mids[1]], target.dHighlights);
        }
      }
      const chosen = best!;
      const missed = errors(chosen.look).filter((x) => miss(x) > 2 * x.tol);
      return { patch: chosen.patch, look: chosen.look, rounds, missed: missed.map((x) => `${x.name} ${round1(x.got)} against a target of ${round1(x.want)}`) };
    };

    // Inside of a mask as the render shows it: mean colour, luminance and saturation (what analyze_photo reports).
    const measureMask = async (path: string, maskId: string) => {
      const { base, overlay } = await overlayParts(path, maskId, 400);
      const rep: any = maskReport(base, overlay);
      base.close();
      overlay?.close();
      return rep.inside_mask as { mean_rgb?: number[]; luminance?: { mean: number }; saturation_pct?: number; warm_minus_cool?: number } | undefined;
    };

    // neutral_white: nudge the dress mask's temperature and tint until its inside measures neutral.
    const neutralizeDress = async (path: string, maskId: string, root: string, roots: string[], inner?: JobCtx) => {
      const own = (findMask(maskId).adjustments ?? {}) as any;
      let temperature: number = own.temperature ?? 0;
      let tint: number = own.tint ?? 0;
      let tintGain = 0.4;
      let previousGreen: number | undefined;
      const measured: Array<{ r: number; g: number; b: number }> = [];
      for (let i = 0; i < 3; i++) {
        const inside = await measureMask(path, maskId);
        const rgb = inside?.mean_rgb;
        if (!rgb) break;
        const [r, g, b] = rgb;
        const warm = r - b, green = g - (r + b) / 2;
        measured.push({ r: Math.round(r), g: Math.round(g), b: Math.round(b) });
        if (Math.abs(warm) <= 5 && Math.abs(green) <= 5) break;
        if (previousGreen !== undefined && Math.abs(green) > Math.abs(previousGreen)) tintGain = -tintGain; // wrong direction: flip once
        previousGreen = green;
        temperature = Math.max(-100, Math.min(100, temperature - warm * 0.4));
        tint = Math.max(-100, Math.min(100, tint + green * tintGain));
        await handle('update_mask', { path, root, roots, mask_id: maskId, adjustments: { temperature: Math.round(temperature), tint: Math.round(tint) } }, inner);
      }
      return { temperature: Math.round(temperature), tint: Math.round(tint), measured };
    };

    // Measure one photo for assess_photos. With faces it is opened in the editor (the face model works on the
    // loaded photo) and the sharpness of its largest face is measured next to the whole frame.
    const assessOne = async (p: string, wantFaces: boolean, root: string, roots: string[], inner?: JobCtx): Promise<AssessRow> => {
      const image = entryFor(p);
      const row: AssessRow = { path: relPath(root, image.path), ok: true };
      let adjustments: Adjustments;
      let maskData: string | null = null;
      let note: string | undefined;
      if (wantFaces) {
        await handle('open_photo', { path: p, root, roots }, inner);
        adjustments = editor().adjustments;
        for (let attempt = 0; attempt < 3; attempt++) {
          try {
            const found = await invoke<Record<string, any>>(Invokes.GenerateAiFaceMask, {
              jsAdjustments: getTransformAdjustments(adjustments),
              path: image.path,
              parts: FACE_REGION_PARTS,
              flipHorizontal: adjustments.flipHorizontal,
              flipVertical: adjustments.flipVertical,
              orientationSteps: adjustments.orientationSteps,
              rotation: adjustments.rotation,
              taskId: `assess-${uuidv4()}`,
            });
            if (found.noResult) note = `no face found (${found.noResult})`;
            else maskData = found.maskDataBase64 ?? null;
            break;
          } catch (e) {
            note = `face detection failed (${String(e)})`;
            // Too little free memory is the usual cause and passes; anything else will not.
            if (!/memor|8007000E/i.test(String(e))) break;
            await sleep(3000);
          }
        }
      } else {
        adjustments = await adjustmentsFor(image.path);
      }
      // Original colours (no user grading), uncropped, so the frame lines up with the face mask.
      const plain = { ...getOriginalAdjustments(adjustments), crop: null, aspectRatio: INITIAL_ADJUSTMENTS.aspectRatio } as Adjustments;
      const frame = await render(image.path, plain, wantFaces ? ASSESS_SIDE_FACES : ASSESS_SIDE);
      const W = frame.width;
      const H = frame.height;
      const { canvas, ctx } = canvasFrom(W, H);
      ctx.drawImage(frame, 0, 0);
      frame.close();
      const luma = lumaPlane(ctx.getImageData(0, 0, W, H).data, W * H);
      const stats = statsOf(canvas);
      // Noise looks like detail to a Laplacian, so the flattest tiles set a floor that is taken off every figure.
      const tiles = tileEnergies(luma, W, H);
      const floor = quantileOf(tiles, 0.1);
      row.noise_floor = round1(floor);
      row.sharp_global = round1(Math.max(0, laplacianEnergy(luma, W, H, null).energy - floor));
      row.sharp_region = round1(Math.max(0, quantileOf(tiles, 0.9) - floor));
      row.luma_mean = stats?.luminance.mean;
      row.clipped_shadows_pct = stats?.clipped_shadows_pct;
      row.clipped_highlights_pct = stats?.clipped_highlights_pct;
      row.faces = 0;
      row.sharp_face = null;
      if (maskData) {
        const maskBitmap = await createImageBitmap(await (await fetch(maskData)).blob());
        if (Math.abs(maskBitmap.width / maskBitmap.height - W / H) > 0.03) {
          note = 'the face mask and the photo do not line up (rotated or odd aspect); face sharpness skipped';
        } else {
          const g = canvasFrom(W, H);
          g.ctx.drawImage(maskBitmap, 0, 0, W, H);
          const px = g.ctx.getImageData(0, 0, W, H).data;
          const gray = new Uint8Array(W * H);
          for (let i = 0; i < gray.length; i++) gray[i] = px[i * 4];
          const faces = faceSharpness(gray, luma, W, H);
          row.faces = faces.length;
          if (faces.length) {
            row.sharp_face = round1(Math.max(0, faces[0].sharp - floor));
            row.face_area_pct = faces[0].area_pct;
            // Faces far softer than the rest of the frame mean the focus landed elsewhere.
            row.face_vs_frame = row.sharp_region! > 1 ? Math.round((row.sharp_face / row.sharp_region!) * 100) / 100 : null;
          } else {
            note = 'the face found is too small to measure; ranked on the sharpest region';
          }
        }
        maskBitmap.close();
      }
      if (note) row.note = note;
      return row;
    };

    // Requests the bridge asked to stop, and what each running request is (an export is stopped by the app).
    const stopRequests = new Set<string>();
    const running = new Map<string, string>();

    const handle = async (op: string, args: Args, job?: JobCtx): Promise<unknown> => {
      const root: string = args.root;
      ROOTS = Array.isArray(args.roots) ? args.roots : [args.root];
      const h = handlersRef.current;
      const tools = toolsRef.current;

      switch (op) {
        case 'status': {
          const { selectedImage, historyIndex, history, zoom, compareView } = editor();
          const lib = library();
          const visible = visibleList();
          return {
            bridge_version: 3,
            view: useUIStore.getState().activeView,
            folder: lib.currentFolderPath ? relPath(root, lib.currentFolderPath) : null,
            photos_total: lib.imageList.length,
            photos_visible: visible.length,
            selection: lib.multiSelectedPaths.map((p) => relPath(root, p)),
            active_photo: selectedImage ? relPath(root, selectedImage.path) : null,
            active_ready: !!selectedImage?.isReady,
            filter: lib.filterCriteria,
            sort: lib.sortCriteria,
            zoom,
            compare_view: compareView,
            can_undo: historyIndex > 0,
            can_redo: historyIndex < history.length - 1,
          };
        }

        case 'open_folder': {
          const folder: string = args.folder;
          const lib = library();
          if (!lib.rootPaths.includes(folder) && isRoot(folder)) {
            lib.setLibrary({ rootPaths: [...lib.rootPaths, folder] });
            // Same as the "Open folder" button: the new root belongs to the saved session.
            const { appSettings, handleSettingsChange } = useSettingsStore.getState();
            if (appSettings) handleSettingsChange({ ...appSettings, rootFolders: [...lib.rootPaths, folder] } as any);
            try {
              const tree = await invoke(Invokes.GetFolderTree, {
                path: folder,
                expandedFolders: [folder],
                showImageCounts: false,
              });
              library().setLibrary((s) => ({ folderTrees: [...s.folderTrees, tree] }));
            } catch (e) {
              console.warn('MCP: folder tree unavailable', e);
            }
          }
          await h.handleSelectSubfolder(folder, isRoot(folder));
          const deadline = Date.now() + 30000;
          await sleep(150);
          while (library().isViewLoading && Date.now() < deadline) await sleep(100);
          const total = library().imageList.length;
          const visible = visibleList().length;
          return {
            folder: relPath(root, folder) || '.',
            photos_total: total,
            photos_visible: visible,
            ...(visible < total
              ? { warning: `${total - visible} photo(s) are hidden by the active filter (${JSON.stringify(library().filterCriteria)}). Call set_filter with clear true to see them all.` }
              : {}),
          };
        }

        case 'list_photos': {
          const scope = args.scope === 'all' ? 'all' : 'visible';
          const list = scope === 'all' ? library().imageList : visibleList();
          const offset = Math.max(0, Number(args.offset) || 0);
          const limit = Math.min(MAX_PAGE, Math.max(1, Number(args.limit) || 100));
          return {
            scope,
            total: list.length,
            offset,
            items: list.slice(offset, offset + limit).map((img) => summary(root, img)),
          };
        }

        case 'select_photos': {
          let targets: ImageFile[];
          if (Array.isArray(args.paths)) targets = args.paths.map((p: string) => entryFor(p));
          else if (args.scope === 'visible') targets = visibleList();
          else if (args.scope === 'all') targets = library().imageList;
          else if (args.scope === 'none') targets = [];
          else return fail('Provide paths or scope (visible, all, none)');
          const paths = targets.map((t) => t.path);
          library().setLibrary({
            multiSelectedPaths: paths,
            libraryActivePath: paths[0] ?? null,
            selectionAnchorPath: paths[0] ?? null,
          });
          return { selected: paths.length, paths: paths.slice(0, MAX_PAGE).map((p) => relPath(root, p)) };
        }

        case 'open_photo': {
          const image = entryFor(args.path);
          library().setLibrary({
            multiSelectedPaths: [image.path],
            libraryActivePath: image.path,
            selectionAnchorPath: image.path,
          });
          await h.handleImageSelect(image.path, true);
          const s = await waitReady(image.path);
          const outOfSync = await waitInSync(image.path);
          if (outOfSync) {
            note(`open_photo OUT OF SYNC ${relPath(root, image.path)}: ${outOfSync}`);
            fail(`The photo opened but ${outOfSync}; do not edit it yet. Call open_photo again.`);
          }
          // A RAW whose only readable image is its embedded thumbnail (160x120) opens without an error.
          const previewOnly = s.isRaw && Math.max(s.width, s.height) < 800;
          return {
            ...summary(root, image),
            width: s.width,
            height: s.height,
            revision: revisionOf(editor().adjustments),
            ...(previewOnly
              ? { preview_only: true, warning: `The file decoded to only a ${s.width}x${s.height} preview: it is damaged or has no full image. Edits and exports of it would be tiny; do not rate it by its look.` }
              : {}),
          };
        }

        case 'get_photo': {
          const image = entryFor(args.path);
          const s = editor().selectedImage;
          const active = !!s && norm(s.path) === norm(image.path);
          return {
            ...summary(root, image),
            active,
            ...(active && s!.isReady
              ? {
                  width: s!.width,
                  height: s!.height,
                  revision: revisionOf(editor().adjustments),
                  history_index: editor().historyIndex,
                  history_length: editor().history.length,
                  masks: (editor().adjustments.masks || []).length,
                }
              : {}),
          };
        }

        case 'set_rating': {
          const stars = args.stars;
          if (!Number.isInteger(stars) || stars < 0 || stars > 5) fail('stars must be an integer from 0 to 5');
          return metadataOp(root, args, Invokes.SetRatingForPaths, (paths) => ({ paths, rating: stars }), (m) => m.rating === stars);
        }

        case 'set_flag': {
          const flag = args.flag === 'pick' ? ImageFlag.Pick : args.flag === 'reject' ? ImageFlag.Reject : null;
          if (args.flag != null && flag === null) fail("flag must be 'pick', 'reject' or null");
          return metadataOp(root, args, Invokes.SetFlagForPaths, (paths) => ({ paths, flag }), (m) => (m.flag ?? null) === flag);
        }

        case 'set_color': {
          const color: string | null = args.color ?? null;
          if (color !== null && !COLOR_LABELS.some((c) => c.name === color)) {
            fail(`color must be one of ${COLOR_LABELS.map((c) => c.name).join(', ')} or null`);
          }
          return metadataOp(
            root,
            args,
            Invokes.SetColorLabelForPaths,
            (paths) => ({ paths, color }),
            (m) => ((m.tags as string[] | null)?.find((t) => t.startsWith('color:'))?.substring(6) ?? null) === color,
          );
        }

        case 'set_filter': {
          const criteria: Args = {};
          if (args.clear) Object.assign(criteria, { colors: [], rating: 0, ratingOperator: 'gte', rawStatus: 'all', editedStatus: 'all', flagStatus: 'all' });
          if (args.rating !== undefined) criteria.rating = args.rating;
          if (args.rating_operator !== undefined) criteria.ratingOperator = args.rating_operator;
          if (args.colors !== undefined) criteria.colors = args.colors;
          if (args.raw_status !== undefined) criteria.rawStatus = args.raw_status;
          if (args.edited_status !== undefined) criteria.editedStatus = args.edited_status;
          if (args.flag_status !== undefined) criteria.flagStatus = args.flag_status;
          library().setFilterCriteria(criteria);
          await sleep(100);
          return { filter: library().filterCriteria, photos_visible: visibleList().length };
        }

        case 'trash': {
          if (!Array.isArray(args.paths) || args.paths.length === 0) fail('paths is required');
          if (args.paths.length > 200) fail('At most 200 photos per trash request');
          const targets = args.paths.map((p: string) => entryFor(p).path);
          await h.executeDelete(targets, { includeAssociated: false });
          return { requested: targets.length };
        }

        case 'copy_adjustments': {
          await h.handleCopyAdjustments(entryFor(args.path).path);
          return { copied_from: relPath(root, entryFor(args.path).path), keys: Object.keys(editor().copiedAdjustments || {}).length };
        }

        case 'paste_adjustments': {
          const copied = editor().copiedAdjustments as any;
          if (!copied) fail('Nothing copied; call copy_adjustments first');
          const targets = (args.paths as string[]).map((p) => entryFor(p).path);
          // What the paste is going to write: same rule as handlePasteAdjustments. Masks and lens data are
          // rewritten on the way in (new ids, resolved lens), so they are not part of the check.
          const rules = useSettingsStore.getState().appSettings?.copyPasteSettings;
          const expected: Record<string, unknown> = {};
          for (const key of rules?.includedAdjustments ?? []) {
            if (!Object.prototype.hasOwnProperty.call(copied, key)) continue;
            if (['masks', 'aiPatches', 'lensMaker', 'lensModel', 'lensDistortionParams'].includes(key)) continue;
            const value = copied[key];
            if (rules?.mode === PasteMode.Merge && JSON.stringify(value) === JSON.stringify((INITIAL_ADJUSTMENTS as any)[key])) continue;
            expected[key] = value;
          }
          h.handlePasteAdjustments(targets);
          const files = await confirmSaved(targets, (saved) => isSubset(expected, saved));
          const failedFiles = files.filter((f) => !f.ok);
          return {
            ok: failedFiles.length === 0,
            pasted_keys: Object.keys(expected),
            ...(files.length > 20 ? { count: files.length, failed: failedFiles } : { files }),
          };
        }

        case 'set_view': {
          if (args.zoom !== undefined) {
            if (args.zoom === 'fit') h.handleZoomChange(1, true);
            else if (typeof args.zoom === 'number') h.handleZoomChange(args.zoom, false);
            else fail("zoom must be 'fit' or a number (1 = 100%)");
          }
          if (args.compare !== undefined) {
            const mode = args.compare;
            if (mode === 'off') editor().setEditor({ showOriginal: false, previewOverride: null, compareView: 'off' });
            else if (['toggle', 'vertical', 'horizontal', 'sideBySide'].includes(mode)) tools.selectCompareMode(mode);
            else fail('compare must be off, toggle, vertical, horizontal or sideBySide');
          }
          await sleep(150);
          return { zoom: editor().zoom, compare_view: editor().compareView, show_original: editor().showOriginal };
        }

        case 'get_adjustments': {
          activeReady(args.path);
          const { masks, aiPatches, ...rest } = editor().adjustments as any;
          const changed = Object.keys(rest).filter(
            (k) => JSON.stringify(rest[k]) !== JSON.stringify((INITIAL_ADJUSTMENTS as any)[k]),
          );
          return {
            revision: revisionOf(editor().adjustments),
            changed_keys: changed,
            adjustments: rest,
            masks: (masks || []).length,
            ai_patches: (aiPatches || []).length,
          };
        }

        case 'set_adjustments': {
          const s = activeReady(args.path);
          const patch = args.patch;
          if (!isPlainObject(patch) || Object.keys(patch).length === 0) fail('patch must be a non-empty object');
          const current = editor().adjustments as any;
          if (args.expected_revision && args.expected_revision !== revisionOf(current)) {
            fail(`Stale revision: the photo changed (now ${revisionOf(current)}). Read get_adjustments and retry.`);
          }
          for (const [key, value] of Object.entries(patch)) {
            if (FORBIDDEN_PATCH_KEYS.has(key)) fail(`${key} are edited with the mask tools, not set_adjustments`);
            if (!(key in INITIAL_ADJUSTMENTS)) fail(`Unknown adjustment: ${key}`);
            const reference = (INITIAL_ADJUSTMENTS as any)[key];
            if (typeof reference === 'number' && typeof value !== 'number') fail(`${key} must be a number`);
            if (typeof reference === 'boolean' && typeof value !== 'boolean') fail(`${key} must be a boolean`);
          }
          const clamped: Record<string, { asked: number; used: number }> = {};
          for (const [key, value] of Object.entries(patch)) {
            const range = ADJUSTMENT_RANGES[key];
            if (range && typeof value === 'number' && (value < range[0] || value > range[1])) {
              const used = Math.min(range[1], Math.max(range[0], value));
              patch[key] = used;
              clamped[key] = { asked: value, used };
            }
          }
          const before = revisionOf(current);
          const merged = deepMerge(current, patch);
          tools.setAdjustments(() => merged);
          await settle();
          const saved = await persist(s.path, patch);
          return {
            applied: true,
            ...(Object.keys(clamped).length ? { clamped } : {}),
            ...saved,
            revision: revisionOf(editor().adjustments),
            previous_revision: before,
            history_index: editor().historyIndex,
          };
        }

        case 'reset_adjustments': {
          const targets = (args.paths as string[]).map((p) => entryFor(p).path);
          h.handleResetAdjustments(targets);
          const initial = revisionOf(INITIAL_ADJUSTMENTS);
          const files = await confirmSaved(targets, (saved) => (saved.masks || []).length === 0 && revisionOf(saved) === initial);
          return { ok: files.every((f) => f.ok), files };
        }

        case 'undo':
        case 'redo': {
          const s = activeReady(args.path);
          debouncedSetHistory.flush();
          const before = editor().historyIndex;
          if (op === 'undo') editor().undo();
          else editor().redo();
          if (editor().historyIndex === before) fail(op === 'undo' ? 'Nothing to undo' : 'Nothing to redo');
          await settle();
          const saved = await persist(s.path);
          return { ...saved, revision: revisionOf(editor().adjustments), history_index: editor().historyIndex, history_length: editor().history.length };
        }

        case 'preview': {
          const image = entryFor(args.path);
          const adjustments = await adjustmentsFor(image.path);
          const maxDim = Math.min(2400, Math.max(200, Number(args.max_dimension) || 1200));
          const compare: string = args.compare || 'after';
          const region = args.region;
          const targetAdjustments = compare === 'before' ? getOriginalAdjustments(adjustments) : adjustments;
          const sources: Array<{ label: string; bitmap: ImageBitmap }> = [];
          const renderDim = region ? 0 : maxDim;
          if (compare === 'before' || compare === 'after') {
            sources.push({ label: compare, bitmap: await render(image.path, targetAdjustments, renderDim) });
          } else if (compare === 'side_by_side') {
            sources.push({ label: 'before', bitmap: await render(image.path, getOriginalAdjustments(adjustments), renderDim) });
            sources.push({ label: 'after', bitmap: await render(image.path, adjustments, renderDim) });
          } else fail("compare must be 'after', 'before' or 'side_by_side'");

          const cropped = sources.map(({ bitmap }) => {
            let sx = 0, sy = 0, sw = bitmap.width, sh = bitmap.height;
            if (region) {
              const { x, y, width, height } = region;
              if ([x, y, width, height].some((v) => typeof v !== 'number') || width <= 0 || height <= 0 || x < 0 || y < 0 || x + width > 1.0001 || y + height > 1.0001) {
                fail('region needs normalized x, y, width, height within 0..1');
              }
              sx = Math.round(x * bitmap.width);
              sy = Math.round(y * bitmap.height);
              sw = Math.max(1, Math.round(width * bitmap.width));
              sh = Math.max(1, Math.round(height * bitmap.height));
            }
            const k = Math.min(1, maxDim / Math.max(sw, sh));
            return { bitmap, sx, sy, sw, sh, dw: Math.round(sw * k), dh: Math.round(sh * k) };
          });
          const totalWidth = cropped.reduce((sum, c) => sum + c.dw, 0);
          const totalHeight = Math.max(...cropped.map((c) => c.dh));
          const { canvas, ctx } = canvasFrom(totalWidth, totalHeight);
          let cursor = 0;
          for (const c of cropped) {
            ctx.drawImage(c.bitmap, c.sx, c.sy, c.sw, c.sh, cursor, 0, c.dw, c.dh);
            cursor += c.dw;
          }
          const active = editor().selectedImage;
          const analysis = Object.fromEntries(
            cropped.map((c, i) => {
              const small = smallCanvas(c.bitmap, c.sx, c.sy, c.sw, c.sh);
              return [sources[i].label, { ...statsOf(small), luma_grid_3x3: lumaGrid(small) }];
            }),
          );
          const incomplete = sources.some(({ bitmap }) => looksIncomplete(bitmap));
          // Full-size decoded frames hold tens of megabytes each; do not wait for the garbage collector.
          sources.forEach(({ bitmap }) => bitmap.close());
          return {
            path: relPath(root, image.path),
            revision: revisionOf(adjustments),
            compare,
            analysis,
            region: region ?? null,
            ...(incomplete ? { warning: 'The render still looks incomplete (a blank white edge) after three tries; ask again before judging this photo.' } : {}),
            width: canvas.width,
            height: canvas.height,
            native_size: { width: cropped[0].sw, height: cropped[0].sh },
            source: active && norm(active.path) === norm(image.path) ? 'editor' : 'saved',
            jpeg: await canvasJpeg(canvas),
          };
        }

        case 'list_masks': {
          activeReady(args.path);
          const full = args.detail === 'full';
          return { revision: revisionOf(editor().adjustments), masks: (editor().adjustments.masks || []).map((m) => describeMask(m, full)) };
        }

        case 'add_mask': {
          const s = activeReady(args.path);
          const specs: Args[] = args.sub_masks;
          if (!Array.isArray(specs) || specs.length === 0) fail('sub_masks must list at least one component');
          const dim = imageSpace();
          const adjPatch = args.adjustments ?? {};
          for (const key of Object.keys(adjPatch)) {
            if (!(key in INITIAL_MASK_ADJUSTMENTS)) fail(`Unknown mask adjustment: ${key}`);
          }
          const allowed = new Set<string>([Mask.Linear, Mask.Radial, Mask.Brush, Mask.All, Mask.AiSubject, Mask.AiForeground, Mask.AiSky, Mask.AiText, Mask.AiFace, 'ai-dress']);
          const generators: Array<() => Promise<void>> = [];
          const containerId = uuidv4();
          const dressReports: Record<string, unknown>[] = [];
          const subMasks = specs.map((spec) => {
            if (!allowed.has(spec.type)) fail(`Unsupported mask type ${spec.type}`);
            const isDress = spec.type === 'ai-dress';
            const sm: any = createSubMask(isDress ? Mask.AiText : spec.type, { width: dim.width, height: dim.height }, spec.mode);
            const params = spec.params ?? {};
            if (isDress) {
              const options: DressCleanOptions = {
                region: toBox(params.region),
                exclude: Array.isArray(params.exclude) ? params.exclude.map(toBox).filter((b: Box | undefined): b is Box => !!b) : [],
                chroma_scale: typeof params.chroma_scale === 'number' ? params.chroma_scale : undefined,
                dark_ratio: typeof params.dark_ratio === 'number' ? params.dark_ratio : undefined,
              };
              sm.parameters = { ...sm.parameters, prompt: params.prompt || DRESS_PROMPT, grow: params.grow ?? 0, feather: params.feather ?? 0.1 };
              generators.push(async () => {
                dressReports.push(await dressSilhouette(s.path, containerId, sm.id, params.prompt, options));
              });
            } else if (spec.type === Mask.Brush) {
              sm.parameters = { lines: toLines(params.lines ?? [], dim) };
            } else if (spec.type === Mask.AiText) {
              if (!params.prompt) fail('ai-text needs params.prompt');
              sm.parameters = { ...sm.parameters, prompt: params.prompt, grow: params.grow ?? 0, feather: params.feather ?? 0 };
              generators.push(() => tools.ai.handleGenerateAiTextMask(sm.id, params.prompt));
            } else if (spec.type === Mask.AiFace) {
              const parts: string[] = params.parts ?? ['faceSkin'];
              if (parts.some((p) => !(FACE_PARTS as readonly string[]).includes(p))) fail(`parts must be among ${FACE_PARTS.join(', ')}`);
              sm.parameters = { ...sm.parameters, parts, grow: params.grow ?? 0, feather: params.feather ?? 0 };
              generators.push(() => tools.ai.handleGenerateAiFaceMask(sm.id, parts));
            } else if (spec.type === Mask.AiForeground) {
              sm.parameters = { ...sm.parameters, grow: params.grow ?? 0, feather: params.feather ?? 0 };
              generators.push(() => tools.ai.handleGenerateAiForegroundMask(sm.id));
            } else if (spec.type === Mask.AiSky) {
              sm.parameters = { ...sm.parameters, grow: params.grow ?? 0, feather: params.feather ?? 0 };
              generators.push(() => tools.ai.handleGenerateAiSkyMask(sm.id));
            } else if (spec.type === Mask.AiSubject) {
              const box = params.box;
              if (!box) fail('ai-subject needs params.box {x1,y1,x2,y2} (fractions) around the subject');
              sm.parameters = { ...sm.parameters, grow: params.grow ?? 0, feather: params.feather ?? 0 };
              generators.push(() =>
                tools.ai.handleGenerateAiMask(sm.id, { x: box.x1 * dim.width, y: box.y1 * dim.height }, { x: box.x2 * dim.width, y: box.y2 * dim.height }),
              );
            } else {
              sm.parameters = toPixelParams(spec.type, { ...sm.parameters, ...params }, dim);
            }
            if (typeof spec.invert === 'boolean') sm.invert = spec.invert;
            if (typeof spec.opacity === 'number') sm.opacity = spec.opacity;
            return sm as SubMask;
          });
          const container: MaskContainer = {
            ...INITIAL_MASK_CONTAINER,
            id: containerId,
            name: args.name || 'MCP mask',
            invert: !!args.invert,
            opacity: typeof args.opacity === 'number' ? args.opacity : 100,
            adjustments: deepMerge(INITIAL_MASK_ADJUSTMENTS, adjPatch),
            subMasks,
          };
          // Replacing in the same step keeps one dress mask per photo even if the editor state is stale.
          const replaced = new Set<string>(typeof args.replace_mask_id === 'string' ? [args.replace_mask_id] : []);
          if (typeof args.replace_name === 'string') {
            const names = [args.replace_name, LEGACY_MASK_NAMES[args.replace_name]];
            for (const m of editor().adjustments.masks || []) if (names.includes(m.name)) replaced.add(m.id);
          }
          // A failed AI step must not cost the photo its good mask: with AI components the old mask stays until the new one has data.
          const deferRemoval = generators.length > 0 && replaced.size > 0;
          setMasks((masks) => [...(deferRemoval ? masks : masks.filter((m) => !replaced.has(m.id))), container]);
          await settle();
          for (const sm of subMasks) aiFailures.delete(sm.id);
          for (const [step, generate] of generators.entries()) {
            job?.report({ done: step, total: generators.length, last: 'segmenting' });
            await generate();
            // The original of a big RAW may still be loading when the segmentation asks for it, and memory can be
            // short for a moment: both pass, so wait and ask again before the photo counts as failed. (The dress
            // component does its own retries.)
            for (let attempt = 1; attempt <= 3 && dressReports.length === 0; attempt++) {
              const current = findMask(container.id);
              const transient = subMasks.some(
                (sm: SubMask) =>
                  !current.subMasks.find((x: SubMask) => x.id === sm.id)?.parameters?.maskDataBase64 &&
                  /memor|8007000E|bad alloc|No original image loaded/i.test(aiFailures.get(sm.id) ?? ''),
              );
              if (!transient) break;
              note(`AI mask hit a transient error, retry ${attempt}`);
              await sleep(3000 * attempt);
              for (const sm of subMasks) aiFailures.delete(sm.id);
              await generate();
            }
          }
          const result = findMask(container.id);
          const aiIds = new Set(specs.map((spec, i) => (String(spec.type).startsWith('ai-') ? subMasks[i].id : null)).filter(Boolean));
          const missing = result.subMasks.filter((sm: SubMask) => aiIds.has(sm.id) && !sm.parameters?.maskDataBase64);
          const causes = missing.map((sm: SubMask) => aiFailures.get(sm.id)).filter(Boolean);
          let check: Record<string, unknown> | undefined;
          // Any AI mask gets its coverage checked: a segmentation that returns data but selects nothing is a failure too.
          if ((dressReports.length || aiIds.size > 0) && args.check !== false) {
            // Numbers an agent without eyes can act on: where the mask landed and what it covers.
            const { base, overlay } = await overlayParts(s.path, container.id, 400);
            const rep: any = maskReport(base, overlay);
            base.close();
            overlay?.close();
            check = {
              coverage_pct: rep.coverage_pct,
              bbox: rep.bbox,
              empty: rep.empty,
              inside_saturation_pct: rep.inside_mask?.saturation_pct ?? null,
              inside_warm_minus_cool: rep.inside_mask?.warm_minus_cool ?? null,
            };
          }
          // Transaction: a mask with no data (or an empty dress) is rolled back, and the mask it was to replace stays.
          const noMask = generators.length > 0 && (missing.length > 0 || dressReports.some((r: any) => r.found === false) || (check as any)?.empty === true);
          if (noMask || deferRemoval) {
            setMasks((masks) => masks.filter((m) => (noMask ? m.id !== container.id : !replaced.has(m.id))));
            await settle();
          }
          const saved = await persist(s.path);
          if (noMask) {
            const why = causes.length ? causes.join('; ') : (dressReports.find((r: any) => r.warning) as any)?.warning ?? 'the segmentation found nothing';
            fail(`[mask_not_created] No mask was added (${why}).${replaced.size ? ` The previous mask "${args.replace_name ?? [...replaced][0]}" was kept untouched.` : ''}`);
          }
          return {
            mask_id: container.id,
            sub_mask_ids: subMasks.map((sm) => sm.id),
            masks_total: (editor().adjustments.masks || []).length,
            ...(replaced.size ? { replaced: [...replaced] } : {}),
            ...saved,
            revision: revisionOf(editor().adjustments),
            ...(dressReports.length ? { dress: dressReports.length === 1 ? dressReports[0] : dressReports, ...(check ? { check } : {}) } : {}),
            ...(missing.length
              ? { warning: `Segmentation produced no mask for ${missing.length} component(s)${causes.length ? ` (${causes.join('; ')})` : ''}; inspect with mask_overlay` }
              : {}),
          };
        }

        case 'update_mask': {
          const s = activeReady(args.path);
          const existing = findMask(args.mask_id);
          const dim = imageSpace();
          if (args.adjustments) {
            for (const key of Object.keys(args.adjustments)) {
              if (!(key in INITIAL_MASK_ADJUSTMENTS)) fail(`Unknown mask adjustment: ${key}`);
            }
          }
          const updates: Args[] = args.sub_masks ?? [];
          for (const u of updates) if (!existing.subMasks.some((sm: SubMask) => sm.id === u.id)) fail(`Unknown sub-mask id: ${u.id}`);
          setMasks((masks) =>
            masks.map((m) => {
              if (m.id !== args.mask_id) return m;
              return {
                ...m,
                ...(typeof args.name === 'string' ? { name: args.name } : {}),
                ...(typeof args.invert === 'boolean' ? { invert: args.invert } : {}),
                ...(typeof args.opacity === 'number' ? { opacity: args.opacity } : {}),
                ...(typeof args.visible === 'boolean' ? { visible: args.visible } : {}),
                adjustments: args.adjustments ? deepMerge(m.adjustments, args.adjustments) : m.adjustments,
                subMasks: m.subMasks.map((sm) => {
                  const u = updates.find((x) => x.id === sm.id);
                  if (!u) return sm;
                  let parameters = sm.parameters;
                  if (u.params) parameters = { ...parameters, ...toPixelParams(sm.type, u.params, dim) };
                  if (u.add_lines) parameters = { ...parameters, lines: [...(parameters?.lines ?? []), ...toLines(u.add_lines, dim)] };
                  if (u.clear_lines) parameters = { ...parameters, lines: [] };
                  return {
                    ...sm,
                    ...(u.mode ? { mode: u.mode } : {}),
                    ...(typeof u.invert === 'boolean' ? { invert: u.invert } : {}),
                    ...(typeof u.opacity === 'number' ? { opacity: u.opacity } : {}),
                    ...(typeof u.visible === 'boolean' ? { visible: u.visible } : {}),
                    parameters,
                  };
                }),
              };
            }),
          );
          await settle();
          const saved = await persist(s.path);
          return { ...saved, mask: describeMask(findMask(args.mask_id)), revision: revisionOf(editor().adjustments) };
        }

        case 'delete_mask': {
          const s = activeReady(args.path);
          findMask(args.mask_id);
          setMasks((masks) => masks.filter((m) => m.id !== args.mask_id));
          await settle();
          const saved = await persist(s.path);
          return { ...saved, remaining: (editor().adjustments.masks || []).length, revision: revisionOf(editor().adjustments) };
        }

        case 'select_dress': {
          const paths: string[] = args.paths;
          if (!Array.isArray(paths) || paths.length === 0) fail('paths must list at least one photo');
          if (paths.length > 60) fail('Too many photos in one call (max 60)');
          const name: string = args.name || 'Wedding dress';
          const adjustments = args.adjustments ?? { hsl: { blues: { saturation: -100 }, aquas: { saturation: -60 }, purples: { saturation: -40 } } };
          const params: Args = {};
          for (const key of ['prompt', 'region', 'exclude', 'chroma_scale', 'dark_ratio', 'grow', 'feather']) {
            if (args[key] !== undefined) params[key] = args[key];
          }
          const wantSheet = args.preview === true && paths.length <= 24;
          const results: Args[] = [];
          const sheet: Array<{ label: string; canvas: HTMLCanvasElement }> = [];
          // The steps inside report nothing themselves: this loop owns the progress (photos done of total).
          const inner: JobCtx | undefined = job && { ...job, report: () => {} };
          let stopped = false;
          for (const p of paths) {
            if (job?.stopped()) {
              stopped = true;
              break;
            }
            const label = relPath(root, p);
            try {
              await handle('open_photo', { path: p, root, roots: args.roots }, inner);
              const made: any = await handle('add_mask', {
                path: p, root, roots: args.roots, name, replace_name: name, adjustments, sub_masks: [{ type: 'ai-dress', params }],
              }, inner);
              const dress = made.dress ?? {};
              const ok = dress.found !== false && made.check?.empty !== true;
              const tuned = ok && args.mode === 'neutral_white' ? await neutralizeDress(p, made.mask_id, root, args.roots, inner) : undefined;
              results.push({
                path: label,
                ok,
                ...(tuned ? { neutral_white: tuned } : {}),
                mask_id: made.mask_id,
                masks_total: made.masks_total,
                coverage_pct: made.check?.coverage_pct ?? null,
                bbox: made.check?.bbox ?? null,
                removed_pct: dress.kept_pct !== undefined ? Math.round((100 - dress.kept_pct) * 10) / 10 : null,
                pieces_dropped: dress.pieces_dropped ?? null,
                ...(dress.warning || made.warning ? { warning: dress.warning ?? made.warning } : {}),
                ...(dress.skipped ? { note: dress.skipped } : {}),
              });
              if (wantSheet && ok) sheet.push({ label, canvas: (await overlayImage(p, made.mask_id, 360)).canvas });
            } catch (e) {
              results.push({ path: label, ok: false, error: e instanceof Error ? e.message : String(e) });
            }
            job?.report({ done: results.length, total: paths.length, last: label }, results);
          }
          const jpeg = sheet.length ? await contactSheet(sheet) : undefined;
          return {
            photos: paths.length,
            done: results.filter((r) => r.ok).length,
            failed: results.filter((r) => !r.ok).length,
            ...(stopped ? { cancelled: true, not_started: paths.length - results.length } : {}),
            results,
            ...(jpeg ? { jpeg } : {}),
          };
        }

        case 'look': {
          const reference = typeof args.reference === 'string' ? args.reference : '';
          if (!reference) fail('reference must name an image');
          const adjustments = await adjustmentsFor(reference);
          const bitmap = await render(reference, adjustments, 640);
          const small = smallCanvas(bitmap, 0, 0, bitmap.width, bitmap.height, MATCH_SIDE);
          const smallData = (small.getContext('2d') as CanvasRenderingContext2D).getImageData(0, 0, small.width, small.height).data;
          const look = lookOf(smallData, small.width * small.height);
          const palette = paletteOf(smallData, small.width * small.height);
          const { canvas, ctx } = canvasFrom(bitmap.width, bitmap.height);
          ctx.drawImage(bitmap, 0, 0);
          bitmap.close();
          return {
            reference: relPath(root, reference),
            palette: paletteNumbers(palette),
            palette_in_words: describePalette(palette),
            tone: { ...lookNumbers(look), description: describeLook(look) },
            how_to_read:
              'LOOK AT THE PICTURE FIRST, then use the numbers. palette is what makes a grade: whites and blacks are measured on near-neutral pixels, so their cast is the cast of the light and of the grade, not of the objects. cast is [warm, green]: warm is red minus blue (positive orange, negative blue), green is green above the red-blue average (negative magenta). colours lists each colour family the editor can adjust (reds, oranges, yellows, greens, aquas, blues, purples, magentas): share of the coloured area, saturation, luminance and average hue in degrees. tone.cast per zone reads the objects too (a field makes the midtones green), so do not copy it. Read how the reference was made, write the recipe in photographic terms, then work photo by photo with compare_to_reference and style_photo.',
            jpeg: await canvasJpeg(canvas),
          };
        }

        case 'style_photo': {
          const reference = typeof args.reference === 'string' ? args.reference : '';
          if (!reference) fail('reference must name the image whose look is copied');
          const image = entryFor(args.path);
          const wantsChange = isPlainObject(args.patch) || args.reset === true;
          if (args.patch !== undefined && !isPlainObject(args.patch)) fail('patch must be an object of adjustments');
          const ref = await readAt(reference, await adjustmentsFor(reference), MATCH_SIDE);
          const before = await readAt(image.path, await adjustmentsFor(image.path), MATCH_SIDE);
          let after = before;
          let saved: Record<string, unknown> | undefined;
          if (wantsChange) {
            await handle('open_photo', { path: image.path, root, roots: args.roots });
            const merged = deepMerge(args.reset === true ? lookResetPatch() : {}, isPlainObject(args.patch) ? args.patch : {});
            if (Object.keys(merged).length === 0) fail('nothing to apply');
            saved = (await handle('set_adjustments', { path: image.path, root, roots: args.roots, patch: merged })) as Record<string, unknown>;
            after = await readAt(image.path, editor().adjustments, MATCH_SIDE);
          }
          const items = [{ label: 'REF', canvas: ref.canvas }];
          if (wantsChange) items.push({ label: 'BEF', canvas: before.canvas });
          items.push({ label: wantsChange ? 'AFT' : 'NOW', canvas: after.canvas });
          return {
            path: relPath(root, image.path),
            reference: relPath(root, reference),
            ...(wantsChange ? { applied: { reset: args.reset === true, patch: args.patch ?? {}, saved: saved?.saved === true, ...(saved?.clamped ? { clamped: saved.clamped } : {}), ...(saved?.save_problem ? { problem: saved.save_problem } : {}) } } : {}),
            reference_palette: paletteNumbers(ref.palette),
            ...(wantsChange ? { before_palette: paletteNumbers(before.palette) } : {}),
            palette: paletteNumbers(after.palette),
            tone: { reference: lookNumbers(ref.look), photo: lookNumbers(after.look) },
            to_get_closer: adviceFor(after.palette, ref.palette),
            how_to_read:
              'The picture shows the reference, then (after a change) the photo before, then the photo now. Judge it by eye as a photographer would: do the whites and blacks feel like the reference, are the colours as pleasant, does it still look like a natural photo of this scene and this light and not a filter? to_get_closer lists measured differences as suggestions, not orders: some do not apply to this photo (a night flash photo is not golden hour). Hue sliders: check the direction in the picture.',
            jpeg: await contactSheet(items, 480),
          };
        }

        case 'review_set': {
          const paths: string[] = args.paths;
          if (!Array.isArray(paths) || paths.length === 0) fail('paths must list at least one photo');
          if (paths.length > 24) fail('Too many photos for one sheet (max 24)');
          const reference = typeof args.reference === 'string' ? args.reference : '';
          if (!reference) fail('reference must name the image whose look is copied');
          const ref = await readAt(reference, await adjustmentsFor(reference), 360);
          const sheet = [{ label: 'REF', canvas: ref.canvas }];
          const rows: Args[] = [];
          const looks: Look[] = [];
          for (const [n, p] of paths.entries()) {
            const image = entryFor(p);
            const now = await readAt(image.path, await adjustmentsFor(image.path), 360);
            sheet.push({ label: String(n + 1), canvas: now.canvas });
            looks.push(now.look);
            rows.push({
              n: n + 1,
              path: relPath(root, image.path),
              whites: now.palette.whites ? { level: now.palette.whites.level, cast: now.palette.whites.cast } : null,
              blacks: now.palette.blacks ? { level: now.palette.blacks.level, cast: now.palette.blacks.cast } : null,
              brightness_p50: now.look.luma.p50,
              saturation_pct: round1(now.look.saturation),
              greens_saturation_pct: now.palette.bands.greens?.sat ?? null,
            });
          }
          return {
            reference: relPath(root, reference),
            reference_palette: paletteNumbers(ref.palette),
            photos: rows,
            consistency: spreadOf(looks),
            how_to_read: 'One sheet: the reference first, then each photo as it is now, numbered like photos. Look for photos that break the set: whites or blacks of another colour, a colour that stands out, one much brighter or darker than its neighbours of the same scene. Fix those with style_photo.',
            jpeg: await contactSheet(sheet, paths.length <= 6 ? 400 : paths.length <= 12 ? 320 : 280),
          };
        }

        case 'match_style': {
          const paths: string[] = args.paths;
          if (!Array.isArray(paths) || paths.length === 0) fail('paths must list at least one photo');
          if (paths.length > 60) fail('Too many photos in one call (max 60)');
          const reference: string = typeof args.reference === 'string' ? args.reference : '';
          if (!reference) fail('reference must name the image whose look is copied');
          const unit = (v: unknown, fallback: number) => (typeof v === 'number' && Number.isFinite(v) ? Math.min(1, Math.max(0, v)) : fallback);
          const strength = unit(args.strength, 0.8);
          const mixing = unit(args.coherence, 0.5);
          // The mechanical copy is only trusted for tone: colour taken from a picture's averages follows its subject.
          const asked: unknown[] = Array.isArray(args.what) && args.what.length ? args.what : ['tone'];
          const groups = new Set(asked.filter((g): g is string => typeof g === 'string' && STYLE_FAMILIES.includes(g)));
          if (groups.size === 0) fail(`what must name some of: ${STYLE_FAMILIES.join(', ')}`);
          const dryRun = args.dry_run === true;
          const inner: JobCtx | undefined = job && { ...job, report: () => {} };

          const refAdjustments = await adjustmentsFor(reference);
          const refLook = await lookAt(reference, refAdjustments, MATCH_SIDE);
          const mix: Mix = {
            tone: groups.has('tone') ? strength : 0,
            color: groups.has('color') ? strength : 0,
            grading: groups.has('grading') ? strength * 0.5 : 0,
            saturation: groups.has('saturation') ? strength : 0,
          };

          // First every photo is measured as it is (with the sliders this tool owns at zero), so the targets and
          // the coherence pull can be decided on the whole set before any photo is changed.
          type Item = { path: string; label: string; image: ImageFile; neutral: Adjustments; own: Look };
          const items: Item[] = [];
          const results: Args[] = [];
          let stopped = false;
          for (const p of paths) {
            if (job?.stopped()) {
              stopped = true;
              break;
            }
            const label = relPath(root, p);
            try {
              const image = entryFor(p);
              const neutral = withoutStyle(await adjustmentsFor(image.path), groups);
              items.push({ path: p, label, image, neutral, own: await lookAt(image.path, neutral, MATCH_SIDE) });
            } catch (e) {
              results.push({ path: label, ok: false, error: e instanceof Error ? e.message : String(e) });
            }
            job?.report({ phase: 'measuring', done: items.length + results.length, total: paths.length, last: label });
          }
          const targets = coherent(items.map((it) => targetFor(it.own, refLook, mix)), mixing);

          const afters: Look[] = [];
          const sheet: Array<{ label: string; canvas: HTMLCanvasElement }> = [];
          const wantSheet = args.preview === true && items.length <= 24;
          if (wantSheet) {
            const bitmap = await render(reference, refAdjustments, 360);
            const { canvas, ctx } = canvasFrom(bitmap.width, bitmap.height);
            ctx.drawImage(bitmap, 0, 0);
            bitmap.close();
            sheet.push({ label: 'REF', canvas });
          }
          let applied = 0;
          for (let n = 0; n < items.length && !stopped; n++) {
            if (job?.stopped()) {
              stopped = true;
              break;
            }
            const it = items[n];
            try {
              const solved = await solveStyle(it.image, it.neutral, it.own, targets[n], groups);
              const row: Args = {
                path: it.label,
                ok: true,
                before: brief(it.own),
                target: brief(targets[n]),
                after: brief(solved.look),
                adjustments: solved.patch,
                rounds: solved.rounds,
              };
              const warnings: string[] = [];
              if (solved.missed.length) warnings.push(`target not reached: ${solved.missed.join('; ')} (the light of this photo is too far from the reference, or a slider ran out of range)`);
              if (solved.look.luma.p95 >= 252) warnings.push('highlights close to clipping');
              if (!dryRun) {
                await handle('open_photo', { path: it.path, root, roots: args.roots }, inner);
                const set: any = await handle('set_adjustments', { path: it.path, root, roots: args.roots, patch: solved.patch }, inner);
                row.saved = set.saved === true;
                if (set.saved !== true) warnings.push(`not saved: ${set.save_problem ?? 'the sidecar does not show the change'}`);
                else applied++;
              }
              if (warnings.length) row.warning = warnings.join('; ');
              results.push(row);
              afters.push(solved.look);
              if (wantSheet) {
                const bitmap = await render(it.image.path, deepMerge(it.neutral, solved.patch), 360);
                const { canvas, ctx } = canvasFrom(bitmap.width, bitmap.height);
                ctx.drawImage(bitmap, 0, 0);
                bitmap.close();
                sheet.push({ label: String(n + 1), canvas });
              }
            } catch (e) {
              results.push({ path: it.label, ok: false, error: e instanceof Error ? e.message : String(e) });
            }
            job?.report({ phase: 'matching', done: results.length, total: paths.length, last: it.label }, results.slice(-3));
          }
          const ok = results.filter((r) => r.ok);
          const jpeg = sheet.length > 1 ? await contactSheet(sheet) : undefined;
          return {
            reference: relPath(root, reference),
            reference_look: brief(refLook),
            reference_description: describeLook(refLook),
            settings: { strength, coherence: mixing, what: [...groups], dry_run: dryRun },
            photos: paths.length,
            done: ok.length,
            ...(dryRun ? { would_change: ok.length } : { saved: applied }),
            failed: results.filter((r) => !r.ok).length,
            ...(stopped ? { cancelled: true, not_started: paths.length - results.length } : {}),
            // How much the set agrees: standard deviation of the figures that make a gallery consistent.
            consistency: afters.length > 1
              ? { before: spreadOf(items.map((it) => it.own)), after: spreadOf(afters), note: 'lower is more consistent; brightness is in levels (0-255), warmth and green in colour units, saturation in points' }
              : undefined,
            results,
            ...(jpeg ? { jpeg } : {}),
            how_to_read:
              'Each photo is moved toward the reference but only as far as its own light allows (a dark room is not turned into daylight), and every target is then pulled toward the average of the set by coherence. before/target/after use: shadows_p5, brightness_p50, highlights_p95 (0-255), warm (red minus blue), green and saturation_pct. A warning says which figure could not be reached.',
          };
        }

        case 'select_skin': {
          const paths: string[] = args.paths;
          if (!Array.isArray(paths) || paths.length === 0) fail('paths must list at least one photo');
          if (paths.length > 60) fail('Too many photos in one call (max 60)');
          const name: string = args.name || 'Light skin';
          const target: number = typeof args.target_luminance === 'number' ? args.target_luminance : 135;
          const parts: string[] = Array.isArray(args.parts) ? args.parts : ['faceSkin', 'bodySkin'];
          const start: Args = { exposure: 0.3, shadows: 12, highlights: -10, clarity: -8, ...(args.adjustments ?? {}) };
          const results: Args[] = [];
          const inner: JobCtx | undefined = job && { ...job, report: () => {} };
          let stopped = false;
          for (const p of paths) {
            if (job?.stopped()) {
              stopped = true;
              break;
            }
            const label = relPath(root, p);
            try {
              await handle('open_photo', { path: p, root, roots: args.roots }, inner);
              const made: any = await handle('add_mask', {
                path: p, root, roots: args.roots, name, replace_name: name, adjustments: start, sub_masks: [{ type: 'ai-face', params: { parts } }],
              }, inner);
              let exposure: number = start.exposure, saturation: number = start.saturation ?? 0, temperature: number = start.temperature ?? 0;
              let inside = await measureMask(p, made.mask_id);
              // Closed loop on the skin itself: bring it to the target luminance without going pale or grey.
              for (let i = 0; i < 3 && inside?.luminance; i++) {
                const lum = inside.luminance.mean, sat = inside.saturation_pct ?? 0, warm = inside.warm_minus_cool ?? 0;
                let changed = false;
                if (Math.abs(lum - target) > 8) {
                  exposure = Math.max(-0.5, Math.min(0.7, exposure + Math.log2(target / Math.max(lum, 1)) * 0.7));
                  changed = true;
                }
                if (sat < 14 && saturation < 20) {
                  saturation += 6;
                  changed = true;
                }
                if (warm < 0 && temperature < 25) {
                  temperature += 6;
                  changed = true;
                }
                if (!changed) break;
                await handle('update_mask', {
                  path: p, root, roots: args.roots, mask_id: made.mask_id,
                  adjustments: { exposure: Math.round(exposure * 100) / 100, saturation, temperature },
                }, inner);
                inside = await measureMask(p, made.mask_id);
              }
              const flags: string[] = [];
              if (inside?.luminance && inside.luminance.mean < target - 25) flags.push(`skin still dark (${Math.round(inside.luminance.mean)} against a target of ${Math.round(target)}; the lift is capped at +0.7 EV, so raise the shadows or the photo exposure)`);
              if (inside && (inside.saturation_pct ?? 0) < 12) flags.push('skin pale or grey');
              if (inside && (inside.warm_minus_cool ?? 0) < 0) flags.push('skin cool or grey');
              results.push({
                path: label,
                ok: true,
                mask_id: made.mask_id,
                skin: inside ? { luminance: Math.round(inside.luminance?.mean ?? 0), saturation_pct: inside.saturation_pct, warm_minus_cool: inside.warm_minus_cool } : null,
                adjustments: { exposure: Math.round(exposure * 100) / 100, saturation, temperature },
                ...(flags.length ? { warning: flags.join('; ') } : {}),
              });
            } catch (e) {
              results.push({ path: label, ok: false, error: e instanceof Error ? e.message : String(e) });
            }
            job?.report({ done: results.length, total: paths.length, last: label }, results);
          }
          return {
            photos: paths.length,
            done: results.filter((r) => r.ok).length,
            failed: results.filter((r) => !r.ok).length,
            ...(stopped ? { cancelled: true, not_started: paths.length - results.length } : {}),
            results,
          };
        }

        case 'assess': {
          const paths: string[] = args.paths;
          if (!Array.isArray(paths) || paths.length === 0) fail('paths must list at least one photo');
          if (paths.length > 400) fail('Too many photos in one call (max 400)');
          const wantFaces = args.faces !== false;
          const inner: JobCtx | undefined = job && { ...job, report: () => {} };
          const rows: AssessRow[] = [];
          const absolute = new Map<string, string>(); // displayed path -> the path the editor knows
          let stopped = false;
          for (const p of paths) {
            if (job?.stopped()) {
              stopped = true;
              break;
            }
            absolute.set(relPath(root, p), p);
            try {
              rows.push(await assessOne(p, wantFaces, root, args.roots, inner));
            } catch (e) {
              rows.push({ path: relPath(root, p), ok: false, error: e instanceof Error ? e.message : String(e) });
            }
            job?.report({ done: rows.length, total: paths.length, last: rows[rows.length - 1].path }, rows.slice(-3));
          }
          const ranked = rankAssessments(rows, wantFaces ? ASSESS_SIDE_FACES : ASSESS_SIDE);
          const byStars: Record<string, string[]> = { '5': [], '4': [], '3': [], '2': [], '1': [] };
          for (const r of ranked.ok) byStars[String(r.stars)].push(r.path);
          const suspects = ranked.ok
            .filter((r) => r.suspect_blurry)
            .sort((a, b) => a.z! - b.z!)
            .map((r) => ({ path: r.path, sharp: r.reference, face_vs_frame: r.face_vs_frame, z: r.z, faces: r.faces, reason: r.reason, ...(r.note ? { note: r.note } : {}) }));
          const checkByEye = ranked.ok.filter((r) => r.noisy).map((r) => ({ path: r.path, noise_floor: r.noise_floor, reason: r.reason }));
          let applied: unknown;
          if (args.apply_stars === true && !stopped) {
            // A RAW+JPEG pair is one shot: the partner file gets the same rating.
            const sameShot = (a: string, b: string) => norm(a).replace(/\.[^.\\]+$/, '') === norm(b).replace(/\.[^.\\]+$/, '');
            const failures: unknown[] = [];
            let rated = 0;
            for (const stars of [5, 4, 3, 2, 1]) {
              const group = byStars[String(stars)].map((rel) => absolute.get(rel)!);
              if (group.length === 0) continue;
              const withPartners = library().imageList.filter((img) => group.some((g) => sameShot(g, img.path))).map((img) => img.path);
              const done: any = await metadataOp(root, { paths: withPartners, detail: 'summary' }, Invokes.SetRatingForPaths, (ps) => ({ paths: ps, rating: stars }), (m) => m.rating === stars);
              rated += withPartners.length;
              if (!done.ok) failures.push(...done.failed);
            }
            applied = { files_rated: rated, failed: failures };
          }
          const withFaces = ranked.ok.filter((r) => (r.faces ?? 0) > 0).length;
          return {
            photos: paths.length,
            measured: ranked.ok.length,
            failed: rows.filter((r) => !r.ok).map((r) => ({ path: r.path, error: r.error })),
            with_faces: withFaces,
            without_faces: ranked.ok.length - withFaces,
            reference_sharpness: ranked.reference,
            ...(Object.keys(ranked.by_group).length ? { reference_by_file_type: ranked.by_group } : {}),
            suspect_blurry: suspects,
            ...(checkByEye.length ? { check_by_eye: checkByEye } : {}),
            stars: Object.fromEntries(Object.entries(byStars).map(([k, v]) => [k, args.apply_stars === true ? v.length : v])),
            ...(applied ? { applied } : {}),
            ...(stopped ? { cancelled: true, not_measured: paths.length - rows.length } : {}),
            ...(!ranked.enough ? { warning: 'Fewer than 8 photos measured: ranking and blur suspects need a bigger set to mean anything.' } : {}),
            how_to_read:
              'Sharpness is the detail energy of the largest face (or of the sharpest region when no face was found); higher is sharper and values only compare inside this run, and each file type (camera) is judged against its own kind. Every figure has the noise floor of its own photo taken off. suspect_blurry means the face is much softer than the rest of its frame (face_vs_frame under 0.12) and soft among the faces of its file type (z under -1), or the photo is far below the photos of its own file type (z under -2 and under half the typical sharpness of its type; a file type with fewer than 8 photos is compared with the whole set): look at a zoomed preview of the face before trashing. stars are suggested from a quality score (sharpness first, then exposure) by position in the set: top 8% 5, next 17% 4, next 35% 3, next 25% 2, the rest and every blur suspect 1.',
            ...(args.detail === 'full' ? { rows: ranked.ok.concat(rows.filter((r) => !r.ok)) } : {}),
          };
        }

        case 'mask_overlay': {
          const maxDim = Math.min(2400, Math.max(200, Number(args.max_dimension) || 1200));
          const out = await overlayImage(args.path, args.mask_id, maxDim);
          return {
            path: relPath(root, args.path),
            mask_id: args.mask_id,
            revision: revisionOf(editor().adjustments),
            width: out.width,
            height: out.height,
            coverage: out.empty ? 'empty: the mask selects nothing' : 'red overlay marks the selected area',
            mask_stats: out.report,
            jpeg: await canvasJpeg(out.canvas),
          };
        }

        case 'analyze': {
          const image = entryFor(args.path);
          const maxDim = Math.min(1200, Math.max(200, Number(args.max_dimension) || 600));
          const out: Record<string, unknown> = { path: relPath(root, image.path) };
          let bitmap: ImageBitmap;
          if (args.mask_id) {
            const { base, overlay } = await overlayParts(image.path, args.mask_id, maxDim);
            bitmap = base;
            out.mask = maskReport(base, overlay);
            overlay?.close();
          } else {
            const adjustments = await adjustmentsFor(image.path);
            out.revision = revisionOf(adjustments);
            const use = args.compare === 'before' ? getOriginalAdjustments(adjustments) : adjustments;
            out.compare = args.compare === 'before' ? 'before' : 'after';
            bitmap = await render(image.path, use, maxDim);
          }
          const r = args.region;
          let sx = 0, sy = 0, sw = bitmap.width, sh = bitmap.height;
          if (r) {
            sx = Math.round(r.x * bitmap.width);
            sy = Math.round(r.y * bitmap.height);
            sw = Math.max(1, Math.round(r.width * bitmap.width));
            sh = Math.max(1, Math.round(r.height * bitmap.height));
          }
          const small = smallCanvas(bitmap, sx, sy, sw, sh, 640);
          out.region = r ?? null;
          out.stats = statsOf(small);
          out.luma_grid_3x3 = lumaGrid(small);
          bitmap.close();
          return out;
        }

        case 'transform': {
          const s = activeReady(args.path);
          const cur = editor().adjustments;
          const patch: Record<string, unknown> = {};
          if (args.rotate_steps !== undefined) patch.orientationSteps = ((Math.round(args.rotate_steps) % 4) + 4) % 4;
          if (typeof args.flip_horizontal === 'boolean') patch.flipHorizontal = args.flip_horizontal;
          if (typeof args.flip_vertical === 'boolean') patch.flipVertical = args.flip_vertical;
          const steps = (patch.orientationSteps as number | undefined) ?? cur.orientationSteps ?? 0;
          const width = steps % 2 === 1 ? s.height : s.width;
          const height = steps % 2 === 1 ? s.width : s.height;
          let next: any = { ...cur, ...patch };
          if (typeof args.straighten_degrees === 'number') {
            if (Math.abs(args.straighten_degrees) > 45) fail('straighten_degrees must be within -45..45');
            next = withRotation(next, s, args.straighten_degrees);
          }
          if (args.clear_crop) {
            next.crop = null;
            next.aspectRatio = null;
          } else if (args.crop) {
            const { x, y, width: w, height: h } = args.crop;
            if ([x, y, w, h].some((v) => typeof v !== 'number') || w <= 0 || h <= 0 || x < 0 || y < 0 || x + w > 1.0001 || y + h > 1.0001) {
              fail('crop needs fractions x, y, width, height within 0..1 of the oriented image');
            }
            next.crop = { x: Math.round(x * width), y: Math.round(y * height), width: Math.round(w * width), height: Math.round(h * height) };
          } else if (typeof args.aspect_ratio === 'number' && args.aspect_ratio > 0) {
            next.aspectRatio = args.aspect_ratio;
            next.crop = calculateCenteredCrop(s.width, s.height, steps, args.aspect_ratio, next.rotation || 0);
          }
          tools.setAdjustments(() => next);
          await settle();
          const saved = await persist(s.path);
          const a = editor().adjustments;
          return {
            ...saved,
            revision: revisionOf(a),
            orientation_steps: a.orientationSteps,
            flip_horizontal: a.flipHorizontal,
            flip_vertical: a.flipVertical,
            straighten_degrees: a.rotation,
            aspect_ratio: a.aspectRatio,
            crop: a.crop,
            oriented_size: { width, height },
          };
        }

        case 'auto_adjust': {
          const s = activeReady(args.path);
          const mode = args.mode ?? 'normal';
          if (!['subtle', 'normal', 'strong'].includes(mode)) fail('mode must be subtle, normal or strong');
          const before = editor().adjustments as any;
          await tools.handleAutoAdjustments(mode);
          await settle();
          const after = editor().adjustments as any;
          const changed = Object.keys(after).filter((k) => JSON.stringify(after[k]) !== JSON.stringify(before[k]));
          if (changed.length === 0) fail('Auto adjust changed nothing');
          const saved = await persist(s.path);
          return {
            ...saved,
            mode,
            revision: revisionOf(after),
            changed: Object.fromEntries(changed.filter((k) => typeof after[k] !== 'object').map((k) => [k, after[k]])),
          };
        }

        case 'set_tags': {
          const clean = (list: unknown) => (Array.isArray(list) ? list : []).map((t) => String(t).trim()).filter(Boolean);
          const add = clean(args.add);
          const remove = clean(args.remove);
          if (add.length + remove.length === 0) fail('Provide add and/or remove tags');
          const targets = (args.paths as string[]).map((p) => entryFor(p));
          const paths = targets.map((t) => t.path);
          for (const tag of add) await invoke(Invokes.AddTagForPaths, { paths, tag: `user:${tag}` });
          for (const tag of remove) await invoke(Invokes.RemoveTagForPaths, { paths, tag: `user:${tag}` });
          const files = [];
          for (const image of targets) {
            const meta: any = await invoke(Invokes.LoadMetadata, { path: image.path });
            library().setLibrary((state) => ({
              imageList: state.imageList.map((img) => (img.path === image.path ? { ...img, tags: meta.tags ?? null } : img)),
            }));
            const have = ((meta.tags as string[] | null) ?? []).filter((t) => t.startsWith('user:')).map((t) => t.slice(5));
            files.push({
              path: relPath(root, image.path),
              ok: add.every((t) => have.includes(t)) && remove.every((t) => !have.includes(t)),
              tags: have,
            });
          }
          return { ok: files.every((f) => f.ok), files };
        }

        case 'list_presets': {
          const items: any[] = await invoke(Invokes.LoadPresets);
          const out: unknown[] = [];
          const visit = (item: any, folder?: string) => {
            if (item.preset) {
              const a = item.preset.adjustments || {};
              out.push({
                id: item.preset.id,
                name: item.preset.name,
                folder: folder ?? null,
                type: item.preset.presetType ?? 'style',
                changes: Object.keys(a).filter((k) => k !== 'masks'),
                has_masks: Array.isArray(a.masks) && a.masks.length > 0,
              });
            }
            if (item.folder) (item.children || []).forEach((c: any) => visit(c, item.folder.name));
          };
          items.forEach((i) => visit(i));
          return { presets: out };
        }

        case 'apply_preset': {
          const s = activeReady(args.path);
          const items: any[] = await invoke(Invokes.LoadPresets);
          const found: any[] = [];
          const visit = (item: any) => {
            if (item.preset) found.push(item.preset);
            if (item.folder) (item.children || []).forEach(visit);
          };
          items.forEach(visit);
          const needle = String(args.preset ?? '').toLowerCase();
          const preset = found.find((p) => p.id === args.preset) ?? found.find((p) => String(p.name).toLowerCase() === needle);
          if (!preset) fail(`Unknown preset: ${args.preset}. Use list_presets.`);
          const { masks, ...rest } = preset.adjustments || {};
          const hasAi = Array.isArray(masks) && masks.some((m: any) => (m.subMasks || []).some((sm: any) => String(sm.type).startsWith('ai-')));
          const before = revisionOf(editor().adjustments);
          tools.setAdjustments((prev: Adjustments) => ({ ...prev, ...rest, ...(masks && !hasAi ? { masks } : {}) }));
          await settle();
          const saved = await persist(s.path);
          return {
            ...saved,
            preset: preset.name,
            revision: revisionOf(editor().adjustments),
            changed: revisionOf(editor().adjustments) !== before,
            ...(hasAi ? { warning: 'This preset has AI masks; they were skipped. Recreate them with add_mask.' } : {}),
          };
        }

        case 'describe_adjustments': {
          const ranges = ADJUSTMENT_RANGES;
          const simple: Record<string, unknown> = {};
          const structured: Record<string, unknown> = {};
          for (const [key, value] of Object.entries(INITIAL_ADJUSTMENTS as Record<string, any>)) {
            if (['masks', 'aiPatches', 'activeTools'].includes(key)) continue;
            if (value !== null && typeof value === 'object') structured[key] = /curve/i.test(key) ? '(curve data; see get_adjustments)' : value;
            else simple[key] = { default: value, type: typeof value, ...(ranges[key] ? { range: ranges[key] } : {}) };
          }
          return {
            note: 'Ranges are the editor sliders; values outside may be clamped. Nested objects (hsl, colorGrading, ...) merge: send only the parts to change.',
            adjustments: simple,
            structured_defaults: structured,
            mask_local_adjustments: Object.keys(INITIAL_MASK_ADJUSTMENTS).filter((k) => k !== 'activeTools'),
          };
        }

        case 'export': {
          const paths: string[] = (args.paths || []).map((p: string) => entryFor(p).path);
          if (paths.length === 0) fail('paths is required');
          const format: string = args.format || 'jpeg';
          if (!['jpeg', 'png', 'tiff', 'webp'].includes(format)) fail('format must be jpeg, png, tiff or webp');
          debouncedSetHistory.flush();
          debouncedSave.flush();
          await sleep(300);
          const active = editor().selectedImage;
          const activeIncluded = !!active && paths.some((p) => norm(p) === norm(active.path));
          const exportSettings = {
            filenameTemplate: args.filename_template ?? '{original_filename}',
            jpegQuality: Math.min(100, Math.max(1, Number(args.quality) || 92)),
            tiffBitDepth: args.tiff_bit_depth === 8 ? 8 : 16,
            keepMetadata: args.keep_metadata !== false,
            preserveTimestamps: false,
            resize: args.max_dimension ? { mode: 'longEdge', value: Math.round(args.max_dimension), dontEnlarge: true } : null,
            border: null,
            pad: null,
            stripGps: !!args.strip_gps,
            watermark: null,
            exportMasks: false,
            preserveFolders: false,
            destinationType: 'customFolder',
            subfolder: '',
          };
          // export_images only starts the job; completion is announced by events.
          type Outcome = { kind: 'complete' | 'cancelled' | 'error'; message?: string };
          let finish: (o: Outcome) => void = () => {};
          const outcome = new Promise<Outcome>((resolve) => {
            finish = resolve;
          });
          let lastSignal = Date.now();
          let exportedSoFar = 0;
          const stops = await Promise.all([
            listen('export-complete', () => finish({ kind: 'complete' })),
            listen('export-cancelled', () => finish({ kind: 'cancelled' })),
            listen<string>('export-error', (event) => finish({ kind: 'error', message: String(event.payload) })),
            listen<{ current: number; total: number; path: string }>('batch-export-progress', (event) => {
              lastSignal = Date.now();
              exportedSoFar = event.payload.current;
              job?.report({ done: event.payload.current, total: event.payload.total, last: relPath(root, event.payload.path) });
            }),
          ]);
          try {
            await invoke(Invokes.ExportImages, {
              paths,
              outputFolderOrFile: args.output,
              isExplicitFilePath: false,
              baseOriginFolders: [root],
              exportSettings,
              outputFormat: format,
              currentEditPath: activeIncluded ? active!.path : null,
              currentEditAdjustments: activeIncluded ? editor().adjustments : null,
            });
            // No fixed limit: an export that keeps reporting progress may take as long as it needs. Only a
            // long silence counts as a failure.
            let watchdog: ReturnType<typeof setInterval> | undefined;
            const silent = new Promise<Outcome>((resolve) => {
              watchdog = setInterval(() => {
                if (Date.now() - lastSignal > 540000) resolve({ kind: 'error', message: 'No progress for 9 minutes; check the output folder' });
              }, 5000);
            });
            const done = await Promise.race([outcome, silent]).finally(() => clearInterval(watchdog));
            if (done.kind === 'error') fail(`Export failed: ${done.message}`);
            if (done.kind === 'cancelled') return { exported: exportedSoFar, cancelled: true };
          } finally {
            stops.forEach((stop) => stop());
          }
          return { exported: paths.length, format };
        }

        case 'cancel_export':
          await invoke(Invokes.CancelExport);
          return { cancelled: true };

        // From cancel_job: ask the request that is running to stop at its next step.
        case 'cancel': {
          const request = String(args.request ?? '');
          stopRequests.add(request);
          if (running.get(request) === 'export') await invoke(Invokes.CancelExport);
          return { ok: true };
        }

        default:
          return fail(`Unsupported operation: ${op}`);
      }
    };

    const listener = listen<Request>('rapidraw-mcp-request', async ({ payload }) => {
      if (disposed) return;
      const call = payload.call ?? '-';
      const began = performance.now();
      const active = editor().selectedImage;
      note(`${call} ${payload.op} window start (window ${document.visibilityState}, active ${active ? `${relPath(ROOTS[0] ?? '', active.path)} ready=${active.isReady}` : 'none'})`);
      let result: unknown;
      running.set(payload.id, payload.op);
      const job: JobCtx = {
        stopped: () => stopRequests.has(payload.id),
        report: (progress, partial) => {
          void invoke('mcp_progress', { id: payload.id, progress, partial: partial ?? null }).catch(() => {});
        },
      };
      try {
        result = await handle(payload.op, payload.args ?? {}, job);
      } catch (e) {
        result = { error: e instanceof Error ? e.message : String(e) };
      } finally {
        running.delete(payload.id);
        stopRequests.delete(payload.id);
      }
      note(`${call} ${payload.op} window done in ${Math.round(performance.now() - began)} ms${(result as any)?.error ? `: ${(result as any).error}` : ''}`);
      await invoke('mcp_reply', { id: payload.id, result });
    });
    // A new session id on every page load tells the bridge that anything it asked before is gone.
    void listener.then(() => invoke('mcp_ready', { session: uuidv4() }).catch(() => {}));
    return () => {
      disposed = true;
      void listener.then((off) => off());
    };
  }, []);
}
