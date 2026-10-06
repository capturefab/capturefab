#!/usr/bin/env python3
"""Unit tests for the release, site, and terminal-screenshot scripts."""
import contextlib
import importlib.util
import io
import json
import os
import plistlib
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import release

SCRIPTS = Path(__file__).resolve().parent
ATOM = '{http://www.w3.org/2005/Atom}'
SITE = 'https://example.invalid/capturefab/'
MACHO_ARM64 = b'\xcf\xfa\xed\xfe' + struct.pack('<I', 0x0100000C)


def load(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), SCRIPTS / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


site = load('site-release')
svg = load('cli-svg')
checker = load('test-site')


def published(version, date='2026-10-05', **asset):
    item = dict(name=f'capturefab-{version}-linux-x86_64-headless.tar.gz', os='linux', arch='x86_64', variant='headless', available=True, sha256='a' * 64, size=2097152, url=f'https://example.invalid/v{version}/capturefab.tar.gz', validation='native-smoke-passed', platform_signature='unsigned')
    item.update(asset)
    missing = dict(name=f'capturefab-{version}-linux-armv7-desktop', os='linux', arch='armv7', variant='desktop', available=False, reason='Not built for this release')
    return dict(version=version, date=date, url=f'https://example.invalid/releases/tag/v{version}', checksums_url=f'https://example.invalid/v{version}/SHA256SUMS', assets=[item, missing])


class ReleaseTests(unittest.TestCase):
    def test_version_requires_semver(self):
        self.assertEqual(release.version('v1.2.3-rc.1'), '1.2.3-rc.1')
        for value in ['1.2', '01.2.3', '1.2.3-01', '1.2.3.4']:
            with self.assertRaises(ValueError):
                release.version(value)

    def test_inspect_binary_checks_target_headers(self):
        headers = {
            ('linux', 'aarch64'): b'\x7fELF\x02\x01' + bytes(12) + struct.pack('<H', 183),
            ('linux', 'armv7'): b'\x7fELF\x01\x01' + bytes(12) + struct.pack('<H', 40),
            ('macos', 'x86_64'): b'\xcf\xfa\xed\xfe' + struct.pack('<I', 0x01000007),
            ('windows', 'aarch64'): b'MZ' + bytes(58) + struct.pack('<I', 64) + b'PE\0\0' + struct.pack('<H', 0xAA64),
        }
        with tempfile.TemporaryDirectory() as temp:
            for (os_, arch), header in headers.items():
                path = Path(temp) / f'{os_}-{arch}'
                path.write_bytes(header.ljust(128, b'\0'))
                release.inspect_binary(path, os_, arch)
                with self.assertRaises(ValueError):
                    release.inspect_binary(path, os_, 'x86_64' if arch != 'x86_64' else 'aarch64')

    @staticmethod
    def elf(versions):
        """A 64-bit ELF whose .gnu.version_r needs `versions` from libc.so.6."""
        strings = b'\0libc.so.6\0' + b''.join(v.encode() + b'\0' for v in versions)
        names = [strings.index(v.encode() + b'\0') for v in versions]
        verneed = struct.pack('<HHIII', 1, len(versions), 1, 16, 0)
        for i, name in enumerate(names):
            verneed += struct.pack('<IHHII', 0, 0, i + 2, name, 16 if i + 1 < len(versions) else 0)
        strings_at, verneed_at = 64, 64 + len(strings)
        sections_at = verneed_at + len(verneed)
        header = b'\x7fELF\x02\x01'.ljust(16, b'\0') + struct.pack('<HH', 2, 183)
        header = header.ljust(0x28, b'\0') + struct.pack('<Q', sections_at)
        header = header.ljust(0x3A, b'\0') + struct.pack('<HHH', 64, 3, 0)
        section = lambda kind, offset, size, link, info: struct.pack('<IIQQQQIIQQ', 0, kind, 0, 0, offset, size, link, info, 1, 0)
        sections = bytes(64) + section(3, strings_at, len(strings), 0, 0) + section(0x6FFFFFFE, verneed_at, len(verneed), 1, 1)
        return header.ljust(64, b'\0') + strings + verneed + sections

    @staticmethod
    def macho(minimum, legacy=False):
        """A 64-bit Mach-O with one minimum-version load command."""
        value = minimum[0] << 16 | minimum[1] << 8
        command = struct.pack('<IIII', 0x24, 16, value, value) if legacy else struct.pack('<IIIIII', 0x32, 24, 1, value, value, 0)
        return MACHO_ARM64 + struct.pack('<IIIII', 0, 2, 1, len(command), 0) + bytes(4) + command

    @staticmethod
    def pe(imports, delayed=()):
        """A PE32+ image with one section holding import and delay-import tables."""
        base, data = 0x1000, bytearray(0x400)
        cursor = 0x100
        def put(blob):
            nonlocal cursor
            data[cursor:cursor + len(blob)] = blob
            cursor += len(blob)
            return base + cursor - len(blob)
        names = [put(n.encode() + b'\0') for n in imports]
        delayed_names = [put(n.encode() + b'\0') for n in delayed]
        cursor = (cursor + 15) & ~15
        table = put(b''.join(struct.pack('<IIIII', 0, 0, 0, n, 0) for n in names) + bytes(20))
        delay_table = put(b''.join(struct.pack('<IIIIIIII', 1, n, 0, 0, 0, 0, 0, 0) for n in delayed_names) + bytes(32)) if delayed else 0
        optional = struct.pack('<H', 0x20B).ljust(112, b'\0')
        directories = bytearray(16 * 8)
        directories[8:16] = struct.pack('<II', table, 0)
        directories[13 * 8:13 * 8 + 8] = struct.pack('<II', delay_table, 0)
        optional += bytes(directories)
        section = b'.idata\0\0' + struct.pack('<IIII', len(data), base, len(data), 0x400) + bytes(16)
        headers = b'MZ'.ljust(60, b'\0') + struct.pack('<I', 64) + b'PE\0\0' + struct.pack('<HHIIIHH', 0x8664, 1, 0, 0, 0, len(optional), 0) + optional + section
        return headers.ljust(0x400, b'\0') + bytes(data)

    def test_compatibility_reads_glibc_macos_minimum_and_windows_imports(self):
        self.assertEqual(release.elf_glibc(self.elf(['GLIBC_2.17', 'GLIBC_2.28', 'GLIBC_PRIVATE'])), (2, 28))
        self.assertEqual(release.elf_glibc(self.elf(['GLIBC_2.2.5', 'GLIBC_2.34'])), (2, 34))
        self.assertEqual(release.macho_minimum(self.macho((11, 0))), (11, 0))
        self.assertEqual(release.macho_minimum(self.macho((10, 15), legacy=True)), (10, 15))
        fat = b'\xca\xfe\xba\xbe' + struct.pack('>I', 2)
        slices = [self.macho((10, 15)), self.macho((11, 0))]
        offsets = [64, 64 + len(slices[0])]
        fat += b''.join(struct.pack('>IIIII', 0, 0, o, len(b), 0) for o, b in zip(offsets, slices))
        self.assertEqual(release.macho_minimum(fat.ljust(64, b'\0') + b''.join(slices)), (11, 0))
        self.assertEqual(release.pe_imports(self.pe(['KERNEL32.dll', 'VCRUNTIME140.dll'], ['msvcp140.dll'])), ['KERNEL32.dll', 'VCRUNTIME140.dll', 'msvcp140.dll'])
        with tempfile.TemporaryDirectory() as temp:
            for os_, data in [('linux', b'\x7fELF\x02\x01' + bytes(10)), ('macos', MACHO_ARM64 + struct.pack('<III', 0, 2, 5)), ('windows', b'MZ' + bytes(10))]:
                path = Path(temp) / os_
                path.write_bytes(data)
                with self.assertRaisesRegex(ValueError, 'cannot read'):
                    release.check_compatibility(path, os_, 'x86_64')

    def test_compatibility_rejects_newer_systems_and_vc_runtime(self):
        cases = [
            ('linux', 'x86_64', self.elf(['GLIBC_2.28']), None),
            ('linux', 'aarch64', self.elf(['GLIBC_2.31']), 'glibc 2.31'),
            ('macos', 'aarch64', self.macho((11, 0)), None),
            ('macos', 'x86_64', self.macho((11, 0)), 'macOS'),
            ('windows', 'x86_64', self.pe(['KERNEL32.dll', 'api-ms-win-crt-runtime-l1-1-0.dll']), None),
            ('windows', 'aarch64', self.pe(['KERNEL32.dll'], ['VCRUNTIME140_1.dll']), 'Visual C++'),
        ]
        with tempfile.TemporaryDirectory() as temp:
            for os_, arch, data, error in cases:
                path = Path(temp) / f'{os_}-{arch}'
                path.write_bytes(data)
                if error is None:
                    release.check_compatibility(path, os_, arch)
                else:
                    with self.assertRaisesRegex(ValueError, error):
                        release.check_compatibility(path, os_, arch)

    def test_sbom_lists_linked_crates_with_lock_checksums_and_pinned_media(self):
        metadata = dict(resolve=dict(root='app', nodes=[
            dict(id='app', deps=[dict(pkg='a', dep_kinds=[dict(kind=None)]), dict(pkg='dev', dep_kinds=[dict(kind='dev')])]),
            dict(id='a', deps=[dict(pkg='b', dep_kinds=[dict(kind=None)])]), dict(id='b', deps=[]), dict(id='dev', deps=[])]),
            packages=[dict(id=i, name=i, version='1.0.0', license='MIT' if i != 'b' else None) for i in ['app', 'a', 'b', 'dev']])
        lock = '[[package]]\nname = "a"\nversion = "1.0.0"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "' + 'c' * 64 + '"\n\n[[package]]\nname = "b"\nversion = "1.0.0"\n'
        recipe = 'FFMPEG_VERSION=9.0.2\nFFMPEG_SHA256=' + 'f' * 64 + '\nX264_COMMIT=abc\n'
        bom = release.cyclonedx(metadata, lock, recipe, '1.2.3', 'owner/repo', '00000000-0000-0000-0000-000000000000')
        components = {c['bom-ref']: c for c in bom['components']}
        self.assertEqual(set(components), {'pkg:cargo/a@1.0.0', 'pkg:cargo/b@1.0.0', 'media:ffmpeg', 'media:x264'})
        self.assertEqual(components['pkg:cargo/a@1.0.0']['hashes'], [dict(alg='SHA-256', content='c' * 64)])
        self.assertNotIn('hashes', components['pkg:cargo/b@1.0.0'])
        self.assertNotIn('licenses', components['pkg:cargo/b@1.0.0'])
        self.assertEqual((components['media:x264']['version'], components['media:ffmpeg']['hashes'][0]['content']), ('abc', 'f' * 64))
        self.assertEqual(bom['dependencies'][1], dict(ref='media:ffmpeg', dependsOn=['media:x264']))

    def test_set_version_raises_cargo_toml_and_lock_only_forward(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'Cargo.toml').write_text('[package]\nname = "capturefab"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\nserde = { version = "1" }\n')
            (root / 'Cargo.lock').write_text('[[package]]\nname = "aaa"\nversion = "0.1.0"\n\n[[package]]\nname = "capturefab"\nversion = "0.1.0"\ndependencies = []\n')
            self.assertEqual(release.set_version('v0.2.0-rc.1', root), ('0.1.0', '0.2.0-rc.1'))
            self.assertIn('version = "0.2.0-rc.1"\nedition', (root / 'Cargo.toml').read_text())
            self.assertIn('serde = { version = "1" }', (root / 'Cargo.toml').read_text())
            self.assertIn('name = "aaa"\nversion = "0.1.0"', (root / 'Cargo.lock').read_text())
            self.assertIn('name = "capturefab"\nversion = "0.2.0-rc.1"', (root / 'Cargo.lock').read_text())
            self.assertEqual(release.set_version('0.2.0', root), ('0.2.0-rc.1', '0.2.0'))
            for older in ['0.2.0', '0.2.0-rc.2', '0.1.9', 'banana']:
                with self.assertRaises(ValueError):
                    release.set_version(older, root)

    def test_embeds_requires_the_whole_payload(self):
        payload = os.urandom(70000)
        with tempfile.TemporaryDirectory() as temp:
            ffmpeg, binary, truncated = (Path(temp) / name for name in ('ffmpeg', 'binary', 'truncated'))
            ffmpeg.write_bytes(payload)
            binary.write_bytes(b'head' + payload[:65536] + b'x' + payload + b'tail')
            truncated.write_bytes(b'head' + payload[:-1] + b'tail')
            self.assertTrue(release.embeds(binary, ffmpeg))
            self.assertFalse(release.embeds(truncated, ffmpeg))

    def test_srt_protocols_needs_input_and_output(self):
        self.assertTrue(release.srt_protocols('Input:\n  file\n  srt\nOutput:\n  file\n  srt\n'))
        self.assertFalse(release.srt_protocols('Input:\n  srt\nOutput:\n  file\n'))
        self.assertFalse(release.srt_protocols('Input:\n  srtp\nOutput:\n  srtp\n'))

    def test_license_notices_cover_linked_crates_once_per_text(self):
        with tempfile.TemporaryDirectory() as temp:
            def crate(name, version, license, **files):
                folder = Path(temp) / f'{name}-{version}'
                folder.mkdir()
                for file, text in files.items():
                    (folder / file).write_text(text)
                return dict(id=name, name=name, version=version, license=license, license_file=None, manifest_path=str(folder / 'Cargo.toml'))
            normal, build = [dict(kind=None, target=None)], [dict(kind='build', target=None)]
            metadata = dict(
                packages=[crate('app', '1.0.0', 'GPL-3.0-only'), crate('a', '1.0.0', 'MIT OR Apache-2.0', **{'LICENSE-MIT': 'MIT text', 'LICENSE-APACHE': 'Apache text'}), crate('b', '3.0.0', 'MIT', LICENSE='build-only text'), crate('c', '2.0.0', 'Apache-2.0', **{'LICENSE-APACHE': 'Apache text'}), crate('d', '4.0.0', 'MIT')],
                resolve=dict(root='app', nodes=[dict(id='app', deps=[dict(pkg='a', dep_kinds=normal), dict(pkg='b', dep_kinds=build)]), dict(id='a', deps=[dict(pkg='c', dep_kinds=normal), dict(pkg='d', dep_kinds=normal)]), dict(id='b', deps=[]), dict(id='c', deps=[]), dict(id='d', deps=[])]),
            )
            text = release.license_notices(metadata)
        self.assertEqual(text.count('Apache text'), 1)
        self.assertIn('===== a 1.0.0 (MIT OR Apache-2.0), c 2.0.0 (Apache-2.0) =====', text)
        self.assertIn('MIT text', text)
        self.assertIn('===== d 4.0.0 (MIT) =====\n\nThis crate includes no license file', text)
        self.assertNotIn('build-only text', text)
        self.assertNotIn('app 1.0.0', text)

    @unittest.skipUnless(shutil.which('git'), 'git is required')
    def test_source_archive_is_the_committed_tree(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp) / 'repo'
            (repo / 'assets').mkdir(parents=True)
            (repo / 'assets' / 'Info.plist').write_text('plist')
            (repo / 'Cargo.toml').write_text('[package]')
            git = ['git', '-C', str(repo), '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', '-c', 'commit.gpgsign=false']
            subprocess.run(git + ['init', '-q'], check=True)
            subprocess.run(git + ['add', '.'], check=True)
            subprocess.run(git + ['commit', '-q', '--no-verify', '-m', 'source'], check=True)
            (repo / 'untracked.txt').write_text('local')
            original_run = release.run
            def vendor(args, **kwargs):
                if args[:2] != ['cargo', 'vendor']:
                    return original_run(args, **kwargs)
                folder = Path(args[-1]) / 'dependency-1.0.0'
                folder.mkdir(parents=True)
                (folder / 'lib.rs').write_text('dependency source')
                return subprocess.CompletedProcess(args, 0, stdout=f'[source.vendored-sources]\ndirectory = "{args[-1]}"\n')
            with mock.patch.object(release, 'run', side_effect=vendor):
                with tarfile.open(release.source_archive(Path(temp), '1.2.3', repo)) as archive:
                    names = archive.getnames()
                    self.assertEqual(archive.extractfile('capturefab-1.2.3/vendor/dependency-1.0.0/lib.rs').read(), b'dependency source')
                    self.assertIn(b'directory = "vendor"', archive.extractfile('capturefab-1.2.3/.cargo/vendor-config.toml').read())
                    self.assertIn(b'--offline --locked', archive.extractfile('capturefab-1.2.3/SOURCE-README.txt').read())
            self.assertIn('capturefab-1.2.3/assets/Info.plist', names)
            self.assertNotIn('capturefab-1.2.3/untracked.txt', names)
            (repo / 'Cargo.toml').write_text('[changed]')
            with self.assertRaises(ValueError):
                release.source_archive(Path(temp), '1.2.3', repo)

    @unittest.skipUnless((release.ROOT / '.git').exists(), 'requires a git checkout')
    def test_info_plist_is_tracked_for_source_archives(self):
        subprocess.run(['git', 'ls-files', '--error-unmatch', 'assets/Info.plist'], cwd=release.ROOT, check=True, capture_output=True)

    def test_package_layout_and_app_bundle(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = Path(temp)
            bundle = temp / 'bundle'
            (bundle / 'bin').mkdir(parents=True)
            (bundle / 'licenses').mkdir()
            (bundle / 'licenses' / 'NOTICE.txt').write_text('notice')
            (bundle / 'build-manifest.txt').write_text('manifest')
            ffmpeg = self.macho((11, 0)) + os.urandom(70000)
            (bundle / 'bin' / 'ffmpeg').write_bytes(ffmpeg)
            binary = temp / 'capturefab'
            binary.write_bytes(self.macho((11, 0)) + b'code' + ffmpeg)
            args = SimpleNamespace(os='macos', arch='aarch64', target='aarch64-apple-darwin', version='1.2.3-rc.1', ffmpeg_bundle=bundle, output=temp / 'dist', skip_smoke=True)
            with mock.patch.object(release, 'notices', return_value='crate licenses'), mock.patch.dict(os.environ, {'MACOS_SIGN_IDENTITY': ''}):
                item = release.package(args, binary, 'desktop')
                (bundle / 'bin' / 'ffmpeg').write_bytes(self.macho((11, 0)) + os.urandom(70000))
                with self.assertRaisesRegex(ValueError, 'does not embed'):
                    release.package(args, binary, 'headless')
                # An embedded FFmpeg built for a newer macOS is refused too.
                (bundle / 'bin' / 'ffmpeg').write_bytes(self.macho((13, 0)) + os.urandom(70000))
                with self.assertRaisesRegex(ValueError, 'requires macOS'):
                    release.package(args, binary, 'headless')
            path = temp / 'dist' / item['name']
            self.assertEqual((item['validation'], item['platform_signature'], item['sha256']), ('cross-compiled', 'unsigned', release.digest(path)))
            with zipfile.ZipFile(path) as archive:
                stem = 'capturefab-1.2.3-rc.1-macos-aarch64-desktop'
                info = plistlib.loads(archive.read(f'{stem}/Capturefab.app/Contents/Info.plist'))
                self.assertEqual(archive.read(f'{stem}/THIRD-PARTY-LICENSES.txt'), b'crate licenses')
                self.assertTrue(archive.getinfo(f'{stem}/capturefab').external_attr >> 16 & 0o100)
                self.assertIn(f'{stem}/media-licenses/NOTICE.txt', archive.namelist())
            source = plistlib.loads((release.ROOT / 'assets' / 'Info.plist').read_bytes())
            self.assertEqual(info['CFBundleIdentifier'], source['CFBundleIdentifier'])
            self.assertEqual((info['CFBundleShortVersionString'], info['CFBundlePackageType']), ('1.2.3', 'APPL'))

    def assemble_fixture(self, temp, **options):
        builds = temp / 'builds' / 'build-linux-x86_64'
        builds.mkdir(parents=True)
        asset = builds / 'capturefab-1.2.3-linux-x86_64-headless.tar.gz'
        asset.write_bytes(b'archive')
        media = builds / 'capturefab-1.2.3-linux-x86_64-media-sources.tar.gz'
        media.write_bytes(b'sources')
        item = release.record(asset, 'linux', 'x86_64', 'headless', target='x86_64-unknown-linux-gnu', platform_signature='unsigned', validation='native-smoke-passed')
        failure = dict(os='linux', arch='x86_64', variant='desktop', error='cargo exited with status 101')
        (builds / 'build-linux-x86_64.json').write_text(json.dumps(dict(schema_version=1, version='1.2.3', assets=[item], failures=[failure],
                                                                  media_source=dict(name=media.name, sha256=release.digest(media), size=media.stat().st_size))))

        def source(output, version):
            path = output / f'capturefab-{version}-source.tar.gz'
            path.write_bytes(b'source')
            return path
        args = SimpleNamespace(version='1.2.3', input=temp / 'builds', output=temp / 'release', repository='owner/repo', download_base='https://example.invalid/v1.2.3/', gpg_key=None, provenance=False)
        vars(args).update(options)
        with mock.patch.object(release, 'source_archive', side_effect=source), contextlib.redirect_stdout(io.StringIO()):
            release.assemble(args)
        return asset, args

    def test_assemble_requires_verified_matching_media_source(self):
        for change in ['missing', 'modified', 'traversal', 'wrong-platform', 'unverified']:
            with self.subTest(change=change), tempfile.TemporaryDirectory() as temp:
                _, args = self.assemble_fixture(Path(temp))
                report_path = next(Path(args.input).rglob('build-*.json'))
                report = json.loads(report_path.read_text())
                media = report_path.parent / report['media_source']['name']
                if change == 'missing':
                    media.unlink()
                elif change == 'modified':
                    media.write_bytes(b'different source')
                elif change == 'traversal':
                    report['media_source']['name'] = '../' + media.name
                elif change == 'wrong-platform':
                    wrong = media.with_name(media.name.replace('linux-x86_64', 'macos-aarch64'))
                    media.rename(wrong)
                    report['media_source']['name'] = wrong.name
                else:
                    report['media_source'] = media.name
                report_path.write_text(json.dumps(report))
                args.output = Path(temp) / 'retry'
                with self.assertRaisesRegex(ValueError, 'media source'):
                    release.assemble(args)

    def test_media_sources_require_each_pinned_archive_and_enabled_headers(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = Path(temp)
            (temp / 'scripts').mkdir()
            source = temp / 'bundle' / 'sources'
            source.mkdir(parents=True)
            output = temp / 'dist'
            output.mkdir()
            sources = [('FFMPEG_VERSION', 'FFMPEG_SHA256', 'ffmpeg-1.0.tar.xz'), ('OPENSSL_VERSION', 'OPENSSL_SHA256', 'openssl-1.0.tar.gz'), ('SRT_VERSION', 'SRT_SHA256', 'srt-1.0.tar.gz'), ('X264_COMMIT', 'X264_SHA256', 'x264-1.0.tar.gz'), ('NV_HEADERS_VERSION', 'NV_HEADERS_SHA256', 'nv-codec-headers-1.0.tar.gz'), ('AMF_VERSION', 'AMF_SHA256', 'amf-headers-1.0.tar.gz')]
            declarations = []
            for version_key, hash_key, name in sources:
                archive = source / name
                archive.write_bytes(f'exact {name}'.encode())
                declarations += [f'{version_key}=1.0', f'{hash_key}={release.digest(archive)}']
            (temp / 'scripts/build-ffmpeg.sh').write_text('\n'.join(declarations))
            if os.name != 'nt':
                cached = temp / 'cached-ffmpeg.tar.xz'
                (source / 'ffmpeg-1.0.tar.xz').rename(cached)
                (source / 'ffmpeg-1.0.tar.xz').symlink_to(cached)
            manifest = temp / 'bundle/build-manifest.txt'
            manifest.write_text('FFmpeg 1.0\nOpenSSL 1.0\nSRT 1.0\nx264 1.0\nNVENC/NVDEC headers enabled: 1 (1.0)\nAMF headers enabled: 0 (1.0)\n')
            args = SimpleNamespace(version='1.2.3', os='linux', arch='x86_64', ffmpeg_bundle=temp / 'bundle', output=output)
            with mock.patch.object(release, 'ROOT', temp):
                with tarfile.open(release.media_sources(args)) as archive:
                    names = archive.getnames()
                    self.assertTrue(archive.getmember('media-sources/ffmpeg-1.0.tar.xz').isfile())
                self.assertIn('media-sources/nv-codec-headers-1.0.tar.gz', names)
                self.assertNotIn('media-sources/amf-headers-1.0.tar.gz', names)
                for _, _, name in sources[:-1]:
                    path = source / name
                    original = path.read_bytes()
                    path.write_bytes(b'modified')
                    with self.assertRaisesRegex(ValueError, 'missing or modified'):
                        release.media_sources(args)
                    path.unlink()
                    with self.assertRaisesRegex(ValueError, 'missing or modified'):
                        release.media_sources(args)
                    path.write_bytes(original)
                manifest.write_text('FFmpeg 1.0\nOpenSSL 1.0\nSRT 1.0\nx264 1.0\nFFmpeg configure arguments:\n--disable-autodetect --enable-libx264\n')
                with tarfile.open(release.media_sources(args)) as archive:
                    self.assertNotIn('media-sources/nv-codec-headers-1.0.tar.gz', archive.getnames())
                manifest.write_text(manifest.read_text() + '--enable-nvenc\n')
                with self.assertRaisesRegex(ValueError, 'does not identify'):
                    release.media_sources(args)

    def test_repackage_reuses_each_verified_variant_without_compiling(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            items = []
            for variant in ['desktop', 'headless']:
                stem = f'capturefab-1.2.3-macos-aarch64-{variant}'
                archive_path = output / f'{stem}.zip'
                with zipfile.ZipFile(archive_path, 'w') as archive:
                    archive.writestr(f'{stem}/capturefab', variant.encode())
                items.append(release.record(archive_path, 'macos', 'aarch64', variant))
            report_path = output / 'build-macos-aarch64.json'
            report_path.write_text(json.dumps(dict(version='1.2.3', assets=items)))
            args = SimpleNamespace(version='1.2.3', os='macos', arch='aarch64', output=output)
            def package(args, binary, variant):
                self.assertEqual(binary.read_bytes(), variant.encode())
                self.assertTrue(binary.stat().st_mode & 0o100)
                return variant
            with mock.patch.object(release, 'package', side_effect=package), mock.patch.object(release, 'run', side_effect=AssertionError('repackaging must not compile')):
                self.assertEqual(release.repackage(args, 'desktop'), 'desktop')
                self.assertEqual(release.repackage(args, 'headless'), 'headless')
                (output / items[0]['name']).write_bytes(b'modified')
                with self.assertRaisesRegex(ValueError, 'missing or modified'):
                    release.repackage(args, 'desktop')

    def test_assemble_lists_every_target_and_checksums_every_file(self):
        with tempfile.TemporaryDirectory() as temp:
            asset, args = self.assemble_fixture(Path(temp), provenance=True)
            output = Path(temp) / 'release'
            manifest = json.loads((output / 'releases.json').read_text())
            entry = manifest['releases'][0]
            assets = {(a['os'], a['arch'], a['variant']): a for a in entry['assets']}
            self.assertEqual((manifest['latest'], len(assets)), ('1.2.3', len(release.TARGETS)))
            self.assertEqual(assets[('linux', 'x86_64', 'headless')]['url'], f'https://example.invalid/v1.2.3/{asset.name}')
            self.assertEqual(assets[('linux', 'x86_64', 'desktop')]['reason'], 'cargo exited with status 101')
            self.assertEqual(assets[('macos', 'aarch64', 'desktop')]['reason'], 'Not built for this release')
            self.assertEqual((entry['checksums_url'], entry['source_url']), ('https://example.invalid/v1.2.3/SHA256SUMS', 'https://example.invalid/v1.2.3/capturefab-1.2.3-source.tar.gz'))
            self.assertNotIn('checksums_signature_url', entry)
            sums = {name: digest for digest, name in (line.split('  ') for line in (output / 'SHA256SUMS').read_text().splitlines())}
            self.assertEqual(set(sums), {asset.name, 'capturefab-1.2.3-linux-x86_64-media-sources.tar.gz', 'capturefab-1.2.3-source.tar.gz', 'capturefab-1.2.3-sbom.cdx.json', 'releases.json'})
            self.assertEqual(entry['sbom_url'], 'https://example.invalid/v1.2.3/capturefab-1.2.3-sbom.cdx.json')
            bom = json.loads((output / 'capturefab-1.2.3-sbom.cdx.json').read_text())
            self.assertEqual((bom['bomFormat'], bom['metadata']['component']['purl']), ('CycloneDX', 'pkg:github/owner/repo@v1.2.3'))
            self.assertIn('pkg:cargo/serde', {c.get('purl', '').split('@')[0] for c in bom['components']})
            self.assertEqual(sums['releases.json'], release.digest(output / 'releases.json'))
            notes = (output / 'release-notes.md').read_text()
            self.assertIn('- linux x86_64 desktop: cargo exited with status 101', notes)
            self.assertIn('gh attestation verify FILE --repo owner/repo', notes)
            self.assertIn('--predicate-type https://cyclonedx.org/bom', notes)
            self.assertNotIn('SHA256SUMS.asc', notes)
            with self.assertRaisesRegex(ValueError, 'must be empty'):
                release.assemble(args)
            asset.write_bytes(b'modified')
            args.output = Path(temp) / 'retry'
            with self.assertRaisesRegex(ValueError, 'modified'):
                release.assemble(args)

    @unittest.skipUnless(shutil.which('gpg'), 'gpg is required')
    def test_assemble_signs_checksums_with_the_named_key(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as temp:
            gpg = ['gpg', '--batch', '--homedir', home, '--pinentry-mode', 'loopback', '--passphrase', '']
            subprocess.run(gpg + ['--quick-gen-key', 'Capturefab Test <test@example.invalid>', 'ed25519', 'sign', 'never'], check=True, capture_output=True)
            listing = subprocess.run(['gpg', '--batch', '--homedir', home, '--with-colons', '--list-secret-keys'], check=True, capture_output=True, text=True).stdout
            fingerprint = next(line.split(':')[9] for line in listing.splitlines() if line.startswith('fpr:'))
            with mock.patch.dict(os.environ, {'GNUPGHOME': home, 'RELEASE_GPG_PASSPHRASE': ''}):
                self.assemble_fixture(Path(temp), gpg_key=fingerprint)
                output = Path(temp) / 'release'
                subprocess.run(['gpg', '--batch', '--homedir', home, '--verify', output / 'SHA256SUMS.asc', output / 'SHA256SUMS'], check=True, capture_output=True)
            self.assertIn(f'by key {fingerprint}', (output / 'release-notes.md').read_text())
            self.assertEqual(json.loads((output / 'releases.json').read_text())['releases'][0]['checksums_signature_url'], 'https://example.invalid/v1.2.3/SHA256SUMS.asc')
            self.assertNotIn('SHA256SUMS.asc', (output / 'SHA256SUMS').read_text())
            subprocess.run(['gpgconf', '--homedir', home, '--kill', 'all'], capture_output=True)


class SiteTests(unittest.TestCase):
    def test_empty_manifest_has_an_honest_empty_state(self):
        empty = dict(schema_version=1, latest=None, releases=[])
        page = site.downloads(empty, SITE)
        self.assertIn('No published binaries yet.', page)
        self.assertNotIn('<table', page)
        feed = ET.fromstring(site.feed(empty, SITE))
        self.assertEqual((feed.findtext(f'{ATOM}updated'), feed.findall(f'{ATOM}entry')), (site.FEED_EPOCH, []))
        self.assertEqual(feed.find(f"{ATOM}link[@rel='self']").get('href'), f'{SITE}feed.xml')

    def test_downloads_page_links_assets_and_escapes_text(self):
        manifest = dict(schema_version=1, latest='1.2.3', releases=[published('1.2.3', name='a<b>.tar.gz')])
        manifest['releases'][0]['assets'][1]['reason'] = '<script>alert(1)</script>'
        page = site.downloads(manifest, SITE)
        parser = checker.Page()
        parser.feed(page)
        self.assertEqual(parser.errors, [])
        self.assertIn('v1.2.3', parser.ids)
        self.assertIn('https://example.invalid/v1.2.3/capturefab.tar.gz', parser.links)
        self.assertIn('a&lt;b&gt;.tar.gz</a><br>2.0 MiB', page)
        self.assertNotIn('<script>', page)
        self.assertIn('Smoke-tested on its own platform · Unsigned', page)
        self.assertIn('/ latest', page)

    def test_unsafe_manifest_links_are_rejected(self):
        manifest = dict(schema_version=1, latest='1.2.3', releases=[published('1.2.3', url='javascript:alert(1)')])
        with self.assertRaises(ValueError):
            site.downloads(manifest, SITE)

    def test_feed_orders_entries_by_semver(self):
        manifest = dict(schema_version=1, latest='1.10.0', releases=[published('1.2.0', '2026-01-01'), published('2.0.0-rc.1', '2026-03-01'), published('1.10.0', '2026-02-01')])
        feed = ET.fromstring(site.feed(manifest, SITE))
        self.assertEqual([e.findtext(f'{ATOM}title') for e in feed.findall(f'{ATOM}entry')], ['capturefab 2.0.0-rc.1', 'capturefab 1.10.0', 'capturefab 1.2.0'])
        self.assertEqual(feed.findtext(f'{ATOM}updated'), '2026-03-01T00:00:00Z')
        self.assertEqual(feed.find(f'{ATOM}entry/{ATOM}summary').text, 'Published downloads: Linux x64 headless.')

    def test_precedence_follows_semver(self):
        versions = ['1.0.0-alpha', '1.0.0-alpha.1', '1.0.0-alpha.beta', '1.0.0-beta', '1.0.0-beta.2', '1.0.0-beta.11', '1.0.0-rc.1', '1.0.0']
        self.assertEqual(sorted(reversed(versions), key=site.precedence), versions)

    def test_merge_replaces_versions_and_prefers_stable_latest(self):
        empty = dict(schema_version=1, latest=None, releases=[])
        merged = site.merge(empty, dict(schema_version=1, latest='1.1.0-rc.1', releases=[published('1.1.0-rc.1')]))
        self.assertEqual(merged['latest'], '1.1.0-rc.1')
        merged = site.merge(merged, dict(schema_version=1, latest='1.0.0', releases=[published('1.0.0', '2026-01-01')]))
        self.assertEqual(([r['version'] for r in merged['releases']], merged['latest']), (['1.1.0-rc.1', '1.0.0'], '1.0.0'))
        merged = site.merge(merged, dict(schema_version=1, latest='1.0.0', releases=[published('1.0.0', '2026-02-02')]))
        self.assertEqual([r['date'] for r in merged['releases']], ['2026-10-05', '2026-02-02'])

    def test_check_reports_stale_pages(self):
        with tempfile.TemporaryDirectory() as temp:
            web = Path(temp)
            shutil.copy2(SCRIPTS.parent / 'web' / 'index.html', web)
            (web / 'releases.json').write_text(json.dumps(dict(schema_version=1, latest=None, releases=[])))
            command = [sys.executable, '-B', str(SCRIPTS / 'site-release.py'), '--web', str(web)]
            self.assertEqual(subprocess.run(command + ['--check'], capture_output=True).returncode, 1)
            subprocess.run(command, check=True, capture_output=True)
            self.assertEqual(subprocess.run(command + ['--check'], capture_output=True).returncode, 0)
            update = web / 'update.json'
            update.write_text(json.dumps(dict(schema_version=1, latest='1.2.3', releases=[published('1.2.3')])))
            subprocess.run(command + ['--merge', str(update)], check=True, capture_output=True)
            self.assertEqual(json.loads((web / 'releases.json').read_text())['latest'], '1.2.3')
            self.assertIn('id="v1.2.3"', (web / 'downloads.html').read_text())


class TerminalSvgTests(unittest.TestCase):
    def test_render_escapes_text_and_fits_lines(self):
        lines = ['$ capturefab --camera "a<b>" get Width', 'Width   640 & more', '$ ']
        image = svg.render(lines)
        root = ET.fromstring(image)
        self.assertEqual((root.get('width'), root.get('height')), (str(svg.WIDTH), str(svg.height(lines))))
        self.assertIn('a&lt;b&gt;', image)
        self.assertEqual(len(root.findall('.//{http://www.w3.org/2000/svg}g/{http://www.w3.org/2000/svg}text')), len(lines))
        with self.assertRaises(ValueError):
            svg.render(['x' * (svg.COLUMNS + 1)])

    def test_simulated_only_rejects_real_cameras(self):
        header = 'ID   TRANSPORT  MODEL  SERIAL\n'
        self.assertTrue(svg.simulated_only(header + 'sim:0   simulator  Pattern camera  SIM0\n'))
        self.assertFalse(svg.simulated_only(header + 'sim:0   simulator  Pattern camera  SIM0\n192.168.1.10  gige  a2A1920  12345678\n'))

    @unittest.skipIf(os.name == 'nt', 'uses an executable script as a fake binary')
    def test_transcript_runs_isolated_commands_and_refuses_real_cameras(self):
        with tempfile.TemporaryDirectory() as temp:
            fake = Path(temp) / 'capturefab'
            row = Path(temp) / 'row'
            row.write_text('sim:0  simulator  Pattern camera  SIM0')
            fake.write_text(f'#!{sys.executable}\nimport os, sys\nif "CAPTUREFAB_SESSION" in os.environ or not os.environ.get("CAPTUREFAB_SESSION_DIR"):\n    sys.exit(3)\nprint("ID  TRANSPORT  MODEL  SERIAL\\n" + open({str(row)!r}).read() if "discover" in sys.argv else " ".join(sys.argv[1:]))\n')
            fake.chmod(0o755)
            with mock.patch.dict(os.environ, {'CAPTUREFAB_SESSION': 'gui'}):
                lines = svg.transcript(fake)
            self.assertEqual(lines[:3], ['$ capturefab --simulate discover', 'ID  TRANSPORT  MODEL  SERIAL', 'sim:0  simulator  Pattern camera  SIM0'])
            self.assertEqual(lines[-3:], ['$ capturefab --camera sim:0 capture -o frame.png', '--camera sim:0 capture -o frame.png', '$ '])
            row.write_text('10.0.0.2  gige  Camera  1234')
            with self.assertRaises(SystemExit):
                svg.transcript(fake)


class SiteValidatorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.web = Path(self.temp.name) / 'web'
        shutil.copytree(SCRIPTS.parent / 'web', self.web)

    def tearDown(self):
        self.temp.cleanup()

    def errors(self):
        return '\n'.join(checker.validate(self.web)[0])

    def test_repository_site_is_valid(self):
        self.assertEqual(self.errors(), '')

    def test_detects_image_size_mismatch(self):
        index = self.web / 'index.html'
        index.write_text(index.read_text().replace('width="1200"', 'width="1201"'))
        self.assertIn('images/cli.svg is 1200x', self.errors())

    def test_detects_stale_pages_and_feed(self):
        (self.web / 'releases.json').write_text(json.dumps(dict(schema_version=1, latest='1.2.3', releases=[published('1.2.3')])))
        errors = self.errors()
        self.assertIn('downloads.html is stale', errors)
        self.assertIn('feed.xml: 0 entries for 1 releases', errors)

    def test_detects_missing_anchor_and_active_svg(self):
        downloads = self.web / 'downloads.html'
        downloads.write_text(downloads.read_text().replace('href="index.html#download"', 'href="index.html#missing"'))
        image = self.web / 'images' / 'cli.svg'
        image.write_text(image.read_text().replace('</svg>', '<script>alert(1)</script></svg>'))
        errors = self.errors()
        self.assertIn('downloads.html: missing anchor: index.html#missing', errors)
        self.assertIn('cli.svg: active or remote SVG content in <script>', errors)


if __name__ == '__main__':
    unittest.main()
