#define _POSIX_C_SOURCE 200809L
#include "erika.h"
#include <X11/Xlib.h>
#include <inttypes.h>
#include <stdio.h>
#include <time.h>

static double seconds(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return now.tv_sec + now.tv_nsec / 1e9;
}

static int tick_for(ErikaPresenterHandle *player, double duration,
                    ErikaPresenterStats *stats) {
    double end = seconds() + duration;
    struct timespec delay = {0, 16000000};
    while (seconds() < end) {
        if (erika_presenter_render_tick(player, seconds(), stats) != ErikaStatus_Ok)
            return 0;
        nanosleep(&delay, NULL);
    }
    return 1;
}

/* Links the actual shared C ABI, independently of the Rust demo. */
int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: capi_smoke FILE_OR_URL\n");
        return 2;
    }
    XInitThreads();
    Display *display = XOpenDisplay(NULL);
    if (!display) {
        fprintf(stderr, "X11 display unavailable\n");
        return 1;
    }
    Window window = XCreateSimpleWindow(display, RootWindow(display, 0),
                                        0, 0, 640, 360, 0, 0, 0);
    XMapWindow(display, window);
    XSync(display, False);
    ErikaPresenterHandle *player = erika_presenter_create();
    ErikaPresenterStats stats = {0};
    int result = 1;
#define CHECK(condition) do { if (!(condition)) { \
    fprintf(stderr, "failed: %s\n", #condition); goto cleanup; } } while (0)
    CHECK(player != NULL);
    CHECK(erika_presenter_attach_wgpu_surface(player, ErikaWgpuSurfaceKind_XlibWindow,
          0, (uint64_t)(uintptr_t)display, 640, 360, 1.0) != ErikaStatus_Ok);
    CHECK(erika_presenter_attach_wgpu_surface(player, ErikaWgpuSurfaceKind_XlibWindow,
          window, (uint64_t)(uintptr_t)display, 640, 360, 1.0) == ErikaStatus_Ok);
    CHECK(erika_presenter_open(player, argv[1]) == ErikaStatus_Ok);
    CHECK(erika_presenter_play(player) == ErikaStatus_Ok);
    CHECK(tick_for(player, 3.0, &stats));
    CHECK(stats.rendered_video_frames > 0 && stats.audio_clock_read_frames > 0);
    CHECK(erika_presenter_pause(player) == ErikaStatus_Ok);
    CHECK(tick_for(player, 0.1, &stats));
    uint64_t paused_frames = stats.audio_clock_read_frames;
    CHECK(tick_for(player, 0.2, &stats));
    CHECK(stats.audio_clock_read_frames == paused_frames);
    /* EGL borrows the display. Detach must release it before the host closes
       the connection, and reattach must retain the paused video image. */
    uint64_t paused_video_frames = stats.rendered_video_frames;
    CHECK(erika_presenter_detach_surface(player) == ErikaStatus_Ok);
    XDestroyWindow(display, window);
    XCloseDisplay(display);
    window = 0;
    display = XOpenDisplay(NULL);
    CHECK(display != NULL);
    window = XCreateSimpleWindow(display, RootWindow(display, 0),
                                0, 0, 640, 360, 0, 0, 0);
    XMapWindow(display, window);
    XSync(display, False);
    CHECK(erika_presenter_attach_wgpu_surface(player, ErikaWgpuSurfaceKind_XlibWindow,
          window, (uint64_t)(uintptr_t)display, 640, 360, 1.0) == ErikaStatus_Ok);
    CHECK(tick_for(player, 0.3, &stats));
    CHECK(stats.audio_clock_read_frames == paused_frames);
    CHECK(stats.rendered_video_frames > paused_video_frames);
    CHECK(erika_presenter_seek(player, 1000000) == ErikaStatus_Ok);
    XResizeWindow(display, window, 320, 180);
    XSync(display, False);
    CHECK(erika_presenter_resize_surface(player, 320, 180, 1.0) == ErikaStatus_Ok);
    CHECK(erika_presenter_play(player) == ErikaStatus_Ok);
    CHECK(tick_for(player, 3.0, &stats));
    CHECK(stats.audio_clock_read_frames > paused_frames);
    CHECK(stats.render_failures == 0 && stats.audio_failures == 0);
    printf("C ABI: video=%" PRIu64 " audio=%" PRIu64 " errors=%" PRIu64 "/%" PRIu64 "\n",
           stats.rendered_video_frames, stats.audio_clock_read_frames,
           stats.render_failures, stats.audio_failures);
    CHECK(erika_presenter_close(player) == ErikaStatus_Ok);
    CHECK(erika_presenter_detach_surface(player) == ErikaStatus_Ok);
    result = 0;
cleanup:
    if (result) {
        char *error = erika_last_error_message();
        fprintf(stderr, "Erika: %s\n", error ? error : "no error detail");
        erika_string_free(error);
    }
    if (player)
        erika_presenter_destroy(player);
    if (display) {
        if (window)
            XDestroyWindow(display, window);
        XCloseDisplay(display);
    }
    return result;
}
