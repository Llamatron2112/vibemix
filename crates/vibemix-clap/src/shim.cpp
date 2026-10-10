// CLAP host shim.
//
// Wraps the CLAP C ABI (vendored under vendor/clap) behind a small, flat C
// interface that `host.rs` mirrors, the same way `vibemix-vst3` wraps the VST3
// SDK. Covers audio-effect hosting: discovery, instancing, audio I/O (main bus
// plus an optional sidechain input), parameters, state, latency, transport and
// an X11 editor window.

#include <clap/clap.h>

#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>

#include <dlfcn.h>
#include <time.h>

#include <atomic>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <system_error>
#include <vector>

// Host identity reported to plugins.
#define VIBEMIX_HOST_NAME "vibemix"
#define VIBEMIX_HOST_VENDOR "vibemix"
#define VIBEMIX_HOST_URL "https://github.com/vibemix"
#define VIBEMIX_HOST_VERSION "0.1.0"

// WM_CLASS shared with the LV2 host so the desktop groups plugin windows.
#define VIBEMIX_WM_CLASS "vibemix-gui"

static thread_local bool tls_audio_thread = false;

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

static std::string json_escape(const char* text) {
  std::string out;
  if (!text) return out;
  for (const char* p = text; *p; ++p) {
    const unsigned char c = static_cast<unsigned char>(*p);
    switch (c) {
      case '"': out += "\\\""; break;
      case '\\': out += "\\\\"; break;
      case '\n': out += "\\n"; break;
      case '\r': out += "\\r"; break;
      case '\t': out += "\\t"; break;
      default:
        if (c < 0x20) {
          char buf[8];
          std::snprintf(buf, sizeof(buf), "\\u%04x", c);
          out += buf;
        } else {
          out += static_cast<char>(c);
        }
    }
  }
  return out;
}

static std::string bool_str(bool value) { return value ? "true" : "false"; }

// ---------------------------------------------------------------------------
// Module refcounting: a `.clap` file can host several plugins; keep it loaded
// while any instance is alive and only `deinit`/`dlclose` when the last one
// goes away.
// ---------------------------------------------------------------------------

struct Module {
  void* dl = nullptr;
  const clap_plugin_entry_t* entry = nullptr;
  int refs = 0;
};

static std::mutex g_modules_mutex;
static std::map<std::string, Module> g_modules;

// Returns the loaded entry for `path`, opening and initialising it on first
// use. Returns nullptr on failure.
static const clap_plugin_entry_t* module_acquire(const std::string& path) {
  std::lock_guard<std::mutex> lock(g_modules_mutex);
  auto it = g_modules.find(path);
  if (it != g_modules.end()) {
    it->second.refs += 1;
    return it->second.entry;
  }
  void* dl = dlopen(path.c_str(), RTLD_NOW | RTLD_LOCAL);
  if (!dl) {
    std::fprintf(stderr, "[clap] cannot load %s: %s\n", path.c_str(), dlerror());
    return nullptr;
  }
  const auto* entry =
      static_cast<const clap_plugin_entry_t*>(dlsym(dl, "clap_entry"));
  if (!entry) {
    std::fprintf(stderr, "[clap] %s: missing clap_entry\n", path.c_str());
    dlclose(dl);
    return nullptr;
  }
  if (entry->init && !entry->init(path.c_str())) {
    std::fprintf(stderr, "[clap] %s: init failed\n", path.c_str());
    dlclose(dl);
    return nullptr;
  }
  Module module{dl, entry, 1};
  g_modules.emplace(path, module);
  return entry;
}

static void module_release(const std::string& path) {
  std::lock_guard<std::mutex> lock(g_modules_mutex);
  auto it = g_modules.find(path);
  if (it == g_modules.end()) return;
  if (--it->second.refs > 0) return;
  if (it->second.entry && it->second.entry->deinit) it->second.entry->deinit();
  if (it->second.dl) dlclose(it->second.dl);
  g_modules.erase(it);
}

// ---------------------------------------------------------------------------
// Hosted instance
// ---------------------------------------------------------------------------

struct Port {
  bool is_input = false;
  bool is_main = false;
  uint32_t channels = 0;
  clap_id id = 0;
  std::vector<float*> ptrs;
  std::vector<float> storage;  // owned buffers for sidechain inputs
};

struct ParamEdit {
  uint32_t index;
  float value;
};

struct Timer {
  clap_id id;
  uint32_t period_ms;
  int64_t last_ms;
};

struct Hosted {
  std::string path;
  const clap_plugin_t* plugin = nullptr;
  const clap_plugin_params_t* params = nullptr;
  const clap_plugin_state_t* state = nullptr;
  const clap_plugin_latency_t* latency = nullptr;
  const clap_plugin_gui_t* gui = nullptr;
  const clap_plugin_audio_ports_t* audio_ports = nullptr;
  const clap_plugin_timer_support_t* plugin_timer = nullptr;

  clap_host_t host{};

  std::vector<Port> in_ports;
  std::vector<Port> out_ports;
  uint32_t main_in_index = 0;
  uint32_t main_out_index = 0;
  int sidechain_port = -1;

  uint32_t param_count = 0;
  std::vector<clap_id> param_ids;  // index -> id

  // Host -> plugin parameter changes, filled on the control thread and drained
  // on the audio thread.
  std::mutex pending_mutex;
  std::vector<clap_event_param_value_t> pending_params;

  // Plugin -> host parameter edits captured from output events.
  std::mutex edit_mutex;
  std::vector<ParamEdit> param_edits;

  // Per-block MIDI (audio thread only).
  std::vector<clap_event_midi_t> midi;
  bool transport_valid = false;
  clap_event_transport_t transport{};

  // Main-thread callbacks requested by the plugin.
  std::atomic<bool> want_callback{false};
  std::atomic<bool> want_process{false};
  std::atomic<bool> want_restart{false};

  std::vector<Timer> timers;

  // Editor (X11).
  Display* dpy = nullptr;
  Window win = 0;
  Atom wm_delete = 0;
  bool gui_created = false;
  bool want_close = false;

  // `start_processing`/`stop_processing` must run on the audio thread, so the
  // start is deferred to the first `clap_run`.
  bool want_start_processing = false;
  bool processing = false;
};

static Hosted* self(const clap_host_t* host) {
  return static_cast<Hosted*>(host->host_data);
}

// ---------------------------------------------------------------------------
// Host extensions
// ---------------------------------------------------------------------------

static void host_log(const clap_host_t*, clap_log_severity severity, const char* msg) {
  if (severity >= CLAP_LOG_WARNING && msg) std::fprintf(stderr, "[clap] %s\n", msg);
}

static bool host_is_main_thread(const clap_host_t*) { return !tls_audio_thread; }
static bool host_is_audio_thread(const clap_host_t*) { return tls_audio_thread; }

static void host_params_rescan(const clap_host_t*, clap_param_rescan_flags) {}
static void host_params_clear(const clap_host_t*, clap_id, clap_param_clear_flags) {}
static void host_params_flush(const clap_host_t*) {
  // `clap_plugin_params.flush` is [active ? audio-thread : main-thread]; since
  // we process continuously, pending host parameter changes are delivered in
  // process(). Nothing to do here.
}

static void host_gui_hints_changed(const clap_host_t*) {}
static void host_gui_closed(const clap_host_t* host, bool) {
  if (Hosted* h = self(host)) h->want_close = true;
}
static bool host_gui_request_resize(const clap_host_t* host, uint32_t width, uint32_t height) {
  Hosted* h = self(host);
  if (!h || !h->dpy || !h->win || width == 0 || height == 0) return false;
  XResizeWindow(h->dpy, h->win, width, height);
  return true;
}
static bool host_gui_request_show(const clap_host_t* host) {
  Hosted* h = self(host);
  if (!h || !h->gui || !h->gui->show) return false;
  return h->gui->show(h->plugin);
}
static bool host_gui_request_hide(const clap_host_t* host) {
  Hosted* h = self(host);
  if (!h || !h->gui || !h->gui->hide) return false;
  return h->gui->hide(h->plugin);
}

static bool host_timer_register(const clap_host_t* host, uint32_t period_ms, clap_id* timer_id) {
  Hosted* h = self(host);
  if (!h) return false;
  const clap_id id = static_cast<clap_id>(h->timers.size() + 1);
  h->timers.push_back(Timer{id, period_ms ? period_ms : 30, 0});
  if (timer_id) *timer_id = id;
  return true;
}
static bool host_timer_unregister(const clap_host_t* host, clap_id timer_id) {
  Hosted* h = self(host);
  if (!h) return false;
  for (auto it = h->timers.begin(); it != h->timers.end(); ++it) {
    if (it->id == timer_id) {
      h->timers.erase(it);
      return true;
    }
  }
  return false;
}

static bool host_audio_ports_supported(const clap_host_t*, uint32_t) { return false; }
static void host_audio_ports_rescan(const clap_host_t*, uint32_t) {}

static void host_request_restart(const clap_host_t* host) {
  if (Hosted* h = self(host)) h->want_restart.store(true);
}
static void host_request_process(const clap_host_t* host) {
  if (Hosted* h = self(host)) h->want_process.store(true);
}
static void host_request_callback(const clap_host_t* host) {
  if (Hosted* h = self(host)) h->want_callback.store(true);
}

static const void* host_get_extension(const clap_host_t* host, const char* id) {
  (void)host;
  if (!id) return nullptr;
  static const clap_host_log_t log_ext{host_log};
  static const clap_host_thread_check_t thread_ext{host_is_main_thread, host_is_audio_thread};
  static const clap_host_params_t params_ext{host_params_rescan, host_params_clear,
                                             host_params_flush};
  static const clap_host_gui_t gui_ext{host_gui_hints_changed, host_gui_request_resize,
                                       host_gui_request_show, host_gui_request_hide,
                                       host_gui_closed};
  static const clap_host_timer_support_t timer_ext{host_timer_register, host_timer_unregister};
  static const clap_host_audio_ports_t audio_ports_ext{host_audio_ports_supported,
                                                       host_audio_ports_rescan};
  if (!std::strcmp(id, CLAP_EXT_LOG)) return &log_ext;
  if (!std::strcmp(id, CLAP_EXT_THREAD_CHECK)) return &thread_ext;
  if (!std::strcmp(id, CLAP_EXT_PARAMS)) return &params_ext;
  if (!std::strcmp(id, CLAP_EXT_GUI)) return &gui_ext;
  if (!std::strcmp(id, CLAP_EXT_TIMER_SUPPORT)) return &timer_ext;
  if (!std::strcmp(id, CLAP_EXT_AUDIO_PORTS)) return &audio_ports_ext;
  return nullptr;
}

// ---------------------------------------------------------------------------
// Event lists
// ---------------------------------------------------------------------------

struct InEvents {
  std::vector<const clap_event_header_t*> events;
};

static uint32_t in_events_size(const clap_input_events_t* list) {
  return static_cast<uint32_t>(static_cast<InEvents*>(list->ctx)->events.size());
}
static const clap_event_header_t* in_events_get(const clap_input_events_t* list, uint32_t index) {
  auto* ctx = static_cast<InEvents*>(list->ctx);
  return index < ctx->events.size() ? ctx->events[index] : nullptr;
}

struct OutEvents {
  std::vector<std::vector<uint8_t>> events;
};

static bool out_events_push(const clap_output_events_t* list, const clap_event_header_t* event) {
  auto* ctx = static_cast<OutEvents*>(list->ctx);
  if (!event || event->size == 0) return false;
  ctx->events.emplace_back(static_cast<const uint8_t*>(static_cast<const void*>(event)),
                           static_cast<const uint8_t*>(static_cast<const void*>(event)) + event->size);
  return true;
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

static void scan_one_file(const std::string& path, std::string& json, bool& first) {
  const clap_plugin_entry_t* entry = module_acquire(path);
  if (!entry) return;
  const auto* factory = static_cast<const clap_plugin_factory_t*>(
      entry->get_factory ? entry->get_factory(CLAP_PLUGIN_FACTORY_ID) : nullptr);
  if (!factory) {
    module_release(path);
    return;
  }
  const uint32_t count = factory->get_plugin_count ? factory->get_plugin_count(factory) : 0;
  for (uint32_t i = 0; i < count; ++i) {
    const clap_plugin_descriptor_t* desc =
        factory->get_plugin_descriptor ? factory->get_plugin_descriptor(factory, i) : nullptr;
    if (!desc || !desc->id || !desc->name) continue;
    std::string features;
    if (desc->features) {
      for (const char* const* f = desc->features; *f; ++f) {
        if (!features.empty()) features += ",";
        features += *f;
      }
    }
    if (!first) json += ",";
    first = false;
    json += "{\"id\":\"" + json_escape(desc->id) + "\"";
    json += ",\"name\":\"" + json_escape(desc->name) + "\"";
    json += ",\"vendor\":\"" + json_escape(desc->vendor) + "\"";
    json += ",\"version\":\"" + json_escape(desc->version) + "\"";
    json += ",\"description\":\"" + json_escape(desc->description) + "\"";
    json += ",\"url\":\"" + json_escape(desc->url) + "\"";
    json += ",\"features\":\"" + json_escape(features.c_str()) + "\"";
    json += ",\"bundle\":\"" + json_escape(path.c_str()) + "\"";
    json += "}";
  }
  module_release(path);
}

// Recursively enumerates `.clap` entries under `dir` (the CLAP spec requires
// recursion; u-he nests its bundle in a `u-he/` subdirectory). Each entry may
// be a plugin file (or a symlink to one) or a bundle directory.
extern "C" char* clap_scan(const char* dir) {
  std::string json = "[";
  bool first = true;
  std::error_code ec;
  if (dir) {
    std::filesystem::recursive_directory_iterator it(
        dir, std::filesystem::directory_options::skip_permission_denied, ec);
    const std::filesystem::recursive_directory_iterator end;
    for (; it != end; it.increment(ec)) {
      if (ec) {
        ec.clear();
        continue;
      }
      const auto& path = it->path();
      if (path.extension() != ".clap") continue;
      // Do not descend into the matched bundle.
      it.disable_recursion_pending();
      std::string file;
      std::error_code stat_error;
      if (std::filesystem::is_regular_file(path, stat_error)) {
        file = path.string();
      } else if (std::filesystem::is_directory(path, stat_error)) {
        std::error_code inner_error;
        for (const auto& inner : std::filesystem::directory_iterator(path, inner_error)) {
          if (std::filesystem::is_regular_file(inner.path())) {
            file = inner.path().string();
            break;
          }
        }
      } else {
        continue;
      }
      if (file.empty()) continue;
      scan_one_file(file, json, first);
    }
  }
  json += "]";
  char* out = static_cast<char*>(std::malloc(json.size() + 1));
  if (!out) return nullptr;
  std::memcpy(out, json.c_str(), json.size() + 1);
  return out;
}

extern "C" void clap_string_free(char* text) {
  if (text) std::free(text);
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

static void collect_ports(Hosted* h) {
  h->in_ports.clear();
  h->out_ports.clear();
  if (!h->audio_ports) return;
  const bool have_count = h->audio_ports->count != nullptr;
  const bool have_get = h->audio_ports->get != nullptr;
  if (!have_count || !have_get) return;
  for (int is_input = 1; is_input >= 0; --is_input) {
    const bool input = is_input == 1;
    const uint32_t count = h->audio_ports->count(h->plugin, input);
    for (uint32_t i = 0; i < count; ++i) {
      clap_audio_port_info_t info{};
      if (!h->audio_ports->get(h->plugin, i, input, &info)) continue;
      Port port;
      port.is_input = input;
      port.is_main = (info.flags & CLAP_AUDIO_PORT_IS_MAIN) != 0;
      port.channels = info.channel_count;
      port.id = info.id;
      port.ptrs.assign(port.channels, nullptr);
      if (input) {
        h->in_ports.push_back(std::move(port));
      } else {
        h->out_ports.push_back(std::move(port));
      }
    }
  }
  // Main port is normally index 0; fall back to the first port.
  h->main_in_index = 0;
  for (uint32_t i = 0; i < h->in_ports.size(); ++i) {
    if (h->in_ports[i].is_main) {
      h->main_in_index = i;
      break;
    }
  }
  h->main_out_index = 0;
  for (uint32_t i = 0; i < h->out_ports.size(); ++i) {
    if (h->out_ports[i].is_main) {
      h->main_out_index = i;
      break;
    }
  }
  // First non-main input port is the sidechain the mixer can drive.
  h->sidechain_port = -1;
  for (uint32_t i = 0; i < h->in_ports.size(); ++i) {
    if (i != h->main_in_index && h->in_ports[i].channels > 0) {
      h->sidechain_port = static_cast<int>(i);
      break;
    }
  }
}

static void collect_params(Hosted* h) {
  h->param_ids.clear();
  h->param_count = 0;
  if (!h->params || !h->params->count || !h->params->get_info) return;
  h->param_count = h->params->count(h->plugin);
  h->param_ids.reserve(h->param_count);
  for (uint32_t i = 0; i < h->param_count; ++i) {
    clap_param_info_t info{};
    if (!h->params->get_info(h->plugin, i, &info)) continue;
    h->param_ids.push_back(info.id);
  }
}

extern "C" void* clap_open(const char* bundle, const char* plugin_id, double sample_rate,
                int min_frames, int max_frames) {
  if (!bundle || !plugin_id) return nullptr;
  const clap_plugin_entry_t* entry = module_acquire(bundle);
  if (!entry) return nullptr;
  const auto* factory = static_cast<const clap_plugin_factory_t*>(
      entry->get_factory ? entry->get_factory(CLAP_PLUGIN_FACTORY_ID) : nullptr);
  if (!factory || !factory->create_plugin) {
    module_release(bundle);
    return nullptr;
  }
  Hosted* h = new Hosted();
  h->path = bundle;
  h->host.clap_version = CLAP_VERSION;
  h->host.host_data = h;
  h->host.name = VIBEMIX_HOST_NAME;
  h->host.vendor = VIBEMIX_HOST_VENDOR;
  h->host.url = VIBEMIX_HOST_URL;
  h->host.version = VIBEMIX_HOST_VERSION;
  h->host.get_extension = host_get_extension;
  h->host.request_restart = host_request_restart;
  h->host.request_process = host_request_process;
  h->host.request_callback = host_request_callback;

  h->plugin = factory->create_plugin(factory, &h->host, plugin_id);
  if (!h->plugin || !h->plugin->init || !h->plugin->init(h->plugin)) {
    if (h->plugin && h->plugin->destroy) h->plugin->destroy(h->plugin);
    delete h;
    module_release(bundle);
    return nullptr;
  }

  h->params = static_cast<const clap_plugin_params_t*>(
      h->plugin->get_extension(h->plugin, CLAP_EXT_PARAMS));
  h->state = static_cast<const clap_plugin_state_t*>(
      h->plugin->get_extension(h->plugin, CLAP_EXT_STATE));
  h->latency = static_cast<const clap_plugin_latency_t*>(
      h->plugin->get_extension(h->plugin, CLAP_EXT_LATENCY));
  h->gui = static_cast<const clap_plugin_gui_t*>(
      h->plugin->get_extension(h->plugin, CLAP_EXT_GUI));
  h->audio_ports = static_cast<const clap_plugin_audio_ports_t*>(
      h->plugin->get_extension(h->plugin, CLAP_EXT_AUDIO_PORTS));
  h->plugin_timer = static_cast<const clap_plugin_timer_support_t*>(
      h->plugin->get_extension(h->plugin, CLAP_EXT_TIMER_SUPPORT));

  collect_ports(h);
  collect_params(h);

  const uint32_t min = min_frames > 0 ? static_cast<uint32_t>(min_frames) : 1;
  const uint32_t max = max_frames > 0 ? static_cast<uint32_t>(max_frames) : 8192;
  if (!h->plugin->activate || !h->plugin->activate(h->plugin, sample_rate, min, max)) {
    h->plugin->destroy(h->plugin);
    delete h;
    module_release(bundle);
    return nullptr;
  }
  // CLAP requires `start_processing` on the audio thread; defer it to the first
  // `clap_run`.
  h->want_start_processing = true;
  return h;
}

extern "C" void clap_close(void* handle) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h) return;
  if (h->plugin) {
    if (h->processing) {
      // The graph is retired from the control thread, but the plugin expects
      // `stop_processing` on the audio thread; report it for the duration of
      // the call so the plugin's thread check passes.
      tls_audio_thread = true;
      if (h->plugin->stop_processing) h->plugin->stop_processing(h->plugin);
      tls_audio_thread = false;
      h->processing = false;
    }
    if (h->plugin->deactivate) h->plugin->deactivate(h->plugin);
    if (h->plugin->destroy) h->plugin->destroy(h->plugin);
  }
  module_release(h->path);
  delete h;
}

// Main input/output channel counts (0 when absent).
extern "C" void clap_channel_counts(void* handle, int* in_channels, int* out_channels) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (in_channels) {
    *in_channels = h && h->main_in_index < h->in_ports.size()
                       ? static_cast<int>(h->in_ports[h->main_in_index].channels)
                       : 0;
  }
  if (out_channels) {
    *out_channels = h && h->main_out_index < h->out_ports.size()
                        ? static_cast<int>(h->out_ports[h->main_out_index].channels)
                        : 0;
  }
}

extern "C" int clap_has_sidechain(void* handle) {
  Hosted* h = static_cast<Hosted*>(handle);
  return h && h->sidechain_port >= 0 ? 1 : 0;
}

extern "C" void clap_connect_input(void* handle, int channel, float* data) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || h->main_in_index >= h->in_ports.size()) return;
  Port& port = h->in_ports[h->main_in_index];
  if (channel >= 0 && static_cast<size_t>(channel) < port.ptrs.size())
    port.ptrs[channel] = data;
}

extern "C" void clap_connect_output(void* handle, int channel, float* data) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || h->main_out_index >= h->out_ports.size()) return;
  Port& port = h->out_ports[h->main_out_index];
  if (channel >= 0 && static_cast<size_t>(channel) < port.ptrs.size())
    port.ptrs[channel] = data;
}

extern "C" void clap_write_sidechain(void* handle, const float* left, const float* right, int frames) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || h->sidechain_port < 0 || frames <= 0) return;
  Port& port = h->in_ports[h->sidechain_port];
  if (port.channels == 0) return;
  if (port.storage.size() < static_cast<size_t>(port.channels) * frames)
    port.storage.resize(static_cast<size_t>(port.channels) * frames, 0.0f);
  for (uint32_t ch = 0; ch < port.channels; ++ch) {
    float* dst = port.storage.data() + ch * frames;
    const float* src = ch == 0 ? left : (ch == 1 ? right : nullptr);
    if (src) {
      std::memcpy(dst, src, sizeof(float) * frames);
    } else {
      std::memset(dst, 0, sizeof(float) * frames);
    }
    port.ptrs[ch] = dst;
  }
}

// ---------------------------------------------------------------------------
// Processing
// ---------------------------------------------------------------------------

static void apply_output_events(Hosted* h, const OutEvents& out) {
  std::lock_guard<std::mutex> lock(h->edit_mutex);
  for (const auto& bytes : out.events) {
    if (bytes.size() < sizeof(clap_event_header_t)) continue;
    const auto* header = reinterpret_cast<const clap_event_header_t*>(bytes.data());
    if (header->type != CLAP_EVENT_PARAM_VALUE) continue;
    const auto* ev = reinterpret_cast<const clap_event_param_value_t*>(bytes.data());
    for (uint32_t i = 0; i < h->param_ids.size(); ++i) {
      if (h->param_ids[i] == ev->param_id) {
        h->param_edits.push_back(ParamEdit{i, static_cast<float>(ev->value)});
        break;
      }
    }
  }
}

extern "C" void clap_run(void* handle, int frames) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !h->plugin || !h->plugin->process || frames <= 0) return;

  std::vector<std::vector<uint8_t>> storage;
  InEvents in_ctx;
  OutEvents out_ctx;

  // Host parameter changes queued by the control thread.
  {
    std::unique_lock<std::mutex> lock(h->pending_mutex, std::try_to_lock);
    if (lock.owns_lock()) {
      for (const auto& ev : h->pending_params) {
        auto* bytes = reinterpret_cast<const uint8_t*>(static_cast<const void*>(&ev));
        storage.emplace_back(bytes, bytes + ev.header.size);
      }
      h->pending_params.clear();
    }
  }
  // MIDI events for this block.
  for (const auto& ev : h->midi) {
    auto* bytes = reinterpret_cast<const uint8_t*>(static_cast<const void*>(&ev));
    storage.emplace_back(bytes, bytes + ev.header.size);
  }
  h->midi.clear();
  // Transport.
  if (h->transport_valid) {
    auto* bytes = reinterpret_cast<const uint8_t*>(static_cast<const void*>(&h->transport));
    storage.emplace_back(bytes, bytes + h->transport.header.size);
  }

  in_ctx.events.reserve(storage.size());
  for (const auto& bytes : storage)
    in_ctx.events.push_back(reinterpret_cast<const clap_event_header_t*>(bytes.data()));
  clap_input_events_t in_list{&in_ctx, in_events_size, in_events_get};
  clap_output_events_t out_list{&out_ctx, out_events_push};

  std::vector<clap_audio_buffer_t> in_buffers(h->in_ports.size());
  for (size_t i = 0; i < h->in_ports.size(); ++i) {
    in_buffers[i].data32 = h->in_ports[i].ptrs.empty() ? nullptr : h->in_ports[i].ptrs.data();
    in_buffers[i].data64 = nullptr;
    in_buffers[i].channel_count = h->in_ports[i].channels;
    in_buffers[i].latency = 0;
    in_buffers[i].constant_mask = 0;
  }
  std::vector<clap_audio_buffer_t> out_buffers(h->out_ports.size());
  for (size_t i = 0; i < h->out_ports.size(); ++i) {
    out_buffers[i].data32 = h->out_ports[i].ptrs.empty() ? nullptr : h->out_ports[i].ptrs.data();
    out_buffers[i].data64 = nullptr;
    out_buffers[i].channel_count = h->out_ports[i].channels;
    out_buffers[i].latency = 0;
    out_buffers[i].constant_mask = 0;
  }

  clap_process_t proc{};
  proc.steady_time = -1;
  proc.frames_count = static_cast<uint32_t>(frames);
  proc.transport = h->transport_valid ? &h->transport : nullptr;
  proc.audio_inputs = in_buffers.data();
  proc.audio_inputs_count = static_cast<uint32_t>(in_buffers.size());
  proc.audio_outputs = out_buffers.data();
  proc.audio_outputs_count = static_cast<uint32_t>(out_buffers.size());
  proc.in_events = &in_list;
  proc.out_events = &out_list;

  tls_audio_thread = true;
  if (h->want_start_processing) {
    h->want_start_processing = false;
    if (h->plugin->start_processing) h->plugin->start_processing(h->plugin);
    h->processing = true;
  }
  h->plugin->process(h->plugin, &proc);
  tls_audio_thread = false;

  apply_output_events(h, out_ctx);
}

extern "C" void clap_midi_event(void* handle, int32_t sample_offset, int32_t status, int32_t d1, int32_t d2) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h) return;
  clap_event_midi_t ev{};
  ev.header.size = sizeof(ev);
  ev.header.time = static_cast<uint32_t>(sample_offset < 0 ? 0 : sample_offset);
  ev.header.space_id = CLAP_CORE_EVENT_SPACE_ID;
  ev.header.type = CLAP_EVENT_MIDI;
  ev.header.flags = 0;
  ev.port_index = 0;
  ev.data[0] = static_cast<uint8_t>(status);
  ev.data[1] = static_cast<uint8_t>(d1);
  ev.data[2] = static_cast<uint8_t>(d2);
  h->midi.push_back(ev);
}

extern "C" void clap_set_position(void* handle, double seconds, double beats, double bpm, bool playing,
                       int32_t bar_number, uint16_t tsig_num, uint16_t tsig_denom) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h) return;
  h->transport = clap_event_transport_t{};
  h->transport.header.size = sizeof(h->transport);
  h->transport.header.space_id = CLAP_CORE_EVENT_SPACE_ID;
  h->transport.header.type = CLAP_EVENT_TRANSPORT;
  h->transport.flags = CLAP_TRANSPORT_HAS_SECONDS_TIMELINE | CLAP_TRANSPORT_HAS_BEATS_TIMELINE |
                       CLAP_TRANSPORT_HAS_TEMPO | CLAP_TRANSPORT_HAS_TIME_SIGNATURE;
  if (playing) h->transport.flags |= CLAP_TRANSPORT_IS_PLAYING;
  h->transport.song_pos_seconds = static_cast<clap_sectime>(seconds * CLAP_SECTIME_FACTOR);
  h->transport.song_pos_beats = static_cast<clap_beattime>(beats * CLAP_BEATTIME_FACTOR);
  h->transport.tempo = bpm > 0.0 ? bpm : 120.0;
  h->transport.tempo_inc = 0.0;
  h->transport.bar_number = bar_number;
  h->transport.tsig_num = tsig_num ? tsig_num : 4;
  h->transport.tsig_denom = tsig_denom ? tsig_denom : 4;
  h->transport_valid = true;
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

extern "C" int clap_param_count(void* handle) {
  Hosted* h = static_cast<Hosted*>(handle);
  return h ? static_cast<int>(h->param_ids.size()) : 0;
}

extern "C" int clap_param_info(void* handle, int index, char* name, int name_cap, char* module, int module_cap,
                    float* def, int* flags) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !h->params || !h->params->get_info || index < 0 ||
      static_cast<size_t>(index) >= h->param_ids.size())
    return 0;
  clap_param_info_t info{};
  if (!h->params->get_info(h->plugin, static_cast<uint32_t>(index), &info)) return 0;
  if (name && name_cap > 0) {
    std::strncpy(name, info.name, name_cap - 1);
    name[name_cap - 1] = '\0';
  }
  if (module && module_cap > 0) {
    std::strncpy(module, info.module, module_cap - 1);
    module[module_cap - 1] = '\0';
  }
  if (def) *def = static_cast<float>(info.default_value);
  if (flags) *flags = static_cast<int>(info.flags);
  return 1;
}

extern "C" void clap_set_param(void* handle, int index, float value) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || index < 0 || static_cast<size_t>(index) >= h->param_ids.size()) return;
  clap_event_param_value_t ev{};
  ev.header.size = sizeof(ev);
  ev.header.time = 0;
  ev.header.space_id = CLAP_CORE_EVENT_SPACE_ID;
  ev.header.type = CLAP_EVENT_PARAM_VALUE;
  ev.param_id = h->param_ids[static_cast<size_t>(index)];
  ev.cookie = nullptr;
  ev.note_id = -1;
  ev.port_index = -1;
  ev.channel = -1;
  ev.key = -1;
  ev.value = value;
  std::lock_guard<std::mutex> lock(h->pending_mutex);
  h->pending_params.push_back(ev);
}

extern "C" float clap_get_param(void* handle, int index) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !h->params || !h->params->get_value || index < 0 ||
      static_cast<size_t>(index) >= h->param_ids.size())
    return 0.0f;
  double value = 0.0;
  if (!h->params->get_value(h->plugin, h->param_ids[static_cast<size_t>(index)], &value))
    return 0.0f;
  return static_cast<float>(value);
}

extern "C" int clap_param_value_to_text(void* handle, int index, float value, char* out, int cap) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !h->params || !h->params->value_to_text || index < 0 ||
      static_cast<size_t>(index) >= h->param_ids.size() || !out || cap <= 0)
    return 0;
  return h->params->value_to_text(h->plugin, h->param_ids[static_cast<size_t>(index)], value, out,
                                  static_cast<uint32_t>(cap))
             ? 1
             : 0;
}

/// Pops a parameter edit made in the plugin's own UI. Returns 1 and writes the
/// parameter index and value when one is available.
extern "C" int clap_take_param_edit(void* handle, int* index, float* value) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h) return 0;
  std::lock_guard<std::mutex> lock(h->edit_mutex);
  if (h->param_edits.empty()) return 0;
  ParamEdit edit = h->param_edits.front();
  h->param_edits.erase(h->param_edits.begin());
  if (index) *index = static_cast<int>(edit.index);
  if (value) *value = edit.value;
  return 1;
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct OutStream {
  std::vector<uint8_t> bytes;
};
struct InStream {
  const uint8_t* data;
  uint64_t size;
  uint64_t pos;
};

static int64_t out_write(const clap_ostream_t* stream, const void* buffer, uint64_t size) {
  auto* ctx = static_cast<OutStream*>(stream->ctx);
  const auto* src = static_cast<const uint8_t*>(buffer);
  ctx->bytes.insert(ctx->bytes.end(), src, src + size);
  return static_cast<int64_t>(size);
}

static int64_t in_read(const clap_istream_t* stream, void* buffer, uint64_t size) {
  auto* ctx = static_cast<InStream*>(stream->ctx);
  const uint64_t remaining = ctx->size - ctx->pos;
  const uint64_t take = size < remaining ? size : remaining;
  std::memcpy(buffer, ctx->data + ctx->pos, take);
  ctx->pos += take;
  return static_cast<int64_t>(take);
}

extern "C" int clap_save_state(void* handle, char* out, int cap) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !h->state || !h->state->save) return 0;
  OutStream stream;
  clap_ostream_t ostream{&stream, out_write};
  if (!h->state->save(h->plugin, &ostream)) return 0;
  const int size = static_cast<int>(stream.bytes.size());
  if (out && cap >= size) std::memcpy(out, stream.bytes.data(), size);
  return size;
}

extern "C" void clap_load_state(void* handle, const char* data, int len) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !h->state || !h->state->load || !data || len <= 0) return;
  InStream stream{reinterpret_cast<const uint8_t*>(data), static_cast<uint64_t>(len), 0};
  clap_istream_t istream{&stream, in_read};
  h->state->load(h->plugin, &istream);
}

// ---------------------------------------------------------------------------
// Misc info
// ---------------------------------------------------------------------------

extern "C" int clap_latency(void* handle) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !h->latency || !h->latency->get) return 0;
  return static_cast<int>(h->latency->get(h->plugin));
}

extern "C" int clap_bus_info(void* handle, char* out, int cap) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !out || cap <= 0) return 0;
  std::string text;
  for (size_t i = 0; i < h->in_ports.size(); ++i) {
    text += " in" + std::to_string(i) + "=" + std::to_string(h->in_ports[i].channels) +
            (h->in_ports[i].is_main ? "m" : "a");
  }
  for (size_t i = 0; i < h->out_ports.size(); ++i) {
    text += " out" + std::to_string(i) + "=" + std::to_string(h->out_ports[i].channels) +
            (h->out_ports[i].is_main ? "m" : "a");
  }
  std::snprintf(out, cap, "%s", text.c_str());
  return 1;
}

extern "C" int clap_editor_info(void* handle, char* out, int cap) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !out || cap <= 0) return 0;
  const char* info = "none";
  if (h->gui) info = "gui";
  std::snprintf(out, cap, "%s", info);
  return 1;
}

// ---------------------------------------------------------------------------
// Main-thread polling (callbacks, timers, pending flush)
// ---------------------------------------------------------------------------

static int64_t now_ms() {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1000LL + ts.tv_nsec / 1000000LL;
}

extern "C" void clap_poll(void* handle) {
  Hosted* h = static_cast<Hosted*>(handle);
  if (!h || !h->plugin) return;
  const bool callback = h->want_callback.exchange(false);
  h->want_process.exchange(false);
  h->want_restart.exchange(false);
  if (callback && h->plugin->on_main_thread) h->plugin->on_main_thread(h->plugin);
  if (h->plugin_timer && !h->timers.empty()) {
    const int64_t now = now_ms();
    for (auto& timer : h->timers) {
      if (now - timer.last_ms >= static_cast<int64_t>(timer.period_ms)) {
        timer.last_ms = now;
        h->plugin_timer->on_timer(h->plugin, timer.id);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Editor window (X11), mirroring the VST3 shim
// ---------------------------------------------------------------------------

extern "C" void* clap_view_open(void* instance, const char* title) {
  Hosted* h = static_cast<Hosted*>(instance);
  if (!h || !h->gui || !h->gui->create || !h->gui->set_parent || !h->gui->show) return nullptr;
  if (h->gui_created) return nullptr;

  Display* dpy = XOpenDisplay(nullptr);
  if (!dpy) return nullptr;
  const int screen = DefaultScreen(dpy);
  Window win = XCreateSimpleWindow(dpy, RootWindow(dpy, screen), 80, 80, 760, 520, 0,
                                   BlackPixel(dpy, screen), WhitePixel(dpy, screen));
  const char* wm_title = (title && *title) ? title : "vibemix plugin";
  XStoreName(dpy, win, wm_title);
  XClassHint hint;
  hint.res_name = const_cast<char*>(VIBEMIX_WM_CLASS);
  hint.res_class = const_cast<char*>(VIBEMIX_WM_CLASS);
  XSetClassHint(dpy, win, &hint);
  h->wm_delete = XInternAtom(dpy, "WM_DELETE_WINDOW", False);
  XSetWMProtocols(dpy, win, &h->wm_delete, 1);
  XMapWindow(dpy, win);

  if (!h->gui->create(h->plugin, CLAP_WINDOW_API_X11, false)) {
    XDestroyWindow(dpy, win);
    XCloseDisplay(dpy);
    return nullptr;
  }
  clap_window_t window{};
  window.api = CLAP_WINDOW_API_X11;
  window.x11 = static_cast<clap_xwnd>(win);
  if (!h->gui->set_parent(h->plugin, &window)) {
    h->gui->destroy(h->plugin);
    XDestroyWindow(dpy, win);
    XCloseDisplay(dpy);
    return nullptr;
  }
  // The host titles the embedded window itself (XStoreName above);
  // `suggest_title` is only valid for floating windows, and plugins that
  // enforce the CLAP contract abort if it is called on an embedded one.
  if (h->gui->show) h->gui->show(h->plugin);
  if (std::getenv("VIBEMIX_CLAP_DEBUG")) {
    XWindowAttributes attrs {};
    XSync(dpy, False);
    XGetWindowAttributes(dpy, win, &attrs);
    std::fprintf(stderr, "[clap] view win=0x%lx map_state=%d %dx%d\n", (unsigned long)win,
                 attrs.map_state, attrs.width, attrs.height);
  }
  h->dpy = dpy;
  h->win = win;
  h->gui_created = true;
  h->want_close = false;
  return h;
}

extern "C" void clap_view_pump(void* view) {
  Hosted* h = static_cast<Hosted*>(view);
  if (!h || !h->gui_created || !h->dpy) return;
  while (XPending(h->dpy)) {
    XEvent event;
    XNextEvent(h->dpy, &event);
    if (event.type == ClientMessage &&
        static_cast<Atom>(event.xclient.data.l[0]) == h->wm_delete) {
      h->want_close = true;
    }
  }
  // The plugin may have asked the host to resize its window.
  if (h->gui && h->gui->get_size) {
    uint32_t width = 0;
    uint32_t height = 0;
    if (h->gui->get_size(h->plugin, &width, &height) && width > 0 && height > 0) {
      XResizeWindow(h->dpy, h->win, width, height);
    }
  }
}

extern "C" int clap_view_wants_close(void* view) {
  Hosted* h = static_cast<Hosted*>(view);
  return h && (h->want_close || !h->gui_created) ? 1 : 0;
}

extern "C" void clap_view_close(void* view) {
  Hosted* h = static_cast<Hosted*>(view);
  if (!h) return;
  if (h->gui_created && h->gui) {
    if (h->gui->hide) h->gui->hide(h->plugin);
    if (h->gui->destroy) h->gui->destroy(h->plugin);
  }
  if (h->dpy && h->win) XDestroyWindow(h->dpy, h->win);
  if (h->dpy) XCloseDisplay(h->dpy);
  h->dpy = nullptr;
  h->win = 0;
  h->gui_created = false;
}
