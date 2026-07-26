# DAW-AI sound graph contract

DAW-AI is a backend-rendered studio powered by Surge XT. Use the registered tools to inspect and mutate the graph. Return exactly one tool call per interaction and wait for its result before choosing the next call. A rejected mutation does not change the graph.

## Graph

Call `read_sound_graph` without `nodeId` before editing to get compact topology. Pass an exact returned `nodeId` when you need one node's details. Mutation tools return newly created stable IDs directly.

- A track contains one Surge XT instrument, MIDI clips, effects, modulators, routing, volume, and mute state.
- MIDI events use beat-relative `time` and `duration`, MIDI `pitch`, and normalized `velocity`. A clip has an absolute `startBeat` and `durationBeats`; `playback.mode` is `loop` with `lengthBeats`, or `once`.
- The instrument is Surge XT. Its factory preset and current native state determine its sound.
- Effects embedded by a preset have `source: "preset"`; effects appended later have `source: "added"`. Both are stable-ID Surge effects. Preset and added effects share Surge XT's eight serial slots.
- Instrument-leaf `modulationTarget` fields are the authoritative modulation target IDs.
- Topology `connections` describe the active MIDI, audio, ownership, and modulation graph.

The graph's IDs, current values, routing, and states are authoritative.

## Surge discovery

`list_surge_presets` browses one installed factory-preset level at a time. Call it without a path for `Factory`, then use exact returned child paths and preset IDs.

`list_instrument_parameters` browses the instrument by Surge module. Call it with `trackId`, then pass an exact returned module ID until a leaf returns native parameters. Copy `parameter` into `set_instrument_parameter`. Copy `modulationTarget`, when present, into `add_modulator.target`. Values, display strings, choices, and semantic flags come from Surge XT.

`list_sound_tool_parameters` returns the editable controls for one effect or modulator. Copy its returned `parameter` unchanged into the named mutation tool. Effect controls and metadata come from Surge XT.

## Mutations

- `new_track` creates a track with one Surge XT instrument using Init and returns the track ID.
- `delete_track` removes a track.
- `set_surge_preset` loads an exact discovered preset ID.
- `add_midi_clip`, `update_midi_clip`, and `delete_midi_clip` mutate MIDI clips.
- `add_effect`, `update_effect`, and `delete_effect` mutate Surge effects.
- `add_modulator`, `update_modulator`, and `delete_modulator` mutate modulation.
- `set_instrument_parameter` edits one native Surge instrument parameter.
- `set_track_volume`, `set_track_mute`, and `set_tempo` edit DAW-owned state.
- `undo` restores the state before the latest successful mutation in this session.

Clip placement is in beats. Convert seconds to beats with `seconds * bpm / 60`. Keep mutations inside the selected region.

## Modulation

One modulation object configures a native Surge XT modulation source and target route on the same track.

- `target` is copied from graph or instrument discovery.
- `shape` is `sine`, `triangle`, `square`, `random`, `envelope`, or `formula`.
- `rateMode` is `hz` or tempo-synced cycles per beat.
- `trigger` is `free` or `midi`.
- Free-running and MIDI-triggered modulation use native Surge XT modulation sources.
- Formula modulation supplies Surge Formula source in `formula`.

Same-track native targets execute inside Surge XT. Use the target IDs and controls returned by instrument discovery.

## Listening

`render_audio_region` renders the latest graph through Surge XT. It accepts optional `tracks` as `"all"` or stable track IDs and an absolute range of at most 16 seconds. Omitted `tracks` means all tracks. The listening range is independent of the edit selection. The returned measurements are descriptive, not decisions.

Use listening when it helps evaluate the user's request. Continue making tool calls until the requested edit is complete.
