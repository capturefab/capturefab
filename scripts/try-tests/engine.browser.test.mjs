// Runs web/try/engine.mjs in real Chrome with its fake camera: the module worker,
// the VideoFrame pixel paths, stills, the Pattern camera and the archive helpers.
// Skips unless playwright-core can be required (for example with NODE_PATH set)
// and Chrome is installed (CAPTUREFAB_CHROME, or the usual macOS and Linux paths).
// Run: NODE_PATH=/path/to/node_modules node --test scripts/try-tests/engine.browser.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const web = fileURLToPath(new URL('../../web/', import.meta.url));
const chrome = [
  process.env.CAPTUREFAB_CHROME,
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/usr/bin/google-chrome',
  '/usr/bin/chromium'
].find((path) => path && existsSync(path));
let playwright = null;
try {
  playwright = createRequire(import.meta.url)('playwright-core');
} catch {
  // Not installed: the test skips.
}
const skip = !playwright ? 'playwright-core is not installed' : !chrome ? 'Chrome is not installed' : false;

const TYPES = { '.html': 'text/html', '.mjs': 'text/javascript', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.wasm': 'application/wasm', '.svg': 'image/svg+xml' };

/** web/ over loopback; `wasmType` lets a test serve the module with the wrong MIME type. */
const serve = (wasmType) => new Promise((resolve) => {
  const server = createServer((request, response) => {
    const url = new URL(request.url, 'http://localhost');
    if (url.pathname === '/smoke.html') {
      response.writeHead(200, { 'content-type': 'text/html' });
      response.end('<!doctype html><html lang="en"><title>smoke</title><main></main></html>');
      return;
    }
    if (url.pathname === '/tier-worker.mjs') {
      // worker.mjs in a browser without RGBA copies (mode=yuv), without any
      // VideoFrame copies (mode=canvas), or one that ignores copyTo's `format`
      // as WebKit does (mode=webkit), to exercise the other pixel paths.
      response.writeHead(200, { 'content-type': 'text/javascript' });
      response.end(`
        const mode = new URL(import.meta.url).searchParams.get('mode');
        const { allocationSize, copyTo } = VideoFrame.prototype;
        const strip = (options) => options && { ...options, format: undefined, colorSpace: undefined };
        if (mode === 'webkit') {
          VideoFrame.prototype.allocationSize = function (options) {
            return allocationSize.call(this, strip(options));
          };
          VideoFrame.prototype.copyTo = function (destination, options) {
            return copyTo.call(this, destination, strip(options));
          };
        } else {
          VideoFrame.prototype.allocationSize = function (options) {
            if (options?.format) throw new TypeError('no RGBA copies');
            return allocationSize.call(this, options);
          };
          if (mode === 'canvas') VideoFrame.prototype.copyTo = () => Promise.reject(new TypeError('no copies'));
        }
        await import('/try/worker.mjs');
      `);
      return;
    }
    const file = normalize(join(web, decodeURIComponent(url.pathname)));
    if (!file.startsWith(web) || !existsSync(file)) {
      response.writeHead(404);
      response.end();
      return;
    }
    const type = extname(file) === '.wasm' ? wasmType : TYPES[extname(file)] ?? 'application/octet-stream';
    response.writeHead(200, { 'content-type': type });
    response.end(readFileSync(file));
  });
  server.listen(0, '127.0.0.1', () => resolve(server));
});

/** Everything below runs in the page. */
async function smoke() {
  const { Engine } = await import('/try/engine.mjs');
  const core = await import('/try/core.mjs');
  const out = {};
  /** The provenance JSON a PNG's capturefab iTXt chunk holds. */
  const pngMeta = (bytes) => {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    for (let at = 8; at + 12 <= bytes.length;) {
      const length = view.getUint32(at);
      const type = new TextDecoder('latin1').decode(bytes.subarray(at + 4, at + 8));
      const data = bytes.subarray(at + 8, at + 8 + length);
      const prefix = 'capturefab\0\0\0\0\0';
      if (type === 'iTXt' && new TextDecoder('latin1').decode(data.subarray(0, prefix.length)) === prefix) {
        return new TextDecoder().decode(data.subarray(prefix.length));
      }
      at += 12 + length;
    }
    return null;
  };
  const jpegMeta = (bytes) => {
    for (let i = 2; i < bytes.length - 1; i++) {
      if (bytes[i] === 0xff && bytes[i + 1] === 0xfe) {
        const length = (bytes[i + 2] << 8) | bytes[i + 3];
        return new TextDecoder().decode(bytes.subarray(i + 4, i + 2 + length));
      }
    }
    return null;
  };
  const engine = await Engine.create();
  out.info = engine.info;

  const stream = await navigator.mediaDevices.getUserMedia({ video: { width: { ideal: 640 }, height: { ideal: 480 } }, audio: false });
  const video = document.createElement('video');
  video.muted = true;
  video.playsInline = true;
  video.srcObject = stream;
  document.body.append(video);
  await video.play();
  await new Promise((resolve) => video.requestVideoFrameCallback(resolve));

  const frame = new VideoFrame(video);
  out.sourceFormat = frame.format;
  const analyzed = await engine.analyze(frame, { scopes: true, focus: [0, 0, 64, 64] });
  out.transferredClosed = frame.format === null && frame.codedWidth === 0;
  out.analyzed = { frame: analyzed.frame, samples: analyzed.scopes.samples, mono: analyzed.scopes.mono, lumaLength: analyzed.scopes.luma.length, redIsArray: analyzed.scopes.red instanceof Uint32Array, focusKeys: Object.keys(analyzed.focus ?? {}) };
  const region = await engine.analyze(null, { scopes: [10, 10, 32, 32], focus: [0, 0, 2, 2] });
  out.region = { samples: region.scopes.samples, focus: region.focus, sameId: region.frame.id === analyzed.frame.id };
  const inspected = await engine.inspect([0.25, 0.25, 0.5, 0.5]);
  out.inspected = { keys: Object.keys(inspected).sort(), region: inspected.region, frame: inspected.frame };
  out.badRegion = await engine.inspect([0.5, 0, 0.6, 1]).then(() => 'resolved', (error) => error.message);

  const meta = { width: 1, frame_id: 99, capturefab: 'browser', context: { station: '5', line: 'L1' }, camera: { vendor: 'Webcam', model: 'Fake' } };
  const png = await engine.capture('png', meta, new VideoFrame(video));
  const pngBytes = new Uint8Array(await png.blob.arrayBuffer());
  const pngImage = await createImageBitmap(png.blob);
  out.png = { mime: png.mime, ext: png.ext, size: [png.width, png.height], decoded: [pngImage.width, pngImage.height], pixelFormat: png.pixelFormat, code: png.pixelFormatCode, bytes: png.bytes, signature: Array.from(pngBytes.subarray(0, 8)), text: pngMeta(pngBytes), frameId: png.frameId, timestampNs: png.timestampNs };
  pngImage.close();
  const jpeg = await engine.capture('jpeg', meta, null);
  const jpegBytes = new Uint8Array(await jpeg.blob.arrayBuffer());
  const jpegImage = await createImageBitmap(jpeg.blob);
  out.jpeg = { mime: jpeg.mime, ext: jpeg.ext, decoded: [jpegImage.width, jpegImage.height], comment: JSON.parse(jpegMeta(jpegBytes)), soi: [jpegBytes[0], jpegBytes[1]] };
  jpegImage.close();
  const raw = await engine.capture('raw', null, null);
  out.raw = { size: raw.blob.size, bytes: raw.bytes, mime: raw.mime };
  out.ppmHeader = new TextDecoder().decode((await (await engine.capture('ppm', null, null)).blob.arrayBuffer()).slice(0, 15));
  out.pgm = await engine.capture('pgm', null, null).then(() => 'resolved', (error) => error.message);
  out.unknown = await engine.capture('tiff', null, null).then(() => 'resolved', (error) => error.message);

  // An ImageBitmap goes through the canvas.
  const bitmap = await createImageBitmap(video);
  const fromBitmap = await engine.analyze(bitmap, { scopes: true });
  out.bitmap = { path: fromBitmap.frame.path, size: [fromBitmap.frame.width, fromBitmap.frame.height], closed: bitmap.width === 0 };

  // The worker's other pixel paths measure the same picture alike.
  const tier = async (mode) => {
    const worker = new Worker(`/tier-worker.mjs?mode=${mode}`, { type: 'module' });
    const message = (filter) => new Promise((resolve) => {
      const listener = (event) => {
        if (filter(event.data)) {
          worker.removeEventListener('message', listener);
          resolve(event.data);
        }
      };
      worker.addEventListener('message', listener);
    });
    await message((data) => data.event === 'ready');
    const reference = new VideoFrame(video);
    const tierFrame = reference.clone();
    const replied = message((data) => data.id === 1);
    worker.postMessage({ id: 1, op: 'analyze', args: { frame: tierFrame, want: { scopes: true } } }, [tierFrame]);
    const result = (await replied).result;
    const expected = await engine.analyze(reference, { scopes: true });
    worker.terminate();
    return { path: result.frame.path, size: [result.frame.width, result.frame.height], mean: result.scopes.mean, expectedMean: expected.scopes.mean, expectedPath: expected.frame.path };
  };
  out.tiers = { yuv: await tier('yuv'), canvas: await tier('canvas') };

  // Four-byte frames keep their colours, also where copyTo ignores `format`.
  const bgra = () => new VideoFrame(new Uint8Array([0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255]), { format: 'BGRA', codedWidth: 2, codedHeight: 2, timestamp: 5 });
  const rawPixels = async (result) => Array.from(new Uint8Array(await result.blob.arrayBuffer()));
  const bgraRaw = await engine.capture('raw', null, bgra());
  out.bgra = { pixels: await rawPixels(bgraRaw), path: (await engine.analyze(null)).frame.path };
  const webkit = new Worker('/tier-worker.mjs?mode=webkit', { type: 'module' });
  await new Promise((resolve) => webkit.addEventListener('message', (event) => event.data.event === 'ready' && resolve(), { once: false }));
  const webkitFrame = bgra();
  const webkitReply = new Promise((resolve) => webkit.addEventListener('message', (event) => event.data.id === 1 && resolve(event.data)));
  webkit.postMessage({ id: 1, op: 'capture', args: { format: 'raw', frame: webkitFrame } }, [webkitFrame]);
  const webkitResult = await webkitReply;
  out.bgraWebkit = webkitResult.ok ? await rawPixels(webkitResult.result) : webkitResult.error;
  webkit.terminate();

  // The YUV path, run here on the main thread: Chrome's own RGBA conversion of
  // the same camera frame is the reference.
  const wasm = await core.instantiate(fetch('/try/capturefab.wasm'));
  const sample = new VideoFrame(video);
  const { width, height } = sample.visibleRect;
  const layout = await sample.copyTo(wasm.buffer(0, sample.allocationSize()));
  wasm.frameYuv(sample.format === 'NV12' ? 'NV12' : 'I420', width, height, layout, { matrix: core.matrixCode(sample.colorSpace), fullRange: sample.colorSpace.fullRange === true });
  const ours = wasm.convert('rgb');
  const theirs = new Uint8Array(width * height * 4);
  await sample.copyTo(theirs, { format: 'RGBA', colorSpace: 'srgb' });
  let total = 0;
  let close = 0;
  for (let i = 0, j = 0; i < ours.length; i += 3, j += 4) {
    for (let c = 0; c < 3; c++) {
      const difference = Math.abs(ours[i + c] - theirs[j + c]);
      total += difference;
      if (difference <= 6) close++;
    }
  }
  out.yuv = { format: sample.format, colorSpace: sample.colorSpace.toJSON(), meanDifference: total / ours.length, closeShare: close / ours.length };
  sample.close();

  // The Pattern camera.
  const opened = await engine.simOpen();
  out.simInfo = opened.info;
  out.simFeatureNames = opened.features.map((f) => f.name);
  const simFrames = [];
  const onFrame = (event) => {
    const { bitmap: image, ...detail } = event.detail;
    simFrames.push({ ...detail, bitmapSize: [image.width, image.height] });
    image.close();
  };
  engine.addEventListener('simframe', onFrame);
  await engine.simStart();
  await new Promise((resolve) => setTimeout(resolve, 400));
  out.simWhileRunning = await engine.simSet('Width', '800').then(() => 'resolved', (error) => error.message);
  out.simGain = await engine.simSet('Gain', '6');
  const simAnalyzed = await engine.analyze(null, { scopes: true });
  await engine.simStop();
  const stoppedAt = simFrames.length;
  await new Promise((resolve) => setTimeout(resolve, 150));
  out.sim = { count: stoppedAt, after: simFrames.length - stoppedAt, first: simFrames[0], ids: simFrames.map((f) => f.id), path: simAnalyzed.frame.path, pixelFormat: simAnalyzed.frame.pixelFormat, mono: simAnalyzed.scopes.mono };
  out.simMax = await engine.simSet('Width', '9999').then(() => 'resolved', (error) => error.message);
  out.simRgb = await engine.simSet('PixelFormat', 'RGB8');
  const features = await engine.simFeatures();
  out.simFeature = features.find((f) => f.name === 'PixelFormat');
  await engine.simStart();
  await new Promise((resolve) => setTimeout(resolve, 150));
  await engine.simStop();
  out.simLastFormat = simFrames.at(-1).pixelFormat;
  const simPng = await engine.capture('png', { capturefab: 'browser', frame_id: 0, timestamp_ns: 0 }, null);
  out.simPng = { pixelFormat: simPng.pixelFormat, size: [simPng.width, simPng.height], frameId: simPng.frameId, timestampNs: simPng.timestampNs, meta: JSON.parse(pngMeta(new Uint8Array(await simPng.blob.arrayBuffer()))) };

  // A paused Pattern camera keeps acquiring but renders nothing until resumed.
  await engine.simStart();
  await new Promise((resolve) => setTimeout(resolve, 150));
  out.pause = { paused: await engine.simPause(true) };
  const pausedAt = simFrames.length;
  const lastBeforePause = simFrames.at(-1).id;
  await new Promise((resolve) => setTimeout(resolve, 250));
  out.pause.during = simFrames.length - pausedAt;
  out.pause.locked = await engine.simSet('Width', '800').then(() => 'resolved', (error) => error.message);
  out.pause.resumed = await engine.simPause(false);
  await new Promise((resolve) => setTimeout(resolve, 200));
  out.pause.after = simFrames.length - pausedAt;
  out.pause.continued = simFrames[pausedAt]?.id === lastBeforePause + 1;
  await engine.simStop();
  out.pause.idle = await engine.simPause(false);
  engine.removeEventListener('simframe', onFrame);

  // A page that is slow to take pictures gets the newest ones; the frame clock and
  // ids keep going, so the gaps show as skipped frames.
  await engine.simSet('AcquisitionFrameRate', '120');
  const slow = [];
  const onSlowFrame = (event) => {
    slow.push(event.detail.id);
    event.detail.bitmap.close();
    const until = performance.now() + 40;
    while (performance.now() < until) {
      // Busy, as a page doing other work.
    }
  };
  engine.addEventListener('simframe', onSlowFrame);
  await engine.simStart();
  await new Promise((resolve) => setTimeout(resolve, 600));
  await engine.simStop();
  engine.removeEventListener('simframe', onSlowFrame);
  out.slow = { taken: slow.length, lastId: slow.at(-1), firstId: slow[0], increasing: slow.every((id, i) => i === 0 || id > slow[i - 1]) };

  out.crc = await engine.crc32(new Blob(['123456789']));
  const zip = await engine.zip([{ name: 'a/one.txt', blob: new Blob(['first']), date: new Date(2026, 9, 25, 14, 30, 12) }, { name: 'two.png', blob: png.blob, date: new Date() }]);
  const zipBytes = new Uint8Array(await zip.arrayBuffer());
  out.zip = { type: zip.type, size: zip.size, start: Array.from(zipBytes.subarray(0, 4)), endSignature: new DataView(zipBytes.buffer).getUint32(zipBytes.length - 22, true) };

  engine.terminate();
  const closing = new VideoFrame(video);
  out.afterTerminate = await engine.analyze(closing, { scopes: true }).then(() => 'resolved', (error) => error.message);
  out.closedAfterTerminate = closing.format === null;
  stream.getTracks().forEach((track) => track.stop());
  return out;
}

test('the engine runs in Chrome with a fake camera', { skip, timeout: 120_000 }, async () => {
  const server = await serve('application/wasm');
  const browser = await playwright.chromium.launch({ executablePath: chrome, args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream'] });
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.goto(`http://127.0.0.1:${server.address().port}/smoke.html`);
    const out = await page.evaluate(smoke);
    if (process.env.CAPTUREFAB_SMOKE_VERBOSE) console.log(JSON.stringify(out, null, 2));
    assert.deepEqual(errors, []);
    assert.equal(out.info.abi, 1);
    assert.match(out.info.version, /^\d+\.\d+\.\d+$/);
    assert.ok(out.transferredClosed, 'the page keeps no handle to a transferred frame');
    assert.equal(out.analyzed.frame.path, 'rgba');
    assert.equal(out.analyzed.frame.pixelFormat, 'RGB8');
    assert.equal(out.analyzed.frame.id, 1);
    assert.ok(out.analyzed.samples > 0 && !out.analyzed.mono && out.analyzed.lumaLength === 64 && out.analyzed.redIsArray);
    assert.deepEqual(out.analyzed.focusKeys, ['laplacian', 'tenengrad', 'brenner', 'variance']);
    assert.equal(out.region.samples, 32 * 32);
    assert.equal(out.region.focus.laplacian >= 0, true, 'a region at the edge is clamped, not refused');
    assert.ok(out.region.sameId);
    assert.deepEqual(out.inspected.keys, ['auto_meter', 'exposure', 'focus', 'frame', 'notes', 'region']);
    assert.deepEqual(out.inspected.region.normalized, [0.25, 0.25, 0.5, 0.5]);
    assert.equal(out.inspected.frame.pixel_format, 'RGB8');
    assert.equal(out.badRegion, 'region must be x, y, width, height inside 0..1 with positive width and height');
    assert.deepEqual(out.png.signature, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    assert.deepEqual(out.png.decoded, out.png.size);
    assert.equal(out.png.mime, 'image/png');
    assert.equal(out.png.code, 0x02180014);
    assert.equal(out.png.bytes, out.png.size[0] * out.png.size[1] * 3);
    assert.equal(
      out.png.text,
      `{"camera":{"model":"Fake","vendor":"Webcam"},"capturefab":"browser","context":{"line":"L1","station":"5"},"frame_id":${out.png.frameId},"height":${out.png.size[1]},"pixel_format":"RGB8","timestamp_ns":${out.png.timestampNs},"width":${out.png.size[0]}}`,
      'the provenance iTXt chunk holds sorted compact JSON, with the frame fields of the encoded frame'
    );
    assert.equal(out.jpeg.ext, 'jpg');
    assert.deepEqual(out.jpeg.soi, [0xff, 0xd8]);
    assert.deepEqual(out.jpeg.decoded, out.png.size);
    assert.equal(out.jpeg.comment.capturefab, 'browser');
    assert.equal(out.jpeg.comment.frame_id, out.png.frameId, 'the JPEG encodes the same current frame');
    assert.equal(out.raw.size, out.raw.bytes);
    assert.equal(out.raw.mime, 'application/octet-stream');
    assert.match(out.ppmHeader, /^P6\n\d+ \d+\n255\n/);
    assert.equal(out.pgm, 'PGM requires a monochrome image; use PNG or PPM');
    assert.equal(out.unknown, 'unknown image format tiff; use png, jpeg, raw, pgm or ppm');
    assert.equal(out.tiers.yuv.path, out.sourceFormat === 'NV12' ? 'nv12' : 'i420');
    assert.equal(out.tiers.canvas.path, 'canvas');
    for (const tier of Object.values(out.tiers)) {
      assert.deepEqual(tier.size, out.png.size);
      assert.equal(tier.expectedPath, 'rgba');
      assert.ok(Math.abs(tier.mean - tier.expectedMean) < 0.01, JSON.stringify(out.tiers));
    }
    assert.deepEqual(out.bgra.pixels, [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
    assert.equal(out.bgra.path, 'rgba');
    assert.deepEqual(out.bgraWebkit, out.bgra.pixels, 'BGRA stays BGRA where copyTo ignores format');
    assert.equal(out.bitmap.path, 'canvas');
    assert.ok(out.bitmap.closed);
    assert.ok(out.yuv.meanDifference < 3, JSON.stringify(out.yuv));
    assert.ok(out.yuv.closeShare > 0.95, JSON.stringify(out.yuv));
    assert.deepEqual(out.simInfo, { id: 'sim:0', vendor: 'Capturefab', model: 'Pattern camera', serial: 'SIM0', transport: 'simulator' });
    assert.equal(out.simFeatureNames.length, 9);
    assert.ok(out.sim.count >= 8 && out.sim.count <= 16, `${out.sim.count} frames in 0.4 s at 30 fps`);
    assert.equal(out.sim.after, 0, 'no frames after stop');
    assert.deepEqual(out.sim.ids, out.sim.ids.map((_, i) => i + 1));
    assert.deepEqual([out.sim.first.width, out.sim.first.height, out.sim.first.pixelFormat], [640, 480, 'Mono8']);
    assert.deepEqual(out.sim.first.bitmapSize, [640, 480]);
    assert.equal(out.sim.path, 'sim');
    assert.equal(out.sim.pixelFormat, 'Mono8');
    assert.ok(out.sim.mono);
    assert.equal(out.simWhileRunning, 'cannot set Width to "800": stop acquisition before changing image format');
    assert.deepEqual(out.simGain, { name: 'Gain', value: 6 });
    assert.equal(out.simMax, 'cannot set Width to "9999": Width violates Max=1920; enter 1920 or less');
    assert.deepEqual(out.simRgb, { name: 'PixelFormat', value: 'RGB8' });
    assert.equal(out.simFeature.value, 'RGB8');
    assert.equal(out.simFeature.locked_while_streaming, true);
    assert.equal(out.simLastFormat, 'RGB8');
    assert.equal(out.simPng.pixelFormat, 'RGB8');
    assert.deepEqual(out.simPng.size, [640, 480]);
    assert.ok(out.simPng.frameId > 0 && out.simPng.timestampNs > 0, JSON.stringify(out.simPng));
    assert.deepEqual(
      [out.simPng.meta.frame_id, out.simPng.meta.timestamp_ns, out.simPng.meta.width, out.simPng.meta.height, out.simPng.meta.pixel_format],
      [out.simPng.frameId, out.simPng.timestampNs, 640, 480, 'RGB8'],
      'embedded provenance describes the encoded Pattern camera frame'
    );
    assert.deepEqual(out.pause.paused, { paused: true });
    assert.equal(out.pause.during, 0, 'no frames while paused');
    assert.equal(out.pause.locked, 'cannot set Width to "800": stop acquisition before changing image format');
    assert.deepEqual(out.pause.resumed, { paused: false });
    assert.ok(out.pause.after > 0, JSON.stringify(out.pause));
    assert.ok(out.pause.continued, 'frame ids carry on after a resume');
    assert.deepEqual(out.pause.idle, { paused: false });
    assert.ok(out.slow.increasing);
    assert.ok(out.slow.taken < out.slow.lastId - out.slow.firstId, JSON.stringify(out.slow));
    assert.ok(out.slow.taken >= 5, JSON.stringify(out.slow));
    assert.equal(out.crc, 0xcbf43926);
    assert.equal(out.zip.type, 'application/zip');
    assert.deepEqual(out.zip.start, [0x50, 0x4b, 0x03, 0x04]);
    assert.equal(out.zip.endSignature, 0x06054b50);
    assert.equal(out.afterTerminate, 'The image engine stopped.');
    assert.ok(out.closedAfterTerminate, 'frames passed after terminate are closed');
  } finally {
    await browser.close();
    server.close();
  }
});

test('the engine loads a module served with the wrong MIME type', { skip, timeout: 60_000 }, async () => {
  const server = await serve('application/octet-stream');
  const browser = await playwright.chromium.launch({ executablePath: chrome });
  try {
    const page = await browser.newPage();
    await page.goto(`http://127.0.0.1:${server.address().port}/smoke.html`);
    const info = await page.evaluate(async () => {
      const { Engine } = await import('/try/engine.mjs');
      const engine = await Engine.create();
      const opened = await engine.simOpen();
      engine.terminate();
      return { info: engine.info, model: opened.info.model };
    });
    assert.deepEqual(info, { info: { abi: 1, version: info.info.version }, model: 'Pattern camera' });
  } finally {
    await browser.close();
    server.close();
  }
});

test('a missing module rejects create() with a readable message', { skip, timeout: 60_000 }, async () => {
  const server = await serve('application/wasm');
  const browser = await playwright.chromium.launch({ executablePath: chrome });
  try {
    const page = await browser.newPage();
    await page.route('**/capturefab.wasm', (route) => route.fulfill({ status: 404, body: '' }));
    await page.goto(`http://127.0.0.1:${server.address().port}/smoke.html`);
    const message = await page.evaluate(async () => {
      const { Engine } = await import('/try/engine.mjs');
      return Engine.create().then(() => 'resolved', (error) => error.message);
    });
    assert.equal(message, 'capturefab.wasm did not load (HTTP 404).');
  } finally {
    await browser.close();
    server.close();
  }
});
