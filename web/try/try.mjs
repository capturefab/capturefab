// "Try with your webcam": Capturefab in a full-viewport dialog on the page that
// imports this module. web/app.js loads it with import() when a launcher is pressed,
// then calls launch(button). Nothing here runs until then, and nothing leaves the
// computer: the camera, the pixel engine and the saved files all stay in this tab.
import { CapturefabTry, TAG } from './workbench.mjs';

// The dialog itself lives in the page, so it gets a tiny document-level sheet; the
// workbench inside it is styled from its own shadow root.
const DOCUMENT_CSS = `
dialog.cf-try {
  position: fixed; inset: 0; box-sizing: border-box;
  width: 100vw; width: 100dvw; height: 100vh; height: 100dvh;
  max-width: none; max-height: none; margin: 0; padding: 0; border: 0;
  background: transparent; color-scheme: light dark; overflow: hidden; outline: none;
  opacity: 1; scale: 1;
  transition: opacity 220ms cubic-bezier(.33, 1, .68, 1), scale 220ms cubic-bezier(.33, 1, .68, 1),
    overlay 220ms allow-discrete, display 220ms allow-discrete;
}
dialog.cf-try:not([open]) { opacity: 0; scale: .985; transition-duration: 140ms; }
@starting-style { dialog.cf-try[open] { opacity: 0; scale: .985; } }
dialog.cf-try::backdrop { background: rgb(0 0 0 / .42); }
dialog.cf-try > capturefab-try { display: block; width: 100%; height: 100%; }
@media (prefers-reduced-motion: reduce) {
  dialog.cf-try, dialog.cf-try:not([open]) { scale: 1; }
  @starting-style { dialog.cf-try[open] { scale: 1; } }
}
`;

let prepared = null;
let dialog = null;
let element = null;
let opener = null;
let overflow = null;

/** Why Capturefab cannot run in this browser, in words for people; '' when it can. */
export const unsupported = () => {
  const scope = globalThis;
  if (scope.isSecureContext !== true) return 'This page is not secure. Open it with https, or from localhost.';
  if (typeof scope.HTMLDialogElement !== 'function' || !('showModal' in scope.HTMLDialogElement.prototype)) {
    return 'This browser cannot show dialogs. Update the browser, or download Capturefab.';
  }
  if (!scope.customElements || typeof scope.Element?.prototype.attachShadow !== 'function') {
    return 'This browser cannot define custom elements. Update the browser, or download Capturefab.';
  }
  if (typeof scope.WebAssembly !== 'object') return 'WebAssembly is turned off or not available in this browser.';
  if (typeof scope.Worker !== 'function') return 'This browser cannot run web workers. Update the browser, or download Capturefab.';
  if (typeof scope.VideoFrame !== 'function' && typeof scope.createImageBitmap !== 'function') {
    return 'This browser cannot read video frames. Update the browser, or download Capturefab.';
  }
  return '';
};

const canAdopt = () => 'adoptedStyleSheets' in Document.prototype && 'replaceSync' in CSSStyleSheet.prototype;

const sheetFrom = (text) => {
  if (!canAdopt()) return text;
  const sheet = new CSSStyleSheet();
  sheet.replaceSync(text);
  return sheet;
};

const fetchText = async (name, required) => {
  try {
    const response = await fetch(new URL(name, import.meta.url), { credentials: 'same-origin' });
    if (!response.ok) throw new Error(`${name}: HTTP ${response.status}`);
    return await response.text();
  } catch (error) {
    if (required) throw new Error('Its styles did not load. Make sure that you are online, then try again.', { cause: error });
    console.warn('Capturefab: optional styles did not load', error);
    return '';
  }
};

// Load the styles once, define the element, and style the dialog in the page.
const prepare = () => {
  prepared ??= (async () => {
    const [workbench, panes] = await Promise.all([fetchText('./workbench.css', true), fetchText('./panes.css', false)]);
    CapturefabTry.styles = [workbench, panes].filter(Boolean).map(sheetFrom);
    if (!customElements.get(TAG)) customElements.define(TAG, CapturefabTry);
    const page = sheetFrom(DOCUMENT_CSS);
    if (typeof page === 'string') {
      const style = document.createElement('style');
      style.textContent = page;
      document.head.append(style);
    } else {
      document.adoptedStyleSheets = [...document.adoptedStyleSheets, page];
    }
  })();
  prepared.catch(() => { prepared = null; });
  return prepared;
};

// Modules the workbench imports only once it opens; fetched alongside the styles
// instead of one after another.
const LATER = ['./engine.mjs', './core.mjs', './sidebar.mjs', './inspector.mjs', './features.mjs', './library.mjs'];
let preloaded = false;

const preload = () => {
  if (preloaded) return;
  preloaded = true;
  for (const name of LATER) {
    const link = document.createElement('link');
    link.rel = 'modulepreload';
    link.href = new URL(name, import.meta.url).href;
    document.head.append(link);
  }
  // The engine's module chain leads to its worker and wasm: start it at once.
  import('./engine.mjs').catch(() => {});
};

const lockScroll = () => {
  const root = document.documentElement;
  overflow ??= root.style.overflow;
  root.style.overflow = 'hidden';
};

const unlockScroll = () => {
  if (overflow === null) return;
  document.documentElement.style.overflow = overflow;
  overflow = null;
};

const build = () => {
  dialog = document.createElement('dialog');
  dialog.className = 'cf-try';
  dialog.setAttribute('aria-label', 'Capturefab in your browser');
  element = document.createElement(TAG);
  dialog.append(element);
  // The ✕ button and the workbench's own Escape handling ask to close.
  element.addEventListener('cf-close', () => dialog.close());
  // Escape and requestClose(): let the workbench put away a panel first.
  dialog.addEventListener('cancel', (event) => {
    if (element.consumeEscape()) event.preventDefault();
  });
  dialog.addEventListener('close', () => {
    // Recordings are finished and saved in the background; the dialog is already gone.
    element.shutdown();
    unlockScroll();
    const back = opener;
    opener = null;
    if (back?.isConnected && typeof back.focus === 'function') back.focus({ focusVisible: true });
  });
  document.body.append(dialog);
};

/**
 * Open Capturefab in a modal dialog. `button` gets focus back when it closes. With
 * `autostart`, the webcam is requested at once (the click is the user gesture); the
 * welcome screen offers the Pattern camera when there is none or access is refused.
 * Throws Error(message) with words for people when this browser cannot run it.
 */
export async function launch(button, { autostart = true } = {}) {
  const reason = unsupported();
  if (reason) throw new Error(reason);
  preload();
  await prepare();
  if (!dialog || !dialog.isConnected) build();
  if (button instanceof HTMLElement) opener = button;
  // A launcher pressed from the keyboard shows its focus ring; so should the workbench.
  const keyboard = Boolean(document.activeElement?.matches?.(':focus-visible'));
  if (!dialog.open) {
    lockScroll();
    dialog.showModal();
  }
  await element.open({ autostart, keyboard });
  return element;
}
