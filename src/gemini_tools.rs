use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value as JsonValue};

use crate::audio_analysis::{self, MAX_REGION_SECONDS};
#[cfg(test)]
use crate::model::TrackRole;
use crate::model::{
    MAX_LOOP_PLAYBACK_BEATS, MAX_MIDI_EVENTS_PER_CLIP, MAX_MIDI_NOTE_DURATION_BEATS,
    MAX_ONCE_PLAYBACK_BEATS, MIN_MIDI_NOTE_BEATS, MidiClipSpec, ModulatorSpec,
    PROJECT_SCHEMA_VERSION, Project, Studio, StudioError, TRACK_COLOR_PALETTE, json_string,
};
use crate::prompt::{Action, EditPlan, MAX_COMPOUND_ACTIONS, MidiNote};
use crate::storage::{ProjectStore, read_bounded_text, replace_text_file};

pub(crate) const READ_TOOL_NAME: &str = "read_sound_graph";
pub(crate) const AUDIO_TOOL_NAME: &str = "render_audio_region";
pub(crate) const ANALYZE_AUDIO_TOOL_NAME: &str = "analyze_audio";
pub(crate) const AUDITION_TOOL_NAME: &str = "audition_instrument";
pub(crate) const PRESET_TOOL_NAME: &str = "list_surge_presets";
pub(crate) const INSTRUMENT_PARAMETER_TOOL_NAME: &str = "list_instrument_parameters";
pub(crate) const SOUND_TOOL_PARAMETER_TOOL_NAME: &str = "list_sound_tool_parameters";
const SET_INSTRUMENT_PARAMETER_TOOL_NAME: &str = "set_instrument_parameter";
const SET_INSTRUMENT_PARAMETERS_TOOL_NAME: &str = "set_instrument_parameters";
const UPDATE_EFFECT_PARAMETERS_TOOL_NAME: &str = "update_effect_parameters";
const GRAPH_FILE: &str = "sound-graph.json";
const REQUEST_FILE: &str = "request.json";
const SESSION_FILE: &str = "session.json";
const PROGRESS_DIRECTORY: &str = "edit-progress";
const PENDING_PROGRESS_DIRECTORY: &str = ".edit-progress.pending";
const PROGRESS_PLAN_FILE: &str = "plan.json";
const PROGRESS_GRAPH_FILE: &str = "project.json";
const UNDO_GRAPH_FILE: &str = "undo-sound-graph.json";
pub(crate) const MUTATION_TOOL_NAMES: &[&str] = &[
    "new_track",
    "delete_track",
    "set_track_identity",
    "set_surge_preset",
    "add_midi_clip",
    "update_midi_clip",
    "delete_midi_clip",
    "add_effect",
    "update_effect",
    UPDATE_EFFECT_PARAMETERS_TOOL_NAME,
    "delete_effect",
    "add_modulator",
    "update_modulator",
    "delete_modulator",
    SET_INSTRUMENT_PARAMETER_TOOL_NAME,
    SET_INSTRUMENT_PARAMETERS_TOOL_NAME,
    "set_track_volume",
    "set_track_mute",
    "set_tempo",
    "undo",
];
const AUDIO_REGION_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["start", "end"],
  "properties": {
    "tracks": {
      "description": "Tracks to render. Omit or use \"all\" for the full mix, or provide stable track IDs from sound-graph.json to isolate selected tracks.",
      "oneOf": [
        { "type": "string", "enum": ["all"] },
        {
          "type": "array",
          "items": { "type": "integer", "minimum": 1 },
          "minItems": 1,
          "maxItems": 32,
          "uniqueItems": true
        }
      ]
    },
    "start": {
      "type": "number",
      "minimum": 0,
      "description": "Absolute start time in project seconds. This listening range is independent of the selected edit region and may include context before it."
    },
    "end": {
      "type": "number",
      "exclusiveMinimum": 0,
      "description": "Absolute end time in project seconds, after start and no more than 16 seconds later. It may include context after the selected edit region."
    }
  }
}"#;
const EFFECT_NAMES: &[&str] = &[
    "Delay",
    "Reverb 1",
    "Phaser",
    "Rotary Speaker",
    "Distortion",
    "EQ",
    "Frequency Shifter",
    "Conditioner",
    "Chorus",
    "Reverb 2",
    "Flanger",
    "Ring Modulator",
    "Airwindows",
    "Neuron",
    "Graphic EQ",
    "Resonator",
    "CHOW",
    "Exciter",
    "Ensemble",
    "Combulator",
    "Nimbus",
    "Treemonster",
    "Waveshaper",
    "Mid-Side Tool",
    "Bonsai",
    "Floaty Delay",
    "Convolution",
];
static SESSION_ID: AtomicU64 = AtomicU64::new(1);
const DEFAULT_SESSION_RETENTION_DAYS: u64 = 30;
const DEFAULT_SESSION_RETENTION_COUNT: usize = 100;
const DEFAULT_SESSION_RETENTION_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SESSION_JSON_BYTES: u64 = 64 * 1024;
const MAX_PROGRESS_PLAN_BYTES: u64 = 64 * 1024;
const MAX_SOUND_GRAPH_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
struct SessionRetention {
    maximum_age: Duration,
    maximum_count: usize,
    maximum_bytes: u64,
}

impl SessionRetention {
    fn configured() -> Self {
        Self {
            maximum_age: Duration::from_secs(
                configured_u64(
                    "DAW_AI_GEMINI_SESSION_RETENTION_DAYS",
                    DEFAULT_SESSION_RETENTION_DAYS,
                )
                .saturating_mul(24 * 60 * 60),
            ),
            maximum_count: configured_u64(
                "DAW_AI_GEMINI_SESSION_RETENTION_COUNT",
                DEFAULT_SESSION_RETENTION_COUNT as u64,
            ) as usize,
            maximum_bytes: configured_u64(
                "DAW_AI_GEMINI_SESSION_RETENTION_BYTES",
                DEFAULT_SESSION_RETENTION_BYTES,
            ),
        }
    }
}

pub(crate) struct EditSession {
    path: PathBuf,
    persistent: bool,
}

impl EditSession {
    #[cfg(test)]
    pub(crate) fn create(
        project: &Project,
        prompt: &str,
        start: f32,
        end: f32,
    ) -> io::Result<Self> {
        Self::create_in(&session_root(), project, prompt, start, end, false)
    }

    pub(crate) fn create_in(
        root: &Path,
        project: &Project,
        prompt: &str,
        start: f32,
        end: f32,
        batch_parameter_tools: bool,
    ) -> io::Result<Self> {
        apply_session_retention_with(root, SessionRetention::configured())?;
        let path = reserve_session_directory(root)?;
        let result = (|| {
            let project = project.clone();
            write_new(&path.join(GRAPH_FILE), &project.to_json())?;
            write_new(
                &path.join(REQUEST_FILE),
                &format!(
                    "{{\"start\":{start},\"end\":{end},\"prompt\":{}}}",
                    json_string(prompt)
                ),
            )?;
            write_new(
                &path.join(SESSION_FILE),
                &serde_json::json!({
                    "id": path.file_name().unwrap_or_default().to_string_lossy(),
                    "createdAt": unix_milliseconds(),
                    "updatedAt": unix_milliseconds(),
                    "status": "running",
                    "model": crate::gemini::GEMINI_MODEL,
                    "prompt": prompt,
                    "start": start,
                    "end": end,
                    "appliedSteps": 0,
                    "audioListens": 0,
                    "batchParameterTools": batch_parameter_tools,
                    "detail": "Gemini session started"
                })
                .to_string(),
            )?;
            Ok(Self {
                path: path.clone(),
                persistent: !cfg!(test),
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&path);
        }
        result
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn synchronize_project(&self, project: &Project) -> Result<(), String> {
        write_replace(
            &self.path.join(GRAPH_FILE),
            &format!("{}\n", project.to_json()),
        )
        .map_err(|error| format!("could not synchronize committed sound graph: {error}"))
    }

    pub(crate) fn record_exchange(
        &self,
        name: &str,
        request: &JsonValue,
        response: &str,
    ) -> io::Result<()> {
        write_new(
            &self.path.join(format!("{name}-request.json")),
            &request.to_string(),
        )?;
        write_new(&self.path.join(format!("{name}-response.json")), response)
    }

    pub(crate) fn record_audio(&self, sequence: usize, wav: &[u8]) -> io::Result<String> {
        let name = format!("audio-{sequence:03}.wav");
        write_new_with(&self.path.join(&name), |file| file.write_all(wav))?;
        Ok(name)
    }

    pub(crate) fn update_status(
        &self,
        status: &str,
        detail: &str,
        applied_steps: usize,
        audio_listens: usize,
    ) -> io::Result<()> {
        let source = self.metadata_source()?;
        self.update_status_from(&source, status, detail, applied_steps, audio_listens)
    }

    pub(crate) fn update_status_from(
        &self,
        source: &str,
        status: &str,
        detail: &str,
        applied_steps: usize,
        audio_listens: usize,
    ) -> io::Result<()> {
        let path = self.path.join(SESSION_FILE);
        let mut value = serde_json::from_str::<JsonValue>(source)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid session record"))?;
        object.insert("status".to_owned(), JsonValue::String(status.to_owned()));
        object.insert("detail".to_owned(), JsonValue::String(detail.to_owned()));
        object.insert("updatedAt".to_owned(), unix_milliseconds().into());
        object.insert("appliedSteps".to_owned(), applied_steps.into());
        object.insert("audioListens".to_owned(), audio_listens.into());
        write_replace(&path, &value.to_string())
    }

    pub(crate) fn metadata_source(&self) -> io::Result<String> {
        read_bounded_text(
            &self.path.join(SESSION_FILE),
            MAX_SESSION_JSON_BYTES,
            "Gemini session metadata",
        )
    }

    pub(crate) fn stats(&self) -> io::Result<(usize, usize)> {
        let source = self.metadata_source()?;
        let value = serde_json::from_str::<JsonValue>(&source)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let applied_steps = value
            .get("appliedSteps")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize;
        let audio_listens = value
            .get("audioListens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize;
        Ok((applied_steps, audio_listens))
    }

    pub(crate) fn finish(&self, plans: Vec<EditPlan>) -> Result<(EditPlan, Project), String> {
        let mut actions = Vec::new();
        let mut summary = None;
        for plan in plans {
            actions.push(plan.action);
            summary = Some(plan.summary);
        }
        if actions.is_empty() {
            return Err("Gemini did not use a registered graph mutation tool".to_owned());
        }
        let action = bounded_compound(actions);
        let graph = read_bounded_text(
            &self.path.join(GRAPH_FILE),
            MAX_SOUND_GRAPH_BYTES,
            "Gemini sound graph",
        )
        .map_err(|error| format!("could not read Gemini sound graph: {error}"))?;
        let project = Project::from_json(&graph)
            .map_err(|error| format!("Gemini left an invalid sound graph: {error}"))?;
        Ok((
            EditPlan {
                action,
                summary: summary.expect("plans were nonempty"),
            },
            project,
        ))
    }

    pub(crate) fn take_update(&self) -> Result<Option<(EditPlan, Project)>, String> {
        let path = progress_path(&self.path);
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!("could not inspect Gemini edit progress: {error}"));
            }
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("Gemini edit progress handoff is not a regular directory".to_owned());
        }
        let plan_source = read_bounded_text(
            &path.join(PROGRESS_PLAN_FILE),
            MAX_PROGRESS_PLAN_BYTES,
            "Gemini edit plan progress",
        )
        .map_err(|error| format!("could not read Gemini edit plan progress: {error}"))?;
        let graph_source = read_bounded_text(
            &path.join(PROGRESS_GRAPH_FILE),
            MAX_SOUND_GRAPH_BYTES,
            "Gemini sound graph progress",
        )
        .map_err(|error| format!("could not read Gemini sound graph progress: {error}"))?;
        let plan = if let Some(summary) = serde_json::from_str::<JsonValue>(&plan_source)
            .ok()
            .filter(|value| value.get("graphMutation") == Some(&JsonValue::Bool(true)))
            .and_then(|value| {
                value
                    .get("summary")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned)
            }) {
            EditPlan {
                action: Action::GraphMutation,
                summary,
            }
        } else {
            return Err("Gemini edit progress did not contain a graph mutation".to_owned());
        };
        let project = Project::from_json(&graph_source)
            .map_err(|error| format!("Gemini edit progress is invalid: {error}"))?;
        fs::remove_dir_all(&path)
            .map_err(|error| format!("could not consume Gemini edit progress: {error}"))?;
        Ok(Some((plan, project)))
    }
}

impl Drop for EditSession {
    fn drop(&mut self) {
        if self.persistent {
            return;
        }
        if let Err(error) = fs::remove_dir_all(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("warning: could not remove Gemini test session: {error}");
            }
        }
    }
}

pub(crate) fn tool_declarations(batch_parameter_tools: bool) -> Vec<JsonValue> {
    let audio_schema = serde_json::from_str::<JsonValue>(AUDIO_REGION_SCHEMA)
        .expect("embedded audio schema is valid JSON");
    let mut tools = vec![
        serde_json::json!({
            "type": "function",
            "name": READ_TOOL_NAME,
            "description": "Read the latest compact sound-graph topology, or pass an exact returned nodeId to inspect that one track, instrument, MIDI clip, effect, or modulator in detail.",
            "parameters": {
                "type": "object",
                "properties": {
                    "nodeId": {
                        "type":"string",
                        "pattern":"^(master|track:[1-9][0-9]*|instrument:[1-9][0-9]*|clip:[1-9][0-9]*|effect:[1-9][0-9]*|modulator:[1-9][0-9]*)$",
                        "maxLength":64,
                        "description":"Exact nodeId returned by a topology read. Omit for compact topology."
                    }
                },
                "additionalProperties": false
            }
        }),
        serde_json::json!({
            "type": "function",
            "name": AUDIO_TOOL_NAME,
            "description": "Render all tracks (the default) or selected track IDs over an absolute project range and return WAV audio for listening. This tool returns audio without measurements. The listening range is independent of the selected edit scope.",
            "parameters": audio_schema.clone()
        }),
        serde_json::json!({
            "type": "function",
            "name": ANALYZE_AUDIO_TOOL_NAME,
            "description": "Objectively analyze the rendered full mix and every selected track over an absolute project range. Returns standard signal-level and spectral measurements without audio and without musical judgments.",
            "parameters": audio_schema
        }),
        function(
            AUDITION_TOOL_NAME,
            "Render a disposable Surge XT instrument audition without changing the sound graph or edit history. Supply an installed preset, a short beat-relative MIDI sequence, and optionally one effect. The current project tempo is used.",
            object_schema(
                serde_json::json!({
                    "presetId":{"type":"string","minLength":1,"maxLength":200,"description":"Exact installed preset ID returned by list_surge_presets."},
                    "durationBeats":{"type":"number","minimum":0.25,"maximum":16,"description":"Audition length in beats; the resulting audio may not exceed 16 seconds."},
                    "events":{
                        "type":"array","minItems":1,"maxItems":MAX_MIDI_EVENTS_PER_CLIP,
                        "items":{"type":"object","properties":{
                            "time":{"type":"number","minimum":0,"maximum":16},
                            "duration":{"type":"number","minimum":MIN_MIDI_NOTE_BEATS,"maximum":MAX_MIDI_NOTE_DURATION_BEATS},
                            "pitch":{"type":"integer","minimum":0,"maximum":127},
                            "velocity":{"type":"number","minimum":0.01,"maximum":1}
                        },"required":["time","duration","pitch","velocity"],"additionalProperties":false}
                    },
                    "effect":{
                        "type":"object",
                        "properties":{
                            "name":{"type":"string","enum":EFFECT_NAMES},
                            "mix":{"type":"number","minimum":0,"maximum":1}
                        },
                        "required":["name","mix"],
                        "additionalProperties":false
                    }
                }),
                &["presetId", "durationBeats", "events"],
            ),
        ),
        function(
            PRESET_TOOL_NAME,
            "Browse one level of the installed Surge XT factory preset hierarchy. Start at Factory and continue with exact returned folder paths until preset IDs are returned for set_surge_preset.",
            object_schema(
                serde_json::json!({
                    "path":{"type":"string","minLength":7,"maxLength":160,"description":"Exact folder path returned by a prior call. Omit to browse the Factory root."}
                }),
                &[],
            ),
        ),
        function(
            INSTRUMENT_PARAMETER_TOOL_NAME,
            "Browse Surge XT by module. Start with only trackId, then copy one module ID into the next call. At a leaf copy parameter into set_instrument_parameter, or modulationTarget into add_modulator.target.",
            object_schema(
                serde_json::json!({
                    "trackId":{"type":"integer","minimum":1},
                    "module":{"type":"string","maxLength":48,"description":"Exact module ID returned by the previous call. Omit to list top-level modules."}
                }),
                &["trackId"],
            ),
        ),
        function(
            SOUND_TOOL_PARAMETER_TOOL_NAME,
            "List every editable control for one effect or modulator. Effect controls and metadata come directly from Surge XT. Returned parameter IDs are for update_effect/update_modulator, not modulation targets.",
            object_schema(
                serde_json::json!({
                    "trackId":{"type":"integer","minimum":1},
                    "tool":{"type":"string","enum":["effect","modulator"]},
                    "toolId":{"type":"integer","minimum":1}
                }),
                &["trackId", "tool", "toolId"],
            ),
        ),
    ];
    tools.extend(mutation_tool_declarations(batch_parameter_tools));
    tools
}

fn object_schema(properties: JsonValue, required: &[&str]) -> JsonValue {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn function(name: &str, description: &str, parameters: JsonValue) -> JsonValue {
    serde_json::json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": parameters
    })
}

fn mutation_tool_declarations(batch_parameter_tools: bool) -> Vec<JsonValue> {
    let id = || serde_json::json!({"type":"integer","minimum":1});
    let notes = || {
        serde_json::json!({
            "type":"array","maxItems":MAX_MIDI_EVENTS_PER_CLIP,"items":{"type":"object","properties":{
                "time":{"type":"number","minimum":0,"maximum":256,"description":"Beat offset from the clip start. Differences between event times determine retrigger speed and may be smaller than note duration."},
                "duration":{"type":"number","minimum":MIN_MIDI_NOTE_BEATS,"maximum":MAX_MIDI_NOTE_DURATION_BEATS,"description":"MIDI gate length in beats, independent of spacing between event times. Gates may span up to 16 beats; 0.0625 beats is a 1/64 note."},
                "pitch":{"type":"integer","minimum":0,"maximum":127},
                "velocity":{"type":"number","minimum":0.01,"maximum":1}
            },"required":["time","duration","pitch","velocity"],"additionalProperties":false}
        })
    };
    let clip_properties = || {
        serde_json::json!({
            "trackId":id(), "label":{"type":"string","minLength":1,"maxLength":64},
            "startBeat":{"type":"number","minimum":0,"description":"Absolute beat from the start of the project."},
            "durationBeats":{"type":"number","minimum":0.25,"maximum":MAX_ONCE_PLAYBACK_BEATS},
            "playback":{"oneOf":[
                {"type":"object","properties":{"mode":{"type":"string","enum":["loop"]},"lengthBeats":{"type":"number","minimum":0.25,"maximum":MAX_LOOP_PLAYBACK_BEATS}},"required":["mode","lengthBeats"],"additionalProperties":false},
                {"type":"object","properties":{"mode":{"type":"string","enum":["once"]}},"required":["mode"],"additionalProperties":false}
            ]},
            "events":notes()
        })
    };
    let mut tools = vec![
        function(
            "new_track",
            "Create one neutral Surge XT track with Init and no clips, effects, or modulators. Choose a short descriptive role label and a color from the palette. Returns its stable ID.",
            object_schema(
                serde_json::json!({
                    "description":{"type":"string","minLength":1,"maxLength":16,"description":"Short role or purpose, such as Snare Build or Bass Drop."},
                    "color":{"type":"string","enum":TRACK_COLOR_PALETTE,"description":"Display color chosen from the DAW-AI track palette."}
                }),
                &["description", "color"],
            ),
        ),
        function(
            "delete_track",
            "Delete one track by stable ID. Use undo if this was a mistake.",
            object_schema(serde_json::json!({"trackId":id()}), &["trackId"]),
        ),
        function(
            "set_track_identity",
            "Set an existing track's short display name and palette color. Use this to give the initial Empty Track a musical identity once its purpose is known.",
            object_schema(
                serde_json::json!({
                    "trackId":id(),
                    "name":{"type":"string","minLength":1,"maxLength":16,"description":"Short musical role or purpose, such as Lead or Bass Drop."},
                    "color":{"type":"string","enum":TRACK_COLOR_PALETTE,"description":"Display color chosen from the DAW-AI track palette."}
                }),
                &["trackId", "name", "color"],
            ),
        ),
        function(
            "set_surge_preset",
            "Load one installed Surge XT factory preset onto a track using a stable preset ID returned by list_surge_presets.",
            object_schema(
                serde_json::json!({
                    "trackId":id(),
                    "presetId":{"type":"string","minLength":1,"maxLength":200}
                }),
                &["trackId", "presetId"],
            ),
        ),
        function(
            "add_midi_clip",
            "Add a beat-positioned MIDI clip without changing other clips.",
            object_schema(
                clip_properties(),
                &[
                    "trackId",
                    "label",
                    "startBeat",
                    "durationBeats",
                    "playback",
                    "events",
                ],
            ),
        ),
        function(
            "update_midi_clip",
            "Replace all fields and events of one existing MIDI clip. This changes the whole clip; to preserve material outside an edit region, keep it and add a separate regional clip, or explicitly split it into clips that preserve the surrounding material.",
            object_schema(
                {
                    let mut p = clip_properties();
                    p.as_object_mut().unwrap().insert("clipId".to_owned(), id());
                    p
                },
                &[
                    "trackId",
                    "clipId",
                    "label",
                    "startBeat",
                    "durationBeats",
                    "playback",
                    "events",
                ],
            ),
        ),
        function(
            "delete_midi_clip",
            "Delete one MIDI clip by stable track and clip IDs.",
            object_schema(
                serde_json::json!({"trackId":id(),"clipId":id()}),
                &["trackId", "clipId"],
            ),
        ),
        function(
            "add_effect",
            "Append a named effect after the preset's visible embedded effects and set its mix. Surge XT has eight serial slots shared by preset effects and added effects. Returns the stable effect ID; use list_sound_tool_parameters to list all of its controls.",
            object_schema(
                serde_json::json!({"trackId":id(),"name":{"type":"string","enum":EFFECT_NAMES},"mix":{"type":"number","minimum":0,"maximum":1}}),
                &["trackId", "name", "mix"],
            ),
        ),
        function(
            "update_effect",
            "Update one effect control. Copy parameter unchanged from list_sound_tool_parameters.",
            parameter_schema("effectId"),
        ),
        function(
            "delete_effect",
            "Delete one effect by stable track and effect IDs.",
            object_schema(
                serde_json::json!({"trackId":id(),"effectId":id()}),
                &["trackId", "effectId"],
            ),
        ),
        function(
            "add_modulator",
            "Add a native Surge XT modulator and return its stable ID. Copy target from an instrument leaf's modulationTarget field; never copy its parameter field.",
            object_schema(
                serde_json::json!({"trackId":id(),"target":{"type":"string","pattern":"^native:[0-9]+$","maxLength":96},"shape":{"type":"string","enum":["sine","triangle","square","random","envelope","formula"]},"formula":{"type":"string","minLength":1,"maxLength":8192},"rate":{"type":"number","minimum":0.01,"maximum":20},"rateMode":{"type":"string","enum":["hz","tempo"]},"depth":{"type":"number","minimum":0,"maximum":1},"trigger":{"type":"string","enum":["free","midi"]},"attackMs":{"type":"number","minimum":0,"maximum":1000},"releaseMs":{"type":"number","minimum":1,"maximum":5000},"polarity":{"type":"string","enum":["increase","decrease"]}}),
                &[
                    "trackId",
                    "target",
                    "shape",
                    "rate",
                    "rateMode",
                    "depth",
                    "trigger",
                    "attackMs",
                    "releaseMs",
                    "polarity",
                ],
            ),
        ),
        function(
            "update_modulator",
            "Update one modulator control. Copy parameter unchanged from list_sound_tool_parameters; parameter=formula accepts full Surge Formula source.",
            parameter_schema_with_value_limit("modulatorId", 8_192),
        ),
        function(
            "delete_modulator",
            "Delete one modulator by stable track and modulator IDs.",
            object_schema(
                serde_json::json!({"trackId":id(),"modulatorId":id()}),
                &["trackId", "modulatorId"],
            ),
        ),
        function(
            SET_INSTRUMENT_PARAMETER_TOOL_NAME,
            "Set one Surge XT instrument control. Copy parameter unchanged from list_instrument_parameters. Values are strings because Surge controls may be numeric, Boolean, or enumerated.",
            object_schema(
                serde_json::json!({"trackId":id(),"parameter":{"type":"string","pattern":"^native:[0-9]+$","maxLength":64,"description":"Exact native parameter ID returned by list_instrument_parameters."},"value":{"type":"string","minLength":1,"maxLength":96}}),
                &["trackId", "parameter", "value"],
            ),
        ),
        function(
            "set_track_volume",
            "Set one track's static mix volume.",
            object_schema(
                serde_json::json!({"trackId":id(),"volume":{"type":"number","minimum":0,"maximum":1.5}}),
                &["trackId", "volume"],
            ),
        ),
        function(
            "set_track_mute",
            "Set the sole authoritative mute state of one track.",
            object_schema(
                serde_json::json!({"trackId":id(),"muted":{"type":"boolean"}}),
                &["trackId", "muted"],
            ),
        ),
        function(
            "set_tempo",
            "Set project tempo in beats per minute.",
            object_schema(
                serde_json::json!({"bpm":{"type":"integer","minimum":60,"maximum":180}}),
                &["bpm"],
            ),
        ),
        function(
            "undo",
            "Undo the most recent successful graph mutation made in this edit session.",
            object_schema(serde_json::json!({}), &[]),
        ),
    ];
    if batch_parameter_tools {
        let changes = serde_json::json!({
            "type":"array",
            "minItems":1,
            "maxItems":32,
            "items":{
                "type":"object",
                "properties":{
                    "parameter":{"type":"string","minLength":1,"maxLength":64},
                    "value":{"type":"string","minLength":1,"maxLength":96}
                },
                "required":["parameter","value"],
                "additionalProperties":false
            }
        });
        tools.push(function(
            SET_INSTRUMENT_PARAMETERS_TOOL_NAME,
            "Atomically set several Surge XT instrument controls on one track. Copy each parameter unchanged from list_instrument_parameters. The whole call fails without changes if any item is invalid.",
            object_schema(
                serde_json::json!({
                    "trackId":id(),
                    "changes":changes.clone()
                }),
                &["trackId", "changes"],
            ),
        ));
        tools.push(function(
            UPDATE_EFFECT_PARAMETERS_TOOL_NAME,
            "Atomically update several controls on one effect. Copy each parameter unchanged from list_sound_tool_parameters. The whole call fails without changes if any item is invalid.",
            object_schema(
                serde_json::json!({
                    "trackId":id(),
                    "effectId":id(),
                    "changes":changes
                }),
                &["trackId", "effectId", "changes"],
            ),
        ));
    }
    tools
}

fn parameter_schema(id_name: &str) -> JsonValue {
    parameter_schema_with_value_limit(id_name, 96)
}

fn parameter_schema_with_value_limit(id_name: &str, value_max_length: usize) -> JsonValue {
    object_schema(
        serde_json::json!({
            "trackId":{"type":"integer","minimum":1},
            (id_name):{"type":"integer","minimum":1},
            "parameter":{"type":"string","minLength":1,"maxLength":64},
            "value":{"type":"string","minLength":1,"maxLength":value_max_length}
        }),
        &["trackId", id_name, "parameter", "value"],
    )
}

#[derive(Debug)]
pub(crate) struct AudioRender {
    pub(crate) description: String,
    pub(crate) measurements: JsonValue,
    pub(crate) wav: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct AudioRenderRequest {
    pub(crate) project: Project,
    pub(crate) track_ids: Vec<u64>,
    pub(crate) start: f32,
    pub(crate) end: f32,
    pub(crate) description: String,
}

pub(crate) fn read_sound_graph(
    session_path: &Path,
    arguments: &JsonValue,
) -> Result<String, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "sound graph arguments must be an object".to_owned())?;
    let project = current_project(session_path)?;
    if let Some(node_id) = object.get("nodeId") {
        let node_id = node_id
            .as_str()
            .ok_or_else(|| "nodeId must be a string".to_owned())?;
        return read_sound_node(session_path, &project, node_id).map(|value| value.to_string());
    }
    Ok(sound_graph_topology(&project).to_string())
}

fn sound_graph_topology(project: &Project) -> JsonValue {
    let mut nodes = vec![serde_json::json!({
        "nodeId":"master",
        "type":"master",
        "name":project.name
    })];
    let mut connections = Vec::new();
    for track in &project.tracks {
        let track_node = format!("track:{}", track.id);
        let instrument_node = format!("instrument:{}", track.instrument.id);
        nodes.push(serde_json::json!({
            "nodeId":track_node,
            "type":"track",
            "name":track.name,
            "role":track.role.as_str(),
            "volume":track.volume,
            "muted":track.muted
        }));
        nodes.push(serde_json::json!({
            "nodeId":instrument_node,
            "type":"instrument",
            "trackId":track.id,
            "engine":track.instrument.engine,
            "preset":track.instrument.preset
        }));
        connections.push(serde_json::json!({
            "from":track_node,"to":instrument_node,"type":"owns"
        }));
        for clip in &track.clips {
            let node_id = format!("clip:{}", clip.id);
            nodes.push(serde_json::json!({
                "nodeId":node_id,
                "type":"midiClip",
                "trackId":track.id,
                "label":clip.label,
                "start":clip.start,
                "end":clip.end,
                "eventCount":clip.events.len()
            }));
            connections.push(serde_json::json!({
                "from":track_node,"to":node_id,"type":"owns"
            }));
            connections.push(serde_json::json!({
                "from":node_id,"to":instrument_node,"type":"midi"
            }));
        }
        let mut audio_source = instrument_node.clone();
        for effect_id in &track.routing.effect_order {
            let Some(effect) = track.effects.iter().find(|effect| effect.id == *effect_id) else {
                continue;
            };
            let node_id = format!("effect:{}", effect.id);
            nodes.push(serde_json::json!({
                "nodeId":node_id,
                "type":"effect",
                "trackId":track.id,
                "name":effect.name,
                "source":if effect.preset_slot.is_some() {"preset"} else {"added"},
                "enabled":effect.enabled
            }));
            connections.push(serde_json::json!({
                "from":track_node,"to":node_id,"type":"owns"
            }));
            if effect.enabled {
                connections.push(serde_json::json!({
                    "from":audio_source,"to":node_id,"type":"audio"
                }));
                audio_source = node_id;
            }
        }
        connections.push(serde_json::json!({
            "from":audio_source,"to":"master","type":"audio","trackId":track.id
        }));
        for modulator in &track.modulators {
            let node_id = format!("modulator:{}", modulator.id);
            nodes.push(serde_json::json!({
                "nodeId":node_id,
                "type":"modulator",
                "trackId":track.id,
                "shape":modulator.shape,
                "target":modulator.target,
                "enabled":modulator.enabled
            }));
            connections.push(serde_json::json!({
                "from":track_node,"to":node_id,"type":"owns"
            }));
            if modulator.enabled {
                connections.push(serde_json::json!({
                    "from":node_id,
                    "to":instrument_node,
                    "type":"modulation",
                    "target":modulator.target
                }));
            }
        }
    }
    serde_json::json!({
        "schemaVersion":PROJECT_SCHEMA_VERSION,
        "name":project.name,
        "bpm":project.bpm,
        "duration":project.duration,
        "version":project.version,
        "nodeIdType":"nodeId",
        "nodes":nodes,
        "connections":connections
    })
}

fn read_sound_node(
    session_path: &Path,
    project: &Project,
    node_id: &str,
) -> Result<JsonValue, String> {
    if node_id == "master" {
        return Ok(serde_json::json!({
            "nodeId":"master","type":"master","name":project.name,
            "bpm":project.bpm,"duration":project.duration,"version":project.version,
            "controls":[{"parameter":"bpm","value":project.bpm,"minimum":60,"maximum":180,"mutationTool":"set_tempo"}]
        }));
    }
    let (kind, id) = node_id
        .split_once(':')
        .ok_or_else(|| "nodeId must be copied from read_sound_graph topology".to_owned())?;
    let id = id
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| "nodeId must contain a positive stable ID".to_owned())?;
    for track in &project.tracks {
        match kind {
            "track" if track.id == id => {
                return Ok(serde_json::json!({
                    "nodeId":node_id,"type":"track","id":track.id,"name":track.name,
                    "role":track.role.as_str(),"color":track.color,"volume":track.volume,
                    "muted":track.muted,
                    "controls":[
                        {"parameters":["name","color"],"mutationTool":"set_track_identity","colorPalette":TRACK_COLOR_PALETTE},
                        {"parameter":"volume","value":track.volume,"minimum":0,"maximum":1.5,"mutationTool":"set_track_volume"},
                        {"parameter":"muted","value":track.muted,"mutationTool":"set_track_mute"}
                    ],
                    "children":std::iter::once(format!("instrument:{}", track.instrument.id))
                        .chain(track.clips.iter().map(|clip| format!("clip:{}", clip.id)))
                        .chain(track.effects.iter().map(|effect| format!("effect:{}", effect.id)))
                        .chain(track.modulators.iter().map(|modulator| format!("modulator:{}", modulator.id)))
                        .collect::<Vec<_>>()
                }));
            }
            "instrument" if track.instrument.id == id => {
                return Ok(serde_json::json!({
                    "nodeId":node_id,"type":"instrument","id":id,"trackId":track.id,
                    "engine":track.instrument.engine,"preset":track.instrument.preset,
                    "nativeOverrides":track.instrument.native_overrides,
                    "parameterBrowser":{"tool":INSTRUMENT_PARAMETER_TOOL_NAME,"arguments":{"trackId":track.id}},
                    "presetBrowser":{"tool":PRESET_TOOL_NAME,"arguments":{}}
                }));
            }
            "clip" => {
                if let Some(clip) = track.clips.iter().find(|clip| clip.id == id) {
                    let beats_per_second = f32::from(project.bpm) / 60.0;
                    return Ok(serde_json::json!({
                        "nodeId":node_id,"type":"midiClip","id":id,"trackId":track.id,
                        "label":clip.label,"startBeat":clip.start * beats_per_second,
                        "durationBeats":(clip.end - clip.start) * beats_per_second,
                        "playback":if clip.playback_mode == "loop" {
                            serde_json::json!({"mode":"loop","lengthBeats":clip.loop_beats})
                        } else { serde_json::json!({"mode":"once"}) },
                        "events":clip.events.iter().map(|event| serde_json::json!({
                            "id":event.id,"time":event.time,"duration":event.duration,
                            "pitch":event.pitch,"velocity":event.velocity
                        })).collect::<Vec<_>>()
                    }));
                }
            }
            "effect" => {
                if let Some(effect) = track.effects.iter().find(|effect| effect.id == id) {
                    let controls = serde_json::from_str::<JsonValue>(&list_sound_tool_parameters(
                        session_path,
                        &serde_json::json!({"trackId":track.id,"tool":"effect","toolId":id}),
                    )?)
                    .map_err(|error| format!("could not serialize effect controls: {error}"))?;
                    return Ok(serde_json::json!({
                        "nodeId":node_id,"type":"effect","id":id,"trackId":track.id,
                        "name":effect.name,
                        "source":if effect.preset_slot.is_some() {"preset"} else {"added"},
                        "controls":controls["parameters"]
                    }));
                }
            }
            "modulator" => {
                if let Some(modulator) = track.modulators.iter().find(|item| item.id == id) {
                    let controls = serde_json::from_str::<JsonValue>(&list_sound_tool_parameters(
                        session_path,
                        &serde_json::json!({"trackId":track.id,"tool":"modulator","toolId":id}),
                    )?)
                    .map_err(|error| format!("could not serialize modulator controls: {error}"))?;
                    return Ok(serde_json::json!({
                        "nodeId":node_id,"type":"modulator","id":id,"trackId":track.id,
                        "name":modulator.name,"controls":controls["parameters"]
                    }));
                }
            }
            _ => {}
        }
    }
    Err(format!(
        "node {node_id} does not exist; call read_sound_graph without nodeId for current topology"
    ))
}

pub(crate) fn list_surge_presets(arguments: &JsonValue) -> Result<String, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "preset catalog arguments must be an object".to_owned())?;
    let path = object
        .get("path")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "path must be a string".to_owned())
        })
        .transpose()?
        .unwrap_or_else(|| "Factory".to_owned());
    let catalog = crate::surge_presets::render_safe_catalog();
    let level = crate::surge_presets::browse(&catalog, &path)
        .ok_or_else(|| format!("preset folder does not exist: {path}; browse from Factory"))?;
    let folders = level
        .folders
        .iter()
        .map(|folder| {
            serde_json::json!({
                "name":folder.name,
                "path":folder.path,
                "presetCount":folder.preset_count
            })
        })
        .collect::<Vec<_>>();
    let presets = level
        .presets
        .iter()
        .map(|preset| {
            serde_json::json!({
                "id":preset.id,
                "name":preset.name
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "installed":!catalog.is_empty(),
        "total":catalog.len(),
        "path":level.path,
        "parent":level.parent,
        "folders":folders,
        "presets":presets
    })
    .to_string())
}

pub(crate) fn list_instrument_parameters(
    session_path: &Path,
    arguments: &JsonValue,
) -> Result<String, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "tool arguments must be an object".to_owned())?;
    let track_id = required_id(object, "trackId")?;
    let module = object.get("module").and_then(JsonValue::as_str);
    let project = current_project(session_path)?;
    let track = project
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .ok_or_else(|| format!("track {track_id} does not exist"))?;
    let parameters = crate::surge::instrument_parameters_for_instrument(&track.instrument);
    if module.is_none() {
        let mut global = module_entry(
            "global",
            "Global",
            module_parameters(&parameters, "global").len(),
        );
        global["state"] = serde_json::json!(module_state(&parameters, "global"));
        return Ok(serde_json::json!({
            "trackId": track_id,
            "preset": track.instrument.preset,
            "midiContext": surge_midi_context(&parameters),
            "modules": [
                global,
                module_entry("scene:a", "Scene A", 0),
                module_entry("scene:b", "Scene B", 0),
                module_entry("effects", "Effects", track.effects.len())
            ]
        })
        .to_string());
    }
    let module = module.expect("checked module");
    if matches!(module, "scene:a" | "scene:b") {
        let scene = module.strip_prefix("scene:").expect("scene module");
        return Ok(serde_json::json!({
            "trackId": track_id,
            "preset": track.instrument.preset,
            "module": module,
            "modules": scene_modules(scene, &parameters)
        })
        .to_string());
    }
    if matches!(module, "scene:a/lfos" | "scene:b/lfos") {
        let scene = module
            .strip_prefix("scene:")
            .and_then(|value| value.strip_suffix("/lfos"))
            .expect("LFO module");
        return Ok(serde_json::json!({
            "trackId": track_id,
            "preset": track.instrument.preset,
            "module": module,
            "modules": lfo_modules(scene, &parameters)
        })
        .to_string());
    }
    if module == "effects" {
        let modules = track
            .effects
            .iter()
            .map(|effect| {
                serde_json::json!({
                    "id": format!("effect:{}", effect.id),
                    "name": effect.name,
                    "source": if effect.preset_slot.is_some() { "preset" } else { "added" },
                    "nextTool": SOUND_TOOL_PARAMETER_TOOL_NAME,
                    "nextArguments": {
                        "trackId": track_id,
                        "tool": "effect",
                        "toolId": effect.id
                    }
                })
            })
            .collect::<Vec<_>>();
        return Ok(serde_json::json!({
            "trackId": track_id,
            "preset": track.instrument.preset,
            "module": module,
            "modules": modules
        })
        .to_string());
    }
    let selected = module_parameters(&parameters, module);
    if selected.is_empty() {
        return Err("unknown instrument module".to_owned());
    }
    let parameters = selected
        .into_iter()
        .map(|parameter| {
            let requested_override = track
                .instrument
                .native_overrides
                .get(&parameter.id)
                .copied();
            let overridden =
                requested_override.is_some_and(|value| (value - parameter.value).abs() < 0.000_01);
            let mut value = serde_json::json!({
                "parameter": format!("native:{}", parameter.id),
                "name": parameter.name,
                "value": parameter.value,
                "presetValue": parameter.preset_value,
                "display": parameter.display,
                "overridden": overridden,
                "kind": if parameter.boolean {
                    "boolean"
                } else if !parameter.choices.is_empty() || parameter.discrete {
                    "selection"
                } else {
                    "continuous"
                },
                "mutationTool":SET_INSTRUMENT_PARAMETER_TOOL_NAME
            });
            if parameter.voice_modulatable || parameter.scene_modulatable {
                value["modulationTarget"] = serde_json::json!(format!("native:{}", parameter.id));
                value["modulation"] = serde_json::json!({
                    "midi": parameter.voice_modulatable,
                    "free": parameter.scene_modulatable
                });
            }
            if !parameter.choices.is_empty() {
                value["choices"] = serde_json::json!(
                    parameter
                        .choices
                        .iter()
                        .map(|(value, display)| serde_json::json!({
                            "value": value,
                            "display": display
                        }))
                        .collect::<Vec<_>>()
                );
            }
            for (field, enabled) in [
                ("bipolar", parameter.bipolar),
                ("tempoSync", parameter.tempo_sync),
                ("supportsDeactivation", parameter.can_deactivate),
                ("deactivated", parameter.deactivated),
            ] {
                if enabled {
                    value[field] = JsonValue::Bool(true);
                }
            }
            value
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "trackId": track_id,
        "preset": track.instrument.preset,
        "module": module,
        "idType": "editableParameter",
        "parameters": parameters
    })
    .to_string())
}

fn surge_midi_context(parameters: &[crate::surge::InstrumentParameter]) -> JsonValue {
    let display = |name: &str| {
        parameters
            .iter()
            .find(|parameter| parameter.name == name)
            .map(|parameter| parameter.display.clone())
    };
    serde_json::json!({
        "sceneMode": display("Scene Mode"),
        "splitPoint": display("Split Point"),
        "sceneA": {
            "octave": display("Scene A Octave"),
            "pitch": display("Scene A Pitch")
        },
        "sceneB": {
            "octave": display("Scene B Octave"),
            "pitch": display("Scene B Pitch")
        }
    })
}

fn module_entry(id: &str, name: &str, count: usize) -> JsonValue {
    let mut value = serde_json::json!({"id":id,"name":name});
    if count > 0 {
        value["parameterCount"] = serde_json::json!(count);
    }
    value
}

fn scene_modules(scene: &str, parameters: &[crate::surge::InstrumentParameter]) -> Vec<JsonValue> {
    [
        ("voice", "Voice"),
        ("osc:1", "Oscillator 1"),
        ("osc:2", "Oscillator 2"),
        ("osc:3", "Oscillator 3"),
        ("ring:1x2", "Ring Modulation 1x2"),
        ("ring:2x3", "Ring Modulation 2x3"),
        ("noise", "Noise"),
        ("output", "Mixer and Output"),
        ("filter-routing", "Filter Routing and Waveshaper"),
        ("filter:1", "Filter 1"),
        ("filter:2", "Filter 2"),
        ("envelope:amp", "Amp Envelope"),
        ("envelope:filter", "Filter Envelope"),
        ("lfos", "LFOs"),
    ]
    .into_iter()
    .map(|(suffix, name)| {
        let id = format!("scene:{scene}/{suffix}");
        let count = if suffix == "lfos" {
            0
        } else {
            module_parameters(parameters, &id).len()
        };
        let mut entry = module_entry(&id, name, count);
        let state = module_state(parameters, &id);
        if !state.is_empty() {
            entry["state"] = serde_json::json!(state);
        }
        entry
    })
    .collect()
}

fn lfo_modules(scene: &str, parameters: &[crate::surge::InstrumentParameter]) -> Vec<JsonValue> {
    ["voice", "scene"]
        .into_iter()
        .flat_map(|kind| {
            (1..=6).map(move |number| {
                let id = format!("scene:{scene}/lfo:{kind}:{number}");
                let name = if kind == "voice" {
                    format!("Voice LFO {number}")
                } else {
                    format!("Scene LFO {number}")
                };
                let mut entry = module_entry(&id, &name, 13);
                entry["state"] = serde_json::json!(module_state(parameters, &id));
                entry
            })
        })
        .collect()
}

fn module_state(parameters: &[crate::surge::InstrumentParameter], module: &str) -> Vec<JsonValue> {
    let suffixes: &[&str] = match module.rsplit('/').next().unwrap_or(module) {
        "global" => &["Global Volume", "Active Scene", "Scene Mode"],
        "voice" => &["Octave", "Pitch", "Play Mode"],
        "osc:1" | "osc:2" | "osc:3" => &["Type", "Octave", "Pitch", "Volume"],
        "noise" => &["Color", "Volume", "Route"],
        "output" => &["Volume", "Pan", "Width"],
        "filter-routing" => &[
            "Filter Configuration",
            "Waveshaper Type",
            "Waveshaper Drive",
        ],
        "filter:1" | "filter:2" => &["Type", "Cutoff", "Resonance"],
        "envelope:amp" | "envelope:filter" => &["Attack", "Decay", "Sustain", "Release"],
        leaf if leaf.starts_with("lfo:") => &["Type", "Rate", "Trigger Mode"],
        _ => &[],
    };
    let selected = module_parameters(parameters, module);
    suffixes
        .iter()
        .filter_map(|suffix| {
            selected
                .iter()
                .find(|parameter| parameter.name.ends_with(suffix))
                .map(|parameter| {
                    serde_json::json!({
                        "name": suffix,
                        "display": parameter.display
                    })
                })
        })
        .collect()
}

fn module_parameters<'a>(
    parameters: &'a [crate::surge::InstrumentParameter],
    module: &str,
) -> Vec<&'a crate::surge::InstrumentParameter> {
    let matches = |parameter: &&crate::surge::InstrumentParameter| {
        let name = parameter.name.as_str();
        match module {
            "global" => !name.starts_with("Scene ") && !is_effect_slot_parameter(name),
            _ => scene_module_matches(name, module),
        }
    };
    parameters.iter().filter(matches).collect()
}

fn is_effect_slot_parameter(name: &str) -> bool {
    let Some(slot) = name.strip_prefix("FX ") else {
        return false;
    };
    matches!(
        slot.as_bytes(),
        [b'A' | b'B' | b'S' | b'G', b'1'..=b'4', b' ', ..]
    )
}

fn scene_module_matches(name: &str, module: &str) -> bool {
    let Some(path) = module.strip_prefix("scene:") else {
        return false;
    };
    let Some((scene, leaf)) = path.split_once('/') else {
        return false;
    };
    let scene_name = match scene {
        "a" => "Scene A ",
        "b" => "Scene B ",
        _ => return false,
    };
    let Some(local) = name.strip_prefix(scene_name) else {
        return false;
    };
    match leaf {
        "voice" => [
            "Octave",
            "Pitch",
            "Portamento",
            "Play Mode",
            "FM Routing",
            "FM Depth",
            "Osc Drift",
            "Keytrack Root Key",
            "Pitch Bend Up Range",
            "Pitch Bend Down Range",
            "VCA Gain",
            "Velocity > VCA Gain",
        ]
        .contains(&local),
        "osc:1" | "osc:2" | "osc:3" => {
            let number = leaf.strip_prefix("osc:").expect("oscillator leaf");
            local.starts_with(&format!("Osc {number} "))
        }
        "ring:1x2" => local.starts_with("Ring Modulation 1x2 "),
        "ring:2x3" => local.starts_with("Ring Modulation 2x3 "),
        "noise" => local.starts_with("Noise "),
        "output" => {
            matches!(local, "Volume" | "Pan" | "Width")
                || (local.starts_with("Send FX ") && local.ends_with(" Level"))
        }
        "filter-routing" => matches!(
            local,
            "Pre-Filter Gain"
                | "Feedback"
                | "Filter Configuration"
                | "Filter Balance"
                | "Highpass"
                | "Waveshaper Type"
                | "Waveshaper Drive"
        ),
        "filter:1" => local.starts_with("Filter 1 "),
        "filter:2" => local.starts_with("Filter 2 ") || local == "Link Resonance",
        "envelope:amp" => local.starts_with("Amp EG "),
        "envelope:filter" => local.starts_with("Filter EG "),
        _ => lfo_module_matches(local, leaf),
    }
}

fn lfo_module_matches(local: &str, leaf: &str) -> bool {
    let Some(specifier) = leaf.strip_prefix("lfo:") else {
        return false;
    };
    let Some((kind, number)) = specifier.split_once(':') else {
        return false;
    };
    let prefix = match kind {
        "voice" => format!("LFO {number} "),
        "scene" => format!("Scene LFO {number} "),
        _ => return false,
    };
    local.starts_with(&prefix)
}

pub(crate) fn list_sound_tool_parameters(
    session_path: &Path,
    arguments: &JsonValue,
) -> Result<String, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "tool arguments must be an object".to_owned())?;
    let track_id = required_id(object, "trackId")?;
    let tool_id = required_id(object, "toolId")?;
    let tool = required_string(object, "tool")?;
    let project = current_project(session_path)?;
    let track = project
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .ok_or_else(|| format!("track {track_id} does not exist"))?;
    let parameters = match tool {
        "effect" => {
            let effect = track
                .effects
                .iter()
                .find(|effect| effect.id == tool_id)
                .ok_or_else(|| format!("effect {tool_id} does not exist"))?;
            let semantics = crate::surge::effect_parameter_semantics(
                &track.instrument,
                &track.effects,
                &track.routing.effect_order,
                track.id,
                effect.id,
            );
            let mut values = vec![
                serde_json::json!({"parameter":"enabled","name":"Enabled","value":effect.enabled,"type":"boolean"}),
                serde_json::json!({"parameter":"mix","name":"Mix","value":effect.mix,"minimum":0,"maximum":1}),
            ];
            let mut discovered = crate::surge::effect_parameter_values(&effect.name);
            for (parameter, value) in &mut discovered {
                if let Some(current) = semantics.get(parameter) {
                    *value = current.value;
                }
            }
            values.extend(discovered.iter().map(|(parameter, value)| {
                let mut discovered = serde_json::json!({
                    "parameter":parameter,
                    "name":display_parameter_name(parameter),
                    "value":value,
                    "minimum":0,
                    "maximum":1
                });
                add_effect_semantics(&mut discovered, semantics.get(parameter.as_str()));
                discovered
            }));
            for value in &mut values {
                let parameter = value["parameter"].as_str().unwrap_or_default();
                add_effect_semantics(value, semantics.get(parameter));
            }
            values
        }
        "modulator" => {
            let modulator = track
                .modulators
                .iter()
                .find(|modulator| modulator.id == tool_id)
                .ok_or_else(|| format!("modulator {tool_id} does not exist"))?;
            let mut values = vec![
                serde_json::json!({"parameter":"enabled","name":"Enabled","value":modulator.enabled,"type":"boolean"}),
                serde_json::json!({"parameter":"shape","name":"Shape","value":modulator.shape,"choices":["sine","triangle","square","random","envelope","formula"]}),
                serde_json::json!({"parameter":"target","name":"Target","value":modulator.target}),
                serde_json::json!({"parameter":"rate","name":"Rate","value":modulator.rate,"minimum":0.01,"maximum":20}),
                serde_json::json!({"parameter":"rateMode","name":"Rate mode","value":modulator.rate_mode,"choices":["hz","tempo"]}),
                serde_json::json!({"parameter":"depth","name":"Depth","value":modulator.depth,"minimum":0,"maximum":1}),
                serde_json::json!({"parameter":"trigger","name":"Trigger","value":modulator.trigger,"choices":["free","midi"]}),
                serde_json::json!({"parameter":"polarity","name":"Polarity","value":modulator.polarity,"choices":["increase","decrease"]}),
                serde_json::json!({"parameter":"attackMs","name":"Attack","value":modulator.attack_ms,"minimum":0,"maximum":1000,"unit":"ms"}),
                serde_json::json!({"parameter":"releaseMs","name":"Release","value":modulator.release_ms,"minimum":1,"maximum":5000,"unit":"ms"}),
            ];
            if modulator.shape == "formula" {
                values.push(serde_json::json!({"parameter":"formula","name":"Surge Formula (Lua)","value":modulator.formula,"maximumLength":8192}));
            }
            values
        }
        _ => return Err("tool must be effect or modulator".to_owned()),
    };
    let mutation_tool = if tool == "effect" {
        "update_effect"
    } else {
        "update_modulator"
    };
    Ok(serde_json::json!({
        "trackId":track_id,
        "tool":tool,
        "toolId":tool_id,
        "idType":"editableParameter",
        "source": if tool == "effect" {
            track.effects.iter().find(|effect| effect.id == tool_id)
                .map(|effect| if effect.preset_slot.is_some() {"preset"} else {"added"})
        } else { None },
        "parameters":parameters.into_iter().map(|mut parameter| {
            parameter.as_object_mut().expect("parameter object").insert(
                "mutationTool".to_owned(),
                JsonValue::String(mutation_tool.to_owned())
            );
            parameter
        }).collect::<Vec<_>>()
    })
    .to_string())
}

fn add_effect_semantics(
    value: &mut JsonValue,
    semantics: Option<&crate::surge::EffectParameterSemantics>,
) {
    let Some(semantics) = semantics else {
        return;
    };
    value["display"] = JsonValue::String(semantics.display.clone());
    value["kind"] = JsonValue::String(
        if semantics.boolean {
            "boolean"
        } else if !semantics.choices.is_empty() || semantics.discrete {
            "selection"
        } else {
            "continuous"
        }
        .to_owned(),
    );
    if !semantics.choices.is_empty() {
        value["choices"] = serde_json::json!(
            semantics
                .choices
                .iter()
                .map(|(value, display)| serde_json::json!({
                    "value": value,
                    "display": display
                }))
                .collect::<Vec<_>>()
        );
    }
    for (field, enabled) in [
        ("bipolar", semantics.bipolar),
        ("tempoSync", semantics.tempo_sync),
        ("supportsDeactivation", semantics.can_deactivate),
        ("deactivated", semantics.deactivated),
    ] {
        if enabled {
            value[field] = JsonValue::Bool(true);
        }
    }
}

fn display_parameter_name(name: &str) -> String {
    let mut display = String::new();
    for (index, character) in name.chars().enumerate() {
        if index > 0 && character.is_ascii_uppercase() {
            display.push(' ');
        }
        display.push(character);
    }
    if let Some(first) = display.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    display
}

pub(crate) fn is_mutation_tool(name: &str) -> bool {
    MUTATION_TOOL_NAMES.contains(&name)
}

pub(crate) fn apply_agent_mutation(
    session_path: &Path,
    name: &str,
    arguments: &JsonValue,
) -> Result<String, String> {
    wait_for_progress_handoff(session_path);
    let graph_path = session_path.join(GRAPH_FILE);
    let (store, mut studio) = ProjectStore::open(graph_path)
        .map_err(|error| format!("Could not load sound-graph.json: {error}"))?;
    let original = studio.project().clone();
    let (selection_start, selection_end) = edit_selection(session_path)?;
    let object = arguments
        .as_object()
        .ok_or_else(|| "tool arguments must be an object".to_owned())?;
    let mut result_id = None;
    let mut midi_context = None;
    let summary = match name {
        "new_track" => {
            let description = required_string(object, "description")?;
            let color = required_string(object, "color")?;
            if description.trim().is_empty() || description.chars().count() > 16 {
                return Err("description must contain between 1 and 16 characters".to_owned());
            }
            if !TRACK_COLOR_PALETTE.contains(&color) {
                return Err("color must be chosen from the new_track palette".to_owned());
            }
            let id = studio
                .add_described_channel(description, color)
                .map_err(studio_error_message)?;
            result_id = Some(id);
            format!("Created track {id}")
        }
        "delete_track" => {
            let id = required_id(object, "trackId")?;
            studio.delete_channel(id).map_err(studio_error_message)?;
            format!("Deleted track {id}")
        }
        "set_track_identity" => {
            let track_id = required_id(object, "trackId")?;
            let name = required_string(object, "name")?;
            let color = required_string(object, "color")?;
            if name.trim().is_empty() || name.chars().count() > 16 {
                return Err("name must contain between 1 and 16 characters".to_owned());
            }
            if !TRACK_COLOR_PALETTE.contains(&color) {
                return Err("color must be chosen from the set_track_identity palette".to_owned());
            }
            studio
                .set_track_identity(track_id, name, color)
                .map_err(studio_error_message)?;
            format!("Named track {track_id} {name} and set its color to {color}")
        }
        "set_surge_preset" => {
            let track_id = required_id(object, "trackId")?;
            let preset_id = required_string(object, "presetId")?;
            if crate::surge_presets::find(preset_id).is_none() {
                return Err(format!(
                    "Surge XT factory preset is not installed: {preset_id}; use {PRESET_TOOL_NAME} to discover available preset IDs"
                ));
            }
            if let Some(error) = crate::surge_presets::headless_render_error(preset_id) {
                return Err(error);
            }
            let instrument_id = studio
                .project()
                .tracks
                .iter()
                .find(|track| track.id == track_id)
                .map(|track| track.instrument.id)
                .ok_or_else(|| format!("track {track_id} does not exist"))?;
            studio
                .configure_sound_tool(
                    track_id,
                    "instrument",
                    instrument_id,
                    None,
                    "preset",
                    preset_id,
                )
                .map_err(studio_error_message)?;
            let instrument = &studio
                .project()
                .tracks
                .iter()
                .find(|track| track.id == track_id)
                .expect("configured track")
                .instrument;
            midi_context = Some(surge_midi_context(
                &crate::surge::instrument_parameters_for_instrument(instrument),
            ));
            format!("Loaded Surge XT preset {preset_id} on track {track_id}")
        }
        "add_midi_clip" => {
            let (track_id, spec) = clip_arguments(object, studio.project().bpm)?;
            validate_clip_selection(&spec, selection_start, selection_end)?;
            let id = studio
                .create_midi_clip(track_id, &spec)
                .map_err(studio_error_message)?;
            result_id = Some(id);
            format!("Added MIDI clip {id} to track {track_id}")
        }
        "update_midi_clip" => {
            let clip_id = required_id(object, "clipId")?;
            let (track_id, spec) = clip_arguments(object, studio.project().bpm)?;
            studio
                .replace_midi_clip(track_id, clip_id, &spec, selection_start, selection_end)
                .map_err(studio_error_message)?;
            format!("Updated MIDI clip {clip_id} on track {track_id}")
        }
        "delete_midi_clip" => {
            let track_id = required_id(object, "trackId")?;
            let clip_id = required_id(object, "clipId")?;
            studio
                .delete_midi_clip(track_id, clip_id, selection_start, selection_end)
                .map_err(studio_error_message)?;
            format!("Deleted MIDI clip {clip_id} from track {track_id}")
        }
        "add_effect" => {
            let track_id = required_id(object, "trackId")?;
            let effect_name = required_string(object, "name")?;
            let mix = required_number(object, "mix")?;
            let effect_id = studio
                .create_effect(track_id, effect_name, mix as f32)
                .map_err(studio_error_message)?;
            result_id = Some(effect_id);
            format!("Added {effect_name} effect {effect_id} to track {track_id}")
        }
        "update_effect" => update_parameter(&mut studio, object, "effect", "effectId")?,
        UPDATE_EFFECT_PARAMETERS_TOOL_NAME => {
            let track_id = required_id(object, "trackId")?;
            let effect_id = required_id(object, "effectId")?;
            let changes = parameter_changes(object)?;
            for (parameter, value) in &changes {
                update_parameter(
                    &mut studio,
                    &serde_json::json!({
                        "trackId":track_id,
                        "effectId":effect_id,
                        "parameter":parameter,
                        "value":value
                    })
                    .as_object()
                    .expect("batch effect change is an object")
                    .clone(),
                    "effect",
                    "effectId",
                )?;
            }
            format!(
                "Updated {} parameters on effect {effect_id} on track {track_id}",
                changes.len()
            )
        }
        "delete_effect" => {
            let track_id = required_id(object, "trackId")?;
            let effect_id = required_id(object, "effectId")?;
            studio
                .delete_effect(track_id, effect_id)
                .map_err(studio_error_message)?;
            format!("Deleted effect {effect_id} from track {track_id}")
        }
        "add_modulator" => {
            let track_id = required_id(object, "trackId")?;
            let target = required_string(object, "target")?;
            let shape = required_string(object, "shape")?;
            if !matches!(
                shape,
                "sine" | "triangle" | "square" | "random" | "envelope" | "formula"
            ) {
                return Err(
                    "shape must be sine, triangle, square, random, envelope, or formula".to_owned(),
                );
            }
            let rate = required_number(object, "rate")? as f32;
            let rate_mode = required_string(object, "rateMode")?;
            let depth = required_number(object, "depth")? as f32;
            let trigger = required_string(object, "trigger")?;
            if !matches!(trigger, "free" | "midi") {
                return Err("trigger must be free or midi".to_owned());
            }
            let attack_ms = required_number(object, "attackMs")? as f32;
            let release_ms = required_number(object, "releaseMs")? as f32;
            let polarity = required_string(object, "polarity")?;
            let formula = object
                .get("formula")
                .and_then(JsonValue::as_str)
                .unwrap_or("");
            let id = studio
                .create_modulator(
                    track_id,
                    ModulatorSpec {
                        target,
                        shape,
                        rate,
                        rate_mode,
                        depth,
                        trigger,
                        source_track_id: None,
                        attack_ms,
                        release_ms,
                        threshold: 0.0,
                        polarity,
                        formula,
                    },
                )
                .map_err(studio_error_message)?;
            result_id = Some(id);
            format!("Added modulator {id} to track {track_id}")
        }
        "update_modulator" => update_parameter(&mut studio, object, "modulator", "modulatorId")?,
        "delete_modulator" => {
            let track_id = required_id(object, "trackId")?;
            let modulator_id = required_id(object, "modulatorId")?;
            studio
                .delete_modulator(track_id, modulator_id)
                .map_err(studio_error_message)?;
            format!("Deleted modulator {modulator_id} from track {track_id}")
        }
        SET_INSTRUMENT_PARAMETER_TOOL_NAME => {
            let track_id = required_id(object, "trackId")?;
            let tool = "instrument";
            let tool_id = studio
                .project()
                .tracks
                .iter()
                .find(|track| track.id == track_id)
                .map(|track| track.instrument.id)
                .ok_or_else(|| format!("track {track_id} does not exist"))?;
            let parameter = required_string(object, "parameter")?;
            let value = required_string(object, "value")?;
            studio
                .configure_sound_tool(track_id, tool, tool_id, None, parameter, value)
                .map_err(|error| {
                    parameter_error_message(
                        error,
                        studio.project(),
                        track_id,
                        tool,
                        tool_id,
                        parameter,
                    )
                })?;
            format!("Set Surge XT instrument parameter {parameter} on track {track_id}")
        }
        SET_INSTRUMENT_PARAMETERS_TOOL_NAME => {
            let track_id = required_id(object, "trackId")?;
            let tool_id = studio
                .project()
                .tracks
                .iter()
                .find(|track| track.id == track_id)
                .map(|track| track.instrument.id)
                .ok_or_else(|| format!("track {track_id} does not exist"))?;
            let changes = parameter_changes(object)?;
            for (parameter, value) in &changes {
                studio
                    .configure_sound_tool(track_id, "instrument", tool_id, None, parameter, value)
                    .map_err(|error| {
                        parameter_error_message(
                            error,
                            studio.project(),
                            track_id,
                            "instrument",
                            tool_id,
                            parameter,
                        )
                    })?;
            }
            format!(
                "Set {} Surge XT instrument parameters on track {track_id}",
                changes.len()
            )
        }
        "set_parameter" => {
            let track_id = required_id(object, "trackId")?;
            let tool = required_string(object, "tool")?;
            let tool_id = required_id(object, "toolId")?;
            let clip_id = object
                .get("clipId")
                .and_then(JsonValue::as_u64)
                .filter(|id| *id > 0);
            let parameter = required_string(object, "parameter")?;
            let value = required_string(object, "value")?;
            studio
                .configure_sound_tool(track_id, tool, tool_id, clip_id, parameter, value)
                .map_err(|error| {
                    parameter_error_message(
                        error,
                        studio.project(),
                        track_id,
                        tool,
                        tool_id,
                        parameter,
                    )
                })?;
            format!("Set {tool} {tool_id} {parameter} on track {track_id}")
        }
        "set_track_volume" => {
            let track_id = required_id(object, "trackId")?;
            let volume = required_number(object, "volume")? as f32;
            studio
                .set_mix(track_id, Some(volume), None)
                .map_err(studio_error_message)?;
            format!("Set track {track_id} volume to {volume}")
        }
        "set_track_mute" => {
            let track_id = required_id(object, "trackId")?;
            let muted = object
                .get("muted")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| "muted must be a boolean".to_owned())?;
            studio
                .set_mix(track_id, None, Some(muted))
                .map_err(studio_error_message)?;
            format!("Set track {track_id} muted to {muted}")
        }
        "set_tempo" => {
            let bpm = required_id(object, "bpm")?
                .try_into()
                .map_err(|_| "bpm is out of range".to_owned())?;
            studio.set_tempo(bpm).map_err(studio_error_message)?;
            format!("Set tempo to {bpm} BPM")
        }
        "undo" => return undo_agent_mutation(session_path, &store, &original),
        _ => return Err(format!("unknown graph mutation tool: {name}")),
    };

    let undo_path = session_path.join(UNDO_GRAPH_FILE);
    let previous_undo = fs::read_to_string(&undo_path).ok();
    let transaction = (|| {
        write_replace(&undo_path, &original.to_json())
            .map_err(|error| format!("could not save undo snapshot: {error}"))?;
        store
            .save(studio.project())
            .map_err(|error| format!("Could not write sound-graph.json: {error}"))?;
        publish_progress(session_path, &plan_json(&summary), studio.project())
    })();
    if let Err(error) = transaction {
        let graph_rollback = store
            .save(&original)
            .map_err(|rollback| rollback.to_string());
        let undo_rollback = match previous_undo {
            Some(source) => write_replace(&undo_path, source.trim_end())
                .map_err(|rollback| rollback.to_string()),
            None => match fs::remove_file(&undo_path) {
                Ok(()) => Ok(()),
                Err(rollback) if rollback.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(rollback) => Err(rollback.to_string()),
            },
        };
        if let Err(rollback) = graph_rollback.and(undo_rollback) {
            return Err(format!(
                "{error}; could not restore failed mutation: {rollback}"
            ));
        }
        return Err(error);
    }
    let mut response = serde_json::json!({
        "message": summary,
        "version": studio.project().version,
        "id": result_id,
        "channels": sound_tool_inventory(studio.project()),
        "timing": {
            "bpm": studio.project().bpm,
            "secondsPerBeat": 60.0 / f64::from(studio.project().bpm)
        }
    });
    if matches!(name, "add_midi_clip" | "update_midi_clip") {
        let start_beats = required_number(object, "startBeat")?;
        let duration_beats = required_number(object, "durationBeats")?;
        let seconds_per_beat = 60.0 / f64::from(studio.project().bpm);
        response["clipTiming"] = serde_json::json!({
            "startBeats": start_beats,
            "endBeats": start_beats + duration_beats,
            "durationBeats": duration_beats,
            "startSeconds": start_beats * seconds_per_beat,
            "endSeconds": (start_beats + duration_beats) * seconds_per_beat,
            "durationSeconds": duration_beats * seconds_per_beat
        });
    }
    if let Some(context) = midi_context {
        response["midiContext"] = context;
    }
    if let Some(display) = mutation_display(studio.project(), name, object) {
        response["display"] = JsonValue::String(display);
    }
    Ok(response.to_string())
}

fn mutation_display(
    project: &Project,
    mutation: &str,
    arguments: &Map<String, JsonValue>,
) -> Option<String> {
    let track_id = arguments.get("trackId")?.as_u64()?;
    let track = project.tracks.iter().find(|track| track.id == track_id)?;
    let parameter = arguments.get("parameter")?.as_str()?;
    if mutation == SET_INSTRUMENT_PARAMETER_TOOL_NAME
        || (mutation == "set_parameter" && arguments.get("tool")?.as_str()? == "instrument")
    {
        let native_id = parameter.strip_prefix("native:")?.parse::<i32>().ok()?;
        return crate::surge::instrument_parameters_for_instrument(&track.instrument)
            .into_iter()
            .find(|candidate| candidate.id == native_id)
            .map(|candidate| candidate.display);
    }
    if mutation == "update_effect" {
        let effect_id = arguments.get("effectId")?.as_u64()?;
        return crate::surge::effect_parameter_semantics(
            &track.instrument,
            &track.effects,
            &track.routing.effect_order,
            track.id,
            effect_id,
        )
        .remove(parameter)
        .map(|semantics| semantics.display);
    }
    None
}

fn edit_selection(session_path: &Path) -> Result<(f32, f32), String> {
    let source = read_bounded_text(
        &session_path.join(REQUEST_FILE),
        MAX_SESSION_JSON_BYTES,
        "Gemini edit request",
    )
    .map_err(|error| format!("could not read edit request: {error}"))?;
    let request: JsonValue = serde_json::from_str(&source)
        .map_err(|error| format!("edit request was invalid: {error}"))?;
    let start = request
        .get("start")
        .and_then(JsonValue::as_f64)
        .ok_or_else(|| "edit request omitted selection start".to_owned())? as f32;
    let end = request
        .get("end")
        .and_then(JsonValue::as_f64)
        .ok_or_else(|| "edit request omitted selection end".to_owned())? as f32;
    if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
        return Err("edit request selection is invalid".to_owned());
    }
    Ok((start, end))
}

fn validate_clip_selection(
    spec: &MidiClipSpec,
    selection_start: f32,
    selection_end: f32,
) -> Result<(), String> {
    if spec.start < selection_start || spec.end > selection_end {
        return Err(format!(
            "MIDI clip must stay within the selected region ({selection_start}-{selection_end}s)"
        ));
    }
    Ok(())
}

fn required_id(object: &Map<String, JsonValue>, name: &str) -> Result<u64, String> {
    object
        .get(name)
        .and_then(JsonValue::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{name} must be a positive integer"))
}

fn required_number(object: &Map<String, JsonValue>, name: &str) -> Result<f64, String> {
    object
        .get(name)
        .and_then(JsonValue::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("{name} must be a finite number"))
}

fn required_string<'a>(object: &'a Map<String, JsonValue>, name: &str) -> Result<&'a str, String> {
    object
        .get(name)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} must be a nonempty string"))
}

fn parameter_changes(object: &Map<String, JsonValue>) -> Result<Vec<(&str, &str)>, String> {
    let changes = object
        .get("changes")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "changes must be an array".to_owned())?;
    if changes.is_empty() || changes.len() > 32 {
        return Err("changes must contain between 1 and 32 items".to_owned());
    }
    let mut seen = BTreeSet::new();
    changes
        .iter()
        .map(|change| {
            let change = change
                .as_object()
                .ok_or_else(|| "each change must be an object".to_owned())?;
            let parameter = required_string(change, "parameter")?;
            let value = required_string(change, "value")?;
            if !seen.insert(parameter) {
                return Err(format!("parameter {parameter} appears more than once"));
            }
            Ok((parameter, value))
        })
        .collect()
}

fn clip_arguments(
    object: &Map<String, JsonValue>,
    bpm: u16,
) -> Result<(u64, MidiClipSpec), String> {
    let label = required_string(object, "label")?;
    let label_length = label.chars().count();
    if !(1..=64).contains(&label_length) {
        return Err("label must contain between 1 and 64 characters".to_owned());
    }
    let events = object
        .get("events")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "events must be an array".to_owned())?;
    let notes = events
        .iter()
        .map(|event| {
            let event = event
                .as_object()
                .ok_or_else(|| "each event must be an object".to_owned())?;
            let pitch = required_id_or_zero(event, "pitch")?
                .try_into()
                .map_err(|_| "pitch is out of range".to_owned())?;
            Ok(MidiNote {
                time: required_number(event, "time")? as f32,
                duration: required_number(event, "duration")? as f32,
                pitch,
                velocity: required_number(event, "velocity")? as f32,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let start_beat = required_number(object, "startBeat")? as f32;
    let duration_beats = required_number(object, "durationBeats")? as f32;
    let playback = object
        .get("playback")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| "playback must be an object".to_owned())?;
    let playback_mode = required_string(playback, "mode")?;
    let maximum_events = MAX_MIDI_EVENTS_PER_CLIP;
    if events.len() > maximum_events {
        return Err(format!(
            "{playback_mode} has {} events; maximum is {maximum_events}",
            events.len()
        ));
    }
    let loop_beats = match playback_mode {
        "loop" => required_number(playback, "lengthBeats")? as f32,
        "once" => duration_beats,
        _ => return Err("playback mode must be loop or once".to_owned()),
    };
    if start_beat < 0.0 {
        return Err("startBeat must be at least 0".to_owned());
    }
    if !(0.25..=MAX_ONCE_PLAYBACK_BEATS).contains(&duration_beats) {
        return Err(format!(
            "durationBeats must be between 0.25 and {MAX_ONCE_PLAYBACK_BEATS}"
        ));
    }
    let maximum_playback_beats = if playback_mode == "loop" {
        MAX_LOOP_PLAYBACK_BEATS
    } else {
        MAX_ONCE_PLAYBACK_BEATS
    };
    if !(0.25..=maximum_playback_beats).contains(&loop_beats) {
        return Err(format!(
            "{playback_mode} playback length must be between 0.25 and {maximum_playback_beats} beats"
        ));
    }
    for (index, note) in notes.iter().enumerate() {
        if !(0.0..loop_beats).contains(&note.time) {
            return Err(format!(
                "events[{index}].time must be at least 0 and before the {loop_beats}-beat playback length"
            ));
        }
        let maximum_note_beats = loop_beats.min(crate::model::MAX_MIDI_NOTE_DURATION_BEATS);
        if !(MIN_MIDI_NOTE_BEATS..=maximum_note_beats).contains(&note.duration) {
            return Err(format!(
                "events[{index}].duration must be between {MIN_MIDI_NOTE_BEATS} and {maximum_note_beats} beats"
            ));
        }
        if !(0.01..=1.0).contains(&note.velocity) {
            return Err(format!(
                "events[{index}].velocity must be between 0.01 and 1"
            ));
        }
    }
    let seconds_per_beat = 60.0 / f32::from(bpm);
    Ok((
        required_id(object, "trackId")?,
        MidiClipSpec {
            label: label.to_owned(),
            start: start_beat * seconds_per_beat,
            end: (start_beat + duration_beats) * seconds_per_beat,
            playback_mode: playback_mode.to_owned(),
            loop_beats,
            notes,
        },
    ))
}

fn required_id_or_zero(object: &Map<String, JsonValue>, name: &str) -> Result<u64, String> {
    object
        .get(name)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| format!("{name} must be a nonnegative integer"))
}

fn update_parameter(
    studio: &mut Studio,
    object: &Map<String, JsonValue>,
    tool: &str,
    id_name: &str,
) -> Result<String, String> {
    let track_id = required_id(object, "trackId")?;
    let tool_id = required_id(object, id_name)?;
    let parameter = required_string(object, "parameter")?;
    let value = required_string(object, "value")?;
    let normalized_value = if tool == "effect" {
        normalize_effect_parameter_value(studio.project(), track_id, tool_id, parameter, value)?
    } else {
        value.to_owned()
    };
    studio
        .configure_sound_tool(track_id, tool, tool_id, None, parameter, &normalized_value)
        .map_err(|error| {
            parameter_error_message(error, studio.project(), track_id, tool, tool_id, parameter)
        })?;
    Ok(format!(
        "Updated {tool} {tool_id} {parameter} on track {track_id}"
    ))
}

fn normalize_effect_parameter_value(
    project: &Project,
    track_id: u64,
    effect_id: u64,
    parameter: &str,
    value: &str,
) -> Result<String, String> {
    let track = project
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .ok_or_else(|| format!("track {track_id} does not exist"))?;
    let semantics = crate::surge::effect_parameter_semantics(
        &track.instrument,
        &track.effects,
        &track.routing.effect_order,
        track_id,
        effect_id,
    );
    let Some(semantics) = semantics.get(parameter) else {
        let available = semantics.keys().cloned().collect::<Vec<_>>().join(", ");
        return Err(format!(
            "effect parameter {parameter} is invalid; valid parameters: {available}"
        ));
    };
    if semantics.choices.is_empty() {
        return Ok(value.to_owned());
    }
    if let Some((choice, _)) = semantics
        .choices
        .iter()
        .find(|(_, display)| display.eq_ignore_ascii_case(value.trim()))
    {
        return Ok(choice.to_string());
    }
    if let Ok(number) = value.parse::<f32>()
        && semantics
            .choices
            .iter()
            .any(|(choice, _)| (*choice - number).abs() < 0.000_001)
    {
        return Ok(number.to_string());
    }
    let choices = semantics
        .choices
        .iter()
        .map(|(choice, display)| format!("{display} ({choice})"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "effect parameter {parameter} must be one of: {choices}"
    ))
}

fn parameter_error_message(
    error: StudioError,
    project: &Project,
    track_id: u64,
    tool: &str,
    tool_id: u64,
    parameter: &str,
) -> String {
    if tool == "instrument"
        && (parameter.starts_with("instrument.") || parameter.starts_with("effect:"))
    {
        return "modulation target used as editable parameter; use a discovered parameter ID"
            .to_owned();
    }
    match error {
        StudioError::UnknownSoundTool => format!("{tool} {tool_id} not found"),
        StudioError::InvalidSoundTool => {
            if tool == "effect" {
                return normalize_effect_parameter_value(project, track_id, tool_id, parameter, "")
                    .expect_err("an empty effect selection must be rejected");
            }
            format!("invalid {tool} parameter or value: {parameter}")
        }
        other => studio_error_message(other),
    }
}

fn plan_json(summary: &str) -> String {
    serde_json::json!({"graphMutation":true,"summary":summary}).to_string()
}

fn undo_agent_mutation(
    session_path: &Path,
    store: &ProjectStore,
    current: &Project,
) -> Result<String, String> {
    let undo_path = session_path.join(UNDO_GRAPH_FILE);
    let source = read_bounded_text(&undo_path, MAX_SOUND_GRAPH_BYTES, "Gemini undo snapshot")
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                "nothing to undo in this edit session".to_owned()
            } else {
                format!("could not read undo snapshot: {error}")
            }
        })?;
    let mut restored = Project::from_json(&source)
        .map_err(|error| format!("undo snapshot is invalid: {error}"))?;
    restored.version = current.version.saturating_add(1);
    let summary = "Undid the previous graph mutation";
    let transaction = (|| {
        store
            .save(&restored)
            .map_err(|error| format!("could not restore undo snapshot: {error}"))?;
        fs::remove_file(&undo_path)
            .map_err(|error| format!("could not consume undo snapshot: {error}"))?;
        publish_progress(session_path, &plan_json(summary), &restored)
    })();
    if let Err(error) = transaction {
        let graph_rollback = store.save(current).map_err(|rollback| rollback.to_string());
        let undo_rollback = if undo_path.exists() {
            Ok(())
        } else {
            write_replace(&undo_path, source.trim_end()).map_err(|rollback| rollback.to_string())
        };
        if let Err(rollback) = graph_rollback.and(undo_rollback) {
            return Err(format!(
                "{error}; could not restore failed undo: {rollback}"
            ));
        }
        return Err(error);
    }
    Ok(serde_json::json!({
        "message":summary,
        "version":restored.version,
        "channels":sound_tool_inventory(&restored)
    })
    .to_string())
}

#[cfg(test)]
pub(crate) fn render_audio(
    session_path: &Path,
    arguments: &JsonValue,
) -> Result<AudioRender, String> {
    render_audio_request(prepare_audio_render(session_path, arguments)?)
}

pub(crate) fn prepare_audio_render(
    session_path: &Path,
    arguments: &JsonValue,
) -> Result<AudioRenderRequest, String> {
    let project = current_project(session_path)?;
    let (track_ids, start, end) = audio_region_arguments(&project, arguments)?;
    let description = format!(
        "Rendered {} from {:.3} to {:.3} seconds",
        selected_channel_labels(&project, &track_ids),
        start,
        end,
    );
    Ok(AudioRenderRequest {
        project,
        track_ids,
        start,
        end,
        description,
    })
}

pub(crate) fn prepare_instrument_audition(
    session_path: &Path,
    arguments: &JsonValue,
) -> Result<AudioRenderRequest, String> {
    let current = current_project(session_path)?;
    let object = arguments
        .as_object()
        .ok_or_else(|| "audition arguments must be an object".to_owned())?;
    let preset_id = required_string(object, "presetId")?;
    if crate::surge_presets::find(preset_id).is_none() {
        return Err(format!(
            "Surge XT factory preset is not installed: {preset_id}; use {PRESET_TOOL_NAME} to discover valid preset IDs"
        ));
    }
    if let Some(error) = crate::surge_presets::headless_render_error(preset_id) {
        return Err(error);
    }
    let duration_beats = required_number(object, "durationBeats")? as f32;
    let seconds_per_beat = 60.0 / f32::from(current.bpm);
    let duration_seconds = duration_beats * seconds_per_beat;
    if !(0.25..=16.0).contains(&duration_beats) || duration_seconds > MAX_REGION_SECONDS {
        let maximum_beats = (MAX_REGION_SECONDS / seconds_per_beat).min(16.0);
        return Err(format!(
            "durationBeats must be between 0.25 and {maximum_beats:.3} at {} BPM (0.125 to {MAX_REGION_SECONDS} seconds)",
            current.bpm
        ));
    }

    let mut project = Project::initial();
    project.bpm = current.bpm;
    project.duration = duration_seconds.max(0.25);
    let mut studio = Studio::from_project(project);
    let track_id = studio.project().tracks[0].id;
    let instrument_id = studio.project().tracks[0].instrument.id;
    studio
        .configure_sound_tool(
            track_id,
            "instrument",
            instrument_id,
            None,
            "preset",
            preset_id,
        )
        .map_err(studio_error_message)?;
    if let Some(effect) = object.get("effect") {
        let effect = effect
            .as_object()
            .ok_or_else(|| "effect must be an object with name and mix".to_owned())?;
        let name = required_string(effect, "name")?;
        if !EFFECT_NAMES.contains(&name) {
            return Err(format!(
                "effect.name must be one of: {}",
                EFFECT_NAMES.join(", ")
            ));
        }
        let mix = required_number(effect, "mix")? as f32;
        if !(0.0..=1.0).contains(&mix) {
            return Err("effect.mix must be between 0 and 1".to_owned());
        }
        studio
            .create_effect(track_id, name, mix)
            .map_err(studio_error_message)?;
    }
    let mut clip = object.clone();
    clip.insert("trackId".to_owned(), track_id.into());
    clip.insert("label".to_owned(), "Audition".into());
    clip.insert("startBeat".to_owned(), 0.into());
    clip.insert("playback".to_owned(), serde_json::json!({"mode":"once"}));
    let (_, spec) = clip_arguments(&clip, current.bpm)?;
    studio
        .create_midi_clip(track_id, &spec)
        .map_err(studio_error_message)?;
    let effect_description = object
        .get("effect")
        .and_then(JsonValue::as_object)
        .and_then(|effect| effect.get("name"))
        .and_then(JsonValue::as_str)
        .map_or_else(String::new, |name| format!(" through {name}"));
    Ok(AudioRenderRequest {
        project: studio.project().clone(),
        track_ids: vec![track_id],
        start: 0.0,
        end: duration_seconds,
        description: format!(
            "Auditioned {preset_id}{effect_description} for {duration_beats:.3} beats ({duration_seconds:.3} seconds) at {} BPM",
            current.bpm
        ),
    })
}

pub(crate) fn render_audio_request(request: AudioRenderRequest) -> Result<AudioRender, String> {
    render_audio_request_cancellable(request, || false)
}

pub(crate) fn render_audio_request_cancellable(
    request: AudioRenderRequest,
    cancelled: impl FnMut() -> bool,
) -> Result<AudioRender, String> {
    let regions = audio_analysis::render_region_with_tracks_cancellable(
        &request.project,
        &request.track_ids,
        request.start,
        request.end,
        cancelled,
    )?;
    validate_feedback_audio(&regions.mix)?;
    let measurements = audio_measurements(&request, "Surge XT", &regions);
    Ok(AudioRender {
        description: format!(
            "{} using the Surge XT rendering engine. Listen to the audio itself and describe the audible rhythm, subdivision, energy contour, timbre, transitions, and shortcomings before deciding what to do next.",
            request.description
        ),
        measurements,
        wav: audio_analysis::wav_bytes(&regions.mix.samples),
    })
}

fn validate_feedback_audio(region: &audio_analysis::AudioRegion) -> Result<(), String> {
    validate_feedback_samples(&region.samples, region.event_count)
}

fn validate_feedback_samples(samples: &[f32], event_count: usize) -> Result<(), String> {
    if !samples.iter().all(|sample| sample.is_finite()) {
        return Err(
            "Surge XT produced non-finite audio; choose another preset or effect".to_owned(),
        );
    }
    let peak = samples.iter().copied().map(f32::abs).fold(0.0, f32::max);
    if peak > 4.0 {
        return Err(format!(
            "Surge XT produced a pathological peak ({:.2} full scale); choose another preset or effect",
            peak
        ));
    }
    let dc_offset = if samples.is_empty() {
        0.0
    } else {
        samples.iter().sum::<f32>() / samples.len() as f32
    };
    if dc_offset.abs() > 0.25 {
        return Err(format!(
            "Surge XT produced a pathological DC offset ({dc_offset:.2}); choose another preset or effect"
        ));
    }
    if event_count > 0 && peak <= 0.000_01 {
        return Err(format!(
            "Surge XT rendered silence despite {} MIDI events; choose another preset or effect",
            event_count
        ));
    }
    Ok(())
}

fn audio_measurements(
    request: &AudioRenderRequest,
    backend: &str,
    regions: &audio_analysis::AudioRegions,
) -> JsonValue {
    let seconds = |value: f32| (f64::from(value) * 1_000_000.0).round() / 1_000_000.0;
    let tracks = regions
        .tracks
        .iter()
        .filter_map(|(track_id, region)| {
            request
                .project
                .tracks
                .iter()
                .find(|track| track.id == *track_id)
                .map(|track| {
                    serde_json::json!({
                        "trackId": track.id,
                        "trackName": track.name,
                        "muted": track.muted,
                        "measurements": region_measurements(region)
                    })
                })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "renderer": backend,
        "bpm": request.project.bpm,
        "secondsPerBeat": seconds(60.0 / f32::from(request.project.bpm)),
        "sampleRateHz": audio_analysis::SAMPLE_RATE,
        "channelCount": audio_analysis::CHANNEL_COUNT,
        "startSeconds": seconds(request.start),
        "endSeconds": seconds(request.end),
        "durationSeconds": seconds(request.end - request.start),
        "startBeats": seconds(request.start * f32::from(request.project.bpm) / 60.0),
        "endBeats": seconds(request.end * f32::from(request.project.bpm) / 60.0),
        "durationBeats": seconds((request.end - request.start) * f32::from(request.project.bpm) / 60.0),
        "frequencyBandsHz": {
            "low": [0, 250],
            "mid": [250, 2500],
            "high": [2500, audio_analysis::SAMPLE_RATE / 2]
        },
        "mix": region_measurements(&regions.mix),
        "tracks": tracks
    })
}

fn region_measurements(region: &audio_analysis::AudioRegion) -> JsonValue {
    let analysis = audio_analysis::analyze(region);
    let amplitude_dbfs = |amplitude: f32| {
        if amplitude > 0.0 {
            Some(20.0 * amplitude.log10())
        } else {
            None
        }
    };
    let dc_offset = if region.samples.is_empty() {
        0.0
    } else {
        region.samples.iter().sum::<f32>() / region.samples.len() as f32
    };
    serde_json::json!({
        "peakDbfs": amplitude_dbfs(analysis.peak),
        "rmsDbfs": amplitude_dbfs(analysis.rms),
        "crestFactorDb": if analysis.rms > 0.0 {
            Some(20.0 * (analysis.peak / analysis.rms).log10())
        } else {
            None
        },
        "clippedSampleCount": region.samples.iter().filter(|sample| sample.abs() >= 1.0).count(),
        "dcOffset": dc_offset,
        "zeroCrossingRate": analysis.zero_crossing_rate,
        "spectralCentroidHz": analysis.spectral_centroid_hz,
        "lowBandEnergyRatio": analysis.low_energy_ratio,
        "midBandEnergyRatio": analysis.mid_energy_ratio,
        "highBandEnergyRatio": analysis.high_energy_ratio
    })
}

fn current_project(session_path: &Path) -> Result<Project, String> {
    let source = read_bounded_text(
        &session_path.join(GRAPH_FILE),
        MAX_SOUND_GRAPH_BYTES,
        "Gemini sound graph",
    )
    .map_err(|error| format!("could not read sound-graph.json: {error}"))?;
    Project::from_json(&source).map_err(|error| format!("sound-graph.json is invalid: {error}"))
}

fn audio_region_arguments(
    project: &Project,
    arguments: &JsonValue,
) -> Result<(Vec<u64>, f32, f32), String> {
    let arguments = arguments
        .as_object()
        .ok_or_else(|| "audio analysis arguments must be an object".to_owned())?;
    let track_ids = match arguments.get("tracks") {
        None => project.tracks.iter().map(|track| track.id).collect(),
        Some(JsonValue::String(value)) if value == "all" => {
            project.tracks.iter().map(|track| track.id).collect()
        }
        Some(JsonValue::Array(values)) => {
            if values.is_empty() || values.len() > 32 {
                return Err("tracks must contain between 1 and 32 track IDs".to_owned());
            }
            let mut track_ids = Vec::with_capacity(values.len());
            for value in values {
                let track_id = value
                    .as_u64()
                    .filter(|track_id| *track_id > 0)
                    .ok_or_else(|| "tracks must contain positive integers".to_owned())?;
                if track_ids.contains(&track_id) {
                    return Err(format!("track {track_id} was requested more than once"));
                }
                if !project.tracks.iter().any(|track| track.id == track_id) {
                    let available = project
                        .tracks
                        .iter()
                        .map(|track| track.id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!(
                        "track {track_id} does not exist; available track IDs: {available}"
                    ));
                }
                track_ids.push(track_id);
            }
            track_ids
        }
        Some(_) => return Err("tracks must be \"all\" or an array of track IDs".to_owned()),
    };
    let number = |name: &str| {
        arguments
            .get(name)
            .and_then(JsonValue::as_f64)
            .filter(|value| value.is_finite())
            .map(|value| value as f32)
            .ok_or_else(|| format!("{name} must be a finite number"))
    };
    let start = number("start")?;
    let end = number("end")?;
    if start < 0.0 || end <= start || end > project.duration {
        return Err(format!(
            "render range must be between 0 and {:.3} seconds with end after start",
            project.duration
        ));
    }
    if end - start > MAX_REGION_SECONDS {
        return Err(format!(
            "render ranges are limited to {MAX_REGION_SECONDS} seconds"
        ));
    }
    Ok((track_ids, start, end))
}

fn selected_channel_labels(project: &Project, track_ids: &[u64]) -> String {
    track_ids
        .iter()
        .filter_map(|track_id| project.tracks.iter().find(|track| track.id == *track_id))
        .map(|track| format!("Track ID {}", track.id))
        .collect::<Vec<_>>()
        .join(", ")
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

pub(crate) fn base64_audio(bytes: &[u8]) -> String {
    base64(bytes)
}

fn sound_tool_inventory(project: &Project) -> Vec<JsonValue> {
    project
        .tracks
        .iter()
        .map(|track| {
            serde_json::json!({
                "id": track.id,
                "instrumentId": track.instrument.id,
                "effects": track.effects.iter().map(|effect| {
                    serde_json::json!({"id": effect.id, "name": effect.name})
                }).collect::<Vec<_>>(),
                "modulators": track.modulators.iter().map(|modulator| {
                    serde_json::json!({
                        "id": modulator.id,
                        "name": modulator.name,
                        "target": modulator.target,
                        "trigger": modulator.trigger,
                        "sourceTrackId": modulator.source_track_id
                    })
                }).collect::<Vec<_>>(),
                "clips": track.clips.iter().map(|clip| {
                    serde_json::json!({"id": clip.id, "label": clip.label})
                }).collect::<Vec<_>>()
            })
        })
        .collect()
}

fn wait_for_progress_handoff(session_path: &Path) {
    let path = progress_path(session_path);
    // The single slot transfers ownership in order and bounds temporary graph storage.
    while path.exists() {
        thread::sleep(Duration::from_millis(10));
    }
}

fn publish_progress(session_path: &Path, plan: &str, project: &Project) -> Result<(), String> {
    let pending = session_path.join(PENDING_PROGRESS_DIRECTORY);
    let published = progress_path(session_path);
    let result = (|| {
        fs::create_dir(&pending)
            .map_err(|error| format!("could not prepare Gemini edit progress: {error}"))?;
        write_new(&pending.join(PROGRESS_PLAN_FILE), plan)
            .map_err(|error| format!("could not record Gemini edit plan progress: {error}"))?;
        write_new(&pending.join(PROGRESS_GRAPH_FILE), &project.to_json())
            .map_err(|error| format!("could not record Gemini sound graph progress: {error}"))?;
        fs::rename(&pending, &published)
            .map_err(|error| format!("could not publish Gemini edit progress: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&pending);
    }
    result
}

fn progress_path(session_path: &Path) -> PathBuf {
    session_path.join(PROGRESS_DIRECTORY)
}

fn bounded_compound(mut actions: Vec<Action>) -> Action {
    while actions.len() > MAX_COMPOUND_ACTIONS {
        let mut grouped = Vec::with_capacity(actions.len().div_ceil(MAX_COMPOUND_ACTIONS));
        let mut remaining = actions.into_iter();
        loop {
            let children = remaining
                .by_ref()
                .take(MAX_COMPOUND_ACTIONS)
                .collect::<Vec<_>>();
            if children.is_empty() {
                break;
            }
            grouped.push(action_group(children));
        }
        actions = grouped;
    }
    action_group(actions)
}

fn action_group(mut actions: Vec<Action>) -> Action {
    if actions.len() == 1 {
        actions.pop().expect("one action")
    } else {
        Action::Compound { actions }
    }
}

fn studio_error_message(error: StudioError) -> String {
    match error {
        StudioError::EmptyPrompt => "The edit request is empty.".to_owned(),
        StudioError::InvalidPrompt => "The edit request is too long.".to_owned(),
        StudioError::InvalidSelection => {
            "The selected region is outside the sound graph duration.".to_owned()
        }
        StudioError::UnknownTrack => "track not found; call read_sound_graph".to_owned(),
        StudioError::InvalidMix => "mixer value out of range".to_owned(),
        StudioError::InvalidDuration => "project duration out of range".to_owned(),
        StudioError::InvalidChannel => "channel limit exceeded".to_owned(),
        StudioError::LastTrack => "cannot delete the only track; create another first".to_owned(),
        StudioError::UnknownSoundTool => {
            "sound-tool ID not found; call read_sound_graph".to_owned()
        }
        StudioError::InvalidSoundTool => "invalid sound-tool parameter or value".to_owned(),
        StudioError::EffectCapacity => {
            "effect chain full: Surge XT supports at most 8 enabled serial effects".to_owned()
        }
    }
}

fn reserve_session_directory(root: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    set_private_directory(root)?;
    for _ in 0..64 {
        let id = SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(
            "{}-{}-{id}",
            unix_milliseconds(),
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                if let Err(error) = set_private_directory(&path) {
                    let _ = fs::remove_dir(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a Gemini edit session",
    ))
}

#[cfg(test)]
pub(crate) fn session_root() -> PathBuf {
    if let Some(path) =
        std::env::var_os("DAW_AI_GEMINI_SESSION_DIR").filter(|path| !path.is_empty())
    {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("DAW_AI_PROJECT_PATH").filter(|path| !path.is_empty()) {
        if let Some(parent) = Path::new(&path).parent() {
            return parent.join("gemini-sessions");
        }
    }
    if cfg!(test) {
        return std::env::temp_dir().join(format!("daw-ai-gemini-tests-{}", std::process::id()));
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("gemini-sessions")
}

#[cfg(test)]
pub(crate) fn session_summaries() -> io::Result<Vec<JsonValue>> {
    session_summaries_in(&session_root())
}

pub(crate) fn session_summaries_in(root: &Path) -> io::Result<Vec<JsonValue>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path().join(SESSION_FILE);
        if !path.is_file() {
            continue;
        }
        let Ok(source) =
            read_bounded_text(&path, MAX_SESSION_JSON_BYTES, "Gemini session metadata")
        else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<JsonValue>(&source) else {
            continue;
        };
        sessions.push(value);
    }
    sessions.sort_by_key(|session| {
        std::cmp::Reverse(
            session
                .get("createdAt")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
        )
    });
    sessions.truncate(100);
    Ok(sessions)
}

pub(crate) fn apply_session_retention(root: &Path) -> io::Result<()> {
    apply_session_retention_with(root, SessionRetention::configured())
}

struct RetainedSession {
    path: PathBuf,
    updated: SystemTime,
    running: bool,
    bytes: u64,
}

fn apply_session_retention_with(root: &Path, policy: SessionRetention) -> io::Result<()> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let metadata_path = entry.path().join(SESSION_FILE);
        let Some(metadata) = read_bounded_text(
            &metadata_path,
            MAX_SESSION_JSON_BYTES,
            "Gemini session metadata",
        )
        .ok()
        .and_then(|source| serde_json::from_str::<JsonValue>(&source).ok())
        .filter(|metadata| valid_session_metadata(&entry.path(), metadata)) else {
            continue;
        };
        let running = metadata.get("status").and_then(JsonValue::as_str) == Some("running");
        let updated = metadata
            .get("updatedAt")
            .and_then(JsonValue::as_u64)
            .map(|milliseconds| UNIX_EPOCH + Duration::from_millis(milliseconds))
            .unwrap_or(UNIX_EPOCH);
        sessions.push(RetainedSession {
            bytes: directory_bytes(&entry.path())?,
            path: entry.path(),
            updated,
            running,
        });
    }
    sessions.sort_by_key(|session| session.updated);
    let now = SystemTime::now();
    let mut total_bytes = sessions.iter().map(|session| session.bytes).sum::<u64>();

    for session in sessions.iter_mut().filter(|session| !session.running) {
        let expired = now.duration_since(session.updated).unwrap_or_default() > policy.maximum_age;
        let over_budget = total_bytes > policy.maximum_bytes;
        if !expired && !over_budget {
            continue;
        }
        for entry in fs::read_dir(&session.path)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                continue;
            }
            let is_audio = entry.path().extension().and_then(|value| value.to_str()) == Some("wav");
            if is_audio {
                let bytes = entry.metadata()?.len();
                fs::remove_file(entry.path())?;
                session.bytes = session.bytes.saturating_sub(bytes);
                total_bytes = total_bytes.saturating_sub(bytes);
            }
        }
    }

    let mut retained_count = sessions.len();
    for session in sessions.iter().filter(|session| !session.running) {
        let expired = now.duration_since(session.updated).unwrap_or_default() > policy.maximum_age;
        if !expired && retained_count <= policy.maximum_count && total_bytes <= policy.maximum_bytes
        {
            continue;
        }
        fs::remove_dir_all(&session.path)?;
        retained_count = retained_count.saturating_sub(1);
        total_bytes = total_bytes.saturating_sub(session.bytes);
    }
    Ok(())
}

fn valid_session_metadata(path: &Path, metadata: &JsonValue) -> bool {
    let directory_id = path.file_name().and_then(|name| name.to_str());
    metadata.get("id").and_then(JsonValue::as_str) == directory_id
        && metadata
            .get("createdAt")
            .and_then(JsonValue::as_u64)
            .is_some()
        && metadata
            .get("updatedAt")
            .and_then(JsonValue::as_u64)
            .is_some()
        && matches!(
            metadata.get("status").and_then(JsonValue::as_str),
            Some("running" | "completed" | "failed")
        )
        && path.join(GRAPH_FILE).is_file()
        && path.join(REQUEST_FILE).is_file()
}

fn directory_bytes(path: &Path) -> io::Result<u64> {
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.path().symlink_metadata()?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        bytes = bytes.saturating_add(if metadata.is_dir() {
            directory_bytes(&entry.path())?
        } else {
            metadata.len()
        });
    }
    Ok(bytes)
}

fn configured_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

fn unix_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_new(path: &Path, source: &str) -> io::Result<()> {
    write_new_with(path, |file| {
        file.write_all(source.as_bytes())?;
        file.write_all(b"\n")
    })
}

fn write_new_with(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = write(&mut file).and_then(|()| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn write_replace(path: &Path, source: &str) -> io::Result<()> {
    replace_text_file(path, &format!("{}\n", source.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with_effect(name: &str) -> Project {
        let mut project = Project::demo();
        let effect = crate::model::Effect {
            id: 99_000,
            name: name.to_owned(),
            preset_slot: None,
            mix: 0.5,
            enabled: true,
            parameters: crate::surge::effect_parameter_values(name),
            parameter_overrides: Vec::new(),
            tempo_sync_parameters: Vec::new(),
            deactivated_parameters: Vec::new(),
        };
        project.tracks[0].routing.effect_order.push(effect.id);
        project.tracks[0].effects.push(effect);
        project
    }

    #[test]
    fn declares_direct_graph_editing_and_audio_tools() {
        let declarations = tool_declarations(false);
        let names = declarations
            .iter()
            .filter_map(|tool| tool.get("name").and_then(JsonValue::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            names[0..7],
            [
                READ_TOOL_NAME,
                AUDIO_TOOL_NAME,
                ANALYZE_AUDIO_TOOL_NAME,
                AUDITION_TOOL_NAME,
                PRESET_TOOL_NAME,
                INSTRUMENT_PARAMETER_TOOL_NAME,
                SOUND_TOOL_PARAMETER_TOOL_NAME,
            ]
        );
        assert!(!names.contains(&SET_INSTRUMENT_PARAMETERS_TOOL_NAME));
        assert!(!names.contains(&UPDATE_EFFECT_PARAMETERS_TOOL_NAME));
        assert_eq!(
            &names[7..],
            MUTATION_TOOL_NAMES
                .iter()
                .copied()
                .filter(|name| {
                    *name != SET_INSTRUMENT_PARAMETERS_TOOL_NAME
                        && *name != UPDATE_EFFECT_PARAMETERS_TOOL_NAME
                })
                .collect::<Vec<_>>()
        );
        assert!(
            declarations[1]["description"]
                .as_str()
                .unwrap()
                .contains("without measurements")
        );
        let analysis_description = declarations[2]["description"]
            .as_str()
            .expect("analysis tool description");
        assert!(analysis_description.contains("without audio"));
        assert!(analysis_description.contains("without musical judgments"));
        assert_eq!(
            declarations[3]["parameters"]["properties"]["durationBeats"]["maximum"],
            16
        );
        let midi = declarations
            .iter()
            .find(|tool| tool["name"] == "add_midi_clip")
            .expect("MIDI clip declaration");
        assert_eq!(
            midi["description"],
            "Add a beat-positioned MIDI clip without changing other clips."
        );
        assert_eq!(
            midi["parameters"]["properties"]["durationBeats"]["maximum"],
            MAX_ONCE_PLAYBACK_BEATS
        );
        assert_eq!(
            midi["parameters"]["properties"]["events"]["maxItems"],
            MAX_MIDI_EVENTS_PER_CLIP
        );
        assert_eq!(
            midi["parameters"]["properties"]["events"]["items"]["properties"]["duration"]["minimum"],
            MIN_MIDI_NOTE_BEATS
        );
        assert_eq!(
            midi["parameters"]["properties"]["events"]["items"]["properties"]["duration"]["maximum"],
            MAX_MIDI_NOTE_DURATION_BEATS
        );
        let new_track = declarations
            .iter()
            .find(|tool| tool["name"] == "new_track")
            .expect("new track declaration");
        assert_eq!(
            new_track["parameters"]["properties"]
                .as_object()
                .expect("new track properties")
                .len(),
            2
        );
        assert_eq!(
            new_track["parameters"]["properties"]["description"]["maxLength"],
            16
        );
        assert_eq!(
            new_track["parameters"]["properties"]["color"]["enum"]
                .as_array()
                .map(Vec::len),
            Some(TRACK_COLOR_PALETTE.len())
        );
        let set_track_identity = declarations
            .iter()
            .find(|tool| tool["name"] == "set_track_identity")
            .expect("track identity declaration");
        assert_eq!(
            set_track_identity["parameters"]["required"],
            serde_json::json!(["trackId", "name", "color"])
        );
        assert_eq!(
            set_track_identity["parameters"]["properties"]["color"]["enum"]
                .as_array()
                .map(Vec::len),
            Some(TRACK_COLOR_PALETTE.len())
        );
        let update_modulator = declarations
            .iter()
            .find(|tool| tool["name"] == "update_modulator")
            .expect("modulator update declaration");
        assert_eq!(
            update_modulator["parameters"]["properties"]["value"]["maxLength"],
            8_192
        );

        let batch = tool_declarations(true);
        for name in [
            SET_INSTRUMENT_PARAMETERS_TOOL_NAME,
            UPDATE_EFFECT_PARAMETERS_TOOL_NAME,
        ] {
            let declaration = batch
                .iter()
                .find(|tool| tool["name"] == name)
                .expect("enabled batch declaration");
            assert_eq!(
                declaration["parameters"]["properties"]["changes"]["maxItems"],
                32
            );
        }
    }

    #[test]
    fn batch_parameter_mutations_are_atomic_and_reject_duplicates() {
        let project = project_with_effect("Tape");
        let track = &project.tracks[0];
        let track_id = track.id;
        let effect_id = track.effects[0].id;
        let session = EditSession::create(&project, "batch controls", 0.0, 1.0).expect("session");

        let response: JsonValue = serde_json::from_str(
            &apply_agent_mutation(
                session.path(),
                UPDATE_EFFECT_PARAMETERS_TOOL_NAME,
                &serde_json::json!({
                    "trackId":track_id,
                    "effectId":effect_id,
                    "changes":[
                        {"parameter":"mix","value":"0.2"},
                        {"parameter":"Amount","value":"0.7"}
                    ]
                }),
            )
            .expect("effect batch"),
        )
        .expect("batch response");
        assert!(
            response["message"]
                .as_str()
                .unwrap()
                .contains("2 parameters")
        );
        let updated = session.take_update().unwrap().expect("one atomic update");
        let effect = &updated.1.tracks[0].effects[0];
        assert!((effect.mix - 0.2).abs() < 0.001);
        assert!(
            effect
                .parameters
                .get("Amount")
                .is_some_and(|value| (*value - 0.7).abs() < 0.001)
        );
        assert!(effect.parameter_overrides.contains(&"Amount".to_owned()));
        assert!(session.take_update().unwrap().is_none());

        let before = current_project(session.path()).unwrap().to_json();
        let error = apply_agent_mutation(
            session.path(),
            SET_INSTRUMENT_PARAMETERS_TOOL_NAME,
            &serde_json::json!({
                "trackId":track_id,
                "changes":[
                    {"parameter":"native:264","value":"0.4"},
                    {"parameter":"native:999999","value":"0.5"}
                ]
            }),
        )
        .expect_err("invalid second item rejects batch");
        assert!(error.contains("native:999999"));
        assert_eq!(current_project(session.path()).unwrap().to_json(), before);
        assert!(session.take_update().unwrap().is_none());

        let error = apply_agent_mutation(
            session.path(),
            SET_INSTRUMENT_PARAMETERS_TOOL_NAME,
            &serde_json::json!({
                "trackId":track_id,
                "changes":[
                    {"parameter":"native:264","value":"0.4"},
                    {"parameter":"native:264","value":"0.5"}
                ]
            }),
        )
        .expect_err("duplicate parameter rejected");
        assert!(error.contains("appears more than once"));
        assert_eq!(current_project(session.path()).unwrap().to_json(), before);

        let response: JsonValue = serde_json::from_str(
            &apply_agent_mutation(
                session.path(),
                SET_INSTRUMENT_PARAMETERS_TOOL_NAME,
                &serde_json::json!({
                    "trackId":track_id,
                    "changes":[
                        {"parameter":"native:264","value":"0.4"},
                        {"parameter":"native:265","value":"0.6"}
                    ]
                }),
            )
            .expect("instrument batch"),
        )
        .expect("batch response");
        assert!(response["message"].as_str().unwrap().contains("2 Surge XT"));
        let updated = session.take_update().unwrap().expect("one atomic update");
        assert_eq!(
            updated.1.tracks[0]
                .instrument
                .native_overrides
                .iter()
                .filter(|(parameter, _)| matches!(**parameter, 264 | 265))
                .count(),
            2
        );
    }

    #[test]
    fn disposable_audition_builds_audio_request_without_mutating_session() {
        let original = Project::initial();
        let session = EditSession::create(&original, "audition a lead", 0.0, 4.0).expect("session");
        let request = prepare_instrument_audition(
            session.path(),
            &serde_json::json!({
                "presetId":"Factory/Leads/Classic Lead 1",
                "durationBeats":2,
                "events":[
                    {"time":0,"duration":0.25,"pitch":60,"velocity":0.8},
                    {"time":1,"duration":0.25,"pitch":64,"velocity":0.8}
                ],
                "effect":{"name":"Delay","mix":0.25}
            }),
        )
        .expect("audition request");
        assert_eq!(request.project.bpm, original.bpm);
        assert_eq!(request.project.tracks.len(), 1);
        assert_eq!(
            request.project.tracks[0].instrument.preset,
            "Factory/Leads/Classic Lead 1"
        );
        assert_eq!(request.project.tracks[0].clips.len(), 1);
        assert_eq!(request.project.tracks[0].effects.len(), 1);
        assert_eq!(request.end, 1.0);
        assert!(session.take_update().unwrap().is_none());
        assert_eq!(
            current_project(session.path()).unwrap().to_json(),
            original.to_json()
        );

        let wavetable_request = prepare_instrument_audition(
            session.path(),
            &serde_json::json!({
                "presetId":"Factory/FX/Space Adventure 1",
                "durationBeats":1,
                "events":[{"time":0,"duration":0.25,"pitch":60,"velocity":0.8}]
            }),
        )
        .expect("factory wavetable audition preset");
        assert_eq!(
            wavetable_request.project.tracks[0].instrument.preset,
            "Factory/FX/Space Adventure 1"
        );
    }

    #[test]
    fn effect_selection_accepts_display_labels_and_reports_valid_choices() {
        let project = project_with_effect("Distortion");
        let track = &project.tracks[0];
        let effect = &track.effects[0];
        let semantics = crate::surge::effect_parameter_semantics(
            &track.instrument,
            &track.effects,
            &track.routing.effect_order,
            track.id,
            effect.id,
        );
        let (parameter, choice) = semantics
            .iter()
            .find_map(|(parameter, semantics)| {
                semantics
                    .choices
                    .first()
                    .map(|choice| (parameter.clone(), choice.clone()))
            })
            .expect("selection effect parameter");
        let session =
            EditSession::create(&project, "set effect choice", 0.0, 1.0).expect("session");
        apply_agent_mutation(
            session.path(),
            "update_effect",
            &serde_json::json!({
                "trackId":track.id,
                "effectId":effect.id,
                "parameter":parameter,
                "value":choice.1
            }),
        )
        .expect("display label accepted");
        session.take_update().unwrap().expect("effect update");
        let error = apply_agent_mutation(
            session.path(),
            "update_effect",
            &serde_json::json!({
                "trackId":track.id,
                "effectId":effect.id,
                "parameter":parameter,
                "value":"not a choice"
            }),
        )
        .expect_err("invalid display label");
        assert!(error.contains("must be one of:"));
        assert!(error.contains(&choice.1));
    }

    #[test]
    fn sound_graph_reads_compact_topology_and_one_node_at_a_time() {
        let project = Project::demo();
        let session =
            EditSession::create(&project, "inspect migrated controls", 0.0, 1.0).expect("session");
        let response = read_sound_graph(session.path(), &serde_json::json!({})).expect("topology");
        assert!(
            response.len() < 8_000,
            "topology was {} bytes",
            response.len()
        );
        let response: JsonValue = serde_json::from_str(&response).expect("graph JSON");
        assert!(response.get("tracks").is_none());
        assert!(
            response["nodes"]
                .as_array()
                .is_some_and(|nodes| !nodes.is_empty())
        );
        assert!(
            response["connections"]
                .as_array()
                .is_some_and(|connections| !connections.is_empty())
        );
        assert!(!response.to_string().contains("modulationTargets"));

        let track = &project.tracks[1];
        let instrument = read_sound_graph(
            session.path(),
            &serde_json::json!({"nodeId":format!("instrument:{}", track.instrument.id)}),
        )
        .expect("instrument detail");
        let instrument: JsonValue = serde_json::from_str(&instrument).expect("instrument JSON");
        assert_eq!(instrument["preset"], track.instrument.preset);
        assert_eq!(
            instrument["parameterBrowser"]["tool"],
            INSTRUMENT_PARAMETER_TOOL_NAME
        );
        assert!(instrument.get("modulationTargets").is_none());

        let clip = read_sound_graph(
            session.path(),
            &serde_json::json!({"nodeId":format!("clip:{}", track.clips[0].id)}),
        )
        .expect("clip detail");
        let clip: JsonValue = serde_json::from_str(&clip).expect("clip JSON");
        assert_eq!(
            clip["events"].as_array().map(Vec::len),
            Some(track.clips[0].events.len())
        );
    }

    #[test]
    fn instrument_parameter_listing_browses_small_native_modules() {
        let project = Project::demo();
        let track_id = project.tracks[1].id;
        let session = EditSession::create(&project, "browse controls", 0.0, 1.0).expect("session");
        let index: JsonValue = serde_json::from_str(
            &list_instrument_parameters(session.path(), &serde_json::json!({"trackId":track_id}))
                .expect("module index"),
        )
        .expect("module JSON");
        let modules = index["modules"].as_array().expect("modules");
        assert_eq!(modules.len(), 4);
        assert_eq!(modules[0]["id"], "global");
        assert!(index["midiContext"].is_object());
        assert!(index["midiContext"].get("recommendedRange").is_none());
        assert!(index.to_string().len() < 2_000);

        let scene: JsonValue = serde_json::from_str(
            &list_instrument_parameters(
                session.path(),
                &serde_json::json!({"trackId":track_id,"module":"scene:a"}),
            )
            .expect("scene modules"),
        )
        .expect("scene JSON");
        assert!(scene["modules"].as_array().expect("scene modules").len() >= 10);
        assert!(scene.to_string().len() < 6_000);
        assert!(
            scene["modules"]
                .as_array()
                .expect("scene modules")
                .iter()
                .find(|module| module["id"] == "scene:a/osc:1")
                .and_then(|module| module["state"].as_array())
                .is_some_and(|state| state.len() == 4)
        );

        let oscillator: JsonValue = serde_json::from_str(
            &list_instrument_parameters(
                session.path(),
                &serde_json::json!({"trackId":track_id,"module":"scene:a/osc:1"}),
            )
            .expect("oscillator parameters"),
        )
        .expect("oscillator JSON");
        assert!(!oscillator.to_string().contains("automationTarget"));
        assert_eq!(
            oscillator["parameters"]
                .as_array()
                .expect("parameters")
                .len(),
            16
        );
        let oscillator_parameters = oscillator["parameters"].as_array().expect("parameters");
        assert_eq!(oscillator["idType"], "editableParameter");
        assert!(
            oscillator_parameters
                .iter()
                .any(|parameter| { parameter["modulationTarget"] == parameter["parameter"] })
        );
        let mute = oscillator_parameters
            .iter()
            .find(|parameter| parameter["name"] == "Scene A Osc 1 Mute")
            .expect("oscillator mute");
        assert_eq!(mute["kind"], "boolean");
        assert!(mute.get("modulationTarget").is_none());
        assert!(oscillator.to_string().len() < 12_000);

        let filter: JsonValue = serde_json::from_str(
            &list_instrument_parameters(
                session.path(),
                &serde_json::json!({"trackId":track_id,"module":"scene:a/filter:1"}),
            )
            .expect("filter parameters"),
        )
        .expect("filter JSON");
        let selection = filter["parameters"]
            .as_array()
            .expect("filter parameters")
            .iter()
            .find(|parameter| parameter["kind"] == "selection")
            .expect("Surge selection control");
        let choice = selection["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .expect("Surge selection choice");
        assert!(
            choice["display"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        let mutation: JsonValue = serde_json::from_str(
            &apply_agent_mutation(
                session.path(),
                SET_INSTRUMENT_PARAMETER_TOOL_NAME,
                &serde_json::json!({
                    "trackId":track_id,
                    "parameter":selection["parameter"],
                    "value":choice["value"].to_string()
                }),
            )
            .expect("set Surge selection"),
        )
        .expect("mutation JSON");
        assert_eq!(mutation["display"], choice["display"]);
        session.take_update().unwrap().expect("selection update");

        let lfos: JsonValue = serde_json::from_str(
            &list_instrument_parameters(
                session.path(),
                &serde_json::json!({"trackId":track_id,"module":"scene:b/lfos"}),
            )
            .expect("LFO modules"),
        )
        .expect("LFO JSON");
        assert_eq!(lfos["modules"].as_array().expect("LFO modules").len(), 12);
        assert!(lfos.to_string().len() < 6_000);

        let lfo: JsonValue = serde_json::from_str(
            &list_instrument_parameters(
                session.path(),
                &serde_json::json!({"trackId":track_id,"module":"scene:b/lfo:scene:4"}),
            )
            .expect("LFO parameters"),
        )
        .expect("LFO JSON");
        assert_eq!(lfo["parameters"].as_array().expect("parameters").len(), 13);
        let rate = lfo["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .find(|parameter| parameter["name"] == "Scene B Scene LFO 4 Rate")
            .expect("LFO rate");
        assert_eq!(rate["tempoSync"], true);
        assert!(lfo.to_string().len() < 10_000);
    }

    #[test]
    fn formula_discovery_routes_long_source_to_modulator_update() {
        let mut project = project_with_effect("Delay");
        project.tracks[1].modulators[0].shape = "formula".to_owned();
        project.tracks[1].modulators[0].formula = "return sin(phase)".to_owned();
        let track = &project.tracks[1];
        let modulator = &track.modulators[0];
        let session = EditSession::create(&project, "inspect formula", 0.0, 1.0).expect("session");

        let response = list_sound_tool_parameters(
            session.path(),
            &serde_json::json!({
                "trackId":track.id,
                "tool":"modulator",
                "toolId":modulator.id
            }),
        )
        .expect("modulator parameters");
        let response: JsonValue = serde_json::from_str(&response).expect("parameter JSON");
        let formula = response["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .find(|parameter| parameter["parameter"] == "formula")
            .expect("formula parameter");
        assert_eq!(formula["maximumLength"], 8_192);
        assert_eq!(formula["mutationTool"], "update_modulator");
    }

    #[test]
    fn effect_discovery_exposes_only_native_surge_names() {
        let mut project = project_with_effect("Exciter");
        let track = &mut project.tracks[0];
        let effect = &mut track.effects[0];
        effect.name = "Delay".to_owned();
        effect.parameters = crate::surge::effect_parameter_values("Delay");
        let track_id = track.id;
        let effect_id = effect.id;
        let session = EditSession::create(&project, "inspect delay", 0.0, 1.0).expect("session");

        let response = list_sound_tool_parameters(
            session.path(),
            &serde_json::json!({
                "trackId":track_id,
                "tool":"effect",
                "toolId":effect_id
            }),
        )
        .expect("effect parameters");
        let response: JsonValue = serde_json::from_str(&response).expect("parameter JSON");
        assert_eq!(response["idType"], "editableParameter");
        let names = response["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .filter_map(|parameter| parameter["parameter"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(&names[..2], ["enabled", "mix"]);
        assert!(names.contains(&"Feedback"));
        assert!(!names.contains(&"time"));
        let mix = response["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .find(|parameter| parameter["parameter"] == "mix")
            .expect("mix");
        assert!(mix["display"].is_string());
        assert!(mix["kind"].is_string());
    }

    #[test]
    fn generic_effect_discovery_returns_every_surge_control_with_semantics() {
        let mut project = project_with_effect("Graphic EQ");
        let track = &mut project.tracks[0];
        let effect = &mut track.effects[0];
        effect.name = "Exciter".to_owned();
        effect.parameters.clear();
        let track_id = track.id;
        let effect_id = effect.id;
        let session = EditSession::create(&project, "inspect exciter", 0.0, 1.0).expect("session");
        let response: JsonValue = serde_json::from_str(
            &list_sound_tool_parameters(
                session.path(),
                &serde_json::json!({
                    "trackId":track_id,
                    "tool":"effect",
                    "toolId":effect_id
                }),
            )
            .expect("effect parameters"),
        )
        .expect("parameter JSON");
        let parameters = response["parameters"].as_array().expect("parameters");
        assert!(!parameters.is_empty());
        assert!(
            parameters
                .iter()
                .filter(|parameter| {
                    !matches!(parameter["parameter"].as_str(), Some("enabled" | "mix"))
                })
                .all(|parameter| {
                    parameter["display"].is_string() && parameter["kind"].is_string()
                })
        );
    }

    #[test]
    fn every_discovered_native_effect_parameter_is_editable() {
        let mut project = project_with_effect("Graphic EQ");
        let track = &mut project.tracks[0];
        let effect = &mut track.effects[0];
        effect.name = "Graphic EQ".to_owned();
        effect.parameters = crate::surge::effect_parameter_values("Graphic EQ");
        let track_id = track.id;
        let effect_id = effect.id;
        let session =
            EditSession::create(&project, "edit discovered EQ band", 0.0, 1.0).expect("session");
        let response: JsonValue = serde_json::from_str(
            &list_sound_tool_parameters(
                session.path(),
                &serde_json::json!({
                    "trackId":track_id,
                    "tool":"effect",
                    "toolId":effect_id
                }),
            )
            .expect("effect parameters"),
        )
        .expect("parameter JSON");
        let parameter = response["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .find(|parameter| parameter["kind"] == "continuous")
            .and_then(|parameter| parameter["parameter"].as_str())
            .expect("native effect parameter");
        let mutation: JsonValue = serde_json::from_str(
            &apply_agent_mutation(
                session.path(),
                "update_effect",
                &serde_json::json!({
                    "trackId":track_id,
                    "effectId":effect_id,
                    "parameter":parameter,
                    "value":"0.75"
                }),
            )
            .expect("discovered parameter update"),
        )
        .expect("mutation JSON");
        assert!(
            mutation["display"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        let (_, updated) = session.take_update().unwrap().expect("published update");
        assert_eq!(
            updated.tracks[0].effects[0].parameters.get(parameter),
            Some(&0.75)
        );
    }

    #[test]
    fn mutation_errors_and_modulator_rate_mode_are_explicit() {
        let initial = Project::initial();
        let session =
            EditSession::create(&initial, "exercise concise mutations", 0.0, 1.0).expect("session");
        let error = apply_agent_mutation(
            session.path(),
            "delete_track",
            &serde_json::json!({"trackId":initial.tracks[0].id}),
        )
        .expect_err("only track cannot be deleted");
        assert_eq!(error, "cannot delete the only track; create another first");

        let project = Project::demo();
        let track = &project.tracks[1];
        let session =
            EditSession::create(&project, "add tempo modulation", 0.0, 1.0).expect("session");
        apply_agent_mutation(
            session.path(),
            "add_modulator",
            &serde_json::json!({
                "trackId":track.id,
                "target":track.modulators[0].target,
                "shape":"sine",
                "rate":2,
                "rateMode":"tempo",
                "depth":0.5,
                "trigger":"free",
                "attackMs":5,
                "releaseMs":180,
                "polarity":"increase"
            }),
        )
        .expect("tempo-synced modulator");
        let (_, updated) = session.take_update().unwrap().expect("modulator update");
        assert_eq!(
            updated.tracks[1]
                .modulators
                .last()
                .expect("new modulator")
                .rate_mode,
            "tempo"
        );
    }

    #[test]
    fn studio_contract_documents_every_registered_tool() {
        let contract = include_str!("../gemini/STUDIO.md");
        for name in [
            READ_TOOL_NAME,
            AUDIO_TOOL_NAME,
            PRESET_TOOL_NAME,
            INSTRUMENT_PARAMETER_TOOL_NAME,
            SOUND_TOOL_PARAMETER_TOOL_NAME,
        ]
        .into_iter()
        .chain(MUTATION_TOOL_NAMES.iter().copied())
        {
            assert!(
                contract.contains(&format!("`{name}`")),
                "gemini/STUDIO.md does not document {name}"
            );
        }
    }

    #[test]
    fn persists_session_metadata_and_wav_artifacts() {
        let session =
            EditSession::create(&Project::demo(), "test the drop", 0.0, 2.0).expect("edit session");
        let rendered = render_audio(
            session.path(),
            &serde_json::json!({"tracks": [1, 2], "start": 0, "end": 1}),
        )
        .expect("audio render");
        assert_eq!(&rendered.wav[..4], b"RIFF");
        assert_eq!(&rendered.wav[8..12], b"WAVE");
        let artifact = session
            .record_audio(1, &rendered.wav)
            .expect("WAV artifact");
        assert!(session.path().join(artifact).is_file());
        session
            .update_status("completed", "Done", 2, 1)
            .expect("session metadata");
        let session_id = session.path().file_name().unwrap().to_string_lossy();
        let summaries = session_summaries().expect("session summaries");
        let summary = summaries
            .iter()
            .find(|summary| summary["id"] == session_id.as_ref())
            .expect("current session summary");
        assert_eq!(summary["status"], "completed");
        assert_eq!(summary["appliedSteps"], 2);
        assert_eq!(summary["audioListens"], 1);
    }

    #[cfg(unix)]
    #[test]
    fn trusted_session_metadata_replaces_a_workspace_symlink() {
        let session =
            EditSession::create(&Project::demo(), "secure metadata", 0.0, 2.0).expect("session");
        let trusted = session.metadata_source().expect("trusted metadata");
        let target = session.path().join("unrelated.json");
        fs::write(&target, r#"{"secret":"keep"}"#).expect("symlink target");
        fs::remove_file(session.path().join(SESSION_FILE)).expect("remove metadata path");
        std::os::unix::fs::symlink(&target, session.path().join(SESSION_FILE))
            .expect("metadata symlink");

        session
            .update_status_from(&trusted, "failed", "safe", 0, 0)
            .expect("safe finalization");
        assert_eq!(
            fs::read_to_string(target).expect("target unchanged"),
            r#"{"secret":"keep"}"#
        );
        assert_eq!(
            serde_json::from_str::<JsonValue>(
                &session.metadata_source().expect("restored metadata")
            )
            .expect("metadata JSON")["status"],
            "failed"
        );
    }

    #[test]
    fn retention_preserves_running_sessions_and_prunes_old_audio_first() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-retention-{}-{}",
            std::process::id(),
            SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("retention root");
        let old = root.join("old");
        let running = root.join("running");
        let unknown = root.join("unrelated");
        let malformed = root.join("malformed");
        fs::create_dir(&old).expect("old session");
        fs::create_dir(&running).expect("running session");
        fs::create_dir(&unknown).expect("unrelated directory");
        fs::create_dir(&malformed).expect("malformed directory");
        write_new(
            &old.join(SESSION_FILE),
            r#"{"id":"old","status":"completed","createdAt":1,"updatedAt":1}"#,
        )
        .expect("old metadata");
        write_new(
            &running.join(SESSION_FILE),
            r#"{"id":"running","status":"running","createdAt":1,"updatedAt":1}"#,
        )
        .expect("running metadata");
        for session in [&old, &running] {
            write_new(&session.join(GRAPH_FILE), "{}").expect("session graph marker");
            write_new(&session.join(REQUEST_FILE), "{}").expect("session request marker");
        }
        fs::write(unknown.join("keep.txt"), b"not a DAW-AI session").expect("unrelated content");
        fs::write(malformed.join(SESSION_FILE), b"{not JSON").expect("malformed metadata");
        fs::write(malformed.join("keep.txt"), b"keep malformed session")
            .expect("malformed content");
        fs::write(old.join("audio-001.wav"), vec![0_u8; 128]).expect("old audio");
        fs::write(running.join("audio-001.wav"), vec![0_u8; 128]).expect("running audio");
        #[cfg(unix)]
        std::os::unix::fs::symlink(".", running.join("loop")).expect("session symlink");

        apply_session_retention_with(
            &root,
            SessionRetention {
                maximum_age: Duration::ZERO,
                maximum_count: 10,
                maximum_bytes: u64::MAX,
            },
        )
        .expect("retention");

        assert!(!old.exists());
        assert!(running.join("audio-001.wav").is_file());
        #[cfg(unix)]
        assert!(running.join("loop").symlink_metadata().is_ok());
        assert!(unknown.join("keep.txt").is_file());
        assert!(malformed.join("keep.txt").is_file());
        fs::remove_dir_all(root).expect("remove retention root");
    }

    #[test]
    fn crud_mutations_publish_stable_ids_and_undo_the_last_change() {
        let original = Project::demo();
        let session =
            EditSession::create(&original, "shape the bass", 4.0, 8.0).expect("edit session");
        let response = apply_agent_mutation(
            session.path(),
            "new_track",
            &serde_json::json!({"description":"Snare Build","color":"#ff91ad"}),
        )
        .expect("new track");
        let response: JsonValue = serde_json::from_str(&response).unwrap();
        let track_id = response["id"].as_u64().expect("created track ID");
        let (plan, project) = session.take_update().unwrap().expect("published update");
        assert_eq!(plan.action, Action::GraphMutation);
        let track = project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("created track");
        assert!(track.clips.is_empty());
        assert_eq!(track.role, TrackRole::Neutral);
        assert_eq!(track.name, "Snare Build");
        assert_eq!(track.color, "#ff91ad");
        assert_eq!(track.volume, 1.0);
        assert_eq!(track.instrument.preset, "Init");
        assert!(track.effects.is_empty());
        assert!(track.modulators.is_empty());

        apply_agent_mutation(session.path(), "undo", &serde_json::json!({})).expect("undo");
        let (_, project) = session.take_update().unwrap().expect("published undo");
        assert_eq!(project.tracks.len(), original.tracks.len());
        assert!(!project.tracks.iter().any(|track| track.id == track_id));
    }

    #[test]
    fn existing_initial_track_can_be_named_and_colored() {
        let original = Project::initial();
        let track_id = original.tracks[0].id;
        let session =
            EditSession::create(&original, "make a bass line", 0.0, 4.0).expect("edit session");

        apply_agent_mutation(
            session.path(),
            "set_track_identity",
            &serde_json::json!({"trackId":track_id,"name":"Bass","color":"#8ca9ff"}),
        )
        .expect("set initial track identity");
        let (_, project) = session.take_update().unwrap().expect("identity update");
        assert_eq!(project.tracks.len(), 1);
        assert_eq!(project.tracks[0].name, "Bass");
        assert_eq!(project.tracks[0].color, "#8ca9ff");

        apply_agent_mutation(session.path(), "undo", &serde_json::json!({})).expect("undo");
        let (_, project) = session.take_update().unwrap().expect("undo update");
        assert_eq!(project.tracks[0].name, "Empty Track");
        assert_eq!(project.tracks[0].color, original.tracks[0].color);
    }

    #[test]
    fn track_identity_rejects_invalid_names_and_colors_with_actionable_errors() {
        let original = Project::initial();
        let track_id = original.tracks[0].id;
        let session =
            EditSession::create(&original, "make a bass line", 0.0, 4.0).expect("edit session");

        let error = apply_agent_mutation(
            session.path(),
            "set_track_identity",
            &serde_json::json!({"trackId":track_id,"name":" ","color":"#8ca9ff"}),
        )
        .expect_err("blank name");
        assert_eq!(error, "name must be a nonempty string");
        assert!(session.take_update().unwrap().is_none());

        let error = apply_agent_mutation(
            session.path(),
            "set_track_identity",
            &serde_json::json!({
                "trackId":track_id,
                "name":"This name is much too long",
                "color":"#8ca9ff"
            }),
        )
        .expect_err("overlong name");
        assert_eq!(error, "name must contain between 1 and 16 characters");
        assert!(session.take_update().unwrap().is_none());

        let error = apply_agent_mutation(
            session.path(),
            "set_track_identity",
            &serde_json::json!({"trackId":track_id,"name":"Bass","color":"#000000"}),
        )
        .expect_err("unknown color");
        assert_eq!(
            error,
            "color must be chosen from the set_track_identity palette"
        );
        assert!(session.take_update().unwrap().is_none());

        let graph = ProjectStore::open(session.path().join(GRAPH_FILE))
            .expect("sound graph")
            .1;
        assert_eq!(graph.project().to_json(), original.to_json());
    }

    #[test]
    fn midi_pitches_are_not_constrained_by_track_opinions() {
        let original = Project::initial();
        let session = EditSession::create(&original, "add notes", 0.0, 4.0).expect("edit session");
        let response = apply_agent_mutation(
            session.path(),
            "new_track",
            &serde_json::json!({"description":"Percussion","color":"#67d5e8"}),
        )
        .expect("new track");
        let response: JsonValue = serde_json::from_str(&response).unwrap();
        let track_id = response["id"].as_u64().expect("created track ID");
        session.take_update().unwrap().expect("published update");

        apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":track_id,
                "label":"Multiple pitches",
                "startBeat":0,
                "durationBeats":4,
                "playback":{"mode":"loop","lengthBeats":4},
                "events":[
                    {"time":0,"duration":0.125,"pitch":42,"velocity":0.8},
                    {"time":1,"duration":0.125,"pitch":36,"velocity":0.9}
                ]
            }),
        )
        .expect("multiple pitches are valid");
    }

    #[test]
    fn factory_presets_can_be_browsed_and_loaded_by_stable_id() {
        let root: JsonValue =
            serde_json::from_str(&list_surge_presets(&serde_json::json!({})).expect("preset root"))
                .expect("root JSON");
        assert!(root["total"].as_u64().unwrap() > 100);
        assert_eq!(root["path"], "Factory");
        let pads = root["folders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|folder| folder["path"] == "Factory/Pads")
            .expect("Pads folder");
        assert!(pads["presetCount"].as_u64().unwrap() > 10);
        assert!(pads.get("suggestedRoles").is_none());
        assert!(pads.get("description").is_none());

        let catalog: JsonValue = serde_json::from_str(
            &list_surge_presets(&serde_json::json!({"path":"Factory/Leads"}))
                .expect("Leads catalog"),
        )
        .expect("catalog JSON");
        assert_eq!(catalog["parent"], "Factory");
        assert!(
            catalog["presets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|preset| preset["id"] == "Factory/Leads/Scream Lead")
        );

        let preset_with_effects = crate::surge_presets::catalog()
            .into_iter()
            .find(|preset| {
                crate::surge::preset_effects(&preset.id).is_ok_and(|effects| !effects.is_empty())
            })
            .expect("factory preset with embedded effects");
        let session =
            EditSession::create(&Project::demo(), "change the patch", 0.0, 2.0).expect("session");
        let added_effect_ids = Project::demo().tracks[2]
            .effects
            .iter()
            .map(|effect| effect.id)
            .collect::<Vec<_>>();
        let response = apply_agent_mutation(
            session.path(),
            "set_surge_preset",
            &serde_json::json!({
                "trackId":3,
                "presetId":preset_with_effects.id
            }),
        )
        .expect("factory preset mutation");
        let response: JsonValue = serde_json::from_str(&response).expect("mutation JSON");
        assert!(response["midiContext"]["sceneMode"].is_string());
        assert!(response["midiContext"].get("recommendedRange").is_none());
        let (_, project) = session.take_update().unwrap().expect("published update");
        assert_eq!(project.tracks[2].instrument.preset, preset_with_effects.id);
        assert!(
            !project.tracks[2].effects.is_empty()
                && project.tracks[2]
                    .effects
                    .iter()
                    .any(|effect| effect.preset_slot.is_some()),
            "factory effects must be visible as preset-sourced graph effects"
        );
        assert!(added_effect_ids.iter().all(|id| {
            project.tracks[2]
                .effects
                .iter()
                .any(|effect| effect.id == *id && effect.preset_slot.is_none())
        }));
        assert_eq!(
            project.tracks[2].routing.effect_order.len(),
            project.tracks[2].effects.len()
        );

        apply_agent_mutation(
            session.path(),
            "set_surge_preset",
            &serde_json::json!({
                "trackId":3,
                "presetId":"Factory/Polysynths/Anthemish 1"
            }),
        )
        .expect("preset with non-deactivatable inactive effect rate");
        let (_, project) = session.take_update().unwrap().expect("preset update");
        crate::project_file::parse_project(&project.to_json())
            .expect("preset effect state remains persistable");
    }

    #[test]
    fn midi_tools_support_repeating_patterns_and_long_once_phrases() {
        let mut studio = Studio::from_project(Project::demo());
        studio.set_tempo(120).expect("tempo");
        let session =
            EditSession::create(studio.project(), "write a melody", 0.0, 16.0).expect("session");
        let events = (0..64)
            .map(|index| {
                serde_json::json!({
                    "time":index as f32 / 2.0,
                    "duration":0.25,
                    "pitch":60 + index % 12,
                    "velocity":0.8
                })
            })
            .collect::<Vec<_>>();
        apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":3,
                "label":"Sixteen-bar melody",
                "startBeat":0,
                "durationBeats":32,
                "playback":{"mode":"once"},
                "events":events
            }),
        )
        .expect("once phrase");
        let (_, project) = session.take_update().unwrap().expect("phrase update");
        let phrase = project.tracks[2].clips.last().expect("phrase clip");
        assert_eq!(phrase.playback_mode, "once");
        assert_eq!(phrase.loop_beats, 32.0);
        assert_eq!((phrase.start, phrase.end), (0.0, 16.0));
        assert_eq!(phrase.events.len(), 64);

        let loop_events = (0..33)
            .map(|index| {
                serde_json::json!({
                    "time":index as f32 / 16.0,
                    "duration":0.0625,
                    "pitch":36,
                    "velocity":0.7
                })
            })
            .collect::<Vec<_>>();
        apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":1,
                "label":"Oversized loop",
                "startBeat":0,
                "durationBeats":4,
                "playback":{"mode":"loop","lengthBeats":4},
                "events":loop_events
            }),
        )
        .expect("dense loop");
        let (_, project) = session.take_update().unwrap().expect("loop update");
        assert_eq!(project.tracks[0].clips.last().unwrap().events.len(), 33);
    }

    #[test]
    fn midi_tools_accept_schema_length_and_report_invalid_lengths_directly() {
        let mut studio = Studio::from_project(Project::demo());
        studio.set_tempo(128).expect("tempo");
        let session = EditSession::create(studio.project(), "write a long arrangement", 0.0, 32.0)
            .expect("session");
        apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":3,
                "label":"Long phrase",
                "startBeat":0,
                "durationBeats":66,
                "playback":{"mode":"once"},
                "events":[{"time":65,"duration":0.5,"pitch":72,"velocity":0.8}]
            }),
        )
        .expect("66-beat phrase accepted everywhere");
        let (_, project) = session.take_update().unwrap().expect("phrase update");
        assert_eq!(project.tracks[2].clips.last().unwrap().loop_beats, 66.0);

        let error = apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":3,
                "label":"Too long",
                "startBeat":0,
                "durationBeats":257,
                "playback":{"mode":"once"},
                "events":[]
            }),
        )
        .expect_err("oversized phrase");
        assert_eq!(error, "durationBeats must be between 0.25 and 256");

        let error = apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":3,
                "label":"Too long loop",
                "startBeat":0,
                "durationBeats":32,
                "playback":{"mode":"loop","lengthBeats":17},
                "events":[]
            }),
        )
        .expect_err("oversized loop");
        assert_eq!(
            error,
            "loop playback length must be between 0.25 and 16 beats"
        );
    }

    #[test]
    fn midi_tools_round_trip_1024_densely_spaced_notes() {
        let original = Project::initial();
        let session =
            EditSession::create(&original, "write a dense roll", 0.0, 4.0).expect("session");
        let events = (0..MAX_MIDI_EVENTS_PER_CLIP)
            .map(|index| {
                serde_json::json!({
                    "time":index as f32 / 256.0,
                    "duration":MIN_MIDI_NOTE_BEATS,
                    "pitch":60,
                    "velocity":0.8
                })
            })
            .collect::<Vec<_>>();
        apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":1,
                "label":"Dense roll",
                "startBeat":0,
                "durationBeats":4,
                "playback":{"mode":"once"},
                "events":events
            }),
        )
        .expect("dense roll");
        let (_, project) = session.take_update().unwrap().expect("dense update");
        let clip = project.tracks[0].clips.last().expect("dense clip");
        assert_eq!(clip.events.len(), MAX_MIDI_EVENTS_PER_CLIP);
        assert!(
            clip.events
                .iter()
                .all(|event| event.duration == MIN_MIDI_NOTE_BEATS)
        );
        Project::from_json(&project.to_json()).expect("dense project round trip");
    }

    #[test]
    fn failed_progress_publication_rolls_back_graph_and_undo_snapshot() {
        let original = Project::demo();
        let session = EditSession::create(&original, "change tempo", 0.0, 4.0).expect("session");
        let mut prior_undo = Studio::from_project(original.clone());
        prior_undo.set_tempo(90).expect("prior undo state");
        write_replace(
            &session.path().join(UNDO_GRAPH_FILE),
            &prior_undo.project().to_json(),
        )
        .expect("prior undo snapshot");
        let undo_before = fs::read_to_string(session.path().join(UNDO_GRAPH_FILE)).expect("undo");
        fs::create_dir(session.path().join(PENDING_PROGRESS_DIRECTORY))
            .expect("blocked progress handoff");

        let error =
            apply_agent_mutation(session.path(), "set_tempo", &serde_json::json!({"bpm":130}))
                .expect_err("progress publication failure");

        assert!(error.contains("could not prepare Gemini edit progress"));
        let restored = ProjectStore::open(session.path().join(GRAPH_FILE))
            .expect("restored graph")
            .1;
        assert_eq!(restored.project().to_json(), original.to_json());
        assert_eq!(
            fs::read_to_string(session.path().join(UNDO_GRAPH_FILE)).expect("restored undo"),
            undo_before
        );

        fs::create_dir(session.path().join(PENDING_PROGRESS_DIRECTORY))
            .expect("blocked undo handoff");
        let error = apply_agent_mutation(session.path(), "undo", &serde_json::json!({}))
            .expect_err("undo publication failure");
        assert!(error.contains("could not prepare Gemini edit progress"));
        let restored = ProjectStore::open(session.path().join(GRAPH_FILE))
            .expect("graph after failed undo")
            .1;
        assert_eq!(restored.project().to_json(), original.to_json());
        assert_eq!(
            fs::read_to_string(session.path().join(UNDO_GRAPH_FILE)).expect("undo after failure"),
            undo_before
        );
    }

    #[test]
    fn committed_graph_metadata_is_synchronized_before_the_next_mutation() {
        let session =
            EditSession::create(&Project::demo(), "two edits", 0.0, 8.0).expect("edit session");
        apply_agent_mutation(session.path(), "set_tempo", &serde_json::json!({"bpm":120}))
            .expect("first mutation");
        let (plan, submitted) = session.take_update().unwrap().expect("first update");

        let mut live = Studio::from_project(Project::demo());
        live.replace_graph(submitted, 0.0, 8.0, "two edits", plan)
            .expect("server commit metadata");
        session
            .synchronize_project(live.project())
            .expect("canonical synchronization");

        apply_agent_mutation(
            session.path(),
            "update_midi_clip",
            &serde_json::json!({
                "trackId":1,"clipId":11,"label":"Updated drums","startBeat":0,
                "durationBeats":16,"playback":{"mode":"loop","lengthBeats":4},"events":[
                    {"time":0,"duration":0.25,"pitch":36,"velocity":0.9}
                ]
            }),
        )
        .expect("second mutation after synchronization");
        let (_, submitted) = session.take_update().unwrap().expect("second update");
        live.replace_graph(
            submitted,
            0.0,
            8.0,
            "two edits",
            EditPlan {
                action: Action::GraphMutation,
                summary: "Updated drums".to_owned(),
            },
        )
        .expect("second server commit has no ID collision");
        Project::from_json(&live.project().to_json()).expect("committed graph validates");
        let clips = &live.project().tracks[0].clips;
        assert_eq!((clips[0].start, clips[0].end), (0.0, 8.0));
        assert_eq!((clips[1].start, clips[1].end), (8.0, 32.0));

        let error = apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":1,"label":"Outside selection","startBeat":16,
                "durationBeats":8,"playback":{"mode":"loop","lengthBeats":4},"events":[]
            }),
        )
        .expect_err("MIDI outside the selected region");
        assert!(error.contains("selected region"));

        apply_agent_mutation(
            session.path(),
            "delete_midi_clip",
            &serde_json::json!({"trackId":1,"clipId":11}),
        )
        .expect("selection-scoped MIDI deletion");
        let (_, deleted) = session.take_update().unwrap().expect("delete update");
        assert!(deleted.tracks[0].clips.iter().all(|clip| clip.start >= 8.0));
    }

    #[test]
    fn track_mix_is_explicit_reversible_and_effect_delete_is_physical() {
        let session =
            EditSession::create(&Project::demo(), "edit safely", 0.0, 4.0).expect("edit session");
        apply_agent_mutation(
            session.path(),
            "set_track_volume",
            &serde_json::json!({"trackId":2,"volume":1.25}),
        )
        .expect("volume");
        let (_, louder) = session.take_update().unwrap().expect("volume update");
        assert_eq!(louder.tracks[1].volume, 1.25);

        let error = apply_agent_mutation(
            session.path(),
            "set_track_volume",
            &serde_json::json!({"trackId":2,"volume":1.51}),
        )
        .expect_err("out-of-range volume");
        assert_eq!(error, "mixer value out of range");
        assert!(session.take_update().unwrap().is_none());

        apply_agent_mutation(
            session.path(),
            "set_track_mute",
            &serde_json::json!({"trackId":2,"muted":true}),
        )
        .expect("mute");
        let (_, muted) = session.take_update().unwrap().expect("mute update");
        assert!(muted.tracks[1].muted);

        apply_agent_mutation(
            session.path(),
            "set_track_mute",
            &serde_json::json!({"trackId":2,"muted":false}),
        )
        .expect("unmute");
        let (_, unmuted) = session.take_update().unwrap().expect("unmute update");
        assert!(!unmuted.tracks[1].muted);

        let response = apply_agent_mutation(
            session.path(),
            "add_effect",
            &serde_json::json!({"trackId":2,"name":"Distortion","mix":0.5}),
        )
        .expect("add effect");
        let effect_id = serde_json::from_str::<JsonValue>(&response).unwrap()["id"]
            .as_u64()
            .unwrap();
        session.take_update().unwrap().expect("effect update");
        apply_agent_mutation(
            session.path(),
            "delete_effect",
            &serde_json::json!({"trackId":2,"effectId":effect_id}),
        )
        .expect("delete effect");
        let (_, deleted) = session.take_update().unwrap().expect("delete update");
        assert!(
            deleted.tracks[1]
                .effects
                .iter()
                .all(|effect| effect.id != effect_id)
        );
        assert!(!deleted.tracks[1].routing.effect_order.contains(&effect_id));
    }

    #[test]
    fn audio_render_validates_stable_channel_ids() {
        let session =
            EditSession::create(&Project::demo(), "listen", 0.0, 2.0).expect("edit session");
        let error = render_audio(
            session.path(),
            &serde_json::json!({"tracks": [999], "start": 0, "end": 1}),
        )
        .expect_err("unknown channel");
        assert!(error.contains("available track IDs"));
        assert!(error.contains("available track IDs: 1"));
    }

    #[test]
    fn audio_render_defaults_to_all_tracks_and_accepts_explicit_all() {
        let session =
            EditSession::create(&Project::demo(), "listen", 0.0, 2.0).expect("edit session");
        let omitted =
            prepare_audio_render(session.path(), &serde_json::json!({"start": 0, "end": 1}))
                .expect("default all-track render");
        let explicit = prepare_audio_render(
            session.path(),
            &serde_json::json!({"tracks": "all", "start": 0, "end": 1}),
        )
        .expect("explicit all-track render");
        let expected = Project::demo()
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>();

        assert_eq!(omitted.track_ids, expected);
        assert_eq!(explicit.track_ids, expected);
    }

    #[test]
    fn unsafe_headless_effects_are_not_exposed_or_mutable() {
        let declarations = tool_declarations(true);
        let add_effect = declarations
            .iter()
            .find(|tool| tool["name"] == "add_effect")
            .expect("effect declaration");
        let names = add_effect["parameters"]["properties"]["name"]["enum"]
            .as_array()
            .expect("effect names");
        for unsafe_name in ["Audio Input", "Spring Reverb", "Tape", "Vocoder"] {
            assert!(
                !names.iter().any(|name| name == unsafe_name),
                "{unsafe_name} must not be exposed"
            );
            let mut studio = Studio::new();
            let track_id = studio.project().tracks[0].id;
            assert_eq!(
                studio.create_effect(track_id, unsafe_name, 0.5),
                Err(StudioError::InvalidSoundTool)
            );
        }
    }

    #[test]
    fn feedback_render_rejects_pathological_native_audio() {
        let project = project_with_effect("Tape");
        let request = AudioRenderRequest {
            track_ids: vec![project.tracks[0].id],
            project,
            start: 0.0,
            end: 1.0,
            description: "pathological Tape render".to_owned(),
        };
        let error = render_audio_request(request).expect_err("Tape output must be rejected");
        assert!(
            error.contains("pathological"),
            "unexpected render error: {error}"
        );

        assert!(validate_feedback_samples(&[0.1, -0.1], 1).is_ok());
        assert!(
            validate_feedback_samples(&[f32::NAN], 0)
                .unwrap_err()
                .contains("non-finite")
        );
        assert!(
            validate_feedback_samples(&[5.0, -5.0], 0)
                .unwrap_err()
                .contains("peak")
        );
        assert!(
            validate_feedback_samples(&[0.5, 0.5], 0)
                .unwrap_err()
                .contains("DC offset")
        );
        assert!(
            validate_feedback_samples(&[0.0, 0.0], 1)
                .unwrap_err()
                .contains("silence")
        );
    }

    #[test]
    fn audio_analysis_has_exactly_ten_standard_metrics_per_mix_and_track() {
        let session =
            EditSession::create(&Project::demo(), "listen", 0.0, 2.0).expect("edit session");
        let arguments = serde_json::json!({"tracks":[2],"start":0,"end":0.1});
        let surge = render_audio_request(
            prepare_audio_render(session.path(), &arguments).expect("Surge request"),
        )
        .expect("Surge render");

        assert!(surge.description.contains("Surge XT rendering engine"));
        assert!(!surge.description.contains("custom Rust audio engine"));
        assert_eq!(surge.measurements["sampleRateHz"], 16_000);
        assert_eq!(surge.measurements["channelCount"], 2);
        assert_eq!(surge.measurements["startSeconds"], 0.0);
        assert_eq!(surge.measurements["endSeconds"], 0.1);
        assert_eq!(
            surge.measurements["tracks"]
                .as_array()
                .expect("per-track measurements")
                .len(),
            1
        );
        assert_eq!(surge.measurements["tracks"][0]["trackId"], 2);
        let expected = [
            "clippedSampleCount",
            "crestFactorDb",
            "dcOffset",
            "highBandEnergyRatio",
            "lowBandEnergyRatio",
            "midBandEnergyRatio",
            "peakDbfs",
            "rmsDbfs",
            "spectralCentroidHz",
            "zeroCrossingRate",
        ];
        for measurements in [
            &surge.measurements["mix"],
            &surge.measurements["tracks"][0]["measurements"],
        ] {
            let object = measurements.as_object().expect("measurement object");
            assert_eq!(object.len(), 10);
            assert!(expected.iter().all(|name| object.contains_key(*name)));
            assert!(
                object
                    .values()
                    .all(|value| value.is_number() || value.is_null())
            );
        }
    }

    #[test]
    fn audio_render_range_is_independent_of_the_edit_selection() {
        let session = EditSession::create(&Project::demo(), "listen in context", 8.0, 12.0)
            .expect("edit session");
        let request = prepare_audio_render(
            session.path(),
            &serde_json::json!({"tracks": [1, 2, 3], "start": 2, "end": 7}),
        )
        .expect("context render outside selection");

        assert_eq!(request.start, 2.0);
        assert_eq!(request.end, 7.0);
        assert!(request.description.contains("2.000 to 7.000 seconds"));
    }
}
