// Test double for the Swift C-ABI loader; no decoding, GPU work, or windows.
// Regenerate only when the public erika.h signatures change.
#include "erika.h"
#include <assert.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct ErikaPresenterHandle {
  pthread_t owner;
  bool attached;
  int pending_event;
  uint64_t position;
  unsigned next_volume;
  ErikaDanmakuConfig config;
  uint64_t layer;
  bool detach_failed;
  bool playing;
  struct ErikaPresenterHandle *next;
};
static atomic_int creates, destroys, ticks, commands, entered, fail_detach;
static pthread_mutex_t hold_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t hold_ready = PTHREAD_COND_INITIALIZER;
static bool held;
static int hold_kind;
static pthread_mutex_t surface_lock = PTHREAD_MUTEX_INITIALIZER;
static ErikaPresenterHandle *presenters;
static void check_owner(ErikaPresenterHandle *p) {
  assert(p && !pthread_main_np() && pthread_equal(p->owner, pthread_self()));
}
static ErikaPresenterHandle *make_presenter(void) {
  assert(!pthread_main_np());
  ErikaPresenterHandle *p = calloc(1, sizeof(*p));
  p->owner = pthread_self();
  pthread_mutex_lock(&surface_lock);
  for (ErikaPresenterHandle *other = presenters; other; other = other->next)
    assert(!pthread_equal(other->owner, p->owner));
  p->next = presenters;
  presenters = p;
  pthread_mutex_unlock(&surface_lock);
  atomic_fetch_add(&creates, 1);
  return p;
}
static void hold_if_requested(int kind) {
  atomic_fetch_add(&entered, 1);
  pthread_mutex_lock(&hold_lock);
  while (held && hold_kind == kind) pthread_cond_wait(&hold_ready, &hold_lock);
  pthread_mutex_unlock(&hold_lock);
}
// Test control is process-global; it never accesses a presenter handle.
void erika_test_hold(int value) {
  pthread_mutex_lock(&hold_lock); held = value != 0; hold_kind = value;
  if (value == 3) atomic_store(&fail_detach, 1);
  if (!held) pthread_cond_broadcast(&hold_ready);
  pthread_mutex_unlock(&hold_lock);
}
int erika_test_count(int which) {
  switch(which) {
    case 0: return atomic_load(&creates);
    case 1: return atomic_load(&destroys);
    case 2: return atomic_load(&ticks);
    case 3: return atomic_load(&commands);
    default: return atomic_load(&entered);
  }
}

void erika_danmaku_track_info_free(ErikaDanmakuTrackInfo *track) {  }
ErikaStatus erika_export_frame(
    const ErikaFrameExportOptions *options,
    ErikaFrameExportResult *out_result) { return ErikaStatus_Ok; }
ErikaStatus erika_export_gif(
    const ErikaGifExportOptions *options,
    ErikaGifExportResult *out_result) { return ErikaStatus_Ok; }
void erika_frame_export_result_free(ErikaFrameExportResult *result) {  }
void erika_frame_export_session_close(ErikaFrameExportSession *session) {  }
ErikaStatus erika_frame_export_session_frame(
    ErikaFrameExportSession *session,
    const ErikaFrameExportRequest *request,
    ErikaFrameExportResult *out_result) { return ErikaStatus_Ok; }
ErikaStatus erika_frame_export_session_open(
    const ErikaFrameExportSessionOptions *options,
    ErikaFrameExportSession **out_session) { return ErikaStatus_Ok; }
char * erika_last_error_message(void) { return strdup("owner-test native error"); }
ErikaStatus erika_log_read_json(
    uint32_t limit,
    int32_t min_level,
    uint64_t since_seq,
    char **out_json) { return ErikaStatus_Ok; }
ErikaStatus erika_presenter_add_danmaku_track_file(
    ErikaPresenterHandle *handle,
    const char *uri,
    const char *name,
    int64_t offset_micros,
    uint64_t *out_track_id) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_add_danmaku_track_json(
    ErikaPresenterHandle *handle,
    const char *json,
    const char *name,
    int64_t offset_micros,
    uint64_t *out_track_id) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_add_external_subtitle(
    ErikaPresenterHandle *handle,
    const char *uri,
    int64_t *out_track_id) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_attach_flutter_texture(
    ErikaPresenterHandle *handle,
    ErikaFlutterTextureKind kind,
    int64_t texture_id,
    uint32_t width,
    uint32_t height,
    double scale) {
  check_owner(handle);
  pthread_mutex_lock(&surface_lock);
  handle->layer = 0;
  handle->attached = true;
  pthread_mutex_unlock(&surface_lock);
  return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_attach_metal_layer(
    ErikaPresenterHandle *handle,
    uint64_t raw_layer,
    uint32_t width,
    uint32_t height,
    double scale) {
  check_owner(handle);
  pthread_mutex_lock(&surface_lock);
  for (ErikaPresenterHandle *p = presenters; p; p = p->next)
    assert(p == handle || !p->attached || p->layer != raw_layer);
  handle->layer = raw_layer;
  handle->attached = true;
  pthread_mutex_unlock(&surface_lock);
  return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_audio_only_tick(
    ErikaPresenterHandle *handle,
    ErikaPresenterStats *out_stats) { return erika_presenter_render_tick(handle, 0, out_stats); }
ErikaStatus erika_presenter_capture_frame_rgba(
    ErikaPresenterHandle *handle,
    uint32_t width,
    uint32_t height,
    uint8_t *out_rgba,
    uintptr_t out_capacity) { check_owner(handle); assert(out_capacity >= (uintptr_t)width * height * 4); memset(out_rgba, 127, (size_t)width * height * 4); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_clear_danmaku(ErikaPresenterHandle *handle) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_clear_subtitle_memory_fonts(ErikaPresenterHandle *handle) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_clear_upscaler_model(ErikaPresenterHandle *handle) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_close(ErikaPresenterHandle *handle) { check_owner(handle); return ErikaStatus_Ok; }
ErikaPresenterHandle * erika_presenter_create(void) { return make_presenter(); }
ErikaPresenterHandle * erika_presenter_create_with_output_mode(
    int32_t output_mode,
    float edr_headroom) { return make_presenter(); }
ErikaStatus erika_presenter_danmaku_tracks(
    ErikaPresenterHandle *handle,
    ErikaDanmakuTrackInfo *out_tracks,
    uintptr_t capacity,
    uintptr_t *out_len) { check_owner(handle); *out_len = 0; return ErikaStatus_Ok; }
void erika_presenter_destroy(ErikaPresenterHandle *handle) {
  check_owner(handle);
  pthread_mutex_lock(&surface_lock);
  assert(!handle->attached || handle->detach_failed);
  ErikaPresenterHandle **p = &presenters;
  while (*p != handle) p = &(*p)->next;
  *p = handle->next;
  pthread_mutex_unlock(&surface_lock);
  atomic_fetch_add(&destroys, 1);
  free(handle);
}
ErikaStatus erika_presenter_detach_surface(ErikaPresenterHandle *handle) {
  check_owner(handle);
  if (atomic_exchange(&fail_detach, 0)) {
    handle->detach_failed = true;
    return ErikaStatus_PlayerError;
  }
  pthread_mutex_lock(&surface_lock);
  handle->attached = false;
  handle->layer = 0;
  pthread_mutex_unlock(&surface_lock);
  return ErikaStatus_Ok;
}
ErikaStatus erika_presenter_get_danmaku_config(
    ErikaPresenterHandle *handle,
    ErikaDanmakuConfig *out_config) { check_owner(handle); *out_config = handle->config; return ErikaStatus_Ok; }
ErikaStatus erika_presenter_get_diagnostics_json(
    ErikaPresenterHandle *handle,
    char **out_json) { check_owner(handle); *out_json = strdup("{}"); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_get_output_status(
    ErikaPresenterHandle *handle,
    ErikaOutputStatus *out_status) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_get_resource_status(
    ErikaPresenterHandle *handle,
    ErikaPresenterResourceStatus *out_status) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_get_subtitle_memory_font_status(
    ErikaPresenterHandle *handle,
    ErikaSubtitleMemoryFontStatus *out_status) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_get_upscaler_status(
    ErikaPresenterHandle *handle,
    ErikaUpscalerStatus *out_status) { check_owner(handle); return ErikaStatus_Ok; }
char * erika_presenter_invoke_json(
    ErikaPresenterHandle *handle,
    const char *method,
    const char *arguments_json) {
  if (!handle) return strdup("{\"ok\":true,\"value\":{\"methods\":[]}}");
  check_owner(handle);
  if (!strcmp(method, "playing")) return strdup(handle->playing ? "true" : "false");
  if (!strcmp(method, "config")) {
    char buffer[128];
    snprintf(buffer, sizeof(buffer), "{\"fontSize\":%g,\"opacity\":%g}",
             handle->config.font_size, handle->config.opacity);
    return strdup(buffer);
  }
  return strdup("{\"ok\":true,\"value\":17}");
}
ErikaStatus erika_presenter_load_danmaku_file(
    ErikaPresenterHandle *handle,
    const char *uri) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_load_danmaku_json(
    ErikaPresenterHandle *handle,
    const char *json) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_load_upscaler_model(
    ErikaPresenterHandle *handle,
    const char *name,
    const uint8_t *data,
    uintptr_t data_len) { check_owner(handle); assert(data_len == 3 && data[0] == 1 && data[2] == 3); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_open(ErikaPresenterHandle *handle, const char *uri) { check_owner(handle); atomic_fetch_add(&commands, 1); hold_if_requested(1); handle->pending_event = 1; return ErikaStatus_Ok; }
ErikaStatus erika_presenter_open_with_headers(
    ErikaPresenterHandle *handle,
    const char *uri,
    const ErikaHttpHeader *headers,
    uintptr_t header_count) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_open_with_options(
    ErikaPresenterHandle *handle,
    const char *uri,
    const ErikaOpenOptions *options) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_pause(ErikaPresenterHandle *handle) { check_owner(handle); handle->playing = false; return ErikaStatus_Ok; }
ErikaStatus erika_presenter_play(ErikaPresenterHandle *handle) { check_owner(handle); handle->playing = true; return ErikaStatus_Ok; }
ErikaStatus erika_presenter_poll_event(ErikaPresenterHandle *handle, ErikaEvent *out_event) { check_owner(handle); if (!handle->pending_event) return ErikaStatus_NoEvent; handle->pending_event = 0; memset(out_event, 0, sizeof(*out_event)); out_event->kind = 3; out_event->position_micros = handle->position; return ErikaStatus_Ok; }
ErikaStatus erika_presenter_register_subtitle_memory_font(
    ErikaPresenterHandle *handle,
    const uint8_t *data,
    uintptr_t data_len,
    uint64_t *out_font_id) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_remove_danmaku_track(
    ErikaPresenterHandle *handle,
    uint64_t track_id) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_remove_subtitle_track(
    ErikaPresenterHandle *handle,
    int64_t track_id) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_render_tick(
    ErikaPresenterHandle *handle,
    double time_seconds,
    ErikaPresenterStats *out_stats) { check_owner(handle); atomic_fetch_add(&ticks, 1); hold_if_requested(2); memset(out_stats, 0, sizeof(*out_stats)); out_stats->rendered_test_frames = atomic_load(&ticks); handle->pending_event = 1; return ErikaStatus_Ok; }
ErikaStatus erika_presenter_render_tick_with_timing(
    ErikaPresenterHandle *handle,
    double time_seconds,
    const double *presentation_delay_seconds,
    ErikaPresenterStats *out_stats) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_resize_surface(
    ErikaPresenterHandle *handle,
    uint32_t width,
    uint32_t height,
    double scale) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_seek(ErikaPresenterHandle *handle, uint64_t position_micros) { check_owner(handle); atomic_fetch_add(&commands, 1); handle->position = position_micros; handle->pending_event = 1; return ErikaStatus_Ok; }
ErikaStatus erika_presenter_select_audio_track(
    ErikaPresenterHandle *handle,
    int64_t track_id) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_select_subtitle_memory_fonts(
    ErikaPresenterHandle *handle,
    const uint64_t *font_ids,
    uintptr_t font_count) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_select_subtitle_track(
    ErikaPresenterHandle *handle,
    int64_t track_id) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_danmaku_block_words_json(
    ErikaPresenterHandle *handle,
    const char *json) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_danmaku_config_ptr(
    ErikaPresenterHandle *handle,
    const ErikaDanmakuConfig *config) { check_owner(handle); handle->config = *config; return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_danmaku_enabled(
    ErikaPresenterHandle *handle,
    bool enabled) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_danmaku_font(
    ErikaPresenterHandle *handle,
    const char *family,
    const char *file_path) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_danmaku_global_offset(
    ErikaPresenterHandle *handle,
    int64_t offset_micros) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_danmaku_track_enabled(
    ErikaPresenterHandle *handle,
    uint64_t track_id,
    bool enabled) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_danmaku_track_offset(
    ErikaPresenterHandle *handle,
    uint64_t track_id,
    int64_t offset_micros) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_debug_hud_enabled(
    ErikaPresenterHandle *handle,
    bool enabled) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_flutter_texture_buffer(
    ErikaPresenterHandle *handle,
    uint64_t raw_texture,
    uint32_t width,
    uint32_t height) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_output_headroom(
    ErikaPresenterHandle *handle,
    float headroom,
    bool known) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_playback_rate(ErikaPresenterHandle *handle, double rate) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_subtitle_font(
    ErikaPresenterHandle *handle,
    const char *family,
    const char *file_path) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_subtitle_scale(ErikaPresenterHandle *handle, double scale) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_subtitle_style(
    ErikaPresenterHandle *handle,
    ErikaSubtitleStyle style) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_upscaler(ErikaPresenterHandle *handle, int32_t mode) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_set_volume(ErikaPresenterHandle *handle, double volume) { check_owner(handle); assert(volume == handle->next_volume++); atomic_fetch_add(&commands, 1); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_stop(ErikaPresenterHandle *handle) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_track_selection(
    ErikaPresenterHandle *handle,
    ErikaTrackSelection *out_selection) { check_owner(handle); return ErikaStatus_Ok; }
ErikaStatus erika_presenter_tracks(
    ErikaPresenterHandle *handle,
    ErikaTrackInfo *out_tracks,
    uintptr_t capacity,
    uintptr_t *out_len) { check_owner(handle); *out_len = 0; return ErikaStatus_Ok; }
void erika_string_free(char *value) { free(value); }
void erika_subtitle_memory_font_status_free(
    ErikaSubtitleMemoryFontStatus *status) {  }
void erika_track_info_free(ErikaTrackInfo *track) {  }
