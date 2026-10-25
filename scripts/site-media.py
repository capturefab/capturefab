#!/usr/bin/env python3
"""Record genuine simulator workflows from the native renderer for the site.

Runs on a desktop with a display, not in CI. Requires Pillow and the pinned
FFmpeg from scripts/build-ffmpeg.sh (or --ffmpeg); FFprobe is not needed. This
uses isolated session data and built-in simulator cameras. It captures only
app-rendered pixels, never a display, physical camera, saved user session or
generated UI.

    uv run --with pillow scripts/site-media.py

Without --binary the script builds an optimized desktop binary with
`cargo build --release` first: an unoptimized build cannot read renderer
frames back fast enough for the 24 frames/s the clips require.

The renderer captures frames in real time at up to 30 fps. Frames are timed by
the renderer's own staging clock, only ever repeated (never interpolated), and
the clips are rejected unless at least 24 frames/s were captured, no gap
exceeded 100 ms and the camera preview shows new frames.
"""

import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
OUTPUT_FPS = 30
MIN_CAPTURE_FPS = 24
MAX_GAP_MS = 100
MIN_DISTINCT_FRACTION = 0.8
CLIP_BYTE_BUDGET = 1_500_000
TOTAL_BYTE_BUDGET = 3_500_000
CRF_LADDER = (21, 23, 25, 27)
CODED_CHANGE_BYTES = 400
CONCAT_TIMEBASE = 90000
WIDTH, HEIGHT = 1100, 700
# Frames are checked at half scale, in gray.
CHECK_WIDTH, CHECK_HEIGHT = WIDTH // 2, HEIGHT // 2
# The camera preview, without side panels, toolbar or text counters.
PREVIEW_REGION = (150, 40, 400, 300)
# Text-heavy columns: the left sidebar and the right inspector. A staged
# frame whose text was not drawn before readback comes out blank here.
TEXT_REGIONS = ((5, 20, 140, 270), (410, 20, 545, 340))
MIN_PREVIEW_DIFFERENCE = 0.5
METHOD = ("Actual iced desktop renderer frames captured in real time at up to 30 fps; frames are only "
          "repeated, never interpolated; H.264 encoding only. Acquisition is real time, so frames "
          "differ between recordings.")

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


class MediaError(RuntimeError):
    """A recording that must not replace the published clips."""


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def app_version(binary):
    """The version the recording binary reports, or None when it cannot say."""
    try:
        result = subprocess.run([str(binary), "--version"], capture_output=True, text=True, timeout=20, check=True)
    except (OSError, subprocess.SubprocessError):
        return None
    return result.stdout.strip().removeprefix("capturefab ").strip() or None


def run(command, **kwargs):
    return subprocess.run(command, check=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, **kwargs)


def load_test_site():
    spec = importlib.util.spec_from_file_location("test_site", Path(__file__).with_name("test-site.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# FFmpeg ---------------------------------------------------------------------

def host_triple():
    output = run(["rustc", "-vV"], text=True, timeout=30).stdout
    for line in output.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    raise MediaError("rustc -vV did not name the host target")


def resolve_ffmpeg(explicit=None, environ=os.environ, root=ROOT, triple=None):
    """The FFmpeg to encode with: --ffmpeg, then CAPTUREFAB_FFMPEG, then the
    pinned build in target/ffmpeg/<host>/bin. A PATH ffmpeg must be explicit."""
    if explicit:
        return Path(explicit)
    if environ.get("CAPTUREFAB_FFMPEG"):
        return Path(environ["CAPTUREFAB_FFMPEG"])
    path = root / "target/ffmpeg" / (triple or host_triple()) / "bin/ffmpeg"
    if not path.is_file():
        raise MediaError(f"No pinned FFmpeg at {path}; run scripts/build-ffmpeg.sh, or pass --ffmpeg")
    return path


def pinned_ffmpeg_version(script=ROOT / "scripts/build-ffmpeg.sh"):
    match = re.search(r"^FFMPEG_VERSION=(\S+)$", script.read_text(), re.M)
    return match.group(1) if match else None


def check_ffmpeg(path, runner=run):
    """Refuse a nonfree or x264-less FFmpeg; say whether it is the pinned build."""
    version = runner([str(path), "-hide_banner", "-version"], text=True, timeout=20).stdout
    buildconf = runner([str(path), "-hide_banner", "-buildconf"], text=True, timeout=20).stdout
    encoders = runner([str(path), "-hide_banner", "-encoders"], text=True, timeout=20).stdout
    if "--enable-nonfree" in buildconf:
        raise MediaError(f"{path} is a nonfree FFmpeg build; its output could not be redistributed")
    if not re.search(r"^\s*V\S*\s+libx264\s", encoders, re.M):
        raise MediaError(f"{path} has no libx264 encoder")
    first = version.splitlines()[0].strip() if version.strip() else "unknown"
    pinned = pinned_ffmpeg_version()
    return {
        "ffmpeg_version": first,
        "ffmpeg_pinned": bool(pinned) and first.startswith(f"ffmpeg version {pinned} ")
        and "--disable-ffprobe" in buildconf
        and "--enable-encoder='rawvideo,ppm,wrapped_avframe,ffv1,libx264'" in buildconf,
    }


# Recording ------------------------------------------------------------------

def build_binary():
    """Build the optimized desktop binary the clips are recorded with."""
    subprocess.run(["cargo", "build", "--release", "--bin", "capturefab"], cwd=ROOT, check=True)
    target = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target")
    if not target.is_absolute():
        target = ROOT / target
    return target / "release/capturefab"


def sequence_timeout(frames, fps):
    """Mirror of `ScreenshotSequence::timeout` in src/gui/scene.rs."""
    return 30 + (max(1 / fps, 0.25) * frames if frames > 1 else 0)


def deadline_seconds(frames, fps):
    # The app reports its own timeout first; this only bounds a hung process.
    return sequence_timeout(frames, fps) + 20


def read_sidecar_lines(path):
    """Complete JSON lines written so far."""
    try:
        text = path.read_text()
    except FileNotFoundError:
        return []
    complete = text[:text.rfind("\n") + 1]
    return [json.loads(line) for line in complete.splitlines() if line.strip()]


def parse_sidecar(path, frames, directory):
    """Every frame 0..frames-1 exactly once, each naming a bare file that exists."""
    entries = read_sidecar_lines(path)
    indexes = sorted(entry.get("index") for entry in entries)
    if indexes != list(range(frames)):
        missing = sorted(set(range(frames)) - set(indexes))
        duplicates = sorted({index for index in indexes if indexes.count(index) > 1})
        raise MediaError(f"Renderer timing lists {len(indexes)} of {frames} frames"
                         f" (missing {missing[:5]}, duplicated {duplicates[:5]})")
    for entry in entries:
        file = entry.get("file")
        if not isinstance(file, str) or Path(file).name != file or "/" in file or "\\" in file:
            raise MediaError(f"Renderer timing names a non-local frame file: {file!r}")
        if not (directory / file).is_file():
            raise MediaError(f"Renderer frame is missing: {file}")
        if not isinstance(entry.get("staged_ms"), (int, float)):
            raise MediaError(f"Renderer timing for frame {entry['index']} has no staging time")
    return sorted(entries, key=lambda entry: entry["index"])


def rebase(entries):
    """Drop frame 0, whose interval includes one-time scene setup, and time
    the rest from frame 1's staging."""
    rest = [dict(entry) for entry in entries if entry["index"] != 0]
    if not rest:
        raise MediaError("Renderer captured no frames after the first")
    origin = rest[0]["staged_ms"]
    for entry in rest:
        entry["staged_ms"] = round(entry["staged_ms"] - origin, 3)
    return rest


def record(binary, scene, temp, frames, fps):
    env = dict(os.environ)
    for key in ("CAPTUREFAB_SESSION", "CAPTUREFAB_EXECUTABLE", "CAPTUREFAB_CAMERA"):
        env.pop(key, None)
    for directory in ("sessions", "cache", "shots"):
        (temp / directory).mkdir(mode=0o700)
    env.update(CAPTUREFAB_SESSION_DIR=str(temp / "sessions"),
               CAPTUREFAB_CACHE_DIR=str(temp / "cache"),
               CAPTUREFAB_EFFICIENCY="smooth",
               CAPTUREFAB_SCREENSHOT_SCENE=scene["scene"],
               CAPTUREFAB_SCREENSHOT_SIZE=f"{WIDTH}x{HEIGHT}",
               CAPTUREFAB_SCREENSHOT_FRAMES=str(frames),
               CAPTUREFAB_SCREENSHOT_FPS=str(fps))
    shots = temp / "shots"
    sidecar = shots / "frame-timing.jsonl"
    log_path = temp / "renderer.log"
    commands = []
    limit = deadline_seconds(frames, fps)
    with log_path.open("wb") as log:
        process = subprocess.Popen([str(binary), "gui", "--name", "website-media",
                                    "--screenshot", str(shots / "frame.png"),
                                    "--screenshot-cameras", str(scene["cameras"])],
                                   env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + limit
            origin = None
            pending = list(scene["actions"])
            while process.poll() is None:
                if time.monotonic() > deadline:
                    raise MediaError(f"Renderer sequence exceeded its {limit:.0f}-second deadline")
                lines = read_sidecar_lines(sidecar)
                if origin is None:
                    first = next((line for line in lines if line["index"] == 1), None)
                    origin = first and first["staged_ms"]
                if origin is not None and pending:
                    # The renderer's own clock: when the newest frame was staged.
                    latest = max(line["staged_ms"] for line in lines) - origin
                    if latest >= pending[0][0] * 1000:
                        after, arguments = pending.pop(0)
                        commands.append(act(binary, env, temp, after, latest, arguments))
                time.sleep(0.025)
            if process.returncode:
                raise MediaError(f"Renderer exited with {process.returncode}")
            if pending:
                raise MediaError("Renderer ended before all real session actions completed")
            entries = parse_sidecar(sidecar, frames, shots)
        except (MediaError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            log.flush()
            raise MediaError(f"{scene['scene']}: {error}\nRenderer log:\n"
                             f"{log_path.read_text(errors='replace')}") from error
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    return entries, commands, shots


def act(binary, env, temp, after, latest_ms, arguments):
    """Run one real session CLI command and verify what it saved."""
    arguments = [part.replace("{temp}", str(temp)) for part in arguments]
    command = [str(binary), "--json", "--session", "website-media", "--actor", "Website demo", *arguments]
    result = run(command, env=env, timeout=15)
    value = json.loads(result.stdout)["result"]
    saved = None
    if "capture" in arguments:
        saved = int(arguments[arguments.index("-n") + 1]) if "-n" in arguments else 1
        if value["count"] != saved or len(value["files"]) != saved:
            raise MediaError("Actual capture result did not match requested frame count")
        for file in value["files"]:
            if not Path(file).read_bytes().startswith(b"\x89PNG\r\n\x1a\n"):
                raise MediaError("Actual capture did not save a PNG frame")
    # Persist only generic arguments and actual capture counts, never
    # temporary authenticated session descriptors or absolute paths.
    return {"after_seconds": after, "renderer_seconds_at_send": round(latest_ms / 1000, 3),
            "arguments": [part.replace(str(temp), "{isolated-demo}") for part in arguments],
            "verified_saved_frames": saved}


# Timing ---------------------------------------------------------------------

def timing_report(name, entries, duration):
    """Measure the rebased frames inside the excerpt; refuse a clip that would
    show fewer than 24 captured frames a second, a long gap, or a stale preview."""
    inside = [entry for entry in entries if 0 <= entry["staged_ms"] <= duration * 1000]
    if len(inside) < 2:
        raise MediaError(f"{name}: no renderer frames in the {duration:.1f} s excerpt")
    times = [entry["staged_ms"] for entry in inside]
    gaps = [(later - earlier, earlier) for earlier, later in zip(times, times[1:])]
    span = (times[-1] - times[0]) / 1000
    observed = (len(inside) - 1) / span if span > 0 else 0
    longest, longest_at = max(gaps)
    ordered = sorted(gap for gap, _ in gaps)
    p95 = ordered[min(len(ordered) - 1, math.ceil(0.95 * len(ordered)) - 1)]
    distinct = len({entry["preview_frame"] for entry in inside if entry.get("preview_frame") is not None})
    held = sum(int(entry.get("held_by_pending") or 0) for entry in inside)
    report = {"observed_fps": round(observed, 3), "max_frame_gap_ms": round(longest, 1),
              "p95_frame_gap_ms": round(p95, 1), "distinct_preview_frames": distinct,
              "held_by_pending": held}
    problem = None
    if entries[-1]["staged_ms"] < duration * 1000:
        problem = (f"renderer frames cover only {entries[-1]['staged_ms'] / 1000:.2f} s of the"
                   f" {duration:.1f} s excerpt (increase --frames")
    elif observed < MIN_CAPTURE_FPS:
        problem = (f"renderer captured {observed:.1f} fps over the {duration:.1f} s excerpt"
                   f" (at least {MIN_CAPTURE_FPS} needed")
    elif longest > MAX_GAP_MS:
        problem = (f"renderer left a {longest:.0f} ms gap in the {duration:.1f} s excerpt"
                   f" (at most {MAX_GAP_MS} allowed")
    elif distinct / (duration * OUTPUT_FPS) < MIN_DISTINCT_FRACTION:
        problem = (f"preview showed {distinct} distinct camera frames over the {duration:.1f} s excerpt"
                   f" (at least {math.ceil(MIN_DISTINCT_FRACTION * duration * OUTPUT_FPS)} needed")
    if problem:
        raise MediaError(f"{name}: {problem}; longest gap {longest:.0f} ms at {longest_at / 1000:.2f} s,"
                         f" staging held {held} times by pending jobs). Use a release binary"
                         " (the default without --binary) and close other GPU-heavy apps.")
    return report


def concat_listing(entries, directory):
    """An FFmpeg concat script timing each frame by the renderer's staging
    clock: a frame lasts until the next was staged."""
    intervals = [(later["staged_ms"] - earlier["staged_ms"]) / 1000
                 for earlier, later in zip(entries, entries[1:])]
    if not intervals or min(intervals) <= 0:
        raise MediaError("Renderer frame staging times must increase")
    intervals.append(sorted(intervals)[len(intervals) // 2])
    # Each PNG opens as an image2 stream whose default 25 fps time base the
    # concat demuxer would inherit, rounding every staging time to 40 ms and
    # repeating every sixth 30 fps frame; a 1/90000 time base keeps them.
    return "".join(f"file '{(directory / entry['file']).as_posix()}'\noption framerate {CONCAT_TIMEBASE}\n"
                   f"duration {interval:.6f}\n"
                   for entry, interval in zip(entries, intervals))


# Encoding -------------------------------------------------------------------

def encode_command(ffmpeg, listing, duration, crf, output):
    # No B-frames: a B-frame can code a genuinely new image in under 400
    # bytes, while a P-frame codes one in more and a repeat in far less, so
    # scripts/test-site.py can count repeated frames from sample sizes.
    return [str(ffmpeg), "-hide_banner", "-loglevel", "error", "-f", "concat", "-safe", "0",
            "-i", str(listing), "-t", str(duration),
            "-vf", f"scale={WIDTH}:{HEIGHT}:flags=lanczos,fps={OUTPUT_FPS}",
            "-an", "-c:v", "libx264", "-preset", "slow", "-crf", str(crf),
            "-bf", "0", "-g", "60", "-keyint_min", "60", "-sc_threshold", "0", "-threads", "4",
            "-pix_fmt", "yuv420p", "-fflags", "+bitexact", "-flags:v", "+bitexact",
            "-map_metadata", "-1", "-movflags", "+faststart", "-y", str(output)]


def encode(name, ffmpeg, listing, output, duration, runner=run):
    """Encode at the first CRF on the ladder that fits the clip budget."""
    for crf in CRF_LADDER:
        runner(encode_command(ffmpeg, listing, duration, crf, output), timeout=300)
        size = output.stat().st_size
        if size <= CLIP_BYTE_BUDGET:
            return crf
    raise MediaError(f"{name}: {size:,} bytes at crf {CRF_LADDER[-1]} exceeds the {CLIP_BYTE_BUDGET:,}-byte clip budget")


def poster(ffmpeg, clip, at, output):
    """One decoded frame as an optimized PNG; the pinned FFmpeg has no PNG encoder."""
    from PIL import Image

    pixels = run([str(ffmpeg), "-hide_banner", "-loglevel", "error", "-ss", str(at), "-i", str(clip),
                  "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"], timeout=60).stdout
    if len(pixels) != WIDTH * HEIGHT * 3:
        raise MediaError(f"Could not decode a poster frame from {clip.name}")
    Image.frombytes("RGB", (WIDTH, HEIGHT), pixels).save(output, optimize=True)


# Verification ---------------------------------------------------------------

def gray_frames(ffmpeg, clip):
    data = run([str(ffmpeg), "-hide_banner", "-loglevel", "error", "-i", str(clip),
                "-vf", f"scale={CHECK_WIDTH}:{CHECK_HEIGHT}:flags=area,format=gray",
                "-f", "rawvideo", "-"], timeout=120).stdout
    size = CHECK_WIDTH * CHECK_HEIGHT
    return [data[offset:offset + size] for offset in range(0, len(data), size)], len(data)


def blank_text_frames(frames, size=(CHECK_WIDTH, CHECK_HEIGHT), regions=TEXT_REGIONS):
    """Indexes of frames whose text columns are far flatter than usual: text
    laid out anew and not drawn before readback comes out blank."""
    from PIL import Image, ImageStat

    blank = set()
    for region in regions:
        spreads = [ImageStat.Stat(Image.frombytes("L", size, frame).crop(region)).stddev[0]
                   for frame in frames]
        median = sorted(spreads)[len(spreads) // 2]
        blank.update(index for index, spread in enumerate(spreads) if spread < 0.5 * median)
    return sorted(blank)


def preview_differences(frames, size=(CHECK_WIDTH, CHECK_HEIGHT), region=PREVIEW_REGION):
    """Mean absolute difference of the preview between consecutive frames."""
    from PIL import Image, ImageChops, ImageStat

    crops = [Image.frombytes("L", size, frame).crop(region) for frame in frames]
    return [ImageStat.Stat(ImageChops.difference(a, b)).mean[0] for a, b in zip(crops, crops[1:])]


def verify(name, ffmpeg, path, duration):
    from PIL import Image, ImageChops, ImageStat

    timing = load_test_site().mp4_video_timing(path)
    expected = round(duration * OUTPUT_FPS)
    seconds = timing["duration"] / timing["timescale"]
    if (timing["codec"] != "avc1" or (timing["width"], timing["height"]) != (WIDTH, HEIGHT)
            or len(timing["stts"]) != 1 or timing["samples"] != expected
            or abs(timing["samples"] / seconds - OUTPUT_FPS) > 0.05 or abs(seconds - duration) > 0.05):
        raise MediaError(f"{name}: expected {expected} constant-rate {WIDTH}x{HEIGHT} H.264 samples"
                         f" over {duration} s, found {len(timing['stts'])} timing runs,"
                         f" {timing['samples']} samples over {seconds:.3f} s")
    probe = subprocess.run([str(ffmpeg), "-hide_banner", "-i", str(path)], stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE, text=True, timeout=30).stderr
    streams = [line for line in probe.splitlines() if "Stream #" in line]
    if len(streams) != 1 or "Video: h264" not in streams[0] or "yuv420p" not in streams[0]:
        raise MediaError(f"{name}: expected one silent H.264/yuv420p video stream")
    frames, size = gray_frames(ffmpeg, path)
    if size != expected * CHECK_WIDTH * CHECK_HEIGHT:
        raise MediaError(f"{name}: decoded {size} bytes, expected {expected} frames")
    differences = preview_differences(frames)
    motion = sum(1 for value in differences if value > MIN_PREVIEW_DIFFERENCE) / duration
    if motion < MIN_CAPTURE_FPS:
        raise MediaError(f"{name}: the camera preview changes in only {motion:.1f} frames a second"
                         f" (at least {MIN_CAPTURE_FPS} needed)")
    blank = blank_text_frames(frames)
    if blank:
        raise MediaError(f"{name}: {len(blank)} frames have a blank text region"
                         " (staged text was not drawn before readback)")
    # Movement must be in the actual camera preview across the clip, not
    # just a changing frame rate.
    samples = []
    previous = None
    for at in (0.5, duration / 2, duration - 0.5):
        index = min(len(frames) - 1, round(at * OUTPUT_FPS))
        preview = Image.frombytes("L", (CHECK_WIDTH, CHECK_HEIGHT), frames[index]).crop(PREVIEW_REGION)
        if previous is not None:
            diff = ImageChops.difference(previous, preview)
            mean = ImageStat.Stat(diff).mean[0]
            changed = 1 - diff.histogram()[0] / (diff.width * diff.height)
            if mean < 0.5 or changed < 0.05:
                raise MediaError(f"{name}: camera preview does not visibly change")
            samples.append({"mean_pixel_difference": round(mean, 3), "changed_fraction": round(changed, 4)})
        previous = preview
    return {"width": WIDTH, "height": HEIGHT, "duration_seconds": round(seconds, 3),
            "fps": OUTPUT_FPS, "frame_count": timing["samples"],
            "preview_motion_fps": round(motion, 3),
            "coded_change_samples": sum(1 for sample in timing["sizes"] if sample >= CODED_CHANGE_BYTES),
            "codec": "h264", "pixel_format": "yuv420p", "audio": False,
            "preview_frame_differences": samples}


# Main -----------------------------------------------------------------------

def default_frames(fps, duration):
    return math.ceil(fps * (duration + 1)) + 1


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, help="A desktop build to record; default: build one with cargo build --release")
    parser.add_argument("--allow-debug", action="store_true", help="Record with a debug-profile --binary anyway")
    parser.add_argument("--output", type=Path, default=ROOT / "web/media")
    parser.add_argument("--ffmpeg", help="FFmpeg to encode with; default: CAPTUREFAB_FFMPEG, then the pinned build in target/ffmpeg")
    parser.add_argument("--frames", type=int, help="Renderer frames per clip; default covers the excerpt plus a second")
    parser.add_argument("--fps", type=int, default=OUTPUT_FPS, help="Renderer capture rate")
    parser.add_argument("--duration", type=float, default=8)
    parser.add_argument("--attempts", type=int, default=3,
                        help="Recordings per clip before a timing failure is final; the renderer can stall")
    args = parser.parse_args()
    frames = args.frames or default_frames(args.fps, args.duration)
    if (not 2 <= frames <= 900 or not 1 <= args.fps <= 60 or not 3 <= args.duration <= 12
            or not 1 <= args.attempts <= 5):
        parser.error("frames must be 2–900, fps 1–60, duration 3–12 seconds and attempts 1–5")
    try:
        import PIL  # noqa: F401
    except ImportError:
        parser.error("Pillow is required: uv run --with pillow scripts/site-media.py")
    if args.binary and args.binary.resolve().parent.name == "debug" and not args.allow_debug:
        parser.error("a debug-profile binary cannot read frames back fast enough; omit --binary to build"
                     " a release one, or pass --allow-debug")
    try:
        ffmpeg = resolve_ffmpeg(args.ffmpeg)
        encoder = check_ffmpeg(ffmpeg)
        binary = (args.binary or build_binary()).resolve()
        output = args.output.resolve()
        output.mkdir(parents=True, exist_ok=True)
        manifest = record_all(binary, ffmpeg, encoder, output, frames, args)
    except (MediaError, subprocess.CalledProcessError) as error:
        detail = getattr(error, "stderr", None)
        if isinstance(detail, bytes):
            detail = detail.decode(errors="replace")
        raise SystemExit(f"{error}\n{detail or ''}".rstrip())
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


def record_all(binary, ffmpeg, encoder, output, frames, args):
    """Record and check every clip in a staging directory; web/media changes
    only once all of them pass."""
    manifest = {
        "method": METHOD,
        "source_note": "Desktop build from workspace source. The visible save and settings results come from real session CLI commands.",
        "binary_sha256": sha256(binary),
        # Captions may name this version; scripts/test-site.py checks that they do.
        "app_version": app_version(binary),
        "demo": "Built-in simulator cameras in isolated temporary sessions. No physical cameras or saved user data.",
        "clips": [],
    }
    with tempfile.TemporaryDirectory(prefix="capturefab-site-media-") as staging_name:
        staging = Path(staging_name)
        for name, scene in CLIPS.items():
            with tempfile.TemporaryDirectory(prefix=f"capturefab-{name}-") as directory:
                for attempt in range(1, args.attempts + 1):
                    # A fresh session each time; only a recording that
                    # passes every gate is kept.
                    temp = Path(directory) / f"attempt-{attempt}"
                    temp.mkdir()
                    raw, commands, shots = record(binary, scene, temp, frames, args.fps)
                    entries = rebase(raw)
                    try:
                        report = timing_report(name, entries, args.duration)
                        break
                    except MediaError as error:
                        if attempt == args.attempts:
                            raise
                        print(f"{error} Recording again ({attempt} of {args.attempts} attempts).", flush=True)
                listing = temp / "frames.txt"
                listing.write_text(concat_listing(entries, shots))
                path = staging / f"{name}.mp4"
                crf = encode(name, ffmpeg, listing, path, args.duration)
                metadata = verify(name, ffmpeg, path, args.duration)
                poster_path = path.with_suffix(".png")
                poster(ffmpeg, path, scene["poster_time"], poster_path)
                actions = [{key: value for key, value in action.items() if value is not None}
                           for action in commands]
                manifest["clips"].append({
                    "file": path.name, "poster": poster_path.name, "sha256": sha256(path),
                    "poster_sha256": sha256(poster_path), "size_bytes": path.stat().st_size,
                    **metadata,
                    "encoder": {"codec": "libx264", "crf": crf, "preset": "slow", "bframes": 0, "gop": 60, "threads": 4, **encoder},
                    "source": {"method": "Built-in renderer screenshot sequence with real simulator acquisition and session CLI commands",
                               "scene": scene["scene"], "cameras": scene["cameras"], "frames": len(raw),
                               "capture_fps_target": args.fps, **report, "efficiency": "smooth",
                               "playback_excerpt": {"start_seconds": 0, "end_seconds": args.duration, "speed": 1},
                               "first_frame_sha256": sha256(shots / raw[0]["file"]),
                               "last_frame_sha256": sha256(shots / raw[-1]["file"]),
                               "actions": actions},
                    "description": scene["description"],
                })
                print(f"{path.name}: {report['observed_fps']:.1f} fps captured, {metadata['preview_motion_fps']:.1f}"
                      f" preview changes/s, crf {crf}, {path.stat().st_size:,} bytes", flush=True)
        names = {clip["file"] for clip in manifest["clips"]}
        total = sum(clip["size_bytes"] for clip in manifest["clips"])
        total += sum(path.stat().st_size for path in output.glob("*.mp4") if path.name not in names)
        if total > TOTAL_BYTE_BUDGET:
            raise MediaError(f"Clips total {total:,} bytes, over the {TOTAL_BYTE_BUDGET:,}-byte budget")
        for clip in manifest["clips"]:
            for key in ("file", "poster"):
                shutil.move(staging / clip[key], output / clip[key])
    return manifest


if __name__ == "__main__":
    main()
