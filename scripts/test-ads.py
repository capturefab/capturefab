#!/usr/bin/env python3
"""Check ADS capture/export against the independent pyads reference server.

Optional development check, never a runtime dependency. Install pyads==3.6.0
in a development environment, then run this script with a built Capturefab binary.
The unmodified pyads AdvancedHandler and AdsTestServer bind an ephemeral loopback
port; no PLC, TwinCAT installation, route changes or controller writes are needed.
"""
import argparse
import csv
import importlib.metadata
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/capturefab'))
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    try:
        from pyads import constants
        from pyads.testserver import AdsTestServer, AdvancedHandler, PLCVariable
    except ImportError:
        parser.exit(1, 'Install optional test dependency pyads==3.6.0 first.\n')

    handler = AdvancedHandler()
    counter = 2**64 - 1
    signed = -(2**63)
    label = '温度😀'
    labels = ['café', '']
    wide_labels = ['温', 'é']
    handler.add_variable(PLCVariable('MAIN.counter', struct.pack('<Q', counter),
                                    constants.ADST_UINT64, 'ULINT',
                                    index_group=0x4020, index_offset=16))
    handler.add_variable(PLCVariable('MAIN.signed', struct.pack('<q', signed),
                                    constants.ADST_INT64, 'LINT',
                                    index_group=0x4020, index_offset=24))
    handler.add_variable(PLCVariable('MAIN.wide', (label + '\0').encode('utf-16-le'),
                                    constants.ADST_WSTRING, 'WSTRING(4)',
                                    index_group=0x4020, index_offset=32))
    handler.add_variable(PLCVariable('MAIN.labels', b'caf\xe9\0\xff\0stale',
                                    constants.ADST_STRING, 'ARRAY [1..2] OF STRING(5)',
                                    index_group=0x4020, index_offset=64))
    wide_slots = b''.join(struct.pack('<4H', ord(text), 0, 0xd800, 0xffff)
                         for text in wide_labels)
    handler.add_variable(PLCVariable('MAIN.wide_labels', wide_slots,
                                    constants.ADST_WSTRING, 'ARRAY [1..2] OF WSTRING(3)',
                                    index_group=0x4020, index_offset=96))
    server = AdsTestServer(handler=handler, port=0, logging=False)
    port = server.server.getsockname()[1]
    server.start()
    try:
        with tempfile.TemporaryDirectory(prefix='capturefab-ads-reference-') as folder:
            work = Path(folder)
            env = dict(os.environ, CAPTUREFAB_SESSION_DIR=str(work / 'sessions'),
                       CAPTUREFAB_CACHE_DIR=str(work / 'cache'), CAPTUREFAB_KEEP='0')
            for key in ['CAPTUREFAB_SESSION', 'CAPTUREFAB_CAMERA', 'CAPTUREFAB_JSON',
                        'CAPTUREFAB_CAMERA_PASSWORD', 'CAPTUREFAB_STATE_DIR']:
                env.pop(key, None)

            def run(*arguments):
                result = subprocess.run([str(binary), '--json', *arguments], cwd=work,
                                        env=env, text=True, capture_output=True, timeout=20)
                if result.returncode:
                    raise AssertionError(f'{arguments}: {result.stdout}\n{result.stderr}')
                return json.loads(result.stdout)

            config = {'version': 1, 'sources': [{
                'id': 'plc', 'protocol': 'ads', 'address': f'127.0.0.1:{port}',
                'target_net_id': '192.0.2.10.1.1', 'source_net_id': '192.0.2.20.1.1',
                'every_ms': 3600000, 'timeout_ms': 1000, 'signals': [
                    {'name': 'direct', 'index_group': 0x4020, 'index_offset': 16,
                     'data_type': 'u64'},
                    {'name': 'symbolic', 'symbol': 'MAIN.signed', 'data_type': 'i64'},
                    {'name': 'wide', 'symbol': 'MAIN.wide', 'data_type': 'wstring',
                     'length': 10},
                    {'name': 'labels', 'index_group': 0x4020, 'index_offset': 64,
                     'data_type': 'string', 'length': 6, 'elements': 2, 'encoding': 'latin1'},
                    {'name': 'wide_labels', 'symbol': 'MAIN.wide_labels',
                     'data_type': 'wstring', 'length': 8, 'elements': 2},
                ],
            }]}
            (work / 'sources.json').write_text(json.dumps(config))
            result = run('data', 'record', '--config', 'sources.json', '--for', '500ms',
                         '-o', 'run.jsonl')
            assert result['status'] == 'complete', result
            rows = [json.loads(line) for line in (work / 'run.jsonl').read_text().splitlines()]
            expected = {'direct': counter, 'symbolic': signed, 'wide': label,
                        'labels': labels, 'wide_labels': wide_labels}
            samples = [row for row in rows if row['kind'] == 'sample']
            assert {row['signal']: row['value'] for row in samples} == expected, samples
            assert len(samples) == len(expected), samples
            requests = [(struct.unpack('<H', request.ams_header.command_id)[0],
                         struct.unpack('<I', request.ams_header.data[:4])[0])
                        for request in server.request_history]
            assert requests == [(2, 0x4020), (9, 0xf003), (2, 0xf005),
                                (9, 0xf003), (2, 0xf005), (2, 0x4020),
                                (9, 0xf003), (2, 0xf005),
                                (3, 0xf006), (3, 0xf006), (3, 0xf006)], requests
            for form in ['jsonl', 'csv']:
                exported = run('data', 'export', 'run.jsonl', '--format', form,
                               '-o', f'samples.{form}')
                assert exported['complete'] and exported['samples'] == len(samples), exported
            exported = [json.loads(line) for line in (work / 'samples.jsonl').read_text().splitlines()]
            assert {row['signal']: row['value'] for row in exported} == expected, exported
            with (work / 'samples.csv').open(newline='') as file:
                for row in csv.DictReader(file):
                    assert json.loads(row['value_json']) == expected[row['signal']], row
            print(f'ADS reference pyads {importlib.metadata.version("pyads")}: '
                  'direct and symbolic exact 64-bit reads, Latin-1 and UTF-16 arrays, handle release, '
                  'CSV/JSONL export passed.')
    finally:
        # pyads closes its listener before its select loop exits in stop().
        # Stop that loop first to avoid its harmless teardown EBADF traceback.
        server._run = False
        server.join(timeout=2)
        server.close()


if __name__ == '__main__':
    main()
