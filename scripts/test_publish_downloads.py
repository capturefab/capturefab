#!/usr/bin/env python3
"""Unit tests for the download bucket uploader, against a stand-in for wrangler."""
import hashlib
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location('publish_downloads', SCRIPTS / 'publish-downloads.py')
publish = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publish)

# Keeps objects as files under BUCKET_DIR/<bucket>/<key> and logs each call.
FAKE = '''import shutil, sys
from pathlib import Path
store, log = Path(sys.argv[1]), Path(sys.argv[2])
args = sys.argv[3:]
with log.open('a') as stream:
    stream.write(' '.join(args) + '\\n')
assert args[:2] == ['r2', 'object'] and args[-1] == '--remote', args
action, key, path = args[2], args[3], Path(args[args.index('--file') + 1])
target = store / key
if action == 'get':
    if (store / 'offline').exists():
        sys.exit('fetch failed: network unreachable')
    if not target.is_file():
        sys.exit('The specified key does not exist.')
    shutil.copyfile(target, path)
else:
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(path, target)
'''


class Publish(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.release, self.store, self.log = self.root / 'release', self.root / 'store', self.root / 'log'
        self.release.mkdir()
        fake = self.root / 'wrangler.py'
        fake.write_text(FAKE)
        self.command = [sys.executable, str(fake), str(self.store), str(self.log)]
        for name, data in [('capturefab-1.2.3-linux-x86_64.tar.gz', b'linux'), ('capturefab-1.2.3-source.tar.gz', b'source')]:
            (self.release / name).write_bytes(data)
        sums = ''.join(f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n' for p in sorted(self.release.iterdir()))
        (self.release / 'SHA256SUMS').write_text(sums)
        (self.release / 'release-notes.md').write_text('not uploaded')

    def run_publish(self, **kwargs):
        publish.publish(self.release, '1.2.3', 'bucket', self.command, **kwargs)

    def puts(self):
        return [line.split()[3] for line in self.log.read_text().splitlines() if line.split()[2] == 'put']

    def test_uploads_listed_files_then_checksums(self):
        self.run_publish()
        self.assertEqual(self.puts(), ['bucket/v1.2.3/capturefab-1.2.3-linux-x86_64.tar.gz', 'bucket/v1.2.3/capturefab-1.2.3-source.tar.gz', 'bucket/v1.2.3/SHA256SUMS'])
        self.assertEqual((self.store / 'bucket/v1.2.3/capturefab-1.2.3-source.tar.gz').read_bytes(), b'source')
        self.assertIn('--content-type application/gzip', self.log.read_text())
        self.assertFalse((self.store / 'bucket/v1.2.3/release-notes.md').exists())

    def test_running_again_keeps_identical_objects(self):
        self.run_publish()
        self.log.unlink()
        self.run_publish()
        self.assertEqual(self.puts(), [])

    def test_refuses_to_replace_different_contents(self):
        target = self.store / 'bucket/v1.2.3/capturefab-1.2.3-source.tar.gz'
        target.parent.mkdir(parents=True)
        target.write_bytes(b'other')
        with self.assertRaisesRegex(ValueError, 'different contents'):
            self.run_publish()
        self.assertEqual(target.read_bytes(), b'other')

    def test_replaces_a_failed_candidates_files_before_promotion(self):
        target = self.store / 'bucket/v1.2.3/capturefab-1.2.3-source.tar.gz'
        target.parent.mkdir(parents=True)
        target.write_bytes(b'other')
        self.run_publish(unpublished=True)
        self.assertEqual(target.read_bytes(), b'source')

    def test_aborts_when_an_object_cannot_be_read(self):
        self.store.mkdir()
        (self.store / 'offline').touch()
        with self.assertRaisesRegex(RuntimeError, 'cannot read'):
            self.run_publish(unpublished=True)
        self.assertEqual(self.puts(), [])

    def test_refuses_files_that_do_not_match_checksums(self):
        (self.release / 'capturefab-1.2.3-source.tar.gz').write_bytes(b'tampered')
        with self.assertRaisesRegex(ValueError, 'does not match SHA256SUMS'):
            self.run_publish()
        self.assertFalse(self.log.exists())

    def test_dry_run_uploads_nothing(self):
        self.run_publish(dry_run=True)
        self.assertEqual(self.puts(), [])


if __name__ == '__main__':
    unittest.main()
