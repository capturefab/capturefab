# Camera capture on the plant floor

Use filenames and tags to tie each image to its camera, part and shift. PNG and JPEG
images also carry the tags and available camera settings in their metadata.

## Name files for analysis

```sh
capturefab capture -c sim:0 -n 20 -o 'line1/{date}/{tag:part}/{camera}-{time}-{seq:4}.png' --tag part=A-100 --tag shift=B
```

| Placeholder | Value |
| --- | --- |
| `{camera}` | Camera label, usually the model and serial number, safe for filenames |
| `{serial}` | Camera serial number, or `unknown` if the serial cannot be used in a filename |
| `{seq}`, `{seq:N}` | Next available sequence number for the filename pattern, padded to four digits or to `N` digits |
| `{frame}` | Frame number within this capture, starting at `000001` |
| `{time}` | Frame time with milliseconds, for example `20261031-142233.123` |
| `{datetime}` | Frame time to the second, for example `20261031-142233` |
| `{date}` | Frame date, for example `2026-10-31` |
| `{tag:KEY}` | Value from `--tag KEY=VALUE`, safe for use in a path |
| `{ext}` | Image extension, `png`, `jpg` or `raw` |

Use `{seq}` in the filename. The date and time use the UTC offset of the command that
requests the capture. To sort images into folders by label, use
`-o 'labels/{tag:label}/{seq:5}.png' --tag label=defect`.

Capturefab picks a free name for templates with `{seq}` or `{time}`. For a fixed name,
Capturefab returns `exists` if the file is present. Use `--on-exists number` to add
suffixes such as `-0001`.

## Preserve sensor samples

| Format | What the file keeps |
| --- | --- |
| PNG | Full precision for monochrome samples from Mono10 to Mono16, including packed formats. Color and Bayer frames become 8-bit RGB. |
| JPEG | 8-bit images with lossy compression. Use `--quality` to choose the quality. |
| Raw | Unencoded frame bytes at their original bit depth, with a JSON sidecar that describes their layout. |

Use raw files for Bayer, depth or thermal measurements that depend on the original samples.
For `depth.raw`, read `depth.raw.json` for the dimensions, pixel format, PFNC code and row length
or plane layout. Save the relevant camera calibration alongside the images.

```sh
capturefab capture -c sim:depth -o depth.raw
capturefab get --json -c sim:depth Scan3dCoordinateScale Scan3dCoordinateOffset > depth-calibration.json
```

The JSON capture result reports the saved paths, dimensions, pixel format and available
settings. Its `bit_depth` gives the output depth. When the output loses precision,
`source_bits` gives the source depth. Use `--manifest` to save a separate record for each
image, with its SHA-256 hash and tags.

```sh
capturefab capture -c sim:0 -o 'parts/{seq}.png' --tag part=A-100 --manifest captures.jsonl
```

## Restore a setup across cameras and shifts

Save the setup on the first camera. Use `--dry-run` to see which settings would change on
the second camera before you apply the preset.

```sh
capturefab preset save line1 -c sim:0 --note 'ring light 60%, f/8'
capturefab preset export line1 -o line1.json
capturefab preset apply line1 -c sim:1 --dry-run
capturefab preset apply line1 -c sim:1
```

By default, Capturefab can skip values the target camera cannot use. Use `--strict` to
treat those values as an error. If the apply fails after a change, Capturefab tries to
restore the previous settings. With `--json`, read `error.details.rollback` when present.

A preset lives on the computer. On a camera that supports user sets, `--persist UserSet1`
also saves the settings on the camera and selects that set for the next power-on.

## Run a capture unattended

Use `--background` to leave a time lapse or a triggered capture running in a session.
See the [background capture example](cli-workflows.md#run-a-time-lapse-in-the-background)
or the [line computer guide](line-computer.md) to run a session at boot.
