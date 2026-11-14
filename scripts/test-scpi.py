#!/usr/bin/env python3
"""Check native SCPI capture/export against the upstream scpi-parser TCP demo.

Optional development check; Python 3.9+ and a POSIX C compiler (Linux/macOS).
No runtime dependency or physical qualification.
Uses BSD-2-Clause scpi-parser revision 886615902b2777b71860445305bfe5a3faef4f3e:
https://github.com/j123b567/scpi-parser/tree/886615902b2777b71860445305bfe5a3faef4f3e
The parser, instrument callbacks and license remain unmodified. Only the demo's
transport is adapted: loopback address, a temporary port, accept-loop backoff,
and disabling Linux-only TCP_CORK on macOS. A C compiler builds one temporary
executable.

The script checks DMM/identity/version queries, error-queue dequeue semantics,
and the upstream TEST:ARBitrary? definite-block echo. All sources, executable,
recordings and artifacts are removed on exit. --archive permits offline reuse
of the small verified archive; an explicitly supplied archive is never deleted.
"""
import argparse
import base64
import csv
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.request


REVISION = '886615902b2777b71860445305bfe5a3faef4f3e'
ARCHIVE_SHA256 = '00437ac3aaae361ce4f3859f37dde16d6f24db206ec15220c3f036c7455d1120'
ARCHIVE_URL = f'https://codeload.github.com/j123b567/scpi-parser/tar.gz/{REVISION}'
MAX_ARCHIVE = 1 << 20
CORE = ['error', 'expression', 'fifo', 'ieee488', 'lexer', 'minimal', 'parser', 'units', 'utils']
HEADERS = ['cc', 'config', 'constants', 'error', 'expression', 'ieee488', 'minimal',
           'parser', 'scpi', 'types', 'units', 'utils']
FILES = ['LICENSE', 'examples/test-tcp/main.c', 'examples/common/scpi-def.c',
         'examples/common/scpi-def.h']
FILES += [f'libscpi/src/{name}.c' for name in CORE]
FILES += [f'libscpi/src/{name}_private.h' for name in ['fifo', 'lexer', 'parser', 'utils']]
FILES += [f'libscpi/inc/scpi/{name}.h' for name in HEADERS]


def stop_process(process):
    # The compiler may have children. Keep them in this script's own group so
    # a timeout or interrupted run retires them before deleting temporary files.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        return process.communicate(timeout=2)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        return process.communicate(timeout=5)


def checked_run(command, **options):
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True, **options)
    try:
        stdout, stderr = process.communicate(timeout=45)
    except BaseException:
        stop_process(process)
        raise
    if process.returncode:
        raise RuntimeError(f'{command[0]} exited {process.returncode}:\n{stdout}\n{stderr}')
    return stdout


def prepare_reference(work, archive_path, compiler):
    if archive_path:
        if archive_path.stat().st_size > MAX_ARCHIVE:
            raise RuntimeError('SCPI reference archive exceeds 1 MiB')
        data = archive_path.read_bytes()
    else:
        request = urllib.request.Request(ARCHIVE_URL,
                                         headers={'User-Agent': 'Capturefab reference check'})
        with urllib.request.urlopen(request, timeout=30) as response:
            data = response.read(MAX_ARCHIVE + 1)
    if len(data) > MAX_ARCHIVE or hashlib.sha256(data).hexdigest() != ARCHIVE_SHA256:
        raise RuntimeError('SCPI reference archive size or pinned SHA256 differs')
    source = work / 'reference'
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
        for name in FILES:
            member = archive.getmember(f'scpi-parser-{REVISION}/{name}')
            if not member.isfile() or member.size > 128 * 1024:
                raise RuntimeError(f'invalid reference source member: {name}')
            target = source / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(archive.extractfile(member).read())
    main = source / 'examples/test-tcp/main.c'
    text = main.read_text()
    adaptations = {
        'htonl(INADDR_ANY)': 'htonl(INADDR_LOOPBACK)',
        'listenfd = createServer(5025);': 'listenfd = createServer(atoi(argv[1]));',
        'if (clifd < 0) continue;': 'if (clifd < 0) { usleep(1000); continue; }',
    }
    for before, after in adaptations.items():
        if text.count(before) != 1:
            raise RuntimeError(f'upstream transport adaptation no longer matches: {before}')
        text = text.replace(before, after)
    if platform.system() == 'Darwin':
        # TCP_NOPUSH does not flush like Linux TCP_CORK. The accepted socket
        # already uses TCP_NODELAY, so leave corking disabled on this platform.
        cork = 'setsockopt(fd, IPPROTO_TCP, TCP_CORK, &state, sizeof(state));'
        if text.count(cork) != 2:
            raise RuntimeError('upstream TCP_CORK adaptation no longer matches')
        text = text.replace(cork, '(void) state;')
    main.write_text(text)
    server = work / 'scpi-reference-server'
    command = [compiler, '-std=c99', '-D_DEFAULT_SOURCE', '-O2']
    command += ['-I', str(source / 'libscpi/inc'), str(main),
                str(source / 'examples/common/scpi-def.c')]
    command += [str(source / f'libscpi/src/{name}.c') for name in CORE]
    checked_run(command + ['-lm', '-o', str(server)])
    return server


def temporary_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


def socket_budget(connection, deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError('SCPI reference query exceeded its total deadline')
    connection.settimeout(remaining)


def read_exact(connection, size, deadline):
    reply = bytearray()
    while len(reply) < size:
        socket_budget(connection, deadline)
        part = connection.recv(size - len(reply))
        if not part:
            raise RuntimeError('SCPI reference reply ended before its declared size')
        reply.extend(part)
    return bytes(reply)


def read_line(connection, limit, deadline):
    reply = bytearray()
    while len(reply) < limit:
        reply.extend(read_exact(connection, 1, deadline))
        if reply.endswith(b'\n'):
            return bytes(reply)
    raise RuntimeError('SCPI reference reply exceeds its line limit')


def reference_query(address, query, block=False):
    deadline = time.monotonic() + 2
    with socket.create_connection(address, timeout=2) as connection:
        socket_budget(connection, deadline)
        connection.sendall(query.encode('ascii') + b'\n')
        if block:
            first = read_exact(connection, 2, deadline)
            assert first[:1] == b'#' and first[1:2] in b'123456789'
            digits = read_exact(connection, int(first[1:2]), deadline)
            assert digits.isdigit()
            length = int(digits)
            assert length <= 64 * 1024
            payload = read_exact(connection, length, deadline)
            end = read_exact(connection, 2, deadline)
            assert end == b'\r\n'
            return first + digits + payload + end
        reply = read_line(connection, 64 * 1024, deadline)
        assert reply.endswith(b'\r\n'), reply
        return reply


def seed_error(address):
    # Only the temporary upstream simulator receives this undefined query.
    # The count query verifies parsing completed without dequeuing the error.
    deadline = time.monotonic() + 2
    with socket.create_connection(address, timeout=2) as connection:
        socket_budget(connection, deadline)
        connection.sendall(b'UNDEFINED:HEADER?\nSYST:ERR:COUNT?\n')
        assert int(read_line(connection, 128, deadline)) == 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/capturefab'))
    parser.add_argument('--archive', type=Path, help='offline copy of the pinned source tar.gz')
    parser.add_argument('--cc', default='cc', help='C compiler for the temporary reference server')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    compiler = shutil.which(args.cc)
    if compiler is None:
        parser.error(f'C compiler not found: {args.cc}')
    archive = args.archive.resolve(strict=True) if args.archive else None
    with tempfile.TemporaryDirectory(prefix='capturefab-scpi-reference-') as directory:
        work = Path(directory).resolve()
        server = prepare_reference(work, archive, compiler)
        address = ('127.0.0.1', temporary_port())
        environment = dict(os.environ, CAPTUREFAB_SESSION_DIR=str(work / 'sessions'),
                           CAPTUREFAB_CACHE_DIR=str(work / 'cache'), CAPTUREFAB_KEEP='0')
        for key in ['CAPTUREFAB_SESSION', 'CAPTUREFAB_CAMERA', 'CAPTUREFAB_JSON',
                    'CAPTUREFAB_CAMERA_PASSWORD', 'CAPTUREFAB_STATE_DIR']:
            environment.pop(key, None)

        def run(*arguments):
            return json.loads(checked_run([str(binary), '--json', *arguments],
                                          cwd=work, env=environment))

        def capture(name, signals):
            config = {'version': 1, 'sources': [{
                'id': 'meter', 'protocol': 'scpi',
                'connection': {'transport': 'tcp', 'address': '{}:{}'.format(*address)},
                'every_ms': 3_600_000, 'timeout_ms': 1000,
                'signals': [dict(signal, response_terminator='crlf') for signal in signals],
            }]}
            (work / 'sources.json').write_text(json.dumps(config))
            summary = run('data', 'record', '--config', 'sources.json', '--for', '500ms',
                          '-o', f'{name}.jsonl')
            rows = [json.loads(line) for line in (work / f'{name}.jsonl').read_text().splitlines()]
            samples = [row for row in rows if row['kind'] == 'sample']
            assert len(samples) == len(signals) and summary['status'] == 'complete', summary
            assert rows[-1]['status'] == 'complete', rows[-1]
            for form in ['csv', 'jsonl']:
                exported = run('data', 'export', f'{name}.jsonl', '--format', form,
                               '-o', f'{name}-samples.{form}')
                assert exported['complete'] and exported['samples'] == len(samples), exported
            with (work / f'{name}-samples.csv').open(newline='') as output:
                values = {row['signal']: json.loads(row['value_json'])
                          for row in csv.DictReader(output)}
            expected = {row['signal']: row['value'] for row in samples}
            assert values == expected
            exported = [json.loads(line) for line in
                        (work / f'{name}-samples.jsonl').read_text().splitlines()]
            assert {row['signal']: row['value'] for row in exported} == expected
            return expected

        with (work / 'server.log').open('w') as log:
            process = subprocess.Popen([str(server), str(address[1])], stdout=log,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            try:
                deadline = time.monotonic() + 5
                while True:
                    if process.poll() is not None:
                        raise RuntimeError((work / 'server.log').read_text())
                    try:
                        with socket.create_connection(address, timeout=0.1):
                            break
                    except OSError:
                        if time.monotonic() >= deadline:
                            raise RuntimeError('upstream SCPI TCP demo did not start') from None
                        time.sleep(0.02)
                queries = {'identity': '*IDN?', 'voltage': 'MEAS:VOLT:DC? 10,0.001',
                           'version': 'SYST:VERS?', 'errors': 'SYST:ERR?'}
                raw = {}
                for name, query in queries.items():
                    try:
                        raw[name] = reference_query(address, query)
                    except Exception as error:
                        raise RuntimeError(f'upstream SCPI reference query {query!r}: {error}') from error
                signals = [{'name': name, 'query': query,
                            'format': 'text' if name == 'identity' else
                                      'error_queue' if name == 'errors' else 'number'}
                           for name, query in queries.items()]
                signals[1]['unit'] = 'V'
                values = capture('lines', signals)
                for name, value in values.items():
                    assert value['kind'] == 'scpi_reply' and value['query'] == queries[name]
                    assert isinstance(value['received_monotonic_ns'], int)
                    assert value['received_monotonic_ns'] > 0
                    assert value['raw']['encoding'] == 'base64'
                    assert base64.b64decode(value['raw']['data'], validate=True) == raw[name]
                assert values['identity']['value'] == raw['identity'].decode().rstrip('\r\n')
                assert values['voltage']['value'] == 0 and values['voltage']['unit'] == 'V'
                assert values['version']['value'] == 1999
                code, message = next(csv.reader([raw['errors'].decode().rstrip('\r\n')]))
                assert values['errors']['value'] == {'code': int(code), 'message': message}
                assert values['errors']['status'] == 'no_error'

                query = 'TEST:ARB? #2100123456789'
                raw_block = reference_query(address, query, block=True)
                assert raw_block == b'#2100123456789\r\n'
                block = capture('block', [{'name': 'trace', 'query': query,
                                         'format': 'binary_block', 'output': 'trace-{seq}.bin',
                                         'data_type': 'u8', 'byte_order': 'little'}])['trace']
                assert block['raw']['encoding'] == 'file'
                artifact = Path(block['raw']['path'])
                if not artifact.is_absolute():
                    artifact = work / artifact
                assert artifact.resolve().is_relative_to(work)
                assert artifact.read_bytes() == raw_block
                assert block['raw']['bytes'] == len(raw_block)
                assert block['raw']['sha256'] == hashlib.sha256(raw_block).hexdigest()
                assert block['block'] == {'header': '#210', 'payload_offset': 4,
                                          'payload_bytes': 10, 'data_type': 'u8',
                                          'elements': 10, 'byte_order': 'little'}
                metadata_path = Path(block['metadata_path'])
                assert metadata_path.resolve().is_relative_to(work)
                metadata = json.loads(metadata_path.read_text())
                assert metadata['format_version'] == 1 and metadata['signal'] == 'trace'
                assert metadata['response'] == block

                seed_error(address)
                expected_error = reference_query(address, 'SYST:ERR?')
                code, message = next(csv.reader([expected_error.decode().rstrip('\r\n')]))
                assert int(code) == -113 and message.startswith('Undefined header')
                # The upstream parser can append device-dependent information.
                # Compare its full reply, then seed the same error for capture.
                seed_error(address)
                error = capture('error', [{'name': 'errors', 'query': 'SYST:ERR?',
                                          'format': 'error_queue'}])['errors']
                assert error['status'] == 'instrument_error' and error['value']['code'] == -113
                assert error['value'] == {'code': int(code), 'message': message}, error
                error_raw = base64.b64decode(error['raw']['data'], validate=True)
                assert error_raw == expected_error
                assert int(reference_query(address, 'SYST:ERR:COUNT?')) == 0
                print(f'scpi-parser {REVISION}: identity/version/DMM queries, exact raw replies, '
                      'error queue, definite block artifact and CSV/JSONL exports verified')
            except Exception as error:
                diagnostic = (work / 'server.log').read_text(errors='replace')[-8192:]
                raise RuntimeError(f'{error}\nUpstream SCPI demo diagnostics:\n{diagnostic}') from error
            finally:
                stop_process(process)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
