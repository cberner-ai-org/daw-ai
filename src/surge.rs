use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use surge_rs::glue::synthesizer::{SurgeId, SurgeSynthesizer};

use crate::model::{Effect, Instrument, Modulator};

pub(crate) const BLOCK_SIZE: usize = 32;
pub(crate) const SERIAL_EFFECT_SLOT_COUNT: usize = 8;
const SERIAL_EFFECT_SLOTS: [&str; SERIAL_EFFECT_SLOT_COUNT] = [
    "FX A1", "FX A2", "FX A3", "FX A4", "FX G1", "FX G2", "FX G3", "FX G4",
];

// The alpha binding does not expose Surge's parameter count. This comfortably
// covers the current engine while from_synth_side_id rejects unused indices.
const MAX_NATIVE_PARAMETERS: i32 = 800;
const VOICE_LFO_SOURCE: i32 = 17;
const SCENE_LFO_SOURCE: i32 = 23;

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
        && modulator.target.starts_with("native:")
        && modulator
            .source_track_id
            .is_none_or(|source_track_id| source_track_id == track_id)
}

static SURGE_ENGINE_LOCK: Mutex<()> = Mutex::new(());
type InstrumentParameterCache = HashMap<String, Arc<Mutex<Option<Vec<InstrumentParameter>>>>>;
static INSTRUMENT_PARAMETER_CACHE: OnceLock<Mutex<InstrumentParameterCache>> = OnceLock::new();
static EFFECT_PARAMETER_CACHE: OnceLock<Mutex<HashMap<String, BTreeMap<String, f32>>>> =
    OnceLock::new();
static EFFECT_CONTROL_SEMANTICS_CACHE: OnceLock<
    Mutex<HashMap<String, HashMap<String, EffectParameterSemantics>>>,
> = OnceLock::new();
pub(crate) struct Engine {
    synth: SurgeSynthesizer,
    _guard: MutexGuard<'static, ()>,
    parameters: HashMap<String, i32>,
    effect_mix_parameters: HashMap<u64, String>,
    effect_parameters: HashMap<(u64, String), String>,
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
        };
        engine.apply_preset(&instrument.preset)?;
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
        Err(format!("Not a Surge XT modulation target: {target}"))
    }

    pub(crate) fn play_note(&mut self, key: u8, velocity: f32, note_id: u64) {
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

    pub(crate) fn set_free_modulator_phases(
        &mut self,
        modulators: &[Modulator],
        project_time: f64,
        bpm: f64,
    ) -> Result<(), String> {
        for (slot, modulator) in modulators
            .iter()
            .filter(|modulator| modulator.enabled && modulator.trigger == "free")
            .enumerate()
        {
            let cycles = if modulator.rate_mode == "tempo" {
                project_time * bpm / 60.0 * f64::from(modulator.rate)
            } else {
                project_time * f64::from(modulator.rate)
            };
            if !self.synth.set_lfo_phase(0, slot as i32 + 6, cycles as f32) {
                return Err("Surge XT rejected the free-running LFO phase".to_owned());
            }
        }
        Ok(())
    }

    pub(crate) fn release_note(&mut self, key: u8, note_id: u64) {
        self.synth
            .release_note(0, key.min(127) as i8, 0, note_id as i32);
    }

    pub(crate) fn set_parameter(&mut self, graph_name: &str, value: f32) -> Result<(), String> {
        let index = self
            .parameters
            .get(graph_name)
            .copied()
            .ok_or_else(|| format!("Surge XT parameter is unavailable: {graph_name}"))?;
        let mut id = SurgeId::empty();
        if !self.synth.from_synth_side_id(index, &mut id) {
            return Err(format!("Surge XT rejected parameter: {graph_name}"));
        }
        if !self
            .synth
            .set_parameter01(&mut id, value.clamp(0.0, 1.0), None, None)
        {
            return Err(format!(
                "Surge XT cannot use this {graph_name} value in the current patch"
            ));
        }
        Ok(())
    }

    pub(crate) fn process(&mut self) -> [[f32; BLOCK_SIZE]; 2] {
        self.synth.process();
        self.synth.pull_buffer()
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

    fn apply_preset(&mut self, preset: &str) -> Result<(), String> {
        if preset == "Init" {
            // SurgeSynthesizer::new already provides Surge XT's native init state.
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
        self.effect_mix_parameters.clear();
        self.effect_parameters.clear();
        let mut assigned = HashMap::new();
        let mut occupied = [false; SERIAL_EFFECT_SLOT_COUNT];
        for effect in effects.iter().filter(|effect| effect.enabled) {
            if let Some(slot) = effect
                .preset_slot
                .filter(|slot| *slot < SERIAL_EFFECT_SLOT_COUNT)
            {
                assigned.insert(effect.id, slot);
                occupied[slot] = true;
            }
        }
        for effect_id in effect_order {
            if !effects
                .iter()
                .any(|effect| effect.id == *effect_id && effect.enabled)
            {
                continue;
            }
            if assigned.contains_key(effect_id) {
                continue;
            }
            let slot = occupied
                .iter()
                .position(|occupied| !occupied)
                .ok_or_else(|| {
                    format!(
                        "Surge XT supports at most {SERIAL_EFFECT_SLOT_COUNT} enabled track effects"
                    )
                })?;
            occupied[slot] = true;
            assigned.insert(*effect_id, slot);
        }
        for (slot_index, slot) in SERIAL_EFFECT_SLOTS.iter().enumerate() {
            if !occupied[slot_index]
                && self
                    .parameter_value(&format!("{slot} FX Type"))
                    .is_some_and(|value| value >= 0.02)
            {
                self.set_parameter(&format!("{slot} FX Type"), 0.0)?;
                self.synth.process();
                self.parameters = parameter_map(&self.synth);
            }
        }
        for effect_id in effect_order {
            let Some(effect) = effects.iter().find(|effect| {
                effect.id == *effect_id && effect.enabled && is_native_effect(&effect.name)
            }) else {
                continue;
            };
            let slot = SERIAL_EFFECT_SLOTS[assigned[effect_id]];
            let type_index = effect_type_index(&effect.name)
                .ok_or_else(|| format!("unsupported Surge XT effect: {}", effect.name))?;
            let desired_type = type_index as f32 / (SURGE_EFFECT_TYPES.len() - 1) as f32;
            if self
                .parameter_value(&format!("{slot} FX Type"))
                .is_none_or(|current| (current - desired_type).abs() > 0.000_01)
            {
                self.set_parameter(&format!("{slot} FX Type"), desired_type)?;
                self.synth.process();
                self.parameters = parameter_map(&self.synth);
            }
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
            if self.parameters.contains_key(&mix_parameter)
                && (effect.preset_slot.is_none()
                    || effect.parameter_overrides.iter().any(|item| item == "mix"))
            {
                self.set_parameter(&mix_parameter, effect.mix)?;
            }
            self.effect_mix_parameters.insert(effect.id, mix_parameter);
            for (parameter, value) in &effect.parameters {
                let native = format!("{slot} {parameter}");
                if self.parameters.contains_key(&native) {
                    self.set_parameter(&native, *value)?;
                    let index = self.parameters[&native];
                    let mut id = SurgeId::empty();
                    if !self.synth.from_synth_side_id(index, &mut id) {
                        return Err(format!(
                            "Surge XT effect parameter is unavailable: {parameter}"
                        ));
                    }
                    if !self.synth.set_parameter_temposync(
                        &id,
                        effect.tempo_sync_parameters.contains(parameter),
                    ) || !self.synth.set_parameter_deactivated(
                        &id,
                        effect.deactivated_parameters.contains(parameter),
                    ) {
                        return Err(format!("Surge XT rejected {parameter} effect state"));
                    }
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

pub(crate) fn instrument_parameters(preset: &str) -> Vec<InstrumentParameter> {
    let cache = INSTRUMENT_PARAMETER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let entry = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(preset.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone();
    let mut cached = entry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(parameters) = cached.as_ref() {
        return parameters.clone();
    }
    let parameters: Vec<InstrumentParameter> = {
        let instrument = Instrument {
            id: 1,
            engine: crate::model::SURGE_ENGINE.to_owned(),
            preset: preset.to_owned(),
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
    };
    if !parameters.is_empty() {
        *cached = Some(parameters.clone());
    }
    parameters
}

pub(crate) fn instrument_parameters_for_instrument(
    instrument: &Instrument,
) -> Vec<InstrumentParameter> {
    let parameters = instrument_parameters(&instrument.preset);
    if instrument.native_overrides.is_empty() {
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
        .and_then(|id| id.parse::<i32>().ok());
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

pub(crate) fn effect_control_semantics(name: &str) -> HashMap<String, EffectParameterSemantics> {
    let cache = EFFECT_CONTROL_SEMANTICS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(semantics) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned()
    {
        return semantics;
    }
    let instrument = Instrument {
        id: 1,
        engine: crate::model::SURGE_ENGINE.to_owned(),
        preset: "Init".to_owned(),
        native_overrides: BTreeMap::new(),
    };
    let effect = Effect {
        id: 1,
        name: name.to_owned(),
        preset_slot: None,
        mix: 0.5,
        enabled: true,
        parameters: BTreeMap::new(),
        parameter_overrides: Vec::new(),
        tempo_sync_parameters: Vec::new(),
        deactivated_parameters: Vec::new(),
    };
    let semantics =
        effect_parameter_semantics(&instrument, std::slice::from_ref(&effect), &[1], 1, 1);
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.to_owned(), semantics.clone());
    semantics
}

pub(crate) fn preset_effects(preset: &str) -> Result<Vec<Effect>, String> {
    let instrument = Instrument {
        id: 1,
        engine: crate::model::SURGE_ENGINE.to_owned(),
        preset: preset.to_owned(),
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
        let (parameters, tempo_sync_parameters, deactivated_parameters) =
            effect_state_for_slot(&engine, slot);
        effects.push(Effect {
            id: 0,
            name: (*name).to_owned(),
            preset_slot: Some(slot_index),
            mix,
            enabled: true,
            parameters,
            parameter_overrides: Vec::new(),
            tempo_sync_parameters,
            deactivated_parameters,
        });
    }
    Ok(effects)
}

fn effect_state_for_slot(
    engine: &Engine,
    slot: &str,
) -> (BTreeMap<String, f32>, Vec<String>, Vec<String>) {
    let mut parameters = BTreeMap::new();
    let mut tempo_sync = Vec::new();
    let mut deactivated = Vec::new();
    for native in engine.parameters.keys() {
        let Some(parameter) = native
            .strip_prefix(&format!("{slot} "))
            .filter(|parameter| {
                *parameter != "FX Type"
                    && *parameter != "Mix"
                    && !is_generic_effect_parameter(parameter)
            })
        else {
            continue;
        };
        let Some(index) = engine.parameters.get(native) else {
            continue;
        };
        let mut id = SurgeId::empty();
        if !engine.synth.from_synth_side_id(*index, &mut id) {
            continue;
        }
        parameters.insert(parameter.to_owned(), engine.synth.get_parameter01(&mut id));
        if engine.synth.parameter_can_temposync(&id) && engine.synth.parameter_is_temposync(&id) {
            tempo_sync.push(parameter.to_owned());
        }
        if engine.synth.parameter_can_deactivate(&id) && engine.synth.parameter_is_deactivated(&id)
        {
            deactivated.push(parameter.to_owned());
        }
    }
    (parameters, tempo_sync, deactivated)
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
        let target = instrument_parameters_for_instrument(&instrument)
            .into_iter()
            .find(|parameter| {
                parameter.scene_modulatable && parameter.name.ends_with("Filter 1 Cutoff")
            })
            .map(|parameter| format!("native:{}", parameter.id))
            .expect("modulatable native parameter");
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
            target,
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

    #[cfg(any())]
    #[test]
    fn factory_patch_parameters_change_only_when_explicitly_overridden() {
        let mut instrument = crate::model::Project::demo().tracks[2].instrument.clone();
        instrument.preset = "Factory/Leads/Violini Solo".to_owned();
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
            enabled: true,
            parameters: BTreeMap::new(),
            parameter_overrides: Vec::new(),
            tempo_sync_parameters: Vec::new(),
            deactivated_parameters: Vec::new(),
        };
        let mut effects = (0..SERIAL_EFFECT_SLOT_COUNT - 1)
            .map(|index| Effect {
                id: 100 + index as u64,
                name: "Distortion".to_owned(),
                preset_slot: None,
                mix: 0.5,
                enabled: true,
                parameters: distortion_parameters.clone(),
                parameter_overrides: Vec::new(),
                tempo_sync_parameters: Vec::new(),
                deactivated_parameters: Vec::new(),
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
    fn graph_effect_orchestration_preserves_native_send_slots() {
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
        assert_ne!(
            engine.parameter_display("FX S1 FX Type").as_deref(),
            Some("Off")
        );
    }

    #[test]
    fn preset_effect_slots_stay_native_while_added_effects_use_open_slots() {
        let instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        let preset = Effect {
            id: 90,
            name: "Reverb 2".to_owned(),
            preset_slot: Some(1),
            mix: 0.2,
            enabled: true,
            parameters: effect_parameter_values("Reverb 2"),
            parameter_overrides: Vec::new(),
            tempo_sync_parameters: Vec::new(),
            deactivated_parameters: Vec::new(),
        };
        let added = Effect {
            id: 91,
            name: "Distortion".to_owned(),
            preset_slot: None,
            mix: 0.8,
            enabled: true,
            parameters: effect_parameter_values("Distortion"),
            parameter_overrides: Vec::new(),
            tempo_sync_parameters: Vec::new(),
            deactivated_parameters: Vec::new(),
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
            Some("Distortion")
        );
        assert_eq!(
            first.parameter_display("FX A2 FX Type").as_deref(),
            Some("Reverb 2")
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
        let mut effects = preset_effects(&instrument.preset).expect("factory preset effects");
        assert!(effects.iter().any(|effect| !effect.parameters.is_empty()));
        for (index, name) in ["Flanger", "EQ", "CHOW", "Delay"].iter().enumerate() {
            effects.push(Effect {
                id: 200 + index as u64,
                name: (*name).to_owned(),
                preset_slot: None,
                mix: 0.5,
                enabled: true,
                parameters: crate::surge::effect_parameter_values(name),
                parameter_overrides: Vec::new(),
                tempo_sync_parameters: Vec::new(),
                deactivated_parameters: Vec::new(),
            });
        }
        for (index, effect) in effects.iter_mut().enumerate() {
            effect.id = 100 + index as u64;
        }
        let order = effects.iter().map(|effect| effect.id).collect::<Vec<_>>();
        let target = instrument_parameters_for_instrument(&instrument)
            .into_iter()
            .find(|parameter| parameter.voice_modulatable)
            .map(|parameter| format!("native:{}", parameter.id))
            .expect("modulatable native parameter");
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
            target,
            enabled: true,
        };
        let mut engine = Engine::new(&instrument, &effects, &order, &[modulator], 1, 16_000.0)
            .expect("mixed preset and added effect chain");
        engine.play_note(41, 0.95, 1);
        for _ in 0..3_500 {
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
        let expected = crate::surge::effect_parameter_values("Distortion");
        let effect = Effect {
            id: 77,
            name: "Distortion".to_owned(),
            preset_slot: None,
            mix: 0.6,
            enabled: true,
            parameters: BTreeMap::new(),
            parameter_overrides: Vec::new(),
            tempo_sync_parameters: Vec::new(),
            deactivated_parameters: Vec::new(),
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
        for (parameter, expected) in &expected {
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
                enabled: true,
                parameters: crate::surge::effect_parameter_values(name),
                parameter_overrides: Vec::new(),
                tempo_sync_parameters: Vec::new(),
                deactivated_parameters: Vec::new(),
            };
            let mut engine = Engine::new(&instrument, &[effect], &[77], &[], 1, 16_000.0)
                .unwrap_or_else(|error| panic!("{name} did not load: {error}"));
            engine.process();
        }
    }

    #[test]
    fn disabled_effects_keep_their_native_parameter_semantics() {
        let instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        let effect = Effect {
            id: 99,
            name: "EQ".to_owned(),
            preset_slot: None,
            mix: 0.5,
            enabled: false,
            parameters: effect_parameter_values("EQ"),
            parameter_overrides: Vec::new(),
            tempo_sync_parameters: Vec::new(),
            deactivated_parameters: Vec::new(),
        };
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
