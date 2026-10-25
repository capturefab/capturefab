// Typed access to capturefab.wasm, the Rust port of the desktop app's frame
// measurements, conversions, still encoders and Pattern camera (web-try/,
// ABI described in web-try/src/abi.rs). Used by worker.mjs and by the Node tests
// in scripts/try-tests, so this module touches no DOM and no window.
//
// The module owns its memory, and any call may grow it, which detaches earlier
// views of memory.buffer: every view here is made right after the call that
// sized it and is copied (or used) before the next call.

export const ABI = 1;

/** CameraKind codes of the ABI. */
export const KIND = Object.freeze({
  unknown: 0,
  area_scan: 1,
  line_scan: 2,
  depth: 3,
  microscope: 4,
  thermal: 5,
  document_scanner: 6
});

/** PFNC codes the browser uses. */
export const PIXEL_FORMAT = Object.freeze({
  Mono8: 0x01080001,
  RGB8: 0x02180014,
  BGR8: 0x02180015,
  Mono10: 0x01100003,
  Mono12: 0x01100005,
  Mono16: 0x01100007,
  BayerRG8: 0x01080009
});

const FRAME_SLOT = 0;
const ARGUMENT_SLOT = 1;
const ENCODE = { raw: 0, ppm: 1, pgm: 2 };
const CONVERT = { rgb: 0, gray8: 1, rgba: 2 };
// CRC input goes through the argument buffer in pieces of at most this size.
const CRC_CHUNK = 1 << 20;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** UTF-8 bytes of `text`. */
export const utf8 = (text) => encoder.encode(String(text));

/**
 * The YUV matrix code for a VideoColorSpace: 0 BT.601, 1 BT.709, 2 BT.2020. An empty
 * or unknown colour space is BT.709, as frames from a <video> often report none.
 */
export const matrixCode = (colorSpace) => {
  switch (colorSpace?.matrix) {
    case 'bt470bg':
    case 'smpte170m':
      return 0;
    case 'bt2020-ncl':
      return 2;
    default:
      return 1;
  }
};

const isBytes = (value) => value instanceof Uint8Array;

/** A Uint8Array over any BufferSource. */
const bytesOf = (value) => {
  if (isBytes(value)) return value;
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  throw new TypeError('expected bytes');
};

/** Instantiate from a Response (or a promise of one), bytes or a compiled WebAssembly.Module. */
export async function instantiate(source) {
  if (typeof WebAssembly !== 'object') throw new Error('This browser cannot run WebAssembly.');
  let instance;
  try {
    source = await source;
    if (source instanceof WebAssembly.Module) {
      instance = await WebAssembly.instantiate(source, {});
    } else if (typeof Response === 'function' && source instanceof Response) {
      if (!source.ok) throw new Error(`capturefab.wasm did not load (HTTP ${source.status}).`);
      const type = (source.headers.get('content-type') || '').split(';')[0].trim().toLowerCase();
      if (type === 'application/wasm' && typeof WebAssembly.instantiateStreaming === 'function') {
        try {
          ({ instance } = await WebAssembly.instantiateStreaming(source.clone(), {}));
        } catch (error) {
          // A compile error fails the same way below; anything else (a proxy that
          // breaks streaming) gets the plain download.
          if (error instanceof WebAssembly.CompileError) throw error;
        }
      }
      instance ??= (await WebAssembly.instantiate(await source.arrayBuffer(), {})).instance;
    } else {
      instance = (await WebAssembly.instantiate(bytesOf(source), {})).instance;
    }
  } catch (error) {
    if (error instanceof WebAssembly.CompileError || error instanceof WebAssembly.LinkError) {
      throw new Error('This browser cannot run Capturefab\'s WebAssembly module (it needs WebAssembly SIMD).', { cause: error });
    }
    throw error;
  }
  const core = new Core(instance);
  if (core.abi !== ABI) throw new Error(`capturefab.wasm has ABI ${core.abi}; this page needs ABI ${ABI}. Reload the page.`);
  return core;
}

/** The module's exports with typed arguments and results. Not reentrant. */
export class Core {
  #x;
  #labels = new Map();

  constructor(instance) {
    this.#x = instance.exports;
  }

  get abi() {
    return this.#x.cf_abi() >>> 0;
  }

  get version() {
    this.#x.cf_version();
    return this.#text();
  }

  /** The memory's current buffer (detached by the next call that grows it). */
  get #buffer() {
    return this.#x.memory.buffer;
  }

  #out() {
    return new Uint8Array(this.#buffer, this.#x.cf_out_ptr() >>> 0, this.#x.cf_out_len() >>> 0);
  }

  #bytes() {
    return this.#out().slice();
  }

  #text() {
    return decoder.decode(this.#out());
  }

  #json() {
    return JSON.parse(this.#text());
  }

  /** The status, or throws the module's message (the desktop app's wording). */
  #check(status) {
    if (status < 0) throw new Error(this.#text());
    return status;
  }

  /**
   * A view of `len` bytes of a module-owned buffer: slot 0 for frame pixels, slot 1
   * for arguments. Fill it before the next call into the module.
   */
  buffer(slot, len) {
    const pointer = this.#x.cf_buffer(slot, len) >>> 0;
    if (!pointer && len) throw new Error('The image engine is out of memory.');
    return new Uint8Array(this.#buffer, pointer, len);
  }

  /** Pack byte or text arguments back to back into slot 1; their lengths. */
  #arguments(...parts) {
    const encoded = parts.map((part) => (typeof part === 'string' ? utf8(part) : bytesOf(part)));
    const total = encoded.reduce((sum, part) => sum + part.length, 0);
    const view = this.buffer(ARGUMENT_SLOT, total);
    let at = 0;
    for (const part of encoded) {
      view.set(part, at);
      at += part.length;
    }
    return encoded.map((part) => part.length);
  }

  // -- The current frame ------------------------------------------------------

  /** Four bytes per pixel, already in slot 0, become the current RGB8 frame. */
  frameRgbx(width, height, stride, { bgr = false, id = 0, timestampNs = 0 } = {}) {
    this.#check(this.#x.cf_frame_rgbx(width, height, stride, bgr ? 1 : 0, id, timestampNs));
  }

  /**
   * 4:2:0 planes, already in slot 0, become the current RGB8 frame. `layout` is
   * copyTo's PlaneLayout list: Y, U, V (I420, I420A) or Y, UV (NV12).
   */
  frameYuv(format, width, height, layout, { matrix = 1, fullRange = false, id = 0, timestampNs = 0 } = {}) {
    const nv12 = format === 'NV12';
    const [y, u, v = { offset: 0, stride: 0 }] = layout;
    this.#check(this.#x.cf_frame_yuv(
      nv12 ? 1 : 0, width, height,
      y.offset, y.stride, u.offset, u.stride,
      nv12 ? 0 : v.offset, nv12 ? 0 : v.stride,
      matrix, fullRange ? 1 : 0, id, timestampNs
    ));
  }

  /** `bytes` (or slot 0's first `len` bytes when bytes is null) become the current frame as PFNC `pixelFormat`. */
  frameRaw(pixelFormat, width, height, bytes, { id = 0, timestampNs = 0, len = 0 } = {}) {
    if (bytes) {
      const data = bytesOf(bytes);
      this.buffer(FRAME_SLOT, data.length).set(data);
      len = data.length;
    }
    this.#check(this.#x.cf_frame_raw(pixelFormat >>> 0, width, height, len, id, timestampNs));
  }

  /** The current frame's shape, or null before the first. */
  frameInfo() {
    const width = this.#x.cf_frame_width() >>> 0;
    if (!width) return null;
    const pixelFormatCode = this.#x.cf_frame_format() >>> 0;
    return {
      id: this.#x.cf_frame_id(),
      width,
      height: this.#x.cf_frame_height() >>> 0,
      pixelFormatCode,
      pixelFormat: this.pixelFormatLabel(pixelFormatCode),
      timestampNs: this.#x.cf_frame_timestamp(),
      bytes: this.#x.cf_frame_bytes() >>> 0
    };
  }

  /** 'Mono8', 'RGB8', … or '0x…' for codes without a name. */
  pixelFormatLabel(code) {
    code >>>= 0;
    let label = this.#labels.get(code);
    if (label === undefined) {
      this.#x.cf_pixel_format_label(code);
      label = this.#text();
      this.#labels.set(code, label);
    }
    return label;
  }

  convertible(code) {
    return this.#x.cf_convertible(code >>> 0) === 1;
  }

  // -- Measurements -----------------------------------------------------------

  /** inspect::measure of a pixel region: { exposure, auto_meter, focus, notes }. */
  measure(kind, [x, y, width, height]) {
    this.#check(this.#x.cf_measure(kind, x, y, width, height));
    return this.#json();
  }

  /** inspect::inspect of a normalized region (null: the whole frame), without a preview image. */
  inspect(kind, region) {
    const [x, y, width, height] = region ?? [0, 0, 1, 1];
    this.#check(this.#x.cf_inspect(kind, region ? 0 : 1, x, y, width, height));
    return this.#json();
  }

  /**
   * frame::exposure of the whole frame (region null) or of a pixel region's crop:
   * f32 values as Numbers, histograms as Uint32Array(64), colour null when mono.
   */
  scopes(region) {
    const [x, y, width, height] = region ?? [0, 0, 0, 0];
    this.#check(this.#x.cf_scopes(region ? 0 : 1, x, y, width, height));
    const out = this.#out();
    const view = new DataView(out.buffer, out.byteOffset, out.byteLength);
    const bins = (at) => {
      const values = new Uint32Array(64);
      for (let i = 0; i < 64; i++) values[i] = view.getUint32(at + 4 * i, true);
      return values;
    };
    const mono = view.getUint32(4, true) === 1;
    return {
      samples: view.getUint32(0, true),
      mono,
      mean: view.getFloat32(8, true),
      shadows: view.getFloat32(12, true),
      highlights: view.getFloat32(16, true),
      luma: bins(20),
      red: mono ? null : bins(276),
      green: mono ? null : bins(532),
      blue: mono ? null : bins(788)
    };
  }

  /** frame::sharpness of a pixel region of the full frame; throws when the frame is too small. */
  focus([x, y, width, height]) {
    this.#check(this.#x.cf_focus(x, y, width, height));
    const out = this.#out();
    const view = new DataView(out.buffer, out.byteOffset, out.byteLength);
    return {
      laplacian: view.getFloat32(0, true),
      tenengrad: view.getFloat32(4, true),
      brenner: view.getFloat32(8, true),
      variance: view.getFloat32(12, true)
    };
  }

  /** inspect::region_pixels: a normalized region in pixels of a width × height frame. */
  regionPixels([x, y, w, h], width, height) {
    this.#check(this.#x.cf_region_pixels(x, y, w, h, width, height));
    const out = this.#out();
    const view = new DataView(out.buffer, out.byteOffset, out.byteLength);
    return [0, 4, 8, 12].map((at) => view.getUint32(at, true));
  }

  /** Throws the desktop's message for a region outside 0..1. */
  validateRegion([x, y, w, h]) {
    this.#check(this.#x.cf_validate_region(x, y, w, h));
  }

  // -- Conversions and encoders -----------------------------------------------

  /** The current frame as 'rgb', 'gray8' (null when not monochrome) or 'rgba' bytes (a copy). */
  convert(what) {
    const status = this.#check(this.#x.cf_convert(CONVERT[what] ?? 2));
    return status === 1 ? null : this.#bytes();
  }

  /** The current frame as RGBA, as a view valid only until the next call. */
  rgbaView() {
    this.#check(this.#x.cf_convert(CONVERT.rgba));
    return this.#out();
  }

  /** A 'raw', 'ppm' or 'pgm' file of the current frame. */
  encode(format) {
    this.#check(this.#x.cf_encode(ENCODE[format] ?? 99));
    return this.#bytes();
  }

  /** Throws when provenance JSON of this many UTF-8 bytes is too large for a still. */
  checkMeta(byteLength) {
    this.#check(this.#x.cf_check_meta(byteLength));
  }

  /** Paeth-filtered PNG scanlines of the current frame and their colour type (0 gray, 2 RGB). */
  pngRows() {
    const colorType = this.#check(this.#x.cf_png_rows());
    return { colorType, rows: this.#bytes() };
  }

  /** A PNG file from a zlib stream of pngRows(), with provenance and context text chunks. */
  pngAssemble(width, height, colorType, zlib, meta = null, description = null) {
    const [zlibLen, metaLen, descriptionLen] = this.#arguments(zlib, meta ?? '', meta ? description ?? '' : '');
    this.#check(this.#x.cf_png_assemble(width, height, colorType, zlibLen, metaLen, descriptionLen));
    return this.#bytes();
  }

  /** The JPEG with a COM segment holding `payload` after its APPn segments. */
  jpegCom(jpeg, payload) {
    const [jpegLen, payloadLen] = this.#arguments(jpeg, payload);
    this.#check(this.#x.cf_jpeg_com(jpegLen, payloadLen));
    return this.#bytes();
  }

  jpegWellFormed(jpeg, width, height) {
    const [len] = this.#arguments(jpeg);
    return this.#x.cf_jpeg_well_formed(len, width, height) === 1;
  }

  /** CRC-32 continued from `crc` (0 to start) over `bytes`. */
  crc32(crc, bytes) {
    const data = bytesOf(bytes);
    crc >>>= 0;
    for (let at = 0; at < data.length; at += CRC_CHUNK) {
      const piece = data.subarray(at, at + CRC_CHUNK);
      this.buffer(ARGUMENT_SLOT, piece.length).set(piece);
      crc = this.#x.cf_crc32(crc, piece.length) >>> 0;
    }
    return crc;
  }

  // -- Pattern camera ---------------------------------------------------------

  /** A fresh, stopped Pattern camera with default settings. */
  simReset() {
    this.#check(this.#x.cf_sim_reset());
  }

  simInfo() {
    this.#check(this.#x.cf_sim_info());
    return this.#json();
  }

  /** FeatureInfo[] byte-sorted by name, each with locked_while_streaming. */
  simFeatures() {
    this.#check(this.#x.cf_sim_features());
    return this.#json();
  }

  simGet(name) {
    const [nameLen] = this.#arguments(name);
    this.#check(this.#x.cf_sim_get(nameLen));
    return this.#json();
  }

  /** { name, value } with the value read back; throws the desktop's message. */
  simSet(name, text) {
    const [nameLen, valueLen] = this.#arguments(String(name), String(text));
    this.#check(this.#x.cf_sim_set(nameLen, valueLen));
    return this.#json();
  }

  simStart() {
    this.#check(this.#x.cf_sim_start());
  }

  simStop() {
    this.#check(this.#x.cf_sim_stop());
  }

  get simRunning() {
    return this.#x.cf_sim_running() === 1;
  }

  /** Seconds from one frame to the next: 1 / rate, or the exposure when longer. */
  get simPeriod() {
    return this.#x.cf_sim_period();
  }

  /** Render the next frame into the current frame. */
  simRender(timestampNs) {
    this.#check(this.#x.cf_sim_render(timestampNs));
  }
}

// -- Provenance JSON --------------------------------------------------------------

// serde_json orders object keys by their UTF-8 bytes, which is code point order.
const byCodePoint = (a, b) => {
  const left = a[Symbol.iterator]();
  const right = b[Symbol.iterator]();
  for (;;) {
    const x = left.next();
    const y = right.next();
    if (x.done || y.done) return x.done === y.done ? 0 : x.done ? -1 : 1;
    const difference = x.value.codePointAt(0) - y.value.codePointAt(0);
    if (difference) return difference;
  }
};

/** A deep copy with object keys sorted as the desktop's serde_json writes them. */
export const sortKeys = (value) => {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value && typeof value === 'object' && !(value instanceof Date)) {
    const sorted = {};
    for (const key of Object.keys(value).sort(byCodePoint)) {
      if (value[key] !== undefined) sorted[key] = sortKeys(value[key]);
    }
    return sorted;
  }
  return value;
};

/** Compact provenance JSON with sorted keys, as stills embed it. */
export const metaJson = (meta) => JSON.stringify(sortKeys(meta));

/** The context tags of provenance metadata as `key=value; …`, or null when there are none (frame.rs `description`). */
export const contextDescription = (meta) => {
  const context = meta?.context;
  if (!context || typeof context !== 'object' || Array.isArray(context)) return null;
  const tags = Object.keys(context)
    .sort(byCodePoint)
    .filter((key) => typeof context[key] === 'string')
    .map((key) => `${key}=${context[key]}`);
  return tags.length ? tags.join('; ') : null;
};

// -- ZIP ---------------------------------------------------------------------------

/** MS-DOS time and date fields for a local `date`, clamped to 1980–2107. */
export const dosDateTime = (date) => {
  const valid = date instanceof Date && !Number.isNaN(date.getTime()) ? date : new Date();
  const year = Math.min(2107, Math.max(1980, valid.getFullYear()));
  const clamped = year !== valid.getFullYear();
  const time = clamped ? 0 : (valid.getHours() << 11) | (valid.getMinutes() << 5) | (valid.getSeconds() >> 1);
  const day = clamped ? (year === 1980 ? (1 << 5) | 1 : (12 << 5) | 31) : ((valid.getMonth() + 1) << 5) | valid.getDate();
  return { time, date: ((year - 1980) << 9) | day };
};

const FOUR_GIB = 0xffffffff;

/**
 * The parts of a stored (uncompressed) ZIP archive: each entry's local header
 * followed by its data as given (a Blob or bytes), then the central directory.
 * Entries are { name, data, size, crc, date }; names are UTF-8 with '/' folders.
 * ZIP64 records are added only when an offset or size needs them.
 */
export const zipParts = (entries) => {
  const parts = [];
  const central = [];
  let offset = 0;
  for (const entry of entries) {
    const name = utf8(entry.name);
    if (!name.length || name.length > 0xffff) throw new Error(`cannot store ${entry.name} in a ZIP archive`);
    const size = Number(entry.size);
    const { time, date } = dosDateTime(entry.date);
    const large = size >= FOUR_GIB;
    const far = offset >= FOUR_GIB;
    const version = large || far ? 45 : 20;
    // Local header: ZIP64 sizes when the entry is large.
    const localExtra = large ? zip64Extra([size, size]) : new Uint8Array(0);
    const local = new Uint8Array(30 + name.length + localExtra.length);
    const lv = new DataView(local.buffer);
    lv.setUint32(0, 0x04034b50, true);
    lv.setUint16(4, version, true);
    lv.setUint16(6, 0x0800, true);
    lv.setUint16(8, 0, true);
    lv.setUint16(10, time, true);
    lv.setUint16(12, date, true);
    lv.setUint32(14, entry.crc >>> 0, true);
    lv.setUint32(18, large ? FOUR_GIB : size, true);
    lv.setUint32(22, large ? FOUR_GIB : size, true);
    lv.setUint16(26, name.length, true);
    lv.setUint16(28, localExtra.length, true);
    local.set(name, 30);
    local.set(localExtra, 30 + name.length);
    parts.push(local, entry.data);
    // Central directory record.
    const fields = [];
    if (large) fields.push(size, size);
    if (far) fields.push(offset);
    const extra = fields.length ? zip64Extra(fields) : new Uint8Array(0);
    const record = new Uint8Array(46 + name.length + extra.length);
    const cv = new DataView(record.buffer);
    cv.setUint32(0, 0x02014b50, true);
    cv.setUint16(4, version, true);
    cv.setUint16(6, version, true);
    cv.setUint16(8, 0x0800, true);
    cv.setUint16(10, 0, true);
    cv.setUint16(12, time, true);
    cv.setUint16(14, date, true);
    cv.setUint32(16, entry.crc >>> 0, true);
    cv.setUint32(20, large ? FOUR_GIB : size, true);
    cv.setUint32(24, large ? FOUR_GIB : size, true);
    cv.setUint16(28, name.length, true);
    cv.setUint16(30, extra.length, true);
    cv.setUint32(42, far ? FOUR_GIB : offset, true);
    record.set(name, 46);
    record.set(extra, 46 + name.length);
    central.push(record);
    offset += local.length + size;
  }
  const directorySize = central.reduce((sum, record) => sum + record.length, 0);
  const count = entries.length;
  const zip64 = count >= 0xffff || offset >= FOUR_GIB || directorySize >= FOUR_GIB;
  const tail = [];
  if (zip64) {
    const record = new Uint8Array(56);
    const rv = new DataView(record.buffer);
    rv.setUint32(0, 0x06064b50, true);
    rv.setBigUint64(4, 44n, true);
    rv.setUint16(12, 45, true);
    rv.setUint16(14, 45, true);
    rv.setBigUint64(24, BigInt(count), true);
    rv.setBigUint64(32, BigInt(count), true);
    rv.setBigUint64(40, BigInt(directorySize), true);
    rv.setBigUint64(48, BigInt(offset), true);
    const locator = new Uint8Array(20);
    const pv = new DataView(locator.buffer);
    pv.setUint32(0, 0x07064b50, true);
    pv.setBigUint64(8, BigInt(offset + directorySize), true);
    pv.setUint32(16, 1, true);
    tail.push(record, locator);
  }
  const end = new Uint8Array(22);
  const ev = new DataView(end.buffer);
  ev.setUint32(0, 0x06054b50, true);
  ev.setUint16(8, Math.min(count, 0xffff), true);
  ev.setUint16(10, Math.min(count, 0xffff), true);
  ev.setUint32(12, Math.min(directorySize, FOUR_GIB), true);
  ev.setUint32(16, Math.min(offset, FOUR_GIB), true);
  return [...parts, ...central, ...tail, end];
};

/** A ZIP64 extended information extra field holding 64-bit values. */
const zip64Extra = (values) => {
  const extra = new Uint8Array(4 + 8 * values.length);
  const view = new DataView(extra.buffer);
  view.setUint16(0, 0x0001, true);
  view.setUint16(2, 8 * values.length, true);
  values.forEach((value, i) => view.setBigUint64(4 + 8 * i, BigInt(value), true));
  return extra;
};
