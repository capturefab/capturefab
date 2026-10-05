/* Static release selection and optional copy buttons. No telemetry or remote dependencies. */
(() => {
  'use strict';
  document.documentElement.dataset.js = 'true';
  const $ = (id) => document.getElementById(id);
  const os = $('os-select');
  const arch = $('arch-select');
  const variant = $('variant-select');
  let manifest = null;
  let release = null;
  let assets = [];
  let processorChosen = false;
  const osNames = { macos: 'macOS', windows: 'Windows', linux: 'Linux' };
  const archNames = { x86_64: 'x64', aarch64: 'ARM64', armv7: 'ARMv7' };
  const osAliases = { darwin: 'macos', mac: 'macos', win: 'windows', win32: 'windows' };
  const archAliases = { x64: 'x86_64', amd64: 'x86_64', arm64: 'aarch64', arm32: 'armv7', armv7l: 'armv7' };
  const safeUrl = (value) => {
    if (typeof value !== 'string' || !value || /[\u0000-\u001f]/.test(value)) return null;
    try {
      const url = new URL(value, document.baseURI);
      return ['https:', 'http:'].includes(url.protocol) || (url.protocol === 'file:' && location.protocol === 'file:') ? url.href : null;
    } catch (_) { return null; }
  };
  const text = (id, value) => { $(id).textContent = value; };
  const formatSize = (value) => typeof value === 'number' && value > 0 ? `${(value / 1048576).toFixed(1)} MiB` : '';
  const isPublished = (asset) => asset.available === true && safeUrl(asset.url) && /^[a-f0-9]{64}$/i.test(asset.sha256 || '') && osNames[asset.os] && archNames[asset.arch] && ['desktop', 'headless'].includes(asset.variant);
  const makeLink = (label, value) => {
    const href = safeUrl(value);
    if (!href) return document.createTextNode(label);
    const link = document.createElement('a');
    link.href = href;
    link.textContent = label;
    return link;
  };
  const clearDownload = () => {
    $('primary-download').hidden = true;
    $('primary-download').removeAttribute('href');
    $('verify-box').hidden = true;
    $('asset-detail').hidden = true;
    $('verify-links').replaceChildren();
  };
  const renderSelection = () => {
    clearDownload();
    if (!manifest) return;
    if (!release || !assets.some(isPublished)) {
      text('download-label', 'No published binaries yet');
      text('download-description', 'Use the source checkout and build guide, or follow the release feed for new downloads.');
      return;
    }
    if (!os.value || !arch.value) {
      text('download-label', `capturefab ${release.version}`);
      text('download-description', 'Choose an operating system and processor to select the matching binary.');
      return;
    }
    const matching = assets.filter((asset) => asset.os === os.value && asset.arch === arch.value && asset.variant === variant.value);
    const selected = matching.find(isPublished);
    text('download-label', `${osNames[os.value]} / ${archNames[arch.value]} / ${variant.value}`);
    if (!selected) {
      text('download-description', 'This build is not published yet. Check all published builds or follow the release feed.');
      return;
    }
    text('download-description', `capturefab ${release.version}${release.date ? ` · ${release.date}` : ''}`);
    text('asset-detail', [selected.name, formatSize(selected.size)].filter(Boolean).join(' · '));
    $('asset-detail').hidden = false;
    const download = $('primary-download');
    download.href = safeUrl(selected.url);
    download.setAttribute('download', selected.name);
    download.setAttribute('aria-label', `Download capturefab ${release.version} for ${osNames[os.value]} ${archNames[arch.value]} (${variant.value})`);
    download.hidden = false;
    text('checksum-text', `SHA-256: ${selected.sha256}`);
    const filename = /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(selected.name || '') ? selected.name : 'DOWNLOADED_FILE';
    text('verify-command', os.value === 'macos' ? `shasum -a 256 ${filename}` : os.value === 'windows' ? `Get-FileHash ${filename} -Algorithm SHA256` : `sha256sum ${filename}`);
    const links = $('verify-links');
    if (selected.signature_url) links.append(makeLink('Signature', selected.signature_url));
    if (selected.bundle_url && selected.bundle_url !== selected.url) links.append(makeLink('Application bundle', selected.bundle_url));
    if (release.checksums_url) links.append(makeLink('SHA256SUMS', release.checksums_url));
    if (release.checksums_signature_url) links.append(makeLink('Checksum signature', release.checksums_signature_url));
    if (release.url) links.append(makeLink('Release notes', release.url));
    $('verify-box').hidden = false;
  };
  const tableNotice = (message) => {
    const row = document.createElement('tr');
    const cell = document.createElement('td');
    cell.colSpan = 5; cell.textContent = message;
    row.append(cell); $('asset-table').replaceChildren(row);
  };
  const renderTable = () => {
    const body = $('asset-table');
    body.replaceChildren();
    if (!assets.length) {
      tableNotice('No releases have been published.');
      return;
    }
    for (const asset of assets) {
      const row = document.createElement('tr');
      for (const value of [osNames[asset.os] || asset.os, archNames[asset.arch] || asset.arch, asset.variant]) {
        const cell = document.createElement('td'); cell.textContent = value; row.append(cell);
      }
      const file = document.createElement('td');
      const verify = document.createElement('td');
      if (isPublished(asset)) {
        file.append(makeLink(asset.name, asset.url));
        verify.append(makeLink('Manifest / SHA-256', 'releases.json'));
        if (asset.signature_url) { verify.append(document.createElement('br'), makeLink('Signature', asset.signature_url)); }
      } else {
        file.textContent = 'Not published'; file.className = 'unavailable'; verify.textContent = '—';
      }
      row.append(file, verify); body.append(row);
    }
  };
  const updateHint = () => {
    const hints = {
      macos: 'Apple Silicon (M1 and later) uses ARM64. Intel Macs use x64. If unsure, check Apple menu → About This Mac.',
      windows: 'Most Intel and AMD PCs use x64. Windows on ARM uses ARM64. Check Settings → System → About → System type.',
      linux: 'Run uname -m: x86_64 means x64; aarch64 means ARM64; armv7l means ARMv7. Pi and Jetson downloads must match the installed OS architecture.'
    };
    text('platform-hint', hints[os.value] || 'Choose the computer running capturefab. Architecture selection is required when the browser cannot identify it reliably.');
  };
  for (const control of [os, arch, variant]) control.addEventListener('change', () => { if (control === arch) processorChosen = true; updateHint(); renderSelection(); });
  document.querySelector('.platform-form').addEventListener('submit', (event) => event.preventDefault());
  const detectPlatform = async () => {
    const platform = navigator.userAgentData?.platform || navigator.platform || '';
    const ua = navigator.userAgent || '';
    if (/mac/i.test(platform)) os.value = 'macos';
    else if (/win/i.test(platform)) os.value = 'windows';
    else if (/linux/i.test(platform) && !/android/i.test(ua)) os.value = 'linux';
    // Macintosh and Windows user-agent strings can report x64 on ARM hardware.
    // Only use explicit architecture information; leave the choice to the user otherwise.
    if (navigator.userAgentData?.getHighEntropyValues) {
      try {
        const hints = await navigator.userAgentData.getHighEntropyValues(['architecture', 'bitness']);
        if (!processorChosen) {
          if (hints.architecture === 'arm') arch.value = hints.bitness === '64' ? 'aarch64' : hints.bitness === '32' ? 'armv7' : '';
          else if (hints.architecture === 'x86' && hints.bitness === '64') arch.value = 'x86_64';
        }
      } catch (_) { /* Explicit selectors remain available. */ }
    } else if (/\baarch64\b|\barm64\b/i.test(ua) && os.value === 'linux') arch.value = 'aarch64';
    updateHint(); renderSelection();
  };
  fetch('releases.json', { cache: 'no-cache' })
    .then((response) => { if (!response.ok) throw new Error('manifest unavailable'); return response.json(); })
    .then((data) => {
      if (data.schema_version !== 1 || !Array.isArray(data.releases)) throw new Error('unsupported manifest');
      manifest = data;
      release = data.releases.find((item) => item.version === data.latest) || data.releases[0] || null;
      if (release && typeof release.version !== 'string') throw new Error('invalid release');
      assets = Array.isArray(release?.assets) ? release.assets.filter((item) => item && typeof item.name === 'string').map((asset) => ({ ...asset, os: osAliases[asset.os] || asset.os, arch: archAliases[asset.arch] || asset.arch })) : [];
      text('release-version', release ? `v${release.version}` : 'source checkout');
      renderTable(); renderSelection();
    })
    .catch(() => {
      text('download-label', 'Open the release manifest');
      text('download-description', 'Automatic selection is unavailable. The manifest contains published download URLs and checksums.');
      clearDownload();
      tableNotice('Open all downloads or releases.json to inspect the available builds.');
    });
  detectPlatform();
  const copy = async (value) => {
    if (navigator.clipboard?.writeText) return navigator.clipboard.writeText(value);
    const area = document.createElement('textarea'); area.value = value; area.style.position = 'fixed'; area.style.opacity = '0';
    document.body.append(area); area.select(); const ok = document.execCommand('copy'); area.remove();
    if (!ok) throw new Error('copy unavailable');
  };
  document.querySelectorAll('.copy-button').forEach((button) => button.addEventListener('click', async () => {
    try { await copy(button.parentElement.querySelector('code').textContent); button.textContent = 'Copied'; }
    catch (_) { button.textContent = 'Select text'; }
    window.setTimeout(() => { button.textContent = 'Copy'; }, 1800);
  }));
})();
