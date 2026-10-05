#!/usr/bin/env python3
"""Run a real Capturefab → MediaMTX → Capturefab RTSP/SRT round trip.

Start MediaMTX with loopback RTSP :18554 and SRT :18890. The Capturefab
binary must embed FFmpeg or CAPTUREFAB_FFMPEG must point at a compatible build.
"""
import argparse
import json
import os
import pathlib
import socket
import subprocess
import tempfile
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--binary", default="target/debug/capturefab")
p.add_argument("--srt", action="store_true")
p.add_argument("--output")
a = p.parse_args()
root = pathlib.Path(a.output or tempfile.mkdtemp(prefix="capturefab-media-test-"))
root.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, CAPTUREFAB_SESSION_DIR=str(root / "sessions"))
binary = str(pathlib.Path(a.binary).resolve())
publish = "srt://127.0.0.1:18890?streamid=publish:capturefab-srt&pkt_size=1316" if a.srt else "rtsp://127.0.0.1:18554/capturefab-rtsp"
receive = "srt://127.0.0.1:18890?streamid=read:capturefab-srt" if a.srt else publish
log = (root / "forward.log").open("wb")
forward = subprocess.Popen([binary, "--json", "--camera", "sim:media-test", "forward", "-o", publish, "--encoder", "libx264"], env=env, stdout=log, stderr=subprocess.STDOUT)
try:
    for attempt in range(60):
        if forward.poll() is not None:
            raise RuntimeError((root / "forward.log").read_text())
        if a.srt:
            time.sleep(0.1)
            if attempt >= 20:
                break
        else:
            try:
                with socket.create_connection(("127.0.0.1", 18554), 0.3) as s:
                    s.sendall(f"DESCRIBE {receive} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n".encode())
                    if b"200 OK" in s.recv(4096):
                        break
            except OSError:
                pass
            time.sleep(0.1)
    else:
        raise RuntimeError("MediaMTX did not report an active stream")
    captured = subprocess.run([binary, "--json", "--timeout-ms", "10000", "--camera", receive, "capture", "-n", "3", "-o", str(root / "frames")], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=40)
    (root / "capture.json").write_bytes(captured.stdout)
    if captured.returncode:
        raise RuntimeError(captured.stdout.decode() + captured.stderr.decode())
    result = json.loads(captured.stdout)["result"]
    assert result["count"] == 3
    for f in result["frames"]:
        assert (f["width"], f["height"]) == (640, 480)
        assert f["bytes"] == 640 * 480 * 3
    for name in result["files"]:
        assert pathlib.Path(name).read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
    print(json.dumps({"ok": True, "protocol": "srt" if a.srt else "rtsp", "artifacts": str(root), "frames": result["frames"]}))
finally:
    if forward.poll() is None:
        forward.send_signal(__import__("signal").SIGINT)
        try:
            forward.wait(timeout=10)
        except subprocess.TimeoutExpired:
            forward.kill()
            forward.wait()
    log.close()
