#!/usr/bin/env python3
"""Build/package real binaries and assemble an honest, checksummed release."""
import argparse
import contextlib
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
import time
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[1]
LINE_COMPUTER = re.search(r'^repository\s*=\s*"([^"]+)"', (ROOT / 'Cargo.toml').read_text(), re.M)[1] + '/blob/main/docs/line-computer.md'
SEMVER = re.compile(r'(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?')
# No 32-bit ARM: Raspberry Pi 3 and later run the 64-bit Raspberry Pi OS.
TARGETS = [(os_, arch) for os_, arches in [('linux', ['x86_64', 'aarch64']), ('macos', ['x86_64', 'aarch64']), ('windows', ['x86_64', 'aarch64'])] for arch in arches]
# The oldest systems release executables must start on: glibc 2.28 (RHEL,
# Rocky and Alma 8, Debian 10, Ubuntu 18.10 and newer) and the macOS
# deployment targets. Windows executables must not need the Visual C++
# redistributable, which is not part of Windows.
GLIBC_BASELINE = (2, 28)
MACOS_BASELINE = {'aarch64': (11, 0), 'x86_64': (10, 15)}
VC_RUNTIME = re.compile(r'(vcruntime|msvcp|msvcr|vcomp|concrt|vccorlib)\d*(_\d+)?d?\.dll', re.I)
LICENSE_FILE = re.compile(r'(licen[cs]e|copying|notice|unlicense)', re.I)

def version(value=None):
    value = value or re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / 'Cargo.toml').read_text(), re.M)[1]
    value = value[1:] if value.startswith('v') else value
    match = SEMVER.fullmatch(value)
    if not match or (match[4] and any(x.isdigit() and len(x) > 1 and x[0] == '0' for x in match[4].split('.'))):
        raise ValueError('version must follow Semantic Versioning, such as 0.1.0 or 0.2.0-rc.1')
    return value

def precedence(value):
    """A sort key that orders versions by Semantic Versioning precedence."""
    match = SEMVER.fullmatch(value)
    if not match:
        raise ValueError(f'invalid release version: {value}')
    pre = tuple((0, int(x), '') if x.isdigit() else (1, 0, x) for x in (match[4] or '').split('.') if x)
    return int(match[1]), int(match[2]), int(match[3]), not pre, pre

def set_version(value, root=ROOT):
    """Raise the package version in Cargo.toml and Cargo.lock, for a release
    pull request; the new version must be newer than the current one."""
    toml, lock = root / 'Cargo.toml', root / 'Cargo.lock'
    current = re.search(r'^version\s*=\s*"([^"]+)"', toml.read_text(), re.M)[1]
    new = version(value)
    if precedence(new) <= precedence(current):
        raise ValueError(f'{new} is not newer than the current version {current}')
    updated, count = re.subn(r'^(\[package\]\n(?:(?!\[)[^\n]*\n)*?version\s*=\s*)"[^"]+"', lambda m: f'{m[1]}"{new}"', toml.read_text(), count=1, flags=re.M)
    locked, locked_count = re.subn(r'(\[\[package\]\]\nname = "capturefab"\nversion = )"[^"]+"', lambda m: f'{m[1]}"{new}"', lock.read_text(), count=1)
    if count != 1 or locked_count != 1:
        raise ValueError('cannot find the package version in Cargo.toml and Cargo.lock')
    toml.write_text(updated)
    lock.write_text(locked)
    return current, new

def digest(path):
    result = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()

def run(args, **kwargs):
    if kwargs.get('text'): kwargs.setdefault('encoding', 'utf-8')
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

def elf_glibc(data):
    """Newest GLIBC_x.y symbol version an ELF executable requires, or None."""
    end = '<' if data[5] == 1 else '>'
    if data[4] == 2:
        (shoff,), (shentsize, shnum) = struct.unpack_from(end + 'Q', data, 0x28), struct.unpack_from(end + 'HH', data, 0x3A)
        layout = end + 'IIQQQQII'
    else:
        (shoff,), (shentsize, shnum) = struct.unpack_from(end + 'I', data, 0x20), struct.unpack_from(end + 'HH', data, 0x2E)
        layout = end + 'IIIIIIII'
    sections = [struct.unpack_from(layout, data, shoff + i * shentsize) for i in range(shnum)]
    newest = None
    for _, kind, _, _, offset, size, link, count in sections:
        if kind != 0x6FFFFFFE:  # SHT_GNU_verneed
            continue
        strings = sections[link][4]
        entry = offset
        for _ in range(count):
            _, aux_count, _, aux, following = struct.unpack_from(end + 'HHIII', data, entry)
            at = entry + aux
            for _ in range(aux_count):
                _, _, _, name, step = struct.unpack_from(end + 'IHHII', data, at)
                text = data[strings + name:data.index(b'\0', strings + name)].decode('ascii', 'replace')
                match = re.fullmatch(r'GLIBC_(\d+)\.(\d+)(?:\.\d+)?', text)
                if match:
                    found = int(match[1]), int(match[2])
                    newest = max(newest or found, found)
                at += step
            if not following:
                break
            entry += following
    return newest

def macho_minimum(data):
    """Minimum macOS version a Mach-O executable declares, or None; the
    highest across the slices of a universal binary."""
    if data[:4] == b'\xca\xfe\xba\xbe':
        slices = [struct.unpack_from('>IIIII', data, 8 + i * 20) for i in range(struct.unpack_from('>I', data, 4)[0])]
        found = [macho_minimum(data[offset:offset + size]) for _, _, offset, size, _ in slices]
        return max((v for v in found if v), default=None)
    end = '<' if data[0] == 0xCF else '>'
    (count,), at = struct.unpack_from(end + 'I', data, 16), 32
    for _ in range(count):
        command, size = struct.unpack_from(end + 'II', data, at)
        if command in (0x32, 0x24):  # LC_BUILD_VERSION (minos at 12), LC_VERSION_MIN_MACOSX (8)
            value = struct.unpack_from(end + 'I', data, at + (12 if command == 0x32 else 8))[0]
            return value >> 16, (value >> 8) & 0xFF
        at += size
    return None

def pe_imports(data):
    """DLLs a PE executable imports, including delay-loaded ones."""
    pe = struct.unpack_from('<I', data, 60)[0]
    sections, optional_size = struct.unpack_from('<H', data, pe + 6)[0], struct.unpack_from('<H', data, pe + 20)[0]
    optional = pe + 24
    directories = optional + (112 if struct.unpack_from('<H', data, optional)[0] == 0x20B else 96)
    table = [struct.unpack_from('<IIII', data, optional + optional_size + i * 40 + 8) for i in range(sections)]
    def offset(rva):
        for size, address, raw_size, raw in table:
            if address <= rva < address + max(size, raw_size):
                return raw + rva - address
        raise ValueError('PE address outside every section')
    names = []
    for index, width, name_at in [(1, 20, 12), (13, 32, 4)]:  # import, delay-import directories
        rva = struct.unpack_from('<I', data, directories + index * 8)[0]
        if not rva:
            continue
        at = offset(rva)
        while any(data[at:at + width]):
            name = offset(struct.unpack_from('<I', data, at + name_at)[0])
            names.append(data[name:data.index(b'\0', name)].decode('ascii', 'replace'))
            at += width
    return names

def check_compatibility(path, os_, arch):
    """Refuse an executable that needs a newer system than the release baseline."""
    data = Path(path).read_bytes()
    name = Path(path).name
    try:
        check_headers(data, name, os_, arch)
    except (struct.error, IndexError, KeyError) as error:
        raise ValueError(f'cannot read the system requirements of {name}: {error}') from None

def check_headers(data, name, os_, arch):
    if os_ == 'linux':
        newest = elf_glibc(data)
        if newest and newest > GLIBC_BASELINE:
            raise ValueError(f'{name} requires glibc {newest[0]}.{newest[1]}; releases support {GLIBC_BASELINE[0]}.{GLIBC_BASELINE[1]}')
    elif os_ == 'macos':
        minimum, baseline = macho_minimum(data), MACOS_BASELINE[arch]
        if minimum is None or minimum > baseline:
            raise ValueError(f'{name} requires macOS {minimum}; releases support {baseline[0]}.{baseline[1]}')
    else:
        runtime = [dll for dll in pe_imports(data) if VC_RUNTIME.fullmatch(dll)]
        if runtime:
            raise ValueError(f'{name} needs the Visual C++ runtime ({", ".join(runtime)}); link it statically')

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

def linked_packages(metadata):
    """Packages reachable from the root through normal (linked) dependencies."""
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    packages = {package['id']: package for package in metadata['packages']}
    linked, pending = set(), [metadata['resolve']['root']]
    while pending:
        for dep in nodes[pending.pop()]['deps']:
            if dep['pkg'] not in linked and any(kind['kind'] is None for kind in dep['dep_kinds']):
                linked.add(dep['pkg'])
                pending.append(dep['pkg'])
    return packages, linked

# Media payload components, by the build recipe's variable prefix: name,
# license as built, and upstream location.
MEDIA = [('FFMPEG', 'ffmpeg', 'GPL-3.0-or-later', 'https://ffmpeg.org/'), ('X264', 'x264', 'GPL-2.0-or-later', 'https://code.videolan.org/videolan/x264'),
         ('SRT', 'srt', 'MPL-2.0', 'https://github.com/Haivision/srt'), ('OPENSSL', 'openssl', 'Apache-2.0', 'https://www.openssl.org/'),
         ('NV_HEADERS', 'nv-codec-headers', 'MIT', 'https://github.com/FFmpeg/nv-codec-headers'), ('VULKAN_HEADERS', 'Vulkan-Headers', 'Apache-2.0 OR MIT', 'https://github.com/KhronosGroup/Vulkan-Headers'),
         ('LIBVPL', 'libvpl', 'MIT', 'https://github.com/intel/libvpl'), ('AMF', 'AMF', 'MIT', 'https://github.com/GPUOpen-LibrariesAndSDKs/AMF')]

WEB_CLIENT = ROOT / 'web-client'

def web_dist():
    """The browser client web-client/build.sh built, or None. Release builds
    embed it when it exists (the embedded-web feature), so `serve --listen`
    needs no --web-dir."""
    dist = WEB_CLIENT / 'dist'
    return dist if all((dist / name).is_file() for name in ('index.html', 'capturefab_web_bg.wasm')) else None

def web_metadata():
    return json.loads(run(['cargo', 'metadata', '--format-version', '1', '--locked', '--filter-platform', 'wasm32-unknown-unknown', '--manifest-path', WEB_CLIENT / 'Cargo.toml'], cwd=ROOT, capture_output=True, text=True).stdout)

def cargo_features(web):
    """The release cargo feature flags, plus embedded-web with a built client."""
    return ['--features', 'embedded-web'] if web else []

def crate_components(metadata, lock):
    """CycloneDX components for the crates a package links, with Cargo.lock checksums."""
    checksums = dict(((m[1], m[2]), m[3]) for m in re.finditer(r'\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"\n(?:source = "[^"]*"\n)?(?:checksum = "([0-9a-f]{64})")?', lock))
    packages, linked = linked_packages(metadata)
    crates = []
    for package in sorted((packages[i] for i in linked), key=lambda p: (p['name'], p['version'])):
        purl = f"pkg:cargo/{package['name']}@{package['version']}"
        item = dict(type='library', **{'bom-ref': purl}, name=package['name'], version=package['version'], purl=purl, scope='required')
        if package.get('license'):
            item['licenses'] = [dict(expression=package['license'])]
        if checksums.get((package['name'], package['version'])):
            item['hashes'] = [dict(alg='SHA-256', content=checksums[(package['name'], package['version'])])]
        crates.append(item)
    return crates

def cyclonedx(metadata, lock, recipe, ver, repository, serial, web=None):
    """A CycloneDX 1.6 SBOM for every platform of a release: the Rust crates
    linked with all features, with Cargo.lock checksums, those of the browser
    client when `web` holds its metadata and lock file, and the media payload
    components pinned by the FFmpeg build recipe."""
    crates = crate_components(metadata, lock)
    if web:
        known = {item['bom-ref'] for item in crates}
        crates += [item for item in crate_components(*web) if item['bom-ref'] not in known]
        crates.sort(key=lambda item: (item['name'], item['version']))
    pinned = dict(re.findall(r'^([A-Z0-9_]+)=(\S+)$', recipe, re.M))
    media = []
    for prefix, name, license_, url in MEDIA:
        version_ = pinned.get(f'{prefix}_VERSION') or pinned.get(f'{prefix}_COMMIT')
        if not version_:
            continue
        item = dict(type='library', **{'bom-ref': f'media:{name}'}, name=name, version=version_, scope='required', licenses=[dict(expression=license_)],
                    description='Part of the FFmpeg executable embedded in each package; which components a platform enables is listed in its media-build-manifest.txt.',
                    externalReferences=[dict(type='website', url=url)])
        if pinned.get(f'{prefix}_SHA256'):
            item['hashes'] = [dict(alg='SHA-256', content=pinned[f'{prefix}_SHA256'])]
        media.append(item)
    root = dict(type='application', **{'bom-ref': 'capturefab'}, name='capturefab', version=ver, licenses=[dict(license=dict(id='MIT'))], purl=f'pkg:github/{repository}@v{ver}')
    return {'bomFormat': 'CycloneDX', 'specVersion': '1.6', 'serialNumber': f'urn:uuid:{serial}', 'version': 1,
            'metadata': dict(timestamp=dt.datetime.now(dt.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'), tools=dict(components=[dict(type='application', name='capturefab scripts/release.py')]), component=root),
            'components': crates + media,
            'dependencies': [dict(ref='capturefab', dependsOn=[c['bom-ref'] for c in crates] + ['media:ffmpeg']), dict(ref='media:ffmpeg', dependsOn=[m['bom-ref'] for m in media if m['bom-ref'] != 'media:ffmpeg'])]}

def sbom(output, ver, repository, web=False):
    metadata = run(['cargo', 'metadata', '--format-version', '1', '--locked', '--all-features'], cwd=ROOT, capture_output=True, text=True).stdout
    web = (web_metadata(), (WEB_CLIENT / 'Cargo.lock').read_text()) if web else None
    document = cyclonedx(json.loads(metadata), (ROOT / 'Cargo.lock').read_text(), (ROOT / 'scripts' / 'build-ffmpeg.sh').read_text(), ver, repository, uuid.uuid4(), web)
    path = output / f'capturefab-{ver}-sbom.cdx.json'
    path.write_text(json.dumps(document, indent=1) + '\n')
    return path

def license_notices(metadata, title='Rust crates compiled into this Capturefab executable and their license texts.'):
    packages, linked = linked_packages(metadata)
    texts = {}
    for package in sorted((packages[i] for i in linked), key=lambda p: (p['name'], p['version'])):
        folder = Path(package['manifest_path']).parent
        files = sorted(p for p in folder.iterdir() if p.is_file() and LICENSE_FILE.match(p.name))
        files += [folder / package['license_file']] if package.get('license_file') else []
        found = dict.fromkeys(f.read_text(encoding='utf-8', errors='replace').strip() for f in files)
        for text in found or ['This crate includes no license file; its license expression is listed above.']:
            label = f"{package['name']} {package['version']} ({package['license'] or 'see license file'})"
            # MPL-2.0 requires telling recipients where the covered source is.
            if 'MPL-2.0' in (package.get('license') or ''):
                label += f"; source: {package.get('repository') or 'https://crates.io/crates/' + package['name']}"
            texts.setdefault(text, []).append(label)
    return f'{title}\n' + ''.join(f'\n===== {", ".join(names)} =====\n\n{text}\n' for text, names in texts.items())

def notices(target, web=False):
    metadata = run(['cargo', 'metadata', '--format-version', '1', '--locked', '--filter-platform', target], cwd=ROOT, capture_output=True, text=True).stdout
    text = license_notices(json.loads(metadata))
    if web:
        text += '\n' + license_notices(web_metadata(), 'Rust crates compiled into the browser client (web-client) built into this executable for serve --listen, and their license texts.')
    text += f"\n===== Phosphor Icons 2.1.2 font (MIT), assets/fonts =====\n\n{(ROOT / 'assets/fonts/Phosphor-LICENSE.txt').read_text(encoding='utf-8').strip()}\n"
    return text

def link(base, name):
    return f'{base.rstrip("/")}/{name}' if base else name

def record(path, os_, arch, **extra):
    return dict(name=path.name, os=os_, arch=arch, available=True,
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
    with tempfile.TemporaryDirectory(prefix='capturefab-release-smoke-') as temp:
        # Sessions, the storage ledger and the extracted FFmpeg stay out of
        # the builder's own folders.
        env['CAPTUREFAB_SESSION_DIR'] = str(Path(temp) / 'sessions')
        env['CAPTUREFAB_CACHE_DIR'] = str(Path(temp) / 'cache')
        run([binary, '--version'], stdout=subprocess.PIPE, text=True, env=env)
        result = run([binary, '--json', 'ffmpeg'], stdout=subprocess.PIPE, text=True, env=env)
        media = json.loads(result.stdout)['result']
        if not media.get('embedded'):
            raise ValueError('release binary does not embed FFmpeg')
        if not srt_protocols(media.get('protocols', '')):
            raise ValueError('release FFmpeg does not expose SRT input and output')
        run([binary, '--camera', 'sim:release', 'capture', '-n', '2', '-o', str(Path(temp) / 'frames')], env=env, stdout=subprocess.DEVNULL)
        if len(list((Path(temp) / 'frames').glob('*.png'))) != 2:
            raise ValueError('release capture smoke did not produce two PNGs')

def codesign(path, entitlements):
    run(['codesign', '--force', '--options', 'runtime', '--timestamp', '--entitlements', entitlements, '--sign', os.environ['MACOS_SIGN_IDENTITY'], path])
    run(['codesign', '--verify', '--strict', '--deep', path])

def notarize(path):
    profile = ['--keychain-profile', os.environ['MACOS_NOTARY_PROFILE']]
    # notarytool exits 0 for a finished but rejected submission; read its status.
    result = json.loads(run(['xcrun', 'notarytool', 'submit', path, *profile, '--wait', '--output-format', 'json'], capture_output=True, text=True).stdout)
    if result.get('status') != 'Accepted':
        if result.get('id'):
            subprocess.run(['xcrun', 'notarytool', 'log', result['id'], *profile])
        raise RuntimeError(f'notarization of {Path(path).name} ended with status {result.get("status")!r}')

def stapler(action, app):
    for attempt in range(3):
        result = subprocess.run(['xcrun', 'stapler', action, str(app)],
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        print(result.stdout, end='')
        if result.returncode == 0:
            return
        # Both staple and validate can lose the Apple ticket-service connection.
        # Retry only the observed CloudKit transport error, never a bad ticket.
        if (result.returncode != 68 or 'NSURLErrorDomain Code=-1005' not in result.stdout
                or 'https://api.apple-cloudkit.com/' not in result.stdout
                or attempt == 2):
            raise RuntimeError(f'stapler {action} exited with status {result.returncode}')
        print(f'Retrying stapler {action} after Apple CloudKit connection loss (attempt {attempt + 2}/3).')
        time.sleep(5)

def gatekeeper(app, kind='execute'):
    # spctl accepts a Developer ID app only when it is notarized, so this is
    # the check a downloaded copy will face; stapler proves offline validity.
    # A disk image is assessed as a document opened by its primary signature.
    stapler('staple', app)
    stapler('validate', app)
    context = ['--context', 'context:primary-signature'] if kind == 'open' else []
    run(['spctl', '--assess', '--type', kind, *context, '--verbose', app])

# LZFSE (ULFO) needs macOS 10.11; the oldest supported macOS is 10.15.
DMG_FORMAT = 'ULFO'
HDIUTIL_TIMEOUT = 600
DMG_ROOT = {'Capturefab.app', 'Applications', 'README.txt'}
APP_EXECUTABLE = 'Capturefab.app/Contents/MacOS/capturefab'

def hdiutil(*args):
    # Only the subcommand is named in errors; paths and volume names stay out.
    try:
        result = subprocess.run(['hdiutil', *map(str, args)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, timeout=HDIUTIL_TIMEOUT)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f'hdiutil {args[0]} timed out after {HDIUTIL_TIMEOUT} s') from None
    print(result.stdout, end='')
    return result

def make_dmg(source, output, volume):
    for attempt in range(3):
        result = hdiutil('create', '-volname', volume, '-srcfolder', source, '-fs', 'HFS+', '-format', DMG_FORMAT, '-ov', output)
        if result.returncode == 0:
            break
        # Runners sometimes hold the new device briefly; retry only that.
        if 'Resource busy' not in result.stdout or attempt == 2:
            raise RuntimeError(f'hdiutil create exited with status {result.returncode}')
        print(f'Retrying hdiutil create after a busy device (attempt {attempt + 2}/3).')
        time.sleep(5)
    result = hdiutil('verify', output)
    if result.returncode != 0:
        raise RuntimeError(f'hdiutil verify exited with status {result.returncode}')

@contextlib.contextmanager
def attached(dmg):
    with tempfile.TemporaryDirectory(prefix='capturefab-dmg-') as temp:
        mountpoint = Path(temp) / 'mount'
        mountpoint.mkdir()
        # -nobrowse keeps the image out of Finder and Capturefab's volume list.
        result = hdiutil('attach', '-readonly', '-nobrowse', '-noautoopen', '-mountpoint', mountpoint, dmg)
        if result.returncode != 0:
            raise RuntimeError(f'hdiutil attach exited with status {result.returncode}')
        try:
            yield mountpoint
        finally:
            if hdiutil('detach', mountpoint).returncode != 0:
                hdiutil('detach', mountpoint, '-force')

def dmg_extract(dmg, member, dest):
    with attached(dmg) as mountpoint:
        shutil.copy2(mountpoint / member, dest)

def check_dmg(dmg, expected_app_binary):
    with attached(dmg) as mountpoint:
        root = {p.name for p in mountpoint.iterdir() if not p.name.startswith('.')}
        if root != DMG_ROOT:
            raise RuntimeError(f'disk image root holds {sorted(root)}, expected {sorted(DMG_ROOT)}')
        applications = mountpoint / 'Applications'
        if not applications.is_symlink() or os.readlink(applications) != '/Applications':
            raise RuntimeError('disk image Applications entry is not a link to /Applications')
        contents = mountpoint / 'Capturefab.app' / 'Contents'
        if digest(mountpoint / APP_EXECUTABLE) != digest(expected_app_binary):
            raise RuntimeError('disk image executable differs from the packaged executable')
        for required in ['Info.plist', 'Resources/licenses/THIRD-PARTY-LICENSES.txt']:
            if not (contents / required).is_file():
                raise RuntimeError(f'disk image app is missing Contents/{required}')

def codesign_dmg(path):
    run(['codesign', '--force', '--timestamp', '--sign', os.environ['MACOS_SIGN_IDENTITY'], path])
    run(['codesign', '--verify', '--strict', path])

def macos_readme(version, arch, app_signed, browser):
    text = (f'Capturefab {version} (macOS {arch})\n'
            'Install: drag Capturefab to the Applications folder, then open it from Applications. Eject this disk image afterwards.\n'
            'Command line: the app contains the CLI. Run\n'
            f'  /Applications/{APP_EXECUTABLE} --help\n'
            'To type just capturefab, add it to PATH once (zsh):\n'
            '  echo \'export PATH="/Applications/Capturefab.app/Contents/MacOS:$PATH"\' >> ~/.zprofile\n'
            'then open a new terminal and run capturefab doctor.\n'
            'FFmpeg is embedded; no separately installed media executable or camera SDK is required.\n'
            f'App signature: {app_signed}. Verify the downloaded disk image against the release SHA256SUMS.\n'
            'Licenses: Capturefab.app/Contents/Resources/licenses (Finder: Control-click the app, Show Package Contents).\n'
            'Corresponding source: the release source and media-sources archives.\n')
    return text + (BROWSER_README if browser else '')

BROWSER_README = 'Remote browser: capturefab serve --name line1 --listen TAILNET-IP:8787 serves the browser interface. Anyone who can reach that address can control the cameras; bind it to a Tailscale or loopback address.\n'

def licenses(args, browser, folder):
    shutil.copy2(ROOT / 'LICENSE', folder / 'LICENSE')
    shutil.copytree(Path(args.ffmpeg_bundle) / 'licenses', folder / 'media-licenses')
    shutil.copy2(Path(args.ffmpeg_bundle) / 'build-manifest.txt', folder / 'media-build-manifest.txt')
    (folder / 'THIRD-PARTY-LICENSES.txt').write_text(notices(args.target, browser), encoding='utf-8')

def package_macos(args, binary, browser, output, stem, temp):
    stage = Path(temp) / stem
    app = stage / 'Capturefab.app'
    contents = app / 'Contents'
    (contents / 'MacOS').mkdir(parents=True)
    (contents / 'Resources' / 'licenses').mkdir(parents=True)
    app_binary = contents / 'MacOS' / 'capturefab'
    shutil.copy2(binary, app_binary)
    app_binary.chmod(0o755)
    shutil.copy2(ROOT / 'assets/icons/capturefab.icns', contents / 'Resources/capturefab.icns')
    info = plistlib.loads((ROOT / 'assets' / 'Info.plist').read_bytes())
    short = '.'.join(SEMVER.fullmatch(args.version).groups()[:3])
    info.update(CFBundlePackageType='APPL', CFBundleShortVersionString=short, CFBundleVersion=short)
    (contents / 'Info.plist').write_bytes(plistlib.dumps(info))
    # Resources are sealed by the signature, so the licenses go in first; they
    # then stay with the executable after the drag to Applications.
    licenses(args, browser, contents / 'Resources' / 'licenses')
    os.symlink('/Applications', stage / 'Applications')
    signed = 'unsigned'
    notary = os.environ.get('MACOS_NOTARY_PROFILE')
    if os.environ.get('MACOS_SIGN_IDENTITY'):
        entitlements = Path(temp) / 'entitlements.plist'
        entitlements.write_bytes(plistlib.dumps({'com.apple.security.device.camera': True}))
        codesign(app_binary, entitlements)
        codesign(app, entitlements)
        signed = 'apple-developer-id'
        if notary:
            # Notarize and staple the app itself so the copy dragged out of the
            # image opens offline; this zip is only the submission.
            submission = Path(temp) / 'app.zip'
            run(['ditto', '-c', '-k', '--keepParent', app, submission])
            notarize(submission)
            gatekeeper(app)
            signed = 'apple-notarized'
    # The README reports the app's state; the image is signed after it exists.
    (stage / 'README.txt').write_text(macos_readme(args.version, args.arch, signed, browser), encoding='utf-8')
    dmg = output / f'{stem}.dmg'
    make_dmg(stage, dmg, f'Capturefab {args.version}')
    if signed != 'unsigned':
        codesign_dmg(dmg)
        if notary:
            notarize(dmg)
            gatekeeper(dmg, 'open')
    check_dmg(dmg, app_binary)
    return dmg, signed

def package(args, binary):
    if args.os == 'macos' and shutil.which('hdiutil') is None:
        raise ValueError('macOS disk images need hdiutil; package macOS builds on macOS')
    inspect_binary(binary, args.os, args.arch)
    ffmpeg = Path(args.ffmpeg_bundle) / 'bin' / ('ffmpeg.exe' if args.os == 'windows' else 'ffmpeg')
    inspect_binary(ffmpeg, args.os, args.arch)
    for executable in (binary, ffmpeg):
        check_compatibility(executable, args.os, args.arch)
    if not embeds(binary, ffmpeg):
        raise ValueError(f'{binary} does not embed {ffmpeg}')
    web = web_dist()
    browser = bool(web) and embeds(binary, web / 'capturefab_web_bg.wasm')
    if not args.skip_smoke:
        smoke(binary)
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    stem = f'capturefab-{args.version}-{args.os}-{args.arch}'
    signed = 'unsigned'
    extra = dict(target=args.target, validation='cross-compiled' if args.skip_smoke else 'native-smoke-passed', **(dict(browser_client=True) if browser else {}))
    with tempfile.TemporaryDirectory(prefix='capturefab-package-') as temp:
        if args.os == 'macos':
            path, signed = package_macos(args, binary, browser, output, stem, temp)
            return record(path, args.os, args.arch, platform_signature=signed, **extra)
        stage = Path(temp) / stem
        stage.mkdir()
        executable = stage / ('capturefab.exe' if args.os == 'windows' else 'capturefab')
        shutil.copy2(binary, executable)
        executable.chmod(0o755)
        if args.os == 'windows' and os.environ.get('WINDOWS_CERTIFICATE_PFX'):
            tool = os.environ.get('WINDOWS_SIGNTOOL', 'signtool.exe')
            run([tool, 'sign', '/fd', 'SHA256', '/tr', 'http://timestamp.digicert.com', '/td', 'SHA256', '/f', os.environ['WINDOWS_CERTIFICATE_PFX'], '/p', os.environ.get('WINDOWS_CERTIFICATE_PASSWORD', ''), executable])
            signed = 'authenticode'
        licenses(args, browser, stage)
        readme = f'Capturefab {args.version}\n{args.os}/{args.arch}\nRun capturefab --help for CLI usage. With no arguments it opens the GUI, or lists the CLI commands when no display is available. capturefab doctor checks this computer.\nFFmpeg is embedded; no separately installed media executable or camera SDK is required.\nPlatform signature: {signed}. Verify the download against the release SHA256SUMS.\nLicenses: LICENSE (Capturefab), THIRD-PARTY-LICENSES.txt (Rust crates and bundled fonts), media-licenses/ (FFmpeg payload).\nCorresponding source: the release source and media-sources archives.\n'
        if browser:
            readme += BROWSER_README
        if args.os == 'linux':
            readme += f'Run it as a service on a Raspberry Pi, Jetson or industrial PC: {LINE_COMPUTER}\n'
        (stage / 'README.txt').write_text(readme)
        if args.os == 'linux':
            applications = stage / 'share/applications'
            applications.mkdir(parents=True)
            shutil.copy2(ROOT / 'assets/capturefab.desktop', applications / 'capturefab.desktop')
            for size, suffix in [('scalable', 'svg'), ('256x256', 'png')]:
                icons = stage / 'share/icons/hicolor' / size / 'apps'
                icons.mkdir(parents=True)
                shutil.copy2(ROOT / f'assets/icons/capturefab.{suffix}', icons / f'capturefab.{suffix}')
            (stage / 'INSTALL-DESKTOP.txt').write_text(
                'To add Capturefab to your application launcher, install capturefab on your PATH,\n'
                'then copy the contents of share/ into ~/.local/share/ (or /usr/local/share/\n'
                'for all users). Keep the applications/ and icons/ subdirectories intact.\n'
            )
        path = output / (stem + ('.zip' if args.os == 'windows' else '.tar.gz'))
        archive_tree(stage, path, stem)
    return record(path, args.os, args.arch, platform_signature=signed, **extra)

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
        f'zlib-{pins["ZLIB_VERSION"]}.tar.gz': pins['ZLIB_SHA256'],
    }
    for label, key in [('FFmpeg', 'FFMPEG_VERSION'), ('OpenSSL', 'OPENSSL_VERSION'), ('SRT', 'SRT_VERSION'), ('x264', 'X264_COMMIT'), ('zlib', 'ZLIB_VERSION')]:
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

def repackage(args):
    output = Path(args.output)
    report = json.loads((output / f'build-{args.os}-{args.arch}.json').read_text())
    if report.get('version') != args.version:
        raise ValueError('existing package has a different release version')
    item = next((item for item in report.get('assets', []) if (item['os'], item['arch']) == (args.os, args.arch)), None)
    if item is None:
        raise ValueError('no verified package exists to repackage')
    archive_path = verified_file(output, item, 'existing package')
    stem = f'capturefab-{args.version}-{args.os}-{args.arch}'
    name = 'capturefab.exe' if args.os == 'windows' else 'capturefab'
    with tempfile.TemporaryDirectory(prefix='capturefab-repackage-') as temp:
        binary = Path(temp) / name
        if args.os == 'macos':
            if archive_path.suffix != '.dmg':
                raise ValueError('existing package has an unexpected format')
            dmg_extract(archive_path, APP_EXECUTABLE, binary)
        elif archive_path.suffix == '.zip':
            with zipfile.ZipFile(archive_path) as archive:
                binary.write_bytes(archive.read(f'{stem}/{name}'))
        else:
            with tarfile.open(archive_path) as archive:
                with archive.extractfile(f'{stem}/{name}') as stream:
                    binary.write_bytes(stream.read())
        binary.chmod(0o755)
        return package(args, binary)

def build(args):
    args.version = version(args.version)
    if args.version != version():
        raise ValueError('release version must match Cargo.toml')
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    assets, failures = [], []
    try:
        if args.repackage:
            assets.append(repackage(args))
        else:
            if args.binary:
                binary = Path(args.binary).resolve()
            else:
                web = web_dist()
                command = ['cargo', 'build', '--locked', '--release', '--target', args.target, *cargo_features(web)]
                env = dict(os.environ, CAPTUREFAB_FFMPEG_BINARY=str((Path(args.ffmpeg_bundle) / 'bin' / ('ffmpeg.exe' if args.os == 'windows' else 'ffmpeg')).resolve()), CAPTUREFAB_FFMPEG_LICENSE=str((Path(args.ffmpeg_bundle) / 'licenses/NOTICE.txt').resolve()))
                if web:
                    env['CAPTUREFAB_WEB_DIST'] = str(web.resolve())
                run(command, cwd=ROOT, env=env)
                binary = (ROOT / os.environ.get('CARGO_TARGET_DIR', 'target') / args.target / 'release' / ('capturefab.exe' if args.os == 'windows' else 'capturefab')).resolve()
            assets.append(package(args, binary))
    except (ValueError, RuntimeError, OSError) as error:
        failures.append(dict(os=args.os, arch=args.arch, error=str(error)))
        print(f'build: {error}', file=sys.stderr)
    source = media_sources(args)
    report = dict(schema_version=1, version=args.version, target=args.target, assets=assets, failures=failures,
                  media_source=dict(name=source.name, sha256=digest(source), size=source.stat().st_size))
    (output / f'build-{args.os}-{args.arch}.json').write_text(json.dumps(report, indent=2) + '\n')
    if failures:
        raise RuntimeError(f'{len(failures)} build(s) failed; successful artifacts and failure report retained')

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
            key = item['os'], item['arch']
            if key in successes:
                raise ValueError(f'duplicate platform artifact: {key}')
            shutil.copy2(path, output / path.name)
            item['url'] = link(args.download_base, path.name)
            successes[key] = item
        for item in report.get('failures', []):
            failures[(item['os'], item['arch'])] = item['error']
    if not successes:
        raise ValueError('no verified build assets found; refusing to create a release')
    assets = [successes.get(key) or dict(os=key[0], arch=key[1], name=f'capturefab-{ver}-{key[0]}-{key[1]}', available=False, reason=failures.get(key, 'Not built for this release')) for key in TARGETS]
    source = source_archive(output, ver)
    bill = sbom(output, ver, args.repository, any(item.get('browser_client') for item in successes.values()))
    signature = 'SHA256SUMS.asc' if args.gpg_key else None
    release = dict(version=ver, date=dt.datetime.now(dt.timezone.utc).date().isoformat(), url=f'https://github.com/{args.repository}/releases/tag/v{ver}', checksums_url=link(args.download_base, 'SHA256SUMS'), source_url=link(args.download_base, source.name), sbom_url=link(args.download_base, bill.name), assets=assets)
    if signature:
        release['checksums_signature_url'] = link(args.download_base, signature)
    (output / 'releases.json').write_text(json.dumps(dict(schema_version=1, latest=ver, releases=[release]), indent=2) + '\n')
    files = sorted(p for p in output.iterdir() if p.is_file() and not p.name.startswith('SHA256SUMS') and p.name != 'release-notes.md')
    (output / 'SHA256SUMS').write_text(''.join(f'{digest(p)}  {p.name}\n' for p in files))
    if signature:
        run(['gpg', '--batch', '--yes', '--pinentry-mode', 'loopback', '--passphrase-fd', '0', '--local-user', args.gpg_key, '--armor', '--detach-sign', '--output', output / signature, output / 'SHA256SUMS'], input=os.environ.get('RELEASE_GPG_PASSPHRASE', ''), text=True)
    unavailable = [f'- {a["os"]} {a["arch"]}: {a["reason"]}' for a in assets if not a['available']]
    notes = [f'Capturefab {ver}', '', 'One executable per platform: it opens the GUI with no arguments (or lists the CLI commands when no display is available) and provides the CLI and authenticated session automation.', '', 'Build artifacts:']
    notes += [f'- {key[0]} {key[1]}: {item["validation"]}, platform signature {item["platform_signature"]}' for key, item in successes.items()]
    if any(key[0] == 'macos' for key in successes):
        notes += ['- macOS: open the .dmg and drag Capturefab to Applications. Where the platform signature says apple-notarized, both the disk image and the app inside are notarized and stapled.',
                  '', 'Upgrade note:', '- The macOS download no longer includes the ./capturefab launcher; use /Applications/Capturefab.app/Contents/MacOS/capturefab or add that folder to PATH.']
    notes += ['', 'Unavailable targets:'] + unavailable if unavailable else []
    notes += ['', 'Verification:', '- Check downloads against SHA256SUMS, for example `sha256sum --check --ignore-missing SHA256SUMS`.']
    notes += [f'- SHA256SUMS.asc is an OpenPGP signature by key {args.gpg_key}: `gpg --verify SHA256SUMS.asc SHA256SUMS`.'] if signature else []
    notes += [f'- GitHub build provenance and an SBOM attestation cover every file in SHA256SUMS: `gh attestation verify FILE --repo {args.repository}` (add `--predicate-type https://cyclonedx.org/bom` for the SBOM).'] if args.provenance else []
    notes += [f'- {bill.name} is a CycloneDX software bill of materials: the Rust crates linked into the executable, with Cargo.lock checksums, and the pinned media components.']
    notes += ['- Apple Developer ID, notarization and Windows Authenticode apply only where the platform signature above says so.', '', f'Corresponding source: {source.name} contains the exact Capturefab commit and vendored Cargo.lock dependency sources with an offline build configuration; each *-media-sources.tar.gz holds the verified FFmpeg, x264, SRT, OpenSSL, zlib and enabled header archives with their build manifest. Packages list Rust crate licenses in THIRD-PARTY-LICENSES.txt (on macOS inside Capturefab.app/Contents/Resources/licenses).']
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
    build_parser.add_argument('--arch', choices=['x86_64', 'aarch64'], required=True)
    build_parser.add_argument('--ffmpeg-bundle', required=True)
    build_parser.add_argument('--binary', help='package an already built release executable')
    build_parser.add_argument('--repackage', action='store_true', help='sign/repackage verified archives already in --output without recompiling')
    build_parser.add_argument('--skip-smoke', action='store_true', help='cross build: label unexecuted target honestly')
    build_parser.add_argument('--output', default='dist')
    bump = commands.add_parser('set-version', help='raise the version in Cargo.toml and Cargo.lock')
    bump.add_argument('version')
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
        elif args.command == 'set-version':
            current, new = set_version(args.version)
            print(f'{current} -> {new}')
        elif args.command == 'build':
            if args.binary and args.repackage:
                raise ValueError('--binary and --repackage cannot be combined')
            build(args)
        else:
            if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repository):
                raise ValueError('repository must be OWNER/NAME')
            assemble(args)
    except (ValueError, RuntimeError, OSError) as error:
        parser.exit(1, f'release: {error}\n')

if __name__ == '__main__':
    main()
