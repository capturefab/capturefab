// File names and the saved-files library of web/try/library.mjs: the desktop's
// naming.rs tests and storage limits, run against the in-memory store ("This tab").
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  BUDGET_BYTES, BUDGET_FILES, Library, NAMINGS, buildName, cameraLabel, formatExtension, highestSeq, humanBytes,
  limitBytes, namePlan, naming, preview, roundedBytes, seqOf, stamps, tokens, typeOf
} from '../../web/try/library.mjs';

// 2026-10-17 10:15:00 local time, as naming.rs's tests use.
const FIXED = new Date(2026, 9, 17, 10, 15, 0);
const PLACEHOLDERS = 'capture output supports only the {camera}, {date}, {time}, {datetime}, {seq}, {frame} and {tag:KEY} placeholders';

test('camera labels are safe file names (naming.rs camera_labels_are_safe)', () => {
  assert.equal(cameraLabel('Sim Cam/2', 'SIM0'), 'Sim-Cam-2-SIM0');
  assert.equal(cameraLabel('..', ''), 'camera');
  assert.equal(cameraLabel('con', ''), 'camera');
  assert.equal(cameraLabel('COM1', ''), 'camera');
  assert.equal(cameraLabel('COM0', ''), 'COM0');
  assert.equal(cameraLabel('LPT9', 'x'), 'LPT9-x');
  assert.equal(cameraLabel('nul.txt', ''), 'camera');
  const long = cameraLabel('é@:/'.repeat(40), 'x'.repeat(100));
  assert.ok(long.length <= 64 && !/[@:/]/.test(long), long);
  assert.equal(long, 'x'.repeat(64));
  assert.equal(cameraLabel('Pattern camera', 'SIM0'), 'Pattern-camera-SIM0');
  assert.equal(cameraLabel('FaceTime HD Camera', '3f9a12bc'), 'FaceTime-HD-Camera-3f9a12bc');
  assert.equal(cameraLabel('Logi C920 (046d:082d)', 'ab12cd34'), 'Logi-C920-046d-082d-ab12cd34');
  assert.equal(cameraLabel('Cam 😀 HD', 'S1'), 'Cam-HD-S1');
  assert.equal(cameraLabel(`${'x'.repeat(63)}.y`, 'z'), 'x'.repeat(63));
  assert.equal(cameraLabel('', ''), 'camera');
});

test('templates expand and preview {seq} as #### (naming.rs expansion_and_preview)', () => {
  assert.deepEqual(stamps(FIXED), { date: '2026-10-17', time: '101500', datetime: '20261017-101500' });
  assert.equal(preview('{date}/{camera}-{datetime}-{seq}.png', 'A-1', FIXED), '2026-10-17/A-1-20261017-101500-####.png');
  assert.equal(preview('t{time}-{frame}', 'c', FIXED), 't101500-000001');
  assert.equal(preview('{camera}-{seq}.png', 'A', FIXED, 13), 'A-0013.png');
  assert.equal(preview('{camera}-{seq}.png', 'A', FIXED), 'A-####.png');
  assert.equal(preview('shots/{tag:label}/{camera}-{seq}.png', 'c', FIXED), 'shots/{tag:label}/c-####.png');
  for (const bad of ['{unknown}.png', 'a{b.png', 'a}b.png', '{Camera}.png', '{tag}/x.png', '{tag:Bad}/x.png']) {
    assert.throws(() => tokens(bad), { message: PLACEHOLDERS }, bad);
  }
  assert.equal(formatExtension('jpeg'), 'jpg');
  assert.equal(formatExtension('pgm'), 'pgm');
});

test('the presets are the desktop file-name choices', () => {
  assert.deepEqual(NAMINGS.map((n) => n.label), ['By date, camera and time (recommended)', 'Camera and time', 'Numbered (capture-1, capture-2…)']);
  assert.equal(naming('dated').template('png'), '{date}/{camera}-{datetime}-{seq}.png');
  assert.equal(naming('camera-time').template('jpg'), '{camera}-{datetime}-{seq}.jpg');
  assert.equal(naming('numbered').template('mp4'), 'capture-{seq}.mp4');
  assert.equal(naming('unknown').id, 'dated');
});

test('{seq} continues per folder and pattern (naming.rs seq_continues_per_folder_and_pattern)', () => {
  const plan = namePlan('{camera}-{datetime}-{seq}.png', { camera: 'A', now: FIXED });
  assert.deepEqual(plan, { dir: '', parts: [{ lit: 'A-' }, { stamp: '20261017-101500' }, { lit: '-' }, { seq: true }, { lit: '.png' }] });
  const names = ['A-20261017-101500-0007.png', 'A-20261017-101501-0012.png', 'B-20261017-101500-0099.png', 'A-notastamp-0500.png'];
  assert.equal(highestSeq(names, plan.parts), 12);
  assert.equal(buildName(plan.parts, 13), 'A-20261017-101500-0013.png');
  assert.equal(buildName(plan.parts, 123456), 'A-20261017-101500-123456.png', 'four digits at least');
  assert.equal(seqOf('A-20261017-101500-0007.jpg', plan.parts), null);
  assert.equal(seqOf('A-20261017-101500-.png', plan.parts), null);
  const dated = namePlan('{date}/{camera}-{datetime}-{seq}.png', { camera: 'A', now: FIXED });
  assert.equal(dated.dir, '2026-10-17');
  assert.deepEqual(dated.parts, plan.parts);
  assert.deepEqual(namePlan('capture-{seq}.png', { camera: 'A', now: FIXED }).parts, [{ lit: 'capture-' }, { seq: true }, { lit: '.png' }]);
  assert.deepEqual(namePlan('{camera}.png', { camera: 'A', now: FIXED }), { fixed: 'A.png' });
});

test('sizes read as the desktop writes them', () => {
  assert.equal(humanBytes(512), '512 B');
  assert.equal(humanBytes(2048), '2 KiB');
  assert.equal(humanBytes(1.5 * 2 ** 20), '1.5 MiB');
  assert.equal(humanBytes(10 * 2 ** 30), '10 GiB');
  assert.equal(humanBytes(284 * 1024 + 300), '284 KiB');
  assert.equal(limitBytes(BUDGET_BYTES), '2 GiB');
  assert.equal(limitBytes(1536 * 2 ** 20), '1,536 MiB');
  assert.equal(limitBytes(1000), '1,000 bytes');
  assert.equal(roundedBytes(8.5 * 2 ** 30), '8.5 GiB');
  assert.equal(roundedBytes(2 ** 20), '1.0 MiB');
  assert.equal(roundedBytes(1000), '1,000 bytes');
  assert.equal(BUDGET_FILES, 1000);
  assert.equal(typeOf('a/b.JPG'), 'image/jpeg');
  assert.equal(typeOf('clip.webm'), 'video/webm');
  assert.equal(typeOf('frame.raw'), 'application/octet-stream');
});

const blob = (size, type = 'image/png') => new Blob([new Uint8Array(size)], { type });

test('saves never overwrite and take the next free number', async () => {
  const library = new Library();
  assert.equal(library.kind, 'memory');
  assert.equal(library.place, 'This tab');
  let changes = 0;
  library.addEventListener('change', () => { changes += 1; });
  const label = cameraLabel('Pattern camera', 'SIM0');
  const first = await library.save(blob(10), { ext: 'png', label, date: FIXED });
  assert.deepEqual({ ...first, created: 0 }, {
    name: 'Pattern-camera-SIM0-20261017-101500-0001.png',
    path: '2026-10-17/Pattern-camera-SIM0-20261017-101500-0001.png',
    location: 'Capturefab/2026-10-17/Pattern-camera-SIM0-20261017-101500-0001.png',
    size: 10,
    type: 'image/png',
    created: 0
  });
  // Saves at once are serialised, so each gets its own number, even a second later.
  const later = new Date(FIXED.getTime() + 1000);
  const saved = await Promise.all([1, 2, 3].map((n) => library.save(blob(n), { ext: 'png', label, date: n === 3 ? later : FIXED })));
  assert.deepEqual(saved.map((r) => r.name), [
    'Pattern-camera-SIM0-20261017-101500-0002.png',
    'Pattern-camera-SIM0-20261017-101500-0003.png',
    'Pattern-camera-SIM0-20261017-101501-0004.png'
  ]);
  // Another format, day, preset or camera numbers on its own.
  assert.equal((await library.save(blob(1, 'image/jpeg'), { ext: 'jpg', label, date: FIXED })).name, 'Pattern-camera-SIM0-20261017-101500-0001.jpg');
  assert.equal((await library.save(blob(1), { ext: 'png', label, date: new Date(2026, 9, 18, 9, 0, 0) })).path, '2026-10-18/Pattern-camera-SIM0-20261018-090000-0001.png');
  assert.equal((await library.save(blob(1), { ext: 'png', label, date: FIXED, naming: 'numbered' })).path, 'capture-0001.png');
  assert.equal((await library.save(blob(1), { ext: 'png', label, date: FIXED, naming: 'numbered' })).path, 'capture-0002.png');
  assert.equal((await library.save(blob(1, 'video/webm'), { ext: 'webm', label: 'cam', date: FIXED, naming: 'camera-time' })).path, 'cam-20261017-101500-0001.webm');
  assert.equal(changes, 9);
  const listed = await library.list();
  assert.equal(listed.length, 9);
  assert.ok(listed.every((r, i) => i === 0 || r.created <= listed[i - 1].created), 'newest first');
  const file = await library.file(first.path);
  assert.equal(file.name, first.name);
  assert.equal(file.size, 10);
  // Deleting a file frees its name for nothing: numbering goes on from the highest.
  await library.remove(saved[2].path);
  assert.equal((await library.save(blob(1), { ext: 'png', label, date: FIXED })).name, 'Pattern-camera-SIM0-20261017-101500-0004.png');
  await assert.rejects(library.file('missing.png'), { message: 'missing.png is no longer saved' });
});

test('saves stop at the limits with the desktop refusal', async () => {
  const library = new Library({ budgetBytes: 100, budgetFiles: 3 });
  await library.save(blob(40), { ext: 'png', label: 'c', date: FIXED });
  await library.save(blob(40), { ext: 'png', label: 'c', date: FIXED });
  await assert.rejects(library.save(blob(30), { ext: 'png', label: 'c', date: FIXED }), {
    message: 'Storage full: 80 B allocated and 2 retained files (limits: 100 B, 3 files); delete files to continue'
  });
  await library.save(blob(20), { ext: 'png', label: 'c', date: FIXED });
  await assert.rejects(library.save(blob(0), { ext: 'png', label: 'c', date: FIXED }), {
    message: 'Storage full: 100 B allocated and 3 retained files (limits: 100 B, 3 files); delete files to continue'
  });
  const estimate = await library.estimate();
  assert.deepEqual({ ...estimate, quota: null, usage: null }, { bytes: 100, files: 3, budgetBytes: 100, budgetFiles: 3, quota: null, usage: null, persisted: null });
  assert.equal(await library.removeAll(), 3);
  assert.deepEqual(await library.list(), []);
  await library.check(100);
  const big = new Library();
  assert.equal(big.budgetBytes, 2 * 2 ** 30);
  assert.equal(big.budgetFiles, 1000);
});

test('Download all hands every file to the engine zip writer, oldest first', async () => {
  const library = new Library();
  await assert.rejects(library.zipAll(null), { message: 'Capturefab is still loading; try again in a moment' });
  await assert.rejects(library.zipAll({ zip: async () => blob(0) }), { message: 'There are no saved files to download' });
  await library.save(blob(3), { ext: 'png', label: 'c', date: FIXED });
  await library.save(blob(4), { ext: 'ppm', label: 'c', date: FIXED, naming: 'numbered' });
  let entries = null;
  const zipped = await library.zipAll({ zip: async (given) => { entries = given; return new Blob(['zip'], { type: 'application/zip' }); } });
  assert.equal(zipped.type, 'application/zip');
  assert.deepEqual(entries.map((e) => [e.name, e.blob.size, e.date instanceof Date]), [
    ['2026-10-17/c-20261017-101500-0001.png', 3, true],
    ['capture-0001.ppm', 4, true]
  ]);
});

// A directory handle kept in memory, as much of the File System Access API as the library uses.
class FakeDir {
  kind = 'directory';
  #items = new Map();
  constructor(name) {
    this.name = name;
  }
  async getDirectoryHandle(name, { create = false } = {}) {
    let item = this.#items.get(name);
    if (!item && create) this.#items.set(name, (item = new FakeDir(name)));
    if (!item) throw Object.assign(new Error(`${name} not found`), { name: 'NotFoundError' });
    if (item.kind !== 'directory') throw Object.assign(new Error(`${name} is a file`), { name: 'TypeMismatchError' });
    return item;
  }
  async getFileHandle(name, { create = false } = {}) {
    let item = this.#items.get(name);
    if (!item && create) {
      let file = new File([], name);
      item = {
        kind: 'file',
        getFile: async () => file,
        createWritable: async () => ({ write: async (blob) => { file = new File([blob], name); }, close: async () => {} })
      };
      this.#items.set(name, item);
    }
    if (!item) throw Object.assign(new Error(`${name} not found`), { name: 'NotFoundError' });
    if (item.kind !== 'file') throw Object.assign(new Error(`${name} is a folder`), { name: 'TypeMismatchError' });
    return item;
  }
  async removeEntry(name) {
    if (!this.#items.delete(name)) throw Object.assign(new Error(`${name} not found`), { name: 'NotFoundError' });
  }
  async *keys() {
    yield* this.#items.keys();
  }
  async *entries() {
    yield* this.#items.entries();
  }
}

test('saving in this browser does not wait for the persistent-storage prompt (Firefox)', async (t) => {
  const top = new FakeDir('');
  let asks = 0;
  let answer;
  const storage = {
    getDirectory: async () => top,
    persisted: async () => false,
    // Firefox shows a doorhanger and settles only when the visitor answers it.
    persist: () => {
      asks += 1;
      return new Promise((resolve) => { answer = resolve; });
    }
  };
  const had = Object.getOwnPropertyDescriptor(globalThis.navigator, 'storage');
  Object.defineProperty(globalThis.navigator, 'storage', { value: storage, configurable: true });
  const handle = globalThis.FileSystemFileHandle;
  globalThis.FileSystemFileHandle ??= class { createWritable() {} };
  t.after(() => {
    if (had) Object.defineProperty(globalThis.navigator, 'storage', had);
    else delete globalThis.navigator.storage;
    if (handle === undefined) delete globalThis.FileSystemFileHandle;
  });
  const library = new Library();
  await library.useBrowser();
  assert.equal(library.kind, 'opfs');
  assert.equal(library.persisted, false);
  const saved = await library.save(blob(5), { ext: 'png', label: 'c', date: FIXED });
  assert.equal(saved.name, 'c-20261017-101500-0001.png');
  await library.save(blob(5), { ext: 'png', label: 'c', date: FIXED });
  assert.equal(asks, 1, 'asked once, on the first save');
  assert.equal(library.persisted, false);
  let changes = 0;
  library.addEventListener('change', () => { changes += 1; });
  answer(true);
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(library.persisted, true);
  assert.equal(changes, 1, 'the answer arrives as a change');
});
