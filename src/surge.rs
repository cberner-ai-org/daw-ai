use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard, OnceLock},
};

use surge_rs::glue::synthesizer::{SurgeId, SurgeSynthesizer};

use crate::model::{Effect, Instrument, Modulator};

pub(crate) const BLOCK_SIZE: usize = 32;
pub(crate) const SERIAL_EFFECT_SLOT_COUNT: usize = 8;
pub(crate) const AUDIO_INPUT_EFFECT_SLOT_COUNT: usize = 1;
const SERIAL_EFFECT_SLOTS: [&str; SERIAL_EFFECT_SLOT_COUNT] = [
    "FX A1", "FX A2", "FX A3", "FX A4", "FX G1", "FX G2", "FX G3", "FX G4",
];

// The alpha binding does not expose Surge's parameter count. This comfortably
// covers the current engine while from_synth_side_id rejects unused indices.
const MAX_NATIVE_PARAMETERS: i32 = 800;
const FILTER_LP12: f32 = 1.0 / 31.0;
const OSC_SINE: f32 = 1.0 / 11.0;
const OSC_SH_NOISE: f32 = 3.0 / 11.0;
const OSC_FM2: f32 = 6.0 / 11.0;
const OSC_MODERN: f32 = 8.0 / 11.0;

fn envelope_time_parameter(milliseconds: f32) -> f32 {
    if milliseconds <= 0.0 {
        0.0
    } else {
        ((milliseconds / 1_000.0).log2() + 10.0).clamp(0.0, 10.0) / 10.0
    }
}

pub(crate) fn is_native_modulator(track_id: u64, modulator: &Modulator) -> bool {
    modulator.enabled
        && modulator.trigger != "audio"
        && (modulator.target.starts_with("instrument.") || modulator.target.starts_with("native:"))
        && modulator
            .source_track_id
            .is_none_or(|source_track_id| source_track_id == track_id)
}

const NATIVE_PARAMETERS: &[(&str, &str)] = &[
    ("attack", "A Amp EG Attack"),
    ("decay", "A Amp EG Decay"),
    ("sustain", "A Amp EG Sustain"),
    ("release", "A Amp EG Release"),
    ("cutoff", "A Filter 1 Cutoff"),
    ("resonance", "A Filter 1 Resonance"),
    ("pitch", "A Pitch"),
    ("output", "A Osc 1 Volume"),
];

const STARTER_PATCH_BASE: &[(&str, f32)] = &[
    ("A Filter 1 Type", FILTER_LP12),
    ("A Osc 1 Retrigger", 1.0),
    ("A Osc 2 Retrigger", 1.0),
    ("A Osc 3 Retrigger", 1.0),
    ("Global Volume", 1.0),
];

static SURGE_ENGINE_LOCK: Mutex<()> = Mutex::new(());
static INSTRUMENT_PARAMETER_CACHE: OnceLock<Mutex<HashMap<String, Vec<InstrumentParameter>>>> =
    OnceLock::new();
#[cfg(test)]
thread_local! {
    static ENGINE_CREATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) struct Engine {
    synth: SurgeSynthesizer,
    _guard: MutexGuard<'static, ()>,
    parameters: HashMap<String, i32>,
    effect_mix_parameters: HashMap<u64, String>,
    effect_parameters: HashMap<(u64, String), String>,
    drum_pitch_range: Option<(u8, u8)>,
    drum_pitch: u8,
    native_modulators: HashMap<u64, NativeModulatorRoute>,
}

#[derive(Clone, Copy)]
struct NativeModulatorRoute {
    lfo: i32,
    target: i32,
    source: i32,
    direction: f32,
    tempo_sync: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct InstrumentParameter {
    pub(crate) id: i32,
    pub(crate) name: String,
    pub(crate) value: f32,
    pub(crate) display: String,
    pub(crate) common: bool,
}

impl Engine {
    pub(crate) fn new(
        instrument: &Instrument,
        effects: &[Effect],
        effect_order: &[u64],
        modulators: &[Modulator],
        track_id: u64,
        sample_rate: f32,
    ) -> Result<Self, String> {
        #[cfg(test)]
        ENGINE_CREATIONS.set(ENGINE_CREATIONS.get() + 1);
        let guard = SURGE_ENGINE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut synth = SurgeSynthesizer::new(sample_rate);
        synth.process();
        let mut engine = Self {
            _guard: guard,
            parameters: parameter_map(&synth),
            synth,
            effect_mix_parameters: HashMap::new(),
            effect_parameters: HashMap::new(),
            drum_pitch_range: drum_pitch_range(&instrument.preset),
            drum_pitch: 0,
            native_modulators: HashMap::new(),
        };
        engine.set_drum_timbre(instrument.timbre);
        engine.apply_preset(&instrument.preset)?;
        engine.set_instrument_parameters(instrument)?;
        engine.set_native_overrides(&instrument.native_overrides)?;
        if !effects.is_empty() {
            engine.apply_effects(effects, effect_order)?;
        }
        engine.apply_native_modulators(modulators, track_id)?;
        Ok(engine)
    }

    fn apply_native_modulators(
        &mut self,
        modulators: &[Modulator],
        track_id: u64,
    ) -> Result<(), String> {
        let mut voice_slot = 0;
        let mut scene_slot = 0;
        for modulator in modulators
            .iter()
            .filter(|modulator| is_native_modulator(track_id, modulator))
        {
            let voice = modulator.trigger == "midi";
            let slot = if voice {
                let slot = voice_slot;
                voice_slot += 1;
                slot
            } else {
                let slot = scene_slot;
                scene_slot += 1;
                slot
            };
            if slot >= 6 {
                return Err(format!(
                    "Surge XT supports at most six {} native modulators per track",
                    if voice {
                        "MIDI-triggered"
                    } else {
                        "free-running"
                    }
                ));
            }
            let target = self.native_modulation_target(&modulator.target)?;
            let shape = match modulator.shape.as_str() {
                "sine" => 0,
                "triangle" => 1,
                "square" => 2,
                "random" => 5,
                "envelope" => 6,
                "formula" => 9,
                _ => {
                    return Err(format!(
                        "Unsupported Surge XT modulation shape: {}",
                        modulator.shape
                    ));
                }
            };
            let native_rate = if modulator.rate_mode == "tempo" {
                modulator.rate * 2.0
            } else {
                modulator.rate
            };
            let rate = ((native_rate.log2() + 8.0) / 18.0).clamp(0.0, 1.0);
            let attack = envelope_time_parameter(modulator.attack_ms);
            let release = envelope_time_parameter(modulator.release_ms);
            let configured = self.synth.configure_lfo(
                0,
                if voice { slot } else { slot + 6 },
                shape,
                rate,
                modulator.rate_mode == "tempo",
                0.0,
                0.0,
                attack,
                release,
                0.0,
                release,
                if voice { 1 } else { 0 },
                modulator.shape == "envelope",
                &modulator.formula,
            );
            let source = if voice { 17 + slot } else { 23 + slot };
            let direction = if modulator.polarity == "decrease" {
                -1.0
            } else {
                1.0
            };
            if !configured
                || !self
                    .synth
                    .set_modulation(target, source, 0, direction * modulator.depth)
            {
                return Err(format!(
                    "Surge XT rejected modulation route to {}",
                    modulator.target
                ));
            }
            self.native_modulators.insert(
                modulator.id,
                NativeModulatorRoute {
                    lfo: if voice { slot } else { slot + 6 },
                    target,
                    source,
                    direction,
                    tempo_sync: modulator.rate_mode == "tempo",
                },
            );
        }
        Ok(())
    }

    pub(crate) fn set_native_modulator_controls(
        &mut self,
        id: u64,
        rate: f32,
        depth: f32,
    ) -> Result<(), String> {
        let route = self
            .native_modulators
            .get(&id)
            .copied()
            .ok_or_else(|| format!("Surge XT native modulator {id} is unavailable"))?;
        let native_rate = if route.tempo_sync { rate * 2.0 } else { rate };
        let normalized_rate =
            ((native_rate.max(f32::MIN_POSITIVE).log2() + 8.0) / 18.0).clamp(0.0, 1.0);
        if !self
            .synth
            .set_lfo_rate(0, route.lfo, normalized_rate, route.tempo_sync)
            || !self
                .synth
                .set_modulation(route.target, route.source, 0, route.direction * depth)
        {
            return Err(format!(
                "Surge XT rejected runtime controls for native modulator {id}"
            ));
        }
        Ok(())
    }

    fn native_modulation_target(&self, target: &str) -> Result<i32, String> {
        if let Some(index) = target.strip_prefix("native:") {
            return index
                .parse::<i32>()
                .ok()
                .filter(|index| (0..MAX_NATIVE_PARAMETERS).contains(index))
                .ok_or_else(|| format!("Invalid Surge XT modulation target: {target}"));
        }
        let graph_name = target
            .strip_prefix("instrument.")
            .ok_or_else(|| format!("Not a Surge XT modulation target: {target}"))?;
        let native_name = NATIVE_PARAMETERS
            .iter()
            .find_map(|(graph, native)| (*graph == graph_name).then_some(*native))
            .unwrap_or(graph_name);
        self.parameters
            .get(native_name)
            .copied()
            .ok_or_else(|| format!("Surge XT parameter is unavailable: {native_name}"))
    }

    pub(crate) fn play_note(&mut self, key: u8, velocity: f32, note_id: u64) {
        let key = self.drum_pitch_range.map_or(key, |_| self.drum_pitch);
        self.synth.play_note(
            0,
            key.min(127) as i8,
            (velocity.clamp(0.0, 1.0) * 127.0).round() as i8,
            0,
            note_id as i32,
            0,
        );
    }

    pub(crate) fn set_tempo(&mut self, bpm: f64) {
        self.synth.set_tempo(bpm);
    }

    pub(crate) fn release_note(&mut self, key: u8, note_id: u64) {
        if self.drum_pitch_range.is_some() {
            return;
        }
        self.synth
            .release_note(0, key.min(127) as i8, 0, note_id as i32);
    }

    pub(crate) fn set_parameter(&mut self, graph_name: &str, value: f32) -> Result<(), String> {
        if graph_name == "timbre" {
            self.set_drum_timbre(value);
            return Ok(());
        }
        let native_name = NATIVE_PARAMETERS
            .iter()
            .find_map(|(graph, native)| (*graph == graph_name).then_some(*native))
            .unwrap_or(graph_name);
        let index = self
            .parameters
            .get(native_name)
            .copied()
            .ok_or_else(|| format!("Surge XT parameter is unavailable: {native_name}"))?;
        let mut id = SurgeId::empty();
        if !self.synth.from_synth_side_id(index, &mut id) {
            return Err(format!("Surge XT rejected parameter: {native_name}"));
        }
        self.synth
            .set_parameter01(&mut id, value.clamp(0.0, 1.0), None, None);
        Ok(())
    }

    pub(crate) fn process(&mut self) -> [[f32; BLOCK_SIZE]; 2] {
        self.synth.process();
        self.synth.pull_buffer()
    }

    pub(crate) fn process_with_input(
        &mut self,
        input: [[f32; BLOCK_SIZE]; 2],
    ) -> [[f32; BLOCK_SIZE]; 2] {
        self.synth.set_input_buffer(input);
        self.process()
    }

    pub(crate) fn set_effect_mix(&mut self, effect_id: u64, value: f32) -> Result<(), String> {
        let Some(parameter) = self.effect_mix_parameters.get(&effect_id).cloned() else {
            return Ok(());
        };
        self.set_parameter(&parameter, value)
    }

    pub(crate) fn set_effect_parameter(
        &mut self,
        effect_id: u64,
        parameter: &str,
        value: f32,
    ) -> Result<(), String> {
        let Some(native) = self
            .effect_parameters
            .get(&(effect_id, parameter.to_owned()))
            .cloned()
        else {
            return Ok(());
        };
        self.set_parameter(&native, value)
    }

    fn set_instrument_parameters(&mut self, instrument: &Instrument) -> Result<(), String> {
        for (name, value) in [
            ("attack", instrument.attack),
            ("decay", instrument.decay),
            ("sustain", instrument.sustain),
            ("release", instrument.release),
            ("cutoff", instrument.cutoff),
            ("resonance", instrument.resonance),
            ("pitch", instrument.pitch),
            ("timbre", instrument.timbre),
            ("output", instrument.output),
        ] {
            if instrument.overrides(name) {
                self.set_parameter(name, value)?;
            }
        }
        Ok(())
    }

    fn set_native_overrides(
        &mut self,
        overrides: &std::collections::BTreeMap<i32, f32>,
    ) -> Result<(), String> {
        for (&index, &value) in overrides {
            let mut id = SurgeId::empty();
            if !self.synth.from_synth_side_id(index, &mut id) {
                return Err(format!("Surge XT parameter is unavailable: {index}"));
            }
            self.synth
                .set_parameter01(&mut id, value.clamp(0.0, 1.0), None, None);
        }
        Ok(())
    }

    fn set_drum_timbre(&mut self, value: f32) {
        if let Some((minimum, maximum)) = self.drum_pitch_range {
            self.drum_pitch = (f32::from(minimum)
                + value.clamp(0.0, 1.0) * f32::from(maximum - minimum))
            .round() as u8;
        }
    }

    pub(crate) fn instrument_parameter_value(&self, graph_name: &str) -> Option<f32> {
        let native_name = NATIVE_PARAMETERS
            .iter()
            .find_map(|(graph, native)| (*graph == graph_name).then_some(*native))
            .unwrap_or(graph_name);
        self.parameter_value(native_name)
    }

    fn apply_preset(&mut self, preset: &str) -> Result<(), String> {
        if let Some(preset) = preset_parameters(preset) {
            for &(parameter, value) in STARTER_PATCH_BASE.iter().chain(preset) {
                self.set_parameter(parameter, value)?;
            }
        } else if let Some(factory) = crate::surge_presets::find(preset) {
            let mut data = std::fs::read(&factory.path)
                .map_err(|error| format!("could not read Surge XT preset {preset}: {error}"))?;
            self.synth.load_raw(&mut data, Some(true));
        } else {
            return Err(format!("unsupported Surge XT preset: {preset}"));
        }
        // Oscillator types can change the names of their mode parameters.
        self.synth.process();
        self.parameters = parameter_map(&self.synth);
        Ok(())
    }

    fn apply_effects(&mut self, effects: &[Effect], effect_order: &[u64]) -> Result<(), String> {
        let enabled = effect_order
            .iter()
            .filter(|effect_id| {
                effects.iter().any(|effect| {
                    effect.id == **effect_id && effect.enabled && is_native_effect(&effect.name)
                })
            })
            .count();
        if enabled > SERIAL_EFFECT_SLOT_COUNT {
            return Err(format!(
                "Surge XT supports at most {SERIAL_EFFECT_SLOT_COUNT} enabled track effects"
            ));
        }
        // Once the graph declares effects it owns the complete serial chain.
        // Preset effects are replaced so capacity and ordering never depend on hidden preset state.
        for slot in SERIAL_EFFECT_SLOTS {
            self.set_parameter(&format!("{slot} FX Type"), 0.0)?;
        }
        self.synth.process();
        self.parameters = parameter_map(&self.synth);
        let mut available = SERIAL_EFFECT_SLOTS.into_iter();
        for effect_id in effect_order {
            let Some(effect) = effects.iter().find(|effect| {
                effect.id == *effect_id && effect.enabled && is_native_effect(&effect.name)
            }) else {
                continue;
            };
            let type_index = effect_type_index(&effect.name)
                .ok_or_else(|| format!("unsupported Surge XT effect: {}", effect.name))?;
            let slot = available.next().ok_or_else(|| {
                format!(
                    "Surge XT supports at most {SERIAL_EFFECT_SLOT_COUNT} enabled track effects"
                )
            })?;
            self.set_parameter(
                &format!("{slot} FX Type"),
                type_index as f32 / (SURGE_EFFECT_TYPES.len() - 1) as f32,
            )?;
            self.synth.process();
            self.parameters = parameter_map(&self.synth);
            let mix_parameter = format!("{slot} Mix");
            if self.parameters.contains_key(&mix_parameter) {
                self.set_parameter(&mix_parameter, effect.mix)?;
                self.effect_mix_parameters.insert(effect.id, mix_parameter);
            }
            for spec in crate::model::effect_parameter_specs(&effect.name) {
                let native = format!("{slot} {}", spec.native);
                if self.parameters.contains_key(&native) {
                    let value = effect
                        .parameters
                        .get(spec.name)
                        .copied()
                        .unwrap_or(spec.default);
                    self.set_parameter(&native, value)?;
                    self.effect_parameters
                        .insert((effect.id, spec.name.to_owned()), native);
                }
            }
            if effect.name == "Low-pass filter" {
                self.register_legacy_filter_parameters(effect, slot)?;
            }
        }
        Ok(())
    }

    fn register_legacy_filter_parameters(
        &mut self,
        effect: &Effect,
        slot: &str,
    ) -> Result<(), String> {
        for (graph, native, value) in [
            (
                "cutoff",
                "Frequency 3",
                effect.cutoff_hz.map(normalize_filter_cutoff).unwrap_or(0.5),
            ),
            (
                "resonance",
                "Gain 3",
                effect
                    .resonance
                    .map(normalize_filter_resonance)
                    .unwrap_or(0.5),
            ),
        ] {
            let native = format!("{slot} {native}");
            if self.parameters.contains_key(&native) {
                self.set_parameter(&native, value)?;
                self.effect_parameters
                    .insert((effect.id, graph.to_owned()), native);
            }
        }
        Ok(())
    }

    fn parameter_value(&self, name: &str) -> Option<f32> {
        let index = self.parameters.get(name)?;
        let mut id = SurgeId::empty();
        self.synth
            .from_synth_side_id(*index, &mut id)
            .then(|| self.synth.get_parameter01(&mut id))
    }

    #[cfg(test)]
    fn occupied_effect_slots(&self) -> usize {
        SERIAL_EFFECT_SLOTS
            .iter()
            .filter(|slot| {
                self.parameter_value(&format!("{slot} FX Type"))
                    .is_some_and(|value| value >= 0.02)
            })
            .count()
    }

    #[cfg(test)]
    fn effect_parameter_value(&self, effect_id: u64, parameter: &str) -> Option<f32> {
        self.effect_parameters
            .get(&(effect_id, parameter.to_owned()))
            .and_then(|native| self.parameter_value(native))
    }
}

fn drum_pitch_range(preset: &str) -> Option<(u8, u8)> {
    match preset {
        "Surge Kick" => Some((24, 60)),
        "Surge Snare" | "Surge Percussion" => Some((84, 120)),
        "Surge Closed Hat" | "Surge Open Hat" | "Surge Crash" => Some((108, 127)),
        _ => None,
    }
}

pub(crate) fn instrument_parameter_defaults(preset: &str) -> Result<[f32; 8], String> {
    let instrument = Instrument {
        id: 1,
        engine: crate::model::SURGE_ENGINE.to_owned(),
        preset: preset.to_owned(),
        attack: 0.0,
        decay: 0.0,
        sustain: 0.0,
        release: 0.0,
        cutoff: 0.0,
        resonance: 0.0,
        pitch: 0.0,
        timbre: 0.5,
        output: 0.0,
        parameter_overrides: Vec::new(),
        native_overrides: std::collections::BTreeMap::new(),
    };
    let engine = Engine::new(&instrument, &[], &[], &[], 1, 48_000.0)?;
    let value = |name| {
        engine
            .instrument_parameter_value(name)
            .ok_or_else(|| format!("Surge XT parameter is unavailable: {name}"))
    };
    Ok([
        value("attack")?,
        value("decay")?,
        value("sustain")?,
        value("release")?,
        value("cutoff")?,
        value("resonance")?,
        value("pitch")?,
        value("output")?,
    ])
}

pub(crate) fn instrument_parameters(preset: &str) -> Vec<InstrumentParameter> {
    let cache = INSTRUMENT_PARAMETER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(parameters) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(preset)
        .cloned()
    {
        return parameters;
    }
    let instrument = Instrument {
        id: 1,
        engine: crate::model::SURGE_ENGINE.to_owned(),
        preset: preset.to_owned(),
        attack: 0.0,
        decay: 0.0,
        sustain: 0.0,
        release: 0.0,
        cutoff: 0.0,
        resonance: 0.0,
        pitch: 0.0,
        timbre: 0.5,
        output: 0.0,
        parameter_overrides: Vec::new(),
        native_overrides: std::collections::BTreeMap::new(),
    };
    let Ok(engine) = Engine::new(&instrument, &[], &[], &[], 1, 48_000.0) else {
        return Vec::new();
    };
    let parameters = (0..MAX_NATIVE_PARAMETERS)
        .filter_map(|index| {
            let mut id = SurgeId::empty();
            engine.synth.from_synth_side_id(index, &mut id).then(|| {
                let name = engine.synth.get_parameter_accessible_name(&mut id);
                let common = is_common_parameter(&name);
                InstrumentParameter {
                    id: index,
                    name,
                    value: engine.synth.get_parameter01(&mut id),
                    display: engine.synth.get_parameter_display(&mut id),
                    common,
                }
            })
        })
        .collect::<Vec<_>>();
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(preset.to_owned(), parameters.clone());
    parameters
}

pub(crate) fn legacy_instrument_parameter_override(
    instrument: &Instrument,
    native_id: i32,
) -> Option<f32> {
    let native_name = instrument_parameters(&instrument.preset)
        .into_iter()
        .find(|parameter| parameter.id == native_id)?
        .name;
    let graph_name = NATIVE_PARAMETERS.iter().find_map(|(graph, native)| {
        (native_name.ends_with(native) && instrument.overrides(graph)).then_some(*graph)
    })?;
    match graph_name {
        "attack" => Some(instrument.attack),
        "decay" => Some(instrument.decay),
        "sustain" => Some(instrument.sustain),
        "release" => Some(instrument.release),
        "cutoff" => Some(instrument.cutoff),
        "resonance" => Some(instrument.resonance),
        "pitch" => Some(instrument.pitch),
        "output" => Some(instrument.output),
        _ => None,
    }
}

fn is_common_parameter(name: &str) -> bool {
    [
        "Amp EG Attack",
        "Amp EG Decay",
        "Amp EG Sustain",
        "Amp EG Release",
        "Filter 1 Cutoff",
        "Filter 1 Resonance",
        "Osc 1 Volume",
        "Pitch",
        "Global Volume",
    ]
    .iter()
    .any(|candidate| name.contains(candidate))
}

#[cfg(test)]
pub(crate) fn reset_engine_creation_count() {
    ENGINE_CREATIONS.set(0);
}

#[cfg(test)]
pub(crate) fn engine_creation_count() -> usize {
    ENGINE_CREATIONS.get()
}

pub(crate) const SURGE_EFFECT_TYPES: &[&str] = &[
    "Off",
    "Delay",
    "Reverb 1",
    "Phaser",
    "Rotary Speaker",
    "Distortion",
    "EQ",
    "Frequency Shifter",
    "Conditioner",
    "Chorus",
    "Vocoder",
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
    "Tape",
    "Treemonster",
    "Waveshaper",
    "Mid-Side Tool",
    "Spring Reverb",
    "Bonsai",
    "Audio Input",
    "Floaty Delay",
    "Convolution",
];

pub(crate) fn effect_type_index(name: &str) -> Option<usize> {
    let native = match name {
        "Reverb" | "Room" => "Reverb 2",
        "Echo" => "Delay",
        "Low-pass filter" => "EQ",
        "Punch compressor" => "Conditioner",
        "Drive" => "Distortion",
        "Shimmer" => "Nimbus",
        name => name,
    };
    SURGE_EFFECT_TYPES
        .iter()
        .position(|candidate| *candidate == native)
        .filter(|index| *index > 0)
}

pub(crate) fn is_native_effect(name: &str) -> bool {
    effect_type_index(name).is_some()
}

pub(crate) fn normalize_filter_cutoff(value: f32) -> f32 {
    let minimum = crate::model::FILTER_CUTOFF_MIN_HZ;
    let maximum = crate::model::FILTER_CUTOFF_MAX_HZ;
    ((value.clamp(minimum, maximum) / minimum).ln() / (maximum / minimum).ln()).clamp(0.0, 1.0)
}

pub(crate) fn normalize_filter_resonance(value: f32) -> f32 {
    ((value.clamp(
        crate::model::FILTER_RESONANCE_MIN,
        crate::model::FILTER_RESONANCE_MAX,
    ) - crate::model::FILTER_RESONANCE_MIN)
        / (crate::model::FILTER_RESONANCE_MAX - crate::model::FILTER_RESONANCE_MIN))
        .clamp(0.0, 1.0)
}

fn parameter_map(synth: &SurgeSynthesizer) -> HashMap<String, i32> {
    let mut map = HashMap::new();
    for index in 0..MAX_NATIVE_PARAMETERS {
        let mut id = SurgeId::empty();
        if synth.from_synth_side_id(index, &mut id) {
            map.insert(synth.get_parameter_name(&mut id), index);
        }
    }
    map
}

fn preset_parameters(preset: &str) -> Option<&'static [(&'static str, f32)]> {
    match preset {
        "Init" => Some(&[
            ("A Osc 1 Type", 0.0),
            ("A Osc 1 Volume", 0.72),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Kick" => Some(&[
            ("A Osc 1 Type", OSC_SINE),
            ("A Osc 1 Volume", 1.0),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.4),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.2),
            ("A Filter 1 Cutoff", 0.35),
            ("A Filter 1 Resonance", 0.15),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Snare" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 1.0),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.38),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.22),
            ("A Filter 1 Cutoff", 0.82),
            ("A Filter 1 Resonance", 0.1),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Closed Hat" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 1.0),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.18),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.08),
            ("A Filter 1 Cutoff", 0.96),
            ("A Filter 1 Resonance", 0.08),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Open Hat" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 1.0),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.42),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.3),
            ("A Filter 1 Cutoff", 0.94),
            ("A Filter 1 Resonance", 0.08),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Crash" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 1.0),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.7),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.62),
            ("A Filter 1 Cutoff", 0.9),
            ("A Filter 1 Resonance", 0.06),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Percussion" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 0.6),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.3),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.18),
            ("A Filter 1 Cutoff", 0.75),
            ("A Filter 1 Resonance", 0.1),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Bass" => Some(&[
            ("A Osc 1 Type", 0.0),
            ("A Osc 1 Volume", 0.95),
            ("A Osc 2 Mute", 0.0),
            ("A Osc 2 Type", OSC_SINE),
            ("A Osc 2 Octave", 0.25),
            ("A Osc 2 Volume", 0.7),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Pad" => Some(&[
            ("A Osc 1 Type", OSC_MODERN),
            ("A Osc 1 Volume", 0.9),
            ("A Osc 2 Mute", 0.0),
            ("A Osc 2 Type", OSC_SINE),
            ("A Osc 2 Volume", 0.75),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Lead" => Some(&[
            ("A Osc 1 Type", OSC_MODERN),
            ("A Osc 1 Volume", 0.78),
            ("A Osc 2 Mute", 0.0),
            ("A Osc 2 Type", 0.0),
            ("A Osc 2 Volume", 0.28),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Atmosphere" => Some(&[
            ("A Osc 1 Type", OSC_FM2),
            ("A Osc 1 Volume", 0.58),
            ("A Osc 2 Mute", 0.0),
            ("A Osc 2 Type", OSC_SINE),
            ("A Osc 2 Octave", 0.75),
            ("A Osc 2 Volume", 0.32),
            ("A Osc 3 Mute", 1.0),
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_supports_multiple_headless_engines() {
        let instrument = crate::model::Project::demo().tracks[0].instrument.clone();
        for _ in 0..2 {
            let mut engine =
                Engine::new(&instrument, &[], &[], &[], 1, 16_000.0).expect("Surge XT engine");
            engine.process();
        }
    }

    #[test]
    fn native_formula_modulation_changes_the_surge_render() {
        let instrument = crate::model::Project::demo()
            .tracks
            .into_iter()
            .find(|track| track.role == crate::model::TrackRole::Bass)
            .expect("demo bass")
            .instrument;
        let render = |modulators: &[Modulator]| {
            let mut engine = Engine::new(&instrument, &[], &[], modulators, 1, 16_000.0)
                .expect("Surge XT engine");
            engine.play_note(48, 0.9, 1);
            (0..96)
                .flat_map(|_| engine.process()[0])
                .collect::<Vec<_>>()
        };
        let baseline = render(&[]);
        let formula = Modulator {
            id: 99,
            name: "Native formula".to_owned(),
            shape: "formula".to_owned(),
            rate: 2.0,
            rate_mode: "hz".to_owned(),
            trigger: "free".to_owned(),
            source_track_id: None,
            attack_ms: 0.0,
            release_ms: 100.0,
            threshold: 0.0,
            polarity: "increase".to_owned(),
            formula: "function process(state)\n state.output = 1\n return state\nend".to_owned(),
            depth: 0.9,
            target: "instrument.cutoff".to_owned(),
            enabled: true,
        };
        let modulated = render(&[formula]);
        let difference = baseline
            .iter()
            .zip(modulated)
            .map(|(left, right)| (left - right).abs())
            .sum::<f32>()
            / baseline.len() as f32;
        assert!(difference > 0.000_01);
    }

    #[test]
    fn factory_patch_loads_into_the_headless_engine() {
        let mut instrument = crate::model::Project::demo().tracks[2].instrument.clone();
        instrument.preset = "Factory/Pads/Flux Capacitor".to_owned();
        instrument.parameter_overrides.clear();
        let mut engine =
            Engine::new(&instrument, &[], &[], &[], 1, 16_000.0).expect("factory Surge XT patch");
        engine.play_note(60, 0.8, 1);
        let energy = (0..32)
            .map(|_| engine.process())
            .flat_map(|block| block[0])
            .map(f32::abs)
            .sum::<f32>();
        assert!(energy > 0.001, "factory patch rendered silence");
    }

    #[test]
    fn factory_patch_parameters_change_only_when_explicitly_overridden() {
        let mut instrument = crate::model::Project::demo().tracks[2].instrument.clone();
        instrument.preset = "Factory/Leads/Violini Solo".to_owned();
        instrument.parameter_overrides.clear();
        instrument.cutoff = 0.01;
        let native = Engine::new(&instrument, &[], &[], &[], 1, 16_000.0)
            .expect("factory Surge XT patch")
            .instrument_parameter_value("cutoff")
            .expect("native cutoff");
        assert!((native - instrument.cutoff).abs() > 0.01);

        instrument.parameter_overrides.push("cutoff".to_owned());
        let overridden = Engine::new(&instrument, &[], &[], &[], 1, 16_000.0)
            .expect("overridden factory Surge XT patch")
            .instrument_parameter_value("cutoff")
            .expect("overridden cutoff");
        assert!((overridden - instrument.cutoff).abs() < 0.001);
    }

    #[test]
    fn graph_effects_replace_busy_factory_slots_up_to_the_native_limit() {
        let mut instrument = crate::model::Project::demo().tracks[2].instrument.clone();
        instrument.preset = "Factory/Pads/Flux Capacitor".to_owned();
        instrument.parameter_overrides.clear();
        let mut engine =
            Engine::new(&instrument, &[], &[], &[], 1, 16_000.0).expect("factory Surge XT patch");
        engine
            .set_parameter(
                "FX A1 FX Type",
                effect_type_index("Reverb 2").expect("reverb type") as f32
                    / (SURGE_EFFECT_TYPES.len() - 1) as f32,
            )
            .expect("embedded preset effect");
        engine.synth.process();
        engine.parameters = parameter_map(&engine.synth);
        assert_eq!(engine.occupied_effect_slots(), 1);

        let mut effects = (0..SERIAL_EFFECT_SLOT_COUNT)
            .map(|index| Effect {
                id: 100 + index as u64,
                name: "Distortion".to_owned(),
                mix: 0.5,
                cutoff_hz: None,
                resonance: None,
                enabled: true,
                parameters: crate::model::effect_parameter_specs("Distortion")
                    .iter()
                    .map(|spec| (spec.name.to_owned(), spec.default))
                    .collect(),
                parameter_overrides: Vec::new(),
            })
            .collect::<Vec<_>>();
        let order = effects.iter().map(|effect| effect.id).collect::<Vec<_>>();
        engine
            .apply_effects(&effects, &order)
            .expect("full graph-owned Surge effect chain");
        assert_eq!(engine.occupied_effect_slots(), SERIAL_EFFECT_SLOT_COUNT);

        for effect in &mut effects {
            effect.enabled = false;
        }
        engine
            .set_parameter(
                "FX A1 FX Type",
                effect_type_index("Reverb 2").expect("reverb type") as f32
                    / (SURGE_EFFECT_TYPES.len() - 1) as f32,
            )
            .expect("restored embedded preset effect");
        engine
            .apply_effects(&effects, &order)
            .expect("disabled graph-owned effect chain");
        assert_eq!(engine.occupied_effect_slots(), 0);
    }

    #[test]
    fn graph_effect_defaults_are_applied_without_explicit_overrides() {
        let instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        let effect = Effect {
            id: 77,
            name: "Distortion".to_owned(),
            mix: 0.6,
            cutoff_hz: None,
            resonance: None,
            enabled: true,
            parameters: crate::model::effect_parameter_specs("Distortion")
                .iter()
                .map(|spec| (spec.name.to_owned(), spec.default))
                .collect(),
            parameter_overrides: Vec::new(),
        };
        let engine = Engine::new(
            &instrument,
            std::slice::from_ref(&effect),
            &[effect.id],
            &[],
            1,
            16_000.0,
        )
        .expect("graph effect defaults");
        for spec in crate::model::effect_parameter_specs("Distortion") {
            let actual = engine
                .effect_parameter_value(effect.id, spec.name)
                .unwrap_or_else(|| panic!("missing native {} parameter", spec.name));
            assert!(
                (actual - spec.default).abs() < 0.001,
                "{} used native {actual} instead of graph default {}",
                spec.name,
                spec.default
            );
        }
    }

    #[test]
    fn every_exposed_native_effect_loads_in_a_headless_slot() {
        let instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        for name in SURGE_EFFECT_TYPES
            .iter()
            .skip(1)
            .filter(|name| **name != "Audio Input")
        {
            let effect = Effect {
                id: 77,
                name: (*name).to_owned(),
                mix: 0.5,
                cutoff_hz: None,
                resonance: None,
                enabled: true,
                parameters: crate::model::effect_parameter_specs(name)
                    .iter()
                    .map(|spec| (spec.name.to_owned(), spec.default))
                    .collect(),
                parameter_overrides: Vec::new(),
            };
            let mut engine = Engine::new(&instrument, &[effect], &[77], &[], 1, 16_000.0)
                .unwrap_or_else(|error| panic!("{name} did not load: {error}"));
            engine.process();
        }
    }

    #[test]
    fn friendly_effect_aliases_are_native_surge_effects() {
        for alias in [
            "Reverb",
            "Room",
            "Echo",
            "Low-pass filter",
            "Punch compressor",
            "Drive",
            "Shimmer",
        ] {
            assert!(is_native_effect(alias), "{alias} bypassed Surge XT");
        }
    }
}
