# Folder review

Folder review finds what is worth a look in a folder of images and videos, such as field photos, earlier captures or long recordings. It reuses auto capture's encoders and scoring for files on disk instead of a live camera: every item is embedded once, the most interesting pending item is offered, and each decision reranks the rest straight away. The CLI is `capturefab folder-review`; the desktop app has **File › Review a Folder…**.

## What a session holds

A session is one DuckDB file:

- `review_session` holds the folder's absolute path, the encoder and model fingerprint, the rank mode, the sensitivity, the video sampling interval and the time of the last scan.
- `files` holds every media file found, by path relative to the folder, with its size and modification time and whether it decoded (`ok`, `failed` with the error, or `missing`).
- `items` holds one row per still and one per sampled video frame (`offset_ms`). Each row has the size and modification time its file had when it was sampled, its embedding, its label (`pending`, `kept`, `discarded` or `duplicate`, and for a duplicate which item it repeats), the score and reason shown when it was decided, the ranking state described below, and where it was exported.
- `examples` holds the descriptors imported with `--examples`.

The folder is never written. Frames are decoded from their files again whenever they are shown or exported. An item whose file's size or modification time differs from when it was sampled is not shown or exported: its pending items leave the queue with a note, and the next scan samples the file again. Desktop sessions are kept in the state directory under `folder-reviews/`, named after the folder and a hash of its path, so choosing the folder again resumes its review.

## Scanning

The walk visits every regular file below the folder whose extension is a supported image or video, sorted by path. Hidden entries and symbolic links are skipped, never followed, and the walk stops at 32 levels and 20,000 media files. JPEG and PNG decode in process at full size. Other stills decode through FFmpeg, scaled to fit 1920×1080, and each video sample is a FFmpeg seek to the next multiple of `--video-interval-ms`, scaled into a 960×540 box with its aspect ratio kept, up to 600 samples per video. Showing a video frame makes the same seek, so it is exactly what was embedded.

Each file's rows are committed in one transaction after the file is sampled. A stopped or interrupted scan therefore keeps every finished file and resamples only the file it was working on. A rescan:

- samples new files;
- resamples files whose size or modification time changed, and files that failed before (FFmpeg may since have become available);
- marks files that are gone as `missing`.

When a file is resampled or goes missing, its pending items and duplicates are deleted and its decisions are kept. A duplicate in another file that repeated one of those deleted items becomes pending again. A session holds at most 20,000 items, so that ranking fits in memory and a decision takes one pass over the pending items; rows a resampled file replaces do not count. When a scan reaches that limit it says so (`limited`), and a video cut short by it is recorded as failed, so a later scan tries it again.

## Ranking

Distances are the encoder's own (see auto capture): half the chord length between DINOv2 unit vectors, or the RMS difference of native descriptors. Every pending item keeps three distances: to the nearest reviewed item, to the nearest kept item or positive example, and to the nearest discarded item or negative example. A decision can only lower these distances, so it updates all of them in one pass and saves only the ones that changed, in the same transaction as the decision. Reopening a session restores the ranking exactly.

**Novelty** (`--rank novelty`, the default without examples) is farthest-point sampling.

- An item's novelty is its distance to the nearest reviewed item divided by the encoder's novelty scale, capped at 1. Before anything is reviewed, the distance is measured to the mean of all pending items instead.
- The score is that novelty times one minus the similarity to the nearest discarded item, `exp(-d²/2σ²)`. A discard therefore pushes down its look-alikes as well as covering its neighbourhood.
- Reviewing the top item each time spreads a small review budget across the whole folder, rather than spending it on many variants of one scene.

**Similar** (`--rank similar`, the default with `--examples`) is auto capture's preference.

- Affinity is the similarity to the nearest kept item or positive example.
- If any discarded items or negative examples exist, affinity is multiplied by `d_discarded / (d_kept + d_discarded)`.
- The score is that preference times `0.9 + 0.1 × novelty`.
- With nothing kept yet, items rank by novelty, so the first examples can still be found.

`next` offers the best pending item whose score is at least `0.90 − 0.35 × sensitivity`. This is the threshold auto capture uses to suggest a frame. `--all`, or **Also show items below the threshold** in the app, offers the best pending item regardless.

**Repeats.** A scanned item closer than the encoder's duplicate distance (0.08 for DINOv2, 0.018 native) to either of the following is labelled `duplicate` and never queued:

- one of the 64 items scanned just before it;
- a reviewed item.

A decision also turns pending look-alikes within that distance into duplicates of the decided item.

## Examples from auto capture

`new --examples CAPTURE.duckdb` opens a capture session read-only. It copies up to 1,024 of that session's newest captures: manual and kept ones as positive examples, and discarded ones as negative examples. The descriptors only compare within one model, so the review must use the capture session's encoder and model fingerprint. Without `--encoder` (or with `auto`) the review adopts it, and any other explicit encoder is refused. The capture session is not changed. If the desktop app has it open, close it there first: DuckDB lets only one process hold a session file.

## Export

`export` copies kept items that have not been exported yet into `--to DIR`, or into a saved `--destination` (by default its `folder-review` subfolder), keeping their paths relative to the folder:

- Stills are copied byte for byte.
- Video frames are saved as `STEM-OFFSETms.png`.
- Every file goes through the storage ledger and budget and never replaces anything; a taken name gets `-1`, `-2` and so on.
- A bucket destination queues each file for upload.

Items whose file changed since they were sampled are skipped and reported, even after a rescan, since the decision was about the old content; and each export is recorded, so running export again copies only items kept since.
