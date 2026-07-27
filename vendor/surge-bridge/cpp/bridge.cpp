#include "bridge.h"
#include "src/common/SurgeSynthesizer.h"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>

class ErrCork : public SurgeSynthesizer::PluginLayer {
  public:
    void surgeParameterUpdated(const SurgeSynthesizer::ID &id, float d) override {}
    void surgeMacroUpdated(long macroNum, float d) override {}
};

extern "C" {
    SurgeSynthesizer* create_engine(float sr) {
        static ErrCork layer;
        auto* surge = new SurgeSynthesizer(
            &layer, SurgeStorage::skipPatchLoadDataPathSentinel);

        surge->setSamplerate(sr);
        surge->time_data.tempo = 120;
        surge->time_data.ppqPos = 0;
        surge->storage.rngGen.g.seed(0);
        std::srand(0);

        return surge;
	}

    bool surge_load_builtin_wavetables(SurgeSynthesizer* surge,
                                       const char* windows, std::size_t windows_size,
                                       const char* initial, std::size_t initial_size) {
        if (!surge || !windows || !initial ||
            !surge->storage.load_wt_wt_mem(windows, windows_size, &surge->storage.WindowWT)) {
            return false;
        }
        for (auto& scene : surge->storage.getPatch().scene) {
            for (auto& oscillator : scene.osc) {
                if (!surge->storage.load_wt_wt_mem(initial, initial_size, &oscillator.wt)) {
                    return false;
                }
            }
        }
        return true;
    }

    SurgePatch* create_patch() {
        SurgeStorage::SurgeStorageConfig sconf;
        sconf.scanWavetableAndPatches = false;
        sconf.createUserDirectory = false;

        auto* storage = new SurgeStorage(sconf);
        return new SurgePatch(storage);
    }

    void destroy_engine(SurgeSynthesizer* surge) {
        if (surge) delete surge;
    }

    void destroy_patch(SurgePatch* patch) {
        if (patch) delete patch;
    }

    void destroy_parameter(Parameter* parameter) {
        if (parameter) delete parameter;
    }

    void surge_set_tempo(SurgeSynthesizer* surge, double bpm) {
        if (surge) surge->time_data.tempo = std::clamp(bpm, 1.0, 999.0);
    }

    bool surge_parameter_value_available(SurgeSynthesizer* surge, int parameter, float value) {
        if (!surge || parameter < 0 ||
            parameter >= static_cast<int>(surge->storage.getPatch().param_ptr.size())) {
            return false;
        }
        auto* target = surge->storage.getPatch().param_ptr[parameter];
        if (target->ctrltype == ct_osctype) {
            const int choice = target->val_min.i + static_cast<int>(std::lround(
                std::clamp(value, 0.0f, 1.0f) * (target->val_max.i - target->val_min.i)));
            const int scene = target->scene - 1;
            const int oscillator = target->ctrlgroup_entry;
            if (scene >= 0 && scene < n_scenes && oscillator >= 0 && oscillator < n_oscs &&
                uses_wavetabledata(choice) &&
                !surge->storage.getPatch().scene[scene].osc[oscillator].wt.everBuilt) {
                return false;
            }
        }
        return true;
    }

    bool surge_set_parameter01_safe(SurgeSynthesizer* surge, int parameter, float value) {
        if (!surge_parameter_value_available(surge, parameter, value)) {
            return false;
        }
        SurgeSynthesizer::ID id;
        if (!surge->fromSynthSideId(parameter, id)) {
            return false;
        }
        surge->setParameter01(id, std::clamp(value, 0.0f, 1.0f));
        return true;
    }

    bool surge_set_modulation(SurgeSynthesizer* surge, int target, int source,
                              int source_scene, float depth) {
        if (!surge || source <= ms_original || source >= n_modsources ||
            !surge->isValidModulation(target, static_cast<modsources>(source))) {
            return false;
        }
        return surge->setModDepth01(target, static_cast<modsources>(source),
                                    source_scene, 0, depth);
    }

    bool surge_is_valid_modulation(SurgeSynthesizer* surge, int target, int source) {
        return surge && source > ms_original && source < n_modsources &&
               surge->isValidModulation(target, static_cast<modsources>(source));
    }

    void surge_clear_modulation(SurgeSynthesizer* surge, int target, int source,
                                int source_scene) {
        if (surge && source > ms_original && source < n_modsources) {
            surge->clearModulation(target, static_cast<modsources>(source),
                                   source_scene, 0, true);
        }
    }

    bool surge_configure_lfo(SurgeSynthesizer* surge, int scene, int lfo, int shape,
                             float rate, bool tempo_sync, float delay, float hold, float attack,
                             float decay, float sustain, float release,
                             int trigger_mode, bool unipolar, const char* formula) {
        if (!surge || scene < 0 || scene >= n_scenes || lfo < 0 || lfo >= n_lfos ||
            shape < lt_sine || shape >= n_lfo_types) {
            return false;
        }
        auto &patch = surge->storage.getPatch();
        auto &storage = patch.scene[scene].lfo[lfo];
        storage.shape.val.i = shape;
        storage.rate.set_value_f01(std::clamp(rate, 0.0f, 1.0f));
        storage.rate.temposync = tempo_sync;
        storage.delay.set_value_f01(std::clamp(delay, 0.0f, 1.0f));
        storage.hold.set_value_f01(std::clamp(hold, 0.0f, 1.0f));
        storage.attack.set_value_f01(std::clamp(attack, 0.0f, 1.0f));
        storage.decay.set_value_f01(std::clamp(decay, 0.0f, 1.0f));
        storage.sustain.set_value_f01(std::clamp(sustain, 0.0f, 1.0f));
        storage.release.set_value_f01(std::clamp(release, 0.0f, 1.0f));
        storage.trigmode.set_value_f01(storage.trigmode.value_to_normalized(trigger_mode), true);
        storage.unipolar.set_value_f01(unipolar ? 1.0f : 0.0f, true);
        if (shape == lt_formula) {
            patch.formulamods[scene][lfo].setFormula(formula ? formula : "");
        }
        return true;
    }

    bool surge_set_lfo_rate(SurgeSynthesizer* surge, int scene, int lfo,
                            float rate, bool tempo_sync) {
        if (!surge || scene < 0 || scene >= n_scenes || lfo < 0 || lfo >= n_lfos) {
            return false;
        }
        auto &storage = surge->storage.getPatch().scene[scene].lfo[lfo];
        storage.rate.set_value_f01(std::clamp(rate, 0.0f, 1.0f));
        storage.rate.temposync = tempo_sync;
        return true;
    }

    bool surge_set_lfo_phase(SurgeSynthesizer* surge, int scene, int lfo, float phase) {
        if (!surge || scene < 0 || scene >= n_scenes || lfo < 0 || lfo >= n_lfos) {
            return false;
        }
        surge->storage.getPatch().scene[scene].lfo[lfo].start_phase.set_value_f01(
            std::clamp(phase, 0.0f, 1.0f));
        return true;
    }

    Parameter* surge_parameter(SurgeSynthesizer* surge, int parameter) {
        if (!surge || parameter < 0 ||
            parameter >= static_cast<int>(surge->storage.getPatch().param_ptr.size())) {
            return nullptr;
        }
        return surge->storage.getPatch().param_ptr[parameter];
    }

    bool surge_parameter_is_bipolar(SurgeSynthesizer* surge, int parameter) {
        auto* value = surge_parameter(surge, parameter);
        return value && value->is_bipolar();
    }

    bool surge_parameter_is_discrete(SurgeSynthesizer* surge, int parameter) {
        auto* value = surge_parameter(surge, parameter);
        return value && value->is_discrete_selection();
    }

    bool surge_parameter_is_boolean(SurgeSynthesizer* surge, int parameter) {
        if (!surge) return false;
        SurgeSynthesizer::ID id;
        if (!surge->fromSynthSideId(parameter, id)) return false;
        return surge->getParameterIsBoolean(id);
    }

    bool surge_parameter_can_temposync(SurgeSynthesizer* surge, int parameter) {
        auto* value = surge_parameter(surge, parameter);
        return value && value->can_temposync();
    }

    bool surge_parameter_is_temposync(SurgeSynthesizer* surge, int parameter) {
        auto* value = surge_parameter(surge, parameter);
        return value && value->temposync;
    }

    bool surge_parameter_can_deactivate(SurgeSynthesizer* surge, int parameter) {
        auto* value = surge_parameter(surge, parameter);
        return value && value->can_deactivate();
    }

    bool surge_parameter_is_deactivated(SurgeSynthesizer* surge, int parameter) {
        auto* value = surge_parameter(surge, parameter);
        return value && value->appears_deactivated();
    }

    bool surge_set_parameter_temposync(SurgeSynthesizer* surge, int parameter, bool enabled) {
        auto* value = surge_parameter(surge, parameter);
        if (!value || (enabled && !value->can_temposync())) return false;
        value->temposync = enabled;
        return true;
    }

    bool surge_set_parameter_deactivated(SurgeSynthesizer* surge, int parameter, bool enabled) {
        auto* value = surge_parameter(surge, parameter);
        if (!value || (enabled && !value->can_deactivate())) return false;
        value->deactivated = enabled;
        return true;
    }

    int surge_parameter_choice_count(SurgeSynthesizer* surge, int parameter) {
        auto* value = surge_parameter(surge, parameter);
        if (!value) return 0;
        if (value->valtype == vt_bool) return 2;
        if (value->valtype != vt_int) return 0;
        return std::max(0, value->val_max.i - value->val_min.i + 1);
    }

    float surge_parameter_choice_value(SurgeSynthesizer* surge, int parameter, int choice) {
        auto* value = surge_parameter(surge, parameter);
        auto count = surge_parameter_choice_count(surge, parameter);
        if (!value || choice < 0 || choice >= count) return 0.f;
        if (value->valtype == vt_bool) return choice == 0 ? 0.f : 1.f;
        return value->value_to_normalized(static_cast<float>(value->val_min.i + choice));
    }

    void surge_parameter_choice_display(SurgeSynthesizer* surge, int parameter, int choice,
                                        char* output, int output_size) {
        if (!output || output_size <= 0) return;
        auto* value = surge_parameter(surge, parameter);
        auto normalized = surge_parameter_choice_value(surge, parameter, choice);
        auto display = value ? value->get_display(true, normalized) : std::string{};
        std::snprintf(output, static_cast<size_t>(output_size), "%s", display.c_str());
    }
}
