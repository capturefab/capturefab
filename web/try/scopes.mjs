// Calibration scopes floating over the stage, ported from the desktop app
// (src/gui/scopes.rs, sparkline.rs): a histogram of the picture's levels with
// clipping, focus scores for a region over the image, and the title bar's frame
// rate sparkline. The figures come from the engine's port of src/frame.rs.
// Logic classes (FocusMeter, History) touch no DOM, so Node tests import them.
import { icon } from './icons.mjs';
import { focusScore, fixed, percent, trend } from './format.mjs';

const f32 = Math.fround;

/** Clipping above this share of samples is called out. */
export const CLIP_WARN = f32(0.005);
/** Focus scores kept for the trace. */
export const HISTORY = 120;
/** At this share of the peak a region comes into focus... */
const IN_FOCUS = f32(0.97);
/** ...and below this one it leaves it again, so jitter near the peak does not flicker. */
const IN_FOCUS_EXIT = f32(0.9);
/** Frames measured before the peak means anything. */
export const SETTLE = 10;
/** Smallest region side, in sensor pixels. */
export const MIN_PIXELS = 16;
/** The middle quarter of the frame. */
export const DEFAULT_REGION = Object.freeze([0.375, 0.375, 0.25, 0.25]);

// Colors on the dark stage (scopes.rs:38-40, style.rs STAGE).
const RED = [255, 84.15, 79.05];
const GREEN = [63.75, 216.75, 107.1];
const BLUE = [84.15, 147.9, 255];
const WHITE = [255, 255, 255];
const WARN = [255, 159, 10];
const LIVE = [50, 209, 88];
const ACCENT = [76, 141, 248];
const rgba = ([r, g, b], a) => `rgba(${r}, ${g}, ${b}, ${Math.max(0, Math.min(1, a))})`;

/** The focus measures: all are computed, one is charted (scopes.rs:43-140). */
export const METRICS = Object.freeze([
  { key: 'laplacian', label: 'Laplacian', plain: 'Edges', hint: 'Variance of the Laplacian: responds to fine edges' },
  { key: 'tenengrad', label: 'Tenengrad', plain: 'Strong edges', hint: 'Squared Sobel gradient: steady on strong edges and in noise' },
  { key: 'brenner', label: 'Brenner', plain: 'Texture', hint: 'Squared difference two pixels apart: favors fine texture' },
  { key: 'variance', label: 'Variance', plain: 'Contrast', hint: 'Normalized intensity variance: contrast based, gentle on noise' }
]);

const metricOf = (key) => METRICS.find((m) => m.key === key) ?? METRICS[0];

/** The chooser's tooltip: "Focus measure: Edges (Laplacian). Variance of the Laplacian: …". */
export const metricAbout = (key) => {
  const m = metricOf(key);
  return `Focus measure: ${m.plain} (${m.label}). ${m.hint}`;
};

/** A saved region made valid: finite, inside the frame and not empty (scopes.rs sanitize). */
export const sanitizeRegion = (region) => {
  const values = Array.isArray(region) ? region.slice(0, 4).map(Number) : [];
  if (values.length !== 4 || !values.every(Number.isFinite)) return sanitizeRegion(DEFAULT_REGION);
  const [x, y] = values;
  const w = f32(Math.min(1, Math.max(1e-4, values[2])));
  const h = f32(Math.min(1, Math.max(1e-4, values[3])));
  return [f32(Math.min(1 - w, Math.max(0, x))), f32(Math.min(1 - h, Math.max(0, y))), w, h];
};

const ZERO = Object.freeze({ laplacian: 0, tenengrad: 0, brenner: 0, variance: 0 });

/** Focus scores of a region over time, the peak and whether it is in focus (scopes.rs FocusMeter). */
export class FocusMeter {
  region;
  metric;
  history = [];
  peak = { ...ZERO };
  sharp = false;
  locks = 0;
  /** The scores on show are of the region before it moved; the next score starts afresh. */
  moved = false;

  constructor(region = DEFAULT_REGION, metric = 'laplacian') {
    this.region = sanitizeRegion(region);
    this.metric = metricOf(metric).key;
  }

  /** Move the region; its scores stay on show until the next one replaces them. */
  moveTo(region) {
    this.region = region.map(f32);
    this.moved = this.history.length > 0;
  }

  /** The region in pixels of a `width` × `height` frame. */
  pixels(width, height) {
    const [x, y, w, h] = this.region;
    const W = f32(width);
    const H = f32(height);
    return [
      Math.round(f32(x * W)),
      Math.round(f32(y * H)),
      Math.max(1, Math.round(f32(w * W))),
      Math.max(1, Math.round(f32(h * H)))
    ];
  }

  record(score) {
    if (this.moved) this.reset();
    const settled = this.history.length >= SETTLE;
    if (this.history.length === HISTORY) this.history.shift();
    const s = {
      laplacian: f32(score.laplacian),
      tenengrad: f32(score.tenengrad),
      brenner: f32(score.brenner),
      variance: f32(score.variance)
    };
    this.history.push(s);
    for (const key of Object.keys(ZERO)) this.peak[key] = Math.max(this.peak[key], s[key]);
    const was = this.sharp;
    const enough = was ? IN_FOCUS_EXIT : IN_FOCUS;
    const share = this.share();
    this.sharp = this.history.length >= SETTLE && share !== null && share >= enough;
    // Settling at a fresh peak is no lock-on; coming back to it is.
    if (this.sharp && !was && settled) this.locks += 1;
  }

  reset() {
    this.history = [];
    this.peak = { ...ZERO };
    this.sharp = false;
    this.moved = false;
  }

  latest() {
    const last = this.history.at(-1);
    return last ? last[this.metric] : null;
  }

  peakValue() {
    return this.peak[this.metric];
  }

  /** The latest score as a share of the peak, 0–1, or null before any score. */
  share() {
    const latest = this.latest();
    if (latest === null) return null;
    const peak = this.peakValue();
    return peak > 0 ? f32(latest / peak) : 0;
  }

  /** Chart another measure; its verdict is judged afresh, and a switch is no lock-on. */
  setMetric(metric) {
    this.metric = metricOf(metric).key;
    const share = this.share();
    this.sharp = this.history.length >= SETTLE && share !== null && share >= IN_FOCUS;
  }

  get inFocus() {
    return this.sharp;
  }

  get settled() {
    return this.history.length >= SETTLE;
  }

  trace() {
    return this.history.map((s) => s[this.metric]);
  }

  /** What the region's tag says: "741  93%", or the measure's name before a score. */
  label() {
    const latest = this.latest();
    const share = this.share();
    if (latest === null || share === null) return metricOf(this.metric).plain;
    return `${focusScore(latest)}  ${fixed(f32(share * 100), 0)}%`;
  }

  /** The verdict line under the score. */
  verdict() {
    if (this.sharp) return 'In focus';
    if (this.history.length >= SETTLE) return 'Adjust to raise the score';
    if (this.share() !== null) return 'Measuring…';
    return 'Waiting for a frame';
  }
}

/** Samples of a value over a fixed span, taken at most once per period (sparkline.rs History). */
export class History {
  samples = [];
  #counter = null;

  constructor(period = 250, span = 30000) {
    this.period = period;
    this.span = span;
  }

  /** Record `value` at `now` (ms) if a period has passed; flagged when `counter` rose. */
  offer(now, value, counter) {
    const last = this.samples.at(-1);
    if (last && now - last.at < this.period) return;
    const flagged = this.#counter !== null && counter > this.#counter;
    this.#counter = counter;
    this.samples.push({ at: now, value: Number.isFinite(value) ? Math.max(0, value) : 0, flagged });
    while (this.samples.length && now - this.samples[0].at > this.span) this.samples.shift();
  }

  clear() {
    this.samples = [];
    this.#counter = null;
  }

  elapsed() {
    return this.samples.length ? this.samples.at(-1).at - this.samples[0].at : 0;
  }

  /** Enough samples, far enough apart, for a line that shows a trend. */
  ready() {
    return this.samples.length >= 3 && this.elapsed() >= 1750;
  }

  range() {
    if (!this.samples.length) return null;
    const values = this.samples.map((s) => s.value);
    return [Math.min(...values), Math.max(...values)];
  }

  flagged() {
    return this.samples.filter((s) => s.flagged).length;
  }

  /** The tooltip: "Frame rate, last 30 s: 24.5 fps to 30.0 fps\nMarked: skipped frames (1)". */
  about(what, value, flagged) {
    return trend(this.samples, this.elapsed() / 1000, what, value, flagged);
  }
}

/** A canvas kept at the device pixel ratio; returns its 2D context in CSS pixels, or null when unsized. */
const sized = (canvas) => {
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  if (!width || !height) return null;
  const ratio = Math.min(3, globalThis.devicePixelRatio || 1);
  const w = Math.round(width * ratio);
  const h = Math.round(height * ratio);
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  const ctx = canvas.getContext('2d');
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, width, height);
  return { ctx, width, height };
};

/** Draw the sparkline of `history` (sparkline.rs Sparkline / Baseline). `color`/`flag` are CSS colors. */
export const drawSparkline = (canvas, history, { color, flag, coming }) => {
  const area = sized(canvas);
  if (!area) return;
  const { ctx, width, height } = area;
  const inset = 2;
  if (!history.ready()) {
    if (!coming) return;
    // The zero line, dotted: a chart on its way rather than a rule.
    ctx.globalAlpha = 0.55;
    ctx.fillStyle = color;
    const y = Math.round(height - inset - 1);
    for (let x = inset; x + 1.5 <= width - inset; x += 4) ctx.fillRect(x, y, 1.5, 1.5);
    ctx.globalAlpha = 1;
    return;
  }
  const samples = history.samples;
  const newest = samples.at(-1).at;
  const top = Math.max(1, history.range()[1], Number.EPSILON);
  const w = width - inset * 2;
  const h = height - inset * 2;
  const points = samples.map((s) => {
    const age = Math.min(1, (newest - s.at) / history.span);
    return [inset + w * (1 - age), inset + h * (1 - Math.min(1, s.value / top))];
  });
  ctx.beginPath();
  ctx.moveTo(points[0][0], height);
  for (const [x, y] of points) ctx.lineTo(x, y);
  ctx.lineTo(points.at(-1)[0], height);
  ctx.closePath();
  ctx.globalAlpha = 0.16;
  ctx.fillStyle = color;
  ctx.fill();
  ctx.globalAlpha = 1;
  ctx.beginPath();
  points.forEach(([x, y], i) => (i ? ctx.lineTo(x, y) : ctx.moveTo(x, y)));
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.25;
  ctx.lineJoin = 'round';
  ctx.stroke();
  ctx.globalAlpha = 0.55;
  ctx.fillStyle = flag;
  samples.forEach((s, i) => { if (s.flagged) ctx.fillRect(points[i][0] - 0.75, 0, 1.5, height); });
  ctx.globalAlpha = 1;
  ctx.beginPath();
  ctx.arc(points.at(-1)[0], points.at(-1)[1], 1.75, 0, Math.PI * 2);
  ctx.fillStyle = color;
  ctx.fill();
};

/** The histogram plot (scopes.rs ExposureScope): RGB curves, white luma, sqrt scale, glowing clipped ends. */
const drawHistogram = (canvas, scopes) => {
  const area = sized(canvas);
  if (!area) return;
  const { ctx, width, height } = area;
  for (let quarter = 1; quarter < 4; quarter += 1) {
    const x = Math.round((width * quarter) / 4) + 0.5;
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, height);
    ctx.strokeStyle = rgba(WHITE, 0.07);
    ctx.lineWidth = 1;
    ctx.stroke();
  }
  ctx.fillStyle = rgba(WHITE, 0.14);
  ctx.fillRect(0, height - 1, width, 1);
  if (!scopes) return;
  const channels = scopes.mono || !scopes.red ? null : [scopes.red, scopes.green, scopes.blue];
  let max = 1;
  for (const bins of [scopes.luma, ...(channels ?? [])]) for (const n of bins) if (n > max) max = n;
  const step = width / scopes.luma.length;
  const y = (n) => height - Math.sqrt(n / max) * height;
  const curve = (bins, color, fill, line) => {
    ctx.beginPath();
    ctx.moveTo(0, height);
    ctx.lineTo(0, y(bins[0]));
    for (let i = 0; i < bins.length; i += 1) ctx.lineTo((i + 0.5) * step, y(bins[i]));
    ctx.lineTo(width, y(bins[bins.length - 1]));
    ctx.lineTo(width, height);
    ctx.closePath();
    const gradient = ctx.createLinearGradient(0, 0, 0, height);
    gradient.addColorStop(0, rgba(color, fill));
    gradient.addColorStop(1, rgba(color, fill * 0.25));
    ctx.fillStyle = gradient;
    ctx.fill();
    ctx.strokeStyle = rgba(color, line);
    ctx.lineWidth = 1.2;
    ctx.lineJoin = 'round';
    ctx.stroke();
  };
  if (channels) {
    channels.forEach((bins, i) => curve(bins, [RED, GREEN, BLUE][i], 0.22, 0.75));
    curve(scopes.luma, WHITE, 0.16, 0.9);
  } else {
    curve(scopes.luma, WHITE, 0.4, 0.95);
  }
  const glow = 16;
  for (const [share, from, to] of [[scopes.shadows, 0, glow], [scopes.highlights, width, width - glow]]) {
    if (!(f32(share) > CLIP_WARN)) continue;
    const gradient = ctx.createLinearGradient(from, 0, to, 0);
    gradient.addColorStop(0, rgba(WARN, 0.55));
    gradient.addColorStop(1, rgba(WARN, 0));
    ctx.fillStyle = gradient;
    ctx.fillRect(Math.min(from, to), 0, glow, height);
    ctx.fillStyle = rgba(WARN, 0.95);
    ctx.fillRect(from > 0 ? from - 2 : 0, 0, 2, height);
  }
};

/** Recent focus scores rising toward the dashed peak, newest on the right (scopes.rs Trace). */
const drawTrace = (canvas, values, peak, sharp) => {
  const area = sized(canvas);
  if (!area) return;
  const { ctx, width, height } = area;
  const top = 4;
  ctx.beginPath();
  ctx.setLineDash([3, 3]);
  ctx.moveTo(0, top);
  ctx.lineTo(width, top);
  ctx.strokeStyle = rgba(WHITE, 0.28);
  ctx.lineWidth = 1;
  ctx.stroke();
  ctx.setLineDash([]);
  if (!values.length || !(peak > 0)) return;
  const color = sharp ? LIVE : ACCENT;
  const step = width / (HISTORY - 1);
  const start = width - (values.length - 1) * step;
  const point = (i, v) => [start + i * step, height - Math.min(1, Math.max(0, v / peak)) * (height - top)];
  ctx.beginPath();
  ctx.moveTo(start, height);
  values.forEach((v, i) => ctx.lineTo(...point(i, v)));
  ctx.lineTo(width, height);
  ctx.closePath();
  const gradient = ctx.createLinearGradient(0, top, 0, height);
  gradient.addColorStop(0, rgba(color, 0.35));
  gradient.addColorStop(1, rgba(color, 0));
  ctx.fillStyle = gradient;
  ctx.fill();
  ctx.beginPath();
  values.forEach((v, i) => (i ? ctx.lineTo(...point(i, v)) : ctx.moveTo(...point(i, v))));
  ctx.strokeStyle = rgba(color, 1);
  ctx.lineWidth = 1.5;
  ctx.lineJoin = 'round';
  ctx.stroke();
  const end = point(values.length - 1, values.at(-1));
  ctx.beginPath();
  ctx.arc(end[0], end[1], 4.5, 0, Math.PI * 2);
  ctx.fillStyle = rgba(color, 0.3);
  ctx.fill();
  ctx.beginPath();
  ctx.arc(end[0], end[1], 2.5, 0, Math.PI * 2);
  ctx.fillStyle = rgba(color, 1);
  ctx.fill();
};

const make = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};

/**
 * The scope cards in the stage's top right corner. `on` holds callbacks:
 * hideFocus(), resetPeak(), metric(key), adjust() (a clipping chip pressed).
 */
export class Scopes {
  element = make('div', 'scopes');
  #histogram = { card: null };
  #focus = { card: null };
  #exposure = null;
  #meter = null;
  #on;
  #resize;

  constructor(on = {}) {
    this.#on = on;
    this.#buildHistogram();
    this.#buildFocus();
    this.element.append(this.#histogram.card, this.#focus.card);
    this.#resize = new ResizeObserver(() => this.#draw());
    this.#resize.observe(this.#histogram.plot);
    this.#resize.observe(this.#focus.trace);
  }

  #buildHistogram() {
    const card = make('section', 'scope glass histogram');
    card.setAttribute('aria-label', 'Histogram');
    card.hidden = true;
    const header = make('header');
    const title = make('h3', '', 'Histogram');
    // Not "Exposure": that is the control in the inspector.
    const mean = make('span', 'mean', 'Mean –');
    mean.setAttribute('aria-hidden', 'true');
    header.append(title, mean);
    const plot = make('canvas', 'plot');
    plot.setAttribute('aria-hidden', 'true');
    const footer = make('footer');
    const chip = (label, about) => {
      const node = make('button', 'chip');
      node.type = 'button';
      node.title = about;
      const text = make('span', '', `${label} 0%`);
      node.append(make('span', 'dot'), text);
      node.addEventListener('click', () => this.#on.adjust?.());
      return { node, text, label, about };
    };
    const shadows = chip('Shadows', 'Samples crushed to black');
    const highlights = chip('Highlights', 'Samples with a channel blown out');
    footer.append(shadows.node, highlights.node);
    const summary = make('p', 'sr-only', 'Histogram: waiting for a frame.');
    card.append(header, plot, footer, summary);
    this.#histogram = { card, mean, plot, shadows, highlights, summary };
  }

  #buildFocus() {
    const card = make('section', 'scope glass focus');
    card.setAttribute('aria-label', 'Focus');
    card.hidden = true;
    const header = make('header');
    const title = make('h3', '', 'Focus');
    const select = make('select');
    select.setAttribute('aria-label', 'Focus measure');
    for (const m of METRICS) {
      const option = make('option', '', m.plain);
      option.value = m.key;
      select.append(option);
    }
    select.addEventListener('change', () => this.#on.metric?.(select.value));
    const glassButton = (name, label) => {
      const button = make('button', 'btn glass');
      button.type = 'button';
      button.title = label;
      button.setAttribute('aria-label', label);
      button.append(icon(name, 12));
      // 5 + 12 + 5 plus the border: a 24 px target (WCAG 2.5.8) beside its neighbour.
      button.style.padding = '5px';
      return button;
    };
    const reset = glassButton('reset', 'Reset peak');
    reset.addEventListener('click', () => this.#on.resetPeak?.());
    const close = glassButton('x', 'Hide focus region');
    close.addEventListener('click', () => this.#on.hideFocus?.());
    header.append(title, select, reset, close);
    const reading = make('div', 'reading');
    const score = make('span', 'score', '–');
    const share = make('div', 'share');
    const ofPeak = make('span', 'of-peak', '');
    const verdict = make('span', 'verdict', 'Waiting for a frame');
    share.append(ofPeak, verdict);
    reading.append(score, share);
    reading.setAttribute('aria-hidden', 'true');
    const trace = make('canvas', 'trace');
    trace.setAttribute('aria-hidden', 'true');
    const summary = make('p', 'sr-only', 'Focus: waiting for a frame.');
    card.append(header, reading, trace, summary);
    this.#focus = { card, select, reading, score, ofPeak, verdict, trace, summary, check: icon('check-circle', 11) };
  }

  get histogramVisible() {
    return !this.#histogram.card.hidden;
  }

  get focusVisible() {
    return !this.#focus.card.hidden;
  }

  show(name, visible) {
    const card = name === 'focus' ? this.#focus.card : this.#histogram.card;
    card.hidden = !visible;
    this.element.hidden = this.#histogram.card.hidden && this.#focus.card.hidden;
    if (visible) this.#draw();
  }

  /** New histogram figures from the engine (`analyze().scopes`), or null to clear. */
  setExposure(scopes) {
    this.#exposure = scopes;
    const h = this.#histogram;
    if (!scopes) {
      h.mean.textContent = 'Mean –';
      h.summary.textContent = 'Histogram: waiting for a frame.';
      for (const chip of [h.shadows, h.highlights]) {
        chip.text.textContent = `${chip.label} 0%`;
        chip.node.classList.remove('over');
      }
    } else {
      const mean = `Mean ${fixed(f32(f32(scopes.mean) * 100), 0)}%`;
      h.mean.textContent = mean;
      const parts = [mean];
      for (const [chip, share] of [[h.shadows, scopes.shadows], [h.highlights, scopes.highlights]]) {
        const text = `${chip.label} ${percent(share)}`;
        const over = f32(share) > CLIP_WARN;
        if (chip.text.textContent !== text) chip.text.textContent = text;
        chip.node.classList.toggle('over', over);
        chip.node.setAttribute('aria-label', text);
        parts.push(over ? `${text}, clipped` : text);
      }
      h.summary.textContent = `Histogram: ${parts.join(', ')}.`;
    }
    if (this.histogramVisible) drawHistogram(h.plot, scopes);
  }

  /** Show the meter's latest state. */
  setFocus(meter) {
    this.#meter = meter;
    const f = this.#focus;
    if (f.select.value !== meter.metric) f.select.value = meter.metric;
    f.select.title = metricAbout(meter.metric);
    const latest = meter.latest();
    const share = meter.share();
    const sharp = meter.inFocus;
    f.card.classList.toggle('sharp', sharp);
    f.score.textContent = latest === null ? '–' : focusScore(latest);
    f.ofPeak.textContent = share === null ? '' : `${fixed(f32(share * 100), 0)}% of peak`;
    const verdict = meter.verdict();
    if (sharp) f.verdict.replaceChildren(f.check, verdict);
    else f.verdict.replaceChildren(verdict);
    const plain = metricOf(meter.metric).plain;
    const end = (text) => (text.endsWith('…') ? text : `${text}.`);
    f.summary.textContent = latest === null
      ? `Focus, ${plain}: ${end(verdict.toLowerCase())}`
      : `Focus, ${plain}: score ${focusScore(latest)}, ${f.ofPeak.textContent}. ${end(verdict)}`;
    if (this.focusVisible) drawTrace(f.trace, meter.trace(), meter.peakValue(), sharp);
  }

  #draw() {
    if (this.histogramVisible) drawHistogram(this.#histogram.plot, this.#exposure);
    if (this.focusVisible && this.#meter) drawTrace(this.#focus.trace, this.#meter.trace(), this.#meter.peakValue(), this.#meter.inFocus);
    else if (this.focusVisible) drawTrace(this.#focus.trace, [], 0, false);
  }

  destroy() {
    this.#resize.disconnect();
  }
}
