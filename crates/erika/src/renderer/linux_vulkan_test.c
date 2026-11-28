// Included only with ERIKA_TEST_LINUX_VULKAN=1. Creates known decoder-like
// images on the real Vulkan device, without requiring a VA-API decoder.
// Pixel upload here is fixture setup, never part of the production import.
ErikaLinuxVkLease *erika_linux_vk_test_direct(ErikaLinuxVk *s, int p010,
        uint64_t images[2], AVFrame **producer) {
    AVBufferRef *ref = av_hwframe_ctx_alloc(s->device_ref);
    AVFrame *cpu = av_frame_alloc(), *gpu = av_frame_alloc();
    int ret = AVERROR(ENOMEM);
    if (!ref || !cpu || !gpu) goto done;
    AVHWFramesContext *fc = (AVHWFramesContext*)ref->data;
    fc->format = AV_PIX_FMT_VULKAN;
    fc->sw_format = p010 ? AV_PIX_FMT_P010 : AV_PIX_FMT_NV12;
    fc->width = 16; fc->height = 8;
    AVVulkanFramesContext *vfc = fc->hwctx;
    vfc->flags = AV_VK_FRAME_FLAG_DISABLE_MULTIPLANE;
    vfc->usage = VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    ret = av_hwframe_ctx_init(ref);
    if (ret < 0) goto done;
    ret = av_hwframe_get_buffer(ref, gpu, 0);
    if (ret < 0) goto done;
    cpu->format = fc->sw_format; cpu->width = fc->width; cpu->height = fc->height;
    ret = av_frame_get_buffer(cpu, 0);
    if (ret < 0) goto done;
    for (int plane = 0; plane < 2; plane++) {
        for (int y = 0; y < (plane ? 4 : 8); y++) {
            for (int x = 0; x < 16; x++) {
                int value = plane ? (x % 2 ? 170 : 90) : 32 + x * 12;
                uint8_t *row = cpu->data[plane] + y * cpu->linesize[plane];
                if (p010) ((uint16_t*)row)[x] = (uint16_t)(value << 8);
                else row[x] = (uint8_t)value;
            }
        }
    }
    ret = av_hwframe_transfer_data(gpu, cpu, 0);
    if (ret < 0) goto done;
    *producer = av_frame_clone(gpu);
    if (!*producer) { ret = AVERROR(ENOMEM); goto done; }
    av_frame_free(&cpu); av_buffer_unref(&ref);
    return direct_acquire(s, gpu, 0, images);
done:
    fail(s, "create direct-sampling fixture", ret);
    av_frame_free(&cpu); av_frame_free(&gpu); av_buffer_unref(&ref);
    return NULL;
}

int erika_linux_vk_test_released(ErikaLinuxVk *s, AVFrame *producer) {
    AVVkFrame *frame = (AVVkFrame*)producer->data[0];
    // transfer upload + acquire + release must all have advanced the timeline.
    for (int i = 0; i < 2; i++) {
        if (frame->sem_value[i] < 3 || frame->layout[i] != VK_IMAGE_LAYOUT_GENERAL)
            return fail(s, "direct fixture release state", AVERROR(EINVAL));
    }
    AVFrame *cpu = av_frame_alloc();
    if (!cpu) return AVERROR(ENOMEM);
    int ret = av_hwframe_transfer_data(cpu, producer, 0);
    if (ret >= 0) {
        for (int plane = 0; plane < 2; plane++) {
            for (int y = 0; y < (plane ? 4 : 8); y++) {
                for (int x = 0; x < 16; x++) {
                    int expected = plane ? (x % 2 ? 170 : 90) : 32 + x * 12;
                    uint8_t *row = cpu->data[plane] + y * cpu->linesize[plane];
                    int actual = cpu->format == AV_PIX_FMT_P010 ? ((uint16_t*)row)[x] >> 8 : row[x];
                    if (actual != expected) ret = AVERROR(EINVAL);
                }
            }
        }
    }
    av_frame_free(&cpu);
    if (ret < 0) fail(s, "direct fixture pixels changed", ret);
    return ret;
}
