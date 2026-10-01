#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <dlfcn.h>
#include <GL/glx.h>
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <lilv/lilv.h>
#include <suil/suil.h>

/* WM_CLASS applied to every engine-hosted plugin window so desktop
   environments group them under the vibemix application (matching the
   StartupWMClass in packaging/vibemix.desktop). */
#define VIBEMIX_WM_CLASS "vibemix-gui"

/* GTK is an optional runtime dependency: it is dlopen()ed on demand and the
   engine still runs when it (or its plugin UIs) are absent. GTK2 and GTK3
   cannot be initialized in the same process, so exactly one major version is
   chosen (VIBEMIX_GTK=gtk2|gtk3 overrides the automatic choice). */
#define VIBEMIX_GTK_RESIDENT_FEATURE "http://lv2plug.in/ns/extensions/ui#makeResident"

#include <lv2/atom/atom.h>
#include <lv2/data-access/data-access.h>
#include <lv2/instance-access/instance-access.h>
#include <lv2/options/options.h>
#include <lv2/ui/ui.h>
#include <lv2/urid/urid.h>

typedef void (*VibemixPortWrite)(void* user, uint32_t port_index, uint32_t size,
                                 uint32_t format, const void* buffer);

typedef struct {
  void* handle;
  uint32_t (*map)(void* handle, const char* uri);
} VibemixUridMap;

typedef struct {
  int kind; /* 0 = X11, 2 = GTK2, 3 = GTK3 */
  Display* dpy;
  Window win;
  Atom wm_delete;
  void* gtk_window;
  void* gtk_box;
  unsigned long delete_signal;
  SuilHost* host;
  SuilInstance* inst;
  VibemixPortWrite write_cb;
  void* write_user;
  int close_requested;
  int shown;
  LV2UI_Resize* resize;
  float update_rate;
  float scale_factor;
  LV2_Options_Option options[3];
  LV2_Feature options_feature;
} VibemixUi;

/* GTK entry points resolved from the dlopen()ed libraries. */
typedef struct {
  void* gtk;
  void* gobject;
  int (*init_check)(int* argc, char*** argv);
  void* (*window_new)(int type);
  void (*window_set_title)(void* window, const char* title);
  void (*window_set_wmclass)(void* window, const char* name, const char* klass);
  void (*window_resize)(void* window, int width, int height);
  void (*container_add)(void* container, void* widget);
  void* (*vbox_new)(int homogeneous, int spacing);
  void* (*box_new)(int orientation, int spacing);
  void (*widget_show_all)(void* widget);
  void (*widget_hide)(void* widget);
  void (*widget_destroy)(void* widget);
  void* (*widget_get_parent)(void* widget);
  int (*events_pending)(void);
  int (*main_iteration_do)(int blocking);
  unsigned long (*signal_connect_data)(void* instance, const char* signal,
                                       void* handler, void* data, void* destroy,
                                       int flags);
  void (*signal_handler_disconnect)(void* instance, unsigned long handler_id);
} VibemixGtk;

static VibemixGtk g_gtk;
static int g_gtk_loaded = 0;   /* symbols resolved */
static int g_gtk_inited = 0;   /* gtk_init_check() has run */
static int g_gtk_version = -1; /* -1 = undecided, 0 = none, 2, 3 */

static LilvWorld* g_world = NULL;

static pthread_once_t g_x_once = PTHREAD_ONCE_INIT;

static void vibemix_x_init_once(void) {
  /* Xlib requires XInitThreads() to be the first Xlib call in the process.
     Plugin UIs (notably LSP's X11/GL toolkit) use Xlib from multiple threads
     and call XInitThreads() themselves, but by then the engine has already
     opened a display, so their call has no effect and the UI can freeze. */
  XInitThreads();
}

void vibemix_ui_init(void) {
  pthread_once(&g_x_once, vibemix_x_init_once);
}

static const void* data_access_fn(const char* uri) {
  (void)uri;
  return NULL;
}
static LV2_Extension_Data_Feature g_data = {data_access_fn};

static LilvWorld* ensure_world(void) {
  if (!g_world) {
    g_world = lilv_world_new();
    lilv_world_load_all(g_world);
  }
  return g_world;
}

/* Number of plugins that expose at least one UI of `type_uri`. */
static int count_uis_of_type(const char* type_uri) {
  LilvWorld* world = ensure_world();
  const LilvPlugins* plugins = lilv_world_get_all_plugins(world);
  int count = 0;
  LILV_FOREACH(plugins, pit, plugins) {
    const LilvPlugin* plugin = lilv_plugins_get(plugins, pit);
    LilvUIs* uis = lilv_plugin_get_uis(plugin);
    int hit = 0;
    LILV_FOREACH(uis, it, uis) {
      const LilvUI* ui = lilv_uis_get(uis, it);
      const LilvNodes* classes = lilv_ui_get_classes(ui);
      LILV_FOREACH(nodes, cit, classes) {
        const char* cls = lilv_node_as_uri(lilv_nodes_get(classes, cit));
        if (!strcmp(cls, type_uri)) {
          hit = 1;
          break;
        }
      }
      if (hit) {
        break;
      }
    }
    lilv_uis_free(uis);
    if (hit) {
      count++;
    }
  }
  return count;
}

static int load_symbol(void* lib, const char* name, void* out) {
  void* sym = dlsym(lib, name);
  if (!sym) {
    return 0;
  }
  memcpy(out, &sym, sizeof(sym));
  return 1;
}

#define GTK_REQUIRE(field, name)                             \
  do {                                                       \
    if (!load_symbol(g_gtk.gtk, (name), &g_gtk.field)) {     \
      fprintf(stderr, "[vibemix-ui] missing GTK symbol %s\n", (name)); \
      return 0;                                              \
    }                                                        \
  } while (0)

static int gtk_load(int version) {
  memset(&g_gtk, 0, sizeof(g_gtk));
  const char* gtk_name = (version == 2) ? "libgtk-x11-2.0.so.0" : "libgtk-3.so.0";
  g_gtk.gtk = dlopen(gtk_name, RTLD_NOW | RTLD_GLOBAL);
  if (!g_gtk.gtk) {
    return 0;
  }
  g_gtk.gobject = dlopen("libgobject-2.0.so.0", RTLD_NOW | RTLD_GLOBAL);
  if (!g_gtk.gobject) {
    g_gtk.gobject = g_gtk.gtk;
  }

  GTK_REQUIRE(init_check, "gtk_init_check");
  GTK_REQUIRE(window_new, "gtk_window_new");
  GTK_REQUIRE(window_set_title, "gtk_window_set_title");
  GTK_REQUIRE(window_resize, "gtk_window_resize");
  GTK_REQUIRE(container_add, "gtk_container_add");
  GTK_REQUIRE(widget_show_all, "gtk_widget_show_all");
  GTK_REQUIRE(widget_destroy, "gtk_widget_destroy");
  GTK_REQUIRE(events_pending, "gtk_events_pending");
  GTK_REQUIRE(main_iteration_do, "gtk_main_iteration_do");
  if (version == 2) {
    GTK_REQUIRE(vbox_new, "gtk_vbox_new");
  } else {
    GTK_REQUIRE(box_new, "gtk_box_new");
  }
  /* Optional symbols. */
  load_symbol(g_gtk.gtk, "gtk_window_set_wmclass", &g_gtk.window_set_wmclass);
  load_symbol(g_gtk.gtk, "gtk_widget_get_parent", &g_gtk.widget_get_parent);
  load_symbol(g_gtk.gtk, "gtk_widget_hide", &g_gtk.widget_hide);
  if (!load_symbol(g_gtk.gobject, "g_signal_connect_data",
                   &g_gtk.signal_connect_data)) {
    fprintf(stderr, "[vibemix-ui] missing GObject symbol g_signal_connect_data\n");
    return 0;
  }
  load_symbol(g_gtk.gobject, "g_signal_handler_disconnect",
              &g_gtk.signal_handler_disconnect);

  g_gtk_loaded = 1;
  fprintf(stderr, "[vibemix-ui] hosting GTK%d plugin UIs\n", version);
  return 1;
}

/* Decide (once) which GTK major version to use, if any. This only scans the
   plugin metadata; it never loads GTK, so discovery stays toolkit-free. */
int vibemix_gtk_version(void) {
  if (g_gtk_version >= 0) {
    return g_gtk_version;
  }

  const char* env = getenv("VIBEMIX_GTK");
  if (env) {
    if (!strcmp(env, "2") || !strcmp(env, "gtk2")) {
      g_gtk_version = 2;
      return g_gtk_version;
    }
    if (!strcmp(env, "3") || !strcmp(env, "gtk3")) {
      g_gtk_version = 3;
      return g_gtk_version;
    }
    if (!strcmp(env, "0") || !strcmp(env, "none") || !strcmp(env, "off")) {
      g_gtk_version = 0;
      return g_gtk_version;
    }
  }

  int n2 = count_uis_of_type(LV2_UI__GtkUI);
  int n3 = count_uis_of_type(LV2_UI__Gtk3UI);
  if (n2 <= 0 && n3 <= 0) {
    g_gtk_version = 0;
  } else {
    /* Prefer the version with more hostable UIs; ties go to GTK2 (where the
       real-world plugins are today). */
    g_gtk_version = (n3 > n2) ? 3 : 2;
  }
  return g_gtk_version;
}

int vibemix_gtk_available(void) {
  int version = vibemix_gtk_version();
  if (version == 0) {
    return 0;
  }
  if (g_gtk_loaded) {
    return 1;
  }
  /* Load exactly one GTK major version for the life of the process (loading
     both is unsupported); if it fails, GTK UIs are disabled. */
  if (!gtk_load(version)) {
    fprintf(stderr, "[vibemix-ui] GTK%d unavailable; disabling GTK UIs\n", version);
    g_gtk_version = 0;
    return 0;
  }
  return 1;
}

static int ui_x_error(Display* dpy, XErrorEvent* event) {
  char buffer[256];
  XGetErrorText(dpy, event->error_code, buffer, sizeof(buffer));
  fprintf(stderr, "[vibemix-ui] X error: %s (request %d)\n", buffer,
          event->request_code);
  return 0;
}

static int ui_resize_cb(void* handle, int width, int height) {
  VibemixUi* ui = (VibemixUi*)handle;
  if (!ui) {
    return 0;
  }
  if (ui->kind == 0) {
    if (ui->dpy) {
      XResizeWindow(ui->dpy, ui->win, (unsigned)width, (unsigned)height);
    }
  } else if (ui->gtk_window && g_gtk.window_resize) {
    g_gtk.window_resize(ui->gtk_window, width, height);
  }
  return 0;
}

static int gtk_delete_event(void* widget, void* event, void* data) {
  (void)widget;
  (void)event;
  VibemixUi* ui = (VibemixUi*)data;
  if (ui) {
    ui->close_requested = 1;
    /* Hide right away so the window disappears while the control loop tears
       it down on its next tick. */
    if (g_gtk.widget_hide) {
      g_gtk.widget_hide(widget);
    }
  }
  return 1; /* TRUE: keep the window; we destroy it on close. */
}

static void suil_write(SuilController controller, uint32_t port_index, uint32_t size,
                       uint32_t format, const void* buffer) {
  VibemixUi* ui = (VibemixUi*)controller;
  if (ui && ui->write_cb) {
    ui->write_cb(ui->write_user, port_index, size, format, buffer);
  }
}

static uint32_t suil_index(SuilController controller, const char* symbol) {
  (void)controller;
  (void)symbol;
  return 0;
}

int vibemix_ui_supported(const char* host_type, const char* ui_type) {
  return (int)suil_ui_supported(host_type, ui_type);
}

/* Whether a UI declares `lv2:requiredFeature instance-access`. Such a UI reads
   the DSP instance directly and can only be hosted in the engine process. */
static int ui_requires_instance_access(LilvWorld* world, const LilvUI* ui) {
  static const char* REQUIRED = "http://lv2plug.in/ns/lv2core#requiredFeature";
  static const char* INSTANCE = "http://lv2plug.in/ns/ext/instance-access";
  LilvNode* predicate = lilv_new_uri(world, REQUIRED);
  LilvNode* wanted = lilv_new_uri(world, INSTANCE);
  LilvNodes* nodes = lilv_world_find_nodes(world, lilv_ui_get_uri(ui), predicate, NULL);
  int requires = 0;
  if (nodes) {
    LILV_FOREACH(nodes, nit, nodes) {
      if (lilv_node_equals(lilv_nodes_get(nodes, nit), wanted)) {
        requires = 1;
        break;
      }
    }
    lilv_nodes_free(nodes);
  }
  lilv_node_free(predicate);
  lilv_node_free(wanted);
  return requires;
}

/* Finds a hostable UI of `want_type`. Prefers a UI that does not require
   `instance-access` (so it can be hosted out of process); only falls back to an
   instance-access UI when it is the only option. Reports which kind was chosen
   via `requires_instance_access`. */
int vibemix_ui_find(const char* plugin_uri, const char* want_type, char* ui_uri,
                    int ui_uri_cap, char* bundle, int bundle_cap, char* binary,
                    int binary_cap, int* requires_instance_access) {
  LilvWorld* world = ensure_world();
  if (requires_instance_access) {
    *requires_instance_access = 0;
  }

  LilvNode* pnode = lilv_new_uri(world, plugin_uri);
  const LilvPlugin* plugin =
      lilv_plugins_get_by_uri(lilv_world_get_all_plugins(world), pnode);
  lilv_node_free(pnode);
  if (!plugin) {
    return 0;
  }

  int found = 0;
  int found_requires = 0;
  LilvUIs* uis = lilv_plugin_get_uis(plugin);
  LILV_FOREACH(uis, it, uis) {
    const LilvUI* ui = lilv_uis_get(uis, it);
    int supported = 0;
    const LilvNodes* classes = lilv_ui_get_classes(ui);
    LILV_FOREACH(nodes, cit, classes) {
      const char* cls = lilv_node_as_uri(lilv_nodes_get(classes, cit));
      if (suil_ui_supported(want_type, cls)) {
        supported = 1;
        break;
      }
    }
    if (!supported) {
      continue;
    }
    char* bundle_path =
        lilv_file_uri_parse(lilv_node_as_uri(lilv_ui_get_bundle_uri(ui)), NULL);
    char* binary_path =
        lilv_file_uri_parse(lilv_node_as_uri(lilv_ui_get_binary_uri(ui)), NULL);
    if (binary_path && access(binary_path, R_OK) == 0) {
      int requires = ui_requires_instance_access(world, ui);
      /* Keep the first candidate, but replace it if it needs instance-access
         and this one does not. */
      if (!found || (found_requires && !requires)) {
        snprintf(ui_uri, ui_uri_cap, "%s", lilv_node_as_uri(lilv_ui_get_uri(ui)));
        snprintf(bundle, bundle_cap, "%s", bundle_path ? bundle_path : "");
        snprintf(binary, binary_cap, "%s", binary_path);
        found = 1;
        found_requires = requires;
      }
    }
    lilv_free(bundle_path);
    lilv_free(binary_path);
  }
  lilv_uis_free(uis);
  if (found && requires_instance_access) {
    *requires_instance_access = found_requires;
  }
  return found;
}

/* Create the X11 host window and set its app identity. */
static int open_x11(VibemixUi* ui, int headless, const char* title) {
  XSetErrorHandler(ui_x_error);
  Display* dpy = XOpenDisplay(NULL);
  if (!dpy) {
    fprintf(stderr, "[vibemix-ui] cannot open X display\n");
    return 0;
  }
  ui->kind = 0;
  ui->dpy = dpy;

  int screen = DefaultScreen(dpy);
  ui->win = XCreateSimpleWindow(dpy, RootWindow(dpy, screen), 80, 80, 760, 520, 0,
                                BlackPixel(dpy, screen), WhitePixel(dpy, screen));
  XStoreName(dpy, ui->win, title);
  {
    Atom net_wm_name = XInternAtom(dpy, "_NET_WM_NAME", False);
    Atom utf8_string = XInternAtom(dpy, "UTF8_STRING", False);
    XChangeProperty(dpy, ui->win, net_wm_name, utf8_string, 8, PropModeReplace,
                    (const unsigned char*)title, (int)strlen(title));
  }
  {
    XClassHint class_hint;
    class_hint.res_name = (char*)VIBEMIX_WM_CLASS;
    class_hint.res_class = (char*)VIBEMIX_WM_CLASS;
    XSetClassHint(dpy, ui->win, &class_hint);
  }
  {
    Atom net_wm_pid = XInternAtom(dpy, "_NET_WM_PID", False);
    long pid = (long)getpid();
    XChangeProperty(dpy, ui->win, net_wm_pid, XA_CARDINAL, 32, PropModeReplace,
                    (unsigned char*)&pid, 1);
  }
  ui->wm_delete = XInternAtom(dpy, "WM_DELETE_WINDOW", False);
  XSetWMProtocols(dpy, ui->win, &ui->wm_delete, 1);
  if (!headless) {
    XMapWindow(dpy, ui->win);
  }
  return 1;
}

/* Create the GTK host window and container widget. */
static int open_gtk(VibemixUi* ui, const char* ui_type, int headless, const char* title) {
  if (!vibemix_gtk_available()) {
    return 0;
  }
  if (!g_gtk_inited) {
    if (!g_gtk.init_check(NULL, NULL)) {
      fprintf(stderr, "[vibemix-ui] gtk_init_check failed\n");
      return 0;
    }
    g_gtk_inited = 1;
  }

  int version = !strcmp(ui_type, LV2_UI__Gtk3UI) ? 3 : 2;
  ui->kind = version;
  ui->gtk_window = g_gtk.window_new(0 /* GTK_WINDOW_TOPLEVEL */);
  if (!ui->gtk_window) {
    return 0;
  }
  g_gtk.window_set_title(ui->gtk_window, title);
  if (g_gtk.window_set_wmclass) {
    g_gtk.window_set_wmclass(ui->gtk_window, VIBEMIX_WM_CLASS, VIBEMIX_WM_CLASS);
  }
  ui->gtk_box = (version == 3) ? g_gtk.box_new(1 /* VERTICAL */, 0)
                               : g_gtk.vbox_new(0, 0);
  if (!ui->gtk_box) {
    return 0;
  }
  g_gtk.container_add(ui->gtk_window, ui->gtk_box);
  ui->delete_signal = g_gtk.signal_connect_data(ui->gtk_window, "delete-event",
                                                (void*)gtk_delete_event, ui, NULL, 0);
  (void)headless;
  return 1;
}

void vibemix_ui_set_title(void* handle, const char* title) {
  VibemixUi* ui = (VibemixUi*)handle;
  if (!ui || !title || !*title) {
    return;
  }
  if (ui->kind == 0) {
    if (ui->dpy) {
      XStoreName(ui->dpy, ui->win, title);
      Atom net_wm_name = XInternAtom(ui->dpy, "_NET_WM_NAME", False);
      Atom utf8_string = XInternAtom(ui->dpy, "UTF8_STRING", False);
      XChangeProperty(ui->dpy, ui->win, net_wm_name, utf8_string, 8, PropModeReplace,
                      (const unsigned char*)title, (int)strlen(title));
    }
  } else if (ui->gtk_window) {
    g_gtk.window_set_title(ui->gtk_window, title);
  }
}

void* vibemix_ui_open(const char* plugin_uri, const char* ui_uri, const char* ui_type,
                      const char* bundle_path, const char* binary_path,
                      void* plugin_handle, void* urid_map, void* urid_unmap,
                      VibemixPortWrite write_cb, void* write_user, int headless,
                      const char* title) {
  vibemix_ui_init();

  int is_gtk = (!strcmp(ui_type, LV2_UI__GtkUI) || !strcmp(ui_type, LV2_UI__Gtk3UI));
  const char* window_title = (title && *title) ? title : "vibemix plugin UI";

  VibemixUi* ui = (VibemixUi*)calloc(1, sizeof(VibemixUi));
  ui->write_cb = write_cb;
  ui->write_user = write_user;
  ui->shown = 1;

  void* parent_data = NULL;
  if (is_gtk) {
    if (!open_gtk(ui, ui_type, headless, window_title)) {
      free(ui);
      return NULL;
    }
    parent_data = ui->gtk_box;
  } else {
    if (!open_x11(ui, headless, window_title)) {
      free(ui);
      return NULL;
    }
    parent_data = (void*)(uintptr_t)ui->win;
  }

  ui->resize = (LV2UI_Resize*)malloc(sizeof(LV2UI_Resize));
  ui->resize->handle = ui;
  ui->resize->ui_resize = ui_resize_cb;

  /* Advertise UI options (refresh rate and scale factor). Plugins such as LSP
     read ui:updateRate from the options feature to drive their idle timers. */
  ui->update_rate = 30.0f;
  ui->scale_factor = 1.0f;
  memset(ui->options, 0, sizeof(ui->options));
  if (urid_map) {
    VibemixUridMap* umap = (VibemixUridMap*)urid_map;
    uint32_t atom_float = umap->map(umap->handle, LV2_ATOM__Float);
    ui->options[0].context = LV2_OPTIONS_INSTANCE;
    ui->options[0].subject = 0;
    ui->options[0].key = umap->map(umap->handle, LV2_UI__updateRate);
    ui->options[0].size = sizeof(float);
    ui->options[0].type = atom_float;
    ui->options[0].value = &ui->update_rate;
    ui->options[1].context = LV2_OPTIONS_INSTANCE;
    ui->options[1].subject = 0;
    ui->options[1].key = umap->map(umap->handle, LV2_UI__scaleFactor);
    ui->options[1].size = sizeof(float);
    ui->options[1].type = atom_float;
    ui->options[1].value = &ui->scale_factor;
  }
  ui->options_feature.URI = LV2_OPTIONS__options;
  ui->options_feature.data = ui->options;

  int resident = 1;
  char* ui_title = NULL;
  LV2_Feature parent_feature = {LV2_UI__parent, parent_data};
  LV2_Feature instance_feature = {LV2_INSTANCE_ACCESS_URI, plugin_handle};
  LV2_Feature data_feature = {LV2_DATA_ACCESS_URI, &g_data};
  LV2_Feature map_feature = {LV2_URID__map, urid_map};
  LV2_Feature unmap_feature = {LV2_URID__unmap, urid_unmap};
  LV2_Feature resize_feature = {"http://lv2plug.in/ns/extensions/ui#resize", ui->resize};
  LV2_Feature show_feature = {"http://lv2plug.in/ns/extensions/ui#showInterface",
                              &ui->shown};
  LV2_Feature resident_feature = {VIBEMIX_GTK_RESIDENT_FEATURE, &resident};
  LV2_Feature title_feature = {LV2_UI__windowTitle, &ui_title};
  const LV2_Feature* features[] = {&parent_feature,   &instance_feature,
                                   &data_feature,     &map_feature,
                                   &unmap_feature,    &resize_feature,
                                   &show_feature,     &ui->options_feature,
                                   &resident_feature, &title_feature,
                                   NULL};

  ui->host = suil_host_new(suil_write, suil_index, NULL, NULL);

  /* Pin the UI module resident. Suil would otherwise dlclose it when the UI is
     freed, unloading its toolkit dependencies (GTK/GLib); reloading them on a
     later open re-registers GObject types ("cannot register existing type
     'PangoFontMap'") and can hang or crash the host, so keep the module loaded
     for the lifetime of the process.

     Keep it RTLD_LOCAL: the module is a plugin (for some, the DSP and the UI
     share one .so, e.g. the JUCE-based CHOW plugins). Loading it RTLD_GLOBAL
     would publish its symbols process-wide, so instantiating another plugin
     could bind to this module's copy of shared libraries (JUCE, libstdc++,
     ...) and crash. RTLD_NODELETE is what prevents the unload, not RTLD_GLOBAL. */
  if (binary_path && *binary_path) {
    if (!dlopen(binary_path, RTLD_NOW | RTLD_LOCAL | RTLD_NODELETE)) {
      fprintf(stderr, "[vibemix-ui] could not pin UI binary %s: %s\n", binary_path,
              dlerror());
    }
  }

  /* For GTK UIs the container type matches the UI type (native hosting). */
  const char* container_type = is_gtk ? ui_type : LV2_UI__X11UI;
  ui->inst = suil_instance_new(ui->host, (SuilController)ui, container_type, plugin_uri,
                               ui_uri, ui_type, bundle_path, binary_path, features);
  if (!ui->inst) {
    fprintf(stderr, "[vibemix-ui] failed to instantiate UI %s\n", ui_uri);
    suil_host_free(ui->host);
    if (ui->kind == 0 && ui->dpy) {
      XDestroyWindow(ui->dpy, ui->win);
      XCloseDisplay(ui->dpy);
    } else if (ui->gtk_window) {
      g_gtk.widget_destroy(ui->gtk_window);
    }
    free(ui->resize);
    free(ui);
    return NULL;
  }

  if (is_gtk) {
    void* widget = suil_instance_get_widget(ui->inst);
    if (widget && g_gtk.widget_get_parent && !g_gtk.widget_get_parent(widget)) {
      g_gtk.container_add(ui->gtk_box, widget);
    }
    if (!headless) {
      g_gtk.widget_show_all(ui->gtk_window);
    }
    for (int i = 0; i < 256 && g_gtk.events_pending(); ++i) {
      g_gtk.main_iteration_do(0);
    }
  }

  /* The UI may have proposed a window title via ui:windowTitle. */
  if (ui_title && *ui_title) {
    vibemix_ui_set_title(ui, ui_title);
  }

  return ui;
}

void vibemix_ui_pump(void* handle) {
  VibemixUi* ui = (VibemixUi*)handle;
  if (!ui || ui->close_requested) {
    return;
  }

  if (ui->kind == 0) {
    while (XPending(ui->dpy)) {
      XEvent event;
      XNextEvent(ui->dpy, &event);
      if (event.type == ClientMessage &&
          (Atom)event.xclient.data.l[0] == ui->wm_delete) {
        ui->close_requested = 1;
      }
    }
  } else if (g_gtk_loaded) {
    /* Bounded drain: a UI that continuously posts events must not be able to
       starve the control loop. */
    for (int i = 0; i < 256 && g_gtk.events_pending(); ++i) {
      g_gtk.main_iteration_do(0);
    }
  }

  const LV2UI_Idle_Interface* idle = (const LV2UI_Idle_Interface*)suil_instance_extension_data(
      ui->inst, "http://lv2plug.in/ns/extensions/ui#idleInterface");
  if (idle && idle->idle) {
    idle->idle(suil_instance_get_handle(ui->inst));
  }
}

void vibemix_ui_port_event(void* handle, uint32_t port_index, uint32_t size,
                           uint32_t format, const void* buffer) {
  VibemixUi* ui = (VibemixUi*)handle;
  if (!ui || !ui->inst || ui->close_requested) {
    return;
  }
  suil_instance_port_event(ui->inst, port_index, size, format, buffer);
}

int vibemix_ui_wants_close(void* handle) {
  VibemixUi* ui = (VibemixUi*)handle;
  return ui ? ui->close_requested : 1;
}

void vibemix_ui_close(void* handle) {
  VibemixUi* ui = (VibemixUi*)handle;
  if (!ui) {
    return;
  }

  if (ui->kind == 0) {
    if (ui->inst) {
      suil_instance_free(ui->inst);
      ui->inst = NULL;
    }
    if (ui->host) {
      suil_host_free(ui->host);
      ui->host = NULL;
    }
    if (ui->dpy) {
      XDestroyWindow(ui->dpy, ui->win);
      XCloseDisplay(ui->dpy);
      ui->dpy = NULL;
    }
  } else {
    /* Destroy the host window first: it destroys the plugin widget and the
       plugin's own "destroy" handler clears its internal pointer, so its
       cleanup (from suil_instance_free) has nothing dangling to touch. This
       matches the well-tested jalv ordering and avoids calling plugin cleanup
       while its widgets are still parented. */
    if (ui->gtk_window && g_gtk_loaded) {
      if (ui->delete_signal && g_gtk.signal_handler_disconnect) {
        g_gtk.signal_handler_disconnect(ui->gtk_window, ui->delete_signal);
        ui->delete_signal = 0;
      }
      g_gtk.widget_destroy(ui->gtk_window);
      ui->gtk_window = NULL;
    }
    if (ui->inst) {
      suil_instance_free(ui->inst);
      ui->inst = NULL;
    }
    if (ui->host) {
      suil_host_free(ui->host);
      ui->host = NULL;
    }
  }

  free(ui->resize);
  free(ui);
}
