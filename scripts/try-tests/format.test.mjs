// Tests for the workbench's text formatting (web/try/format.mjs) and the scope logic
// it shares with the desktop app (web/try/scopes.mjs). Run: node --test scripts/try-tests/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  bytes, chord, clock, count, fixed, fnv1a64, focusScore, fps, json, middleEllipsis, percent, plural, silence, sortKeys, span, trend, withChord
} from '../../web/try/format.mjs';
import { DEFAULT_REGION, FocusMeter, History, METRICS, SETTLE, metricAbout, sanitizeRegion } from '../../web/try/scopes.mjs';

test('fixed writes decimals as Rust does, ties to even from the exact value', () => {
  // format!("{:.0}") / ("{:.1}") / ("{:.2}") on the same f64 values, run with rustc.
  const cases = [
    [0.5, '0', '0.5', '0.50'],
    [1.5, '2', '1.5', '1.50'],
    [2.5, '2', '2.5', '2.50'],
    [2.25, '2', '2.2', '2.25'],
    [0.125, '0', '0.1', '0.12'],
    [-0.4, '-0', '-0.4', '-0.40'],
    [46.5, '46', '46.5', '46.50'],
    [0.05, '0', '0.1', '0.05'],
    [99.95, '100', '100.0', '99.95']
  ];
  for (const [value, d0, d1, d2] of cases) {
    assert.equal(fixed(value, 0), d0, `${value} {:.0}`);
    assert.equal(fixed(value, 1), d1, `${value} {:.1}`);
    assert.equal(fixed(value, 2), d2, `${value} {:.2}`);
  }
  assert.equal(fixed(-0, 1), '-0.0');
  assert.equal(fixed(Math.fround(Math.fround(0.0123) * 100), 1), '1.2');
  assert.equal(fixed(1e21, 0), '1000000000000000000000');
  assert.equal(fixed(Infinity, 1), 'inf');
  assert.equal(fixed(NaN, 1), 'NaN');
});

test('percent follows the scopes rule', () => {
  assert.equal(percent(0), '0%');
  assert.equal(percent(-0.2), '0%');
  assert.equal(percent(0.0000001), '<0.1%');
  assert.equal(percent(0.000999), '<0.1%');
  assert.equal(percent(0.001), '0.1%');
  assert.equal(percent(0.005), '0.5%');
  assert.equal(percent(0.0123), '1.2%');
  assert.equal(percent(0.02), '2.0%');
  assert.equal(percent(0.0999), '10.0%');
  assert.equal(percent(0.1), '10%');
  assert.equal(percent(0.46), '46%');
  assert.equal(percent(1), '100%');
});

test('frame rates and focus scores', () => {
  assert.equal(fps(29.97), '30.0 fps');
  assert.equal(fps(0), '0.0 fps');
  assert.equal(fps(NaN), '0.0 fps');
  assert.equal(focusScore(12400), '12.4k');
  assert.equal(focusScore(10000), '10.0k');
  assert.equal(focusScore(1203.4), '1203');
  assert.equal(focusScore(41.66), '41.7');
  assert.equal(focusScore(0.4249), '0.42');
  assert.equal(focusScore(0), '0.00');
});

test('bytes reads like the desktop storage limits', () => {
  assert.equal(bytes(0), '0 B');
  assert.equal(bytes(512), '512 B');
  assert.equal(bytes(1023), '1023 B');
  assert.equal(bytes(1024), '1 KiB');
  assert.equal(bytes(1536), '1.5 KiB');
  assert.equal(bytes(2048), '2 KiB');
  assert.equal(bytes(10240), '10 KiB');
  assert.equal(bytes(290816), '284 KiB');
  assert.equal(bytes(1572864), '1.5 MiB');
  assert.equal(bytes(10 * 1024 ** 3), '10 GiB');
  assert.equal(bytes(2 * 1024 ** 3), '2 GiB');
});

test('counts, plurals, clocks and spans', () => {
  assert.equal(count(0), '0');
  assert.equal(count(999), '999');
  assert.equal(count(1204), '1,204');
  assert.equal(count(1234567), '1,234,567');
  assert.equal(count(-1204), '-1,204');
  assert.equal(plural(1, 'frame', 'frames'), '1 frame');
  assert.equal(plural(1204, 'frame', 'frames'), '1,204 frames');
  assert.equal(clock(0), '00:00');
  assert.equal(clock(247_000), '04:07');
  assert.equal(clock(247_999), '04:07');
  assert.equal(clock(3_723_000), '1:02:03');
  assert.equal(span(6400), '6 s');
  assert.equal(span(130_000), '2 min');
  assert.equal(span(7_300_000), '2 h');
  assert.equal(silence(6400), 'No new frame for 6 s');
  assert.equal(silence(300), 'No new frame for 1 s');
});

test('trend names its span, range and marks', () => {
  const samples = [{ value: 30, flagged: false }, { value: 24.5, flagged: true }];
  assert.equal(trend(samples, 2, 'Frame rate', fps, 'lost frames'), 'Frame rate, last 2 s: 24.5 fps to 30.0 fps\nMarked: lost frames (1)');
  assert.equal(trend([], 0, 'Frame rate', fps, 'lost frames'), 'Frame rate, last 1 s');
});

test('chords use Apple glyphs in Apple order, and names elsewhere', () => {
  assert.equal(chord('Mod+S', true), '⌘S');
  assert.equal(chord('Mod+S', false), 'Ctrl+S');
  assert.equal(chord('Mod+Shift+S', true), '⇧⌘S');
  assert.equal(chord('Mod+Shift+S', false), 'Ctrl+Shift+S');
  assert.equal(chord('Mod+Alt+0', true), '⌥⌘0');
  assert.equal(chord('Mod+Alt+0', false), 'Ctrl+Alt+0');
  assert.equal(chord('Ctrl+Alt+Shift+Mod+K', true), '⌃⌥⇧⌘K');
  assert.equal(chord('Mod++', true), '⌘+');
  assert.equal(chord('Mod+=', false), 'Ctrl+=');
  assert.equal(chord('Mod+-', true), '⌘-');
  assert.equal(chord('Space', true), 'Space');
  assert.equal(chord('h', false), 'H');
  assert.equal(chord('Escape', false), 'Esc');
  assert.equal(chord('Enter', true), '↩');
  assert.equal(chord('', true), '');
  assert.equal(withChord('Capture from this camera', 'Mod+S', true), 'Capture from this camera · ⌘S');
  assert.equal(withChord('Histogram', 'H', false), 'Histogram · H');
  assert.equal(withChord('Session activity', '', false), 'Session activity');
});

test('sortKeys sorts by UTF-16 code unit, deeply, like serde_json', () => {
  const value = { b: 1, B: 2, a: { z: [3, { y: 1, x: 2 }], _: null }, skip: undefined, fn: () => 1, typed: new Uint32Array([1, 2]) };
  const sorted = sortKeys(value);
  assert.deepEqual(Object.keys(sorted), ['B', 'a', 'b', 'typed']);
  assert.deepEqual(Object.keys(sorted.a), ['_', 'z']);
  assert.deepEqual(Object.keys(sorted.a.z[1]), ['x', 'y']);
  assert.deepEqual(sorted.typed, [1, 2]);
  assert.notEqual(sorted.a, value.a, 'a deep copy');
  assert.equal(json({ b: 1, a: [1, 2] }), '{\n  "a": [\n    1,\n    2\n  ],\n  "b": 1\n}');
  assert.equal(json(new Date(0)), '"1970-01-01T00:00:00.000Z"');
  assert.equal(sortKeys(null), null);
  assert.equal(sortKeys('x'), 'x');
});

test('middleEllipsis keeps both ends', () => {
  const name = '2026-10-25/Pattern-camera-SIM0-20261025-143012-####.png';
  const short = middleEllipsis(name, 30);
  assert.equal([...short].length, 30);
  assert.ok(short.startsWith('2026-10-25/Pat'));
  assert.ok(short.endsWith('####.png'));
  assert.ok(short.includes('…'));
  assert.equal(middleEllipsis('short.png', 30), 'short.png');
  assert.equal(middleEllipsis('🙂🙂🙂🙂🙂', 3), '🙂…🙂');
  assert.equal(middleEllipsis('abc', 1), '…');
});

test('fnv1a64 matches the FNV-1a test vectors and media.rs', () => {
  assert.equal(fnv1a64(''), 'cbf29ce484222325');
  assert.equal(fnv1a64('a'), 'af63dc4c8601ec8c');
  assert.equal(fnv1a64('foobar'), '85944171f73967e8');
  assert.equal(fnv1a64('é').length, 16);
  assert.notEqual(fnv1a64('é'), fnv1a64('e'));
});

test('focus regions are sanitized and placed in pixels like the desktop', () => {
  assert.deepEqual(sanitizeRegion([NaN, 0, 1, 1]), DEFAULT_REGION.map(Math.fround));
  assert.deepEqual(sanitizeRegion([0.9, 0.9, 0.5, 0.5]), [0.5, 0.5, 0.5, 0.5]);
  assert.deepEqual(sanitizeRegion([-1, 0, 0, 2]).map((v) => Number(v.toFixed(4))), [0, 0, 0.0001, 1]);
  const meter = new FocusMeter();
  assert.deepEqual(meter.pixels(640, 480), [240, 180, 160, 120]);
  assert.deepEqual(meter.pixels(1, 1), [0, 0, 1, 1]);
  assert.deepEqual(meter.pixels(33, 17), [12, 6, 8, 4]);
});

test('the focus meter settles, locks on with hysteresis and resets after a move', () => {
  const score = (v) => ({ laplacian: v, tenengrad: v * 2, brenner: v / 2, variance: 1 });
  const meter = new FocusMeter();
  assert.equal(meter.verdict(), 'Waiting for a frame');
  assert.equal(meter.label(), 'Edges');
  meter.record(score(800));
  assert.equal(meter.verdict(), 'Measuring…');
  for (let i = 1; i < SETTLE; i += 1) meter.record(score(800));
  // Settling at a fresh peak is in focus, but no lock-on.
  assert.equal(meter.inFocus, true);
  assert.equal(meter.locks, 0);
  assert.equal(meter.verdict(), 'In focus');
  meter.record(score(741));
  // 741 / 800 = 0.926: still in focus, above the 0.90 exit.
  assert.equal(meter.inFocus, true);
  assert.equal(meter.label(), '741  93%');
  meter.record(score(400));
  assert.equal(meter.inFocus, false);
  assert.equal(meter.verdict(), 'Adjust to raise the score');
  meter.record(score(760));
  // 0.95 is not enough to come back in: that takes 0.97.
  assert.equal(meter.inFocus, false);
  meter.record(score(790));
  assert.equal(meter.inFocus, true);
  assert.equal(meter.locks, 1);
  meter.setMetric('tenengrad');
  assert.equal(meter.latest(), 1580);
  assert.equal(meter.peakValue(), 1600);
  meter.moveTo([0.1, 0.1, 0.2, 0.2]);
  assert.equal(meter.latest(), 1580, 'scores stay on show until the next one');
  meter.record(score(10));
  assert.equal(meter.history.length, 1);
  assert.equal(meter.peakValue(), 20);
  meter.reset();
  assert.equal(meter.share(), null);
  assert.equal(meter.history.length, 0);
});

test('the trace keeps the last 120 scores', () => {
  const meter = new FocusMeter();
  for (let i = 0; i < 130; i += 1) meter.record({ laplacian: i, tenengrad: 0, brenner: 0, variance: 0 });
  assert.equal(meter.trace().length, 120);
  assert.equal(meter.trace()[0], 10);
  assert.equal(meter.peakValue(), 129);
});

test('focus measures carry the desktop words', () => {
  assert.deepEqual(METRICS.map((m) => m.plain), ['Edges', 'Strong edges', 'Texture', 'Contrast']);
  assert.equal(metricAbout('laplacian'), 'Focus measure: Edges (Laplacian). Variance of the Laplacian: responds to fine edges');
});

test('sparkline history samples every 250 ms and flags losses', () => {
  const history = new History();
  history.offer(0, 30, 0);
  history.offer(60, 10, 5);
  history.offer(250, 29, 2);
  history.offer(500, NaN, 2);
  assert.deepEqual(history.samples.map(({ value, flagged }) => ({ value, flagged })), [
    { value: 30, flagged: false },
    { value: 29, flagged: true },
    { value: 0, flagged: false }
  ]);
  assert.equal(history.ready(), false);
  history.offer(1750, 30, 2);
  assert.equal(history.ready(), true);
  assert.equal(history.about('Frame rate', fps, 'skipped frames'), 'Frame rate, last 1 s: 0.0 fps to 30.0 fps\nMarked: skipped frames (1)');
  history.offer(40_000, 30, 2);
  assert.equal(history.samples.length, 1, 'samples older than 30 s go');
});
