#!/usr/bin/env python3
"""Check S7 capture/export against an independent python-snap7 reference server.

Optional development check, never a runtime dependency. Install python-snap7==3.2.1
in a development environment, then run this script with a built Capturefab binary.
The server binds loopback on an ephemeral port; no PLC or root access is needed.
"""
import argparse
import base64
import csv
from datetime import timedelta
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import tempfile


def file_digest(path):
    digest = hashlib.sha256()
    with path.open('rb') as file:
        for block in iter(lambda: file.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def reference_sources():
    distribution = importlib.metadata.distribution('python-snap7')
    hashes = {}
    for entry in distribution.files or []:
        if entry.suffix != '.py' or entry.hash is None:
            continue
        digest = file_digest(Path(distribution.locate_file(entry)))
        assert entry.hash.mode == 'sha256', entry
        encoded = base64.urlsafe_b64encode(bytes.fromhex(digest)).decode('ascii').rstrip('=')
        assert encoded == entry.hash.value, f'reference source differs from wheel: {entry}'
        hashes[str(entry)] = digest
    assert hashes, 'reference wheel has no verifiable Python source records'
    return hashes


def command_output(command, *, cwd, env):
    process = subprocess.Popen(command, cwd=cwd, env=env, text=True,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=os.name == 'posix')
    try:
        stdout, stderr = process.communicate(timeout=20)
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    finally:
        # A worker can outlive a completed group leader. Clean the owned group
        # after success as well as timeout; S7 acquisition itself uses threads.
        if os.name == 'posix':
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        elif process.poll() is None:
            process.kill()
        process.communicate(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/capturefab'))
    parser.add_argument('--report', type=Path, help='write the completed reference comparison report')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    try:
        from snap7.server import Server
        from snap7.type import SrvArea
        from snap7.client import Client
        from snap7.util.getters import get_s5time, get_string, get_wstring
    except ImportError:
        parser.exit(1, 'Install optional test dependency python-snap7==3.2.1 first.\n')

    # Independent server memory uses standard struct encodings, not Capturefab's
    # decoder or protocol fixtures. Include every scalar type and memory area.
    version = importlib.metadata.version('python-snap7')
    if version != '3.2.1':
        parser.exit(1, f'This reference check requires python-snap7==3.2.1; found {version}.\n')
    source_hashes = reference_sources()
    db = bytearray(256 * 1024)
    cases = [
        ('byte', 'u8', 'B', 250), ('signed_byte', 'i8', 'b', -125),
        ('word', 'u16', 'H', 65000), ('signed_word', 'i16', 'h', -30000),
        ('dword', 'u32', 'I', 4294967294), ('signed_dword', 'i32', 'i', -2147483647),
        ('counter', 'u64', 'Q', 18446744073709551614),
        ('signed_counter', 'i64', 'q', -9223372036854775807),
        ('real', 'f32', 'f', 12.5), ('double', 'f64', 'd', -0.125),
    ]
    signals = []
    expected = {}
    offset = 0
    for name, kind, encoding, value in cases:
        struct.pack_into('>' + encoding, db, offset, value)
        signals.append({'name': name, 'db': 1, 'address': offset, 'data_type': kind})
        expected[name] = value
        offset += struct.calcsize(encoding)
    signals += [
        {'name': 'input', 'area': 'input', 'address': 0, 'data_type': 'bool', 'bit': 7},
        {'name': 'output', 'area': 'output', 'address': 1, 'data_type': 'bool', 'bit': 3},
        {'name': 'merker', 'area': 'merker', 'address': 0, 'data_type': 'i16', 'scale': 0.01},
    ]
    expected.update(input=True, output=False, merker=-1.25)
    timers = bytearray(64)
    counters = bytearray(64)
    struct.pack_into('>H', timers, 2 * 7, 0x2127)
    struct.pack_into('>H', counters, 2 * 21, 0x0999)
    struct.pack_into('>HH', db, 120, 0x0054, 0x3999)
    signals.extend([
        {'name': 'timer_ms', 'area': 'timer', 'address': 7, 'data_type': 's5time'},
        {'name': 'counter_word', 'area': 'counter', 'address': 21},
        {'name': 'short_preset_ms', 'db': 1, 'address': 120, 'data_type': 's5time'},
        {'name': 'long_preset_ms', 'db': 1, 'address': 122, 'data_type': 's5time'},
    ])
    expected.update(timer_ms=127000, counter_word=0x0999,
                    short_preset_ms=540, long_preset_ms=9990000)
    text_cases = []
    offset = 128

    def add_text(name, kind, capacity, payload, encoding=None):
        nonlocal offset
        units = len(payload) if kind == 'string' else len(payload) // 2
        header = bytes([capacity, units]) if kind == 'string' else struct.pack('>HH', capacity, units)
        storage = capacity + 2 if kind == 'string' else capacity * 2 + 4
        assert len(header) + len(payload) <= storage
        slot = header + payload + bytes([0xa5]) * (storage - len(header) - len(payload))
        assert offset + storage <= len(db)
        db[offset:offset + storage] = slot
        signal = dict(name=name, db=1, address=offset, data_type=kind, capacity=capacity)
        if encoding is not None:
            signal['encoding'] = encoding
        signals.append(signal)
        selected = 'utf16be' if kind == 'wstring' else encoding
        value = dict(kind='s7_text', data_type=kind, capacity=capacity, length=units,
                     storage_bytes=storage, decode_status='raw_only',
                     raw=dict(encoding='base64', scope='header_and_active',
                              bytes=len(header) + len(payload),
                              data=base64.b64encode(header + payload).decode('ascii')))
        if selected is not None:
            value['encoding'] = selected
            codec = {'windows1252': 'cp1252', 'utf16be': 'utf-16-be'}.get(selected, selected)
            try:
                value['text'] = payload.decode(codec, errors='strict')
            except UnicodeDecodeError:
                value['decode_status'] = 'invalid_encoding'
            else:
                value['decode_status'] = 'decoded'
        expected[name] = value
        text_cases.append((name, kind, offset, storage, capacity, encoding, bytes(slot)))
        offset += storage + 2

    add_text('ascii', 'string', 24, b'line\x00,1\n', 'ascii')
    add_text('zero_capacity', 'string', 0, b'', 'ascii')
    add_text('empty', 'string', 5, b'', 'ascii')
    add_text('maximum_string', 'string', 254, b'X' * 253 + b'\x00', 'ascii')
    add_text('western', 'string', 16, b'caf\xe9 \x80 \x93OK\x94', 'windows1252')
    add_text('western_undefined', 'string', 4, b'\x81', 'windows1252')
    add_text('utf8', 'string', 32, '部品α'.encode('utf-8'), 'utf8')
    add_text('utf8_invalid', 'string', 8, b'\xff\xc3', 'utf8')
    add_text('raw_only', 'string', 4, b'\xff\x81')
    add_text('ascii_invalid', 'string', 8, b'\x80', 'ascii')
    add_text('wide', 'wstring', 300, '温度\x00Привет\n,'.encode('utf-16-be'))
    add_text('wide_invalid', 'wstring', 4, b'\xd8\x00')
    add_text('wide_empty', 'wstring', 4, b'')
    add_text('wide_zero_capacity', 'wstring', 0, b'')
    add_text('maximum_wstring', 'wstring', 65534, ('A' * 65534).encode('utf-16-be'))
    memory_before = bytes(db)
    indexed_before = (bytes(timers), bytes(counters))
    server = Server(log=False)
    for area, index, data in [
        (SrvArea.DB, 1, db), (SrvArea.PE, 0, bytearray([0x80, 0])),
        (SrvArea.PA, 0, bytearray([0xff, 0xf7])),
        (SrvArea.MK, 0, bytearray(struct.pack('>h', -125))),
        (SrvArea.TM, 0, timers), (SrvArea.CT, 0, counters),
    ]:
        server.register_area(area, index, data)
    server.start_to('127.0.0.1', tcp_port=0)
    try:
        port = server.server_socket.getsockname()[1]
        reference = Client()
        getter_checked = 0
        try:
            reference.connect('127.0.0.1', 0, 2, tcp_port=port)
            timer_word = reference.tm_read(7, 1)
            assert timer_word == timers[14:16], timer_word
            counter_word = reference.ct_read(21, 1)
            assert counter_word == counters[42:44], counter_word
            assert int.from_bytes(counter_word, 'big') == expected['counter_word']
            for name, value in [
                ('timer_ms', timer_word),
                ('short_preset_ms', reference.db_read(1, 120, 2)),
                ('long_preset_ms', reference.db_read(1, 122, 2)),
            ]:
                assert get_s5time(value, 0) == str(timedelta(milliseconds=expected[name])), name
            for name, kind, start, storage, capacity, encoding, slot in text_cases:
                actual = bytes(reference.db_read(1, start, storage))
                assert actual == slot, name
                value = expected[name]
                if value['decode_status'] == 'raw_only':
                    continue
                # The pinned getter compares WSTRING capacity *bytes* to16382.
                # Keep its source unchanged; validate the larger slot by the
                # independent client's exact read plus Python's UTF16 decoder.
                if kind == 'wstring' and capacity * 2 > 16382:
                    length = struct.unpack_from('>H', actual, 2)[0]
                    assert actual[4:4 + length * 2].decode('utf-16-be') == value['text']
                    continue
                try:
                    if kind == 'wstring':
                        decoded = get_wstring(bytearray(actual), 0)
                    else:
                        codec = 'cp1252' if encoding == 'windows1252' else encoding
                        decoded = get_string(bytearray(actual), 0, encoding=codec)
                except UnicodeDecodeError:
                    assert value['decode_status'] == 'invalid_encoding', name
                else:
                    assert value['decode_status'] == 'decoded' and decoded == value['text'], name
                getter_checked += 1
        finally:
            reference.disconnect()
        with tempfile.TemporaryDirectory(prefix='capturefab-s7-reference-') as folder:
            work = Path(folder)
            env = dict(os.environ, CAPTUREFAB_SESSION_DIR=str(work / 'sessions'),
                       CAPTUREFAB_CACHE_DIR=str(work / 'cache'), CAPTUREFAB_KEEP='0')
            for key in ['CAPTUREFAB_SESSION', 'CAPTUREFAB_CAMERA', 'CAPTUREFAB_JSON']:
                env.pop(key, None)

            def run(*arguments):
                result = command_output([str(binary), '--json', *arguments], cwd=work, env=env)
                if result.returncode:
                    raise AssertionError(f'{arguments}: {result.stdout}\n{result.stderr}')
                return json.loads(result.stdout)

            config = {'version': 1, 'sources': [{
                'id': 'plc', 'protocol': 's7', 'address': f'127.0.0.1:{port}',
                'every_ms': 3_600_000, 'timeout_ms': 5000, 'signals': signals,
            }]}
            (work / 'sources.json').write_text(json.dumps(config))
            result = run('data', 'record', '--config', 'sources.json', '--for', '1500ms',
                         '-o', 'run.jsonl')
            assert result['status'] == 'complete', result
            rows = [json.loads(line) for line in (work / 'run.jsonl').read_text().splitlines()]
            assert rows[0]['sources'] == [{'id': 'plc', 'protocol': 's7'}], rows[0]
            samples = [row for row in rows if row['kind'] == 'sample']
            assert len(samples) == len(expected), (len(samples), len(expected))
            assert {row['signal'] for row in samples} == set(expected), samples
            for row in samples:
                assert type(row['value']) is type(expected[row['signal']]), row
                assert row['value'] == expected[row['signal']], row
            for form in ['jsonl', 'csv']:
                exported = run('data', 'export', 'run.jsonl', '--format', form,
                               '-o', f'samples.{form}')
                assert exported['complete'] and exported['samples'] == len(samples), exported
            csv.field_size_limit(2 << 20)
            with (work / 'samples.csv').open(newline='') as file:
                exported_csv = list(csv.DictReader(file))
            assert len(exported_csv) == len(samples)
            assert {row['signal'] for row in exported_csv} == set(expected)
            for row in exported_csv:
                assert json.loads(row['value_json']) == expected[row['signal']], row
            exported_jsonl = [json.loads(line) for line in (work / 'samples.jsonl').read_text().splitlines()]
            assert len(exported_jsonl) == len(samples)
            assert {row['signal'] for row in exported_jsonl} == set(expected)
            for row in exported_jsonl:
                assert row['value'] == expected[row['signal']], row
            assert bytes(db) == memory_before, 'read-only capture changed reference DB memory'
            assert (bytes(timers), bytes(counters)) == indexed_before, 'capture changed indexed memory'
            assert reference_sources() == source_hashes
            report = dict(reference='python-snap7', version=version, source_modified=False,
                          source_matches_wheel_records=True, reference_source_files=len(source_hashes),
                          reference_sources_sha256=hashlib.sha256(json.dumps(source_hashes, sort_keys=True).encode()).hexdigest(),
                          signals=len(expected), text_signals=len(text_cases), samples=len(samples),
                          reference_indexed_words_checked=2, reference_s5time_values_checked=3,
                          reference_text_slots_read=len(text_cases), reference_getters_checked=getter_checked,
                          all_csv_jsonl_values_checked=True, memory_unchanged=True,
                          db_sha256=hashlib.sha256(memory_before).hexdigest(),
                          binary_sha256=file_digest(binary),
                          script_sha256=file_digest(Path(__file__)),
                          reference_getter_limit='WSTRING byte-capacity check excludes65534-unit getter; full raw read and stdlib decode checked')
            if args.report is not None:
                args.report.write_text(json.dumps(report, indent=2) + '\n')
            print(f'S7 reference python-snap7 {version}: '
                  f'{len(expected)} typed signals, {len(samples)} samples, CSV/JSONL export passed.')
    finally:
        server.stop()


if __name__ == '__main__':
    if os.name == 'posix':
        def terminate(_signum, _frame):
            # Let an outer whole-run watchdog unwind command/server cleanup.
            raise SystemExit('S7 reference check interrupted by watchdog')
        signal.signal(signal.SIGTERM, terminate)
    main()
