// C ABI around the VST3 SDK hosting helpers. The Rust side owns plugin
// instances through opaque `Hosted*` handles.

#include <X11/Xlib.h>
#include <X11/Xatom.h>

#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <memory>
#include <mutex>
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

/// Minimal IComponentHandler: many plugins expect the host to provide one.
class ComponentHandler : public IComponentHandler {
public:
    ComponentHandler() { FUNKNOWN_CTOR }
    ~ComponentHandler() noexcept { FUNKNOWN_DTOR }

    tresult PLUGIN_API beginEdit(ParamID) SMTG_OVERRIDE { return kResultOk; }
    tresult PLUGIN_API performEdit(ParamID, ParamValue) SMTG_OVERRIDE { return kResultOk; }
    tresult PLUGIN_API endEdit(ParamID) SMTG_OVERRIDE { return kResultOk; }
    tresult PLUGIN_API restartComponent(int32) SMTG_OVERRIDE { return kResultOk; }

    DECLARE_FUNKNOWN_METHODS
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
    ParameterChanges changes;
    ParameterChanges out_changes;
    EventList events;
    ProcessContext context {};
    bool active = false;
    tresult setup_result = kResultOk;
    tresult active_result = kResultOk;
    tresult processing_result = kResultOk;
    tresult last_result = kResultOk;
};

struct VibemixVst3View;

/// The frame JUCE (and others) call to resize/close the editor window.
class PlugFrame : public IPlugFrame {
public:
    explicit PlugFrame(VibemixVst3View* owner) : owner(owner) {}

    tresult PLUGIN_API resizeView(IPlugView* view, ViewRect* newSize) override;

    DECLARE_FUNKNOWN_METHODS

private:
    VibemixVst3View* owner;
};

IMPLEMENT_FUNKNOWN_METHODS(PlugFrame, IPlugFrame, IPlugFrame::iid)

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
        hosted->component_handler = new ComponentHandler();
        hosted->controller->setComponentHandler(hosted->component_handler);
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

    // Ask for the layout we will actually deliver (mono/stereo on bus 0), so
    // plugins that require an explicit arrangement configure correctly.
    const int32 num_arr_in = in_buses > 0 ? in_buses : 0;
    const int32 num_arr_out = out_buses > 0 ? out_buses : 0;
    if (num_arr_in > 0 && num_arr_out > 0) {
        std::vector<SpeakerArrangement> arr_in(num_arr_in, SpeakerArr::kEmpty);
        std::vector<SpeakerArrangement> arr_out(num_arr_out, SpeakerArr::kEmpty);
        arr_in[0] = hosted->in_channels == 1 ? SpeakerArr::kMono : SpeakerArr::kStereo;
        arr_out[0] = hosted->out_channels == 1 ? SpeakerArr::kMono : SpeakerArr::kStereo;
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
    if (out_buses > 0) hosted->component->activateBus(kAudio, kOutput, 0, true);

    hosted->processing_result = hosted->processor->setProcessing(true);
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

/// Opens the plugin's editor embedded into a fresh X11 window. Returns an
/// opaque view handle, or null when the plugin has no X11 editor.
void* vst3_view_open(void* instance, const char* title) {
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
    XMapWindow(dpy, win);
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

/// Processes pending X events; a WM_DELETE_WINDOW sets the close flag.
void vst3_view_pump(void* handle) {
    auto* self = static_cast<VibemixVst3View*>(handle);
    if (!self || !self->dpy) return;
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

/// Mirrors a host-driven parameter change into the plugin's editor.
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
