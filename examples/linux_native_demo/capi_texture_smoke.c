#include "erika.h"
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static void check(ErikaStatus s) {
  if (s == ErikaStatus_Ok) return;
  char *message = erika_last_error_message();
  fprintf(stderr, "Erika error %d: %s\n", s, message ? message : "");
  erika_string_free(message); exit(1);
}
static double now(void) {
  struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec + ts.tv_nsec / 1e9;
}
int main(int argc, char **argv) {
  if (argc != 2) { fprintf(stderr, "usage: capi_texture_smoke VIDEO\n"); return 2; }
  ErikaPresenterHandle *p = erika_presenter_create(); assert(p);
  check(erika_presenter_attach_flutter_texture(p, ErikaFlutterTextureKind_LinuxTextureRegistrar, 1, 640, 360, 1));
  check(erika_presenter_open(p, argv[1])); check(erika_presenter_play(p));
  unsigned char *rgba = malloc(640 * 360 * 4); assert(rgba);
  unsigned copied = 0, changed = 0; unsigned long previous = 0;
  int paused = 0, resumed = 0, resized = 0, plain_paused = 0, plain_resumed = 0;
  uint64_t audio_at_pause = 0;
  const double resume_at = getenv("ERIKA_TEST_LONG_PAUSE") ? 17.0 : 5.0;
  const double start = now(); ErikaPresenterStats stats = {0};
  while (now() - start < resume_at + 5.0) {
    double elapsed = now() - start;
    if (!paused && elapsed > 2) { check(erika_presenter_pause(p)); paused = 1; }
    if (!resumed && elapsed > 2.3) {
      check(erika_presenter_detach_surface(p));
      check(erika_presenter_attach_flutter_texture(p, ErikaFlutterTextureKind_LinuxTextureRegistrar, 2, 640, 360, 1));
      check(erika_presenter_seek(p, 3000000)); check(erika_presenter_play(p)); resumed = 1;
    }
    if (!resized && elapsed > 4) { check(erika_presenter_resize_surface(p, 480, 270, 1)); resized = 1; }
    if (!plain_paused && elapsed > 4.5) {
      check(erika_presenter_pause(p)); plain_paused = 1;
      audio_at_pause = stats.audio_clock_read_frames;
    }
    if (!plain_resumed && elapsed > resume_at) {
      check(erika_presenter_play(p)); plain_resumed = 1;
    }
    check(erika_presenter_render_tick(p, elapsed, &stats));
    uint32_t w = 0, h = 0;
    ErikaStatus status = erika_presenter_copy_flutter_frame_rgba(p, rgba, 640 * 360 * 4, &w, &h);
    if (status == ErikaStatus_Ok) {
      assert(w == (resized ? 480 : 640) && h == (resized ? 270 : 360));
      unsigned long checksum = 0;
      for (unsigned i = 0; i < w * h * 4; i += 97) checksum = checksum * 33 + rgba[i];
      if (copied && checksum != previous) ++changed;
      previous = checksum; ++copied;
    } else if (status != ErikaStatus_NoEvent) check(status);
    ErikaEvent event;
    while (erika_presenter_poll_event(p, &event) == ErikaStatus_Ok) {
      if (event.kind == ErikaEventKind_Error) check(ErikaStatus_PlayerError);
    }
    usleep(16000);
  }
  printf("texture_frames=%u changed_frames=%u hardware=%lu software=%lu render_errors=%lu audio_errors=%lu audio_frames=%lu\n",
    copied, changed, stats.hardware_video_frames, stats.software_video_frames,
    stats.render_failures, stats.audio_failures, stats.audio_clock_read_frames);
  assert(copied > 30 && changed > 20 && stats.hardware_video_frames > 30 && stats.software_video_frames == 0);
  assert(stats.render_failures == 0 && stats.audio_failures == 0 && stats.audio_clock_read_frames > 100000);
  assert(plain_resumed && stats.audio_clock_read_frames > audio_at_pause + 24000);
  check(erika_presenter_detach_surface(p)); erika_presenter_destroy(p); free(rgba);
  return 0;
}
