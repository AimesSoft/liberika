#include <pulse/pulseaudio.h>
#include <stdint.h>
#include <stdlib.h>

typedef void (*ErikaPulseFill)(void *, float *, size_t);

typedef struct ErikaPulse {
    pa_threaded_mainloop *loop;
    pa_context *context;
    pa_stream *stream;
    ErikaPulseFill fill;
    void *userdata;
} ErikaPulse;

static void context_changed(pa_context *context, void *data) {
    (void)context;
    pa_threaded_mainloop_signal(((ErikaPulse *)data)->loop, 0);
}

static void stream_changed(pa_stream *stream, void *data) {
    (void)stream;
    pa_threaded_mainloop_signal(((ErikaPulse *)data)->loop, 0);
}

static void write_audio(pa_stream *stream, size_t requested, void *data) {
    ErikaPulse *output = data;
    void *buffer = NULL;
    size_t length = requested;
    if (pa_stream_begin_write(stream, &buffer, &length) < 0)
        return;
    if (length == 0) {
        pa_stream_cancel_write(stream);
        return;
    }
    output->fill(output->userdata, buffer, length / sizeof(float));
    pa_stream_write(stream, buffer, length, NULL, 0, PA_SEEK_RELATIVE);
}

void erika_pulse_close(ErikaPulse *output) {
    if (!output)
        return;
    /* Joining the mainloop guarantees no callback can use Rust's userdata
       after the caller releases its Box. */
    if (output->loop)
        pa_threaded_mainloop_stop(output->loop);
    if (output->stream) {
        pa_stream_disconnect(output->stream);
        pa_stream_unref(output->stream);
    }
    if (output->context) {
        pa_context_disconnect(output->context);
        pa_context_unref(output->context);
    }
    if (output->loop)
        pa_threaded_mainloop_free(output->loop);
    free(output);
}

ErikaPulse *erika_pulse_open(uint32_t rate, uint8_t channels,
                           ErikaPulseFill fill, void *userdata, int *error) {
    ErikaPulse *output = calloc(1, sizeof(*output));
    *error = PA_ERR_INTERNAL;
    if (!output)
        return NULL;
    output->fill = fill;
    output->userdata = userdata;
    output->loop = pa_threaded_mainloop_new();
    if (!output->loop)
        goto fail;
    output->context = pa_context_new(pa_threaded_mainloop_get_api(output->loop), "Erika");
    if (!output->context)
        goto fail;
    pa_context_set_state_callback(output->context, context_changed, output);
    if (pa_threaded_mainloop_start(output->loop) < 0)
        goto fail;
    pa_threaded_mainloop_lock(output->loop);
    if (pa_context_connect(output->context, NULL, PA_CONTEXT_NOAUTOSPAWN, NULL) < 0)
        goto fail_locked;
    while (pa_context_get_state(output->context) != PA_CONTEXT_READY) {
        if (!PA_CONTEXT_IS_GOOD(pa_context_get_state(output->context)))
            goto fail_locked;
        pa_threaded_mainloop_wait(output->loop);
    }
    pa_sample_spec spec = { PA_SAMPLE_FLOAT32NE, rate, channels };
    output->stream = pa_stream_new(output->context, "Playback", &spec, NULL);
    if (!output->stream)
        goto fail_locked;
    pa_stream_set_state_callback(output->stream, stream_changed, output);
    pa_stream_set_write_callback(output->stream, write_audio, output);
    pa_buffer_attr attr = {
        .maxlength = (uint32_t)-1,
        .tlength = (uint32_t)pa_usec_to_bytes(40000, &spec),
        .prebuf = 0,
        .minreq = (uint32_t)pa_usec_to_bytes(10000, &spec),
        .fragsize = (uint32_t)-1,
    };
    if (pa_stream_connect_playback(output->stream, NULL, &attr,
            PA_STREAM_START_CORKED | PA_STREAM_INTERPOLATE_TIMING |
            PA_STREAM_AUTO_TIMING_UPDATE | PA_STREAM_ADJUST_LATENCY,
            NULL, NULL) < 0)
        goto fail_locked;
    while (pa_stream_get_state(output->stream) != PA_STREAM_READY) {
        if (!PA_STREAM_IS_GOOD(pa_stream_get_state(output->stream)) ||
            !PA_CONTEXT_IS_GOOD(pa_context_get_state(output->context)))
            goto fail_locked;
        pa_threaded_mainloop_wait(output->loop);
    }
    pa_threaded_mainloop_unlock(output->loop);
    *error = 0;
    return output;

fail_locked:
    *error = pa_context_errno(output->context);
    pa_threaded_mainloop_unlock(output->loop);
fail:
    erika_pulse_close(output);
    return NULL;
}

/* All operations and callbacks share the threaded mainloop lock. */
int erika_pulse_control(ErikaPulse *output, int running, int flush) {
    pa_threaded_mainloop_lock(output->loop);
    /* Cork/flush are ordered by PulseAudio's protocol. Waiting synchronously
       for a remote sink's acknowledgement stalls the presenter, starving the
       very audio queue needed to resume after a long pause. The Rust active
       flag gates the callback immediately; report connection failures through
       erika_pulse_status instead of blocking the GTK/render thread here. */
    pa_operation *operation = flush
        ? pa_stream_flush(output->stream, NULL, NULL)
        : pa_stream_cork(output->stream, !running, NULL, NULL);
    int error = operation ? 0 : pa_context_errno(output->context);
    if (operation) pa_operation_unref(operation);
    else if (error == 0)
        error = PA_ERR_BADSTATE;
    pa_threaded_mainloop_unlock(output->loop);
    return error;
}

uint64_t erika_pulse_latency(ErikaPulse *output) {
    pa_usec_t latency = 0;
    int negative = 0;
    pa_threaded_mainloop_lock(output->loop);
    if (pa_stream_get_latency(output->stream, &latency, &negative) < 0 || negative)
        latency = 0;
    pa_threaded_mainloop_unlock(output->loop);
    return latency;
}

int erika_pulse_status(ErikaPulse *output) {
    pa_threaded_mainloop_lock(output->loop);
    int error = 0;
    if (pa_context_get_state(output->context) != PA_CONTEXT_READY ||
        pa_stream_get_state(output->stream) != PA_STREAM_READY) {
        error = pa_context_errno(output->context);
        if (error == 0)
            error = PA_ERR_BADSTATE;
    }
    pa_threaded_mainloop_unlock(output->loop);
    return error;
}
