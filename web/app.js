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
  // A browser on a 32-bit ARM system, for which there is no build.
  let thirtyTwoBitArm = false;
  const osNames = { macos: 'macOS', windows: 'Windows', linux: 'Linux' };
  const archNames = { x86_64: 'x64', aarch64: 'ARM64' };
  const osAliases = { darwin: 'macos', mac: 'macos', win: 'windows', win32: 'windows' };
  const archAliases = { x64: 'x86_64', amd64: 'x86_64', arm64: 'aarch64' };
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
    if ($('install-hint')) $('install-hint').hidden = true;
  };
  const renderSelection = () => {
    clearDownload();
    if (!manifest) return;
    document.querySelector('.download-result').removeAttribute('data-state');
    if (!release || !assets.some(isPublished)) {
      text('download-label', 'No published binaries yet');
      text('download-description', 'Use the source build guide. Read the release feed for new downloads.');
      return;
    }
    if (!os.value || !arch.value) {
      text('download-label', `capturefab ${release.version}`);
      text('download-description', 'Select your operating system and processor to find the applicable build.');
      return;
    }
    const matching = assets.filter((asset) => asset.os === os.value && asset.arch === arch.value && asset.variant === variant.value);
    const selected = matching.find(isPublished);
    text('download-label', `${osNames[os.value]} / ${archNames[arch.value]} / ${variant.value}`);
    if (!selected) {
      text('download-description', 'This build is not available. Open all downloads or read the release feed.');
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
    download.textContent = `Download for ${osNames[os.value]}`;
    if ($('install-hint')) {
      const instructions = {
        macos: selected.variant === 'desktop' ? 'Extract the ZIP file. Move Capturefab.app to Applications. Open Capturefab.app.' : 'Extract the ZIP file. In a terminal, run capturefab --help from the extracted folder.',
        windows: 'Extract all files from the ZIP archive.' + (selected.variant === 'headless' ? ' In PowerShell, run .\\capturefab.exe --help from the extracted folder.' : ' Open capturefab.exe from the extracted folder.'),
        linux: 'Extract the .tar.gz archive. From the extracted folder, use ./capturefab for the desktop or ./capturefab --help for CLI options.'
      };
      text('install-hint', instructions[os.value]);
      $('install-hint').hidden = false;
    }
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
  const updateHint = () => {
    const hints = {
      macos: 'Apple silicon (M1 and later) uses ARM64. Intel Macs use x64. Read Apple menu → About This Mac to identify the processor.',
      windows: 'Intel and AMD PCs usually use x64. Windows on ARM uses ARM64. Read Settings → System → About → System type.',
      linux: 'Run dpkg --print-architecture: amd64 means x64 and arm64 means ARM64. armhf is 32-bit ARM, which has no build; Raspberry Pi 3 and later run the 64-bit Raspberry Pi OS. Without dpkg, run uname -m, but it prints aarch64 on 32-bit Raspberry Pi OS too.'
    };
    const armhf = 'This browser runs on 32-bit ARM, which has no Capturefab build. Raspberry Pi 3 and later run the 64-bit Raspberry Pi OS, which uses ARM64. dpkg --print-architecture prints arm64 there and armhf on a 32-bit system; uname -m prints aarch64 on both.';
    text('platform-hint', (thirtyTwoBitArm && os.value === 'linux' && !arch.value ? armhf : hints[os.value]) || 'Select the computer that runs Capturefab. If the browser cannot identify the processor architecture, select it.');
  };
  const hasDownloadPanel = Boolean(os && arch && variant);
  if (hasDownloadPanel) {
    for (const control of [os, arch, variant]) control.addEventListener('change', () => { if (control === arch || control === os) processorChosen = true; updateHint(); renderSelection(); });
    document.querySelector('.platform-form').addEventListener('submit', (event) => event.preventDefault());
  }
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
          if (hints.architecture === 'arm' && hints.bitness === '64') arch.value = 'aarch64';
          else if (hints.architecture === 'x86' && hints.bitness === '64') arch.value = 'x86_64';
          thirtyTwoBitArm = hints.architecture === 'arm' && hints.bitness === '32';
        }
      } catch (_) { /* Explicit selectors remain available. */ }
    } else if (/\baarch64\b|\barm64\b/i.test(ua) && os.value === 'linux') arch.value = 'aarch64';
    else if (/\barmv[67]l?\b|\barmhf\b/i.test(ua) && os.value === 'linux') thirtyTwoBitArm = true;
    updateHint(); renderSelection();
  };
  if (hasDownloadPanel) fetch('releases.json', { cache: 'no-cache' })
    .then((response) => { if (!response.ok) throw new Error('manifest unavailable'); return response.json(); })
    .then((data) => {
      if (data.schema_version !== 1 || !Array.isArray(data.releases)) throw new Error('unsupported manifest');
      manifest = data;
      release = data.releases.find((item) => item.version === data.latest) || data.releases[0] || null;
      if (release && typeof release.version !== 'string') throw new Error('invalid release');
      assets = Array.isArray(release?.assets) ? release.assets.filter((item) => item && typeof item.name === 'string').map((asset) => ({ ...asset, os: osAliases[asset.os] || asset.os, arch: archAliases[asset.arch] || asset.arch })) : [];
      text('release-version', release ? `v${release.version}` : 'none published yet');
      renderSelection();
    })
    .catch(() => {
      manifest = null;
      release = null;
      assets = [];
      text('release-version', 'see the manifest');
      text('download-label', 'Use the full download list');
      text('download-description', 'Automatic selection is unavailable. Open all downloads below to select a file and read its checksum.');
      clearDownload();
      document.querySelector('.download-result').dataset.state = 'warning';
    });
  if (hasDownloadPanel) detectPlatform();

  // The native controls always work. Automatic playback is limited to visible
  // clips and respects the viewer's motion and data preferences.
  const clips = [...document.querySelectorAll('.product-video')];
  const motion = window.matchMedia('(prefers-reduced-motion: reduce)');
  const connection = navigator.connection;
  const canObserve = 'IntersectionObserver' in window;
  const playback = new Map(clips.map((video) => [video, { visible: !canObserve, userPaused: false, autoPaused: false, autoStarting: false, automatic: false }]));
  const syncPlayback = (video) => {
    const state = playback.get(video);
    const allowAutomatic = canObserve && !motion.matches && !connection?.saveData;
    if (!state.visible || document.hidden || (!allowAutomatic && (state.automatic || state.autoStarting))) {
      state.autoPaused = true;
      state.autoStarting = false;
      state.automatic = false;
      video.pause();
    } else if (allowAutomatic && !state.userPaused && video.paused) {
      state.autoPaused = false;
      state.autoStarting = true;
      video.play().catch(() => { state.autoStarting = false; /* Native controls remain available. */ });
    }
  };
  clips.forEach((video) => {
    video.addEventListener('pause', () => {
      if (!playback.get(video).autoPaused) playback.get(video).userPaused = true;
    });
    video.addEventListener('play', () => {
      if (video.paused) return; // A canceled play request can leave a queued event.
      const state = playback.get(video);
      state.userPaused = false;
      state.autoPaused = false;
      state.automatic = state.autoStarting;
      state.autoStarting = false;
    });
  });
  if (clips.length && canObserve) {
    const observer = new IntersectionObserver((entries) => entries.forEach(({ target, isIntersecting, intersectionRatio }) => {
      playback.get(target).visible = isIntersecting && intersectionRatio >= 0.4;
      syncPlayback(target);
    }), { threshold: [0, 0.4] });
    clips.forEach((video) => observer.observe(video));
  }
  document.addEventListener('visibilitychange', () => clips.forEach(syncPlayback));
  motion.addEventListener?.('change', () => clips.forEach(syncPlayback));
  connection?.addEventListener?.('change', () => clips.forEach(syncPlayback));
  const copy = async (value) => {
    if (navigator.clipboard?.writeText) return navigator.clipboard.writeText(value);
    const area = document.createElement('textarea'); area.value = value; area.style.position = 'fixed'; area.style.opacity = '0';
    document.body.append(area); area.select(); const ok = document.execCommand('copy'); area.remove();
    if (!ok) throw new Error('copy unavailable');
  };
  document.querySelectorAll('.copy-button').forEach((button) => button.addEventListener('click', async () => {
    try { await copy(button.parentElement.querySelector('code').textContent); button.textContent = 'Copied'; button.dataset.state = 'success'; }
    catch (_) { button.textContent = 'Select text'; button.dataset.state = 'warning'; }
    window.setTimeout(() => { button.textContent = 'Copy'; button.removeAttribute('data-state'); }, 1800);
  }));
})();
