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
    bool surge_is_valid_modulation(SurgeSynthesizer* surge, int target, int source);
    void surge_clear_modulation(SurgeSynthesizer* surge, int target, int source,
                                int source_scene);
    bool surge_configure_lfo(SurgeSynthesizer* surge, int scene, int lfo, int shape,
                             float rate, bool tempo_sync, float delay, float hold, float attack,
                             float decay, float sustain, float release,
                             int trigger_mode, bool unipolar, const char* formula);
    bool surge_set_lfo_rate(SurgeSynthesizer* surge, int scene, int lfo,
                            float rate, bool tempo_sync);
    bool surge_parameter_is_bipolar(SurgeSynthesizer* surge, int parameter);
    bool surge_parameter_is_discrete(SurgeSynthesizer* surge, int parameter);
    bool surge_parameter_is_boolean(SurgeSynthesizer* surge, int parameter);
    bool surge_parameter_can_temposync(SurgeSynthesizer* surge, int parameter);
    bool surge_parameter_can_deactivate(SurgeSynthesizer* surge, int parameter);
    bool surge_parameter_is_deactivated(SurgeSynthesizer* surge, int parameter);
    int surge_parameter_choice_count(SurgeSynthesizer* surge, int parameter);
    float surge_parameter_choice_value(SurgeSynthesizer* surge, int parameter, int choice);
    void surge_parameter_choice_display(SurgeSynthesizer* surge, int parameter, int choice,
                                        char* output, int output_size);
    bool surge_set_parameter01_safe(SurgeSynthesizer* surge, int parameter, float value);
    bool surge_parameter_value_available(SurgeSynthesizer* surge, int parameter, float value);
}
