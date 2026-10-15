#!/usr/bin/env python3
"""Sign native-tested Windows artifacts on an x64 runner without rebuilding."""
import argparse
import copy
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import struct
import tempfile
import zipfile

import release

ARCHES = ('x86_64', 'aarch64')
VARIANTS = ('desktop', 'headless')


def empty_output(path):
    path = Path(path)
    if path.exists() and (not path.is_dir() or any(path.iterdir())):
        raise ValueError(f'{path} must be empty')
    return path


def read_package(path, ver, arch, variant):
    """Read only named members; never extract an artifact-controlled path."""
    stem = f'capturefab-{ver}-windows-{arch}-{variant}'
    binary_name, readme_name = f'{stem}/capturefab.exe', f'{stem}/README.txt'
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        if len(names) != len(set(names)):
            raise ValueError(f'duplicate ZIP member in {path.name}')
        for name in names:
            parts = PurePosixPath(name).parts
            if (not name.startswith(stem + '/') or '\\' in name
                    or '..' in parts or name.startswith('/')
                    or (name.lower().endswith('.exe') and name != binary_name)):
                raise ValueError(f'unexpected ZIP member in {path.name}: {name}')
        if binary_name not in names or readme_name not in names:
            raise ValueError(f'executable or README missing from {path.name}')
        binary, readme = archive.read(binary_name), archive.read(readme_name)
        if readme.decode('utf-8').splitlines()[:2] != [f'Capturefab {ver}', f'windows/{arch}; {variant}']:
            raise ValueError(f'README has the wrong release identity: {path.name}')
        if readme.count(b'Platform signature: unsigned.') != 1:
            raise ValueError(f'README must declare an unsigned executable: {path.name}')
        return binary, binary_name, readme_name


def builds(ver, root):
    """Require every native-validated Windows variant and verified source."""
    root = Path(root)
    reports = sorted(root.rglob('build-*.json'))
    expected = {f'build-windows-{arch}.json' for arch in ARCHES}
    if len(reports) != len(expected) or {path.name for path in reports} != expected:
        raise ValueError('exactly the x86_64 and aarch64 Windows build reports are required')
    result = []
    for report_path in reports:
        report = json.loads(report_path.read_text(encoding='utf-8'))
        arch = next(arch for arch in ARCHES if report_path.name == f'build-windows-{arch}.json')
        target = f'{arch}-pc-windows-msvc'
        if (not isinstance(report, dict) or report.get('schema_version') != 1
                or report.get('version') != ver or report.get('target') != target
                or report.get('failures') != []):
            raise ValueError(f'invalid or failed Windows build report: {report_path.name}')
        assets = report.get('assets')
        if not isinstance(assets, list) or len(assets) != len(VARIANTS):
            raise ValueError(f'both Windows variants are required: {report_path.name}')
        seen, packages = set(), []
        for item in assets:
            if not isinstance(item, dict):
                raise ValueError(f'invalid Windows asset: {report_path.name}')
            variant = item.get('variant')
            if (variant not in VARIANTS or variant in seen
                    or item.get('os') != 'windows' or item.get('arch') != arch
                    or item.get('target') != target or item.get('available') is not True
                    or item.get('validation') != 'native-smoke-passed'
                    or item.get('platform_signature') != 'unsigned'
                    or item.get('name') != f'capturefab-{ver}-windows-{arch}-{variant}.zip'):
                raise ValueError(f'Windows asset is not a native-tested unsigned {arch} variant')
            seen.add(variant)
            path = release.verified_file(report_path.parent, item, 'Windows package')
            binary, binary_name, readme_name = read_package(path, ver, arch, variant)
            packages.append(dict(item=item, path=path, binary=binary,
                                 binary_name=binary_name, readme_name=readme_name,
                                 signed_name=f'capturefab-{arch}-{variant}.exe'))
        media = report.get('media_source')
        media_path = release.verified_file(report_path.parent, media, 'corresponding media source')
        if media['name'] != f'capturefab-{ver}-windows-{arch}-media-sources.tar.gz':
            raise ValueError('corresponding media source has the wrong Windows platform')
        result.append(dict(path=report_path, report=report, arch=arch,
                           packages=packages, media_path=media_path))
    return result


def pe_signature_fields(data):
    """Return the two PE fields Authenticode may change, with strict bounds."""
    try:
        if data[:2] != b'MZ':
            raise ValueError('expected a PE executable')
        pe = struct.unpack_from('<I', data, 60)[0]
        if data[pe:pe + 4] != b'PE\0\0':
            raise ValueError('invalid PE executable')
        optional_size = struct.unpack_from('<H', data, pe + 20)[0]
        optional = pe + 24
        magic = struct.unpack_from('<H', data, optional)[0]
        if magic not in (0x10B, 0x20B):
            raise ValueError('invalid PE optional header')
        directories = optional + (112 if magic == 0x20B else 96)
        security = directories + 4 * 8
        if (optional_size < security + 8 - optional
                or optional + optional_size > len(data)
                or struct.unpack_from('<I', data, directories - 4)[0] < 5):
            raise ValueError('PE certificate directory is missing')
        return optional + 64, security, struct.unpack_from('<II', data, security)
    except struct.error:
        raise ValueError('truncated PE executable') from None


def same_tested_binary(original, signed):
    """Permit only a checksum, certificate directory and appended signature.

    Authenticode excludes these fields from its digest. Comparing everything
    else ensures the signed file still contains the exact native-tested code,
    including its embedded FFmpeg, without executing an ARM binary on x64.
    Layout: https://learn.microsoft.com/windows/win32/debug/pe-format
    """
    checksum, security, original_certificate = pe_signature_fields(original)
    signed_checksum, signed_security, (offset, size) = pe_signature_fields(signed)
    if original_certificate != (0, 0):
        raise ValueError('native-tested executable already has a certificate')
    if (checksum, security) != (signed_checksum, signed_security):
        raise ValueError('signing changed the PE header layout')
    if (offset != (len(original) + 7) // 8 * 8 or size < 8
            or offset + size != len(signed)
            or any(signed[len(original):offset])):
        raise ValueError('signed executable must append an aligned Authenticode certificate')
    before, after = bytearray(original), bytearray(signed[:len(original)])
    for start, width in ((checksum, 4), (security, 8)):
        before[start:start + width] = after[start:start + width] = bytes(width)
    if before != after:
        raise ValueError('signing changed native-tested executable bytes')


def publish_directory(staged, output):
    # Keep partial results out of the directory consumed by artifact upload.
    empty_output(output)
    if output.exists():
        output.rmdir()
    staged.rename(output)


def prepare(args):
    ver = release.version(args.version)
    output = empty_output(args.output)
    validated = builds(ver, args.input)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='capturefab-signing-', dir=output.parent) as temp:
        stage = Path(temp) / 'executables'
        stage.mkdir()
        for build in validated:
            for package in build['packages']:
                path = stage / package['signed_name']
                path.write_bytes(package['binary'])
                release.inspect_binary(path, 'windows', build['arch'])
                release.check_compatibility(path, 'windows', build['arch'])
                if pe_signature_fields(package['binary'])[2] != (0, 0):
                    raise ValueError('native-tested executable already has a certificate')
        publish_directory(stage, output)


def finish(args):
    ver = release.version(args.version)
    output = empty_output(args.output)
    validated = builds(ver, args.input)
    signed_root = Path(args.signed)
    expected = {p['signed_name'] for b in validated for p in b['packages']}
    if (not signed_root.is_dir()
            or {p.name for p in signed_root.iterdir()} != expected
            or any(not (signed_root / name).is_file() for name in expected)):
        raise ValueError('exactly four signed Windows executables are required')
    # Check every signature before creating any output archives or reports.
    for build in validated:
        for package in build['packages']:
            path = signed_root / package['signed_name']
            signed = path.read_bytes()
            same_tested_binary(package['binary'], signed)
            release.inspect_binary(path, 'windows', build['arch'])
            release.check_compatibility(path, 'windows', build['arch'])
            release.run([os.environ.get('WINDOWS_SIGNTOOL', 'signtool.exe'),
                         'verify', '/pa', '/all', '/tw', path])
            package['signed'] = signed
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='capturefab-signed-', dir=output.parent) as temp:
        stage = Path(temp) / 'artifacts'
        stage.mkdir()
        for build in validated:
            report = copy.deepcopy(build['report'])
            for package, item in zip(build['packages'], report['assets']):
                target = stage / package['path'].name
                with zipfile.ZipFile(package['path']) as source, zipfile.ZipFile(target, 'w') as archive:
                    archive.comment = source.comment
                    for info in source.infolist():
                        data = source.read(info)
                        if info.filename == package['binary_name']:
                            data = package['signed']
                        elif info.filename == package['readme_name']:
                            data = data.replace(b'Platform signature: unsigned.', b'Platform signature: authenticode.')
                        archive.writestr(copy.copy(info), data)
                item.update(sha256=release.digest(target), size=target.stat().st_size,
                            platform_signature='authenticode')
            shutil.copy2(build['media_path'], stage / build['media_path'].name)
            (stage / build['path'].name).write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
        publish_directory(stage, output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    for name in ('prepare', 'finish'):
        command = commands.add_parser(name)
        command.add_argument('--version', required=True)
        command.add_argument('--input', required=True, type=Path)
        command.add_argument('--output', required=True, type=Path)
        if name == 'finish':
            command.add_argument('--signed', required=True, type=Path)
    args = parser.parse_args()
    try:
        {'prepare': prepare, 'finish': finish}[args.command](args)
    except (ValueError, RuntimeError, OSError, zipfile.BadZipFile) as error:
        parser.exit(1, f'{error}\n')


if __name__ == '__main__':
    main()
