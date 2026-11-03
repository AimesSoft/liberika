#include "include/erika_flutter/erika_flutter_plugin.h"
#include "erika.h"
#include "native_video_surface.h"
#include "presenter_worker.h"
#include <atomic>
#include <functional>
#include <deque>
#include <epoxy/egl.h>
#include <epoxy/glx.h>
#include <algorithm>
#include <cmath>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <string>
#include <vector>

// Main-thread producers never modify published pixels. Flutter's raster
// thread keeps the returned buffer alive until its next copy_pixels call.
struct Pixels { uint32_t width = 1, height = 1; std::vector<uint8_t> rgba{0,0,0,255}; };
struct PixelState { std::mutex mutex; std::shared_ptr<Pixels> pending, published = std::make_shared<Pixels>(); };
typedef struct { FlPixelBufferTexture parent; PixelState* pixels; } ErikaTexture;
typedef struct { FlPixelBufferTextureClass parent_class; } ErikaTextureClass;
G_DEFINE_TYPE(ErikaTexture, erika_texture, fl_pixel_buffer_texture_get_type())
static gboolean copy_pixels(FlPixelBufferTexture* texture, const uint8_t** buffer,
                            uint32_t* width, uint32_t* height, GError**) {
  auto* state = reinterpret_cast<ErikaTexture*>(texture)->pixels;
  std::lock_guard<std::mutex> lock(state->mutex);
  if (state->pending) state->published = std::move(state->pending);
  *buffer = state->published->rgba.data();
  *width = state->published->width; *height = state->published->height;
  return TRUE;
}
static void texture_finalize(GObject* object) {
  delete reinterpret_cast<ErikaTexture*>(object)->pixels;
  G_OBJECT_CLASS(erika_texture_parent_class)->finalize(object);
}
static void erika_texture_class_init(ErikaTextureClass* klass) {
  FL_PIXEL_BUFFER_TEXTURE_CLASS(klass)->copy_pixels = copy_pixels;
  G_OBJECT_CLASS(klass)->finalize = texture_finalize;
}
static void erika_texture_init(ErikaTexture* self) { self->pixels = new PixelState(); }

struct Plugin;
using Completion = std::function<void()>;
struct CompletionQueue {
  std::mutex mutex;
  std::deque<Completion> tasks;
  guint source = 0;
  void Drain() {
    std::deque<Completion> ready;
    { std::lock_guard<std::mutex> lock(mutex); ready.swap(tasks); }
    for (auto& task : ready) task();
  }
  void Post(Completion task) {
    std::lock_guard<std::mutex> lock(mutex);
    tasks.push_back(std::move(task));
    if (!source) {
      // invoke() can run inline if the caller can acquire an idle context.
      // An idle source always delivers on the default platform loop.
      source = g_idle_add_full(G_PRIORITY_DEFAULT, [](gpointer data) -> gboolean {
        auto* queue = static_cast<CompletionQueue*>(data);
        { std::lock_guard<std::mutex> lock(queue->mutex); queue->source = 0; }
        queue->Drain();
        return G_SOURCE_REMOVE;
      }, this, nullptr);
    }
  }
  ~CompletionQueue() { if (source) g_source_remove(source); }
};
struct Player {
  // Only the owner accesses handle and frame target state.
  ErikaPresenterHandle* handle = nullptr;
  uint32_t frame_width = 1, frame_height = 1;
  int64_t frame_texture = 0, frame_epoch = 0;
  bool traced = false;
  std::string surface_error;
  const std::thread::id platform = std::this_thread::get_id();
  std::shared_ptr<std::atomic<bool>> delivery_pending = std::make_shared<std::atomic<bool>>(false);
  // Platform-only bookkeeping.
  int64_t texture = 0, epoch = 0;
  std::unique_ptr<NativeVideoSurface> native;
  int64_t native_generation = 0, native_view_id = -1;
  erika::PresenterWorker worker;
  void DetachFence() {
    worker.Call([this] {
      frame_texture = 0;
      if (handle) erika_presenter_detach_surface(handle);
    });
  }
  ~Player() {
    // Retain wl_surface until the native detach has drained prior rendering.
    native.reset();
    worker.Stop([this] {
      if (handle) { erika_presenter_detach_surface(handle); erika_presenter_destroy(handle); }
      handle = nullptr;
    });
  }
};
struct Texture { ErikaTexture* object; uint32_t width, height; double scale; };
struct Plugin {
  FlTextureRegistrar* registrar;
  FlEventChannel* events;
  bool listening = false;
  std::atomic<uint64_t> listener_generation{0};
  int64_t next_id = 1;
  CompletionQueue completions;
  std::map<int64_t, std::unique_ptr<Player>> players;
  std::map<int64_t, Texture> textures;
  ~Plugin() {
    listening = false;
    ++listener_generation;
    players.clear(); // Native tasks never wait for a platform completion.
    completions.Drain(); // Complete every accepted method call before channels die.
    for (auto& item : textures) {
      fl_texture_registrar_unregister_texture(registrar, FL_TEXTURE(item.second.object));
      g_object_unref(item.second.object);
    }
    fl_event_channel_set_stream_handlers(events, nullptr, nullptr, nullptr, nullptr);
    g_object_unref(events); g_object_unref(registrar);
  }
};
static FlValue* arg(FlValue* args, const char* key) {
  return args && fl_value_get_type(args) == FL_VALUE_TYPE_MAP ? fl_value_lookup_string(args, key) : nullptr;
}
static double number(FlValue* args, const char* key, double fallback = 0) {
  auto* value = arg(args, key);
  if (!value) return fallback;
  if (fl_value_get_type(value) == FL_VALUE_TYPE_INT) return fl_value_get_int(value);
  if (fl_value_get_type(value) == FL_VALUE_TYPE_FLOAT) return fl_value_get_float(value);
  return fallback;
}
static const char* string_arg(FlValue* args, const char* key) {
  auto* value = arg(args, key);
  return value && fl_value_get_type(value) == FL_VALUE_TYPE_STRING ? fl_value_get_string(value) : nullptr;
}
static bool boolean(FlValue* args, const char* key) {
  auto* value = arg(args, key);
  return value && fl_value_get_type(value) == FL_VALUE_TYPE_BOOL && fl_value_get_bool(value);
}
static void check(ErikaStatus status) {
  if (status == ErikaStatus_Ok) return;
  char* raw = erika_last_error_message();
  std::string message = raw ? raw : "Erika operation failed";
  erika_string_free(raw);
  throw std::runtime_error(message);
}
static FlView* find_view(GtkWidget* widget, int64_t id) {
  if (FL_IS_VIEW(widget) && (id < 0 || fl_view_get_id(FL_VIEW(widget)) == id))
    return FL_VIEW(widget);
  if (!GTK_IS_CONTAINER(widget)) return nullptr;
  GList* children = gtk_container_get_children(GTK_CONTAINER(widget));
  FlView* result = nullptr;
  for (auto* child = children; child && !result; child = child->next)
    result = find_view(GTK_WIDGET(child->data), id);
  g_list_free(children);
  return result;
}
static FlView* find_view(int64_t id) {
  GList* windows = gtk_window_list_toplevels();
  FlView* result = nullptr;
  for (auto* window = windows; window && !result; window = window->next)
    result = find_view(GTK_WIDGET(window->data), id);
  g_list_free(windows);
  if (!result) throw std::runtime_error("The requested Flutter view is not available");
  return result;
}
static FlValue* decode_owned(const std::string& raw) {
  if (raw.empty()) return nullptr;
  g_autoptr(FlJsonMessageCodec) codec = fl_json_message_codec_new();
  g_autoptr(GBytes) bytes = g_bytes_new(raw.data(), raw.size());
  g_autoptr(GError) error = nullptr;
  g_autoptr(FlValue) envelope = fl_message_codec_decode_message(FL_MESSAGE_CODEC(codec), bytes, &error);
  if (!envelope) throw std::runtime_error(error ? error->message : "Invalid Erika JSON");
  if (!boolean(envelope, "ok")) {
    const char* message = string_arg(envelope, "error");
    throw std::runtime_error(message ? message : "Erika operation failed");
  }
  auto* result = arg(envelope, "value");
  return result ? fl_value_ref(result) : fl_value_new_null();
}
static void success(FlMethodCall* call, FlValue* value = nullptr) {
  fl_method_call_respond_success(call, value, nullptr);
}
static void send_event(Plugin* plugin, int64_t id, FlValue* event) {
  if (!plugin->listening || !event || fl_value_get_type(event) != FL_VALUE_TYPE_MAP) return;
  fl_value_set_string_take(event, "playerId", fl_value_new_int(id));
  fl_event_channel_send(plugin->events, event, nullptr, nullptr);
}
static std::string take_json(char* raw) {
  std::string result = raw ? raw : "";
  erika_string_free(raw);
  return result;
}
static void tick(Plugin* plugin, Player* player, int64_t id) {
  if (!player->handle) return;
  const auto start = g_get_monotonic_time();
  const bool trace = g_strcmp0(g_getenv("ERIKA_FLUTTER_FRAME_TRACE"), "1") == 0;
  if (trace && !player->traced) {
    player->traced = true;
    g_message("presenter_worker player=%" G_GINT64_FORMAT " platform_thread=%zu render_thread=%zu off_platform=%s",
      id, std::hash<std::thread::id>{}(player->platform),
      std::hash<std::thread::id>{}(std::this_thread::get_id()),
      player->platform != std::this_thread::get_id() ? "true" : "false");
  }
  std::vector<std::pair<std::string, std::string>> events;
  std::shared_ptr<Pixels> pixels;
  std::string error;
  const auto generation = plugin->listener_generation.load();
  const auto texture = player->frame_texture, epoch = player->frame_epoch;
  const bool deliver = !player->delivery_pending->exchange(true);
  try {
    ErikaPresenterStats stats{};
    check(erika_presenter_render_tick(player->handle, start / 1000000.0, &stats));
    if (deliver && texture) {
      pixels = std::make_shared<Pixels>();
      pixels->rgba.resize(static_cast<size_t>(player->frame_width) * player->frame_height * 4);
      const auto status = erika_presenter_copy_flutter_frame_rgba(player->handle,
        pixels->rgba.data(), pixels->rgba.size(), &pixels->width, &pixels->height);
      if (status != ErikaStatus_Ok) { pixels.reset(); if (status != ErikaStatus_NoEvent) check(status); }
    }
    if (deliver) for (int count = 0; count < 128; ++count) {
      auto raw = take_json(erika_presenter_poll_event_json(player->handle));
      if (raw.empty()) break;
      // Parsing here uses isolated values; no Flutter channel/registrar on owner.
      g_autoptr(FlValue) event = decode_owned(raw);
      const auto kind = static_cast<int>(number(event, "kind"));
      std::string tracks;
      if (kind == ErikaEventKind_TracksChanged || kind == ErikaEventKind_TrackSelectionChanged)
        tracks = take_json(erika_presenter_invoke_json(player->handle, "tracks", "{}"));
      events.emplace_back(std::move(raw), std::move(tracks));
    }
  } catch (const std::exception& failure) { error = failure.what(); }
  const double work_ms = (g_get_monotonic_time() - start) / 1000.0;
  if (trace && work_ms >= 16)
    g_message("frame_tick player=%" G_GINT64_FORMAT " work_ms=%.3f", id, work_ms);
  if (!deliver) return;
  plugin->completions.Post([plugin, id, texture, epoch, generation,
      pending = player->delivery_pending, pixels = std::move(pixels),
      events = std::move(events), error = std::move(error)] {
    pending->store(false);
    auto found = plugin->players.find(id);
    if (found == plugin->players.end()) return;
    auto target = plugin->textures.find(texture);
    if (pixels && found->second->epoch == epoch && found->second->texture == texture &&
        target != plugin->textures.end()) {
      {
        std::lock_guard<std::mutex> lock(target->second.object->pixels->mutex);
        target->second.object->pixels->pending = pixels;
      }
      // Registrar calls touch FlEngine; keep registration, notification and
      // unregistration on the platform. copy_pixels alone runs on the raster thread.
      fl_texture_registrar_mark_texture_frame_available(plugin->registrar, FL_TEXTURE(target->second.object));
    }
    if (generation != plugin->listener_generation.load()) return;
    try {
      for (const auto& raw : events) {
        g_autoptr(FlValue) event = decode_owned(raw.first);
        if (!raw.second.empty()) {
          g_autoptr(FlValue) tracks = decode_owned(raw.second);
          fl_value_set_string(event, "trackList", tracks);
        }
        send_event(plugin, id, event);
      }
      if (!error.empty()) {
        g_autoptr(FlValue) event = fl_value_new_map();
        fl_value_set_string_take(event, "kind", fl_value_new_int(ErikaEventKind_Error));
        fl_value_set_string_take(event, "error", fl_value_new_string(error.c_str()));
        send_event(plugin, id, event);
      }
    } catch (const std::exception& failure) { g_warning("Erika event: %s", failure.what()); }
  });
}
static void target_changed(Player& player) {
  ++player.epoch;
  const auto texture = player.texture, epoch = player.epoch;
  player.worker.Post([&player, texture, epoch] { player.frame_texture = texture; player.frame_epoch = epoch; });
}
static void detach_texture(Plugin* plugin, int64_t id) {
  for (auto& item : plugin->players) if (item.second->texture == id) {
    auto& player = *item.second;
    player.texture = 0; target_changed(player);
    // A retirement fence retains the registrar object until all native work ends.
    player.DetachFence();
  }
}
static void queue_method(Plugin* plugin, Player* player, FlMethodCall* call,
                         std::function<FlValue*()> work, Completion failed = {}) {
  g_object_ref(call);
  player->worker.Post([plugin, call, work = std::move(work), failed = std::move(failed)] {
    FlValue* result = nullptr;
    std::string error;
    try { result = work(); } catch (const std::exception& failure) { error = failure.what(); }
    plugin->completions.Post([call, result, error = std::move(error), failed] {
      if (error.empty()) success(call, result);
      else {
        if (failed) failed();
        fl_method_call_respond_error(call, "erika_linux", error.c_str(), nullptr, nullptr);
      }
      if (result) fl_value_unref(result);
      g_object_unref(call);
    });
  });
}
static void method_call(FlMethodChannel*, FlMethodCall* call, gpointer data) {
  auto* plugin = static_cast<Plugin*>(data);
  std::string method = fl_method_call_get_name(call);
  FlValue* args = fl_method_call_get_args(call);
  try {
    if (method == "create") {
      auto player = std::make_unique<Player>();
      ErikaPresenterConfig config{};
      config.output_mode = static_cast<int32_t>(number(args, "outputMode"));
      config.edr_headroom = static_cast<float>(number(args, "edrHeadroom", 1));
      config.luma_upscaler = static_cast<int32_t>(number(args, "upscaler"));
      config.video_alpha_mode = static_cast<int32_t>(number(args, "videoAlphaMode"));
      auto id = plugin->next_id++;
      auto* owner = player.get();
      plugin->players.emplace(id, std::move(player));
      const bool debug = g_strcmp0(g_getenv("ERIKA_DEBUG_HUD"), "1") == 0;
      queue_method(plugin, owner, call, [owner, config, id, debug] {
        owner->handle = erika_presenter_create_with_config(config);
        if (!owner->handle) check(ErikaStatus_PlayerError);
        if (debug) check(erika_presenter_set_debug_hud_enabled(owner->handle, true));
        return fl_value_new_int(id);
      }, [plugin, id] { plugin->players.erase(id); });
      owner->worker.SetFrameTask([plugin, owner, id] { tick(plugin, owner, id); });
      return;
    }
    if (method == "createTexture") {
      uint32_t width = std::clamp(number(args, "width", 1), 1.0, 8192.0);
      uint32_t height = std::clamp(number(args, "height", 1), 1.0, 8192.0);
      auto* texture = reinterpret_cast<ErikaTexture*>(g_object_new(erika_texture_get_type(), nullptr));
      if (!fl_texture_registrar_register_texture(plugin->registrar, FL_TEXTURE(texture))) {
        g_object_unref(texture); throw std::runtime_error("Flutter texture registration failed");
      }
      auto id = fl_texture_get_id(FL_TEXTURE(texture));
      plugin->textures.emplace(id, Texture{texture, width, height, number(args, "scale", 1)});
      g_autoptr(FlValue) value = fl_value_new_int(id); success(call, value); return;
    }
    if (method == "resizeTexture" || method == "releaseTexture") {
      auto id = static_cast<int64_t>(number(args, "textureId"));
      auto found = plugin->textures.find(id);
      if (found == plugin->textures.end()) { success(call); return; }
      if (method == "releaseTexture") {
        detach_texture(plugin, id);
        fl_texture_registrar_unregister_texture(plugin->registrar, FL_TEXTURE(found->second.object));
        g_object_unref(found->second.object); plugin->textures.erase(found);
      } else {
        auto& target = found->second;
        target.width = std::clamp(number(args, "width", 1), 1.0, 8192.0);
        target.height = std::clamp(number(args, "height", 1), 1.0, 8192.0);
        target.scale = number(args, "scale", 1);
        for (auto& item : plugin->players) if (item.second->texture == id)
          {
            auto* owner = item.second.get();
            ++owner->epoch;
            const auto epoch = owner->epoch;
            queue_method(plugin, owner, call, [owner, target, epoch]() -> FlValue* {
              owner->frame_width = target.width; owner->frame_height = target.height; owner->frame_epoch = epoch;
              check(erika_presenter_resize_surface(owner->handle, target.width, target.height, target.scale));
              return nullptr;
            });
            return;
          }
      }
      success(call); return;
    }
    auto id = static_cast<int64_t>(number(args, "playerId"));
    auto found = plugin->players.find(id);
    if (found == plugin->players.end()) {
      if (method == "dispose" || method == "detachView") { success(call); return; }
      throw std::runtime_error("Unknown Erika player");
    }
    auto& player = *found->second;
    if (method == "dispose") { plugin->players.erase(found); success(call); return; }
    if (method == "attachOverlay") {
      const auto view_id = static_cast<int64_t>(number(args, "flutterViewId", -1));
      if (boolean(args, "secondaryWindow") && view_id < 0)
        throw std::runtime_error("A secondary Linux window requires flutterViewId");
      if (player.native && player.native_view_id != view_id) player.native.reset();
      if (!player.native) {
        player.texture = 0; target_changed(player);
        player.DetachFence();
        auto* owner = &player;
        player.native = std::make_unique<NativeVideoSurface>(find_view(view_id),
          [owner](NativeVideoSurface::Operation operation) {
            owner->worker.Post([owner, operation] {
              owner->surface_error.clear();
              try { operation(owner->handle); }
              catch (const std::exception& error) { owner->surface_error = error.what(); }
            });
          }, [owner] { owner->DetachFence(); });
        player.native_view_id = view_id;
        player.native_generation = 0;
      }
      g_autoptr(FlValue) value = fl_value_new_int(-1); success(call, value); return;
    }
    if (method == "setOverlayFrame") {
      const auto generation = static_cast<int64_t>(number(args, "generation"));
      const bool visible = boolean(args, "visible");
      if (!player.native) { success(call); return; }
      if (generation < player.native_generation || (!visible && generation != player.native_generation)) {
        success(call); return;
      }
      const char* blend = string_arg(args, "blendMode");
      if ((blend && strcmp(blend, "srcOver") != 0) || number(args, "opacity", 1) != 1)
        throw std::runtime_error("Native Linux video requires opaque srcOver composition");
      player.native->update(number(args, "x"), number(args, "y"), number(args, "width"), number(args, "height"), visible);
      if (visible) player.native_generation = generation;
      const auto epoch = ++player.epoch;
      auto* owner = &player;
      queue_method(plugin, owner, call, [owner]() -> FlValue* {
        if (!owner->surface_error.empty()) throw std::runtime_error(owner->surface_error);
        return nullptr;
      }, [plugin, id, epoch] {
        auto found = plugin->players.find(id);
        if (found != plugin->players.end() && found->second->epoch == epoch) found->second->native.reset();
      });
      return;
    }
    if (method == "detachOverlay") {
      const auto generation = static_cast<int64_t>(number(args, "generation", player.native_generation));
      if (generation == player.native_generation) player.native.reset();
      success(call); return;
    }
    if (method == "attachView") {
      player.native.reset();
      auto texture_id = static_cast<int64_t>(number(args, "viewId"));
      const auto& texture = plugin->textures.at(texture_id);
      detach_texture(plugin, texture_id);
      player.texture = texture_id; target_changed(player);
      auto* owner = &player;
      queue_method(plugin, owner, call, [owner, texture, texture_id] {
        check(erika_presenter_attach_flutter_texture(owner->handle, ErikaFlutterTextureKind_LinuxTextureRegistrar,
          texture_id, texture.width, texture.height, texture.scale));
        owner->frame_width = texture.width; owner->frame_height = texture.height;
        return nullptr;
      });
      return;
    }
    if (method == "detachView") {
      if (player.texture == static_cast<int64_t>(number(args, "viewId"))) {
        player.texture = 0; target_changed(player);
        auto* owner = &player;
        queue_method(plugin, owner, call, [owner] {
          check(erika_presenter_detach_surface(owner->handle)); return nullptr;
        });
        return;
      }
      success(call); return;
    }
    if (method == "setMediaMetadata" || method == "setSystemMediaNavigation") {
      // Desktop MPRIS is optional; these hints do not change playback.
      success(call); return;
    }
    // Byte arguments are copied here for the owner; the JSON hop below cannot
    // carry them.
    const bool binary = method == "registerSubtitleMemoryFont" || method == "loadUpscalerModel";
    std::vector<uint8_t> payload;
    std::string payload_name;
    if (binary) {
      auto* bytes = arg(args, "data");
      if (!bytes || fl_value_get_type(bytes) != FL_VALUE_TYPE_UINT8_LIST)
        throw std::runtime_error(method == "loadUpscalerModel" ? "Expected model bytes" : "Expected font bytes");
      payload.assign(fl_value_get_uint8_list(bytes), fl_value_get_uint8_list(bytes) + fl_value_get_length(bytes));
      const char* name = string_arg(args, "name");
      payload_name = name && *name ? name : "custom";
    }
    if (method == "loadUpscalerModel" && payload.empty()) throw std::runtime_error("Expected model bytes");
    g_autoptr(FlJsonMessageCodec) codec = fl_json_message_codec_new();
    g_autoptr(GError) error = nullptr;
    g_autoptr(GBytes) bytes = fl_message_codec_encode_message(FL_MESSAGE_CODEC(codec),
      binary ? nullptr : args, &error);
    if (!bytes) throw std::runtime_error(error->message);
    gsize size; const auto* encoded = static_cast<const char*>(g_bytes_get_data(bytes, &size));
    std::string json(encoded, size);
    auto* owner = &player;
    queue_method(plugin, owner, call, [owner, method, json, payload = std::move(payload),
                                       payload_name = std::move(payload_name)]() -> FlValue* {
      g_autoptr(FlJsonMessageCodec) codec = fl_json_message_codec_new();
      g_autoptr(GBytes) bytes = g_bytes_new(json.data(), json.size());
      g_autoptr(GError) error = nullptr;
      g_autoptr(FlValue) args = fl_message_codec_decode_message(FL_MESSAGE_CODEC(codec), bytes, &error);
      if (!args) throw std::runtime_error(error ? error->message : "Invalid method arguments");
      auto& player = *owner;
      if (!player.handle) throw std::runtime_error("Presenter creation failed");
    if (method == "registerSubtitleMemoryFont") {
      uint64_t font_id = 0;
      check(erika_presenter_register_subtitle_memory_font(player.handle, payload.data(), payload.size(), &font_id));
      return fl_value_new_int(font_id);
    }
    if (method == "loadUpscalerModel") {
      check(erika_presenter_load_upscaler_model(player.handle, payload_name.c_str(), payload.data(), payload.size()));
      return nullptr;
    }
    if (method == "clearUpscalerModel") {
      check(erika_presenter_clear_upscaler_model(player.handle));
      return nullptr;
    }
    if (method == "setSubtitleStyle") {
      ErikaSubtitleStyle style{};
      style.font_family = string_arg(args, "fontFamily"); style.font_file_path = string_arg(args, "fontFilePath");
      style.primary_color_rgba = number(args, "primaryColorRgba", 0xFFFFFFFF);
      style.outline_color_rgba = number(args, "outlineColorRgba", 0x0000007F);
      style.font_size = number(args, "fontSize", 48); style.outline_width = number(args, "outlineWidth", 2);
      style.bold = boolean(args, "bold"); style.italic = boolean(args, "italic");
      style.underline = boolean(args, "underline"); style.strike_out = boolean(args, "strikeOut");
      style.spacing = number(args, "spacing"); style.scale_x_percent = number(args, "scaleXPercent", 100);
      style.scale_y_percent = number(args, "scaleYPercent", 100); style.border_style = number(args, "borderStyle", 1);
      style.shadow_depth = number(args, "shadowDepth"); style.blur = number(args, "blur");
      style.alignment = number(args, "alignment", 2); style.margin_left = number(args, "marginLeft", 48);
      style.margin_right = number(args, "marginRight", 48); style.margin_vertical = number(args, "marginVertical", 54);
      style.override_mask = number(args, "overrideMask");
      check(erika_presenter_set_subtitle_style(player.handle, style)); return nullptr;
    }
    if (method == "screenshot") {
      uint32_t width = std::clamp(number(args, "width", 1280), 1.0, 8192.0);
      uint32_t height = std::clamp(number(args, "height", 720), 1.0, 8192.0);
      std::vector<uint8_t> rgba(static_cast<size_t>(width) * height * 4);
      check(erika_presenter_capture_frame_rgba(player.handle, width, height, rgba.data(), rgba.size()));
      // The Dart screenshot contract is tightly packed RGBA, not encoded PNG.
      return fl_value_new_uint8_list(rgba.data(), rgba.size());
    }
    return decode_owned(take_json(erika_presenter_invoke_json(player.handle, method.c_str(), json.c_str())));
    });
  } catch (const std::exception& error) {
    fl_method_call_respond_error(call, "erika_linux", error.what(), nullptr, nullptr);
  }
}
static FlMethodErrorResponse* listen(FlEventChannel*, FlValue*, gpointer data) {
  auto* plugin = static_cast<Plugin*>(data); ++plugin->listener_generation; plugin->listening = true; return nullptr;
}
static FlMethodErrorResponse* cancel(FlEventChannel*, FlValue*, gpointer data) {
  auto* plugin = static_cast<Plugin*>(data); ++plugin->listener_generation; plugin->listening = false; return nullptr;
}
void erika_flutter_plugin_register_with_registrar(FlPluginRegistrar* registrar) {
  auto* plugin = new Plugin();
  plugin->registrar = FL_TEXTURE_REGISTRAR(g_object_ref(fl_plugin_registrar_get_texture_registrar(registrar)));
  auto* messenger = fl_plugin_registrar_get_messenger(registrar);
  g_autoptr(FlStandardMethodCodec) codec = fl_standard_method_codec_new();
  plugin->events = fl_event_channel_new(messenger, "erika_flutter/events", FL_METHOD_CODEC(codec));
  fl_event_channel_set_stream_handlers(plugin->events, listen, cancel, plugin, nullptr);
  g_autoptr(FlMethodChannel) channel = fl_method_channel_new(messenger, "erika_flutter/player", FL_METHOD_CODEC(codec));
  fl_method_channel_set_method_call_handler(channel, method_call, plugin, [](gpointer data) { delete static_cast<Plugin*>(data); });
}
