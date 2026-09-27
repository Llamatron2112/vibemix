#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <dlfcn.h>
#include <GL/glx.h>
#include <X11/Xlib.h>
#include <lilv/lilv.h>
#include <suil/suil.h>

#include <lv2/data-access/data-access.h>
#include <lv2/instance-access/instance-access.h>
#include <lv2/ui/ui.h>
#include <lv2/urid/urid.h>

typedef void (*VibemixPortWrite)(void* user, uint32_t port_index, uint32_t size,
                                 uint32_t format, const void* buffer);

typedef struct {
  Display* dpy;
  Window win;
  Atom wm_delete;
  SuilHost* host;
  SuilInstance* inst;
  VibemixPortWrite write_cb;
  void* write_user;
  int close_requested;
  int shown;
  LV2UI_Resize* resize;
} VibemixUi;

static LilvWorld* g_world = NULL;

static const void* data_access_fn(const char* uri) {
  (void)uri;
  return NULL;
}
static LV2_Extension_Data_Feature g_data = {data_access_fn};

static int ui_x_error(Display* dpy, XErrorEvent* event) {
  char buffer[256];
  XGetErrorText(dpy, event->error_code, buffer, sizeof(buffer));
  fprintf(stderr, "[vibemix-ui] X error: %s (request %d)\n", buffer,
          event->request_code);
  return 0;
}

static int ui_resize_cb(void* handle, int width, int height) {
  VibemixUi* ui = (VibemixUi*)handle;
  if (ui && ui->dpy) {
    XResizeWindow(ui->dpy, ui->win, (unsigned)width, (unsigned)height);
  }
  return 0;
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

int vibemix_ui_find(const char* plugin_uri, const char* want_type, char* ui_uri,
                    int ui_uri_cap, char* bundle, int bundle_cap, char* binary,
                    int binary_cap) {
  if (!g_world) {
    g_world = lilv_world_new();
    lilv_world_load_all(g_world);
  }

  LilvNode* pnode = lilv_new_uri(g_world, plugin_uri);
  const LilvPlugin* plugin =
      lilv_plugins_get_by_uri(lilv_world_get_all_plugins(g_world), pnode);
  lilv_node_free(pnode);
  if (!plugin) {
    return 0;
  }

  int found = 0;
  LilvUIs* uis = lilv_plugin_get_uis(plugin);
  LILV_FOREACH(uis, it, uis) {
    const LilvUI* ui = lilv_uis_get(uis, it);
    const LilvNodes* classes = lilv_ui_get_classes(ui);
    LILV_FOREACH(nodes, cit, classes) {
      const char* cls = lilv_node_as_uri(lilv_nodes_get(classes, cit));
      if (!suil_ui_supported(want_type, cls)) {
        continue;
      }
      char* bundle_path =
          lilv_file_uri_parse(lilv_node_as_uri(lilv_ui_get_bundle_uri(ui)), NULL);
      char* binary_path =
          lilv_file_uri_parse(lilv_node_as_uri(lilv_ui_get_binary_uri(ui)), NULL);
      if (binary_path && access(binary_path, R_OK) == 0) {
        snprintf(ui_uri, ui_uri_cap, "%s", lilv_node_as_uri(lilv_ui_get_uri(ui)));
        snprintf(bundle, bundle_cap, "%s", bundle_path ? bundle_path : "");
        snprintf(binary, binary_cap, "%s", binary_path);
        found = 1;
      }
      lilv_free(bundle_path);
      lilv_free(binary_path);
      if (found) {
        break;
      }
    }
    if (found) {
      break;
    }
  }
  lilv_uis_free(uis);
  return found;
}

void* vibemix_ui_open(const char* plugin_uri, const char* ui_uri, const char* ui_type,
                      const char* bundle_path, const char* binary_path,
                      void* plugin_handle, void* urid_map, void* urid_unmap,
                      VibemixPortWrite write_cb, void* write_user, int headless) {
  XSetErrorHandler(ui_x_error);
  Display* dpy = XOpenDisplay(NULL);
  if (!dpy) {
    return NULL;
  }

  VibemixUi* ui = (VibemixUi*)calloc(1, sizeof(VibemixUi));
  ui->dpy = dpy;
  ui->write_cb = write_cb;
  ui->write_user = write_user;
  ui->shown = 1;

  int screen = DefaultScreen(dpy);
  ui->win = XCreateSimpleWindow(dpy, RootWindow(dpy, screen), 80, 80, 760, 520, 0,
                                BlackPixel(dpy, screen), WhitePixel(dpy, screen));
  XStoreName(dpy, ui->win, "vibemix plugin UI");
  ui->wm_delete = XInternAtom(dpy, "WM_DELETE_WINDOW", False);
  XSetWMProtocols(dpy, ui->win, &ui->wm_delete, 1);
  if (!headless) {
    XMapWindow(dpy, ui->win);
  }

  ui->resize = (LV2UI_Resize*)malloc(sizeof(LV2UI_Resize));
  ui->resize->handle = ui;
  ui->resize->ui_resize = ui_resize_cb;

  LV2_Feature parent_feature = {"http://lv2plug.in/ns/extensions/ui#parent",
                                (void*)(uintptr_t)ui->win};
  LV2_Feature instance_feature = {LV2_INSTANCE_ACCESS_URI, plugin_handle};
  LV2_Feature data_feature = {LV2_DATA_ACCESS_URI, &g_data};
  LV2_Feature map_feature = {LV2_URID__map, urid_map};
  LV2_Feature unmap_feature = {LV2_URID__unmap, urid_unmap};
  LV2_Feature resize_feature = {"http://lv2plug.in/ns/extensions/ui#resize", ui->resize};
  LV2_Feature show_feature = {"http://lv2plug.in/ns/extensions/ui#showInterface",
                              &ui->shown};
  const LV2_Feature* features[] = {&parent_feature,  &instance_feature,
                                   &data_feature,    &map_feature,
                                   &unmap_feature,   &resize_feature,
                                   &show_feature,    NULL};

  ui->host = suil_host_new(suil_write, suil_index, NULL, NULL);

  /* Pin the UI module resident and globally visible. Suil would otherwise
     dlclose it when the UI is freed, unloading its toolkit dependencies
     (GTK/GLib). Reloading them on a later open re-registers GObject types
     ("cannot register existing type 'PangoFontMap'") and can hang or crash
     the host, so keep the module loaded for the lifetime of the process. */
  if (binary_path && *binary_path) {
    dlopen(binary_path, RTLD_NOW | RTLD_GLOBAL | RTLD_NODELETE);
  }

  ui->inst = suil_instance_new(ui->host, (SuilController)ui,
                               "http://lv2plug.in/ns/extensions/ui#X11UI", plugin_uri,
                               ui_uri, ui_type, bundle_path, binary_path, features);
  if (!ui->inst) {
    suil_host_free(ui->host);
    XDestroyWindow(dpy, ui->win);
    XCloseDisplay(dpy);
    free(ui->resize);
    free(ui);
    return NULL;
  }

  return ui;
}

void vibemix_ui_pump(void* handle) {
  VibemixUi* ui = (VibemixUi*)handle;
  if (!ui || ui->close_requested) {
    return;
  }

  while (XPending(ui->dpy)) {
    XEvent event;
    XNextEvent(ui->dpy, &event);
    if (event.type == ClientMessage &&
        (Atom)event.xclient.data.l[0] == ui->wm_delete) {
      ui->close_requested = 1;
    }
  }

  const LV2UI_Idle_Interface* idle = (const LV2UI_Idle_Interface*)suil_instance_extension_data(
      ui->inst, "http://lv2plug.in/ns/extensions/ui#idleInterface");
  if (idle && idle->idle) {
    idle->idle(suil_instance_get_handle(ui->inst));
  }
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
  if (ui->inst) {
    suil_instance_free(ui->inst);
  }
  if (ui->host) {
    suil_host_free(ui->host);
  }
  if (ui->dpy) {
    XDestroyWindow(ui->dpy, ui->win);
    XCloseDisplay(ui->dpy);
  }
  free(ui->resize);
  free(ui);
}
