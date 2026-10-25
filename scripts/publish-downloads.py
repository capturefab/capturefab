#!/usr/bin/env python3
"""Upload a release's files to the capturefab.com download bucket as v<version>/<file>.

Every file must match SHA256SUMS. An object that already exists is kept when its
bytes match and is an error when they differ: a published version never changes.
Before promotion (--unpublished), different bytes are an earlier failed candidate's
and are replaced. SHA256SUMS and its signature go last, so the list never names a
missing file."""
import argparse
import os
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

from release import digest, version

ROOT = Path(__file__).resolve().parents[1]
BUCKET = 'capturefab-downloads'
# The Worker in site/ serves the bucket here; release.py writes these URLs.
DOWNLOAD_BASE = 'https://capturefab.com/download'
# R2's NoSuchKey answer, as wrangler prints it; every other failed read aborts.
MISSING = ('The specified key does not exist', 'NoSuchKey', '10007')
TYPES = [('.tar.gz', 'application/gzip'), ('.zip', 'application/zip'), ('.dmg', 'application/x-apple-diskimage'),
         ('.json', 'application/json'), ('.asc', 'application/pgp-signature')]

def content_type(name):
    if name == 'SHA256SUMS':
        return 'text/plain; charset=utf-8'
    return next((kind for suffix, kind in TYPES if name.endswith(suffix)), 'application/octet-stream')

def checksums(folder):
    sums = folder / 'SHA256SUMS'
    if not sums.is_file():
        raise ValueError(f'{sums} is missing')
    listed = {}
    for line in sums.read_text(encoding='utf-8').splitlines():
        value, _, name = line.partition('  ')
        if len(value) != 64 or not name or '/' in name or name in listed:
            raise ValueError(f'malformed SHA256SUMS line: {line!r}')
        listed[name] = value
    for name, value in listed.items():
        path = folder / name
        if not path.is_file() or digest(path) != value:
            raise ValueError(f'{name} does not match SHA256SUMS')
    return listed

def wrangler(command, *args):
    return subprocess.run([*command, 'r2', 'object', *args, '--remote'], cwd=ROOT / 'site', capture_output=True, text=True)

def publish(folder, ver, bucket, command, dry_run=False, unpublished=False):
    listed = checksums(folder)
    names = sorted(listed) + [n for n in ('SHA256SUMS', 'SHA256SUMS.asc') if (folder / n).is_file()]
    uploaded = kept = 0
    with tempfile.TemporaryDirectory(prefix='capturefab-downloads-') as temp:
        for name in names:
            key, path = f'{bucket}/v{ver}/{name}', folder / name
            existing = Path(temp) / name
            result = wrangler(command, 'get', key, '--file', str(existing))
            if result.returncode == 0 and existing.is_file():
                same = digest(existing) == digest(path)
                existing.unlink()
                if same:
                    kept += 1
                    continue
                if not unpublished:
                    raise ValueError(f'v{ver}/{name} is already published with different contents; refusing to replace it')
                print(f'replacing {key} from an earlier unpublished candidate')
            elif result.returncode == 0 or not any(text in result.stdout + result.stderr for text in MISSING):
                # Not a clear "no such key": the object may exist, so it must not be overwritten.
                raise RuntimeError(f'cannot read {key} to compare it: {(result.stderr or result.stdout).strip()}')
            if dry_run:
                print(f'would upload {key}')
                continue
            result = wrangler(command, 'put', key, '--file', str(path), '--content-type', content_type(name))
            if result.returncode:
                raise RuntimeError(f'upload of {name} failed: {(result.stderr or result.stdout).strip()}')
            uploaded += 1
            print(f'uploaded {key}')
    print(f'v{ver}: {uploaded} uploaded, {kept} already published; served at {DOWNLOAD_BASE}/v{ver}/', file=sys.stderr)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    parser.add_argument('--input', type=Path, required=True, help='folder with SHA256SUMS and the files it lists')
    parser.add_argument('--bucket', default=os.environ.get('CAPTUREFAB_DOWNLOAD_BUCKET') or BUCKET)
    parser.add_argument('--wrangler', default='npx --no-install wrangler', help='command that runs wrangler, from site/')
    parser.add_argument('--dry-run', action='store_true', help='check the files and the bucket without uploading')
    parser.add_argument('--unpublished', action='store_true', help='the version has no final release yet: replace files left by a failed candidate')
    args = parser.parse_args()
    try:
        publish(args.input.resolve(), version(args.version), args.bucket, shlex.split(args.wrangler), args.dry_run, args.unpublished)
    except (ValueError, RuntimeError, OSError) as error:
        parser.exit(1, f'publish-downloads: {error}\n')

if __name__ == '__main__':
    main()
