// The maths behind match_style: what a photo looks like as a few numbers, where a reference wants it, and the small
// controllers that move sliders until the render gets there. Nothing in here touches the editor.

/** [warm, green]: warm = R - B, green = G - (R + B) / 2, on the 0-255 scale. Positive warm is orange, negative blue. */
export type Chroma = [number, number];

export type Look = {
  luma: { p5: number; p25: number; p50: number; p75: number; p95: number };
  /** Mean HSV saturation, percent. */
  saturation: number;
  /** Colour cast of the darkest quarter, the middle half and the lightest quarter of the pixels. */
  shadows: Chroma;
  mids: Chroma;
  highlights: Chroma;
};

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));
const lerp = (a: number, b: number, t: number) => a + (b - a) * t;
const round1 = (v: number) => Math.round(v * 10) / 10;
const chroma2 = (a: Chroma, f: (x: number, i: number) => number): Chroma => [f(a[0], 0), f(a[1], 1)];

// One bright red object must not decide the cast of a whole tonal zone.
const PIXEL_CAP: Chroma = [45, 30];

export const lookOf = (data: ArrayLike<number>, pixels: number): Look => {
  const hist = new Float64Array(256);
  for (let i = 0; i < pixels; i++) {
    const o = i * 4;
    hist[Math.round(0.2126 * data[o] + 0.7152 * data[o + 1] + 0.0722 * data[o + 2])]++;
  }
  const quantile = (q: number) => {
    let acc = 0;
    for (let k = 0; k < 256; k++) {
      acc += hist[k];
      if (acc >= q * pixels) return k;
    }
    return 255;
  };
  const luma = { p5: quantile(0.05), p25: quantile(0.25), p50: quantile(0.5), p75: quantile(0.75), p95: quantile(0.95) };
  const zone = [
    { u: 0, v: 0, n: 0 },
    { u: 0, v: 0, n: 0 },
    { u: 0, v: 0, n: 0 },
  ];
  let sat = 0;
  for (let i = 0; i < pixels; i++) {
    const o = i * 4;
    const r = data[o], g = data[o + 1], b = data[o + 2];
    const mx = Math.max(r, g, b);
    sat += mx === 0 ? 0 : (mx - Math.min(r, g, b)) / mx;
    const y = Math.round(0.2126 * r + 0.7152 * g + 0.0722 * b);
    // Clipped pixels carry no colour.
    if (y < 4 || y > 251) continue;
    const z = zone[y <= luma.p25 ? 0 : y >= luma.p75 ? 2 : 1];
    z.u += clamp(r - b, -PIXEL_CAP[0], PIXEL_CAP[0]);
    z.v += clamp(g - (r + b) / 2, -PIXEL_CAP[1], PIXEL_CAP[1]);
    z.n++;
  }
  const mean = (z: { u: number; v: number; n: number }): Chroma => (z.n ? [z.u / z.n, z.v / z.n] : [0, 0]);
  return {
    luma,
    saturation: pixels ? (sat / pixels) * 100 : 0,
    shadows: mean(zone[0]),
    mids: mean(zone[1]),
    highlights: mean(zone[2]),
  };
};

/** The numbers an agent reads, rounded. */
export const lookNumbers = (look: Look) => ({
  luminance: { p5: look.luma.p5, p25: look.luma.p25, p50: look.luma.p50, p75: look.luma.p75, p95: look.luma.p95 },
  contrast_spread: look.luma.p75 - look.luma.p25,
  saturation_pct: round1(look.saturation),
  cast: {
    shadows: look.shadows.map(round1),
    midtones: look.mids.map(round1),
    highlights: look.highlights.map(round1),
  },
});

const castName = ([warm, green]: Chroma) => {
  const parts: string[] = [];
  if (Math.abs(warm) >= 3) parts.push(warm > 0 ? (warm > 12 ? 'strongly warm' : 'warm') : warm < -12 ? 'strongly cool (blue)' : 'cool (blue)');
  if (Math.abs(green) >= 2.5) parts.push(green > 0 ? 'green' : 'magenta');
  return parts.length ? parts.join(' and ') : 'neutral';
};

/** The look in words, so a model that cannot see the picture still knows what it is copying. */
export const describeLook = (look: Look): string[] => {
  const { p5, p25, p50, p75, p95 } = look.luma;
  const lines: string[] = [];
  lines.push(`brightness: ${p50 < 85 ? 'dark, low key' : p50 > 150 ? 'bright, high key' : 'medium'} (median ${p50}/255)`);
  const spread = p75 - p25;
  lines.push(`contrast: ${spread < 45 ? 'soft and flat' : spread > 95 ? 'punchy' : 'moderate'} (middle half spans ${spread} levels)`);
  lines.push(`blacks: ${p5 > 28 ? 'lifted, matte or faded' : p5 < 8 ? 'deep and crushed' : 'natural'} (darkest 5% at ${p5})`);
  lines.push(`highlights: ${p95 < 225 ? 'soft, rolled off, never white' : p95 > 250 ? 'bright, reaching white' : 'natural'} (lightest 5% at ${p95})`);
  lines.push(`saturation: ${look.saturation < 20 ? 'muted' : look.saturation > 45 ? 'vivid' : 'moderate'} (${round1(look.saturation)}%)`);
  lines.push(`overall colour: ${castName(look.mids)}`);
  const dS: Chroma = [look.shadows[0] - look.mids[0], look.shadows[1] - look.mids[1]];
  const dH: Chroma = [look.highlights[0] - look.mids[0], look.highlights[1] - look.mids[1]];
  lines.push(`shadows against the midtones: ${Math.hypot(dS[0], dS[1]) < 3 ? 'same colour' : castName(dS)}`);
  lines.push(`highlights against the midtones: ${Math.hypot(dH[0], dH[1]) < 3 ? 'same colour' : castName(dH)}`);
  return lines;
};

/** Where one photo should end up. Colours are the midtone cast and the shadow and highlight casts RELATIVE to it. */
export type Targets = {
  p5: number;
  p25: number;
  p50: number;
  p75: number;
  p95: number;
  saturation: number;
  mids: Chroma;
  dShadows: Chroma;
  dHighlights: Chroma;
};

/** How far each family moves toward the reference: 0 leaves the photo as it is, 1 copies the reference. */
export type Mix = { tone: number; color: number; grading: number; saturation: number };

// A photo is never pushed further than this from its own light: a candle-lit dinner stays a candle-lit dinner.
const MAX_LEVEL_MOVE = 30;
const MAX_BLACK_MOVE = 15;
const MAX_MIDS_MOVE: Chroma = [28, 18];
// Shadow and highlight colours are the least reliable part of a reference (a bright sky decides the highlights), so
// they move little.
const MAX_SPLIT_MOVE: Chroma = [12, 9];
// Highlights are never asked to rise by much, and never to a level where they clip.
const MAX_HIGHLIGHT_RISE = 12;
const HIGHLIGHT_CEILING = 235;

const relative = (zone: Chroma, mids: Chroma): Chroma => [zone[0] - mids[0], zone[1] - mids[1]];

export const targetFor = (own: Look, ref: Look, mix: Mix): Targets => {
  const level = (a: number, b: number) => clamp(lerp(a, b, mix.tone), a - MAX_LEVEL_MOVE, a + MAX_LEVEL_MOVE);
  let p5 = clamp(lerp(own.luma.p5, ref.luma.p5, mix.tone), own.luma.p5 - MAX_BLACK_MOVE, own.luma.p5 + MAX_BLACK_MOVE);
  let p25 = level(own.luma.p25, ref.luma.p25);
  let p50 = level(own.luma.p50, ref.luma.p50);
  let p75 = level(own.luma.p75, ref.luma.p75);
  let p95 = clamp(lerp(own.luma.p95, ref.luma.p95, mix.tone), own.luma.p95 - MAX_LEVEL_MOVE, Math.min(own.luma.p95 + MAX_HIGHLIGHT_RISE, HIGHLIGHT_CEILING));
  // Keep the order, and never ask for a white that would clip.
  p5 = clamp(p5, 0, 120);
  p25 = Math.max(p25, p5 + 2);
  p50 = Math.max(p50, p25 + 2);
  p75 = Math.max(p75, p50 + 2);
  p95 = clamp(Math.max(p95, p75 + 2), 0, HIGHLIGHT_CEILING);
  const mids = chroma2(own.mids, (x, i) => x + clamp(mix.color * (ref.mids[i] - x), -MAX_MIDS_MOVE[i], MAX_MIDS_MOVE[i]));
  const split = (ownZone: Chroma, refZone: Chroma): Chroma => {
    const a = relative(ownZone, own.mids);
    const b = relative(refZone, ref.mids);
    return chroma2(a, (x, i) => x + clamp(mix.grading * (b[i] - x), -MAX_SPLIT_MOVE[i], MAX_SPLIT_MOVE[i]));
  };
  return {
    p5, p25, p50, p75, p95,
    saturation: lerp(own.saturation, ref.saturation, mix.saturation),
    mids,
    dShadows: split(own.shadows, ref.shadows),
    dHighlights: split(own.highlights, ref.highlights),
  };
};

const NUMERIC_KEYS = ['p5', 'p25', 'p50', 'p75', 'p95', 'saturation'] as const;
const CHROMA_KEYS = ['mids', 'dShadows', 'dHighlights'] as const;

/**
 * Pull every photo's target toward the average target of the set, so a folder shot in different rooms and
 * different light reads as one gallery. 0 keeps each photo's own target, 1 gives every photo the same.
 */
export const coherent = (targets: Targets[], amount: number): Targets[] => {
  if (targets.length < 2 || amount <= 0) return targets;
  const mean = (f: (t: Targets) => number) => targets.reduce((s, t) => s + f(t), 0) / targets.length;
  const avg: Targets = {
    p5: 0, p25: 0, p50: 0, p75: 0, p95: 0, saturation: 0,
    mids: [0, 0], dShadows: [0, 0], dHighlights: [0, 0],
  };
  for (const k of NUMERIC_KEYS) avg[k] = mean((t) => t[k]);
  for (const k of CHROMA_KEYS) avg[k] = [mean((t) => t[k][0]), mean((t) => t[k][1])];
  return targets.map((t) => {
    const out = { ...t };
    for (const k of NUMERIC_KEYS) out[k] = lerp(t[k], avg[k], amount);
    for (const k of CHROMA_KEYS) out[k] = chroma2(t[k], (x, i) => lerp(x, avg[k][i], amount));
    return out;
  });
};

/** Spread (standard deviation) of the figures that make a gallery look consistent. */
export const spreadOf = (looks: Look[]) => {
  const sd = (values: number[]) => {
    if (values.length < 2) return 0;
    const m = values.reduce((s, v) => s + v, 0) / values.length;
    return Math.sqrt(values.reduce((s, v) => s + (v - m) ** 2, 0) / values.length);
  };
  return {
    brightness: round1(sd(looks.map((l) => l.luma.p50))),
    warmth: round1(sd(looks.map((l) => l.mids[0]))),
    green: round1(sd(looks.map((l) => l.mids[1]))),
    saturation: round1(sd(looks.map((l) => l.saturation))),
  };
};

/**
 * One slider driven toward a measured figure. The response of a slider differs per photo, so the gain (figure
 * units per slider unit) starts from a prior and is re-estimated from the last step, sign included.
 */
export class Knob {
  private lastX: number | null = null;
  private lastM = 0;
  private readonly prior: number;
  constructor(
    public value: number,
    private readonly min: number,
    private readonly max: number,
    private gain: number,
    private readonly maxStep: number,
  ) {
    this.prior = Math.abs(gain);
  }
  /**
   * `learn` false skips re-estimating the gain from this step: another slider moved a lot at the same time, so the
   * change in the figure is not this slider's alone.
   */
  step(measured: number, target: number, damping = 0.65, learn = true): number {
    if (learn && this.lastX !== null && Math.abs(this.value - this.lastX) > 0.2) {
      const seen = (measured - this.lastM) / (this.value - this.lastX);
      if (Math.abs(seen) > 0.15 * this.prior) {
        this.gain = Math.sign(seen) * clamp(Math.abs(seen), 0.25 * this.prior, 4 * this.prior);
      }
    }
    this.lastX = this.value;
    this.lastM = measured;
    this.value = clamp(this.value + clamp((damping * (target - measured)) / this.gain, -this.maxStep, this.maxStep), this.min, this.max);
    return this.value;
  }
}

// How the grading hue wheel (HSV hue, full saturation) shows up in the two colour axes. Fitted once from the wheel
// itself so a wanted colour shift can be turned into a hue and a strength.
const WHEEL = (() => {
  const toRgb = (h: number) => {
    const x = 1 - Math.abs(((h / 60) % 2) - 1);
    const t: Array<[number, number, number]> = [[1, x, 0], [x, 1, 0], [0, 1, x], [0, x, 1], [x, 0, 1], [1, 0, x]];
    return t[Math.min(5, Math.floor(h / 60))];
  };
  // Least squares of [warm, green] = J [cos h, sin h] over the wheel.
  let cc = 0, ss = 0, cu = 0, su = 0, cv = 0, sv = 0;
  for (let h = 0; h < 360; h += 5) {
    const [r, g, b] = toRgb(h);
    const c = Math.cos((h * Math.PI) / 180), s = Math.sin((h * Math.PI) / 180);
    const u = r - b, v = g - (r + b) / 2;
    cc += c * c; ss += s * s; cu += c * u; su += s * u; cv += c * v; sv += s * v;
  }
  const j = [[cu / cc, su / ss], [cv / cc, sv / ss]];
  const det = j[0][0] * j[1][1] - j[0][1] * j[1][0];
  const inv = [[j[1][1] / det, -j[0][1] / det], [-j[1][0] / det, j[0][0] / det]];
  return { j, inv };
})();

/**
 * Colour grading of one tonal zone driven toward a wanted cast relative to the midtones. The state is a point on the
 * wheel (x, y); hue is its angle and saturation its length. `gain` converts saturation to measured colour units.
 */
export class Grade {
  x = 0;
  y = 0;
  private lastPredicted: Chroma | null = null;
  private lastMeasured: Chroma | null = null;
  constructor(private gain: number, private readonly maxSat = 40) {}

  get hue() {
    return Math.round(((Math.atan2(this.y, this.x) * 180) / Math.PI + 360) % 360);
  }
  get saturation() {
    return Math.round(Math.hypot(this.x, this.y) * 10) / 10;
  }

  step(measured: Chroma, target: Chroma, damping = 0.65): void {
    if (this.lastPredicted && this.lastMeasured) {
      const moved: Chroma = [measured[0] - this.lastMeasured[0], measured[1] - this.lastMeasured[1]];
      const p = this.lastPredicted;
      const along = (moved[0] * p[0] + moved[1] * p[1]) / Math.max(1e-6, p[0] * p[0] + p[1] * p[1]);
      if (Math.hypot(p[0], p[1]) > 0.5) this.gain *= clamp(along, 0.5, 2);
    }
    const want: Chroma = [target[0] - measured[0], target[1] - measured[1]];
    const dx = (WHEEL.inv[0][0] * want[0] + WHEEL.inv[0][1] * want[1]) / this.gain;
    const dy = (WHEEL.inv[1][0] * want[0] + WHEEL.inv[1][1] * want[1]) / this.gain;
    this.lastMeasured = measured;
    this.lastPredicted = [
      this.gain * (WHEEL.j[0][0] * damping * dx + WHEEL.j[0][1] * damping * dy),
      this.gain * (WHEEL.j[1][0] * damping * dx + WHEEL.j[1][1] * damping * dy),
    ];
    this.x += damping * dx;
    this.y += damping * dy;
    const length = Math.hypot(this.x, this.y);
    if (length > this.maxSat) {
      this.x *= this.maxSat / length;
      this.y *= this.maxSat / length;
    }
  }
}
