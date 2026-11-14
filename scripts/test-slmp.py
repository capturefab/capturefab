#!/usr/bin/env python3
"""Check SLMP capture/export against independent VirtualPLC and pymcprotocol.

Optional development check, never a runtime dependency. With Python 3.10+,
install virtual-plc==0.5.0 and pymcprotocol==0.3.0, then run this script against a
built Capturefab binary. Both reference implementations remain unmodified.
Loopback simulation checks interoperability, not physical PLC qualification.
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
        from pymcprotocol import Type3E
        from virtual_plc import Memory, Server
    except ImportError:
        parser.exit(1, 'Install virtual-plc==0.5.0 and pymcprotocol==0.3.0 with Python 3.10+.\n')

    for profile, device_format in [('Q', 'standard'), ('iQ-R', 'iq_r')]:
        memory = Memory(profile)
        pressure = list(struct.unpack('<HH', struct.pack('<f', 12.5)))
        memory.load({'D': {'100': [0, 32767, 65535], '200': [65535] * 4,
                           '204': [0, 0, 0, 32768], '210': pressure},
                     'M': {'10': [1, 0, 1, 0, 1]}})
        with Server(profile=profile, memory=memory) as server:
            # A separate client also decodes the simulator's values before the
            # native CLI reads them. Signed word returns reflect its public API.
            client = Type3E(plctype=profile)
            client.connect(*server.address)
            try:
                assert client.batchread_wordunits('D100', 3) == [0, 32767, -1]
                assert client.batchread_wordunits('D200', 4) == [-1] * 4
                assert client.batchread_wordunits('D204', 4) == [0, 0, 0, -32768]
                assert client.batchread_bitunits('M10', 5) == [1, 0, 1, 0, 1]
            finally:
                client.close()
            before = memory.dump()
            with tempfile.TemporaryDirectory(prefix='capturefab-slmp-reference-') as folder:
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
                    'id': 'plc', 'protocol': 'slmp',
                    'address': '{}:{}'.format(*server.address), 'device_format': device_format,
                    'every_ms': 3600000, 'timeout_ms': 1000, 'signals': [
                        {'name': 'array', 'device': 'D', 'address': 100, 'elements': 3},
                        {'name': 'counter', 'device': 'D', 'address': 200, 'data_type': 'u64'},
                        {'name': 'signed', 'device': 'D', 'address': 204, 'data_type': 'i64'},
                        {'name': 'pressure', 'device': 'D', 'address': 210, 'data_type': 'f32'},
                        {'name': 'flags', 'device': 'M', 'address': 10, 'data_type': 'bool',
                         'elements': 5},
                    ],
                }]}
                (work / 'sources.json').write_text(json.dumps(config))
                result = run('data', 'record', '--config', 'sources.json', '--for', '500ms',
                             '-o', 'run.jsonl')
                assert result['status'] == 'complete', result
                expected = {'array': [0, 32767, 65535], 'counter': 2**64 - 1,
                            'signed': -(2**63), 'pressure': 12.5,
                            'flags': [True, False, True, False, True]}
                rows = [json.loads(line) for line in (work / 'run.jsonl').read_text().splitlines()]
                samples = [row for row in rows if row['kind'] == 'sample']
                assert len(samples) == len(expected), samples
                assert {row['signal']: row['value'] for row in samples} == expected, samples
                for form in ['jsonl', 'csv']:
                    exported = run('data', 'export', 'run.jsonl', '--format', form,
                                   '-o', f'samples.{form}')
                    assert exported['complete'] and exported['samples'] == len(samples), exported
                with (work / 'samples.csv').open(newline='') as file:
                    for row in csv.DictReader(file):
                        assert json.loads(row['value_json']) == expected[row['signal']], row
            assert memory.dump() == before, 'capture changed PLC memory'
    print(f'SLMP references VirtualPLC {importlib.metadata.version("virtual-plc")} / '
          f'pymcprotocol {importlib.metadata.version("pymcprotocol")}: Q and iQ-R '
          '3E binary reads, word/bit arrays, exact 64-bit, CSV/JSONL export passed.')


if __name__ == '__main__':
    main()
