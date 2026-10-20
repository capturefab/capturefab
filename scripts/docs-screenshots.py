#!/usr/bin/env python3
"""Capture the real desktop renderer and crop documentation illustrations.

Requires a desktop build and Pillow. Each scene uses isolated session data and
built-in simulator cameras; it does not open a user's camera or saved session.
Run on a desktop: python3 scripts/docs-screenshots.py --binary target/debug/capturefab
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
SCENES = {
    "workspace": (1, "dark,features,focus,exposure"),
    "picker": (1, "dark,welcome"),
    "features": (1, "dark,features,noscopes"),
    "capture": (1, "dark,capture,noscopes,inspector-storage-default"),
    "schedule": (1, "dark,capture,noscopes,inspector-widest,inspector-storage-default,scheduled"),
    "destinations": (1, "dark,destinations-list,noscopes"),
    "scopes": (1, "dark,noinspector,focus,exposure"),
    "forwarding": (1, "dark,forward,noscopes,inspector-storage-default"),
    "quick-command": (2, "dark,quick-command-plan,noscopes"),
}

# Crop boxes are in logical desktop pixels at the fixed 1100 x 700 window.
# Originals remain beside the crops so every illustration can be audited.
CROPS = {
    "quick-command": ("quick-command", (0, 0, 1100, 700), "Quick command plan with checked, editable camera tools and an explicit Run plan action. Provider and camera observations are synthetic display fixtures."),
    "workspace": ("workspace", (0, 0, 1100, 700), "A simulator camera in its own workbench window, with scopes and its Features inspector."),
    "cameras": ("picker", (300, 40, 800, 660), "The camera picker in a window without a camera: found simulator cameras, the address field beside Search again, and data sources, Settings, Help and session controls."),
    "features": ("features", (816, 0, 1100, 652), "Quick controls for the selected camera, with search and collapsed feature groups for less common settings."),
    "capture": ("capture", (0, 0, 284, 700), "Ordinary capture controls first, followed by the Capture options (timeout and scheduling), Automatic capture, Storage and retention, and Shell and agents disclosures; no automatic capture session is open in this scene."),
    "schedule": ("schedule", (16, 414, 272, 596), "The capture scheduling form in Capture options, with start delay and frame interval."),
    "destinations": ("destinations", (264, 150, 836, 550), "Destination manager showing example folder and S3-compatible bucket destinations."),
    "scopes": ("scopes", (438, 40, 1096, 442), "Live focus score with its focus measure chooser and exposure histogram alongside the focus region on a simulator image."),
    "forwarding": ("forwarding", (0, 0, 284, 420), "Video panel with the Record to file or Forward to URL choice, the folded Location and name section, a start action and the next file name, followed by the Video settings disclosure summing up codec, frame rate and bitrate."),
    "history": ("destinations", (288, 218, 456, 255), "Undo and Redo controls in the destination manager; macOS keyboard shortcuts are shown."),
}


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/capturefab")
    parser.add_argument("--output", type=Path, default=ROOT / "web/images/docs")
    parser.add_argument("--only", choices=list(SCENES), help="Capture one scene while tuning crops")
    parser.add_argument("--crop-only", action="store_true", help="Reuse the saved renderer originals")
    parser.add_argument("--source-note", default="Current development desktop build from the workspace source.")
    args = parser.parse_args()
    binary = args.binary.resolve()
    binary_digest = sha256(binary)
    output = args.output.resolve()
    originals = output / "source"
    originals.mkdir(parents=True, exist_ok=True)
    scenes = {args.only: SCENES[args.only]} if args.only else SCENES
    if not args.crop_only:
        for name, (cameras, scene) in scenes.items():
            with tempfile.TemporaryDirectory(prefix="capturefab-docs-") as temp:
                env = dict(os.environ)
                for key in ("CAPTUREFAB_SESSION", "CAPTUREFAB_EXECUTABLE"):
                    env.pop(key, None)
                env.update(CAPTUREFAB_SESSION_DIR=str(Path(temp) / "sessions"),
                           CAPTUREFAB_CACHE_DIR=str(Path(temp) / "cache"),
                           CAPTUREFAB_SCREENSHOT_SCENE=scene,
                           CAPTUREFAB_SCREENSHOT_SIZE="1100x700")
                subprocess.run([str(binary), "gui", "--name", "docs", "--screenshot",
                                str(originals / f"{name}.png"), "--screenshot-cameras", str(cameras)],
                               env=env, check=True, timeout=60)
                if sha256(binary) != binary_digest:
                    raise ValueError("The desktop binary changed during capture; finish the build, then recapture all scenes.")
                with Image.open(originals / f"{name}.png") as rendered:
                    rendered.save(originals / f"{name}.png", optimize=True)
                print(f"Captured {name}", flush=True)
    manifest = {
        "method": "Actual iced desktop renderer screenshots; crop only, no generated or reconstructed interface.",
        "source_note": args.source_note,
        "binary_sha256": binary_digest,
        "logical_window": [1100, 700],
        "demo": "Built-in simulator cameras. Destination names, paths and scheduled jobs are built-in demonstration scenes. Capture, scheduling and Video scenes use the validated default storage policy as a read-only display fixture without accessing a storage ledger.",
        "images": [],
    }
    for name, (source, box, description) in CROPS.items():
        path = originals / f"{source}.png"
        if not path.exists():
            continue
        with Image.open(path) as image:
            sx, sy = image.width / 1100, image.height / 700
            if abs(sx - sy) > 0.01:
                raise ValueError(f"{path}: the desktop clamped the requested 1100 x 700 window; recapture on a larger display")
            pixels = tuple(round(value * (sx if index % 2 == 0 else sy)) for index, value in enumerate(box))
            crop = image.crop(pixels)
            cropped_path = output / f"{name}.png"
            crop.save(cropped_path, optimize=True)
            manifest["images"].append({"file": f"{name}.png", "source": f"source/{source}.png",
                                       "source_sha256": sha256(path), "source_size": list(image.size),
                                       "crop_pixels": list(pixels), "size": list(crop.size),
                                       "sha256": sha256(cropped_path),
                                       "scene": SCENES[source][1], "description": description})
            print(f"{name}.png: {crop.width} x {crop.height}", flush=True)
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
