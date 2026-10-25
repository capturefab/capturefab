/* Run with node scripts/test-docs-search.js; no test runner dependency. */
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { normalize, prepareDocuments, searchDocuments, snippet } = require('../web/docs.js');

const fixture = prepareDocuments([
  { id: 'capture', title: 'Capture frames', category: 'Guide', tags: ['snapshots'], summary: 'Save images from a camera.', body: 'capturefab --camera sim:0 capture --destination archive -o run-1. ExposureTime controls capture brightness.', url: 'docs.html#capture' },
  { id: 'destinations', title: 'Save to folders and drives', category: 'Guide', tags: ['destination', 'folder'], summary: 'Choose a named destination for captured images.', body: 'Saved folders accept captures using --destination archive. Choose a folder on your disk or a removable drive.', url: 'docs.html#destinations' },
  { id: 'sessions', title: 'Control an existing session', category: 'Reference', tags: ['sessions', 'rpc'], summary: 'Share the running app’s camera connection.', body: 'capturefab --session gui status --json. Session operations include capture, set, undo and redo.', url: 'docs.html#sessions' },
  { id: 'exposure', title: 'Réglage de l’exposition', category: 'Guide', tags: ['ExposureTime'], summary: 'Tune brightness and exposure time.', body: 'Read ExposureTime and set ExposureTime=5000.', url: 'docs.html#exposure' }
]);
const search = (query) => searchDocuments(fixture, query);
assert.equal(normalize('ÉXPOSURE'), 'exposure');
assert.equal(search('folders --destination')[0].document.id, 'destinations');
assert.equal(search('ExposureTime')[0].document.id, 'exposure');
assert.equal(search('exposure time')[0].document.id, 'exposure');
assert.equal(search('SESSION')[0].document.id, 'sessions');
assert.equal(search('reglage')[0].document.id, 'exposure');
assert.equal(search('capt')[0].document.id, 'capture');
assert.equal(search('capture nonexistent').length, 0);
assert.equal(search('   ').length, fixture.length);
assert.equal(search('!@#$').length, 0);
assert.match(snippet(fixture[0], '--destination'), /--destination/);
assert.match(snippet(fixture[2], 'session'), /--session/);
assert.match(snippet(fixture[2], 'camera'), /running app/);
assert.match(snippet(fixture[2], 'session'), /redo\.$/);

const indexPath = path.join(__dirname, '../web/search-index.json');
if (fs.existsSync(indexPath)) {
  const index = JSON.parse(fs.readFileSync(indexPath, 'utf8'));
  const actual = prepareDocuments(index.documents);
  for (const query of ['folders --destination', 'ExposureTime', 'session', 'simulator', 'storage', 'forward RTSP']) {
    assert.ok(searchDocuments(actual, query).length, `Published documentation must answer ${JSON.stringify(query)}`);
  }
  assert.equal(searchDocuments(actual, 'scanners')[0].document.id, 'scanners');
  // First-launch prompts are found by the words a dialog shows.
  for (const query of ['smartscreen', 'firewall', 'winusb', 'local network', 'install script']) {
    assert.equal(searchDocuments(actual, query)[0].document.id, 'installation', `${query} must find the installation guide first`);
  }
}
console.log('Documentation search ranking, command aliases, multiword matching and snippets passed.');
