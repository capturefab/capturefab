# Prompt-driven video analytics

Capturefab can sample a live camera and ask OpenAI (the API for ChatGPT models), Gemini or Claude to produce numeric measurements. Each measurement has a name, a short prompt and an optional unit. The model receives the configured prompts together with one sampled image and returns structured JSON. Capturefab validates the response before inserting the measurements into its session DuckDB database.

## API keys

In desktop **Settings**, expand **Prompt measurement keys** (or select **Manage API keys in Settings…** in **Analytics**), choose a provider and enter its API key in the password field. Saving places the key in the OS native credential store: macOS Keychain, Windows Credential Manager or Linux Secret Service. Saved keys can be replaced or removed in Settings. A locked or unavailable credential store produces an error; Capturefab does not fall back to saving keys in a preferences file.

Alternatively, set the provider's standard environment variable before launching Capturefab:

```sh
export OPENAI_API_KEY='your-openai-api-key'
export GEMINI_API_KEY='your-gemini-api-key'
export ANTHROPIC_API_KEY='your-anthropic-api-key'
capturefab
```

Gemini also accepts `GOOGLE_API_KEY`. When both Gemini variables are set, `GOOGLE_API_KEY` takes precedence. A nonempty environment key takes precedence over the key saved for that provider. Remove the environment variable and relaunch to use a saved key instead. Applications launched from a desktop icon may have a different environment from a terminal.

Credential status shows the source without revealing the key. Keys are excluded from ordinary preferences, measurement history and charts. Credential access and provider calls run on background workers.

These keys are shared with the [camera assistant](assistant.md). Both use credential-store service `capturefab-ai`, with accounts `openai`, `gemini` and `claude` for OpenAI, Gemini and Anthropic. Saving or removing one of these keys affects both features. Other assistant providers have separate accounts. Assistant configuration lives in `assistant.json`; it does not change measurement prompts or start collection.

The CLI can manage the same saved keys without putting them in arguments:

```sh
capturefab assistant key set anthropic --key-stdin < /private/path/provider-key.txt
capturefab assistant key remove anthropic
capturefab assistant providers --json
```

The provider list uses environment presence and saved-key hints by default. Add `--check-keychain` only when you want credential-store access. Keys and credential-bearing URLs must never appear in measurement prompts or assistant requests.

The environment variable names and authentication follow the official [OpenAI quickstart](https://developers.openai.com/api/docs/quickstart), [Gemini API key guide](https://ai.google.dev/gemini-api/docs/api-key) and [Claude API key guide](https://platform.claude.com/docs/en/get-api-key).

## Configure measurements

1. Connect a camera, select it and start its live stream.
2. Open **Analytics** and choose a provider and a vision model that supports structured JSON output. The model name is editable.
3. Add up to 16 measurements. Give each a name, a prompt and its expected unit.
4. Choose a sampling interval from 5 to 3,600 seconds; the default is 30 seconds.
5. Start collection. Sampled images and measurement prompts are sent to the chosen provider using your API account.

Examples:

| Name | Prompt | Unit |
| --- | --- | --- |
| Tubes | Count the number of tubes visible in the image. | tubes |
| Ribbon length | Estimate the length of ribbon in meters. Use the visible ruler as a scale reference. | m |
| Ball distance | Estimate the distance of the ball from the camera in meters. The ball diameter is 0.22 meters. | m |

Each prompt requests one number. Include the meaning of the number, the desired unit and any known scale or camera calibration needed to interpret it. Physical lengths and distances need enough visual or supplied context to support an estimate. The model is instructed to return an unknown value when it cannot determine the measurement.

Collection is bound to the camera selected when it starts. Changing the measurement configuration pauses collection; explicitly start it again to apply the new configuration. Preferences remember prompts, provider, model and interval, while collection always starts paused after launching the app.

Capturefab samples the latest available frame, skips a repeated frame and makes at most one provider request at a time. The interval begins after a request completes, so slow responses do not create overlapping calls or a catch-up burst. Stopping prevents further calls and discards an unfinished request's response. The underlying HTTPS request may finish after stopping. Provider, validation or database failures pause collection and show the error. Stopping the camera stream or disconnecting the camera also ends collection.

## Structured history and visualizations

A measurement result contains its configured ID, a numeric `value` or JSON `null`, optional confidence from 0 to 1, and a short note. Unknown values remain null in DuckDB and appear as gaps in plots. Failed requests do not insert invented zeros. Capturefab rejects missing, duplicate or unexpected IDs, invalid numbers and malformed structured responses. All measurements from one frame are inserted together in a transaction.

History records the sampling timestamp, camera ID, frame ID, measurement definition, provider, model, value, confidence and note. The charts keep different cameras, prompts, units and models in separate series. Current values and notes are visible in the Analytics panel.

As with capture-counter analytics, measurement history belongs to the running process and is stored in memory. It retains at most 24 hours and 10,000 measurement rows. Closing Capturefab clears the history; saved prompt preferences survive. Raw frames and API keys are not stored in the analytics database. The chart displays up to 24 series panels.

When the optional HTTP listener is enabled in an `ai` build, `GET` and `HEAD /api/measurements` expose the retained measurement rows in the standard JSON envelope, and `/api/measurements.svg` returns the Plotters visualization. The routes enforce the same origin checks as the existing analytics endpoints. They expose measurement history, without any credential configuration or collection controls.

The desktop `gui` feature enables `ai`. Minimal builds with `--no-default-features` omit the integration unless `--features ai` is supplied.
