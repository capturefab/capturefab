# Website content and terminology

This record applies to the public product page, download page and task documentation in `web/`.
The source repository's implementation and release manifest determine factual claims.

## Language source

Use [ASD-STE100 Issue 9, 15 January 2025](https://www.asd-ste100.org/assets/files/ASD-STE100_ISSUE9.pdf).
The official [STEMG FAQ](https://www.asd-ste100.org/STE_faq.html) explains vocabulary, technical terms and writing scope.
These are the editorial sources, not a certification service.

Apply these rules to new and edited prose:

- Limit an instruction sentence to 20 words and a descriptive sentence to 25 words.
- Put one instruction in each sentence. Use an imperative verb.
- Use active voice, one topic per sentence and one topic per paragraph.
- Keep paragraphs to six sentences or fewer.
- Put necessary conditions before their instructions.
- Use the approved meaning and part of speech for general vocabulary.
- Keep exact interface labels, protocol names, command syntax and identifiers.
- Use tables for platform mappings and ordered lists for procedures.

## Vocabulary decisions

The Issue 9 recurring-errors table and dictionary examples support these choices:

| Avoid in ordinary prose | Use |
| --- | --- |
| check as a verb | do a check; examine; make sure |
| inspect when a general examination is meant | examine |
| press a key | push a key |
| need or require as a verb | necessary; must have |
| complete as an adjective | full or completed, as the meaning permits |
| begin | start |
| repeat | do again |
| any as an adjective | identify the applicable item or rewrite the sentence |

Do not apply word replacements inside commands or official technical terms.
For example, “complete corresponding source” has a specific GPL meaning.

## Capturefab technical terms

These terms have specific software meanings in this project.
They supplement general STE vocabulary; they do not exempt ordinary prose from the writing rules.

| Term or family | Meaning and source |
| --- | --- |
| capture, frame, preview | Acquire an image, an acquired image unit, and its displayed view; `src/engine.rs`, `src/frame.rs`, GUI preview code. |
| save, file, filename, path, directory | Software output and filesystem concepts used by the CLI and Capture inspector. |
| select, click, focus, field, checkbox, inspector | Software interface actions and controls. Preserve exact labels from `src/gui/`. |
| session, worker, headless, CLI, RPC, JSON | Local automation and process terms; `src/session/` and installed `schema`/`--help`. |
| upload, queue, S3, bucket, destination | File-transfer and storage terms; destination and upload implementations. |
| stream, streaming, forwarding, recording, encoder, bitrate | Media operations or control names. Use nouns; avoid progressive verb phrases. |
| exposure, gain, histogram, focus region, calibration | Camera and image-measurement terms. Availability depends on the source. |
| GigE Vision, USB3 Vision, GenICam, ONVIF, FFmpeg | Exact protocol, specification or product names. |
| data fields, environment variables, filenames and UI labels | Literal identifiers. Do not change spelling to satisfy general word substitutions. |

## Evidence and audit limits

The content pass shortened documentation sentences, separated procedural actions and removed repeated descriptions.
It retained command examples, section anchors and limits.
It added archive extraction, actual executable names, operating system floors and camera-profile limits.
Those changes use `scripts/release.py`, `docs/release.md`, `docs/camera-compatibility.md` and the current GUI implementation as evidence.

Sentence counts are an editorial aid, not a complete STE validation.
A full dictionary-by-dictionary language review has not been completed.
No certification or universal camera-compatibility claim is made.
Screenshots and recordings must identify simulator data and their development build provenance.
A development screenshot does not establish behavior in every published release.

After a documentation edit, regenerate `web/search-index.json` with `python3 scripts/docs-search.py`.
Run its `--check` mode, `node scripts/test-docs-search.js` and `python3 scripts/test-site.py`.
Render the pages on desktop and mobile before publication.
