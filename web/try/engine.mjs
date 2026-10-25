// The image engine: the desktop app's measurements, still encoders and Pattern
// camera, running in a module worker (worker.mjs) on capturefab.wasm. The rest of
// the workbench talks to the worker only through this class.
//
// Frames passed in are transferred: the engine owns them from the call on and the
// worker closes them. Events: 'simframe' (a Pattern camera frame; the listener
// must close detail.bitmap) and 'error' (detail.message: the engine stopped).
import { contextDescription, sortKeys } from './core.mjs';

// The worker reports in once its module has compiled; a page that cannot fetch it
// fails long before this.
const LOAD_TIMEOUT_MS = 60_000;
const STOPPED = 'The image engine stopped.';

const closeFrame = (frame) => {
  try {
    frame?.close?.();
  } catch {
    // Already closed.
  }
};

const token = Symbol('Engine');

export class Engine extends EventTarget {
  #worker;
  #info = null;
  #next = 1;
  #pending = new Map();
  #stopped = null;
  // { resolve, reject, timer } while create() waits for the worker.
  #ready = null;
  // 'simframe' listeners: without any, bitmaps are closed here instead of leaking.
  #simListeners = new Set();

  constructor(key, worker) {
    super();
    if (key !== token) throw new TypeError('Use Engine.create()');
    this.#worker = worker;
    worker.addEventListener('message', this.#onMessage);
    worker.addEventListener('error', this.#onError);
  }

  /** Start the worker and wait until capturefab.wasm is ready. Rejects with a readable Error. */
  static async create() {
    if (typeof Worker !== 'function') throw new Error('This browser cannot run background workers, which Capturefab needs.');
    let worker;
    try {
      worker = new Worker(new URL('./worker.mjs', import.meta.url), { type: 'module', name: 'capturefab-engine' });
    } catch (error) {
      throw new Error(`The image engine could not start: ${error?.message || error}`, { cause: error });
    }
    const engine = new Engine(token, worker);
    try {
      engine.#info = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('The image engine took too long to load. Check the connection, then try again.')), LOAD_TIMEOUT_MS);
        engine.#ready = { resolve, reject, timer };
      });
    } catch (error) {
      engine.terminate();
      throw error;
    }
    return engine;
  }

  #settleReady(error, info) {
    const ready = this.#ready;
    if (!ready) return false;
    this.#ready = null;
    clearTimeout(ready.timer);
    if (error) ready.reject(error);
    else ready.resolve(info);
    return true;
  }

  #onMessage = (event) => {
    const data = event.data ?? {};
    if (data.event) {
      switch (data.event) {
        case 'ready':
          this.#settleReady(null, Object.freeze({ abi: data.info?.abi, version: data.info?.version }));
          break;
        case 'failed':
          this.#settleReady(new Error(data.message || 'The image engine did not load.'));
          break;
        case 'simframe': {
          const { event: _, ...detail } = data;
          if (this.#stopped || !this.#simListeners.size) {
            detail.bitmap?.close?.();
            if (!this.#stopped) this.#worker.postMessage({ taken: 'simframe' });
            break;
          }
          try {
            this.dispatchEvent(new CustomEvent('simframe', { detail }));
          } finally {
            // Listeners run synchronously; tell the worker the picture was taken.
            this.#worker.postMessage({ taken: 'simframe' });
          }
          break;
        }
        case 'error':
          this.dispatchEvent(new CustomEvent('error', { detail: { message: data.message } }));
          break;
        default:
          break;
      }
      return;
    }
    const request = this.#pending.get(data.id);
    if (!request) return;
    this.#pending.delete(data.id);
    if (data.ok) request.resolve(data.result);
    else request.reject(new Error(data.error));
  };

  #onError = (event) => {
    event.preventDefault?.();
    const message = event?.message
      ? `The image engine stopped: ${event.message}`
      : 'The image engine did not start in this browser. Reload the page, or try a current browser.';
    if (this.#settleReady(new Error(message))) return;
    this.#stop(message);
    this.dispatchEvent(new CustomEvent('error', { detail: { message } }));
  };

  #stop(message) {
    if (this.#stopped) return;
    this.#stopped = message;
    for (const request of this.#pending.values()) request.reject(new Error(message));
    this.#pending.clear();
  }

  addEventListener(type, listener, options) {
    if (type === 'simframe' && listener) this.#simListeners.add(listener);
    super.addEventListener(type, listener, options);
  }

  removeEventListener(type, listener, options) {
    if (type === 'simframe') this.#simListeners.delete(listener);
    super.removeEventListener(type, listener, options);
  }

  /** { abi, version } of the loaded module. */
  get info() {
    return this.#info;
  }

  /** Stop the worker; later calls reject. Safe to call more than once. */
  terminate() {
    this.#settleReady(new Error(STOPPED));
    this.#stop(STOPPED);
    this.#worker.removeEventListener('message', this.#onMessage);
    this.#worker.removeEventListener('error', this.#onError);
    this.#worker.terminate();
  }

  #request(op, args = {}, frame = null) {
    if (this.#stopped) {
      closeFrame(frame);
      return Promise.reject(new Error(this.#stopped));
    }
    const id = this.#next++;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      try {
        this.#worker.postMessage({ id, op, args: frame ? { ...args, frame } : args }, frame ? [frame] : []);
      } catch (error) {
        this.#pending.delete(id);
        closeFrame(frame);
        reject(error);
      }
    });
  }

  /**
   * Make `frame` (VideoFrame, ImageBitmap, or null to keep the current frame) the
   * current frame and measure what `want` asks: { scopes?: [x,y,w,h] | true, focus?: [x,y,w,h] }.
   */
  analyze(frame, want = {}) {
    const scopes = Array.isArray(want?.scopes) ? want.scopes.map(Number) : Boolean(want?.scopes);
    const focus = Array.isArray(want?.focus) ? want.focus.map(Number) : undefined;
    return this.#request('analyze', { want: { scopes, focus } }, frame);
  }

  /** The frame RPC's measurements of a normalized region (null: the whole frame). */
  inspect(region = null) {
    return this.#request('inspect', { region: Array.isArray(region) ? region.map(Number) : null });
  }

  /**
   * Encode the current frame (or `frame`, which becomes current) as 'png', 'jpeg',
   * 'raw', 'pgm' or 'ppm', embedding `meta` as compact JSON with sorted keys. The
   * worker sets frame_id, timestamp_ns, width, height and pixel_format from the
   * frame it encodes, so they always match the result's frameId, timestampNs,
   * width, height and pixelFormat.
   */
  capture(format, meta = null, frame = null) {
    const args = { format: String(format) };
    if (meta) {
      // A plain copy: it crosses to the worker, which serializes it.
      args.meta = sortKeys(meta);
      args.description = contextDescription(meta);
    }
    return this.#request('capture', args, frame);
  }

  /** Reset and open the Pattern camera: { info, features }. */
  simOpen() {
    return this.#request('simOpen');
  }

  /** Start the Pattern camera; frames arrive as 'simframe' events. */
  simStart() {
    return this.#request('simStart');
  }

  simStop() {
    return this.#request('simStop');
  }

  /**
   * Pause (true) or resume (false) the Pattern camera's frames without stopping
   * acquisition, for example while the page is hidden. Settings stay locked as
   * while streaming, and frame ids and timestamps carry on after a resume.
   * Starting or stopping the camera clears a pause. Resolves { paused }.
   */
  simPause(paused) {
    return this.#request('simPause', { paused: Boolean(paused) });
  }

  /** The Pattern camera's FeatureInfo list. */
  simFeatures() {
    return this.#request('simFeatures');
  }

  /** Set a Pattern camera feature: { name, value } with the value read back. */
  simSet(name, text) {
    return this.#request('simSet', { name: String(name), text: String(text) });
  }

  /** The CRC-32 of a Blob, as an unsigned number. */
  crc32(blob) {
    return this.#request('crc32', { blob });
  }

  /** A stored ZIP archive of [{ name, blob, date }] with UTF-8 names. */
  zip(entries) {
    return this.#request('zip', {
      entries: entries.map(({ name, blob, date }) => ({ name: String(name), blob, date: date instanceof Date ? date : new Date(date ?? Date.now()) }))
    });
  }
}
