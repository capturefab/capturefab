#!/usr/bin/env python3
"""Check read-only PCCC capture against unmodified libplctag ab_server/tag_rw2.

Development-only reference tools: libplctag, MPL-2.0, commit
7712ed7f1ee05ba0ebfca641b3f5c43da52a66ed (the tested revision).
Build ab_server and tag_rw2 from https://github.com/libplctag/libplctag.
No reference code or runtime is included in Capturefab.

This upstream simulator hardcodes its response requestor identity instead of
copying each request's identity. The positive reference check explicitly selects
that identity; this does not establish universal VID-zero or physical-device
compatibility. Capturefab's normal strict identity checks remain enabled.
The simulator also lacks extended 255+ logical-address parsing; those addresses
are covered by the CLI fixtures. The reference client seeds only the temporary
simulator; Capturefab only reads.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def run(command, **options):
    result = subprocess.run(command, check=False, capture_output=True, text=True,
                            timeout=20, **options)
    if result.returncode:
        raise RuntimeError(f'{command[0]} exited {result.returncode}:\n{result.stdout}\n{result.stderr}')
    return result.stdout


def port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/capturefab'))
    parser.add_argument('--server', type=Path, required=True, help='libplctag ab_server executable')
    parser.add_argument('--client', type=Path, required=True, help='libplctag tag_rw2 executable')
    args = parser.parse_args()
    binary, server, client = (path.resolve() for path in (args.binary, args.server, args.client))
    for path in (binary, server, client):
        if not path.is_file():
            parser.error(f'executable does not exist: {path}')
    for model in ('SLC500', 'Micrologix'):
        with tempfile.TemporaryDirectory(prefix='capturefab-pccc-reference-') as folder:
            folder = Path(folder)
            number = port()
            with (folder / 'server.log').open('w') as log:
                process = subprocess.Popen([str(server), f'--plc={model}', f'--port={number}',
                                            '--tag=N7[300]', '--tag=F8[10]', '--tag=B3[10]', '--tag=L19[10]'],
                                           stdout=log, stderr=subprocess.STDOUT)
                try:
                    end = time.monotonic() + 5
                    while True:
                        if process.poll() is not None:
                            raise RuntimeError((folder / 'server.log').read_text())
                        try:
                            with socket.create_connection(('127.0.0.1', number), timeout=0.1):
                                break
                        except OSError:
                            if time.monotonic() >= end:
                                raise RuntimeError('reference simulator did not start') from None
                            time.sleep(0.02)

                    def reference(name, kind, count, values=None):
                        tag = f'protocol=ab_eip&gateway=127.0.0.1:{number}&plc={model}&name={name}&elem_count={count}'
                        command = [str(client), f'--type={kind}', f'--tag={tag}', '--timeout=2000']
                        if values is not None:
                            command.append('--write=' + ','.join(map(str, values)))
                        output = run(command)
                        # Keep only data values, ignoring elapsed time and library diagnostics.
                        return [line for line in output.splitlines() if line.startswith('data[')]

                    reference('N7:10', 'sint16', 5, [-32768, 32767, -1, 0, 19])
                    reference('F8:0', 'real32', 2, [1.25, -12.5])
                    reference('B3:0', 'uint16', 1, [32768])
                    before = [reference('N7:10', 'sint16', 5), reference('F8:0', 'real32', 2),
                              reference('B3:0', 'uint16', 1)]
                    if model == 'Micrologix':
                        reference('L19:0', 'sint32', 2, [-2147483648, 2147483647])
                        reference('L19:2', 'uint32', 1, [4294967295])
                        before.extend([reference('L19:0', 'sint32', 2),
                                       reference('L19:2', 'uint32', 1)])
                    config = {'version': 1, 'sources': [{
                        'id': 'plc', 'protocol': 'pccc', 'address': f'127.0.0.1:{number}',
                        'vendor_id': 0xf33d, 'serial_number': 0x21504345,
                        'max_read_bytes': 4, 'every_ms': 20, 'timeout_ms': 1000,
                        'signals': [
                            {'name': 'integers', 'address': 'N7:10', 'elements': 5},
                            {'name': 'floats', 'address': 'F8:0', 'elements': 2},
                            {'name': 'bit', 'address': 'B3:0/15'},
                        ]}]}
                    if model == 'Micrologix':
                        config['sources'][0]['signals'].extend([
                            {'name': 'longs', 'address': 'L19:0', 'elements': 2},
                            {'name': 'unsigned_long', 'address': 'L19:2', 'data_type': 'u32'},
                        ])
                    (folder / 'sources.json').write_text(json.dumps(config))
                    environment = dict(os.environ, CAPTUREFAB_SESSION_DIR=str(folder / 'sessions'),
                                       CAPTUREFAB_CACHE_DIR=str(folder / 'cache'), CAPTUREFAB_KEEP='0')
                    output = run([str(binary), '--json', 'data', 'record', '--config', 'sources.json',
                                  '--for', '300ms', '-o', 'run.jsonl'], cwd=folder, env=environment)
                    summary = json.loads(output)
                    records = [json.loads(line) for line in (folder / 'run.jsonl').read_text().splitlines()]
                    samples = [record for record in records if record['kind'] == 'sample']
                    expected = {'integers': [-32768, 32767, -1, 0, 19],
                                'floats': [1.25, -12.5], 'bit': True}
                    if model == 'Micrologix':
                        expected.update(longs=[-2147483648, 2147483647], unsigned_long=4294967295)
                    assert samples and {sample['signal'] for sample in samples} == set(expected), summary
                    for sample in samples:
                        assert sample['value'] == expected[sample['signal']], sample
                    assert records[-1]['status'] == 'complete', records[-1]
                    after = [reference('N7:10', 'sint16', 5), reference('F8:0', 'real32', 2),
                             reference('B3:0', 'uint16', 1)]
                    if model == 'Micrologix':
                        after.extend([reference('L19:0', 'sint32', 2),
                                      reference('L19:2', 'uint32', 1)])
                    assert before == after and all(before), (before, after)
                    run([str(binary), '--json', 'data', 'export', 'run.jsonl', '-o', 'run.csv'],
                        cwd=folder, env=environment)
                    run([str(binary), '--json', 'data', 'export', 'run.jsonl', '-o', 'samples.jsonl',
                         '--format', 'jsonl'], cwd=folder, env=environment)
                    assert '-32768' in (folder / 'run.csv').read_text()
                    assert '-12.5' in (folder / 'samples.jsonl').read_text()
                    print(f'{model}: {len(samples)} samples, segmented arrays, float/bit values, '
                          'CSV/JSONL exports and unchanged simulator memory verified')
                finally:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
