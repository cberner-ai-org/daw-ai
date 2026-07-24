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
}
