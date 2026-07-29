use std::collections::BTreeMap;
use std::fmt::{self, Write};

use serde::Serialize;

const TRACK_LIMIT: usize = 128;
pub(crate) const EDIT_LOG_LIMIT: usize = 256;
pub(crate) const MAX_PROMPT_CHARACTERS: usize = 2_000;
pub(crate) const PROJECT_SCHEMA_VERSION: u64 = 5;
pub(crate) const MAX_MIDI_EVENTS_PER_CLIP: usize = 1_024;
pub(crate) const MIN_MIDI_NOTE_BEATS: f32 = 0.0625;
pub(crate) const MAX_LOOP_PLAYBACK_BEATS: f32 = 16.0;
pub(crate) const MAX_ONCE_PLAYBACK_BEATS: f32 = 256.0;
pub(crate) const MAX_MIDI_NOTE_DURATION_BEATS: f32 = 16.0;
pub(crate) const SURGE_ENGINE: &str = "Surge XT";
pub(crate) const SURGE_PRESETS: &[&str] = &["Init"];
pub(crate) const TRACK_COLOR_PALETTE: &[&str] = &[
    "#ffb86b", "#74e0bc", "#8ca9ff", "#d99cff", "#ff91ad", "#ffd166", "#67d5e8", "#ff6b6b",
];

pub(crate) fn valid_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

struct ModulationTarget {
    id: String,
    label: String,
    minimum: f32,
    maximum: f32,
    scale: f32,
    mode: &'static str,
}

#[derive(Clone, Debug)]
pub struct Effect {
    pub id: u64,
    pub name: String,
    pub preset_slot: Option<usize>,
    pub mix: f32,
    pub enabled: bool,
    pub parameters: BTreeMap<String, f32>,
    pub parameter_overrides: Vec<String>,
    pub tempo_sync_parameters: Vec<String>,
    pub deactivated_parameters: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Instrument {
    pub id: u64,
    pub engine: String,
    pub preset: String,
    pub native_overrides: BTreeMap<i32, f32>,
}

#[derive(Clone, Debug)]
pub struct ClipEvent {
    pub id: u64,
    pub kind: String,
    pub time: f32,
    pub duration: f32,
    pub pitch: u8,
    pub velocity: f32,
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub id: u64,
    pub label: String,
    pub start: f32,
    pub end: f32,
    pub source_start: f32,
    pub style: String,
    pub playback_mode: String,
    pub loop_beats: f32,
    pub events: Vec<ClipEvent>,
}

pub(crate) struct ModulatorSpec<'a> {
    pub target: &'a str,
    pub shape: &'a str,
    pub rate: f32,
    pub rate_mode: &'a str,
    pub depth: f32,
    pub trigger: &'a str,
    pub source_track_id: Option<u64>,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub threshold: f32,
    pub polarity: &'a str,
    pub formula: &'a str,
}

#[derive(Clone, Debug)]
pub struct Modulator {
    pub id: u64,
    pub name: String,
    pub shape: String,
    pub rate: f32,
    pub rate_mode: String,
    pub trigger: String,
    pub source_track_id: Option<u64>,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub threshold: f32,
    pub polarity: String,
    pub formula: String,
    pub depth: f32,
    pub target: String,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Routing {
    pub effect_order: Vec<u64>,
    pub output: String,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub name: String,
    pub color: String,
    pub volume: f32,
    pub muted: bool,
    pub instrument: Instrument,
    pub effects: Vec<Effect>,
    pub modulators: Vec<Modulator>,
    pub routing: Routing,
    pub clips: Vec<Clip>,
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub id: u64,
    pub start: f32,
    pub end: f32,
    pub prompt: String,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub struct EditOperation {
    pub operation_id: String,
    pub source: String,
    pub status: EditOperationStatus,
    pub applied_steps: usize,
    pub initial_version: u64,
    pub project_version: u64,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditOperationStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl EditOperationStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed_with_changes",
            Self::Interrupted => "interrupted_with_changes",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Project {
    pub name: String,
    pub bpm: u16,
    pub duration: f32,
    pub version: u64,
    pub tracks: Vec<Track>,
    pub edits: Vec<Edit>,
    pub edit_operations: Vec<EditOperation>,
}

pub(crate) struct MidiClipSpec {
    pub(crate) label: String,
    pub(crate) start: f32,
    pub(crate) end: f32,
    pub(crate) playback_mode: String,
    pub(crate) loop_beats: f32,
    pub(crate) notes: Vec<MidiNote>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MidiNote {
    pub(crate) time: f32,
    pub(crate) duration: f32,
    pub(crate) pitch: u8,
    pub(crate) velocity: f32,
}

#[derive(Debug, PartialEq)]
pub struct ProjectFileError(String);

impl ProjectFileError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ProjectFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Project {
    pub(crate) fn initial() -> Self {
        Self {
            name: "Untitled Project".to_owned(),
            bpm: 120,
            duration: 32.0,
            version: 1,
            tracks: vec![empty_track(1)],
            edits: Vec::new(),
            edit_operations: Vec::new(),
        }
    }

    #[must_use]
    pub fn demo() -> Self {
        Self {
            name: "Neon First Light".to_owned(),
            bpm: 112,
            duration: 32.0,
            version: 1,
            tracks: vec![
                demo_track(1, DemoPart::Drums, "Pulse Kit", "#ffb86b"),
                demo_track(2, DemoPart::Bass, "Soft Current", "#74e0bc"),
                demo_track(3, DemoPart::Chords, "Glass Chords", "#8ca9ff"),
            ],
            edits: Vec::new(),
            edit_operations: Vec::new(),
        }
    }

    pub fn from_json(source: &str) -> Result<Self, ProjectFileError> {
        crate::project_file::parse_project(source)
    }

    fn highest_id(&self) -> u64 {
        let mut highest = self.edits.iter().map(|edit| edit.id).max().unwrap_or(0);
        for track in &self.tracks {
            highest = highest.max(track.id).max(track.instrument.id);
            for effect in &track.effects {
                highest = highest.max(effect.id);
            }
            for modulator in &track.modulators {
                highest = highest.max(modulator.id);
            }
            for clip in &track.clips {
                highest = highest.max(clip.id);
                for event in &clip.events {
                    highest = highest.max(event.id);
                }
            }
        }
        highest
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        let mut output = String::with_capacity(4096);
        self.write_graph_json(&mut output);
        output.push_str(",\"edits\":[");
        for (index, edit) in self.edits.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            edit.write_json(&mut output);
        }
        output.push_str("],\"editOperations\":[");
        for (index, operation) in self.edit_operations.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            operation.write_json(&mut output);
        }
        output.push_str("]}");
        output
    }

    fn compact_edit_log(&mut self) {
        if self.edits.len() > EDIT_LOG_LIMIT {
            self.edits.drain(..self.edits.len() - EDIT_LOG_LIMIT);
        }
        if self.edit_operations.len() > EDIT_LOG_LIMIT {
            self.edit_operations
                .drain(..self.edit_operations.len() - EDIT_LOG_LIMIT);
        }
    }

    fn write_graph_json(&self, output: &mut String) {
        write!(
            output,
            "{{\"schemaVersion\":{},\"name\":{},\"bpm\":{},\"duration\":{},\"version\":{},\"tracks\":[",
            PROJECT_SCHEMA_VERSION,
            json_string(&self.name),
            self.bpm,
            decimal(self.duration),
            self.version
        )
        .expect("writing to a string cannot fail");

        for (index, track) in self.tracks.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            track.write_json(output);
        }
        output.push(']');
    }
}

impl Track {
    fn write_json(&self, output: &mut String) {
        write!(
            output,
            concat!(
                "{{\"id\":{},\"name\":{},\"color\":{},",
                "\"volume\":{},\"muted\":{},\"instrument\":{{",
                "\"id\":{},\"type\":\"instrument\",\"engine\":{},\"preset\":{},",
                "\"nativeOverrides\":{{"
            ),
            self.id,
            json_string(&self.name),
            json_string(&self.color),
            decimal(self.volume),
            self.muted,
            self.instrument.id,
            json_string(&self.instrument.engine),
            json_string(&self.instrument.preset)
        )
        .expect("writing to a string cannot fail");
        for (index, (parameter, value)) in self.instrument.native_overrides.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                "{}:{}",
                json_string(&parameter.to_string()),
                decimal(*value)
            )
            .expect("writing to a string cannot fail");
        }
        output.push('}');
        output.push_str("},\"effects\":[");

        for (index, effect) in self.effects.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            let semantics = crate::surge::effect_control_semantics(&effect.name);
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"type\":\"effect\",\"name\":{},",
                    "\"source\":{},\"enabled\":{},\"parameters\":{{\"mix\":{}"
                ),
                effect.id,
                json_string(&effect.name),
                json_string(if effect.preset_slot.is_some() {
                    "preset"
                } else {
                    "added"
                }),
                effect.enabled,
                decimal(effect.mix)
            )
            .expect("writing to a string cannot fail");
            for (name, value) in &effect.parameters {
                write!(output, ",{}:{}", json_string(name), decimal(*value))
                    .expect("writing to a string cannot fail");
            }
            output.push_str("},\"parameterOrder\":[");
            for (index, parameter) in effect.parameters.keys().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&json_string(parameter));
            }
            output.push(']');
            output.push_str(",\"parameterSemantics\":{");
            let mut semantic_index = 0;
            for parameter in effect.parameters.keys() {
                let Some(semantics) = semantics.get(parameter) else {
                    continue;
                };
                if semantic_index > 0 {
                    output.push(',');
                }
                semantic_index += 1;
                write!(
                    output,
                    "{}:{{\"kind\":{},\"choices\":[",
                    json_string(parameter),
                    json_string(if semantics.boolean {
                        "boolean"
                    } else if semantics.discrete || !semantics.choices.is_empty() {
                        "selection"
                    } else {
                        "continuous"
                    })
                )
                .expect("writing to a string cannot fail");
                for (choice_index, (value, display)) in semantics.choices.iter().enumerate() {
                    if choice_index > 0 {
                        output.push(',');
                    }
                    write!(
                        output,
                        "{{\"value\":{},\"display\":{}}}",
                        decimal(*value),
                        json_string(display)
                    )
                    .expect("writing to a string cannot fail");
                }
                output.push_str("]}");
            }
            output.push('}');
            if let Some(slot) = effect.preset_slot {
                write!(output, ",\"presetSlot\":{}", slot + 1)
                    .expect("writing to a string cannot fail");
            }
            output.push_str(",\"overrides\":[");
            for (index, parameter) in effect.parameter_overrides.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&json_string(parameter));
            }
            output.push_str("],\"tempoSync\":[");
            for (index, parameter) in effect.tempo_sync_parameters.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&json_string(parameter));
            }
            output.push_str("],\"deactivated\":[");
            for (index, parameter) in effect.deactivated_parameters.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&json_string(parameter));
            }
            output.push_str("]}");
        }

        output.push_str("],\"modulators\":[");
        for (index, modulator) in self.modulators.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"type\":\"modulator\",\"name\":{},",
                    "\"shape\":{},\"enabled\":{},\"target\":{},\"rateMode\":{},",
                    "\"trigger\":{},\"sourceTrackId\":{},\"polarity\":{},",
                    "\"formula\":{},",
                    "\"parameters\":{{\"rate\":{},\"depth\":{},\"attackMs\":{},",
                    "\"releaseMs\":{},\"threshold\":{}}}}}"
                ),
                modulator.id,
                json_string(&modulator.name),
                json_string(&modulator.shape),
                modulator.enabled,
                json_string(&modulator.target),
                json_string(&modulator.rate_mode),
                json_string(&modulator.trigger),
                modulator
                    .source_track_id
                    .map_or_else(|| "null".to_owned(), |id| id.to_string()),
                json_string(&modulator.polarity),
                json_string(&modulator.formula),
                decimal(modulator.rate),
                decimal(modulator.depth),
                decimal(modulator.attack_ms),
                decimal(modulator.release_ms),
                decimal(modulator.threshold)
            )
            .expect("writing to a string cannot fail");
        }

        output
            .push_str("],\"modulationTargetIdType\":\"modulationTarget\",\"modulationTargets\":[");
        for (index, target) in modulation_targets(self).iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"label\":{},\"minimum\":{},",
                    "\"maximum\":{},\"scale\":{},\"mode\":{}}}"
                ),
                json_string(&target.id),
                json_string(&target.label),
                decimal(target.minimum),
                decimal(target.maximum),
                decimal(target.scale),
                json_string(target.mode)
            )
            .expect("writing to a string cannot fail");
        }

        output.push_str("],\"routing\":{\"audio\":[");
        write!(
            output,
            "{},{}",
            json_string("clips"),
            json_string(&format!("instrument:{}", self.instrument.id))
        )
        .expect("writing to a string cannot fail");
        for effect_id in &self.routing.effect_order {
            write!(output, ",{}", json_string(&format!("effect:{effect_id}")))
                .expect("writing to a string cannot fail");
        }
        write!(
            output,
            ",{}],\"control\":[",
            json_string(&self.routing.output)
        )
        .expect("writing to a string cannot fail");
        for (index, modulator) in self
            .modulators
            .iter()
            .filter(|modulator| modulator.enabled)
            .enumerate()
        {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                "{{\"source\":{},\"target\":{}}}",
                json_string(&format!("modulator:{}", modulator.id)),
                json_string(&modulator.target)
            )
            .expect("writing to a string cannot fail");
        }
        output.push_str("],\"output\":");
        output.push_str(&json_string(&self.routing.output));
        output.push_str(",\"edges\":[");
        let instrument = format!("instrument:{}", self.instrument.id);
        write_signal_edge(output, false, "clips", &instrument, "midi");
        for modulator in self
            .modulators
            .iter()
            .filter(|modulator| modulator.enabled && modulator.trigger != "free")
        {
            let source = match modulator.trigger.as_str() {
                "audio" => format!(
                    "track:{}:output",
                    modulator.source_track_id.unwrap_or(self.id)
                ),
                _ if modulator.source_track_id.is_some_and(|id| id != self.id) => {
                    format!("track:{}:clips", modulator.source_track_id.unwrap())
                }
                _ => "clips".to_owned(),
            };
            write_signal_edge(
                output,
                true,
                &source,
                &format!("modulator:{}", modulator.id),
                if modulator.trigger == "audio" {
                    "audio"
                } else {
                    "midi"
                },
            );
        }
        let mut audio_source = instrument;
        for effect_id in &self.routing.effect_order {
            let effect = format!("effect:{effect_id}");
            write_signal_edge(output, true, &audio_source, &effect, "audio");
            audio_source = effect;
        }
        write_signal_edge(output, true, &audio_source, &self.routing.output, "audio");
        for modulator in self.modulators.iter().filter(|modulator| modulator.enabled) {
            write_signal_edge(
                output,
                true,
                &format!("modulator:{}", modulator.id),
                &modulator.target,
                "control",
            );
        }
        output.push_str("]},\"clips\":[");
        for (index, clip) in self.clips.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"label\":{},\"start\":{},\"end\":{},\"sourceStart\":{},",
                    "\"style\":{},\"playback\":{{\"mode\":{},\"lengthBeats\":{}}},\"events\":["
                ),
                clip.id,
                json_string(&clip.label),
                decimal(clip.start),
                decimal(clip.end),
                decimal(clip.source_start),
                json_string(&clip.style),
                json_string(&clip.playback_mode),
                decimal(clip.loop_beats)
            )
            .expect("writing to a string cannot fail");
            for (event_index, event) in clip.events.iter().enumerate() {
                if event_index > 0 {
                    output.push(',');
                }
                write!(
                    output,
                    concat!(
                        "{{\"id\":{},\"type\":{},\"time\":{},\"duration\":{},",
                        "\"pitch\":{},\"velocity\":{}}}"
                    ),
                    event.id,
                    json_string(&event.kind),
                    decimal(event.time),
                    decimal(event.duration),
                    event.pitch,
                    decimal(event.velocity)
                )
                .expect("writing to a string cannot fail");
            }
            output.push_str("]}");
        }
        output.push_str("]}");
    }
}

impl Edit {
    fn write_json(&self, output: &mut String) {
        write!(
            output,
            concat!(
                "{{\"id\":{},\"start\":{},\"end\":{},\"prompt\":{},",
                "\"summary\":{}}}"
            ),
            self.id,
            decimal(self.start),
            decimal(self.end),
            json_string(&self.prompt),
            json_string(&self.summary)
        )
        .expect("writing to a string cannot fail");
    }
}

impl EditOperation {
    fn write_json(&self, output: &mut String) {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct PersistedEditOperation<'a> {
            operation_id: &'a str,
            source: &'a str,
            status: &'a str,
            applied_steps: usize,
            initial_version: u64,
            project_version: u64,
            message: &'a str,
        }

        let persisted = PersistedEditOperation {
            operation_id: &self.operation_id,
            source: &self.source,
            status: self.status.as_str(),
            applied_steps: self.applied_steps,
            initial_version: self.initial_version,
            project_version: self.project_version,
            message: &self.message,
        };
        output.push_str(
            &serde_json::to_string(&persisted).expect("edit operation fields serialize to JSON"),
        );
    }
}

#[derive(Debug, PartialEq)]
pub enum StudioError {
    EmptyPrompt,
    InvalidPrompt,
    InvalidSelection,
    UnknownTrack,
    InvalidMix,
    InvalidDuration,
    InvalidChannel,
    LastTrack,
    UnknownSoundTool,
    InvalidSoundTool,
    EffectCapacity,
}

#[derive(Clone)]
pub struct Studio {
    project: Project,
    next_id: u64,
}

impl Default for Studio {
    fn default() -> Self {
        Self::new()
    }
}

impl Studio {
    #[must_use]
    pub fn new() -> Self {
        let project = Project::demo();
        let next_id = project
            .highest_id()
            .checked_add(1)
            .expect("demo project exhausted the ID namespace");
        Self { project, next_id }
    }

    #[must_use]
    pub fn from_project(mut project: Project) -> Self {
        project.compact_edit_log();
        let next_id = project
            .highest_id()
            .checked_add(1)
            .expect("project exhausted the ID namespace");
        Self { project, next_id }
    }

    #[must_use]
    pub const fn project(&self) -> &Project {
        &self.project
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        self.to_json_with_can_undo(false)
    }

    #[must_use]
    pub(crate) fn to_json_with_can_undo(&self, can_undo: bool) -> String {
        let mut json = self.project.to_json();
        json.pop();
        write!(json, ",\"canUndo\":{can_undo}}}").expect("writing to a string cannot fail");
        json
    }

    pub fn validate_edit(&self, start: f32, end: f32, prompt: &str) -> Result<(), StudioError> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(StudioError::EmptyPrompt);
        }
        if prompt.chars().count() > MAX_PROMPT_CHARACTERS {
            return Err(StudioError::InvalidPrompt);
        }
        if !start.is_finite()
            || !end.is_finite()
            || start < 0.0
            || end <= start
            || end > self.project.duration
        {
            return Err(StudioError::InvalidSelection);
        }
        Ok(())
    }

    pub fn set_duration(&mut self, duration: f32) -> Result<(), StudioError> {
        if !duration.is_finite() || !(1.0..=300.0).contains(&duration) {
            return Err(StudioError::InvalidDuration);
        }
        self.project.duration = duration;
        for track in &mut self.project.tracks {
            track.clips.retain(|clip| clip.start < duration);
            for clip in &mut track.clips {
                clip.end = clip.end.min(duration);
            }
        }
        self.project.edits.retain_mut(|edit| {
            if edit.start >= duration {
                return false;
            }
            edit.end = edit.end.min(duration);
            true
        });
        self.project.version += 1;
        Ok(())
    }

    pub fn replace_graph(
        &mut self,
        mut project: Project,
        start: f32,
        end: f32,
        prompt: &str,
        plan: crate::prompt::EditPlan,
    ) -> Result<String, StudioError> {
        self.validate_edit(start, end, prompt)?;
        project.edits = self.project.edits.clone();
        project.edit_operations = self.project.edit_operations.clone();
        project.version = self.project.version;
        let next_id = project
            .highest_id()
            .checked_add(1)
            .expect("project exhausted the ID namespace");

        let prompt = prompt.trim();
        let summary = plan.summary;
        self.project = project;
        self.next_id = next_id;
        let edit_id = self.take_id();
        self.project.edits.push(Edit {
            id: edit_id,
            start,
            end,
            prompt: prompt.to_owned(),
            summary: summary.clone(),
        });
        self.project.compact_edit_log();
        self.project.version += 1;
        Ok(summary)
    }

    pub(crate) fn record_operation_step(
        &mut self,
        operation_id: &str,
        source: &str,
        message: &str,
    ) -> bool {
        if let Some(operation) = self
            .project
            .edit_operations
            .iter_mut()
            .find(|operation| operation.operation_id == operation_id)
        {
            if operation.status != EditOperationStatus::Running {
                return false;
            }
            operation.applied_steps += 1;
            operation.project_version = self.project.version;
            operation.message = message.to_owned();
        } else {
            self.project.edit_operations.push(EditOperation {
                operation_id: operation_id.to_owned(),
                source: source.to_owned(),
                status: EditOperationStatus::Running,
                applied_steps: 1,
                initial_version: self.project.version.saturating_sub(1),
                project_version: self.project.version,
                message: message.to_owned(),
            });
        }
        self.project.compact_edit_log();
        true
    }

    pub(crate) fn mark_operation_complete(&mut self, operation_id: &str, message: &str) -> bool {
        let Some(index) = self.project.edit_operations.iter().position(|operation| {
            operation.operation_id == operation_id
                && operation.status == EditOperationStatus::Running
        }) else {
            return false;
        };
        self.project.version += 1;
        let operation = &mut self.project.edit_operations[index];
        operation.status = EditOperationStatus::Completed;
        operation.project_version = self.project.version;
        operation.message = message.chars().take(160).collect();
        true
    }

    pub(crate) fn mark_operation_failed(
        &mut self,
        operation_id: &str,
        interrupted: bool,
        message: &str,
    ) -> bool {
        let Some(operation) = self.project.edit_operations.iter_mut().find(|operation| {
            operation.operation_id == operation_id
                && operation.status == EditOperationStatus::Running
        }) else {
            return false;
        };
        operation.status = if interrupted {
            EditOperationStatus::Interrupted
        } else {
            EditOperationStatus::Failed
        };
        operation.message = message.chars().take(160).collect();
        true
    }

    pub fn set_mix(
        &mut self,
        track_id: u64,
        volume: Option<f32>,
        muted: Option<bool>,
    ) -> Result<(), StudioError> {
        if volume.is_none() && muted.is_none() {
            return Err(StudioError::InvalidMix);
        }
        if volume.is_some_and(|value| !value.is_finite() || !(0.0..=1.5).contains(&value)) {
            return Err(StudioError::InvalidMix);
        }
        if !self.project.tracks.iter().any(|track| track.id == track_id) {
            return Err(StudioError::UnknownTrack);
        }
        let track = self
            .project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("track existence was checked");
        if let Some(volume) = volume {
            track.volume = volume;
        }
        if let Some(muted) = muted {
            track.muted = muted;
        }
        self.project.version += 1;
        Ok(())
    }

    pub(crate) fn add_empty_channel(&mut self) -> Result<u64, StudioError> {
        if self.project.tracks.len() >= TRACK_LIMIT {
            return Err(StudioError::InvalidChannel);
        }
        let track_id = self.take_id();
        let mut track = generated_track(track_id);
        track.instrument.id = self.take_id();
        for effect in &mut track.effects {
            effect.id = self.take_id();
        }
        track.routing.effect_order = track.effects.iter().map(|effect| effect.id).collect();
        for modulator in &mut track.modulators {
            modulator.id = self.take_id();
        }
        self.project.tracks.push(track);
        self.project.version += 1;
        Ok(track_id)
    }

    pub(crate) fn add_described_channel(
        &mut self,
        description: &str,
        color: &str,
    ) -> Result<u64, StudioError> {
        let description = description.trim();
        if description.is_empty()
            || description.chars().count() > 16
            || !TRACK_COLOR_PALETTE.contains(&color)
        {
            return Err(StudioError::InvalidChannel);
        }
        let track_id = self.add_empty_channel()?;
        let track = self
            .project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("newly created track exists");
        track.name = description.to_owned();
        track.color = color.to_owned();
        Ok(track_id)
    }

    pub(crate) fn set_track_identity(
        &mut self,
        track_id: u64,
        name: &str,
        color: &str,
    ) -> Result<(), StudioError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 16 || !TRACK_COLOR_PALETTE.contains(&color) {
            return Err(StudioError::InvalidChannel);
        }
        let track_index = self
            .project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        let track = &mut self.project.tracks[track_index];
        track.name = name.to_owned();
        track.color = color.to_owned();
        self.project.version += 1;
        Ok(())
    }

    pub fn delete_channel(&mut self, track_id: u64) -> Result<(), StudioError> {
        let Some(index) = self
            .project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
        else {
            return Err(StudioError::UnknownTrack);
        };
        if self.project.tracks.len() == 1 {
            return Err(StudioError::LastTrack);
        }

        self.project.tracks.remove(index);
        for track in &mut self.project.tracks {
            track
                .modulators
                .retain(|modulator| modulator.source_track_id != Some(track_id));
        }
        self.project.version += 1;
        Ok(())
    }

    pub(crate) fn delete_midi_clip(
        &mut self,
        track_id: u64,
        clip_id: u64,
        selection_start: f32,
        selection_end: f32,
    ) -> Result<(), StudioError> {
        if selection_end <= selection_start {
            return Err(StudioError::InvalidSoundTool);
        }
        let mut project = self.project.clone();
        let track_index = project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        let clip_index = project.tracks[track_index]
            .clips
            .iter()
            .position(|clip| clip.id == clip_id)
            .ok_or(StudioError::UnknownSoundTool)?;
        let original = project.tracks[track_index].clips.remove(clip_index);
        if original.end <= selection_start || original.start >= selection_end {
            return Err(StudioError::InvalidSoundTool);
        }
        let mut retained = Vec::with_capacity(2);
        if original.start < selection_start {
            let mut left = original.clone();
            left.end = selection_start;
            retained.push(left);
        }
        if original.end > selection_end {
            let mut right = original;
            if !retained.is_empty() {
                right.id = self.take_id();
            }
            right.start = selection_end;
            retained.push(right);
        }
        project.tracks[track_index].clips.extend(retained);
        project.tracks[track_index]
            .clips
            .sort_by(|left, right| left.start.total_cmp(&right.start));
        project.version = self.project.version + 1;
        self.project = project;
        Ok(())
    }

    pub(crate) fn create_midi_clip(
        &mut self,
        track_id: u64,
        spec: &MidiClipSpec,
    ) -> Result<u64, StudioError> {
        if !self.project.tracks.iter().any(|track| track.id == track_id) {
            return Err(StudioError::UnknownTrack);
        }
        validate_clip_fields(
            &spec.label,
            spec.start,
            spec.end,
            &spec.playback_mode,
            spec.loop_beats,
            &spec.notes,
            self.project.duration,
        )?;
        let clip_id = self.take_id();
        let events = spec
            .notes
            .iter()
            .map(|note| ClipEvent {
                id: self.take_id(),
                kind: "note".to_owned(),
                time: note.time,
                duration: note.duration,
                pitch: note.pitch,
                velocity: note.velocity,
            })
            .collect();
        let track = self
            .project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("track was validated");
        track.clips.push(Clip {
            id: clip_id,
            label: spec.label.trim().to_owned(),
            start: spec.start,
            end: spec.end,
            source_start: spec.start,
            style: "generated".to_owned(),
            playback_mode: spec.playback_mode.clone(),
            loop_beats: spec.loop_beats,
            events,
        });
        track
            .clips
            .sort_by(|left, right| left.start.total_cmp(&right.start));
        self.project.version += 1;
        Ok(clip_id)
    }

    pub(crate) fn replace_midi_clip(
        &mut self,
        track_id: u64,
        clip_id: u64,
        spec: &MidiClipSpec,
        selection_start: f32,
        selection_end: f32,
    ) -> Result<(), StudioError> {
        validate_clip_fields(
            &spec.label,
            spec.start,
            spec.end,
            &spec.playback_mode,
            spec.loop_beats,
            &spec.notes,
            self.project.duration,
        )?;
        if spec.start < selection_start
            || spec.end > selection_end
            || selection_end <= selection_start
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let mut project = self.project.clone();
        let track_index = project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        let clip_index = project.tracks[track_index]
            .clips
            .iter()
            .position(|clip| clip.id == clip_id)
            .ok_or(StudioError::UnknownSoundTool)?;
        let original = project.tracks[track_index].clips.remove(clip_index);
        if original.end <= selection_start || original.start >= selection_end {
            return Err(StudioError::InvalidSoundTool);
        }
        if original.end <= spec.start || original.start >= spec.end {
            return Err(StudioError::InvalidSoundTool);
        }
        let events = spec
            .notes
            .iter()
            .map(|note| ClipEvent {
                id: self.take_id(),
                kind: "note".to_owned(),
                time: note.time,
                duration: note.duration,
                pitch: note.pitch,
                velocity: note.velocity,
            })
            .collect();
        let mut replacements = Vec::with_capacity(3);
        if original.start < spec.start {
            let mut left = original.clone();
            left.id = self.take_id();
            left.end = spec.start;
            replacements.push(left);
        }
        replacements.push(Clip {
            id: clip_id,
            label: spec.label.trim().to_owned(),
            start: spec.start,
            end: spec.end,
            source_start: spec.start,
            style: "generated".to_owned(),
            playback_mode: spec.playback_mode.clone(),
            loop_beats: spec.loop_beats,
            events,
        });
        if original.end > spec.end {
            let mut right = original;
            right.id = self.take_id();
            right.start = spec.end;
            replacements.push(right);
        }
        let track = &mut project.tracks[track_index];
        track.clips.extend(replacements);
        track
            .clips
            .sort_by(|left, right| left.start.total_cmp(&right.start));
        project.version = self.project.version + 1;
        self.project = project;
        Ok(())
    }

    pub(crate) fn delete_modulator(
        &mut self,
        track_id: u64,
        modulator_id: u64,
    ) -> Result<(), StudioError> {
        let mut project = self.project.clone();
        let track = project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        let index = track
            .modulators
            .iter()
            .position(|modulator| modulator.id == modulator_id)
            .ok_or(StudioError::UnknownSoundTool)?;
        track.modulators.remove(index);
        project.version = self.project.version + 1;
        self.project = project;
        Ok(())
    }

    pub(crate) fn create_effect(
        &mut self,
        track_id: u64,
        name: &str,
        mix: f32,
    ) -> Result<u64, StudioError> {
        if !crate::surge::is_headless_safe_effect(name)
            || !mix.is_finite()
            || !(0.0..=1.0).contains(&mix)
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let track_index = self
            .project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        if !track_effects_fit_with_added_effect(&self.project.tracks[track_index]) {
            return Err(StudioError::EffectCapacity);
        }
        let id = self.take_id();
        self.project.tracks[track_index]
            .effects
            .push(effect(id, name, mix));
        self.project.tracks[track_index]
            .routing
            .effect_order
            .push(id);
        self.project.version += 1;
        Ok(id)
    }

    pub(crate) fn delete_effect(
        &mut self,
        track_id: u64,
        effect_id: u64,
    ) -> Result<(), StudioError> {
        let mut project = self.project.clone();
        let track = project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        let index = track
            .effects
            .iter()
            .position(|effect| effect.id == effect_id)
            .ok_or(StudioError::UnknownSoundTool)?;
        track.effects.remove(index);
        track.routing.effect_order.retain(|id| *id != effect_id);
        project.version = self.project.version + 1;
        self.project = project;
        Ok(())
    }

    pub(crate) fn set_tempo(&mut self, bpm: u16) -> Result<(), StudioError> {
        if !(60..=180).contains(&bpm) {
            return Err(StudioError::InvalidSoundTool);
        }
        self.project.bpm = bpm;
        self.project.version += 1;
        Ok(())
    }

    pub(crate) fn create_modulator(
        &mut self,
        track_id: u64,
        spec: ModulatorSpec<'_>,
    ) -> Result<u64, StudioError> {
        let track_index = self
            .project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        if !valid_modulator_target_for_trigger(
            &self.project.tracks[track_index],
            spec.target,
            spec.trigger,
        ) || !matches!(
            spec.shape,
            "sine" | "triangle" | "square" | "random" | "envelope" | "formula"
        ) || !spec.rate.is_finite()
            || !(0.01..=20.0).contains(&spec.rate)
            || !matches!(spec.rate_mode, "hz" | "tempo")
            || !spec.depth.is_finite()
            || !(0.0..=1.0).contains(&spec.depth)
            || !matches!(spec.trigger, "free" | "midi")
            || !spec.attack_ms.is_finite()
            || !(0.0..=1_000.0).contains(&spec.attack_ms)
            || !spec.release_ms.is_finite()
            || !(1.0..=5_000.0).contains(&spec.release_ms)
            || !spec.threshold.is_finite()
            || !(0.0..=1.0).contains(&spec.threshold)
            || !matches!(spec.polarity, "increase" | "decrease")
            || spec.formula.len() > 8_192
            || (spec.shape == "formula" && spec.formula.trim().is_empty())
            || spec
                .source_track_id
                .is_some_and(|source| source != track_id)
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let id = self.next_id;
        let candidate = Modulator {
            id,
            name: "AI modulation".to_owned(),
            shape: spec.shape.to_owned(),
            rate: spec.rate,
            rate_mode: spec.rate_mode.to_owned(),
            trigger: spec.trigger.to_owned(),
            source_track_id: (spec.trigger == "midi").then_some(track_id),
            attack_ms: spec.attack_ms,
            release_ms: spec.release_ms,
            threshold: spec.threshold,
            polarity: spec.polarity.to_owned(),
            formula: spec.formula.to_owned(),
            depth: spec.depth,
            target: spec.target.to_owned(),
            enabled: true,
        };
        let mut modulators = self.project.tracks[track_index].modulators.clone();
        modulators.push(candidate.clone());
        if !native_modulator_slots_fit(track_id, &modulators) {
            return Err(StudioError::InvalidSoundTool);
        }
        let id = self.take_id();
        self.project.tracks[track_index].modulators.push(candidate);
        self.project.version += 1;
        Ok(id)
    }

    pub fn configure_sound_tool(
        &mut self,
        track_id: u64,
        tool: &str,
        tool_id: u64,
        clip_id: Option<u64>,
        parameter: &str,
        value: &str,
    ) -> Result<(), StudioError> {
        let mut project = self.project.clone();
        let mut allocated_next_id = self.next_id;
        let track_index = project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        if tool == "instrument" && parameter == "preset" {
            configure_track_preset(
                &mut project,
                track_index,
                tool_id,
                value,
                &mut allocated_next_id,
            )?;
        } else {
            configure_track_tool(
                &mut project.tracks[track_index],
                tool,
                tool_id,
                clip_id,
                parameter,
                value,
            )?;
        }
        let track_ids = project
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>();
        if project.tracks.iter().any(|track| {
            track.modulators.iter().any(|modulator| {
                modulator.trigger != "free"
                    && modulator
                        .source_track_id
                        .is_some_and(|source| !track_ids.contains(&source))
            })
        }) {
            return Err(StudioError::UnknownTrack);
        }
        if project.tracks.iter().any(|track| {
            track
                .modulators
                .iter()
                .any(|modulator| !valid_modulator_configuration(track.id, modulator, &track_ids))
                || !native_modulator_slots_fit(track.id, &track.modulators)
        }) {
            return Err(StudioError::InvalidSoundTool);
        }
        self.next_id = allocated_next_id;
        project.version = self.project.version + 1;
        self.project = project;
        Ok(())
    }

    pub fn reset(&mut self) {
        let version = self.project.version + 1;
        self.project = Project::initial();
        self.project.version = version;
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("project ID namespace exhausted");
        id
    }
}

fn configure_track_preset(
    project: &mut Project,
    track_index: usize,
    instrument_id: u64,
    preset: &str,
    next_id: &mut u64,
) -> Result<(), StudioError> {
    configure_instrument(
        &mut project.tracks[track_index].instrument,
        instrument_id,
        "preset",
        preset,
    )?;
    let track = &mut project.tracks[track_index];
    let previous_preset_ids = track
        .effects
        .iter()
        .filter_map(|effect| effect.preset_slot.map(|slot| (slot, effect.id)))
        .collect::<std::collections::HashMap<_, _>>();
    let previous_order = track.routing.effect_order.clone();
    let added = track
        .effects
        .iter()
        .filter(|effect| effect.preset_slot.is_none())
        .cloned()
        .collect::<Vec<_>>();
    let mut preset_effects =
        crate::surge::preset_effects(preset).map_err(|_| StudioError::InvalidSoundTool)?;
    for effect in &mut preset_effects {
        if let Some(id) = effect
            .preset_slot
            .and_then(|slot| previous_preset_ids.get(&slot))
        {
            effect.id = *id;
        } else {
            effect.id = *next_id;
            *next_id = next_id
                .checked_add(1)
                .ok_or(StudioError::InvalidSoundTool)?;
        }
    }
    let preset_order = preset_effects
        .iter()
        .map(|effect| effect.id)
        .collect::<Vec<_>>();
    let added_ids = added
        .iter()
        .map(|effect| effect.id)
        .collect::<std::collections::HashSet<_>>();
    preset_effects.extend(added);
    track.effects = preset_effects;
    track.routing.effect_order = preset_order;
    track.routing.effect_order.extend(
        previous_order
            .into_iter()
            .filter(|effect_id| added_ids.contains(effect_id)),
    );
    let missing_added = added_ids
        .into_iter()
        .filter(|effect_id| !track.routing.effect_order.contains(effect_id))
        .collect::<Vec<_>>();
    track.routing.effect_order.extend(missing_added);
    if !track_effects_fit(track) {
        return Err(StudioError::EffectCapacity);
    }
    let available_modulation_targets = modulation_targets(track)
        .into_iter()
        .map(|target| target.id)
        .collect::<std::collections::HashSet<_>>();
    let previous_effect_ids = previous_preset_ids
        .values()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let mut retained_modulators = std::mem::take(&mut track.modulators);
    retained_modulators.retain(|modulator| {
        let targets_previous_effect = previous_effect_ids
            .iter()
            .any(|id| modulator.target.starts_with(&format!("effect:{id}.")));
        (!targets_previous_effect || available_modulation_targets.contains(&modulator.target))
            && valid_modulator_target_for_trigger(track, &modulator.target, &modulator.trigger)
    });
    track.modulators = retained_modulators;
    Ok(())
}

fn configure_track_tool(
    track: &mut Track,
    tool: &str,
    tool_id: u64,
    clip_id: Option<u64>,
    parameter: &str,
    value: &str,
) -> Result<(), StudioError> {
    match tool {
        "instrument" => configure_instrument(&mut track.instrument, tool_id, parameter, value),
        "effect" => {
            let semantics = crate::surge::effect_parameter_semantics(
                &track.instrument,
                &track.effects,
                &track.routing.effect_order,
                track.id,
                tool_id,
            );
            let effect = track
                .effects
                .iter_mut()
                .find(|effect| effect.id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            match parameter {
                "mix" => {
                    effect.mix = parse_range(value, 0.0, 1.0)?;
                    mark_effect_override(effect, parameter);
                }
                "enabled" => effect.enabled = parse_bool(value)?,
                parameter if effect.parameters.contains_key(parameter) => {
                    let value = parse_range(value, 0.0, 1.0)?;
                    if semantics.get(parameter).is_some_and(|semantics| {
                        !semantics.choices.is_empty()
                            && !semantics
                                .choices
                                .iter()
                                .any(|(choice, _)| (choice - value).abs() < 0.000_01)
                    }) {
                        return Err(StudioError::InvalidSoundTool);
                    }
                    effect.parameters.insert(parameter.to_owned(), value);
                    mark_effect_override(effect, parameter);
                }
                parameter
                    if crate::surge::effect_parameter_values(&effect.name)
                        .contains_key(parameter) =>
                {
                    let value = parse_range(value, 0.0, 1.0)?;
                    if semantics.get(parameter).is_some_and(|semantics| {
                        !semantics.choices.is_empty()
                            && !semantics
                                .choices
                                .iter()
                                .any(|(choice, _)| (choice - value).abs() < 0.000_01)
                    }) {
                        return Err(StudioError::InvalidSoundTool);
                    }
                    effect.parameters.insert(parameter.to_owned(), value);
                    mark_effect_override(effect, parameter);
                }
                _ => return Err(StudioError::InvalidSoundTool),
            }
            if track_effects_fit(track) {
                Ok(())
            } else {
                Err(StudioError::EffectCapacity)
            }
        }
        "modulator" => {
            let modulator_index = track
                .modulators
                .iter_mut()
                .position(|modulator| modulator.id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            let current = &track.modulators[modulator_index];
            if (parameter == "target"
                && !valid_modulator_target_for_trigger(track, value, &current.trigger))
                || (parameter == "trigger"
                    && !valid_modulator_target_for_trigger(track, &current.target, value))
            {
                return Err(StudioError::InvalidSoundTool);
            }
            let modulator = &mut track.modulators[modulator_index];
            match parameter {
                "shape"
                    if matches!(
                        value,
                        "sine" | "triangle" | "square" | "random" | "envelope" | "formula"
                    ) && (value != "formula" || !modulator.formula.trim().is_empty()) =>
                {
                    modulator.shape = value.to_owned();
                }
                "formula" if !value.trim().is_empty() && value.len() <= 8_192 => {
                    modulator.formula = value.to_owned();
                }
                "rate" => modulator.rate = parse_range(value, 0.01, 20.0)?,
                "rateMode" if matches!(value, "hz" | "tempo") => {
                    modulator.rate_mode = value.to_owned();
                }
                "trigger" if matches!(value, "free" | "midi" | "audio") => {
                    modulator.trigger = value.to_owned();
                    if value == "free" {
                        modulator.source_track_id = None;
                    } else if modulator.source_track_id.is_none() {
                        modulator.source_track_id = Some(track.id);
                    }
                }
                "sourceTrackId" => {
                    modulator.source_track_id =
                        Some(value.parse().map_err(|_| StudioError::InvalidSoundTool)?);
                }
                "attackMs" => modulator.attack_ms = parse_range(value, 0.0, 1_000.0)?,
                "releaseMs" => modulator.release_ms = parse_range(value, 1.0, 5_000.0)?,
                "threshold" => modulator.threshold = parse_range(value, 0.0, 1.0)?,
                "polarity" if matches!(value, "increase" | "decrease") => {
                    modulator.polarity = value.to_owned();
                }
                "depth" => modulator.depth = parse_range(value, 0.0, 1.0)?,
                "target" => modulator.target = value.to_owned(),
                "enabled" => modulator.enabled = parse_bool(value)?,
                _ => return Err(StudioError::InvalidSoundTool),
            }
            Ok(())
        }
        "event" => {
            let clip = track
                .clips
                .iter_mut()
                .find(|clip| Some(clip.id) == clip_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            let event = clip
                .events
                .iter_mut()
                .find(|event| event.id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            match parameter {
                "time" => event.time = parse_range_exclusive(value, 0.0, clip.loop_beats)?,
                "duration" => {
                    event.duration = parse_range(
                        value,
                        MIN_MIDI_NOTE_BEATS,
                        clip.loop_beats.min(MAX_MIDI_NOTE_DURATION_BEATS),
                    )?
                }
                "pitch" => event.pitch = parse_integer_range(value, 0, 127)? as u8,
                "velocity" => event.velocity = parse_range(value, 0.01, 1.0)?,
                _ => return Err(StudioError::InvalidSoundTool),
            }
            clip.events
                .sort_by(|left, right| left.time.total_cmp(&right.time));
            Ok(())
        }
        "routing" if parameter == "position" => {
            let position = parse_integer_range(
                value,
                0,
                track.routing.effect_order.len().saturating_sub(1) as u64,
            )? as usize;
            let current = track
                .routing
                .effect_order
                .iter()
                .position(|effect_id| *effect_id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            let effect_id = track.routing.effect_order.remove(current);
            track.routing.effect_order.insert(position, effect_id);
            Ok(())
        }
        "routing" => Err(StudioError::InvalidSoundTool),
        _ => Err(StudioError::UnknownSoundTool),
    }
}

fn mark_effect_override(effect: &mut Effect, parameter: &str) {
    if !effect
        .parameter_overrides
        .iter()
        .any(|candidate| candidate == parameter)
    {
        effect.parameter_overrides.push(parameter.to_owned());
    }
}

pub(crate) fn track_effects_fit(track: &Track) -> bool {
    let enabled = track.effects.iter().filter(|effect| effect.enabled).count();
    enabled <= crate::surge::SERIAL_EFFECT_SLOT_COUNT
}

fn track_effects_fit_with_added_effect(track: &Track) -> bool {
    let enabled = track.effects.iter().filter(|effect| effect.enabled).count() + 1;
    enabled <= crate::surge::SERIAL_EFFECT_SLOT_COUNT
}

fn validate_clip_fields(
    label: &str,
    start: f32,
    end: f32,
    playback_mode: &str,
    loop_beats: f32,
    notes: &[MidiNote],
    project_duration: f32,
) -> Result<(), StudioError> {
    if label.trim().is_empty()
        || label.chars().count() > 64
        || !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end <= start
        || end > project_duration
        || !loop_beats.is_finite()
        || match playback_mode {
            "loop" => {
                !(0.25..=MAX_LOOP_PLAYBACK_BEATS).contains(&loop_beats)
                    || notes.len() > MAX_MIDI_EVENTS_PER_CLIP
            }
            "once" => {
                !(0.25..=MAX_ONCE_PLAYBACK_BEATS).contains(&loop_beats)
                    || notes.len() > MAX_MIDI_EVENTS_PER_CLIP
            }
            _ => true,
        }
        || notes.iter().any(|note| {
            !note.time.is_finite()
                || !(0.0..loop_beats).contains(&note.time)
                || !note.duration.is_finite()
                || !(MIN_MIDI_NOTE_BEATS..=loop_beats.min(MAX_MIDI_NOTE_DURATION_BEATS))
                    .contains(&note.duration)
                || !note.velocity.is_finite()
                || !(0.01..=1.0).contains(&note.velocity)
        })
    {
        Err(StudioError::InvalidSoundTool)
    } else {
        Ok(())
    }
}

fn configure_instrument(
    instrument: &mut Instrument,
    tool_id: u64,
    parameter: &str,
    value: &str,
) -> Result<(), StudioError> {
    if instrument.id != tool_id {
        return Err(StudioError::UnknownSoundTool);
    }
    if parameter == "preset" {
        return if valid_surge_preset(value) {
            instrument.preset = value.to_owned();
            instrument.native_overrides.clear();
            Ok(())
        } else {
            Err(StudioError::InvalidSoundTool)
        };
    }
    if let Some(native_id) = parameter.strip_prefix("native:") {
        let native_id = native_id
            .parse::<i32>()
            .map_err(|_| StudioError::InvalidSoundTool)?;
        let value = parse_range(value, 0.0, 1.0)?;
        let semantics = crate::surge::instrument_parameters_for_instrument(instrument)
            .iter()
            .find(|candidate| candidate.id == native_id)
            .cloned()
            .ok_or(StudioError::InvalidSoundTool)?;
        if !semantics.choices.is_empty()
            && !semantics
                .choices
                .iter()
                .any(|(choice, _)| (choice - value).abs() < 0.000_01)
        {
            return Err(StudioError::InvalidSoundTool);
        }
        instrument.native_overrides.insert(native_id, value);
        return Ok(());
    }
    Err(StudioError::InvalidSoundTool)
}

pub(crate) fn valid_surge_preset(value: &str) -> bool {
    crate::surge_presets::headless_render_error(value).is_none()
        && (SURGE_PRESETS.contains(&value) || crate::surge_presets::find(value).is_some())
}

fn parse_range(value: &str, minimum: f32, maximum: f32) -> Result<f32, StudioError> {
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite() && (minimum..=maximum).contains(value))
        .ok_or(StudioError::InvalidSoundTool)
}

fn parse_range_exclusive(value: &str, minimum: f32, maximum: f32) -> Result<f32, StudioError> {
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite() && *value >= minimum && *value < maximum)
        .ok_or(StudioError::InvalidSoundTool)
}

fn parse_integer_range(value: &str, minimum: u64, maximum: u64) -> Result<u64, StudioError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| (minimum..=maximum).contains(value))
        .ok_or(StudioError::InvalidSoundTool)
}

fn parse_bool(value: &str) -> Result<bool, StudioError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(StudioError::InvalidSoundTool),
    }
}

fn modulation_targets(track: &Track) -> Vec<ModulationTarget> {
    crate::surge::instrument_parameters_for_instrument(&track.instrument)
        .into_iter()
        .filter(|parameter| parameter.voice_modulatable || parameter.scene_modulatable)
        .map(|parameter| ModulationTarget {
            id: format!("native:{}", parameter.id),
            label: parameter.name,
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        })
        .collect()
}

pub(crate) fn valid_modulator_target(track: &Track, value: &str) -> bool {
    modulation_targets(track)
        .iter()
        .any(|target| target.id == value)
        || value
            .strip_prefix("native:")
            .and_then(|value| value.parse::<i32>().ok())
            .is_some_and(|id| {
                crate::surge::instrument_parameters(&track.instrument.preset)
                    .iter()
                    .any(|parameter| parameter.id == id)
            })
}

pub(crate) fn valid_modulator_target_for_trigger(
    track: &Track,
    value: &str,
    trigger: &str,
) -> bool {
    is_instrument_modulation_target(value)
        && matches!(trigger, "free" | "midi")
        && valid_modulator_target(track, value)
        && crate::surge::instrument_parameter_is_modulatable(&track.instrument, value, trigger)
}

fn is_instrument_modulation_target(target: &str) -> bool {
    target.starts_with("native:")
}

pub(crate) fn valid_modulator_configuration(
    owner_track_id: u64,
    modulator: &Modulator,
    _track_ids: &[u64],
) -> bool {
    let source_track_id = modulator.source_track_id.unwrap_or(owner_track_id);
    is_instrument_modulation_target(&modulator.target)
        && matches!(modulator.trigger.as_str(), "free" | "midi")
        && (modulator.trigger == "free" || source_track_id == owner_track_id)
        && (modulator.shape != "formula" || !modulator.formula.trim().is_empty())
}

pub(crate) fn native_modulator_slots_fit(track_id: u64, modulators: &[Modulator]) -> bool {
    let mut midi_slots = 0;
    let mut scene_slots = 0;
    for modulator in modulators
        .iter()
        .filter(|modulator| crate::surge::is_native_modulator(track_id, modulator))
    {
        if modulator.trigger == "midi" {
            midi_slots += 1;
        } else {
            scene_slots += 1;
        }
    }
    midi_slots <= 6 && scene_slots <= 6
}

#[derive(Clone, Copy)]
enum DemoPart {
    Drums,
    Bass,
    Chords,
}

fn demo_track(id: u64, role: DemoPart, name: &str, color: &str) -> Track {
    let mut track = demo_role_track(id, role);
    track.name = name.to_owned();
    track.color = color.to_owned();
    track.clips = vec![clip(
        id + 10,
        match role {
            DemoPart::Drums => "Pocket beat",
            DemoPart::Bass => "Warm pulse",
            DemoPart::Chords => "Four-chord glow",
        },
        0.0,
        32.0,
        "foundation",
        role,
    )];
    track.modulators.clear();
    #[cfg(test)]
    if let Some(parameter) = crate::surge::instrument_parameters_for_instrument(&track.instrument)
        .into_iter()
        .find(|parameter| parameter.scene_modulatable)
    {
        track.modulators.push(Modulator {
            id: tool_id(id, 50),
            name: "Native test modulation".to_owned(),
            shape: "sine".to_owned(),
            rate: 0.25,
            rate_mode: "hz".to_owned(),
            trigger: "free".to_owned(),
            source_track_id: None,
            attack_ms: 5.0,
            release_ms: 180.0,
            threshold: 0.0,
            polarity: "increase".to_owned(),
            formula: String::new(),
            depth: 0.18,
            target: format!("native:{}", parameter.id),
            enabled: true,
        });
    }
    track
}

fn empty_track(id: u64) -> Track {
    let mut track = generated_track(id);
    track.name = "Empty Track".to_owned();
    track.instrument.preset = "Init".to_owned();
    track.effects.clear();
    track.modulators.clear();
    track.routing.effect_order.clear();
    track.clips.clear();
    track
}

fn generated_track(id: u64) -> Track {
    let instrument_id = tool_id(id, 1);
    Track {
        id,
        name: "Track".to_owned(),
        color: "#808080".to_owned(),
        volume: 1.0,
        muted: false,
        instrument: Instrument {
            id: instrument_id,
            engine: SURGE_ENGINE.to_owned(),
            preset: "Init".to_owned(),
            native_overrides: BTreeMap::new(),
        },
        effects: Vec::new(),
        modulators: Vec::new(),
        routing: Routing {
            effect_order: Vec::new(),
            output: "master".to_owned(),
        },
        clips: Vec::new(),
    }
}

fn demo_role_track(id: u64, role: DemoPart) -> Track {
    let (name, color, preset) = match role {
        DemoPart::Drums => ("AI Drums", "#ffb86b", "Factory/Percussion/Kick 909ish"),
        DemoPart::Bass => ("AI Bass", "#74e0bc", "Factory/Basses/Wide Bassline"),
        DemoPart::Chords => ("AI Chords", "#8ca9ff", "Factory/Polysynths/Anthemish 1"),
    };

    let instrument_id = tool_id(id, 1);

    Track {
        id,
        name: name.to_owned(),
        color: color.to_owned(),
        volume: match role {
            DemoPart::Drums => 0.62,
            DemoPart::Bass => 0.76,
            DemoPart::Chords => 0.68,
        },
        muted: false,
        instrument: Instrument {
            id: instrument_id,
            engine: SURGE_ENGINE.to_owned(),
            preset: preset.to_owned(),
            native_overrides: BTreeMap::new(),
        },
        effects: Vec::new(),
        modulators: Vec::new(),
        routing: Routing {
            effect_order: Vec::new(),
            output: "master".to_owned(),
        },
        clips: Vec::new(),
    }
}

fn clip(id: u64, label: &str, start: f32, end: f32, style: &str, role: DemoPart) -> Clip {
    Clip {
        id,
        label: label.to_owned(),
        start,
        end,
        source_start: start,
        style: style.to_owned(),
        playback_mode: "loop".to_owned(),
        loop_beats: 4.0,
        events: pattern_events(id, role),
    }
}

fn pattern_events(clip_id: u64, role: DemoPart) -> Vec<ClipEvent> {
    let specs: Vec<(&str, f32, f32, u8, f32)> = match role {
        DemoPart::Drums => vec![
            ("note", 0.0, 0.25, 36, 0.92),
            ("note", 1.0, 0.25, 36, 0.84),
            ("note", 2.0, 0.25, 36, 0.88),
            ("note", 3.0, 0.25, 36, 0.84),
        ],
        DemoPart::Bass => vec![
            ("note", 0.0, 0.7, 33, 0.82),
            ("note", 1.0, 0.7, 33, 0.72),
            ("note", 2.0, 0.7, 36, 0.78),
            ("note", 3.0, 0.7, 31, 0.74),
        ],
        DemoPart::Chords => vec![
            ("note", 0.0, 1.85, 57, 0.62),
            ("note", 0.0, 1.85, 60, 0.56),
            ("note", 0.0, 1.85, 64, 0.54),
            ("note", 2.0, 1.85, 53, 0.6),
            ("note", 2.0, 1.85, 57, 0.54),
            ("note", 2.0, 1.85, 60, 0.52),
        ],
    };
    specs
        .into_iter()
        .enumerate()
        .map(
            |(index, (kind, time, duration, pitch, velocity))| ClipEvent {
                id: clip_id * 100 + index as u64 + 1,
                kind: kind.to_owned(),
                time,
                duration,
                pitch,
                velocity,
            },
        )
        .collect()
}

const fn tool_id(track_id: u64, offset: u64) -> u64 {
    track_id * 100 + offset
}

fn effect(id: u64, name: &str, mix: f32) -> Effect {
    Effect {
        id,
        name: name.to_owned(),
        preset_slot: None,
        mix,
        enabled: true,
        parameters: crate::surge::effect_parameter_values(name),
        parameter_overrides: Vec::new(),
        tempo_sync_parameters: Vec::new(),
        deactivated_parameters: Vec::new(),
    }
}

fn decimal(value: f32) -> String {
    let mut value = format!("{value:.6}");
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.push('0');
    }
    value
}

fn write_signal_edge(
    output: &mut String,
    comma: bool,
    source: &str,
    target: &str,
    signal_type: &str,
) {
    if comma {
        output.push(',');
    }
    write!(
        output,
        "{{\"source\":{},\"target\":{},\"type\":{}}}",
        json_string(source),
        json_string(target),
        json_string(signal_type)
    )
    .expect("writing to a string cannot fail");
}

pub(crate) fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("strings must serialize to JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_project_is_one_unbiased_empty_track() {
        let project = Project::initial();
        assert_eq!(project.name, "Untitled Project");
        assert_eq!(project.tracks.len(), 1);
        let track = &project.tracks[0];
        assert_eq!(track.name, "Empty Track");
        assert_eq!(track.instrument.preset, "Init");
        assert!(track.clips.is_empty());
        assert!(track.effects.is_empty());
        assert!(track.modulators.is_empty());
        assert!(track.routing.effect_order.is_empty());
    }

    #[test]
    fn demo_project_contains_a_playable_arrangement() {
        let project = Project::demo();
        assert_eq!(project.bpm, 112);
        assert_eq!(project.tracks.len(), 3);
        assert!(
            project
                .tracks
                .iter()
                .all(|track| track.instrument.engine == SURGE_ENGINE)
        );
        assert_eq!(
            project.tracks[0].instrument.preset,
            "Factory/Percussion/Kick 909ish"
        );
        assert!(
            project.tracks[0].clips[0]
                .events
                .iter()
                .all(|event| event.pitch == 36)
        );
        assert_eq!(
            project.tracks[1].instrument.preset,
            "Factory/Basses/Wide Bassline"
        );
        assert_eq!(
            project.tracks[2].instrument.preset,
            "Factory/Polysynths/Anthemish 1"
        );
        assert!(project.tracks.iter().all(|track| !track.clips.is_empty()));
        assert!(project.tracks.iter().all(|track| {
            !track.clips[0].events.is_empty()
                && track.routing.effect_order.len() == track.effects.len()
        }));
        let json = project.to_json();
        assert!(json.contains("Neon First Light"));
        assert!(json.contains("\"routing\""));
        assert!(json.contains("\"playback\":{\"mode\":\"loop\",\"lengthBeats\":4.0}"));
        assert!(
            json.contains("\"source\":\"clips\",\"target\":\"instrument:101\",\"type\":\"midi\"")
        );
    }

    #[test]
    fn effect_capacity_keeps_every_committed_track_renderable() {
        let mut studio = Studio::from_project(Project::initial());
        let track_id = studio.project().tracks[0].id;
        for _ in 0..crate::surge::SERIAL_EFFECT_SLOT_COUNT {
            studio
                .create_effect(track_id, "Distortion", 0.5)
                .expect("native effect slot");
        }
        assert_eq!(
            studio.create_effect(track_id, "Distortion", 0.5),
            Err(StudioError::EffectCapacity)
        );
    }

    #[test]
    fn native_modulator_slot_limits_are_commit_invariants() {
        let mut studio = Studio::from_project(Project::initial());
        let track_id = studio.project.tracks[0].id;
        let target = modulation_targets(&studio.project.tracks[0])
            .into_iter()
            .find(|target| {
                target
                    .id
                    .strip_prefix("native:")
                    .and_then(|id| id.parse::<i32>().ok())
                    .is_some_and(|id| {
                        crate::surge::instrument_parameters_for_instrument(
                            &studio.project.tracks[0].instrument,
                        )
                        .iter()
                        .any(|parameter| parameter.id == id && parameter.scene_modulatable)
                    })
            })
            .expect("native scene target")
            .id;
        let spec = || ModulatorSpec {
            target: &target,
            shape: "sine",
            formula: "",
            rate: 1.0,
            rate_mode: "hz",
            depth: 0.5,
            trigger: "free",
            source_track_id: None,
            attack_ms: 5.0,
            release_ms: 180.0,
            threshold: 0.1,
            polarity: "increase",
        };
        for _ in 0..6 {
            studio
                .create_modulator(track_id, spec())
                .expect("available native scene slot");
        }
        assert_eq!(
            studio.create_modulator(track_id, spec()),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(studio.project.tracks[0].modulators.len(), 6);
    }

    #[test]
    fn native_modulators_reject_targets_surge_cannot_modulate() {
        let mut studio = Studio::new();
        let track_id = studio.project.tracks[1].id;
        let modulator_id = studio.project.tracks[1].modulators[0].id;
        let mute_id = crate::surge::instrument_parameters_for_instrument(
            &studio.project.tracks[1].instrument,
        )
        .into_iter()
        .find(|parameter| parameter.name.ends_with("Osc 1 Mute"))
        .expect("oscillator mute")
        .id;
        let target = format!("native:{mute_id}");
        let spec = ModulatorSpec {
            target: &target,
            shape: "sine",
            formula: "",
            rate: 1.0,
            rate_mode: "hz",
            depth: 0.5,
            trigger: "free",
            source_track_id: None,
            attack_ms: 5.0,
            release_ms: 180.0,
            threshold: 0.1,
            polarity: "increase",
        };

        assert_eq!(
            studio.create_modulator(track_id, spec),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(
            studio.configure_sound_tool(
                track_id,
                "modulator",
                modulator_id,
                None,
                "target",
                &target,
            ),
            Err(StudioError::InvalidSoundTool)
        );
    }

    #[test]
    fn native_effect_selections_reject_values_between_surge_choices() {
        let mut studio = Studio::new();
        let track_id = studio.project.tracks[1].id;
        let instrument = studio.project.tracks[1].instrument.clone();
        let (effect_name, parameter) = crate::surge::SURGE_EFFECT_TYPES
            .iter()
            .filter(|name| **name != "Off" && **name != "Audio Input")
            .find_map(|name| {
                let candidate = effect(99_003, name, 0.5);
                crate::surge::effect_parameter_semantics(
                    &instrument,
                    std::slice::from_ref(&candidate),
                    &[candidate.id],
                    track_id,
                    candidate.id,
                )
                .into_iter()
                .find(|(parameter, semantics)| parameter != "mix" && !semantics.choices.is_empty())
                .map(|(parameter, _)| ((*name).to_owned(), parameter))
            })
            .expect("native effect selection parameter");
        let effect_id = studio
            .create_effect(track_id, &effect_name, 0.5)
            .expect("effect with selection");
        assert_eq!(
            studio
                .configure_sound_tool(track_id, "effect", effect_id, None, &parameter, "0.123456",),
            Err(StudioError::InvalidSoundTool)
        );
    }

    #[test]
    fn headless_unsafe_presets_cannot_be_configured() {
        let mut studio = Studio::new();
        let track = &studio.project().tracks[0];
        let track_id = track.id;
        let instrument_id = track.instrument.id;
        assert_eq!(
            studio.configure_sound_tool(
                track_id,
                "instrument",
                instrument_id,
                None,
                "preset",
                "Factory/Keys/House Organ",
            ),
            Err(StudioError::InvalidSoundTool)
        );
    }

    #[test]
    fn disabled_modulators_are_not_active_control_routes() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let modulator_id = bass.modulators[0].id;
        studio
            .configure_sound_tool(bass_id, "modulator", modulator_id, None, "enabled", "false")
            .expect("disable modulator");

        let json = studio.to_json();
        assert!(json.contains(&format!("\"id\":{modulator_id},\"type\":\"modulator\"")));
        assert!(json.contains("\"enabled\":false"));
        assert!(!json.contains(&format!("\"source\":\"modulator:{modulator_id}\"")));
    }

    #[test]
    fn sound_tool_validation_preserves_the_project() {
        let mut studio = Studio::new();
        let before = studio.to_json();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let instrument_id = bass.instrument.id;
        assert_eq!(
            studio
                .configure_sound_tool(bass_id, "instrument", instrument_id, None, "attack", "20",),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(studio.to_json(), before);
    }

    #[test]
    fn newly_added_effects_publish_native_surge_controls() {
        let effect = effect(1, "Reverb 2", 0.5);
        assert!(!effect.parameters.is_empty());
        assert!(effect.parameters.keys().all(|parameter| parameter != "mix"));
    }
}
