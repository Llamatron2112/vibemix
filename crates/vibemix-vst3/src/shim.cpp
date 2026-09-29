// C ABI around the VST3 SDK hosting helpers. The Rust side owns plugin
// instances through opaque `Hosted*` handles.

#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <memory>
#include <string>
#include <vector>

#include "public.sdk/source/common/memorystream.h"
#include "public.sdk/source/vst/hosting/eventlist.h"
#include "public.sdk/source/vst/hosting/hostclasses.h"
#include "public.sdk/source/vst/hosting/module.h"
#include "public.sdk/source/vst/hosting/parameterchanges.h"
#include "public.sdk/source/vst/hosting/plugprovider.h"

#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivstprocesscontext.h"

using namespace Steinberg;
using namespace Steinberg::Vst;
namespace Hosting = VST3::Hosting;
namespace fs = std::filesystem;

namespace {

std::string json_escape(const std::string& in) {
    std::string out;
    out.reserve(in.size() + 8);
    for (char c : in) {
        switch (c) {
        case '"': out += "\\\""; break;
        case '\\': out += "\\\\"; break;
        case '\n': out += "\\n"; break;
        case '\r': out += "\\r"; break;
        case '\t': out += "\\t"; break;
        default:
            if (static_cast<unsigned char>(c) < 0x20) {
                char buf[8];
                std::snprintf(buf, sizeof(buf), "\\u%04x", c);
                out += buf;
            } else {
                out += c;
            }
        }
    }
    return out;
}

std::string hex_uid(const VST3::UID& uid) {
    static const char* digits = "0123456789abcdef";
    std::string out;
    const char* data = uid.data();
    out.reserve(32);
    for (int i = 0; i < 16; ++i) {
        const unsigned byte = static_cast<unsigned char>(data[i]);
        out.push_back(digits[byte >> 4]);
        out.push_back(digits[byte & 0xf]);
    }
    return out;
}

bool parse_uid(const char* hex, VST3::UID& uid) {
    if (!hex || std::strlen(hex) != 32) return false;
    Steinberg::TUID bytes {};
    for (int i = 0; i < 16; ++i) {
        auto nibble = [](char c) -> int {
            if (c >= '0' && c <= '9') return c - '0';
            if (c >= 'a' && c <= 'f') return c - 'a' + 10;
            if (c >= 'A' && c <= 'F') return c - 'A' + 10;
            return -1;
        };
        int hi = nibble(hex[i * 2]);
        int lo = nibble(hex[i * 2 + 1]);
        if (hi < 0 || lo < 0) return false;
        bytes[i] = static_cast<char>((hi << 4) | lo);
    }
    uid = VST3::UID::fromTUID(bytes);
    return true;
}

struct Hosted {
    Hosted(Hosting::Module::Ptr m, Hosting::PluginFactory f)
        : module(std::move(m)), factory(std::move(f)) {}

    Hosting::Module::Ptr module;
    Hosting::PluginFactory factory;
    HostApplication host_app;
    std::unique_ptr<PlugProvider> provider;
    IPtr<IComponent> component;
    IPtr<IEditController> controller;
    IPtr<IAudioProcessor> processor;
    int32 in_channels = 0;
    int32 out_channels = 0;
    std::vector<float*> in_ptrs;
    std::vector<float*> out_ptrs;
    ParameterChanges changes;
    ParameterChanges out_changes;
    EventList events;
    ProcessContext context {};
    bool active = false;
};

} // namespace

extern "C" {

void vst3_string_free(char* text) {
    std::free(text);
}

/// Enumerates `*.vst3` bundles in `dir` (non-recursive) and returns a JSON
/// array string (caller frees with `vst3_string_free`).
char* vst3_scan(const char* dir) {
    std::string json = "[";
    bool first = true;
    std::error_code ec;
    if (dir) {
        for (const auto& entry : fs::directory_iterator(dir, ec)) {
            const auto& path = entry.path();
            if (path.extension() != ".vst3") continue;
            std::string error;
            auto module = Hosting::Module::create(path.string(), error);
            if (!module) continue;
            auto factory = module->getFactory();
            for (const auto& ci : factory.classInfos()) {
                if (ci.category() != kVstAudioEffectClass) continue;
                if (!first) json += ",";
                first = false;
                json += "{\"class_id\":\"" + hex_uid(ci.ID()) + "\"";
                json += ",\"name\":\"" + json_escape(ci.name()) + "\"";
                json += ",\"vendor\":\"" + json_escape(ci.vendor()) + "\"";
                json += ",\"bundle\":\"" + json_escape(path.string()) + "\"";
                json += "}";
            }
        }
    }
    json += "]";
    char* out = static_cast<char*>(std::malloc(json.size() + 1));
    if (!out) return nullptr;
    std::memcpy(out, json.c_str(), json.size() + 1);
    return out;
}

void* vst3_open(const char* bundle, const char* class_id, double sample_rate, int block_size,
                int* in_channels, int* out_channels) {
    if (!bundle) return nullptr;
    std::string error;
    auto module = Hosting::Module::create(bundle, error);
    if (!module) return nullptr;
    auto factory = module->getFactory();

    bool have = false;
    Hosting::ClassInfo chosen;
    VST3::UID wanted;
    bool want_id = parse_uid(class_id, wanted);
    for (const auto& ci : factory.classInfos()) {
        if (ci.category() != kVstAudioEffectClass) continue;
        if (want_id) {
            if (ci.ID() == wanted) {
                chosen = ci;
                have = true;
                break;
            }
        } else {
            chosen = ci;
            have = true;
            break;
        }
    }
    if (!have) return nullptr;

    auto* hosted = new Hosted(module, factory);
    PluginContextFactory::instance().setPluginContext(&hosted->host_app);
    hosted->provider = std::make_unique<PlugProvider>(factory, chosen);
    if (!hosted->provider->initialize()) {
        delete hosted;
        return nullptr;
    }
    hosted->component = hosted->provider->getComponentPtr();
    hosted->controller = hosted->provider->getControllerPtr();
    if (!hosted->component) {
        delete hosted;
        return nullptr;
    }
    hosted->component->queryInterface(IAudioProcessor::iid,
                                      reinterpret_cast<void**>(&hosted->processor));
    if (!hosted->processor) {
        delete hosted;
        return nullptr;
    }

    BusInfo bus {};
    hosted->in_channels =
        (hosted->component->getBusInfo(kAudio, kInput, 0, bus) == kResultOk) ? bus.channelCount : 0;
    hosted->out_channels =
        (hosted->component->getBusInfo(kAudio, kOutput, 0, bus) == kResultOk) ? bus.channelCount : 0;

    ProcessSetup setup {};
    setup.processMode = kRealtime;
    setup.symbolicSampleSize = kSample32;
    setup.maxSamplesPerBlock = block_size > 0 ? block_size : 512;
    setup.sampleRate = sample_rate;
    hosted->processor->setupProcessing(setup);
    hosted->component->setActive(true);
    hosted->processor->setProcessing(true);
    hosted->active = true;

    hosted->in_ptrs.assign(hosted->in_channels, nullptr);
    hosted->out_ptrs.assign(hosted->out_channels, nullptr);
    hosted->context.sampleRate = sample_rate;
    hosted->context.state = ProcessContext::kPlaying;

    if (in_channels) *in_channels = hosted->in_channels;
    if (out_channels) *out_channels = hosted->out_channels;
    return hosted;
}

void vst3_close(void* handle) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted) return;
    if (hosted->active && hosted->processor) {
        hosted->processor->setProcessing(false);
        hosted->component->setActive(false);
    }
    delete hosted;
}

void vst3_connect_input(void* handle, int channel, float* data) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (hosted && channel >= 0 && channel < static_cast<int>(hosted->in_ptrs.size()))
        hosted->in_ptrs[channel] = data;
}

void vst3_connect_output(void* handle, int channel, float* data) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (hosted && channel >= 0 && channel < static_cast<int>(hosted->out_ptrs.size()))
        hosted->out_ptrs[channel] = data;
}

int vst3_param_count(void* handle) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->controller) return 0;
    return hosted->controller->getParameterCount();
}

/// Fills name/def/flags for `index`; returns 1 on success.
int vst3_param_info(void* handle, int index, char* name, int name_cap, float* def, int* flags) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->controller) return 0;
    ParameterInfo info {};
    if (hosted->controller->getParameterInfo(index, info) != kResultOk) return 0;
    if (name && name_cap > 0) {
        int i = 0;
        for (; i < name_cap - 1 && info.title[i]; ++i)
            name[i] = static_cast<char>(info.title[i]);
        name[i] = 0;
    }
    if (def) *def = static_cast<float>(info.defaultNormalizedValue);
    if (flags) *flags = info.flags;
    return 1;
}

void vst3_set_param(void* handle, int index, float normalized) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->controller) return;
    ParameterInfo info {};
    if (hosted->controller->getParameterInfo(index, info) != kResultOk) return;
    int32 queue_index = 0;
    if (auto* queue = hosted->changes.addParameterData(info.id, queue_index))
        queue->addPoint(0, normalized, queue_index);
}

float vst3_get_param(void* handle, int index) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->controller) return 0.0f;
    ParameterInfo info {};
    if (hosted->controller->getParameterInfo(index, info) != kResultOk) return 0.0f;
    return static_cast<float>(hosted->controller->getParamNormalized(info.id));
}

void vst3_run(void* handle, int frames) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->processor) return;

    AudioBusBuffers in_bus {};
    in_bus.numChannels = hosted->in_channels;
    in_bus.channelBuffers32 = hosted->in_ptrs.data();
    AudioBusBuffers out_bus {};
    out_bus.numChannels = hosted->out_channels;
    out_bus.channelBuffers32 = hosted->out_ptrs.data();

    ProcessData data {};
    data.processMode = kRealtime;
    data.symbolicSampleSize = kSample32;
    data.numSamples = frames;
    data.numInputs = hosted->in_channels > 0 ? 1 : 0;
    data.inputs = hosted->in_channels > 0 ? &in_bus : nullptr;
    data.numOutputs = hosted->out_channels > 0 ? 1 : 0;
    data.outputs = hosted->out_channels > 0 ? &out_bus : nullptr;
    data.inputParameterChanges = &hosted->changes;
    data.outputParameterChanges = &hosted->out_changes;
    data.inputEvents = &hosted->events;
    data.processContext = &hosted->context;

    hosted->processor->process(data);
    hosted->changes.clearQueue();
    hosted->out_changes.clearQueue();
    hosted->events.clear();
}

/// Writes the component state to `out`; returns the byte count (or -1).
int vst3_save_state(void* handle, char* out, int cap) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->component) return -1;
    MemoryStream stream;
    if (hosted->component->getState(&stream) != kResultOk) return -1;
    const int size = static_cast<int>(stream.getSize());
    if (size <= 0) return 0;
    if (out && cap >= size) std::memcpy(out, stream.getData(), size);
    return size;
}

/// Applies state previously produced by `vst3_save_state`.
void vst3_load_state(void* handle, const char* data, int len) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->component || !data || len <= 0) return;
    MemoryStream stream;
    stream.write(const_cast<char*>(data), len, nullptr);
    stream.seek(0, IBStream::kIBSeekSet, nullptr);
    hosted->component->setState(&stream);
    if (hosted->controller) {
        stream.seek(0, IBStream::kIBSeekSet, nullptr);
        hosted->controller->setComponentState(&stream);
    }
}

} // extern "C"
