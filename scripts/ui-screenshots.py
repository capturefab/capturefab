#!/usr/bin/env python3
"""Capture an auditable desktop UI review matrix with the real iced renderer.

Build first, then run on a desktop:
  python3 scripts/ui-screenshots.py --binary target/debug/capturefab

Each image starts a fresh isolated session with built-in simulator cameras.
Network reports, credentials, measurements and failure states use synthetic
display fixtures. No external camera, provider, credential or storage policy is
changed. Images contain app-rendered pixels; inspect native window chrome and
the separate macOS Settings window in the running app as well.
"""

import argparse
from dataclasses import dataclass
import hashlib
import html
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class Scene:
    name: str
    words: str
    description: str
    cameras: int = 1
    camera: str = None


# Every screen family gets both appearances at the ordinary window size.
# Separate top/bottom views retain genuine scrolling rather than reconstructing
# a long screen from stitched or generated UI pixels.
BASELINE = (
    Scene("welcome", "welcome,side-recent", "Welcome, discovered sources, recent cameras and address entry"),
    Scene("workspace", "features,focus,exposure", "Camera window with scopes and Features inspector"),
    Scene("camera", "stage-controls,noscopes", "Camera window with stage controls"),
    Scene("image", "image-hint,focus,exposure", "Image mode with focus and exposure scopes"),
    Scene("features", "features,noscopes", "Features inspector, Quick controls and collapsed feature groups"),
    Scene("capture", "capture,noscopes,inspector-storage-default", "Capture sidebar with default storage policy"),
    Scene("video", "forward,noscopes,inspector-storage-default", "Local recording and stream forwarding controls"),
    Scene("automatic-capture", "auto-capture,noscopes", "Active automatic-capture session and Review action"),
    Scene("review", "auto-capture-review,noscopes", "Automatic-capture review sheet, preview and Keep/Discard actions"),
    Scene("activity", "logs,log-sample,noscopes", "Session activity and raw log with wrapped failure detail"),
    Scene("destinations", "destinations-list,noscopes", "Destination manager with folder and S3 destinations"),
    Scene("folder-editor", "folder-editor,noscopes", "Folder destination editor"),
    Scene("bucket-editor", "bucket-editor,noscopes", "S3 destination editor with checked example credentials"),
    Scene("help", "help-more,noscopes", "Quick guide steps with connection notes expanded beneath them"),
    Scene("agent-setup", "agent-setup,noscopes", "External-agent session briefing and connection command"),
    Scene("settings", "settings-credentials,noscopes", "Settings sheet fixture including API-key controls"),
    Scene("settings-assistant", "settings-assistant,noscopes", "AI assistant provider, model and credential setup with synthetic status"),
    Scene("quick-command", "quick-command,noscopes", "Quick command before a request, with local suggestions", 2),
    Scene("quick-command-plan", "quick-command-plan,noscopes", "Reviewable assistant plan and editable camera-tool cards", 2),
    Scene("quick-command-invalid", "quick-command-invalid,noscopes", "Assistant plan with invalid fields and a blocking reason", 2),
    Scene("quick-command-running", "quick-command-running,noscopes", "Assistant plan execution with synthetic progress", 2),
    Scene("quick-command-done", "quick-command-done,noscopes", "Completed assistant plan and outcome rows", 2),
    Scene("quick-command-exported", "quick-command-exported,noscopes", "Exported source plan with this workbench's checks-only command and reviewed capture folder", 1),
    Scene("quick-command-setup", "quick-command-setup,noscopes", "Quick command without a configured provider", 2),
    Scene("quick-command-task-setup", "quick-command-task-setup,noscopes", "Assistant camera scope, image permission and hard limits", 2),
    Scene("quick-command-task-running", "quick-command-task-running,noscopes", "Assistant latest view, budgets and task controls", 2),
    Scene("quick-command-task-approval", "quick-command-task-approval,noscopes", "Checked action approval and secure secret field", 2),
    Scene("quick-command-task-paused-human", "quick-command-task-paused-human,noscopes", "Assistant paused after a person changes the camera", 2),
    Scene("quick-command-task-limit", "quick-command-task-limit,noscopes", "Assistant stopped at a hard task limit", 2),
    Scene("quick-command-task-done", "quick-command-task-done,noscopes", "Completed task, changes and remaining effects", 2),
    Scene("quick-command-task-data-setup", "quick-command-task-data-setup,noscopes", "Data-only source scope, separate data permission and capture allowances", 1),
    Scene("quick-command-task-data-running", "quick-command-task-data-running,noscopes", "Data-only values, quality, timestamps and persisted counts", 1),
    Scene("quick-command-task-mixed-running", "quick-command-task-mixed-running,noscopes", "Mixed camera and sensor task with shared dataset status", 2),
    Scene("quick-command-task-data-approval", "quick-command-task-data-approval,noscopes", "Source connection approval with exact local endpoint identity", 1),
    Scene("quick-command-task-data-paused-human", "quick-command-task-data-paused-human,noscopes", "Data task paused after a person replaces its dataset", 1),
    Scene("quick-command-task-data-error", "quick-command-task-data-error,noscopes", "Data task persistence failure and retained accounting", 1),
    Scene("quick-command-task-data-done", "quick-command-task-data-done,noscopes", "Completed data task with final persistence evidence", 1),
    Scene("network", "network,noscopes", "Camera network report with synthetic adapter and unreachable camera"),
    Scene("analytics", "analytics,noscopes", "Session acquisition analytics with live simulator statistics"),
    Scene("measurements", "analytics-measurements,noscopes", "Default prompt measurements form before collection starts"),
    Scene("close-error", "close-error,noscopes", "Save-settings failure alert with Retry and Cancel"),
)

# The compact matrix covers each long sheet and inspector at the largest
# interface size, plus lower-content views that an initial screenshot hides.
COMPACT = (
    Scene("welcome", "welcome,side-recent,side-address", "Compact welcome and address entry"),
    Scene("features", "features,peek,noscopes", "Floating Features inspector at compact width"),
    Scene("capture", "capture,peek,noscopes,inspector-widest,inspector-storage-default", "Floating Capture sidebar with scheduling and retention exposed"),
    Scene("capture-bottom", "capture,peek,noscopes,inspector-widest,inspector-storage-default,inspector-bottom", "Lower capture controls and storage policy after scrolling"),
    Scene("video", "forward,peek,noscopes,inspector-widest", "Compact Video sidebar with longest form choices"),
    Scene("review", "auto-capture-review,noscopes", "Compact automatic-capture review sheet"),
    Scene("activity", "logs,log-sample,noscopes", "Compact Activity pane and wrapped log entries"),
    Scene("destinations", "destinations-remove,noscopes", "Destination removal confirmation with files-preserved wording"),
    Scene("bucket-editor", "bucket-editor-advanced,noscopes", "Compact S3 editor with Advanced expanded"),
    Scene("bucket-bottom", "bucket-editor-advanced,noscopes,sheet-bottom", "S3 editor advanced controls and pinned Save/Cancel actions"),
    Scene("help", "help-more,noscopes,sheet-bottom", "Lower shortcut tables and the terminal or agent session commands"),
    Scene("agent-setup", "agent-setup,noscopes", "Compact external-agent session setup and copy action"),
    Scene("settings", "settings-credential-error,noscopes,sheet-bottom", "API-key controls, credential error and pinned Settings footer"),
    Scene("settings-assistant", "settings-assistant-error,noscopes", "Compact AI assistant setup with field validation and a synthetic provider refusal"),
    Scene("settings-assistant-bottom", "settings-assistant,noscopes,sheet-bottom", "Lower assistant settings and pinned Settings footer after scrolling"),
    Scene("quick-command-plan", "quick-command-plan,noscopes", "Compact assistant plan with pinned actions", 2),
    Scene("quick-command-invalid", "quick-command-invalid,noscopes", "Compact assistant plan validation and focus target", 2),
    Scene("quick-command-data-consent", "quick-command-data-consent,noscopes", "Compact Quick Command with image and data transmission disclosures", 2),
    Scene("quick-command-task-setup", "quick-command-task-setup,noscopes", "Compact assistant task setup", 2),
    Scene("quick-command-task-approval", "quick-command-task-approval,noscopes", "Compact assistant approval controls", 2),
    Scene("quick-command-task-running", "quick-command-task-running,noscopes", "Compact assistant running controls", 2),
    Scene("quick-command-task-data-setup", "quick-command-task-data-setup,noscopes", "Compact data task source scope and capture allowances", 1),
    Scene("quick-command-task-data-running", "quick-command-task-data-running,noscopes", "Compact data values and persistent task controls", 1),
    Scene("quick-command-task-data-approval", "quick-command-task-data-approval,noscopes", "Compact source connection approval", 1),
    Scene("quick-command-task-data-done", "quick-command-task-data-done,noscopes", "Compact final sample accounting and persistence evidence", 1),
    Scene("network", "network-error,noscopes,sheet-bottom", "Unreachable camera details and pinned Scan network action"),
    Scene("analytics", "analytics,noscopes,sheet-bottom", "Lower acquisition analytics and native camera statistics"),
    Scene("measurements", "analytics-invalid,noscopes", "Compact prompt measurement rows and invalid sample interval"),
    Scene("measurements-bottom", "analytics-measurements-populated,noscopes,sheet-bottom", "Populated measurement chart and wrapped result notes"),
    Scene("quick-command-data-sources", "quick-command-data-sources,noscopes", "Compact Plan-first local source binding and data consent", 2),
    Scene("quick-command-data-auth", "quick-command-data-auth,noscopes", "Compact protected source profile editor with a masked OS-store password", 1),
    Scene("quick-command-task-data-auth", "quick-command-task-data-auth,noscopes", "Compact task source profile editor and independent data consent", 1),
    Scene("signals-auth", "signals-auth,peek,noscopes", "Compact manual Sources profile editor with protected credentials", 1),
    Scene("signals-empty", "signals-empty,noscopes", "Compact manual source entry with zero connected cameras or sources", 0),
    Scene("signals-persistence-error", "signals-persistence-error,noscopes,inspector-bottom", "Compact running source with failed writes, zero saved samples and storage loss", 0),
    Scene("quick-command-exported", "quick-command-exported,noscopes", "Compact exported-plan preview command with reviewed capture folder", 1),
)

VARIANTS = (
    Scene("search-empty", "features,inspector-nomatch,noscopes", "Features search with no results"),
    Scene("feature-error", "features,write-error,noscopes", "Rejected feature value and inline validation"),
    Scene("capture-error", "capture,inspector-capture-failed,noscopes", "Capture failure with recovery detail"),
    Scene("destination-error", "editor,noscopes", "S3 destination validation failure"),
    Scene("stage-error", "stalled,stage-error,noscopes", "Stalled stream and camera error overlay"),
    Scene("notice-long", "notice-long,noscopes", "Long command-error notice with disclosure action"),
    Scene("measurement-history", "analytics-measurements-populated,noscopes,sheet-bottom", "Measurement chart, unknown value and wrapped result notes"),
    Scene("settings-assistant-error", "settings-assistant-error,noscopes", "AI assistant field validation and synthetic provider error details"),
    Scene("quick-command-data-consent", "quick-command-data-consent,noscopes", "Quick Command with independent image and data consent enabled", 2),
    Scene("quick-command-thinking", "quick-command-thinking,noscopes", "Pending assistant turn with Stop and read-only plan cards", 2),
    Scene("quick-command-error", "quick-command-error,noscopes", "Assistant provider failure and retry action", 2),
    Scene("quick-command-data-sources", "quick-command-data-sources,noscopes", "Plan-first approved local endpoints and opaque source references", 2),
    Scene("quick-command-data-auth", "quick-command-data-auth,noscopes", "Plan-first explicit local username and protected password setup", 1),
    Scene("quick-command-task-data-auth", "quick-command-task-data-auth,noscopes", "Task source profile creation without provider or preference credential disclosure", 1),
    Scene("signals-auth", "signals-auth,noscopes", "Manual Sources uses the same protected local source profile editor", 1),
    Scene("signals-empty", "signals-empty,noscopes", "Manual source setup and camera picker entry before connecting any camera or source", 0),
    Scene("signals-persistence-error", "signals-persistence-error,noscopes,inspector-bottom", "Manual source persistence failure while the connection remains running", 0),
)

EXTRA_STATES = (
    Scene("active-forwarding", "forward,forwarding,noscopes", "Active stream forwarding and its Stop action"),
    Scene("active-recording", "forward,recording,noscopes", "Active local recording and its Stop action"),
    Scene("timed-recording", "forward,recording,timed,noscopes", "Timed local recording with its time left and end time"),
    Scene("destinations-empty", "destinations,noscopes", "Empty destination manager and creation actions"),
    Scene("bucket-testing", "bucket-editor,testing,noscopes", "Pending destination connectivity test with pinned actions"),
    Scene("discovery-pending", "welcome,searching", "Synthetic pending camera discovery without hardware enumeration"),
    Scene("connection-failed", "welcome,connect-failed", "Failed address connection and retained recovery input"),
    Scene("feature-dirty", "features,inspector-dirty,noscopes", "Unapplied camera-feature draft"),
    Scene("feature-auto", "features,auto,noscopes", "Automatic exposure mode and dependent feature controls"),
)


def extra_state_matrix():
    for index, scene in enumerate(EXTRA_STATES):
        appearance = "light" if index % 2 == 0 else "dark"
        yield (f"{scene.name}-{appearance}", scene, appearance, "standard", (1100, 700), "states")


def matrix(kinds=False):
    if kinds:
        for kind in ("line", "depth", "microscope", "thermal"):
            for tab, words in (("features", "features"), ("capture", "capture"), ("video", "forward")):
                scene = Scene(f"{kind}-{tab}", f"{words},noscopes", f"{kind.title()} simulator: {tab} controls and purpose-specific guidance", camera=f"sim:{kind}")
                yield (f"{scene.name}-light", scene, "light", "standard", (1100, 700), "kinds")
        for kind in ("line", "depth", "thermal"):
            for tab, words in (("capture", "capture"), ("video", "forward")):
                scene = Scene(f"{kind}-{tab}", f"{words},peek,noscopes", f"Compact {kind} guidance and primary {tab} action", camera=f"sim:{kind}")
                yield (f"{scene.name}-compact-dark", scene, "dark", "larger", (900, 620), "kinds")
        for kind in ("scanner", "unknown"):
            for tab, words in (("features", "features"), ("capture", "capture"), ("video", "forward")):
                details = ",inspector-details,inspector-bottom" if kind == "unknown" and tab == "features" else ""
                scene = Scene(f"{kind}-{tab}", f"{words},kind-{kind},noscopes{details}", f"Synthetic {kind} display profile over an isolated simulator; {tab} controls")
                yield (f"{scene.name}-light", scene, "light", "standard", (1100, 700), "kinds")
        for kind, tab, words in (("scanner", "capture", "capture"), ("unknown", "features", "features")):
            scene = Scene(f"{kind}-{tab}", f"{words},kind-{kind},peek,noscopes", f"Compact synthetic {kind} display profile")
            yield (f"{scene.name}-compact-dark", scene, "dark", "larger", (900, 620), "kinds")
        return
    for appearance in ("light", "dark"):
        for scene in BASELINE:
            yield (f"{scene.name}-{appearance}", scene, appearance, "standard", (1100, 700), "baseline")
    for index, scene in enumerate(COMPACT):
        # Alternate appearances across the stress cases; baseline covers both.
        appearance = "light" if index % 2 == 0 else "dark"
        yield (f"{scene.name}-compact-{appearance}", scene, appearance, "larger", (900, 620), "compact")
    for index, scene in enumerate(VARIANTS):
        appearance = "light" if index % 2 == 0 else "dark"
        yield (f"{scene.name}-{appearance}", scene, appearance, "standard", (1100, 700), "states")
    # Append new task-storage cases to preserve existing review artifact order.
    for state in ("setup", "done"):
        name = f"quick-command-task-separate-{state}"
        scene = Scene(name, f"{name},noscopes,sheet-down", f"Separate sensor task dataset: {state}, scrolled storage disclosure and retained receipt", 0)
        for appearance in ("light", "dark"):
            yield (f"{scene.name}-{appearance}", scene, appearance, "standard", (1100, 700), "states")
        yield (f"{scene.name}-compact-dark", scene, "dark", "larger", (900, 620), "compact")


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def isolated_environment():
    """UI fixtures must never inherit provider credentials or cloud auth setup."""
    provider_keys = {
        "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY",
        "OPENROUTER_API_KEY", "GROQ_API_KEY", "DEEPSEEK_API_KEY",
        "CLOUDFLARE_API_TOKEN", "MISTRAL_API_KEY", "XAI_API_KEY",
        "TOGETHER_API_KEY", "FIREWORKS_API_KEY", "CEREBRAS_API_KEY",
        "HF_TOKEN", "MOONSHOT_API_KEY", "DASHSCOPE_API_KEY",
        "PERPLEXITY_API_KEY", "OLLAMA_API_KEY", "LM_API_TOKEN",
    }
    prefixes = ("CAPTUREFAB_", "AWS_", "GOOGLE_", "AZURE_")
    return {key: value for key, value in os.environ.items()
            if not key.upper().startswith(prefixes) and key.upper() not in provider_keys}


def select_cases(kinds=False, only=None, all_screens=False):
    cases = list(matrix()) + list(matrix(True)) + list(extra_state_matrix()) if all_screens else list(matrix(kinds))
    if only:
        missing = set(only) - {case[0] for case in cases}
        if missing:
            raise ValueError("Requested scenes are outside the selected matrix: " + ", ".join(sorted(missing)) + ". Use --all for the combined review or --kinds for camera-purpose scenes.")
        cases = [case for case in cases if case[0] in only]
    if not cases:
        raise ValueError("The selected review matrix has no scenes.")
    return cases


def write_gallery(output, manifest, contact_sheets):
    from PIL import Image, ImageDraw, ImageFont

    cards = []
    for entry in manifest["images"]:
        name = html.escape(entry["name"])
        description = html.escape(entry["description"])
        cards.append(f'<article><a href="{entry["file"]}"><img src="{entry["file"]}" alt="{description}" loading="lazy"></a><h2>{name}</h2><p>{description}</p></article>')
    (output / "index.html").write_text("""<!doctype html>
<meta charset="utf-8"><title>Capturefab desktop UI review</title>
<style>body{margin:24px;background:#ededf0;color:#1d1d1f;font:14px system-ui}h1{font-size:24px}main{display:grid;grid-template-columns:repeat(auto-fit,minmax(440px,1fr));gap:24px}article{padding:12px;background:white;border:1px solid #ccc;border-radius:8px}img{display:block;width:100%;height:auto}h2{font-size:15px;margin:12px 0 6px}p{line-height:1.5}a{color:inherit}</style>
<h1>Capturefab desktop UI review</h1><p>Actual desktop-renderer pixels from isolated simulator sessions. Click an image for its original size. Native window chrome and the separate macOS Settings window require live inspection.</p><main>
""" + "\n".join(cards) + "\n</main>\n")
    if not contact_sheets:
        return
    font = ImageFont.load_default(size=15)
    for group in ("baseline-light", "baseline-dark", "compact", "states", "kinds"):
        entries = [entry for entry in manifest["images"] if
                   (entry["group"] == "baseline" and group == f'baseline-{entry["appearance"]}') or
                   (entry["group"] == group)]
        if not entries:
            continue
        columns, thumb_width, thumb_height = 3, 440, 300
        cell_width, cell_height = thumb_width + 24, thumb_height + 54
        sheet = Image.new("RGB", (columns * cell_width, ((len(entries) + columns - 1) // columns) * cell_height), "#e8e8ec")
        draw = ImageDraw.Draw(sheet)
        for index, entry in enumerate(entries):
            x, y = (index % columns) * cell_width + 12, (index // columns) * cell_height + 12
            with Image.open(output / entry["file"]) as original:
                preview = original.convert("RGB")
                preview.thumbnail((thumb_width, thumb_height), Image.Resampling.LANCZOS)
                sheet.paste(preview, (x, y))
            draw.text((x, y + thumb_height + 8), entry["name"], fill="#1d1d1f", font=font)
        sheet.save(output / f"contact-{group}.jpg", quality=92)


def main():
    cases = list(matrix())
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/capturefab")
    parser.add_argument("--output", type=Path, default=ROOT / "target/ui-review")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--kinds", action="store_true", help="Use the additional camera-purpose matrix instead of the baseline review")
    mode.add_argument("--all", dest="all_screens", action="store_true", help="Capture all baseline, compact, state and camera-purpose scenes in one manifest")
    parser.add_argument("--only", action="append", choices=[case[0] for case in cases + list(matrix(True)) + list(extra_state_matrix())], help="Capture only these named images; repeat to select several")
    parser.add_argument("--list", action="store_true", help="Print the matrix without launching the app")
    parser.add_argument("--no-contact-sheets", action="store_true")
    args = parser.parse_args()
    try:
        cases = select_cases(args.kinds, args.only, args.all_screens)
    except ValueError as error:
        parser.error(str(error))
    if args.list:
        for name, scene, appearance, scale, size, group in cases:
            print(f"{name:38} {size[0]}x{size[1]} {scale:8} {scene.words}")
        return
    from PIL import Image

    binary = args.binary.resolve()
    digest = sha256(binary)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    manifest = {
        "method": "Actual iced desktop renderer screenshots; no generated, stitched or reconstructed UI pixels.",
        "binary_sha256": digest,
        "isolation": "Fresh temporary session per image; built-in simulator cameras. Network, API-key, measurement and failure fixtures are synthetic. No credential/provider operation or external camera is requested.",
        "native_gaps": "Renderer images exclude OS window chrome. Settings uses the fixture sheet; separately inspect the native macOS Settings window. Offscreen content is represented only by explicit scrolled views.",
        "images": [],
    }
    for index, (name, scene, appearance, scale, size, group) in enumerate(cases, 1):
        path = output / f"{name}.png"
        with tempfile.TemporaryDirectory(prefix="capturefab-ui-review-") as temporary:
            env = isolated_environment()
            env.update(CAPTUREFAB_SESSION_DIR=str(Path(temporary) / "sessions"),
                       CAPTUREFAB_CACHE_DIR=str(Path(temporary) / "cache"),
                       CAPTUREFAB_SCREENSHOT_SCENE=f"{appearance},{scene.words}",
                       CAPTUREFAB_SCREENSHOT_SIZE=f"{size[0]}x{size[1]}",
                       CAPTUREFAB_UI_SCALE=scale)
            command = [str(binary)]
            if scene.camera:
                command.extend(["--camera", scene.camera])
            command.extend(["gui", "--name", "ui-review", "--screenshot", str(path), "--screenshot-cameras", str(scene.cameras)])
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=60)
        log = result.stdout + result.stderr
        (output / f"{name}.log").write_text(log)
        result.check_returncode()
        if "screenshot scene: no area takes" in log:
            raise RuntimeError(f"{name}: binary does not recognize a requested scene; see its log")
        if sha256(binary) != digest:
            raise RuntimeError("The binary changed during capture. Rebuild first and recapture the matrix.")
        with Image.open(path) as rendered:
            rendered.load()
            sx, sy = rendered.width / size[0], rendered.height / size[1]
            if abs(sx - sy) > 0.01:
                raise RuntimeError(f"{name}: desktop clamped the requested window; use a larger display")
            pixels = list(rendered.size)
        manifest["images"].append({
            "name": name, "file": path.name, "group": group,
            "description": scene.description, "scene": env["CAPTUREFAB_SCREENSHOT_SCENE"],
            "appearance": appearance, "interface_size": scale,
            "logical_window": list(size), "pixel_size": pixels,
            "cameras": scene.cameras, "camera": scene.camera,
            "synthetic_display_profile": next((word.removeprefix("kind-") for word in scene.words.split(",") if word.startswith("kind-")), None),
            "sha256": sha256(path),
        })
        # Preserve completed evidence even if a later scene fails.
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"[{index}/{len(cases)}] {name}: {pixels[0]}x{pixels[1]}", flush=True)
    write_gallery(output, manifest, not args.no_contact_sheets)
    print(f"Review {output / 'index.html'}", flush=True)


if __name__ == "__main__":
    main()
