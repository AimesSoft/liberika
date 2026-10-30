/* Native Vulkan consumer: imported image/view can be sampled by the host's
 * Vulkan renderer, including a native bridge behind a different wgpu version.
 * No CPU pixel transfer or extra texture copy is performed. */
#include "vulkan_import.h"
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

void erika_example_destroy_texture(VkDevice device, ErikaImportedTexture *texture) {
    if (texture->view) vkDestroyImageView(device, texture->view, NULL);
    if (texture->image) vkDestroyImage(device, texture->image, NULL);
    if (texture->memory) vkFreeMemory(device, texture->memory, NULL);
    erika_texture_frame_release(texture->frame);
    memset(texture, 0, sizeof(*texture));
}

VkResult erika_example_import_texture(VkInstance instance, VkPhysicalDevice physical,
        VkDevice device, ErikaTextureFrame *frame, ErikaImportedTexture *out) {
    (void)instance;
    if (!out) return VK_ERROR_INITIALIZATION_FAILED;
    memset(out, 0, sizeof(*out));
    ErikaExternalTextureInfo info;
    if (erika_texture_frame_get_info(frame, &info) != ErikaStatus_Ok ||
        info.kind != ErikaExternalTextureKind_VulkanOpaqueFd)
        return VK_ERROR_INVALID_EXTERNAL_HANDLE;
    VkPhysicalDeviceIDProperties ids = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES};
    VkPhysicalDeviceProperties2 properties = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = &ids};
    vkGetPhysicalDeviceProperties2(physical, &properties);
    if (memcmp(ids.deviceUUID, info.device_uuid, VK_UUID_SIZE) ||
        memcmp(ids.driverUUID, info.driver_uuid, VK_UUID_SIZE))
        return VK_ERROR_INVALID_EXTERNAL_HANDLE;
    VkExternalMemoryImageCreateInfo external = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT};
    VkImageCreateInfo image = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .pNext = &external,
        .imageType = VK_IMAGE_TYPE_2D, .format = (VkFormat)info.vulkan_format,
        .extent = {info.width, info.height, 1}, .mipLevels = 1, .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = (VkImageTiling)info.vulkan_tiling,
        .usage = info.vulkan_usage, .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED};
    VkResult result = vkCreateImage(device, &image, NULL, &out->image);
    if (result != VK_SUCCESS) goto fail;
    VkMemoryRequirements required;
    vkGetImageMemoryRequirements(device, out->image, &required);
    if (info.memory_type_index >= 32 ||
        !(required.memoryTypeBits & (1u << info.memory_type_index)) ||
        required.size > info.allocation_size) {
        result = VK_ERROR_INVALID_EXTERNAL_HANDLE; goto fail;
    }
    int fd = fcntl((int)info.native_handle, F_DUPFD_CLOEXEC, 0);
    if (fd < 0) { result = VK_ERROR_TOO_MANY_OBJECTS; goto fail; }
    VkImportMemoryFdInfoKHR import = {
        .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
        .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT, .fd = fd};
    VkMemoryDedicatedAllocateInfo dedicated = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        .pNext = &import, .image = out->image};
    VkMemoryAllocateInfo allocate = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &dedicated,
        .allocationSize = info.allocation_size, .memoryTypeIndex = info.memory_type_index};
    result = vkAllocateMemory(device, &allocate, NULL, &out->memory);
    /* vkAllocateMemory consumes the duplicate only on success. */
    if (result != VK_SUCCESS) { close(fd); goto fail; }
    result = vkBindImageMemory(device, out->image, out->memory, 0);
    if (result != VK_SUCCESS) goto fail;
    VkImageViewCreateInfo view = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = out->image, .viewType = VK_IMAGE_VIEW_TYPE_2D,
        .format = image.format,
        .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    result = vkCreateImageView(device, &view, NULL, &out->view);
    if (result != VK_SUCCESS) goto fail;
    out->frame = frame;
    return VK_SUCCESS;
fail:
    erika_example_destroy_texture(device, out);
    return result;
}

void erika_example_acquire_texture(VkCommandBuffer command, uint32_t queue_family,
        const ErikaImportedTexture *texture) {
    VkImageMemoryBarrier acquire = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
        .newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_EXTERNAL,
        .dstQueueFamilyIndex = queue_family,
        .dstAccessMask = VK_ACCESS_SHADER_READ_BIT, .image = texture->image,
        .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    vkCmdPipelineBarrier(command, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
        VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 0, NULL, 0, NULL, 1, &acquire);
}
