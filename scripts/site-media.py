#!/usr/bin/env python3
"""Record genuine simulator workflows from the native renderer for the site.

Requires a desktop GUI build, Pillow, ffmpeg and ffprobe. This uses isolated
session data and built-in simulator cameras. It captures only app-rendered
pixels, never a display, physical camera, saved user session or generated UI.

    cargo build --bin capturefab
    python3 scripts/site-media.py --binary target/debug/capturefab
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

from PIL import Image, ImageChops, ImageStat


ROOT = Path(__file__).resolve().parents[1]
CLIPS = {
    "live-preview": {
        "cameras": 1,
        "scene": "dark,features,noscopes",
        "description": "A built-in simulator camera streams in its window. Real CLI commands change its exposure, then restore it.",
        "actions": [
            (1.5, ["--camera", "sim:0", "set", "ExposureTime=30000"]),
            (5.0, ["--camera", "sim:0", "set", "ExposureTime=10000"]),
        ],
        "poster_time": 0.5,
    },
    "capture-workflow": {
        "cameras": 1,
        "scene": "dark,capture,noscopes",
        "description": "The Capture panel stays visible while real session CLI commands save three simulator frames, then one PNG. The app shows the saved result.",
        "actions": [
            (2.0, ["--camera", "sim:0", "capture", "-n", "3", "-o", "{temp}/frames"]),
            (5.0, ["--camera", "sim:0", "capture", "-o", "{temp}/capture.png"]),
        ],
        "poster_time": 3.2,
    },
}


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command, **kwargs):
    return subprocess.run(command, check=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, **kwargs)


def record(binary, scene, temp, frames, fps):
    env = dict(os.environ)
    for key in ("CAPTUREFAB_SESSION", "CAPTUREFAB_EXECUTABLE", "CAPTUREFAB_CAMERA"):
        env.pop(key, None)
    env.update(CAPTUREFAB_SESSION_DIR=str(temp / "sessions"),
               CAPTUREFAB_CACHE_DIR=str(temp / "cache"),
               CAPTUREFAB_SCREENSHOT_SCENE=scene["scene"],
               CAPTUREFAB_SCREENSHOT_SIZE="1100x700",
               CAPTUREFAB_SCREENSHOT_FRAMES=str(frames),
               CAPTUREFAB_SCREENSHOT_FPS=str(fps))
    log_path = temp / "renderer.log"
    commands = []
    with log_path.open("wb") as log:
        process = subprocess.Popen([str(binary), "gui", "--name", "website-media",
                                    "--screenshot", str(temp / "frame.png"),
                                    "--screenshot-cameras", str(scene["cameras"])],
                                   env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 90
            started = None
            pending = list(scene["actions"])
            while process.poll() is None:
                if time.monotonic() > deadline:
                    raise RuntimeError("Renderer sequence exceeded its 90-second deadline")
                if started is None and (temp / "frame-0000.png").exists():
                    started = time.monotonic()
                if started is not None and pending and time.monotonic() - started >= pending[0][0]:
                    _, arguments = pending.pop(0)
                    arguments = [part.replace("{temp}", str(temp)) for part in arguments]
                    command = [str(binary), "--json", "--session", "website-media", "--actor", "Website demo", *arguments]
                    result = run(command, env=env, timeout=15)
                    value = json.loads(result.stdout)["result"]
                    saved = None
                    if "capture" in arguments:
                        saved = int(arguments[arguments.index("-n") + 1]) if "-n" in arguments else 1
                        if value["count"] != saved or len(value["files"]) != saved:
                            raise RuntimeError("Actual capture result did not match requested frame count")
                        for file in value["files"]:
                            if not Path(file).read_bytes().startswith(b"\x89PNG\r\n\x1a\n"):
                                raise RuntimeError("Actual capture did not save a PNG frame")
                    commands.append({"after_seconds": round(time.monotonic() - started, 3),
                                     "arguments": [part.replace(str(temp), "{isolated-demo}") for part in arguments],
                                     "verified_saved_frames": saved})
                time.sleep(0.025)
            if process.returncode:
                raise RuntimeError(log_path.read_text())
            if pending:
                raise RuntimeError("Renderer ended before all real session actions completed")
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    paths = sorted(temp.glob("frame-*.png"))
    if len(paths) != frames:
        raise RuntimeError(f"Expected {frames} genuine renderer frames, found {len(paths)}")
    return paths, commands


def encode(paths, output, duration, poster_time, temp):
    # Preserve observed wall-clock intervals between renderer frames. The
    # 24-fps delivery stream repeats frames; it never interpolates fake UI.
    times = [path.stat().st_mtime_ns / 1e9 for path in paths]
    intervals = [later - earlier for earlier, later in zip(times, times[1:])]
    if min(intervals) <= 0:
        raise RuntimeError("Renderer frame timestamps must increase")
    source_duration = times[-1] - times[0]
    if source_duration < duration:
        raise RuntimeError(f"Only {source_duration:.2f}s of real render frames for {duration:.2f}s clip; increase --frames")
    listing = temp / "frames.txt"
    listing.write_text("".join(f"file '{path.as_posix()}'\nduration {interval:.9f}\n"
                               for path, interval in zip(paths[:-1], intervals))
                       + f"file '{paths[-1].as_posix()}'\n")
    run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "concat", "-safe", "0",
         "-i", str(listing), "-t", str(duration), "-vf", "scale=1100:700:flags=lanczos,fps=24",
         "-an", "-c:v", "libx264", "-preset", "slow", "-crf", "21", "-pix_fmt", "yuv420p",
         "-movflags", "+faststart", "-y", str(output)], timeout=90)
    poster = output.with_suffix(".png")
    run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-ss", str(poster_time),
         "-i", str(output), "-frames:v", "1", "-y", str(poster)], timeout=20)
    with Image.open(poster) as image:
        image.save(poster, optimize=True)
    return source_duration


def verify(path, temp):
    probe = json.loads(run(["ffprobe", "-v", "error", "-show_streams", "-show_format",
                            "-of", "json", str(path)], timeout=20).stdout)
    streams = probe["streams"]
    if len(streams) != 1 or streams[0]["codec_name"] != "h264" or streams[0]["pix_fmt"] != "yuv420p":
        raise RuntimeError(f"Expected one silent H.264/yuv420p video stream: {path}")
    video = streams[0]
    duration = float(probe["format"]["duration"])
    differences = []
    previous = None
    for index, at in enumerate((0.5, duration / 2, duration - 0.5)):
        sample = temp / f"sample-{index}.png"
        run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-ss", str(at), "-i", str(path),
             "-frames:v", "1", "-y", str(sample)], timeout=20)
        with Image.open(sample) as frame:
            # Exclude the side panels, toolbar and text counters. Movement must
            # be in the actual camera preview, not just a changing frame rate.
            preview = frame.crop((300, 80, 800, 600)).convert("L")
            if previous is not None:
                diff = ImageChops.difference(previous, preview)
                mean = ImageStat.Stat(diff).mean[0]
                changed = 1 - diff.histogram()[0] / (diff.width * diff.height)
                if mean < 0.5 or changed < 0.05:
                    raise RuntimeError(f"Camera preview does not visibly change in {path}")
                differences.append({"mean_pixel_difference": round(mean, 3),
                                    "changed_fraction": round(changed, 4)})
            previous = preview.copy()
    return {"width": video["width"], "height": video["height"], "duration_seconds": duration,
            "fps": 24, "codec": "h264", "pixel_format": "yuv420p", "audio": False,
            "preview_frame_differences": differences}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/capturefab")
    parser.add_argument("--output", type=Path, default=ROOT / "web/media")
    parser.add_argument("--frames", type=int, default=64)
    parser.add_argument("--fps", type=int, default=12, help="Maximum renderer capture rate; actual timestamps are preserved")
    parser.add_argument("--duration", type=float, default=8)
    args = parser.parse_args()
    if not 2 <= args.frames <= 240 or not 1 <= args.fps <= 15 or not 3 <= args.duration <= 12:
        parser.error("frames must be 2–240, fps 1–15 and duration 3–12 seconds")
    for tool in ("ffmpeg", "ffprobe"):
        if not shutil.which(tool):
            parser.error(f"{tool} is required")
    binary = args.binary.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    manifest = {
        "method": "Actual iced desktop renderer frames; H.264 encoding only. No reconstructed interface or desktop recording.",
        "source_note": "Current development desktop build from workspace source. The visible save and settings results come from real session CLI commands.",
        "binary_sha256": sha256(binary),
        "demo": "Built-in simulator cameras in isolated temporary sessions. No physical cameras or saved user data.",
        "clips": [],
    }
    for name, scene in CLIPS.items():
        with tempfile.TemporaryDirectory(prefix=f"capturefab-{name}-") as directory:
            temp = Path(directory)
            paths, commands = record(binary, scene, temp, args.frames, args.fps)
            path = output / f"{name}.mp4"
            source_duration = encode(paths, path, args.duration, scene["poster_time"], temp)
            metadata = verify(path, temp)
            poster = path.with_suffix(".png")
            # Persist only generic arguments and actual capture counts, never
            # temporary authenticated session descriptors or absolute paths.
            actions = [{key: value for key, value in action.items() if value is not None}
                       for action in commands]
            manifest["clips"].append({
                "file": path.name, "poster": poster.name, "sha256": sha256(path),
                "poster_sha256": sha256(poster), "size_bytes": path.stat().st_size,
                **metadata,
                "source": {"method": "Built-in renderer screenshot sequence with real simulator acquisition and session CLI commands",
                           "scene": scene["scene"], "cameras": scene["cameras"], "frames": len(paths),
                           "duration_seconds": round(source_duration, 3), "maximum_fps": args.fps,
                           "observed_fps": round((len(paths) - 1) / source_duration, 3),
                           "playback_excerpt": {"start_seconds": 0, "end_seconds": args.duration, "speed": 1},
                           "first_frame_sha256": sha256(paths[0]), "last_frame_sha256": sha256(paths[-1]),
                           "actions": actions},
                "description": scene["description"],
            })
            print(f"{path.name}: {metadata['width']} × {metadata['height']}, {metadata['duration_seconds']:.2f}s, {path.stat().st_size:,} bytes", flush=True)
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
