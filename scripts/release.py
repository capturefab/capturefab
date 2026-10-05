#!/usr/bin/env python3
"""Build/package real binaries and assemble an honest, checksummed release."""
import argparse
import datetime as dt
import hashlib
import io
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
SEMVER = re.compile(r'(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?')
TARGETS = [(os_, arch, variant) for os_, arches in [('linux', ['x86_64', 'aarch64', 'armv7']), ('macos', ['x86_64', 'aarch64']), ('windows', ['x86_64', 'aarch64'])] for arch in arches for variant in ['desktop', 'headless']]
FEATURES = {'desktop': ['--features', 'wgpu'], 'headless': ['--no-default-features', '--features', 'usb,jpeg,nvjpeg']}
LICENSE_FILE = re.compile(r'(licen[cs]e|copying|notice|unlicense)', re.I)

def version(value=None):
    value = value or re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / 'Cargo.toml').read_text(), re.M)[1]
    value = value[1:] if value.startswith('v') else value
    match = SEMVER.fullmatch(value)
    if not match or (match[4] and any(x.isdigit() and len(x) > 1 and x[0] == '0' for x in match[4].split('.'))):
        raise ValueError('version must follow Semantic Versioning, such as 0.1.0 or 0.2.0-rc.1')
    return value

def digest(path):
    result = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()

def run(args, **kwargs):
    result = subprocess.run([str(x) for x in args], **kwargs)
    if result.returncode:
        # Do not echo arguments: signing tools can receive private passwords.
        raise RuntimeError(f'{Path(str(args[0])).name} exited with status {result.returncode}')
    return result

def inspect_binary(path, os_, arch):
    with Path(path).open('rb') as stream:
        header = stream.read(4096)
        if header.startswith(b'\x7fELF'):
            endian = '<' if header[5] == 1 else '>'
            kind, machine = 'linux', struct.unpack_from(endian + 'H', header, 18)[0]
            found = {62: 'x86_64', 183: 'aarch64', 40: 'armv7'}.get(machine)
        elif header.startswith(b'MZ'):
            stream.seek(struct.unpack_from('<I', header, 60)[0])
            pe = stream.read(6)
            if pe[:4] != b'PE\0\0':
                raise ValueError('invalid PE executable')
            kind, found = 'windows', {0x8664: 'x86_64', 0xAA64: 'aarch64'}.get(struct.unpack_from('<H', pe, 4)[0])
        elif header[:4] in [b'\xcf\xfa\xed\xfe', b'\xfe\xed\xfa\xcf']:
            endian = '<' if header[0] == 0xCF else '>'
            kind, found = 'macos', {0x01000007: 'x86_64', 0x0100000C: 'aarch64'}.get(struct.unpack_from(endian + 'I', header, 4)[0])
        else:
            raise ValueError('expected a target-specific ELF, PE, or Mach-O executable')
    if (kind, found) != (os_, arch):
        raise ValueError(f'wrong executable target: {kind}/{found}, expected {os_}/{arch}')

def embeds(binary, payload):
    data, needle = Path(binary).read_bytes(), Path(payload).read_bytes()
    start = data.find(needle[:65536])
    while start >= 0 and data[start:start + len(needle)] != needle:
        start = data.find(needle[:65536], start + 1)
    return start >= 0

def archive_tree(source, output, prefix=None):
    if output.suffix == '.zip':
        with zipfile.ZipFile(output, 'w', zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for path in sorted(source.rglob('*')):
                if path.is_file():
                    name = str(path.relative_to(source))
                    archive.write(path, f'{prefix}/{name}' if prefix else name)
    else:
        with tarfile.open(output, 'w:gz', format=tarfile.PAX_FORMAT) as archive:
            archive.add(source, arcname=prefix or source.name)

def source_archive(output, ver, root=ROOT):
    if run(['git', 'status', '--porcelain', '--untracked-files=no'], cwd=root, capture_output=True, text=True).stdout:
        raise ValueError('commit or discard tracked changes first: the source archive must match the built commit')
    path = output / f'capturefab-{ver}-source.tar.gz'
    prefix = f'capturefab-{ver}'
    with tempfile.TemporaryDirectory(prefix='capturefab-corresponding-source-') as temp:
        temp = Path(temp)
        committed = temp / 'committed.tar'
        run(['git', 'archive', '--format=tar', f'--prefix={prefix}/', '--output', committed, 'HEAD'], cwd=root)
        vendor = temp / 'vendor'
        config = run(['cargo', 'vendor', '--locked', '--versioned-dirs', vendor], cwd=root, capture_output=True, text=True).stdout
        config = re.sub(r'^directory = .+$', 'directory = "vendor"', config, flags=re.M)
        readme = ('This archive contains the committed Capturefab tree and the exact Cargo.lock dependency sources in vendor/.\n'
                  'Use the vendored configuration to build without contacting a package registry:\n'
                  '  cargo --config .cargo/vendor-config.toml build --offline --locked\n'
                  'Release media sources and their build recipe are in the matching platform media-sources archive.\n')
        with tarfile.open(committed) as source, tarfile.open(path, 'w:gz', format=tarfile.PAX_FORMAT) as archive:
            for member in source.getmembers():
                archive.addfile(member, source.extractfile(member) if member.isfile() else None)
            archive.add(vendor, arcname=f'{prefix}/vendor')
            for name, text in [('.cargo/vendor-config.toml', config), ('SOURCE-README.txt', readme)]:
                if f'{prefix}/{name}' in source.getnames():
                    raise ValueError(f'generated corresponding-source file conflicts with committed {name}')
                data = text.encode('utf-8')
                member = tarfile.TarInfo(f'{prefix}/{name}')
                member.size, member.mode = len(data), 0o644
                archive.addfile(member, io.BytesIO(data))
    return path

def license_notices(metadata):
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    packages = {package['id']: package for package in metadata['packages']}
    linked, pending = set(), [metadata['resolve']['root']]
    while pending:
        for dep in nodes[pending.pop()]['deps']:
            if dep['pkg'] not in linked and any(kind['kind'] is None for kind in dep['dep_kinds']):
                linked.add(dep['pkg'])
                pending.append(dep['pkg'])
    texts = {}
    for package in sorted((packages[i] for i in linked), key=lambda p: (p['name'], p['version'])):
        folder = Path(package['manifest_path']).parent
        files = sorted(p for p in folder.iterdir() if p.is_file() and LICENSE_FILE.match(p.name))
        files += [folder / package['license_file']] if package.get('license_file') else []
        found = dict.fromkeys(f.read_text(encoding='utf-8', errors='replace').strip() for f in files)
        for text in found or ['This crate includes no license file; its license expression is listed above.']:
            texts.setdefault(text, []).append(f"{package['name']} {package['version']} ({package['license'] or 'see license file'})")
    return 'Rust crates compiled into this Capturefab executable and their license texts.\n' + ''.join(f'\n===== {", ".join(names)} =====\n\n{text}\n' for text, names in texts.items())

def notices(target, variant):
    metadata = run(['cargo', 'metadata', '--format-version', '1', '--locked', '--filter-platform', target, *FEATURES[variant]], cwd=ROOT, capture_output=True, text=True).stdout
    return license_notices(json.loads(metadata))

def link(base, name):
    return f'{base.rstrip("/")}/{name}' if base else name

def record(path, os_, arch, variant, **extra):
    return dict(name=path.name, os=os_, arch=arch, variant=variant, available=True,
                sha256=digest(path), size=path.stat().st_size, **extra)

def verified_file(parent, item, label):
    if not isinstance(item, dict) or not isinstance(item.get('name'), str) or Path(item['name']).name != item['name'] or any(c in item['name'] for c in '/\\'):
        raise ValueError(f'invalid {label} path')
    path = parent / item['name']
    if not path.is_file() or digest(path) != item.get('sha256') or path.stat().st_size != item.get('size'):
        raise ValueError(f'{label} missing or modified: {item["name"]}')
    return path

def srt_protocols(text):
    return sum(line.strip() == 'srt' for line in text.splitlines()) >= 2

def smoke(binary):
    env = dict(os.environ)
    env.pop('CAPTUREFAB_FFMPEG', None)
    env.pop('CAPTUREFAB_SESSION', None)
    env.pop('CAPTUREFAB_EXECUTABLE', None)
    run([binary, '--version'], capture_output=True, text=True, env=env)
    result = run([binary, '--json', 'ffmpeg'], capture_output=True, text=True, env=env)
    media = json.loads(result.stdout)['result']
    if not media.get('embedded'):
        raise ValueError('release binary does not embed FFmpeg')
    if not srt_protocols(media.get('protocols', '')):
        raise ValueError('release FFmpeg does not expose SRT input and output')
    with tempfile.TemporaryDirectory(prefix='capturefab-release-smoke-') as temp:
        env['CAPTUREFAB_SESSION_DIR'] = str(Path(temp) / 'sessions')
        run([binary, '--camera', 'sim:release', 'capture', '-n', '2', '-o', str(Path(temp) / 'frames')], env=env, capture_output=True)
        if len(list((Path(temp) / 'frames').glob('*.png'))) != 2:
            raise ValueError('release capture smoke did not produce two PNGs')

def codesign(path, entitlements):
    run(['codesign', '--force', '--options', 'runtime', '--timestamp', '--entitlements', entitlements, '--sign', os.environ['MACOS_SIGN_IDENTITY'], path])

def package(args, binary, variant):
    inspect_binary(binary, args.os, args.arch)
    ffmpeg = Path(args.ffmpeg_bundle) / 'bin' / ('ffmpeg.exe' if args.os == 'windows' else 'ffmpeg')
    inspect_binary(ffmpeg, args.os, args.arch)
    if not embeds(binary, ffmpeg):
        raise ValueError(f'{binary} does not embed {ffmpeg}')
    if not args.skip_smoke:
        smoke(binary)
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    stem = f'capturefab-{args.version}-{args.os}-{args.arch}-{variant}'
    signed = 'unsigned'
    with tempfile.TemporaryDirectory(prefix='capturefab-package-') as temp:
        stage = Path(temp) / stem
        stage.mkdir()
        executable = stage / ('capturefab.exe' if args.os == 'windows' else 'capturefab')
        shutil.copy2(binary, executable)
        executable.chmod(0o755)
        entitlements = Path(temp) / 'entitlements.plist'
        if args.os == 'macos' and os.environ.get('MACOS_SIGN_IDENTITY'):
            entitlements.write_bytes(plistlib.dumps({'com.apple.security.device.camera': True}))
            codesign(executable, entitlements)
            signed = 'apple-developer-id'
        elif args.os == 'windows' and os.environ.get('WINDOWS_CERTIFICATE_PFX'):
            tool = os.environ.get('WINDOWS_SIGNTOOL', 'signtool.exe')
            run([tool, 'sign', '/fd', 'SHA256', '/tr', 'http://timestamp.digicert.com', '/td', 'SHA256', '/f', os.environ['WINDOWS_CERTIFICATE_PFX'], '/p', os.environ.get('WINDOWS_CERTIFICATE_PASSWORD', ''), executable])
            signed = 'authenticode'
        shutil.copy2(ROOT / 'LICENSE', stage / 'LICENSE')
        shutil.copytree(Path(args.ffmpeg_bundle) / 'licenses', stage / 'media-licenses')
        shutil.copy2(Path(args.ffmpeg_bundle) / 'build-manifest.txt', stage / 'media-build-manifest.txt')
        (stage / 'THIRD-PARTY-LICENSES.txt').write_text(notices(args.target, variant), encoding='utf-8')
        (stage / 'README.txt').write_text(f'Capturefab {args.version}\n{args.os}/{args.arch}; {variant}\nRun capturefab --help for CLI usage. Desktop builds open the GUI with no arguments.\nFFmpeg is embedded; no separately installed media executable or camera SDK is required.\nPlatform signature: {signed}. Verify the download against the release SHA256SUMS.\nLicenses: LICENSE (Capturefab), THIRD-PARTY-LICENSES.txt (Rust crates), media-licenses/ (FFmpeg payload).\nCorresponding source: the release source and media-sources archives. See docs/hardware-validation.md for test scope.\n')
        if args.os == 'macos' and variant == 'desktop':
            app = stage / 'Capturefab.app' / 'Contents'
            (app / 'MacOS').mkdir(parents=True)
            shutil.copy2(executable, app / 'MacOS' / 'capturefab')
            info = plistlib.loads((ROOT / 'assets' / 'Info.plist').read_bytes())
            short = '.'.join(SEMVER.fullmatch(args.version).groups()[:3])
            info.update(CFBundlePackageType='APPL', CFBundleShortVersionString=short, CFBundleVersion=short)
            (app / 'Info.plist').write_bytes(plistlib.dumps(info))
            if signed == 'apple-developer-id':
                codesign(app.parent, entitlements)
        path = output / (stem + ('.zip' if args.os in ['macos', 'windows'] else '.tar.gz'))
        archive_tree(stage, path, stem)
        if args.os == 'macos' and signed == 'apple-developer-id' and os.environ.get('MACOS_NOTARY_PROFILE'):
            run(['xcrun', 'notarytool', 'submit', path, '--keychain-profile', os.environ['MACOS_NOTARY_PROFILE'], '--wait'])
            # Staple the app, then recreate the archive with its ticket included.
            if variant == 'desktop':
                run(['xcrun', 'stapler', 'staple', app.parent])
                archive_tree(stage, path, stem)
            signed = 'apple-notarized'
    return record(path, args.os, args.arch, variant, target=args.target, platform_signature=signed, validation='cross-compiled' if args.skip_smoke else 'native-smoke-passed')

def media_sources(args):
    output = Path(args.output)
    name = f'capturefab-{args.version}-{args.os}-{args.arch}-media-sources.tar.gz'
    path = output / name
    source = Path(args.ffmpeg_bundle) / 'sources'
    recipe = ROOT / 'scripts/build-ffmpeg.sh'
    pins = dict(re.findall(r'^([A-Z0-9_]+)=([^\s]+)$', recipe.read_text(), re.M))
    manifest_path = Path(args.ffmpeg_bundle) / 'build-manifest.txt'
    manifest = manifest_path.read_text()
    required = {
        f'ffmpeg-{pins["FFMPEG_VERSION"]}.tar.xz': pins['FFMPEG_SHA256'],
        f'openssl-{pins["OPENSSL_VERSION"]}.tar.gz': pins['OPENSSL_SHA256'],
        f'srt-{pins["SRT_VERSION"]}.tar.gz': pins['SRT_SHA256'],
        f'x264-{pins["X264_COMMIT"]}.tar.gz': pins['X264_SHA256'],
    }
    for label, key in [('FFmpeg', 'FFMPEG_VERSION'), ('OpenSSL', 'OPENSSL_VERSION'), ('SRT', 'SRT_VERSION'), ('x264', 'X264_COMMIT')]:
        if not re.search(rf'^{label} {re.escape(pins[key])}$', manifest, re.M):
            raise ValueError(f'FFmpeg build manifest does not match the pinned {label} source')
    for label, name, key, version_key, flags in [('NVENC/NVDEC', f'nv-codec-headers-{pins["NV_HEADERS_VERSION"]}.tar.gz', 'NV_HEADERS_SHA256', 'NV_HEADERS_VERSION', ['ffnvcodec', 'nvenc', 'nvdec', 'cuvid']), ('AMF', f'amf-headers-{pins["AMF_VERSION"]}.tar.gz', 'AMF_SHA256', 'AMF_VERSION', ['amf'])]:
        toggle = re.search(rf'^{label} headers enabled: ([01]) \(([^)]+)\)$', manifest, re.M)
        if not toggle:
            # Older bundles predate optional GPU headers. Their explicit
            # configure flags can prove that no such source was compiled.
            if '--disable-autodetect' in manifest and not any(f'--enable-{flag}' in manifest for flag in flags):
                continue
            raise ValueError(f'FFmpeg build manifest does not identify the pinned {label} headers')
        if toggle[1] == '1':
            if toggle[2] != pins[version_key]:
                raise ValueError(f'FFmpeg build manifest does not match the pinned {label} header source')
            required[name] = pins[key]
    for name, expected in required.items():
        archive_path = source / name
        if not archive_path.is_file() or digest(archive_path) != expected:
            raise ValueError(f'exact FFmpeg source archive missing or modified: {archive_path}; copy DOWNLOAD_DIR into the bundle sources directory')
    with tarfile.open(path, 'w:gz', dereference=True) as archive:
        for name in sorted(required):
            archive_path = source / name
            archive.add(archive_path, arcname=f'media-sources/{archive_path.name}')
        archive.add(manifest_path, arcname='media-sources/build-manifest.txt')
        archive.add(recipe, arcname='media-sources/build-ffmpeg.sh')
    return path

def repackage(args, variant):
    output = Path(args.output)
    report = json.loads((output / f'build-{args.os}-{args.arch}.json').read_text())
    if report.get('version') != args.version:
        raise ValueError('existing package has a different release version')
    item = next((item for item in report.get('assets', []) if (item['os'], item['arch'], item['variant']) == (args.os, args.arch, variant)), None)
    if item is None:
        raise ValueError(f'no verified {variant} package exists to repackage')
    archive_path = verified_file(output, item, 'existing package')
    stem = f'capturefab-{args.version}-{args.os}-{args.arch}-{variant}'
    name = 'capturefab.exe' if args.os == 'windows' else 'capturefab'
    with tempfile.TemporaryDirectory(prefix='capturefab-repackage-') as temp:
        binary = Path(temp) / name
        if archive_path.suffix == '.zip':
            with zipfile.ZipFile(archive_path) as archive:
                binary.write_bytes(archive.read(f'{stem}/{name}'))
        else:
            with tarfile.open(archive_path) as archive:
                with archive.extractfile(f'{stem}/{name}') as stream:
                    binary.write_bytes(stream.read())
        binary.chmod(0o755)
        return package(args, binary, variant)

def build(args):
    args.version = version(args.version)
    if args.version != version():
        raise ValueError('release version must match Cargo.toml')
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    assets, failures = [], []
    for variant in args.variants.split(','):
        if variant not in FEATURES:
            raise ValueError('variants must be desktop and/or headless')
        try:
            if args.repackage:
                assets.append(repackage(args, variant))
                continue
            if args.binary:
                binary = Path(args.binary).resolve()
            else:
                command = ['cargo', 'build', '--locked', '--release', '--target', args.target, *FEATURES[variant]]
                env = dict(os.environ, CAPTUREFAB_FFMPEG_BINARY=str((Path(args.ffmpeg_bundle) / 'bin' / ('ffmpeg.exe' if args.os == 'windows' else 'ffmpeg')).resolve()), CAPTUREFAB_FFMPEG_LICENSE=str((Path(args.ffmpeg_bundle) / 'licenses/NOTICE.txt').resolve()))
                run(command, cwd=ROOT, env=env)
                binary = (ROOT / os.environ.get('CARGO_TARGET_DIR', 'target') / args.target / 'release' / ('capturefab.exe' if args.os == 'windows' else 'capturefab')).resolve()
            assets.append(package(args, binary, variant))
        except (ValueError, RuntimeError, OSError) as error:
            failures.append(dict(os=args.os, arch=args.arch, variant=variant, error=str(error)))
            print(f'{variant}: {error}', file=sys.stderr)
    source = media_sources(args)
    report = dict(schema_version=1, version=args.version, target=args.target, assets=assets, failures=failures,
                  media_source=dict(name=source.name, sha256=digest(source), size=source.stat().st_size))
    (output / f'build-{args.os}-{args.arch}.json').write_text(json.dumps(report, indent=2) + '\n')
    if failures:
        raise RuntimeError(f'{len(failures)} variant(s) failed; successful artifacts and failure report retained')

def assemble(args):
    ver = version(args.version)
    root, output = Path(args.input), Path(args.output)
    if output.exists() and any(output.iterdir()):
        raise ValueError(f'{output} must be empty so that only verified files are released')
    output.mkdir(parents=True, exist_ok=True)
    successes, failures = {}, {}
    for report_path in sorted(root.rglob('build-*.json')):
        report = json.loads(report_path.read_text())
        if report['version'] != ver:
            raise ValueError('mixed release versions')
        media = report.get('media_source')
        if report.get('assets'):
            if not isinstance(media, dict):
                raise ValueError('verified corresponding media source is required for every successful build')
            media_path = verified_file(report_path.parent, media, 'corresponding media source')
            expected_names = {f'capturefab-{ver}-{item["os"]}-{item["arch"]}-media-sources.tar.gz' for item in report['assets']}
            if expected_names != {media['name']}:
                raise ValueError(f'corresponding media source has the wrong platform: {media["name"]}')
            shutil.copy2(media_path, output / media_path.name)
        for item in report.get('assets', []):
            path = verified_file(report_path.parent, item, 'asset')
            key = item['os'], item['arch'], item['variant']
            if key in successes:
                raise ValueError(f'duplicate platform artifact: {key}')
            shutil.copy2(path, output / path.name)
            item['url'] = link(args.download_base, path.name)
            successes[key] = item
        for item in report.get('failures', []):
            failures[(item['os'], item['arch'], item['variant'])] = item['error']
    if not successes:
        raise ValueError('no verified build assets found; refusing to create a release')
    assets = [successes.get(key) or dict(os=key[0], arch=key[1], variant=key[2], name=f'capturefab-{ver}-{key[0]}-{key[1]}-{key[2]}', available=False, reason=failures.get(key, 'Not built for this release')) for key in TARGETS]
    source = source_archive(output, ver)
    signature = 'SHA256SUMS.asc' if args.gpg_key else None
    release = dict(version=ver, date=dt.datetime.now(dt.timezone.utc).date().isoformat(), url=f'https://github.com/{args.repository}/releases/tag/v{ver}', checksums_url=link(args.download_base, 'SHA256SUMS'), source_url=link(args.download_base, source.name), assets=assets)
    if signature:
        release['checksums_signature_url'] = link(args.download_base, signature)
    (output / 'releases.json').write_text(json.dumps(dict(schema_version=1, latest=ver, releases=[release]), indent=2) + '\n')
    files = sorted(p for p in output.iterdir() if p.is_file() and not p.name.startswith('SHA256SUMS') and p.name != 'release-notes.md')
    (output / 'SHA256SUMS').write_text(''.join(f'{digest(p)}  {p.name}\n' for p in files))
    if signature:
        run(['gpg', '--batch', '--yes', '--pinentry-mode', 'loopback', '--passphrase-fd', '0', '--local-user', args.gpg_key, '--armor', '--detach-sign', '--output', output / signature, output / 'SHA256SUMS'], input=os.environ.get('RELEASE_GPG_PASSPHRASE', ''), text=True)
    unavailable = [f'- {a["os"]} {a["arch"]} {a["variant"]}: {a["reason"]}' for a in assets if not a['available']]
    notes = [f'Capturefab {ver}', '', 'One executable per platform: GUI by default in desktop builds; CLI and authenticated session automation in both variants.', '', 'Build artifacts:']
    notes += [f'- {key[0]} {key[1]} {key[2]}: {item["validation"]}, platform signature {item["platform_signature"]}' for key, item in successes.items()]
    notes += ['', 'Unavailable targets:'] + unavailable if unavailable else []
    notes += ['', 'Verification:', '- Check downloads against SHA256SUMS, for example `sha256sum --check --ignore-missing SHA256SUMS`.']
    notes += [f'- SHA256SUMS.asc is an OpenPGP signature by key {args.gpg_key}: `gpg --verify SHA256SUMS.asc SHA256SUMS`.'] if signature else []
    notes += [f'- GitHub build provenance covers every file in SHA256SUMS: `gh attestation verify FILE --repo {args.repository}`.'] if args.provenance else []
    notes += ['- Apple Developer ID, notarization and Windows Authenticode apply only where the platform signature above says so.', '', f'Corresponding source: {source.name} contains the exact Capturefab commit and vendored Cargo.lock dependency sources with an offline build configuration; each *-media-sources.tar.gz holds the verified FFmpeg, x264, SRT, OpenSSL and enabled header archives with their build manifest. Packages list Rust crate licenses in THIRD-PARTY-LICENSES.txt. See docs/hardware-validation.md for actual hardware test scope.']
    (output / 'release-notes.md').write_text('\n'.join(notes) + '\n')
    print(f'Prepared {ver}: {len(successes)} platform assets; {len(assets) - len(successes)} unavailable selections.')

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    check = commands.add_parser('version')
    check.add_argument('--version')
    build_parser = commands.add_parser('build')
    build_parser.add_argument('--version')
    build_parser.add_argument('--target', required=True)
    build_parser.add_argument('--os', choices=['linux', 'macos', 'windows'], required=True)
    build_parser.add_argument('--arch', choices=['x86_64', 'aarch64', 'armv7'], required=True)
    build_parser.add_argument('--variants', default='desktop,headless')
    build_parser.add_argument('--ffmpeg-bundle', required=True)
    build_parser.add_argument('--binary', help='package an already built release executable (one --variants value)')
    build_parser.add_argument('--repackage', action='store_true', help='sign/repackage verified archives already in --output without recompiling')
    build_parser.add_argument('--skip-smoke', action='store_true', help='cross build: label unexecuted target honestly')
    build_parser.add_argument('--output', default='dist')
    collect = commands.add_parser('assemble')
    collect.add_argument('--version')
    collect.add_argument('--input', required=True)
    collect.add_argument('--output', default='dist/release')
    collect.add_argument('--repository', default='capturefab/capturefab')
    collect.add_argument('--download-base', default='')
    collect.add_argument('--gpg-key', help='sign SHA256SUMS with this OpenPGP key; passphrase from RELEASE_GPG_PASSPHRASE')
    collect.add_argument('--provenance', action='store_true', help='document GitHub build provenance created for SHA256SUMS')
    args = parser.parse_args()
    try:
        if args.command == 'version':
            wanted = version(args.version)
            if wanted != version():
                raise ValueError('version must match Cargo.toml')
            print(wanted)
        elif args.command == 'build':
            if args.binary and args.repackage:
                raise ValueError('--binary and --repackage cannot be combined')
            if args.binary and ',' in args.variants:
                raise ValueError('--binary requires one explicit --variants value')
            build(args)
        else:
            if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repository):
                raise ValueError('repository must be OWNER/NAME')
            assemble(args)
    except (ValueError, RuntimeError, OSError) as error:
        parser.exit(1, f'release: {error}\n')

if __name__ == '__main__':
    main()
