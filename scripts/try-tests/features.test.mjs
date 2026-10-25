// Feature checks, captions, labels and webcam controls of web/try/features.mjs,
// against the desktop app's own outputs. Expected texts were produced by the Rust
// sources they port (src/genicam.rs, src/gui/format.rs, src/gui/widgets.rs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  FeatureScale, QUICK_ORDER, SETTINGS_ALLOWLIST, applyWebcamFeature, caption, display, draftError, exponential1,
  featureJson, featureRank, fixed, groupOf, label, matches, metaSettings, msReadout, quickFeatures, rangeCaption,
  rangeNumber, streamLocked, validateInput, valueText, webcamFeatures, words
} from '../../web/try/features.mjs';

const feature = (fields) => ({
  display_name: '', value: null, writable: true, description: '', unit: null, min: null, max: null, inc: null,
  choices: [], error: null, ...fields
});

const WIDTH = feature({ name: 'Width', kind: 'Integer', min: 64, max: 1920, inc: 1 });
const EXPOSURE = feature({ name: 'ExposureTime', kind: 'Float', min: 10, max: 1000000, unit: 'us' });
const GAIN_STEP = feature({ name: 'Gain', kind: 'Integer', min: 0, max: 24, inc: 2 });
const BLACK = feature({ name: 'Black', kind: 'Float', min: 0.5, max: 10, inc: 0.1 });
const PIXEL_FORMAT = feature({ name: 'PixelFormat', kind: 'Enumeration', choices: ['Mono8', 'RGB8', 'Mono12', 'BayerRG8'] });
const TORCH = feature({ name: 'Torch', kind: 'Boolean' });
const START = feature({ name: 'AcquisitionStart', kind: 'Command' });
const USER_NAME = feature({ name: 'UserName', kind: 'StringReg' });
const OFFSET = feature({ name: 'Offset', kind: 'Integer', inc: 4 });
const ODD = feature({ name: 'Odd', kind: 'Integer', min: 0.5, max: 10 });
const BAD = feature({ name: 'Bad', kind: 'Float', min: 10, max: 1 });
const BIG = feature({ name: 'Big', kind: 'Integer', min: -9007199254740993, max: 9223372036854775807, inc: 1 });

// [feature, draft, FeatureInfo::validate_input as `{:#}`, the field's one-line caption]
const DRAFTS = [
  [WIDTH, '9999', 'Width violates Max=1920; enter 1920 or less', 'Too high: use at most 1920'],
  [WIDTH, '63', 'Width violates Min=64; enter 64 or greater', 'Too low: use at least 64'],
  [WIDTH, '640', null, null],
  [WIDTH, '640.0', null, null],
  [WIDTH, '6.4e2', null, null],
  [WIDTH, '0x280', null, null],
  [WIDTH, '0xg', 'Width requires an exact integer: invalid hexadecimal integer or integer overflow: invalid digit found in string', 'Width requires an exact integer: invalid hexadecimal integer or integer overflow: invalid digit found in string'],
  [WIDTH, '0x', 'Width requires an exact integer: invalid hexadecimal integer or integer overflow: cannot parse integer from empty string', 'Width requires an exact integer: invalid hexadecimal integer or integer overflow: cannot parse integer from empty string'],
  [WIDTH, ' 640 ', null, null],
  [WIDTH, '', 'Width requires an exact integer: expected a decimal or hexadecimal integer', 'Width requires an exact integer: expected a decimal or hexadecimal integer'],
  [WIDTH, 'fast', 'Width requires an exact integer: expected a decimal or hexadecimal integer', 'Width requires an exact integer: expected a decimal or hexadecimal integer'],
  [WIDTH, '1.5', 'Width requires an exact integer: fractional values are not allowed', 'Too low: use at least 64'],
  [WIDTH, '1e', 'Width requires an exact integer: invalid integer exponent: cannot parse integer from empty string', 'Width requires an exact integer: invalid integer exponent: cannot parse integer from empty string'],
  [WIDTH, '1e5e6', 'Width requires an exact integer: invalid integer exponent', 'Width requires an exact integer: invalid integer exponent'],
  [WIDTH, '99999999999999999999', 'Width requires an exact integer: integer overflow: number too large to fit in target type', 'Too high: use at most 1920'],
  [WIDTH, '18446744073709551616', 'Width requires an exact integer: integer overflow: number too large to fit in target type', 'Too high: use at most 1920'],
  [WIDTH, '-9223372036854775809', 'Width requires an exact integer: signed integer overflow', 'Too low: use at least 64'],
  [WIDTH, '1e21', 'Width requires an exact integer: integer overflow', 'Too high: use at most 1920'],
  [WIDTH, '-0', 'Width violates Min=64; enter 64 or greater', 'Too low: use at least 64'],
  [WIDTH, '+5', 'Width violates Min=64; enter 64 or greater', 'Too low: use at least 64'],
  [EXPOSURE, '5', 'ExposureTime violates Min=10; enter 10 or greater', 'Too low: use at least 10 µs'],
  [EXPOSURE, '2000000', 'ExposureTime violates Max=1000000; enter 1000000 or less', 'Too high: use at most 1,000,000 µs'],
  [EXPOSURE, '15000', null, null],
  [EXPOSURE, 'fast', 'ExposureTime requires a finite number: invalid numeric value: invalid float literal', 'ExposureTime requires a finite number: invalid numeric value: invalid float literal'],
  [EXPOSURE, '', 'ExposureTime requires a finite number: invalid numeric value: cannot parse float from empty string', 'ExposureTime requires a finite number: invalid numeric value: cannot parse float from empty string'],
  [EXPOSURE, 'inf', 'ExposureTime requires a finite number: expected finite number', 'ExposureTime requires a finite number: expected finite number'],
  [EXPOSURE, 'NaN', 'ExposureTime requires a finite number: expected finite number', 'ExposureTime requires a finite number: expected finite number'],
  [EXPOSURE, '1e400', 'ExposureTime requires a finite number: expected finite number', 'ExposureTime requires a finite number: expected finite number'],
  [EXPOSURE, '1e-400', 'ExposureTime requires a finite number: numeric value is too small to represent; enter 0 explicitly or a representable nonzero value', 'Too low: use at least 10 µs'],
  [EXPOSURE, '0e-400', 'ExposureTime violates Min=10; enter 10 or greater', 'Too low: use at least 10 µs'],
  [EXPOSURE, '1_000', 'ExposureTime requires a finite number: invalid numeric value: invalid float literal', 'ExposureTime requires a finite number: invalid numeric value: invalid float literal'],
  [EXPOSURE, '0b101', 'ExposureTime requires a finite number: invalid numeric value: invalid float literal', 'ExposureTime requires a finite number: invalid numeric value: invalid float literal'],
  [EXPOSURE, '.5', 'ExposureTime violates Min=10; enter 10 or greater', 'Too low: use at least 10 µs'],
  [EXPOSURE, '5.', 'ExposureTime violates Min=10; enter 10 or greater', 'Too low: use at least 10 µs'],
  [EXPOSURE, '1e1000000000000', 'ExposureTime requires a finite number: expected finite number', 'ExposureTime requires a finite number: expected finite number'],
  [EXPOSURE, '0x+10', null, null],
  [EXPOSURE, '0xzz', 'ExposureTime requires a finite number: invalid hexadecimal integer or integer overflow: invalid digit found in string', 'ExposureTime requires a finite number: invalid hexadecimal integer or integer overflow: invalid digit found in string'],
  [GAIN_STEP, '3', 'Gain requires increment 2 from 0', 'Gain requires increment 2 from 0'],
  [GAIN_STEP, '4', null, null],
  [GAIN_STEP, '-1', 'Gain violates Min=0; enter 0 or greater', 'Too low: use at least 0'],
  [GAIN_STEP, '26', 'Gain violates Max=24; enter 24 or less', 'Too high: use at most 24'],
  [OFFSET, '-3', 'Offset requires increment 4 from 0', 'Offset requires increment 4 from 0'],
  [OFFSET, '8', null, null],
  [BLACK, '0.3', 'Black violates Min=0.5; enter 0.5 or greater', 'Too low: use at least 0.5'],
  [BLACK, '0.30000000001', 'Black violates Min=0.5; enter 0.5 or greater', 'Too low: use at least 0.5'],
  [BLACK, '10.05', 'Black violates Max=10; enter 10 or less', 'Too high: use at most 10'],
  [BLACK, '5.25', 'Black requires increment 0.1 from 0.5; nearest value is 5.300000000000001', 'Black requires increment 0.1 from 0.5; nearest value is 5.300000000000001'],
  [BLACK, '0.6', null, null],
  [PIXEL_FORMAT, 'Mono8', null, null],
  [PIXEL_FORMAT, 'mono8', 'PixelFormat must be one of: Mono8, RGB8, Mono12, BayerRG8', 'PixelFormat must be one of: Mono8, RGB8, Mono12, BayerRG8'],
  [PIXEL_FORMAT, 'Mono8 ', 'PixelFormat must be one of: Mono8, RGB8, Mono12, BayerRG8', 'PixelFormat must be one of: Mono8, RGB8, Mono12, BayerRG8'],
  [TORCH, 'On', null, null],
  [TORCH, 'no', 'Torch must be true or false (also accepts 1/0 and on/off)', 'Torch must be true or false (also accepts 1/0 and on/off)'],
  [TORCH, ' 0 ', null, null],
  [START, '1', 'use execute for command AcquisitionStart', 'Use execute for command AcquisitionStart'],
  [USER_NAME, 'a\u0000b', 'UserName cannot contain a NUL character', 'UserName cannot contain a NUL character'],
  [USER_NAME, 'ok', null, null],
  [ODD, '3', 'expected an exactly representable integer', 'Expected an exactly representable integer'],
  [BAD, '5', 'Bad has invalid camera bounds', 'Too low: use at least 10'],
  [BIG, '9223372036854775807', null, null]
];

test('drafts are checked with the desktop wording', () => {
  for (const [f, draft, raw, shown] of DRAFTS) {
    assert.equal(validateInput(f, draft), raw, `${f.name} ${JSON.stringify(draft)}`);
    assert.equal(draftError(f, draft), shown, `${f.name} ${JSON.stringify(draft)} caption`);
  }
});

test('simulator setter messages read as the desktop writes them', () => {
  const height = feature({ name: 'Height', kind: 'Integer', min: 64, max: 1080, inc: 1 });
  assert.equal(validateInput(WIDTH, '9999'), 'Width violates Max=1920; enter 1920 or less');
  assert.equal(validateInput(height, '32'), 'Height violates Min=64; enter 64 or greater');
  const gain = feature({ name: 'Gain', kind: 'Float', min: 0, max: 24, unit: 'dB' });
  assert.equal(validateInput(gain, '30'), 'Gain violates Max=24; enter 24 or less');
  assert.equal(validateInput(PIXEL_FORMAT, 'X'), 'PixelFormat must be one of: Mono8, RGB8, Mono12, BayerRG8');
});

test('Rust number formatting rounds exact ties to even', () => {
  assert.equal(fixed(0.0625, 3), '0.062');
  assert.equal(fixed(2.5, 0), '2');
  assert.equal(fixed(3.5, 0), '4');
  assert.equal(fixed(-0.04, 1), '-0.0');
  assert.equal(fixed(1e21, 1), '1000000000000000000000.0');
  assert.equal(exponential1(1.25e16), '1.2e16');
  assert.equal(exponential1(9.96e-5), '1.0e-4');
  assert.equal(display(1e21), '1000000000000000000000');
  assert.equal(display(1e-7), '0.0000001');
  assert.equal(display(0.5), '0.5');
});

test('ranges read with grouped digits and real units (format.rs tests)', () => {
  const exposure = (min, max, unit) => feature({ name: 'ExposureTime', kind: 'Float', min, max, unit });
  assert.equal(rangeCaption(exposure(10, 1000000, 'us')), '10 – 1,000,000 µs');
  assert.equal(rangeCaption(exposure(0, 24, 'dB')), '0 – 24 dB');
  assert.equal(rangeCaption(exposure(-20000, 0.5, null)), '-20,000 – 0.5');
  assert.equal(rangeCaption(exposure(null, 9999, null)), 'max 9999');
  assert.equal(rangeCaption(exposure(null, null, 'Hz')), 'Hz');
  assert.equal(rangeCaption(exposure(null, null, null)), '');
  assert.equal(rangeCaption(exposure(-0, 3.4028234663852886e38, null)), '0 – 3.4e38');
  assert.equal(rangeNumber(1e20), '1e20');
  assert.equal(rangeNumber(3.4028235e38), '3.4e38');
  assert.equal(rangeNumber(9223372036854775807), '9.2e18');
  assert.equal(rangeNumber(-0), '0');
  assert.equal(rangeNumber(0.0001), '1e-4');
  assert.equal(rangeNumber(-0.0004), '-4e-4');
  assert.equal(rangeNumber(0.001), '0.001');
  assert.equal(rangeNumber(999999999999999), '999,999,999,999,999');
  const rate = feature({ name: 'AcquisitionFrameRate', kind: 'Float', min: 1, max: 120, unit: 'Hz' });
  assert.equal(rangeCaption(rate), '1 – 120 fps');
  assert.equal(rangeCaption({ ...rate, name: 'LineTriggerRate' }), '1 – 120 Hz');
  assert.equal(rangeCaption(feature({ name: 'Width', kind: 'Integer', min: 64, max: 1920, unit: 'px' })), '64 – 1920 px', 'grouped only from 10,000');
});

test('long exposures add their value in milliseconds', () => {
  const exposure = (unit, max = 1000000) => feature({ name: 'ExposureTime', kind: 'Float', min: 10, max, unit });
  assert.equal(msReadout(exposure('us'), 10000), '= 10 ms');
  assert.equal(msReadout(exposure('usec'), 12345), '= 12.345 ms');
  assert.equal(msReadout(exposure('us'), 1500000), '= 1,500 ms');
  assert.equal(msReadout(exposure('ms'), 10000), null);
  assert.equal(msReadout(exposure('us', 999), 500), null, 'a sub-millisecond range stays in µs');
  assert.equal(caption(exposure('us'), 10000), '= 10 ms · 10 – 1,000,000 µs');
  assert.equal(caption(feature({ name: 'Gain', kind: 'Float', min: 0, max: 24, unit: 'dB' }), 0), '0 – 24 dB');
});

test('values read as text that parses back (format.rs value_text)', () => {
  assert.equal(valueText(10000), '10000');
  assert.equal(valueText(30), '30');
  assert.equal(valueText(-0), '0');
  assert.equal(valueText(-4), '-4');
  assert.equal(valueText(2.5), '2.5');
  assert.equal(valueText(1e21), '1e+21');
  assert.equal(valueText(1e-6), '1e-6');
  assert.equal(valueText(0.00001), '0.00001');
  assert.equal(valueText('Mono8'), 'Mono8');
  assert.equal(valueText(true), 'true');
  assert.equal(valueText(null), '—');
});

test('names read as words, groups and setup order follow the desktop', () => {
  assert.equal(words('AcquisitionFrameRate'), 'Acquisition Frame Rate');
  assert.equal(words('GevSCPSPacketSize'), 'Gev SCPS Packet Size');
  assert.equal(words('OffsetX'), 'Offset X');
  assert.equal(words('U3VMaxPacket'), 'U3V Max Packet');
  assert.equal(words('Device_Temperature'), 'Device Temperature');
  assert.equal(label({ name: 'ExposureTime', display_name: 'ExposureTime' }), 'Exposure');
  assert.equal(label({ name: 'AcquisitionFrameRate', display_name: '' }), 'Frame rate');
  assert.equal(label({ name: 'ExposureAuto', display_name: '' }), 'Auto exposure');
  assert.equal(label({ name: 'DeviceScanType', display_name: 'DeviceScanType' }), 'Device Scan Type');
  assert.equal(label({ name: 'Foo', display_name: 'Custom name' }), 'Custom name');
  assert.equal(groupOf('Width'), 'Image');
  assert.equal(groupOf('AutoExposureTimeUpperLimit'), 'Acquisition');
  assert.equal(groupOf('DeviceScanType'), 'Device');
  assert.equal(groupOf('GevSCPSPacketSize'), 'Transport');
  assert.equal(groupOf('Brightness'), 'Other');
  const sorted = (names) => [...names].sort((a, b) => featureRank(a) - featureRank(b));
  assert.deepEqual(sorted(['Height', 'PixelFormat', 'Width', 'TestPattern', 'OffsetX', 'WidthMax']),
    ['Width', 'WidthMax', 'Height', 'OffsetX', 'PixelFormat', 'TestPattern']);
  assert.deepEqual(sorted(['Gain', 'AcquisitionStart', 'ExposureTime', 'AcquisitionFrameRate', 'GainAuto', 'ExposureAuto', 'BlackLevel']),
    ['AcquisitionStart', 'AcquisitionFrameRate', 'ExposureAuto', 'ExposureTime', 'GainAuto', 'Gain', 'BlackLevel']);
  assert.ok(streamLocked('PixelFormat') && !streamLocked('Gain'));
});

// The Pattern camera's features as NodeMap::features returns them (simulator.md §2).
const SIM = [
  feature({ name: 'AcquisitionFrameRate', kind: 'Float', value: 30, min: 1, max: 120, unit: 'Hz', description: 'Requested frames per second. Exposure may limit the frame rate.' }),
  feature({ name: 'AcquisitionStart', kind: 'Command' }),
  feature({ name: 'AcquisitionStop', kind: 'Command' }),
  feature({ name: 'DeviceScanType', kind: 'Enumeration', value: 'Areascan', writable: false, choices: ['Areascan', 'Linescan'] }),
  feature({ name: 'ExposureTime', kind: 'Float', value: 10000, min: 10, max: 1000000, unit: 'us' }),
  feature({ name: 'Gain', kind: 'Float', value: 0, min: 0, max: 24, unit: 'dB' }),
  feature({ name: 'Height', kind: 'Integer', value: 480, min: 64, max: 1080, inc: 1 }),
  feature({ name: 'PixelFormat', kind: 'Enumeration', value: 'Mono8', choices: ['Mono8', 'RGB8', 'Mono12', 'BayerRG8'] }),
  feature({ name: 'Width', kind: 'Integer', value: 640, min: 64, max: 1920, inc: 1 })
];

test('quick controls and search follow the desktop inspector', () => {
  assert.deepEqual(QUICK_ORDER.slice(0, 3), ['ExposureAuto', 'ExposureTime', 'ExposureTimeAbs']);
  assert.deepEqual(quickFeatures(SIM).map((f) => f.name), ['ExposureTime', 'Gain', 'AcquisitionFrameRate']);
  const found = (query) => SIM.filter((f) => matches(f, query)).map((f) => f.name);
  assert.deepEqual(found('resolution'), ['Height', 'Width']);
  assert.deepEqual(found('fps'), ['AcquisitionFrameRate']);
  assert.deepEqual(found('shutter'), ['ExposureTime']);
  assert.deepEqual(found('scan type'), ['DeviceScanType']);
  assert.deepEqual(found('colour'), ['PixelFormat']);
});

test('saved stills record only the allowlisted settings', () => {
  assert.equal(SETTINGS_ALLOWLIST.length, 19);
  assert.deepEqual(metaSettings(SIM), { AcquisitionFrameRate: 30, ExposureTime: 10000, Gain: 0 });
  assert.deepEqual(Object.keys(metaSettings([
    feature({ name: 'Gain', value: 1 }), feature({ name: 'BalanceWhiteAuto', value: 'Continuous' }),
    feature({ name: 'ExposureAuto', value: 'Off' }), feature({ name: 'Width', value: 640 }),
    feature({ name: 'ExposureTime', value: null }), feature({ name: 'GainAuto', value: 'x', error: 'unreadable' }),
    feature({ name: 'TriggerMode', value: `On\u0007${'x'.repeat(80)}` })
  ])), ['BalanceWhiteAuto', 'ExposureAuto', 'Gain', 'TriggerMode']);
  const bounded = metaSettings([feature({ name: 'TriggerSource', value: `\u0000${'y'.repeat(80)}` })]);
  assert.equal(bounded.TriggerSource, 'y'.repeat(64));
  assert.deepEqual(metaSettings([]), {});
});

test('feature JSON keeps the desktop field order and drops browser extras', () => {
  const json = featureJson({ ...SIM[4], locked_while_streaming: false, constraint: 'exposureTime' });
  assert.deepEqual(Object.keys(json), ['name', 'display_name', 'kind', 'value', 'writable', 'description', 'unit', 'min', 'max', 'inc', 'choices', 'error']);
  assert.equal(JSON.stringify(json), '{"name":"ExposureTime","display_name":"","kind":"Float","value":10000,"writable":true,"description":"","unit":"us","min":10,"max":1000000,"inc":null,"choices":[],"error":null}');
});

test('quick sliders scale and snap as the desktop slider does', () => {
  const exposure = FeatureScale.of(SIM[4]);
  assert.ok(exposure.log);
  assert.equal(exposure.position(10000), 0.6);
  assert.equal(exposure.snap(500005), 500000);
  assert.equal(exposure.step(10000, true), 10500);
  assert.equal(exposure.step(10000, false), 9520);
  assert.equal(exposure.text(500005), '500005');
  const gain = FeatureScale.of(SIM[5]);
  assert.deepEqual([0, 24, 12, 0.0625, 13.37].map((v) => gain.snap(v)), [0, 24, 12, 0.1, 13.4]);
  assert.deepEqual([0, 12, 13.37].map((v) => gain.step(v, true)), [0.2, 12.200000000000001, 13.600000000000001]);
  assert.deepEqual([0.0625, 13.37].map((v) => gain.text(v)), ['0.062', '13.37']);
  const width = FeatureScale.of({ ...SIM[8], inc: 16 });
  assert.deepEqual([500, 1919].map((v) => width.snap(v)), [496, 1920]);
  assert.deepEqual([0, 0.25, 0.5, 1].map((p) => width.value(p)), [64, 528, 992, 1920]);
  assert.equal(FeatureScale.of(SIM[7]), null, 'enumerations have no slider');
  assert.equal(FeatureScale.of(feature({ name: 'Huge', kind: 'Float', min: 0, max: 1e8 })), null, 'too wide');
});

// A MediaStreamTrack stand-in: capabilities, settings, and applyConstraints that
// applies exact and ideal values and advanced sets to the settings, as a camera
// that can deliver what it is asked for.
const fakeTrack = (caps, settings, { reject = null } = {}) => {
  const state = { settings: structuredClone(settings) };
  return {
    readyState: 'live',
    calls: [],
    getCapabilities: () => structuredClone(caps),
    getSettings: () => structuredClone(state.settings),
    async applyConstraints(constraints) {
      this.calls.push(structuredClone(constraints));
      if (reject) throw reject;
      for (const [key, value] of Object.entries(constraints)) {
        if (key === 'advanced' || key === 'deviceId') continue;
        state.settings[key] = value && typeof value === 'object' ? value.exact ?? value.ideal : value;
      }
      for (const set of constraints.advanced ?? []) Object.assign(state.settings, set);
    }
  };
};

// What a webcam with image controls reports (the fake device stream offers these).
const CAPS = {
  deviceId: 'cam', groupId: 'g', aspectRatio: { min: 0.0007, max: 1280 },
  width: { min: 1, max: 1280 }, height: { min: 1, max: 720 }, frameRate: { min: 1, max: 30.000030517578125 },
  facingMode: [], resizeMode: ['none', 'crop-and-scale'],
  exposureMode: ['continuous', 'manual'], exposureTime: { min: 0.1, max: 10000, step: 0.1 },
  focusMode: ['continuous', 'manual', 'single-shot'], focusDistance: { min: 0, max: 10, step: 0.1 },
  whiteBalanceMode: ['continuous', 'manual'], colorTemperature: { min: 2500, max: 6500, step: 1 },
  brightness: { min: -64, max: 64, step: 1 }, torch: true, backgroundBlur: [false]
};
const SETTINGS = {
  deviceId: 'cam', groupId: 'g', width: 1280, height: 720, frameRate: 30.000030517578125, resizeMode: 'none',
  exposureMode: 'continuous', exposureTime: 333.3, focusMode: 'manual', focusDistance: 1.5,
  whiteBalanceMode: 'continuous', colorTemperature: 4600, brightness: 0, torch: false, backgroundBlur: false
};

test('webcam capabilities become FeatureInfo objects', () => {
  const features = webcamFeatures(fakeTrack(CAPS, SETTINGS));
  assert.deepEqual(features.map((f) => f.name), [
    'AcquisitionFrameRate', 'BackgroundBlur', 'BalanceWhiteAuto', 'Brightness', 'ColorTemperature', 'ExposureAuto',
    'ExposureTime', 'FocusAuto', 'FocusDistance', 'Height', 'ResizeMode', 'Torch', 'Width'
  ]);
  const byName = Object.fromEntries(features.map((f) => [f.name, f]));
  for (const f of features) {
    assert.deepEqual(Object.keys(featureJson(f)), Object.keys(featureJson(SIM[0])));
    assert.equal(f.locked_while_streaming, false);
  }
  assert.deepEqual(byName.ExposureAuto, { ...byName.ExposureAuto, kind: 'Enumeration', value: 'Continuous', choices: ['Off', 'Continuous'], writable: true, constraint: 'exposureMode' });
  assert.deepEqual(
    [byName.ExposureTime.kind, byName.ExposureTime.value, byName.ExposureTime.min, byName.ExposureTime.max, byName.ExposureTime.inc, byName.ExposureTime.unit],
    ['Float', 33330, 10, 1000000, 10, 'us'], 'exposureTime is in 100 µs units'
  );
  assert.equal(byName.ExposureTime.managed_by_auto, true, 'auto exposure sets it');
  assert.equal(byName.FocusDistance.managed_by_auto, false, 'focus is manual');
  assert.equal(byName.ColorTemperature.managed_by_auto, true);
  assert.deepEqual(byName.FocusAuto.choices, ['Off', 'Once', 'Continuous']);
  assert.equal(byName.FocusAuto.value, 'Off');
  assert.deepEqual([byName.AcquisitionFrameRate.value, byName.AcquisitionFrameRate.max, byName.AcquisitionFrameRate.unit], [30, 30, 'Hz']);
  assert.deepEqual([byName.Width.kind, byName.Width.value, byName.Width.min, byName.Width.max, byName.Width.unit], ['Integer', 1280, 1, 1280, 'px']);
  assert.deepEqual([byName.Brightness.kind, byName.Brightness.inc], ['Integer', 1]);
  assert.deepEqual([byName.Torch.kind, byName.Torch.value, byName.Torch.writable], ['Boolean', false, true]);
  assert.deepEqual([byName.BackgroundBlur.kind, byName.BackgroundBlur.writable], ['Boolean', false]);
  assert.deepEqual([byName.ResizeMode.choices, byName.ResizeMode.value], [['none', 'crop-and-scale'], 'none']);
  assert.equal(label(byName.ExposureTime), 'Exposure');
  assert.equal(rangeCaption(byName.ExposureTime), '10 – 1,000,000 µs');
  assert.deepEqual(metaSettings(features), { AcquisitionFrameRate: 30, BalanceWhiteAuto: 'Continuous', ExposureAuto: 'Continuous', ExposureTime: 33330 });
});

test('webcams without capabilities show what their settings say, read only', () => {
  const track = { readyState: 'live', getSettings: () => ({ width: 640, height: 480, frameRate: 30, facingMode: 'user' }) };
  const features = webcamFeatures(track);
  assert.deepEqual(features.map((f) => [f.name, f.value, f.writable]), [
    ['AcquisitionFrameRate', 30, false], ['FacingMode', 'user', false], ['Height', 480, false], ['Width', 640, false]
  ]);
  assert.deepEqual(webcamFeatures(null), []);
});

test('webcam writes keep the video format and put modes before values', async () => {
  const track = fakeTrack(CAPS, SETTINGS);
  const format = { deviceId: { exact: 'cam' }, width: { ideal: 1280 }, height: { ideal: 720 }, frameRate: { ideal: 30.000030517578125 }, resizeMode: { ideal: 'none' } };
  assert.deepEqual(await applyWebcamFeature(track, 'ExposureAuto', 'Off'), { name: 'ExposureAuto', value: 'Off' });
  assert.deepEqual(await applyWebcamFeature(track, 'ExposureTime', '5000'), { name: 'ExposureTime', value: 5000 });
  assert.deepEqual(track.calls.at(-1), { ...format, advanced: [{ exposureMode: 'manual' }, { exposureTime: 50 }] });
  // A format change goes alone, then the image controls are restated.
  assert.deepEqual(await applyWebcamFeature(track, 'Width', '640'), { name: 'Width', value: 640 });
  const sized = { ...format, width: { exact: 640 } };
  assert.deepEqual(track.calls.slice(-2), [sized, { ...sized, advanced: [{ exposureMode: 'manual' }, { exposureTime: 50 }] }]);
  // What was set exactly stays exact.
  await applyWebcamFeature(track, 'AcquisitionFrameRate', '15');
  assert.deepEqual(track.calls.at(-2), { ...sized, frameRate: { exact: 15 } });
  // Back to auto: the manual exposure is no longer sent.
  await applyWebcamFeature(track, 'ExposureAuto', 'Continuous');
  assert.deepEqual(track.calls.at(-1).advanced, [{ exposureMode: 'continuous' }]);
  assert.deepEqual(await applyWebcamFeature(track, 'Torch', 'on'), { name: 'Torch', value: true });
  assert.deepEqual(track.calls.at(-1).advanced, [{ exposureMode: 'continuous' }, { torch: true }]);
});

test('webcam writes are refused in the desktop wording', async () => {
  const track = fakeTrack(CAPS, SETTINGS);
  await assert.rejects(applyWebcamFeature(track, 'ExposureTime', '5000'),
    { message: 'cannot set ExposureTime to "5000": turn off Auto exposure to set Exposure' });
  await assert.rejects(applyWebcamFeature(track, 'Width', '9999'),
    { message: 'cannot set Width to "9999": Width violates Max=1280; enter 1280 or less' });
  await assert.rejects(applyWebcamFeature(track, 'ExposureAuto', 'Once'),
    { message: 'cannot set ExposureAuto to "Once": ExposureAuto must be one of: Off, Continuous' });
  await assert.rejects(applyWebcamFeature(track, 'BackgroundBlur', 'true'),
    { message: 'cannot set BackgroundBlur to "true": BackgroundBlur is read-only' });
  await assert.rejects(applyWebcamFeature(track, 'Gain', '1'), { message: 'cannot set Gain to "1": this camera has no Gain feature' });
  assert.equal(track.calls.length, 0, 'nothing invalid reaches the camera');
  const refusing = fakeTrack(CAPS, SETTINGS, { reject: Object.assign(new Error('x'), { name: 'OverconstrainedError', constraint: 'width' }) });
  await assert.rejects(applyWebcamFeature(refusing, 'Width', '1000'),
    { message: 'cannot set Width to "1000": the camera cannot use this value' });
  await assert.rejects(applyWebcamFeature({ ...track, readyState: 'ended' }, 'Width', '640'),
    { message: 'cannot set Width to "640": the camera has stopped; start it again' });
});
