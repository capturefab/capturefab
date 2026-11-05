# Input fuzz tests

Run the deterministic mutation and generated-property tests with:

```sh
cargo test --lib fuzz_
cargo test --lib fuzz_ --no-default-features
```

These run in normal `cargo test` CI without extra dependencies or hardware.
The fixed PRNG seeds make failures reproducible. Mutation failures include the
input; keep a minimized failing input as a regression when fixing a bug.

Coverage lives in `src/cli/fuzz.rs` and `controls::tests::fuzz_feature_globs`:

| Input surface | Checks |
| --- | --- |
| CLI arguments (including environment-backed options) | Discover every command, nested command, positional argument and value-taking option from Clap; parse mutated values and invalid UTF-8 on Unix without dispatching commands |
| Shell lines | Quote/escape parsing and arbitrary-token quoting round trips |
| Durations, waits, times, byte sizes, debug hex | Mutated text, overflow and Unicode; hex and 10,000 generated nanosecond duration round trips |
| Local paths | Source/output resolution and normalization idempotence |
| Features, assignments, canonical controls | Parse/validate and name round trips; simulator dry runs for every canonical control preserve readable control state |
| Feature patterns | Exhaustive comparison with a small reference glob matcher; long adversarial patterns |
| Output templates and context | Template grammar, tag keys, path-component sanitization, context parsing and validation |
| Camera selectors and credentials | Media, simulator and ONVIF shape validation, credential splitting/decoding, URL redaction idempotence and password stdin semantics |
| Session JSON | Raw mutations plus valid fixtures for every command; recursively replace containers and leaves, remove fields, mutate array elements; deserialize/validate/serialize round trips |
| Preset files | Raw document mutations and structured mutations of a valid preset, including selector banks; accepted documents validate and round trip |
| GenICam XML | Mutations of XML and arbitrary text through the bounded XML parser |

The corpus includes empty strings, numeric extremes, control characters,
malformed syntax, multibyte Unicode, credential URLs and byte-length boundaries.
Credential parsing runs in a separate test process because registering arbitrary
secrets must not affect concurrently running tests. Tests never dispatch a
generated CLI or session command. Control semantics use the in-process simulator.

For a longer campaign, override the mutation count per corpus (CLI and structured
JSON use smaller defaults because they run a corpus for each argument or field):

```sh
CAPTUREFAB_FUZZ_CASES=100000 cargo test --lib fuzz_text_inputs
CAPTUREFAB_FUZZ_CASES=4096 cargo test --lib fuzz_structured_commands
CAPTUREFAB_FUZZ_CASES=4096 cargo test --lib fuzz_cli_arguments
```

These are deterministic mutation tests, not a coverage-guided engine or proof
that every possible input is safe. Camera/network packets, platform drivers and
third-party media codecs also have their own tests; this suite targets inputs
Capturefab parses from its users.
