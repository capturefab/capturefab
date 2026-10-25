// The image engine's module worker: owns capturefab.wasm and the current frame,
// measures, encodes stills, and runs the Pattern camera. engine.mjs is its only
// client. Requests run one at a time in arrival order, including their awaits,
// so the frame a request starts with is the frame it finishes with.
//
// Messages in: { id, op, args }. Replies: { id, ok: true, result } or
// { id, ok: false, error }. Events: { event: 'ready' | 'failed' | 'simframe' | 'error' }.
import { KIND, instantiate, matrixCode, metaJson, utf8, zipParts } from './core.mjs';

/** @type {import('./core.mjs').Core | null} */
let core = null;
// How the current frame was read: 'rgba' | 'nv12' | 'i420' | 'canvas' | 'sim'.
let path = null;
// Frames ingested here get their own ids, counted from 1.
let frames = 0;
// Whether VideoFrame.copyTo converts YUV frames to RGBA: undefined until one tells.
let rgbaCopy;
let scratch = null;

// JPEG quality: the desktop's fixed still quality (jpeg.rs QUALITY = 90).
const JPEG_QUALITY = 0.9;
const MIME = {
  png: 'image/png',
  jpeg: 'image/jpeg',
  raw: 'application/octet-stream',
  ppm: 'image/x-portable-pixmap',
  pgm: 'image/x-portable-graymap'
};
const EXTENSION = { png: 'png', jpeg: 'jpg', raw: 'raw', ppm: 'ppm', pgm: 'pgm' };

const closeFrame = (frame) => {
  try {
    frame?.close?.();
  } catch {
    // Already closed.
  }
};

/** A 2D context of at least width × height, reused between frames. */
const canvasContext = (width, height) => {
  if (typeof OffscreenCanvas !== 'function') return null;
  if (!scratch || scratch.canvas.width !== width || scratch.canvas.height !== height) {
    const canvas = new OffscreenCanvas(width, height);
    scratch = canvas.getContext('2d', { willReadFrequently: true, alpha: false });
    if (!scratch) return null;
  }
  return scratch;
};

/**
 * A trap inside the module (a failed allocation, a panic) leaves its state
 * borrowed for good: every later call would trap too.
 */
const fatal = (error) => typeof WebAssembly === 'object' && error instanceof WebAssembly.RuntimeError;
const FATAL_MESSAGE = 'The image engine ran out of memory and stopped.';

const isVideoFrame = (frame) => typeof frame?.copyTo === 'function' && typeof frame?.allocationSize === 'function';

/** Draw a frame or bitmap and read its pixels back: the path that works everywhere. */
const ingestCanvas = (frame, width, height, id, timestampNs) => {
  const context = canvasContext(width, height);
  if (!context) throw new Error('This browser cannot read video frames in a worker.');
  context.drawImage(frame, 0, 0, width, height);
  const { data } = context.getImageData(0, 0, width, height);
  core.buffer(0, data.length).set(data);
  core.frameRgbx(width, height, width * 4, { id, timestampNs });
  return 'canvas';
};

const FOUR_BYTE = new Set(['RGBA', 'RGBX', 'BGRA', 'BGRX']);
const YUV = new Set(['NV12', 'I420', 'I420A']);
// Formats whose own copy is never four bytes a pixel, so a four-byte RGBA size
// proves that the browser converts. (I444A and I422P10 copy four bytes a pixel.)
const CONVERTIBLE = new Set([...YUV, 'I422', 'I444', 'I422A']);

/**
 * Make `frame` (a VideoFrame or ImageBitmap) the current RGB8 frame: four-byte
 * frames copied as they are, YUV frames as RGBA from copyTo when the browser
 * converts, else their NV12/I420 planes converted here, else a canvas read-back.
 * Always closes the frame.
 */
async function ingest(frame) {
  try {
    const id = ++frames;
    if (!isVideoFrame(frame)) {
      const { width, height } = frame;
      if (!width || !height) throw new Error('The picture has no size yet.');
      path = ingestCanvas(frame, width, height, id, Math.round(performance.now() * 1e6));
      return;
    }
    const timestampNs = Math.max(0, Number(frame.timestamp) || 0) * 1000;
    // A rotated or flipped frame is drawn upright, as the preview shows it.
    if (frame.rotation || frame.flip) {
      path = ingestCanvas(frame, frame.displayWidth, frame.displayHeight, id, timestampNs);
      return;
    }
    const rect = frame.visibleRect;
    const width = rect?.width ?? frame.codedWidth;
    const height = rect?.height ?? frame.codedHeight;
    if (!width || !height) throw new Error('The picture has no size yet.');
    const size = width * height * 4;
    const format = frame.format;
    if (FOUR_BYTE.has(format)) {
      // Copied natively, never through copyTo's `format`: an engine that ignores
      // the option (WebKit) would hand back BGRA bytes labelled as RGBA.
      try {
        const view = core.buffer(0, frame.allocationSize());
        const [plane] = await frame.copyTo(view);
        if (plane.offset) view.copyWithin(0, plane.offset);
        core.frameRgbx(width, height, plane.stride, { bgr: format.startsWith('BGR'), id, timestampNs });
        path = 'rgba';
        return;
      } catch (error) {
        if (fatal(error)) throw error;
        // Fall back to the canvas below.
      }
      path = ingestCanvas(frame, width, height, id, timestampNs);
      return;
    }
    // Frames without a CPU format may convert too; without `format` support their
    // allocationSize throws.
    if (rgbaCopy !== false && (CONVERTIBLE.has(format) || !format)) {
      let converts = false;
      try {
        converts = frame.allocationSize({ format: 'RGBA' }) === size;
      } catch (error) {
        if (fatal(error)) throw error;
      }
      if (format) rgbaCopy = converts;
      if (converts) {
        try {
          // No other call into the module may run while copyTo writes here.
          const view = core.buffer(0, size);
          const [plane] = await frame.copyTo(view, { format: 'RGBA', colorSpace: 'srgb', layout: [{ offset: 0, stride: width * 4 }] });
          if (plane.offset === 0) {
            core.frameRgbx(width, height, plane.stride, { id, timestampNs });
            path = 'rgba';
            return;
          }
        } catch (error) {
          if (fatal(error)) throw error;
          // This frame (perhaps GPU-only) did not convert; try the next way.
        }
      }
    }
    if (YUV.has(format)) {
      try {
        const length = frame.allocationSize();
        const layout = await frame.copyTo(core.buffer(0, length));
        core.frameYuv(format === 'NV12' ? 'NV12' : 'I420', width, height, layout, {
          matrix: matrixCode(frame.colorSpace),
          fullRange: frame.colorSpace?.fullRange === true,
          id,
          timestampNs
        });
        path = format === 'NV12' ? 'nv12' : 'i420';
        return;
      } catch (error) {
        if (fatal(error)) throw error;
        // Fall back to the canvas below.
      }
    }
    path = ingestCanvas(frame, width, height, id, timestampNs);
  } finally {
    closeFrame(frame);
  }
}

const requireFrame = () => {
  const info = core.frameInfo();
  if (!info) throw new Error('Waiting for a frame');
  return info;
};

const kind = () => (path === 'sim' ? KIND.area_scan : KIND.unknown);

async function analyze({ frame, want }) {
  if (frame) await ingest(frame);
  const info = requireFrame();
  const result = { frame: { id: info.id, width: info.width, height: info.height, pixelFormat: info.pixelFormat, path } };
  if (want?.scopes) {
    try {
      result.scopes = core.scopes(Array.isArray(want.scopes) ? want.scopes : null);
    } catch {
      result.scopes = null;
    }
  }
  if (want?.focus) {
    try {
      result.focus = core.focus(want.focus);
    } catch {
      // Too small to measure.
      result.focus = null;
    }
  }
  return result;
}

function inspect({ region }) {
  const info = requireFrame();
  const { image, ...measured } = core.inspect(kind(), region ?? null);
  return {
    ...measured,
    frame: { id: info.id, width: info.width, height: info.height, pixel_format: info.pixelFormat, timestamp_ns: info.timestampNs }
  };
}

/** zlib-wrapped deflate from the browser's CompressionStream. */
async function deflate(bytes) {
  if (typeof CompressionStream !== 'function') throw new Error('This browser cannot compress PNG files; choose JPEG or PPM.');
  const stream = new Blob([bytes]).stream().pipeThrough(new CompressionStream('deflate'));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

/** The current frame as an RGBA canvas, for the browser's JPEG encoder and the Pattern camera's bitmaps. */
function drawRgba(info) {
  const context = canvasContext(info.width, info.height);
  const rgba = core.rgbaView();
  const image = new ImageData(new Uint8ClampedArray(rgba.buffer, rgba.byteOffset, rgba.byteLength), info.width, info.height);
  if (context) {
    context.putImageData(image, 0, 0);
    return { context, image: null };
  }
  // No OffscreenCanvas: hand back a copy that outlives the next call.
  return { context: null, image: new ImageData(new Uint8ClampedArray(image.data), info.width, info.height) };
}

async function capture({ format, meta: provenance, description, frame }) {
  if (frame) await ingest(frame);
  const info = requireFrame();
  if (!(format in MIME)) throw new Error(`unknown image format ${format}; use png, jpeg, raw, pgm or ppm`);
  // The frame fields describe the frame encoded here, as the desktop's still_meta
  // takes them from the Frame it saves; the page supplies the rest.
  const meta = provenance
    ? metaJson({
      ...provenance,
      frame_id: info.id,
      timestamp_ns: info.timestampNs,
      width: info.width,
      height: info.height,
      pixel_format: info.pixelFormat
    })
    : null;
  const metaBytes = meta ? utf8(meta) : null;
  if (metaBytes) core.checkMeta(metaBytes.length);
  let bytes;
  if (format === 'png') {
    const { colorType, rows } = core.pngRows();
    const zlib = await deflate(rows);
    bytes = core.pngAssemble(info.width, info.height, colorType, zlib, meta || null, description || null);
  } else if (format === 'jpeg') {
    const { context } = drawRgba(info);
    if (!context?.canvas?.convertToBlob) throw new Error('This browser cannot encode JPEG files here; choose PNG.');
    const blob = await context.canvas.convertToBlob({ type: 'image/jpeg', quality: JPEG_QUALITY });
    if (blob.type !== 'image/jpeg') throw new Error('This browser cannot encode JPEG files; choose PNG.');
    bytes = new Uint8Array(await blob.arrayBuffer());
    if (metaBytes) bytes = core.jpegCom(bytes, metaBytes);
  } else {
    bytes = core.encode(format);
  }
  return {
    blob: new Blob([bytes], { type: MIME[format] }),
    mime: MIME[format],
    ext: EXTENSION[format],
    width: info.width,
    height: info.height,
    pixelFormat: info.pixelFormat,
    pixelFormatCode: info.pixelFormatCode,
    bytes: info.bytes,
    frameId: info.id,
    timestampNs: info.timestampNs
  };
}

// -- Pattern camera -------------------------------------------------------------

// Bitmaps posted and not yet taken by the page. Like the desktop's newest-frame
// ring, frames keep their clock and ids while the page is busy, and only the
// pictures it cannot take are dropped.
const MAX_UNTAKEN = 2;
// `paused`: acquiring, but no frames are rendered while the page is hidden.
const sim = { epoch: 0, timer: 0, next: 0, generation: 0, untaken: 0, paused: false };

function simStopPacing() {
  clearTimeout(sim.timer);
  sim.timer = 0;
  sim.generation++;
}

/** Schedule the next frame on the frame clock; after a stall it restarts rather than catching up. */
function simSchedule(generation) {
  const delay = Math.max(0, sim.next - performance.now());
  sim.timer = setTimeout(() => enqueue(() => simTick(generation)), delay);
}

async function simTick(generation) {
  if (generation !== sim.generation || !core.simRunning) return;
  const now = performance.now();
  try {
    core.simRender(Math.round((now - sim.epoch) * 1e6));
    path = 'sim';
    if (sim.untaken < MAX_UNTAKEN) await postSimFrame(generation);
  } catch (error) {
    simStopPacing();
    if (fatal(error)) {
      stopEngine();
      return;
    }
    try {
      core.simStop();
    } catch {
      // Already stopped.
    }
    self.postMessage({ event: 'error', message: error?.message || String(error) });
    return;
  }
  sim.next = Math.max(sim.next + core.simPeriod * 1000, performance.now());
  simSchedule(generation);
}

/** Post the current frame as an ImageBitmap 'simframe' event. */
async function postSimFrame(generation) {
  const info = core.frameInfo();
  const { context, image } = drawRgba(info);
  const bitmap = context ? context.canvas.transferToImageBitmap() : await createImageBitmap(image);
  if (generation !== sim.generation) {
    bitmap.close();
    return;
  }
  sim.untaken++;
  self.postMessage({
    event: 'simframe',
    bitmap,
    id: info.id,
    width: info.width,
    height: info.height,
    pixelFormat: info.pixelFormat,
    timestampMs: info.timestampNs / 1e6
  }, [bitmap]);
}

function simOpen() {
  simStopPacing();
  sim.untaken = 0;
  sim.paused = false;
  core.simReset();
  sim.epoch = performance.now();
  return { info: core.simInfo(), features: core.simFeatures() };
}

function simStart() {
  if (core.simRunning && sim.timer) return null;
  core.simStart();
  simStopPacing();
  sim.paused = false;
  sim.untaken = 0;
  // The first frame is due now.
  sim.next = performance.now();
  simSchedule(sim.generation);
  return null;
}

function simStop() {
  simStopPacing();
  sim.paused = false;
  core.simStop();
  return null;
}

/**
 * Pause or resume rendering without stopping acquisition, for a hidden page:
 * features stay locked as while streaming, and frame ids and the clock carry on
 * from where they were. Resuming a camera that is not acquiring does nothing.
 */
function simPause({ paused }) {
  if (paused) {
    if (core.simRunning && !sim.paused) {
      simStopPacing();
      sim.paused = true;
    }
    return { paused: sim.paused };
  }
  if (sim.paused) {
    sim.paused = false;
    simStopPacing();
    if (core.simRunning) {
      sim.next = performance.now();
      simSchedule(sim.generation);
    }
  }
  return { paused: false };
}

// -- Checksums and archives -------------------------------------------------------

async function crcOf(blob) {
  let crc = 0;
  const reader = blob.stream().getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return crc >>> 0;
    crc = core.crc32(crc, value);
  }
}

async function zip({ entries }) {
  const described = [];
  for (const { name, blob, date } of entries) {
    described.push({ name, data: blob, size: blob.size, crc: await crcOf(blob), date: date instanceof Date ? date : new Date(date) });
  }
  return new Blob(zipParts(described), { type: 'application/zip' });
}

// -- Dispatch ---------------------------------------------------------------------

const OPS = {
  analyze,
  inspect,
  capture,
  simOpen,
  simStart,
  simStop,
  simPause,
  simFeatures: () => core.simFeatures(),
  simSet: ({ name, text }) => core.simSet(name, text),
  crc32: ({ blob }) => crcOf(blob),
  zip
};

/** After a trap: drop the module and tell the page, which replaces this worker. */
function stopEngine() {
  if (!core) return;
  core = null;
  simStopPacing();
  self.postMessage({ event: 'error', message: FATAL_MESSAGE });
}

let queue = Promise.resolve();
const enqueue = (task) => {
  queue = queue.then(task, task);
  return queue;
};

self.addEventListener('message', (event) => {
  // The page took a Pattern camera frame.
  if (event.data?.taken === 'simframe') {
    sim.untaken = Math.max(0, sim.untaken - 1);
    return;
  }
  const { id, op, args = {} } = event.data ?? {};
  enqueue(async () => {
    try {
      if (!core) throw new Error('The image engine is not ready.');
      if (!Object.hasOwn(OPS, op)) throw new Error(`unknown request ${op}`);
      const result = await OPS[op](args);
      self.postMessage({ id, ok: true, result });
    } catch (error) {
      if (fatal(error)) {
        self.postMessage({ id, ok: false, error: FATAL_MESSAGE });
        stopEngine();
      } else {
        self.postMessage({ id, ok: false, error: error?.message || String(error) });
      }
    } finally {
      // Frames are transferred in; whatever happened, none stays open.
      closeFrame(args?.frame);
    }
  });
});

(async () => {
  try {
    let response;
    try {
      response = await fetch(new URL('./capturefab.wasm', import.meta.url));
    } catch (error) {
      throw new Error('capturefab.wasm could not be downloaded. Check the connection, then try again.', { cause: error });
    }
    core = await instantiate(response);
    self.postMessage({ event: 'ready', info: { abi: core.abi, version: core.version } });
  } catch (error) {
    self.postMessage({ event: 'failed', message: error?.message || String(error) });
  }
})();
