#define _POSIX_C_SOURCE 200809L
#include "erika.h"
#include <X11/Xlib.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}
static int compare(const void *a, const void *b) {
    double x = *(const double*)a, y = *(const double*)b;
    return (x > y) - (x < y);
}

// Measure the actual C ABI import + draw time, excluding an explicit warmup.
// Hardware/import counts are unique source frames; rendered_video_frames can
// include redraws and must not be used as a source-frame throughput metric.
int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: capi_performance VIDEO WARMUP_SECONDS MEASURE_SECONDS\n");
        return 2;
    }
    double warmup = atof(argv[2]), duration = atof(argv[3]);
    if (warmup < 0 || duration <= 0 || duration > 300) return 2;
    XInitThreads();
    Display *display = XOpenDisplay(NULL);
    if (!display) return 2;
    Window window = XCreateSimpleWindow(display, RootWindow(display, 0), 0, 0, 1920, 1080, 0, 0, 0);
    XStoreName(display, window, "Erika playback performance");
    XMapWindow(display, window);
    XSync(display, False);
    ErikaPresenterHandle *player = erika_presenter_create();
    int result = 1;
    double samples[100000];
    size_t count = 0, over_budget = 0;
    ErikaPresenterStats stats = {0}, first = {0};
    if (!player || erika_presenter_attach_wgpu_surface(player, ErikaWgpuSurfaceKind_XlibWindow,
            window, (uint64_t)(uintptr_t)display, 1920, 1080, 1.0) != ErikaStatus_Ok
        || erika_presenter_open(player, argv[1]) != ErikaStatus_Ok
        || erika_presenter_play(player) != ErikaStatus_Ok) goto done;
    double start = now(), measured_start = 0;
    while (now() - start < warmup + duration) {
        while (XPending(display)) { XEvent event; XNextEvent(display, &event); }
        double before = now();
        if (erika_presenter_render_tick(player, before - start, &stats) != ErikaStatus_Ok) goto done;
        double elapsed = (now() - before) * 1000;
        if (before - start >= warmup) {
            if (!measured_start) { first = stats; measured_start = now(); }
            else if (count < sizeof(samples) / sizeof(samples[0])) {
                samples[count++] = elapsed;
                if (elapsed > 1000.0 / 60.0) over_budget++;
            }
        }
        // Poll at up to 120 Hz; do not limit 60 fps input to 30/59 fps through
        // a fixed sleep added on top of render time.
        double wait = 1.0 / 120.0 - (now() - before);
        if (wait > 0) { struct timespec t = {0, (long)(wait * 1e9)}; nanosleep(&t, NULL); }
    }
    double seconds = now() - measured_start;
    if (!count || seconds <= 0) goto done;
    qsort(samples, count, sizeof(samples[0]), compare);
    printf("{\"seconds\":%.3f,\"ticks\":%zu,\"tickP50Ms\":%.3f,\"tickP95Ms\":%.3f,\"tickP99Ms\":%.3f,\"tickMaxMs\":%.3f,\"ticksOver16_67Ms\":%zu,"
           "\"hardwareFrames\":%"PRIu64",\"sourceFps\":%.3f,\"cpuTransfers\":%"PRIu64",\"directFrames\":%"PRIu64",\"backpressureDrops\":%"PRIu64","
           "\"audioReadFrames\":%"PRIu64",\"audioUnderflowFrames\":%"PRIu64",\"renderErrors\":%"PRIu64",\"audioErrors\":%"PRIu64"}\n",
        seconds, count, samples[count/2], samples[count*95/100], samples[count*99/100], samples[count-1], over_budget,
        stats.hardware_video_frames-first.hardware_video_frames,
        (stats.hardware_video_frames-first.hardware_video_frames)/seconds,
        stats.cpu_video_frame_fallbacks-first.cpu_video_frame_fallbacks,
        stats.direct_zero_copy_video_frames-first.direct_zero_copy_video_frames,
        stats.video_frame_backpressure_drops-first.video_frame_backpressure_drops,
        stats.audio_clock_read_frames-first.audio_clock_read_frames,
        stats.audio_clock_underflow_frames-first.audio_clock_underflow_frames,
        stats.render_failures-first.render_failures, stats.audio_failures-first.audio_failures);
    result = stats.render_failures || stats.audio_failures || !stats.hardware_video_frames;
done:
    if (result) { char *error = erika_last_error_message(); fprintf(stderr, "benchmark failed: %s\n", error ? error : "missing frames"); erika_string_free(error); }
    if (player) erika_presenter_destroy(player);
    XDestroyWindow(display, window); XCloseDisplay(display);
    return result;
}
