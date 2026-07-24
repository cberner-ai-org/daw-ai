use super::hell_ffi;
use super::parameter::Parameter;

use std::ffi;
use std::ffi::CString;
use std::path::Path;

// should this take &self and &mut index instead? for clarity up ahead.
// update: yes. yes it should.
macro_rules! stringer {
    (&$self:ident, &mut $index:ident, $function:ident) => {{
        let mut buffer = [0i8; 256];
        unsafe {
            hell_ffi::$function($self.ptr, &mut $index.0, buffer.as_mut_ptr());
            ffi::CStr::from_ptr(buffer.as_ptr())
                .to_string_lossy()
                .into_owned()
        }
    }}
}

#[repr(transparent)]
pub struct SurgeId(hell_ffi::SurgeSynthesizer_ID);

impl SurgeId {
    pub fn empty() -> Self {
        unsafe { SurgeId(std::mem::zeroed()) }
    }

    pub fn get_synth_side_id(&self) -> i32 {
        unsafe { hell_ffi::getSynthSideId(&self.0) }
    }
}

pub struct SurgeSynthesizer {
    ptr: *mut hell_ffi::SurgeSynthesizer,
}

impl SurgeSynthesizer {
    pub fn new(sample_rate: f32) -> Self {
        unsafe {
            let ptr = hell_ffi::create_engine(sample_rate);
            assert!(!ptr.is_null(), "a surge burnt the bridge down (failed to create the engine).");
            Self { ptr }
        }
    }

    pub fn pull_buffer(&self) -> [[f32; 32]; 2] {
        unsafe { (*self.ptr).output }
    }

    pub fn set_input_buffer(&mut self, input: [[f32; 32]; 2]) {
        unsafe {
            (*self.ptr).input = input;
            (*self.ptr).process_input = true;
        }
    }

    pub fn load_patch_by_path(
        &mut self,
        fxp_path: &Path,
        category_id: i32,
        name: &str,
        force_is_preset: bool
    ) { unsafe {
        let cpath = ffi::CString::new(fxp_path.as_os_str().to_str().unwrap()).unwrap();
        let cname = ffi::CString::new(name).unwrap();

        (*self.ptr).loadPatchByPath(cpath.as_ptr(), category_id, cname.as_ptr(), force_is_preset);
    }}

    // TODO: look into the code and evaluate which functions can take &self.
    pub fn play_note(
        &mut self,
        channel: i8,
        key: i8,
        velocity: i8,
        detune: i8,
        host_noteid: i32,
        force_scene: i32,
    ) { unsafe {
        (*self.ptr).playNote(
            channel,
            key,
            velocity,
            detune,
            host_noteid,
            force_scene,
        );
    }}

    pub fn play_note_by_frequency(&mut self, frequency: f32, velocity: i8, id: i32) {
        unsafe { (*self.ptr).playNoteByFrequency(frequency, velocity, id); }
    }

    pub fn release_note(
        &mut self,
        channel: i8,
        key: i8,
        velocity: i8,
        host_noteid: i32,
    ) {
        unsafe { (*self.ptr).releaseNote(channel, key, velocity, host_noteid); }
    }

    pub fn choke_note(
        &mut self,
        channel: i16,
        key: i16,
        velocity: i8,
        host_noteid: i32,
    ) {
        unsafe { (*self.ptr).chokeNote(channel, key, velocity, host_noteid); }
    }

    pub fn release_note_by_host_note_id(&mut self, host_noteid: i32, velocity: i8) {
        unsafe { (*self.ptr).releaseNoteByHostNoteID(host_noteid, velocity); }
    }

    pub fn release_note_post_hold_check(
        &mut self,
        scene: i32,
        channel: i8,
        key: i8,
        velocity: i8,
        host_noteid: i32,
    ) {
        unsafe { (*self.ptr).releaseNotePostHoldCheck(scene, channel, key, velocity, host_noteid); }
    }

    pub fn reset_pitch_bend(&mut self, channel: i8) {
        unsafe { (*self.ptr).resetPitchBend(channel); }
    }

    pub fn pitch_bend(&mut self, channel: i8, value: i32) {
        unsafe { (*self.ptr).pitchBend(channel, value); }
    }

    pub fn poly_aftertouch(&mut self, channel: i8, key: i32, value: i32) {
        unsafe { (*self.ptr).polyAftertouch(channel, key, value); }
    }

    pub fn channel_aftertouch(&mut self, channel: i8, value: i32) {
        unsafe { (*self.ptr).channelAftertouch(channel, value); }
    }

    pub fn channel_controller(&mut self, channel: i8, cc: i32, value: i32) {
        unsafe { (*self.ptr).channelController(channel, cc, value); }
    }

    pub fn program_change(&mut self, channel: i8, value: i32) {
        unsafe { (*self.ptr).programChange(channel, value); }
    }

    pub fn all_notes_off(&mut self) {
        unsafe { (*self.ptr).allNotesOff(); }
    }

    pub fn all_sounds_off(&mut self) {
        unsafe { (*self.ptr).allSoundOff(); }
    }

    pub fn set_samplerate(&mut self, sample_rate: f32) {
        unsafe { (*self.ptr).setSamplerate(sample_rate); }
    }

    pub fn update_high_low_keys(&mut self, scene: i32) {
        unsafe { (*self.ptr).updateHighLowKeys(scene); }
    }

    pub fn get_num_inputs(&mut self,) -> i32 {
        unsafe { hell_ffi::getNumInputs(self.ptr) }
    }

    pub fn get_num_outputs(&mut self) -> i32 {
        unsafe { hell_ffi::getNumOutputs(self.ptr) }
    }

    pub fn get_block_size(&mut self) -> i32 {
        unsafe { hell_ffi::getBlockSize(self.ptr) }
    }

    pub fn get_mpe_main_channel(&mut self, voice_channel: i32, key: i32) -> i32 {
        unsafe { (*self.ptr).getMpeMainChannel(voice_channel, key) }
    }

    pub fn process(&mut self) {
        unsafe { (*self.ptr).process(); }
    }

    pub fn from_synth_side_id(&self, i: i32, q: &mut SurgeId) -> bool {
        unsafe { hell_ffi::fromSynthSideId(self.ptr, i, &mut q.0) }
    }

    pub fn id_for_parameter(&self, parameter: &Parameter) -> SurgeId {
        unsafe { SurgeId(hell_ffi::idForParameter(self.ptr, parameter.ptr)) }
    }

    pub fn get_parameter_display(&self, index: &mut SurgeId) -> String {
        stringer!(&self, &mut index, getParameterDisplay)
    }

    pub fn get_parameter_display_alt(&self, index: &mut SurgeId) -> String {
        stringer!(&self, &mut index, getParameterDisplayAlt)
    }

    pub fn get_parameter_name(&self, index: &mut SurgeId) -> String {
        stringer!(&self, &mut index, getParameterName)
    }

    pub fn get_parameter_name_extended_by_fx_group(&self, index: &mut SurgeId) -> String {
        stringer!(&self, &mut index, getParameterNameExtendedByFXGroup)
    }

    pub fn get_parameter_accessible_name(&self, index: &mut SurgeId) -> String {
        stringer!(&self, &mut index, getParameterAccessibleName)
    }

    /*pub fn get_parameter_meta(&self, index: &mut SurgeId) -> ParameterMeta {
        let mut buffer = ParameterMeta::default();

        unsafe {
            hell_ffi::getParameterMeta(self.ptr, &mut index.0,
                &mut buffer
                as *mut ParameterMeta
                as *mut _
            );
            buffer
        }



        //todo!();
        // TODO: do what you did for the stringer set and return a PMETA.
        //unsafe { hell_ffi::getParameterMeta(self.ptr, &mut index.0, parametermeta) }
    }*/
    pub fn get_parameter_meta(&self, _index: &mut SurgeId) {
        todo!();
    }

    pub fn get_parameter01(&self, index: &mut SurgeId) -> f32 {
        unsafe { hell_ffi::getParameter01(self.ptr, &mut index.0) }
    }

    pub fn set_parameter01(
        &mut self,
        index: &mut SurgeId,
        value: f32,
        external: Option<bool>,
        force_integer: Option<bool>,
    ) -> bool {
        let external = external.unwrap_or(false);
        let force_integer = force_integer.unwrap_or(false);

        unsafe { hell_ffi::setParameter01(self.ptr, &mut index.0, value, external, force_integer) }
    }

    pub fn set_modulation(
        &mut self,
        target: i32,
        source: i32,
        source_scene: i32,
        depth: f32,
    ) -> bool {
        unsafe {
            surge_bridge::surge_set_modulation(
                self.ptr,
                target,
                source,
                source_scene,
                depth.clamp(-1.0, 1.0),
            )
        }
    }

    pub fn clear_modulation(&mut self, target: i32, source: i32, source_scene: i32) {
        unsafe {
            surge_bridge::surge_clear_modulation(self.ptr, target, source, source_scene);
        }
    }

    pub fn set_tempo(&mut self, bpm: f64) {
        unsafe { surge_bridge::surge_set_tempo(self.ptr, bpm) }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn configure_lfo(
        &mut self,
        scene: i32,
        lfo: i32,
        shape: i32,
        rate: f32,
        tempo_sync: bool,
        delay: f32,
        hold: f32,
        attack: f32,
        decay: f32,
        sustain: f32,
        release: f32,
        trigger_mode: i32,
        unipolar: bool,
        formula: &str,
    ) -> bool {
        let Ok(formula) = CString::new(formula) else {
            return false;
        };
        unsafe {
            surge_bridge::surge_configure_lfo(
                self.ptr,
                scene,
                lfo,
                shape,
                rate,
                tempo_sync,
                delay,
                hold,
                attack,
                decay,
                sustain,
                release,
                trigger_mode,
                unipolar,
                formula.as_ptr(),
            )
        }
    }

    pub fn set_lfo_rate(
        &mut self,
        scene: i32,
        lfo: i32,
        rate: f32,
        tempo_sync: bool,
    ) -> bool {
        unsafe {
            surge_bridge::surge_set_lfo_rate(
                self.ptr,
                scene,
                lfo,
                rate.clamp(0.0, 1.0),
                tempo_sync,
            )
        }
    }

    pub fn apply_parameter_monophonic_modulation(&mut self, arg1: &Parameter, depth: f32) {
        unsafe { (*self.ptr).applyParameterMonophonicModulation(arg1.ptr, depth); }
    }

    pub fn apply_parameter_polyphonic_modulation(
        &mut self,
        arg1: &Parameter,
        note_id: i32,
        key: i16,
        channel: i16,
        depth: f32,
    ) {
        unsafe { (*self.ptr).applyParameterPolyphonicModulation(arg1.ptr, note_id, key, channel, depth); }
    }

    // i64 is crazy.
    // TODO: read next TODO.
    pub fn get_macro_parameter01(&mut self, macro_number: i64) -> f32 {
        unsafe { (*self.ptr).getMacroParameter01(macro_number) }
    }

    // TODO: read next TODO.
    pub fn get_macro_parameter_target01(&mut self, macro_number: i64) -> f32 {
        unsafe { (*self.ptr).getMacroParameterTarget01(macro_number) }
    }

    // TODO: read next TODO.
    pub fn set_macro_parameter01(&mut self, macro_number: i64, value: f32) {
        unsafe { (*self.ptr).setMacroParameter01(macro_number, value); }
    }

    // TODO: rename macro_number to just number? or id?
    pub fn apply_macro_monophonic_modulation(&mut self, macro_number: i64, value: f32) {
        unsafe { (*self.ptr).applyMacroMonophonicModulation(macro_number, value); }
    }

    pub fn load_raw(&mut self, data: &mut [u8], preset: Option<bool>) {
        let size = data.len() as i32;   // TODON'T: use try_from in case patches exceed 2 gigabytes.
        let data = data.as_ptr() as *const std::ffi::c_void;
        let preset = preset.unwrap_or(false);

        unsafe { (*self.ptr).loadRaw(data, size, preset); }
    }
}

impl Drop for SurgeSynthesizer {
    fn drop(&mut self) {
        unsafe {
            hell_ffi::destroy_engine(self.ptr);
        }
    }
}

// TODO: check if this is a mistake to do.
unsafe impl Send for SurgeSynthesizer {}

/*
 * legend:
 * N new surge-rs function.
 * O implemented.
 * X not implemented yet.
 * ? tried to implement.
 * R remade.
 * E implemented; overloading expanded.
 * B implemented; bridge pass.
 *
 * list of stuff:
 * N new
 * N pull_buffer
 * O playNote
 * O playNoteByFrequency
 * O releaseNote
 * O chokeNote
 * O releaseNoteByHostNoteID
 * O releaseNotePostHoldCheck
 * O resetPitchBend
 * O pitchBend
 * O polyAftertouch
 * O channelAftertouch
 * O channelController
 * O programChange
 * O allNotesOff
 * O allSoundsOff
 * O setSamplerate
 * O updateHighLowKeys
 * B getNumInputs
 * B getNumOutputs
 * B getBlockSize
 * O getMpeMainChannel
 * X process
 *
 * B ID.getSynthSideId
 * X ID.toString
 *
 * B fromSynthSideId
 * B idForParameter
 * B getParameterDisplay
 * B getParameterDisplayAlt
 * B getParameterName
 * B getParameterNameExtendedByFxGroup
 * B getParameterAccessibleName
 * B getParameterMeta
 * B getParameter01
 * B setParameter01
 * O applyParameterMonophonicModulation
 * O applyParameterPolyphonicModulation
 * O setMacroParameter01                    order backwards...
 * O getMacroParameter01
 * O getMacroParameterTarget01
 * O applyMacroMonophonicModulation
 *
 * X setNoteExpression
 * X getParameterIsBoolean
 * X stringToNormalizedValue
 * X normalizedToValue
 * X valueToNormalized
 * X sendParameterAutomation
 *
 * X updateDisplay
 * X isValidModulation
 * X isActiveModulation
 * X isAnyActiveModulation
 * X isBipolarModulation
 * X isModsourceUsed                        inconsistent capitalization...
 * X isModDestUsed
 * X IsModulatorDistinctPerScene
 *
 * X setModDepth01
 * X getModDepth01
 * X muteModulation
 * X isModulationMuted
 * X clearModulation
 * X clear_osc_modulation                   inconsistent style...
 */

/* consider turning this:
 *  channel
 *  key
 *  velocity
 *  host_noteid
 * into this:
 *  note
 * okay?
 *
 * also consider pulling back all the unsafes into the function definition.
 *
 * remember that the only difference between &self and &mut self is presentation.
 * once you pull the pointer from underneath, it doesn't matter.
 */
