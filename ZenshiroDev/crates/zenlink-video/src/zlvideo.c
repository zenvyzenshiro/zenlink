/*
 * zlvideo: ZenLink Linux Native Video Decoder using FFmpeg via dlopen
 * Decodes H.264 / HEVC NAL units to 32-bit BGRA frames.
 * Dynamically loads FFmpeg libraries at runtime (supporting FFmpeg 4, 5, 6, 7+)
 * to avoid hard ABI coupling with specific distribution versions (e.g. libavcodec.so.60 vs .61).
 */
#include <dlfcn.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#include <libavcodec/avcodec.h>
#include <libavutil/dict.h>
#include <libavutil/imgutils.h>
#include <libavutil/mem.h>
#include <libavutil/opt.h>
#include <libavutil/pixdesc.h>
#include <libswscale/swscale.h>

/* Function pointer definitions */
typedef const AVCodec *(*fn_avcodec_find_decoder)(enum AVCodecID);
typedef AVCodecContext *(*fn_avcodec_alloc_context3)(const AVCodec *);
typedef AVPacket *(*fn_av_packet_alloc)(void);
typedef void (*fn_av_packet_free)(AVPacket **);
typedef int (*fn_av_new_packet)(AVPacket *, int);
typedef void (*fn_av_packet_unref)(AVPacket *);
typedef int (*fn_avcodec_open2)(AVCodecContext *, const AVCodec *, AVDictionary **);
typedef int (*fn_avcodec_send_packet)(AVCodecContext *, const AVPacket *);
typedef int (*fn_avcodec_receive_frame)(AVCodecContext *, AVFrame *);
typedef void (*fn_avcodec_free_context)(AVCodecContext **);

typedef AVFrame *(*fn_av_frame_alloc)(void);
typedef void (*fn_av_frame_free)(AVFrame **);
typedef void (*fn_av_frame_unref)(AVFrame *);
typedef void *(*fn_av_mallocz)(size_t);
typedef void (*fn_av_free)(void *);
typedef int (*fn_av_dict_set)(AVDictionary **, const char *, const char *, int);
typedef void (*fn_av_dict_free)(AVDictionary **);

typedef struct SwsContext *(*fn_sws_getCachedContext)(struct SwsContext *, int, int, enum AVPixelFormat,
                                                      int, int, enum AVPixelFormat, int,
                                                      SwsFilter *, SwsFilter *, const double *);
typedef int (*fn_sws_scale)(struct SwsContext *, const uint8_t *const [], const int [], int, int,
                            uint8_t *const [], const int []);
typedef void (*fn_sws_freeContext)(struct SwsContext *);

static fn_avcodec_find_decoder p_avcodec_find_decoder = NULL;
static fn_avcodec_alloc_context3 p_avcodec_alloc_context3 = NULL;
typedef AVPacket *(*fn_av_packet_alloc_t)(void);
static fn_av_packet_alloc_t p_av_packet_alloc = NULL;
static fn_av_packet_free p_av_packet_free = NULL;
static fn_av_new_packet p_av_new_packet = NULL;
static fn_av_packet_unref p_av_packet_unref = NULL;
static fn_avcodec_open2 p_avcodec_open2 = NULL;
static fn_avcodec_send_packet p_avcodec_send_packet = NULL;
static fn_avcodec_receive_frame p_avcodec_receive_frame = NULL;
static fn_avcodec_free_context p_avcodec_free_context = NULL;

static fn_av_frame_alloc p_av_frame_alloc = NULL;
static fn_av_frame_free p_av_frame_free = NULL;
static fn_av_frame_unref p_av_frame_unref = NULL;
static fn_av_mallocz p_av_mallocz = NULL;
static fn_av_free p_av_free = NULL;
static fn_av_dict_set p_av_dict_set = NULL;
static fn_av_dict_free p_av_dict_free = NULL;

static fn_sws_getCachedContext p_sws_getCachedContext = NULL;
static fn_sws_scale p_sws_scale = NULL;
static fn_sws_freeContext p_sws_freeContext = NULL;

static void *load_first(const char *const names[])
{
    for (int i = 0; names[i]; i++) {
        void *h = dlopen(names[i], RTLD_NOW | RTLD_GLOBAL);
        if (h)
            return h;
    }
    return NULL;
}

static int init_ffmpeg(void)
{
    static int initialized = 0;
    static int available = 0;
    if (initialized)
        return available ? 0 : -1;
    initialized = 1;

    static const char *const util_names[] = {
        "libavutil.so.59", "libavutil.so.58", "libavutil.so.57", "libavutil.so.56", "libavutil.so", NULL
    };
    static const char *const sws_names[] = {
        "libswscale.so.8", "libswscale.so.7", "libswscale.so.6", "libswscale.so.5", "libswscale.so", NULL
    };
    static const char *const codec_names[] = {
        "libavcodec.so.61", "libavcodec.so.60", "libavcodec.so.59", "libavcodec.so.58", "libavcodec.so", NULL
    };

    void *h_util = load_first(util_names);
    void *h_sws = load_first(sws_names);
    void *h_codec = load_first(codec_names);

    if (!h_util || !h_sws || !h_codec) {
        return -1;
    }

    #define BIND(h, ptr, type, name) do { \
        ptr = (type)dlsym(h, name); \
        if (!ptr) return -1; \
    } while (0)

    BIND(h_util, p_av_frame_alloc, fn_av_frame_alloc, "av_frame_alloc");
    BIND(h_util, p_av_frame_free, fn_av_frame_free, "av_frame_free");
    BIND(h_util, p_av_frame_unref, fn_av_frame_unref, "av_frame_unref");
    BIND(h_util, p_av_mallocz, fn_av_mallocz, "av_mallocz");
    BIND(h_util, p_av_free, fn_av_free, "av_free");
    BIND(h_util, p_av_dict_set, fn_av_dict_set, "av_dict_set");
    BIND(h_util, p_av_dict_free, fn_av_dict_free, "av_dict_free");

    BIND(h_sws, p_sws_getCachedContext, fn_sws_getCachedContext, "sws_getCachedContext");
    BIND(h_sws, p_sws_scale, fn_sws_scale, "sws_scale");
    BIND(h_sws, p_sws_freeContext, fn_sws_freeContext, "sws_freeContext");

    BIND(h_codec, p_avcodec_find_decoder, fn_avcodec_find_decoder, "avcodec_find_decoder");
    BIND(h_codec, p_avcodec_alloc_context3, fn_avcodec_alloc_context3, "avcodec_alloc_context3");
    BIND(h_codec, p_av_packet_alloc, fn_av_packet_alloc_t, "av_packet_alloc");
    BIND(h_codec, p_av_packet_free, fn_av_packet_free, "av_packet_free");
    BIND(h_codec, p_av_new_packet, fn_av_new_packet, "av_new_packet");
    BIND(h_codec, p_av_packet_unref, fn_av_packet_unref, "av_packet_unref");
    BIND(h_codec, p_avcodec_open2, fn_avcodec_open2, "avcodec_open2");
    BIND(h_codec, p_avcodec_send_packet, fn_avcodec_send_packet, "avcodec_send_packet");
    BIND(h_codec, p_avcodec_receive_frame, fn_avcodec_receive_frame, "avcodec_receive_frame");
    BIND(h_codec, p_avcodec_free_context, fn_avcodec_free_context, "avcodec_free_context");

    #undef BIND

    available = 1;
    return 0;
}

typedef struct ZlVideo {
    AVCodecContext *ctx;
    AVPacket *pkt;
    AVFrame *frame;
    struct SwsContext *sws;
    int sws_w, sws_h, sws_fmt;
    uint8_t *bgra_buf;
    int bgra_cap;
} ZlVideo;

int zl_video_codec_available(int is_hevc)
{
    if (init_ffmpeg() < 0)
        return 0;
    enum AVCodecID id = is_hevc ? AV_CODEC_ID_HEVC : AV_CODEC_ID_H264;
    return p_avcodec_find_decoder(id) != NULL ? 1 : 0;
}

void zl_video_close(ZlVideo *v)
{
    if (!v)
        return;
    if (v->sws && p_sws_freeContext)
        p_sws_freeContext(v->sws);
    if (v->bgra_buf && p_av_free)
        p_av_free(v->bgra_buf);
    if (v->frame && p_av_frame_free)
        p_av_frame_free(&v->frame);
    if (v->pkt && p_av_packet_free)
        p_av_packet_free(&v->pkt);
    if (v->ctx && p_avcodec_free_context)
        p_avcodec_free_context(&v->ctx);
    free(v);
}

ZlVideo *zl_video_open(int is_hevc)
{
    if (init_ffmpeg() < 0)
        return NULL;

    enum AVCodecID id = is_hevc ? AV_CODEC_ID_HEVC : AV_CODEC_ID_H264;
    const AVCodec *codec = p_avcodec_find_decoder(id);
    if (!codec)
        return NULL;

    ZlVideo *v = calloc(1, sizeof(*v));
    if (!v)
        return NULL;

    v->ctx = p_avcodec_alloc_context3(codec);
    v->pkt = p_av_packet_alloc();
    v->frame = p_av_frame_alloc();
    if (!v->ctx || !v->pkt || !v->frame)
        goto fail;

    /* Use dictionary options to configure low delay and thread count safely across FFmpeg versions */
    AVDictionary *opts = NULL;
    if (p_av_dict_set) {
        p_av_dict_set(&opts, "threads", "1", 0);
        p_av_dict_set(&opts, "flags", "low_delay", 0);
        p_av_dict_set(&opts, "flags2", "fast", 0);
    }

    if (p_avcodec_open2(v->ctx, codec, &opts) < 0) {
        if (opts && p_av_dict_free)
            p_av_dict_free(&opts);
        goto fail;
    }
    if (opts && p_av_dict_free)
        p_av_dict_free(&opts);

    return v;

fail:
    zl_video_close(v);
    return NULL;
}

int zl_video_decode(ZlVideo *v, const uint8_t *data, int len, uint8_t **out_bgra, int *out_w, int *out_h)
{
    int ret;
    if (!v || !data || len <= 0 || !out_bgra || !out_w || !out_h)
        return -1;

    if (p_av_new_packet(v->pkt, len) < 0)
        return -1;
    memcpy(v->pkt->data, data, len);

    ret = p_avcodec_send_packet(v->ctx, v->pkt);
    p_av_packet_unref(v->pkt);
    if (ret < 0 && ret != AVERROR(EAGAIN))
        return -1;

    ret = p_avcodec_receive_frame(v->ctx, v->frame);
    if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF)
        return 0; // needs more data
    if (ret < 0)
        return -1;

    int w = v->frame->width;
    int h = v->frame->height;
    int fmt = v->frame->format;

    if (w <= 0 || h <= 0 || w > 8192 || h > 8192 || fmt < 0 || !v->frame->data[0]) {
        p_av_frame_unref(v->frame);
        return -1;
    }

    if (!v->sws || v->sws_w != w || v->sws_h != h || v->sws_fmt != fmt) {
        v->sws = p_sws_getCachedContext(v->sws, w, h, (enum AVPixelFormat)fmt,
                                        w, h, AV_PIX_FMT_RGBA,
                                        SWS_FAST_BILINEAR, NULL, NULL, NULL);
        if (!v->sws) {
            p_av_frame_unref(v->frame);
            return -1;
        }
        v->sws_w = w;
        v->sws_h = h;
        v->sws_fmt = fmt;
    }

    int needed = w * h * 4;
    int alloc_size = needed + AV_INPUT_BUFFER_PADDING_SIZE + 512;
    if (alloc_size > v->bgra_cap) {
        if (v->bgra_buf)
            p_av_free(v->bgra_buf);
        v->bgra_buf = p_av_mallocz(alloc_size);
        if (!v->bgra_buf) {
            v->bgra_cap = 0;
            p_av_frame_unref(v->frame);
            return -1;
        }
        v->bgra_cap = alloc_size;
    }

    uint8_t *dst[4] = { v->bgra_buf, NULL, NULL, NULL };
    int dst_linesize[4] = { w * 4, 0, 0, 0 };

    p_sws_scale(v->sws, (const uint8_t * const *)v->frame->data, v->frame->linesize,
                0, h, dst, dst_linesize);

    p_av_frame_unref(v->frame);

    *out_bgra = v->bgra_buf;
    *out_w = w;
    *out_h = h;
    return 1;
}
