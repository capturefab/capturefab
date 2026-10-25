// The workbench shell: the <capturefab-try> element and the `app` hub that the
// panes (sidebar.mjs, inspector.mjs) mount against. It lays out the desktop app's
// frame (src/gui/mod.rs, titlebar.rs, toolbar.rs, welcome.rs): a title bar with the
// camera's status, the stage with its scopes, the Activity log over a toolbar, and
// the Capture | Video and Features panes, which float or open as sheets when narrow.
import { icon } from './icons.mjs';
import { clock, count, fixed, fps as fpsText, json, plural, silence, span, withChord } from './format.mjs';
import {
  Pump, RECENT_SKIP_MS, STALL_MS, SimSource, WebcamSource, cameraPermission, cameraProblem, listWebcams, patternInfo, webcamAvailable
} from './sources.mjs';
import { Stage, spinner } from './stage.mjs';
import { DEFAULT_REGION, FocusMeter, History, Scopes, drawSparkline } from './scopes.mjs';

export const TAG = 'capturefab-try';

const TICK = 250;
const NOTICE_LIFE = 5000;
const SAVED_HOLD = 1600;
const COPIED_HOLD = 1500;
const LOG_LIMIT = 500;
const WIDE = 1080;
const COMPACT = 700;
const SVG = 'http://www.w3.org/2000/svg';

const PHASE_WORDS = {
  requesting: 'Starting…',
  starting: 'Starting…',
  streaming: 'Streaming',
  waiting: 'Waiting for frames',
  stopping: 'Stopping…',
  stopped: 'Ready',
  error: 'Ready'
};

const isApplePlatform = () => /Mac|iPhone|iPad|iPod/i.test(navigator.userAgentData?.platform || navigator.platform || navigator.userAgent || '');

/** An element with a class, and children (strings become text). */
const el = (tag, className = '', ...children) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  node.append(...children.filter((child) => child !== null && child !== undefined && child !== false));
  return node;
};

const button = (className, label, ...children) => {
  const node = el('button', className, ...children);
  node.type = 'button';
  if (label) node.setAttribute('aria-label', label);
  return node;
};

const setText = (node, text) => {
  if (node.textContent !== text) node.textContent = text;
};

const setAttr = (node, name, value) => {
  const text = String(value);
  if (node.getAttribute(name) !== text) node.setAttribute(name, text);
};

let marks = 0;

/** The brand mark, an inline copy of web/favicon.svg (assets/icons/capturefab.svg). */
const brand = (size) => {
  const id = `cf-mark-${(marks += 1)}`;
  const node = (tag, attrs) => {
    const child = document.createElementNS(SVG, tag);
    for (const [name, value] of Object.entries(attrs)) child.setAttribute(name, String(value));
    return child;
  };
  const svg = node('svg', { viewBox: '0 0 128 128', width: size, height: size, class: 'brand', 'aria-hidden': 'true', focusable: 'false' });
  const defs = node('defs', {});
  const gradient = (name, [x1, y1, x2, y2], from, to) => {
    const g = node('linearGradient', { id: `${id}-${name}`, x1, y1, x2, y2, gradientUnits: 'userSpaceOnUse' });
    g.append(node('stop', { 'stop-color': from }), node('stop', { offset: 1, 'stop-color': to }));
    return g;
  };
  defs.append(gradient('body', [20, 4, 104, 124], '#246de8', '#123a8b'), gradient('glass', [48, 46, 80, 84], '#a0fff0', '#36d9dd'));
  svg.append(
    defs,
    node('rect', { x: 4, y: 4, width: 120, height: 120, rx: 28, fill: `url(#${id}-body)` }),
    node('path', { d: 'M84 34H47C34.85 34 25 43.85 25 56V72C25 84.15 34.85 94 47 94H84', fill: 'none', stroke: '#f5fbff', 'stroke-width': 12, 'stroke-linecap': 'round' }),
    node('circle', { cx: 65, cy: 64, r: 20, fill: `url(#${id}-glass)` }),
    node('circle', { cx: 65, cy: 64, r: 10, fill: '#153c85' }),
    node('circle', { cx: 100, cy: 34, r: 7, fill: '#ffb66d' })
  );
  return svg;
};

/** Whether keyboard focus is in a field that takes typed letters. */
const typing = (target) => {
  if (!(target instanceof Element)) return false;
  if (target.isContentEditable) return true;
  const input = target.closest('input');
  if (input) return !['button', 'checkbox', 'radio', 'range', 'color', 'file', 'submit', 'reset', 'image'].includes(input.type);
  return Boolean(target.closest('textarea, select, [contenteditable], [role="textbox"], [role="combobox"], [role="spinbutton"], [role="slider"]'));
};

// localStorage key: 'off' turns off the single-key shortcuts (WCAG 2.1.4).
const SINGLE_KEYS = 'capturefab.try.single-keys';

const readSingleKeys = () => {
  try {
    return globalThis.localStorage?.getItem(SINGLE_KEYS) !== 'off';
  } catch {
    return true;
  }
};

/** Whether Space would press or toggle what has focus. */
const pressable = (target) => target instanceof Element
  && Boolean(target.closest('button, a[href], summary, input, select, label, [role="button"], [role="checkbox"], [role="switch"], [role="radio"], [role="tab"], [role="menuitem"], [role="option"]'));

export class CapturefabTry extends HTMLElement {
  /** CSSStyleSheets (or CSS text where sheets cannot be adopted), set by try.mjs before the element is created. */
  static styles = [];

  #root;
  #app;
  #isApple = isApplePlatform();
  #ui = {};
  #stage;
  #scopes;
  #meter = new FocusMeter(DEFAULT_REGION);
  #history = new History();
  #engine = null;
  #engineLoading = null;
  #engineFailed = '';
  #source = null;
  /** The open camera's identity, frozen: the same object for as long as it stays open. */
  #sourceState = null;
  #phase = 'idle';
  #frameMeta = { pixelFormat: '', path: null };
  #exposure = null;
  #histogramOpen = true;
  #focusOpen = false;
  #mirror = false;
  #imageOnly = false;
  #layout = 'wide';
  #docked = { sidebar: true, inspector: true };
  #floating = { sidebar: false, inspector: false };
  #paneOrder = [];
  #activityOpen = false;
  #unseen = '';
  #entries = [];
  #notice = null;
  #noticeTimer = 0;
  #capturing = false;
  #savedUntil = 0;
  #recording = { active: false, startedAt: 0, mime: '' };
  #destination = 'Saves to This browser';
  #sidebar = null;
  #inspector = null;
  #panes = null;
  #auto = null;
  #autoBusy = false;
  #featureToken = 0;
  #openToken = 0;
  #welcome = { state: 'idle', message: '', cameras: [], connecting: null, failures: new Map(), searching: false, permission: 'prompt', refused: false };
  #choosing = false;
  #isOpen = false;
  #closing = null;
  #session = null;
  #ticker = 0;
  #visibleAt = 0;
  #analysisErrorAt = -Infinity;
  #waiters = [];
  #lastFrameEvent = '';
  #handlers = new Map();
  /** Whether the last input was the keyboard, so moved focus shows its ring only then. */
  #keyboard = false;
  /** Whether Space, H, R and F act as shortcuts (off: remembered in localStorage). */
  #singleKeys = readSingleKeys();

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    const styles = CapturefabTry.styles;
    if (styles.every((s) => typeof s !== 'string')) this.#root.adoptedStyleSheets = styles;
    else for (const s of styles) this.#root.append(el('style', '', typeof s === 'string' ? s : [...s.cssRules].map((r) => r.cssText).join('\n')));
    this.#app = this.#makeApp();
    this.#build();
    this.#listenForever();
  }

  /** The hub the panes use (DESIGN §5). */
  get app() {
    return this.#app;
  }

  // ---------------------------------------------------------------------------
  // The app hub

  #makeApp() {
    const self = this;
    const state = Object.freeze({
      get source() { return self.#sourceState; },
      get phase() { return self.#phase; },
      get running() { return Boolean(self.#source?.running); },
      get frame() { return self.#frameInfo(); },
      get isApple() { return self.#isApple; },
      get mirror() { return self.#mirror; }
    });
    return Object.freeze({
      get engine() { return self.#engine; },
      state,
      on: (type, fn) => {
        if (!self.#handlers.has(type)) self.#handlers.set(type, new Set());
        self.#handlers.get(type).add(fn);
        return () => self.#handlers.get(type)?.delete(fn);
      },
      emit: (type, detail) => self.#emit(type, detail),
      features: () => self.#features(),
      setFeature: (name, text) => self.#setFeature(name, text),
      grabFrame: () => self.#grabFrame(),
      mediaStream: () => self.#source?.mediaStream?.() ?? null,
      flash: () => self.#stage.flash(),
      log: (level, message, detail) => self.#log(level, message, detail),
      notice: (message, level = 'info') => self.#showNotice(message, level),
      showScope: (name, visible) => self.#showScope(name, visible),
      scopeVisible: (name) => (name === 'focus' ? self.#focusOpen : self.#histogramOpen),
      start: () => self.#start(),
      stop: () => self.#stop(),
      /** Single-key shortcuts (Space, H, R, F): whether they are on, and a switch for a pane to offer. */
      get singleKeys() { return self.#singleKeys; },
      setSingleKeys: (on) => self.#setSingleKeys(on)
    });
  }

  #emit(type, detail) {
    for (const fn of [...(this.#handlers.get(type) ?? [])]) {
      try {
        fn(detail);
      } catch (error) {
        console.error(`Capturefab: a '${type}' listener failed`, error);
      }
    }
  }

  async #features() {
    const source = this.#source;
    if (!source) return [];
    if (source.kind === 'sim') return this.#engine ? this.#engine.simFeatures() : [];
    const track = source.track;
    if (!track) return [];
    const features = await this.#featureModule();
    return features?.webcamFeatures ? await features.webcamFeatures(track) : [];
  }

  async #setFeature(name, text) {
    const source = this.#source;
    if (!source) throw new Error('Select a camera first.');
    let result;
    if (source.kind === 'sim') {
      if (!this.#engine) throw new Error('The image engine is not ready yet.');
      result = await this.#engine.simSet(name, text);
    } else {
      const features = await this.#featureModule();
      if (!features?.applyWebcamFeature) throw new Error('Camera controls did not load. Reload the page, then try again.');
      if (!source.track) throw new Error('Start the stream to change camera controls.');
      result = await features.applyWebcamFeature(source.track, name, text);
      // Kept on the camera, not the track, so a Stop and Start does not lose it.
      source.asked = features.webcamAsked?.(source.track) ?? source.asked;
    }
    this.#emit('features-changed', result);
    return result;
  }

  #featureModule() {
    this.#featuresLoading ??= import('./features.mjs').catch((error) => {
      console.error('Capturefab: features.mjs did not load', error);
      this.#featuresLoading = null;
      return null;
    });
    return this.#featuresLoading;
  }

  #featuresLoading = null;

  async #grabFrame() {
    const source = this.#source;
    if (!source) return null;
    return source.grab();
  }

  #frameInfo() {
    const source = this.#source;
    if (!source?.hasFrame) return null;
    return {
      width: source.width,
      height: source.height,
      pixelFormat: this.#frameMeta.pixelFormat || (source.kind === 'sim' ? source.pixelFormat : 'RGB8'),
      count: source.count,
      path: this.#frameMeta.path
    };
  }

  // ---------------------------------------------------------------------------
  // Building

  #build() {
    const ui = this.#ui;
    const bench = el('div', 'workbench');
    bench.dataset.sidebar = 'open';
    bench.dataset.inspector = 'open';
    bench.dataset.layout = 'wide';
    bench.dataset.image = 'off';
    ui.bench = bench;

    ui.sidebar = el('aside', 'side left');
    ui.sidebar.id = 'cf-sidebar';
    ui.sidebar.setAttribute('aria-label', 'Capture and video settings');
    ui.sidebarHost = el('div', 'pane-host');
    ui.sidebarClose = this.#sheetClose('sidebar', 'Close Capture and video settings');
    ui.sidebar.append(ui.sidebarClose, ui.sidebarHost);

    ui.inspector = el('aside', 'side right');
    ui.inspector.id = 'cf-inspector';
    ui.inspector.setAttribute('aria-label', 'Camera features');
    ui.inspectorHost = el('div', 'pane-host');
    ui.inspectorClose = this.#sheetClose('inspector', 'Close Camera features');
    ui.inspector.append(ui.inspectorClose, ui.inspectorHost);

    const main = el('div', 'main');
    main.append(this.#buildTitlebar(), this.#buildStageArea(), this.#buildActivity(), this.#buildToolbar());
    ui.main = main;

    ui.scrim = el('div', 'scrim');
    ui.scrim.setAttribute('aria-hidden', 'true');
    ui.polite = el('div', 'sr-only');
    ui.polite.setAttribute('role', 'status');
    ui.polite.setAttribute('aria-live', 'polite');
    ui.assertive = el('div', 'sr-only');
    ui.assertive.setAttribute('role', 'alert');
    ui.probe = el('span', 'dot warn');
    ui.probe.hidden = true;

    ui.frame = el('div', 'frame', ui.sidebar, main, ui.inspector, ui.scrim);
    bench.append(ui.frame, ui.polite, ui.assertive, ui.probe);
    this.#root.append(bench);
    this.#syncPanes();
    this.#renderWelcome();
    this.#update();
  }

  /** A sheet's own Close button: on a small screen the title bar is inert under the sheet. */
  #sheetClose(name, label) {
    const close = button('btn icon sheet-close', label, icon('x', 16));
    close.title = 'Close';
    close.hidden = true;
    close.addEventListener('click', () => {
      this.#closePane(name);
      (name === 'sidebar' ? this.#ui.sidebarToggle : this.#ui.inspectorToggle).focus({ focusVisible: this.#keyboard });
    });
    return close;
  }

  #buildTitlebar() {
    const ui = this.#ui;
    const bar = el('header', 'titlebar');
    ui.sidebarToggle = button('btn icon', 'Capture and video settings', icon('sidebar', 16));
    ui.sidebarToggle.title = 'Capture and video settings';
    ui.sidebarToggle.setAttribute('aria-controls', 'cf-sidebar');
    ui.model = button('model', '', '');
    ui.model.title = 'Choose another camera';
    ui.model.hidden = true;
    ui.name = el('span', 'model idle', 'Capturefab');
    ui.mark = brand(20);
    ui.dot = el('span', 'dot');
    ui.word = el('span', 'word', 'Ready');
    ui.spark = el('canvas', 'spark');
    ui.spark.setAttribute('aria-hidden', 'true');
    ui.figure = el('span', 'figure', '');
    ui.rate = el('span', 'rate folded', ui.spark, ui.figure);
    ui.pill = el('span', 'pill accent', ui.dot, ui.word, ui.rate);
    ui.recTag = el('span', 'pill rec-tag', el('span', 'dot danger'), el('span', 'word', 'Recording'));
    ui.recTag.hidden = true;
    const status = el('div', 'status', ui.pill, ui.recTag);
    ui.status = status;
    const lead = el('div', 'lead', ui.sidebarToggle, ui.mark, ui.name, ui.model, status);

    ui.auto = button('btn toggle auto', 'Auto exposure', el('span', '', 'Auto', el('span', 'long', ' exposure')));
    ui.auto.setAttribute('aria-pressed', 'false');
    ui.auto.hidden = true;
    ui.stream = button('btn primary stream', '');
    ui.stream.hidden = true;
    if (this.#singleKeys) ui.stream.setAttribute('aria-keyshortcuts', 'Space');
    ui.imageToggle = button('btn icon image-toggle', 'Image only', icon('corners-out', 16));
    ui.imageToggle.title = this.#singleKeys ? withChord('Image only', 'F', this.#isApple) : 'Image only';
    ui.imageToggle.setAttribute('aria-pressed', 'false');
    ui.inspectorToggle = button('btn icon', 'Camera features', icon('sliders', 16));
    ui.inspectorToggle.title = 'Camera features';
    ui.inspectorToggle.setAttribute('aria-controls', 'cf-inspector');
    ui.close = button('btn icon close', 'Close', icon('x', 16));
    ui.close.title = 'Close';
    const actions = el('div', 'actions', ui.auto, ui.stream, ui.imageToggle, ui.inspectorToggle, el('span', 'divider'), ui.close);
    bar.append(lead, actions);

    ui.sidebarToggle.addEventListener('click', () => this.#togglePane('sidebar'));
    ui.inspectorToggle.addEventListener('click', () => this.#togglePane('inspector'));
    ui.model.addEventListener('click', () => this.#choose(true));
    ui.auto.addEventListener('click', () => this.#toggleAuto());
    ui.stream.addEventListener('click', () => this.#toggleStream());
    ui.imageToggle.addEventListener('click', () => this.#setImageOnly(!this.#imageOnly));
    ui.close.addEventListener('click', () => this.dispatchEvent(new Event('cf-close')));
    return bar;
  }

  #buildStageArea() {
    const ui = this.#ui;
    const area = el('main', 'stage-area');
    area.setAttribute('aria-label', 'Camera');
    this.#stage = new Stage({
      isApple: this.#isApple,
      singleKeys: this.#singleKeys,
      on: {
        histogram: () => this.#showScope('histogram'),
        focus: () => this.#showScope('focus'),
        image: () => this.#setImageOnly(!this.#imageOnly),
        region: (region, done) => this.#moveRegion(region, done),
        start: () => this.#start(),
        captureOne: () => this.#captureOne(),
        wake: () => this.#stage.wake()
      }
    });
    this.#scopes = new Scopes({
      hideFocus: () => this.#showScope('focus', false),
      resetPeak: () => {
        this.#meter.reset();
        this.#syncFocus();
      },
      metric: (key) => {
        this.#meter.setMetric(key);
        this.#syncFocus();
      },
      adjust: () => this.#openPane('inspector')
    });
    this.#stage.mountScopes(this.#scopes.element);
    ui.welcome = el('section', 'welcome');
    ui.welcome.setAttribute('aria-labelledby', 'cf-welcome-title');
    area.append(this.#stage.element, ui.welcome);
    ui.stageArea = area;
    return area;
  }

  #buildActivity() {
    const ui = this.#ui;
    const section = el('section', 'activity');
    section.id = 'cf-activity';
    section.setAttribute('aria-labelledby', 'cf-activity-title');
    const title = el('h2', '', 'Activity');
    title.id = 'cf-activity-title';
    ui.copyLog = button('link', 'Copy activity log', 'Copy log');
    ui.copyLog.addEventListener('click', () => this.#copyLog());
    ui.log = el('div', 'log');
    ui.log.setAttribute('role', 'list');
    ui.log.tabIndex = 0;
    ui.log.setAttribute('aria-label', 'Activity log');
    ui.logEmpty = el('p', 'empty', 'Browser actions will appear here, with their progress and results.');
    ui.log.append(ui.logEmpty);
    section.append(el('div', 'inner', el('header', '', title, ui.copyLog), ui.log));
    section.inert = true;
    ui.activity = section;
    return section;
  }

  #buildToolbar() {
    const ui = this.#ui;
    const bar = el('footer', 'toolbar');
    ui.capture = button('btn capture', '');
    ui.capture.setAttribute('aria-keyshortcuts', this.#isApple ? 'Meta+S' : 'Control+S');
    ui.record = button('btn record', '');
    ui.clock = el('span', 'pill clock');
    ui.clock.hidden = true;
    ui.clock.setAttribute('role', 'timer');
    ui.clock.setAttribute('aria-label', 'Recording time');
    const rec = el('span', 'rec', ui.record, ui.clock);
    ui.destinationText = el('span', 'text', this.#destination);
    ui.destination = button('btn subtle destination', '', icon('folder', 14), ui.destinationText);
    ui.destination.title = 'Change it in Capture';
    const left = el('div', 'left', ui.capture, rec, ui.destination);
    ui.toolStatus = el('div', 'status');
    ui.alert = el('span', 'dot alert');
    ui.alert.hidden = true;
    ui.activityToggle = button('btn subtle activity-toggle', 'Activity', icon('activity', 16), el('span', 'word', 'Activity'), ui.alert);
    ui.activityToggle.setAttribute('aria-expanded', 'false');
    ui.activityToggle.setAttribute('aria-controls', 'cf-activity');
    ui.activityToggle.title = 'Session activity';
    const right = el('div', 'right', ui.activityToggle);
    bar.append(left, ui.toolStatus, right);

    ui.capture.addEventListener('click', () => this.#capture());
    ui.record.addEventListener('click', () => this.#toggleRecording());
    ui.destination.addEventListener('click', () => this.#openPane('sidebar', true));
    ui.activityToggle.addEventListener('click', () => this.#setActivity(!this.#activityOpen));
    return bar;
  }

  /** Listeners that live as long as the element: keyboard, layout and pane events. */
  #listenForever() {
    this.addEventListener('pointerdown', () => { this.#keyboard = false; }, { capture: true });
    this.#ui.scrim.addEventListener('click', () => this.#closeFloating());
    new ResizeObserver(([entry]) => {
      const width = entry.contentBoxSize?.[0]?.inlineSize ?? entry.contentRect.width;
      this.#setLayout(width >= WIDE ? 'wide' : width >= COMPACT ? 'narrow' : 'compact');
    }).observe(this.#ui.bench);
    const app = this.#app;
    app.on('recording', (detail) => this.#onRecording(detail));
    app.on('destination', (detail) => {
      if (typeof detail === 'string' && detail) this.#destination = detail;
      this.#update();
    });
    app.on('saved', (record) => {
      // The pane logs the save; the toolbar says it, as the desktop's notice does. The
      // pane also announces it, but only while it shows (a closed pane is inert), so
      // the workbench speaks for it then: one announcement either way.
      if (record?.name) this.#showNotice(`Saved ${record.name}`, 'info', { log: false, announce: this.#ui.sidebar.inert });
      this.#savedUntil = performance.now() + SAVED_HOLD;
      this.#update();
      setTimeout(() => this.#update(), SAVED_HOLD + 20);
    });
    app.on('mirror', (on) => {
      this.#mirror = Boolean(on);
      this.#stage.setMirror(this.#mirror);
    });
    app.on('features-changed', () => this.#refreshFeatures());
  }

  // ---------------------------------------------------------------------------
  // Opening and closing

  /** Show the workbench; with `autostart`, ask for the default webcam at once. `keyboard`: opened from the keyboard. */
  async open({ autostart = true, keyboard = false } = {}) {
    this.#keyboard = keyboard;
    if (this.#closing) await this.#closing;
    if (this.#isOpen) {
      this.#focusInitial();
      return;
    }
    this.#isOpen = true;
    this.#session = new AbortController();
    const { signal } = this.#session;
    this.#visibleAt = performance.now();
    // On the document, so shortcuts work while the dialog itself has focus too.
    document.addEventListener('keydown', (event) => {
      const scope = this.closest('dialog') ?? this;
      // Focus that fell back to the page (a control went away) still belongs to the open modal.
      const stray = (event.target === document.body || event.target === document.documentElement) && scope.matches?.(':modal');
      if (!stray && !event.composedPath().includes(scope)) return;
      if (!['Shift', 'Control', 'Alt', 'Meta'].includes(event.key)) this.#keyboard = true;
      this.#key(event);
    }, { signal });
    document.addEventListener('visibilitychange', () => this.#visibilityChanged(), { signal });
    window.addEventListener('pagehide', () => this.#source?.close(), { signal });
    navigator.mediaDevices?.addEventListener?.('devicechange', () => this.#refreshCameras(), { signal });
    for (const query of ['(prefers-color-scheme: dark)', '(prefers-contrast: more)']) {
      globalThis.matchMedia?.(query).addEventListener('change', () => this.#update(), { signal });
    }
    this.#ticker = setInterval(() => this.#tick(), TICK);
    this.#loadEngine();
    this.#mountPanes();
    this.#choosing = false;
    if (autostart && webcamAvailable()) {
      // The click that opened the dialog lets the browser ask for the camera now.
      this.#openWebcam('');
    } else {
      this.#welcome.state = webcamAvailable() ? 'idle' : 'unsupported';
      this.#refreshCameras();
    }
    this.#renderWelcome();
    this.#update();
    this.#focusInitial();
  }

  #focusInitial() {
    requestAnimationFrame(() => {
      if (!this.#isOpen) return;
      const options = { preventScroll: true, focusVisible: this.#keyboard };
      if (this.#welcomeShown()) this.#root.getElementById('cf-welcome-title')?.focus(options);
      else if (!this.#ui.stream.hidden) this.#ui.stream.focus(options);
    });
  }

  /** Escape: put away the topmost floating panel, the camera chooser, image mode or the log first. */
  consumeEscape() {
    if (this.#imageOnly) {
      this.#setImageOnly(false);
      return true;
    }
    if (this.#choosing && this.#source) {
      this.#choose(false);
      return true;
    }
    if (this.#layout !== 'wide') {
      const open = this.#paneOrder.filter((name) => this.#floating[name]);
      if (open.length) {
        this.#closePane(open.at(-1));
        return true;
      }
    }
    if (this.#activityOpen) {
      this.#setActivity(false);
      this.#ui.activityToggle.focus();
      return true;
    }
    return false;
  }

  /**
   * Stop everything the dialog started: a recording (finished and saved), the camera,
   * the Pattern camera and the engine. Safe to call more than once.
   */
  shutdown() {
    if (!this.#isOpen) return this.#closing ?? Promise.resolve();
    this.#isOpen = false;
    this.#openToken += 1;
    this.#closing = (async () => {
      try {
        await this.#closeSource();
      } catch (error) {
        console.error('Capturefab: closing the camera failed', error);
      }
      clearInterval(this.#ticker);
      clearTimeout(this.#noticeTimer);
      this.#session?.abort();
      this.#session = null;
      for (const waiter of this.#waiters.splice(0)) waiter.reject(new Error('Closed.'));
      const engine = this.#engine;
      this.#engine = null;
      this.#engineLoading = null;
      if (engine) {
        this.#emit('engine', null);
        engine.terminate();
      }
      this.#setImageOnly(false);
      this.#choosing = false;
      this.#welcome.connecting = null;
      this.#notice = null;
      this.#update();
    })().finally(() => {
      this.#closing = null;
    });
    return this.#closing;
  }

  disconnectedCallback() {
    if (this.#isOpen) this.shutdown();
  }

  // ---------------------------------------------------------------------------
  // Engine and panes

  #loadEngine() {
    if (this.#engine) return Promise.resolve(this.#engine);
    if (this.#engineLoading) return this.#engineLoading;
    // The load is known by its promise: shutdown() drops it, so a load that finishes
    // after a close (and perhaps a reopen) ends its own engine instead of installing it.
    const loading = (async () => {
      this.#engineFailed = '';
      this.#update();
      try {
        const { Engine } = await import('./engine.mjs');
        const engine = await Engine.create();
        if (!this.#isOpen || this.#engineLoading !== loading) {
          engine.terminate();
          return null;
        }
        this.#engine = engine;
        engine.addEventListener?.('error', (event) => this.#engineStopped(engine, event?.detail?.message || event?.message || ''));
        this.#emit('engine', engine);
        this.#update();
        return engine;
      } catch (error) {
        if (this.#engineLoading !== loading) return null;
        this.#engineFailed = error?.message || String(error);
        this.#engineLoading = null;
        // The engine words its own failures; anything else gets said what it stopped.
        const text = /image engine/i.test(this.#engineFailed) ? this.#engineFailed : `The image engine did not load: ${this.#engineFailed}`;
        this.#showNotice(text, 'error');
        this.#update();
        return null;
      }
    })();
    this.#engineLoading = loading;
    return loading;
  }

  /** The worker stopped: let go of the engine, and bring up a new one for the camera. */
  #engineStopped(engine, message) {
    if (this.#engine !== engine) return;
    this.#engine = null;
    this.#engineLoading = null;
    engine.terminate();
    this.#emit('engine', null);
    const text = /stopped/i.test(message) ? message : `The image engine stopped${message ? `: ${message}` : '.'}`;
    this.#showNotice(text, 'error');
    if (this.#source?.kind === 'sim') {
      // The Pattern camera lived in that worker.
      this.#closeSource();
      return;
    }
    if (this.#isOpen && this.#engineRestarts < 2) {
      this.#engineRestarts += 1;
      this.#loadEngine();
    }
  }

  #engineRestarts = 0;

  /**
   * A hidden tab: the Pattern camera stops drawing frames nobody sees (acquisition
   * stays on, so its features stay locked), unless a recording needs them.
   */
  #visibilityChanged() {
    if (document.visibilityState === 'visible') {
      this.#visibleAt = performance.now();
      if (this.#pausedForHidden) {
        this.#pausedForHidden = false;
        this.#engine?.simPause(false).catch(() => {});
      }
      return;
    }
    const source = this.#source;
    if (source?.kind === 'sim' && source.running && !this.#recording.active && this.#engine?.simPause) {
      this.#pausedForHidden = true;
      this.#engine.simPause(true).catch(() => {});
    }
  }

  /** Set while the Pattern camera is paused for a hidden tab; simStart, simStop and simOpen clear the pause. */
  #pausedForHidden = false;

  async #mountPanes() {
    this.#panes ??= (async () => {
      const ui = this.#ui;
      const fallback = (host, text) => {
        host.replaceChildren(el('div', 'pane-fallback', icon('warning-circle', 20), el('p', '', text)));
      };
      const [sidebar, inspector] = await Promise.allSettled([import('./sidebar.mjs'), import('./inspector.mjs')]);
      try {
        if (sidebar.status !== 'fulfilled') throw sidebar.reason;
        this.#sidebar = sidebar.value.mountSidebar(ui.sidebarHost, this.#app);
        const text = this.#sidebar?.destinationText?.();
        if (text) this.#destination = text;
      } catch (error) {
        console.error('Capturefab: the Capture pane did not load', error);
        fallback(ui.sidebarHost, 'Capture settings did not load. Reload the page, then try again.');
      }
      try {
        if (inspector.status !== 'fulfilled') throw inspector.reason;
        this.#inspector = inspector.value.mountInspector(ui.inspectorHost, this.#app);
      } catch (error) {
        console.error('Capturefab: the Features pane did not load', error);
        fallback(ui.inspectorHost, 'Camera features did not load. Reload the page, then try again.');
      }
      this.#update();
    })();
    return this.#panes;
  }

  // ---------------------------------------------------------------------------
  // Sources

  #welcomeShown() {
    return !this.#source || this.#choosing;
  }

  /** Show or hide the camera chooser while a camera is open. */
  #choose(on) {
    this.#choosing = Boolean(on) && Boolean(this.#source);
    if (this.#choosing) this.#refreshCameras();
    this.#renderWelcome();
    this.#update();
    if (this.#choosing) this.#root.getElementById('cf-welcome-title')?.focus();
    else this.#ui.model.focus();
  }

  async #refreshCameras() {
    const welcome = this.#welcome;
    if (!webcamAvailable()) {
      welcome.cameras = [];
      if (welcome.state === 'idle') welcome.state = 'unsupported';
      this.#renderWelcome();
      return;
    }
    try {
      const [cameras, permission] = await Promise.all([listWebcams(), cameraPermission()]);
      welcome.cameras = cameras;
      welcome.permission = permission;
      if (permission === 'denied' && welcome.state !== 'requesting') {
        welcome.state = 'blocked';
        welcome.message = cameraProblem({ name: 'NotAllowedError' }).message;
      } else if (welcome.state === 'blocked' && (permission === 'granted' || (!welcome.refused && permission !== 'denied'))) {
        // A refusal the browser does not remember (Safari, a dismissed prompt) still
        // reads 'prompt' here; it stays said until the visitor tries again.
        welcome.state = 'idle';
        welcome.refused = false;
      }
      if (!cameras.length && welcome.state === 'idle') welcome.state = 'none';
      if (cameras.length && welcome.state === 'none') welcome.state = 'idle';
    } catch (error) {
      console.warn('Capturefab: listing cameras failed', error);
    }
    this.#renderWelcome();
  }

  async #searchAgain() {
    const welcome = this.#welcome;
    welcome.searching = true;
    this.#renderWelcome();
    const started = performance.now();
    if (welcome.state === 'none' || welcome.state === 'busy' || welcome.state === 'failed') welcome.state = 'idle';
    welcome.failures.clear();
    welcome.refused = false;
    await this.#refreshCameras();
    // A quick search still reads as one.
    const rest = 400 - (performance.now() - started);
    if (rest > 0) await new Promise((resolve) => setTimeout(resolve, rest));
    welcome.searching = false;
    this.#renderWelcome();
  }

  async #openWebcam(deviceId) {
    const token = ++this.#openToken;
    const welcome = this.#welcome;
    if (this.#source?.kind === 'webcam' && this.#source.deviceId === deviceId && deviceId) {
      this.#choose(false);
      return;
    }
    const refusedBefore = welcome.refused;
    welcome.refused = false;
    welcome.connecting = `webcam:${deviceId}`;
    welcome.failures.delete(welcome.connecting);
    welcome.failures.delete('webcam:');
    this.#renderWelcome();
    await this.#closeSource();
    if (token !== this.#openToken) return;
    // Say "Allow camera access…" only when the browser is about to ask.
    welcome.permission = await cameraPermission();
    if (token !== this.#openToken) return;
    welcome.state = welcome.permission === 'granted' ? 'idle' : 'requesting';
    this.#renderWelcome();
    this.#setPhase('requesting');
    try {
      const source = await WebcamSource.open(deviceId);
      if (token !== this.#openToken || !this.#isOpen) {
        source.close();
        return;
      }
      welcome.state = 'idle';
      welcome.connecting = null;
      this.#adopt(source);
      this.#setPhase(source.hasFrame ? 'streaming' : 'starting');
      this.#log('info', `Opened ${source.info.model}`, { camera: { model: source.info.model, transport: source.info.transport, serial: source.info.serial } });
      this.#refreshCameras();
    } catch (error) {
      if (token !== this.#openToken) return;
      const problem = cameraProblem(error);
      welcome.state = problem.state;
      welcome.message = problem.message;
      if (problem.state === 'blocked') {
        welcome.refused = true;
        // Refused again without the browser asking: Safari keeps an in-page refusal until a reload.
        if (refusedBefore && welcome.permission !== 'denied') welcome.message += " After you change camera access in the browser's website settings, reload this page.";
      }
      // A refusal is said once, above the cards; other failures on the camera's own card.
      if (problem.state !== 'blocked') welcome.failures.set(`webcam:${deviceId}`, problem.message);
      welcome.connecting = null;
      this.#setPhase('idle');
      this.#log(problem.state === 'failed' ? 'error' : 'warn', problem.message, { error: error?.name || 'Error', message: error?.message || String(error) });
      this.#refreshCameras();
    }
    this.#renderWelcome();
    this.#update();
  }

  async #openPattern() {
    const token = ++this.#openToken;
    const welcome = this.#welcome;
    if (this.#source?.kind === 'sim') {
      this.#choose(false);
      return;
    }
    welcome.connecting = 'sim:0';
    welcome.failures.delete('sim:0');
    this.#renderWelcome();
    await this.#closeSource();
    if (token !== this.#openToken) return;
    this.#setPhase('starting');
    try {
      const engine = await this.#loadEngine();
      if (!engine) throw new Error(this.#engineFailed || 'the image engine did not load');
      const source = await SimSource.open(engine);
      if (token !== this.#openToken || !this.#isOpen) {
        source.close();
        return;
      }
      welcome.connecting = null;
      this.#adopt(source);
      this.#setPhase('starting');
      await source.start();
      if (token !== this.#openToken) return;
      this.#setPhase('streaming');
      this.#log('info', `Opened ${source.info.model}`, { camera: { model: source.info.model, transport: source.info.transport, serial: source.info.serial } });
    } catch (error) {
      if (token !== this.#openToken) return;
      const message = `The Pattern camera did not start: ${error?.message || String(error)}`;
      welcome.failures.set('sim:0', message);
      welcome.connecting = null;
      await this.#closeSource();
      this.#setPhase('idle');
      this.#log('error', message);
    }
    this.#renderWelcome();
    this.#update();
  }

  /** Make `source` the open camera: its picture on the stage and its frames measured. */
  #adopt(source) {
    this.#source = source;
    this.#sourceState = Object.freeze({ ...source.info });
    this.#choosing = false;
    this.#frameMeta = { pixelFormat: '', path: null };
    this.#exposure = null;
    this.#scopes.setExposure(null);
    this.#meter.reset();
    this.#history.clear();
    this.#mirror = source.mirrorDefault;
    source.pump = new Pump({
      engine: () => this.#engine,
      want: () => this.#wants(),
      result: (result, want) => this.#analysis(result, want),
      error: (error) => this.#analysisError(error)
    });
    source.onFrame = (now, sized) => this.#onFrame(now, sized);
    if (source.kind === 'webcam') {
      source.addEventListener('ended', () => {
        if (this.#source !== source) return;
        this.#setPhase('stopped');
        this.#stage.setView(source.view);
        this.#showNotice('The camera stopped sending video. Select Start to open it again.', 'warn');
        this.#update();
      });
      source.addEventListener('mute', () => this.#update());
      source.addEventListener('unmute', () => this.#update());
    }
    this.#stage.setMirror(this.#mirror);
    this.#stage.setNatural(source.width, source.height);
    this.#stage.setView(source.view);
    const focusLost = !this.#root.activeElement || this.#ui.welcome.contains(this.#root.activeElement);
    this.#syncPanes(true);
    this.#emit('source', this.#app.state.source);
    this.#refreshFeatures();
    this.#renderWelcome();
    this.#syncFocus();
    this.#update();
    // The welcome screen held focus; give it to the stream's control.
    if (focusLost) requestAnimationFrame(() => this.#ui.stream.focus({ preventScroll: true, focusVisible: this.#keyboard }));
  }

  async #closeSource() {
    const source = this.#source;
    if (!source) return;
    if (this.#recording.active) await this.#stopRecording();
    if (this.#source !== source) return;
    this.#source = null;
    this.#sourceState = null;
    this.#pausedForHidden = false;
    source.pump = null;
    source.onFrame = null;
    source.close();
    this.#stage.setView(null);
    this.#stage.setNatural(0, 0);
    this.#auto = null;
    this.#exposure = null;
    this.#scopes.setExposure(null);
    this.#setPhase('idle');
    this.#syncPanes();
    this.#emit('source', null);
    this.#update();
  }

  async #start() {
    const source = this.#source;
    if (!source || source.running || this.#streamPending()) return;
    this.#setPhase(source.kind === 'webcam' ? 'requesting' : 'starting');
    this.#pausedForHidden = false;
    try {
      await source.start();
      // Closed or replaced meanwhile: make sure the camera it opened is let go.
      if (this.#source !== source) {
        source.close();
        return;
      }
      this.#stage.setView(source.view);
      this.#history.clear();
      this.#setPhase('streaming');
      if (source.kind === 'webcam' && source.asked) await this.#restoreWebcam(source);
    } catch (error) {
      if (this.#source !== source) {
        source.close();
        return;
      }
      const message = source.kind === 'webcam' ? cameraProblem(error).message : `The Pattern camera did not start: ${error?.message || error}`;
      this.#setPhase('stopped');
      this.#showNotice(message, 'error', { detail: { error: error?.name || 'Error', message: error?.message || String(error) } });
    }
    this.#update();
  }

  /** Start or Stop, from the button and Space; ignored while one is on its way. */
  #toggleStream() {
    if (!this.#source || this.#streamPending()) return;
    if (this.#source.running) this.#stop();
    else this.#start();
  }

  /** Whether a start or a stop is on its way. */
  #streamPending() {
    return this.#phase === 'starting' || this.#phase === 'requesting' || this.#phase === 'stopping';
  }

  /** A webcam opened again after a stop: restate what was set in Features. */
  async #restoreWebcam(source) {
    const features = await this.#featureModule();
    if (!features?.restoreWebcamSettings || this.#source !== source || !source.track) return;
    const kept = await features.restoreWebcamSettings(source.track, source.asked);
    if (this.#source !== source) return;
    if (!kept) this.#showNotice('Some camera settings did not carry over after the restart.', 'warn');
    this.#emit('features-changed', null);
  }

  async #stop() {
    const source = this.#source;
    if (!source || !source.running || this.#streamPending()) return;
    if (this.#recording.active) await this.#stopRecording();
    this.#pausedForHidden = false;
    this.#setPhase('stopping');
    try {
      await source.stop();
    } catch (error) {
      this.#log('warn', `Stopping did not finish cleanly: ${error?.message || error}`);
    }
    if (this.#source !== source) return;
    this.#stage.setView(source.view);
    this.#setPhase('stopped');
    this.#update();
  }

  #setPhase(phase) {
    if (phase === this.#phase) return;
    this.#phase = phase;
    this.#emit('phase', phase);
    const word = PHASE_WORDS[phase];
    if (word && phase !== 'requesting') this.#announce(word);
    this.#update();
  }

  #onFrame(now, sized) {
    const source = this.#source;
    if (!source) return;
    if (sized) {
      this.#stage.setNatural(source.width, source.height);
      if (source.count <= 1) {
        this.#stage.reveal();
        this.#stage.wake();
      }
      this.#update();
    }
    if (this.#phase === 'starting' || this.#phase === 'waiting' || this.#phase === 'requesting') this.#setPhase('streaming');
    if (this.#waiters.length) for (const waiter of this.#waiters.splice(0)) waiter.resolve();
  }

  /** The next frame, or a rejection after `timeout` ms. */
  #nextFrame(timeout = 5000) {
    return new Promise((resolve, reject) => {
      const waiter = { resolve, reject };
      this.#waiters.push(waiter);
      setTimeout(() => {
        const i = this.#waiters.indexOf(waiter);
        if (i >= 0) {
          this.#waiters.splice(i, 1);
          reject(new Error('No frame arrived within 5 s.'));
        }
      }, timeout);
    });
  }

  // ---------------------------------------------------------------------------
  // Measurements

  #wants() {
    const source = this.#source;
    const focus = this.#focusOpen && source?.hasFrame ? this.#meter.pixels(source.width, source.height) : null;
    return { scopes: this.#histogramOpen, focus };
  }

  #analysis(result, want) {
    if (!result || !this.#source) return;
    if (result.frame) {
      this.#frameMeta = { pixelFormat: result.frame.pixelFormat || this.#frameMeta.pixelFormat, path: result.frame.path ?? this.#frameMeta.path };
    }
    if (want.scopes && result.scopes) {
      this.#exposure = result.scopes;
      this.#scopes.setExposure(result.scopes);
      this.#scopes.show('histogram', this.#histogramOpen);
    }
    if (want.focus) {
      if (result.focus) this.#meter.record(result.focus);
      this.#syncFocus();
    }
  }

  #analysisError(error) {
    const now = performance.now();
    if (now - this.#analysisErrorAt < 10000) return;
    this.#analysisErrorAt = now;
    this.#log('warn', `Measuring the frame failed: ${error?.message || error}`);
  }

  #showScope(name, visible) {
    if (name === 'focus') {
      this.#focusOpen = visible === undefined ? !this.#focusOpen : Boolean(visible);
      if (this.#focusOpen) this.#meter.reset();
    } else {
      this.#histogramOpen = visible === undefined ? !this.#histogramOpen : Boolean(visible);
    }
    this.#scopes.show('histogram', this.#histogramOpen && Boolean(this.#exposure));
    this.#scopes.show('focus', this.#focusOpen);
    this.#syncFocus();
    this.#emit('scope', { name: name === 'focus' ? 'focus' : 'histogram', visible: name === 'focus' ? this.#focusOpen : this.#histogramOpen });
    this.#update();
  }

  #moveRegion(region, done) {
    this.#meter.moveTo(region);
    this.#syncFocus();
    if (done) {
      // Score it where it landed, without waiting out the focus interval.
      this.#source?.pump?.reset();
    }
  }

  #syncFocus() {
    const meter = this.#meter;
    this.#scopes.setFocus(meter);
    this.#stage.setRegion({
      visible: this.#focusOpen && Boolean(this.#source?.hasFrame),
      region: meter.region,
      label: meter.label(),
      sharp: meter.inFocus,
      locks: meter.locks
    });
  }

  // ---------------------------------------------------------------------------
  // Features shown in the title bar

  async #refreshFeatures() {
    const token = ++this.#featureToken;
    let features = [];
    try {
      features = await this.#features();
    } catch (error) {
      console.warn('Capturefab: reading features failed', error);
    }
    if (token !== this.#featureToken) return;
    const auto = Array.isArray(features) ? features.find((f) => f.name === 'ExposureAuto' && f.writable) : null;
    this.#auto = auto ? { value: auto.value } : null;
    this.#update();
  }

  async #toggleAuto() {
    if (!this.#auto || this.#autoBusy) return;
    const on = this.#auto.value === 'Continuous';
    this.#autoBusy = true;
    this.#update();
    try {
      await this.#setFeature('ExposureAuto', on ? 'Off' : 'Continuous');
      this.#log('info', on ? 'Auto exposure off' : 'Auto exposure on');
    } catch (error) {
      this.#showNotice(error?.message || String(error), 'error');
    } finally {
      this.#autoBusy = false;
      this.#update();
    }
  }

  // ---------------------------------------------------------------------------
  // Capture and recording (the panes own the details)

  #canCapture() {
    return Boolean(this.#source?.hasFrame && this.#engine && this.#sidebar?.capture);
  }

  async #capture() {
    if (this.#capturing || !this.#canCapture()) return;
    this.#capturing = true;
    this.#update();
    try {
      await this.#sidebar.capture();
      this.#savedUntil = performance.now() + SAVED_HOLD;
      setTimeout(() => this.#update(), SAVED_HOLD + 20);
    } catch (error) {
      // The pane logs what went wrong; the toolbar says it.
      this.#showNotice(error?.message || String(error), 'error', { log: false });
    } finally {
      this.#capturing = false;
      this.#update();
    }
  }

  /** "Capture one frame" on the ready screen: start, take the first frame, stop. */
  async #captureOne() {
    const source = this.#source;
    if (!source || this.#capturing) return;
    if (source.hasFrame) {
      await this.#capture();
      return;
    }
    try {
      const frame = this.#nextFrame();
      frame.catch(() => {});
      await this.#start();
      if (this.#source !== source || !source.running) return;
      await frame;
      if (!this.#engine) await this.#loadEngine();
      await this.#capture();
    } catch (error) {
      this.#showNotice(error?.message || String(error), 'error');
    } finally {
      if (this.#source === source && source.running) await this.#stop();
    }
  }

  async #toggleRecording() {
    if (!this.#sidebar?.toggleRecording) return;
    if (!this.#recording.active && !this.#source?.running) return;
    try {
      await this.#sidebar.toggleRecording();
    } catch (error) {
      this.#showNotice(error?.message || String(error), 'error', { log: false });
    }
    this.#update();
  }

  async #stopRecording() {
    if (!this.#recording.active || !this.#sidebar?.toggleRecording) return;
    try {
      await this.#sidebar.toggleRecording();
    } catch (error) {
      this.#log('error', `The recording did not finish: ${error?.message || error}`);
    }
  }

  #onRecording(detail) {
    const active = Boolean(detail?.active);
    this.#recording = { active, startedAt: active ? (detail.startedAt ?? performance.now()) : 0, mime: detail?.mime ?? '' };
    this.#update();
  }

  // ---------------------------------------------------------------------------
  // Layout: panes, image only, Activity

  #setLayout(layout) {
    if (layout === this.#layout) return;
    const was = this.#layout;
    this.#layout = layout;
    this.#ui.bench.dataset.layout = layout;
    if (was === 'wide' || layout === 'wide') this.#floating = { sidebar: false, inspector: false };
    if (layout === 'compact' && this.#floating.sidebar && this.#floating.inspector) this.#floating.sidebar = false;
    this.#syncPanes();
  }

  /** Whether a pane shows; like the desktop, none does before a camera opens. */
  #paneOpen(name) {
    if (!this.#source) return false;
    return this.#layout === 'wide' ? this.#docked[name] : this.#floating[name];
  }

  #togglePane(name) {
    if (this.#paneOpen(name)) this.#closePane(name);
    else this.#openPane(name, this.#layout !== 'wide');
  }

  /** Open a pane; `focus` moves keyboard focus into it. */
  #openPane(name, focus = false) {
    if (this.#imageOnly) this.#setImageOnly(false);
    if (this.#layout === 'wide') this.#docked[name] = true;
    else {
      // A sheet at a time on a small screen.
      if (this.#layout === 'compact') for (const other of ['sidebar', 'inspector']) if (other !== name) this.#floating[other] = false;
      this.#floating[name] = true;
      this.#paneOrder = [...this.#paneOrder.filter((n) => n !== name), name];
    }
    this.#syncPanes(true);
    if (focus) {
      const pane = name === 'sidebar' ? this.#ui.sidebar : this.#ui.inspector;
      requestAnimationFrame(() => this.#paneFocusTarget(pane).focus?.({ preventScroll: true, focusVisible: this.#keyboard }));
    }
  }

  /**
   * Where focus goes in a pane that opens: the sheet's Close button, else the pane's
   * title, else its first control that is not a text field. Never a text field, which
   * would bring up an on-screen keyboard over the sheet.
   */
  #paneFocusTarget(pane) {
    const close = pane.querySelector(':scope > .sheet-close');
    if (close && !close.hidden) return close;
    const title = pane.querySelector('.pane-title');
    if (title) {
      if (!title.hasAttribute('tabindex')) title.tabIndex = -1;
      return title;
    }
    const controls = pane.querySelectorAll('button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])');
    const target = [...controls].find((node) => !typing(node) && !node.closest('[hidden]'));
    if (target) return target;
    if (!pane.hasAttribute('tabindex')) pane.tabIndex = -1;
    return pane;
  }

  #closePane(name) {
    const pane = name === 'sidebar' ? this.#ui.sidebar : this.#ui.inspector;
    const hadFocus = pane.contains(this.#root.activeElement);
    if (this.#layout === 'wide') this.#docked[name] = false;
    else this.#floating[name] = false;
    this.#paneOrder = this.#paneOrder.filter((n) => n !== name);
    this.#syncPanes();
    if (hadFocus) (name === 'sidebar' ? this.#ui.sidebarToggle : this.#ui.inspectorToggle).focus();
  }

  #closeFloating() {
    if (this.#layout === 'wide') return;
    for (const name of ['sidebar', 'inspector']) if (this.#floating[name]) this.#closePane(name);
  }

  #syncPanes(opening = false) {
    const ui = this.#ui;
    const sidebar = this.#paneOpen('sidebar') && !this.#imageOnly;
    const inspector = this.#paneOpen('inspector') && !this.#imageOnly;
    ui.bench.classList.toggle('opening', opening);
    ui.bench.dataset.sidebar = sidebar ? 'open' : 'closed';
    ui.bench.dataset.inspector = inspector ? 'open' : 'closed';
    ui.sidebar.inert = !sidebar;
    ui.inspector.inert = !inspector;
    setAttr(ui.sidebarToggle, 'aria-expanded', sidebar);
    setAttr(ui.inspectorToggle, 'aria-expanded', inspector);
    ui.sidebarToggle.style.color = sidebar ? 'var(--text)' : '';
    ui.inspectorToggle.style.color = inspector ? 'var(--text)' : '';
    ui.sidebarToggle.hidden = !this.#source;
    ui.inspectorToggle.hidden = !this.#source;
    // A sheet is modal over the stage on a small screen, with its own way out.
    const compact = this.#layout === 'compact';
    const sheet = compact && (sidebar || inspector);
    ui.main.inert = sheet;
    for (const [pane, close, open] of [[ui.sidebar, ui.sidebarClose, sidebar], [ui.inspector, ui.inspectorClose, inspector]]) {
      close.hidden = !compact;
      if (compact && open) {
        pane.setAttribute('role', 'dialog');
        pane.setAttribute('aria-modal', 'true');
      } else {
        pane.removeAttribute('role');
        pane.removeAttribute('aria-modal');
      }
    }
  }

  #setImageOnly(on) {
    if (on === this.#imageOnly) return;
    if (on && !this.#source?.hasFrame) return;
    const focusLost = on && [this.#ui.sidebar, this.#ui.inspector, this.#ui.activity, this.#ui.main.querySelector('.titlebar'), this.#ui.main.querySelector('.toolbar')]
      .some((part) => part?.contains(this.#root.activeElement));
    this.#imageOnly = on;
    this.#ui.bench.dataset.image = on ? 'on' : 'off';
    for (const part of [this.#ui.main.querySelector('.titlebar'), this.#ui.main.querySelector('.toolbar')]) part.inert = on;
    this.#ui.activity.inert = on || !this.#activityOpen;
    this.#syncPanes();
    if (focusLost || on) this.#stage.wake();
    if (focusLost) this.#stage.element.querySelector('.controls .image')?.focus();
    this.#update();
  }

  #setActivity(open) {
    this.#activityOpen = open;
    const ui = this.#ui;
    ui.activity.classList.toggle('open', open);
    ui.activity.inert = !open || this.#imageOnly;
    setAttr(ui.activityToggle, 'aria-expanded', open);
    if (open) {
      this.#unseen = '';
      requestAnimationFrame(() => { ui.log.scrollTop = ui.log.scrollHeight; });
    }
    this.#update();
  }

  // ---------------------------------------------------------------------------
  // Keyboard (keys.rs; browser-owned chords such as ⌘R, ⌘J, ⌘W and ⌘L stay unbound)

  #key(event) {
    if (event.defaultPrevented || event.isComposing) return;
    const target = event.composedPath()[0];
    const mod = this.#isApple ? event.metaKey : event.ctrlKey;
    const other = this.#isApple ? event.ctrlKey : event.metaKey;
    const key = event.key;
    if (key === 'Escape') {
      if (this.consumeEscape()) {
        event.preventDefault();
        event.stopPropagation();
      }
      return;
    }
    if (mod && !other) {
      const lower = key.length === 1 ? key.toLowerCase() : key;
      let handled = true;
      if (lower === 's' && !event.shiftKey && !event.altKey) this.#capture();
      else if ((key === '=' || key === '+') && !event.altKey) this.#stage.zoomIn();
      else if ((key === '-' || key === '_') && !event.altKey) this.#stage.zoomOut();
      else if (event.code === 'Digit0' || event.code === 'Numpad0') (event.altKey ? this.#stage.actual() : this.#stage.fit());
      else if (lower === 'f' && !event.shiftKey && !event.altKey) {
        this.#openPane('inspector');
        this.#inspector?.focusSearch?.();
      } else handled = false;
      if (handled) {
        event.preventDefault();
        event.stopPropagation();
      }
      return;
    }
    if (event.metaKey || event.ctrlKey || event.altKey || event.shiftKey || typing(target)) return;
    // Single keys (WCAG 2.1.4): only on the stage and the app's own surfaces, never on a
    // control, the Activity log or the panes, and not at all once turned off.
    if (!this.#singleKeys || !this.#onSurface(target)) return;
    const lower = key.length === 1 ? key.toLowerCase() : key;
    if (key === ' ') {
      if (!this.#source) return;
      event.preventDefault();
      if (event.repeat) return;
      this.#toggleStream();
    } else if (lower === 'h') {
      event.preventDefault();
      this.#showScope('histogram');
    } else if (lower === 'r') {
      event.preventDefault();
      this.#showScope('focus');
    } else if (lower === 'f') {
      event.preventDefault();
      this.#setImageOnly(!this.#imageOnly);
    }
  }

  /**
   * Whether single-key shortcuts may act on `target`: the dialog, the page behind it,
   * or a part of the workbench that is not itself a control (the stage, the bars).
   */
  #onSurface(target) {
    if (!(target instanceof Element)) return true;
    if (target === this || target === document.body || target === document.documentElement || target === this.closest('dialog')) return true;
    if (pressable(target) || typing(target)) return false;
    return !target.closest('.log, .region-handle, .side, [role="slider"], [role="list"], [role="listbox"], [role="menu"], [role="tablist"], [role="radiogroup"]');
  }

  /** Turn the single-key shortcuts (Space, H, R, F) on or off; remembered in this browser. */
  #setSingleKeys(on) {
    this.#singleKeys = Boolean(on);
    try {
      if (on) globalThis.localStorage?.removeItem(SINGLE_KEYS);
      else globalThis.localStorage?.setItem(SINGLE_KEYS, 'off');
    } catch {
      // Storage may be blocked; the choice lasts for this page.
    }
    this.#syncShortcutHints();
    this.#emit('single-keys', this.#singleKeys);
  }

  /** aria-keyshortcuts and tooltips name only the shortcuts that work. */
  #syncShortcutHints() {
    const ui = this.#ui;
    if (this.#singleKeys) ui.stream.setAttribute('aria-keyshortcuts', 'Space');
    else ui.stream.removeAttribute('aria-keyshortcuts');
    ui.imageToggle.title = this.#singleKeys ? withChord('Image only', 'F', this.#isApple) : 'Image only';
    this.#stage?.setSingleKeys(this.#singleKeys);
    this.#update();
  }

  // ---------------------------------------------------------------------------
  // Activity log and notices

  #log(level, message, detail) {
    const levels = { info: 'info', warn: 'warn', warning: 'warn', error: 'error' };
    const kind = levels[level] ?? 'info';
    const entry = { time: new Date(), level: kind, message: String(message ?? ''), detail: detail ?? null };
    this.#entries.push(entry);
    const ui = this.#ui;
    const log = ui.log;
    const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 24;
    ui.logEmpty.remove();
    log.append(this.#entryRow(entry));
    while (this.#entries.length > LOG_LIMIT) {
      this.#entries.shift();
      log.firstElementChild?.remove();
    }
    if (atBottom || !this.#activityOpen) log.scrollTop = log.scrollHeight;
    if (kind !== 'info' && !this.#activityOpen && this.#unseen !== 'error') this.#unseen = kind;
    this.#update();
  }

  #entryRow(entry) {
    const row = el('div', `entry ${entry.level}`);
    row.setAttribute('role', 'listitem');
    const time = el('time', '', entry.time.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' }));
    time.dateTime = entry.time.toISOString();
    time.title = entry.time.toLocaleString();
    const mark = el('span', `mark ${entry.level === 'warn' ? 'warn' : entry.level === 'error' ? 'error' : ''}`);
    if (entry.level === 'warn') mark.append(icon('warning', 11));
    if (entry.level === 'error') mark.append(icon('warning-circle', 11));
    if (entry.level !== 'info') mark.append(el('span', 'sr-only', entry.level === 'warn' ? 'Warning:' : 'Error:'));
    const message = el('span', 'message', entry.message);
    row.append(time, mark, message);
    if (entry.detail !== null && entry.detail !== undefined) {
      const text = typeof entry.detail === 'string' ? entry.detail : json(entry.detail);
      const more = button('link more', 'Details', icon('caret-right', 11));
      more.setAttribute('aria-expanded', 'false');
      more.title = 'Details';
      const pre = el('pre', '', text);
      const copy = button('link copy', 'Copy details', 'Copy');
      copy.addEventListener('click', () => this.#copy(text, copy, 'Copy'));
      const detail = el('div', 'detail', pre, copy);
      detail.hidden = true;
      more.addEventListener('click', () => {
        detail.hidden = !detail.hidden;
        more.setAttribute('aria-expanded', String(!detail.hidden));
      });
      row.append(more, detail);
    }
    return row;
  }

  async #copy(text, control, label) {
    try {
      await navigator.clipboard.writeText(text);
      control.textContent = 'Copied';
      setTimeout(() => { control.textContent = label; }, COPIED_HOLD);
    } catch {
      this.#showNotice('Copying did not work in this browser. Select the text, then copy it.', 'warn');
    }
  }

  #copyLog() {
    const lines = this.#entries.map((entry) => {
      const head = `${entry.time.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' })} ${entry.level.toUpperCase().padEnd(5)} ${entry.message}`;
      if (entry.detail === null || entry.detail === undefined) return head;
      const detail = typeof entry.detail === 'string' ? entry.detail : json(entry.detail);
      return `${head}\n${detail.replace(/^/gm, '    ')}`;
    });
    this.#copy(lines.join('\n') || 'No activity yet.', this.#ui.copyLog, 'Copy log');
  }

  /** A notice in the toolbar's centre for 5 s (errors stay until dismissed); also logged. */
  #showNotice(message, level = 'info', { log = true, detail, announce = true } = {}) {
    const kind = level === 'warning' ? 'warn' : level === 'warn' || level === 'error' ? level : 'info';
    if (log) this.#log(kind, message, detail);
    this.#notice = { message: String(message), level: kind };
    clearTimeout(this.#noticeTimer);
    if (kind !== 'error') {
      this.#noticeTimer = setTimeout(() => {
        this.#notice = null;
        this.#update();
      }, NOTICE_LIFE);
    }
    if (announce) this.#announce(String(message), kind === 'error');
    this.#update();
  }

  #announce(text, urgent = false) {
    const region = urgent ? this.#ui.assertive : this.#ui.polite;
    region.textContent = '';
    // A fresh node makes screen readers read repeated text again.
    requestAnimationFrame(() => { region.textContent = text; });
  }

  // ---------------------------------------------------------------------------
  // Rendering

  #tick() {
    const now = performance.now();
    const source = this.#source;
    // Paused for a hidden tab: no frames are due, so neither the rate nor a stall is measured.
    if (source && !this.#pausedForHidden) {
      source.sample(now);
      if (source.running) {
        if (source.rated) this.#history.offer(now, source.fps, source.skipped);
        const stalled = this.#silence(now) >= STALL_MS;
        if (stalled && this.#phase === 'streaming') this.#setPhase('waiting');
      }
    }
    this.#update(now);
    const frame = this.#frameInfo();
    const key = frame ? `${frame.width}x${frame.height}|${frame.pixelFormat}|${frame.count}|${frame.path}` : '';
    if (key !== this.#lastFrameEvent) {
      this.#lastFrameEvent = key;
      if (frame) this.#emit('frame', frame);
    }
  }

  /** How long the running stream has gone without a frame while this tab was visible. */
  #silence(now) {
    const source = this.#source;
    if (!source?.running || document.visibilityState !== 'visible') return 0;
    return Math.min(source.silence(now), now - this.#visibleAt);
  }

  #toneColors() {
    // Custom properties hold light-dark() unresolved; a styled probe resolves them.
    const ui = this.#ui;
    const tone = getComputedStyle(ui.dot).backgroundColor;
    const flag = getComputedStyle(ui.probe).backgroundColor;
    return { tone, flag };
  }

  #update(now = performance.now()) {
    const ui = this.#ui;
    if (!ui.bench) return;
    const source = this.#source;
    const info = source?.info ?? null;
    const running = Boolean(source?.running);
    const phase = this.#phase;
    const silent = this.#silence(now);

    // Title bar: model, status pill, actions.
    // The camera's model names the window, as on the desktop; a button that offers the other cameras.
    ui.model.hidden = !info;
    ui.name.hidden = Boolean(info);
    if (info) {
      setText(ui.model, info.model);
      // The name may be cut short on a narrow title bar; the tooltip and name keep it whole.
      ui.model.title = `${info.model} — Choose another camera`;
      ui.model.setAttribute('aria-label', `${info.model}. Choose another camera`);
    }
    const word = PHASE_WORDS[phase] ?? '';
    ui.pill.hidden = !word || (!info && phase !== 'requesting');
    setText(ui.word, word);
    const live = phase === 'streaming' || phase === 'waiting';
    const tone = phase === 'waiting' ? 'warn' : live ? 'live' : 'accent';
    for (const t of ['live', 'warn', 'accent']) ui.pill.classList.toggle(t, t === tone);
    ui.rate.classList.toggle('folded', !live);
    setText(ui.figure, live ? fpsText(source.fps) : '');
    if (live) {
      const { tone: color, flag } = this.#toneColors();
      drawSparkline(ui.spark, this.#history, { color, flag, coming: true });
      const about = this.#history.ready()
        ? this.#history.about('Frame rate', (v) => fpsText(v), 'skipped frames')
        : 'No frame rate yet';
      setAttr(ui.spark, 'title', about);
      setAttr(ui.pill, 'title', phase === 'waiting' ? silence(silent) : about);
    } else {
      setAttr(ui.pill, 'title', word);
    }
    ui.recTag.hidden = !this.#recording.active;

    ui.auto.hidden = !this.#auto;
    if (this.#auto) {
      const on = this.#auto.value === 'Continuous';
      setAttr(ui.auto, 'aria-pressed', on);
      ui.auto.disabled = this.#autoBusy;
      setAttr(ui.auto, 'title', 'Tune exposure, gain and frame rate');
    }

    ui.stream.hidden = !info;
    if (info) {
      const pending = phase === 'starting' || phase === 'requesting' || phase === 'stopping';
      // While a start or stop is on its way, the button keeps the label it was pressed with.
      const stopLabel = phase === 'stopping' || (running && !pending);
      const key = `${stopLabel}|${pending}`;
      if (ui.stream.dataset.key !== key) {
        ui.stream.dataset.key = key;
        ui.stream.className = `btn stream ${stopLabel ? '' : 'primary'}`;
        ui.stream.replaceChildren(pending ? spinner(14) : icon(stopLabel ? 'stop-fill' : 'play-fill', 14), el('span', 'label', stopLabel ? 'Stop' : 'Start'));
        ui.stream.setAttribute('aria-label', stopLabel ? 'Stop' : 'Start');
      }
      // Not `disabled`: the button keeps keyboard focus (and the shortcuts) while it waits.
      setAttr(ui.stream, 'aria-disabled', pending);
      setAttr(ui.stream, 'title', phase === 'stopping' ? 'Stopping the stream…' : pending ? 'Starting the stream…' : this.#singleKeys ? withChord('Start or stop acquisition', 'Space', this.#isApple) : 'Start or stop acquisition');
    }
    ui.imageToggle.hidden = !source?.hasFrame;
    setAttr(ui.imageToggle, 'aria-pressed', this.#imageOnly);

    // Stage.
    this.#updateStage(now, silent);

    // Toolbar.
    this.#updateToolbar(now);
  }

  #updateStage(now, silent) {
    const source = this.#source;
    const stage = this.#stage;
    const info = source?.info;
    const frame = this.#frameInfo();
    stage.setToggles({ histogram: this.#histogramOpen, focus: this.#focusOpen, image: this.#imageOnly, intensity: Boolean(source), frame: Boolean(source?.hasFrame) });
    this.#scopes.show('histogram', this.#histogramOpen && Boolean(this.#exposure) && Boolean(source));
    this.#scopes.show('focus', this.#focusOpen && Boolean(source));
    if (!source) {
      stage.setReady(null);
      stage.setBadges({});
      stage.setLabel('No camera');
      return;
    }
    if (frame) {
      stage.setLabel(`${info.model}. ${source.running ? 'Live camera' : 'Last frame'} preview, ${frame.width} by ${frame.height} pixels.`);
      const parts = [];
      if (this.#imageOnly) {
        parts.push(info.model);
        if (source.running) parts.push(fpsText(source.fps));
      }
      parts.push(`${frame.width} × ${frame.height}`, frame.pixelFormat, `${count(frame.count)} frames`);
      let lost = null;
      if (source.skipped > 0) {
        const recent = now - source.skippedAt < RECENT_SKIP_MS;
        lost = {
          text: `${count(source.skipped)} skipped`,
          recent,
          about: `${plural(source.skipped, 'frame', 'frames')} skipped by this preview since the stream started, the last ${span(now - source.skippedAt)} ago`
        };
      }
      stage.setDetails(parts, lost);
    } else {
      stage.setLabel(`${info.model}. Live camera preview.`);
      stage.setDetails([`${count(source.count)} frames`]);
    }
    stage.setBadges({ lastFrame: !source.running && Boolean(frame), stall: source.running && silent >= STALL_MS ? silent : 0 });
    if (frame) {
      stage.setReady(null);
    } else if (source.running || this.#phase === 'starting' || this.#phase === 'requesting') {
      const late = source.running && silent >= STALL_MS;
      stage.setReady({
        title: late ? 'No picture yet' : 'Waiting for the first frame…',
        text: late ? 'The camera is open but sends no picture. Another app may be using it, or it may be covered.' : ''
      });
    } else {
      stage.setReady({ title: 'Ready for your first frame', start: { busy: false }, capture: { busy: this.#capturing, disabled: !this.#sidebar } });
    }
  }

  #updateToolbar(now) {
    const ui = this.#ui;
    const source = this.#source;
    // Capture.
    const saved = !this.#capturing && now < this.#savedUntil;
    const canCapture = this.#canCapture();
    const captureKey = this.#capturing ? 'saving' : saved ? 'saved' : 'idle';
    if (ui.capture.dataset.key !== captureKey) {
      ui.capture.dataset.key = captureKey;
      const glyph = this.#capturing ? spinner(14) : saved ? icon('check-circle', 14) : icon('camera', 14);
      const text = this.#capturing ? 'Saving…' : saved ? 'Saved' : 'Capture';
      ui.capture.replaceChildren(glyph, el('span', 'word', text));
      ui.capture.classList.toggle('saved', saved);
      ui.capture.setAttribute('aria-label', text);
    }
    ui.capture.hidden = !source;
    ui.capture.disabled = !canCapture && !this.#capturing;
    ui.capture.setAttribute('aria-disabled', String(this.#capturing));
    let captureAbout = withChord('Capture from this camera', 'Mod+S', this.#isApple);
    if (this.#capturing) captureAbout = 'Saving the capture…';
    else if (!source) captureAbout = 'Select a camera to capture from';
    else if (!this.#engine) captureAbout = this.#engineFailed ? 'The image engine did not load' : 'The image engine is loading…';
    else if (!this.#sidebar) captureAbout = 'Capture settings did not load';
    else if (!source.hasFrame) captureAbout = 'Waiting for the first frame';
    setAttr(ui.capture, 'title', captureAbout);

    // Record.
    const recording = this.#recording.active;
    ui.record.hidden = !source;
    const recordKey = recording ? 'stop' : 'record';
    if (ui.record.dataset.key !== recordKey) {
      ui.record.dataset.key = recordKey;
      ui.record.replaceChildren(icon(recording ? 'stop-fill' : 'video-camera', 14), el('span', 'word', recording ? 'Stop' : 'Record'));
      ui.record.setAttribute('aria-label', recording ? 'Stop recording' : 'Record');
    }
    ui.record.disabled = !this.#sidebar?.toggleRecording || (!recording && !source?.running);
    setAttr(ui.record, 'title', recording ? 'Stop and finish the recording'
      : !source?.running ? 'Start the stream to record' : 'Record video to a file in this browser');
    ui.clock.hidden = !recording;
    if (recording) setText(ui.clock, clock(now - this.#recording.startedAt));

    // Destination.
    const destination = this.#sidebar?.destinationText?.() || this.#destination;
    setText(ui.destinationText, destination);
    setAttr(ui.destination, 'aria-label', `Capture destination: ${destination}`);
    ui.destination.hidden = !source;

    // Centre status: the notice, else what is loading, else the camera's link.
    const status = ui.toolStatus;
    const notice = this.#notice;
    let key;
    if (notice) key = `notice|${notice.level}|${notice.message}|${this.#activityOpen}`;
    else if (this.#engineLoading && !this.#engine) key = 'loading';
    else if (source?.info) key = `link|${source.info.transport} · S/N ${source.info.serial}`;
    else key = 'none';
    if (status.dataset.key !== key) {
      status.dataset.key = key;
      status.className = 'status';
      if (notice) {
        status.classList.add('notice', notice.level);
        const glyph = notice.level === 'error' ? icon('warning-circle', 13) : notice.level === 'warn' ? icon('warning', 13) : null;
        if (glyph) glyph.classList.add('mark', notice.level === 'error' ? 'error' : 'warn');
        const text = el('span', 'text', notice.message);
        text.title = notice.message;
        const parts = [glyph, text];
        if (notice.level !== 'info' && !this.#activityOpen) {
          const details = button('link', 'Show activity details', 'Details');
          details.addEventListener('click', () => this.#setActivity(true));
          parts.push(details);
        }
        if (notice.level === 'error') {
          const dismiss = button('btn icon', 'Dismiss notification', icon('x', 11));
          dismiss.title = 'Dismiss';
          dismiss.style.width = dismiss.style.height = dismiss.style.minWidth = '22px';
          dismiss.addEventListener('click', () => {
            this.#notice = null;
            this.#update();
          });
          parts.push(dismiss);
        }
        status.replaceChildren(...parts.filter(Boolean));
      } else if (key === 'loading') {
        status.replaceChildren(spinner(12), el('span', 'text', 'Loading the image engine…'));
      } else if (key.startsWith('link|')) {
        status.replaceChildren(el('span', 'text', key.slice(5)));
      } else {
        status.replaceChildren();
      }
    }

    // Activity.
    ui.alert.hidden = !this.#unseen || this.#activityOpen;
    ui.alert.className = `dot alert ${this.#unseen === 'error' ? 'danger' : 'warn'}`;
    setAttr(ui.activityToggle, 'title', this.#unseen === 'error' ? 'Session activity: new errors'
      : this.#unseen === 'warn' ? 'Session activity: new warnings' : 'Session activity');
  }

  #renderWelcome() {
    const ui = this.#ui;
    const welcome = this.#welcome;
    const shown = this.#welcomeShown();
    ui.welcome.hidden = !shown;
    if (!shown) return;
    const available = webcamAvailable();
    const cameras = available ? welcome.cameras : [];
    const items = [];
    // Anonymous entries before access is allowed become one "Webcam" card per entry.
    let unnamed = 0;
    for (const camera of cameras.length ? cameras : (available && welcome.state !== 'blocked' && welcome.state !== 'none' ? [{ deviceId: '', label: '' }] : [])) {
      const label = camera.label || (unnamed++ ? `Webcam ${unnamed}` : 'Webcam');
      const key = `webcam:${camera.deviceId}`;
      const current = this.#source?.kind === 'webcam' && this.#source.deviceId === camera.deviceId && camera.deviceId;
      items.push({
        key,
        icon: 'webcam',
        title: label,
        detail: current ? 'Connected · Webcam · this computer' : 'Webcam · this computer',
        act: () => this.#openWebcam(camera.deviceId)
      });
    }
    items.push({
      key: 'sim:0',
      icon: 'cube',
      title: patternInfo().model,
      detail: this.#source?.kind === 'sim' ? `Connected · ${patternInfo().detail}` : patternInfo().detail,
      act: () => this.#openPattern()
    });

    let subtitle = 'Choose a camera to see a live image.';
    let callout = null;
    if (welcome.state === 'requesting') subtitle = 'Allow camera access in the browser prompt.';
    else if (welcome.state === 'none') subtitle = 'No cameras found yet.';
    else if (welcome.state === 'unsupported') subtitle = 'This browser offers no camera access on this page. Try the Pattern camera.';
    if (welcome.state === 'blocked') callout = { level: 'warn', text: welcome.message };
    else if (welcome.state === 'none' && welcome.message && welcome.message !== subtitle) callout = { level: 'warn', text: welcome.message };

    const title = el('h2', '', 'Your capture workbench');
    title.id = 'cf-welcome-title';
    title.tabIndex = -1;
    const subtitleNode = el('p', 'subtitle', subtitle);
    const children = [brand(44), title, subtitleNode];
    if (callout) {
      const node = el('div', `callout ${callout.level}`, icon(callout.level === 'danger' ? 'warning-circle' : 'warning', 14), el('span', '', callout.text));
      node.setAttribute('role', 'note');
      children.push(node);
    }
    const list = el('ul', 'cards');
    list.setAttribute('aria-label', 'Cameras');
    // The default camera, asked for without an id, is the first one listed.
    const firstWebcam = items.find((item) => item.key.startsWith('webcam:'));
    for (const item of items) {
      const fallback = item === firstWebcam ? 'webcam:' : item.key;
      const connecting = welcome.connecting === item.key || (welcome.connecting === 'webcam:' && item === firstWebcam);
      const failure = welcome.failures.get(item.key) ?? welcome.failures.get(fallback);
      const card = button(`card${failure ? ' failed' : ''}`, '');
      const glyph = el('span', 'glyph', connecting ? spinner(16) : failure ? icon('warning-circle', 20) : icon(item.icon, 20));
      const detail = connecting ? 'Connecting…' : failure ?? item.detail;
      card.append(glyph, el('span', 'text', el('span', 'title', item.title), el('span', 'detail', detail)), icon('caret-right', 13));
      card.setAttribute('aria-label', `${item.title}, ${detail}`);
      if (failure) card.title = failure;
      card.disabled = Boolean(welcome.connecting);
      card.addEventListener('click', item.act);
      list.append(el('li', '', card));
    }
    children.push(list);

    const footer = el('div', 'footer');
    const noWebcam = !available || welcome.state === 'blocked' || welcome.state === 'none';
    if (noWebcam && !this.#source) {
      // With no webcam to choose, the simulator is the quickest way in.
      const sim = button('btn primary', '', welcome.connecting === 'sim:0' ? spinner(14) : icon('cube', 15), 'Try a simulated camera');
      sim.disabled = Boolean(welcome.connecting);
      sim.addEventListener('click', () => this.#openPattern());
      footer.append(sim);
    }
    if (available) {
      const search = button('btn', '', welcome.searching ? spinner(14) : icon('refresh', 15), welcome.searching ? 'Searching…' : 'Search again');
      search.disabled = welcome.searching;
      search.title = 'Look for cameras again';
      search.addEventListener('click', () => this.#searchAgain());
      footer.append(search);
    }
    if (this.#source && this.#choosing) {
      const back = button('btn', '', `Back to ${this.#source.info.model}`);
      back.addEventListener('click', () => this.#choose(false));
      footer.append(back);
    }
    children.push(footer);
    children.push(el('p', 'note', 'Capturefab runs in this tab. Nothing is uploaded. For GigE Vision and USB3 Vision cameras, download the desktop app.'));
    const focused = ui.welcome.contains(this.#root.activeElement) ? this.#root.activeElement : null;
    const focusKey = focused?.closest?.('li') ? [...ui.welcome.querySelectorAll('li')].indexOf(focused.closest('li')) : -1;
    const focusedTitle = focused?.id === 'cf-welcome-title';
    ui.welcome.replaceChildren(el('div', 'inner', ...children));
    // Keep keyboard focus where it was across a re-render.
    if (focusedTitle) title.focus({ preventScroll: true });
    else if (focusKey >= 0) list.querySelectorAll('button')[focusKey]?.focus({ preventScroll: true });
    else if (focused) (footer.querySelector('button:not([disabled])') ?? title).focus({ preventScroll: true });
  }
}
