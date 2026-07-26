pub use surge_sys::*;

unsafe extern "C" {
    pub fn create_engine(sr: f32) -> *mut SurgeSynthesizer;
    pub fn create_patch() -> *mut SurgePatch;
    pub fn destroy_engine(surge: *mut SurgeSynthesizer);
    pub fn destroy_patch(patch: *mut SurgePatch);
    pub fn destroy_parameter(parameter: *mut Parameter);
    pub fn surge_set_tempo(surge: *mut SurgeSynthesizer, bpm: f64);
    pub fn surge_set_modulation(
        surge: *mut SurgeSynthesizer,
        target: i32,
        source: i32,
        source_scene: i32,
        depth: f32,
    ) -> bool;
    pub fn surge_is_valid_modulation(
        surge: *mut SurgeSynthesizer,
        target: i32,
        source: i32,
    ) -> bool;
    pub fn surge_clear_modulation(
        surge: *mut SurgeSynthesizer,
        target: i32,
        source: i32,
        source_scene: i32,
    );
    pub fn surge_configure_lfo(
        surge: *mut SurgeSynthesizer,
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
        formula: *const std::ffi::c_char,
    ) -> bool;
    pub fn surge_set_lfo_rate(
        surge: *mut SurgeSynthesizer,
        scene: i32,
        lfo: i32,
        rate: f32,
        tempo_sync: bool,
    ) -> bool;
    pub fn surge_parameter_is_bipolar(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
    ) -> bool;
    pub fn surge_parameter_is_discrete(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
    ) -> bool;
    pub fn surge_parameter_is_boolean(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
    ) -> bool;
    pub fn surge_parameter_can_temposync(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
    ) -> bool;
    pub fn surge_parameter_is_temposync(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
    ) -> bool;
    pub fn surge_parameter_can_deactivate(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
    ) -> bool;
    pub fn surge_parameter_is_deactivated(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
    ) -> bool;
    pub fn surge_set_parameter_temposync(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
        enabled: bool,
    ) -> bool;
    pub fn surge_set_parameter_deactivated(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
        enabled: bool,
    ) -> bool;
    pub fn surge_parameter_choice_count(surge: *mut SurgeSynthesizer, parameter: i32) -> i32;
    pub fn surge_parameter_choice_value(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
        choice: i32,
    ) -> f32;
    pub fn surge_parameter_choice_display(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
        choice: i32,
        output: *mut std::ffi::c_char,
        output_size: i32,
    );
    pub fn surge_set_parameter01_safe(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
        value: f32,
    ) -> bool;
    pub fn surge_parameter_value_available(
        surge: *mut SurgeSynthesizer,
        parameter: i32,
        value: f32,
    ) -> bool;
}
