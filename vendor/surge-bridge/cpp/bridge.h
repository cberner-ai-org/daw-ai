#pragma once

class SurgeSynthesizer;
class SurgePatch;
class Parameter;

extern "C" {
	SurgeSynthesizer* create_engine(float sr);
	SurgePatch* create_patch();
    void destroy_engine(SurgeSynthesizer* surge);
    void destroy_patch(SurgePatch* patch);
    void destroy_parameter(Parameter* p);
    void surge_set_tempo(SurgeSynthesizer* surge, double bpm);
    bool surge_set_modulation(SurgeSynthesizer* surge, int target, int source,
                              int source_scene, float depth);
    void surge_clear_modulation(SurgeSynthesizer* surge, int target, int source,
                                int source_scene);
    bool surge_configure_lfo(SurgeSynthesizer* surge, int scene, int lfo, int shape,
                             float rate, bool tempo_sync, float delay, float hold, float attack,
                             float decay, float sustain, float release,
                             int trigger_mode, bool unipolar, const char* formula);
    bool surge_set_lfo_rate(SurgeSynthesizer* surge, int scene, int lfo,
                            float rate, bool tempo_sync);
}
