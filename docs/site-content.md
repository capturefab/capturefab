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
| capture, frame, preview | Acquire an image, an acquired image unit, and the small JPEG `inspect` writes; `src/engine.rs`, `src/frame.rs`, `src/inspect.rs`. |
| save, file, filename, path, directory, template | Output and filesystem concepts of `capture`, `record` and `-o`. |
| session, worker, job, CLI, JSON, schema | Local automation and process terms; `src/session.rs`, `src/ipc.rs` and the installed `schema`/`--help`. |
| preset, user set | A settings document on this computer, and the camera's own saved settings. |
| stream, streaming, recording, encoder, bitrate | Media operations or option names. Use nouns; avoid progressive verb phrases. |
| exposure, gain, histogram, focus, sharpness, region, calibration | Camera and image-measurement terms. Availability depends on the source. |
| GigE Vision, USB3 Vision, GenICam, ONVIF, FFmpeg | Exact protocol, specification or product names. |
| data fields, error codes, environment variables, filenames | Literal identifiers. Do not change spelling to satisfy general word substitutions. |

## Evidence and audit limits

Sentence counts are an editorial aid, not a complete STE validation.
No certification or universal camera-compatibility claim is made.
Do not publish camera test results or compatibility claims while the software develops quickly.
Examples must come from the CLI: run them against the simulator (`-c sim:0`) with isolated
`CAPTUREFAB_SESSION_DIR` and `CAPTUREFAB_CACHE_DIR` folders before publishing them.

After a documentation edit, regenerate `web/search-index.json` with `python3 scripts/docs-search.py`.
Run its `--check` mode, `node scripts/test-docs-search.js` and `python3 scripts/test-site.py`.
Render the pages on desktop and mobile before publication.

## Documentation coverage

The public documentation has these entry points:

- `web/index.html`: what Capturefab is, installation and examples.
- `web/docs.html`: setup and task guides.
- `web/reference.html`: every command, option, result field, exit code and error code.
- `web/agents.md` and `web/llms.txt`: instructions and an index for AI agents.
- `web/guides/`: the user guides in `docs/`, published by `scripts/site-guides.py`.

`web/reference.html` is generated, never edited by hand. `scripts/site-reference.py --binary
target/debug/capturefab` records the binary's `schema`, `schema COMMAND` and `-h` examples in
`docs/cli-schema.json` and writes the page from it; without `--binary` it writes the page from
that snapshot. `python3 scripts/test-site.py` fails when the page differs from the snapshot, and
CI fails when the snapshot differs from the built binary. Every command, option and result field
is therefore documented by construction.

Search indexes every HTML page with `data-doc-section` articles. Each article must have a unique
local anchor, matching title and heading, and a summary. Landing-page CLI examples must also
appear in a `docs.html` example, so the task guides keep them current.

For a new feature, add instructions, expected results and applicable limits to the task guides,
regenerate the reference and search, and update `web/agents.md` when agents should use it. For a
removed feature, remove its instructions. A passing check does not establish hardware
compatibility or prove that every sentence is accurate; review behavior and examples against the
binary.
