#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["transformers>=4.56", "torch", "safetensors", "numpy"]
# ///
"""Regenerate the tiny random-weight DINOv2 fixtures in src/autocapture/fixtures.

Each fixture is a transformers Dinov2Model with every parameter randomized,
saved as config.json + model.safetensors, plus expected.json holding the
unit-length CLS embedding transformers computes for a deterministic input.
The Rust CPU and GPU encoders must reproduce it.

With --model DIR (for example target/models/dinov2-small with the upstream
config.json beside it), print the reference embedding of a 224 x 224 input
for the real checkpoint instead.
"""
import argparse, json
from pathlib import Path
import numpy as np, torch
from transformers import Dinov2Config, Dinov2Model

ROOT = Path(__file__).resolve().parents[1] / "src/autocapture/fixtures"
FIXTURES = {
    # A 6 x 8 patch grid against a 10 x 10 position table: non-square bicubic interpolation.
    "dinov2-tiny": dict(hidden_size=32, num_hidden_layers=2, num_attention_heads=2, mlp_ratio=2,
                        patch_size=4, image_size=40, height=24, width=32),
    # The native grid, where transformers uses the position table unchanged.
    "dinov2-native": dict(hidden_size=16, num_hidden_layers=1, num_attention_heads=2, mlp_ratio=4,
                          patch_size=2, image_size=8, height=8, width=8),
}


def pixels(height, width):
    """Must match `pixels` in src/autocapture/dinov2/tests.rs."""
    c, y, x = np.meshgrid(np.arange(3), np.arange(height), np.arange(width), indexing="ij")
    return (np.sin(0.37 * x + 0.11 * y * (c + 1)) * 1.5 + 0.1 * c).astype(np.float32)[None]


def cls(model, image):
    with torch.no_grad():
        v = model(pixel_values=torch.from_numpy(image)).pooler_output[0].double()
    return (v / v.norm()).tolist()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--model", type=Path, help="print the reference CLS for a real checkpoint directory")
    args = parser.parse_args()
    if args.model:
        model = Dinov2Model.from_pretrained(args.model, dtype=torch.float32).eval()
        print(json.dumps({"height": 224, "width": 224, "cls": cls(model, pixels(224, 224))}))
        return
    for name, spec in FIXTURES.items():
        spec = dict(spec)
        height, width = spec.pop("height"), spec.pop("width")
        torch.manual_seed(len(name))
        model = Dinov2Model(Dinov2Config(**spec)).eval()
        with torch.no_grad():
            for parameter in model.parameters():
                parameter.normal_(0.0, 0.5)
        out = ROOT / name
        out.mkdir(parents=True, exist_ok=True)
        model.save_pretrained(out, safe_serialization=True)
        (out / "expected.json").write_text(json.dumps({"height": height, "width": width,
                                                       "cls": cls(model, pixels(height, width))}) + "\n")
        print(out, sorted(p.name for p in out.iterdir()))


if __name__ == "__main__":
    main()
