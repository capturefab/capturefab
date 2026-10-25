// The Features pane, as the desktop's inspector lays it out (src/gui/inspector.rs):
// search, the Exposure well, quick controls with sliders, feature groups, Camera
// details, and Shell & agents for the same JSON the CLI prints.
import { icon, strokeIcon } from './icons.mjs';
import { json, withChord } from './format.mjs';
import {
  FeatureScale, GROUPS, capitalize, caption, draftError, draftNumber, featureJson, featureRank, firstLine, fixed,
  grouped, groupOf, label, matches, msReadout, plural, quickFeatures, streamLocked, unitLabel, valueText
} from './features.mjs';

/** An element with attributes, listeners (`on…`) and children; text never goes through HTML. */
export const h = (tag, props = {}, ...children) => {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === 'class') el.className = value;
    else if (key === 'text') el.textContent = value;
    else if (key.startsWith('on') && typeof value === 'function') el.addEventListener(key.slice(2), value);
    else if (key === 'value' || key === 'checked') el[key] = value;
    else el.setAttribute(key, value === true ? '' : String(value));
  }
  el.append(...children.flat(Infinity).filter((child) => child !== null && child !== undefined && child !== false));
  return el;
};

const KIND = { webcam: 'Webcam · this computer', sim: 'Area scan' };
const GUIDANCE = {
  sim: 'Set the image size, exposure and frame rate for each full image.',
  webcam: 'Use the controls this camera reports to the browser. What it reports depends on the camera, its driver and the browser.'
};
// How frames reach the engine (engine.mjs `analyze` paths).
const PATHS = {
  rgba: 'RGBA copy',
  nv12: 'NV12, converted',
  i420: 'I420, converted',
  canvas: 'Canvas read-back',
  sim: 'Rendered here'
};
const ACCEPTED_MS = 450;
const COPIED_MS = 1500;

const errorText = (error) => (error instanceof Error ? error.message : String(error ?? 'unknown error'));

/** Words joined as "a, b and c". */
const series = (items) => (items.length < 2 ? items.join('') : `${items.slice(0, -1).join(', ')} and ${items.at(-1)}`);

class Inspector {
  #app;
  #root;
  #pane;
  #searchInput;
  #clear;
  #count;
  #well;
  #list;
  #details;
  #detailsBody;
  #shell;
  #shellBody;
  #live;
  #features = [];
  #edits = new Map();
  #refusals = new Map();
  #pending = new Map();
  #queued = new Map();
  #accepted = new Map();
  #open = new Set();
  #query = '';
  #loading = false;
  #reload = false;
  #loadError = null;
  #sourceKey = null;
  #awaitingFrame = false;
  #copied = new Map();
  #timers = new Set();
  #mirror = null;
  #scopeState = '';
  #detailsKey = '';

  constructor(container, app) {
    this.#app = app;
    this.#searchInput = h('input', {
      class: 'field fx-search-input', type: 'search', placeholder: 'Search features', 'aria-label': 'Search features',
      autocomplete: 'off', spellcheck: 'false', enterkeyhint: 'search',
      oninput: () => this.#search(this.#searchInput.value),
      onkeydown: (event) => {
        if (event.key === 'Escape' && this.#searchInput.value !== '') {
          event.preventDefault();
          event.stopPropagation();
          this.#searchInput.value = '';
          this.#search('');
        }
      }
    });
    this.#clear = h('button', { class: 'btn icon fx-search-clear', type: 'button', 'aria-label': 'Clear search', title: 'Clear search', hidden: true, onclick: () => {
      this.#searchInput.value = '';
      this.#search('');
      this.#searchInput.focus();
    } }, icon('x', 11));
    this.#count = h('div', { class: 'fx-count' });
    this.#well = h('section', { class: 'well fx-well', 'aria-labelledby': 'fx-well-title' });
    this.#list = h('div', { class: 'fx-list' });
    this.#detailsBody = h('div', { class: 'fx-disclosure-body' });
    this.#details = h('details', { class: 'disclosure fx-details', ontoggle: () => this.#renderDetails() },
      h('summary', {}, icon('caret-right', 11), h('span', { class: 'title', text: 'Camera details' }), h('span', { class: 'summary' })),
      this.#detailsBody);
    this.#shellBody = h('div', { class: 'fx-disclosure-body' });
    this.#shell = h('details', { class: 'disclosure fx-shell', ontoggle: () => this.#renderShell() },
      h('summary', {}, icon('caret-right', 11), h('span', { class: 'title', text: 'Shell & agents' }), h('span', { class: 'summary', text: 'CLI access' })),
      this.#shellBody);
    this.#live = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
    this.#pane = h('div', { class: 'pane fx-pane' },
      h('div', { class: 'fx-top' },
        h('div', { class: 'fx-search', role: 'search' }, icon('search', 13), this.#searchInput, this.#clear),
        this.#count),
      this.#well,
      this.#list,
      this.#details,
      this.#shell,
      this.#live);
    this.#root = h('div', { class: 'fx-root' },
      h('header', { class: 'fx-head' }, h('h2', { class: 'pane-title', text: 'Features' })),
      this.#pane);
    container.replaceChildren(this.#root);

    app.on('source', () => this.#sourceChanged());
    app.on('features-changed', () => this.#load());
    app.on('phase', () => this.render());
    app.on('frame', () => this.#frame());
    app.on('engine', () => {
      // The Pattern camera's features come from the engine.
      if (this.#source && this.#features.length === 0) this.#load();
      this.#renderShell();
    });
    app.on('mirror', (detail) => {
      const value = [detail, detail?.detail, detail?.mirror].find((v) => typeof v === 'boolean');
      if (value !== undefined) this.#mirror = value;
      this.#detailsKey = '';
      this.#renderDetails();
    });
    app.on('scope', () => this.#renderLinks());
    this.#sourceChanged();
  }

  focusSearch() {
    this.#searchInput.focus();
    this.#searchInput.select();
  }

  refresh() {
    return this.#load();
  }

  // -------------------------------------------------------------------------

  get #state() {
    return this.#app.state ?? {};
  }

  get #source() {
    return this.#state.source ?? null;
  }

  #byName(name) {
    return this.#features.find((feature) => feature.name === name);
  }

  #later(ms, fn) {
    const timer = setTimeout(() => {
      this.#timers.delete(timer);
      fn();
    }, ms);
    this.#timers.add(timer);
  }

  #announce(text) {
    this.#live.textContent = '';
    // A new node each time, so the same words are read again.
    this.#later(30, () => { this.#live.textContent = text; });
  }

  #sourceChanged() {
    const source = this.#source;
    const key = source ? `${source.kind}:${source.id}` : null;
    if (key !== this.#sourceKey) {
      this.#sourceKey = key;
      this.#edits.clear();
      this.#refusals.clear();
      this.#pending.clear();
      this.#queued.clear();
      this.#features = [];
      this.#loadError = null;
      this.#mirror = null;
      this.#awaitingFrame = Boolean(source);
    }
    this.render();
    this.#load();
  }

  #frame() {
    // A webcam may report its controls only once it delivers frames.
    if (this.#awaitingFrame && this.#state.frame) {
      this.#awaitingFrame = false;
      this.#load();
    }
    if (this.#details.open) this.#renderDetails();
    this.#renderLinks();
  }

  async #load() {
    if (this.#loading) {
      this.#reload = true;
      return;
    }
    const source = this.#source;
    if (!source) {
      this.#features = [];
      this.render();
      return;
    }
    this.#loading = true;
    this.#renderCount();
    try {
      const features = await this.#app.features();
      if (source === this.#source) {
        this.#features = Array.isArray(features) ? features : [];
        this.#loadError = null;
      }
    } catch (error) {
      if (source === this.#source) this.#loadError = errorText(error);
    } finally {
      this.#loading = false;
      this.render();
      if (this.#reload) {
        this.#reload = false;
        this.#load();
      }
    }
  }

  #search(query) {
    this.#query = query;
    this.#clear.hidden = query === '';
    this.render();
  }

  // Writes ---------------------------------------------------------------

  #locked(feature) {
    if (!this.#state.running) return false;
    return Boolean(feature.locked_while_streaming ?? (feature.writable && streamLocked(feature.name)));
  }

  #managed(feature) {
    return feature.managed_by_auto === true;
  }

  #editable(feature) {
    return feature.writable && !this.#locked(feature) && !this.#managed(feature) && !feature.error && feature.kind !== 'Command';
  }

  #draft(name, text) {
    this.#refusals.delete(name);
    const feature = this.#byName(name);
    if (feature && text === valueText(feature.value)) this.#edits.delete(name);
    else this.#edits.set(name, text);
    this.#updateRow(name);
  }

  /**
   * Set `name` to `text`. `quiet` (a slider, whose aria-valuetext already speaks the value)
   * announces only refusals and values the camera changed from the one asked for.
   */
  async #apply(name, text, { quiet = false } = {}) {
    const feature = this.#byName(name);
    if (!feature) return;
    const problem = draftError(feature, text);
    if (problem !== null) {
      this.#edits.set(name, text);
      this.#updateRow(name);
      this.#announce(`${label(feature)}: ${problem}`);
      return;
    }
    if (this.#pending.has(name)) {
      this.#queued.set(name, { text, quiet });
      return;
    }
    this.#pending.set(name, text);
    this.#refusals.delete(name);
    this.#updateRow(name);
    this.#renderWell();
    try {
      const result = await this.#app.setFeature(name, text);
      if (this.#edits.get(name) === text) this.#edits.delete(name);
      this.#accepted.set(name, performance.now());
      this.#later(ACCEPTED_MS + 20, () => this.#updateRow(name));
      const shown = valueText(result?.value);
      const wanted = feature.kind === 'Boolean' ? ['true', '1', 'on'].includes(text.trim().toLowerCase()) : text;
      if ((feature.kind === 'Enumeration' || feature.kind === 'Boolean') && result && result.value !== wanted) {
        // The camera answered without taking it; say so rather than look stuck.
        this.#app.log('warn', `${name} is still ${shown}; the camera did not change it`, { name, asked: text, value: result.value ?? null });
        this.#announce(`${label(feature)} is still ${shown}`);
      } else {
        this.#app.log('info', `Set ${name} to ${shown}`, { name, value: result?.value ?? null });
        if (!quiet || this.#differs(result?.value, text)) this.#announce(`${label(feature)} set to ${shown}${feature.unit ? ` ${unitLabel(feature.unit)}` : ''}`);
      }
    } catch (error) {
      const message = errorText(error);
      this.#refusals.set(name, message);
      if (feature.kind !== 'Enumeration' && feature.kind !== 'Boolean') this.#edits.set(name, text);
      this.#app.log('error', message);
      this.#announce(`${label(feature)}: ${capitalize(firstLine(message))}`);
    } finally {
      this.#pending.delete(name);
      const next = this.#queued.get(name);
      this.#queued.delete(name);
      this.#updateRow(name);
      this.#renderWell();
      if (next !== undefined && next.text !== text) this.#apply(name, next.text, { quiet: next.quiet });
    }
  }

  /** Whether the camera's `value` is not the `text` asked for (clamped or rounded). */
  #differs(value, text) {
    const asked = draftNumber(text);
    if (typeof value !== 'number' || asked === null) return valueText(value) !== text;
    return Math.abs(value - asked) > 1e-9 * Math.max(1, Math.abs(value));
  }

  // Rendering ------------------------------------------------------------

  /** Run `rebuild`, then put focus back on the control with the same key inside `within`. */
  #keepFocus(within, rebuild) {
    const active = this.#root.getRootNode().activeElement;
    const key = active && within.contains(active) ? active.dataset?.focusKey : undefined;
    const start = key ? active.selectionStart ?? null : null;
    const end = key ? active.selectionEnd ?? null : null;
    rebuild();
    if (!key) return;
    const again = within.querySelector(`[data-focus-key="${CSS.escape(key)}"]`);
    if (!again || again.disabled) return;
    again.focus({ preventScroll: true });
    if (start !== null && typeof again.setSelectionRange === 'function') {
      try {
        again.setSelectionRange(start, end);
      } catch {
        // Not a text control.
      }
    }
  }

  /** Rebuild the pane from the features, keeping focus, selection and scroll. */
  render() {
    const scroll = this.#pane.scrollTop;
    this.#keepFocus(this.#pane, () => {
      this.#renderCount();
      this.#renderWell();
      this.#renderList();
      this.#detailsKey = '';
      this.#renderDetails();
      this.#renderShell();
    });
    this.#pane.scrollTop = scroll;
  }

  /** The features the groups list: all but AcquisitionStop when one Stream row starts and stops. */
  #listed() {
    const folded = this.#features.some((f) => f.name === 'AcquisitionStart');
    return this.#features.filter((f) => !(folded && f.name === 'AcquisitionStop'));
  }

  #groups(features) {
    return GROUPS.map((group) => [group, features
      .map((feature, index) => ({ feature, index }))
      .filter(({ feature }) => groupOf(feature.name) === group)
      .sort((a, b) => featureRank(a.feature.name) - featureRank(b.feature.name) || a.index - b.index)
      .map(({ feature }) => feature)]).filter(([, members]) => members.length > 0);
  }

  #renderCount() {
    const query = this.#query.trim();
    const listed = this.#listed();
    const groups = this.#groups(listed);
    let text;
    if (!this.#source) text = 'No camera';
    else if (query === '') text = plural(listed.length, 'setting', 'settings');
    else text = `${grouped(listed.filter((f) => matches(f, query)).length)} of ${grouped(listed.length)} features`;
    const parts = [h('span', { class: 'fx-count-text', text })];
    if (query === '' && groups.length > 0) {
      const anyOpen = groups.some(([group]) => this.#open.has(group));
      parts.push(h('button', { class: 'link', type: 'button', text: anyOpen ? 'Collapse all' : 'Expand all', 'data-focus-key': 'expand', onclick: () => {
        for (const [group] of groups) {
          if (anyOpen) this.#open.delete(group);
          else this.#open.add(group);
        }
        this.render();
      } }));
    }
    parts.push(h('span', { class: 'fx-spacer' }));
    if (this.#loading) parts.push(h('span', { class: 'fx-refreshing', text: 'Refreshing…' }));
    else if (this.#source) parts.push(h('button', { class: 'link', type: 'button', text: 'Refresh', title: 'Read the camera’s features again', 'data-focus-key': 'refresh', onclick: () => this.#load() }));
    this.#keepFocus(this.#count, () => this.#count.replaceChildren(...parts));
  }

  #renderWell() {
    const show = this.#query.trim() === '' && this.#source !== null
      && this.#features.some((f) => ['ExposureAuto', 'ExposureTime', 'ExposureTimeAbs', 'Gain', 'AcquisitionFrameRate'].includes(f.name));
    this.#well.hidden = !show;
    if (!show) {
      this.#well.replaceChildren();
      return;
    }
    const auto = this.#byName('ExposureAuto');
    const switching = this.#pending.has('ExposureAuto');
    const head = h('div', { class: 'fx-well-head' }, h('h3', { id: 'fx-well-title', text: 'Exposure' }));
    if (switching) head.append(h('span', { class: 'fx-well-state' }, h('span', { class: 'spinner', style: '--size: 10px' }), 'Switching…'));
    const parts = [head];
    const on = auto?.value === 'Continuous';
    const switchable = auto && auto.writable && auto.choices.includes('Off') && auto.choices.includes('Continuous');
    if (switchable) {
      const isApple = this.#state.isApple;
      const mode = (word, value, hint) => h('button', {
        type: 'button', text: word, 'aria-pressed': String(on === (value === 'Continuous')), 'data-focus-key': `mode:${word}`,
        title: withChord(hint, 'Mod+Shift+A', isApple), disabled: switching,
        onclick: () => {
          if ((value === 'Continuous') !== on) this.#apply('ExposureAuto', value);
        }
      });
      parts.push(h('div', { class: 'segmented fx-modes', role: 'group', 'aria-label': 'Exposure mode' },
        mode('Manual', 'Off', 'Set exposure yourself'),
        mode('Auto', 'Continuous', 'Let the camera set exposure')));
    }
    const has = (name) => this.#features.some((f) => f.name === name);
    const items = [has('ExposureTime') && 'exposure', has('Gain') && 'gain', has('AcquisitionFrameRate') && 'frame rate'].filter(Boolean);
    if (on) {
      const exposure = this.#byName('ExposureTime')?.value;
      const rate = this.#byName('AcquisitionFrameRate')?.value;
      const summary = ['Camera auto exposure'];
      if (typeof exposure === 'number') summary.push(exposure < 1000 ? `${fixed(exposure, 0)} µs` : `${fixed(exposure / 1000, 1)} ms`);
      if (typeof rate === 'number') summary.push(`${fixed(rate, 0)} fps`);
      parts.push(h('p', { class: 'fx-note', text: 'The camera sets its exposure. Choose Manual to set it yourself.' }));
      parts.push(h('p', { class: 'fx-note', text: summary.join(' · ') }));
    } else if (items.includes('exposure') || items.includes('gain')) {
      let text;
      if (items.length === 3 && switchable) text = 'Set exposure, gain and frame rate below, or choose Auto to have them tuned for you.';
      else text = `${capitalize(`set ${series(items)} below`)}${switchable ? ', or choose Auto to have the camera set exposure.' : '.'}`;
      parts.push(h('p', { class: 'fx-note', text }));
    } else {
      parts.push(h('p', { class: 'fx-note', text: 'This camera sets its own exposure. Judge it with the histogram.' }));
    }
    parts.push(h('div', { class: 'fx-links' }));
    this.#keepFocus(this.#well, () => {
      this.#well.replaceChildren(...parts);
      this.#scopeState = '';
      this.#renderLinks();
    });
  }

  #renderLinks() {
    const links = this.#well.querySelector('.fx-links');
    if (!links) return;
    const visible = (name) => {
      try {
        return Boolean(this.#app.scopeVisible?.(name));
      } catch {
        return false;
      }
    };
    const histogram = visible('histogram');
    const focus = visible('focus');
    const state = `${histogram}${focus}${this.#state.isApple}`;
    if (state === this.#scopeState) return;
    this.#scopeState = state;
    const isApple = this.#state.isApple;
    const link = (open, shown, hidden, what, chord, name) => h('button', {
      class: 'link', type: 'button', text: open ? shown : hidden, 'data-focus-key': `scope:${name}`,
      title: withChord(`${open ? 'Hide' : 'Show'} the ${what}`, chord, isApple),
      onclick: () => {
        this.#app.showScope?.(name);
        this.#scopeState = '';
        this.#renderLinks();
      }
    });
    this.#keepFocus(links, () => links.replaceChildren(
      link(histogram, 'Hide histogram', 'Histogram', 'histogram', 'H', 'histogram'),
      link(focus, 'Hide focus region', 'Focus region', 'focus region', 'R', 'focus')));
  }

  #renderList() {
    const source = this.#source;
    if (!source) {
      this.#list.replaceChildren(h('p', { class: 'fx-empty', text: 'Choose a camera to see its features.' }));
      return;
    }
    if (this.#loadError !== null && this.#features.length === 0) {
      this.#list.replaceChildren(h('p', { class: 'fx-empty fx-failed' }, icon('warning-circle', 12), ` Could not read features: ${this.#loadError}`));
      return;
    }
    if (this.#features.length === 0) {
      this.#list.replaceChildren(h('p', { class: 'fx-empty', text: this.#loading ? 'Reading features…' : 'This camera reports no features to the browser.' }));
      return;
    }
    const query = this.#query.trim();
    const searching = query !== '';
    const listed = this.#listed();
    const blocks = [];
    if (!searching) {
      const quick = quickFeatures(this.#features);
      if (quick.length > 0) {
        blocks.push(h('section', { class: 'fx-group fx-quick', 'aria-labelledby': 'fx-quick-title' },
          h('h3', { class: 'section-title', id: 'fx-quick-title', text: 'Quick controls' }),
          this.#rows(quick, true)));
      }
    }
    const quickNames = new Set(quickFeatures(this.#features).map((f) => f.name));
    let matched = 0;
    for (const [group, members] of this.#groups(listed)) {
      const shown = members.filter((f) => (searching ? matches(f, query) : !quickNames.has(f.name)));
      matched += shown.length;
      if (shown.length === 0) continue;
      if (searching) {
        const id = `fx-group-${group}`;
        blocks.push(h('section', { class: 'fx-group', 'aria-labelledby': id },
          h('h3', { class: 'section-title', id, text: group }),
          this.#rows(shown, false)));
        continue;
      }
      const failed = shown.some((f) => this.#refusals.has(f.name));
      const edited = shown.filter((f) => this.#edits.has(f.name)).length;
      let summary = plural(shown.length, 'feature', 'features');
      if (edited > 0) summary += ` · ${edited} edited`;
      const details = h('details', { class: 'disclosure fx-group', open: this.#open.has(group) || failed },
        h('summary', {}, icon('caret-right', 11), h('span', { class: 'title', text: group }), h('span', { class: 'summary', text: summary })));
      details.addEventListener('toggle', () => {
        if (details.open) {
          this.#open.add(group);
          if (!details.querySelector('.fx-rows')) details.append(this.#rows(shown, false));
        } else {
          this.#open.delete(group);
        }
        this.#renderCount();
      });
      if (details.open) details.append(this.#rows(shown, false));
      blocks.push(details);
    }
    if (searching && matched === 0) {
      blocks.push(h('div', { class: 'fx-nomatch' },
        h('p', { text: `No features match “${query}”` }),
        h('button', { class: 'link', type: 'button', text: 'Clear search', onclick: () => {
          this.#searchInput.value = '';
          this.#search('');
          this.#searchInput.focus();
        } })));
    }
    this.#list.replaceChildren(...blocks);
  }

  #rows(features, quick) {
    const rows = h('div', { class: 'fx-rows' });
    if (features.some((f) => this.#locked(f))) {
      const isApple = this.#state.isApple;
      rows.append(h('p', { class: 'fx-locked' }, strokeIcon('lock', 11), 'Locked while streaming',
        h('button', { class: 'link', type: 'button', text: 'Stop', title: withChord('Stop the stream', 'Space', isApple), disabled: this.#state.phase === 'stopping', onclick: () => this.#app.stop() })));
    }
    for (const feature of features) rows.append(this.#row(feature, quick));
    return rows;
  }

  #row(feature, quick) {
    const name = feature.name;
    const text = label(feature);
    const id = `fx-${name}`;
    const about = feature.description ? `${name}\n${feature.description}` : name;
    const locked = this.#locked(feature);
    const managed = this.#managed(feature) && !locked;
    const command = feature.kind === 'Command';
    const readOnly = !feature.writable && !command && !locked;
    const row = h('div', { class: 'fx-row' });
    row.dataset.name = name;
    const labelEl = h(command || readOnly || locked || feature.error ? 'span' : 'label', {
      class: 'fx-label', text: name === 'AcquisitionStart' ? 'Stream' : text, title: readOnly ? `${about}\nRead only` : about, for: command || readOnly || locked ? null : id
    });
    let control;
    let field = null;
    let slider = null;
    if (name === 'AcquisitionStart' && command) {
      const running = this.#state.running;
      const phase = this.#state.phase;
      const busy = phase === 'starting' || phase === 'stopping' || phase === 'requesting';
      const word = phase === 'starting' || phase === 'requesting' ? 'Starting…' : phase === 'stopping' ? 'Stopping…' : running ? 'Stop' : 'Start';
      control = h('button', {
        class: 'btn fx-stream', type: 'button', disabled: busy, 'data-focus-key': `${name}:stream`, 'data-running': running,
        title: withChord(running ? 'Stop the stream' : 'Start the stream', 'Space', this.#state.isApple),
        onclick: () => (running ? this.#app.stop() : this.#app.start())
      }, busy ? h('span', { class: 'spinner', style: '--size: 11px' }) : icon(running ? 'stop-fill' : 'play-fill', 11), word);
    } else if (command) {
      control = h('button', { class: 'btn fx-command', type: 'button', text: 'Execute', disabled: true, title: 'Commands run from the Capturefab app' });
    } else if (feature.error) {
      control = h('span', { class: 'fx-readonly' });
    } else if (!feature.writable || locked) {
      const shown = feature.unit && feature.value !== null && feature.value !== undefined
        ? `${valueText(feature.value)} ${unitLabel(feature.unit)}` : valueText(feature.value);
      control = h('span', { class: 'fx-readonly' },
        locked ? h('span', { class: 'fx-lock', title: `Stop the stream to change ${text}` }, strokeIcon('lock', 11)) : null,
        h('span', { text: shown }));
    } else if (feature.kind === 'Boolean') {
      const pending = this.#pending.get(name);
      const checked = pending !== undefined ? ['true', '1', 'on'].includes(pending) : feature.value === true;
      field = h('input', {
        class: 'fx-check', type: 'checkbox', id, checked, disabled: managed, 'data-focus-key': `${name}:check`,
        onchange: () => this.#apply(name, field.checked ? 'true' : 'false')
      });
      control = field;
    } else if (feature.choices.length > 0) {
      const pending = this.#pending.get(name);
      const current = pending ?? valueText(feature.value);
      field = h('select', {
        class: 'field fx-select', id, disabled: managed, 'data-focus-key': `${name}:select`,
        onchange: () => this.#apply(name, field.value)
      }, feature.choices.includes(current) ? null : h('option', { value: current, text: current, disabled: true }),
      feature.choices.map((choice) => h('option', { value: choice, text: choice })));
      field.value = current;
      control = field;
    } else {
      const current = valueText(feature.value);
      field = h('input', {
        class: 'field fx-input', type: 'text', id, value: this.#edits.get(name) ?? current, placeholder: current,
        inputmode: feature.min !== null && feature.min >= 0 ? 'decimal' : null, autocomplete: 'off', spellcheck: 'false',
        enterkeyhint: 'done', disabled: managed, 'data-focus-key': `${name}:field`,
        'aria-describedby': `${id}-caption`,
        oninput: () => this.#draft(name, field.value),
        onkeydown: (event) => {
          if (event.key === 'Enter') {
            event.preventDefault();
            this.#apply(name, field.value);
          } else if (event.key === 'Escape' && this.#edits.has(name)) {
            event.preventDefault();
            event.stopPropagation();
            this.#edits.delete(name);
            this.#refusals.delete(name);
            field.value = valueText(this.#byName(name)?.value);
            this.#updateRow(name);
          }
        },
        onchange: () => {
          // Leaving a changed field applies it, when it is valid.
          if (this.#edits.has(name) && draftError(feature, field.value) === null) this.#apply(name, field.value);
        }
      });
      const micro = msReadout(feature, feature.value ?? feature.max) !== null;
      control = micro ? h('span', { class: 'fx-unit-field' }, field, h('span', { class: 'fx-unit', text: 'µs', 'aria-hidden': 'true' })) : field;
      const scale = quick ? FeatureScale.of(feature) : null;
      if (scale) {
        slider = h('input', {
          class: 'fx-slider', type: 'range', min: '0', max: '1000', step: '1', 'aria-label': text,
          disabled: managed, 'data-focus-key': `${name}:slider`,
          oninput: () => {
            const value = scale.value(Number(slider.value) / 1000);
            const draft = scale.text(value);
            field.value = draft;
            this.#draft(name, draft);
          },
          onchange: () => this.#apply(name, field.value, { quiet: true }),
          onkeydown: (event) => {
            const current = draftNumber(field.value) ?? (typeof feature.value === 'number' ? feature.value : scale.min);
            let next = null;
            if (event.key === 'ArrowUp' || event.key === 'ArrowRight') next = scale.step(current, true);
            else if (event.key === 'ArrowDown' || event.key === 'ArrowLeft') next = scale.step(current, false);
            else if (event.key === 'PageUp') next = scale.value(scale.position(current) + 0.1);
            else if (event.key === 'PageDown') next = scale.value(scale.position(current) - 0.1);
            else if (event.key === 'Home') next = scale.min;
            else if (event.key === 'End') next = scale.max;
            if (next === null) return;
            event.preventDefault();
            const draft = scale.text(next);
            field.value = draft;
            this.#draft(name, draft);
            this.#apply(name, draft, { quiet: true });
          }
        });
        slider.scale = scale;
      }
    }
    const head = h('div', { class: 'fx-head-row' }, labelEl, h('div', { class: 'fx-control' }, control));
    row.append(head);
    if (slider) row.append(slider);
    row.append(h('div', { class: 'fx-caption', id: `${id}-caption` }));
    row.feature = feature;
    row.field = field;
    row.slider = slider;
    row.managed = managed;
    this.#fillRow(row);
    return row;
  }

  #updateRow(name) {
    const row = [...this.#list.querySelectorAll('.fx-row')].find((el) => el.dataset.name === name);
    if (row) this.#fillRow(row);
    const group = row?.closest('details.fx-group');
    if (group) {
      // The group's "· N edited" summary follows its drafts.
      const names = [...group.querySelectorAll('.fx-row')].map((el) => el.dataset.name);
      const edited = names.filter((n) => this.#edits.has(n)).length;
      const summary = group.querySelector(':scope > summary .summary');
      if (summary) summary.textContent = `${plural(names.length, 'feature', 'features')}${edited > 0 ? ` · ${edited} edited` : ''}`;
    }
  }

  /** A row's field state and caption: range, "Set by auto", the draft's error or the camera's refusal, Apply. */
  #fillRow(row) {
    const { feature, field, slider } = row;
    const name = feature.name;
    const captionEl = row.querySelector('.fx-caption');
    const parts = [];
    let trailing = null;
    const writable = this.#editable(feature) || (row.managed && feature.writable);
    const draft = this.#edits.get(name);
    const current = valueText(feature.value);
    const pending = this.#pending.has(name);
    let error = null;
    if (feature.error) {
      parts.push(h('span', { class: 'fx-warn', text: feature.error }));
    } else if (field && field.classList.contains('fx-input')) {
      if (draft !== undefined && field.value !== draft && this.#root.getRootNode().activeElement !== field) field.value = draft;
      const text = draft ?? current;
      error = draft !== undefined ? draftError(feature, draft) : null;
      if (error === null && this.#refusals.has(name) && !pending) error = capitalize(firstLine(this.#refusals.get(name)));
      const dirty = draft !== undefined && draft !== current;
      field.classList.toggle('dirty', dirty && error === null);
      field.classList.toggle('invalid', error !== null);
      if (error !== null) field.setAttribute('aria-invalid', 'true');
      else field.removeAttribute('aria-invalid');
      const glow = performance.now() - (this.#accepted.get(name) ?? -Infinity) < ACCEPTED_MS;
      field.classList.toggle('accepted', glow && !dirty);
      if (pending) trailing = h('span', { class: 'fx-applying', text: 'Applying…' });
      else if (dirty) {
        trailing = h('button', {
          class: 'btn fx-apply', type: 'button', text: 'Apply', disabled: error !== null,
          title: withChord('Apply', 'Enter', this.#state.isApple), 'data-focus-key': `${name}:apply`,
          onclick: () => this.#apply(name, field.value)
        });
      }
      if (slider) {
        const value = draftNumber(text) ?? (typeof feature.value === 'number' ? feature.value : null);
        if (value !== null) {
          const position = slider.scale.position(value);
          slider.value = String(Math.round(position * 1000));
          slider.style.setProperty('--fill', `${(position * 100).toFixed(2)}%`);
        }
        slider.setAttribute('aria-valuetext', value === null ? '—' : `${slider.scale.text(value)}${feature.unit ? ` ${unitLabel(feature.unit)}` : ''}`);
      }
      if (error === null && writable) {
        const value = draftNumber(text) ?? (typeof feature.value === 'number' ? feature.value : null);
        const readout = caption(feature, value);
        if (readout !== '') parts.push(h('span', { class: 'fx-range', text: readout }));
      }
    } else if (this.#refusals.has(name) && !pending) {
      error = capitalize(firstLine(this.#refusals.get(name)));
    } else if (pending) {
      trailing = h('span', { class: 'fx-applying', text: 'Applying…' });
    }
    if (row.managed) parts.unshift(h('span', { class: 'fx-auto', text: 'Set by auto', title: 'Choose Manual to change it' }));
    if (error !== null) {
      captionEl.replaceChildren(h('span', { class: 'fx-error' }, icon('warning-circle', 11), h('span', { text: error })));
      if (field) field.setAttribute('aria-errormessage', captionEl.id);
    } else {
      captionEl.replaceChildren(...parts);
      if (field) field.removeAttribute('aria-errormessage');
    }
    if (trailing) captionEl.append(trailing);
    captionEl.hidden = captionEl.childElementCount === 0 && !(field && field.classList.contains('fx-input'));
  }

  #renderDetails() {
    const source = this.#source;
    this.#details.hidden = !source || this.#query.trim() !== '';
    if (!source) return;
    const kind = KIND[source.kind] ?? 'Area scan';
    this.#details.querySelector('summary .summary').textContent = kind;
    if (!this.#details.open) {
      this.#detailsBody.replaceChildren();
      this.#detailsKey = '';
      return;
    }
    const frame = this.#state.frame;
    const mirrored = source.kind === 'webcam' ? this.#mirrored() : null;
    // Frames arrive a few times a second; rebuild only when something shown changed.
    const key = JSON.stringify([source.id, source.model, frame?.width, frame?.height, frame?.pixelFormat, frame?.path, mirrored]);
    if (key === this.#detailsKey) return;
    this.#detailsKey = key;
    const row = (name, value) => h('div', { class: 'row' }, h('span', { class: 'label', text: name }), h('span', { class: 'value', text: value }));
    const parts = [h('p', { class: 'fx-note', text: GUIDANCE[source.kind] ?? GUIDANCE.sim })];
    if (source.kind === 'sim') parts.push(h('p', { class: 'fx-note', text: 'Camera type detected automatically.\nDeviceScanType = Areascan' }));
    parts.push(row('Model', source.model ?? '—'));
    if (source.vendor && source.vendor !== 'Webcam') parts.push(row('Vendor', source.vendor));
    parts.push(row('Serial', source.serial ?? '—'));
    parts.push(row('Resolution', frame ? `${frame.width} × ${frame.height}` : '—'));
    parts.push(row('Pixel format', frame?.pixelFormat ?? '—'));
    parts.push(row('Frame path', PATHS[frame?.path] ?? '—'));
    if (source.kind === 'webcam') {
      const box = h('input', {
        type: 'checkbox', class: 'fx-check', id: 'fx-mirror', checked: mirrored, 'data-focus-key': 'mirror',
        onchange: () => {
          this.#mirror = box.checked;
          this.#app.emit('mirror', box.checked);
        }
      });
      parts.push(h('div', { class: 'fx-toggle' }, box, h('label', { for: 'fx-mirror', text: 'Mirror preview' })));
      parts.push(h('p', { class: 'fx-note', text: 'Only the preview is mirrored. Captures and recordings keep the camera’s view.' }));
    }
    this.#keepFocus(this.#detailsBody, () => this.#detailsBody.replaceChildren(...parts));
  }

  #mirrored() {
    if (typeof this.#mirror === 'boolean') return this.#mirror;
    if (typeof this.#state.mirror === 'boolean') return this.#state.mirror;
    const track = this.#app.mediaStream?.()?.getVideoTracks?.()[0];
    const facing = track?.getSettings?.().facingMode ?? '';
    return facing === '' || facing === 'user';
  }

  #renderShell() {
    const source = this.#source;
    this.#shell.hidden = !source || this.#query.trim() !== '';
    if (!source || !this.#shell.open) {
      if (!this.#shell.open) this.#shellBody.replaceChildren();
      return;
    }
    const copy = (key, word, run) => {
      const done = this.#copied.has(key);
      return h('button', {
        class: 'btn fx-copy', type: 'button', 'data-focus-key': `shell:${key}`,
        onclick: async () => {
          try {
            const text = await run();
            await navigator.clipboard.writeText(text);
            this.#copied.set(key, true);
            this.#announce('Copied');
            this.#renderShell();
            this.#later(COPIED_MS, () => {
              this.#copied.delete(key);
              this.#renderShell();
            });
          } catch (error) {
            this.#app.log('error', `Could not copy: ${errorText(error)}`);
          }
        }
      }, icon(done ? 'check-circle' : 'copy', 12), done ? 'Copied' : word);
    };
    const command = source.kind === 'sim' ? 'capturefab --camera sim:0 features --json' : 'capturefab discover --json';
    this.#keepFocus(this.#shellBody, () => this.#shellBody.replaceChildren(
      h('p', { class: 'fx-note', text: 'Capturefab’s command line and agent tools return the same JSON for a camera. Copy what this browser measured and reads, to compare it or to share it.' }),
      h('div', { class: 'fx-copies' },
        copy('frame', 'Copy frame JSON', async () => {
          const engine = this.#app.engine;
          if (!engine) throw new Error('Capturefab is still loading; try again in a moment');
          if (!this.#state.frame) throw new Error('No frame to measure yet');
          // With the scopes hidden the engine may hold an old frame: measure the one on screen now.
          // (The Pattern camera renders straight into the engine, so it has no frame to grab.)
          const fresh = await this.#app.grabFrame();
          if (fresh) await engine.analyze(fresh, {});
          const result = await engine.inspect(null);
          this.#app.log('info', 'Copied frame JSON', result);
          return json(result);
        }),
        copy('features', 'Copy features JSON', async () => {
          const result = { features: this.#features.map(featureJson) };
          this.#app.log('info', 'Copied features JSON', result);
          return JSON.stringify(result, null, 2);
        })),
      h('p', { class: 'fx-note', text: source.kind === 'sim' ? 'With the Capturefab app installed, the same camera answers:' : 'With the Capturefab app installed, find this webcam with:' }),
      h('code', { class: 'mono fx-command-line', text: command })));
  }
}

/** Mount the Features pane in `container`. */
export function mountInspector(container, app) {
  const inspector = new Inspector(container, app);
  return {
    focusSearch: () => inspector.focusSearch(),
    refresh: () => inspector.refresh()
  };
}
