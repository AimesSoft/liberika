#include <libavcodec/avcodec.h>
#ifdef __ANDROID__
#include <libavcodec/jni.h>
#include <libavcodec/mediacodec.h>
#endif
#include <libavformat/avformat.h>
#include <libavformat/avio.h>
#include <libavutil/avutil.h>
#include <libavutil/dict.h>
#include <libavutil/dovi_meta.h>
#include <libavutil/error.h>
#include <libavutil/mastering_display_metadata.h>
#include <libavutil/mem.h>
#include <libavutil/pixdesc.h>
#ifdef __ANDROID__
#include <libavutil/hwcontext_mediacodec.h>
#endif
#ifdef _WIN32
#include <libavutil/hwcontext_d3d11va.h>
#endif
#include <libswresample/swresample.h>
#include <libswscale/swscale.h>
