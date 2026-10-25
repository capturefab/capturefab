// Camera features as the desktop app's Features pane shows them: FeatureInfo
// objects (src/genicam.rs), the desktop's checks and wording for drafts
// (FeatureInfo::validate_input), its captions, labels and groups
// (src/gui/format.rs, src/gui/inspector.rs, src/gui/widgets.rs), and webcam
// controls read from MediaStreamTrack capabilities. Pure: nothing here touches
// the DOM or window at import, so Node can test it.

// ---------------------------------------------------------------------------
// Rust number formatting. The desktop formats with Rust, which rounds exact
// binary values half to even and never writes exponents in `{}`; JavaScript's
// toFixed/toExponential round ties up, so values are expanded exactly here.

const bits = new DataView(new ArrayBuffer(8));

// |x| as an exact decimal: int / 10^scale.
const decompose = (x) => {
  bits.setFloat64(0, x);
  const hi = bits.getUint32(0);
  const lo = bits.getUint32(4);
  const biased = (hi >>> 20) & 0x7ff;
  let mantissa = (BigInt(hi & 0xfffff) << 32n) | BigInt(lo);
  let exponent = -1074;
  if (biased !== 0) {
    mantissa |= 1n << 52n;
    exponent = biased - 1075;
  }
  const negative = hi >>> 31 === 1;
  if (exponent >= 0) return { negative, int: mantissa << BigInt(exponent), scale: 0 };
  return { negative, int: mantissa * 5n ** BigInt(-exponent), scale: -exponent };
};

// int / 10^k rounded half to even.
const divRound = (int, k) => {
  if (k <= 0) return int * 10n ** BigInt(-k);
  const d = 10n ** BigInt(k);
  const q = int / d;
  const twice = (int % d) * 2n;
  return twice > d || (twice === d && (q & 1n) === 1n) ? q + 1n : q;
};

const special = (x) => (Number.isNaN(x) ? 'NaN' : x > 0 ? 'inf' : '-inf');

/** Rust `format!("{x:.digits$}")`. */
export const fixed = (x, digits) => {
  if (!Number.isFinite(x)) return special(x);
  const { negative, int, scale } = decompose(x);
  let text = divRound(int, scale - digits).toString().padStart(digits + 1, '0');
  if (digits > 0) text = `${text.slice(0, -digits)}.${text.slice(-digits)}`;
  return negative ? `-${text}` : text;
};

/** Rust `format!("{x:.1e}")`: two significant digits and a bare exponent. */
export const exponential1 = (x) => {
  if (!Number.isFinite(x)) return special(x);
  const { negative, int, scale } = decompose(x);
  if (int === 0n) return negative ? '-0.0e0' : '0.0e0';
  const length = int.toString().length;
  let exponent = length - 1 - scale;
  let q = divRound(int, length - 2);
  if (q === 100n) {
    q = 10n;
    exponent += 1;
  }
  const d = q.toString();
  return `${negative ? '-' : ''}${d[0]}.${d[1]}e${exponent}`;
};

/** Rust `format!("{x}")` for f64: the shortest digits that read back, never an exponent. */
export const display = (x) => {
  if (!Number.isFinite(x)) return special(x);
  if (x === 0) return Object.is(x, -0) ? '-0' : '0';
  const [mantissa, power] = Math.abs(x).toExponential().split('e');
  const digits = mantissa.replace('.', '');
  const exponent = Number(power);
  let text;
  if (exponent >= digits.length - 1) text = digits + '0'.repeat(exponent - digits.length + 1);
  else if (exponent >= 0) text = `${digits.slice(0, exponent + 1)}.${digits.slice(exponent + 1)}`;
  else text = `0.${'0'.repeat(-exponent - 1)}${digits}`;
  return x < 0 ? `-${text}` : text;
};

// Rust f64::round: halves away from zero.
const round = (x) => Math.sign(x) * Math.round(Math.abs(x));
const fract = (x) => x % 1;

/** 1204 → "1,204" (format.rs `grouped`). */
export const grouped = (value) => {
  const digits = BigInt(value).toString();
  let out = '';
  for (let i = 0; i < digits.length; i += 1) {
    if (i > 0 && (digits.length - i) % 3 === 0) out += ',';
    out += digits[i];
  }
  return out;
};

/** format.rs `number`: whole numbers bare, else up to three decimals. */
export const number = (value) => (fract(value) === 0 ? fixed(value, 0) : fixed(value, 3).replace(/0+$/, '').replace(/\.$/, ''));

/** "1 feature", "1,204 features". */
export const plural = (n, one, many) => (n === 1 ? `1 ${one}` : `${grouped(n)} ${many}`);

export const capitalize = (text) => {
  const [first = ''] = text;
  return first.toUpperCase() + text.slice(first.length);
};

export const firstLine = (text) => String(text).split('\n')[0];

// ---------------------------------------------------------------------------
// FeatureInfo::validate_input (src/genicam.rs), with anyhow's `{:#}` chains.

const U64_MAX = (1n << 64n) - 1n;
const I64_MIN = -(1n << 63n);
const I64_MAX = (1n << 63n) - 1n;
const EXACT = 2 ** 53;
const EMPTY_INT = 'cannot parse integer from empty string';
const BAD_DIGIT = 'invalid digit found in string';
const TOO_LARGE = 'number too large to fit in target type';

const ok = (value) => ({ value });
const err = (error) => ({ error });
const chain = (outer, inner) => `${outer}: ${inner}`;

export const integerKind = (kind) => ['Integer', 'IntReg', 'MaskedIntReg', 'IntConverter'].includes(kind);

const hex = (text) => {
  if (text === '') return err(EMPTY_INT);
  if (text === '+' || text === '-') return err(BAD_DIGIT);
  const digits = text.startsWith('+') ? text.slice(1) : text;
  if (!/^[0-9a-fA-F]+$/.test(digits)) return err(BAD_DIGIT);
  const value = BigInt(`0x${digits}`);
  return value > U64_MAX ? err(TOO_LARGE) : ok(value);
};

const i32 = (text) => {
  if (text === '') return err(EMPTY_INT);
  if (!/^[+-]?\d+$/.test(text)) return err(BAD_DIGIT);
  const value = BigInt(text.replace(/^\+/, ''));
  if (value > 2147483647n) return err(TOO_LARGE);
  if (value < -2147483648n) return err('number too small to fit in target type');
  return ok(Number(value));
};

// genicam.rs `integer_literal`: decimal, scientific and hexadecimal integers, exactly.
const integerLiteral = (input) => {
  const text = input.trim();
  const negative = text.startsWith('-');
  const body = negative ? text.slice(1) : text.startsWith('+') ? text.slice(1) : text;
  let magnitude;
  if (body.startsWith('0x') || body.startsWith('0X')) {
    const parsed = hex(body.slice(2));
    if ('error' in parsed) return err(chain('invalid hexadecimal integer or integer overflow', parsed.error));
    magnitude = parsed.value;
  } else {
    const parts = body.split(/[eE]/);
    const mantissa = parts[0];
    let exponent = 0;
    if (parts.length > 1) {
      const parsed = i32(parts[1]);
      if ('error' in parsed) return err(chain('invalid integer exponent', parsed.error));
      exponent = parsed.value;
    }
    if (parts.length > 2) return err('invalid integer exponent');
    const decimals = mantissa.split('.');
    const whole = decimals[0];
    const fraction = decimals[1] ?? '';
    if (decimals.length > 2 || (whole === '' && fraction === '') || !/^\d*$/.test(whole + fraction)) {
      return err('expected a decimal or hexadecimal integer');
    }
    let digits = (whole + fraction).replace(/^0+/, '');
    if (digits === '') return ok(0n);
    const shift = exponent - fraction.length;
    if (shift < 0) {
      const remove = -shift;
      if (!(remove <= digits.length && /^0*$/.test(digits.slice(digits.length - remove)))) {
        return err('fractional values are not allowed');
      }
      digits = digits.slice(0, digits.length - remove);
    } else {
      if (!(shift <= 20 && digits.length + shift <= 20)) return err('integer overflow');
      digits += '0'.repeat(shift);
    }
    magnitude = BigInt(digits);
    if (magnitude > U64_MAX) return err(chain('integer overflow', TOO_LARGE));
  }
  if (negative) {
    const value = -magnitude;
    return value < I64_MIN ? err('signed integer overflow') : ok(value);
  }
  return ok(magnitude);
};

const RUST_FLOAT = /^[+-]?(?:\d+\.?\d*(?:[eE][+-]?\d+)?|\.\d+(?:[eE][+-]?\d+)?|inf|infinity|nan)$/i;

/** Rust `str::parse::<f64>`: its grammar and its two error texts. */
export const parseRustFloat = (text) => {
  if (text === '') return err('cannot parse float from empty string');
  if (!RUST_FLOAT.test(text)) return err('invalid float literal');
  const word = text.replace(/^[+-]/, '').toLowerCase();
  const sign = text.startsWith('-') ? -1 : 1;
  if (word === 'nan') return ok(NaN);
  if (word === 'inf' || word === 'infinity') return ok(sign * Infinity);
  return ok(Number(text));
};

// genicam.rs `literal`: a number, kept exact while it is an integer.
const literal = (input) => {
  const text = input.trim();
  const body = text.replace(/^[+-]/, '');
  if (body.startsWith('0x') || body.startsWith('0X')) return integerLiteral(text);
  if (/^[+-]?\d+$/.test(text)) {
    const value = BigInt(text.replace(/^\+/, ''));
    if (value >= I64_MIN && value <= I64_MAX) return ok(value);
    if (value >= 0n && value <= U64_MAX && !text.startsWith('-')) return ok(value);
  }
  const integer = integerLiteral(text);
  if ('value' in integer) return integer;
  const parsed = parseRustFloat(text);
  if ('error' in parsed) return err(chain('invalid numeric value', parsed.error));
  const n = parsed.value;
  if (!Number.isFinite(n)) return err('expected finite number');
  if (n === 0 && /[1-9]/.test(text.split(/[eE]/)[0])) {
    return err('numeric value is too small to represent; enter 0 explicitly or a representable nonzero value');
  }
  return ok(n);
};

const inputValue = (name, kind, text) => {
  if (kind === 'String' || kind === 'StringReg' || kind === 'Enumeration') return ok(text);
  if (kind === 'Boolean') {
    const word = text.trim().toLowerCase();
    if (['true', '1', 'on'].includes(word)) return ok(true);
    if (['false', '0', 'off'].includes(word)) return ok(false);
    return err(`${name} must be true or false (also accepts 1/0 and on/off)`);
  }
  if (kind === 'Command') return err(`use execute for command ${name}`);
  if (integerKind(kind)) {
    const parsed = integerLiteral(text);
    return 'error' in parsed ? err(chain(`${name} requires an exact integer`, parsed.error)) : parsed;
  }
  const parsed = literal(text);
  return 'error' in parsed ? err(chain(`${name} requires a finite number`, parsed.error)) : parsed;
};

const exactInteger = (value) => {
  if (typeof value === 'bigint') return ok(value);
  if (!(Number.isFinite(value) && fract(value) === 0 && Math.abs(value) <= EXACT)) {
    return err('expected an exactly representable integer');
  }
  return ok(BigInt(value));
};

const validateNumeric = (name, kind, value, min, max, inc) => {
  if (integerKind(kind)) {
    const v = exactInteger(value);
    if ('error' in v) return chain(`${name} requires an integer`, v.error);
    const bounds = [min, max].map((bound) => (bound == null ? ok(null) : exactInteger(bound)));
    for (const bound of bounds) if ('error' in bound) return bound.error;
    const [low, high] = bounds.map((bound) => bound.value);
    if (low !== null && high !== null && low > high) return `${name} has invalid camera bounds`;
    if (low !== null && v.value < low) return `${name} violates Min=${low}; enter ${low} or greater`;
    if (high !== null && v.value > high) return `${name} violates Max=${high}; enter ${high} or less`;
    if (inc != null) {
      const step = exactInteger(inc);
      if ('error' in step) return step.error;
      if (step.value <= 0n) return `${name} has invalid camera increment ${step.value}`;
      const base = low ?? 0n;
      if ((v.value - base) % step.value !== 0n) return `${name} requires increment ${step.value} from ${base}`;
    }
    return null;
  }
  const v = Number(value);
  if (min != null && max != null && min > max) return `${name} has invalid camera bounds`;
  if (min != null && !(v >= min)) return `${name} violates Min=${display(min)}; enter ${display(min)} or greater`;
  if (max != null && !(v <= max)) return `${name} violates Max=${display(max)}; enter ${display(max)} or less`;
  if (inc != null) {
    if (!(inc > 0)) return `${name} has invalid camera increment ${display(inc)}`;
    const base = min ?? 0;
    const nearest = base + round((v - base) / inc) * inc;
    const tolerance = Math.min(8 * Number.EPSILON * Math.max(Math.abs(v), Math.abs(base), Math.abs(nearest)), inc * 1e-7);
    if (!(Number.isFinite(nearest) && Math.abs(v - nearest) <= tolerance)) {
      return `${name} requires increment ${display(inc)} from ${display(base)}; nearest value is ${display(nearest)}`;
    }
  }
  return null;
};

/**
 * FeatureInfo::validate_input: null when `text` suits `feature`, else the
 * desktop's message, chained as anyhow's `{:#}` writes it.
 */
export const validateInput = (feature, text) => {
  const { name, kind } = feature;
  const parsed = inputValue(name, kind, text);
  if ('error' in parsed) return parsed.error;
  if (kind === 'Enumeration') {
    const choices = feature.choices ?? [];
    return choices.includes(text) ? null : `${name} must be one of: ${choices.join(', ')}`;
  }
  if (kind === 'StringReg') return text.includes('\0') ? `${name} cannot contain a NUL character` : null;
  if (kind === 'String' || kind === 'Boolean') return null;
  const integer = integerKind(kind);
  const exact = (bound) => (bound != null && (!integer || Math.abs(bound) < EXACT) ? bound : null);
  const wide = integer && feature.min != null && Math.abs(feature.min) >= EXACT;
  return validateNumeric(name, kind, parsed.value, exact(feature.min), exact(feature.max), wide ? null : exact(feature.inc));
};

// ---------------------------------------------------------------------------
// How the desktop shows features (src/gui/format.rs, inspector.rs).

/** Features a camera cannot change while it streams (genicam.rs `stream_locked`). */
export const streamLocked = (name) => [
  'Width', 'Height', 'OffsetX', 'OffsetY', 'PixelFormat', 'BinningHorizontal', 'BinningVertical',
  'DecimationHorizontal', 'DecimationVertical', 'VideoMode'
].includes(name);

const upper = /\p{Uppercase}/u;
const lower = /\p{Lowercase}/u;

/** "AcquisitionFrameRate" → "Acquisition Frame Rate", keeping acronyms: "Gev SCPS Packet Size". */
export const words = (name) => {
  const chars = [...name];
  let out = '';
  chars.forEach((c, i) => {
    if (i > 0 && c !== '_') {
      const previous = chars[i - 1];
      const nextLower = i + 1 < chars.length && lower.test(chars[i + 1]);
      const boundary = (upper.test(c) && (lower.test(previous) || (upper.test(previous) && nextLower)))
        || (/[0-9]/.test(c) && lower.test(previous));
      if (boundary && !out.endsWith(' ')) out += ' ';
    }
    out += c === '_' ? ' ' : c;
  });
  return out;
};

// Everyday names for the controls most cameras share (inspector.rs
// `adaptive_feature_label` for area-scan cameras), and for the modes webcams
// report; the GenICam name stays in the tooltip and the search.
const LABELS = {
  ExposureTime: 'Exposure',
  ExposureTimeAbs: 'Exposure',
  AcquisitionFrameRate: 'Frame rate',
  AcquisitionFrameRateAbs: 'Frame rate',
  AcquisitionFrameRateEnable: 'Limit frame rate',
  Gain: 'Gain',
  GainRaw: 'Gain',
  GainAbs: 'Gain',
  ExposureAuto: 'Auto exposure',
  FocusAuto: 'Autofocus',
  BalanceWhiteAuto: 'Auto white balance'
};

/** A feature's label: an everyday name, its display name, or its name as words. */
export const label = (feature) => {
  const name = typeof feature === 'string' ? feature : feature.name;
  const shown = typeof feature === 'string' ? '' : feature.display_name ?? '';
  return LABELS[name] ?? (shown === '' || shown === name ? words(name) : shown);
};

/** The groups an area-scan camera's features fall in, in order. */
export const GROUPS = Object.freeze(['Image', 'Acquisition', 'Device', 'Transport', 'Other']);

const starts = (name, prefixes) => prefixes.some((prefix) => name.startsWith(prefix));

/** format.rs `feature_group`. */
export const groupOf = (name) => {
  if (starts(name, ['Width', 'Height', 'Offset', 'Pixel', 'Binning', 'Decimation', 'Reverse', 'Sensor', 'TestPattern'])) return 'Image';
  if (starts(name, ['Acquisition', 'Exposure', 'Gain', 'Trigger', 'Balance', 'Black', 'Gamma', 'Analog', 'Digital', 'Auto', 'Scan'])) return 'Acquisition';
  if (starts(name, ['Device', 'UserSet', 'Camera', 'Temperature'])) return 'Device';
  if (starts(name, ['Gev', 'U3V', 'TL', 'Stream', 'Payload', 'Packet'])) return 'Transport';
  return 'Other';
};

const SETUP_ORDER = [
  'Width', 'Height', 'OffsetX', 'OffsetY', 'PixelFormat', 'Binning', 'Decimation', 'AcquisitionStart',
  'AcquisitionMode', 'AcquisitionFrameRate', 'ExposureAuto', 'ExposureTime', 'GainAuto', 'Gain'
];

/** format.rs `feature_rank`: where a feature sorts within its group. */
export const featureRank = (name) => {
  const at = SETUP_ORDER.findIndex((prefix) => name.startsWith(prefix));
  return at < 0 ? SETUP_ORDER.length : at;
};

/** The quick controls of an area-scan camera, in order (inspector.rs `quick_feature_rank`). */
export const QUICK_ORDER = Object.freeze([
  'ExposureAuto', 'ExposureTime', 'ExposureTimeAbs', 'GainAuto', 'Gain', 'GainRaw', 'GainAbs',
  'AcquisitionFrameRateEnable', 'AcquisitionFrameRate', 'AcquisitionFrameRateAbs'
]);

/** The quick controls among `features`, in QUICK_ORDER. */
export const quickFeatures = (features) => QUICK_ORDER
  .map((name) => features.find((feature) => feature.name === name))
  .filter(Boolean);

const ALIASES = {
  Width: 'image size dimensions resolution',
  Height: 'image size dimensions resolution',
  AcquisitionFrameRate: 'fps frame rate',
  AcquisitionFrameRateAbs: 'fps frame rate',
  ExposureTime: 'exposure duration shutter',
  ExposureTimeAbs: 'exposure duration shutter',
  PixelFormat: 'pixel format colour color',
  ColorTemperature: 'white balance colour color kelvin',
  BalanceWhiteAuto: 'white balance colour color',
  FocusDistance: 'focus lens',
  FocusAuto: 'focus lens autofocus',
  Torch: 'light flash lamp'
};

/** What a search matches for a feature: its name, label, description and aliases, lowercased. */
export const searchText = (feature) => [
  feature.name, feature.display_name ?? '', label(feature), feature.description ?? '', ALIASES[feature.name] ?? ''
].join('\n').toLowerCase();

/** Whether `feature` matches every word of `query`. */
export const matches = (feature, query) => {
  const terms = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  const text = searchText(feature);
  return terms.every((term) => text.includes(term));
};

// serde_json's number text: integers in full, other values in their shortest
// digits, with an exponent below 1e-5 or from 1e20.
const jsonNumber = (x) => {
  if (!Number.isFinite(x)) return 'null';
  if (Number.isInteger(x) && Math.abs(x) < 2 ** 64) return x === 0 ? '0' : display(x);
  const [mantissa, power] = x.toExponential().split('e');
  const exponent = Number(power);
  if (exponent < -5 || exponent >= 20) return `${mantissa}e${exponent < 0 ? '-' : '+'}${Math.abs(exponent)}`;
  return display(x);
};

/** format.rs `value_text`: a value as text that parses back; "—" for none. */
export const valueText = (value) => {
  if (value === null || value === undefined) return '—';
  if (typeof value === 'string') return value;
  if (typeof value === 'number') return jsonNumber(value);
  if (typeof value === 'boolean' || typeof value === 'bigint') return String(value);
  return JSON.stringify(value);
};

/** A GenICam unit as people write it: "us" → "µs". */
export const unitLabel = (unit) => (unit === 'us' || unit === 'usec' ? 'µs' : unit);

/** The unit a caption shows: "µs" for "us", and "fps" for a frame rate's "Hz". */
export const featureUnit = (feature) => {
  if (feature.unit == null) return null;
  const unit = unitLabel(feature.unit);
  return unit === 'Hz' && feature.name.includes('FrameRate') ? 'fps' : unit;
};

/** A bound in a range caption (format.rs `range_number`). */
export const rangeNumber = (input) => {
  const value = input === 0 ? 0 : input;
  const magnitude = Math.abs(value);
  if (magnitude >= 1e15 || (magnitude > 0 && magnitude < 0.001)) return exponential1(value).replace('.0e', 'e');
  if (fract(value) === 0 && magnitude >= 10000) return `${value < 0 ? '-' : ''}${grouped(magnitude)}`;
  return number(value);
};

/** A feature's range and unit, "10 – 1,000,000 µs"; empty when it has neither. */
export const rangeCaption = (feature) => {
  const { min, max } = feature;
  let range = '';
  if (min != null && max != null) range = `${rangeNumber(min)} – ${rangeNumber(max)}`;
  else if (min != null) range = `min ${rangeNumber(min)}`;
  else if (max != null) range = `max ${rangeNumber(max)}`;
  const unit = featureUnit(feature);
  if (unit == null) return range;
  return range === '' ? unit : `${range} ${unit}`;
};

/** A microsecond value in milliseconds, "= 10 ms"; null for other units or a sub-millisecond range. */
export const msReadout = (feature, value) => {
  if (feature.unit !== 'us' && feature.unit !== 'usec') return null;
  if (typeof value !== 'number' || !Number.isFinite(value)) return null;
  const top = feature.max ?? value;
  if (top < 1000) return null;
  const ms = value / 1000;
  const text = fract(ms) === 0 && Math.abs(ms) < 1e15 ? `${ms < 0 ? '-' : ''}${grouped(Math.abs(ms))}` : rangeNumber(ms);
  return `= ${text} ms`;
};

/** The caption under a writable field: "= 10 ms · 10 – 1,000,000 µs". */
export const caption = (feature, value) => {
  const range = rangeCaption(feature);
  const ms = msReadout(feature, value);
  if (ms === null) return range;
  return range === '' ? ms : `${ms} · ${range}`;
};

/** A draft the field cannot take, as the field says it: "Too low: use at least 10 µs", else `raw`. */
export const draftErrorText = (feature, draft, raw) => {
  const parsed = parseRustFloat(draft.trim());
  if (!('value' in parsed) || !Number.isFinite(parsed.value)) return raw;
  const value = parsed.value;
  const unit = featureUnit(feature);
  const suffix = unit == null ? '' : ` ${unit}`;
  if (feature.min != null && value < feature.min) return `Too low: use at least ${rangeNumber(feature.min)}${suffix}`;
  if (feature.max != null && value > feature.max) return `Too high: use at most ${rangeNumber(feature.max)}${suffix}`;
  return raw;
};

/** The one-line message a field shows for a draft: null when it is fine. */
export const draftError = (feature, draft) => {
  const raw = validateInput(feature, draft);
  return raw === null ? null : capitalize(firstLine(draftErrorText(feature, draft, raw)));
};

/** A feature's draft as a number, the way the desktop reads a field. */
export const draftNumber = (text) => {
  const parsed = parseRustFloat(String(text).trim());
  return 'value' in parsed && Number.isFinite(parsed.value) ? parsed.value : null;
};

// ---------------------------------------------------------------------------
// A quick control's slider (widgets.rs `FeatureScale`): log for exposure,
// snapped to the camera's increment or to a round figure.

export class FeatureScale {
  /** The scale for a feature, or null when its range is missing, empty or too wide. */
  static of(feature) {
    const { name, kind, min, max } = feature;
    if (min == null || max == null || !(Number.isFinite(min) && Number.isFinite(max) && min < max)) return null;
    const integer = ['Integer', 'IntReg', 'MaskedIntReg'].includes(kind);
    if (!integer && kind.toLowerCase() !== 'float') return null;
    const log = (name === 'ExposureTime' || name === 'ExposureTimeAbs') && min > 0;
    const fits = log ? max / min < 1e7 : max - min < 1e7;
    const inc = feature.inc != null && Number.isFinite(feature.inc) && feature.inc > 0 ? feature.inc : null;
    return fits ? new FeatureScale({ min, max, inc, integer, log }) : null;
  }

  constructor({ min, max, inc = null, integer = false, log = false }) {
    Object.assign(this, { min, max, inc, integer, log });
  }

  /** Where `value` sits, 0 at the minimum to 1 at the maximum. */
  position(value) {
    const v = Math.min(Math.max(value, this.min), this.max);
    const at = this.log ? Math.log(v / this.min) / Math.log(this.max / this.min) : (v - this.min) / (this.max - this.min);
    return Math.min(Math.max(at, 0), 1);
  }

  /** The value at `position`, snapped and within the range. */
  value(position) {
    const p = Math.min(Math.max(position, 0), 1);
    return this.snap(this.log ? this.min * (this.max / this.min) ** p : this.min + p * (this.max - this.min));
  }

  /** `value` on the camera's increment, or on a round figure. */
  snap(input) {
    const value = Math.min(Math.max(input, this.min), this.max);
    let snapped;
    if (this.inc !== null) snapped = this.min + round((value - this.min) / this.inc) * this.inc;
    else {
      const unit = this.unit(value);
      snapped = round(value / unit) * unit;
    }
    if (this.integer) snapped = round(snapped);
    let out;
    if (snapped > this.max) out = snapped - Math.max(this.inc ?? 0, this.integer ? 1 : 0);
    else if (snapped < this.min) out = this.min;
    else out = snapped;
    return Math.min(Math.max(out, this.min), this.max);
  }

  unit(value) {
    // f64::MIN_POSITIVE, the smallest normal number.
    const decade = this.log ? Math.max(Math.abs(value), 2.2250738585072014e-308) : this.max - this.min;
    const unit = 10 ** (Math.floor(Math.log10(decade)) - 2);
    return this.integer ? round(Math.max(unit, 1)) : unit;
  }

  /** One step up or down: 5% on a log scale, else the increment or a hundredth of the range. */
  step(value, up) {
    let next;
    if (this.log) next = up ? value * 1.05 : value / 1.05;
    else {
      const step = this.inc ?? (this.integer ? this.unit(value) : (this.max - this.min) / 100);
      next = up ? value + step : value - step;
    }
    let snapped = this.snap(next);
    if (snapped === this.snap(value)) {
      const nudge = this.inc ?? this.unit(value);
      if (nudge > 0) snapped = this.snap(up ? value + nudge : value - nudge);
    }
    return snapped;
  }

  /** `value` as a field holds it: plain digits that parse back. */
  text(value) {
    if (this.integer) return fixed(value, 0);
    const exact = (x) => {
      for (let p = 0; p <= 12; p += 1) {
        const scaled = Math.abs(x) * 10 ** p;
        if (Math.abs(scaled - round(scaled)) < 1e-9 * Math.max(scaled, 1)) return p;
      }
      return 12;
    };
    const places = Math.min(Math.max(Math.max(exact(this.inc ?? this.unit(value)), exact(this.min)), 3), 12);
    const text = fixed(value, places).replace(/0+$/, '').replace(/\.$/, '');
    return text === '-0' ? '0' : text;
  }
}

// ---------------------------------------------------------------------------
// Saved-capture provenance (engine.rs ACQUISITION_FEATURES, `acquisition`).

/** The acquisition features recorded with each saved still, when the camera has them. */
export const SETTINGS_ALLOWLIST = Object.freeze([
  'ExposureTime', 'ExposureTimeAbs', 'ExposureAuto', 'Gain', 'GainRaw', 'GainAuto', 'BlackLevel', 'Gamma',
  'BalanceWhiteAuto', 'AcquisitionFrameRate', 'AcquisitionFrameRateAbs', 'TriggerMode', 'TriggerSource',
  'BinningHorizontal', 'BinningVertical', 'OffsetX', 'OffsetY', 'ReverseX', 'ReverseY'
]);

/** Text without control characters, at most `max` characters (engine.rs `bounded`). */
export const bounded = (text, max) => [...String(text)].filter((c) => !/\p{Cc}/u.test(c)).slice(0, max).join('');

/**
 * The `settings` object of a capture's metadata: the allowlisted features'
 * readable values, keys sorted as serde_json writes them. Empty when none.
 */
export const metaSettings = (features) => {
  const settings = {};
  for (const name of [...SETTINGS_ALLOWLIST].sort()) {
    const feature = features.find((f) => f.name === name);
    if (!feature || feature.error) continue;
    const { value } = feature;
    if (typeof value === 'string') settings[name] = bounded(value, 64);
    else if ((typeof value === 'number' && Number.isFinite(value)) || typeof value === 'boolean') settings[name] = value;
  }
  return settings;
};

/** A FeatureInfo as the desktop serialises it: its twelve keys, in order. */
export const featureJson = (feature) => ({
  name: feature.name,
  display_name: feature.display_name ?? feature.name,
  kind: feature.kind,
  value: feature.value ?? null,
  writable: Boolean(feature.writable),
  description: feature.description ?? '',
  unit: feature.unit ?? null,
  min: feature.min ?? null,
  max: feature.max ?? null,
  inc: feature.inc ?? null,
  choices: feature.choices ?? [],
  error: feature.error ?? null
});

// ---------------------------------------------------------------------------
// Webcam controls: FeatureInfo objects built from a MediaStreamTrack's
// capabilities and settings, and writes through applyConstraints.

const MODE_NAMES = { manual: 'Off', 'single-shot': 'Once', continuous: 'Continuous' };
const MODE_VALUES = { Off: 'manual', Once: 'single-shot', Continuous: 'continuous' };
const MODE_ORDER = ['Off', 'Once', 'Continuous'];

// Each control: its feature name, constraint, kind of value ('number' is an
// Integer when its whole range is whole numbers), unit, and for a
// value an automatic mode sets, the mode feature that manages it. `basic`
// controls choose the video format; the rest are camera image controls.
const WEBCAM = [
  { name: 'Width', constraint: 'width', type: 'integer', unit: 'px', basic: true, description: 'Image width in pixels.' },
  { name: 'Height', constraint: 'height', type: 'integer', unit: 'px', basic: true, description: 'Image height in pixels.' },
  { name: 'AcquisitionFrameRate', constraint: 'frameRate', type: 'float', unit: 'Hz', basic: true, places: 3, description: 'Requested frames per second.' },
  { name: 'ResizeMode', constraint: 'resizeMode', type: 'enum', basic: true, description: 'Whether the browser may crop and scale the camera image to the requested size.' },
  { name: 'ExposureAuto', constraint: 'exposureMode', type: 'mode', description: 'Whether the camera sets its exposure itself.' },
  { name: 'ExposureTime', constraint: 'exposureTime', type: 'float', unit: 'us', scale: 100, auto: 'ExposureAuto', description: 'Exposure time. The browser sets it in steps of 100 µs.' },
  { name: 'ExposureCompensation', constraint: 'exposureCompensation', type: 'float', unit: 'EV', description: 'Brightness the automatic exposure aims for, in exposure values.' },
  { name: 'ISO', constraint: 'iso', type: 'number', description: 'Sensor sensitivity.' },
  { name: 'FocusAuto', constraint: 'focusMode', type: 'mode', description: 'Whether the camera focuses itself.' },
  { name: 'FocusDistance', constraint: 'focusDistance', type: 'float', unit: 'm', auto: 'FocusAuto', description: 'Focus distance in metres.' },
  { name: 'BalanceWhiteAuto', constraint: 'whiteBalanceMode', type: 'mode', description: 'Whether the camera sets its white balance itself.' },
  { name: 'ColorTemperature', constraint: 'colorTemperature', type: 'number', unit: 'K', auto: 'BalanceWhiteAuto', description: 'White balance colour temperature.' },
  { name: 'Brightness', constraint: 'brightness', type: 'number', description: '' },
  { name: 'Contrast', constraint: 'contrast', type: 'number', description: '' },
  { name: 'Saturation', constraint: 'saturation', type: 'number', description: '' },
  { name: 'Sharpness', constraint: 'sharpness', type: 'number', description: '' },
  { name: 'Zoom', constraint: 'zoom', type: 'number', description: '' },
  { name: 'Pan', constraint: 'pan', type: 'number', description: '' },
  { name: 'Tilt', constraint: 'tilt', type: 'number', description: '' },
  { name: 'Torch', constraint: 'torch', type: 'boolean', description: 'The camera light.' },
  { name: 'BackgroundBlur', constraint: 'backgroundBlur', type: 'boolean', readOnly: true, description: 'Background blur the system applies to this camera.' },
  { name: 'FacingMode', constraint: 'facingMode', type: 'enum', readOnly: true, description: 'Which way the camera faces.' }
];
const SPEC = Object.fromEntries(WEBCAM.map((spec) => [spec.name, spec]));
// Modes first, so a camera is in manual mode before the value it then takes.
const ADVANCED_ORDER = WEBCAM.filter((spec) => !spec.basic).sort((a, b) => (b.type === 'mode') - (a.type === 'mode')).map((spec) => spec.constraint);

// What each track was asked for from here: { format: Map of video-format
// constraints set exactly, controls: Map of image-control constraints }.
const applied = new WeakMap();

const attempt = (fn, fallback) => {
  try {
    return fn() ?? fallback;
  } catch {
    return fallback;
  }
};

const cleanNumber = (value, spec) => {
  if (typeof value !== 'number' || !Number.isFinite(value)) return null;
  let v = spec.scale ? Number((value * spec.scale).toPrecision(15)) : value;
  if (spec.places !== undefined) v = Math.round(v * 10 ** spec.places) / 10 ** spec.places;
  return v;
};

const blank = (spec) => ({
  name: spec.name,
  display_name: spec.name,
  kind: 'String',
  value: null,
  writable: false,
  description: spec.description,
  unit: null,
  min: null,
  max: null,
  inc: null,
  choices: [],
  error: null,
  locked_while_streaming: false,
  managed_by_auto: false,
  constraint: spec.constraint
});

const webcamFeature = (spec, caps, settings) => {
  const range = caps[spec.constraint];
  const current = settings[spec.constraint];
  const feature = blank(spec);
  if (spec.type === 'mode') {
    if (!Array.isArray(range)) return null;
    const choices = MODE_ORDER.filter((choice) => range.includes(MODE_VALUES[choice]));
    if (choices.length === 0) return null;
    return { ...feature, kind: 'Enumeration', value: MODE_NAMES[current] ?? null, choices, writable: !spec.readOnly && choices.length > 1 };
  }
  if (spec.type === 'enum') {
    const choices = Array.isArray(range) ? range.filter((choice) => typeof choice === 'string') : [];
    const value = typeof current === 'string' && current !== '' ? current : null;
    if (choices.length === 0 && value === null) return null;
    return { ...feature, kind: 'Enumeration', value, choices, writable: !spec.readOnly && choices.length > 1 };
  }
  if (spec.type === 'boolean') {
    const supported = range === true || (Array.isArray(range) && range.length > 0);
    if (!supported && typeof current !== 'boolean') return null;
    const settable = range === true || (Array.isArray(range) && range.includes(true) && range.includes(false));
    return { ...feature, kind: 'Boolean', value: typeof current === 'boolean' ? current : null, writable: !spec.readOnly && settable };
  }
  // A number: a {min, max, step} range, or only a current value.
  const value = cleanNumber(current, spec);
  const ranged = range && typeof range === 'object' && !Array.isArray(range);
  if (!ranged && value === null) return null;
  const min = ranged ? cleanNumber(range.min, spec) : null;
  const max = ranged ? cleanNumber(range.max, spec) : null;
  const step = ranged ? cleanNumber(range.step, spec) : null;
  const inc = step !== null && step > 0 ? step : null;
  const whole = (v) => v === null || Number.isInteger(v);
  const integer = spec.type === 'integer' || (spec.type === 'number' && min !== null && max !== null && [min, max, inc, value].every(whole));
  const writable = !spec.readOnly && min !== null && max !== null && min < max;
  return { ...feature, kind: integer ? 'Integer' : 'Float', value, unit: spec.unit ?? null, min, max, inc, writable };
};

/**
 * A webcam's controls as FeatureInfo objects, sorted by name: what the
 * browser reports this camera can do, with the values it has now.
 */
export const webcamFeatures = (track) => {
  if (!track) return [];
  const caps = attempt(() => track.getCapabilities?.(), {});
  const settings = attempt(() => track.getSettings?.(), {});
  const features = WEBCAM.map((spec) => webcamFeature(spec, caps, settings)).filter(Boolean);
  for (const feature of features) {
    const mode = SPEC[feature.name].auto;
    if (mode) feature.managed_by_auto = features.some((f) => f.name === mode && f.value === 'Continuous');
  }
  return features.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
};

const parseBoolean = (text) => ['true', '1', 'on'].includes(text.trim().toLowerCase());

const constraintValue = (feature, spec, text) => {
  if (spec.type === 'mode') return MODE_VALUES[text];
  if (spec.type === 'enum') return text;
  if (spec.type === 'boolean') return parseBoolean(text);
  const parsed = integerKind(feature.kind) ? integerLiteral(text) : literal(text);
  const value = Number(parsed.value);
  return spec.scale ? value / spec.scale : value;
};

const applyError = (error) => {
  switch (error?.name) {
    case 'OverconstrainedError':
      return 'the camera cannot use this value';
    case 'NotReadableError':
      return 'the camera is busy or unavailable';
    case 'NotAllowedError':
      return 'the browser did not allow this change';
    default:
      return error?.message || String(error);
  }
};

// A write replaces the track's constraints, so each one restates the video
// format: what was set here exactly, the rest as it is now, on this device.
const basicConstraints = (track, format) => {
  const settings = attempt(() => track.getSettings?.(), {});
  const basic = {};
  if (typeof settings.deviceId === 'string' && settings.deviceId) basic.deviceId = { exact: settings.deviceId };
  for (const key of ['width', 'height', 'frameRate', 'resizeMode']) {
    if (format.has(key)) basic[key] = { exact: format.get(key) };
    else if (settings[key] !== undefined && settings[key] !== '') basic[key] = { ideal: settings[key] };
  }
  return basic;
};

const advancedConstraints = (controls) => ADVANCED_ORDER.filter((key) => controls.has(key)).map((key) => ({ [key]: controls.get(key) }));

/** Whether the camera now reports `value` for `key`, to within its step. */
const honoured = (track, key, value) => {
  const now = attempt(() => track.getSettings?.(), {})[key];
  // A one-off adjustment may report itself done as manual.
  if (value === 'single-shot') return now === 'single-shot' || now === 'manual';
  if (typeof value !== 'number' || typeof now !== 'number') return now === value;
  const step = attempt(() => track.getCapabilities?.(), {})[key]?.step;
  return Math.abs(now - value) <= Math.max(typeof step === 'number' ? step : 0, Math.abs(value) * 1e-6);
};

/**
 * What was asked of `track` from here ({ format, controls }, as Maps), or null; kept
 * on the camera so a stop and start can restate it on the new track.
 */
export const webcamAsked = (track) => {
  const asked = track ? applied.get(track) : null;
  return asked ? { format: new Map(asked.format), controls: new Map(asked.controls) } : null;
};

/**
 * Restate `asked` (from webcamAsked) on a new track of the same camera: the video
 * format first, alone, then the image controls. Returns false when the camera does
 * not report every value afterwards; what it did take is what later writes build on.
 */
export const restoreWebcamSettings = async (track, asked) => {
  if (!track || track.readyState === 'ended' || !asked) return true;
  const format = new Map(asked.format);
  const controls = new Map(asked.controls);
  if (format.size === 0 && controls.size === 0) return true;
  try {
    if (format.size > 0) await track.applyConstraints(basicConstraints(track, format));
  } catch {
    format.clear();
  }
  const advanced = advancedConstraints(controls);
  if (advanced.length > 0) await track.applyConstraints({ ...basicConstraints(track, format), advanced }).catch(() => {});
  let all = asked.format.size === format.size;
  for (const [map, keys] of [[format, [...format.keys()]], [controls, [...controls.keys()]]]) {
    for (const key of keys) {
      if (honoured(track, key, map.get(key))) continue;
      map.delete(key);
      all = false;
    }
  }
  applied.set(track, { format, controls });
  return all;
};

/**
 * Write `text` to feature `name` of a webcam, checked as the desktop checks a
 * draft. A value an automatic mode sets is refused rather than switching the
 * mode silently. Returns the value the camera reports afterwards.
 */
export const applyWebcamFeature = async (track, name, text) => {
  const fail = (cause) => new Error(`cannot set ${name} to "${text}": ${cause}`);
  if (!track || track.readyState === 'ended') throw fail('the camera has stopped; start it again');
  const features = webcamFeatures(track);
  const feature = features.find((f) => f.name === name);
  if (!feature) throw fail(`this camera has no ${name} feature`);
  if (!feature.writable) throw fail(`${name} is read-only`);
  const problem = validateInput(feature, text);
  if (problem !== null) throw fail(problem);
  const spec = SPEC[name];
  if (feature.managed_by_auto) {
    const mode = features.find((f) => f.name === spec.auto);
    throw fail(`turn off ${label(mode)} to set ${label(feature)}`);
  }
  const value = constraintValue(feature, spec, text);
  const asked = applied.get(track) ?? { format: new Map(), controls: new Map() };
  const format = new Map(asked.format);
  const controls = new Map(asked.controls);
  if (spec.basic) format.set(spec.constraint, value);
  else controls.set(spec.constraint, value);
  if (spec.type === 'mode' && value === 'continuous') {
    // The camera takes the value back; do not send it the old manual one.
    for (const managed of WEBCAM) if (managed.auto === name) controls.delete(managed.constraint);
  }
  const basic = basicConstraints(track, format);
  const advanced = advancedConstraints(controls);
  try {
    if (spec.basic) {
      // Chrome applies image controls through a separate path that ignores the
      // video format, so a format change goes alone; the controls follow it.
      await track.applyConstraints(basic);
      if (advanced.length > 0) await track.applyConstraints({ ...basic, advanced }).catch(() => {});
    } else {
      await track.applyConstraints({ ...basic, advanced });
    }
  } catch (error) {
    throw fail(applyError(error));
  }
  applied.set(track, { format, controls });
  const after = webcamFeatures(track).find((f) => f.name === name);
  return { name, value: after ? after.value : null };
};
