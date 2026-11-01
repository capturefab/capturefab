/* Run with node scripts/test-docs-search.js; no test runner dependency. */
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { normalize, prepareDocuments, searchDocuments, snippet } = require('../web/docs.js');

const fixture = prepareDocuments([
  { id: 'capture', title: 'Capture frames', category: 'Guide', tags: ['snapshots'], summary: 'Save images from a camera.', body: 'capturefab capture -c sim:0 -o run-1.png --on-exists number. ExposureTime controls capture brightness.', url: 'docs.html#capture' },
  { id: 'sessions', title: 'Keep cameras open', category: 'Guide', tags: ['session', '--keep'], summary: 'Run commands through a session that holds the camera.', body: 'capturefab inspect -c sim:0 --keep 5m. Background jobs run in a session; jobs stop and jobs wait control them.', url: 'docs.html#sessions' },
  { id: 'json', title: 'JSON results and exit codes', category: 'Reference', tags: ['json', 'exit code'], summary: 'One JSON document on stdout.', body: 'capturefab --json get -c sim:0 Width. Exit 2 means usage; error.code names the reason.', url: 'docs.html#cli-reference' },
  { id: 'exposure', title: 'Réglage de l’exposition', category: 'Guide', tags: ['ExposureTime'], summary: 'Tune brightness and exposure time.', body: 'Read ExposureTime and set ExposureTime=5ms.', url: 'docs.html#exposure' }
]);
const search = (query) => searchDocuments(fixture, query);
assert.equal(normalize('ÉXPOSURE'), 'exposure');
assert.equal(search('keep --keep')[0].document.id, 'sessions');
assert.equal(search('ExposureTime')[0].document.id, 'exposure');
assert.equal(search('exposure time')[0].document.id, 'exposure');
assert.equal(search('SESSION')[0].document.id, 'sessions');
assert.equal(search('reglage')[0].document.id, 'exposure');
assert.equal(search('capt')[0].document.id, 'capture');
assert.equal(search('exit code')[0].document.id, 'json');
assert.equal(search('capture nonexistent').length, 0);
assert.equal(search('   ').length, fixture.length);
assert.equal(search('!@#$').length, 0);
assert.match(snippet(fixture[0], '--on-exists'), /--on-exists/);
assert.match(snippet(fixture[1], 'session'), /session/i);
assert.match(snippet(fixture[1], 'jobs'), /jobs wait/);

const indexPath = path.join(__dirname, '../web/search-index.json');
if (fs.existsSync(indexPath)) {
  const index = JSON.parse(fs.readFileSync(indexPath, 'utf8'));
  const actual = prepareDocuments(index.documents);
  for (const query of ['--keep', 'ExposureTime', 'session', 'simulator', 'trigger', 'record rtsp', 'camera_ambiguous', 'agent']) {
    assert.ok(searchDocuments(actual, query).length, `Published documentation must answer ${JSON.stringify(query)}`);
  }
  for (const [query, id] of [['triggered capture', 'triggers'], ['time lapse', 'scheduling'], ['password', 'logins'], ['restore settings', 'presets'], ['doctor', 'doctor']]) {
    assert.equal(searchDocuments(actual, query)[0].document.id, id, `${query} must find ${id} first`);
  }
  // First-use prompts are found by the words a dialog shows.
  for (const query of ['smartscreen', 'firewall', 'winusb', 'local network', 'install script']) {
    assert.equal(searchDocuments(actual, query)[0].document.id, 'installation', `${query} must find the installation guide first`);
  }
}
console.log('Documentation search ranking, command names, multiword matching and snippets passed.');
