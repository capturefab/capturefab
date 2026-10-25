// Where frames come from: this computer's webcams through getUserMedia, and the
// Pattern camera (sim:0) that the engine's worker renders. A source shows its picture
// in an element on the stage, counts frames and their rate, notices stalls, and feeds
// the engine at most one analysis at a time for the scopes (the desktop measures a
// histogram at most every 200 ms and focus at most every 66 ms, src/gui/mod.rs:130).
import { fnv1a64 } from './format.mjs';

/** A stream with no new frame for this long says "Waiting for frames". */
export const STALL_MS = 3000;
const SCOPES_EVERY = 200;
const FOCUS_EVERY = 66;
/** Frames skipped this recently show in amber. */
export const RECENT_SKIP_MS = 4000;

const PATTERN = Object.freeze({
  kind: 'sim', id: 'sim:0', vendor: 'Capturefab', model: 'Pattern camera', serial: 'SIM0',
  transport: 'Simulator', detail: 'S/N SIM0 · Simulator'
});

/** The Pattern camera's identity, before it opens. */
export const patternInfo = () => ({ ...PATTERN });

/** Whether this page may ask for a camera at all. */
export const webcamAvailable = () => globalThis.isSecureContext === true && typeof navigator.mediaDevices?.getUserMedia === 'function';

/** This computer's video inputs. Before access is allowed, labels (and often ids) are empty. */
export async function listWebcams() {
  if (!webcamAvailable() || typeof navigator.mediaDevices.enumerateDevices !== 'function') return [];
  const devices = await navigator.mediaDevices.enumerateDevices();
  const seen = new Set();
  const cameras = [];
  for (const device of devices) {
    if (device.kind !== 'videoinput') continue;
    // Without access, browsers list anonymous entries that cannot be told apart.
    const key = device.deviceId || '';
    if (seen.has(key)) continue;
    seen.add(key);
    cameras.push({ deviceId: device.deviceId, label: device.label, groupId: device.groupId });
  }
  return cameras;
}

/** Whether this browser reports camera access as refused, without asking. */
export async function cameraPermission() {
  try {
    const status = await navigator.permissions?.query({ name: 'camera' });
    return status?.state ?? 'prompt';
  } catch {
    return 'prompt';
  }
}

/** Why a camera did not open, as `{ state, message }` in the workbench's words. */
export const cameraProblem = (error) => {
  switch (error?.name) {
    case 'NotAllowedError':
    case 'PermissionDeniedError':
    case 'SecurityError':
      return { state: 'blocked', message: "Camera access is blocked. Allow it in this site's settings, or try the Pattern camera." };
    case 'NotFoundError':
    case 'DevicesNotFoundError':
      return { state: 'none', message: 'No cameras found yet.' };
    case 'OverconstrainedError':
      return { state: 'none', message: 'That camera is no longer connected. Select Search again.' };
    case 'NotReadableError':
    case 'TrackStartError':
    case 'AbortError':
      return { state: 'busy', message: 'The camera did not start. Another app or tab may be using it: close it, then select the camera again.' };
    default:
      return { state: 'failed', message: `The camera did not start: ${error?.message || String(error)}` };
  }
};

/** A webcam's identity as the desktop names cameras: model from its label, serial from its id. */
export const webcamInfo = (deviceId, label) => {
  const model = (label || '').trim() || 'Webcam';
  // A short hash, as media.rs hashes stream URLs: never the raw device id.
  const serial = fnv1a64(deviceId || model).slice(0, 8);
  return { kind: 'webcam', id: `webcam:${serial}`, vendor: 'Webcam', model, serial, transport: 'Webcam', detail: 'Webcam · this computer' };
};

/**
 * Engine analyses for the scopes, one in flight at a time. `want()` says what is
 * shown now: `{ scopes: boolean, focus: [x, y, w, h] | null }`. The first frame
 * after `reset()` is analyzed anyway, to learn its pixel format and path.
 */
export class Pump {
  #busy = false;
  #probe = true;
  #scopesAt = -Infinity;
  #focusAt = -Infinity;

  constructor({ engine, want, result, error }) {
    this.engine = engine;
    this.want = want;
    this.result = result;
    this.error = error;
  }

  get busy() {
    return this.#busy;
  }

  reset() {
    this.#probe = true;
    this.#scopesAt = -Infinity;
    this.#focusAt = -Infinity;
  }

  /** A new frame is on screen at `now`; `take()` gives the frame to transfer (or null for the engine's current one). */
  offer(now, take) {
    if (this.#busy || document.visibilityState === 'hidden') return;
    const engine = this.engine();
    if (!engine) return;
    const shown = this.want();
    const want = {};
    if (shown.scopes && now - this.#scopesAt >= SCOPES_EVERY) want.scopes = true;
    if (shown.focus && now - this.#focusAt >= FOCUS_EVERY) want.focus = shown.focus;
    if (!want.scopes && !want.focus && !this.#probe) return;
    this.#busy = true;
    const probe = this.#probe;
    this.#probe = false;
    if (want.scopes) this.#scopesAt = now;
    if (want.focus) this.#focusAt = now;
    (async () => {
      let frame = null;
      try {
        frame = await take();
      } catch {
        // The picture is not ready yet; try again on the next frame.
        if (probe) this.#probe = true;
        return;
      }
      const result = await engine.analyze(frame, want);
      this.result(result, want);
    })().catch((error) => this.error(error)).finally(() => { this.#busy = false; });
  }
}

/** What every source shares: identity, its picture element, and frame counting. */
class Source extends EventTarget {
  info = null;
  running = false;
  width = 0;
  height = 0;
  /** Frames since the stream started, and how many the preview skipped. */
  count = 0;
  skipped = 0;
  skippedAt = -Infinity;
  fps = 0;
  /** Whether `fps` holds a measurement: a window of at least a second has passed. */
  rated = false;
  /** performance.now() of the newest frame, or 0 before the first. */
  frameAt = 0;
  startedAt = 0;
  /** Set by the workbench: a Pump, and a per-frame hook (now) => void. */
  pump = null;
  onFrame = null;
  #base = null;
  #last = 0;
  #window = { at: 0, count: 0 };

  get kind() {
    return this.info?.kind ?? '';
  }

  get hasFrame() {
    return this.width > 0 && this.height > 0;
  }

  /** Count a frame: `presented` is the source's running frame number. */
  tick(now, presented, width, height) {
    if (this.#base === null) {
      this.#base = presented - 1;
      this.#last = presented - 1;
      this.#window = { at: now, count: 0 };
    }
    const gap = presented - this.#last - 1;
    if (gap > 0) {
      this.skipped += gap;
      this.skippedAt = now;
    }
    this.#last = presented;
    this.count = presented - this.#base;
    this.frameAt = now;
    const sized = width !== this.width || height !== this.height;
    this.width = width;
    this.height = height;
    this.#rate(now);
    this.onFrame?.(now, sized);
  }

  /** Bring the frame rate up to date at `now`: frames over a window of at least one second. */
  #rate(now) {
    const elapsed = now - this.#window.at;
    if (elapsed < 1000) return;
    this.fps = ((this.count - this.#window.count) * 1000) / elapsed;
    this.rated = true;
    this.#window = { at: now, count: this.count };
  }

  /** Called from the workbench's clock, so a stalled stream's rate falls. */
  sample(now) {
    if (this.running && this.frameAt && now - this.#window.at >= 2000) this.#rate(now);
  }

  /** Start counting afresh, for a new run of the stream. */
  restart() {
    this.#base = null;
    this.count = 0;
    this.skipped = 0;
    this.skippedAt = -Infinity;
    this.fps = 0;
    this.rated = false;
    this.frameAt = 0;
    this.startedAt = performance.now();
    this.pump?.reset();
  }

  /** Milliseconds since the last frame while running (since start before the first). */
  silence(now) {
    if (!this.running) return 0;
    return now - (this.frameAt || this.startedAt || now);
  }
}

/** Thrown by a camera start that was overtaken by close() or a stop. */
const closedError = () => Object.assign(new Error('The camera was closed.'), { name: 'AbortError' });

/** A webcam on this computer, through getUserMedia. */
export class WebcamSource extends Source {
  video = document.createElement('video');
  /** The last picture, kept on a canvas once the camera is released. */
  still = null;
  mirrorDefault = true;
  #stream = null;
  #track = null;
  #deviceId = '';
  #handle = 0;
  #raf = 0;
  #frames = 0;
  #listeners = null;
  /** Set by close(): a camera that arrives afterwards is let go at once. */
  #closed = false;
  /** Bumped by close() and #halt(), so an acquire still waiting is abandoned. */
  #gen = 0;
  /**
   * What was asked of this camera from the Features pane, { format, controls } as
   * features.mjs keeps it; restated when the camera is opened again after a stop.
   */
  asked = null;

  constructor() {
    super();
    const video = this.video;
    video.muted = true;
    video.defaultMuted = true;
    video.playsInline = true;
    video.autoplay = true;
    video.disablePictureInPicture = true;
    video.setAttribute('playsinline', '');
    video.setAttribute('muted', '');
    video.setAttribute('disablepictureinpicture', '');
    video.setAttribute('disableremoteplayback', '');
    video.setAttribute('aria-hidden', 'true');
  }

  /** Ask for camera `deviceId` (or the default) and start streaming it. */
  static async open(deviceId = '') {
    const source = new WebcamSource();
    await source.#acquire(deviceId);
    return source;
  }

  get track() {
    return this.#track;
  }

  get stream() {
    return this.#stream;
  }

  get deviceId() {
    return this.#deviceId;
  }

  /** The element showing the picture: the live video, or the kept still once stopped. */
  get view() {
    return this.running || !this.still ? this.video : this.still;
  }

  async #acquire(deviceId) {
    if (this.#closed) throw closedError();
    const gen = this.#gen;
    const video = { width: { ideal: 1280 }, height: { ideal: 720 } };
    if (deviceId) video.deviceId = { exact: deviceId };
    // Native camera modes rather than a cropped and scaled picture, where supported.
    if (navigator.mediaDevices.getSupportedConstraints?.().resizeMode) video.resizeMode = 'none';
    // Opened again after a stop: ask for the video format set in Features, loosely, so a
    // camera whose modes changed still opens; the exact values are restated afterwards.
    for (const [key, value] of this.asked?.format ?? []) video[key] = key === 'resizeMode' ? value : { ideal: value };
    const stream = await navigator.mediaDevices.getUserMedia({ video, audio: false });
    if (this.#closed || gen !== this.#gen) {
      // Closed or stopped while the browser was opening it: let it go at once.
      stream.getTracks().forEach((t) => t.stop());
      throw closedError();
    }
    const track = stream.getVideoTracks()[0];
    if (!track) {
      stream.getTracks().forEach((t) => t.stop());
      throw Object.assign(new Error('The camera sent no video track.'), { name: 'NotFoundError' });
    }
    this.#stream = stream;
    this.#track = track;
    const settings = track.getSettings?.() ?? {};
    this.#deviceId = settings.deviceId || deviceId || '';
    this.info = webcamInfo(this.#deviceId, track.label);
    this.mirrorDefault = !settings.facingMode || settings.facingMode === 'user';
    this.#listeners?.abort();
    this.#listeners = new AbortController();
    const { signal } = this.#listeners;
    track.addEventListener('ended', () => {
      if (this.#track !== track) return;
      this.#keepStill();
      this.#halt();
      this.dispatchEvent(new Event('ended'));
    }, { signal });
    track.addEventListener('mute', () => this.dispatchEvent(new Event('mute')), { signal });
    track.addEventListener('unmute', () => this.dispatchEvent(new Event('unmute')), { signal });
    await this.#play();
  }

  async #play() {
    const video = this.video;
    const gen = this.#gen;
    video.srcObject = this.#stream;
    this.restart();
    this.running = true;
    this.#frames = 0;
    this.#loop();
    try {
      await video.play();
    } catch (error) {
      // Autoplay of a muted inline video is allowed; anything else is a real failure,
      // and the camera is let go rather than left on with nothing showing it.
      if (error?.name !== 'AbortError') {
        this.#halt();
        throw error;
      }
    }
    if (this.#closed || gen !== this.#gen) {
      if (gen === this.#gen) this.#halt();
      throw closedError();
    }
  }

  #loop() {
    const video = this.video;
    if (typeof video.requestVideoFrameCallback === 'function') {
      const step = (now, meta) => {
        if (!this.running) return;
        this.#handle = video.requestVideoFrameCallback(step);
        this.#frame(now, meta.presentedFrames ?? (this.#frames += 1), meta.width || video.videoWidth, meta.height || video.videoHeight, meta.mediaTime);
      };
      this.#handle = video.requestVideoFrameCallback(step);
      return;
    }
    // Without requestVideoFrameCallback, a changed playback time marks a new frame.
    let time = -1;
    const poll = (now) => {
      if (!this.running) return;
      this.#raf = requestAnimationFrame(poll);
      if (video.readyState < 2 || video.currentTime === time) return;
      time = video.currentTime;
      this.#frames += 1;
      this.#frame(now, this.#frames, video.videoWidth, video.videoHeight, time);
    };
    this.#raf = requestAnimationFrame(poll);
  }

  #frame(now, presented, width, height, mediaTime) {
    if (!width || !height) return;
    this.tick(now, presented, width, height);
    this.pump?.offer(now, () => this.#grabNow(mediaTime));
  }

  /** The picture on screen now, as a frame for the engine (ownership passes to the caller). */
  #grabNow(mediaTime) {
    const element = this.running ? this.video : this.still;
    if (!element) return null;
    if (element === this.video && this.video.readyState < 2) throw new Error('no picture yet');
    if (typeof VideoFrame === 'function') {
      const timestamp = Math.round((Number.isFinite(mediaTime) ? mediaTime * 1e6 : performance.now() * 1000));
      return new VideoFrame(element, { timestamp });
    }
    return createImageBitmap(element);
  }

  /** The current picture for a capture: a VideoFrame (or ImageBitmap), or null with no picture. */
  async grab() {
    try {
      return await this.#grabNow();
    } catch {
      return null;
    }
  }

  async start() {
    if (this.running) return;
    if (!this.#track || this.#track.readyState === 'ended') await this.#acquire(this.#deviceId);
    else await this.#play();
  }

  /** Release the camera, keeping its last picture on screen. */
  async stop() {
    if (!this.running) return;
    this.#keepStill();
    this.#halt();
  }

  #keepStill() {
    const video = this.video;
    if (video.readyState < 2 || !video.videoWidth) return;
    const canvas = this.still ?? document.createElement('canvas');
    canvas.setAttribute('aria-hidden', 'true');
    canvas.width = video.videoWidth;
    canvas.height = video.videoHeight;
    canvas.getContext('2d')?.drawImage(video, 0, 0);
    this.still = canvas;
  }

  #halt() {
    this.#gen += 1;
    this.running = false;
    if (this.#handle && typeof this.video.cancelVideoFrameCallback === 'function') this.video.cancelVideoFrameCallback(this.#handle);
    cancelAnimationFrame(this.#raf);
    this.#handle = 0;
    this.#raf = 0;
    this.#listeners?.abort();
    this.#listeners = null;
    this.#stream?.getTracks().forEach((track) => track.stop());
    this.#stream = null;
    this.#track = null;
    this.video.pause();
    this.video.srcObject = null;
  }

  mediaStream() {
    return this.running ? this.#stream : null;
  }

  close() {
    this.#closed = true;
    this.#halt();
    this.video.removeAttribute('src');
    this.video.load();
    if (this.still) {
      this.still.width = 0;
      this.still.height = 0;
      this.still = null;
    }
  }
}

/** The Pattern camera (sim:0), rendered by the engine's worker and posted as ImageBitmaps. */
export class SimSource extends Source {
  canvas = document.createElement('canvas');
  mirrorDefault = false;
  #engine;
  #context = null;
  #bitmap = false;
  #stream = null;
  #listening = false;
  #pixelFormat = 'Mono8';

  constructor(engine) {
    super();
    this.#engine = engine;
    this.canvas.setAttribute('aria-hidden', 'true');
    this.#context = this.canvas.getContext('bitmaprenderer');
    this.#bitmap = Boolean(this.#context);
    if (!this.#context) this.#context = this.canvas.getContext('2d');
  }

  static async open(engine) {
    const source = new SimSource(engine);
    const opened = await engine.simOpen();
    const info = opened?.info ?? {};
    source.info = {
      ...PATTERN,
      vendor: info.vendor || PATTERN.vendor,
      model: info.model || PATTERN.model,
      serial: info.serial || PATTERN.serial
    };
    source.info.detail = `S/N ${source.info.serial} · Simulator`;
    return source;
  }

  get view() {
    return this.canvas;
  }

  get pixelFormat() {
    return this.#pixelFormat;
  }

  #onFrame = (event) => {
    const { bitmap, id, width, height, pixelFormat } = event.detail ?? {};
    if (!bitmap) return;
    try {
      if (!this.running) return;
      const canvas = this.canvas;
      if (canvas.width !== width || canvas.height !== height) {
        canvas.width = width;
        canvas.height = height;
      }
      if (this.#bitmap) this.#context.transferFromImageBitmap(bitmap);
      else this.#context?.drawImage(bitmap, 0, 0);
      if (pixelFormat) this.#pixelFormat = pixelFormat;
      const now = performance.now();
      // Frame ids count every frame the worker rendered, so a gap is a skipped frame.
      const number = Number(id);
      this.tick(now, Number.isFinite(number) ? number + 1 : this.count + 1, width, height);
      this.pump?.offer(now, () => null);
    } finally {
      bitmap.close();
    }
  };

  #listen(on) {
    if (on === this.#listening) return;
    this.#listening = on;
    if (on) this.#engine.addEventListener('simframe', this.#onFrame);
    else this.#engine.removeEventListener('simframe', this.#onFrame);
  }

  async start() {
    if (this.running) return;
    this.restart();
    this.running = true;
    this.#listen(true);
    try {
      await this.#engine.simStart();
    } catch (error) {
      this.running = false;
      this.#listen(false);
      throw error;
    }
  }

  async stop() {
    if (!this.running) return;
    this.running = false;
    try {
      await this.#engine.simStop();
    } finally {
      this.#listen(false);
    }
  }

  /** The engine captures its own current frame. */
  async grab() {
    return null;
  }

  /** The stage canvas as a stream, for recording. */
  mediaStream() {
    if (typeof this.canvas.captureStream !== 'function') return null;
    if (!this.#stream || this.#stream.getVideoTracks().every((t) => t.readyState === 'ended')) this.#stream = this.canvas.captureStream();
    return this.#stream;
  }

  close() {
    if (this.running) this.#engine.simStop().catch(() => {});
    this.running = false;
    this.#listen(false);
    this.#stream?.getTracks().forEach((track) => track.stop());
    this.#stream = null;
  }
}
