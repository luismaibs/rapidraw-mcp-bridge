// What a photographer reads off a picture before copying its look: how the whites and the blacks are coloured,
// and how each family of colours behaves (how much of the frame it takes, how saturated, how light, which hue).
// Whites and blacks are measured on near-neutral pixels, so the cast of the light shows and the colour of the
// objects does not: a field of grass makes the midtones green, but it does not make the grade green.

/** [warm, green]: warm = R - B, green = G - (R + B) / 2, on the 0-255 scale. */
type Chroma = [number, number];

export const BAND_NAMES = ['reds', 'oranges', 'yellows', 'greens', 'aquas', 'blues', 'purples', 'magentas'] as const;
// The centres of the editor's HSL bands, in degrees.
const BAND_CENTERS = [358, 25, 60, 115, 180, 225, 280, 330];
// One unit of an HSL hue slider moves a hue this many degrees (0.3 in the editor, doubled by the shader).
const DEGREES_PER_HUE_UNIT = 0.6;

export type Band = { share: number; sat: number; lum: number; hue: number };
export type Ends = { level: number; rgb: [number, number, number]; cast: Chroma };
export type Palette = {
  whites: Ends | null;
  blacks: Ends | null;
  bands: Record<string, Band>;
  /** Percent of the frame that is clearly coloured (not grey, not clipped). */
  colored: number;
};

const round1 = (v: number) => Math.round(v * 10) / 10;
const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));
const circular = (a: number, b: number) => ((a - b + 540) % 360) - 180;

export const paletteOf = (data: ArrayLike<number>, pixels: number): Palette => {
  const hist = new Float64Array(256);
  const lumaAt = (o: number) => Math.round(0.2126 * data[o] + 0.7152 * data[o + 1] + 0.0722 * data[o + 2]);
  for (let i = 0; i < pixels; i++) hist[lumaAt(i * 4)]++;
  const quantile = (q: number) => {
    let acc = 0;
    for (let k = 0; k < 256; k++) {
      acc += hist[k];
      if (acc >= q * pixels) return k;
    }
    return 255;
  };
  const low = quantile(0.08);
  const high = quantile(0.92);
  const ends = {
    white: { r: 0, g: 0, b: 0, y: 0, n: 0 },
    black: { r: 0, g: 0, b: 0, y: 0, n: 0 },
  };
  const bands = BAND_CENTERS.map(() => ({ n: 0, sat: 0, lum: 0, x: 0, y: 0 }));
  let colored = 0;
  for (let i = 0; i < pixels; i++) {
    const o = i * 4;
    const r = data[o], g = data[o + 1], b = data[o + 2];
    const mx = Math.max(r, g, b), mn = Math.min(r, g, b);
    const d = mx - mn;
    const sat = mx === 0 ? 0 : d / mx;
    const y = lumaAt(o);
    if (y >= high && y <= 252 && sat < 0.3) {
      ends.white.r += r; ends.white.g += g; ends.white.b += b; ends.white.y += y; ends.white.n++;
    } else if (y <= low && y >= 1 && sat < 0.5) {
      ends.black.r += r; ends.black.g += g; ends.black.b += b; ends.black.y += y; ends.black.n++;
    }
    if (sat < 0.15 || y < 12 || y > 245) continue;
    let hue = 0;
    if (d > 0) {
      if (mx === r) hue = ((g - b) / d) % 6;
      else if (mx === g) hue = (b - r) / d + 2;
      else hue = (r - g) / d + 4;
      hue = (hue * 60 + 360) % 360;
    }
    let band = 0, best = 999;
    for (let k = 0; k < BAND_CENTERS.length; k++) {
      const dist = Math.abs(circular(hue, BAND_CENTERS[k]));
      if (dist < best) {
        best = dist;
        band = k;
      }
    }
    const acc = bands[band];
    acc.n++;
    acc.sat += sat;
    acc.lum += y;
    acc.x += Math.cos((hue * Math.PI) / 180);
    acc.y += Math.sin((hue * Math.PI) / 180);
    colored++;
  }
  const endOf = (e: typeof ends.white, minShare: number): Ends | null => {
    if (e.n < pixels * minShare) return null;
    const r = e.r / e.n, g = e.g / e.n, b = e.b / e.n;
    return { level: Math.round(e.y / e.n), rgb: [Math.round(r), Math.round(g), Math.round(b)], cast: [round1(r - b), round1(g - (r + b) / 2)] };
  };
  const out: Record<string, Band> = {};
  BAND_NAMES.forEach((name, k) => {
    const a = bands[k];
    if (a.n === 0) return;
    out[name] = {
      share: round1((a.n / Math.max(1, colored)) * 100),
      sat: round1((a.sat / a.n) * 100),
      lum: Math.round(a.lum / a.n),
      hue: Math.round(((Math.atan2(a.y, a.x) * 180) / Math.PI + 360) % 360),
    };
  });
  return { whites: endOf(ends.white, 0.01), blacks: endOf(ends.black, 0.01), bands: out, colored: round1((colored / Math.max(1, pixels)) * 100) };
};

const castWords = ([warm, green]: Chroma) => {
  const parts: string[] = [];
  if (Math.abs(warm) >= 2) parts.push(warm > 0 ? (warm > 10 ? 'clearly warm' : 'slightly warm') : warm < -10 ? 'clearly cool/blue' : 'slightly cool/blue');
  if (Math.abs(green) >= 1.5) parts.push(green > 0 ? (green > 6 ? 'clearly green' : 'slightly green') : green < -6 ? 'clearly magenta' : 'slightly magenta');
  return parts.length ? parts.join(', ') : 'neutral';
};

/** The numbers an agent reads. Colours that take under 1% of the coloured area are left out. */
export const paletteNumbers = (p: Palette) => ({
  whites: p.whites ? { level: p.whites.level, rgb: p.whites.rgb, cast: p.whites.cast, reads_as: castWords(p.whites.cast) } : null,
  blacks: p.blacks ? { level: p.blacks.level, rgb: p.blacks.rgb, cast: p.blacks.cast, reads_as: castWords(p.blacks.cast) } : null,
  coloured_pct_of_frame: p.colored,
  colours: Object.fromEntries(
    Object.entries(p.bands)
      .filter(([, b]) => b.share >= 1)
      .map(([name, b]) => [name, { share_pct: b.share, saturation_pct: b.sat, luminance: b.lum, hue_deg: b.hue }]),
  ),
});

/** The palette in words: how the whites and blacks are made, and how each colour is handled. */
export const describePalette = (p: Palette): string[] => {
  const lines: string[] = [];
  if (p.whites) {
    const w = p.whites;
    lines.push(`whites: level ${w.level}/255 (${w.level > 235 ? 'bright, close to white' : w.level > 205 ? 'soft, not clipped' : 'dull, grey'}), ${castWords(w.cast)} (cast ${w.cast.join(' / ')})`);
  } else lines.push('whites: no neutral bright area to read');
  if (p.blacks) {
    const k = p.blacks;
    lines.push(`blacks: level ${k.level}/255 (${k.level > 32 ? 'lifted, faded, matte' : k.level < 10 ? 'deep, crushed' : 'natural'}), ${castWords(k.cast)} (cast ${k.cast.join(' / ')})`);
  } else lines.push('blacks: no dark neutral area to read');
  const ordered = Object.entries(p.bands).filter(([, b]) => b.share >= 3).sort((a, b) => b[1].share - a[1].share);
  for (const [name, b] of ordered) {
    lines.push(`${name}: ${b.share}% of the colour, saturation ${b.sat}% (${b.sat < 22 ? 'muted' : b.sat > 55 ? 'vivid' : 'moderate'}), luminance ${b.lum}, hue ${b.hue} degrees`);
  }
  return lines;
};

/**
 * What would bring this photo's palette closer to the reference, in terms of the editor's controls. These are
 * measurements turned into suggestions: the agent decides, looking at the picture, which ones make sense for this
 * photo's light and subject.
 */
export const adviceFor = (photo: Palette, ref: Palette): string[] => {
  const tips: string[] = [];
  const ends = (name: string, mine: Ends | null, theirs: Ends | null, tipLevel: (d: number) => string | null) => {
    if (!mine || !theirs) return;
    const dLevel = theirs.level - mine.level;
    const dWarm = theirs.cast[0] - mine.cast[0];
    const dGreen = theirs.cast[1] - mine.cast[1];
    const parts: string[] = [`${name}: this photo ${mine.level} and ${castWords(mine.cast)}; the reference ${theirs.level} and ${castWords(theirs.cast)}`];
    const level = tipLevel(dLevel);
    if (level) parts.push(level);
    if (Math.abs(dWarm) >= 3) parts.push(dWarm > 0 ? `make the ${name} warmer by about ${Math.round(dWarm)} (warm grading in that zone, or temperature if it is the whole light)` : `make the ${name} cooler/bluer by about ${Math.round(-dWarm)}`);
    if (Math.abs(dGreen) >= 2) parts.push(dGreen > 0 ? `add green to the ${name} by about ${round1(dGreen)}` : `add magenta to the ${name} by about ${round1(-dGreen)} (tint toward magenta)`);
    tips.push(parts.join('; '));
  };
  ends('whites', photo.whites, ref.whites, (d) => (d < -12 ? 'the whites are brighter than the reference: lower highlights/whites so they sit softer' : d > 12 ? 'the whites are duller than the reference: raise exposure or whites a little' : null));
  ends('blacks', photo.blacks, ref.blacks, (d) => (d > 12 ? `lift the blacks about ${d} levels for the faded look (blacks slider up, or the first point of the luma curve up to y of about ${ref.blacks?.level ?? 0})` : d < -12 ? 'the blacks are more lifted than the reference: deepen them' : null));
  for (const [name, mine] of Object.entries(photo.bands)) {
    if (mine.share < 4) continue;
    const theirs = ref.bands[name];
    if (!theirs || theirs.share < 1.5) {
      tips.push(`${name}: ${mine.share}% of this photo's colour but almost none of the reference's (${theirs?.share ?? 0}%): it will stand out; reduce its saturation hard (about -40 to -70) and darken it a little`);
      continue;
    }
    const parts: string[] = [];
    const ratio = theirs.sat / Math.max(1, mine.sat);
    if (ratio < 0.8) parts.push(`more muted in the reference (${theirs.sat}% against ${mine.sat}%): saturation about ${Math.round((ratio - 1) * 70)}`);
    else if (ratio > 1.25) parts.push(`richer in the reference (${theirs.sat}% against ${mine.sat}%): saturation about +${Math.round(clamp((ratio - 1) * 70, 0, 60))}`);
    const dHue = circular(theirs.hue, mine.hue);
    if (Math.abs(dHue) >= 8) parts.push(`hue ${mine.hue} -> ${theirs.hue} degrees: about ${Math.round(clamp(dHue / DEGREES_PER_HUE_UNIT, -60, 60))} on the hue slider (check the direction in the preview)`);
    const dLum = theirs.lum - mine.lum;
    if (Math.abs(dLum) >= 15) parts.push(`${dLum > 0 ? 'lighter' : 'darker'} in the reference: luminance about ${Math.round(clamp(dLum * 0.4, -40, 40))}`);
    if (parts.length) tips.push(`${name} (${mine.share}% of the colour): ${parts.join('; ')}`);
  }
  if (tips.length === 0) tips.push('the palette is already close to the reference by these measures');
  return tips;
};
