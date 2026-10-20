#!/usr/bin/env python3
"""The signing pass must preserve exactly the binaries validated natively."""
import importlib.util
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock
import zipfile

import release

spec = importlib.util.spec_from_file_location('sign_windows', Path(__file__).with_name('sign-windows.py'))
sign_windows = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sign_windows)


def executable(arch):
    """A PE32+ with a native target header and an empty import directory."""
    data = bytearray(1024)
    data[:2] = b'MZ'
    struct.pack_into('<I', data, 60, 128)
    data[128:132] = b'PE\0\0'
    struct.pack_into('<HH', data, 132, {'x86_64': 0x8664, 'aarch64': 0xAA64}[arch], 1)
    struct.pack_into('<H', data, 148, 240)
    struct.pack_into('<H', data, 152, 0x20B)
    struct.pack_into('<I', data, 152 + 108, 16)
    # Section virtual size/address/raw size/offset; no imported DLLs.
    struct.pack_into('<IIII', data, 152 + 240 + 8, 512, 4096, 512, 512)
    data[512:520] = b'CODETEST'
    return bytes(data) + f'{arch}/embedded-ffmpeg'.encode()


def signed_binary(data):
    signed = bytearray(data)
    checksum, security, _ = sign_windows.pe_signature_fields(data)
    signed += bytes((-len(signed)) % 8)
    certificate = struct.pack('<IHH', 24, 0x200, 2) + b'signed-test-data'
    struct.pack_into('<I', signed, checksum, 12345)
    struct.pack_into('<II', signed, security, len(signed), len(certificate))
    return bytes(signed) + certificate


class WindowsSigningTests(unittest.TestCase):
    def fixture(self, root):
        args = SimpleNamespace(version='1.2.3', input=root / 'builds',
                               output=root / 'staging', signed=root / 'staging')
        originals = {}
        for arch in sign_windows.ARCHES:
            folder = args.input / f'build-windows-{arch}'
            folder.mkdir(parents=True)
            assets = []
            stem = f'capturefab-1.2.3-windows-{arch}'
            path = folder / f'{stem}.zip'
            binary = executable(arch)
            members = {
                f'{stem}/capturefab.exe': binary,
                f'{stem}/README.txt': (f'Capturefab 1.2.3\r\nwindows/{arch}\r\n'
                                     'Platform signature: unsigned. Verify SHA256SUMS.\r\n').encode(),
                f'{stem}/LICENSE': b'capturefab license',
                f'{stem}/THIRD-PARTY-LICENSES.txt': b'crate licenses',
                f'{stem}/media-licenses/FFmpeg.txt': b'FFmpeg license',
                f'{stem}/media-build-manifest.txt': b'native media metadata',
            }
            with zipfile.ZipFile(path, 'w', zipfile.ZIP_DEFLATED) as archive:
                archive.comment = b'original archive comment'
                for name, data in members.items():
                    info = zipfile.ZipInfo(name, date_time=(2026, 10, 15, 12, 0, 0))
                    info.external_attr = 0o100755 << 16
                    info.comment = b'original member comment'
                    info.compress_type = zipfile.ZIP_DEFLATED
                    archive.writestr(info, data)
            originals[arch] = (path, members)
            assets.append(release.record(path, 'windows', arch,
                                         target=f'{arch}-pc-windows-msvc',
                                         platform_signature='unsigned',
                                         validation='native-smoke-passed',
                                         native_metadata='retain this'))
            media = folder / f'capturefab-1.2.3-windows-{arch}-media-sources.tar.gz'
            media.write_bytes(b'verified native media source ' + arch.encode())
            report = dict(schema_version=1, version='1.2.3', target=f'{arch}-pc-windows-msvc',
                          assets=assets, failures=[], native_report_metadata='retain this too',
                          media_source=dict(name=media.name, sha256=release.digest(media), size=media.stat().st_size))
            (folder / f'build-windows-{arch}.json').write_text(json.dumps(report))
        return args, originals

    def prepare_signed(self, args):
        sign_windows.prepare(args)
        for path in args.output.iterdir():
            path.write_bytes(signed_binary(path.read_bytes()))
        args.output = args.output.parent / 'signed-output'

    def test_both_architectures_preserve_native_artifacts(self):
        with tempfile.TemporaryDirectory() as temp:
            args, originals = self.fixture(Path(temp))
            self.prepare_signed(args)
            calls = []
            def verify(command):
                calls.append(command)
                self.assertEqual(command[:5], ['custom-signtool', 'verify', '/pa', '/all', '/tw'])
                self.assertEqual(len(command), 6)
            with mock.patch.dict(os.environ, WINDOWS_SIGNTOOL='custom-signtool'), mock.patch.object(release, 'run', side_effect=verify):
                sign_windows.finish(args)
            self.assertEqual(len(calls), 2)
            self.assertEqual(len(list(args.output.iterdir())), 6)
            for arch in sign_windows.ARCHES:
                source_folder = args.input / f'build-windows-{arch}'
                report_name = f'build-windows-{arch}.json'
                before = json.loads((source_folder / report_name).read_text())
                after = json.loads((args.output / report_name).read_text())
                self.assertEqual(after['native_report_metadata'], before['native_report_metadata'])
                self.assertEqual(after['media_source'], before['media_source'])
                self.assertEqual((args.output / after['media_source']['name']).read_bytes(),
                                 (source_folder / before['media_source']['name']).read_bytes())
                self.assertEqual(after['failures'], [])
                for item in after['assets']:
                    path, members = originals[arch]
                    output = release.verified_file(args.output, item, 'signed output')
                    self.assertEqual(item['validation'], 'native-smoke-passed')
                    self.assertEqual(item['platform_signature'], 'authenticode')
                    self.assertEqual(item['native_metadata'], 'retain this')
                    self.assertNotEqual(item['sha256'], release.digest(path))
                    with zipfile.ZipFile(path) as original, zipfile.ZipFile(output) as archive:
                        self.assertEqual(archive.namelist(), original.namelist())
                        self.assertEqual(archive.comment, original.comment)
                        for info, previous in zip(archive.infolist(), original.infolist()):
                            self.assertEqual((info.external_attr, info.comment, info.date_time, info.compress_type),
                                             (previous.external_attr, previous.comment, previous.date_time, previous.compress_type))
                            expected = members[info.filename]
                            if info.filename.endswith('/capturefab.exe'):
                                expected = signed_binary(expected)
                            elif info.filename.endswith('/README.txt'):
                                expected = expected.replace(b'Platform signature: unsigned.', b'Platform signature: authenticode.')
                            self.assertEqual(archive.read(info), expected)

    def test_prepare_rejects_missing_duplicate_or_invalid_builds(self):
        for mutation in ('missing-report', 'duplicate-report', 'version', 'schema', 'target', 'failures',
                         'missing-package', 'wrong-platform', 'cross-compiled',
                         'already-signed', 'wrong-asset-target', 'wrong-media', 'tampered-media',
                         'missing-media', 'tampered-package', 'unsafe-package-name'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temp:
                args, _ = self.fixture(Path(temp))
                path = next(args.input.rglob('build-windows-x86_64.json'))
                report = json.loads(path.read_text())
                if mutation == 'missing-report':
                    path.unlink()
                elif mutation == 'duplicate-report':
                    (args.input / path.name).write_text(path.read_text())
                elif mutation in ('version', 'schema', 'target', 'failures'):
                    report[{'schema': 'schema_version'}.get(mutation, mutation)] = {'version': '2.0.0', 'schema': 2,
                                                                                  'target': 'aarch64-pc-windows-msvc',
                                                                                  'failures': [{'error': 'build failed'}]}[mutation]
                elif mutation == 'missing-package':
                    report['assets'].pop()
                elif mutation == 'wrong-platform':
                    report['assets'][0]['os'] = 'linux'
                elif mutation == 'cross-compiled':
                    report['assets'][0]['validation'] = 'cross-compiled'
                elif mutation == 'already-signed':
                    report['assets'][0]['platform_signature'] = 'authenticode'
                elif mutation == 'wrong-asset-target':
                    report['assets'][0]['target'] = 'aarch64-pc-windows-msvc'
                elif mutation == 'wrong-media':
                    report['media_source']['name'] = '../wrong.tar.gz'
                elif mutation == 'tampered-media':
                    (path.parent / report['media_source']['name']).write_bytes(b'tampered')
                elif mutation == 'missing-media':
                    del report['media_source']
                elif mutation == 'tampered-package':
                    (path.parent / report['assets'][0]['name']).write_bytes(b'tampered')
                elif mutation == 'unsafe-package-name':
                    report['assets'][0]['name'] = '../outside.zip'
                if mutation not in ('missing-report', 'duplicate-report'):
                    path.write_text(json.dumps(report))
                with self.assertRaises(ValueError), mock.patch.object(release, 'run', side_effect=AssertionError('must not compile or execute')):
                    sign_windows.prepare(args)
                self.assertFalse(args.output.exists())

    def test_finish_revalidates_original_archives_and_sources(self):
        for mutation in ('package', 'media'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temp:
                args, _ = self.fixture(Path(temp))
                self.prepare_signed(args)
                pattern = '*.zip' if mutation == 'package' else '*-media-sources.tar.gz'
                next(args.input.rglob(pattern)).write_bytes(b'tampered after staging')
                with self.assertRaisesRegex(ValueError, 'missing or modified'), mock.patch.object(release, 'run') as run:
                    sign_windows.finish(args)
                run.assert_not_called()
                self.assertFalse(args.output.exists())

    def test_finish_rejects_missing_extra_wrong_arch_and_changed_code(self):
        for mutation in ('missing', 'extra', 'wrong-arch', 'changed-code', 'unsigned', 'extra-overlay'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temp:
                args, _ = self.fixture(Path(temp))
                self.prepare_signed(args)
                path = args.signed / 'capturefab-aarch64.exe'
                if mutation == 'missing':
                    path.unlink()
                elif mutation == 'extra':
                    (args.signed / 'unexpected.exe').write_bytes(b'wrong')
                elif mutation == 'wrong-arch':
                    path.write_bytes((args.signed / 'capturefab-x86_64.exe').read_bytes())
                elif mutation == 'changed-code':
                    data = bytearray(path.read_bytes())
                    data[512] ^= 1
                    path.write_bytes(data)
                elif mutation == 'unsigned':
                    path.write_bytes(executable('aarch64'))
                else:
                    path.write_bytes(path.read_bytes() + b'additional unverified bytes')
                with self.assertRaises(ValueError), mock.patch.object(release, 'run'):
                    sign_windows.finish(args)
                self.assertFalse(args.output.exists())

    def test_signature_error_or_timestamp_warning_leaves_no_output(self):
        # signtool returns 1 for errors and 2 for warnings, including /tw's
        # warning when a signature has no timestamp. release.run rejects both.
        for status in (1, 2):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as temp:
                args, _ = self.fixture(Path(temp))
                self.prepare_signed(args)
                def verify(command, **kwargs):
                    self.assertEqual(command[1:5], ['verify', '/pa', '/all', '/tw'])
                    return subprocess.CompletedProcess(command, status)
                with mock.patch.object(subprocess, 'run', side_effect=verify):
                    with self.assertRaisesRegex(RuntimeError, f'exited with status {status}'):
                        sign_windows.finish(args)
                self.assertFalse(args.output.exists())

    def test_last_signature_must_pass_before_any_output_is_created(self):
        with tempfile.TemporaryDirectory() as temp:
            args, _ = self.fixture(Path(temp))
            self.prepare_signed(args)
            calls = []
            def verify(command):
                self.assertFalse(args.output.exists())
                calls.append(command)
                if len(calls) == 2:
                    raise RuntimeError('last signature failed')
            with mock.patch.object(release, 'run', side_effect=verify):
                with self.assertRaisesRegex(RuntimeError, 'last signature failed'):
                    sign_windows.finish(args)
            self.assertEqual(len(calls), 2)
            self.assertFalse(args.output.exists())

    def test_only_authenticode_pe_mutations_are_allowed(self):
        original = executable('aarch64') + b'!!'
        valid = signed_binary(original)
        sign_windows.same_tested_binary(original, valid)
        for mutation in ('nonzero-padding', 'changed-checksum-neighbor', 'changed-header', 'bad-directory', 'truncated'):
            with self.subTest(mutation=mutation):
                changed = bytearray(valid)
                checksum, security, (offset, size) = sign_windows.pe_signature_fields(changed)
                if mutation == 'nonzero-padding':
                    # This fixture deliberately has an unaligned native file.
                    self.assertLess(len(original), offset)
                    changed[len(original)] = 1
                elif mutation == 'changed-checksum-neighbor':
                    changed[checksum - 1] ^= 1
                elif mutation == 'changed-header':
                    changed[132] ^= 1
                elif mutation == 'bad-directory':
                    struct.pack_into('<II', changed, security, offset + 1, size - 1)
                else:
                    changed = changed[:100]
                with self.assertRaises(ValueError):
                    sign_windows.same_tested_binary(original, changed)

    def test_outputs_must_be_empty(self):
        with tempfile.TemporaryDirectory() as temp:
            args, _ = self.fixture(Path(temp))
            args.output.mkdir()
            marker = args.output / 'preserve.txt'
            marker.write_text('existing work')
            with self.assertRaisesRegex(ValueError, 'must be empty'):
                sign_windows.prepare(args)
            self.assertEqual(marker.read_text(), 'existing work')

    def test_archive_paths_never_extracted_and_unexpected_members_fail(self):
        for unexpected in ('../escape.exe', '/absolute.txt', 'wrong-prefix/file.txt',
                           'media-licenses/../../escape.txt', 'extra.exe', 'duplicate'):
            with self.subTest(unexpected=unexpected), tempfile.TemporaryDirectory() as temp:
                args, originals = self.fixture(Path(temp))
                path, members = originals['x86_64']
                stem = path.stem
                if unexpected == 'duplicate':
                    # Build a duplicate without zipfile's intentional warning.
                    name = f'{stem}/README.txt'
                    with mock.patch('warnings.warn'):
                        with zipfile.ZipFile(path, 'a') as archive:
                            archive.writestr(name, members[name])
                else:
                    name = unexpected if '/' in unexpected and not unexpected.startswith('media-licenses/') else f'{stem}/{unexpected}'
                    with zipfile.ZipFile(path, 'a') as archive:
                        archive.writestr(name, b'unsafe')
                report_path = path.parent / 'build-windows-x86_64.json'
                report = json.loads(report_path.read_text())
                report['assets'][0].update(sha256=release.digest(path), size=path.stat().st_size)
                report_path.write_text(json.dumps(report))
                with self.assertRaisesRegex(ValueError, 'ZIP member'), mock.patch.object(zipfile.ZipFile, 'extractall', side_effect=AssertionError('must not extract')):
                    sign_windows.prepare(args)
                self.assertFalse(args.output.exists())


if __name__ == '__main__':
    unittest.main()
