#!/usr/bin/env python3
"""Run industrial capture tests in an isolated Linux Docker container.

The source tree and Cargo registry are read-only mounts. Sources and build output
are copied into the disposable container, so this cannot replace host test
executables. The container has no external network and only CAP_NET_RAW; raw
EtherCAT/PROFINET fixtures transmit exclusively on its loopback interface.
--can also grants CAP_NET_ADMIN to create vcan0 inside that network namespace;
the CAN fixtures never use host interfaces, devices, or module-loading privileges.

Run `cargo fetch --locked` first to populate the local registry. The cached
`rust:1` image needs no extra packages for these tests. MQTT TLS is included;
JPEG/video optional-feature tests use the normal platform-specific test jobs.
"""
import argparse
import os
from pathlib import Path
import subprocess
import uuid


ROOT = Path(__file__).resolve().parents[1]
COMMAND = r'''
set -euo pipefail
mkdir -p /cargo/registry /work
tar -C /source -cf - Cargo.toml Cargo.lock build.rs README.md LICENSE .cargo assets src tests benches docs vendor |
    tar --no-same-owner -C /work -xf -
cp -R /host-registry/index /host-registry/cache /cargo/registry/
ln -sfn /host-registry/src /cargo/registry/src
cd /work
features=()
if [[ -n "$CAPTUREFAB_LINUX_TEST_FEATURES" ]]; then
    features=(--features "$CAPTUREFAB_LINUX_TEST_FEATURES")
fi
rustc --version
uname -a
cargo test --offline --locked --no-default-features "${features[@]}" --lib \
    --test data --test data_helper --test data_devices --test data_modbus_tls --test data_jsonl --test data_mqtt \
    --test data_opcua --test data_precision --test data_s7 \
    --test data_ads --test data_slmp --test data_mc_1e --test data_ifm_iot --test data_fins \
    --test data_omron_host_link \
    --test data_pccc --test data_iolink_json --test data_scpi --test data_ethernet_ip \
    --test data_ethernet_ip_assembly --test gentl \
    --test data_camera --test data_camera_precision --test data_video \
    --test data_fieldbus --test data_can
cargo test --offline --locked --no-default-features "${features[@]}" --test data_fieldbus -- --ignored
if [[ "$CAPTUREFAB_CAN_TEST_ISOLATED" == "1" ]]; then
    cargo test --offline --locked --no-default-features "${features[@]}" --test data_can -- --ignored
fi
if [[ "$CAPTUREFAB_LINUX_TEST_CLIPPY" == "1" ]]; then
    cargo clippy --offline --locked --no-default-features "${features[@]}" --all-targets -- -D warnings
fi
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', default='rust:1', help='Linux Rust image with a C compiler')
    parser.add_argument('--features', default='mqtt-tls,gentl', help='optional Cargo features; empty disables all')
    parser.add_argument('--clippy', action='store_true', help='also run strict Clippy; image must include it')
    parser.add_argument('--can', action='store_true',
                        help='also test classic/FD SocketCAN on container-created vcan0 (needs kernel vcan support)')
    cargo_home = Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo'))
    parser.add_argument('--cargo-registry', type=Path, default=cargo_home / 'registry')
    args = parser.parse_args()
    registry = args.cargo_registry.expanduser().resolve()
    if not all((registry / part).is_dir() for part in ['index', 'cache', 'src']):
        parser.error('Cargo registry is incomplete; run cargo fetch --locked first')
    name = f'capturefab-linux-industrial-{uuid.uuid4().hex[:12]}'
    command = [
        'docker', 'run', '--rm', '--init', '--name', name,
        '--ulimit', 'core=0',
        '--network', 'none', '--cap-drop', 'ALL', '--cap-add', 'NET_RAW',
        '--mount', f'type=bind,source={ROOT},target=/source,readonly',
        '--mount', f'type=bind,source={registry},target=/host-registry,readonly',
        '--env', 'CARGO_HOME=/cargo', '--env', 'CARGO_TARGET_DIR=/build',
        '--env', 'CAPTUREFAB_FIELDBUS_TEST_ISOLATED=1',
        '--env', f'CAPTUREFAB_CAN_TEST_ISOLATED={int(args.can)}',
        '--env', f'CAPTUREFAB_LINUX_TEST_FEATURES={args.features}',
        '--env', f'CAPTUREFAB_LINUX_TEST_CLIPPY={int(args.clippy)}',
    ]
    if args.can:
        command.extend(['--cap-add', 'NET_ADMIN'])
    command.extend([args.image, 'bash', '-c', COMMAND])
    try:
        return subprocess.run(command, check=False).returncode
    finally:
        # On interruption, Docker's client can exit before its container does.
        # Remove only the uniquely named container created by this invocation.
        subprocess.run(['docker', 'rm', '-f', name], check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == '__main__':
    raise SystemExit(main())
