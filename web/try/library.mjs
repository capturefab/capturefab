// Saved captures and recordings: the desktop's file names (src/naming.rs) and
// storage limits (src/storage.rs), kept in this browser's private storage
// (OPFS), in a folder the visitor picks, or in memory for this tab. Files are
// never overwritten. Browser APIs are used only inside functions, so Node can
// import this module.
import { fixed, grouped } from './features.mjs';

// ---------------------------------------------------------------------------
// Names (src/naming.rs).

/** A file-name-safe camera label, `model-serial` (naming.rs `camera_label`). */
export const cameraLabel = (model, serial) => {
  let label = '';
  for (const c of `${model}-${serial}`) {
    const safe = /^[A-Za-z0-9._-]$/.test(c) ? c : '-';
    if (!(safe === '-' && label.endsWith('-'))) label += safe;
  }
  label = label.replace(/^[-.]+|[-.]+$/g, '');
  if (label.length > 64) label = label.slice(0, 64).replace(/[-.]+$/, '');
  const stem = label.split('.')[0].toUpperCase();
  const reserved = ['CON', 'PRN', 'AUX', 'NUL'].includes(stem) || /^(COM|LPT)[1-9]$/.test(stem);
  return label === '' || reserved ? 'camera' : label;
};

/** The extension a file of `format` gets: "jpg" for JPEG. */
export const formatExtension = (format) => (format === 'jpeg' ? 'jpg' : format);

/** The desktop's file-name presets that need no capture tags (naming_presets.rs). */
export const NAMINGS = Object.freeze([
  { id: 'dated', label: 'By date, camera and time (recommended)', short: 'By date, camera and time', template: (ext) => `{date}/{camera}-{datetime}-{seq}.${ext}` },
  { id: 'camera-time', label: 'Camera and time', short: 'Camera and time', template: (ext) => `{camera}-{datetime}-{seq}.${ext}` },
  { id: 'numbered', label: 'Numbered (capture-1, capture-2…)', short: 'Numbered', template: (ext) => `capture-{seq}.${ext}` }
]);

export const naming = (id) => NAMINGS.find((preset) => preset.id === id) ?? NAMINGS[0];

const PLACEHOLDER_ERROR = 'capture output supports only the {camera}, {date}, {time}, {datetime}, {seq}, {frame} and {tag:KEY} placeholders';
const KINDS = ['camera', 'date', 'time', 'datetime', 'seq', 'frame'];
/** The largest {seq}. */
export const MAX_SEQ = 99_999_999;

/** A template's parts: `{ lit }` text, or `{ kind }` for a placeholder. */
export const tokens = (template) => {
  const out = [];
  let rest = template;
  for (let open = rest.search(/[{}]/); open >= 0; open = rest.search(/[{}]/)) {
    if (rest[open] !== '{') throw new Error(PLACEHOLDER_ERROR);
    if (open > 0) out.push({ lit: rest.slice(0, open) });
    const close = rest.indexOf('}', open);
    if (close < 0) throw new Error(PLACEHOLDER_ERROR);
    const word = rest.slice(open + 1, close);
    if (KINDS.includes(word)) out.push({ kind: word });
    else if (/^tag:[a-z][a-z0-9_]*$/.test(word)) out.push({ kind: 'tag', key: word.slice(4) });
    else throw new Error(PLACEHOLDER_ERROR);
    rest = rest.slice(close + 1);
  }
  if (rest !== '') out.push({ lit: rest });
  return out;
};

const pad = (n, width = 2) => String(n).padStart(width, '0');

/** `{date}`, `{time}` and `{datetime}` for a local time, as chrono writes them. */
export const stamps = (now) => {
  const date = `${pad(now.getFullYear(), 4)}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
  const time = `${pad(now.getHours())}${pad(now.getMinutes())}${pad(now.getSeconds())}`;
  return { date, time, datetime: `${date.replaceAll('-', '')}-${time}` };
};

const expand = (token, { camera, now, index = 0 }) => {
  if ('lit' in token) return token.lit;
  switch (token.kind) {
    case 'camera':
      return camera;
    case 'frame':
      return pad(index + 1, 6);
    case 'tag':
      throw new Error(`capture output uses {tag:${token.key}} but the capture tag '${token.key}' is empty; fill it in or change the file name pattern`);
    default:
      return stamps(now)[token.kind];
  }
};

/** What a template expands to now; {seq} reads `####` until it is known. */
export const preview = (template, camera, now, seq = null) => {
  let list;
  try {
    list = tokens(template);
  } catch {
    return template;
  }
  return list.map((token) => {
    if (token.kind === 'seq') return seq === null ? '####' : pad(seq, 4);
    if (token.kind === 'tag') return `{tag:${token.key}}`;
    return expand(token, { camera, now });
  }).join('');
};

const isStamp = (token) => ['date', 'time', 'datetime'].includes(token.kind);

/**
 * How a numbered name is built: its folder (relative, '' for the top) and its
 * parts, `{ lit }`, `{ stamp }` (any digits and dashes of that length, so
 * numbering continues across seconds and days) and `{ seq }`. A template
 * without {seq} gives `{ fixed }`, its whole name.
 */
export const namePlan = (template, input) => {
  const list = tokens(template);
  const at = list.findIndex((token) => token.kind === 'seq');
  if (at < 0) return { fixed: list.map((token) => expand(token, input)).join('') };
  const before = list.slice(0, at).map((token) => expand(token, input)).join('');
  const dir = before.slice(0, before.lastIndexOf('/') + 1).replace(/\/+$/, '');
  const parts = [];
  let lit = '';
  for (const token of list.slice(0, at).reverse()) {
    const text = expand(token, input);
    const cut = text.lastIndexOf('/');
    if (cut >= 0) {
      lit = text.slice(cut + 1) + lit;
      break;
    }
    if (isStamp(token)) {
      if (lit !== '') parts.push({ lit });
      lit = '';
      parts.push({ stamp: text });
    } else {
      lit = text + lit;
    }
  }
  if (lit !== '') parts.push({ lit });
  parts.reverse();
  parts.push({ seq: true });
  for (const token of list.slice(at + 1)) {
    const text = expand(token, input);
    parts.push(isStamp(token) ? { stamp: text } : { lit: text });
  }
  return { dir, parts };
};

/** The {seq} a file name holds for these parts, or null when it is not one of them. */
export const seqOf = (name, parts) => {
  let rest = name;
  let seq = null;
  for (const part of parts) {
    if ('lit' in part) {
      if (!rest.startsWith(part.lit)) return null;
      rest = rest.slice(part.lit.length);
    } else if ('stamp' in part) {
      const head = rest.slice(0, part.stamp.length);
      if (head.length < part.stamp.length || !/^[0-9-]*$/.test(head)) return null;
      rest = rest.slice(part.stamp.length);
    } else {
      const digits = /^[0-9]*/.exec(rest)[0];
      if (digits === '') return null;
      seq = digits.length > 10 ? null : Number(digits);
      if (seq !== null && seq > 0xffffffff) seq = null;
      rest = rest.slice(digits.length);
    }
  }
  return rest === '' ? seq : null;
};

/** The highest {seq} already used among `names`, or 0. */
export const highestSeq = (names, parts) => names.reduce((high, name) => Math.max(high, seqOf(name, parts) ?? 0), 0);

/** A name from these parts with {seq} = n, at least four digits. */
export const buildName = (parts, n) => parts.map((part) => ('seq' in part ? pad(n, 4) : part.lit ?? part.stamp)).join('');

// ---------------------------------------------------------------------------
// Sizes and limits (src/storage.rs, src/gui/storage_policy.rs).

/** "512 B", "284 KiB", "2.5 MiB", "10 GiB" (storage.rs `human_bytes`). */
export const humanBytes = (bytes) => {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  let size = bytes / 1024;
  let unit = 0;
  while (size >= 1024 && unit + 1 < units.length) {
    size /= 1024;
    unit += 1;
  }
  const text = size < 10 ? fixed(size, 1) : fixed(size, 0);
  return `${text.replace(/\.0$/, '')} ${units[unit]}`;
};

/** A limit in the largest binary unit it is a whole number of: "2 GiB" (storage_policy.rs `bytes`). */
export const limitBytes = (bytes) => {
  for (const [unit, label] of [[2 ** 30, 'GiB'], [2 ** 20, 'MiB'], [2 ** 10, 'KiB']]) {
    if (bytes >= unit && bytes % unit === 0) return `${grouped(bytes / unit)} ${label}`;
  }
  return `${grouped(bytes)} bytes`;
};

/** To one decimal in the largest binary unit it fills: "4.2 GiB" (storage_policy.rs `rounded`). */
export const roundedBytes = (bytes) => {
  for (const [unit, label] of [[2 ** 40, 'TiB'], [2 ** 30, 'GiB'], [2 ** 20, 'MiB'], [2 ** 10, 'KiB']]) {
    if (bytes >= unit) return `${fixed(bytes / unit, 1)} ${label}`;
  }
  return `${grouped(bytes)} bytes`;
};

/** The browser's default limits: smaller than the desktop's 10 GiB and 10,000 files. */
export const BUDGET_BYTES = 2 * 2 ** 30;
export const BUDGET_FILES = 1000;

/** The desktop's refusal at capacity, with the browser's way out. */
export const storageFull = (bytes, files, budgetBytes, budgetFiles) => new Error(
  `Storage full: ${humanBytes(bytes)} allocated and ${files} retained files (limits: ${humanBytes(budgetBytes)}, ${budgetFiles} files); delete files to continue`
);

const TYPES = {
  png: 'image/png',
  jpg: 'image/jpeg',
  raw: 'application/octet-stream',
  pgm: 'image/x-portable-graymap',
  ppm: 'image/x-portable-pixmap',
  mp4: 'video/mp4',
  webm: 'video/webm',
  zip: 'application/zip'
};

/** The media type of a saved file, by its extension. */
export const typeOf = (name) => TYPES[name.slice(name.lastIndexOf('.') + 1).toLowerCase()] ?? 'application/octet-stream';

const SAVED = /\.(png|jpe?g|raw|pgm|ppm|mp4|webm)$/i;

// ---------------------------------------------------------------------------
// Stores: one directory tree each, addressed by '/'-separated relative paths.

const split = (path) => {
  const at = path.lastIndexOf('/');
  return at < 0 ? ['', path] : [path.slice(0, at), path.slice(at + 1)];
};

const named = (error, name) => error?.name === name;

/** Files kept in memory until the tab closes. */
class MemoryStore {
  kind = 'memory';
  place = 'This tab';
  root = 'Capturefab';
  #files = new Map();

  async names(dir) {
    const prefix = dir === '' ? '' : `${dir}/`;
    const names = new Set();
    for (const path of this.#files.keys()) {
      if (path.startsWith(prefix)) names.add(path.slice(prefix.length).split('/')[0]);
    }
    return [...names];
  }

  async taken(path) {
    return this.#files.has(path) || [...this.#files.keys()].some((other) => other.startsWith(`${path}/`));
  }

  async write(path, blob) {
    const file = new File([blob], split(path)[1], { type: typeOf(path), lastModified: Date.now() });
    this.#files.set(path, file);
    return file;
  }

  async read(path) {
    const file = this.#files.get(path);
    if (!file) throw new Error(`${path} is no longer saved`);
    return file;
  }

  async remove(path) {
    this.#files.delete(path);
  }

  async entries() {
    return [...this.#files].map(([path, file]) => ({ path, file }));
  }
}

// OPFS writes without createWritable (Safari before 26) go through a
// synchronous access handle, which only a dedicated worker can open.
const WRITER = `onmessage = async (event) => {
  const { id, parts, name, blob } = event.data;
  try {
    let dir = await navigator.storage.getDirectory();
    for (const part of parts) dir = await dir.getDirectoryHandle(part, { create: true });
    const file = await dir.getFileHandle(name, { create: true });
    const handle = await file.createSyncAccessHandle();
    try {
      const data = new Uint8Array(await blob.arrayBuffer());
      handle.truncate(0);
      let at = 0;
      while (at < data.length) at += handle.write(data.subarray(at), { at });
      handle.flush();
    } finally {
      handle.close();
    }
    postMessage({ id });
  } catch (error) {
    postMessage({ id, error: { name: error.name, message: error.message } });
  }
};`;

let writer = null;
let writes = 0;
const pending = new Map();

const workerWrite = (parts, name, blob) => {
  if (!writer) {
    const url = URL.createObjectURL(new Blob([WRITER], { type: 'text/javascript' }));
    writer = new Worker(url);
    URL.revokeObjectURL(url);
    writer.onmessage = ({ data }) => {
      const settle = pending.get(data.id);
      pending.delete(data.id);
      if (!settle) return;
      if (data.error) settle.reject(Object.assign(new Error(data.error.message), { name: data.error.name }));
      else settle.resolve();
    };
  }
  const id = (writes += 1);
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    writer.postMessage({ id, parts, name, blob });
  });
};

const canWrite = () => typeof FileSystemFileHandle !== 'undefined' && 'createWritable' in FileSystemFileHandle.prototype;

/**
 * A directory handle: this origin's private folder ('opfs') or a folder the
 * visitor picked ('folder'). A picked folder may hold other files, so its
 * list is what this page saved there, as the desktop's ledger lists only
 * what Capturefab saved.
 */
class HandleStore {
  #root;
  #ledger;
  #prefix;

  constructor(kind, root, { place, name, prefix = [] }) {
    this.kind = kind;
    this.place = place;
    this.root = name;
    this.#root = root;
    this.#prefix = prefix;
    this.#ledger = kind === 'folder' ? new Set() : null;
  }

  async #dir(path, create = false) {
    let dir = this.#root;
    for (const part of path.split('/').filter(Boolean)) dir = await dir.getDirectoryHandle(part, { create });
    return dir;
  }

  async names(path) {
    let dir;
    try {
      dir = await this.#dir(path);
    } catch (error) {
      if (named(error, 'NotFoundError') || named(error, 'TypeMismatchError')) return [];
      throw error;
    }
    const names = [];
    for await (const name of dir.keys()) names.push(name);
    return names;
  }

  async taken(path) {
    const [folder, name] = split(path);
    try {
      const dir = await this.#dir(folder);
      await dir.getFileHandle(name);
      return true;
    } catch (error) {
      if (named(error, 'TypeMismatchError')) return true;
      if (named(error, 'NotFoundError')) return false;
      throw error;
    }
  }

  async write(path, blob) {
    const [folder, name] = split(path);
    const dir = await this.#dir(folder, true);
    try {
      if (this.kind === 'folder' || canWrite()) {
        const handle = await dir.getFileHandle(name, { create: true });
        const stream = await handle.createWritable();
        try {
          await stream.write(blob);
          await stream.close();
        } catch (error) {
          await stream.abort?.().catch(() => {});
          throw error;
        }
      } else {
        await workerWrite([...this.#prefix, ...folder.split('/').filter(Boolean)], name, blob);
      }
    } catch (error) {
      // A partial file must not stay behind under a name that reads as saved.
      await dir.removeEntry(name).catch(() => {});
      throw error;
    }
    this.#ledger?.add(path);
    return this.read(path);
  }

  async read(path) {
    const [folder, name] = split(path);
    try {
      const dir = await this.#dir(folder);
      return await (await dir.getFileHandle(name)).getFile();
    } catch (error) {
      if (named(error, 'NotFoundError')) throw new Error(`${path} is no longer saved`);
      throw error;
    }
  }

  async remove(path) {
    const [folder, name] = split(path);
    try {
      const dir = await this.#dir(folder);
      await dir.removeEntry(name);
    } catch (error) {
      if (!named(error, 'NotFoundError')) throw error;
    }
    this.#ledger?.delete(path);
    // A dated folder goes with its last file; one with other files stays.
    if (folder !== '') {
      const [parent, leaf] = split(folder);
      await this.#dir(parent).then((dir) => dir.removeEntry(leaf)).catch(() => {});
    }
  }

  async entries() {
    if (this.#ledger) {
      const found = [];
      for (const path of [...this.#ledger]) {
        try {
          found.push({ path, file: await this.read(path) });
        } catch {
          this.#ledger.delete(path);
        }
      }
      return found;
    }
    const found = [];
    const walk = async (dir, prefix, depth) => {
      for await (const [name, handle] of dir.entries()) {
        const path = prefix === '' ? name : `${prefix}/${name}`;
        if (handle.kind === 'file') {
          if (SAVED.test(name)) found.push({ path, file: await handle.getFile() });
        } else if (depth < 3) {
          await walk(handle, path, depth + 1);
        }
      }
    };
    await walk(this.#root, '', 0);
    return found;
  }
}

// ---------------------------------------------------------------------------

/** Which places this browser can save to. */
export const storageSupport = () => ({
  browser: typeof navigator !== 'undefined' && typeof navigator.storage?.getDirectory === 'function',
  folder: typeof globalThis.showDirectoryPicker === 'function'
});

const record = (store, path, file) => ({
  name: split(path)[1],
  path,
  location: `${store.root}/${path}`,
  size: file.size,
  type: file.type || typeOf(path),
  created: file.lastModified
});

/**
 * Where captures and recordings are saved, and what is saved there. Saves are
 * serialised (across tabs with Web Locks), each picks the next free name, and
 * none replaces a file. Fires 'change' after anything is saved or deleted,
 * and when the place changes.
 */
export class Library extends EventTarget {
  #memory = new MemoryStore();
  #store = this.#memory;
  #queue = Promise.resolve();
  #persisted = null;
  #askedPersist = false;

  constructor({ budgetBytes = BUDGET_BYTES, budgetFiles = BUDGET_FILES } = {}) {
    super();
    this.budgetBytes = budgetBytes;
    this.budgetFiles = budgetFiles;
  }

  /** 'opfs', 'folder' or 'memory'. */
  get kind() {
    return this.#store.kind;
  }

  /** Where files go, in words: "This browser", a folder's name, or "This tab". */
  get place() {
    return this.#store.place;
  }

  /** The folder name file locations start with. */
  get root() {
    return this.#store.root;
  }

  /** Whether the browser agreed to keep these files when it needs space; null before asking. */
  get persisted() {
    return this.#persisted;
  }

  #changed() {
    this.dispatchEvent(new Event('change'));
  }

  /** Save to this browser's private storage, in a folder named Capturefab. */
  async useBrowser() {
    const top = await navigator.storage.getDirectory();
    const root = await top.getDirectoryHandle('Capturefab', { create: true });
    this.#store = new HandleStore('opfs', root, { place: 'This browser', name: 'Capturefab', prefix: ['Capturefab'] });
    this.#persisted = (await navigator.storage.persisted?.().catch(() => null)) ?? null;
    this.#changed();
  }

  /** Save to a folder the visitor picked with showDirectoryPicker. */
  async useFolder(handle) {
    const mode = { mode: 'readwrite' };
    const granted = async () => typeof handle.queryPermission !== 'function'
      || (await handle.queryPermission(mode)) === 'granted'
      || (await handle.requestPermission?.(mode)) === 'granted';
    if (!(await granted())) {
      throw new Error(`Capturefab cannot save in ${handle.name} without permission to change files there`);
    }
    this.#store = new HandleStore('folder', handle, { place: handle.name, name: handle.name });
    this.#changed();
  }

  /** Keep files in this tab's memory until it closes. */
  useTab() {
    this.#store = this.#memory;
    this.#changed();
  }

  #exclusive(task) {
    const locks = globalThis.navigator?.locks;
    if (typeof locks?.request === 'function') return locks.request('capturefab-library', () => task());
    const run = this.#queue.then(task, task);
    this.#queue = run.catch(() => {});
    return run;
  }

  /** What is saved, newest first. */
  async list() {
    const store = this.#store;
    const entries = await store.entries();
    return entries.map(({ path, file }) => record(store, path, file))
      .sort((a, b) => b.created - a.created || (a.path < b.path ? 1 : -1));
  }

  /** Bytes and files saved, as the limits count them. */
  async usage() {
    const entries = await this.#store.entries();
    return { bytes: entries.reduce((sum, { file }) => sum + file.size, 0), files: entries.length };
  }

  /** Room for a save of `size` bytes, or the desktop's refusal. */
  async check(size = 0) {
    const { bytes, files } = await this.usage();
    if (files >= this.budgetFiles || bytes + size > this.budgetBytes) {
      throw storageFull(bytes, files, this.budgetBytes, this.budgetFiles);
    }
    return { bytes, files };
  }

  /**
   * Save `blob` under the next free name of the `naming` preset for camera
   * `label` at `date`; returns its record.
   */
  async save(blob, { ext, label = 'camera', date = new Date(), naming: preset = 'dated' }) {
    const template = naming(preset).template(ext);
    const saved = await this.#exclusive(async () => {
      const store = this.#store;
      await this.check(blob.size);
      const plan = namePlan(template, { camera: label, now: date });
      let path;
      if (plan.fixed !== undefined) {
        path = plan.fixed;
        if (await store.taken(path)) throw new Error(`output already exists: ${store.root}/${path}; Capturefab never overwrites files, so choose a new name`);
      } else {
        const names = await store.names(plan.dir);
        const taken = new Set(names);
        const at = (n) => (plan.dir === '' ? '' : `${plan.dir}/`) + buildName(plan.parts, n);
        for (let n = highestSeq(names, plan.parts) + 1; ; n += 1) {
          if (n > MAX_SEQ) throw new Error(`no free file name for ${template}; choose another output`);
          if (!taken.has(buildName(plan.parts, n)) && !(await store.taken(at(n)))) {
            path = at(n);
            break;
          }
        }
      }
      let file;
      try {
        file = await store.write(path, blob);
      } catch (error) {
        if (named(error, 'QuotaExceededError')) {
          throw new Error(`Storage full: this browser has no room for ${humanBytes(blob.size)} more; delete files to continue`);
        }
        throw new Error(`save ${store.root}/${path}: ${error.message || error.name}`);
      }
      return record(store, path, file);
    });
    if (this.kind === 'opfs' && this.#persisted !== true && !this.#askedPersist) {
      // Once, on the first save: browsers may decline, and may then clear the files
      // when they need space. Never awaited: Firefox asks the visitor in a doorhanger
      // that can stay open indefinitely, and the save is already done.
      this.#askedPersist = true;
      Promise.resolve(navigator.storage?.persist?.()).then((value) => {
        if (typeof value !== 'boolean') return;
        this.#persisted = value;
        this.#changed();
      }, () => {});
    }
    this.#changed();
    return saved;
  }

  /** The saved file at `path`. */
  file(path) {
    return this.#store.read(path);
  }

  /** Delete the file at `path`. */
  async remove(path) {
    await this.#exclusive(() => this.#store.remove(path));
    this.#changed();
  }

  /** Delete every saved file; returns how many. */
  async removeAll() {
    const count = await this.#exclusive(async () => {
      const entries = await this.#store.entries();
      for (const { path } of entries) await this.#store.remove(path);
      return entries.length;
    });
    this.#changed();
    return count;
  }

  /** Use against the limits, and what the browser grants this site. */
  async estimate() {
    const { bytes, files } = await this.usage();
    const site = await globalThis.navigator?.storage?.estimate?.().catch(() => null);
    return {
      bytes,
      files,
      budgetBytes: this.budgetBytes,
      budgetFiles: this.budgetFiles,
      quota: site?.quota ?? null,
      usage: site?.usage ?? null,
      persisted: this.#persisted
    };
  }

  /** Every saved file in one ZIP archive, through the engine's zip writer. */
  async zipAll(engine) {
    if (!engine) throw new Error('Capturefab is still loading; try again in a moment');
    const records = await this.list();
    if (records.length === 0) throw new Error('There are no saved files to download');
    const entries = [];
    for (const item of records.reverse()) {
      entries.push({ name: item.path, blob: await this.file(item.path), date: new Date(item.created) });
    }
    return engine.zip(entries);
  }
}
