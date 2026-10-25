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
  // As in the DOM, text set directly replaces the children, and children supply the text.
  set textContent(value) { this.ownText = value; this.children = []; }
  get textContent() { return this.children.length ? this.children.map((child) => child.textContent ?? '').join('') : this.ownText; }
  addEventListener(type, listener) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(listener);
  }
  emit(type, detail = {}) {
    const event = { target: this, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; }, ...detail };
    for (const listener of this.listeners.get(type) || []) listener(event);
    return event;
  }
  // Just enough tree for the next-steps panel: a parent, siblings and named ancestors.
  closest(selector) { return this.ancestors?.[selector] || null; }
  matches(selector) { return Boolean(this.selectors?.includes(selector)); }
  querySelector(selector) { return this.children.find((child) => child.matches?.(selector)) || null; }
  querySelectorAll(selector) { return this.children.filter((child) => child.matches?.(selector)); }
  after(node) { this.placedAfter = node; node.placedBefore = this; }
  focus() { this.focused = (this.focused || 0) + 1; }
  scrollIntoView() {}
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
    for (const id of ['os-select', 'arch-select', 'primary-download', 'verify-box', 'asset-detail', 'verify-links', 'install-hint', 'download-label', 'download-description', 'checksum-text', 'verify-command', 'platform-hint', 'release-version', 'next-steps', 'next-steps-title', 'next-steps-list', 'next-steps-close', 'next-steps-verify', 'direct-download']) ids.set(id, new Element());
    ids.get('primary-download').hidden = true;
    ids.get('next-steps').hidden = true;
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
  // The hero and banner buttons, each in an .actions group with a processor line after it.
  const oneclick = [];
  const alts = [];
  for (let index = 0; index < (options.downloads === false ? 0 : 2); index += 1) {
    const group = new Element();
    const link = new Element('a');
    link.selectors = ['[data-oneclick]'];
    link.ancestors = { '.actions': group };
    const alt = new Element('p');
    alt.selectors = ['[data-oneclick-alt]'];
    alt.hidden = true;
    const parent = new Element();
    parent.children = [group, alt];
    group.children = [link];
    group.nextElementSibling = alt;
    alt.parentElement = parent;
    // Nested buttons are found through the alt line's parent.
    parent.querySelector = (selector) => selector === '[data-oneclick]' ? link : null;
    oneclick.push(link); alts.push(alt);
  }
  const runHints = Array.from({ length: options.downloads === false ? 0 : 2 }, () => Object.assign(new Element('span'), { textContent: '# Run from the extracted folder' }));
  const document = new Element();
  document.documentElement = new Element('html');
  document.baseURI = 'https://capturefab.example/';
  document.hidden = Boolean(options.hidden);
  document.getElementById = (id) => ids.get(id) || null;
  document.querySelector = (selector) => ({ '.download-result': result, '.platform-form': form })[selector] || null;
  document.querySelectorAll = (selector) => ({ '.product-video': videos, '[data-oneclick]': oneclick, '[data-oneclick-alt]': alts, '[data-run-hint]': runHints })[selector] || [];
  document.createElement = (tag) => new Element(tag);
  document.createTextNode = (text) => ({ textContent: text });
  const observers = [];
  class Observer {
    constructor(callback) { this.callback = callback; observers.push(this); }
    observe() {}
  }
  // Timers run at once; the page uses them only to let a native download start first.
  const window = { matchMedia: () => motion, setTimeout(callback) { callback(); } };
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
    ids, result, motion, connection, videos, document, fetchCalls, oneclick, alts, runHints,
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
  assert.equal(page.ids.get('verify-command').textContent, `cd ~/Downloads && echo "${desktop.sha256.toLowerCase()}  ${desktop.name}" | shasum -a 256 -c`, 'The Mac check prints OK or FAILED by itself');
  assert.match(page.ids.get('checksum-text').textContent, new RegExp(desktop.sha256));
  assert.match(page.ids.get('install-hint').textContent, /Extract.*Move Capturefab\.app to Applications.*Local Network.*CLI/, 'One macOS story: move the app, allow Local Network, then set up the CLI');
  const firstLaunch = (hint) => hint.children.find((child) => child.tagName === 'A' && child.href === 'https://capturefab.example/docs.html#first-launch');
  assert.ok(firstLaunch(page.ids.get('install-hint')), 'The install hint links to the first-launch steps');
  page.change('os-select', 'windows');
  assert.ok(page.ids.get('install-hint').textContent.includes('.\\capturefab.exe --help'), 'PowerShell instructions must retain the relative-path backslash');
  assert.match(page.ids.get('install-hint').textContent, /Run anyway.*Private networks/, 'Windows warns about SmartScreen and the firewall prompt');
  const windowsArm = latest.assets.find((asset) => asset.os === 'windows' && asset.arch === 'aarch64');
  assert.equal(page.ids.get('verify-command').textContent, `cd $HOME\\Downloads; if ((Get-FileHash .\\${windowsArm.name} -Algorithm SHA256).Hash -eq '${windowsArm.sha256.toLowerCase()}') { 'OK: checksum matches' } else { 'MISMATCH: do not open this file' }`);
  page.change('arch-select', 'x86_64');
  assert.equal(page.ids.get('primary-download').hidden, true, 'Unavailable selections must clear a previous link');
  assert.equal(page.ids.get('primary-download').href, '');
  assert.equal(page.ids.get('verify-box').hidden, true);
  assert.equal(page.ids.get('install-hint').hidden, true);

  page.change('os-select', 'linux');
  assert.match(page.ids.get('platform-hint').textContent, /dpkg --print-architecture/, 'Linux needs a check that tells a 32-bit OS from a 64-bit one');
  page.change('arch-select', 'aarch64');
  const linuxArm = latest.assets.find((asset) => asset.os === 'linux' && asset.arch === 'aarch64');
  assert.match(page.ids.get('verify-command').textContent, new RegExp(`echo "${linuxArm.sha256.toLowerCase()}  ${linuxArm.name.replace(/\./g, '\\.')}" \\| sha256sum -c$`));

  // A name that fails the file name check keeps the hash-only command, since a self-check against a placeholder always fails.
  const oddName = structuredClone(published);
  const odd = assetFor(oddName, 'linux', 'x86_64');
  odd.name = 'capturefab linux.tar.gz';
  odd.sha256 = odd.sha256.toUpperCase();
  const fallback = openPage({ manifest: oddName });
  await fallback.settle();
  fallback.change('os-select', 'linux');
  fallback.change('arch-select', 'x86_64');
  assert.equal(fallback.ids.get('verify-command').textContent, 'sha256sum DOWNLOADED_FILE');
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

const assetFor = (manifest, osId, archId) => manifest.releases.find((release) => release.version === manifest.latest).assets.find((asset) => asset.os === osId && asset.arch === archId);

async function signing() {
  const mac = openPage({ userAgentData: { platform: 'macOS', getHighEntropyValues: async () => ({ architecture: 'arm', bitness: '64' }) } });
  await mac.settle();
  assert.match(mac.ids.get('asset-detail').textContent, /Apple notarized$/);
  assert.doesNotMatch(mac.ids.get('install-hint').textContent, /Control-click|SmartScreen/, 'A notarized Mac build needs no first-open workaround');
  mac.change('os-select', 'windows');
  assert.match(mac.ids.get('asset-detail').textContent, /windows-aarch64\.zip · [0-9.]+ MiB · Unsigned$/);
  assert.match(mac.ids.get('install-hint').textContent, /SmartScreen.*More info → Run anyway/);
  assert.ok(mac.ids.get('install-hint').textContent.includes('.\\capturefab.exe --help'));
  mac.change('os-select', 'linux');
  assert.doesNotMatch(mac.ids.get('install-hint').textContent, /SmartScreen|code-signed/, 'Linux does not warn about unsigned archives');

  const signed = structuredClone(published);
  assetFor(signed, 'windows', 'x86_64').platform_signature = 'authenticode';
  assetFor(signed, 'macos', 'x86_64').platform_signature = 'apple-developer-id';
  const page = openPage({ manifest: signed, userAgentData: { platform: 'Windows', getHighEntropyValues: async () => ({ architecture: 'x86', bitness: '64' }) } });
  await page.settle();
  assert.match(page.ids.get('asset-detail').textContent, /Authenticode$/);
  assert.doesNotMatch(page.ids.get('install-hint').textContent, /SmartScreen/, 'Signed Windows builds must not mention SmartScreen');
  page.change('os-select', 'macos');
  assert.match(page.ids.get('install-hint').textContent, /Control-click Capturefab\.app and choose Open/);
}

async function guessedProcessor() {
  const intel = assetFor(published, 'macos', 'x86_64');
  const page = openPage({ platform: 'MacIntel', userAgent: 'Macintosh; Intel Mac OS X' });
  await page.settle();
  for (const alt of page.alts) {
    assert.equal(alt.hidden, false, 'A guessed processor must offer the other build beside each button');
    const link = alt.children.find((child) => child.tagName === 'A' && child.getAttribute('download'));
    assert.equal(link.href, intel.url);
    assert.match(link.textContent, /Intel Mac\? Download x64/);
    assert.ok(alt.children.some((child) => child.tagName === 'A' && child.href === '#download'), 'Other platforms stays one tap away');
  }
  const link = page.alts[0].children.find((child) => child.getAttribute?.('download'));
  const click = link.emit('click');
  assert.equal(click.defaultPrevented, false, 'The alternate link must still download');
  assert.equal(page.ids.get('arch-select').value, 'x86_64');
  assert.doesNotMatch(page.ids.get('download-label').textContent, /^Best match: /);
  assert.ok(page.oneclick.every((button) => button.href === intel.url), 'Page buttons follow the chosen processor');
  assert.ok(page.alts.every((alt) => alt.hidden), 'A chosen processor is no longer a guess');
  assert.equal(page.ids.get('next-steps').hidden, false);
  assert.match(page.ids.get('next-steps-title').textContent, /macOS \(Intel\)$/);

  const reported = openPage({ userAgentData: { platform: 'macOS', getHighEntropyValues: async () => ({ architecture: 'arm', bitness: '64' }) } });
  await reported.settle();
  assert.ok(reported.alts.every((alt) => alt.hidden), 'A processor the browser reported needs no alternative');

  const single = structuredClone(published);
  assetFor(single, 'macos', 'x86_64').available = false;
  const only = openPage({ manifest: single, platform: 'MacIntel', userAgent: 'Macintosh; Intel Mac OS X' });
  await only.settle();
  assert.ok(only.alts.every((alt) => alt.hidden && !alt.children.length), 'No alternate link without a published alternate build');
}

async function nextSteps() {
  const cases = [
    ['macOS', 'arm', /Capturefab\.app/, 'macos', 'aarch64', './capturefab'],
    ['Windows', 'x86', /\.\\capturefab\.exe/, 'windows', 'x86_64', '.\\capturefab.exe'],
    ['Linux', 'x86', /\.\/capturefab/, 'linux', 'x86_64', './capturefab']
  ];
  for (const [platform, architecture, pattern, osId, archId, command] of cases) {
    const asset = assetFor(published, osId, archId);
    const page = openPage({ userAgentData: { platform, getHighEntropyValues: async () => ({ architecture, bitness: '64' }) } });
    await page.settle();
    assert.ok(page.runHints.every((hint) => hint.textContent.includes(` ${command} `)), `CLI examples name ${command}`);
    const [button] = page.oneclick;
    assert.equal(button.dataset.oneclickState, 'ready');
    const click = button.emit('click');
    assert.equal(click.defaultPrevented, false, 'The native download must not be blocked');
    assert.equal(button.href, asset.url);
    const panel = page.ids.get('next-steps');
    assert.equal(panel.hidden, false);
    const steps = page.ids.get('next-steps-list').children.map((item) => item.textContent).join(' ');
    assert.match(steps, pattern);
    assert.match(steps, /Try a simulated camera/);
    assert.equal(page.ids.get('direct-download').href, asset.url);
    assert.equal(page.ids.get('direct-download').getAttribute('download'), asset.name);
    assert.equal(panel.placedBefore, page.alts[0], 'The panel follows the clicked button and its processor line');
    page.ids.get('next-steps-close').emit('click');
    assert.equal(panel.hidden, true);
    assert.equal(button.focused, 1, 'Dismissing returns focus to the button');
    button.emit('click');
    page.change('os-select', osId === 'linux' ? 'macos' : 'linux');
    assert.equal(panel.hidden, true, 'A new selection hides steps for the previous build');
  }
  const linux = openPage({ userAgentData: { platform: 'Linux', getHighEntropyValues: async () => ({ architecture: 'x86', bitness: '64' }) } });
  await linux.settle();
  linux.oneclick[0].emit('click');
  const linuxSteps = linux.ids.get('next-steps-list').children.map((item) => item.textContent);
  assert.equal(linuxSteps.filter((step) => /doctor/.test(step)).length, 1, 'Linux lists doctor once');
  assert.ok(linuxSteps.every((step) => !/(^|\s)capturefab doctor/.test(step)), 'doctor runs from the extracted folder');

  const mac = openPage({ userAgentData: { platform: 'macOS', getHighEntropyValues: async () => ({ architecture: 'arm', bitness: '64' }) } });
  await mac.settle();
  const hint = mac.ids.get('install-hint');
  assert.ok(hint.children.some((child) => child.tagName === 'A' && child.href.endsWith('docs.html#cli-path')), 'The macOS CLI step links to the PATH setup');
  mac.ids.get('primary-download').emit('click');
  assert.equal(mac.ids.get('next-steps').hidden, false);
  assert.equal(mac.ids.get('next-steps').placedBefore, hint);
  assert.equal(hint.hidden, true, 'The panel replaces the install hint it would repeat');
  const cliStep = mac.ids.get('next-steps-list').children.find((item) => /For the CLI/.test(item.textContent));
  assert.ok(cliStep.children.some((child) => child.tagName === 'A' && child.href.endsWith('docs.html#cli-path')), 'The panel keeps the link');
  mac.ids.get('next-steps-close').emit('click');
  assert.equal(hint.hidden, false, 'Closing the panel shows the install hint again');

  const early = openPage({ fetchError: true, userAgentData: { platform: 'macOS', getHighEntropyValues: async () => ({ architecture: 'arm', bitness: '64' }) } });
  await early.settle();
  early.oneclick[0].emit('click');
  assert.equal(early.ids.get('next-steps').hidden, true, 'An unarmed button only scrolls to the download section');
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
  for (const check of [downloads, signing, guessedProcessor, nextSteps, visiblePlayback, motionAndDataPreferences, missingObserverAndBlockedPlay]) {
    try { await check(); }
    catch (error) { console.error(`${check.name}:`, error); process.exitCode = 1; }
  }
  if (!process.exitCode) console.log('Download selection, fallback and visible-video controls respect viewer choices, motion, data and visibility.');
})().catch((error) => { console.error(error); process.exitCode = 1; });
