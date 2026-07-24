#include "bridge.h"
#include "src/common/SurgeSynthesizer.h"

#include <algorithm>
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

    bool surge_set_modulation(SurgeSynthesizer* surge, int target, int source,
                              int source_scene, float depth) {
        if (!surge || source <= ms_original || source >= n_modsources ||
            !surge->isValidModulation(target, static_cast<modsources>(source))) {
            return false;
        }
        return surge->setModDepth01(target, static_cast<modsources>(source),
                                    source_scene, 0, depth);
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
}
