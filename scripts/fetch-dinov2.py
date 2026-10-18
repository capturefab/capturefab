#!/usr/bin/env python3
"""Fetch the DINOv2 ViT-S/14 weights that Capturefab bundles for auto capture.

Downloads Meta's `facebook/dinov2-small` checkpoint (Apache-2.0) at a pinned
revision, verifies its SHA-256, converts it to float16 safetensors, and writes
target/models/dinov2-small/{model.safetensors,LICENSE,SOURCE.json}. build.rs
embeds that file in release builds (dev and test builds read it in place), so
users never download a model. The conversion is deterministic: the output
SHA-256 is pinned too. Standard library only, so release runners need no
extra packages.
"""
import argparse, hashlib, json, math, struct, sys, urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = "facebook/dinov2-small"
REVISION = "ed25f3a31f01632728cabb09d1542f84ab7b0056"
UPSTREAM_SHA256 = "ae1e99fcefd534ed978cdeb8326f08030c96e28b7a81ffcbc98a857c84d14be1"
LICENSE_URL = "https://raw.githubusercontent.com/facebookresearch/dinov2/7764ea0f912e53c92e82eb78a2a1631e92725fc8/LICENSE"
LICENSE_SHA256 = "600cc67cc4cb2f5ea317dcfc687ad1c74dc4bec8782bbe9db0afd83513b935b7"
OUTPUT_SHA256 = "3ee7cfefceb54f15a72aeb82b2761f58896ded93fb9be82a8c6819b7abee49f8"
# Only the tensors the forward pass reads; the mask token is pretraining-only.
SKIP = {"embeddings.mask_token"}
CONFIG = {"model_type": "dinov2", "hidden_size": 384, "num_hidden_layers": 12, "num_attention_heads": 6,
          "mlp_ratio": 4, "patch_size": 14, "image_size": 518, "layer_norm_eps": 1e-6,
          "hidden_act": "gelu", "qkv_bias": True, "use_swiglu_ffn": False}


def fetch(url, sha256, cache):
    if cache.is_file() and hashlib.sha256(cache.read_bytes()).hexdigest() == sha256:
        return cache.read_bytes()
    print(f"downloading {url}", file=sys.stderr)
    request = urllib.request.Request(url, headers={"User-Agent": "capturefab-fetch-dinov2"})
    data = urllib.request.urlopen(request, timeout=120).read()
    actual = hashlib.sha256(data).hexdigest()
    if sha256 and actual != sha256:
        sys.exit(f"fetch-dinov2: {url} has SHA-256 {actual}, expected {sha256}")
    cache.parent.mkdir(parents=True, exist_ok=True)
    cache.write_bytes(data)
    return data


def to_f16(data):
    (length,) = struct.unpack("<Q", data[:8])
    header = json.loads(data[8:8 + length])
    body = data[8 + length:]
    tensors, blobs, offset = {}, [], 0
    for name in sorted(k for k in header if k != "__metadata__"):
        if name in SKIP:
            continue
        entry = header[name]
        if entry["dtype"] != "F32":
            sys.exit(f"fetch-dinov2: unexpected dtype {entry['dtype']} for {name}")
        start, end = entry["data_offsets"]
        count = (end - start) // 4
        values = struct.unpack(f"<{count}f", body[start:end])
        if not all(map(math.isfinite, values)):
            sys.exit(f"fetch-dinov2: {name} has non-finite values")
        try:
            # IEEE binary16, round to nearest even.
            blob = struct.pack(f"<{count}e", *values)
        except OverflowError:
            sys.exit(f"fetch-dinov2: {name} does not fit float16")
        tensors[name] = {"dtype": "F16", "shape": entry["shape"], "data_offsets": [offset, offset + len(blob)]}
        blobs.append(blob)
        offset += len(blob)
    tensors["__metadata__"] = {"format": "pt", "source": f"{REPOSITORY}@{REVISION}",
                               "upstream_sha256": UPSTREAM_SHA256, "config": json.dumps(CONFIG, sort_keys=True)}
    encoded = json.dumps(tensors, sort_keys=True, separators=(",", ":")).encode()
    encoded += b" " * (-len(encoded) % 8)
    return struct.pack("<Q", len(encoded)) + encoded + b"".join(blobs)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--output", type=Path, default=ROOT / "target/models/dinov2-small")
    args = parser.parse_args()
    cache = ROOT / "target/models/cache"
    upstream = fetch(f"https://huggingface.co/{REPOSITORY}/resolve/{REVISION}/model.safetensors",
                     UPSTREAM_SHA256, cache / f"dinov2-small-{REVISION[:12]}.safetensors")
    license_text = fetch(LICENSE_URL, LICENSE_SHA256, cache / "dinov2-LICENSE")
    model = to_f16(upstream)
    digest = hashlib.sha256(model).hexdigest()
    if OUTPUT_SHA256 and digest != OUTPUT_SHA256:
        sys.exit(f"fetch-dinov2: converted model has SHA-256 {digest}, expected {OUTPUT_SHA256}")
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "model.safetensors").write_bytes(model)
    (args.output / "LICENSE").write_bytes(license_text)
    (args.output / "SOURCE.json").write_text(json.dumps({
        "model": f"https://huggingface.co/{REPOSITORY}", "revision": REVISION, "license": "Apache-2.0",
        "license_url": LICENSE_URL, "upstream_sha256": UPSTREAM_SHA256, "sha256": digest,
        "conversion": "float32 -> float16 (round to nearest even), mask token removed"}, indent=2) + "\n")
    print(f"{args.output / 'model.safetensors'} {len(model)} bytes sha256 {digest}")
    print(f"license sha256 {hashlib.sha256(license_text).hexdigest()}")


if __name__ == "__main__":
    main()
