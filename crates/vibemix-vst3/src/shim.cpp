// C ABI around the VST3 SDK hosting helpers. The Rust side owns plugin
// instances through opaque `Hosted*` handles.

#include <X11/Xlib.h>
#include <X11/Xatom.h>

#include <atomic>
#include <chrono>
#include <cstdlib>
#include <limits>
#include <cstring>
#include <filesystem>
#include <memory>
#include <mutex>
#include <poll.h>
#include <string>
#include <vector>

#include "public.sdk/source/common/memorystream.h"
#include "public.sdk/source/vst/hosting/eventlist.h"
#include "public.sdk/source/vst/hosting/hostclasses.h"
#include "public.sdk/source/vst/hosting/module.h"
#include "public.sdk/source/vst/hosting/parameterchanges.h"
#include "public.sdk/source/vst/hosting/plugprovider.h"

#include "pluginterfaces/gui/iplugview.h"
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

struct Hosted;

/// Minimal IComponentHandler: many plugins expect the host to provide one, and
/// `performEdit` carries the editor's parameter moves back to the host.
class ComponentHandler : public IComponentHandler {
public:
    explicit ComponentHandler(Hosted* owner) : owner(owner) { FUNKNOWN_CTOR }
    ~ComponentHandler() noexcept { FUNKNOWN_DTOR }

    tresult PLUGIN_API beginEdit(ParamID) SMTG_OVERRIDE { return kResultOk; }
    tresult PLUGIN_API performEdit(ParamID id, ParamValue value) SMTG_OVERRIDE;
    tresult PLUGIN_API endEdit(ParamID) SMTG_OVERRIDE { return kResultOk; }
    tresult PLUGIN_API restartComponent(int32 flags) SMTG_OVERRIDE;

    DECLARE_FUNKNOWN_METHODS

private:
    Hosted* owner;
};

IMPLEMENT_FUNKNOWN_METHODS(ComponentHandler, IComponentHandler, IComponentHandler::iid)

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
    IPtr<ComponentHandler> component_handler;
    int32 in_channels = 0;
    int32 out_channels = 0;
    std::vector<float*> in_ptrs;
    std::vector<float*> out_ptrs;
    // All audio buses are presented to the plugin; bus 0 is the wired
    // main signal, aux buses (e.g. a sidechain input) get silence.
    int32 in_bus_count = 0;
    int32 out_bus_count = 0;
    std::vector<AudioBusBuffers> in_bus_buffers;
    std::vector<AudioBusBuffers> out_bus_buffers;
    std::vector<std::vector<float*>> aux_in_ptrs;
    std::vector<std::vector<float*>> aux_out_ptrs;
    std::vector<float> silence_frames;
    std::vector<float> aux_out_frames;
    // First non-CV aux input bus (sidechain), or -1 if none. Its channel
    // buffers are dedicated so the host can write a sidechain signal.
    int32 sidechain_bus = -1;
    std::vector<float> sidechain_frames;
    ParameterChanges changes;
    ParameterChanges out_changes;
    EventList events;
    ProcessContext context {};
    bool active = false;
    tresult setup_result = kResultOk;
    tresult active_result = kResultOk;
    tresult processing_result = kResultOk;
    tresult last_result = kResultOk;
    std::mutex param_edits_mutex;
    std::vector<std::pair<int, float>> param_edits;
    std::atomic<int32> restart_flags {0};
    // Cached parameter ids and last-sent values so an unchanged block does no
    // `getParameterInfo`/automation work on the audio thread.
    std::vector<ParamID> param_ids;
    std::vector<float> last_param;

    void push_param_edit(int index, float value) {
        std::lock_guard<std::mutex> lock(param_edits_mutex);
        if (param_edits.size() < 256) param_edits.emplace_back(index, value);
    }
};

tresult ComponentHandler::performEdit(ParamID id, ParamValue value) {
    if (!owner || !owner->controller) return kResultOk;
    // Keep the controller's value in sync for the editor, then queue the move
    // for the engine (which folds it into the shared parameter state).
    owner->controller->setParamNormalized(id, value);
    const int32 count = owner->controller->getParameterCount();
    for (int32 i = 0; i < count; ++i) {
        ParameterInfo info {};
        if (owner->controller->getParameterInfo(i, info) == kResultOk && info.id == id) {
            owner->push_param_edit(i, static_cast<float>(value));
            break;
        }
    }
    return kResultOk;
}

tresult ComponentHandler::restartComponent(int32 flags) {
    if (owner) owner->restart_flags.fetch_or(flags);
    return kResultOk;
}

struct VibemixVst3View;

/// The frame plugins call to resize/close the editor window. It also implements
/// `IRunLoop` (timers + file-descriptor event handlers), which DPF-based plugins
/// (e.g. Dragonfly) query on the frame and require before attaching.
class PlugFrame : public IPlugFrame, public Linux::IRunLoop {
public:
    explicit PlugFrame(VibemixVst3View* owner) : owner(owner) {}

    tresult PLUGIN_API resizeView(IPlugView* view, ViewRect* newSize) override;

    tresult PLUGIN_API registerEventHandler(Linux::IEventHandler* handler,
                                            Linux::FileDescriptor fd) override;
    tresult PLUGIN_API unregisterEventHandler(Linux::IEventHandler* handler) override;
    tresult PLUGIN_API registerTimer(Linux::ITimerHandler* handler,
                                     Linux::TimerInterval milliseconds) override;
    tresult PLUGIN_API unregisterTimer(Linux::ITimerHandler* handler) override;

    /// Runs due timers and services registered descriptors. Called from the
    /// control loop (`vst3_view_pump`), never the audio thread.
    void pump();

    DECLARE_FUNKNOWN_METHODS

private:
    struct Timer {
        IPtr<Linux::ITimerHandler> handler;
        uint64 interval_ms;
        std::chrono::steady_clock::time_point next;
    };
    struct EventHandler {
        IPtr<Linux::IEventHandler> handler;
        Linux::FileDescriptor fd;
    };
    VibemixVst3View* owner;
    std::vector<Timer> timers;
    std::vector<EventHandler> event_handlers;
};

IMPLEMENT_REFCOUNT(PlugFrame)

// The frame exposes both `IPlugFrame` and `IRunLoop`; each plugin queries the
// one it needs.
tresult PLUGIN_API PlugFrame::queryInterface(const TUID _iid, void** obj) {
    QUERY_INTERFACE(_iid, obj, FUnknown::iid, PlugFrame)
    QUERY_INTERFACE(_iid, obj, IPlugFrame::iid, IPlugFrame)
    QUERY_INTERFACE(_iid, obj, Linux::IRunLoop::iid, Linux::IRunLoop)
    *obj = nullptr;
    return kNoInterface;
}

struct VibemixVst3View {
    IPtr<IEditController> controller;
    IPtr<IPlugView> view;
    IPtr<PlugFrame> frame;
    Display* dpy = nullptr;
    Window win = 0;
    Atom wm_delete = 0;
    bool wants_close = false;
};

tresult PlugFrame::resizeView(IPlugView*, ViewRect* newSize) {
    if (owner && owner->dpy && owner->win && newSize && newSize->getWidth() > 0 &&
        newSize->getHeight() > 0) {
        XResizeWindow(owner->dpy, owner->win, (unsigned)newSize->getWidth(),
                      (unsigned)newSize->getHeight());
        XFlush(owner->dpy);
    }
    return kResultTrue;
}

tresult PLUGIN_API PlugFrame::registerTimer(Linux::ITimerHandler* handler,
                                            Linux::TimerInterval milliseconds) {
    if (!handler) return kInvalidArgument;
    const uint64 interval = milliseconds > 0 ? milliseconds : 16;
    timers.push_back({IPtr<Linux::ITimerHandler>(handler), interval,
                      std::chrono::steady_clock::now() +
                          std::chrono::milliseconds(interval)});
    return kResultOk;
}

tresult PLUGIN_API PlugFrame::unregisterTimer(Linux::ITimerHandler* handler) {
    for (auto it = timers.begin(); it != timers.end(); ++it) {
        if (it->handler.get() == handler) {
            timers.erase(it);
            break;
        }
    }
    return kResultOk;
}

tresult PLUGIN_API PlugFrame::registerEventHandler(Linux::IEventHandler* handler,
                                                   Linux::FileDescriptor fd) {
    if (!handler) return kInvalidArgument;
    event_handlers.push_back({IPtr<Linux::IEventHandler>(handler), fd});
    return kResultOk;
}

tresult PLUGIN_API PlugFrame::unregisterEventHandler(Linux::IEventHandler* handler) {
    for (auto it = event_handlers.begin(); it != event_handlers.end(); ++it) {
        if (it->handler.get() == handler) {
            event_handlers.erase(it);
            break;
        }
    }
    return kResultOk;
}

void PlugFrame::pump() {
    const auto now = std::chrono::steady_clock::now();
    // Snapshot the handlers before invoking them: a callback may register or
    // unregister timers, which would invalidate the vector iterators.
    std::vector<IPtr<Linux::ITimerHandler>> due;
    for (auto& timer : timers) {
        if (now >= timer.next) {
            timer.next = now + std::chrono::milliseconds(timer.interval_ms);
            due.push_back(timer.handler);
        }
    }
    for (auto& handler : due) {
        if (handler) handler->onTimer();
    }

    if (!event_handlers.empty()) {
        std::vector<pollfd> descriptors;
        descriptors.reserve(event_handlers.size());
        for (const auto& entry : event_handlers) {
            descriptors.push_back({entry.fd, POLLIN, 0});
        }
        if (poll(descriptors.data(), descriptors.size(), 0) > 0) {
            for (size_t i = 0; i < descriptors.size(); ++i) {
                if (descriptors[i].revents != 0 && event_handlers[i].handler) {
                    event_handlers[i].handler->onFDIsSet(event_handlers[i].fd);
                }
            }
        }
    }
}

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
    if (hosted->controller) {
        hosted->component_handler = new ComponentHandler(hosted);
        hosted->controller->setComponentHandler(hosted->component_handler);
        // Cache parameter ids and last-sent values once (see `vst3_set_param`).
        const int32 count = hosted->controller->getParameterCount();
        hosted->param_ids.assign(count, kNoParamId);
        hosted->last_param.assign(count, std::numeric_limits<float>::quiet_NaN());
        for (int32 i = 0; i < count; ++i) {
            ParameterInfo info {};
            if (hosted->controller->getParameterInfo(i, info) == kResultOk)
                hosted->param_ids[i] = info.id;
        }
    }
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
    const int32 in_buses = hosted->component->getBusCount(kAudio, kInput);
    const int32 out_buses = hosted->component->getBusCount(kAudio, kOutput);
    hosted->in_channels =
        (hosted->component->getBusInfo(kAudio, kInput, 0, bus) == kResultOk) ? bus.channelCount : 0;
    hosted->out_channels =
        (hosted->component->getBusInfo(kAudio, kOutput, 0, bus) == kResultOk) ? bus.channelCount : 0;

    // First non-CV auxiliary input bus: the sidechain the host can drive.
    for (int32 b = 1; b < in_buses; ++b) {
        BusInfo info {};
        if (hosted->component->getBusInfo(kAudio, kInput, b, info) != kResultOk) continue;
        if (info.busType == kAux && (info.flags & BusInfo::kIsControlVoltage) == 0) {
            hosted->sidechain_bus = b;
            break;
        }
    }

    // Ask for the layout we will actually deliver (mono/stereo on bus 0), so
    // plugins that require an explicit arrangement configure correctly.
    const int32 num_arr_in = in_buses > 0 ? in_buses : 0;
    const int32 num_arr_out = out_buses > 0 ? out_buses : 0;
    if (num_arr_in > 0 && num_arr_out > 0) {
        std::vector<SpeakerArrangement> arr_in(num_arr_in, SpeakerArr::kEmpty);
        std::vector<SpeakerArrangement> arr_out(num_arr_out, SpeakerArr::kEmpty);
        arr_in[0] = hosted->in_channels == 1 ? SpeakerArr::kMono : SpeakerArr::kStereo;
        arr_out[0] = hosted->out_channels == 1 ? SpeakerArr::kMono : SpeakerArr::kStereo;
        if (hosted->sidechain_bus > 0 && hosted->sidechain_bus < num_arr_in) {
            BusInfo sidechain {};
            if (hosted->component->getBusInfo(kAudio, kInput, hosted->sidechain_bus, sidechain) ==
                kResultOk)
                arr_in[hosted->sidechain_bus] =
                    sidechain.channelCount == 1 ? SpeakerArr::kMono : SpeakerArr::kStereo;
        }
        hosted->processor->setBusArrangements(arr_in.data(), num_arr_in, arr_out.data(),
                                              num_arr_out);
    }

    ProcessSetup setup {};
    setup.processMode = kRealtime;
    setup.symbolicSampleSize = kSample32;
    setup.maxSamplesPerBlock = block_size > 0 ? block_size : 512;
    setup.sampleRate = sample_rate;
    hosted->setup_result = hosted->processor->setupProcessing(setup);
    hosted->active_result = hosted->component->setActive(true);

    // Buses are nominally active after setActive, but some plugins (e.g. JUCE's
    // wrapper) only route audio for buses the host explicitly activates.
    if (in_buses > 0) hosted->component->activateBus(kAudio, kInput, 0, true);
    if (hosted->sidechain_bus >= 0)
        hosted->component->activateBus(kAudio, kInput, hosted->sidechain_bus, true);
    if (out_buses > 0) hosted->component->activateBus(kAudio, kOutput, 0, true);

    hosted->processing_result = hosted->processor->setProcessing(true);
    hosted->active = true;

    hosted->in_ptrs.assign(hosted->in_channels, nullptr);
    hosted->out_ptrs.assign(hosted->out_channels, nullptr);

    // Present every bus to the plugin. Bus 0 is wired (in `vst3_run`) to the
    // real buffers; aux input buses (e.g. sidechain) are fed silence and aux
    // output buses get throwaway scratch, so the bus topology matches what the
    // plugin was configured with.
    constexpr int32 kScratchFrames = 8192;
    hosted->in_bus_count = in_buses;
    hosted->out_bus_count = out_buses;
    hosted->in_bus_buffers.assign(in_buses > 0 ? in_buses : 0, AudioBusBuffers {});
    hosted->out_bus_buffers.assign(out_buses > 0 ? out_buses : 0, AudioBusBuffers {});
    hosted->aux_in_ptrs.assign(in_buses > 0 ? in_buses : 0, {});
    hosted->aux_out_ptrs.assign(out_buses > 0 ? out_buses : 0, {});
    hosted->silence_frames.assign(kScratchFrames, 0.0f);

    int32 aux_out_channels = 0;
    for (int32 b = 1; b < out_buses; ++b) {
        BusInfo info {};
        if (hosted->component->getBusInfo(kAudio, kOutput, b, info) == kResultOk)
            aux_out_channels += info.channelCount;
    }
    hosted->aux_out_frames.assign(static_cast<size_t>(aux_out_channels) * kScratchFrames, 0.0f);

    for (int32 b = 1; b < in_buses; ++b) {
        BusInfo info {};
        const int32 channels =
            hosted->component->getBusInfo(kAudio, kInput, b, info) == kResultOk
                ? info.channelCount
                : 0;
        auto& ptrs = hosted->aux_in_ptrs[b];
        if (b == hosted->sidechain_bus && channels > 0) {
            // Dedicated per-channel buffers so the host can write a sidechain.
            hosted->sidechain_frames.assign(
                static_cast<size_t>(channels) * kScratchFrames, 0.0f);
            ptrs.assign(channels, nullptr);
            for (int32 j = 0; j < channels; ++j) {
                ptrs[j] = hosted->sidechain_frames.data() +
                          static_cast<size_t>(j) * kScratchFrames;
            }
        } else {
            ptrs.assign(channels, hosted->silence_frames.data());
        }
        hosted->in_bus_buffers[b].numChannels = channels;
        hosted->in_bus_buffers[b].channelBuffers32 = ptrs.data();
    }
    int32 aux_out_offset = 0;
    for (int32 b = 1; b < out_buses; ++b) {
        BusInfo info {};
        const int32 channels =
            hosted->component->getBusInfo(kAudio, kOutput, b, info) == kResultOk
                ? info.channelCount
                : 0;
        auto& ptrs = hosted->aux_out_ptrs[b];
        ptrs.assign(channels, nullptr);
        for (auto& ptr : ptrs) {
            ptr = hosted->aux_out_frames.data() +
                  static_cast<size_t>(aux_out_offset) * kScratchFrames;
            ++aux_out_offset;
        }
        hosted->out_bus_buffers[b].numChannels = channels;
        hosted->out_bus_buffers[b].channelBuffers32 = ptrs.data();
    }

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

/// Fills name/def/flags/step-count for `index`; returns 1 on success.
int vst3_param_info(void* handle, int index, char* name, int name_cap, float* def, int* flags,
                    int* step_count) {
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
    if (step_count) *step_count = info.stepCount;
    return 1;
}

/// Writes the display string for `normalized` on `index`; returns 1 on success.
int vst3_param_value_string(void* handle, int index, float normalized, char* out, int cap) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->controller || !out || cap <= 0) return 0;
    ParameterInfo info {};
    if (hosted->controller->getParameterInfo(index, info) != kResultOk) return 0;
    String128 text {};
    if (hosted->controller->getParamStringByValue(info.id, normalized, text) != kResultOk)
        return 0;
    int i = 0;
    for (; i < cap - 1 && text[i]; ++i)
        out[i] = static_cast<char>(text[i]);
    out[i] = 0;
    return 1;
}

void vst3_set_param(void* handle, int index, float normalized) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || index < 0 || index >= static_cast<int>(hosted->param_ids.size())) return;
    const ParamID id = hosted->param_ids[index];
    if (id == kNoParamId || hosted->last_param[index] == normalized) return;
    hosted->last_param[index] = normalized;
    int32 queue_index = 0;
    if (auto* queue = hosted->changes.addParameterData(id, queue_index))
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

    // Bus 0 is the wired main signal; its pointer array is refreshed here
    // because the Rust side connects the buffers after opening the plugin.
    if (hosted->in_bus_count > 0) {
        hosted->in_bus_buffers[0].numChannels = hosted->in_channels;
        hosted->in_bus_buffers[0].channelBuffers32 = hosted->in_ptrs.data();
    }
    if (hosted->out_bus_count > 0) {
        hosted->out_bus_buffers[0].numChannels = hosted->out_channels;
        hosted->out_bus_buffers[0].channelBuffers32 = hosted->out_ptrs.data();
    }

    ProcessData data {};
    data.processMode = kRealtime;
    data.symbolicSampleSize = kSample32;
    data.numSamples = frames;
    data.numInputs = hosted->in_bus_count;
    data.inputs = hosted->in_bus_count > 0 ? hosted->in_bus_buffers.data() : nullptr;
    data.numOutputs = hosted->out_bus_count;
    data.outputs = hosted->out_bus_count > 0 ? hosted->out_bus_buffers.data() : nullptr;
    data.inputParameterChanges = &hosted->changes;
    data.outputParameterChanges = &hosted->out_changes;
    data.inputEvents = &hosted->events;
    data.processContext = &hosted->context;

    hosted->last_result = hosted->processor->process(data);
    hosted->changes.clearQueue();
    hosted->out_changes.clearQueue();
    hosted->events.clear();
}

/// Writes a description of the plugin's editor support to `out`.
int vst3_editor_info(void* handle, char* out, int cap) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !out || cap <= 0) return 0;
    if (!hosted->controller) {
        std::snprintf(out, cap, "no-controller");
        return 1;
    }
    IPlugView* view = hosted->controller->createView(ViewType::kEditor);
    if (!view) {
        std::snprintf(out, cap, "no-editor");
        return 1;
    }
    std::string types;
    if (view->isPlatformTypeSupported(kPlatformTypeX11EmbedWindowID) == kResultTrue)
        types += "x11 ";
    if (view->isPlatformTypeSupported(kPlatformTypeHWND) == kResultTrue) types += "hwnd ";
    if (view->isPlatformTypeSupported(kPlatformTypeNSView) == kResultTrue) types += "nsview ";
    if (view->isPlatformTypeSupported(kPlatformTypeUIView) == kResultTrue) types += "uiview ";
    if (types.empty()) types = "unsupported";
    std::snprintf(out, cap, "editor:%s", types.c_str());
    view->release();
    return 1;
}

/// Whether the plugin offers an X11-embeddable editor.
int vst3_has_editor(void* handle) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->controller) return 0;
    IPlugView* view = hosted->controller->createView(ViewType::kEditor);
    if (!view) return 0;
    const int ok = view->isPlatformTypeSupported(kPlatformTypeX11EmbedWindowID) == kResultTrue;
    view->release();
    return ok;
}

/// Whether the plugin has an auxiliary (sidechain) input bus.
int vst3_has_sidechain(void* handle) {
    auto* hosted = static_cast<Hosted*>(handle);
    return (hosted && hosted->sidechain_bus >= 0) ? 1 : 0;
}

/// Writes a stereo sidechain signal into the plugin's sidechain input bus.
void vst3_write_sidechain(void* handle, const float* left, const float* right, int frames) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || hosted->sidechain_bus < 0 || frames <= 0) return;
    auto& ptrs = hosted->aux_in_ptrs[hosted->sidechain_bus];
    const int32 channels = hosted->in_bus_buffers[hosted->sidechain_bus].numChannels;
    if (channels >= 2) {
        if (left) std::memcpy(ptrs[0], left, sizeof(float) * (size_t)frames);
        if (right) std::memcpy(ptrs[1], right, sizeof(float) * (size_t)frames);
        for (int32 j = 2; j < channels; ++j) std::memset(ptrs[j], 0, sizeof(float) * (size_t)frames);
    } else if (channels == 1) {
        for (int32 s = 0; s < frames; ++s)
            ptrs[0][s] = 0.5f * ((left ? left[s] : 0.0f) + (right ? right[s] : 0.0f));
    }
}

/// Writes a short description of the component's audio buses to `out`.
int vst3_bus_info(void* handle, char* out, int cap) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->component || !out || cap <= 0) return 0;
    std::string text;
    const int32 in_buses = hosted->component->getBusCount(kAudio, kInput);
    const int32 out_buses = hosted->component->getBusCount(kAudio, kOutput);
    for (int32 i = 0; i < in_buses; ++i) {
        BusInfo bus {};
        if (hosted->component->getBusInfo(kAudio, kInput, i, bus) == kResultOk)
            text += " in" + std::to_string(i) + "=" + std::to_string(bus.channelCount)
                    + (bus.busType == BusTypes::kMain ? "m" : "a");
    }
    for (int32 i = 0; i < out_buses; ++i) {
        BusInfo bus {};
        if (hosted->component->getBusInfo(kAudio, kOutput, i, bus) == kResultOk)
            text += " out" + std::to_string(i) + "=" + std::to_string(bus.channelCount)
                    + (bus.busType == BusTypes::kMain ? "m" : "a");
    }
    std::snprintf(out, cap, "%s", text.c_str());
    return 1;
}

/// Pops one editor-driven parameter move; returns 0 when the queue is empty.
int vst3_take_param_edit(void* handle, int* index, float* value) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted) return 0;
    std::lock_guard<std::mutex> lock(hosted->param_edits_mutex);
    if (hosted->param_edits.empty()) return 0;
    const auto edit = hosted->param_edits.front();
    hosted->param_edits.erase(hosted->param_edits.begin());
    if (index) *index = edit.first;
    if (value) *value = edit.second;
    return 1;
}

/// Returns and clears the restart flags requested by the plugin.
int vst3_take_restart_flags(void* handle) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted) return 0;
    return hosted->restart_flags.exchange(0);
}

/// The plugin's reported processing latency in samples.
int vst3_latency(void* handle) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted || !hosted->processor) return 0;
    return static_cast<int>(hosted->processor->getLatencySamples());
}

/// Reports the tresults captured during setup and the last `process` call.
int vst3_status(void* handle, int* setup, int* active, int* processing, int* last) {
    auto* hosted = static_cast<Hosted*>(handle);
    if (!hosted) return 0;
    if (setup) *setup = static_cast<int>(hosted->setup_result);
    if (active) *active = static_cast<int>(hosted->active_result);
    if (processing) *processing = static_cast<int>(hosted->processing_result);
    if (last) *last = static_cast<int>(hosted->last_result);
    return 1;
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

void vst3_view_close(void* handle);

/// Opens the plugin's editor embedded into a fresh X11 window (never mapped
/// when `headless`). Returns an opaque view handle, or null on failure.
static void* open_view_impl(void* instance, const char* title, int headless) {
    static std::once_flag x_once;
    std::call_once(x_once, [] { XInitThreads(); });

    auto* hosted = static_cast<Hosted*>(instance);
    if (!hosted || !hosted->controller) return nullptr;
    IPtr<IPlugView> view = hosted->controller->createView(ViewType::kEditor);
    if (!view) return nullptr;
    if (view->isPlatformTypeSupported(kPlatformTypeX11EmbedWindowID) != kResultTrue) return nullptr;

    Display* dpy = XOpenDisplay(nullptr);
    if (!dpy) return nullptr;
    const int screen = DefaultScreen(dpy);
    const char* window_title = (title && *title) ? title : "vibemix plugin";
    Window win = XCreateSimpleWindow(dpy, RootWindow(dpy, screen), 80, 80, 400, 300, 0,
                                     BlackPixel(dpy, screen), WhitePixel(dpy, screen));
    XStoreName(dpy, win, window_title);
    {
        Atom net_wm_name = XInternAtom(dpy, "_NET_WM_NAME", False);
        Atom utf8_string = XInternAtom(dpy, "UTF8_STRING", False);
        XChangeProperty(dpy, win, net_wm_name, utf8_string, 8, PropModeReplace,
                        (const unsigned char*)window_title, (int)std::strlen(window_title));
    }
    Atom wm_delete = XInternAtom(dpy, "WM_DELETE_WINDOW", False);
    XSetWMProtocols(dpy, win, &wm_delete, 1);
    if (!headless) XMapWindow(dpy, win);
    XFlush(dpy);

    auto* self = new VibemixVst3View();
    self->controller = hosted->controller;
    self->view = view;
    self->dpy = dpy;
    self->win = win;
    self->wm_delete = wm_delete;
    self->frame = new PlugFrame(self);
    self->view->setFrame(self->frame);

    if (self->view->attached((void*)(uintptr_t)win, kPlatformTypeX11EmbedWindowID) !=
        kResultTrue) {
        vst3_view_close(self);
        return nullptr;
    }

    ViewRect rect {};
    if (self->view->getSize(&rect) == kResultTrue && rect.getWidth() > 0 &&
        rect.getHeight() > 0) {
        XResizeWindow(dpy, win, (unsigned)rect.getWidth(), (unsigned)rect.getHeight());
        XFlush(dpy);
    }
    return self;
}

void* vst3_view_open(void* instance, const char* title) {
    return open_view_impl(instance, title, 0);
}

/// Creates and tears down the editor headlessly, to catch crashing editors
/// before opening one for real. Returns 1 when the editor was usable.
int vst3_smoke_editor(void* instance) {
    void* view = open_view_impl(instance, "vibemix editor smoke", 1);
    if (!view) return 0;
    vst3_view_close(view);
    return 1;
}

/// Processes pending X events; a WM_DELETE_WINDOW sets the close flag.
void vst3_view_pump(void* handle) {
    auto* self = static_cast<VibemixVst3View*>(handle);
    if (!self) return;
    // Drive the plugin's IRunLoop (timers/fd handlers). DPF plugins use the
    // timer for their idle/repaint pass.
    if (self->frame) self->frame->pump();
    if (!self->dpy) return;
    while (XPending(self->dpy) > 0) {
        XEvent event;
        XNextEvent(self->dpy, &event);
        if (event.type == ClientMessage &&
            (Atom)event.xclient.data.l[0] == self->wm_delete) {
            self->wants_close = true;
        }
    }
}

int vst3_view_wants_close(void* handle) {
    auto* self = static_cast<VibemixVst3View*>(handle);
    return (self && self->wants_close) ? 1 : 0;
}

/// Mirrors a host-driven parameter change into the plugin's editor. Called from
/// the control/UI thread (never the audio thread); `setParamNormalized` is a
/// controller-side, UI-thread call per the VST3 spec, while the processor is
/// driven by the parameter queue in `vst3_run`.
void vst3_view_set_param(void* handle, int index, float value) {
    auto* self = static_cast<VibemixVst3View*>(handle);
    if (!self || !self->controller) return;
    ParameterInfo info {};
    if (self->controller->getParameterInfo(index, info) == kResultOk)
        self->controller->setParamNormalized(info.id, value);
}

void vst3_view_close(void* handle) {
    auto* self = static_cast<VibemixVst3View*>(handle);
    if (!self) return;
    if (self->view) {
        self->view->removed();
        self->view->setFrame(nullptr);
    }
    self->view = nullptr;
    self->frame = nullptr;
    if (self->dpy) {
        if (self->win) XDestroyWindow(self->dpy, self->win);
        XCloseDisplay(self->dpy);
        self->dpy = nullptr;
    }
    delete self;
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
