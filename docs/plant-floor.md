# Camera capture on the plant floor

Capturefab collects images and video with camera identity, capture times and your tags. A camera dataset catalogs the files and notes for later analysis.

## Try a complete capture task

Start a persistent session with a dataset:

```sh
capturefab serve --name line1 --dataset plant.duckdb
```

In another terminal, capture from the built-in camera simulator and record a note:

```sh
capturefab --session line1 --camera sim:0 capture -o 'line1/{date}/{camera}-{datetime}-{seq}.png' --context line=L1 --context part=A-100
capturefab --session line1 dataset note "shift B start" --context line=L1
capturefab --session line1 dataset query "SELECT camera_id, path, wall_ms FROM captures LIMIT 20" --json
capturefab --session line1 dataset export -o exports/shift-b --format csv
```

The export contains capture and recording metadata, notes, a manifest and a README. Images and videos stay at their recorded paths. Exports and captures never overwrite existing outputs.

## Use the desktop

Connect a camera, choose a destination in **Capture**, fill in **Capture tags** and select **Capture** or **Record**. The dataset opens automatically with the first saved file when **Catalog captures automatically** is enabled. Each camera window keeps its preview and controls together.

Use **Capture All** to save one image from every connected camera. Set a schedule for repeated images, or a recording duration for video. Storage limits apply to all saved outputs.

See [CLI workflows](cli-workflows.md) for repeatable camera setups, the [line computer guide](line-computer.md) for a headless service, and [the README](../README.md#save-to-drives-and-folders) for folders, removable drives and mounted network drives.
