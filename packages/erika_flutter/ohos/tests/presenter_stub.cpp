// Test double: verifies native ownership/lifecycle, never decodes real media.
#include "erika.h"
#include <atomic>
#include <cassert>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>

static const auto platform = std::this_thread::get_id();
struct ErikaPresenterHandle {
  std::thread::id owner = std::this_thread::get_id();
  std::string event;
  uint32_t width = 2, height = 2;
  bool attached = false;
  ErikaPresenterConfig config{};
};
static void Owner(ErikaPresenterHandle* p) {
  assert(p && p->owner == std::this_thread::get_id() && p->owner != platform);
}
extern "C" {
ErikaPresenterHandle* erika_presenter_create_with_config(ErikaPresenterConfig config) {
  auto* p = new ErikaPresenterHandle; Owner(p); p->config = config; return p;
}
void erika_presenter_destroy(ErikaPresenterHandle* p) {
  Owner(p); assert(!p->attached);
  std::fprintf(stderr, "stub: create/commands/render/poll/capture/destroy serialized off platform\n"); delete p;
}
char* erika_last_error_message() { return strdup("stub native error"); }
void erika_string_free(char* p) { free(p); }
char* erika_presenter_invoke_json(ErikaPresenterHandle* p, const char* method, const char*) {
  Owner(p);
  if (std::strcmp(method, "config") == 0) {
    // Echoes what reached erika_presenter_create_with_config.
    char json[160];
    std::snprintf(json, sizeof json, R"({"ok":true,"value":[%d,%g,%d,%d]})", p->config.output_mode,
                  p->config.edr_headroom, p->config.luma_upscaler, p->config.video_alpha_mode);
    return strdup(json);
  }
  if (std::strcmp(method, "slow") == 0) std::this_thread::sleep_for(std::chrono::milliseconds(40));
  p->event = R"({"ok":true,"value":{"kind":3,"positionMicros":123}})";
  return strdup(R"({"ok":true,"value":17})");
}
ErikaStatus erika_presenter_render_tick(ErikaPresenterHandle* p, double, ErikaPresenterStats*) {
  Owner(p); std::this_thread::sleep_for(std::chrono::milliseconds(20));
  p->event = R"({"ok":true,"value":{"kind":3,"positionMicros":123}})"; return ErikaStatus_Ok;
}
char* erika_presenter_render_tick_json(ErikaPresenterHandle* p, double time) {
  ErikaPresenterStats stats{}; erika_presenter_render_tick(p, time, &stats); return strdup(R"({"ok":true,"value":{}})");
}
char* erika_presenter_poll_event_json(ErikaPresenterHandle* p) {
  Owner(p); if (p->event.empty()) return nullptr;
  char* raw = strdup(p->event.c_str()); p->event.clear(); return raw;
}
ErikaStatus erika_presenter_register_subtitle_memory_font(ErikaPresenterHandle* p, const uint8_t* bytes, size_t size, uint64_t* font) {
  Owner(p); assert(size == 3 && bytes[0] == 1 && bytes[2] == 3); *font = 42; return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_load_upscaler_model(ErikaPresenterHandle* p, const char*, const uint8_t* bytes, size_t size) {
  Owner(p); assert(size == 3 && bytes[0] == 1 && bytes[2] == 3); return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_attach_wgpu_surface(ErikaPresenterHandle* p, ErikaWgpuSurfaceKind, uint64_t, uint64_t, uint32_t w, uint32_t h, double) {
  Owner(p); p->attached = true; p->width = w; p->height = h; return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_attach_flutter_texture(ErikaPresenterHandle* p, ErikaFlutterTextureKind, int64_t, uint32_t w, uint32_t h, double) {
  Owner(p); p->attached = true; p->width = w; p->height = h; return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_resize_surface(ErikaPresenterHandle* p, uint32_t w, uint32_t h, double) {
  Owner(p); p->width = w; p->height = h; return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_detach_surface(ErikaPresenterHandle* p) { Owner(p); p->attached = false; return ErikaStatus_Ok; }
ErikaStatus erika_presenter_audio_only_tick(ErikaPresenterHandle* p, ErikaPresenterStats*) { Owner(p); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_capture_frame_rgba(ErikaPresenterHandle* p, uint32_t w, uint32_t h, uint8_t* bytes, size_t size) {
  Owner(p); assert(size == static_cast<size_t>(w) * h * 4); std::memset(bytes, 127, size); return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_copy_flutter_frame_rgba(ErikaPresenterHandle* p, uint8_t* bytes, size_t size, uint32_t* w, uint32_t* h) {
  Owner(p); *w = p->width; *h = p->height;
  if (size < static_cast<size_t>(*w) * *h * 4) return ErikaStatus_NoEvent;
  std::memset(bytes, 127, static_cast<size_t>(*w) * *h * 4); return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_set_debug_hud_enabled(ErikaPresenterHandle* p, bool) { Owner(p); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_subtitle_style(ErikaPresenterHandle* p, ErikaSubtitleStyle) { Owner(p); return ErikaStatus_Ok; }
}
