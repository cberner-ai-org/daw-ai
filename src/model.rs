use std::collections::BTreeMap;
use std::fmt::{self, Write};

use serde::Serialize;

use crate::prompt::{Action, PromptEngine};

const HISTORY_LIMIT: usize = 50;
const TRACK_LIMIT: usize = 128;
pub(crate) const EDIT_LOG_LIMIT: usize = 256;
pub(crate) const MAX_PROMPT_CHARACTERS: usize = 2_000;
pub(crate) const FILTER_CUTOFF_MIN_HZ: f32 = 80.0;
pub(crate) const FILTER_CUTOFF_MAX_HZ: f32 = 16_000.0;
pub(crate) const FILTER_CUTOFF_DEFAULT_HZ: f32 = 1_200.0;
pub(crate) const FILTER_RESONANCE_MIN: f32 = 0.1;
pub(crate) const FILTER_RESONANCE_MAX: f32 = 20.0;
pub(crate) const FILTER_RESONANCE_DEFAULT: f32 = 0.7;
pub(crate) const SURGE_ENGINE: &str = "Surge XT";
pub(crate) const SURGE_PRESETS: &[&str] = &[
    "Init",
    "Surge Kick",
    "Surge Snare",
    "Surge Closed Hat",
    "Surge Open Hat",
    "Surge Crash",
    "Surge Percussion",
    "Surge Bass",
    "Surge Pad",
    "Surge Lead",
    "Surge Atmosphere",
];

struct ModulationTarget {
    id: String,
    label: String,
    minimum: f32,
    maximum: f32,
    scale: f32,
    mode: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackRole {
    Drums,
    Bass,
    Chords,
    Lead,
    Texture,
}

impl TrackRole {
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "drums" => Some(Self::Drums),
            "bass" => Some(Self::Bass),
            "chords" => Some(Self::Chords),
            "lead" => Some(Self::Lead),
            "texture" => Some(Self::Texture),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Drums => "drums",
            Self::Bass => "bass",
            Self::Chords => "chords",
            Self::Lead => "lead",
            Self::Texture => "texture",
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Drums => "drums",
            Self::Bass => "bass",
            Self::Chords => "chords",
            Self::Lead => "lead synth",
            Self::Texture => "texture",
        }
    }
}

pub(crate) const fn role_default_filter_cutoff_hz(role: TrackRole) -> f32 {
    match role {
        TrackRole::Drums => 3_150.0,
        TrackRole::Bass => 420.0,
        TrackRole::Chords => 980.0,
        TrackRole::Lead => 1_260.0,
        TrackRole::Texture => 1_470.0,
    }
}

#[derive(Clone, Debug)]
pub struct Effect {
    pub id: u64,
    pub name: String,
    pub mix: f32,
    pub cutoff_hz: Option<f32>,
    pub resonance: Option<f32>,
    pub enabled: bool,
    pub parameters: BTreeMap<String, f32>,
    pub parameter_overrides: Vec<String>,
}

pub(crate) struct EffectParameterSpec {
    pub(crate) name: &'static str,
    pub(crate) native: &'static str,
    pub(crate) default: f32,
}

pub(crate) fn effect_parameter_specs(name: &str) -> &'static [EffectParameterSpec] {
    match name {
        "Delay" | "Echo" | "Floaty Delay" => &[
            EffectParameterSpec {
                name: "time",
                native: "Left",
                default: 0.35,
            },
            EffectParameterSpec {
                name: "feedback",
                native: "Feedback",
                default: 0.3,
            },
            EffectParameterSpec {
                name: "lowCut",
                native: "Low Cut",
                default: 0.15,
            },
            EffectParameterSpec {
                name: "highCut",
                native: "High Cut",
                default: 0.8,
            },
            EffectParameterSpec {
                name: "width",
                native: "Width",
                default: 0.75,
            },
        ],
        "Reverb 1" | "Reverb 2" | "Reverb" | "Room" | "Spring Reverb" => &[
            EffectParameterSpec {
                name: "size",
                native: "Room Size",
                default: 0.55,
            },
            EffectParameterSpec {
                name: "decay",
                native: "Decay Time",
                default: 0.5,
            },
            EffectParameterSpec {
                name: "preDelay",
                native: "Pre-Delay",
                default: 0.15,
            },
            EffectParameterSpec {
                name: "damping",
                native: "HF Damping",
                default: 0.55,
            },
            EffectParameterSpec {
                name: "width",
                native: "Width",
                default: 0.8,
            },
        ],
        "Distortion" | "Drive" | "Waveshaper" | "CHOW" | "Tape" => &[
            EffectParameterSpec {
                name: "drive",
                native: "Drive",
                default: 0.45,
            },
            EffectParameterSpec {
                name: "tone",
                native: "Frequency",
                default: 0.55,
            },
            EffectParameterSpec {
                name: "output",
                native: "Gain",
                default: 0.5,
            },
        ],
        "EQ" | "Graphic EQ" | "Low-pass filter" => &[
            EffectParameterSpec {
                name: "lowGain",
                native: "Gain 1",
                default: 0.5,
            },
            EffectParameterSpec {
                name: "midGain",
                native: "Gain 2",
                default: 0.5,
            },
            EffectParameterSpec {
                name: "highGain",
                native: "Gain 3",
                default: 0.5,
            },
            EffectParameterSpec {
                name: "lowFrequency",
                native: "Frequency 1",
                default: 0.3,
            },
            EffectParameterSpec {
                name: "midFrequency",
                native: "Frequency 2",
                default: 0.5,
            },
            EffectParameterSpec {
                name: "highFrequency",
                native: "Frequency 3",
                default: 0.7,
            },
        ],
        "Conditioner" | "Punch compressor" => &[
            EffectParameterSpec {
                name: "threshold",
                native: "Threshold",
                default: 0.55,
            },
            EffectParameterSpec {
                name: "attack",
                native: "Attack Rate",
                default: 0.35,
            },
            EffectParameterSpec {
                name: "release",
                native: "Release Rate",
                default: 0.55,
            },
            EffectParameterSpec {
                name: "output",
                native: "Gain",
                default: 0.5,
            },
        ],
        "Phaser" | "Chorus" | "Flanger" | "Ensemble" => &[
            EffectParameterSpec {
                name: "rate",
                native: "Rate",
                default: 0.3,
            },
            EffectParameterSpec {
                name: "depth",
                native: "Depth",
                default: 0.5,
            },
            EffectParameterSpec {
                name: "feedback",
                native: "Feedback",
                default: 0.35,
            },
            EffectParameterSpec {
                name: "width",
                native: "Width",
                default: 0.75,
            },
        ],
        _ => &[],
    }
}

#[derive(Clone, Debug)]
pub struct Instrument {
    pub id: u64,
    pub engine: String,
    pub preset: String,
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
    pub cutoff: f32,
    pub resonance: f32,
    pub pitch: f32,
    pub timbre: f32,
    pub output: f32,
    pub parameter_overrides: Vec<String>,
    pub native_overrides: BTreeMap<i32, f32>,
}

impl Instrument {
    pub(crate) fn overrides(&self, parameter: &str) -> bool {
        self.parameter_overrides
            .iter()
            .any(|candidate| candidate == parameter)
    }
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

#[derive(Clone, Debug)]
pub struct AudioClip {
    pub id: u64,
    pub label: String,
    pub start: f32,
    pub end: f32,
    pub asset: String,
    pub source_offset: f32,
    pub source_duration: f32,
    pub gain: f32,
    pub reversed: bool,
}

pub(crate) struct AudioClipSliceSpec<'a> {
    pub(crate) label: &'a str,
    pub(crate) source_start: f32,
    pub(crate) source_end: f32,
    pub(crate) destination_start: f32,
    pub(crate) reversed: bool,
}

pub(crate) struct ModulatorSpec<'a> {
    pub target: &'a str,
    pub shape: &'a str,
    pub rate: f32,
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
    pub role: TrackRole,
    pub color: String,
    pub volume: f32,
    pub muted: bool,
    pub instrument: Instrument,
    pub effects: Vec<Effect>,
    pub modulators: Vec<Modulator>,
    pub routing: Routing,
    pub clips: Vec<Clip>,
    pub audio_clips: Vec<AudioClip>,
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub id: u64,
    pub operation_id: Option<String>,
    pub start: f32,
    pub end: f32,
    pub prompt: String,
    pub summary: String,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub struct EditOperation {
    pub operation_id: String,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelOperationAction {
    Add,
    Delete,
}

#[derive(Clone, Debug)]
pub struct ChannelOperation {
    pub operation_id: String,
    pub action: ChannelOperationAction,
    pub track_id: u64,
    pub role: Option<TrackRole>,
    pub project_version: u64,
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
    pub channel_operations: Vec<ChannelOperation>,
}

pub(crate) struct MidiClipSpec {
    pub(crate) label: String,
    pub(crate) start: f32,
    pub(crate) end: f32,
    pub(crate) playback_mode: String,
    pub(crate) loop_beats: f32,
    pub(crate) notes: Vec<crate::prompt::MidiNote>,
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
            channel_operations: Vec::new(),
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
                demo_track(1, TrackRole::Drums, "Pulse Kit", "#ffb86b"),
                demo_track(2, TrackRole::Bass, "Soft Current", "#74e0bc"),
                demo_track(3, TrackRole::Chords, "Glass Chords", "#8ca9ff"),
            ],
            edits: Vec::new(),
            edit_operations: Vec::new(),
            channel_operations: Vec::new(),
        }
    }

    pub fn from_json(source: &str) -> Result<Self, ProjectFileError> {
        crate::project_file::parse_project(source)
    }

    fn highest_id(&self) -> u64 {
        let mut highest = self
            .edits
            .iter()
            .map(|edit| edit.id)
            .chain(
                self.channel_operations
                    .iter()
                    .map(|operation| operation.track_id),
            )
            .max()
            .unwrap_or(0);
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
            for clip in &track.audio_clips {
                highest = highest.max(clip.id);
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
        output.push_str("],\"channelOperations\":[");
        for (index, operation) in self.channel_operations.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            operation.write_json(&mut output);
        }
        output.push_str("]}");
        output
    }

    fn compact_edit_log(&mut self) {
        let mut excess = self.edits.len().saturating_sub(EDIT_LOG_LIMIT);
        self.edits.retain(|edit| {
            if excess > 0 && !edit.action.has_regional_state() {
                excess -= 1;
                false
            } else {
                true
            }
        });
        if excess > 0 {
            self.edits.drain(..excess);
        }
        if self.edit_operations.len() > EDIT_LOG_LIMIT {
            self.edit_operations
                .drain(..self.edit_operations.len() - EDIT_LOG_LIMIT);
        }
        if self.channel_operations.len() > EDIT_LOG_LIMIT {
            self.channel_operations
                .drain(..self.channel_operations.len() - EDIT_LOG_LIMIT);
        }
    }

    fn write_graph_json(&self, output: &mut String) {
        write!(
            output,
            "{{\"name\":{},\"bpm\":{},\"duration\":{},\"version\":{},\"tracks\":[",
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
                "{{\"id\":{},\"name\":{},\"role\":{},\"color\":{},",
                "\"volume\":{},\"muted\":{},\"instrument\":{{",
                "\"id\":{},\"type\":\"instrument\",\"engine\":{},\"preset\":{},",
                "\"parameters\":{{\"attack\":{},\"decay\":{},\"sustain\":{},\"release\":{},",
                "\"cutoff\":{},\"resonance\":{},\"pitch\":{},\"timbre\":{},\"output\":{}}},",
                "\"overrides\":["
            ),
            self.id,
            json_string(&self.name),
            json_string(self.role.as_str()),
            json_string(&self.color),
            decimal(self.volume),
            self.muted,
            self.instrument.id,
            json_string(&self.instrument.engine),
            json_string(&self.instrument.preset),
            decimal(self.instrument.attack),
            decimal(self.instrument.decay),
            decimal(self.instrument.sustain),
            decimal(self.instrument.release),
            decimal(self.instrument.cutoff),
            decimal(self.instrument.resonance),
            decimal(self.instrument.pitch),
            decimal(self.instrument.timbre),
            decimal(self.instrument.output)
        )
        .expect("writing to a string cannot fail");
        for (index, parameter) in self.instrument.parameter_overrides.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            output.push_str(&json_string(parameter));
        }
        output.push(']');
        if !self.instrument.native_overrides.is_empty() {
            output.push_str(",\"nativeOverrides\":{");
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
        }
        output.push_str("},\"effects\":[");

        for (index, effect) in self.effects.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"type\":\"effect\",\"name\":{},",
                    "\"enabled\":{},\"parameters\":{{\"mix\":{}"
                ),
                effect.id,
                json_string(&effect.name),
                effect.enabled,
                decimal(effect.mix)
            )
            .expect("writing to a string cannot fail");
            if let Some(cutoff_hz) = effect.cutoff_hz {
                write!(output, ",\"cutoff\":{}", decimal(cutoff_hz))
                    .expect("writing to a string cannot fail");
            }
            if let Some(resonance) = effect.resonance {
                write!(output, ",\"resonance\":{}", decimal(resonance))
                    .expect("writing to a string cannot fail");
            }
            for (name, value) in &effect.parameters {
                write!(output, ",{}:{}", json_string(name), decimal(*value))
                    .expect("writing to a string cannot fail");
            }
            output.push_str("},\"overrides\":[");
            for (index, parameter) in effect.parameter_overrides.iter().enumerate() {
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

        output.push_str("],\"modulationTargets\":[");
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

        output.push_str("],\"automationTargets\":[");
        for (index, target) in automation_targets(self).iter().enumerate() {
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
        output.push_str("],\"audioClips\":[");
        for (index, clip) in self.audio_clips.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"label\":{},\"start\":{},\"end\":{},\"asset\":{},",
                    "\"sourceOffset\":{},\"sourceDuration\":{},\"gain\":{},\"reversed\":{}}}"
                ),
                clip.id,
                json_string(&clip.label),
                decimal(clip.start),
                decimal(clip.end),
                json_string(&clip.asset),
                decimal(clip.source_offset),
                decimal(clip.source_duration),
                decimal(clip.gain),
                clip.reversed
            )
            .expect("writing to a string cannot fail");
        }
        output.push_str("]}");
    }
}

impl Edit {
    fn write_json(&self, output: &mut String) {
        write!(output, "{{\"id\":{}", self.id).expect("writing to a string cannot fail");
        if let Some(operation_id) = &self.operation_id {
            write!(output, ",\"operationId\":{}", json_string(operation_id))
                .expect("writing to a string cannot fail");
        }
        write!(
            output,
            concat!(
                ",\"start\":{},\"end\":{},\"prompt\":{},",
                "\"summary\":{},\"action\":"
            ),
            decimal(self.start),
            decimal(self.end),
            json_string(&self.prompt),
            json_string(&self.summary)
        )
        .expect("writing to a string cannot fail");
        self.action.write_json(output);
        output.push('}');
    }
}

impl EditOperation {
    fn write_json(&self, output: &mut String) {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct PersistedEditOperation<'a> {
            operation_id: &'a str,
            status: &'a str,
            applied_steps: usize,
            initial_version: u64,
            project_version: u64,
            message: &'a str,
        }

        let persisted = PersistedEditOperation {
            operation_id: &self.operation_id,
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

impl ChannelOperation {
    fn write_json(&self, output: &mut String) {
        let action = match self.action {
            ChannelOperationAction::Add => "add",
            ChannelOperationAction::Delete => "delete",
        };
        write!(
            output,
            concat!(
                "{{\"operationId\":{},\"action\":{},\"trackId\":{},",
                "\"role\":{},\"projectVersion\":{}}}"
            ),
            json_string(&self.operation_id),
            json_string(action),
            self.track_id,
            self.role
                .map_or_else(|| "null".to_owned(), |role| json_string(role.as_str())),
            self.project_version
        )
        .expect("writing to a string cannot fail");
    }
}

impl Action {
    fn retain_after_track_deletion(&mut self, track_id: u64) -> bool {
        match self {
            Self::GraphMutation => true,
            Self::Compound { actions } => {
                actions.retain_mut(|action| action.retain_after_track_deletion(track_id));
                !actions.is_empty()
            }
            Self::Timed { action, .. } => action.retain_after_track_deletion(track_id),
            Self::Automation {
                track_id: owner_id, ..
            } => *owner_id != track_id,
            _ => true,
        }
    }

    fn has_regional_state(&self) -> bool {
        match self {
            Self::GraphMutation => false,
            Self::Compound { actions } => actions.iter().any(Self::has_regional_state),
            Self::Timed { action, .. } => action.has_regional_state(),
            Self::Gain { .. }
            | Self::Mute { .. }
            | Self::Automation { .. }
            | Self::Effect { .. }
            | Self::RemoveEffect { .. }
            | Self::Filter { .. }
            | Self::Rhythm { .. } => true,
            Self::MidiClip { .. }
            | Self::AddTrack { .. }
            | Self::Instrument { .. }
            | Self::Modulator { .. }
            | Self::Configure { .. }
            | Self::Tempo { .. } => false,
        }
    }

    fn write_json(&self, output: &mut String) {
        match self {
            Self::GraphMutation => write!(output, "{{\"type\":\"graph-mutation\"}}"),
            Self::Compound { actions } => {
                output.push_str("{\"type\":\"compound\",\"actions\":[");
                for (index, action) in actions.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    action.write_json(output);
                }
                output.push_str("]}");
                return;
            }
            Self::Timed { start, end, action } => {
                write!(
                    output,
                    "{{\"type\":\"timed\",\"start\":{},\"end\":{},\"action\":",
                    decimal(*start),
                    decimal(*end)
                )
                .expect("writing to a string cannot fail");
                action.write_json(output);
                output.push('}');
                return;
            }
            Self::Gain { amount, target } => write!(
                output,
                "{{\"type\":\"gain\",\"value\":{},\"target\":{}}}",
                decimal(*amount),
                role_json(*target)
            ),
            Self::Mute { target } => write!(
                output,
                "{{\"type\":\"mute\",\"target\":{}}}",
                role_json(*target)
            ),
            Self::MidiClip {
                track_id,
                target,
                label,
                start,
                end,
                loop_beats,
                notes,
            } => {
                write!(
                    output,
                    concat!(
                        "{{\"type\":\"midi-clip\",\"target\":{},\"trackId\":{},",
                        "\"label\":{},\"start\":{},\"end\":{},\"loopBeats\":{},\"events\":["
                    ),
                    json_string(target.as_str()),
                    track_id,
                    json_string(label),
                    decimal(*start),
                    decimal(*end),
                    decimal(*loop_beats)
                )
                .expect("writing to a string cannot fail");
                for (index, note) in notes.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write!(
                        output,
                        concat!(
                            "{{\"type\":\"note\",\"time\":{},\"duration\":{},",
                            "\"pitch\":{},\"velocity\":{}}}"
                        ),
                        decimal(note.time),
                        decimal(note.duration),
                        note.pitch,
                        decimal(note.velocity)
                    )
                    .expect("writing to a string cannot fail");
                }
                output.push_str("]}");
                return;
            }
            Self::AddTrack { role } => write!(
                output,
                "{{\"type\":\"add-track\",\"target\":{}}}",
                json_string(role.as_str())
            ),
            Self::Instrument { preset, target } => write!(
                output,
                concat!(
                    "{{\"type\":\"instrument\",\"name\":{},",
                    "\"value\":0.0,\"target\":{}}}"
                ),
                json_string(preset),
                json_string(target.as_str())
            ),
            Self::Modulator {
                parameter,
                shape,
                rate,
                depth,
                target,
            } => write!(
                output,
                concat!(
                    "{{\"type\":\"modulator\",\"name\":{},",
                    "\"shape\":{},\"rate\":{},\"value\":{},\"target\":{}}}"
                ),
                json_string(parameter),
                json_string(shape),
                decimal(*rate),
                decimal(*depth),
                json_string(target.as_str())
            ),
            Self::Configure {
                track_id,
                target,
                tool,
                tool_id,
                clip_id,
                parameter,
                value,
            } => write!(
                output,
                concat!(
                    "{{\"type\":\"configure\",\"target\":{},\"trackId\":{},",
                    "\"tool\":{},\"toolId\":{},\"clipId\":{},",
                    "\"parameter\":{},\"setting\":{}}}"
                ),
                json_string(target.as_str()),
                track_id,
                json_string(tool),
                tool_id,
                clip_id.unwrap_or(0),
                json_string(parameter),
                json_string(value)
            ),
            Self::Automation {
                track_id,
                parameter,
                curve,
                points,
                target,
            } => {
                write!(
                    output,
                    concat!(
                        "{{\"type\":\"automation\",\"trackId\":{},\"name\":{},\"curve\":{},",
                        "\"target\":{},\"points\":["
                    ),
                    track_id,
                    json_string(parameter),
                    json_string(curve),
                    json_string(target.as_str())
                )
                .expect("writing to a string cannot fail");
                for (index, point) in points.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write!(
                        output,
                        "{{\"time\":{},\"value\":{}}}",
                        decimal(point.time),
                        decimal(point.value)
                    )
                    .expect("writing to a string cannot fail");
                }
                output.push_str("]}");
                return;
            }
            Self::Effect { name, mix, target } => write!(
                output,
                concat!(
                    "{{\"type\":\"effect\",\"name\":{},\"value\":{},",
                    "\"target\":{}}}"
                ),
                json_string(name),
                decimal(*mix),
                role_json(*target)
            ),
            Self::RemoveEffect { name, target } => write!(
                output,
                concat!(
                    "{{\"type\":\"remove-effect\",\"name\":{},",
                    "\"target\":{}}}"
                ),
                json_string(name),
                role_json(*target)
            ),
            Self::Filter { amount, target } => write!(
                output,
                "{{\"type\":\"filter\",\"value\":{},\"target\":{}}}",
                decimal(*amount),
                role_json(*target)
            ),
            Self::Rhythm { amount, target } => write!(
                output,
                "{{\"type\":\"rhythm\",\"value\":{},\"target\":{}}}",
                decimal(*amount),
                role_json(*target)
            ),
            Self::Tempo { bpm } => write!(
                output,
                "{{\"type\":\"tempo\",\"value\":{bpm},\"target\":\"all\"}}"
            ),
        }
        .expect("writing to a string cannot fail");
    }
}

#[derive(Debug, PartialEq)]
pub enum StudioError {
    EmptyPrompt,
    InvalidPrompt,
    InvalidSelection,
    UnknownTrack,
    InvalidMix,
    InvalidChannel,
    UnknownSoundTool,
    InvalidSoundTool,
    EffectCapacity,
}

#[derive(Clone)]
pub struct Studio {
    project: Project,
    history: Vec<Project>,
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
        Self {
            project,
            history: Vec::new(),
            next_id,
        }
    }

    #[must_use]
    pub fn from_project(mut project: Project) -> Self {
        project.compact_edit_log();
        let next_id = project
            .highest_id()
            .checked_add(1)
            .expect("project exhausted the ID namespace");
        Self {
            project,
            history: Vec::new(),
            next_id,
        }
    }

    #[must_use]
    pub const fn project(&self) -> &Project {
        &self.project
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        self.to_json_with_can_undo(!self.history.is_empty())
    }

    #[must_use]
    pub(crate) fn to_json_with_can_undo(&self, can_undo: bool) -> String {
        let mut json = self.project.to_json();
        json.pop();
        write!(json, ",\"canUndo\":{can_undo}}}").expect("writing to a string cannot fail");
        json
    }

    pub fn apply_prompt(
        &mut self,
        start: f32,
        end: f32,
        prompt: &str,
    ) -> Result<String, StudioError> {
        let plan = PromptEngine::interpret_project(prompt, &self.project, start, end);
        self.apply_plan(start, end, prompt, plan)
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

    pub fn apply_plan(
        &mut self,
        start: f32,
        end: f32,
        prompt: &str,
        plan: crate::prompt::EditPlan,
    ) -> Result<String, StudioError> {
        self.apply_plan_inner(start, end, prompt, None, plan)
    }

    pub(crate) fn apply_plan_for_operation(
        &mut self,
        start: f32,
        end: f32,
        prompt: &str,
        operation_id: String,
        plan: crate::prompt::EditPlan,
    ) -> Result<String, StudioError> {
        self.apply_plan_inner(start, end, prompt, Some(operation_id), plan)
    }

    fn apply_plan_inner(
        &mut self,
        start: f32,
        end: f32,
        prompt: &str,
        operation_id: Option<String>,
        plan: crate::prompt::EditPlan,
    ) -> Result<String, StudioError> {
        self.validate_edit(start, end, prompt)?;
        let mut candidate = Self {
            project: self.project.clone(),
            history: Vec::new(),
            next_id: self.next_id,
        };
        candidate.apply_action(&plan.action, start, end)?;
        let prompt = prompt.trim();
        self.remember();
        self.project = candidate.project;
        self.next_id = candidate.next_id;

        let summary = plan.summary;
        let edit_id = self.take_id();
        self.project.edits.push(Edit {
            id: edit_id,
            operation_id: operation_id.clone(),
            start,
            end,
            prompt: prompt.to_owned(),
            summary: summary.clone(),
            action: plan.action,
        });
        self.project.compact_edit_log();
        self.project.version += 1;
        if let Some(operation_id) = operation_id {
            self.project.edit_operations.push(EditOperation {
                operation_id,
                status: EditOperationStatus::Completed,
                applied_steps: 1,
                initial_version: self.project.version.saturating_sub(1),
                project_version: self.project.version,
                message: summary.clone(),
            });
            self.project.compact_edit_log();
        }
        Ok(summary)
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
        project.channel_operations = self.project.channel_operations.clone();
        project.version = self.project.version;
        let next_id = project
            .highest_id()
            .checked_add(1)
            .expect("project exhausted the ID namespace");

        let prompt = prompt.trim();
        let summary = plan.summary;
        self.remember();
        self.project = project;
        self.next_id = next_id;
        let edit_id = self.take_id();
        self.project.edits.push(Edit {
            id: edit_id,
            operation_id: None,
            start,
            end,
            prompt: prompt.to_owned(),
            summary: summary.clone(),
            action: plan.action,
        });
        self.project.compact_edit_log();
        self.project.version += 1;
        Ok(summary)
    }

    pub(crate) fn record_operation_step(&mut self, operation_id: &str, message: &str) -> bool {
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

    fn apply_action(&mut self, action: &Action, start: f32, end: f32) -> Result<(), StudioError> {
        match action {
            Action::GraphMutation => Ok(()),
            Action::Compound { actions } => {
                for action in actions {
                    self.apply_action(action, start, end)?;
                }
                Ok(())
            }
            Action::Timed {
                start: relative_start,
                end: relative_end,
                action,
            } => {
                let duration = end - start;
                self.apply_action(
                    action,
                    start + duration * relative_start,
                    start + duration * relative_end,
                )
            }
            Action::AddTrack { role } => {
                if self.project.tracks.len() >= TRACK_LIMIT {
                    return Err(StudioError::InvalidChannel);
                }
                self.add_track(*role);
                Ok(())
            }
            Action::MidiClip {
                track_id,
                target,
                label,
                start: clip_start,
                end: clip_end,
                loop_beats,
                notes,
            } => self.add_midi_clip(
                *track_id,
                *target,
                label,
                *clip_start,
                *clip_end,
                *loop_beats,
                notes,
                start,
                end,
            ),
            Action::Tempo { bpm } => {
                self.project.bpm = *bpm;
                Ok(())
            }
            Action::Instrument { preset, target } => {
                let track_index = role_action_track_index(&self.project, *target, None)
                    .ok_or(StudioError::UnknownTrack)?;
                let instrument = &mut self.project.tracks[track_index].instrument;
                if !valid_surge_preset(preset) {
                    return Err(StudioError::InvalidSoundTool);
                }
                instrument.preset = (*preset).to_owned();
                Ok(())
            }
            Action::Modulator {
                parameter,
                shape,
                rate,
                depth,
                target,
            } => self.add_modulator(*target, parameter, shape, *rate, *depth),
            Action::Configure {
                track_id,
                target,
                tool,
                tool_id,
                clip_id,
                parameter,
                value,
                ..
            } => {
                let track = self
                    .project
                    .tracks
                    .iter_mut()
                    .find(|track| track.id == *track_id && track.role == *target)
                    .ok_or(StudioError::UnknownTrack)?;
                configure_track_tool(track, tool, *tool_id, *clip_id, parameter, value)
            }
            Action::Automation {
                track_id,
                parameter,
                points,
                target,
                ..
            } => {
                let track = self
                    .project
                    .tracks
                    .iter()
                    .find(|track| {
                        track.id == *track_id
                            && track.role == *target
                            && valid_automation_target(track, parameter)
                    })
                    .ok_or(StudioError::UnknownSoundTool)?;
                let target = automation_targets(track)
                    .into_iter()
                    .find(|target| target.id == *parameter)
                    .expect("validated automation target exists");
                if points
                    .iter()
                    .any(|point| !(target.minimum..=target.maximum).contains(&point.value))
                {
                    return Err(StudioError::InvalidSoundTool);
                }
                Ok(())
            }
            Action::Gain { target, .. }
            | Action::Mute { target }
            | Action::Effect { target, .. }
            | Action::RemoveEffect { target, .. }
            | Action::Filter { target, .. }
            | Action::Rhythm { target, .. } => {
                if target.is_some_and(|target| {
                    !self.project.tracks.iter().any(|track| track.role == target)
                }) {
                    Err(StudioError::UnknownTrack)
                } else {
                    Ok(())
                }
            }
        }
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

        self.remember();
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

    pub fn add_channel(&mut self, role: TrackRole) -> Result<u64, StudioError> {
        if self.project.tracks.len() >= TRACK_LIMIT {
            return Err(StudioError::InvalidChannel);
        }
        self.remember();
        let track_id = self.add_track(role);
        self.project.version += 1;
        Ok(track_id)
    }

    pub(crate) fn add_empty_channel(&mut self, role: TrackRole) -> Result<u64, StudioError> {
        if self.project.tracks.len() >= TRACK_LIMIT {
            return Err(StudioError::InvalidChannel);
        }
        self.remember();
        let track_id = self.take_id();
        let mut track = generated_track(track_id, role);
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
            return Err(StudioError::InvalidChannel);
        }

        self.remember();
        self.project.tracks.remove(index);
        for track in &mut self.project.tracks {
            track
                .modulators
                .retain(|modulator| modulator.source_track_id != Some(track_id));
        }
        self.project
            .edits
            .retain_mut(|edit| edit.action.retain_after_track_deletion(track_id));
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
        self.remember();
        project.version = self.project.version + 1;
        self.project = project;
        Ok(())
    }

    pub(crate) fn create_midi_clip(
        &mut self,
        track_id: u64,
        spec: &MidiClipSpec,
    ) -> Result<u64, StudioError> {
        let role = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.role)
            .ok_or(StudioError::UnknownTrack)?;
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
        self.remember();
        let track = self
            .project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id && track.role == role)
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

    pub(crate) fn create_audio_clip(
        &mut self,
        track_id: u64,
        mut clip: AudioClip,
    ) -> Result<u64, StudioError> {
        clip.end = clip.start + clip.source_duration;
        if clip.label.trim().is_empty()
            || clip.label.chars().count() > 64
            || !clip.start.is_finite()
            || !clip.source_offset.is_finite()
            || !clip.source_duration.is_finite()
            || !clip.gain.is_finite()
            || clip.start < 0.0
            || clip.source_offset < 0.0
            || !(0.001..=16.0).contains(&clip.source_duration)
            || clip.end > self.project.duration
            || !(0.0..=2.0).contains(&clip.gain)
            || !std::path::Path::new(&clip.asset).is_absolute()
            || !clip.asset.ends_with(".wav")
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let track_index = self
            .project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        if !track_effects_fit(&self.project.tracks[track_index], true) {
            return Err(StudioError::EffectCapacity);
        }
        let id = self.take_id();
        clip.id = id;
        clip.label = clip.label.trim().to_owned();
        self.remember();
        self.project.tracks[track_index].audio_clips.push(clip);
        self.project.tracks[track_index]
            .audio_clips
            .sort_by(|left, right| left.start.total_cmp(&right.start));
        self.project.version += 1;
        Ok(id)
    }

    pub(crate) fn delete_audio_clip(
        &mut self,
        track_id: u64,
        clip_id: u64,
        selection_start: f32,
        selection_end: f32,
    ) -> Result<(), StudioError> {
        if selection_end <= selection_start {
            return Err(StudioError::InvalidSelection);
        }
        let track_index = self
            .project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        let clip_index = self.project.tracks[track_index]
            .audio_clips
            .iter()
            .position(|clip| clip.id == clip_id)
            .ok_or(StudioError::UnknownSoundTool)?;
        let clip = &self.project.tracks[track_index].audio_clips[clip_index];
        if clip.start < selection_start || clip.end > selection_end {
            return Err(StudioError::InvalidSelection);
        }
        self.remember();
        self.project.tracks[track_index]
            .audio_clips
            .remove(clip_index);
        self.project.version += 1;
        Ok(())
    }

    pub(crate) fn slice_audio_clip(
        &mut self,
        track_id: u64,
        clip_id: u64,
        spec: AudioClipSliceSpec<'_>,
    ) -> Result<u64, StudioError> {
        let source = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?
            .audio_clips
            .iter()
            .find(|clip| clip.id == clip_id)
            .ok_or(StudioError::UnknownSoundTool)?
            .clone();
        if spec.source_start < 0.0
            || spec.source_end <= spec.source_start
            || spec.source_end > source.source_duration
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let source_offset = if source.reversed {
            source.source_offset + source.source_duration - spec.source_end
        } else {
            source.source_offset + spec.source_start
        };
        self.create_audio_clip(
            track_id,
            AudioClip {
                id: 0,
                label: spec.label.to_owned(),
                start: spec.destination_start,
                end: 0.0,
                asset: source.asset,
                source_offset,
                source_duration: spec.source_end - spec.source_start,
                gain: source.gain,
                reversed: spec.reversed ^ source.reversed,
            },
        )
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
        self.remember();
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
        self.remember();
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
        if crate::surge::effect_type_index(name).is_none()
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
        self.remember();
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
        self.remember();
        project.version = self.project.version + 1;
        self.project = project;
        Ok(())
    }

    pub(crate) fn set_tempo(&mut self, bpm: u16) -> Result<(), StudioError> {
        if !(60..=180).contains(&bpm) {
            return Err(StudioError::InvalidSoundTool);
        }
        self.remember();
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
        if !valid_modulator_target(&self.project.tracks[track_index], spec.target)
            || !matches!(
                spec.shape,
                "sine" | "triangle" | "square" | "random" | "envelope" | "formula"
            )
            || !spec.rate.is_finite()
            || !(0.01..=20.0).contains(&spec.rate)
            || !spec.depth.is_finite()
            || !(0.0..=1.0).contains(&spec.depth)
            || !matches!(spec.trigger, "free" | "midi" | "audio")
            || !spec.attack_ms.is_finite()
            || !(0.0..=1_000.0).contains(&spec.attack_ms)
            || !spec.release_ms.is_finite()
            || !(1.0..=5_000.0).contains(&spec.release_ms)
            || !spec.threshold.is_finite()
            || !(0.0..=1.0).contains(&spec.threshold)
            || !matches!(spec.polarity, "increase" | "decrease")
            || spec.formula.len() > 8_192
            || (spec.shape == "formula" && spec.formula.trim().is_empty())
            || (spec.shape == "formula"
                && (spec.trigger == "audio"
                    || spec
                        .source_track_id
                        .is_some_and(|source| source != track_id)
                    || !is_instrument_modulation_target(spec.target)))
            || (is_instrument_modulation_target(spec.target)
                && spec.trigger != "free"
                && spec
                    .source_track_id
                    .is_some_and(|source| source != track_id))
            || (spec.trigger == "audio" && is_instrument_modulation_target(spec.target))
            || (spec.trigger != "free"
                && !self
                    .project
                    .tracks
                    .iter()
                    .any(|track| track.id == spec.source_track_id.unwrap_or(track_id)))
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let id = self.take_id();
        self.remember();
        self.project.tracks[track_index].modulators.push(Modulator {
            id,
            name: "AI modulation".to_owned(),
            shape: spec.shape.to_owned(),
            rate: spec.rate,
            rate_mode: "hz".to_owned(),
            trigger: spec.trigger.to_owned(),
            source_track_id: (spec.trigger != "free")
                .then_some(spec.source_track_id.unwrap_or(track_id)),
            attack_ms: spec.attack_ms,
            release_ms: spec.release_ms,
            threshold: spec.threshold,
            polarity: spec.polarity.to_owned(),
            formula: spec.formula.to_owned(),
            depth: spec.depth,
            target: spec.target.to_owned(),
            enabled: true,
        });
        self.project.version += 1;
        Ok(id)
    }

    pub(crate) fn record_channel_operation(
        &mut self,
        operation_id: &str,
        action: ChannelOperationAction,
        track_id: u64,
        role: Option<TrackRole>,
    ) -> bool {
        if self
            .project
            .channel_operations
            .iter()
            .any(|operation| operation.operation_id == operation_id)
        {
            return false;
        }
        self.project.channel_operations.push(ChannelOperation {
            operation_id: operation_id.to_owned(),
            action,
            track_id,
            role,
            project_version: self.project.version,
        });
        self.project.compact_edit_log();
        true
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
        let track = project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        configure_track_tool(track, tool, tool_id, clip_id, parameter, value)?;
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
        }) {
            return Err(StudioError::InvalidSoundTool);
        }

        self.remember();
        project.version = self.project.version + 1;
        self.project = project;
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        let Some(mut previous) = self.history.pop() else {
            return false;
        };
        previous.version = self.project.version + 1;
        self.project = previous;
        true
    }

    pub fn reset(&mut self) {
        self.remember();
        let version = self.project.version + 1;
        self.project = Project::initial();
        self.project.version = version;
    }

    fn remember(&mut self) {
        if self.history.len() == HISTORY_LIMIT {
            self.history.remove(0);
        }
        self.history.push(self.project.clone());
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("project ID namespace exhausted");
        id
    }

    fn add_track(&mut self, role: TrackRole) -> u64 {
        let track_id = self.take_id();
        let mut track = generated_track(track_id, role);
        track.instrument.id = self.take_id();
        self.project.tracks.push(track);
        track_id
    }

    #[allow(clippy::too_many_arguments)]
    fn add_midi_clip(
        &mut self,
        track_id: u64,
        role: TrackRole,
        label: &str,
        relative_start: f32,
        relative_end: f32,
        loop_beats: f32,
        notes: &[crate::prompt::MidiNote],
        selection_start: f32,
        selection_end: f32,
    ) -> Result<(), StudioError> {
        if label.trim().is_empty()
            || label.chars().count() > 64
            || !relative_start.is_finite()
            || !relative_end.is_finite()
            || !(0.0..1.0).contains(&relative_start)
            || !(0.0..=1.0).contains(&relative_end)
            || relative_end <= relative_start
            || !loop_beats.is_finite()
            || !(0.25..=16.0).contains(&loop_beats)
            || notes.len() > 32
            || notes.iter().any(|note| {
                !note.time.is_finite()
                    || !(0.0..loop_beats).contains(&note.time)
                    || !note.duration.is_finite()
                    || !(0.0625..=loop_beats).contains(&note.duration)
                    || !note.velocity.is_finite()
                    || !(0.01..=1.0).contains(&note.velocity)
            })
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let track_index = if track_id == 0 {
            self.project
                .tracks
                .iter()
                .rposition(|track| track.role == role)
        } else {
            self.project
                .tracks
                .iter()
                .position(|track| track.id == track_id && track.role == role)
        }
        .ok_or(StudioError::UnknownTrack)?;
        let selection_duration = selection_end - selection_start;
        let start = selection_start + selection_duration * relative_start;
        let end = selection_start + selection_duration * relative_end;
        let clip = Clip {
            id: self.take_id(),
            label: label.trim().to_owned(),
            start,
            end,
            source_start: start,
            style: "generated".to_owned(),
            playback_mode: "loop".to_owned(),
            loop_beats,
            events: notes
                .iter()
                .map(|note| ClipEvent {
                    id: self.take_id(),
                    kind: "note".to_owned(),
                    time: note.time,
                    duration: note.duration,
                    pitch: note.pitch,
                    velocity: note.velocity,
                })
                .collect(),
        };
        self.replace_track_region(track_index, start, end, clip);
        Ok(())
    }

    fn add_modulator(
        &mut self,
        role: TrackRole,
        parameter: &str,
        shape: &str,
        rate: f32,
        depth: f32,
    ) -> Result<(), StudioError> {
        if !self.project.tracks.iter().any(|track| track.role == role) {
            return Err(StudioError::UnknownTrack);
        }
        if !matches!(
            shape,
            "sine" | "triangle" | "square" | "random" | "envelope"
        ) || !rate.is_finite()
            || !(0.01..=20.0).contains(&rate)
            || !depth.is_finite()
            || !(0.0..=1.0).contains(&depth)
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let track_index = role_action_track_index(&self.project, role, Some(parameter))
            .ok_or(StudioError::InvalidSoundTool)?;
        let id = self.take_id();
        self.project.tracks[track_index].modulators.push(Modulator {
            id,
            name: "AI modulation".to_owned(),
            shape: shape.to_owned(),
            rate,
            rate_mode: "hz".to_owned(),
            trigger: "free".to_owned(),
            source_track_id: None,
            attack_ms: 5.0,
            release_ms: 180.0,
            threshold: 0.1,
            polarity: "increase".to_owned(),
            formula: String::new(),
            depth,
            target: parameter.to_owned(),
            enabled: true,
        });
        Ok(())
    }

    fn replace_track_region(
        &mut self,
        track_index: usize,
        start: f32,
        end: f32,
        replacement: Clip,
    ) {
        let clips = std::mem::take(&mut self.project.tracks[track_index].clips);
        let mut retained = Vec::with_capacity(clips.len() + 1);
        for clip in clips {
            if clip.end <= start || clip.start >= end {
                retained.push(clip);
                continue;
            }

            let spans_left_boundary = clip.start < start;
            if spans_left_boundary {
                let mut left = clip.clone();
                left.end = start;
                retained.push(left);
            }
            if clip.end > end {
                let mut right = clip;
                if spans_left_boundary {
                    right.id = self.take_id();
                }
                right.start = end;
                retained.push(right);
            }
        }
        retained.push(replacement);
        retained.sort_by(|left, right| left.start.total_cmp(&right.start));
        self.project.tracks[track_index].clips = retained;
    }
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
            let effect = track
                .effects
                .iter_mut()
                .find(|effect| effect.id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            match parameter {
                "mix" => effect.mix = parse_range(value, 0.0, 1.0)?,
                "cutoff" if effect.cutoff_hz.is_some() => {
                    effect.cutoff_hz = Some(parse_range(
                        value,
                        FILTER_CUTOFF_MIN_HZ,
                        FILTER_CUTOFF_MAX_HZ,
                    )?);
                }
                "resonance" if effect.resonance.is_some() => {
                    effect.resonance = Some(parse_range(
                        value,
                        FILTER_RESONANCE_MIN,
                        FILTER_RESONANCE_MAX,
                    )?);
                }
                "enabled" => effect.enabled = parse_bool(value)?,
                parameter if effect.parameters.contains_key(parameter) => {
                    effect
                        .parameters
                        .insert(parameter.to_owned(), parse_range(value, 0.0, 1.0)?);
                    if !effect
                        .parameter_overrides
                        .iter()
                        .any(|candidate| candidate == parameter)
                    {
                        effect.parameter_overrides.push(parameter.to_owned());
                    }
                }
                _ => return Err(StudioError::InvalidSoundTool),
            }
            if track_effects_fit(track, !track.audio_clips.is_empty()) {
                Ok(())
            } else {
                Err(StudioError::EffectCapacity)
            }
        }
        "modulator" => {
            if parameter == "target" && !valid_modulator_target(track, value) {
                return Err(StudioError::InvalidSoundTool);
            }
            let modulator = track
                .modulators
                .iter_mut()
                .find(|modulator| modulator.id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
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
                "duration" => event.duration = parse_range(value, 0.0625, clip.loop_beats)?,
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

pub(crate) fn track_effects_fit(track: &Track, has_audio_input: bool) -> bool {
    let enabled = track.effects.iter().filter(|effect| effect.enabled).count();
    let input_slots =
        usize::from(has_audio_input && enabled > 0) * crate::surge::AUDIO_INPUT_EFFECT_SLOT_COUNT;
    enabled + input_slots <= crate::surge::SERIAL_EFFECT_SLOT_COUNT
}

fn track_effects_fit_with_added_effect(track: &Track) -> bool {
    let enabled = track.effects.iter().filter(|effect| effect.enabled).count() + 1;
    let input_slots =
        usize::from(!track.audio_clips.is_empty()) * crate::surge::AUDIO_INPUT_EFFECT_SLOT_COUNT;
    enabled + input_slots <= crate::surge::SERIAL_EFFECT_SLOT_COUNT
}

fn validate_clip_fields(
    label: &str,
    start: f32,
    end: f32,
    playback_mode: &str,
    loop_beats: f32,
    notes: &[crate::prompt::MidiNote],
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
            "loop" => !(0.25..=16.0).contains(&loop_beats) || notes.len() > 32,
            "once" => !(0.25..=64.0).contains(&loop_beats) || notes.len() > 128,
            _ => true,
        }
        || notes.iter().any(|note| {
            !note.time.is_finite()
                || !(0.0..loop_beats).contains(&note.time)
                || !note.duration.is_finite()
                || !(0.0625..=loop_beats).contains(&note.duration)
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
            instrument.parameter_overrides.clear();
            instrument.native_overrides.clear();
            apply_instrument_preset_defaults(instrument);
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
        if !crate::surge::instrument_parameters(&instrument.preset)
            .iter()
            .any(|candidate| candidate.id == native_id)
        {
            return Err(StudioError::InvalidSoundTool);
        }
        instrument.native_overrides.insert(native_id, value);
        return Ok(());
    }
    match parameter {
        "attack" => instrument.attack = parse_range(value, 0.0, 1.0)?,
        "decay" => instrument.decay = parse_range(value, 0.0, 1.0)?,
        "sustain" => instrument.sustain = parse_range(value, 0.0, 1.0)?,
        "release" => instrument.release = parse_range(value, 0.0, 1.0)?,
        "cutoff" => instrument.cutoff = parse_range(value, 0.0, 1.0)?,
        "resonance" => instrument.resonance = parse_range(value, 0.0, 1.0)?,
        "pitch" => instrument.pitch = parse_range(value, 0.0, 1.0)?,
        "timbre" => instrument.timbre = parse_range(value, 0.0, 1.0)?,
        "output" => instrument.output = parse_range(value, 0.0, 1.0)?,
        _ => return Err(StudioError::InvalidSoundTool),
    }
    if !instrument.overrides(parameter) {
        instrument.parameter_overrides.push(parameter.to_owned());
    }
    Ok(())
}

pub(crate) fn apply_instrument_preset_defaults(instrument: &mut Instrument) {
    let defaults = match instrument.preset.as_str() {
        "Surge Kick" => Some((0.0, 0.4, 0.0, 0.2, 0.35, 0.15, 0.5, 0.4, 1.0)),
        "Surge Snare" => Some((0.0, 0.38, 0.0, 0.22, 0.82, 0.1, 0.5, 0.78, 1.0)),
        "Surge Closed Hat" => Some((0.0, 0.18, 0.0, 0.08, 0.96, 0.08, 0.5, 1.0, 1.0)),
        "Surge Open Hat" => Some((0.0, 0.42, 0.0, 0.3, 0.94, 0.08, 0.5, 0.95, 1.0)),
        "Surge Crash" => Some((0.0, 0.7, 0.0, 0.62, 0.9, 0.06, 0.5, 0.92, 1.0)),
        "Surge Percussion" => Some((0.0, 0.35, 0.0, 0.2, 0.82, 0.1, 0.5, 0.72, 0.9)),
        _ => None,
    };
    if let Some((attack, decay, sustain, release, cutoff, resonance, pitch, timbre, output)) =
        defaults
    {
        instrument.attack = attack;
        instrument.decay = decay;
        instrument.sustain = sustain;
        instrument.release = release;
        instrument.cutoff = cutoff;
        instrument.resonance = resonance;
        instrument.pitch = pitch;
        instrument.timbre = timbre;
        instrument.output = output;
    } else if let Ok(
        [
            attack,
            decay,
            sustain,
            release,
            cutoff,
            resonance,
            pitch,
            output,
        ],
    ) = crate::surge::instrument_parameter_defaults(&instrument.preset)
    {
        instrument.attack = attack;
        instrument.decay = decay;
        instrument.sustain = sustain;
        instrument.release = release;
        instrument.cutoff = cutoff;
        instrument.resonance = resonance;
        instrument.pitch = pitch;
        instrument.timbre = 0.5;
        instrument.output = output;
    }
}

pub(crate) fn instrument_parameter_names() -> Vec<String> {
    [
        "attack",
        "decay",
        "sustain",
        "release",
        "cutoff",
        "resonance",
        "pitch",
        "timbre",
        "output",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

pub(crate) fn valid_surge_preset(value: &str) -> bool {
    SURGE_PRESETS.contains(&value) || crate::surge_presets::find(value).is_some()
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
    let mut targets = vec![
        ModulationTarget {
            id: "instrument.attack".to_owned(),
            label: "Surge amp envelope attack".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.release".to_owned(),
            label: "Surge amp envelope release".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.decay".to_owned(),
            label: "Surge amp envelope decay".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.sustain".to_owned(),
            label: "Surge amp envelope sustain".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.cutoff".to_owned(),
            label: "Surge filter 1 cutoff".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.resonance".to_owned(),
            label: "Surge filter 1 resonance".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.pitch".to_owned(),
            label: "Surge scene pitch".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 0.1,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.timbre".to_owned(),
            label: "Dedicated drum timbre".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 0.25,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.output".to_owned(),
            label: "Surge oscillator output".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "multiply",
        },
        ModulationTarget {
            id: "track.volume".to_owned(),
            label: "Track volume".to_owned(),
            minimum: 0.0,
            maximum: 1.5,
            scale: 1.0,
            mode: "multiply",
        },
    ];
    for effect in &track.effects {
        targets.push(ModulationTarget {
            id: format!("effect:{}.mix", effect.id),
            label: format!("{} mix", effect.name),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        });
        if effect.cutoff_hz.is_some() {
            targets.push(ModulationTarget {
                id: format!("effect:{}.cutoff", effect.id),
                label: format!("{} cutoff", effect.name),
                minimum: FILTER_CUTOFF_MIN_HZ,
                maximum: FILTER_CUTOFF_MAX_HZ,
                scale: 4.0,
                mode: "exponential",
            });
        }
        if effect.resonance.is_some() {
            targets.push(ModulationTarget {
                id: format!("effect:{}.resonance", effect.id),
                label: format!("{} resonance", effect.name),
                minimum: FILTER_RESONANCE_MIN,
                maximum: FILTER_RESONANCE_MAX,
                scale: 10.0,
                mode: "add",
            });
        }
        for parameter in effect.parameters.keys() {
            targets.push(ModulationTarget {
                id: format!("effect:{}.{}", effect.id, parameter),
                label: format!("{} {}", effect.name, parameter),
                minimum: 0.0,
                maximum: 1.0,
                scale: 1.0,
                mode: "add",
            });
        }
    }
    targets
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

fn is_instrument_modulation_target(target: &str) -> bool {
    target.starts_with("instrument.") || target.starts_with("native:")
}

fn valid_modulator_configuration(
    owner_track_id: u64,
    modulator: &Modulator,
    track_ids: &[u64],
) -> bool {
    let native_target = is_instrument_modulation_target(&modulator.target);
    let source_track_id = modulator.source_track_id.unwrap_or(owner_track_id);
    let source_exists = modulator.trigger == "free" || track_ids.contains(&source_track_id);
    let native_source_is_local =
        !native_target || modulator.trigger == "free" || source_track_id == owner_track_id;
    let audio_target_is_owned_by_daw = modulator.trigger != "audio" || !native_target;
    let formula_is_native = modulator.shape != "formula"
        || (native_target
            && modulator.trigger != "audio"
            && native_source_is_local
            && !modulator.formula.trim().is_empty());
    source_exists && native_source_is_local && audio_target_is_owned_by_daw && formula_is_native
}

fn automation_targets(track: &Track) -> Vec<ModulationTarget> {
    let mut targets = modulation_targets(track);
    for modulator in &track.modulators {
        targets.push(ModulationTarget {
            id: format!("modulator:{}.rate", modulator.id),
            label: format!("{} rate", modulator.name),
            minimum: 0.01,
            maximum: 20.0,
            scale: 1.0,
            mode: "linear",
        });
        targets.push(ModulationTarget {
            id: format!("modulator:{}.depth", modulator.id),
            label: format!("{} depth", modulator.name),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "linear",
        });
    }
    targets
}

pub(crate) fn valid_automation_target(track: &Track, value: &str) -> bool {
    automation_target_range(track, value).is_some()
}

pub(crate) fn automation_target_range(track: &Track, value: &str) -> Option<(f32, f32)> {
    automation_targets(track)
        .into_iter()
        .find(|target| target.id == value)
        .map(|target| (target.minimum, target.maximum))
}

fn role_action_track_index(
    project: &Project,
    role: TrackRole,
    modulator_target: Option<&str>,
) -> Option<usize> {
    project.tracks.iter().rposition(|track| {
        track.role == role
            && modulator_target.is_none_or(|target| valid_modulator_target(track, target))
    })
}

fn demo_track(id: u64, role: TrackRole, name: &str, color: &str) -> Track {
    let mut track = demo_role_track(id, role);
    track.name = name.to_owned();
    track.color = color.to_owned();
    track.clips = vec![clip(
        id + 10,
        match role {
            TrackRole::Drums => "Pocket beat",
            TrackRole::Bass => "Warm pulse",
            TrackRole::Chords => "Four-chord glow",
            TrackRole::Lead => "Lead phrase",
            TrackRole::Texture => "Air layer",
        },
        0.0,
        32.0,
        "foundation",
        role,
    )];
    track
}

fn empty_track(id: u64) -> Track {
    let mut track = generated_track(id, TrackRole::Lead);
    track.name = "Empty Track".to_owned();
    track.instrument.preset = "Init".to_owned();
    track.effects.clear();
    track.modulators.clear();
    track.routing.effect_order.clear();
    track.clips.clear();
    track
}

fn generated_track(id: u64, role: TrackRole) -> Track {
    let instrument_id = tool_id(id, 1);
    Track {
        id,
        name: match role {
            TrackRole::Drums => "AI Drum Voice",
            TrackRole::Bass => "AI Bass",
            TrackRole::Chords => "AI Chords",
            TrackRole::Lead => "AI Lead",
            TrackRole::Texture => "AI Texture",
        }
        .to_owned(),
        role,
        color: match role {
            TrackRole::Drums => "#ffb86b",
            TrackRole::Bass => "#74e0bc",
            TrackRole::Chords => "#8ca9ff",
            TrackRole::Lead => "#d99cff",
            TrackRole::Texture => "#ff91ad",
        }
        .to_owned(),
        volume: 1.0,
        muted: false,
        instrument: Instrument {
            id: instrument_id,
            engine: SURGE_ENGINE.to_owned(),
            preset: "Init".to_owned(),
            attack: 0.0,
            decay: 0.4,
            sustain: 0.7,
            release: 0.2,
            cutoff: 0.5,
            resonance: 0.0,
            pitch: 0.5,
            timbre: 0.5,
            output: 1.0,
            parameter_overrides: Vec::new(),
            native_overrides: BTreeMap::new(),
        },
        effects: Vec::new(),
        modulators: Vec::new(),
        routing: Routing {
            effect_order: Vec::new(),
            output: "master".to_owned(),
        },
        clips: Vec::new(),
        audio_clips: Vec::new(),
    }
}

fn demo_role_track(id: u64, role: TrackRole) -> Track {
    let (name, color, preset, attack, release, cutoff, effect_specs, modulator) = match role {
        TrackRole::Drums => (
            "AI Drum Voice",
            "#ffb86b",
            "Surge Kick",
            0.0,
            0.25,
            0.82,
            vec![("Punch compressor", 0.34)],
            ("Pulse envelope", "envelope", 2.0, 0.12, "instrument.cutoff"),
        ),
        TrackRole::Bass => (
            "AI Bass",
            "#74e0bc",
            "Surge Bass",
            0.02,
            0.3,
            0.45,
            vec![("Low-pass filter", 0.46)],
            ("Bass movement", "sine", 0.25, 0.18, "instrument.cutoff"),
        ),
        TrackRole::Chords => (
            "AI Chords",
            "#8ca9ff",
            "Surge Pad",
            0.36,
            0.62,
            0.58,
            vec![("Chorus", 0.28), ("Room", 0.2)],
            ("Slow bloom", "triangle", 0.125, 0.16, "instrument.cutoff"),
        ),
        TrackRole::Lead => (
            "AI Lead",
            "#d99cff",
            "Surge Lead",
            0.02,
            0.38,
            0.64,
            vec![("Echo", 0.24)],
            ("Lead vibrato", "sine", 5.0, 0.08, "instrument.pitch"),
        ),
        TrackRole::Texture => (
            "AI Texture",
            "#ff91ad",
            "Surge Atmosphere",
            0.58,
            0.74,
            0.7,
            vec![("Shimmer", 0.38)],
            (
                "Atmosphere drift",
                "random",
                0.18,
                0.22,
                "instrument.cutoff",
            ),
        ),
    };

    let instrument_id = tool_id(id, 1);
    let effects = effect_specs
        .into_iter()
        .enumerate()
        .map(|(index, (name, mix))| effect(tool_id(id, index as u64 + 10), name, mix))
        .collect::<Vec<_>>();
    let effect_order = effects.iter().map(|effect| effect.id).collect();

    Track {
        id,
        name: name.to_owned(),
        role,
        color: color.to_owned(),
        volume: match role {
            TrackRole::Drums => 0.78,
            TrackRole::Bass => 0.95,
            TrackRole::Chords => 0.85,
            TrackRole::Lead => 0.56,
            TrackRole::Texture => 0.44,
        },
        muted: false,
        instrument: Instrument {
            id: instrument_id,
            engine: SURGE_ENGINE.to_owned(),
            preset: preset.to_owned(),
            attack,
            decay: 0.4,
            sustain: 0.7,
            release,
            cutoff,
            resonance: 0.18,
            pitch: 0.5,
            timbre: 0.5,
            output: 0.72,
            parameter_overrides: Vec::new(),
            native_overrides: BTreeMap::new(),
        },
        effects,
        modulators: vec![Modulator {
            id: tool_id(id, 50),
            name: modulator.0.to_owned(),
            shape: modulator.1.to_owned(),
            rate: modulator.2,
            rate_mode: "hz".to_owned(),
            trigger: if modulator.1 == "envelope" {
                "midi".to_owned()
            } else {
                "free".to_owned()
            },
            source_track_id: (modulator.1 == "envelope").then_some(id),
            attack_ms: 5.0,
            release_ms: 180.0,
            threshold: 0.1,
            polarity: "increase".to_owned(),
            formula: String::new(),
            depth: modulator.3,
            target: modulator.4.to_owned(),
            enabled: true,
        }],
        routing: Routing {
            effect_order,
            output: "master".to_owned(),
        },
        clips: Vec::new(),
        audio_clips: Vec::new(),
    }
}

fn clip(id: u64, label: &str, start: f32, end: f32, style: &str, role: TrackRole) -> Clip {
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

fn pattern_events(clip_id: u64, role: TrackRole) -> Vec<ClipEvent> {
    let specs: Vec<(&str, f32, f32, u8, f32)> = match role {
        TrackRole::Drums => vec![
            ("note", 0.0, 0.25, 36, 0.92),
            ("note", 1.0, 0.25, 36, 0.84),
            ("note", 2.0, 0.25, 36, 0.88),
            ("note", 3.0, 0.25, 36, 0.84),
        ],
        TrackRole::Bass => vec![
            ("note", 0.0, 0.7, 33, 0.82),
            ("note", 1.0, 0.7, 33, 0.72),
            ("note", 2.0, 0.7, 36, 0.78),
            ("note", 3.0, 0.7, 31, 0.74),
        ],
        TrackRole::Chords => vec![
            ("note", 0.0, 1.85, 57, 0.62),
            ("note", 0.0, 1.85, 60, 0.56),
            ("note", 0.0, 1.85, 64, 0.54),
            ("note", 2.0, 1.85, 53, 0.6),
            ("note", 2.0, 1.85, 57, 0.54),
            ("note", 2.0, 1.85, 60, 0.52),
        ],
        TrackRole::Lead => vec![
            ("note", 0.0, 0.75, 69, 0.72),
            ("note", 1.0, 0.75, 76, 0.75),
            ("note", 2.0, 0.75, 71, 0.72),
            ("note", 3.0, 0.75, 67, 0.66),
        ],
        TrackRole::Texture => vec![("note", 0.0, 3.8, 64, 0.5), ("note", 0.0, 3.4, 71, 0.38)],
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
    let is_filter = name == "Low-pass filter";
    let parameters = effect_parameter_specs(name)
        .iter()
        .map(|spec| (spec.name.to_owned(), spec.default))
        .collect();
    Effect {
        id,
        name: name.to_owned(),
        mix,
        cutoff_hz: is_filter.then_some(FILTER_CUTOFF_DEFAULT_HZ),
        resonance: is_filter.then_some(FILTER_RESONANCE_DEFAULT),
        enabled: true,
        parameters,
        parameter_overrides: Vec::new(),
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

fn role_json(role: Option<TrackRole>) -> String {
    json_string(role.map_or("all", TrackRole::as_str))
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
        assert_eq!(project.tracks[0].instrument.preset, "Surge Kick");
        assert!(
            project.tracks[0].clips[0]
                .events
                .iter()
                .all(|event| event.pitch == 36)
        );
        assert_eq!(project.tracks[1].instrument.preset, "Surge Bass");
        assert_eq!(project.tracks[2].instrument.preset, "Surge Pad");
        assert!(project.tracks.iter().all(|track| !track.clips.is_empty()));
        assert!(project.tracks.iter().all(|track| {
            !track.clips[0].events.is_empty()
                && !track.modulators.is_empty()
                && track.routing.effect_order.len() == track.effects.len()
        }));
        let json = project.to_json();
        assert!(json.contains("Neon First Light"));
        assert!(json.contains("\"routing\""));
        assert!(json.contains("\"playback\":{\"mode\":\"loop\",\"lengthBeats\":4.0}"));
        assert!(
            json.contains("\"source\":\"clips\",\"target\":\"instrument:101\",\"type\":\"midi\"")
        );
        assert!(json.contains(
            "\"source\":\"instrument:101\",\"target\":\"effect:110\",\"type\":\"audio\""
        ));
        assert!(json.contains(
            "\"source\":\"modulator:150\",\"target\":\"instrument.cutoff\",\"type\":\"control\""
        ));
        assert!(
            json.contains("\"source\":\"clips\",\"target\":\"modulator:150\",\"type\":\"midi\"")
        );
    }

    #[test]
    fn surge_presets_and_native_parameters_are_configurable() {
        let mut studio = Studio::new();
        let bass_id = studio.project().tracks[1].id;
        let instrument_id = studio.project().tracks[1].instrument.id;

        studio
            .configure_sound_tool(
                bass_id,
                "instrument",
                instrument_id,
                None,
                "preset",
                "Surge Pad",
            )
            .expect("published Surge preset");
        let instrument = &studio.project().tracks[1].instrument;
        assert_eq!(instrument.engine, SURGE_ENGINE);
        assert_eq!(instrument.preset, "Surge Pad");
        assert!(instrument.parameter_overrides.is_empty());

        studio
            .configure_sound_tool(bass_id, "instrument", instrument_id, None, "cutoff", "0.4")
            .expect("manual Surge parameter");
        assert_eq!(studio.project().tracks[1].instrument.cutoff, 0.4);
        assert_eq!(studio.project().tracks[1].instrument.preset, "Surge Pad");
        studio
            .configure_sound_tool(
                bass_id,
                "instrument",
                instrument_id,
                None,
                "preset",
                "Factory/Leads/Violini Solo",
            )
            .expect("factory Surge preset");
        assert!(
            studio.project().tracks[1]
                .instrument
                .parameter_overrides
                .is_empty()
        );
        assert_eq!(
            studio.configure_sound_tool(
                bass_id,
                "instrument",
                instrument_id,
                None,
                "preset",
                "Unknown",
            ),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(
            studio.configure_sound_tool(
                bass_id,
                "instrument",
                instrument_id,
                None,
                "preset",
                "Factory/Leads/Definitely Missing",
            ),
            Err(StudioError::InvalidSoundTool)
        );
    }

    #[test]
    fn audio_clip_deletion_cannot_escape_the_selection() {
        let mut studio = Studio::new();
        let track_id = studio.project.tracks[0].id;
        studio.project.tracks[0].audio_clips.push(AudioClip {
            id: 9_001,
            label: "Region".to_owned(),
            start: 1.0,
            end: 3.0,
            asset: "/tmp/unused.wav".to_owned(),
            source_offset: 0.0,
            source_duration: 2.0,
            gain: 1.0,
            reversed: false,
        });

        assert_eq!(
            studio.delete_audio_clip(track_id, 9_001, 1.5, 2.5),
            Err(StudioError::InvalidSelection)
        );
        studio
            .delete_audio_clip(track_id, 9_001, 1.0, 3.0)
            .expect("selected audio clip deletion");
        assert!(studio.project.tracks[0].audio_clips.is_empty());
    }

    #[test]
    fn advanced_channels_are_playable_undoable_and_keep_one_output_path() {
        let mut studio = Studio::new();
        let original = studio.project().to_json();
        let track_id = studio.add_channel(TrackRole::Lead).expect("new channel");
        let added = studio.project().tracks.last().expect("added track");
        assert_eq!(added.id, track_id);
        assert_eq!(added.role, TrackRole::Lead);
        assert_eq!(added.volume, 1.0);
        assert_eq!(added.instrument.preset, "Init");
        assert!(added.clips.is_empty());
        assert!(added.effects.is_empty());
        assert!(added.modulators.is_empty());
        assert_eq!(added.routing.output, "master");

        studio.delete_channel(track_id).expect("delete channel");
        assert_eq!(studio.project().tracks.len(), 3);
        assert!(studio.undo());
        assert!(
            studio
                .project()
                .tracks
                .iter()
                .any(|track| track.id == track_id)
        );
        assert!(studio.undo());
        let mut restored = studio.project().clone();
        restored.version = 1;
        assert_eq!(restored.to_json(), original);

        studio.delete_channel(2).expect("delete bass");
        studio.delete_channel(3).expect("delete chords");
        assert_eq!(studio.delete_channel(1), Err(StudioError::InvalidChannel));
    }

    #[test]
    fn sound_tools_are_configurable_and_undoable() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let instrument_id = bass.instrument.id;
        let effect_id = bass.effects[0].id;
        let modulator_id = bass.modulators[0].id;
        let clip_id = bass.clips[0].id;
        let event_id = bass.clips[0].events[0].id;
        let chords_id = studio.project().tracks[2].id;
        let later_effect_id = studio.project().tracks[2].routing.effect_order[1];

        studio
            .configure_sound_tool(bass_id, "instrument", instrument_id, None, "attack", "0.12")
            .expect("configurable instrument");
        studio
            .configure_sound_tool(bass_id, "instrument", instrument_id, None, "cutoff", "0.41")
            .expect("configurable Surge cutoff");
        studio
            .configure_sound_tool(
                bass_id,
                "instrument",
                instrument_id,
                None,
                "resonance",
                "0.27",
            )
            .expect("configurable Surge resonance");
        studio
            .configure_sound_tool(bass_id, "instrument", instrument_id, None, "pitch", "0.55")
            .expect("configurable Surge pitch");
        studio
            .configure_sound_tool(bass_id, "effect", effect_id, None, "mix", "0.72")
            .expect("configurable effect");
        studio
            .configure_sound_tool(bass_id, "effect", effect_id, None, "cutoff", "640")
            .expect("configurable filter cutoff");
        studio
            .configure_sound_tool(bass_id, "effect", effect_id, None, "resonance", "8.5")
            .expect("configurable filter resonance");
        studio
            .configure_sound_tool(
                bass_id,
                "modulator",
                modulator_id,
                None,
                "target",
                "track.volume",
            )
            .expect("configurable modulator");
        studio
            .configure_sound_tool(
                bass_id,
                "modulator",
                modulator_id,
                None,
                "rateMode",
                "tempo",
            )
            .expect("tempo-synced modulator");
        studio
            .configure_sound_tool(bass_id, "modulator", modulator_id, None, "trigger", "midi")
            .expect("MIDI-triggered modulator");
        studio
            .configure_sound_tool(bass_id, "event", event_id, Some(clip_id), "pitch", "40")
            .expect("configurable clip event");
        studio
            .configure_sound_tool(
                bass_id,
                "event",
                event_id,
                Some(clip_id),
                "duration",
                "0.0625",
            )
            .expect("precise clip duration");
        studio
            .configure_sound_tool(chords_id, "routing", later_effect_id, None, "position", "0")
            .expect("configurable effect routing");

        let bass = &studio.project().tracks[1];
        assert_eq!(bass.instrument.attack, 0.12);
        assert_eq!(bass.instrument.cutoff, 0.41);
        assert_eq!(bass.instrument.resonance, 0.27);
        assert_eq!(bass.instrument.pitch, 0.55);
        assert_eq!(bass.effects[0].mix, 0.72);
        assert_eq!(bass.effects[0].cutoff_hz, Some(640.0));
        assert_eq!(bass.effects[0].resonance, Some(8.5));
        assert_eq!(bass.modulators[0].target, "track.volume");
        assert_eq!(bass.modulators[0].rate_mode, "tempo");
        assert_eq!(bass.modulators[0].trigger, "midi");
        assert_eq!(bass.clips[0].events[0].pitch, 40);
        assert_eq!(bass.clips[0].events[0].duration, 0.0625);
        assert!(studio.to_json().contains("\"duration\":0.0625"));
        assert!(studio.to_json().contains(&format!(
            "\"source\":\"clips\",\"target\":\"modulator:{modulator_id}\",\"type\":\"midi\""
        )));
        assert_eq!(
            studio.project().tracks[2].routing.effect_order[0],
            later_effect_id
        );
        assert!(studio.undo());
        assert_ne!(
            studio.project().tracks[2].routing.effect_order[0],
            later_effect_id
        );
    }

    #[test]
    fn modulator_updates_reject_invalid_cross_field_transitions_atomically() {
        let mut studio = Studio::new();
        let owner_id = studio.project().tracks[1].id;
        let other_id = studio.project().tracks[2].id;
        let modulator_id = studio.project().tracks[1].modulators[0].id;

        let before_audio = studio.project().to_json();
        assert_eq!(
            studio.configure_sound_tool(
                owner_id,
                "modulator",
                modulator_id,
                None,
                "trigger",
                "audio",
            ),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(studio.project().to_json(), before_audio);

        studio
            .configure_sound_tool(owner_id, "modulator", modulator_id, None, "trigger", "midi")
            .expect("local MIDI source");
        let before_cross_track = studio.project().to_json();
        assert_eq!(
            studio.configure_sound_tool(
                owner_id,
                "modulator",
                modulator_id,
                None,
                "sourceTrackId",
                &other_id.to_string(),
            ),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(studio.project().to_json(), before_cross_track);

        studio
            .configure_sound_tool(
                owner_id,
                "modulator",
                modulator_id,
                None,
                "target",
                "track.volume",
            )
            .expect("DAW-owned target");
        studio
            .configure_sound_tool(
                owner_id,
                "modulator",
                modulator_id,
                None,
                "formula",
                "function process(state) state.output = 1 return state end",
            )
            .expect("formula source can be prepared before selecting its shape");
        let before_formula = studio.project().to_json();
        assert_eq!(
            studio.configure_sound_tool(
                owner_id,
                "modulator",
                modulator_id,
                None,
                "shape",
                "formula",
            ),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(studio.project().to_json(), before_formula);
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
        assert_eq!(
            studio.create_audio_clip(
                track_id,
                AudioClip {
                    id: 0,
                    label: "Needs input slot".to_owned(),
                    start: 0.0,
                    end: 0.0,
                    asset: "/tmp/effect-capacity.wav".to_owned(),
                    source_offset: 0.0,
                    source_duration: 1.0,
                    gain: 1.0,
                    reversed: false,
                },
            ),
            Err(StudioError::EffectCapacity)
        );

        studio
            .configure_sound_tool(
                track_id,
                "effect",
                studio.project().tracks[0].effects[0].id,
                None,
                "enabled",
                "false",
            )
            .expect("release one slot");
        studio
            .create_audio_clip(
                track_id,
                AudioClip {
                    id: 0,
                    label: "Fits reserved input".to_owned(),
                    start: 0.0,
                    end: 0.0,
                    asset: "/tmp/effect-capacity.wav".to_owned(),
                    source_offset: 0.0,
                    source_duration: 1.0,
                    gain: 1.0,
                    reversed: false,
                },
            )
            .expect("audio input reservation");
        let disabled_id = studio.project().tracks[0].effects[0].id;
        assert_eq!(
            studio.configure_sound_tool(track_id, "effect", disabled_id, None, "enabled", "true",),
            Err(StudioError::EffectCapacity)
        );
    }

    #[test]
    fn slicing_reversed_audio_uses_playback_relative_coordinates() {
        let sample_rate = crate::audio_analysis::SAMPLE_RATE as usize;
        let path =
            std::env::temp_dir().join(format!("daw-ai-reversed-slice-{}.wav", std::process::id()));
        let samples = (0..sample_rate)
            .map(|index| index as f32 / sample_rate as f32 * 0.8 - 0.4)
            .collect::<Vec<_>>();
        std::fs::write(&path, crate::audio_analysis::wav_bytes(&samples))
            .expect("reversed slice fixture");

        let mut studio = Studio::from_project(Project::initial());
        let track_id = studio.project().tracks[0].id;
        studio.project.tracks[0].volume = 1.0;
        let source_id = studio
            .create_audio_clip(
                track_id,
                AudioClip {
                    id: 0,
                    label: "Reversed source".to_owned(),
                    start: 0.0,
                    end: 0.0,
                    asset: path.to_string_lossy().into_owned(),
                    source_offset: 0.0,
                    source_duration: 1.0,
                    gain: 1.0,
                    reversed: true,
                },
            )
            .expect("source clip");
        studio
            .slice_audio_clip(
                track_id,
                source_id,
                AudioClipSliceSpec {
                    label: "Audible beginning",
                    source_start: 0.0,
                    source_end: 0.25,
                    destination_start: 1.0,
                    reversed: false,
                },
            )
            .expect("reversed slice");

        let sliced = &studio.project().tracks[0].audio_clips[1];
        assert!((sliced.source_offset - 0.75).abs() < 0.000_01);
        assert!(sliced.reversed);
        let rendered =
            crate::audio_analysis::render_region(studio.project(), &[track_id], 0.0, 1.25)
                .expect("reversed clips render");
        let quarter = sample_rate / 4;
        assert_eq!(
            crate::audio_analysis::pcm_bytes(&rendered.samples[..quarter]),
            crate::audio_analysis::pcm_bytes(&rendered.samples[sample_rate..sample_rate + quarter])
        );

        std::fs::remove_file(path).expect("remove reversed slice fixture");
    }

    #[test]
    fn publishes_and_accepts_every_modulation_target() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let modulator_id = bass.modulators[0].id;
        let targets = modulation_targets(bass)
            .into_iter()
            .map(|target| target.id)
            .collect::<Vec<_>>();

        assert!(targets.contains(&"instrument.attack".to_owned()));
        assert!(targets.contains(&"instrument.release".to_owned()));
        assert!(targets.contains(&"instrument.cutoff".to_owned()));
        assert!(targets.contains(&"instrument.pitch".to_owned()));
        assert!(targets.contains(&"instrument.resonance".to_owned()));
        assert!(targets.contains(&"track.volume".to_owned()));
        assert!(targets.contains(&"effect:210.mix".to_owned()));
        assert!(targets.contains(&"effect:210.cutoff".to_owned()));
        assert!(targets.contains(&"effect:210.resonance".to_owned()));
        assert!(targets.contains(&"effect:210.lowGain".to_owned()));

        studio
            .configure_sound_tool(bass_id, "effect", 210, None, "lowGain", "0.72")
            .expect("detailed effect parameter");
        assert_eq!(
            studio.project().tracks[1].effects[0].parameters["lowGain"],
            0.72
        );
        assert!(
            studio.project().tracks[1].effects[0]
                .parameter_overrides
                .contains(&"lowGain".to_owned())
        );

        for target in &targets {
            studio
                .configure_sound_tool(bass_id, "modulator", modulator_id, None, "target", target)
                .expect("published target must be accepted by validation");
        }
        let json = studio.to_json();
        assert!(json.contains("\"modulationTargets\""));
        for target in targets {
            assert!(json.contains(&json_string(&target)));
        }
    }

    #[test]
    fn publishes_and_validates_parameter_automation_targets() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let modulator_id = bass.modulators[0].id;
        let targets = automation_targets(bass)
            .into_iter()
            .map(|target| target.id)
            .collect::<Vec<_>>();
        assert!(targets.contains(&"track.volume".to_owned()));
        assert!(targets.contains(&"effect:210.cutoff".to_owned()));
        assert!(targets.contains(&format!("modulator:{modulator_id}.rate")));
        assert!(targets.contains(&format!("modulator:{modulator_id}.depth")));
        assert!(studio.to_json().contains("\"automationTargets\""));

        let valid = crate::prompt::EditPlan {
            summary: "Raised the bass level".to_owned(),
            action: Action::Timed {
                start: 0.25,
                end: 0.75,
                action: Box::new(Action::Automation {
                    track_id: bass_id,
                    parameter: "track.volume".to_owned(),
                    curve: "linear",
                    points: vec![
                        crate::prompt::AutomationPoint {
                            time: 0.0,
                            value: 0.1,
                        },
                        crate::prompt::AutomationPoint {
                            time: 1.0,
                            value: 1.4,
                        },
                    ],
                    target: TrackRole::Bass,
                }),
            },
        };
        studio
            .apply_plan(0.0, 8.0, "raise the bass through the transition", valid)
            .expect("published automation target");
        let saved = studio.project().to_json();
        assert!(saved.contains("\"type\":\"timed\""));
        assert!(saved.contains("\"type\":\"automation\""));

        let invalid = crate::prompt::EditPlan {
            summary: "Invalid level".to_owned(),
            action: Action::Automation {
                track_id: bass_id,
                parameter: "track.volume".to_owned(),
                curve: "linear",
                points: vec![
                    crate::prompt::AutomationPoint {
                        time: 0.0,
                        value: 0.0,
                    },
                    crate::prompt::AutomationPoint {
                        time: 1.0,
                        value: 2.0,
                    },
                ],
                target: TrackRole::Bass,
            },
        };
        assert!(
            studio
                .apply_plan(0.0, 8.0, "raise it too far", invalid)
                .is_err()
        );
        assert_eq!(studio.project().to_json(), saved);
    }

    #[test]
    fn deleting_a_track_prunes_only_its_owned_automation() {
        let mut studio = Studio::new();
        let bass_id = studio.project().tracks[1].id;
        studio
            .apply_plan(
                0.0,
                4.0,
                "automate the bass and change tempo",
                crate::prompt::EditPlan {
                    summary: "Automated bass and changed tempo".to_owned(),
                    action: Action::Compound {
                        actions: vec![
                            Action::Automation {
                                track_id: bass_id,
                                parameter: "track.volume".to_owned(),
                                curve: "linear",
                                points: vec![
                                    crate::prompt::AutomationPoint {
                                        time: 0.0,
                                        value: 0.2,
                                    },
                                    crate::prompt::AutomationPoint {
                                        time: 1.0,
                                        value: 1.2,
                                    },
                                ],
                                target: TrackRole::Bass,
                            },
                            Action::Tempo { bpm: 128 },
                        ],
                    },
                },
            )
            .expect("valid compound edit");

        studio
            .delete_channel(bass_id)
            .expect("delete automated bass");
        assert!(!studio.to_json().contains("\"type\":\"automation\""));
        assert!(studio.to_json().contains("\"type\":\"tempo\""));
        assert!(studio.undo());
        assert!(studio.to_json().contains("\"type\":\"automation\""));
        assert!(
            studio
                .project()
                .tracks
                .iter()
                .any(|track| track.id == bass_id)
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
    fn generated_modulators_use_collision_free_sound_tool_ids() {
        let mut studio = Studio::new();
        let drums = &studio.project().tracks[0];
        let drums_id = drums.id;
        let seeded_modulator_id = drums.modulators[0].id;
        let seeded_depth = drums.modulators[0].depth;

        for index in 0..30 {
            let plan = crate::prompt::EditPlan {
                action: Action::Modulator {
                    parameter: "instrument.cutoff".to_owned(),
                    shape: "sine",
                    rate: 0.5,
                    depth: 0.2,
                    target: TrackRole::Drums,
                },
                summary: format!("Added drum modulator {index}"),
            };
            studio
                .apply_plan(0.0, 4.0, "add drum modulation", plan)
                .expect("valid drum modulator");
        }

        let mut sound_tool_ids = Vec::new();
        for track in &studio.project().tracks {
            sound_tool_ids.push(track.instrument.id);
            sound_tool_ids.extend(track.effects.iter().map(|effect| effect.id));
            sound_tool_ids.extend(track.modulators.iter().map(|modulator| modulator.id));
        }
        let unique_ids = sound_tool_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique_ids.len(), sound_tool_ids.len());

        let newest_modulator_id = studio.project().tracks[0]
            .modulators
            .last()
            .expect("generated modulator")
            .id;
        studio
            .configure_sound_tool(
                drums_id,
                "modulator",
                newest_modulator_id,
                None,
                "depth",
                "0.73",
            )
            .expect("newest modulator remains addressable");
        let drums = &studio.project().tracks[0];
        assert_eq!(
            drums
                .modulators
                .iter()
                .find(|modulator| modulator.id == newest_modulator_id)
                .expect("newest modulator by stable ID")
                .depth,
            0.73
        );
        assert_eq!(
            drums
                .modulators
                .iter()
                .find(|modulator| modulator.id == seeded_modulator_id)
                .expect("seeded modulator by stable ID")
                .depth,
            seeded_depth
        );
    }

    #[test]
    fn role_actions_target_the_latest_matching_track() {
        let mut studio = Studio::new();
        let original_bass_id = studio.project().tracks[1].id;
        let plan = crate::prompt::EditPlan {
            action: Action::Compound {
                actions: vec![
                    Action::AddTrack {
                        role: TrackRole::Bass,
                    },
                    Action::Instrument {
                        preset: "Surge Lead",
                        target: TrackRole::Bass,
                    },
                    Action::Modulator {
                        parameter: "instrument.attack".to_owned(),
                        shape: "triangle",
                        rate: 0.5,
                        depth: 0.3,
                        target: TrackRole::Bass,
                    },
                ],
            },
            summary: "Added a moving saw bass".to_owned(),
        };
        studio
            .apply_plan(4.0, 8.0, "add a moving saw bass", plan)
            .expect("configure newly added duplicate role");

        let basses = studio
            .project()
            .tracks
            .iter()
            .filter(|track| track.role == TrackRole::Bass)
            .collect::<Vec<_>>();
        assert_eq!(basses.len(), 2);
        assert_eq!(basses[0].id, original_bass_id);
        assert_eq!(basses[0].instrument.preset, "Surge Bass");
        assert_eq!(basses[0].modulators.len(), 1);
        assert_eq!(basses[1].instrument.preset, "Surge Lead");
        assert_eq!(basses[1].modulators.len(), 1);
        assert_eq!(basses[1].modulators[0].target, "instrument.attack");

        let later_target = "instrument.output".to_owned();
        let plan = crate::prompt::EditPlan {
            action: Action::Modulator {
                parameter: later_target.clone(),
                shape: "sine",
                rate: 0.5,
                depth: 0.2,
                target: TrackRole::Bass,
            },
            summary: "Modulated the later bass effect".to_owned(),
        };
        studio
            .apply_plan(4.0, 8.0, "move the later bass filter", plan)
            .expect("resolve a stable effect on a later duplicate role");
        let basses = studio
            .project()
            .tracks
            .iter()
            .filter(|track| track.role == TrackRole::Bass)
            .collect::<Vec<_>>();
        assert_eq!(basses[0].modulators.len(), 1);
        assert_eq!(basses[1].modulators.last().unwrap().target, later_target);
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
    fn applies_stable_id_sound_tool_actions() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let clip_id = bass.clips[0].id;
        let event_id = bass.clips[0].events[0].id;
        let plan = crate::prompt::EditPlan {
            action: Action::Configure {
                track_id: bass.id,
                target: TrackRole::Bass,
                tool: "event",
                tool_id: event_id,
                clip_id: Some(clip_id),
                parameter: "velocity",
                value: "0.5".to_owned(),
            },
            summary: "Adjusted the bass event".to_owned(),
        };
        studio
            .apply_plan(0.0, 4.0, "soften the first bass event", plan)
            .expect("valid stable-ID action");
        assert_eq!(studio.project().tracks[1].clips[0].events[0].velocity, 0.5);
        assert!(
            studio
                .project()
                .to_json()
                .contains("\"type\":\"configure\"")
        );
    }

    #[test]
    fn composes_genre_prompt_from_generic_midi_and_sound_tools() {
        let mut studio = Studio::new();
        let original_bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Bass)
            .expect("seeded bass");
        let original_bass_id = original_bass.id;
        let original_bass_modulator = original_bass.modulators[0].clone();
        let summary = studio
            .apply_prompt(8.0, 16.0, "insert a dubstep drop here")
            .expect("valid edit");

        assert!(summary.contains("half-time drums"));
        assert_eq!(studio.project().tracks.len(), 4);
        assert_eq!(studio.project().edits.len(), 1);
        let drums = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Drums)
            .expect("drum track");
        let drum_clip = drums
            .clips
            .iter()
            .find(|clip| clip.label == "Half-time drums")
            .expect("authored drum MIDI");
        assert_eq!((drum_clip.start, drum_clip.end), (8.0, 16.0));
        assert!(drum_clip.events.iter().all(|event| event.kind == "note"));
        assert!(drum_clip.events.iter().any(|event| event.pitch == 36));
        assert!(drum_clip.events.iter().any(|event| event.pitch == 38));
        assert!(drum_clip.events.iter().any(|event| event.pitch == 41));
        assert!(drum_clip.events.iter().any(|event| event.pitch == 49));

        let original_bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.id == original_bass_id)
            .expect("original bass");
        assert_eq!(original_bass.instrument.preset, "Surge Bass");
        assert_eq!(original_bass.modulators[0].id, original_bass_modulator.id);
        assert_eq!(
            original_bass.modulators[0].shape,
            original_bass_modulator.shape
        );
        let bass_rest = original_bass
            .clips
            .iter()
            .find(|clip| clip.label == "Bass rest")
            .expect("old bass is cleared in the drop region");
        assert_eq!((bass_rest.start, bass_rest.end), (8.0, 16.0));
        assert!(bass_rest.events.is_empty());
        assert!(
            original_bass
                .clips
                .iter()
                .filter(|clip| clip.start < 16.0 && clip.end > 8.0)
                .all(|clip| clip.events.is_empty())
        );

        let bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| {
                track
                    .clips
                    .iter()
                    .any(|clip| clip.label == "Syncopated bass")
            })
            .expect("bass track");
        let drop_bass_id = bass.id;
        assert_eq!(bass.instrument.preset, "Surge Bass");
        assert!(
            bass.clips
                .iter()
                .any(|clip| clip.label == "Syncopated bass")
        );
        let wobble = bass.modulators.last().expect("authored bass modulation");
        assert_eq!(wobble.target, "instrument.cutoff");
        assert_eq!(wobble.shape, "square");
        assert_eq!(wobble.rate, 2.0);
        assert_eq!(wobble.depth, 0.72);
        assert_eq!(bass.modulators.len(), 1);
        assert_eq!(wobble.name, "AI modulation");
        let wobble_id = wobble.id;

        let action_json = studio.project().to_json();
        assert!(action_json.contains("\"type\":\"midi-clip\""));
        assert!(action_json.contains("\"type\":\"add-track\""));
        assert!(action_json.contains("\"type\":\"modulator\""));
        assert!(!action_json.contains("\"type\":\"drop\""));

        studio
            .apply_prompt(8.0, 16.0, "make the drop hit harder")
            .expect("valid refinement");
        let bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.id == drop_bass_id)
            .expect("refined bass");
        assert_eq!(studio.project().tracks.len(), 4);
        assert_eq!(bass.modulators.len(), 1);
        assert_eq!(bass.modulators.last().expect("wobble").id, wobble_id);
        assert_eq!(
            bass.clips
                .iter()
                .filter(|clip| clip.label == "Syncopated bass")
                .count(),
            1
        );
        let action_json = studio.project().to_json();
        assert_eq!(action_json.matches("\"type\":\"gain\"").count(), 1);
        assert!(action_json.contains("\"type\":\"configure\""));

        assert!(studio.undo());
        assert_eq!(studio.project().edits.len(), 1);
        assert!(studio.undo());
        assert_eq!(studio.project().tracks.len(), 3);
        assert!(studio.project().edits.is_empty());
        assert!(!studio.undo());
    }

    #[test]
    fn genre_refinement_reenables_its_region_owned_modulator() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(8.0, 16.0, "insert a dubstep drop here")
            .expect("valid edit");
        let drop_bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| {
                track
                    .clips
                    .iter()
                    .any(|clip| clip.label == "Syncopated bass")
            })
            .expect("drop bass");
        let track_id = drop_bass.id;
        let modulator_id = drop_bass.modulators.last().expect("drop modulation").id;

        studio
            .configure_sound_tool(
                track_id,
                "modulator",
                modulator_id,
                None,
                "enabled",
                "false",
            )
            .expect("disable drop modulation");
        studio
            .apply_prompt(8.0, 16.0, "make the drop hit harder")
            .expect("valid refinement");

        let drop_bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("same drop bass");
        assert!(
            drop_bass
                .modulators
                .iter()
                .find(|modulator| modulator.id == modulator_id)
                .expect("same drop modulation")
                .enabled
        );
    }

    #[test]
    fn genre_plan_targets_role_tracks_with_material_in_the_selection() {
        let mut studio = Studio::new();
        let original_bass_id = studio.project().tracks[1].id;
        let original_drums_id = studio.project().tracks[0].id;
        studio
            .apply_prompt(20.0, 24.0, "add a bass")
            .expect("later bass part");
        studio
            .apply_prompt(20.0, 24.0, "add drums")
            .expect("later drum part");
        let later_bass_id = studio
            .project()
            .tracks
            .iter()
            .rfind(|track| track.role == TrackRole::Bass)
            .expect("later bass")
            .id;
        let later_drums_id = studio
            .project()
            .tracks
            .iter()
            .rfind(|track| track.role == TrackRole::Drums)
            .expect("later drums")
            .id;

        studio
            .apply_prompt(8.0, 16.0, "insert a dubstep drop here")
            .expect("drop over the original parts");

        let track = |id| {
            studio
                .project()
                .tracks
                .iter()
                .find(|track| track.id == id)
                .expect("track by ID")
        };
        assert!(
            track(original_bass_id)
                .clips
                .iter()
                .any(|clip| clip.label == "Bass rest" && clip.events.is_empty())
        );
        assert!(
            !track(later_bass_id)
                .clips
                .iter()
                .any(|clip| clip.label == "Bass rest")
        );
        assert!(
            track(original_drums_id)
                .clips
                .iter()
                .any(|clip| clip.label == "Half-time drums")
        );
        assert!(
            !track(later_drums_id)
                .clips
                .iter()
                .any(|clip| clip.label == "Half-time drums")
        );
    }

    #[test]
    fn midi_removal_clears_instead_of_recomposing_the_selection() {
        let mut studio = Studio::new();
        let bass_id = studio.project().tracks[1].id;
        let summary = studio
            .apply_prompt(8.0, 16.0, "remove the bass MIDI clip")
            .expect("clear bass MIDI");

        assert!(summary.contains("Cleared"));
        let bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.id == bass_id)
            .expect("original bass");
        let rest = bass
            .clips
            .iter()
            .find(|clip| clip.label == "AI MIDI rest")
            .expect("silent replacement");
        assert_eq!((rest.start, rest.end), (8.0, 16.0));
        assert!(rest.events.is_empty());
        assert!(matches!(
            &studio.project().edits[0].action,
            Action::MidiClip {
                track_id,
                target: TrackRole::Bass,
                notes,
                ..
            } if *track_id == bass_id && notes.is_empty()
        ));
    }

    #[test]
    fn midi_prompt_creates_a_missing_role_before_writing_notes() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(0.0, 4.0, "add a lead MIDI clip")
            .expect("create and author a missing lead");

        let lead = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Lead)
            .expect("lead track");
        assert!(lead.clips.iter().any(|clip| clip.label == "AI MIDI clip"));
        assert!(matches!(
            studio.project().edits[0].action,
            Action::Compound { ref actions }
                if matches!(actions.as_slice(), [Action::AddTrack { role: TrackRole::Lead }, Action::MidiClip { target: TrackRole::Lead, .. }])
        ));
    }

    #[test]
    fn midi_clip_replaces_only_its_relative_selection_region() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(0.0, 8.0, "add a lead MIDI clip")
            .expect("existing lead");
        let lead_id = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Lead)
            .expect("lead track")
            .id;
        let plan = crate::prompt::EditPlan {
            action: Action::MidiClip {
                track_id: lead_id,
                target: TrackRole::Lead,
                label: "Replacement MIDI".to_owned(),
                start: 0.4,
                end: 1.0,
                loop_beats: 4.0,
                notes: vec![crate::prompt::MidiNote {
                    time: 0.0,
                    duration: 1.0,
                    pitch: 72,
                    velocity: 0.8,
                }],
            },
            summary: "Rewrote the lead MIDI".to_owned(),
        };
        studio
            .apply_plan(2.0, 6.0, "rewrite the lead MIDI", plan)
            .expect("MIDI replacement over existing lead");

        let clips = &studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Lead)
            .expect("lead track")
            .clips;
        assert_eq!(clips.len(), 3);
        assert_eq!(clips[0].label, "AI MIDI clip");
        assert!((clips[0].end - 3.6).abs() < 0.001);
        assert_eq!(clips[1].label, "Replacement MIDI");
        assert!((clips[1].start - 3.6).abs() < 0.001);
        assert_eq!(clips[1].end, 6.0);
        assert!((clips[1].source_start - 3.6).abs() < 0.001);
        assert_eq!(clips[2].label, "AI MIDI clip");
        assert_eq!((clips[2].start, clips[2].end), (6.0, 8.0));
        assert_eq!(clips[2].source_start, 0.0);
        assert_ne!(clips[0].id, clips[2].id);
        assert_eq!(clips[0].events[0].id, clips[2].events[0].id);
        assert!(clips.windows(2).all(|pair| pair[0].end <= pair[1].start));
    }

    #[test]
    fn rejects_midi_note_duration_longer_than_its_loop() {
        let mut studio = Studio::new();
        let bass_id = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Bass)
            .expect("bass track")
            .id;
        let before = studio.to_json();
        let plan = crate::prompt::EditPlan {
            action: Action::MidiClip {
                track_id: bass_id,
                target: TrackRole::Bass,
                label: "Unsafe short loop".to_owned(),
                start: 0.0,
                end: 1.0,
                loop_beats: 0.25,
                notes: vec![crate::prompt::MidiNote {
                    time: 0.0,
                    duration: 16.0,
                    pitch: 29,
                    velocity: 1.0,
                }],
            },
            summary: "Wrote a short bass loop".to_owned(),
        };

        assert_eq!(
            studio.apply_plan(0.0, 4.0, "write a short bass loop", plan),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(studio.to_json(), before);
    }

    #[test]
    fn compound_actions_commit_only_after_sequential_validation() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(0.0, 8.0, "add a lead MIDI clip")
            .expect("existing lead");
        let lead = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Lead)
            .expect("lead track");
        let track_id = lead.id;
        let clip_id = lead.clips[0].id;
        let event_id = lead.clips[0].events[0].id;
        let before = studio.to_json();
        let before_next_id = studio.next_id;
        let before_history = studio.history.len();
        let stale_configuration = crate::prompt::EditPlan {
            action: Action::Compound {
                actions: vec![
                    Action::MidiClip {
                        track_id,
                        target: TrackRole::Lead,
                        label: "Replacement MIDI".to_owned(),
                        start: 0.0,
                        end: 1.0,
                        loop_beats: 4.0,
                        notes: vec![crate::prompt::MidiNote {
                            time: 0.0,
                            duration: 1.0,
                            pitch: 72,
                            velocity: 0.8,
                        }],
                    },
                    Action::Configure {
                        track_id,
                        target: TrackRole::Lead,
                        tool: "event",
                        tool_id: event_id,
                        clip_id: Some(clip_id),
                        parameter: "pitch",
                        value: "80".to_owned(),
                    },
                ],
            },
            summary: "Replaced then configured stale material".to_owned(),
        };

        assert_eq!(
            studio.apply_plan(
                0.0,
                8.0,
                "replace then retune the old lead",
                stale_configuration
            ),
            Err(StudioError::UnknownSoundTool)
        );
        assert_eq!(studio.to_json(), before);
        assert_eq!(studio.next_id, before_next_id);
        assert_eq!(studio.history.len(), before_history);
    }

    #[test]
    fn validates_prompt_selection_and_mix_changes() {
        let mut studio = Studio::new();
        assert_eq!(
            studio.apply_prompt(0.0, 2.0, "  "),
            Err(StudioError::EmptyPrompt)
        );
        assert_eq!(
            studio.apply_prompt(0.0, 2.0, &"x".repeat(MAX_PROMPT_CHARACTERS + 1)),
            Err(StudioError::InvalidPrompt)
        );
        assert_eq!(
            studio.apply_prompt(4.0, 2.0, "louder"),
            Err(StudioError::InvalidSelection)
        );
        assert_eq!(
            studio.set_mix(999, Some(1.0), None),
            Err(StudioError::UnknownTrack)
        );
        assert_eq!(
            studio.set_mix(1, Some(2.0), None),
            Err(StudioError::InvalidMix)
        );

        studio
            .set_mix(1, Some(0.5), Some(true))
            .expect("valid mix change");
        assert_eq!(studio.project().tracks[0].volume, 0.5);
        assert!(studio.project().tracks[0].muted);
    }

    #[test]
    fn rejects_modifier_targets_that_the_plan_does_not_create() {
        let mut studio = Studio::new();
        let initial = studio.to_json();
        let missing_lead = crate::prompt::EditPlan {
            action: Action::Gain {
                amount: 1.2,
                target: Some(TrackRole::Lead),
            },
            summary: "Lifted the lead".to_owned(),
        };
        assert_eq!(
            studio.apply_plan(4.0, 8.0, "make the lead louder", missing_lead),
            Err(StudioError::UnknownTrack)
        );
        assert_eq!(studio.to_json(), initial);

        for dependent in [
            Action::Instrument {
                preset: "Surge Lead",
                target: TrackRole::Lead,
            },
            Action::Modulator {
                parameter: "instrument.pitch".to_owned(),
                shape: "sine",
                rate: 5.0,
                depth: 0.2,
                target: TrackRole::Lead,
            },
        ] {
            let misordered = crate::prompt::EditPlan {
                action: Action::Compound {
                    actions: vec![
                        dependent,
                        Action::AddTrack {
                            role: TrackRole::Lead,
                        },
                    ],
                },
                summary: "Misordered lead setup".to_owned(),
            };
            assert_eq!(
                studio.apply_plan(4.0, 8.0, "configure then add a lead", misordered),
                Err(StudioError::UnknownTrack)
            );
            assert_eq!(studio.to_json(), initial);
        }

        let created_lead = crate::prompt::EditPlan {
            action: Action::Compound {
                actions: vec![
                    Action::AddTrack {
                        role: TrackRole::Lead,
                    },
                    Action::Gain {
                        amount: 1.2,
                        target: Some(TrackRole::Lead),
                    },
                ],
            },
            summary: "Added and lifted a lead".to_owned(),
        };
        studio
            .apply_plan(4.0, 8.0, "add a louder lead", created_lead)
            .expect("the plan creates its target");
        assert!(
            studio
                .project()
                .tracks
                .iter()
                .any(|track| track.role == TrackRole::Lead)
        );
    }

    #[test]
    fn escapes_user_text_in_json() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(0.0, 2.0, "add a \"spark\"\nplease")
            .expect("valid edit");
        let json = studio.project().to_json();
        assert!(json.contains("add a \\\"spark\\\"\\nplease"));
        assert!(!json.contains("\"spark\"\nplease"));
    }

    #[test]
    fn reset_is_undoable() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(1.0, 3.0, "add texture")
            .expect("valid edit");
        studio.reset();
        assert!(studio.project().edits.is_empty());
        assert!(studio.undo());
        assert_eq!(studio.project().edits.len(), 1);
    }

    #[test]
    fn serialized_studio_reports_real_undo_availability() {
        let mut studio = Studio::new();
        assert!(studio.to_json().contains("\"canUndo\":false"));
        studio
            .apply_prompt(0.0, 2.0, "brighter")
            .expect("valid edit");
        assert!(studio.to_json().contains("\"canUndo\":true"));
        assert!(studio.undo());
        assert!(studio.to_json().contains("\"canUndo\":false"));
    }

    #[test]
    fn replaces_a_gemini_graph_as_one_undoable_edit() {
        let mut studio = Studio::new();
        let before = studio.project().to_json();
        let plan = crate::prompt::EditPlan {
            action: Action::Configure {
                track_id: 2,
                target: TrackRole::Bass,
                tool: "instrument",
                tool_id: 201,
                clip_id: None,
                parameter: "preset",
                value: "Surge Lead".to_owned(),
            },
            summary: "Brightened the bass".to_owned(),
        };
        let mut session = Studio::from_project(studio.project().clone());
        session
            .apply_plan(4.0, 8.0, "brighten the bass", plan.clone())
            .expect("valid session edit");

        studio
            .replace_graph(
                session.project().clone(),
                4.0,
                8.0,
                "brighten the bass",
                plan,
            )
            .expect("valid graph replacement");
        assert_eq!(studio.project().edits.len(), 1);
        assert_eq!(studio.project().version, 2);
        assert_eq!(studio.project().tracks[1].instrument.preset, "Surge Lead");
        assert!(studio.undo());
        let mut restored = studio.project().clone();
        restored.version = 1;
        assert_eq!(restored.to_json(), before);
    }

    #[test]
    fn materialized_edit_log_is_bounded() {
        let mut studio = Studio::new();
        for _ in 0..EDIT_LOG_LIMIT + 8 {
            studio
                .apply_prompt(0.0, 2.0, "increase volume")
                .expect("valid edit");
        }

        assert_eq!(studio.project().edits.len(), EDIT_LOG_LIMIT);
        assert!(Project::from_json(&studio.project().to_json()).is_ok());
    }

    #[test]
    fn regional_edits_do_not_mutate_baseline_effect_chains() {
        let mut studio = Studio::new();
        let baseline: Vec<Vec<String>> = studio
            .project()
            .tracks
            .iter()
            .map(|track| {
                track
                    .effects
                    .iter()
                    .map(|effect| effect.name.clone())
                    .collect()
            })
            .collect();

        studio
            .apply_prompt(8.0, 16.0, "add echo to the chords")
            .expect("valid regional effect");
        studio
            .apply_prompt(16.0, 24.0, "insert a sick drop here")
            .expect("valid regional composition");

        let after: Vec<Vec<String>> = studio
            .project()
            .tracks
            .iter()
            .take(baseline.len())
            .map(|track| {
                track
                    .effects
                    .iter()
                    .map(|effect| effect.name.clone())
                    .collect()
            })
            .collect();
        assert_eq!(after, baseline);
        assert_eq!(studio.project().edits.len(), 2);
    }
}
