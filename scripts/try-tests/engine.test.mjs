// Checks the shipped web/try/capturefab.wasm (or CAPTUREFAB_TRY_WASM) through
// web/try/core.mjs against web-try/tests/golden.json, which the main crate's
// tests/browser_golden.rs computes from the native code: measurements,
// conversions and encoders exactly, Pattern camera pixels within one level.
// Run: node --test scripts/try-tests/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  ABI, KIND, PIXEL_FORMAT, contextDescription, dosDateTime, instantiate, matrixCode, metaJson, sortKeys, utf8, zipParts
} from '../../web/try/core.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const wasmPath = process.env.CAPTUREFAB_TRY_WASM ? resolve(process.env.CAPTUREFAB_TRY_WASM) : resolve(root, 'web/try/capturefab.wasm');
const golden = JSON.parse(readFileSync(resolve(root, 'web-try/tests/golden.json'), 'utf8'));
const core = await instantiate(readFileSync(wasmPath));

const fnv1a64 = (bytes) => {
  let hash = 0xcbf29ce484222325n;
  for (const byte of bytes) {
    hash ^= BigInt(byte);
    hash = (hash * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return hash.toString(16).padStart(16, '0');
};
const hex = (bytes) => Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
const unhex = (text) => Uint8Array.from(text.match(/../g) ?? [], (pair) => parseInt(pair, 16));

// -- tests/browser_golden.rs' frame generators ---------------------------------

const channels = (format) => (format === 0x02180014 || format === 0x02180015 ? 3 : 1);
const sampleBytes = (format) => {
  if ([0x01100003, 0x01100005, 0x01100007, 0x011000b8, 0x011000c7, 0x01100099].includes(format)) return 2;
  return format === 0x012000bf ? 4 : 1;
};
const fullScale = (format) => {
  if (format === 0x01100003) return 1023;
  if (format === 0x01100005 || format === 0x01100099) return 4095;
  if ([0x01100007, 0x011000b8, 0x011000c7].includes(format)) return 65535;
  return 255;
};

const generate = (generator, format, width, height) => {
  const c = channels(format);
  const bytes = sampleBytes(format);
  const len = width * height * c * bytes;
  let data;
  if (generator.kind === 'bytes') {
    data = unhex(generator.hex);
  } else if (generator.kind === 'noise') {
    data = new Uint8Array(len);
    let state = 0x2545f491;
    for (let i = 0; i < len; i++) {
      state = (state ^ (state << 13)) >>> 0;
      state = (state ^ (state >>> 17)) >>> 0;
      state = (state ^ (state << 5)) >>> 0;
      data[i] = state & 0xff;
    }
    if (bytes === 2) {
      const top = fullScale(format) >> 8;
      for (let i = 1; i < len; i += 2) data[i] &= top;
    }
  } else {
    const max = fullScale(format);
    data = new Uint8Array(len);
    const view = new DataView(data.buffer);
    let at = 0;
    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        for (let ch = 0; ch < c; ch++) {
          let value;
          switch (generator.kind) {
            case 'solid':
              value = generator.value[ch];
              break;
            case 'ramp': {
              let level = Math.floor((x * max) / Math.max(width - 1, 1));
              if (x % 8 < 2) level = Math.floor(level / 2);
              value = ch === 0 ? level : ch === 1 ? Math.floor((y * max) / Math.max(height - 1, 1)) : max - level;
              break;
            }
            case 'checker': {
              const on = (Math.floor(x / generator.cell) + Math.floor(y / generator.cell)) % 2 === 1;
              value = ch === 1 ? (on ? Math.floor(max / 2) : Math.floor(max / 4)) : on ? max : 0;
              break;
            }
            case 'stripes': {
              const on = Math.floor(x / generator.period) % 2 === 1;
              const v = on ? Math.floor((max * 200) / 255) : Math.floor((max * 40) / 255);
              value = ch === 2 ? Math.floor(v / 2) : v;
              break;
            }
            default:
              throw new Error(`unknown generator ${generator.kind}`);
          }
          if (bytes === 1) data[at] = value;
          else if (bytes === 2) view.setUint16(at, value, true);
          else view.setUint32(at, value, true);
          at += bytes;
        }
      }
    }
  }
  return generator.length !== undefined ? data.slice(0, generator.length) : data;
};

const load = (entry) => {
  const data = generate(entry.generator, entry.pixel_format, entry.width, entry.height);
  core.frameRaw(entry.pixel_format, entry.width, entry.height, data, { id: 7, timestampNs: 99 });
  return data;
};

const outcome = (run) => {
  try {
    const bytes = run();
    return bytes === null ? null : { fnv1a64: fnv1a64(bytes), len: bytes.length };
  } catch (error) {
    return { error: error.message };
  }
};

// -- Golden comparisons -------------------------------------------------------

test('the module reports its ABI and version', () => {
  assert.equal(core.abi, ABI);
  assert.match(core.version, /^\d+\.\d+\.\d+$/);
  if (!process.env.CAPTUREFAB_TRY_WASM) {
    const manifest = JSON.parse(readFileSync(resolve(root, 'web/try/manifest.json'), 'utf8'));
    assert.equal(manifest.abi, ABI);
  }
});

test('measurements match the native code exactly', () => {
  let checked = 0;
  for (const entry of golden.frames) {
    const data = load(entry);
    assert.equal(fnv1a64(data), entry.fnv1a64, `${entry.name}: input bytes`);
    const info = core.frameInfo();
    assert.deepEqual([info.width, info.height, info.pixelFormatCode, info.bytes, info.id], [entry.width, entry.height, entry.pixel_format, entry.len, 7]);
    for (const m of entry.measure) {
      assert.deepEqual(core.measure(KIND[m.kind], m.region), m.result, `${entry.name} measure ${m.region}`);
      checked++;
    }
    for (const i of entry.inspect) {
      if (i.error) {
        assert.throws(() => core.inspect(KIND.area_scan, i.region), { message: i.error }, `${entry.name} inspect ${i.region}`);
      } else {
        assert.deepEqual(core.inspect(KIND.area_scan, i.region), i.result, `${entry.name} inspect ${i.region}`);
      }
      checked++;
    }
  }
  assert.ok(checked > 300, `${checked} measurements`);
});

test('conversions and raw, PPM and PGM files match the native code', () => {
  for (const entry of golden.frames) {
    load(entry);
    const c = entry.conversions;
    assert.deepEqual(outcome(() => core.convert('rgb')), c.rgb, `${entry.name} rgb`);
    assert.deepEqual(outcome(() => core.convert('rgba')), c.rgba, `${entry.name} rgba`);
    assert.deepEqual(outcome(() => core.convert('gray8')), c.gray8, `${entry.name} gray8`);
    for (const format of ['raw', 'ppm', 'pgm']) {
      assert.deepEqual(outcome(() => core.encode(format)), c[format], `${entry.name} ${format}`);
    }
  }
});

const deflate = async (bytes, stream) => new Uint8Array(await new Response(new Blob([bytes]).stream().pipeThrough(stream)).arrayBuffer());

const pngChunks = (bytes) => {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const chunks = [];
  for (let at = 8; at < bytes.length;) {
    const length = view.getUint32(at);
    const type = String.fromCharCode(...bytes.subarray(at + 4, at + 8));
    const whole = bytes.subarray(at, at + 12 + length);
    chunks.push({ type, data: bytes.subarray(at + 8, at + 8 + length), crc: view.getUint32(at + 8 + length), whole });
    at += 12 + length;
  }
  return chunks;
};

const paeth = (a, b, c) => {
  const pa = Math.abs(b - c);
  const pb = Math.abs(a - c);
  const pc = Math.abs(a + b - 2 * c);
  return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
};

const unfilter = (rows, width, channelCount) => {
  const row = width * channelCount;
  const height = rows.length / (row + 1);
  const out = new Uint8Array(row * height);
  for (let y = 0; y < height; y++) {
    assert.equal(rows[y * (row + 1)], 4, 'Paeth filter');
    for (let i = 0; i < row; i++) {
      const a = i >= channelCount ? out[y * row + i - channelCount] : 0;
      const b = y > 0 ? out[(y - 1) * row + i] : 0;
      const c = y > 0 && i >= channelCount ? out[(y - 1) * row + i - channelCount] : 0;
      out[y * row + i] = (rows[y * (row + 1) + 1 + i] + paeth(a, b, c)) & 0xff;
    }
  }
  return out;
};

test('PNG stills carry the native header and text chunks around browser-compressed pixels', async () => {
  for (const entry of golden.png) {
    load(entry);
    const meta = entry.meta_json ?? (entry.meta_note_length ? `{"note":"${'x'.repeat(entry.meta_note_length)}"}` : null);
    const description = meta ? contextDescription(JSON.parse(meta)) : null;
    const { colorType, rows } = core.pngRows();
    const zlib = await deflate(rows, new CompressionStream('deflate'));
    if (entry.error) {
      assert.throws(() => core.pngAssemble(entry.width, entry.height, colorType, zlib, meta, description), { message: entry.error });
      assert.throws(() => core.checkMeta(utf8(meta).length), { message: entry.error });
      continue;
    }
    const png = core.pngAssemble(entry.width, entry.height, colorType, zlib, meta, description);
    assert.deepEqual([...png.subarray(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    const chunks = pngChunks(png);
    assert.deepEqual(
      chunks.map((chunk) => ({ type: chunk.type, hex: chunk.type === 'IDAT' ? null : hex(chunk.whole) })),
      entry.chunks
    );
    for (const chunk of chunks) {
      assert.equal(chunk.crc, core.crc32(core.crc32(0, utf8(chunk.type)), chunk.data), `${chunk.type} CRC`);
    }
    const idat = chunks.filter((chunk) => chunk.type === 'IDAT');
    const inflated = await deflate(idat.length === 1 ? idat[0].data : new Uint8Array(Buffer.concat(idat.map((c) => c.data))), new DecompressionStream('deflate'));
    const pixels = unfilter(inflated, entry.width, colorType === 0 ? 1 : 3);
    assert.equal(fnv1a64(pixels), entry.pixels_fnv1a64);
  }
});

test('provenance JSON sorts keys like serde_json and context tags become the Description', () => {
  const entry = golden.png.find((p) => p.meta_json?.includes('context'));
  const parsed = JSON.parse(entry.meta_json);
  // JavaScript has no 10000.0; everything else matches byte for byte.
  assert.equal(metaJson(parsed), entry.meta_json.replace(/(\d)\.0(?=[,}\]])/g, '$1'));
  assert.equal(contextDescription(parsed), 'line=L1; station=5');
  assert.equal(contextDescription({ context: { count: 3 } }), null);
  assert.equal(contextDescription({}), null);
  assert.deepEqual(Object.keys(sortKeys({ b: 1, a: { d: 1, c: 2 }, A: 0, é: 1, z: 2 })), ['A', 'a', 'b', 'z', 'é']);
  assert.equal(metaJson({ b: [{ y: 1, x: 2 }], a: null, skip: undefined }), '{"a":null,"b":[{"x":2,"y":1}]}');
});

test('JPEG comments follow APPn segments as the native insert_com places them', () => {
  for (const entry of golden.jpeg) {
    const run = () => core.jpegCom(unhex(entry.jpeg), unhex(entry.payload));
    if (entry.error) assert.throws(run, { message: entry.error });
    else assert.equal(hex(run()), entry.out);
  }
});

// A 2 × 2 baseline JPEG (APP0 JFIF, APP1 Exif, APP13) from macOS sips.
const TINY_JPEG = unhex(
  'ffd8ffe000104a46494600010100004800480000ffe1004c4578696600004d4d002a00000008000187690004000000010000001a000000000003a00100030000000100010000a00200040000000100000002a0030004000000010000000200000000ffed003850686f746f73686f7020332e30003842494d04040000000000003842494d0425000000000010d41d8cd98f00b204e9800998ecf8427effc00011080002000203012200021101031101ffc4001f0000010501010101010100000000000000000102030405060708090a0bffc400b5100002010303020403050504040000017d01020300041105122131410613516107227114328191a1082342b1c11552d1f02433627282090a161718191a25262728292a3435363738393a434445464748494a535455565758595a636465666768696a737475767778797a838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8f9faffc4001f0100030101010101010101010000000000000102030405060708090a0bffc400b51100020102040403040705040400010277000102031104052131061241510761711322328108144291a1b1c109233352f0156272d10a162434e125f11718191a262728292a35363738393a434445464748494a535455565758595a636465666768696a737475767778797a82838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae2e3e4e5e6e7e8e9eaf2f3f4f5f6f7f8f9faffdb0043000606060606060a06060a0e0a0a0a0e120e0e0e0e12171212121212171c1717171717171c1c1c1c1c1c1c1c22222222222227272727272c2c2c2c2c2c2c2c2c2cffdb0043010707070b0a0b130a0a132e1f1a1f2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2effdd00040001ffda000c03010002110311003f00a7a6bb9d3ad8924930a77ff6455ddcdea6a8699ff20db5ff00ae31ff00e822af575981ffd9'
);

test('a real JPEG keeps its headers and gains the provenance comment', () => {
  assert.ok(core.jpegWellFormed(TINY_JPEG, 2, 2));
  assert.ok(!core.jpegWellFormed(TINY_JPEG, 2, 3));
  const payload = utf8(metaJson({ capturefab: 'browser', camera: { model: 'Webcam', vendor: 'Webcam' }, frame_id: 1 }));
  const out = core.jpegCom(TINY_JPEG, payload);
  // SOI, APP0 (18), APP1 (78), APP13 (58), then the comment.
  const at = 2 + 18 + 78 + 58;
  assert.deepEqual([...out.subarray(at - 58, at - 56)], [0xff, 0xed]);
  assert.deepEqual([...out.subarray(at, at + 4)], [0xff, 0xfe, (payload.length + 2) >> 8, (payload.length + 2) & 0xff]);
  assert.equal(new TextDecoder().decode(out.subarray(at + 4, at + 4 + payload.length)), new TextDecoder().decode(payload));
  assert.deepEqual(out.subarray(at + 4 + payload.length), TINY_JPEG.subarray(at));
  assert.ok(core.jpegWellFormed(out, 2, 2));
  assert.throws(() => core.jpegCom(TINY_JPEG, new Uint8Array(65534)), { message: 'JPEG comment is too long' });
});

test('PPM and PGM headers and the format errors read like the desktop', () => {
  core.frameRaw(PIXEL_FORMAT.Mono8, 2, 1, Uint8Array.of(7, 9));
  assert.deepEqual(core.encode('pgm'), Uint8Array.of(...utf8('P5\n2 1\n255\n'), 7, 9));
  assert.deepEqual(core.encode('ppm'), Uint8Array.of(...utf8('P6\n2 1\n255\n'), 7, 7, 7, 9, 9, 9));
  assert.equal(core.pngRows().colorType, 0);
  core.frameRaw(PIXEL_FORMAT.RGB8, 1, 1, Uint8Array.of(1, 2, 3));
  assert.throws(() => core.encode('pgm'), { message: 'PGM requires a monochrome image; use PNG or PPM' });
  assert.equal(core.convert('gray8'), null);
  assert.equal(core.pngRows().colorType, 2);
  assert.throws(() => core.checkMeta(16 * 1024 + 1), { message: 'capture metadata exceeds 16384 bytes' });
  core.checkMeta(16 * 1024);
});

test('CRC-32 matches known vectors and streams across pieces', () => {
  assert.equal(core.crc32(0, new Uint8Array(0)), 0);
  assert.equal(core.crc32(0, utf8('123456789')), 0xcbf43926);
  assert.equal(core.crc32(0, utf8('The quick brown fox jumps over the lazy dog')), 0x414fa339);
  assert.equal(core.crc32(0, utf8('IEND')), 0xae426082);
  const big = new Uint8Array(3 * (1 << 20) + 17).map((_, i) => (i * 7 + 3) & 0xff);
  const whole = core.crc32(0, big);
  assert.equal(core.crc32(core.crc32(0, big.subarray(0, 1000)), big.subarray(1000)), whole);
  // An independent bitwise CRC of the large input.
  let crc = 0xffffffff;
  for (const byte of big) {
    crc ^= byte;
    for (let k = 0; k < 8; k++) crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
  }
  assert.equal(whole, (crc ^ 0xffffffff) >>> 0);
});

const concat = (parts) => {
  const out = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
};

test('ZIP archives are stored with UTF-8 names, CRCs and a central directory', () => {
  const date = new Date(2026, 9, 25, 14, 30, 12);
  const files = [
    { name: '2026-10-25/Pattern-camera-SIM0-20261025-143012-0001.png', data: utf8('first file') },
    { name: 'Kamera-ü/notes.txt', data: new Uint8Array(0) }
  ];
  const zip = concat(zipParts(files.map((f) => ({ ...f, size: f.data.length, crc: core.crc32(0, f.data), date }))));
  const view = new DataView(zip.buffer);
  const end = zip.length - 22;
  assert.equal(view.getUint32(end, true), 0x06054b50);
  assert.equal(view.getUint16(end + 10, true), 2);
  const directorySize = view.getUint32(end + 12, true);
  const directory = view.getUint32(end + 16, true);
  assert.equal(directory + directorySize, end);
  let at = directory;
  const { time, date: day } = dosDateTime(date);
  assert.equal(time, (14 << 11) | (30 << 5) | 6);
  assert.equal(day, ((2026 - 1980) << 9) | (10 << 5) | 25);
  for (const file of files) {
    assert.equal(view.getUint32(at, true), 0x02014b50);
    assert.equal(view.getUint16(at + 8, true), 0x0800, 'UTF-8 names');
    assert.equal(view.getUint16(at + 10, true), 0, 'stored');
    assert.equal(view.getUint32(at + 16, true), core.crc32(0, file.data));
    assert.equal(view.getUint32(at + 20, true), file.data.length);
    const nameLength = view.getUint16(at + 28, true);
    assert.equal(new TextDecoder().decode(zip.subarray(at + 46, at + 46 + nameLength)), file.name);
    const local = view.getUint32(at + 42, true);
    assert.equal(view.getUint32(local, true), 0x04034b50);
    assert.equal(view.getUint32(local + 14, true), core.crc32(0, file.data));
    const dataAt = local + 30 + view.getUint16(local + 26, true) + view.getUint16(local + 28, true);
    assert.deepEqual(zip.subarray(dataAt, dataAt + file.data.length), file.data);
    at += 46 + nameLength + view.getUint16(at + 30, true);
  }
  // Entries past 4 GiB carry ZIP64 sizes.
  const parts = zipParts([{ name: 'huge.mp4', data: new Uint8Array(0), size: 5 * 2 ** 30, crc: 1, date }]);
  const local = new DataView(parts[0].buffer);
  assert.equal(local.getUint32(18, true), 0xffffffff);
  assert.equal(local.getUint16(28, true), 20);
  assert.equal(local.getBigUint64(30 + 8 + 4, true), 5n * 2n ** 30n);
  assert.equal(new DataView(parts.at(-3).buffer).getUint32(0, true), 0x06064b50);
  assert.deepEqual(dosDateTime(new Date(1970, 0, 1)), { time: 0, date: (1 << 5) | 1 });
});

test('YUV and RGBA video frames become RGB8 like the formulas', () => {
  // 2 × 2 I420 in BT.709 limited range: Y plane, U, V.
  core.buffer(0, 6).set([63, 173, 32, 126, 102, 240]);
  core.frameYuv('I420', 2, 1, [{ offset: 0, stride: 2 }, { offset: 4, stride: 1 }, { offset: 5, stride: 1 }], { matrix: matrixCode({ matrix: 'bt709' }) });
  assert.deepEqual([...core.convert('rgb')], [255, 1, 0, 255, 129, 128]);
  // The same pixels as NV12, and BT.601 for an SDTV colour space.
  core.buffer(0, 4).set([63, 173, 102, 240]);
  core.frameYuv('NV12', 2, 1, [{ offset: 0, stride: 2 }, { offset: 2, stride: 2 }], { matrix: matrixCode({ matrix: 'smpte170m' }) });
  assert.deepEqual([...core.convert('rgb')].slice(0, 3), [233, 0, 2]);
  assert.equal(matrixCode(null), 1);
  assert.equal(matrixCode({ matrix: 'bt2020-ncl' }), 2);
  core.buffer(0, 12).set([1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0]);
  core.frameRgbx(2, 1, 12, { bgr: true, id: 4, timestampNs: 5e9 });
  assert.deepEqual([...core.convert('rgb')], [3, 2, 1, 6, 5, 4]);
  const info = core.frameInfo();
  assert.deepEqual([info.pixelFormat, info.id, info.timestampNs], ['RGB8', 4, 5e9]);
  core.buffer(0, 4);
  assert.throws(() => core.frameRgbx(2, 1, 8), { message: 'the video frame\'s planes do not fit its copied pixels' });
});

test('regions map to pixels and bad regions read like the desktop', () => {
  assert.deepEqual(core.regionPixels([0.25, 0.25, 0.5, 0.5], 640, 480), [160, 120, 320, 240]);
  assert.deepEqual(core.regionPixels([1, 1, 1e-9, 1e-9], 640, 480), [639, 479, 1, 1]);
  core.validateRegion([0.7, 0.7, 0.3, 0.3]);
  assert.throws(() => core.validateRegion([0.5, 0, 0.6, 1]), { message: 'region must be x, y, width, height inside 0..1 with positive width and height' });
  assert.equal(core.pixelFormatLabel(0x01100099), '0x01100099');
  assert.ok(core.convertible(PIXEL_FORMAT.BayerRG8));
  assert.ok(!core.convertible(0x012000bf));
});

test('scopes and focus read the f32 measurements the histogram and focus cards use', () => {
  const entry = golden.frames.find((f) => f.name === 'RGB8-ramp-64x32');
  load(entry);
  const whole = core.scopes(null);
  const measured = entry.measure.find((m) => m.region[2] === entry.width).result;
  assert.equal(whole.samples, 64 * 32);
  assert.equal(whole.mono, false);
  assert.equal(Math.round(whole.mean * 1e4) / 1e4, measured.exposure.mean);
  assert.deepEqual(Array.from({ length: 16 }, (_, i) => whole.luma.slice(4 * i, 4 * i + 4).reduce((a, b) => a + b, 0)), measured.exposure.histogram16);
  assert.equal(whole.red.length, 64);
  const focus = core.focus([0, 0, 64, 32]);
  assert.equal(Math.round(focus.tenengrad * 1e4) / 1e4, measured.focus.tenengrad);
  const region = core.scopes([16, 8, 8, 8]);
  assert.equal(region.samples, 64);
  core.frameRaw(PIXEL_FORMAT.Mono8, 2, 1, Uint8Array.of(0, 255));
  assert.equal(core.scopes(null).red, null);
  assert.throws(() => core.focus([0, 0, 2, 1]), { message: 'frame is too small to measure sharpness' });
});

// -- The Pattern camera ---------------------------------------------------------

const withoutLocks = (features) => features.map(({ locked_while_streaming: locked, ...feature }) => {
  assert.equal(locked, ['Width', 'Height', 'PixelFormat'].includes(feature.name));
  return feature;
});

test('Pattern camera features and writes match the desktop, messages included', () => {
  core.simReset();
  assert.deepEqual(core.simInfo(), golden.sim.info);
  assert.deepEqual(withoutLocks(core.simFeatures()), golden.sim.features);
  for (const op of golden.sim.ops) {
    if (op.op === 'start') core.simStart();
    else if (op.op === 'stop') core.simStop();
    else if (op.op === 'features') assert.deepEqual(withoutLocks(core.simFeatures()), op.result);
    else if (op.error) assert.throws(() => core.simSet(op.name, op.value), { message: op.error }, `${op.name} ${op.value}`);
    else assert.deepEqual(core.simSet(op.name, op.value), op.result, `${op.name} ${op.value}`);
  }
  core.simReset();
  assert.equal(core.simRunning, false);
  assert.equal(core.simGet('Width'), 640);
  assert.throws(() => core.simRender(0), { message: 'camera is not streaming' });
});

test('Pattern camera frames match the native simulator within one level', () => {
  let samples = 0;
  for (const run of golden.sim.runs) {
    core.simReset();
    for (const [name, value] of run.settings) core.simSet(name, value);
    core.simStart();
    assert.equal(core.simPeriod, Math.max(1 / 120, core.simGet('ExposureTime') / 1e6));
    for (let index = 0; index <= 10; index++) {
      core.simRender(index * 1e6);
      const expected = run.frames.find((f) => f.index === index);
      if (!expected) continue;
      const info = core.frameInfo();
      assert.deepEqual([info.id, info.width, info.height, info.pixelFormatCode, info.bytes, info.timestampNs], [expected.id, expected.width, expected.height, expected.pixel_format, expected.len, index * 1e6]);
      const raw = core.encode('raw');
      const mono12 = info.pixelFormatCode === PIXEL_FORMAT.Mono12;
      const bpp = raw.length / (info.width * info.height);
      for (const [x, y, want] of expected.samples) {
        const at = (y * info.width + x) * bpp;
        const got = mono12 ? [(raw[at] | (raw[at + 1] << 8)) >> 4, raw[at] & 15] : [...raw.subarray(at, at + bpp)];
        got.forEach((value, i) => assert.ok(Math.abs(value - want[i]) <= 1, `(${x}, ${y}) frame ${index}: ${value} vs ${want[i]}`));
        samples += got.length;
      }
      let sum = 0;
      let count = 0;
      if (mono12) {
        for (let i = 0; i < raw.length; i += 2) {
          sum += (raw[i] | (raw[i + 1] << 8)) >> 4;
          count++;
        }
      } else {
        for (const byte of raw) sum += byte;
        count = raw.length;
      }
      assert.equal(count, expected.level_count);
      assert.ok(Math.abs(sum - expected.level_sum) <= Math.floor(expected.level_sum / 256) + 1, `${sum} vs ${expected.level_sum}`);
    }
    core.simStop();
  }
  assert.ok(samples > 1000);
});
