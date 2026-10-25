#include <stdarg.h>
#include <stddef.h>

/*
 * FFmpeg av_log bridge.
 *
 * av_log callbacks receive a va_list, which stable Rust cannot consume, so the
 * formatting happens here. The callback is process-wide and runs on whichever
 * thread logs (demux, decoder and frame threads of every player), so it keeps
 * to a stack buffer and hands the formatted line to a Rust sink that never
 * blocks. Every message is still forwarded to av_log_default_callback, which
 * keeps FFmpeg's existing stderr output, level filtering and repeat
 * suppression exactly as before.
 *
 * The declarations below are FFmpeg's public libavutil/log.h API; they are
 * spelled out (like libass_log_bridge.c does for libass) so this file builds
 * without the FFmpeg include path.
 */

extern void av_log_set_callback(void (*callback)(void *, int, const char *, va_list));
extern void av_log_default_callback(void *avcl, int level, const char *fmt, va_list vl);
extern int av_log_format_line2(
    void *ptr,
    int level,
    const char *fmt,
    va_list vl,
    char *line,
    int line_size,
    int *print_prefix);

#define ERIKA_AV_LOG_WARNING 24
#define ERIKA_FFMPEG_LOG_LINE_BYTES 1024

typedef void (*ErikaFfmpegLogSink)(int level, const char *line);

static ErikaFfmpegLogSink erika_ffmpeg_log_sink = NULL;

static void erika_ffmpeg_log_callback(void *avcl, int level, const char *fmt, va_list vl) {
    ErikaFfmpegLogSink sink = erika_ffmpeg_log_sink;
    /* Bits above the low byte carry AV_LOG_C() colour hints. */
    int severity = level >= 0 ? (level & 0xff) : level;
    if (sink && fmt && severity >= 0 && severity <= ERIKA_AV_LOG_WARNING) {
        char line[ERIKA_FFMPEG_LOG_LINE_BYTES];
        int print_prefix = 1;
        va_list copy;
        va_copy(copy, vl);
        int written = av_log_format_line2(avcl, level, fmt, copy, line, (int) sizeof(line), &print_prefix);
        va_end(copy);
        if (written >= 0) {
            line[sizeof(line) - 1] = '\0';
            sink(severity, line);
        }
    }
    av_log_default_callback(avcl, level, fmt, vl);
}

/* Installs the bridge once per process. The sink is set before the callback
 * is published and never changes afterwards. */
void erika_ffmpeg_install_log_bridge(ErikaFfmpegLogSink sink) {
    if (!sink)
        return;
    erika_ffmpeg_log_sink = sink;
    av_log_set_callback(erika_ffmpeg_log_callback);
}
