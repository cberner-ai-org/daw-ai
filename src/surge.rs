use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use surge_rs::glue::synthesizer::{SurgeId, SurgeSynthesizer};

use crate::model::{Effect, Instrument, Modulator};

pub(crate) const BLOCK_SIZE: usize = 32;
pub(crate) const SERIAL_EFFECT_SLOT_COUNT: usize = 8;
pub(crate) const AUDIO_INPUT_EFFECT_SLOT_COUNT: usize = 1;
const SERIAL_EFFECT_SLOTS: [&str; SERIAL_EFFECT_SLOT_COUNT] = [
    "FX A1", "FX A2", "FX A3", "FX A4", "FX G1", "FX G2", "FX G3", "FX G4",
];
const ALL_EFFECT_SLOTS: [&str; 16] = [
    "FX A1", "FX A2", "FX A3", "FX A4", "FX B1", "FX B2", "FX B3", "FX B4", "FX S1", "FX S2",
    "FX S3", "FX S4", "FX G1", "FX G2", "FX G3", "FX G4",
];

// The alpha binding does not expose Surge's parameter count. This comfortably
// covers the current engine while from_synth_side_id rejects unused indices.
const MAX_NATIVE_PARAMETERS: i32 = 800;
const VOICE_LFO_SOURCE: i32 = 17;
const SCENE_LFO_SOURCE: i32 = 23;
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
type InstrumentParameterCache = HashMap<String, Arc<OnceLock<Vec<InstrumentParameter>>>>;
static INSTRUMENT_PARAMETER_CACHE: OnceLock<Mutex<InstrumentParameterCache>> = OnceLock::new();
static EFFECT_PARAMETER_CACHE: OnceLock<Mutex<HashMap<String, BTreeMap<String, f32>>>> =
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
    pub(crate) preset_value: f32,
    pub(crate) display: String,
    pub(crate) common: bool,
    pub(crate) boolean: bool,
    pub(crate) discrete: bool,
    pub(crate) bipolar: bool,
    pub(crate) tempo_sync: bool,
    pub(crate) can_deactivate: bool,
    pub(crate) deactivated: bool,
    pub(crate) choices: Vec<(f32, String)>,
    pub(crate) voice_modulatable: bool,
    pub(crate) scene_modulatable: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct EffectParameterSemantics {
    pub(crate) value: f32,
    pub(crate) display: String,
    pub(crate) boolean: bool,
    pub(crate) discrete: bool,
    pub(crate) bipolar: bool,
    pub(crate) tempo_sync: bool,
    pub(crate) can_deactivate: bool,
    pub(crate) deactivated: bool,
    pub(crate) choices: Vec<(f32, String)>,
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
        Self::new_with_graph_effects(
            instrument,
            effects,
            effect_order,
            modulators,
            track_id,
            sample_rate,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_graph_effects(
        instrument: &Instrument,
        effects: &[Effect],
        effect_order: &[u64],
        modulators: &[Modulator],
        track_id: u64,
        sample_rate: f32,
        graph_owns_effects: bool,
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
        if graph_owns_effects {
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
            let source = if voice {
                VOICE_LFO_SOURCE + slot
            } else {
                SCENE_LFO_SOURCE + slot
            };
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
        if !self
            .synth
            .set_parameter01(&mut id, value.clamp(0.0, 1.0), None, None)
        {
            return Err(format!(
                "Surge XT cannot use this {native_name} value in the current patch"
            ));
        }
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
            // Old sessions may contain oscillator choices which require wavetable
            // data the loaded patch does not provide. Keep the patch's safe value.
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
            std::fs::File::open(&factory.path)
                .map_err(|error| format!("could not read Surge XT preset {preset}: {error}"))?;
            self.synth
                .load_patch_by_path(&factory.path, -1, preset, false);
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
        // preset_slot is provenance; effect_order owns the runtime serial order.
        self.effect_mix_parameters.clear();
        self.effect_parameters.clear();
        for slot in ALL_EFFECT_SLOTS {
            let parameter = format!("{slot} FX Type");
            if self
                .parameter_value(&parameter)
                .is_some_and(|value| value >= 0.02)
            {
                self.set_parameter(&parameter, 0.0)?;
                self.synth.process();
                self.parameters = parameter_map(&self.synth);
            }
        }
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
            for native in self.parameters.keys() {
                if let Some(parameter) =
                    native
                        .strip_prefix(&format!("{slot} "))
                        .filter(|parameter| {
                            *parameter != "FX Type"
                                && *parameter != "Mix"
                                && !is_generic_effect_parameter(parameter)
                        })
                {
                    self.effect_parameters
                        .insert((effect.id, parameter.to_owned()), native.clone());
                }
            }
            let mix_parameter = format!("{slot} Mix");
            if self.parameters.contains_key(&mix_parameter) {
                self.set_parameter(&mix_parameter, effect.mix)?;
                self.effect_mix_parameters.insert(effect.id, mix_parameter);
            }
            for (parameter, value) in &effect.parameters {
                let native = format!("{slot} {parameter}");
                if self.parameters.contains_key(&native) {
                    self.set_parameter(&native, *value)?;
                    self.effect_parameters
                        .insert((effect.id, parameter.clone()), native);
                }
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

    fn parameter_semantics(&self, name: &str) -> Option<EffectParameterSemantics> {
        let index = *self.parameters.get(name)?;
        let mut id = SurgeId::empty();
        self.synth
            .from_synth_side_id(index, &mut id)
            .then(|| EffectParameterSemantics {
                value: self.synth.get_parameter01(&mut id),
                display: self.synth.get_parameter_display(&mut id),
                boolean: self.synth.parameter_is_boolean(&id),
                discrete: self.synth.parameter_is_discrete(&id),
                bipolar: self.synth.parameter_is_bipolar(&id),
                tempo_sync: self.synth.parameter_can_temposync(&id),
                can_deactivate: self.synth.parameter_can_deactivate(&id),
                deactivated: self.synth.parameter_is_deactivated(&id),
                choices: self.synth.parameter_choices(&id),
            })
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

    #[cfg(test)]
    fn parameter_display(&self, name: &str) -> Option<String> {
        let index = *self.parameters.get(name)?;
        let mut id = SurgeId::empty();
        self.synth
            .from_synth_side_id(index, &mut id)
            .then(|| self.synth.get_parameter_display(&mut id))
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
    let entry = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(preset.to_owned())
        .or_insert_with(|| Arc::new(OnceLock::new()))
        .clone();
    entry
        .get_or_init(|| {
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
            (0..MAX_NATIVE_PARAMETERS)
                .filter_map(|index| {
                    let mut id = SurgeId::empty();
                    engine.synth.from_synth_side_id(index, &mut id).then(|| {
                        let name = engine.synth.get_parameter_accessible_name(&mut id);
                        let common = is_common_parameter(&name);
                        InstrumentParameter {
                            id: index,
                            name,
                            value: engine.synth.get_parameter01(&mut id),
                            preset_value: engine.synth.get_parameter01(&mut id),
                            display: engine.synth.get_parameter_display(&mut id),
                            common,
                            boolean: engine.synth.parameter_is_boolean(&id),
                            discrete: engine.synth.parameter_is_discrete(&id),
                            bipolar: engine.synth.parameter_is_bipolar(&id),
                            tempo_sync: engine.synth.parameter_can_temposync(&id),
                            can_deactivate: engine.synth.parameter_can_deactivate(&id),
                            deactivated: engine.synth.parameter_is_deactivated(&id),
                            choices: engine.synth.parameter_choices(&id),
                            voice_modulatable: engine
                                .synth
                                .is_valid_modulation(index, VOICE_LFO_SOURCE),
                            scene_modulatable: engine
                                .synth
                                .is_valid_modulation(index, SCENE_LFO_SOURCE),
                        }
                    })
                })
                .collect()
        })
        .clone()
}

pub(crate) fn instrument_parameters_for_instrument(
    instrument: &Instrument,
) -> Vec<InstrumentParameter> {
    let parameters = instrument_parameters(&instrument.preset);
    if instrument.native_overrides.is_empty() && instrument.parameter_overrides.is_empty() {
        return parameters;
    }
    let Ok(engine) = Engine::new(instrument, &[], &[], &[], 1, 48_000.0) else {
        return parameters;
    };
    let preset_values = parameters
        .into_iter()
        .map(|parameter| (parameter.id, parameter.preset_value))
        .collect::<HashMap<_, _>>();
    (0..MAX_NATIVE_PARAMETERS)
        .filter_map(|index| {
            let mut id = SurgeId::empty();
            engine.synth.from_synth_side_id(index, &mut id).then(|| {
                let name = engine.synth.get_parameter_accessible_name(&mut id);
                InstrumentParameter {
                    id: index,
                    common: is_common_parameter(&name),
                    name,
                    value: engine.synth.get_parameter01(&mut id),
                    preset_value: preset_values
                        .get(&index)
                        .copied()
                        .unwrap_or_else(|| engine.synth.get_parameter01(&mut id)),
                    display: engine.synth.get_parameter_display(&mut id),
                    boolean: engine.synth.parameter_is_boolean(&id),
                    discrete: engine.synth.parameter_is_discrete(&id),
                    bipolar: engine.synth.parameter_is_bipolar(&id),
                    tempo_sync: engine.synth.parameter_can_temposync(&id),
                    can_deactivate: engine.synth.parameter_can_deactivate(&id),
                    deactivated: engine.synth.parameter_is_deactivated(&id),
                    choices: engine.synth.parameter_choices(&id),
                    voice_modulatable: engine.synth.is_valid_modulation(index, VOICE_LFO_SOURCE),
                    scene_modulatable: engine.synth.is_valid_modulation(index, SCENE_LFO_SOURCE),
                }
            })
        })
        .collect()
}

pub(crate) fn instrument_parameter_is_modulatable(
    instrument: &Instrument,
    target: &str,
    trigger: &str,
) -> bool {
    let native_id = target
        .strip_prefix("native:")
        .and_then(|id| id.parse::<i32>().ok())
        .or_else(|| {
            let graph_name = target.strip_prefix("instrument.")?;
            instrument_parameters_for_instrument(instrument)
                .into_iter()
                .find(|parameter| {
                    instrument_graph_parameter(&instrument.preset, parameter.id) == Some(graph_name)
                })
                .map(|parameter| parameter.id)
        });
    let Some(id) = native_id else {
        return false;
    };
    instrument_parameters_for_instrument(instrument)
        .into_iter()
        .find(|parameter| parameter.id == id)
        .is_some_and(|parameter| match trigger {
            "midi" => parameter.voice_modulatable,
            "free" => parameter.scene_modulatable,
            _ => false,
        })
}

pub(crate) fn effect_parameter_semantics(
    instrument: &Instrument,
    effects: &[Effect],
    _effect_order: &[u64],
    track_id: u64,
    effect_id: u64,
) -> HashMap<String, EffectParameterSemantics> {
    let Some(mut effect) = effects
        .iter()
        .find(|effect| effect.id == effect_id)
        .cloned()
    else {
        return HashMap::new();
    };
    effect.enabled = true;
    let semantic_order = [effect.id];
    let Ok(engine) = Engine::new(
        instrument,
        std::slice::from_ref(&effect),
        &semantic_order,
        &[],
        track_id,
        48_000.0,
    ) else {
        return HashMap::new();
    };
    let mut result = HashMap::new();
    if let Some(native) = engine.effect_mix_parameters.get(&effect_id) {
        if let Some(semantics) = engine.parameter_semantics(native) {
            result.insert("mix".to_owned(), semantics);
        }
    }
    for ((candidate_id, parameter), native) in &engine.effect_parameters {
        if *candidate_id == effect_id {
            if let Some(semantics) = engine.parameter_semantics(native) {
                result.insert(parameter.clone(), semantics);
            }
        }
    }
    result
}

pub(crate) fn preset_effects(preset: &str) -> Result<Vec<Effect>, String> {
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
    let engine = Engine::new_with_graph_effects(&instrument, &[], &[], &[], 1, 48_000.0, false)?;
    let mut effects = Vec::new();
    for (slot_index, slot) in SERIAL_EFFECT_SLOTS.iter().enumerate() {
        let Some(kind) = engine.parameter_value(&format!("{slot} FX Type")) else {
            continue;
        };
        let type_index = (kind * (SURGE_EFFECT_TYPES.len() - 1) as f32).round() as usize;
        let Some(name) = SURGE_EFFECT_TYPES
            .get(type_index)
            .filter(|name| **name != "Off")
        else {
            continue;
        };
        let mix = engine
            .parameter_value(&format!("{slot} Mix"))
            .unwrap_or(1.0);
        let parameters = engine
            .parameters
            .keys()
            .filter_map(|native| {
                native
                    .strip_prefix(&format!("{slot} "))
                    .filter(|name| {
                        *name != "FX Type" && *name != "Mix" && !is_generic_effect_parameter(name)
                    })
                    .and_then(|name| {
                        engine
                            .parameter_value(native)
                            .map(|value| (name.to_owned(), value))
                    })
            })
            .collect();
        effects.push(Effect {
            id: 0,
            name: (*name).to_owned(),
            preset_slot: Some(slot_index),
            mix,
            cutoff_hz: None,
            resonance: None,
            enabled: true,
            parameters,
            parameter_overrides: Vec::new(),
        });
    }
    Ok(effects)
}

pub(crate) fn effect_parameter_values(name: &str) -> BTreeMap<String, f32> {
    let cache = EFFECT_PARAMETER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(parameters) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned()
    {
        return parameters;
    }
    let instrument = Instrument {
        id: 1,
        engine: crate::model::SURGE_ENGINE.to_owned(),
        preset: "Init".to_owned(),
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
        native_overrides: BTreeMap::new(),
    };
    let Ok(mut engine) = Engine::new(&instrument, &[], &[], &[], 1, 48_000.0) else {
        return BTreeMap::new();
    };
    let Some(type_index) = effect_type_index(name) else {
        return BTreeMap::new();
    };
    let slot = SERIAL_EFFECT_SLOTS[0];
    if engine
        .set_parameter(
            &format!("{slot} FX Type"),
            type_index as f32 / (SURGE_EFFECT_TYPES.len() - 1) as f32,
        )
        .is_err()
    {
        return BTreeMap::new();
    }
    engine.synth.process();
    engine.parameters = parameter_map(&engine.synth);
    let parameters: BTreeMap<String, f32> = engine
        .parameters
        .keys()
        .filter_map(|native| {
            native
                .strip_prefix(&format!("{slot} "))
                .filter(|parameter| {
                    *parameter != "FX Type"
                        && *parameter != "Mix"
                        && !is_generic_effect_parameter(parameter)
                })
                .and_then(|parameter| {
                    engine
                        .parameter_value(native)
                        .map(|value| (parameter.to_owned(), value))
                })
        })
        .collect();
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.to_owned(), parameters.clone());
    parameters
}

fn is_generic_effect_parameter(name: &str) -> bool {
    name.strip_prefix("Param ")
        .is_some_and(|number| number.parse::<u8>().is_ok())
}

pub(crate) fn legacy_instrument_parameter_override(
    instrument: &Instrument,
    native_id: i32,
) -> Option<f32> {
    let graph_name = instrument_graph_parameter(&instrument.preset, native_id)?;
    if !instrument.overrides(graph_name) {
        return None;
    }
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

pub(crate) fn instrument_graph_parameter(preset: &str, native_id: i32) -> Option<&'static str> {
    let native_name = instrument_parameters(preset)
        .into_iter()
        .find(|parameter| parameter.id == native_id)?
        .name;
    NATIVE_PARAMETERS
        .iter()
        .find_map(|(graph, native)| native_name.ends_with(native).then_some(*graph))
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
    SURGE_EFFECT_TYPES
        .iter()
        .position(|candidate| *candidate == name)
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
    fn graph_effects_append_after_visible_preset_slots_up_to_the_native_limit() {
        let distortion_parameters = crate::surge::effect_parameter_values("Distortion");
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

        let preset_effect = Effect {
            id: 99,
            name: "Reverb 2".to_owned(),
            preset_slot: Some(0),
            mix: engine
                .parameter_value("FX A1 Mix")
                .expect("preset effect mix"),
            cutoff_hz: None,
            resonance: None,
            enabled: true,
            parameters: BTreeMap::new(),
            parameter_overrides: Vec::new(),
        };
        let mut effects = (0..SERIAL_EFFECT_SLOT_COUNT - 1)
            .map(|index| Effect {
                id: 100 + index as u64,
                name: "Distortion".to_owned(),
                preset_slot: None,
                mix: 0.5,
                cutoff_hz: None,
                resonance: None,
                enabled: true,
                parameters: distortion_parameters.clone(),
                parameter_overrides: Vec::new(),
            })
            .collect::<Vec<_>>();
        effects.insert(0, preset_effect);
        let order = effects.iter().map(|effect| effect.id).collect::<Vec<_>>();
        engine
            .apply_effects(&effects, &order)
            .expect("preset and added Surge effect chain");
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
    fn graph_effect_rebuild_clears_unsupported_scene_and_send_slots() {
        let instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        let mut engine =
            Engine::new_with_graph_effects(&instrument, &[], &[], &[], 1, 16_000.0, false)
                .expect("raw preset engine");
        let distortion = effect_type_index("Distortion").expect("distortion type") as f32
            / (SURGE_EFFECT_TYPES.len() - 1) as f32;
        engine
            .set_parameter("FX B1 FX Type", distortion)
            .expect("Scene B effect");
        engine
            .set_parameter("FX S1 FX Type", distortion)
            .expect("send effect");
        engine.apply_effects(&[], &[]).expect("empty graph chain");

        assert_eq!(
            engine.parameter_display("FX B1 FX Type").as_deref(),
            Some("Off")
        );
        assert_eq!(
            engine.parameter_display("FX S1 FX Type").as_deref(),
            Some("Off")
        );
    }

    #[test]
    fn routing_order_rebuilds_gapped_preset_and_added_effect_slots() {
        let instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        let preset = Effect {
            id: 90,
            name: "Reverb 2".to_owned(),
            preset_slot: Some(1),
            mix: 0.2,
            cutoff_hz: None,
            resonance: None,
            enabled: true,
            parameters: effect_parameter_values("Reverb 2"),
            parameter_overrides: Vec::new(),
        };
        let added = Effect {
            id: 91,
            name: "Distortion".to_owned(),
            preset_slot: None,
            mix: 0.8,
            cutoff_hz: None,
            resonance: None,
            enabled: true,
            parameters: effect_parameter_values("Distortion"),
            parameter_overrides: Vec::new(),
        };
        let effects = [preset, added];
        let first = Engine::new(
            &instrument,
            &effects,
            &[effects[0].id, effects[1].id],
            &[],
            1,
            16_000.0,
        )
        .expect("preset then added");
        assert_eq!(
            first.parameter_display("FX A1 FX Type").as_deref(),
            Some("Reverb 2")
        );
        assert_eq!(
            first.parameter_display("FX A2 FX Type").as_deref(),
            Some("Distortion")
        );
        drop(first);

        let reversed = Engine::new(
            &instrument,
            &effects,
            &[effects[1].id, effects[0].id],
            &[],
            1,
            16_000.0,
        )
        .expect("added then preset");
        assert_eq!(
            reversed.parameter_display("FX A1 FX Type").as_deref(),
            Some("Distortion")
        );
        assert_eq!(
            reversed.parameter_display("FX A2 FX Type").as_deref(),
            Some("Reverb 2")
        );
    }

    #[test]
    fn factory_preset_and_added_effect_chain_renders_audio() {
        let mut instrument = crate::model::Project::demo().tracks[2].instrument.clone();
        instrument.preset = "Factory/Basses/Evilous".to_owned();
        instrument.parameter_overrides.clear();
        let mut effects = preset_effects(&instrument.preset).expect("factory preset effects");
        for (index, name) in ["Flanger", "EQ", "CHOW", "Delay"].iter().enumerate() {
            effects.push(Effect {
                id: 200 + index as u64,
                name: (*name).to_owned(),
                preset_slot: None,
                mix: 0.5,
                cutoff_hz: None,
                resonance: None,
                enabled: true,
                parameters: crate::surge::effect_parameter_values(name),
                parameter_overrides: Vec::new(),
            });
        }
        for (index, effect) in effects.iter_mut().enumerate() {
            effect.id = 100 + index as u64;
        }
        let order = effects.iter().map(|effect| effect.id).collect::<Vec<_>>();
        let modulator = Modulator {
            id: 113,
            name: "AI modulation".to_owned(),
            shape: "triangle".to_owned(),
            rate: 4.0,
            rate_mode: "hz".to_owned(),
            trigger: "midi".to_owned(),
            source_track_id: Some(1),
            attack_ms: 0.0,
            release_ms: 10.0,
            threshold: 0.0,
            polarity: "increase".to_owned(),
            formula: String::new(),
            depth: 0.8,
            target: "instrument.cutoff".to_owned(),
            enabled: true,
        };
        let mut engine = Engine::new(&instrument, &effects, &order, &[modulator], 1, 16_000.0)
            .expect("mixed preset and added effect chain");
        engine.play_note(41, 0.95, 1);
        for _ in 0..3_500 {
            engine
                .set_native_modulator_controls(113, 4.0, 0.8)
                .expect("runtime modulation controls");
            for effect in &effects {
                engine
                    .set_effect_mix(effect.id, effect.mix)
                    .expect("runtime effect mix");
            }
            engine.process();
        }
        drop(engine);
        instrument.preset = "Factory/Basses/Sub 1".to_owned();
        let mut next =
            Engine::new(&instrument, &[], &[], &[], 2, 16_000.0).expect("next track engine");
        next.play_note(29, 0.9, 2);
        for _ in 0..3_500 {
            next.process();
        }
    }

    #[test]
    fn graph_effect_uses_native_surge_defaults() {
        let instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        let effect = Effect {
            id: 77,
            name: "Distortion".to_owned(),
            preset_slot: None,
            mix: 0.6,
            cutoff_hz: None,
            resonance: None,
            enabled: true,
            parameters: crate::surge::effect_parameter_values("Distortion"),
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
        for (parameter, expected) in &effect.parameters {
            let actual = engine
                .effect_parameter_value(effect.id, parameter)
                .unwrap_or_else(|| panic!("missing native {parameter} parameter"));
            assert!(
                (actual - expected).abs() < 0.001,
                "{parameter} used {actual} instead of Surge default {expected}"
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
                preset_slot: None,
                mix: 0.5,
                cutoff_hz: None,
                resonance: None,
                enabled: true,
                parameters: crate::surge::effect_parameter_values(name),
                parameter_overrides: Vec::new(),
            };
            let mut engine = Engine::new(&instrument, &[effect], &[77], &[], 1, 16_000.0)
                .unwrap_or_else(|error| panic!("{name} did not load: {error}"));
            engine.process();
        }
    }

    #[test]
    fn disabled_effects_keep_their_native_parameter_semantics() {
        let instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        let mut effect = crate::model::Project::demo().tracks[1].effects[0].clone();
        effect.enabled = false;
        effect.id = 99;
        let mut effects = (0..SERIAL_EFFECT_SLOT_COUNT)
            .map(|index| {
                let mut enabled = effect.clone();
                enabled.id = index as u64 + 1;
                enabled.enabled = true;
                enabled
            })
            .collect::<Vec<_>>();
        effects.push(effect.clone());
        let order = effects.iter().map(|effect| effect.id).collect::<Vec<_>>();
        let semantics = effect_parameter_semantics(&instrument, &effects, &order, 1, effect.id);

        assert!(semantics.contains_key("mix"));
        assert!(
            effect
                .parameters
                .keys()
                .any(|parameter| semantics.contains_key(parameter))
        );
    }

    #[test]
    fn effect_names_are_exactly_surge_names() {
        assert!(is_native_effect("Reverb 2"));
        assert!(is_native_effect("Distortion"));
        assert!(!is_native_effect("Room"));
        assert!(!is_native_effect("Drive"));
    }

    #[test]
    fn headless_choices_exclude_oscillator_types_without_required_data() {
        let mut instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        instrument.preset = "Factory/Basses/Behemoth".to_owned();
        instrument.native_overrides.insert(234, 2.0 / 11.0);

        let parameters = instrument_parameters_for_instrument(&instrument);
        let oscillator_type = parameters
            .iter()
            .find(|parameter| parameter.id == 234)
            .expect("oscillator type");
        assert!(
            oscillator_type
                .choices
                .iter()
                .all(|(_, display)| display != "Wavetable" && display != "Window")
        );

        let mut engine = Engine::new(&instrument, &[], &[], &[], 1, 16_000.0).expect("safe engine");
        engine.play_note(36, 1.0, 1);
        engine.process();
    }

    #[test]
    fn overridden_parameter_keeps_its_cached_preset_value() {
        let mut instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        let parameter = instrument_parameters(&instrument.preset)
            .into_iter()
            .find(|parameter| parameter.name.ends_with("Osc 1 Mute"))
            .expect("oscillator mute");
        instrument.native_overrides.insert(parameter.id, 1.0);

        let current = instrument_parameters_for_instrument(&instrument)
            .into_iter()
            .find(|candidate| candidate.id == parameter.id)
            .expect("current oscillator mute");
        assert_eq!(current.value, 1.0);
        assert_eq!(current.preset_value, parameter.preset_value);
    }
}
