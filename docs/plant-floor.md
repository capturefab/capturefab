# Camera capture on the plant floor

Capturefab saves images and video with names, folders and tags that analysis tools can read
without a database: the file name and folder carry the camera, time, sequence and your labels,
and each PNG or JPEG carries its tags and camera settings in its metadata.

## Name files for analysis

```sh
capturefab capture -c sim:0 -n 20 -o 'line1/{date}/{tag:part}/{camera}-{time}-{seq:4}.png' --tag part=A-100 --tag shift=B
```

| Placeholder | Becomes |
| --- | --- |
| `{camera}` | the camera's model and serial, safe for file names |
| `{serial}` | the serial number |
| `{seq}`, `{seq:N}` | 1, 2, 3 …, padded to N digits |
| `{time}` | the frame's time, `20261031-142233.123` |
| `{date}` | the date, `2026-10-31` |
| `{tag:KEY}` | the value of `--tag KEY=VALUE` |
| `{ext}` | `png`, `jpg` or `raw` |

To sort images into a folder per class while capturing, put a tag in the path:
`-o 'labels/{tag:label}/{seq:5}.png' --tag label=defect`. Existing files are never overwritten: a
name without `{seq}` that exists fails with `exists`, or `--on-exists number` adds `-0001`,
`-0002` …

## Keep measurements exact

PNG keeps 8-bit images losslessly; `jpg` is smaller (`--quality`, 90 by default). `raw` writes
the frame's bytes exactly as the camera sent them, at their full bit depth: use it for Bayer data
and for depth or thermal samples, and record the camera's calibration features alongside
(`capturefab get -c CAM PixelFormat Width Height …`). The JSON result of every capture lists the
files with their size, pixel format and the camera settings the frame was taken with, which a
script can append to its own log:

```sh
capturefab capture --json -c sim:0 -o 'parts/{seq}.png' --tag part=A-100 >> captures.jsonl
```

## Repeat a setup across cameras and shifts

```sh
capturefab preset save line1 -c sim:0 --note 'ring light 60%, f/8'
capturefab preset export line1 -o line1.json          # keep it with the line's documents
capturefab preset apply line1 -c sim:1 --dry-run       # what would change on another camera
capturefab preset apply line1 -c sim:1
```

A preset applies as one change, and puts every value back if any write fails. `--strict` refuses
to apply anything when a saved value does not fit the camera; `--persist` also stores the result
in the camera's user set, so it survives a power cycle.

## Run it unattended

A time lapse or a triggered station keeps running in a session:
`capturefab capture -c CAM --every 1m --for 8h -o 'shift/{seq:4}.jpg' --background`. See
[CLI workflows](cli-workflows.md) for complete recipes and the
[line computer guide](line-computer.md) for a session that starts at boot.
