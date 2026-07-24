# DAW-AI

DAW-AI is a local, prompt-driven music studio for making music without learning a traditional DAW. Select a region of the timeline, describe the change in everyday language, and hear the arrangement update immediately.

The project is a small Rust server with a responsive browser client. Surge XT renders the sound graph by default, with a custom built-in Rust backend available for debugging, and the browser only plays the resulting WAV. Prompted edits are produced by Gemini 3.6 Flash, which can hear renders made by the selected backend.

## Run it

Prerequisites:

- Rust 1.85 or newer
- `curl`
- A [Gemini API key](https://ai.google.dev/gemini-api/docs/api-key)
- `just` (optional, but recommended)

Set the standard environment variable:

```sh
export GEMINI_API_KEY="your-key"
```

For a system service, use systemd's `LoadCredential=` with a credential named `gemini-api-key`; DAW-AI automatically reads it from `CREDENTIALS_DIRECTORY`. This keeps the key out of the unit environment, process arguments, and home directories. For interactive use, `~/gemini_creds.txt` remains a fallback. A raw key, `GEMINI_API_KEY=...`, `Gemini API key: ...`, and `export GEMINI_API_KEY=...` are accepted. `DAW_AI_GEMINI_API_KEY` and `DAW_AI_GEMINI_CREDENTIALS` provide explicit overrides.

The model can search and load Surge XT's factory `.fxp` library. Development builds discover it in the pinned Surge checkout automatically. Packaged deployments should copy Surge's `resources/data/patches_factory` directory to `/usr/local/share/daw-ai/patches_factory`, or set `DAW_AI_SURGE_PRESET_DIR` to its installed location.

Start the studio on the charter's default port:

```sh
just run
```

Then open <http://127.0.0.1:8888>.

Use another port when needed:

```sh
just run 9000
```

You can also run it directly:

```sh
cargo run -- --port 8888
```

## Studio workflow

1. Drag over any part of the arrangement to set the edit region. On touch devices, swipe to pan normally or tap **Select region** before dragging a selection.
2. Enter a request such as `increase the volume`, `add a bass`, `make the chords warm and spacious`, or `turn this section into a dubstep drop`.
3. Press **Make change**, then use the transport to hear the result. The button becomes **Interrupt** while Gemini is working.
4. Use session history to inspect earlier states and move forward again, or download the complete arrangement with **Export WAV**.
5. Switch to **Advanced** to edit MIDI notes in the piano roll and select instrument, effect, and modulator nodes in the sound graph to edit their parameters. Tracks can also be created and deleted there. The **Debug** tab selects the Surge XT or built-in sound engine for instruments and effects, lists retained Gemini sessions, and provides a copyable environment and browser-error report.

No login is required. DAW-AI assigns each browser a private random cookie and stores its project under `users/<cookie>/sound-graph.json` beside `DAW_AI_PROJECT_PATH` (or beside the working-directory default). Each user has independent edit jobs, history, playback, backend selection, and project state. DAW-AI creates the demo graph for a new user and safely saves every accepted prompt, mixer change, Advanced edit, undo, reset, and history selection.

For each prompt, Gemini receives the selected edit range and the checked-in synth contract under `gemini/`. It can read the latest graph; create, update, and delete tracks and their MIDI clips, effects, and modulators through narrow stable-ID functions; set instrument and routing parameters, track mute, and tempo; undo its latest mutation; search for musical context; and choose channels plus absolute project start/end times to render as WAV audio directly into its next multimodal turn. Listening is independent of edit scope and entirely model-directed: the integration encourages it when useful but never requires it. The Rust backend renders the project without depending on the user's tab.

Gemini may render before or after edits whenever listening would help, but no separate model reviews or rejects its completion decision. It may also complete based on graph inspection alone. There is no predetermined iteration or tool-call limit; the overall 20-minute request timeout is the loop boundary. The server publishes each successful atomic mutation as an undoable edit while Gemini is still working. Direct Advanced edits and channel creation or deletion use the same persisted graph.

Prompted edits run as asynchronous jobs so reverse proxies never need to hold one request open while Gemini works. The browser polls short status requests, fetches each published intermediate project and the completed project, and shows the current phase, applied steps, and elapsed time. Gemini may spend up to 20 minutes on an edit; if the project changes before that edit finishes, the result is rejected instead of overwriting newer work.

Every Gemini session is retained locally with request/response JSON, graph state, metadata, and rendered WAV artifacts. By default sessions live beside `DAW_AI_PROJECT_PATH` in `gemini-sessions/`, or in the working directory's `gemini-sessions/` when no project path is configured. Override this with `DAW_AI_GEMINI_SESSION_DIR`. The Debug tab lists the latest sessions by timestamp.

Completed and failed sessions are retained for 30 days, up to 100 sessions and 512 MiB per session directory root. Under storage pressure, old WAV artifacts are pruned before old session records; running sessions are never removed. Configure the limits with `DAW_AI_GEMINI_SESSION_RETENTION_DAYS`, `DAW_AI_GEMINI_SESSION_RETENTION_COUNT`, and `DAW_AI_GEMINI_SESSION_RETENTION_BYTES`.

## Development

Development checks additionally require Node.js 22 or newer and Chrome or Chromium. Set `CHROME_PATH` when the browser workflow suite cannot discover the executable. Verify browser discovery with `just qa-browser-setup`.

Run formatting checks, Clippy with warnings denied, Rust tests, and the headless-browser workflow suite:

```sh
just test
```

The server binds only to `127.0.0.1`, requires no web authentication, and embeds the client assets in the executable. Its same-origin API lives under `/api`. Server warnings and errors go to stderr; handled and unhandled browser errors are forwarded to the same log with bounded messages and retained in the Debug report for the current page session. Reverse proxies can publish any valid hostname without DAW-AI configuration, whether they preserve `Host` or provide the public authority as `X-Forwarded-Host`. Cross-origin mutations remain rejected. The test suite injects the deterministic demo planner with `DAW_AI_PROMPT_ENGINE=demo` and an isolated project path, so CI never needs Gemini credentials or model usage.

### Dependency policy

Third-party packages are reserved for complex, standards-sensitive boundaries or charter-mandated sound engines. `serde_json` handles JSON parsing and string escaping for Gemini and persisted project data. DAW-AI's own source code is MIT-licensed. The official alpha `surge-rs` binding builds and statically links the complete Surge XT engine; because Surge XT and its binding are GPL-3.0-or-later, distributions of the combined binary must comply with GPL-3.0-or-later. Domain-specific sound-graph validation and serialization remain in the project, while the narrow HTTP server, form decoder, temporary-file handling, CLI parser, browser harness, and `curl`-backed outbound HTTPS boundary continue to use platform tools and APIs.
