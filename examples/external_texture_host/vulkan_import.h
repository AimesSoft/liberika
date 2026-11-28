#ifndef ERIKA_EXAMPLE_VULKAN_IMPORT_H
#define ERIKA_EXAMPLE_VULKAN_IMPORT_H
#include <vulkan/vulkan.h>
#include "erika.h"

typedef struct ErikaImportedTexture {
    VkImage image;
    VkDeviceMemory memory;
    VkImageView view;
    ErikaTextureFrame *frame;
} ErikaImportedTexture;

/* Device must enable VK_KHR_external_memory_fd. On success this takes frame;
 * on failure the caller still owns it. Use the exact exporting GPU/driver. */
VkResult erika_example_import_texture(VkInstance instance, VkPhysicalDevice physical,
    VkDevice device, ErikaTextureFrame *frame, ErikaImportedTexture *out);

/* Record once, before first sampling on this queue family. Producer GPU work
 * completed before export returned; no external semaphore wait is needed. */
void erika_example_acquire_texture(VkCommandBuffer command, uint32_t queue_family,
    const ErikaImportedTexture *texture);

/* Call only after ALL consuming GPU submissions have completed. */
void erika_example_destroy_texture(VkDevice device, ErikaImportedTexture *texture);
#endif
