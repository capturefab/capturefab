// The left pane, as the desktop's sidebar lays it out (src/gui/inspector.rs
// `capture_settings`, `forward_settings`, `storage_settings`): Capture | Video,
// where files go and how they are named, Capture & save, recording, storage
// limits and the files saved so far. Everything stays in this browser.
import { icon, strokeIcon } from './icons.mjs';
import { bytes, clock, middleEllipsis, withChord } from './format.mjs';
import { bounded, capitalize, firstLine, grouped, metaSettings, plural } from './features.mjs';
import {
  Library, NAMINGS, cameraLabel, formatExtension, limitBytes, naming, preview, roundedBytes, stamps, storageSupport
} from './library.mjs';
import { h } from './inspector.mjs';

const FORMATS = [['png', 'PNG image'], ['jpeg', 'JPEG image'], ['raw', 'Raw pixels'], ['pgm', 'PGM monochrome'], ['ppm', 'PPM color']];
const RECORD_TYPES = [
  { mime: 'video/mp4;codecs=avc1.42E01E', ext: 'mp4', label: 'MP4 · H.264' },
  { mime: 'video/mp4', ext: 'mp4', label: 'MP4' },
  { mime: 'video/webm;codecs=vp9', ext: 'webm', label: 'WebM · VP9' },
  { mime: 'video/webm', ext: 'webm', label: 'WebM' }
];
const PLACES = { opfs: 'This browser', folder: 'Folder on this computer…', memory: 'This tab' };
/** The largest recording, as the desktop's per-recording limit. */
const RECORDING_LIMIT = 2 ** 30;
/** Room kept free for the last part a recorder hands over after its limit. */
const RECORDING_MARGIN = 16 * 2 ** 20;
const SAVED_MS = 1600;
const LIST_ROWS = 60;
const PREFS = 'capturefab.try.panes';
const ABOUT_NAMES = '{camera}, {date}, {time} and {datetime} fill in when saving; {seq} becomes the next free number';

const errorText = (error) => (error instanceof Error ? error.message : String(error ?? 'unknown error'));

const readPrefs = () => {
  try {
    return JSON.parse(globalThis.localStorage?.getItem(PREFS) ?? '{}') ?? {};
  } catch {
    return {};
  }
};

const writePrefs = (prefs) => {
  try {
    globalThis.localStorage?.setItem(PREFS, JSON.stringify(prefs));
  } catch {
    // Private windows may refuse storage; the choices last for this visit.
  }
};

/** A path's file name and its folder. */
const splitPath = (path) => {
  const at = path.lastIndexOf('/');
  return at < 0 ? [path, ''] : [path.slice(at + 1), path.slice(0, at)];
};

// A small preview of a saved PGM/PPM or raw frame, for formats an <img> cannot show.
const NETPBM = /^P([56])\s+(?:#[^\n]*\s+)*(\d+)\s+(?:#[^\n]*\s+)*(\d+)\s+(?:#[^\n]*\s+)*(\d+)\s/;

const pixelsOf = async (blob, ext, frame) => {
  const data = new Uint8Array(await blob.arrayBuffer());
  if (ext === 'pgm' || ext === 'ppm') {
    const header = new TextDecoder('latin1').decode(data.subarray(0, 64));
    const match = NETPBM.exec(header);
    if (!match) return null;
    const [, kind, width, height, max] = match;
    return { width: Number(width), height: Number(height), channels: kind === '6' ? 3 : 1, wide: Number(max) > 255, big: true, data: data.subarray(match[0].length) };
  }
  const name = frame?.pixelFormat ?? '';
  const width = frame?.width ?? 0;
  const height = frame?.height ?? 0;
  if (!width || !height) return null;
  if (name === 'RGB8' || name === 'BGR8') return { width, height, channels: 3, bgr: name === 'BGR8', data };
  if (/^(Mono8|Bayer..8)$/.test(name)) return { width, height, channels: 1, data };
  if (name === 'Mono12') return { width, height, channels: 1, wide: true, shift: 4, data };
  if (name === 'Mono16') return { width, height, channels: 1, wide: true, data };
  return null;
};

const thumbCanvas = async (blob, ext, frame) => {
  const pixels = await pixelsOf(blob, ext, frame);
  if (!pixels) return null;
  const { width, height, channels, data } = pixels;
  const step = channels * (pixels.wide ? 2 : 1);
  if (data.length < width * height * step) return null;
  const scale = Math.min(1, 80 / height);
  const w = Math.max(1, Math.round(width * scale));
  const hgt = Math.max(1, Math.round(height * scale));
  const canvas = document.createElement('canvas');
  canvas.width = w;
  canvas.height = hgt;
  canvas.className = 'thumb';
  const image = new ImageData(w, hgt);
  const sample = (at) => {
    if (!pixels.wide) return data[at];
    const value = pixels.big ? (data[at] << 8) | data[at + 1] : data[at] | (data[at + 1] << 8);
    return pixels.shift ? Math.min(255, value >> pixels.shift) : value >> 8;
  };
  for (let y = 0; y < hgt; y += 1) {
    const sy = Math.min(height - 1, Math.floor(y / scale));
    for (let x = 0; x < w; x += 1) {
      const sx = Math.min(width - 1, Math.floor(x / scale));
      const at = (sy * width + sx) * step;
      const o = (y * w + x) * 4;
      if (channels === 3) {
        const a = sample(at);
        const b = sample(at + step / 3);
        const c = sample(at + (2 * step) / 3);
        [image.data[o], image.data[o + 1], image.data[o + 2]] = pixels.bgr ? [c, b, a] : [a, b, c];
      } else {
        image.data[o] = image.data[o + 1] = image.data[o + 2] = sample(at);
      }
      image.data[o + 3] = 255;
    }
  }
  canvas.getContext('2d').putImageData(image, 0, 0);
  return canvas;
};

let measurer = null;

/** Show `text` in `el` cut in the middle to fit its width, keeping both ends (oneline.rs `keep_ends`). */
const fitMiddle = (el, text) => {
  el.dataset.full = text;
  const width = el.clientWidth;
  if (!width) {
    el.textContent = middleEllipsis(text, 30);
    return;
  }
  measurer ??= document.createElement('canvas').getContext('2d');
  const style = getComputedStyle(el);
  measurer.font = `${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
  const fits = (candidate) => measurer.measureText(candidate).width <= width;
  if (fits(text)) {
    el.textContent = text;
    return;
  }
  let low = 1;
  let high = [...text].length;
  while (low < high) {
    const mid = Math.ceil((low + high) / 2);
    if (fits(middleEllipsis(text, mid))) low = mid;
    else high = mid - 1;
  }
  el.textContent = middleEllipsis(text, low);
};

class Sidebar {
  #app;
  #root;
  #pane;
  #tabs;
  #capturePanel;
  #videoPanel;
  #storage;
  #storageBody;
  #files;
  #filesBody;
  #live;
  #library = new Library();
  #prefs = readPrefs();
  #format;
  #naming;
  #recordType;
  #recordTypes = [];
  #tab;
  #storeNote = null;
  #storeBusy = false;
  #capturing = null;
  #captureError = null;
  #savedUntil = 0;
  #savedCount = 0;
  #last = null;
  #thumbUrl = null;
  #recording = null;
  #recordError = null;
  #listUrls = new Set();
  #listLimit = LIST_ROWS;
  #confirm = null;
  #zipping = false;
  #timer = null;
  #destination = '';
  #usage = { bytes: 0, files: 0, quota: null, usage: null };
  #listed = [];
  #reads = 0;
  #hadFrame = false;
  /** Where focus goes once the list re-renders after a delete: { key, fallback }. */
  #pendingFocus = null;

  constructor(container, app) {
    this.#app = app;
    this.#format = FORMATS.some(([id]) => id === this.#prefs.format) ? this.#prefs.format : 'png';
    this.#naming = naming(this.#prefs.naming).id;
    this.#tab = this.#prefs.tab === 'video' ? 'video' : 'capture';
    const supported = (mime) => {
      try {
        return typeof MediaRecorder === 'function' && MediaRecorder.isTypeSupported(mime);
      } catch {
        return false;
      }
    };
    this.#recordTypes = RECORD_TYPES.filter((type) => supported(type.mime));
    this.#recordType = this.#recordTypes.find((type) => type.mime === this.#prefs.record) ?? this.#recordTypes[0] ?? null;

    const tab = (id, word, hint) => h('button', {
      type: 'button', text: word, title: hint, 'aria-pressed': String(this.#tab === id), 'aria-controls': `sb-${id}`,
      onclick: () => this.#showTab(id)
    });
    this.#tabs = h('div', { class: 'segmented sb-tabs', role: 'group', 'aria-label': 'Panel' },
      tab('capture', 'Capture', 'Capture panel'),
      tab('video', 'Video', 'Video recording'));
    this.#capturePanel = h('section', { class: 'sb-panel', id: 'sb-capture', 'aria-label': 'Capture' });
    this.#videoPanel = h('section', { class: 'sb-panel', id: 'sb-video', 'aria-label': 'Video' });
    this.#storageBody = h('div', { class: 'sb-disclosure-body' });
    this.#storage = h('details', { class: 'disclosure sb-storage', ontoggle: () => this.#renderStorage() },
      h('summary', {}, icon('caret-right', 11), h('span', { class: 'title', text: 'Storage & retention' }), h('span', { class: 'summary' })),
      this.#storageBody);
    this.#filesBody = h('div', { class: 'sb-disclosure-body' });
    this.#files = h('details', { class: 'disclosure sb-files', ontoggle: () => this.#renderFiles() },
      h('summary', {}, icon('caret-right', 11), h('span', { class: 'title', text: 'Saved files' }), h('span', { class: 'summary' })),
      this.#filesBody);
    this.#live = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
    this.#pane = h('div', { class: 'pane sb-pane' },
      this.#capturePanel, this.#videoPanel,
      h('div', { class: 'sb-library' }, this.#storage, this.#files),
      this.#live);
    this.#root = h('div', { class: 'sb-root' }, h('header', { class: 'sb-head' }, this.#tabs), this.#pane);
    container.replaceChildren(this.#root);
    // Names cut to fit follow the pane's width (it is a sheet on small screens).
    new ResizeObserver(() => {
      for (const el of this.#pane.querySelectorAll('[data-full]')) fitMiddle(el, el.dataset.full);
    }).observe(this.#pane);

    this.#library.addEventListener('change', () => this.#libraryChanged());
    app.on('source', () => this.#sourceChanged());
    // The workbench loads a new engine each time it opens: tick again from then on.
    app.on('engine', (engine) => {
      if (engine) this.resume();
      this.#render();
    });
    app.on('frame', () => {
      if (this.#capturing) return;
      const had = this.#hadFrame;
      this.#hadFrame = Boolean(this.#state.frame);
      this.#renderCaptureButton();
      if (had !== this.#hadFrame) this.#renderStatus();
    });
    app.on('phase', () => {
      const phase = app.state?.phase;
      // A stopped stream sends no more frames: finish and save the recording.
      if (this.#recording && ['stopping', 'stopped', 'idle', 'error'].includes(phase)) this.stopRecording();
      this.#render();
    });
    // Nothing runs while the dialog is closed.
    this.#dialog()?.addEventListener('close', () => this.pause());
    this.#showTab(this.#tab, false);
    this.#start();
  }

  /** The dialog the workbench sits in, across its shadow root; null when there is none. */
  #dialog() {
    let node = this.#root;
    while (node) {
      const dialog = node.closest?.('dialog');
      if (dialog) return dialog;
      node = node.getRootNode?.()?.host ?? null;
    }
    return null;
  }

  async #start() {
    if (storageSupport().browser) {
      try {
        await this.#library.useBrowser();
      } catch (error) {
        this.#library.useTab();
        this.#storeNote = 'This browser does not let this page keep files, so they stay in this tab until you close it.';
        this.#app.log('warn', `Saving in this tab: ${errorText(error)}`);
      }
    } else {
      this.#storeNote = 'This browser cannot keep files for this page, so they stay in this tab until you close it.';
    }
    this.#libraryChanged();
    this.#render();
    if (this.#dialog()?.open !== false) this.resume();
  }

  // Public ---------------------------------------------------------------

  /** Stop the once-a-second refresh (the dialog closed). */
  pause() {
    clearInterval(this.#timer);
    this.#timer = null;
  }

  /** Refresh where files go once a second, even while the pane is collapsed: the toolbar shows it. */
  resume() {
    if (this.#timer !== null) return;
    this.#timer = setInterval(() => {
      if (!this.#root.isConnected || this.#dialog()?.open === false) this.pause();
      else this.#tick();
    }, 1000);
    this.#tick();
  }

  /** Capture the current picture and save it; resolves to the `capture --json` result. */
  async capture() {
    if (this.#capturing) return this.#capturing;
    const run = this.#captureOnce();
    this.#capturing = run;
    this.#captureError = null;
    this.#renderCapture();
    try {
      const result = await run;
      this.#savedUntil = performance.now() + SAVED_MS;
      setTimeout(() => this.#renderCaptureButton(), SAVED_MS + 20);
      return result;
    } catch (error) {
      this.#captureError = errorText(error);
      this.#app.log('error', this.#captureError);
      this.#say(capitalize(firstLine(this.#captureError)));
      throw error;
    } finally {
      this.#capturing = null;
      this.#renderCapture();
    }
  }

  /** Start recording, or stop and save the recording. */
  async toggleRecording() {
    if (this.#recording) {
      await this.stopRecording();
      return;
    }
    try {
      await this.#startRecording();
    } catch (error) {
      this.#recordError = errorText(error);
      this.#app.log('error', this.#recordError);
      this.#say(capitalize(firstLine(this.#recordError)));
      this.#renderVideo();
      throw error;
    }
  }

  /** Stop the recording and save it; resolves to its record, or null. */
  stopRecording() {
    const recording = this.#recording;
    if (!recording) return Promise.resolve(null);
    if (!recording.stopping) {
      recording.stopping = true;
      try {
        if (recording.recorder.state !== 'inactive') recording.recorder.stop();
      } catch {
        // Already stopped: the stop event saves it.
      }
      this.#renderVideo();
    }
    return recording.saved;
  }

  /** Where the next capture goes: "Saves to This browser · Capturefab/2026-10-25". */
  destinationText() {
    const library = this.#library;
    const path = preview(this.#template(), this.#label(), new Date());
    const [, folder] = splitPath(path);
    const first = folder.split('/')[0];
    if (library.kind === 'folder') return `Saves to ${library.root}${first ? `/${first}` : ''}`;
    if (library.kind === 'opfs') return `Saves to This browser · ${library.root}${first ? `/${first}` : ''}`;
    return `Saves to This tab${first ? ` · ${first}` : ''}`;
  }

  // State ----------------------------------------------------------------

  get #state() {
    return this.#app.state ?? {};
  }

  #label() {
    const source = this.#state.source;
    return source ? cameraLabel(source.model ?? '', source.serial ?? '') : 'camera';
  }

  #template(ext = formatExtension(this.#format)) {
    return naming(this.#naming).template(ext);
  }

  #savePrefs() {
    this.#prefs = { ...this.#prefs, format: this.#format, naming: this.#naming, tab: this.#tab, record: this.#recordType?.mime };
    writePrefs(this.#prefs);
  }

  #say(text) {
    this.#live.textContent = '';
    setTimeout(() => { this.#live.textContent = text; }, 30);
  }

  #tick() {
    if (!this.#root.isConnected) return;
    const text = this.destinationText();
    if (text !== this.#destination) {
      this.#destination = text;
      this.#app.emit('destination', text);
      this.#renderNext();
    } else if (this.#root.checkVisibility?.() !== false) {
      this.#renderNext();
    }
    if (this.#recording) this.#renderRecordingStats();
  }

  #showTab(id, save = true) {
    this.#tab = id;
    for (const button of this.#tabs.children) button.setAttribute('aria-pressed', String(button.getAttribute('aria-controls') === `sb-${id}`));
    this.#capturePanel.hidden = id !== 'capture';
    this.#videoPanel.hidden = id !== 'video';
    this.#pane.scrollTop = 0;
    if (save) this.#savePrefs();
    this.#render();
  }

  #sourceChanged() {
    if (this.#recording && this.#recording.source.id !== this.#state.source?.id) this.stopRecording();
    this.#captureError = null;
    this.#recordError = null;
    this.#render();
    this.#tick();
  }

  async #libraryChanged() {
    const read = (this.#reads += 1);
    try {
      const [listed, estimate] = await Promise.all([this.#library.list(), this.#library.estimate()]);
      // A later change may have finished first; keep its newer list.
      if (read !== this.#reads) return;
      this.#listed = listed;
      this.#usage = estimate;
    } catch (error) {
      this.#app.log('warn', `Could not read saved files: ${errorText(error)}`);
    }
    this.#renderStorage();
    this.#renderFiles();
    this.#focusPending();
    this.#tick();
  }

  /** After a delete re-rendered the list, put focus back where the deleted control was. */
  #focusPending() {
    const pending = this.#pendingFocus;
    if (!pending) return;
    this.#pendingFocus = null;
    // Only when focus was lost with the removed row, not when the visitor moved on.
    const active = this.#root.getRootNode().activeElement;
    if (active?.isConnected && active !== document.body) return;
    const target = (pending.key && this.#filesBody.querySelector(`[data-focus-key="${CSS.escape(pending.key)}"]`)) || pending.fallback;
    target?.focus({ focusVisible: true });
  }

  // Capture --------------------------------------------------------------

  async #captureOnce() {
    const app = this.#app;
    const source = this.#state.source;
    if (!source) throw new Error('Choose a camera to capture.');
    const engine = app.engine;
    if (!engine) throw new Error('Capturefab is still loading; try again in a moment');
    const format = this.#format;
    const label = this.#label();
    let features = [];
    try {
      features = (await app.features()) ?? [];
    } catch {
      // Without readable settings the capture still saves, as the desktop's does.
    }
    const shown = this.#state.frame;
    const frame = await app.grabFrame();
    if (!frame && (source.kind === 'webcam' || !shown)) throw new Error('No frame to capture yet. Start the stream, then capture.');
    const wall = Date.now();
    // Provenance, as the desktop writes it (engine.rs `still_meta`): never a serial or device ID.
    // The engine fills frame_id, timestamp_ns, width, height and pixel_format from the frame it encodes.
    const meta = {
      capturefab: 'browser',
      wall_ms: wall,
      camera: { vendor: bounded(source.vendor ?? '', 128), model: bounded(source.model ?? '', 128) }
    };
    const settings = metaSettings(features);
    if (Object.keys(settings).length > 0) meta.settings = settings;
    const result = await engine.capture(format, meta, frame);
    app.flash?.();
    const record = await this.#library.save(result.blob, { ext: result.ext, label, date: new Date(wall), naming: this.#naming });
    this.#savedCount += 1;
    const frameJson = {
      id: result.frameId,
      width: result.width,
      height: result.height,
      pixel_format: result.pixelFormatCode,
      timestamp_ns: result.timestampNs,
      pixel_format_name: result.pixelFormat,
      bytes: result.bytes,
      wall_ms: wall,
      ...(Object.keys(settings).length > 0 ? { settings } : {}),
      file: record.location,
      file_bytes: record.size
    };
    const captured = { files: [record.location], frames: [frameJson], count: 1 };
    await this.#showSaved(record, result);
    app.log('info', `Saved ${record.name}`, captured);
    this.#say(`Saved ${record.name}`);
    app.emit('saved', record);
    return captured;
  }

  /** Forget the last saved file and release its thumbnail. */
  #forgetLast() {
    this.#last = null;
    if (this.#thumbUrl) URL.revokeObjectURL(this.#thumbUrl);
    this.#thumbUrl = null;
  }

  async #showSaved(record, result) {
    let thumb = null;
    const ext = record.name.slice(record.name.lastIndexOf('.') + 1);
    this.#forgetLast();
    try {
      if (ext === 'png' || ext === 'jpg') {
        this.#thumbUrl = URL.createObjectURL(result.blob);
        thumb = h('img', { class: 'thumb', src: this.#thumbUrl, alt: '', decoding: 'async' });
      } else {
        thumb = await thumbCanvas(result.blob, ext, result);
      }
    } catch {
      thumb = null;
    }
    this.#last = { record, thumb, shape: `${result.width}×${result.height} ${ext.toUpperCase()}` };
  }

  // Recording ------------------------------------------------------------

  async #startRecording() {
    const app = this.#app;
    const source = this.#state.source;
    if (!source) throw new Error('Choose a camera to record.');
    const type = this.#recordType;
    if (!type) throw new Error('This browser cannot record video.');
    const tracks = (app.mediaStream?.()?.getVideoTracks?.() ?? []).filter((track) => track.readyState === 'live');
    if (tracks.length === 0 || !this.#state.running) throw new Error('Start the stream to record.');
    const { bytes: used } = await this.#library.check(RECORDING_MARGIN);
    const room = Math.min(RECORDING_LIMIT, this.#library.budgetBytes - used - RECORDING_MARGIN);
    this.#recordError = null;
    let recorder;
    try {
      recorder = new MediaRecorder(new MediaStream(tracks), { mimeType: type.mime });
    } catch (error) {
      throw new Error(`This browser cannot record ${type.label}: ${errorText(error)}`);
    }
    const recording = {
      recorder, type, source, room,
      label: this.#label(),
      naming: this.#naming,
      chunks: [],
      bytes: 0,
      startedAt: performance.now(),
      wall: new Date(),
      stopping: false,
      reason: null,
      error: null
    };
    const stopped = new Promise((resolve) => recorder.addEventListener('stop', resolve, { once: true }));
    recording.saved = stopped.then(() => this.#finishRecording(recording));
    recorder.addEventListener('dataavailable', (event) => {
      if (!event.data || event.data.size === 0) return;
      recording.chunks.push(event.data);
      recording.bytes += event.data.size;
      if (recording.bytes >= recording.room && !recording.reason) {
        recording.reason = recording.room >= RECORDING_LIMIT ? 'limit' : 'full';
        this.stopRecording();
      }
    });
    recorder.addEventListener('error', (event) => {
      recording.error = event.error?.message || 'the recorder stopped';
    });
    recorder.start(1000);
    this.#recording = recording;
    const mime = recorder.mimeType || type.mime;
    app.emit('recording', { active: true, startedAt: recording.startedAt, mime });
    app.log('info', `Recording ${source.model}`, { format: mime, limit_bytes: room });
    this.#say('Recording');
    this.#renderVideo();
  }

  async #finishRecording(recording) {
    if (this.#recording === recording) this.#recording = null;
    const mime = recording.recorder.mimeType || recording.type.mime;
    const blob = new Blob(recording.chunks, { type: mime });
    recording.chunks = [];
    const duration = performance.now() - recording.startedAt;
    this.#app.emit('recording', { active: false });
    this.#renderVideo();
    if (recording.error) this.#app.log('warn', `The recording stopped early: ${recording.error}`);
    if (blob.size === 0) {
      this.#recordError = 'The recording had no video to save.';
      this.#app.log('warn', this.#recordError);
      this.#renderVideo();
      return null;
    }
    try {
      const record = await this.#library.save(blob, { ext: recording.type.ext, label: recording.label, date: recording.wall, naming: recording.naming });
      this.#app.log('info', `Saved ${record.name}`, { file: record.location, file_bytes: record.size, duration_ms: Math.round(duration), format: mime });
      if (recording.reason === 'limit') this.#app.notice?.(`Recording stopped at the ${limitBytes(RECORDING_LIMIT)} file limit and was saved`, 'warn');
      else if (recording.reason === 'full') this.#app.notice?.('Recording stopped because storage is full, and was saved', 'warn');
      this.#say(`Saved ${record.name}`);
      this.#app.emit('saved', record);
      return record;
    } catch (error) {
      // Never lose a recording: hand it to the browser's downloads instead.
      const name = `${recording.label}-${stamps(recording.wall).datetime}.${recording.type.ext}`;
      this.#offer(blob, name);
      this.#recordError = `${errorText(error)}. The recording was downloaded as ${name} instead.`;
      this.#app.log('error', this.#recordError, { file: name, file_bytes: blob.size });
      this.#say(capitalize(firstLine(this.#recordError)));
      return null;
    } finally {
      this.#renderVideo();
    }
  }

  // Files ----------------------------------------------------------------

  #offer(blob, name) {
    const url = URL.createObjectURL(blob);
    const link = h('a', { href: url, download: name, hidden: true });
    this.#root.append(link);
    link.click();
    link.remove();
    // Some browsers read the file after click returns.
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }

  async #download(record) {
    try {
      this.#offer(await this.#library.file(record.path), record.name);
    } catch (error) {
      this.#app.log('error', `Could not download ${record.name}: ${errorText(error)}`);
    }
  }

  async #downloadAll() {
    if (this.#zipping) return;
    this.#zipping = true;
    this.#renderStorage();
    try {
      const zip = await this.#library.zipAll(this.#app.engine);
      const name = `Capturefab-${stamps(new Date()).datetime}.zip`;
      this.#offer(zip, name);
      this.#app.log('info', `Downloaded ${plural(this.#listed.length, 'file', 'files')} as ${name}`, { file: name, file_bytes: zip.size });
    } catch (error) {
      this.#app.log('error', `Could not download all files: ${errorText(error)}`);
      this.#say(errorText(error));
    } finally {
      this.#zipping = false;
      this.#renderStorage();
    }
  }

  async #delete(record) {
    this.#confirm = null;
    // The row goes away with its buttons: focus moves to the next row's, else the previous row's.
    const at = this.#listed.findIndex((other) => other.path === record.path);
    const neighbour = this.#listed[at + 1] ?? this.#listed[at - 1] ?? null;
    this.#pendingFocus = {
      key: neighbour && neighbour.path !== record.path ? `delete:${neighbour.path}` : null,
      fallback: this.#files.querySelector('summary')
    };
    try {
      await this.#library.remove(record.path);
      if (this.#last?.record.path === record.path) this.#forgetLast();
      this.#app.log('info', `Deleted ${record.name}`, { file: record.location });
      this.#say(`Deleted ${record.name}`);
      this.#renderCapture();
    } catch (error) {
      this.#pendingFocus = null;
      this.#app.log('error', `Could not delete ${record.name}: ${errorText(error)}`);
    }
  }

  async #deleteAll() {
    this.#confirm = null;
    // Download all and Delete all… are disabled once nothing is left.
    this.#pendingFocus = { key: null, fallback: this.#storage.querySelector('summary') };
    try {
      const count = await this.#library.removeAll();
      this.#forgetLast();
      this.#app.log('info', `Deleted ${plural(count, 'file', 'files')}`, { place: this.#library.place, files: count });
      this.#say(`Deleted ${plural(count, 'file', 'files')}`);
      this.#renderCapture();
    } catch (error) {
      this.#pendingFocus = null;
      this.#app.log('error', `Could not delete all files: ${errorText(error)}`);
    }
  }

  async #useStore(kind) {
    if (this.#storeBusy) return;
    this.#storeBusy = true;
    const library = this.#library;
    try {
      if (kind === 'opfs') await library.useBrowser();
      else if (kind === 'folder') {
        const handle = await globalThis.showDirectoryPicker({ id: 'capturefab', mode: 'readwrite', startIn: 'pictures' });
        await library.useFolder(handle);
      } else library.useTab();
      this.#storeNote = null;
      this.#forgetLast();
      this.#app.log('info', `Saving to ${library.place}`);
    } catch (error) {
      if (error?.name !== 'AbortError') {
        this.#storeNote = `Could not use ${PLACES[kind].replace('…', '')}: ${errorText(error)}`;
        this.#app.log('warn', this.#storeNote);
      }
    } finally {
      this.#storeBusy = false;
      this.#render();
      this.#tick();
    }
  }

  // Rendering ------------------------------------------------------------

  #keepFocus(within, rebuild) {
    const active = this.#root.getRootNode().activeElement;
    const key = active && within.contains(active) ? active.dataset?.focusKey : undefined;
    rebuild();
    if (key) within.querySelector(`[data-focus-key="${CSS.escape(key)}"]`)?.focus({ preventScroll: true });
  }

  #render() {
    this.#renderCapture();
    this.#renderVideo();
    this.#renderStorage();
    this.#renderFiles();
  }

  #formRow(text, control, id) {
    return h('div', { class: 'sb-row' }, h('label', { class: 'sb-label', for: id, text }), control);
  }

  #select(id, options, value, onchange, key) {
    const select = h('select', { class: 'field', id, 'data-focus-key': key, onchange: () => onchange(select.value) },
      options.map(([v, text, disabled]) => h('option', { value: v, text, disabled })));
    select.value = value;
    return select;
  }

  #renderCapture() {
    if (this.#capturePanel.hidden) return;
    const library = this.#library;
    const support = storageSupport();
    const places = [['opfs', PLACES.opfs, !support.browser], ['folder', PLACES.folder, !support.folder], ['memory', PLACES.memory]]
      .filter(([kind, , disabled]) => !disabled || kind === library.kind);
    const parts = [h('h3', { class: 'section-title sb-first', text: 'Save frames' })];
    parts.push(h('div', { class: 'sb-stack' },
      h('label', { class: 'sb-label', for: 'sb-place', text: 'Save to' }),
      this.#select('sb-place', places, library.kind, (kind) => this.#useStore(kind), 'place')));
    if (library.kind === 'folder') {
      parts.push(h('div', { class: 'sb-row' }, h('span', { class: 'sb-label', text: 'Folder' }),
        h('span', { class: 'sb-value' }, h('span', { class: 'sb-folder', text: library.root, title: library.root }),
          h('button', { class: 'link', type: 'button', text: 'Choose…', 'data-focus-key': 'choose', onclick: () => this.#useStore('folder') }))));
    } else if (library.kind === 'opfs') {
      parts.push(h('div', { class: 'sb-row' }, h('span', { class: 'sb-label', text: 'Folder' }),
        h('span', { class: 'sb-value' }, h('span', { class: 'sb-folder', text: library.root, title: 'A private folder this site keeps in this browser' }))));
    }
    if (this.#storeNote) parts.push(h('p', { class: 'sb-note', text: this.#storeNote }));
    parts.push(this.#formRow('File names', this.#select('sb-naming', NAMINGS.map((n) => [n.id, n.label]), this.#naming, (id) => {
      this.#naming = id;
      this.#savePrefs();
      this.#tick();
      this.#renderCapture();
    }, 'naming'), 'sb-naming'));
    parts.push(this.#formRow('Format', this.#select('sb-format', FORMATS, this.#format, (id) => {
      this.#format = id;
      this.#captureError = null;
      this.#savePrefs();
      this.#renderCapture();
    }, 'format'), 'sb-format'));
    parts.push(h('div', { class: 'sb-action' }));
    parts.push(h('div', { class: 'sb-status' }));
    parts.push(h('div', { class: 'sb-next' }));
    this.#keepFocus(this.#capturePanel, () => this.#capturePanel.replaceChildren(...parts));
    this.#renderCaptureButton();
    this.#renderStatus();
    this.#renderNext();
  }

  #renderCaptureButton() {
    const slot = this.#capturePanel.querySelector('.sb-action');
    if (!slot) return;
    const source = this.#state.source;
    const ready = Boolean(source && this.#app.engine && this.#state.frame);
    const busy = Boolean(this.#capturing);
    const saved = !busy && performance.now() < this.#savedUntil;
    const state = `${ready}${busy}${saved}${this.#state.isApple}`;
    if (slot.dataset.state === state) return;
    slot.dataset.state = state;
    const glyphEl = busy ? h('span', { class: 'spinner', style: '--size: 14px' }) : icon(saved ? 'check-circle' : 'camera', 14);
    const word = busy ? 'Saving…' : saved ? 'Saved' : 'Capture & save';
    this.#keepFocus(slot, () => slot.replaceChildren(h('button', {
      class: `btn primary sb-wide${busy ? ' busy' : ''}`, type: 'button', disabled: !ready && !busy, 'aria-disabled': busy ? 'true' : null,
      title: withChord('Capture and save', 'Mod+S', this.#state.isApple), 'data-focus-key': 'capture',
      onclick: () => {
        if (!busy) this.capture().catch(() => {});
      }
    }, glyphEl, word)));
  }

  #renderStatus() {
    const slot = this.#capturePanel.querySelector('.sb-status');
    if (!slot) return;
    const source = this.#state.source;
    let content = null;
    if (!source) content = h('p', { class: 'sb-reason', text: 'Choose a camera to capture.' });
    else if (!this.#app.engine) content = h('p', { class: 'sb-reason', text: 'Capturefab is still loading…' });
    else if (!this.#state.frame) {
      content = h('p', { class: 'sb-reason', text: this.#state.running ? 'Waiting for the first frame…' : 'Start the stream to capture.' });
    }
    else if (this.#captureError) {
      content = h('p', { class: 'sb-error', title: this.#captureError, role: 'note' }, icon('warning-circle', 11), h('span', { text: capitalize(firstLine(this.#captureError)) }));
    } else if (this.#last) {
      const { record, thumb, shape } = this.#last;
      content = h('div', { class: 'sb-saved' },
        thumb,
        h('div', { class: 'sb-saved-text' },
          h('div', { class: 'sb-saved-line' },
            icon('check-circle', 12),
            h('span', { class: 'sb-saved-name', text: record.name, title: record.location, 'data-full': record.name }),
            h('button', {
              class: 'btn icon sb-mini', type: 'button', 'aria-label': `Download ${record.name}`, title: 'Download', 'data-focus-key': 'download-last',
              onclick: () => this.#download(record)
            }, strokeIcon('download', 12))),
          h('p', { class: 'sb-tally', text: `${grouped(Math.max(1, this.#savedCount))} saved this session · ${shape}` })));
    }
    this.#keepFocus(slot, () => slot.replaceChildren(...(content ? [content] : [])));
    for (const el of slot.querySelectorAll('[data-full]')) fitMiddle(el, el.dataset.full);
  }

  #renderNext() {
    const slot = this.#capturePanel.querySelector('.sb-next');
    if (slot) this.#fillPreview(slot, 'Next file', this.#template());
    const saves = this.#videoPanel.querySelector('.sb-next');
    if (saves && this.#recordType) this.#fillPreview(saves, 'Saves as', this.#template(this.#recordType.ext));
  }

  #fillPreview(slot, word, template) {
    const path = preview(template, this.#label(), new Date());
    const [leaf, folder] = splitPath(path);
    const root = this.#library.root;
    const key = `${word}${path}${root}`;
    if (slot.dataset.key === key) return;
    slot.dataset.key = key;
    const where = `${root}${folder ? `/${folder}` : ''}`;
    slot.title = `In ${where}\n${root}/${path}\n${ABOUT_NAMES}`;
    const name = h('span', { class: 'sb-next-name' });
    slot.replaceChildren(h('div', { class: 'sb-next-line' }, icon('folder', 12), h('span', { class: 'sb-next-word', text: word }), name));
    if (folder) slot.append(h('div', { class: 'sb-next-folder', text: `in ${folder}/` }));
    fitMiddle(name, leaf);
  }

  #renderVideo() {
    if (this.#videoPanel.hidden) return;
    const recording = this.#recording;
    const parts = [h('h3', { class: 'section-title sb-first', text: 'Record or forward video' })];
    parts.push(h('div', { class: 'segmented sb-output', role: 'group', 'aria-label': 'Video output' },
      h('button', { type: 'button', 'aria-pressed': 'true', text: 'Record to file', title: 'Save video to a file in this browser' }),
      h('button', { type: 'button', 'aria-pressed': 'false', disabled: true, text: 'Forward to URL', title: 'Forwarding to a server needs the Capturefab app' })));
    if (recording) {
      const place = this.destinationText().replace(/^Saves to /, '');
      parts.push(h('div', { class: 'well sb-recording' },
        h('div', { class: 'sb-recording-head' }, h('span', { class: 'dot danger' }), h('span', { class: 'sb-recording-title', text: 'Recording' })),
        h('p', { class: 'mono sb-recording-place', text: place, title: place, 'data-full': place }),
        h('p', { class: 'sb-recording-stats', 'aria-live': 'off' }),
        h('button', {
          class: 'btn sb-wide', type: 'button', disabled: recording.stopping, 'data-focus-key': 'record',
          onclick: () => this.stopRecording()
        }, recording.stopping ? h('span', { class: 'spinner', style: '--size: 12px' }) : icon('stop-fill', 12), recording.stopping ? 'Stopping…' : 'Stop recording')));
      parts.push(h('p', { class: 'sb-reason', text: 'Stop recording to change its settings.' }));
    } else {
      const source = this.#state.source;
      const ready = Boolean(source && this.#recordType && this.#state.running);
      parts.push(h('div', { class: 'sb-action' }, h('button', {
        class: 'btn primary sb-wide', type: 'button', disabled: !ready, 'data-focus-key': 'record',
        onclick: () => this.toggleRecording().catch(() => {})
      }, h('span', { class: 'sb-rec-dot', 'aria-hidden': 'true' }), 'Start recording')));
      let reason = null;
      if (!source) reason = h('p', { class: 'sb-reason', text: 'Choose a camera to record.' });
      else if (!this.#recordType) reason = h('p', { class: 'sb-reason', text: 'This browser cannot record video.' });
      else if (this.#recordError) {
        reason = h('p', { class: 'sb-error', title: this.#recordError, role: 'note' }, icon('warning-circle', 11), h('span', { text: capitalize(firstLine(this.#recordError)) }));
      } else if (!this.#state.running) reason = h('p', { class: 'sb-reason', text: 'Start the stream to record.' });
      if (reason) parts.push(reason);
      if (this.#recordType) parts.push(h('div', { class: 'sb-next' }));
      if (this.#recordTypes.length > 0) {
        const settings = h('details', { class: 'disclosure sb-video-settings', open: this.#prefs.videoOpen === true },
          h('summary', {}, icon('caret-right', 11), h('span', { class: 'title', text: 'Video settings' }), h('span', { class: 'summary', text: this.#recordType.label })),
          h('div', { class: 'sb-disclosure-body' },
            this.#formRow('Format', this.#select('sb-record-type', this.#recordTypes.map((type) => [type.mime, type.label]), this.#recordType.mime, (mime) => {
              this.#recordType = this.#recordTypes.find((type) => type.mime === mime) ?? this.#recordType;
              this.#savePrefs();
              this.#renderVideo();
            }, 'record-type'), 'sb-record-type'),
            h('div', { class: 'sb-row' }, h('span', { class: 'sb-label', text: 'Maximum file' }), h('span', { class: 'sb-value', text: limitBytes(RECORDING_LIMIT) })),
            h('p', { class: 'sb-note', text: 'Recording stops when it reaches this size, or when storage is full. Recordings save where captures do, without sound.' })));
        settings.addEventListener('toggle', () => {
          this.#prefs.videoOpen = settings.open;
          writePrefs(this.#prefs);
        });
        parts.push(settings);
      }
    }
    this.#keepFocus(this.#videoPanel, () => this.#videoPanel.replaceChildren(...parts));
    for (const el of this.#videoPanel.querySelectorAll('[data-full]')) fitMiddle(el, el.dataset.full);
    this.#renderNext();
    this.#renderRecordingStats();
  }

  #renderRecordingStats() {
    const stats = this.#videoPanel.querySelector('.sb-recording-stats');
    const recording = this.#recording;
    if (!stats || !recording) return;
    const time = clock(performance.now() - recording.startedAt);
    stats.textContent = recording.bytes > 0 ? `${time} · ${bytes(recording.bytes)} written` : time;
  }

  #renderStorage() {
    const usage = this.#usage;
    const limits = `${limitBytes(this.#library.budgetBytes)} · ${grouped(this.#library.budgetFiles)} files`;
    const fraction = Math.min(1, Math.max(usage.bytes / this.#library.budgetBytes, usage.files / this.#library.budgetFiles));
    const tone = fraction >= 0.95 ? 'danger' : fraction >= 0.8 ? 'warn' : 'normal';
    const summary = this.#storage.querySelector('summary .summary');
    summary.textContent = limits;
    const meterText = `${roundedBytes(usage.bytes)} of ${limitBytes(this.#library.budgetBytes)} · ${grouped(usage.files)} of ${grouped(this.#library.budgetFiles)} files`;
    const meter = () => h('div', { class: `sb-meter ${tone}` },
      h('p', { class: 'sb-meter-text', text: meterText }),
      h('div', {
        class: 'sb-bar', role: 'progressbar', 'aria-label': `Storage ${Math.round(fraction * 100)}% full: ${meterText}`,
        'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': String(Math.round(fraction * 100))
      }, h('div', { class: 'sb-fill', style: `width: ${(fraction * 100).toFixed(1)}%` })));
    // Folded, the meter shows only once storage is nearly full (inspector.rs storage_settings).
    const folded = this.#storage.open ? null : tone === 'normal' ? null : meter();
    const after = this.#storage.nextElementSibling;
    this.#storage.parentElement?.querySelector(':scope > .sb-meter-folded')?.remove();
    if (folded) {
      folded.classList.add('sb-meter-folded');
      this.#storage.parentElement?.insertBefore(folded, after);
    }
    if (!this.#storage.open) {
      this.#storageBody.replaceChildren();
      return;
    }
    const library = this.#library;
    const parts = [meter()];
    if (usage.quota) {
      const free = Math.max(0, usage.quota - (usage.usage ?? 0));
      parts.push(h('p', { class: 'sb-note', text: `${roundedBytes(free)} free for this site in this browser` }));
    }
    parts.push(h('p', { class: 'sb-note', text: 'At capacity, saving stops and existing files are preserved.' }));
    if (library.kind === 'opfs' && usage.persisted !== true) {
      parts.push(h('p', { class: 'sb-note', text: 'The browser may remove these files when it needs space. Download the ones you want to keep.' }));
    } else if (library.kind === 'memory') {
      parts.push(h('p', { class: 'sb-note', text: 'Files in this tab go when you close it. Download the ones you want to keep.' }));
    }
    if (this.#confirm === 'all') {
      parts.push(h('div', { class: 'sb-confirm', role: 'group', 'aria-label': 'Delete all saved files' },
        h('p', { text: `Delete all ${plural(this.#listed.length, 'saved file', 'saved files')} in ${library.place}? This cannot be undone.` }),
        h('div', { class: 'sb-buttons' },
          h('button', { class: 'btn danger', type: 'button', text: 'Delete all', 'data-focus-key': 'delete-all-yes', onclick: () => this.#deleteAll() }),
          h('button', { class: 'btn', type: 'button', text: 'Cancel', 'data-focus-key': 'delete-all-no', onclick: () => {
            this.#confirm = null;
            this.#renderStorage();
            this.#storageBody.querySelector('[data-focus-key="delete-all"]')?.focus();
          } }))));
    } else {
      const none = this.#listed.length === 0;
      parts.push(h('div', { class: 'sb-buttons' },
        h('button', {
          class: 'btn', type: 'button', disabled: none || this.#zipping || !this.#app.engine, 'data-focus-key': 'zip',
          onclick: () => this.#downloadAll()
        }, this.#zipping ? h('span', { class: 'spinner', style: '--size: 12px' }) : strokeIcon('download', 12), this.#zipping ? 'Preparing…' : 'Download all (.zip)'),
        h('button', {
          class: 'btn subtle sb-danger', type: 'button', text: 'Delete all…', disabled: none, 'data-focus-key': 'delete-all',
          onclick: () => {
            this.#confirm = 'all';
            this.#renderStorage();
            this.#storageBody.querySelector('[data-focus-key="delete-all-no"]')?.focus();
          }
        })));
    }
    this.#keepFocus(this.#storageBody, () => this.#storageBody.replaceChildren(...parts));
  }

  #renderFiles() {
    const listed = this.#listed;
    this.#files.querySelector('summary .summary').textContent = listed.length === 0 ? 'None' : plural(listed.length, 'file', 'files');
    for (const url of this.#listUrls) URL.revokeObjectURL(url);
    this.#listUrls.clear();
    if (!this.#files.open) {
      this.#filesBody.replaceChildren();
      this.#listLimit = LIST_ROWS;
      return;
    }
    if (listed.length === 0) {
      this.#filesBody.replaceChildren(h('p', { class: 'sb-note', text: `Captures and recordings you save in ${this.#library.place} appear here.` }));
      return;
    }
    const items = listed.slice(0, this.#listLimit).map((record) => {
      const ext = record.name.slice(record.name.lastIndexOf('.') + 1).toLowerCase();
      let thumb;
      if (ext === 'png' || ext === 'jpg') {
        thumb = h('img', { class: 'thumb', alt: '', decoding: 'async', loading: 'lazy' });
        this.#library.file(record.path).then((file) => {
          if (!thumb.isConnected) return;
          const url = URL.createObjectURL(file);
          this.#listUrls.add(url);
          thumb.src = url;
        }).catch(() => {});
      } else {
        thumb = h('span', { class: 'thumb sb-thumb-kind', 'aria-hidden': 'true' },
          ext === 'mp4' || ext === 'webm' ? icon('video-camera', 14) : strokeIcon('file', 14),
          h('span', { text: ext.toUpperCase() }));
      }
      const when = new Date(record.created);
      const time = `${String(when.getHours()).padStart(2, '0')}:${String(when.getMinutes()).padStart(2, '0')}:${String(when.getSeconds()).padStart(2, '0')}`;
      const confirming = this.#confirm === record.path;
      const foot = confirming
        ? h('div', { class: 'sb-file-foot', role: 'group', 'aria-label': `Delete ${record.name}?` },
          h('span', { class: 'sb-file-meta sb-danger-text', text: 'Delete this file?' }),
          h('button', { class: 'btn subtle sb-danger', type: 'button', text: 'Delete', 'aria-label': `Delete ${record.name}`, 'data-focus-key': `yes:${record.path}`, onclick: () => this.#delete(record) }),
          h('button', { class: 'btn subtle', type: 'button', text: 'Cancel', 'data-focus-key': `no:${record.path}`, onclick: () => {
            this.#confirm = null;
            this.#renderFiles();
            this.#filesBody.querySelector(`[data-focus-key="${CSS.escape(`delete:${record.path}`)}"]`)?.focus();
          } }))
        : h('div', { class: 'sb-file-foot' },
          h('span', { class: 'sb-file-meta', text: `${bytes(record.size)} · ${time}` }),
          h('button', { class: 'btn icon sb-mini', type: 'button', 'aria-label': `Download ${record.name}`, title: 'Download', 'data-focus-key': `download:${record.path}`, onclick: () => this.#download(record) }, strokeIcon('download', 13)),
          h('button', { class: 'btn icon sb-mini', type: 'button', 'aria-label': `Delete ${record.name}`, title: 'Delete', 'data-focus-key': `delete:${record.path}`, onclick: () => {
            this.#confirm = record.path;
            this.#renderFiles();
            this.#filesBody.querySelector(`[data-focus-key="${CSS.escape(`no:${record.path}`)}"]`)?.focus();
          } }, strokeIcon('trash', 13)));
      return h('li', { class: 'sb-file' },
        thumb,
        h('div', { class: 'sb-file-text' },
          h('span', { class: 'sb-file-name', text: record.name, title: record.location, 'data-full': record.name }),
          foot));
    });
    const parts = [h('ul', { class: 'sb-file-list' }, items)];
    const more = listed.length - this.#listLimit;
    if (more > 0) {
      parts.push(h('button', { class: 'link', type: 'button', text: `Show ${grouped(more)} more`, 'data-focus-key': 'more', onclick: () => {
        this.#listLimit = Infinity;
        this.#renderFiles();
      } }));
    }
    this.#keepFocus(this.#filesBody, () => this.#filesBody.replaceChildren(...parts));
    for (const el of this.#filesBody.querySelectorAll('[data-full]')) fitMiddle(el, el.dataset.full);
  }
}

/** Mount the Capture | Video pane in `container`. */
export function mountSidebar(container, app) {
  const sidebar = new Sidebar(container, app);
  return {
    capture: () => sidebar.capture(),
    toggleRecording: () => sidebar.toggleRecording(),
    stopRecording: () => sidebar.stopRecording(),
    destinationText: () => sidebar.destinationText(),
    pause: () => sidebar.pause(),
    resume: () => sidebar.resume()
  };
}
