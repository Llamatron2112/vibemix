// Minimal VST3 effect (processor + controller) used by vibemix-vst3's tests.

#include <algorithm>
#include <cmath>

#include "public.sdk/source/main/pluginfactory.h"
#include "public.sdk/source/vst/vstaudioeffect.h"
#include "public.sdk/source/vst/vsteditcontroller.h"
#include "public.sdk/source/vst/vstparameters.h"

#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace Spike {

static const FUID kProcessorUID(0x9f1c2b30, 0x4d5e6a70, 0x8b9c0d1e, 0x2f3a4b5c);
static const FUID kControllerUID(0xa1b2c3d4, 0xe5f60718, 0x293a4b5c, 0x6d7e8f90);
static const ParamID kGainId = 0;

class SpikeProcessor : public AudioEffect {
public:
    SpikeProcessor() { setControllerClass(kControllerUID); }

    static FUnknown* createInstance(void*) {
        return static_cast<IAudioProcessor*>(new SpikeProcessor());
    }

    tresult PLUGIN_API initialize(FUnknown* context) SMTG_OVERRIDE {
        tresult result = AudioEffect::initialize(context);
        if (result != kResultOk) return result;
        addAudioInput(STR16("In"), SpeakerArr::kStereo);
        addAudioOutput(STR16("Out"), SpeakerArr::kStereo);
        return kResultOk;
    }

    tresult PLUGIN_API process(ProcessData& data) SMTG_OVERRIDE {
        if (data.numInputs == 0 || data.numOutputs == 0) return kResultOk;
        if (data.inputParameterChanges) {
            const int32 count = data.inputParameterChanges->getParameterCount();
            for (int32 i = 0; i < count; ++i) {
                IParamValueQueue* queue = data.inputParameterChanges->getParameterData(i);
                if (queue && queue->getParameterId() == kGainId && queue->getPointCount() > 0) {
                    ParamValue value = 0.0;
                    int32 sampleOffset = 0;
                    queue->getPoint(0, sampleOffset, value);
                    gain_ = static_cast<float>(value);
                }
            }
        }
        const int32 samples = data.numSamples;
        const int32 channels = std::min(data.inputs[0].numChannels, data.outputs[0].numChannels);
        for (int32 c = 0; c < channels; ++c) {
            const float* input = data.inputs[0].channelBuffers32[c];
            float* output = data.outputs[0].channelBuffers32[c];
            if (!input || !output) continue;
            for (int32 s = 0; s < samples; ++s) output[s] = input[s] * gain_;
        }
        return kResultOk;
    }

    tresult PLUGIN_API getState(IBStream* state) SMTG_OVERRIDE {
        if (!state) return kResultFalse;
        return state->write(&gain_, sizeof(gain_), nullptr);
    }

    tresult PLUGIN_API setState(IBStream* state) SMTG_OVERRIDE {
        if (!state) return kResultFalse;
        float value = gain_;
        state->read(&value, sizeof(value), nullptr);
        gain_ = value;
        return kResultOk;
    }

private:
    float gain_ = 0.5f;
};

class SpikeController : public EditController {
public:
    SpikeController() {
        parameters.addParameter(
            new RangeParameter(STR16("Gain"), kGainId, STR16("Gain"), 0.0, 1.0, 0.5));
    }

    static FUnknown* createInstance(void*) {
        return static_cast<IEditController*>(new SpikeController());
    }

    tresult PLUGIN_API setComponentState(IBStream* state) SMTG_OVERRIDE {
        if (!state) return kResultFalse;
        float value = 0.5f;
        state->read(&value, sizeof(value), nullptr);
        setParamNormalized(kGainId, value);
        return kResultOk;
    }
};

} // namespace Spike

BEGIN_FACTORY_DEF("Vibemix", "https://example.com", "mailto:dev@example.com")

DEF_CLASS2(INLINE_UID_FROM_FUID(Spike::kProcessorUID), PClassInfo::kManyInstances,
           kVstAudioEffectClass, "Vibemix Spike Gain", Vst::kDistributable, Vst::PlugType::kFx,
           "1.0.0", kVstVersionString, Spike::SpikeProcessor::createInstance)

DEF_CLASS2(INLINE_UID_FROM_FUID(Spike::kControllerUID), PClassInfo::kManyInstances,
           kVstComponentControllerClass, "Vibemix Spike Gain", 0u, "", "1.0.0",
           kVstVersionString, Spike::SpikeController::createInstance)

END_FACTORY
