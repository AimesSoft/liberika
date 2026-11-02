#include "native_video_surface.h"
#include <gdk/gdkwayland.h>
#include <wayland-client.h>
#include <algorithm>
#include <cmath>
#include <cstring>
#include <stdexcept>
#include <string>
#include <map>

namespace {
void checked(ErikaStatus status) {
  if (status == ErikaStatus_Ok) return;
  char* raw = erika_last_error_message();
  std::string error = raw ? raw : "Native video surface operation failed";
  erika_string_free(raw);
  throw std::runtime_error(error);
}
// GTK paints the toplevel's themed background underneath Flutter. Clearing
// Flutter alone cannot reveal an underlay below that opaque GTK background.
struct TransparentHost {
  GtkWidget* window;
  GtkCssProvider* css = gtk_css_provider_new();
  gboolean was_app_paintable;
  explicit TransparentHost(GtkWidget* widget) : window(widget),
      was_app_paintable(gtk_widget_get_app_paintable(widget)) {
    g_object_add_weak_pointer(G_OBJECT(window), reinterpret_cast<gpointer*>(&window));
    gtk_css_provider_load_from_data(css, "window { background-color: transparent; background-image: none; }", -1, nullptr);
    gtk_style_context_add_provider(gtk_widget_get_style_context(window), GTK_STYLE_PROVIDER(css), GTK_STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
    gtk_widget_set_app_paintable(window, TRUE);
  }
  ~TransparentHost() {
    if (window) {
      gtk_style_context_remove_provider(gtk_widget_get_style_context(window), GTK_STYLE_PROVIDER(css));
      gtk_widget_set_app_paintable(window, was_app_paintable);
      g_object_remove_weak_pointer(G_OBJECT(window), reinterpret_cast<gpointer*>(&window));
    }
    g_object_unref(css);
  }
};
std::shared_ptr<TransparentHost> transparent_host(GtkWidget* window) {
  static std::map<GtkWidget*, std::weak_ptr<TransparentHost>> hosts;
  for (auto it = hosts.begin(); it != hosts.end();) {
    if (it->second.expired()) it = hosts.erase(it); else ++it;
  }
  auto host = hosts[window].lock();
  if (!host) { host = std::make_shared<TransparentHost>(window); hosts[window] = host; }
  return host;
}
}

struct NativeVideoSurface::Impl {
  FlView* view = nullptr;
  std::shared_ptr<TransparentHost> transparency;
  std::function<void(Operation)> post;
  std::function<void()> detach_fence;
  wl_display* display = nullptr;             // GTK owns the display/parent.
  wl_event_queue* queue = nullptr;
  wl_registry* registry = nullptr;
  wl_compositor* compositor = nullptr;
  wl_subcompositor* subcompositor = nullptr;
  wl_surface* surface = nullptr;
  wl_surface* parent_surface = nullptr;
  wl_subsurface* subsurface = nullptr;
  bool attached = false;
  bool shown = false;
  int width = 0, height = 0, scale = 0;

  ~Impl() {
    // The renderer must release its wl_egl_window / VkSurface before wl_surface.
    if (attached) detach_fence();
    if (subsurface) wl_subsurface_destroy(subsurface);
    if (surface) wl_surface_destroy(surface);
    if (subcompositor) wl_subcompositor_destroy(subcompositor);
    if (compositor) wl_compositor_destroy(compositor);
    if (registry) wl_registry_destroy(registry);
    if (queue) wl_event_queue_destroy(queue);
    if (view) g_object_remove_weak_pointer(G_OBJECT(view), reinterpret_cast<gpointer*>(&view));
  }
  static void global(void* data, wl_registry* registry, uint32_t name,
                     const char* interface, uint32_t version) {
    auto* self = static_cast<Impl*>(data);
    if (strcmp(interface, "wl_compositor") == 0) {
      self->compositor = static_cast<wl_compositor*>(wl_registry_bind(
          registry, name, &wl_compositor_interface, std::min(version, 4u)));
    } else if (strcmp(interface, "wl_subcompositor") == 0) {
      self->subcompositor = static_cast<wl_subcompositor*>(wl_registry_bind(
          registry, name, &wl_subcompositor_interface, 1));
    }
  }
  static void removed(void*, wl_registry*, uint32_t) {}
};

NativeVideoSurface::NativeVideoSurface(FlView* view, std::function<void(Operation)> post,
                                     std::function<void()> detach_fence)
    : impl_(std::make_unique<Impl>()) {
  auto& s = *impl_;
  GdkDisplay* display = gtk_widget_get_display(GTK_WIDGET(view));
  if (!GDK_IS_WAYLAND_DISPLAY(display))
    throw std::runtime_error("Native Linux video requires Wayland; start with GDK_BACKEND=wayland. Use the texture view for X11.");
  GtkWidget* top = gtk_widget_get_toplevel(GTK_WIDGET(view));
  GdkWindow* parent = gtk_widget_get_window(top);
  if (!parent || !GDK_IS_WAYLAND_WINDOW(parent))
    throw std::runtime_error("The Flutter Wayland window is not realized yet");
  wl_surface* parent_surface = gdk_wayland_window_get_wl_surface(parent);
  if (!parent_surface) throw std::runtime_error("Flutter has no Wayland surface yet");
  s.view = view;
  s.transparency = transparent_host(top);
  g_object_add_weak_pointer(G_OBJECT(view), reinterpret_cast<gpointer*>(&s.view));
  s.post = std::move(post);
  s.detach_fence = std::move(detach_fence);
  s.parent_surface = parent_surface;
  s.display = gdk_wayland_display_get_wl_display(display);
  s.queue = wl_display_create_queue(s.display);
  if (!s.queue) throw std::runtime_error("Cannot create video event queue");
  // Avoid dispatching GTK-owned callbacks recursively during registry discovery.
  auto* wrapper = static_cast<wl_display*>(wl_proxy_create_wrapper(s.display));
  if (!wrapper) throw std::runtime_error("Cannot wrap Wayland display");
  wl_proxy_set_queue(reinterpret_cast<wl_proxy*>(wrapper), s.queue);
  s.registry = wl_display_get_registry(wrapper);
  wl_proxy_wrapper_destroy(wrapper);
  static const wl_registry_listener listener = {Impl::global, Impl::removed};
  wl_registry_add_listener(s.registry, &listener, &s);
  if (wl_display_roundtrip_queue(s.display, s.queue) < 0 || !s.compositor || !s.subcompositor)
    throw std::runtime_error("Wayland compositor does not support video subsurfaces");
  s.surface = wl_compositor_create_surface(s.compositor);
  s.subsurface = wl_subcompositor_get_subsurface(s.subcompositor, s.surface, parent_surface);
  wl_subsurface_place_below(s.subsurface, parent_surface);
  wl_subsurface_set_desync(s.subsurface);
  // GTK owns parent commits, including xdg configure acknowledgements. Queue
  // a draw below so GTK applies the stacking state with its next valid frame.
  // Committing the parent here can race a pending GTK resize/configure.
  wl_region* empty = wl_compositor_create_region(s.compositor);
  wl_surface_set_input_region(s.surface, empty); // All input stays in Flutter.
  wl_region_destroy(empty);
  const GdkRGBA transparent{0, 0, 0, 0};
  fl_view_set_background_color(view, &transparent);
  gdk_window_set_opaque_region(parent, nullptr);
  gtk_widget_queue_draw(GTK_WIDGET(view));
}

NativeVideoSurface::~NativeVideoSurface() = default;

bool NativeVideoSurface::visible() const {
  return impl_->view && impl_->shown && gtk_widget_get_mapped(GTK_WIDGET(impl_->view));
}

void NativeVideoSurface::update(double x, double y, double w, double h, bool visible) {
  if (!std::isfinite(x) || !std::isfinite(y) || !std::isfinite(w) || !std::isfinite(h))
    throw std::runtime_error("Non-finite native video bounds");
  auto& s = *impl_;
  if (!s.view) throw std::runtime_error("The Flutter view has been destroyed");
  if (!visible || w <= 0 || h <= 0) {
    s.shown = false;
    if (s.attached) {
      s.detach_fence();
      s.attached = false;
    }
    wl_surface_attach(s.surface, nullptr, 0, 0);
    wl_surface_commit(s.surface);
    wl_display_flush(s.display);
    return;
  }
  int ox = 0, oy = 0;
  gtk_widget_translate_coordinates(GTK_WIDGET(s.view), gtk_widget_get_toplevel(GTK_WIDGET(s.view)), 0, 0, &ox, &oy);
  const int scale = std::max(1, gtk_widget_get_scale_factor(GTK_WIDGET(s.view)));
  const int width = std::clamp(static_cast<int>(std::round(std::min(w, 16384.0) * scale)), 1, 16384);
  const int height = std::clamp(static_cast<int>(std::round(std::min(h, 16384.0) * scale)), 1, 16384);
  wl_subsurface_set_position(s.subsurface, static_cast<int>(std::clamp(x + ox, -32768.0, 32768.0)),
                             static_cast<int>(std::clamp(y + oy, -32768.0, 32768.0)));
  wl_surface_set_buffer_scale(s.surface, scale);
  if (!s.attached) {
    // GTK and parent/subsurface protocol operations stay on the platform.
    // libwayland supports requests from multiple threads. EGL/Vulkan and the
    // presenter belong to the owner; destruction fences before releasing wl_surface.
    auto* surface = s.surface;
    auto* display = s.display;
    s.post([surface, display, width, height, scale](ErikaPresenterHandle* presenter) {
      checked(erika_presenter_attach_wgpu_surface(presenter, ErikaWgpuSurfaceKind_WaylandSurface,
          reinterpret_cast<uintptr_t>(surface), reinterpret_cast<uintptr_t>(display), width, height, scale));
    });
    s.attached = true;
  } else if (width != s.width || height != s.height || scale != s.scale) {
    s.post([width, height, scale](ErikaPresenterHandle* presenter) {
      checked(erika_presenter_resize_surface(presenter, width, height, scale));
    });
  }
  s.width = width; s.height = height; s.scale = scale; s.shown = true;
  wl_display_dispatch_queue_pending(s.display, s.queue);
  gtk_widget_queue_draw(GTK_WIDGET(s.view));
  wl_display_flush(s.display);
}
