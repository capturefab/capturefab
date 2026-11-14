#!/usr/bin/env python3
"""Probe unmodified pinned libplctag and optionally compare native capture.

Optional POSIX development check (Linux/macOS), Python 3.9+.
MPL-2.0 development tools, pinned as scripts/test-pccc.py. Only tag_rw2 seeds
an owned temporary simulator. Capturefab and the wire probe only read values.
The stock ControlLogix simulator caps full response packets at504 bytes; no
parser, capacity, route or callback source is patched for this check.
Use --micro800 to compare the stock Micro800 Class 3 client and server.
Its registration requires context zero; this is a simulator restriction, not a
claim that nonzero opaque registration contexts violate EtherNet/IP.
"""
import argparse
import csv
import json
import os
from pathlib import Path
import re
import signal
import socket
import struct
import subprocess
import tempfile
import time

PIN = '7712ed7f1ee05ba0ebfca641b3f5c43da52a66ed'

def signal_group(process, sig):
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass

def run(command, **options):
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True, **options)
    try:
        stdout, stderr = process.communicate(timeout=20)
        if process.returncode:
            raise RuntimeError(f'{command[0]} exited {process.returncode}:\n{stdout}\n{stderr}')
        return stdout
    finally:
        # The group can outlive its leader and keep communicate() pipes open.
        signal_group(process, signal.SIGKILL)
        try:
            process.wait(timeout=5)
        finally:
            process.stdout.close()
            process.stderr.close()

def port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]

class Wire:
    def __init__(self, number):
        self.deadline = time.monotonic() + 5
        self.stream = socket.create_connection(('127.0.0.1', number), timeout=2)
        self.session = 0
        self.context = 0
        try:
            header, body = self.exchange(0x65, b'\x01\x00\x00\x00')
            self.session = header[2]
            assert self.session and body == b'\x01\x00\x00\x00'
        except BaseException:
            self.stream.close()
            raise

    def remaining(self):
        left = self.deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError('one wire probe deadline expired')
        return left

    def exact(self, count):
        data = bytearray()
        while len(data) < count:
            self.stream.settimeout(self.remaining())
            part = self.stream.recv(count - len(data))
            assert part, 'reference closed inside a reply'
            data.extend(part)
        return bytes(data)

    def exchange(self, command, body):
        if command != 0x65:
            self.context += 1
        self.stream.settimeout(self.remaining())
        self.stream.sendall(struct.pack('<HHIIQI', command, len(body), self.session, 0, self.context, 0) + body)
        header = struct.unpack('<HHIIQI', self.exact(24))
        assert header[0] == command and header[3] == 0 and header[4] == self.context and header[5] == 0
        if command != 0x65:
            assert header[2] == self.session
        assert header[1] <= 480, ('stock504-byte packet cap exceeded', header)
        return header, self.exact(header[1])

    def cip(self, request, routed=True):
        if routed:
            request = (b'\x52\x02\x20\x06\x24\x01\x0a\x0e' + struct.pack('<H', len(request)) + request
                       + (b'\x00' if len(request) % 2 else b'') + b'\x01\x00\x01\x00')
        body = struct.pack('<IHHHHHH', 0, 0, 2, 0, 0, 0xb2, len(request)) + request
        header, response = self.exchange(0x6f, body)
        assert len(response) >= 16 and response[:14] == struct.pack('<IHHHHH', 0, 0, 2, 0, 0, 0xb2)
        assert struct.unpack_from('<H', response, 14)[0] == len(response) - 16
        return response[16:], header[1] + 24

    def array(self, name, elements, code, fmt, routed=True):
        width = struct.calcsize(fmt)
        path = bytes([0x91, len(name)]) + name.encode('ascii')
        if len(name) % 2:
            path += b'\x00'
        path += b'\x28\x03'
        data = bytearray()
        evidence = []
        while True:
            service = 0x4c if not evidence else 0x52
            request = bytes([service, len(path) // 2]) + path + struct.pack('<H', elements)
            if evidence:
                request += struct.pack('<I', len(data))
            response, packet_bytes = self.cip(request, routed)
            assert len(response) >= 6
            assert response[0] == service | 0x80, ('reference used a different response service', response.hex())
            assert response[1] == response[3] == 0 and response[2] in (0, 6)
            assert struct.unpack_from('<H', response, 4)[0] == code
            payload = response[6:]
            assert payload and len(data) + len(payload) <= elements * width
            evidence.append({'request_service': hex(service), 'reply_service': hex(response[0]),
                             'status': response[2], 'offset': len(data), 'payload_bytes': len(payload),
                             'packet_bytes': packet_bytes, 'raw_cip_prefix_hex': response[:14].hex()})
            data.extend(payload)
            if response[2] == 0:
                assert len(data) == elements * width
                break
            assert len(data) < elements * width
        assert len(evidence) >= 3, 'large array did not force multiple independent replies'
        return list(struct.unpack('<' + str(elements) + fmt, data)), evidence

    def close(self):
        try:
            # Unregister has no reply; never let cleanup extend the wire budget.
            self.stream.setblocking(False)
            try:
                self.stream.send(struct.pack('<HHIIQI', 0x66, 0, self.session, 0, 0, 0))
            except OSError:
                pass
        finally:
            self.stream.close()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server', type=Path, required=True)
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--binary', type=Path, help='candidate native CLI to compare after its build')
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--micro800', action='store_true', help='use the stock connected Micro800 profile')
    args = parser.parse_args()
    server, client = args.server.resolve(), args.client.resolve()
    assert server.is_file() and client.is_file()
    definitions = [('Counts', 'DINT', 'sint32', 0xc4, 'i', [-2147483648 + index * 3 for index in range(300)]),
                   ('Precise', 'LINT', 'sint64', 0xc5, 'q', [-9223372036854775808 + index for index in range(300)]),
                   ('Pressure', 'REAL', 'real32', 0xca, 'f', [index / 4 for index in range(300)])]
    model = 'Micro800' if args.micro800 else 'ControlLogix'
    route = [] if args.micro800 else ['--path=1,0']
    report = {'reference_commit': PIN, 'reference_modified': False, 'model': model, 'response_packet_cap': 504,
              'wire': {}, 'native_compared': bool(args.binary)}
    with tempfile.TemporaryDirectory(prefix='capturefab-cip-probe-') as folder:
        folder = Path(folder)
        number = port()
        log = (folder / 'server.log').open('w')
        process = subprocess.Popen([str(server), f'--plc={model}', f'--port={number}', *route]
                                   + [f'--tag={name}:{kind}[300]' for name, kind, *_ in definitions],
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            until = time.monotonic() + 5
            while True:
                if process.poll() is not None:
                    raise RuntimeError((folder / 'server.log').read_text())
                try:
                    with socket.create_connection(('127.0.0.1', number), timeout=0.1):
                        break
                except OSError:
                    assert time.monotonic() < until, 'reference server startup timed out'
                    time.sleep(0.02)

            def reference(name, kind, count, values=None):
                tag_route = '&use_connected_msg=1' if args.micro800 else '&path=1,0&use_connected_msg=0'
                tag = (f'protocol=ab_eip&gateway=127.0.0.1:{number}&plc={model}{tag_route}'
                       f'&name={name}&elem_count={count}&allow_packing=0')
                command = [str(client), f'--type={kind}', f'--tag={tag}', '--timeout=5000']
                if values is not None:
                    command.append('--write=' + ','.join(map(str, values)))
                output = run(command)
                if values is not None:
                    return None
                selected = []
                for line in output.splitlines():
                    match = re.fullmatch(r'data\[\d+\]=([^ ]+)(?: .*)?', line)
                    if match:
                        selected.append(float(match[1]) if kind == 'real32' else int(match[1]))
                assert len(selected) == count, (name, len(selected), output[-2000:])
                return selected

            before = {}
            for name, _, kind, code, fmt, seeded in definitions:
                reference(name + '[0]', kind, 300, seeded)
                before[name] = reference(name + '[3]', kind, 256)
                assert before[name] == seeded[3:259]
                if not args.micro800:
                    wire = Wire(number)
                    try:
                        decoded, evidence = wire.array(name, 256, code, fmt)
                    finally:
                        wire.close()
                    assert decoded == before[name]
                    report['wire'][name] = evidence

            if args.binary:
                binary = args.binary.resolve()
                assert binary.is_file()
                native_route = {'profile': 'micro800'} if args.micro800 else {'slot': 0}
                config = {'version': 1, 'sources': [{'id': 'plc', 'protocol': 'ethernet_ip', **native_route,
                          'address': f'127.0.0.1:{number}', 'every_ms': 3600000, 'timeout_ms': 2000,
                          'signals': [{'name': name, 'tag': name + '[3]', 'elements': 256} for name in before]}]}
                (folder / 'sources.json').write_text(json.dumps(config))
                environment = dict(os.environ, CAPTUREFAB_SESSION_DIR=str(folder / 'sessions'),
                                   CAPTUREFAB_CACHE_DIR=str(folder / 'cache'), CAPTUREFAB_KEEP='0')
                run([str(binary), '--json', 'data', 'record', '--config', 'sources.json', '--for', '1s', '-o', 'run.jsonl'],
                    cwd=folder, env=environment)
                records = [json.loads(line) for line in (folder / 'run.jsonl').read_text().splitlines()]
                samples = [row for row in records if row['kind'] == 'sample']
                assert len(samples) == len(before) and records[-1]['status'] == 'complete'
                for sample in samples:
                    assert sample['value'] == before[sample['signal']], sample
                run([str(binary), '--json', 'data', 'export', 'run.jsonl', '-o', 'values.csv'], cwd=folder, env=environment)
                run([str(binary), '--json', 'data', 'export', 'run.jsonl', '-o', 'values.jsonl', '--format', 'jsonl'],
                    cwd=folder, env=environment)
                with (folder / 'values.csv').open(newline='') as output:
                    exported_csv = {
                        row['signal']: json.loads(row['value_json'])
                        for row in csv.DictReader(output)
                    }
                assert exported_csv == before
                exported = [json.loads(line) for line in (folder / 'values.jsonl').read_text().splitlines()]
                assert {row['signal']: row['value'] for row in exported} == before
                report['native_samples'] = len(samples)
            for name, _, kind, _, _, _ in definitions:
                assert reference(name + '[3]', kind, 256) == before[name], 'simulator memory changed after read-only acquisition'
            report['memory_unchanged'] = True
        finally:
            try:
                signal_group(process, signal.SIGTERM)
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    signal_group(process, signal.SIGKILL)
                    process.wait(timeout=3)
                finally:
                    signal_group(process, signal.SIGKILL)
            finally:
                log.close()
                args.report.with_suffix('.server.log').write_text((folder / 'server.log').read_text()[-12000:])
    args.report.write_text(json.dumps(report, indent=2))
    print(json.dumps(report))

if __name__ == '__main__':
    main()
