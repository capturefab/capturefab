// Figures and words formatted as the desktop app writes them (src/gui/format.rs,
// scopes.rs, toolbar.rs, keys.rs, storage.rs), so the browser shows the same text.
// Pure functions only: Node imports this module in scripts/try-tests.

const f32 = Math.fround;

/**
 * `value` with `digits` decimals, as Rust's `format!("{:.N}")` writes it: from the
 * exact binary value, with exact ties rounded to even (toFixed rounds them up).
 */
export const fixed = (value, digits = 0) => {
  if (Number.isNaN(value)) return 'NaN';
  const negative = value < 0 || Object.is(value, -0);
  const magnitude = Math.abs(value);
  if (magnitude === Infinity) return negative ? '-inf' : 'inf';
  let text;
  if (magnitude >= 1e21) {
    text = BigInt(magnitude).toString() + (digits ? `.${'0'.repeat(digits)}` : '');
  } else {
    text = magnitude.toFixed(digits);
    // A tie terminates within digits + 1 places, so this longer form is exact for it.
    const long = magnitude.toFixed(Math.min(100, digits + 30));
    const point = long.indexOf('.');
    const rest = long.slice(point + 1 + digits);
    if (rest[0] === '5' && /^50*$/.test(rest)) {
      const kept = long.slice(0, digits ? point + 1 + digits : point);
      if (Number(kept.at(-1)) % 2 === 0) text = kept;
    }
  }
  return negative ? `-${text}` : text;
};

/** A share of samples (0–1) as the scopes show it: "0%", "<0.1%", "2.0%", "46%". */
export const percent = (share) => {
  const p = f32(f32(share) * 100);
  if (!(p > 0)) return '0%';
  if (p < f32(0.1)) return '<0.1%';
  if (p < 10) return `${fixed(p, 1)}%`;
  return `${fixed(p, 0)}%`;
};

/** A frame rate: "29.9 fps". */
export const fps = (value) => `${fixed(Number.isFinite(value) ? value : 0, 1)} fps`;

/** A focus score in four significant figures at most: "0.42", "41.7", "1203", "12.4k". */
export const focusScore = (value) => {
  const v = f32(value);
  if (v >= 10000) return `${fixed(f32(v / 1000), 1)}k`;
  if (v >= 100) return fixed(v, 0);
  if (v >= 10) return fixed(v, 1);
  return fixed(v, 2);
};

/** A byte count in binary units, as storage limits are written: "512 B", "2 KiB", "1.5 MiB", "10 GiB". */
export const bytes = (n) => {
  const units = ['KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  const value = Math.max(0, Math.floor(Number(n) || 0));
  if (value < 1024) return `${value} B`;
  let size = value / 1024;
  let unit = 0;
  while (size >= 1024 && unit + 1 < units.length) {
    size /= 1024;
    unit += 1;
  }
  const text = size < 10 ? fixed(size, 1) : fixed(size, 0);
  return `${text.endsWith('.0') ? text.slice(0, -2) : text} ${units[unit]}`;
};

/** A whole number with thousands separators: "1,204". */
export const count = (n) => {
  const value = Math.trunc(Number(n) || 0);
  const digits = String(Math.abs(value));
  let out = '';
  for (let i = 0; i < digits.length; i += 1) {
    if (i > 0 && (digits.length - i) % 3 === 0) out += ',';
    out += digits[i];
  }
  return value < 0 ? `-${out}` : out;
};

/** "1 frame", "1,204 frames". */
export const plural = (n, one, many) => (n === 1 ? `1 ${one}` : `${count(n)} ${many}`);

/** A recording clock: "04:07", or "1:02:03" past an hour. */
export const clock = (ms) => {
  const seconds = Math.max(0, Math.floor((Number(ms) || 0) / 1000));
  const two = (v) => String(v).padStart(2, '0');
  if (seconds >= 3600) return `${Math.floor(seconds / 3600)}:${two(Math.floor(seconds / 60) % 60)}:${two(seconds % 60)}`;
  return `${two(Math.floor(seconds / 60))}:${two(seconds % 60)}`;
};

/** A duration in its largest whole unit: "6 s", "2 min", "2 h". */
export const span = (ms) => {
  const seconds = Math.max(0, Math.floor((Number(ms) || 0) / 1000));
  if (seconds < 60) return `${seconds} s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min`;
  return `${Math.floor(seconds / 3600)} h`;
};

/** What a stalled camera's badge says: "No new frame for 6 s". */
export const silence = (ms) => `No new frame for ${span(Math.max(1000, Number(ms) || 0))}`;

/**
 * A sparkline's tooltip: "Frame rate, last 30 s: 24.5 fps to 30.0 fps", with
 * "\nMarked: {flagged} ({n})" when samples are marked. `samples` are {value, flagged}.
 */
export const trend = (samples, seconds, what, value, flagged) => {
  let about = `${what}, last ${Math.max(1, Math.floor(seconds))} s`;
  if (samples.length) {
    const values = samples.map((s) => s.value);
    about += `: ${value(Math.min(...values))} to ${value(Math.max(...values))}`;
  }
  const marked = samples.filter((s) => s.flagged).length;
  return marked ? `${about}\nMarked: ${flagged} (${marked})` : about;
};

const MAC_MODS = [['ctrl', '⌃'], ['alt', '⌥'], ['shift', '⇧'], ['meta', '⌘']];
const OTHER_MODS = [['ctrl', 'Ctrl'], ['alt', 'Alt'], ['shift', 'Shift'], ['meta', 'Win']];

/**
 * A shortcut as the desktop's tooltips write it. `spec` names modifiers and a key
 * joined by '+', e.g. 'Mod+Shift+S', 'Mod+=', 'Mod++', 'Space'; Mod is ⌘ on Apple
 * systems and Ctrl elsewhere. Apple order is ⌃⌥⇧⌘ with glyphs; others "Ctrl+Shift+S".
 */
export const chord = (spec, isApple = false) => {
  if (!spec) return '';
  const parts = String(spec).split('+');
  // 'Mod++' and '+' name the plus key itself.
  if (parts.length > 1 && parts.at(-1) === '' && parts.at(-2) === '') parts.splice(-2, 2, '+');
  else if (spec === '+') parts.splice(0, parts.length, '+');
  const raw = parts.pop();
  const held = { ctrl: false, alt: false, shift: false, meta: false };
  for (const part of parts) {
    const name = part.toLowerCase();
    if (name === 'mod') held[isApple ? 'meta' : 'ctrl'] = true;
    else if (name === 'ctrl' || name === 'control') held.ctrl = true;
    else if (name === 'alt' || name === 'option') held.alt = true;
    else if (name === 'shift') held.shift = true;
    else if (name === 'meta' || name === 'cmd' || name === 'command') held.meta = true;
  }
  const names = {
    space: 'Space', escape: 'Esc', esc: 'Esc', enter: isApple ? '↩' : 'Enter',
    left: isApple ? '←' : 'Left', right: isApple ? '→' : 'Right',
    up: isApple ? '↑' : 'Up', down: isApple ? '↓' : 'Down', pageup: 'PgUp', pagedown: 'PgDn'
  };
  const key = names[raw.toLowerCase()] ?? (raw.length === 1 ? raw.toUpperCase() : raw);
  if (isApple) return MAC_MODS.filter(([m]) => held[m]).map(([, g]) => g).join('') + key;
  return [...OTHER_MODS.filter(([m]) => held[m]).map(([, g]) => g), key].join('+');
};

/** Tooltip text in the desktop's "description · shortcut" style. */
export const withChord = (text, spec, isApple = false) => (spec ? `${text} · ${chord(spec, isApple)}` : text);

/**
 * A deep copy of JSON-like `value` with object keys sorted by UTF-16 code unit, as
 * serde_json's BTreeMap writes them for ASCII keys. Typed arrays become arrays.
 */
export const sortKeys = (value) => {
  if (value === null || typeof value !== 'object') return value;
  if (typeof value.toJSON === 'function') return sortKeys(value.toJSON());
  if (Array.isArray(value)) return value.map((item) => (item === undefined || typeof item === 'function' ? null : sortKeys(item)));
  if (ArrayBuffer.isView(value)) return Array.from(value);
  const out = {};
  for (const key of Object.keys(value).sort()) {
    const item = value[key];
    if (item === undefined || typeof item === 'function' || typeof item === 'symbol') continue;
    out[key] = sortKeys(item);
  }
  return out;
};

/** Two-space indented JSON with sorted keys. */
export const json = (value) => JSON.stringify(sortKeys(value), null, 2);

/** `text` shortened to `max` characters with an ellipsis in the middle, keeping both ends. */
export const middleEllipsis = (text, max) => {
  const chars = [...String(text ?? '')];
  if (chars.length <= max) return chars.join('');
  if (max <= 1) return '…';
  const head = Math.ceil((max - 1) / 2);
  const tail = Math.floor((max - 1) / 2);
  return `${chars.slice(0, head).join('')}…${tail ? chars.slice(-tail).join('') : ''}`;
};

const FNV_OFFSET = 0xcbf29ce484222325n;
const FNV_PRIME = 0x100000001b3n;
const MASK = 0xffffffffffffffffn;

/** FNV-1a 64 over the UTF-8 bytes of `text`, as 16 lowercase hex digits (media.rs hashes URLs so). */
export const fnv1a64 = (text) => {
  let hash = FNV_OFFSET;
  for (const byte of new TextEncoder().encode(String(text ?? ''))) {
    hash = ((hash ^ BigInt(byte)) * FNV_PRIME) & MASK;
  }
  return hash.toString(16).padStart(16, '0');
};
