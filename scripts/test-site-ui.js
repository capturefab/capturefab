/* Run with node scripts/test-site-ui.js; no browser or package dependency. */
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const published = JSON.parse(fs.readFileSync(path.join(__dirname, '../web/releases.json'), 'utf8'));

class Element {
  constructor(tag = 'div') {
    this.tagName = tag.toUpperCase();
    this.listeners = new Map();
    this.attributes = new Map();
    this.dataset = {};
    this.style = {};
    this.children = [];
    this.hidden = false;
    this.textContent = '';
    this.value = '';
  }
  addEventListener(type, listener) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(listener);
  }
  emit(type) {
    for (const listener of this.listeners.get(type) || []) listener({ target: this, preventDefault() {} });
  }
  setAttribute(name, value) { this.attributes.set(name, String(value)); }
  getAttribute(name) { return this.attributes.get(name) ?? null; }
  removeAttribute(name) {
    this.attributes.delete(name);
    if (name === 'data-state') delete this.dataset.state;
  }
  set href(value) { this.setAttribute('href', value); }
  get href() { return this.getAttribute('href') || ''; }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
}

function deferred() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
}

function openPage(options = {}) {
  const events = [];
  const ids = new Map();
  if (options.downloads !== false) {
    for (const id of ['os-select', 'arch-select', 'primary-download', 'verify-box', 'asset-detail', 'verify-links', 'install-hint', 'download-label', 'download-description', 'checksum-text', 'verify-command', 'platform-hint', 'release-version']) ids.set(id, new Element());
    ids.get('primary-download').hidden = true;
  }
  const result = new Element();
  const form = new Element('form');
  const motion = new Element();
  motion.matches = Boolean(options.reducedMotion);
  const connection = new Element();
  connection.saveData = Boolean(options.saveData);
  const videos = Array.from({ length: options.videoCount ?? 2 }, () => {
    const video = new Element('video');
    video.paused = true;
    video.playAttempts = 0;
    video.rejectPlay = false;
    // Real media events are asynchronous. Keep them separate from method calls
    // so visibility changes and explicit controls can race as they do in a browser.
    video.play = () => {
      video.playAttempts += 1;
      if (video.rejectPlay) return Promise.reject(new Error('Playback was blocked'));
      if (video.paused) {
        video.paused = false;
        events.push(() => video.emit('play'));
      }
      return Promise.resolve();
    };
    video.pause = () => {
      if (!video.paused) {
        video.paused = true;
        events.push(() => video.emit('pause'));
      }
    };
    return video;
  });
  const document = new Element();
  document.documentElement = new Element('html');
  document.baseURI = 'https://capturefab.example/';
  document.hidden = Boolean(options.hidden);
  document.getElementById = (id) => ids.get(id) || null;
  document.querySelector = (selector) => ({ '.download-result': result, '.platform-form': form })[selector] || null;
  document.querySelectorAll = (selector) => selector === '.product-video' ? videos : [];
  document.createElement = (tag) => new Element(tag);
  document.createTextNode = (text) => ({ textContent: text });
  const observers = [];
  class Observer {
    constructor(callback) { this.callback = callback; observers.push(this); }
    observe() {}
  }
  const window = { matchMedia: () => motion, setTimeout() {} };
  if (options.observer !== false) window.IntersectionObserver = Observer;
  const navigator = {
    platform: options.platform || '',
    userAgent: options.userAgent || '',
    connection,
    ...(options.userAgentData ? { userAgentData: options.userAgentData } : {})
  };
  const fetchCalls = [];
  const fetch = (url, settings) => {
    fetchCalls.push({ url, settings });
    if (options.fetchError) return Promise.reject(new Error('Offline'));
    return Promise.resolve({ ok: true, json: async () => options.manifest ?? published });
  };
  vm.runInNewContext(source, { document, window, navigator, fetch, URL, IntersectionObserver: Observer }, { filename: 'web/app.js' });
  return {
    ids, result, motion, connection, videos, document, fetchCalls,
    async settle() {
      for (let pass = 0; pass < 6; pass += 1) {
        await Promise.resolve();
        while (events.length) events.shift()();
      }
    },
    change(id, value) { const element = ids.get(id); element.value = value; element.emit('change'); },
    intersect(index, ratio) {
      for (const observer of observers) observer.callback([{ target: videos[index], isIntersecting: ratio > 0, intersectionRatio: ratio }]);
    },
    visibility(hidden) { document.hidden = hidden; document.emit('visibilitychange'); },
    preference(reduced) { motion.matches = reduced; motion.emit('change'); },
    dataSaving(enabled) { connection.saveData = enabled; connection.emit('change'); }
  };
}

async function downloads() {
  const latest = published.releases.find((release) => release.version === published.latest);
  // One build of the latest release left unpublished, to check unavailable selections.
  const partial = structuredClone(published);
  const missing = partial.releases.find((release) => release.version === partial.latest).assets
    .find((asset) => asset.os === 'windows' && asset.arch === 'x86_64');
  missing.available = false;
  const page = openPage({ platform: 'MacIntel', userAgent: 'Macintosh; Intel Mac OS X', manifest: partial });
  await page.settle();
  assert.equal(page.ids.get('os-select').value, 'macos');
  assert.equal(page.ids.get('arch-select').value, 'aarch64', 'Mac user agents cannot report Apple silicon, so the best guess is ARM64');
  assert.match(page.ids.get('download-label').textContent, /^Best match: /);
  page.change('arch-select', 'aarch64');
  const desktop = latest.assets.find((asset) => asset.os === 'macos' && asset.arch === 'aarch64' && asset.available);
  assert.equal(page.ids.get('primary-download').href, desktop.url);
  assert.equal(page.ids.get('primary-download').hidden, false);
  assert.match(page.ids.get('primary-download').getAttribute('aria-label'), /macOS ARM64/);
  assert.equal(page.ids.get('verify-command').textContent, `shasum -a 256 ${desktop.name}`);
  assert.match(page.ids.get('checksum-text').textContent, new RegExp(desktop.sha256));
  assert.match(page.ids.get('install-hint').textContent, /Extract.*Capturefab\.app/);
  page.change('os-select', 'windows');
  assert.ok(page.ids.get('install-hint').textContent.includes('.\\capturefab.exe --help'), 'PowerShell instructions must retain the relative-path backslash');
  page.change('arch-select', 'x86_64');
  assert.equal(page.ids.get('primary-download').hidden, true, 'Unavailable selections must clear a previous link');
  assert.equal(page.ids.get('primary-download').href, '');
  assert.equal(page.ids.get('verify-box').hidden, true);
  assert.equal(page.ids.get('install-hint').hidden, true);

  page.change('os-select', 'linux');
  assert.match(page.ids.get('platform-hint').textContent, /dpkg --print-architecture/, 'Linux needs a check that tells a 32-bit OS from a 64-bit one');
  assert.doesNotMatch(page.ids.get('platform-hint').textContent, /ARMv7/);

  for (const options of [
    { platform: 'Linux armv7l', userAgent: 'X11; Linux armv7l' },
    { userAgentData: { platform: 'Linux', getHighEntropyValues: async () => ({ architecture: 'arm', bitness: '32' }) } }
  ]) {
    const armhf = openPage(options);
    await armhf.settle();
    assert.equal(armhf.ids.get('os-select').value, 'linux');
    assert.equal(armhf.ids.get('arch-select').value, '', 'There is no 32-bit ARM build to select');
    assert.match(armhf.ids.get('platform-hint').textContent, /32-bit ARM.*64-bit Raspberry Pi OS.*dpkg --print-architecture/, 'A 32-bit ARM browser must learn how to get a usable system');
    armhf.change('arch-select', 'aarch64');
    assert.doesNotMatch(armhf.ids.get('platform-hint').textContent, /This browser runs/);
  }

  const offline = openPage({ fetchError: true });
  await offline.settle();
  assert.equal(offline.result.dataset.state, 'warning');
  assert.equal(offline.ids.get('primary-download').hidden, true);
  assert.match(offline.ids.get('download-description').textContent, /all downloads/i);

  const unsafe = structuredClone(published);
  unsafe.releases.find((release) => release.version === unsafe.latest).assets.forEach((asset) => { asset.url = 'javascript:alert(1)'; });
  const blocked = openPage({ manifest: unsafe });
  await blocked.settle();
  blocked.change('os-select', 'macos');
  blocked.change('arch-select', 'aarch64');
  assert.equal(blocked.ids.get('primary-download').hidden, true, 'Unsafe manifest links must never become downloads');

  const hints = deferred();
  const pending = openPage({ userAgentData: { platform: 'Windows', getHighEntropyValues: () => hints.promise } });
  pending.change('arch-select', 'aarch64');
  hints.resolve({ architecture: 'x86', bitness: '64' });
  await pending.settle();
  assert.equal(pending.ids.get('arch-select').value, 'aarch64', 'Late browser hints must preserve an explicit processor selection');

  const otherComputerHints = deferred();
  const otherComputer = openPage({ userAgentData: { platform: 'Windows', getHighEntropyValues: () => otherComputerHints.promise } });
  otherComputer.change('os-select', 'macos');
  otherComputerHints.resolve({ architecture: 'x86', bitness: '64' });
  await otherComputer.settle();
  assert.equal(otherComputer.ids.get('os-select').value, 'macos');
  assert.equal(otherComputer.ids.get('arch-select').value, '', 'Changing the target OS must not reuse a late hardware hint from this computer');

  const detected = openPage({ userAgentData: { platform: 'macOS', getHighEntropyValues: async () => ({ architecture: 'arm', bitness: '64' }) } });
  await detected.settle();
  assert.equal(detected.ids.get('primary-download').href, desktop.url, 'An explicit browser architecture hint may select the matching published build');
  const empty = openPage({ manifest: { schema_version: 1, latest: null, releases: [] } });
  await empty.settle();
  assert.equal(empty.ids.get('primary-download').hidden, true);
  assert.match(empty.ids.get('download-label').textContent, /No published binaries/);

  const invalidHints = deferred();
  const invalid = openPage({
    manifest: { schema_version: 1, latest: 'broken', releases: [{ version: 123, assets: [] }] },
    userAgentData: { platform: 'Windows', getHighEntropyValues: () => invalidHints.promise }
  });
  await invalid.settle();
  assert.equal(invalid.result.dataset.state, 'warning');
  invalidHints.resolve({ architecture: 'x86', bitness: '64' });
  await invalid.settle();
  assert.equal(invalid.result.dataset.state, 'warning', 'Late platform detection must not replace a manifest failure with an empty-release message');
  assert.match(invalid.ids.get('download-description').textContent, /all downloads/i);

  const docs = openPage({ downloads: false });
  await docs.settle();
  assert.equal(docs.fetchCalls.length, 0, 'Pages without a download panel need no release request');
}

async function visiblePlayback() {
  const page = openPage({ downloads: false });
  await page.settle();
  assert.ok(page.videos.every((video) => video.paused), 'Wait for observed visibility before starting clips');
  page.intersect(0, 0.39);
  await page.settle();
  assert.equal(page.videos[0].paused, true);
  page.intersect(0, 0.5);
  await page.settle();
  assert.equal(page.videos[0].paused, false);
  assert.equal(page.videos[1].paused, true, 'Offscreen clips must not load and play with a visible neighbor');
  page.intersect(0, 0);
  await page.settle();
  assert.equal(page.videos[0].paused, true);
  page.intersect(0, 0.8);
  await page.settle();
  assert.equal(page.videos[0].paused, false, 'An automatic offscreen pause may resume on return');
  page.videos[0].pause();
  await page.settle();
  page.intersect(0, 0);
  page.intersect(0, 0.8);
  page.visibility(true);
  page.visibility(false);
  await page.settle();
  assert.equal(page.videos[0].paused, true, 'A viewer pause must survive scrolling and tab changes');
  page.intersect(1, 0.8);
  await page.settle();
  assert.equal(page.videos[1].paused, false, 'Pausing one clip must not stop other visible clips');
  page.visibility(true);
  await page.settle();
  assert.equal(page.videos[1].paused, true, 'A hidden document must pause playback');
  page.visibility(false);
  await page.settle();
  assert.equal(page.videos[1].paused, false);
}

async function motionAndDataPreferences() {
  for (const preference of ['reducedMotion', 'saveData']) {
    const page = openPage({ downloads: false, [preference]: true });
    page.intersect(0, 0.8);
    await page.settle();
    assert.equal(page.videos[0].playAttempts, 0, `${preference} must suppress automatic playback`);
    await page.videos[0].play();
    await page.settle();
    page.intersect(0, 0.9);
    page.visibility(false);
    await page.settle();
    assert.equal(page.videos[0].paused, false, `${preference} must permit explicit native playback`);
  }
  const automatic = openPage({ downloads: false });
  automatic.intersect(0, 0.8);
  await automatic.settle();
  automatic.preference(true);
  await automatic.settle();
  assert.equal(automatic.videos[0].paused, true, 'Enabling reduced motion must stop automatic playback');
  automatic.preference(false);
  await automatic.settle();
  assert.equal(automatic.videos[0].paused, false);
  automatic.dataSaving(true);
  await automatic.settle();
  assert.equal(automatic.videos[0].paused, true, 'Enabling data saving must stop automatic playback');

  const starting = openPage({ downloads: false });
  starting.intersect(0, 0.8);
  // Change the preference before the browser delivers the queued play event.
  // Cancellation queues pause after play; the stale play must not turn that
  // automatic cancellation into a viewer pause or prevent later resumption.
  starting.preference(true);
  await starting.settle();
  assert.equal(starting.videos[0].paused, true, 'A motion preference change must also cancel an automatic start before its play event arrives');
  starting.preference(false);
  await starting.settle();
  assert.equal(starting.videos[0].paused, false, 'Canceling an automatic start must not count as a viewer pause');
}

async function missingObserverAndBlockedPlay() {
  const manual = openPage({ downloads: false, observer: false });
  await manual.settle();
  assert.equal(manual.videos[0].playAttempts, 0, 'Without visibility observation, clips require native play controls');
  await manual.videos[0].play();
  await manual.settle();
  manual.visibility(false);
  await manual.settle();
  assert.equal(manual.videos[0].paused, false, 'Missing IntersectionObserver must not interrupt a visible manual clip');
  manual.visibility(true);
  await manual.settle();
  assert.equal(manual.videos[0].paused, true);

  const blocked = openPage({ downloads: false });
  blocked.videos[0].rejectPlay = true;
  blocked.intersect(0, 0.8);
  await blocked.settle();
  assert.equal(blocked.videos[0].paused, true);
  blocked.videos[0].rejectPlay = false;
  await blocked.videos[0].play();
  await blocked.settle();
  blocked.intersect(0, 0.9);
  await blocked.settle();
  assert.equal(blocked.videos[0].paused, false, 'A rejected automatic start must leave native playback available');
}

(async () => {
  for (const check of [downloads, visiblePlayback, motionAndDataPreferences, missingObserverAndBlockedPlay]) {
    try { await check(); }
    catch (error) { console.error(`${check.name}:`, error); process.exitCode = 1; }
  }
  if (!process.exitCode) console.log('Download selection, fallback and visible-video controls respect viewer choices, motion, data and visibility.');
})().catch((error) => { console.error(error); process.exitCode = 1; });
