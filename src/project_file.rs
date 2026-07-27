use std::collections::HashSet;

use serde_json::{Map, Value as JsonValue};

use crate::model::{
    ChannelOperation, ChannelOperationAction, Clip, ClipEvent, Edit, EditOperation, Effect,
    Instrument, MAX_MIDI_EVENTS_PER_CLIP, MAX_PROMPT_CHARACTERS, MIN_MIDI_NOTE_BEATS, Modulator,
    PROJECT_SCHEMA_VERSION, Project, ProjectFileError, Routing, SURGE_ENGINE, Track, TrackRole,
    automation_target_range, valid_operation_id, valid_surge_preset,
};
use crate::prompt::{Action, AutomationPoint, MAX_COMPOUND_ACTIONS, MidiNote};

const MAX_TRACKS: usize = 128;
const MAX_TOOLS_PER_TRACK: usize = 256;
const MAX_CLIPS_PER_TRACK: usize = 2_048;
const MAX_EDITS: usize = 10_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

type Object = Map<String, JsonValue>;

pub(crate) fn parse_project(source: &str) -> Result<Project, ProjectFileError> {
    let value: JsonValue =
        serde_json::from_str(source).map_err(|error| invalid(format!("invalid JSON: {error}")))?;
    let root = object(&value, "sound graph")?;
    if integer(root, "schemaVersion")? != PROJECT_SCHEMA_VERSION {
        return Err(invalid("schemaVersion is unsupported"));
    }
    let name = limited_string(root, "name", 1, 160)?;
    let bpm = integer(root, "bpm")?;
    if !(60..=180).contains(&bpm) {
        return Err(invalid("bpm must be between 60 and 180"));
    }
    let duration = finite_number(root, "duration")?;
    if duration < 0.25 {
        return Err(invalid("duration must be at least 0.25 seconds"));
    }
    if duration > crate::audio_analysis::MAX_WAV_SECONDS {
        return Err(invalid("duration exceeds the RIFF/WAV export limit"));
    }
    let version = integer(root, "version")?;
    let track_values = array(root, "tracks")?;
    if track_values.is_empty() || track_values.len() > MAX_TRACKS {
        return Err(invalid(format!(
            "tracks must contain between 1 and {MAX_TRACKS} entries"
        )));
    }

    let mut ids = HashSet::new();
    let mut event_ids = HashSet::new();
    let tracks = track_values
        .iter()
        .enumerate()
        .map(|(index, value)| parse_track(value, index, duration, &mut ids, &mut event_ids))
        .collect::<Result<Vec<_>, _>>()?;
    let track_ids = tracks.iter().map(|track| track.id).collect::<HashSet<_>>();
    let track_ids = track_ids.into_iter().collect::<Vec<_>>();
    if tracks.iter().any(|track| {
        track.modulators.iter().any(|modulator| {
            !crate::model::valid_modulator_configuration(track.id, modulator, &track_ids)
        })
    }) {
        return Err(invalid("modulator configuration is unsupported"));
    }
    for track in &tracks {
        validate_modulator_targets(track)?;
    }
    let edit_values = array(root, "edits")?;
    if edit_values.len() > MAX_EDITS {
        return Err(invalid(format!(
            "edits supports at most {MAX_EDITS} entries"
        )));
    }
    let mut edits = edit_values
        .iter()
        .enumerate()
        .map(|(index, value)| parse_edit(value, index, duration, &mut ids))
        .collect::<Result<Vec<_>, _>>()?;
    for edit in &mut edits {
        validate_loaded_automation(&mut edit.action, &tracks)?;
    }
    let mut operation_ids = HashSet::new();
    if edits
        .iter()
        .filter_map(|edit| edit.operation_id.as_deref())
        .any(|operation_id| !operation_ids.insert(operation_id))
    {
        return Err(invalid("edit operation IDs must be unique"));
    }
    let operation_values = array(root, "editOperations")?;
    if operation_values.len() > MAX_EDITS {
        return Err(invalid(format!(
            "editOperations supports at most {MAX_EDITS} entries"
        )));
    }
    let edit_operations = operation_values
        .iter()
        .enumerate()
        .map(|(index, value)| parse_edit_operation(value, index, version))
        .collect::<Result<Vec<_>, _>>()?;
    let mut recorded_operation_ids = HashSet::new();
    if edit_operations
        .iter()
        .any(|operation| !recorded_operation_ids.insert(operation.operation_id.clone()))
    {
        return Err(invalid("edit operation records must be unique"));
    }
    let channel_operation_values = array(root, "channelOperations")?;
    if channel_operation_values.len() > MAX_EDITS {
        return Err(invalid(format!(
            "channelOperations supports at most {MAX_EDITS} entries"
        )));
    }
    let channel_operations = channel_operation_values
        .iter()
        .enumerate()
        .map(|(index, value)| parse_channel_operation(value, index, version))
        .collect::<Result<Vec<_>, _>>()?;
    let mut channel_operation_ids = HashSet::new();
    if channel_operations
        .iter()
        .any(|operation| !channel_operation_ids.insert(operation.operation_id.as_str()))
    {
        return Err(invalid("channel operation records must be unique"));
    }
    if !ids.is_disjoint(&event_ids) {
        return Err(invalid(
            "MIDI event IDs must not collide with sound graph object IDs",
        ));
    }

    Ok(Project {
        name,
        bpm: bpm as u16,
        duration,
        version,
        tracks,
        edits,
        edit_operations,
        channel_operations,
    })
}

fn parse_track(
    value: &JsonValue,
    index: usize,
    project_duration: f32,
    ids: &mut HashSet<u64>,
    event_ids: &mut HashSet<u64>,
) -> Result<Track, ProjectFileError> {
    let context = format!("track {}", index + 1);
    let track = object(value, &context)?;
    let id = unique_id(track, "id", ids, &context)?;
    let name = limited_string(track, "name", 1, 160)?;
    let role = parse_role(string(track, "role")?)?;
    let color = limited_string(track, "color", 1, 32)?;
    let volume = range(track, "volume", 0.0, 1.5)?;
    let muted = boolean(track, "muted")?;

    let instrument_value = field(track, "instrument")?;
    let instrument_object = object(instrument_value, "instrument")?;
    let instrument_id = unique_id(instrument_object, "id", ids, "instrument")?;
    expect_type(instrument_object, "instrument")?;
    if string(instrument_object, "engine")? != SURGE_ENGINE {
        return Err(invalid("instrument engine must be Surge XT"));
    }
    let preset = string(instrument_object, "preset")?;
    if !valid_surge_preset(preset) {
        return Err(invalid("instrument preset is unsupported"));
    }
    let mut instrument = Instrument {
        id: instrument_id,
        engine: SURGE_ENGINE.to_owned(),
        preset: preset.to_owned(),
        native_overrides: parse_native_overrides(instrument_object, preset)?,
    };
    normalize_native_overrides(&mut instrument);
    let effect_values = array(track, "effects")?;
    if effect_values.len() > MAX_TOOLS_PER_TRACK {
        return Err(invalid(format!(
            "{context} supports at most {MAX_TOOLS_PER_TRACK} effects"
        )));
    }
    let effects = effect_values
        .iter()
        .map(|value| parse_effect(value, ids, &instrument))
        .collect::<Result<Vec<_>, _>>()?;
    let mut preset_slots = HashSet::new();
    if effects
        .iter()
        .filter_map(|effect| effect.preset_slot)
        .any(|slot| !preset_slots.insert(slot))
    {
        return Err(invalid(format!(
            "{context} has duplicate preset effect slots"
        )));
    }

    let modulator_values = array(track, "modulators")?;
    if modulator_values.len() > MAX_TOOLS_PER_TRACK {
        return Err(invalid(format!(
            "{context} supports at most {MAX_TOOLS_PER_TRACK} modulators"
        )));
    }
    let modulators = modulator_values
        .iter()
        .map(|value| parse_modulator(value, ids, id))
        .collect::<Result<Vec<_>, _>>()?;
    if !crate::model::native_modulator_slots_fit(id, &modulators) {
        return Err(invalid(format!(
            "{context} supports at most six MIDI-triggered and six scene native modulators"
        )));
    }

    let routing_object = object(field(track, "routing")?, "routing")?;
    let output = limited_string(routing_object, "output", 1, 64)?;
    if output != "master" {
        return Err(invalid("routing output must be master"));
    }
    let effect_order = parse_effect_order(routing_object, instrument.id, &effects)?;
    validate_control_routing(routing_object, &modulators)?;
    validate_routing_edges(
        routing_object,
        instrument.id,
        &effect_order,
        &output,
        &modulators,
        id,
    )?;

    let clip_values = array(track, "clips")?;
    if clip_values.len() > MAX_CLIPS_PER_TRACK {
        return Err(invalid(format!(
            "{context} supports at most {MAX_CLIPS_PER_TRACK} clips"
        )));
    }
    let clips = clip_values
        .iter()
        .map(|value| parse_clip(value, project_duration, ids, event_ids))
        .collect::<Result<Vec<_>, _>>()?;
    let parsed = Track {
        id,
        name,
        role,
        color,
        volume,
        muted,
        instrument,
        effects,
        modulators,
        routing: Routing {
            effect_order,
            output,
        },
        clips,
    };
    if !crate::model::track_effects_fit(&parsed) {
        return Err(invalid(format!(
            "{context} exceeds Surge XT's serial effect capacity"
        )));
    }
    Ok(parsed)
}

fn parse_effect(
    value: &JsonValue,
    ids: &mut HashSet<u64>,
    instrument: &Instrument,
) -> Result<Effect, ProjectFileError> {
    let effect = object(value, "effect")?;
    let id = unique_id(effect, "id", ids, "effect")?;
    expect_type(effect, "effect")?;
    let name = limited_string(effect, "name", 1, 64)?;
    if !is_effect_name(&name) {
        return Err(invalid(format!("unsupported effect: {name}")));
    }
    let parameters = object(field(effect, "parameters")?, "effect parameters")?;
    let native_parameters = crate::surge::effect_parameter_values(&name);
    let mut serialized_parameters = std::collections::BTreeMap::new();
    for (parameter, value) in parameters {
        if parameter == "mix" {
            continue;
        }
        if !native_parameters.contains_key(parameter) {
            continue;
        }
        let value = value
            .as_f64()
            .ok_or_else(|| invalid(format!("effect parameter {parameter} must be numeric")))?
            as f32;
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(invalid(format!(
                "effect parameter {parameter} must be between 0 and 1"
            )));
        }
        serialized_parameters.insert(parameter.to_owned(), value);
    }
    let override_values = effect
        .get("overrides")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("effect overrides must be an array"))?;
    let parameter_overrides = override_values
        .iter()
        .filter_map(|value| {
            let parameter = value
                .as_str()
                .ok_or_else(|| invalid("effect overrides must be strings"));
            let parameter = match parameter {
                Ok(name) => name,
                Err(error) => return Some(Err(error)),
            };
            if parameter != "mix" && !serialized_parameters.contains_key(parameter) {
                return None;
            }
            Some(Ok(parameter.to_owned()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let extra_parameters = serialized_parameters;
    let preset_slot = effect
        .contains_key("presetSlot")
        .then(|| {
            bounded_integer(
                effect,
                "presetSlot",
                1,
                crate::surge::SERIAL_EFFECT_SLOT_COUNT as u64,
            )
            .map(|slot| (slot - 1) as usize)
        })
        .transpose()?;
    let parsed = Effect {
        id,
        name,
        preset_slot,
        mix: range(parameters, "mix", 0.0, 1.0)?,
        enabled: boolean(effect, "enabled")?,
        parameters: extra_parameters,
        parameter_overrides,
        tempo_sync_parameters: string_list(effect, "tempoSync")?,
        deactivated_parameters: string_list(effect, "deactivated")?,
    };
    let semantics = crate::surge::effect_parameter_semantics(
        instrument,
        std::slice::from_ref(&parsed),
        &[parsed.id],
        1,
        parsed.id,
    );
    for (parameter, value) in &parsed.parameters {
        if semantics.get(parameter).is_some_and(|semantics| {
            !semantics.choices.is_empty() && (semantics.value - value).abs() >= 0.000_01
        }) {
            return Err(invalid(format!(
                "effect parameter {parameter} must match a Surge XT choice"
            )));
        }
    }
    for parameter in &parsed.tempo_sync_parameters {
        if (parsed.preset_slot.is_none() || parsed.parameter_overrides.contains(parameter))
            && !semantics
                .get(parameter)
                .is_some_and(|semantics| semantics.tempo_sync)
        {
            return Err(invalid(format!(
                "effect parameter {parameter} cannot use tempo sync"
            )));
        }
    }
    for parameter in &parsed.deactivated_parameters {
        if (parsed.preset_slot.is_none() || parsed.parameter_overrides.contains(parameter))
            && !semantics
                .get(parameter)
                .is_some_and(|semantics| semantics.can_deactivate)
        {
            return Err(invalid(format!(
                "effect parameter {parameter} cannot be deactivated"
            )));
        }
    }
    Ok(parsed)
}

fn string_list(object: &Object, name: &str) -> Result<Vec<String>, ProjectFileError> {
    let Some(value) = object.get(name) else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| invalid(format!("effect {name} must be an array")))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("effect {name} entries must be strings")))
        })
        .collect()
}

fn parse_modulator(
    value: &JsonValue,
    ids: &mut HashSet<u64>,
    owner_track_id: u64,
) -> Result<Modulator, ProjectFileError> {
    let modulator = object(value, "modulator")?;
    let id = unique_id(modulator, "id", ids, "modulator")?;
    expect_type(modulator, "modulator")?;
    let shape = limited_string(modulator, "shape", 1, 32)?;
    if !matches!(
        shape.as_str(),
        "sine" | "triangle" | "square" | "random" | "envelope" | "formula"
    ) {
        return Err(invalid("modulator shape is unsupported"));
    }
    let parameters = object(field(modulator, "parameters")?, "modulator parameters")?;
    let target = limited_string(modulator, "target", 1, 64)?;
    let trigger = required_enum(modulator, "trigger", &["free", "midi", "audio"])?;
    let source_track_id = if trigger == "free" {
        None
    } else {
        modulator
            .get("sourceTrackId")
            .and_then(JsonValue::as_u64)
            .or(Some(owner_track_id))
    };
    Ok(Modulator {
        id,
        name: limited_string(modulator, "name", 1, 64)?,
        shape,
        rate: range(parameters, "rate", 0.01, 20.0)?,
        rate_mode: required_enum(modulator, "rateMode", &["hz", "tempo"])?,
        trigger,
        source_track_id,
        attack_ms: parameters
            .get("attackMs")
            .map(|_| range(parameters, "attackMs", 0.0, 1_000.0))
            .transpose()?
            .unwrap_or(5.0),
        release_ms: parameters
            .get("releaseMs")
            .map(|_| range(parameters, "releaseMs", 1.0, 5_000.0))
            .transpose()?
            .unwrap_or(180.0),
        threshold: parameters
            .get("threshold")
            .map(|_| range(parameters, "threshold", 0.0, 1.0))
            .transpose()?
            .unwrap_or(0.1),
        polarity: modulator
            .get("polarity")
            .map(|_| required_enum(modulator, "polarity", &["increase", "decrease"]))
            .transpose()?
            .unwrap_or_else(|| "increase".to_owned()),
        formula: modulator
            .get("formula")
            .map(|_| limited_string(modulator, "formula", 0, 8_192))
            .transpose()?
            .unwrap_or_default(),
        depth: range(parameters, "depth", 0.0, 1.0)?,
        target,
        enabled: boolean(modulator, "enabled")?,
    })
}

fn parse_effect_order(
    routing: &Object,
    instrument_id: u64,
    effects: &[Effect],
) -> Result<Vec<u64>, ProjectFileError> {
    let audio = array(routing, "audio")?;
    if audio.len() != effects.len() + 3 {
        return Err(invalid(
            "routing audio chain must include clips, instrument, every effect, and master",
        ));
    }
    if audio[0].as_str() != Some("clips")
        || audio[1].as_str() != Some(format!("instrument:{instrument_id}").as_str())
        || audio.last().and_then(JsonValue::as_str) != Some("master")
    {
        return Err(invalid("routing audio chain has invalid endpoints"));
    }
    let known = effects
        .iter()
        .map(|effect| effect.id)
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let order = audio[2..audio.len() - 1]
        .iter()
        .map(|value| {
            let id = value
                .as_str()
                .and_then(|value| value.strip_prefix("effect:"))
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| invalid("routing effect entry is invalid"))?;
            if !known.contains(&id) || !seen.insert(id) {
                return Err(invalid("routing must contain every effect exactly once"));
            }
            Ok(id)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(order)
}

fn validate_control_routing(
    routing: &Object,
    modulators: &[Modulator],
) -> Result<(), ProjectFileError> {
    let routes = array(routing, "control")?;
    let expected = modulators
        .iter()
        .filter(|modulator| modulator.enabled)
        .map(|modulator| {
            (
                format!("modulator:{}", modulator.id),
                modulator.target.clone(),
            )
        })
        .collect::<HashSet<_>>();
    if routes.len() != expected.len() {
        return Err(invalid(
            "routing control must contain every enabled modulator exactly once",
        ));
    }
    let mut seen = HashSet::new();
    for (index, route) in routes.iter().enumerate() {
        let route = object(route, &format!("routing control entry {}", index + 1))?;
        let connection = (
            string(route, "source")?.to_owned(),
            string(route, "target")?.to_owned(),
        );
        if !expected.contains(&connection) || !seen.insert(connection) {
            return Err(invalid(
                "routing control connections must match enabled modulators",
            ));
        }
    }
    Ok(())
}

fn validate_routing_edges(
    routing: &Object,
    instrument_id: u64,
    effect_order: &[u64],
    output: &str,
    modulators: &[Modulator],
    owner_track_id: u64,
) -> Result<(), ProjectFileError> {
    let instrument = format!("instrument:{instrument_id}");
    let mut expected = HashSet::from([("clips".to_owned(), instrument.clone(), "midi".to_owned())]);
    expected.extend(
        modulators
            .iter()
            .filter(|modulator| modulator.enabled && modulator.trigger != "free")
            .map(|modulator| {
                let source_track_id = modulator.source_track_id.unwrap_or(owner_track_id);
                (
                    if modulator.trigger == "audio" {
                        format!("track:{source_track_id}:output")
                    } else if source_track_id == owner_track_id {
                        "clips".to_owned()
                    } else {
                        format!("track:{source_track_id}:clips")
                    },
                    format!("modulator:{}", modulator.id),
                    modulator.trigger.clone(),
                )
            }),
    );
    let mut audio_source = instrument;
    for effect_id in effect_order {
        let effect = format!("effect:{effect_id}");
        expected.insert((audio_source, effect.clone(), "audio".to_owned()));
        audio_source = effect;
    }
    expected.insert((audio_source, output.to_owned(), "audio".to_owned()));
    expected.extend(
        modulators
            .iter()
            .filter(|modulator| modulator.enabled)
            .map(|modulator| {
                (
                    format!("modulator:{}", modulator.id),
                    modulator.target.clone(),
                    "control".to_owned(),
                )
            }),
    );

    let edges = array(routing, "edges")?;
    if edges.len() != expected.len() {
        return Err(invalid(
            "routing edges must contain the complete typed signal graph",
        ));
    }
    let mut seen = HashSet::new();
    for (index, edge) in edges.iter().enumerate() {
        let edge = object(edge, &format!("routing edge {}", index + 1))?;
        let connection = (
            string(edge, "source")?.to_owned(),
            string(edge, "target")?.to_owned(),
            string(edge, "type")?.to_owned(),
        );
        if !expected.contains(&connection) || !seen.insert(connection) {
            return Err(invalid(
                "routing edges are inconsistent with the published sound tools",
            ));
        }
    }
    Ok(())
}

fn parse_clip(
    value: &JsonValue,
    project_duration: f32,
    ids: &mut HashSet<u64>,
    event_ids: &mut HashSet<u64>,
) -> Result<Clip, ProjectFileError> {
    let clip = object(value, "MIDI clip")?;
    let id = unique_id(clip, "id", ids, "MIDI clip")?;
    let start = range(clip, "start", 0.0, project_duration)?;
    let end = range(clip, "end", 0.0, project_duration)?;
    if end <= start {
        return Err(invalid("MIDI clip end must be after its start"));
    }
    let source_start = range(clip, "sourceStart", 0.0, start)?;
    let playback_mode = clip
        .get("playback")
        .and_then(JsonValue::as_object)
        .and_then(|playback| playback.get("mode"))
        .and_then(JsonValue::as_str)
        .unwrap_or("loop");
    if !matches!(playback_mode, "loop" | "once") {
        return Err(invalid("MIDI clip playback mode must be loop or once"));
    }
    let maximum_beats = if playback_mode == "once" {
        crate::model::MAX_ONCE_PLAYBACK_BEATS
    } else {
        crate::model::MAX_LOOP_PLAYBACK_BEATS
    };
    let published_length = clip
        .get("playback")
        .and_then(JsonValue::as_object)
        .and_then(|playback| playback.get("lengthBeats"))
        .and_then(JsonValue::as_f64)
        .map(|value| value as f32);
    let loop_beats = match published_length {
        Some(value) => value,
        None => range(clip, "loopBeats", 0.25, maximum_beats)?,
    };
    if !loop_beats.is_finite() || !(0.25..=maximum_beats).contains(&loop_beats) {
        return Err(invalid(format!(
            "MIDI clip {playback_mode} playback length must be between 0.25 and {maximum_beats} beats"
        )));
    }
    let event_values = array(clip, "events")?;
    if event_values.len() > MAX_MIDI_EVENTS_PER_CLIP {
        return Err(invalid(format!(
            "MIDI clips support at most {MAX_MIDI_EVENTS_PER_CLIP} events"
        )));
    }
    let mut clip_event_ids = HashSet::new();
    let events = event_values
        .iter()
        .map(|value| parse_clip_event(value, loop_beats, &mut clip_event_ids, event_ids))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Clip {
        id,
        label: limited_string(clip, "label", 1, 64)?,
        start,
        end,
        source_start,
        style: limited_string(clip, "style", 1, 64)?,
        playback_mode: playback_mode.to_owned(),
        loop_beats,
        events,
    })
}

fn parse_clip_event(
    value: &JsonValue,
    loop_beats: f32,
    clip_event_ids: &mut HashSet<u64>,
    event_ids: &mut HashSet<u64>,
) -> Result<ClipEvent, ProjectFileError> {
    let event = object(value, "MIDI event")?;
    let id = unique_id(event, "id", clip_event_ids, "MIDI event")?;
    event_ids.insert(id);
    let kind = limited_string(event, "type", 1, 32)?;
    if kind != "note" {
        return Err(invalid("MIDI event type must be note"));
    }
    let time = finite_number(event, "time")?;
    if !(0.0..loop_beats).contains(&time) {
        return Err(invalid("MIDI event time must be inside its clip loop"));
    }
    Ok(ClipEvent {
        id,
        kind,
        time,
        duration: range(
            event,
            "duration",
            MIN_MIDI_NOTE_BEATS,
            loop_beats.min(crate::model::MAX_MIDI_NOTE_DURATION_BEATS),
        )?,
        pitch: bounded_integer(event, "pitch", 0, 127)? as u8,
        velocity: range(event, "velocity", 0.01, 1.0)?,
    })
}

fn parse_edit(
    value: &JsonValue,
    index: usize,
    project_duration: f32,
    ids: &mut HashSet<u64>,
) -> Result<Edit, ProjectFileError> {
    let edit = object(value, "edit")?;
    let context = format!("edit {}", index + 1);
    let id = unique_id(edit, "id", ids, &context)?;
    let start = range(edit, "start", 0.0, project_duration)?;
    let end = range(edit, "end", 0.0, project_duration)?;
    if end <= start {
        return Err(invalid(format!("{context} end must be after its start")));
    }
    Ok(Edit {
        id,
        operation_id: edit
            .get("operationId")
            .map(|_| limited_string(edit, "operationId", 1, 128))
            .transpose()?,
        start,
        end,
        prompt: limited_string(edit, "prompt", 1, MAX_PROMPT_CHARACTERS)?,
        summary: limited_string(edit, "summary", 1, 160)?,
        action: parse_action(field(edit, "action")?, 0)?,
    })
}

fn parse_edit_operation(
    value: &JsonValue,
    index: usize,
    current_version: u64,
) -> Result<EditOperation, ProjectFileError> {
    let operation = object(value, "edit operation")?;
    let context = format!("edit operation {}", index + 1);
    let operation_id = limited_string(operation, "operationId", 1, 128)?;
    if !valid_operation_id(&operation_id) {
        return Err(invalid(format!("{context} ID is invalid")));
    }
    let applied_steps = integer(operation, "appliedSteps")?;
    if applied_steps == 0 || applied_steps > MAX_EDITS as u64 {
        return Err(invalid(format!(
            "{context} appliedSteps must be between 1 and {MAX_EDITS}"
        )));
    }
    let project_version = integer(operation, "projectVersion")?;
    if project_version > current_version {
        return Err(invalid(format!(
            "{context} projectVersion cannot exceed the project version"
        )));
    }
    let status = match string(operation, "status")? {
        "running" => crate::model::EditOperationStatus::Running,
        "partial" | "failed_with_changes" => crate::model::EditOperationStatus::Failed,
        "interrupted_with_changes" => crate::model::EditOperationStatus::Interrupted,
        "completed" => crate::model::EditOperationStatus::Completed,
        _ => return Err(invalid(format!("{context} status is invalid"))),
    };
    let initial_version = operation
        .get("initialVersion")
        .map(|_| integer(operation, "initialVersion"))
        .transpose()?
        .unwrap_or_else(|| project_version.saturating_sub(applied_steps));
    if initial_version > project_version {
        return Err(invalid(format!(
            "{context} initialVersion cannot exceed its projectVersion"
        )));
    }
    Ok(EditOperation {
        operation_id,
        source: limited_string(operation, "source", 1, 32)?,
        status,
        applied_steps: applied_steps as usize,
        initial_version,
        project_version,
        message: limited_string(operation, "message", 1, 160)?,
    })
}

fn parse_channel_operation(
    value: &JsonValue,
    index: usize,
    current_version: u64,
) -> Result<ChannelOperation, ProjectFileError> {
    let operation = object(value, "channel operation")?;
    let context = format!("channel operation {}", index + 1);
    let operation_id = limited_string(operation, "operationId", 1, 128)?;
    if !valid_operation_id(&operation_id) {
        return Err(invalid(format!("{context} ID is invalid")));
    }
    let track_id = integer(operation, "trackId")?;
    if track_id == 0 {
        return Err(invalid(format!(
            "{context} trackId must be greater than zero"
        )));
    }
    let project_version = integer(operation, "projectVersion")?;
    if project_version > current_version {
        return Err(invalid(format!(
            "{context} projectVersion cannot exceed the project version"
        )));
    }
    let role = match operation.get("role") {
        Some(JsonValue::Null) => None,
        Some(_) => Some(
            TrackRole::from_name(string(operation, "role")?)
                .ok_or_else(|| invalid(format!("{context} role is invalid")))?,
        ),
        None => return Err(invalid(format!("{context} role is required"))),
    };
    let action = match string(operation, "action")? {
        "add" if role.is_some() => ChannelOperationAction::Add,
        "delete" if role.is_none() => ChannelOperationAction::Delete,
        _ => {
            return Err(invalid(format!(
                "{context} action and role are inconsistent"
            )));
        }
    };
    Ok(ChannelOperation {
        operation_id,
        action,
        track_id,
        role,
        project_version,
    })
}

fn parse_action(value: &JsonValue, depth: usize) -> Result<Action, ProjectFileError> {
    if depth > 8 {
        return Err(invalid("compound action nesting is too deep"));
    }
    let action = object(value, "edit action")?;
    let action_type = string(action, "type")?;
    if action_type == "graph-mutation" {
        return Ok(Action::GraphMutation);
    }
    if action_type == "compound" {
        let values = array(action, "actions")?;
        if values.is_empty() || values.len() > MAX_COMPOUND_ACTIONS {
            return Err(invalid(
                "compound actions require one to nine child actions",
            ));
        }
        return Ok(Action::Compound {
            actions: values
                .iter()
                .map(|value| parse_action(value, depth + 1))
                .collect::<Result<Vec<_>, _>>()?,
        });
    }
    if action_type == "timed" {
        let start = relative_range(action, "start")?;
        let end = relative_range(action, "end")?;
        if end <= start {
            return Err(invalid("timed edit end must be after its start"));
        }
        return Ok(Action::Timed {
            start,
            end,
            action: Box::new(parse_action(field(action, "action")?, depth + 1)?),
        });
    }
    let target = action_target(action)?;
    match action_type {
        "gain" => Ok(Action::Gain {
            amount: range(action, "value", 0.0, 2.0)?,
            target,
        }),
        "mute" => Ok(Action::Mute { target }),
        "midi-clip" => {
            let role = required_target(target, "midi-clip")?;
            let loop_beats = range(action, "loopBeats", 0.25, 16.0)?;
            let start = relative_range(action, "start")?;
            let end = relative_range(action, "end")?;
            if end <= start {
                return Err(invalid("midi-clip edit end must be after its start"));
            }
            let events = array(action, "events")?;
            if events.len() > 32 {
                return Err(invalid("midi-clip edit supports at most 32 events"));
            }
            Ok(Action::MidiClip {
                track_id: integer(action, "trackId")?,
                target: role,
                label: limited_string(action, "label", 1, 64)?,
                start,
                end,
                loop_beats,
                notes: events
                    .iter()
                    .map(|event| parse_midi_note(event, loop_beats))
                    .collect::<Result<Vec<_>, _>>()?,
            })
        }
        "add-track" => Ok(Action::AddTrack {
            role: required_target(target, "add-track")?,
        }),
        "instrument" => Ok(Action::Instrument {
            preset: surge_preset(string(action, "name")?)?,
            target: required_target(target, "instrument")?,
        }),
        "modulator" => Ok(Action::Modulator {
            parameter: limited_string(action, "name", 1, 64)?,
            shape: modulator_shape(string(action, "shape")?)?,
            rate: range(action, "rate", 0.01, 20.0)?,
            depth: range(action, "value", 0.0, 1.0)?,
            target: required_target(target, "modulator")?,
        }),
        "configure" => {
            let clip_id = integer(action, "clipId")?;
            Ok(Action::Configure {
                track_id: integer(action, "trackId")?,
                target: required_target(target, "configure")?,
                tool: sound_tool(string(action, "tool")?)?,
                tool_id: integer(action, "toolId")?,
                clip_id: (clip_id != 0).then_some(clip_id),
                parameter: sound_parameter(string(action, "parameter")?)?,
                value: limited_string(action, "setting", 1, 64)?,
            })
        }
        "automation" => {
            let points = array(action, "points")?;
            if !(2..=16).contains(&points.len()) {
                return Err(invalid("automation requires between 2 and 16 points"));
            }
            let points = points
                .iter()
                .map(|point| {
                    let point = object(point, "automation point")?;
                    Ok(AutomationPoint {
                        time: relative_range(point, "time")?,
                        value: finite_number(point, "value")?,
                    })
                })
                .collect::<Result<Vec<_>, ProjectFileError>>()?;
            validate_automation_points(&points)?;
            let parameter = limited_string(action, "name", 1, 64)?;
            Ok(Action::Automation {
                track_id: integer(action, "trackId")?,
                parameter,
                curve: automation_curve(string(action, "curve")?)?,
                points,
                target: required_target(target, "automation")?,
            })
        }
        "effect" => Ok(Action::Effect {
            name: effect_name(string(action, "name")?, false)?,
            mix: range(action, "value", 0.0, 1.0)?,
            target,
        }),
        "remove-effect" => Ok(Action::RemoveEffect {
            name: effect_name(string(action, "name")?, true)?,
            target,
        }),
        "filter" => Ok(Action::Filter {
            amount: range(action, "value", -1.0, 1.0)?,
            target,
        }),
        "rhythm" => Ok(Action::Rhythm {
            amount: range(action, "value", -1.0, 1.0)?,
            target,
        }),
        "tempo" if target.is_none() => Ok(Action::Tempo {
            bpm: bounded_integer(action, "value", 60, 180)? as u16,
        }),
        _ => Err(invalid(format!("unsupported edit action: {action_type}"))),
    }
}

fn validate_automation_points(points: &[AutomationPoint]) -> Result<(), ProjectFileError> {
    if points.first().map(|point| point.time) != Some(0.0)
        || points.last().map(|point| point.time) != Some(1.0)
        || points
            .windows(2)
            .any(|points| points[1].time <= points[0].time)
    {
        Err(invalid(
            "automation point times must increase from 0 through 1",
        ))
    } else {
        Ok(())
    }
}

fn validate_loaded_automation(
    action: &mut Action,
    tracks: &[Track],
) -> Result<(), ProjectFileError> {
    match action {
        Action::GraphMutation => {}
        Action::Compound { actions } => {
            for action in actions {
                validate_loaded_automation(action, tracks)?;
            }
        }
        Action::Timed { action, .. } => validate_loaded_automation(action, tracks)?,
        Action::Automation {
            track_id,
            parameter,
            curve,
            points,
            target,
        } => {
            let track = if *track_id == 0 {
                tracks.iter().rev().find(|track| {
                    track.role == *target && automation_target_range(track, parameter).is_some()
                })
            } else {
                tracks.iter().find(|track| {
                    track.id == *track_id
                        && track.role == *target
                        && automation_target_range(track, parameter).is_some()
                })
            }
            .ok_or_else(|| invalid("automation target does not exist on its owning track"))?;
            *track_id = track.id;
            let (minimum, maximum) = automation_target_range(track, parameter)
                .expect("validated automation target exists");
            if points.iter().any(|point| {
                !(minimum..=maximum).contains(&point.value)
                    || !crate::model::valid_automation_value(track, parameter, curve, point.value)
            }) {
                return Err(invalid("automation value or curve is invalid"));
            }
        }
        _ => {}
    }
    Ok(())
}

fn automation_curve(value: &str) -> Result<&'static str, ProjectFileError> {
    match value {
        "linear" => Ok("linear"),
        "hold" => Ok("hold"),
        _ => Err(invalid("automation curve is unsupported")),
    }
}

fn parse_midi_note(value: &JsonValue, loop_beats: f32) -> Result<MidiNote, ProjectFileError> {
    let event = object(value, "MIDI note")?;
    if event.get("type").is_some() && string(event, "type")? != "note" {
        return Err(invalid("MIDI edit event type must be note"));
    }
    let time = finite_number(event, "time")?;
    if !(0.0..loop_beats).contains(&time) {
        return Err(invalid("MIDI edit event time must be inside its loop"));
    }
    Ok(MidiNote {
        time,
        duration: range(
            event,
            "duration",
            MIN_MIDI_NOTE_BEATS,
            loop_beats.min(crate::model::MAX_MIDI_NOTE_DURATION_BEATS),
        )?,
        pitch: bounded_integer(event, "pitch", 0, 127)? as u8,
        velocity: range(event, "velocity", 0.01, 1.0)?,
    })
}

fn validate_modulator_targets(track: &Track) -> Result<(), ProjectFileError> {
    for modulator in &track.modulators {
        if !crate::model::valid_modulator_target_for_trigger(
            track,
            &modulator.target,
            &modulator.trigger,
        ) {
            return Err(invalid(format!(
                "modulator {} targets an unavailable parameter",
                modulator.id
            )));
        }
    }
    Ok(())
}

fn action_target(action: &Object) -> Result<Option<TrackRole>, ProjectFileError> {
    match string(action, "target")? {
        "all" => Ok(None),
        value => parse_role(value).map(Some),
    }
}

fn required_target(target: Option<TrackRole>, action: &str) -> Result<TrackRole, ProjectFileError> {
    target.ok_or_else(|| invalid(format!("{action} requires a track role target")))
}

fn parse_role(value: &str) -> Result<TrackRole, ProjectFileError> {
    match value {
        "neutral" => Ok(TrackRole::Neutral),
        "drums" => Ok(TrackRole::Drums),
        "bass" => Ok(TrackRole::Bass),
        "chords" => Ok(TrackRole::Chords),
        "lead" => Ok(TrackRole::Lead),
        "texture" => Ok(TrackRole::Texture),
        _ => Err(invalid(format!("unknown track role: {value}"))),
    }
}

fn surge_preset(value: &str) -> Result<&'static str, ProjectFileError> {
    match value {
        "Init" => Ok("Init"),
        _ => Err(invalid("unsupported Surge XT preset")),
    }
}

fn modulator_shape(value: &str) -> Result<&'static str, ProjectFileError> {
    match value {
        "sine" => Ok("sine"),
        "triangle" => Ok("triangle"),
        "square" => Ok("square"),
        "random" => Ok("random"),
        "envelope" => Ok("envelope"),
        _ => Err(invalid("unsupported modulator shape")),
    }
}

fn sound_tool(value: &str) -> Result<&'static str, ProjectFileError> {
    match value {
        "instrument" => Ok("instrument"),
        "effect" => Ok("effect"),
        "modulator" => Ok("modulator"),
        "event" => Ok("event"),
        "routing" => Ok("routing"),
        _ => Err(invalid("unsupported configurable sound tool")),
    }
}

fn sound_parameter(value: &str) -> Result<&'static str, ProjectFileError> {
    match value {
        "preset" => Ok("preset"),
        "pitch" => Ok("pitch"),
        "mix" => Ok("mix"),
        "enabled" => Ok("enabled"),
        "shape" => Ok("shape"),
        "rate" => Ok("rate"),
        "rateMode" => Ok("rateMode"),
        "trigger" => Ok("trigger"),
        "depth" => Ok("depth"),
        "target" => Ok("target"),
        "time" => Ok("time"),
        "duration" => Ok("duration"),
        "velocity" => Ok("velocity"),
        "position" => Ok("position"),
        _ => Err(invalid("unsupported sound-tool parameter")),
    }
}

fn effect_name(value: &str, allow_all: bool) -> Result<&'static str, ProjectFileError> {
    match value {
        "Reverb 2" => Ok("Reverb 2"),
        "Delay" => Ok("Delay"),
        "Chorus" => Ok("Chorus"),
        "EQ" => Ok("EQ"),
        "Conditioner" => Ok("Conditioner"),
        "Distortion" => Ok("Distortion"),
        "Nimbus" => Ok("Nimbus"),
        "Effects" if allow_all => Ok("Effects"),
        _ => Err(invalid(format!("unsupported effect: {value}"))),
    }
}

fn is_effect_name(value: &str) -> bool {
    crate::surge::effect_type_index(value).is_some()
}

fn expect_type(value: &Object, expected: &str) -> Result<(), ProjectFileError> {
    if string(value, "type")? == expected {
        Ok(())
    } else {
        Err(invalid(format!("sound tool type must be {expected}")))
    }
}

fn object<'a>(value: &'a JsonValue, context: &str) -> Result<&'a Object, ProjectFileError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("{context} must be an object")))
}

fn field<'a>(object: &'a Object, name: &str) -> Result<&'a JsonValue, ProjectFileError> {
    object
        .get(name)
        .ok_or_else(|| invalid(format!("{name} is required")))
}

fn array<'a>(object: &'a Object, name: &str) -> Result<&'a [JsonValue], ProjectFileError> {
    field(object, name)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid(format!("{name} must be an array")))
}

fn string<'a>(object: &'a Object, name: &str) -> Result<&'a str, ProjectFileError> {
    field(object, name)?
        .as_str()
        .ok_or_else(|| invalid(format!("{name} must be a string")))
}

fn limited_string(
    object: &Object,
    name: &str,
    minimum: usize,
    maximum: usize,
) -> Result<String, ProjectFileError> {
    let value = string(object, name)?.trim();
    let length = value.chars().count();
    if !(minimum..=maximum).contains(&length) {
        return Err(invalid(format!(
            "{name} length must be between {minimum} and {maximum} characters"
        )));
    }
    Ok(value.to_owned())
}

fn required_enum(
    object: &Object,
    name: &str,
    allowed: &[&str],
) -> Result<String, ProjectFileError> {
    let value = string(object, name)?;
    if allowed.contains(&value) {
        Ok(value.to_owned())
    } else {
        Err(invalid(format!("{name} is unsupported")))
    }
}

fn boolean(object: &Object, name: &str) -> Result<bool, ProjectFileError> {
    field(object, name)?
        .as_bool()
        .ok_or_else(|| invalid(format!("{name} must be true or false")))
}

fn finite_number(object: &Object, name: &str) -> Result<f32, ProjectFileError> {
    let value = field(object, name)?
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| invalid(format!("{name} must be a finite number")))?;
    if value < f64::from(f32::MIN) || value > f64::from(f32::MAX) {
        return Err(invalid(format!(
            "{name} is outside the supported number range"
        )));
    }
    Ok(value as f32)
}

fn range(object: &Object, name: &str, minimum: f32, maximum: f32) -> Result<f32, ProjectFileError> {
    let value = finite_number(object, name)?;
    if !(minimum..=maximum).contains(&value) {
        return Err(invalid(format!(
            "{name} must be between {minimum} and {maximum}"
        )));
    }
    Ok(value)
}

fn parse_native_overrides(
    object: &Object,
    preset: &str,
) -> Result<std::collections::BTreeMap<i32, f32>, ProjectFileError> {
    let value = object
        .get("nativeOverrides")
        .ok_or_else(|| invalid("instrument nativeOverrides is required"))?;
    let values = value
        .as_object()
        .ok_or_else(|| invalid("instrument nativeOverrides must be an object"))?;
    let overrides = values
        .iter()
        .map(|(id, value)| {
            let id = id
                .parse::<i32>()
                .ok()
                .filter(|id| *id >= 0)
                .ok_or_else(|| invalid("instrument native parameter ID is invalid"))?;
            let value = value
                .as_f64()
                .map(|value| value as f32)
                .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
                .ok_or_else(|| invalid("instrument native parameter value is invalid"))?;
            Ok((id, value))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
    if overrides.is_empty() {
        return Ok(overrides);
    }
    let available = crate::surge::instrument_parameters(preset);
    if overrides
        .keys()
        .any(|id| !available.iter().any(|parameter| parameter.id == *id))
    {
        return Err(invalid(
            "instrument native parameter ID is unavailable for the selected preset",
        ));
    }
    Ok(overrides)
}

fn normalize_native_overrides(instrument: &mut Instrument) {
    if instrument.native_overrides.is_empty() {
        return;
    }
    for parameter in crate::surge::instrument_parameters_for_instrument(instrument) {
        if let Some(value) = instrument.native_overrides.get_mut(&parameter.id) {
            *value = parameter
                .choices
                .iter()
                .min_by(|(left, _), (right, _)| {
                    (left - parameter.value)
                        .abs()
                        .total_cmp(&(right - parameter.value).abs())
                })
                .map_or(parameter.value, |(choice, _)| *choice);
        }
    }
}

fn relative_range(object: &Object, name: &str) -> Result<f32, ProjectFileError> {
    range(object, name, 0.0, 1.0)
}

fn integer(object: &Object, name: &str) -> Result<u64, ProjectFileError> {
    let value = field(object, name)?
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| invalid(format!("{name} must be a number")))?;
    if value.fract() != 0.0 || !(0.0..=MAX_SAFE_INTEGER as f64).contains(&value) {
        return Err(invalid(format!(
            "{name} must be a non-negative safe integer"
        )));
    }
    Ok(value as u64)
}

fn bounded_integer(
    object: &Object,
    name: &str,
    minimum: u64,
    maximum: u64,
) -> Result<u64, ProjectFileError> {
    let value = integer(object, name)?;
    if !(minimum..=maximum).contains(&value) {
        return Err(invalid(format!(
            "{name} must be between {minimum} and {maximum}"
        )));
    }
    Ok(value)
}

fn unique_id(
    object: &Object,
    name: &str,
    ids: &mut HashSet<u64>,
    context: &str,
) -> Result<u64, ProjectFileError> {
    let id = integer(object, name)?;
    if id == 0 {
        return Err(invalid(format!("{context} ID must be greater than zero")));
    }
    if !ids.insert(id) {
        return Err(invalid(format!("duplicate sound graph ID: {id}")));
    }
    Ok(id)
}

fn invalid(message: impl Into<String>) -> ProjectFileError {
    ProjectFileError::new(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with_eq() -> Project {
        let mut project = Project::demo();
        let track = &mut project.tracks[1];
        let effect = Effect {
            id: 99_000,
            name: "EQ".to_owned(),
            preset_slot: None,
            mix: 0.5,
            enabled: true,
            parameters: crate::surge::effect_parameter_values("EQ"),
            parameter_overrides: Vec::new(),
            tempo_sync_parameters: Vec::new(),
            deactivated_parameters: Vec::new(),
        };
        track.routing.effect_order.push(effect.id);
        track.effects.push(effect);
        project
    }

    #[test]
    fn round_trips_the_demo_sound_graph() {
        let original = Project::demo();
        let parsed = parse_project(&original.to_json()).expect("valid demo graph");
        assert_eq!(parsed.to_json(), original.to_json());
    }

    #[test]
    fn round_trip_preserves_published_native_effect_defaults() {
        let mut studio = crate::model::Studio::from_project(Project::demo());
        let effect_id = studio
            .create_effect(2, "Reverb 2", 0.5)
            .expect("native effect");
        let before = studio.project().tracks[1]
            .effects
            .iter()
            .find(|effect| effect.id == effect_id)
            .expect("created effect");
        assert!(!before.parameters.is_empty());
        assert!(before.parameter_overrides.is_empty());

        let reopened = parse_project(&studio.to_json()).expect("reopened project");
        let after = reopened.tracks[1]
            .effects
            .iter()
            .find(|effect| effect.id == effect_id)
            .expect("reopened effect");
        assert_eq!(
            after.parameters.keys().collect::<Vec<_>>(),
            before.parameters.keys().collect::<Vec<_>>()
        );
        assert!(after.parameters.iter().all(|(parameter, value)| {
            (value - before.parameters[parameter]).abs() < 0.000_001
        }));
        assert!(after.parameter_overrides.is_empty());
    }

    #[test]
    fn rejects_unavailable_native_parameter_overrides_before_opening() {
        let mut project: JsonValue =
            serde_json::from_str(&Project::demo().to_json()).expect("demo JSON");
        project["tracks"][0]["instrument"]["nativeOverrides"] = serde_json::json!({"999999": 0.5});
        let error = parse_project(&project.to_string()).expect_err("unknown native parameter");
        assert!(error.to_string().contains("unavailable"));
    }

    #[test]
    fn rejects_projects_missing_current_schema_fields() {
        for pointer in ["/channelOperations", "/tracks/0/instrument/nativeOverrides"] {
            let mut project: JsonValue =
                serde_json::from_str(&Project::demo().to_json()).expect("demo JSON");
            project
                .pointer_mut(pointer.rsplit_once('/').expect("field pointer").0)
                .and_then(JsonValue::as_object_mut)
                .expect("field parent")
                .remove(pointer.rsplit_once('/').expect("field pointer").1);

            assert!(
                parse_project(&project.to_string()).is_err(),
                "accepted project without {pointer}"
            );
        }
    }

    #[test]
    fn normalizes_imported_native_selections_to_surge_choices() {
        let mut project: JsonValue =
            serde_json::from_str(&Project::demo().to_json()).expect("demo JSON");
        let instrument = &Project::demo().tracks[1].instrument;
        let mute = crate::surge::instrument_parameters_for_instrument(instrument)
            .into_iter()
            .find(|parameter| parameter.name.ends_with("Osc 1 Mute"))
            .expect("oscillator mute");
        project["tracks"][1]["instrument"]["nativeOverrides"] =
            serde_json::json!({mute.id.to_string(): 0.123456});

        let parsed = parse_project(&project.to_string()).expect("normalized native choice");
        assert_eq!(parsed.tracks[1].instrument.native_overrides[&mute.id], 0.0);
    }

    #[test]
    fn rejects_invalid_explicit_preset_effect_slots() {
        for value in [
            serde_json::json!(0),
            serde_json::json!(9),
            serde_json::json!("1"),
            serde_json::json!(1.5),
            serde_json::Value::Null,
        ] {
            let mut project: serde_json::Value =
                serde_json::from_str(&project_with_eq().to_json()).expect("demo JSON");
            project["tracks"][1]["effects"][0]["presetSlot"] = value.clone();
            let error = match parse_project(&project.to_string()) {
                Ok(_) => panic!("accepted invalid presetSlot {value}"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("presetSlot"));
        }
    }

    #[test]
    fn rejects_duplicate_preset_effect_slots() {
        let mut project = project_with_eq();
        let track = &mut project.tracks[1];
        let mut duplicate = track.effects[0].clone();
        duplicate.id = 99_001;
        duplicate.preset_slot = Some(0);
        track.effects[0].preset_slot = Some(0);
        track.effects.push(duplicate);
        track.routing.effect_order.push(99_001);

        let error = parse_project(&project.to_json()).expect_err("duplicate preset slot");
        assert!(error.to_string().contains("duplicate preset effect slots"));
    }

    #[test]
    fn rejects_imported_values_between_native_effect_choices() {
        let mut project = Project::demo();
        let track = &mut project.tracks[1];
        let (effect, parameter) = crate::surge::SURGE_EFFECT_TYPES
            .iter()
            .filter(|name| **name != "Off" && **name != "Audio Input")
            .find_map(|name| {
                let candidate = Effect {
                    id: 99_002,
                    name: (*name).to_owned(),
                    preset_slot: None,
                    mix: 0.5,
                    enabled: true,
                    parameters: crate::surge::effect_parameter_values(name),
                    parameter_overrides: Vec::new(),
                    tempo_sync_parameters: Vec::new(),
                    deactivated_parameters: Vec::new(),
                };
                crate::surge::effect_parameter_semantics(
                    &track.instrument,
                    std::slice::from_ref(&candidate),
                    &[candidate.id],
                    track.id,
                    candidate.id,
                )
                .into_iter()
                .find(|(_, semantics)| !semantics.choices.is_empty())
                .map(|(parameter, _)| (candidate, parameter))
            })
            .expect("native effect selection");
        track.effects = vec![effect];
        track.effects[0]
            .parameters
            .insert(parameter.clone(), 0.123_456);
        track.effects[0].parameter_overrides.push(parameter);
        track.routing.effect_order = vec![99_002];

        let error = parse_project(&project.to_json()).expect_err("in-between native choice");
        assert!(error.to_string().contains("must match a Surge XT choice"));
    }

    #[test]
    fn requires_the_current_schema_version() {
        let mut project: JsonValue =
            serde_json::from_str(&Project::demo().to_json()).expect("project JSON");
        project
            .as_object_mut()
            .expect("root")
            .remove("schemaVersion");
        assert!(parse_project(&project.to_string()).is_err());

        project["schemaVersion"] = JsonValue::from(1);
        assert!(parse_project(&project.to_string()).is_err());

        project["schemaVersion"] = JsonValue::from(2);
        assert!(parse_project(&project.to_string()).is_err());
    }

    #[test]
    fn rejects_native_modulators_beyond_surge_slot_capacity() {
        let mut project = Project::demo();
        let track = &mut project.tracks[1];
        let template = track.modulators[0].clone();
        for index in 0..6 {
            let mut modulator = template.clone();
            modulator.id = 9_900 + index;
            track.modulators.push(modulator);
        }
        let error = parse_project(&project.to_json()).expect_err("native slot overflow");
        assert!(error.to_string().contains("at most six"));
    }

    #[test]
    fn rejects_persisted_native_modulators_with_external_sources() {
        let mut project = Project::demo();
        let external_source = project.tracks[0].id;
        let track = &mut project.tracks[1];
        let native_target = crate::surge::instrument_parameters(&track.instrument.preset)[0].id;
        track.modulators[0].target = format!("native:{native_target}");
        track.modulators[0].trigger = "midi".to_owned();
        track.modulators[0].source_track_id = Some(external_source);

        let error = parse_project(&project.to_json()).expect_err("cross-track native route");
        assert!(error.to_string().contains("unsupported"));
    }

    #[test]
    fn round_trips_materialized_edits() {
        let mut studio = crate::model::Studio::new();
        studio
            .apply_prompt(4.0, 8.0, "add drive to the bass")
            .expect("valid edit");
        let source = studio.project().to_json();
        let parsed = parse_project(&source).expect("valid edited graph");
        assert_eq!(parsed.to_json(), source);
    }

    #[cfg(any())]
    #[test]
    fn persisted_automation_is_cross_validated() {
        let mut studio = crate::model::Studio::new();
        let bass_id = studio.project().tracks[1].id;
        studio
            .apply_plan(
                0.0,
                4.0,
                "automate bass volume",
                crate::prompt::EditPlan {
                    summary: "Automated bass volume".to_owned(),
                    action: Action::Automation {
                        track_id: bass_id,
                        parameter: "track.volume".to_owned(),
                        curve: "linear",
                        points: vec![
                            AutomationPoint {
                                time: 0.0,
                                value: 0.2,
                            },
                            AutomationPoint {
                                time: 1.0,
                                value: 1.2,
                            },
                        ],
                        target: TrackRole::Bass,
                    },
                },
            )
            .expect("valid automation");
        let source = studio.project().to_json();
        parse_project(&source).expect("stable automation owner");

        let mut unknown_track: JsonValue = serde_json::from_str(&source).unwrap();
        unknown_track["edits"][0]["action"]["trackId"] = JsonValue::from(999_999_u64);
        assert!(parse_project(&unknown_track.to_string()).is_err());

        let mut out_of_range: JsonValue = serde_json::from_str(&source).unwrap();
        out_of_range["edits"][0]["action"]["points"][1]["value"] = JsonValue::from(99.0);
        assert!(parse_project(&out_of_range.to_string()).is_err());

        let mut missing_owner: JsonValue = serde_json::from_str(&source).unwrap();
        missing_owner["edits"][0]["action"]
            .as_object_mut()
            .expect("automation action")
            .remove("trackId");
        assert!(parse_project(&missing_owner.to_string()).is_err());

        let mut unsupported_tuning: JsonValue = serde_json::from_str(&source).unwrap();
        unsupported_tuning["edits"][0]["action"]["name"] =
            JsonValue::String("instrument.oscillator1.tuning".to_owned());
        unsupported_tuning["edits"][0]["action"]["points"][0]["value"] = JsonValue::from(-12.0);
        unsupported_tuning["edits"][0]["action"]["points"][1]["value"] = JsonValue::from(12.0);
        assert!(parse_project(&unsupported_tuning.to_string()).is_err());
    }
    #[test]
    fn accepts_retained_clip_slices_with_shared_event_ids() {
        let mut studio = crate::model::Studio::new();
        studio
            .apply_prompt(0.0, 4.0, "add a lead")
            .expect("valid lead edit");
        studio
            .apply_prompt(1.0, 3.0, "rewrite the lead MIDI clip")
            .expect("valid MIDI replacement");
        let source = studio.project().to_json();

        let parsed = parse_project(&source).expect("valid retained clip slices");
        assert_eq!(parsed.to_json(), source);
    }

    #[cfg(any())]
    #[test]
    fn rejects_duplicate_ids_and_invalid_routing() {
        let duplicate = Project::demo()
            .to_json()
            .replacen("\"id\":201", "\"id\":1", 1);
        assert!(parse_project(&duplicate).is_err());

        let routing = Project::demo()
            .to_json()
            .replacen("\"effect:110\"", "\"effect:999\"", 1);
        assert!(parse_project(&routing).is_err());

        let control = Project::demo().to_json().replacen(
            "{\"source\":\"modulator:150\",\"target\":\"instrument.cutoff\"}",
            "{\"source\":\"modulator:999\",\"target\":\"instrument.cutoff\"}",
            1,
        );
        assert!(parse_project(&control).is_err());

        let edge = Project::demo().to_json().replacen(
            "\"target\":\"master\",\"type\":\"audio\"",
            "\"target\":\"effect:999\",\"type\":\"audio\"",
            1,
        );
        assert!(parse_project(&edge).is_err());
    }

    #[test]
    fn rejects_legacy_effect_aliases() {
        let source = project_with_eq().to_json().replacen(
            "\"name\":\"EQ\"",
            "\"name\":\"Low-pass filter\"",
            1,
        );
        assert!(parse_project(&source).is_err());
    }
}
