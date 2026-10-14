import('./capturefab_web.js').then(({ default: init }) => init()).catch(() => {
  const status = document.getElementById('notice');
  status.textContent = 'The WASM client could not load. Rebuild web-client/dist and reload this page.';
  status.className = 'message error';
  document.getElementById('server-status').textContent = 'Client unavailable';
  document.getElementById('connection-dot').className = 'dot offline';
});
