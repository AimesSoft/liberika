#define _POSIX_C_SOURCE 200809L
#include "erika.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

// Software frames must not silently bypass either strict import requirement.
// Run each mode in a fresh process so process-wide environment is isolated.
int main(int argc, char **argv) {
    if (argc != 3 || (strcmp(argv[1], "gpu") && strcmp(argv[1], "zero"))) return 2;
    const int zero = !strcmp(argv[1], "zero");
    const char *expected = zero ? "direct zero-copy" : "GPU-only decoded frames";
    setenv("ERIKA_HWDEC", "software", 1);
    setenv("ERIKA_REQUIRE_HARDWARE_DECODE", "0", 1);
    setenv(zero ? "ERIKA_REQUIRE_ZERO_COPY" : "ERIKA_REQUIRE_GPU_FRAMES", "1", 1);
    ErikaPresenterHandle *player = erika_presenter_create();
    if (!player) return 1;
    int result = 1;
    if (erika_presenter_attach_flutter_texture(player,
            ErikaFlutterTextureKind_LinuxTextureRegistrar, 1, 160, 90, 1) != ErikaStatus_Ok
        || erika_presenter_open(player, argv[2]) != ErikaStatus_Ok
        || erika_presenter_play(player) != ErikaStatus_Ok) goto done;
    for (int i = 0; i < 500; ++i) {
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        ErikaPresenterStats stats = {0};
        ErikaStatus status = erika_presenter_render_tick(player,
            now.tv_sec + now.tv_nsec / 1e9, &stats);
        if (status != ErikaStatus_Ok) {
            char *error = erika_last_error_message();
            if (error && strstr(error, expected)) {
                printf("strict %s rejected CPU frame: %s\n", argv[1], error);
                result = 0;
            } else fprintf(stderr, "unexpected error: %s\n", error ? error : "none");
            erika_string_free(error);
            break;
        }
        if (stats.rendered_video_frames) {
            fprintf(stderr, "strict mode incorrectly rendered a CPU frame\n");
            break;
        }
        // Presenter import failures are normally delivered asynchronously as
        // error events; a successful tick alone does not mean a frame rendered.
        char *event;
        while ((event = erika_presenter_poll_event_json(player)) != NULL) {
            const int rejected = strstr(event, expected) != NULL;
            erika_string_free(event);
            if (rejected) {
                printf("strict %s rejected CPU frame via error event\n", argv[1]);
                result = 0;
                goto done;
            }
        }
        const struct timespec delay = {0, 10000000};
        nanosleep(&delay, NULL);
    }
done:
    erika_presenter_destroy(player);
    return result;
}
