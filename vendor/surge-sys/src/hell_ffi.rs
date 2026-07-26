include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
use std::ffi;

unsafe extern "C" {
    pub fn create_engine(sr: f32) -> *mut SurgeSynthesizer;
    pub fn create_patch() -> *mut SurgePatch;
    pub fn destroy_engine(surge: *mut SurgeSynthesizer);
    pub fn destroy_patch(patch: *mut SurgePatch);
    pub fn destroy_parameter(p: *mut Parameter);

    pub fn getNumInputs(surge: *mut SurgeSynthesizer) -> i32;   // TODO: convert to const?
    pub fn getNumOutputs(surge: *mut SurgeSynthesizer) -> i32;  // update: no, but look into it more.
    pub fn getBlockSize(surge: *mut SurgeSynthesizer) -> i32;   // as in, it didn't work.

    pub fn getSynthSideId(id: *const SurgeSynthesizer_ID) -> i32;

    pub fn fromSynthSideId(surge: *const SurgeSynthesizer, i: i32 , q: *mut SurgeSynthesizer_ID) -> bool;
    pub fn idForParameter(
        surge: *const SurgeSynthesizer,
        p: *const Parameter,
        q: *mut SurgeSynthesizer_ID,
    );
    pub fn getParameterDisplay(surge: *const SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, text: *mut ffi::c_char);
    pub fn getParameterDisplayAlt(surge: *const SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, text: *mut ffi::c_char);
    pub fn getParameterName(surge: *const SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, text: *mut ffi::c_char);
    pub fn getParameterNameExtendedByFXGroup(surge: *const SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, text: *mut ffi::c_char);
    pub fn getParameterAccessibleName(surge: *const SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, text: *mut ffi::c_char);
    pub fn getParameterMeta(surge: *const SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, pm: *mut parametermeta);
    pub fn getParameter01(surge: *const SurgeSynthesizer, index: *mut SurgeSynthesizer_ID) -> f32;
    pub fn setParameter01(surge: *mut SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, value: f32, external: bool, force_integer: bool) -> bool;

    pub fn normalizedToValue(surge: *const SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, value: f32) -> f32;
    pub fn valueToNormalized(surge: *const SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, value: f32) -> f32;
    pub fn sendParameterAutomation(surge: *mut SurgeSynthesizer, index: *mut SurgeSynthesizer_ID, value: f32) -> f32;
}
