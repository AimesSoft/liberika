/* Browser demux and ASS bridge. One instance per player worker. Pixel data is
 * never decoded here: compressed AV packets go to the browser's WebCodecs. */
#include <emscripten.h>
#include <libavformat/avformat.h>
#include <libavcodec/avcodec.h>
#include <libavutil/bprint.h>
#include <libavutil/pixdesc.h>
#include <ass/ass.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static AVFormatContext *format;
static AVIOContext *io;
static AVPacket *packet;
static int64_t position, source_size;
static double origin;
static ASS_Library *library;
static ASS_Renderer *renderer;
static ASS_Track *subtitles;
static int subtitle_stream = -1;
static char error_text[256];
static char *info;
static int renderer_width, renderer_height, subtitle_changed;
static size_t font_bytes;
static char default_family[1024] = "Droid Sans Fallback";

EM_ASYNC_JS(int, source_read, (double offset, int destination, int length), {
    try { return await Module.readSource(offset, destination, length); }
    catch (e) { Module.sourceError = String(e); return -5; }
});
static int read_packet(void *opaque, uint8_t *buffer, int size) {
    if (position >= source_size) return AVERROR_EOF;
    int remaining = (int)((source_size-position) < size ? source_size-position : size);
    int count = source_read((double)position, (int)(uintptr_t)buffer, remaining);
    if (count > 0) position += count;
    return count == 0 ? AVERROR_EOF : count;
}
static int64_t seek_source(void *opaque, int64_t offset, int whence) {
    if (whence == AVSEEK_SIZE) return source_size;
    whence &= ~AVSEEK_FORCE;
    int64_t next = whence == SEEK_SET ? offset : whence == SEEK_CUR ? position+offset : whence == SEEK_END ? source_size+offset : -1;
    if (next < 0 || next > source_size) return AVERROR(EINVAL);
    return position = next;
}
static int fail(int code) { av_strerror(code, error_text, sizeof(error_text)); return code; }
const char *erika_error(void) { return error_text; }
static void quote(AVBPrint *out, const char *text) {
    av_bprint_chars(out, '"', 1);
    for (const unsigned char *p = (const unsigned char *)(text ? text : ""); *p; ++p) {
        if (*p == '"' || *p == '\\') av_bprintf(out, "\\%c", *p);
        else if (*p < 32) av_bprintf(out, "\\u%04x", *p);
        else av_bprint_chars(out, *p, 1);
    }
    av_bprint_chars(out, '"', 1);
}
static const char *metadata(AVDictionary *dict, const char *key) {
    AVDictionaryEntry *entry = av_dict_get(dict, key, NULL, 0);
    return entry ? entry->value : "";
}
void erika_close(void) {
    if (subtitles) ass_free_track(subtitles);
    if (renderer) ass_renderer_done(renderer);
    if (library) ass_library_done(library);
    subtitles = NULL; renderer = NULL; library = NULL; subtitle_stream = -1;
    renderer_width = renderer_height = 0; font_bytes = 0;
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) { av_freep(&io->buffer); avio_context_free(&io); }
    av_freep(&info);
}
int erika_open(double size, const char *name) {
    erika_close(); position = 0; source_size = (int64_t)size;
    uint8_t *buffer = av_malloc(65536);
    if (!buffer) return fail(AVERROR(ENOMEM));
    io = avio_alloc_context(buffer, 65536, 0, NULL, read_packet, NULL, seek_source);
    if (!io) { av_free(buffer); return fail(AVERROR(ENOMEM)); }
    format = avformat_alloc_context();
    if (!format) return fail(AVERROR(ENOMEM));
    format->pb = io; format->flags |= AVFMT_FLAG_CUSTOM_IO;
    format->probesize = 2 * 1024 * 1024;
    format->max_analyze_duration = 3 * AV_TIME_BASE;
    int code = avformat_open_input(&format, name, NULL, NULL);
    if (code < 0) return fail(code);
    code = avformat_find_stream_info(format, NULL);
    if (code < 0) return fail(code);
    packet = av_packet_alloc();
    if (!packet) return fail(AVERROR(ENOMEM));
    origin = format->start_time == AV_NOPTS_VALUE ? 0 : (double)format->start_time / AV_TIME_BASE;
    library = ass_library_init();
    if (!library) return fail(AVERROR(ENOMEM));
    renderer = ass_renderer_init(library);
    if (!renderer) return fail(AVERROR(ENOMEM));
    ass_set_cache_limits(renderer, 2048, 64);
    ass_set_fonts(renderer, NULL, default_family, ASS_FONTPROVIDER_NONE, NULL, 1);
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        AVStream *s = format->streams[i]; AVCodecParameters *p = s->codecpar;
        if (p->codec_type == AVMEDIA_TYPE_ATTACHMENT && p->extradata_size > 0 && p->extradata_size <= 32*1024*1024 && font_bytes + p->extradata_size <= 128*1024*1024) {
            ass_add_font(library, metadata(s->metadata, "filename"), (char*)p->extradata, p->extradata_size);
            font_bytes += p->extradata_size;
        }
    }
    ass_set_fonts(renderer, NULL, default_family, ASS_FONTPROVIDER_NONE, NULL, 1);
    return 0;
}
const char *erika_info(void) {
    if (!format) return "{}";
    av_freep(&info);
    AVBPrint out; av_bprint_init(&out, 1024, 1024*1024);
    double duration = format->duration == AV_NOPTS_VALUE ? 0 : (double)format->duration/AV_TIME_BASE;
    av_bprintf(&out, "{\"duration\":%.6f,\"tracks\":[", duration);
    for (unsigned i=0; i<format->nb_streams; ++i) {
        AVStream *s=format->streams[i]; AVCodecParameters *p=s->codecpar;
        const AVPixFmtDescriptor *desc=av_pix_fmt_desc_get(p->format);
        av_bprintf(&out, "%s{\"id\":%u,\"type\":", i ? ",":"", i);
        quote(&out, av_get_media_type_string(p->codec_type));
        av_bprintf(&out, ",\"codec\":"); quote(&out, avcodec_get_name(p->codec_id));
        av_bprintf(&out, ",\"name\":"); quote(&out, metadata(s->metadata,"title"));
        av_bprintf(&out, ",\"language\":"); quote(&out, metadata(s->metadata,"language"));
        av_bprintf(&out, ",\"width\":%d,\"height\":%d,\"sampleRate\":%d,\"channels\":%d,\"profile\":%d,\"level\":%d,\"bitDepth\":%d,\"extraPtr\":%u,\"extraSize\":%d}",
            p->width,p->height,p->sample_rate,p->ch_layout.nb_channels,p->profile,p->level,desc?desc->comp[0].depth:8,(unsigned)(uintptr_t)p->extradata,p->extradata_size);
    }
    av_bprintf(&out,"],\"chapters\":[");
    for (unsigned i=0; i<format->nb_chapters; ++i) {
        AVChapter *c = format->chapters[i];
        double start = c->start * av_q2d(c->time_base) - origin;
        double end = c->end * av_q2d(c->time_base) - origin;
        av_bprintf(&out, "%s{\"title\":", i ? "," : "");
        quote(&out, metadata(c->metadata, "title"));
        av_bprintf(&out, ",\"start\":%.6f,\"end\":%.6f}", start, end);
    }
    av_bprintf(&out,"]}"); av_bprint_finalize(&out,&info); return info;
}
int erika_subtitle_select(int stream) {
    if (subtitles) ass_free_track(subtitles);
    subtitles=NULL; subtitle_stream=-1;
    if (stream < 0) return 0;
    if (!format || (unsigned)stream>=format->nb_streams) return fail(AVERROR(EINVAL));
    AVCodecParameters *p=format->streams[stream]->codecpar;
    if (p->codec_id!=AV_CODEC_ID_ASS && p->codec_id!=AV_CODEC_ID_SSA) return fail(AVERROR(ENOSYS));
    subtitles=ass_new_track(library);
    if (!subtitles) return fail(AVERROR(ENOMEM));
    ass_process_codec_private(subtitles,(char*)p->extradata,p->extradata_size);
    ass_configure_prune(subtitles, 60000);
    subtitle_stream=stream; return 0;
}
int erika_subtitle_load(char *data, int length) {
    if (subtitles) ass_free_track(subtitles);
    subtitles=ass_read_memory(library,data,length,NULL); subtitle_stream=-1;
    return subtitles ? 0 : fail(AVERROR_INVALIDDATA);
}
int erika_set_default_family(const char *family) {
    snprintf(default_family, sizeof default_family, "%s", family);
    return 0;
}
int erika_add_font(char *name, char *data, int length) {
    if (!library || length<1 || length>32*1024*1024 || font_bytes + length > 128*1024*1024) return fail(AVERROR(EINVAL));
    ass_add_font(library,name,data,length); font_bytes += length;
    ass_set_fonts(renderer,NULL,default_family,ASS_FONTPROVIDER_NONE,NULL,1); return 0;
}
int erika_read(void) {
    av_packet_unref(packet);
    int result=av_read_frame(format,packet);
    if (result==AVERROR_EOF) return 0;
    if (result<0) return fail(result);
    if (packet->stream_index==subtitle_stream && subtitles) {
        AVStream *s=format->streams[subtitle_stream];
        double pts=(packet->pts==AV_NOPTS_VALUE?0:packet->pts*av_q2d(s->time_base))-origin;
        ass_process_chunk(subtitles,(char*)packet->data,packet->size,(long long)(pts*1000),(long long)(packet->duration*av_q2d(s->time_base)*1000));
    }
    return 1;
}
uint8_t *erika_packet_data(void) { return packet->data; }
int erika_packet_size(void) { return packet->size; }
int erika_packet_stream(void) { return packet->stream_index; }
double erika_packet_time(void) { return ((packet->pts==AV_NOPTS_VALUE ? (packet->dts==AV_NOPTS_VALUE?0:packet->dts) : packet->pts)*av_q2d(format->streams[packet->stream_index]->time_base)-origin)*1000000; }
double erika_packet_duration(void) { return packet->duration*av_q2d(format->streams[packet->stream_index]->time_base)*1000000; }
int erika_packet_key(void) { return !!(packet->flags & AV_PKT_FLAG_KEY); }
int erika_seek(double seconds) {
    int result=avformat_seek_file(format,-1,INT64_MIN,(int64_t)((seconds+origin)*AV_TIME_BASE),INT64_MAX,0);
    if (result<0) return fail(result);
    av_packet_unref(packet);
    // ASS events are on an absolute timeline. Keep already observed long events
    // across seeks; libass read-order checking suppresses replayed duplicates.
    return 0;
}
ASS_Image *erika_subtitle_render(double seconds, int width, int height, double scale) {
    if (!subtitles || !renderer) return NULL;
    if (renderer_width != width || renderer_height != height) {
        ass_set_frame_size(renderer,width,height); ass_set_storage_size(renderer,width,height);
        renderer_width=width; renderer_height=height;
    }
    ass_set_font_scale(renderer,scale);
    return ass_render_frame(renderer,subtitles,(long long)(seconds*1000),&subtitle_changed);
}
ASS_Image *erika_image_next(ASS_Image *image) { return image->next; }
uint8_t *erika_image_data(ASS_Image *image) { return image->bitmap; }
int erika_image_width(ASS_Image *image) { return image->w; }
int erika_image_height(ASS_Image *image) { return image->h; }
int erika_image_stride(ASS_Image *image) { return image->stride; }
int erika_image_x(ASS_Image *image) { return image->dst_x; }
int erika_image_y(ASS_Image *image) { return image->dst_y; }
uint32_t erika_image_color(ASS_Image *image) { return image->color; }

int erika_subtitle_changed(void) { return subtitle_changed; }
