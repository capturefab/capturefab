// The camera stage (src/gui/stage.rs): the picture on dark, fitted or zoomed, with
// its glass control bar, badges, capture flash and the focus region editor
// (src/gui/scopes.rs RegionEditor) floating over it.
import { icon } from './icons.mjs';
import { fixed, silence, withChord } from './format.mjs';
import { MIN_PIXELS } from './scopes.mjs';

const f32 = Math.fround;
const ZOOM_MIN = 0.1;
const ZOOM_MAX = 8;
const ZOOM_STEP = 1.25;
const ZOOM_MS = 180;
const LOCK_MS = 260;
const CONTROLS_IDLE = 2000;
/** How close to an edge or corner of the region grabs it, in screen points. */
const GRIP = 8;
const ON_STAGE = '#f5f5f7';
const ACCENT_TEXT = '#a8caff';
const LIVE_INK = '#ffffff';

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const reduceMotion = () => globalThis.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false;
const easeOut = (t) => 1 - (1 - t) ** 3;

const make = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};

/** The 8-spoke activity indicator drawn by workbench.css. */
export const spinner = (size = 14) => {
  const node = make('span', 'spinner');
  node.setAttribute('aria-hidden', 'true');
  if (size !== 14) node.style.setProperty('--size', `${size}px`);
  return node;
};

const glassButton = (name, label, size = 14) => {
  const button = make('button', 'btn glass');
  button.type = 'button';
  button.setAttribute('aria-label', label);
  button.append(icon(name, size));
  return button;
};

/**
 * The stage. `on` holds callbacks: histogram(), focus(), image(), region(region, done),
 * start(), captureOne(), wake(). Coordinates of the focus region are normalized to the
 * frame as the camera sends it, never mirrored.
 */
export class Stage {
  element = make('div', 'stage');
  #on;
  #isApple;
  #singleKeys;
  #viewport = make('div', 'viewport');
  #view = null;
  #overlay = make('canvas', 'overlay');
  #handle = make('div', 'region-handle');
  #flash = make('div', 'flash');
  #badges = make('div', 'badges');
  #ready = make('div', 'ready');
  #controls = make('div', 'controls glass');
  #ui = {};
  #natural = { width: 0, height: 0 };
  #box = { width: 0, height: 0 };
  #image = { x: 0, y: 0, width: 0, height: 0 };
  #fit = true;
  #zoom = 1;
  #pan = { x: 0, y: 0 };
  #animation = null;
  #mirror = false;
  #region = { visible: false, region: [0.375, 0.375, 0.25, 0.25], label: '', sharp: false };
  #drag = null;
  #pointers = new Map();
  #pinch = null;
  #lock = { at: -Infinity, raf: 0 };
  #idle = 0;
  #frame = 0;
  #resize;
  #ac = new AbortController();

  constructor({ isApple = false, singleKeys = true, on = {} } = {}) {
    this.#on = on;
    this.#isApple = isApple;
    this.#singleKeys = Boolean(singleKeys);
    this.#viewport.setAttribute('role', 'img');
    this.#viewport.setAttribute('aria-label', 'Camera preview');
    this.#overlay.setAttribute('aria-hidden', 'true');
    this.#flash.setAttribute('aria-hidden', 'true');
    this.#ready.hidden = true;
    this.#buildHandle();
    this.#buildControls();
    this.element.append(this.#viewport, this.#overlay, this.#handle, this.#flash, this.#badges, this.#ready, this.#controls);
    this.#resize = new ResizeObserver(([entry]) => {
      const box = entry.contentBoxSize?.[0];
      this.#box = box ? { width: box.inlineSize, height: box.blockSize } : { width: entry.contentRect.width, height: entry.contentRect.height };
      this.layout();
    });
    this.#resize.observe(this.element);
    this.#listen();
  }

  /** Where the scope cards go. */
  mountScopes(node) {
    this.element.insertBefore(node, this.#controls);
  }

  // Picture

  /** Show `view` (a video or canvas) as the picture, or nothing. */
  setView(view) {
    if (view === this.#view) return;
    this.#view?.remove();
    this.#view = view;
    if (view) this.#viewport.append(view);
    this.layout();
  }

  /** The size of the frames the camera sends. */
  setNatural(width, height) {
    if (width === this.#natural.width && height === this.#natural.height) return;
    this.#natural = { width, height };
    this.layout();
  }

  get natural() {
    return { ...this.#natural };
  }

  setMirror(on) {
    if (this.#mirror === Boolean(on)) return;
    this.#mirror = Boolean(on);
    this.layout();
  }

  /** The screen reader's name for the picture (stage.rs:830-840). */
  setLabel(text) {
    if (this.#viewport.getAttribute('aria-label') !== text) this.#viewport.setAttribute('aria-label', text);
  }

  /** Fade the picture in on a camera's first frame. */
  reveal() {
    if (reduceMotion()) return;
    this.#viewport.classList.add('fading');
    requestAnimationFrame(() => requestAnimationFrame(() => this.#viewport.classList.remove('fading')));
  }

  // Zoom and pan

  #fitScale() {
    const { width: nw, height: nh } = this.#natural;
    if (!nw || !nh || !this.#box.width || !this.#box.height) return 1;
    return Math.max(0.01, Math.min(this.#box.width / nw, this.#box.height / nh));
  }

  /** The scale shown now, mid-animation included. */
  get scale() {
    const target = this.#fit ? this.#fitScale() : this.#zoom;
    const a = this.#animation;
    if (!a) return target;
    const t = clamp((performance.now() - a.start) / ZOOM_MS, 0, 1);
    if (t >= 1) return target;
    return a.from * (target / a.from) ** easeOut(t);
  }

  get fitted() {
    return this.#fit;
  }

  #animate(from) {
    if (reduceMotion() || !(from > 0)) {
      this.#animation = null;
      this.layout();
      return;
    }
    this.#animation = { start: performance.now(), from };
    const step = () => {
      this.layout();
      if (this.#animation && performance.now() - this.#animation.start < ZOOM_MS) this.#animation.raf = requestAnimationFrame(step);
      else this.#animation = null;
    };
    cancelAnimationFrame(this.#animation.raf ?? 0);
    this.#animation.raf = requestAnimationFrame(step);
  }

  /** Zoom by `factor` about the stage point `at` (screen coordinates in the stage), or its center. */
  zoomBy(factor, at = null) {
    if (!this.#natural.width) return;
    const from = this.scale;
    const to = clamp(from * factor, ZOOM_MIN, ZOOM_MAX);
    if (Math.abs(to - from) < 1e-6) return;
    this.#zoomTo(to, at, from);
  }

  #zoomTo(to, at, from = this.scale) {
    if (at) {
      // Keep the image point under `at` where it is.
      const cx = this.#box.width / 2;
      const cy = this.#box.height / 2;
      const ratio = to / from;
      this.#pan.x = (at.x - cx) - (at.x - cx - this.#pan.x) * ratio;
      this.#pan.y = (at.y - cy) - (at.y - cy - this.#pan.y) * ratio;
    } else if (this.#fit) {
      this.#pan = { x: 0, y: 0 };
    } else {
      this.#pan.x *= to / from;
      this.#pan.y *= to / from;
    }
    this.#fit = false;
    this.#zoom = to;
    this.#animate(at ? 0 : from);
    this.#on.wake?.();
  }

  zoomIn() {
    this.zoomBy(ZOOM_STEP);
  }

  zoomOut() {
    this.zoomBy(1 / ZOOM_STEP);
  }

  /** Zoom to fit. */
  fit() {
    if (this.#fit) return;
    const from = this.scale;
    this.#fit = true;
    this.#pan = { x: 0, y: 0 };
    this.#animate(from);
  }

  /** Actual pixels: one frame pixel per CSS pixel. */
  actual() {
    if (!this.#natural.width) return;
    if (!this.#fit && Math.abs(this.#zoom - 1) < 0.01) return;
    this.#zoomTo(1, null);
  }

  /** Move the picture by (dx, dy) screen points, as far as it can go (keyboard panning). */
  panBy(dx, dy) {
    if (!this.#pannable()) return;
    this.#pan.x += dx;
    this.#pan.y += dy;
    this.layout();
    this.#on.wake?.();
  }

  /** Make the picture a focusable, arrow-key pannable group while zoomed past the stage. */
  #syncPannable() {
    const viewport = this.#viewport;
    const pannable = Boolean(this.#natural.width && this.#view) && this.#pannable();
    if (!pannable) {
      if (!viewport.hasAttribute('tabindex')) return;
      const focused = this.element.getRootNode().activeElement === viewport;
      viewport.removeAttribute('tabindex');
      viewport.setAttribute('role', 'img');
      viewport.removeAttribute('aria-roledescription');
      viewport.removeAttribute('aria-description');
      if (focused) this.#ui.fit?.focus();
      return;
    }
    if (!viewport.hasAttribute('tabindex')) {
      viewport.tabIndex = 0;
      viewport.setAttribute('role', 'group');
      viewport.setAttribute('aria-roledescription', 'zoomed preview');
    }
    // Where the view is, so a pan can be heard: the stage centre as a share of the frame, on screen.
    const img = this.#image;
    const across = Math.round(clamp((this.#box.width / 2 - img.x) / img.width, 0, 1) * 100);
    const down = Math.round(clamp((this.#box.height / 2 - img.y) / img.height, 0, 1) * 100);
    const text = `Zoomed to ${fixed(this.scale * 100, 0)}%, centred ${across}% across and ${down}% down. Arrow keys pan; with Shift, further.`;
    if (viewport.getAttribute('aria-description') !== text) viewport.setAttribute('aria-description', text);
  }

  #panKey(event) {
    const arrows = { ArrowLeft: [1, 0], ArrowRight: [-1, 0], ArrowUp: [0, 1], ArrowDown: [0, -1] };
    const step = arrows[event.key];
    if (!step || event.metaKey || event.ctrlKey || event.altKey || !this.#pannable()) return;
    event.preventDefault();
    event.stopPropagation();
    // Left shows more of the picture's left side: the picture moves right under the stage.
    const share = event.shiftKey ? 1 : 0.1;
    this.panBy(step[0] * this.#box.width * share, step[1] * this.#box.height * share);
  }

  /** Place the picture and everything over it. */
  layout() {
    const { width: nw, height: nh } = this.#natural;
    const { width: cw, height: ch } = this.#box;
    const scale = this.scale;
    const width = nw * scale;
    const height = nh * scale;
    const room = { x: Math.max(0, (width - cw) / 2), y: Math.max(0, (height - ch) / 2) };
    this.#pan.x = clamp(this.#pan.x, -room.x, room.x);
    this.#pan.y = clamp(this.#pan.y, -room.y, room.y);
    const x = (cw - width) / 2 + this.#pan.x;
    const y = (ch - height) / 2 + this.#pan.y;
    this.#image = { x, y, width, height };
    const view = this.#view;
    if (view && nw && nh) {
      view.style.width = `${width}px`;
      view.style.height = `${height}px`;
      view.style.transform = this.#mirror ? `translate(${x + width}px, ${y}px) scaleX(-1)` : `translate(${x}px, ${y}px)`;
    }
    this.#viewport.classList.toggle('pixelated', scale >= 2);
    const ui = this.#ui;
    if (ui.readout) {
      ui.readout.textContent = nw ? `${fixed(scale * 100, 0)}%` : '–';
      const resting = this.#fit ? this.#fitScale() : this.#zoom;
      ui.zoomOut.disabled = !nw || resting <= ZOOM_MIN + 1e-3;
      ui.zoomIn.disabled = !nw || resting >= ZOOM_MAX - 1e-3;
      ui.fit.setAttribute('aria-pressed', String(this.#fit));
      ui.actual.setAttribute('aria-pressed', String(!this.#fit && Math.abs(this.#zoom - 1) < 0.01));
    }
    this.#overlay.classList.toggle('panning', !this.#region.visible && this.#pannable());
    this.#syncPannable();
    this.#placeHandle();
    this.#draw();
  }

  #pannable() {
    return this.#image.width > this.#box.width + 0.5 || this.#image.height > this.#box.height + 0.5;
  }

  // Badges, ready screen, flash

  /** `lastFrame` when stopped with a picture; `stall` ms without a frame (0 for none). */
  setBadges({ lastFrame = false, stall = 0 } = {}) {
    const key = `${lastFrame}|${stall ? silence(stall) : ''}`;
    if (this.#badges.dataset.key === key) return;
    this.#badges.dataset.key = key;
    const badges = [];
    if (lastFrame) badges.push(make('span', 'badge', 'Last frame'));
    if (stall) {
      const badge = make('span', 'badge');
      badge.append(icon('warning', 12), silence(stall));
      badges.push(badge);
    }
    this.#badges.replaceChildren(...badges);
  }

  /**
   * The screen before the first frame (welcome.rs ready()): `null` hides it, else
   * `{ title, text?, start?: {busy}, capture?: {busy, disabled} }`.
   */
  setReady(state) {
    const ready = this.#ready;
    if (!state) {
      ready.hidden = true;
      return;
    }
    const key = JSON.stringify(state);
    ready.hidden = false;
    if (ready.dataset.key === key) return;
    ready.dataset.key = key;
    const children = [icon('camera', 40), make('h2', '', state.title)];
    if (state.text) children.push(make('p', '', state.text));
    if (state.start || state.capture) {
      const actions = make('div', 'actions');
      if (state.start) {
        const start = make('button', 'btn primary');
        start.type = 'button';
        start.append(state.start.busy ? spinner() : icon('play-fill', 14), 'Start stream');
        start.disabled = Boolean(state.start.busy);
        start.title = this.#keyTitle('Start or stop acquisition', 'Space');
        if (this.#singleKeys) start.setAttribute('aria-keyshortcuts', 'Space');
        start.addEventListener('click', () => this.#on.start?.());
        actions.append(start);
      }
      if (state.capture) {
        const capture = make('button', 'btn glass', 'Capture one frame');
        capture.type = 'button';
        capture.disabled = Boolean(state.capture.busy || state.capture.disabled);
        capture.addEventListener('click', () => this.#on.captureOne?.());
        actions.append(capture);
      }
      children.push(actions);
    }
    ready.replaceChildren(...children);
  }

  /** The capture flash over the picture: a white wash and edge for 320 ms (edge only under Reduce Motion). */
  flash() {
    const { x, y, width, height } = this.#image;
    const left = Math.max(0, x);
    const top = Math.max(0, y);
    const right = Math.min(this.#box.width, x + width);
    const bottom = Math.min(this.#box.height, y + height);
    if (right <= left || bottom <= top) return;
    const flash = this.#flash;
    Object.assign(flash.style, { transform: `translate(${left}px, ${top}px)`, width: `${right - left}px`, height: `${bottom - top}px` });
    flash.classList.remove('on');
    void flash.offsetWidth;
    flash.classList.add('on');
  }

  // Controls

  #buildControls() {
    const bar = this.#controls;
    bar.setAttribute('role', 'group');
    bar.setAttribute('aria-label', 'View');
    const chord = (text, spec) => withChord(text, spec, this.#isApple);
    const segments = make('div', 'segmented glass');
    segments.setAttribute('role', 'group');
    segments.setAttribute('aria-label', 'Zoom');
    const segment = (label, about, spec, act) => {
      const button = make('button', '', label);
      button.type = 'button';
      button.title = chord(about, spec);
      button.setAttribute('aria-keyshortcuts', spec.replace('Mod', this.#isApple ? 'Meta' : 'Control'));
      button.addEventListener('click', act);
      segments.append(button);
      return button;
    };
    const fit = segment('Fit', 'Zoom to fit', 'Mod+0', () => this.fit());
    const actual = segment('1:1', 'Actual pixels', 'Mod+Alt+0', () => this.actual());
    const group = make('div', 'group');
    const zoomOut = glassButton('minus', 'Zoom out');
    zoomOut.title = chord('Zoom out', 'Mod+-');
    zoomOut.addEventListener('click', () => this.zoomOut());
    const readout = make('span', 'zoom-readout', '–');
    readout.setAttribute('aria-label', 'Zoom');
    const zoomIn = glassButton('plus', 'Zoom in');
    zoomIn.title = chord('Zoom in', this.#isApple ? 'Mod++' : 'Mod+=');
    zoomIn.addEventListener('click', () => this.zoomIn());
    group.append(zoomOut, readout, zoomIn);
    const histogram = glassButton('chart-bar', 'Histogram');
    histogram.title = this.#keyTitle('Histogram', 'H');
    histogram.setAttribute('aria-pressed', 'false');
    histogram.addEventListener('click', () => this.#on.histogram?.());
    const focus = glassButton('scan', 'Focus region');
    focus.title = this.#keyTitle('Focus region', 'R');
    focus.setAttribute('aria-pressed', 'false');
    focus.addEventListener('click', () => this.#on.focus?.());
    const details = make('span', 'details');
    details.style.whiteSpace = 'pre';
    const image = glassButton('corners-out', 'Image only');
    image.classList.add('image');
    image.title = this.#keyTitle('Image only', 'F');
    image.setAttribute('aria-pressed', 'false');
    image.addEventListener('click', () => this.#on.image?.());
    bar.append(segments, group, histogram, focus, details, image);
    this.#ui = { fit, actual, zoomOut, zoomIn, readout, histogram, focus, details, image };
  }

  /** A tooltip naming the single-key shortcut `key` only while those shortcuts are on. */
  #keyTitle(text, key) {
    return this.#singleKeys ? withChord(text, key, this.#isApple) : text;
  }

  /** Whether the single-key shortcuts (Space, H, R, F) work, so tooltips name them or not. */
  setSingleKeys(on) {
    on = Boolean(on);
    if (on === this.#singleKeys) return;
    this.#singleKeys = on;
    const ui = this.#ui;
    ui.histogram.title = this.#keyTitle('Histogram', 'H');
    ui.focus.title = this.#keyTitle('Focus region', 'R');
    ui.image.title = this.#keyTitle(ui.image.getAttribute('aria-label') ?? 'Image only', 'F');
    // The ready screen's Start button names Space: rebuild it.
    const ready = this.#ready;
    if (ready.dataset.key) {
      const state = JSON.parse(ready.dataset.key);
      delete ready.dataset.key;
      if (!ready.hidden) this.setReady(state);
    }
  }

  /**
   * Which scopes show, and whether the picture is alone. `intensity` is false with no
   * camera; `frame` (default `intensity`) is whether there is a picture to show alone.
   */
  setToggles({ histogram, focus, image, intensity = true, frame = intensity }) {
    const ui = this.#ui;
    const key = `${histogram}|${focus}|${image}|${intensity}|${frame}`;
    if (ui.toggles === key) return;
    ui.toggles = key;
    ui.histogram.setAttribute('aria-pressed', String(Boolean(histogram)));
    ui.focus.setAttribute('aria-pressed', String(Boolean(focus)));
    ui.histogram.disabled = !intensity;
    ui.focus.disabled = !intensity;
    ui.image.setAttribute('aria-pressed', String(Boolean(image)));
    // Image only needs a picture; it is left when the picture goes, so never disabled while on.
    ui.image.disabled = !frame && !image;
    const label = image ? 'Leave image only' : 'Image only';
    ui.image.setAttribute('aria-label', label);
    ui.image.title = this.#keyTitle(label, 'F');
    ui.image.replaceChildren(icon(image ? 'corners-in' : 'corners-out', 14));
  }

  /** The frame's details, joined by "  ·  "; `lost` is an optional "{n} skipped" in amber while recent. */
  setDetails(parts, lost = null) {
    const ui = this.#ui;
    const key = `${parts.join('|')}|${lost?.text ?? ''}|${lost?.recent ?? ''}`;
    if (ui.details.dataset.key === key) return;
    ui.details.dataset.key = key;
    const nodes = [parts.join('  ·  ')];
    if (lost) {
      const span = make('span', lost.recent ? 'lost' : '', lost.text);
      if (lost.about) span.title = lost.about;
      nodes.push('  ·  ', span);
    }
    ui.details.replaceChildren(...nodes);
  }

  /** Show the control bar, then let it rest after the pointer stops for 2 s. */
  wake() {
    this.#controls.classList.remove('resting');
    clearTimeout(this.#idle);
    // Without a hovering pointer nothing would bring them back but a tap: they stay.
    if (globalThis.matchMedia?.('(hover: none)').matches) return;
    this.#idle = setTimeout(() => this.#controls.classList.add('resting'), CONTROLS_IDLE);
  }

  // Focus region

  /** `{ visible, region:[x,y,w,h], label, sharp, locks }`; a rise in `locks` plays the lock-on. */
  setRegion(state) {
    const before = this.#region;
    this.#region = { ...before, ...state };
    if (state.locks !== undefined && before.locks !== undefined && state.locks > before.locks) this.#playLock();
    this.#overlay.classList.toggle('editing', this.#region.visible);
    this.#handle.hidden = !this.#region.visible;
    this.layout();
  }

  #playLock() {
    this.#lock.at = performance.now();
    cancelAnimationFrame(this.#lock.raf);
    const step = () => {
      this.#draw();
      if (performance.now() - this.#lock.at < LOCK_MS) this.#lock.raf = requestAnimationFrame(step);
    };
    this.#lock.raf = requestAnimationFrame(step);
  }

  /** The region on screen, mirrored like the picture. */
  #onScreen(region = this.#region.region) {
    const [x, y, w, h] = region;
    const img = this.#image;
    const sx = this.#mirror ? 1 - x - w : x;
    return { x: img.x + sx * img.width, y: img.y + y * img.height, width: w * img.width, height: h * img.height };
  }

  /** A stage point in normalized frame coordinates, kept inside the frame. */
  #normalized(point) {
    const img = this.#image;
    let x = clamp((point.x - img.x) / img.width, 0, 1);
    const y = clamp((point.y - img.y) / img.height, 0, 1);
    if (this.#mirror) x = 1 - x;
    return { x, y };
  }

  #grip(point) {
    const r = this.#onScreen();
    const right = r.x + r.width;
    const bottom = r.y + r.height;
    const across = point.y > r.y - GRIP && point.y < bottom + GRIP;
    const along = point.x > r.x - GRIP && point.x < right + GRIP;
    const near = (a, b) => Math.abs(a - b) < GRIP;
    // On a small region both edges are near: take the closer one.
    const l = across && near(point.x, r.x) && Math.abs(point.x - r.x) <= Math.abs(point.x - right);
    const rr = across && near(point.x, right) && !l;
    const t = along && near(point.y, r.y) && Math.abs(point.y - r.y) <= Math.abs(point.y - bottom);
    const b = along && near(point.y, bottom) && !t;
    if (l || t || rr || b) {
      // Screen sides to frame sides: mirroring swaps left and right.
      return { kind: 'sides', sides: this.#mirror ? [rr, t, l, b] : [l, t, rr, b] };
    }
    if (point.x >= r.x && point.x <= right && point.y >= r.y && point.y <= bottom) return { kind: 'move' };
    return null;
  }

  #minSize() {
    const { width, height } = this.#natural;
    return { w: Math.min(1, MIN_PIXELS / Math.max(1, width)), h: Math.min(1, MIN_PIXELS / Math.max(1, height)) };
  }

  /** The region after dragging `grip` from `from` to `to`, all normalized (RegionEditor::dragged). */
  #dragged(grip, from, to, start) {
    const base = this.#minSize();
    const min = { w: Math.min(base.w, start[2]), h: Math.min(base.h, start[3]) };
    const dx = to.x - from.x;
    const dy = to.y - from.y;
    if (grip.kind === 'move') {
      return [clamp(start[0] + dx, 0, 1 - start[2]), clamp(start[1] + dy, 0, 1 - start[3]), start[2], start[3]];
    }
    if (grip.kind === 'sides') {
      const [left, top, right, bottom] = grip.sides;
      let x0 = start[0];
      let y0 = start[1];
      let x1 = start[0] + start[2];
      let y1 = start[1] + start[3];
      if (left) x0 = clamp(x0 + dx, 0, x1 - min.w);
      if (right) x1 = clamp(x1 + dx, x0 + min.w, 1);
      if (top) y0 = clamp(y0 + dy, 0, y1 - min.h);
      if (bottom) y1 = clamp(y1 + dy, y0 + min.h, 1);
      return [x0, y0, x1 - x0, y1 - y0];
    }
    const w = Math.max(Math.abs(to.x - from.x), min.w);
    const h = Math.max(Math.abs(to.y - from.y), min.h);
    const x = to.x < from.x ? from.x - w : from.x;
    const y = to.y < from.y ? from.y - h : from.y;
    return [clamp(x, 0, 1 - w), clamp(y, 0, 1 - h), w, h];
  }

  /** The current region centered on `point`, for a click outside it. */
  #centered(point) {
    const [, , w, h] = this.#region.region;
    return [clamp(point.x - w / 2, 0, 1 - w), clamp(point.y - h / 2, 0, 1 - h), w, h];
  }

  #inImage(point) {
    const img = this.#image;
    return point.x >= img.x && point.x <= img.x + img.width && point.y >= img.y && point.y <= img.y + img.height;
  }

  #buildHandle() {
    const handle = this.#handle;
    handle.tabIndex = 0;
    handle.hidden = true;
    handle.setAttribute('role', 'group');
    handle.setAttribute('aria-roledescription', 'focus region');
    handle.setAttribute('aria-label', 'Focus region');
    handle.setAttribute('aria-description', 'Arrow keys move it; with Shift, further. Alt with arrow keys changes its size.');
    handle.addEventListener('keydown', (event) => {
      const arrows = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] };
      const step = arrows[event.key];
      if (!step || event.metaKey || event.ctrlKey) return;
      event.preventDefault();
      event.stopPropagation();
      const amount = event.shiftKey ? 0.1 : 0.01;
      const [x, y, w, h] = this.#region.region;
      const min = this.#minSize();
      let next;
      if (event.altKey) {
        const h2 = clamp(h + step[1] * amount, min.h, 1 - y);
        if (this.#mirror) {
          // Mirrored, the screen-left edge is the frame's right edge (x + w): keep it put.
          const w2 = clamp(w + step[0] * amount, min.w, x + w);
          next = [x + w - w2, y, w2, h2];
        } else {
          next = [x, y, clamp(w + step[0] * amount, min.w, 1 - x), h2];
        }
      } else {
        const dx = (this.#mirror ? -step[0] : step[0]) * amount;
        next = [clamp(x + dx, 0, 1 - w), clamp(y + step[1] * amount, 0, 1 - h), w, h];
      }
      this.#on.region?.(next.map(f32), true);
    });
  }

  #placeHandle() {
    const handle = this.#handle;
    if (!this.#region.visible || !this.#natural.width) return;
    const r = this.#onScreen();
    Object.assign(handle.style, { transform: `translate(${r.x}px, ${r.y}px)`, width: `${Math.max(1, r.width)}px`, height: `${Math.max(1, r.height)}px` });
    const [x, y, w, h] = this.#region.region;
    const { width, height } = this.#natural;
    const label = `Focus region, ${Math.round(w * width)} by ${Math.round(h * height)} pixels at ${Math.round(x * width)}, ${Math.round(y * height)}`;
    if (handle.getAttribute('aria-label') !== label) handle.setAttribute('aria-label', label);
  }

  /** Redraw the region editor on the next animation frame. */
  #draw() {
    if (this.#frame) return;
    this.#frame = requestAnimationFrame(() => {
      this.#frame = 0;
      this.#paint();
    });
  }

  #paint() {
    const canvas = this.#overlay;
    const { width: cw, height: ch } = this.#box;
    const ratio = Math.min(3, globalThis.devicePixelRatio || 1);
    const w = Math.round(cw * ratio);
    const h = Math.round(ch * ratio);
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
    }
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    ctx.clearRect(0, 0, cw, ch);
    if (!this.#region.visible || !this.#natural.width || !this.#view) return;
    const img = this.#image;
    const r = this.#onScreen();
    const right = r.x + r.width;
    const bottom = r.y + r.height;
    // Dim the image around the region, in one even-odd fill so no seams show.
    ctx.beginPath();
    ctx.rect(img.x, img.y, img.width, img.height);
    ctx.rect(r.x, r.y, r.width, r.height);
    ctx.fillStyle = 'rgba(0, 0, 0, 0.4)';
    ctx.fill('evenodd');
    ctx.lineWidth = 3;
    ctx.strokeStyle = 'rgba(0, 0, 0, 0.85)';
    ctx.strokeRect(r.x, r.y, r.width, r.height);
    ctx.lineWidth = 1;
    ctx.strokeStyle = 'rgba(255, 255, 255, 0.9)';
    ctx.strokeRect(r.x, r.y, r.width, r.height);
    const color = this.#region.sharp ? LIVE_INK : ACCENT_TEXT;
    // Locking on: 0 at either end, 1 at the height of it.
    const t = clamp((performance.now() - this.#lock.at) / LOCK_MS, 0, 1);
    const bump = t < 1 ? Math.sin((1 - t) * Math.PI) : 0;
    if (bump > 0.01) {
      ctx.lineWidth = 1;
      ctx.strokeStyle = color;
      ctx.globalAlpha = 0.5 * bump;
      ctx.strokeRect(r.x - 3, r.y - 3, r.width + 6, r.height + 6);
      ctx.globalAlpha = 1;
    }
    const swell = reduceMotion() ? 0 : bump;
    const arm = Math.min(14 + 6 * swell, r.width / 3, r.height / 3);
    ctx.beginPath();
    for (const [x, y, sx, sy] of [[r.x, r.y, 1, 1], [right, r.y, -1, 1], [r.x, bottom, 1, -1], [right, bottom, -1, -1]]) {
      ctx.moveTo(x + sx * arm, y);
      ctx.lineTo(x, y);
      ctx.lineTo(x, y + sy * arm);
    }
    ctx.lineCap = 'round';
    ctx.lineJoin = 'miter';
    ctx.lineWidth = 5 + swell;
    ctx.strokeStyle = 'rgba(0, 0, 0, 0.85)';
    ctx.stroke();
    ctx.lineWidth = 3 + swell;
    ctx.strokeStyle = color;
    ctx.stroke();
    // The score on a tag above the region, or its size while dragging.
    const label = this.#drag?.moved
      ? `${Math.round(this.#region.region[2] * this.#natural.width)} × ${Math.round(this.#region.region[3] * this.#natural.height)}`
      : this.#region.label;
    if (!label) return;
    const size = 11;
    const tag = { width: [...label].length * size * 0.62 + 14, height: 20 };
    const above = r.y - tag.height - 6;
    const at = { x: clamp(r.x, 0, Math.max(0, cw - tag.width)), y: above >= 0 ? above : r.y + 6 };
    ctx.fillStyle = 'rgba(28, 28, 30, 0.82)';
    ctx.beginPath();
    if (typeof ctx.roundRect === 'function') ctx.roundRect(at.x, at.y, tag.width, tag.height, 6);
    else ctx.rect(at.x, at.y, tag.width, tag.height);
    ctx.fill();
    ctx.fillStyle = ON_STAGE;
    ctx.font = `${size}px ui-monospace, Menlo, Consolas, monospace`;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(label, at.x + tag.width / 2, at.y + tag.height / 2 + 0.5);
  }

  // Pointer, wheel and touch

  #point(event) {
    const box = this.element.getBoundingClientRect();
    return { x: event.clientX - box.left, y: event.clientY - box.top };
  }

  #listen() {
    const { signal } = this.#ac;
    const stage = this.element;
    const overlay = this.#overlay;
    this.#viewport.addEventListener('keydown', (event) => this.#panKey(event), { signal });
    stage.addEventListener('pointermove', (event) => {
      if (event.pointerType === 'mouse') this.wake();
      this.#on.pointer?.();
    }, { signal });
    stage.addEventListener('pointerdown', (event) => {
      const target = event.target;
      const onPicture = target === overlay || target === stage || this.#viewport.contains(target);
      if (event.pointerType !== 'mouse' && onPicture && !this.#region.visible && !this.#pannable()) {
        // A tap on the picture shows or hides the controls.
        if (this.#controls.classList.contains('resting')) this.wake();
        else this.#controls.classList.add('resting');
      }
    }, { signal });
    stage.addEventListener('dblclick', (event) => {
      const target = event.composedPath()[0];
      if (target !== overlay && target !== stage && !this.#viewport.contains(target)) return;
      event.preventDefault();
      this.#on.image?.();
    }, { signal });
    stage.addEventListener('wheel', (event) => {
      if (!this.#natural.width || !this.#view) return;
      if (event.ctrlKey) {
        // Pinch on a trackpad, or Ctrl with the wheel: zoom about the pointer.
        event.preventDefault();
        const factor = Math.exp(-event.deltaY * (event.deltaMode === 1 ? 0.05 : 0.002));
        const from = this.scale;
        const to = clamp(from * factor, ZOOM_MIN, ZOOM_MAX);
        if (to !== from) this.#zoomTo(to, this.#point(event), from);
        return;
      }
      if (!this.#pannable()) return;
      event.preventDefault();
      const unit = event.deltaMode === 1 ? 16 : 1;
      this.#pan.x -= event.deltaX * unit;
      this.#pan.y -= event.deltaY * unit;
      this.layout();
    }, { signal, passive: false });
    overlay.addEventListener('pointerdown', (event) => this.#down(event), { signal });
    overlay.addEventListener('pointermove', (event) => this.#move(event), { signal });
    overlay.addEventListener('pointerup', (event) => this.#up(event), { signal });
    overlay.addEventListener('pointercancel', (event) => this.#up(event, true), { signal });
  }

  #down(event) {
    if (event.button !== 0 && event.pointerType === 'mouse' && event.button !== 1) return;
    const point = this.#point(event);
    this.#pointers.set(event.pointerId, point);
    if (this.#pointers.size === 2) {
      // Two fingers: pinch to zoom, and drop any region drag.
      const [a, b] = [...this.#pointers.values()];
      this.#pinch = { distance: Math.hypot(a.x - b.x, a.y - b.y) || 1, scale: this.scale };
      if (this.#drag?.kind === 'region') this.#on.region?.(this.#drag.start, true);
      this.#drag = null;
      return;
    }
    if (this.#pointers.size > 2) return;
    const panning = event.button === 1 || !this.#region.visible;
    if (panning) {
      if (!this.#pannable()) return;
      this.#drag = { kind: 'pan', from: point, pan: { ...this.#pan }, moved: false };
      this.#overlay.classList.add('grabbing');
    } else {
      let grip = this.#grip(point);
      if (!grip) {
        if (!this.#inImage(point)) return;
        grip = { kind: 'new' };
      }
      this.#drag = { kind: 'region', grip, from: this.#normalized(point), screen: point, start: [...this.#region.region], moved: false };
    }
    event.preventDefault();
    this.#overlay.setPointerCapture?.(event.pointerId);
  }

  #move(event) {
    const point = this.#point(event);
    if (this.#pointers.has(event.pointerId)) this.#pointers.set(event.pointerId, point);
    if (this.#pinch && this.#pointers.size >= 2) {
      const [a, b] = [...this.#pointers.values()];
      const distance = Math.hypot(a.x - b.x, a.y - b.y) || 1;
      const to = clamp(this.#pinch.scale * (distance / this.#pinch.distance), ZOOM_MIN, ZOOM_MAX);
      const from = this.scale;
      if (Math.abs(to - from) > 1e-4) this.#zoomTo(to, { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 }, from);
      return;
    }
    const drag = this.#drag;
    if (!drag) {
      if (this.#region.visible && event.pointerType === 'mouse') this.#cursor(point);
      return;
    }
    if (drag.kind === 'pan') {
      this.#pan = { x: drag.pan.x + point.x - drag.from.x, y: drag.pan.y + point.y - drag.from.y };
      drag.moved = true;
      this.layout();
      return;
    }
    const to = this.#normalized(point);
    const distance = Math.abs(point.x - drag.screen.x) + Math.abs(point.y - drag.screen.y);
    drag.moved ||= distance > 3;
    if (!drag.moved) return;
    this.#on.region?.(this.#dragged(drag.grip, drag.from, to, drag.start).map(f32), false);
  }

  #up(event, cancelled = false) {
    this.#pointers.delete(event.pointerId);
    if (this.#pointers.size < 2) this.#pinch = null;
    const drag = this.#drag;
    this.#drag = null;
    this.#overlay.classList.remove('grabbing');
    if (!drag || drag.kind !== 'region') return;
    if (cancelled) {
      this.#on.region?.(drag.start, true);
      return;
    }
    if (drag.grip.kind === 'new' && !drag.moved) this.#on.region?.(this.#centered(drag.from).map(f32), true);
    // Moves score the region as it goes; this scores it where it was let go.
    else if (drag.moved) this.#on.region?.([...this.#region.region], true);
    this.#draw();
  }

  #cursor(point) {
    const grip = this.#grip(point);
    let cursor = this.#inImage(point) ? 'crosshair' : 'default';
    if (grip?.kind === 'move') cursor = 'grab';
    else if (grip?.kind === 'sides') {
      const [l, t, r, b] = grip.sides;
      const horizontal = l || r;
      const vertical = t || b;
      // Mirroring flips which diagonal a corner drags along.
      const down = (l && t) || (r && b);
      if (horizontal && vertical) cursor = (down !== this.#mirror) ? 'nwse-resize' : 'nesw-resize';
      else cursor = horizontal ? 'ew-resize' : 'ns-resize';
    }
    if (this.#overlay.style.cursor !== cursor) this.#overlay.style.cursor = cursor;
  }

  destroy() {
    this.#ac.abort();
    this.#resize.disconnect();
    clearTimeout(this.#idle);
    cancelAnimationFrame(this.#frame);
    cancelAnimationFrame(this.#lock.raf);
    if (this.#animation) cancelAnimationFrame(this.#animation.raf ?? 0);
  }
}
