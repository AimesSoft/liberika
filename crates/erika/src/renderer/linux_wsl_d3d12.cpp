// Optional WSL Mesa D3D12 -> Dozen bridge. This is a GPU plane copy, not direct
// zero-copy. Requires the matching Vulkan/DXCore adapter and a shareable VA
// pool.
// winadapter defines IUnknown for the Linux DirectX headers. Keep it first.
// clang-format off
#include <wsl/winadapter.h>
#include <d3d12.h>
#include <dxcore.h>
// clang-format on
#include <vulkan/vulkan.h>
extern "C" {
#include <libavutil/frame.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <va/va_drmcommon.h>
}
#include <chrono>
#include <cstdio>
#include <cstring>
#include <dlfcn.h>
#include <memory>
#include <stdexcept>
#include <thread>
#include <unistd.h>
#include <vector>

template <class T> struct Com {
  T *p = nullptr;
  ~Com() {
    if (p)
      p->Release();
  }
  T *operator->() const { return p; }
  void **out() { return reinterpret_cast<void **>(&p); }
  Com() = default;
  Com(const Com &) = delete;
  Com &operator=(const Com &) = delete;
};
static void hr(HRESULT value, const char *stage) {
  if (FAILED(value)) {
    char text[160];
    snprintf(text, sizeof(text), "%s: HRESULT 0x%08x", stage, (unsigned)value);
    throw std::runtime_error(text);
  }
}
static void vr(VkResult value, const char *stage) {
  if (value != VK_SUCCESS) {
    char text[160];
    snprintf(text, sizeof(text), "%s: VkResult %d", stage, value);
    throw std::runtime_error(text);
  }
}
struct WslFrame;
struct WslState {
  VkDevice vk;
  VkQueue queue;
  uint32_t family;
  VkPhysicalDeviceMemoryProperties memory;
  VkCommandPool pool = VK_NULL_HANDLE;
  VkCommandBuffer command = VK_NULL_HANDLE;
  VkFence fence = VK_NULL_HANDLE;
  Com<ID3D12Device> device;
  Com<ID3D12CommandQueue> copy_queue;
  Com<ID3D12CommandAllocator> allocator;
  Com<ID3D12GraphicsCommandList> list;
  Com<ID3D12Fence> copy_fence;
  uint64_t serial = 0;
  bool poisoned = false;
  std::vector<WslFrame *> idle_frames;
  ~WslState();
};
struct WslFrame {
  WslState *state;
  VkImage images[2] = {};
  VkDeviceMemory memory[2] = {};
  Com<ID3D12Resource> planes[2];
  int width = 0, height = 0;
  bool p010 = false;
  explicit WslFrame(WslState *s) : state(s) {}
  ~WslFrame() {
    for (int i = 0; i < 2; i++) {
      if (images[i])
        vkDestroyImage(state->vk, images[i], nullptr);
      if (memory[i])
        vkFreeMemory(state->vk, memory[i], nullptr);
    }
  }
};
WslState::~WslState() {
  for (auto frame : idle_frames)
    delete frame;
  if (fence)
    vkDestroyFence(vk, fence, nullptr);
  if (pool)
    vkDestroyCommandPool(vk, pool, nullptr);
}
struct Fd {
  int value = -1;
  ~Fd() {
    if (value >= 0)
      close(value);
  }
};
struct FrameRef {
  AVFrame *value = nullptr;
  ~FrameRef() { av_frame_free(&value); }
};

static void transition(WslFrame *f, VkImageLayout before, VkImageLayout after,
                       uint32_t from, uint32_t to) {
  WslState *s = f->state;
  vr(vkResetCommandBuffer(s->command, 0), "reset Vulkan command");
  VkCommandBufferBeginInfo begin = {};
  begin.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
  begin.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
  vr(vkBeginCommandBuffer(s->command, &begin), "begin Vulkan command");
  VkImageMemoryBarrier barriers[2] = {};
  for (int i = 0; i < 2; i++) {
    auto &b = barriers[i];
    b.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
    b.oldLayout = before;
    b.newLayout = after;
    b.srcQueueFamilyIndex = from;
    b.dstQueueFamilyIndex = to;
    b.image = f->images[i];
    b.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    b.srcAccessMask = before == VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL
                          ? VK_ACCESS_SHADER_READ_BIT
                          : 0;
    b.dstAccessMask = after == VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL
                          ? VK_ACCESS_SHADER_READ_BIT
                          : 0;
  }
  vkCmdPipelineBarrier(s->command, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                       VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 0, nullptr, 0,
                       nullptr, 2, barriers);
  vr(vkEndCommandBuffer(s->command), "end Vulkan command");
  vr(vkResetFences(s->vk, 1, &s->fence), "reset Vulkan fence");
  VkSubmitInfo submit = {};
  submit.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
  submit.commandBufferCount = 1;
  submit.pCommandBuffers = &s->command;
  vr(vkQueueSubmit(s->queue, 1, &submit, s->fence), "submit Vulkan transition");
  vr(vkWaitForFences(s->vk, 1, &s->fence, VK_TRUE, UINT64_MAX),
     "wait Vulkan transition");
}

extern "C" void *erika_wsl_create(uint64_t physical, uint64_t device,
                                  uint64_t queue, uint32_t family, char *error,
                                  size_t error_size) {
  try {
    auto s = std::make_unique<WslState>();
    s->vk = (VkDevice)(uintptr_t)device;
    s->queue = (VkQueue)(uintptr_t)queue;
    s->family = family;
    VkPhysicalDevice gpu = (VkPhysicalDevice)(uintptr_t)physical;
    VkPhysicalDeviceIDProperties id = {};
    id.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES;
    VkPhysicalDeviceProperties2 props = {};
    props.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
    props.pNext = &id;
    vkGetPhysicalDeviceProperties2(gpu, &props);
    if (!id.deviceLUIDValid)
      throw std::runtime_error("WSL bridge requires a Vulkan adapter LUID");
    // Keep runtime TLS callbacks loaded until process exit.
    void *runtime = dlopen("libd3d12.so", RTLD_NOW | RTLD_NODELETE);
    void *dxcore = dlopen("libdxcore.so", RTLD_NOW | RTLD_NODELETE);
    if (!runtime || !dxcore)
      throw std::runtime_error("WSL D3D12/DXCore runtime unavailable");
    auto create = (HRESULT (*)(IUnknown *, D3D_FEATURE_LEVEL, REFIID,
                               void **))dlsym(runtime, "D3D12CreateDevice");
    auto factory_fn = (HRESULT (*)(REFIID, void **))dlsym(
        dxcore, "DXCoreCreateAdapterFactory");
    if (!create || !factory_fn)
      throw std::runtime_error("WSL runtime entry points unavailable");
    Com<IDXCoreAdapterFactory> factory;
    hr(factory_fn(IID_IDXCoreAdapterFactory, factory.out()), "DXCore factory");
    Com<IDXCoreAdapterList> adapters;
    hr(factory->CreateAdapterList(1, &DXCORE_ADAPTER_ATTRIBUTE_D3D12_GRAPHICS,
                                  IID_IDXCoreAdapterList, adapters.out()),
       "DXCore adapters");
    for (uint32_t i = 0; i < adapters->GetAdapterCount(); i++) {
      Com<IDXCoreAdapter> adapter;
      hr(adapters->GetAdapter(i, IID_IDXCoreAdapter, adapter.out()),
         "DXCore adapter");
      LUID luid = {};
      hr(adapter->GetProperty(DXCoreAdapterProperty::InstanceLuid, sizeof(luid),
                              &luid),
         "DXCore LUID");
      if (memcmp(&luid, id.deviceLUID, VK_LUID_SIZE))
        continue;
      hr(create(adapter.p, D3D_FEATURE_LEVEL_11_0, IID_ID3D12Device,
                s->device.out()),
         "D3D12 device");
      break;
    }
    if (!s->device.p)
      throw std::runtime_error("No DXCore adapter matches the Vulkan device");
    D3D12_COMMAND_QUEUE_DESC q = {};
    q.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    hr(s->device->CreateCommandQueue(&q, IID_ID3D12CommandQueue,
                                     s->copy_queue.out()),
       "D3D12 copy queue");
    hr(s->device->CreateCommandAllocator(q.Type, IID_ID3D12CommandAllocator,
                                         s->allocator.out()),
       "D3D12 allocator");
    hr(s->device->CreateCommandList(0, q.Type, s->allocator.p, nullptr,
                                    IID_ID3D12GraphicsCommandList,
                                    s->list.out()),
       "D3D12 command list");
    hr(s->list->Close(), "close initial D3D12 list");
    hr(s->device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_ID3D12Fence,
                              s->copy_fence.out()),
       "D3D12 fence");
    vkGetPhysicalDeviceMemoryProperties(gpu, &s->memory);
    VkCommandPoolCreateInfo pool = {};
    pool.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
    pool.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    pool.queueFamilyIndex = family;
    vr(vkCreateCommandPool(s->vk, &pool, nullptr, &s->pool),
       "Vulkan command pool");
    VkCommandBufferAllocateInfo cmd = {};
    cmd.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
    cmd.commandPool = s->pool;
    cmd.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    cmd.commandBufferCount = 1;
    vr(vkAllocateCommandBuffers(s->vk, &cmd, &s->command),
       "Vulkan command buffer");
    VkFenceCreateInfo fence = {};
    fence.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO;
    vr(vkCreateFence(s->vk, &fence, nullptr, &s->fence), "Vulkan fence");
    return s.release();
  } catch (const std::exception &e) {
    snprintf(error, error_size, "%s", e.what());
    return nullptr;
  }
}

extern "C" void erika_wsl_destroy(void *state) {
  auto s = static_cast<WslState *>(state);
  // A live device with an unsignalled submission cannot safely release the
  // allocator, queue or referenced resources. Quarantine until process exit.
  if (s && !s->poisoned)
    delete s;
}

extern "C" void *erika_wsl_copy(void *state, const AVFrame *frame,
                                uint64_t images[2], char *error,
                                size_t error_size) {
  auto s = static_cast<WslState *>(state);
  Com<ID3D12Resource> source;
  FrameRef retained;
  std::unique_ptr<WslFrame> output;
  bool submitted = false, completed = false;
  try {
    if (s->poisoned)
      throw std::runtime_error("WSL copy queue failed; recreate the renderer");
    if (!frame || frame->format != AV_PIX_FMT_VAAPI || !frame->hw_frames_ctx)
      throw std::runtime_error("WSL copy requires VA-API");
    retained.value = av_frame_clone(frame);
    if (!retained.value)
      throw std::runtime_error("Could not retain the VA-API decoder surface");
    auto fc = (AVHWFramesContext *)frame->hw_frames_ctx->data;
    bool p010 = fc->sw_format == AV_PIX_FMT_P010;
    if (!p010 && fc->sw_format != AV_PIX_FMT_NV12)
      throw std::runtime_error("WSL copy requires NV12/P010");
    auto va = (AVVAAPIDeviceContext *)fc->device_ctx->hwctx;
    const char *vendor = vaQueryVendorString(va->display);
    if (!vendor || !strstr(vendor, "D3D12"))
      throw std::runtime_error("WSL copy only accepts Mesa D3D12 VA surfaces");
    VASurfaceID surface = (VASurfaceID)(uintptr_t)frame->data[3];
    VAStatus status = vaSyncSurface(va->display, surface);
    if (status)
      throw std::runtime_error(vaErrorStr(status));
    VADRMPRIMESurfaceDescriptor desc = {};
    status = vaExportSurfaceHandle(
        va->display, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
        VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS, &desc);
    if (status)
      throw std::runtime_error(vaErrorStr(status));
    Fd input_fds[4];
    for (unsigned i = 0; i < desc.num_objects && i < 4; i++)
      input_fds[i].value = desc.objects[i].fd;
    // Mesa D3D12 exports opaque NT-resource FDs, not DMA-BUF memory.
    if (desc.num_objects != 1 ||
        desc.objects[0].drm_format_modifier != UINT64_MAX)
      throw std::runtime_error("Expected one opaque D3D12 resource");
    hr(s->device->OpenSharedHandle((HANDLE)(intptr_t)input_fds[0].value,
                                   IID_ID3D12Resource, source.out()),
       "open decoder resource");
    auto source_desc = source->GetDesc();
    if (source_desc.Format != (p010 ? DXGI_FORMAT_P010 : DXGI_FORMAT_NV12) ||
        source_desc.Width < (unsigned)frame->width ||
        source_desc.Height < (unsigned)frame->height ||
        source_desc.DepthOrArraySize != 1 || source_desc.MipLevels != 1)
      throw std::runtime_error("Unsupported decoder texture shape");
    while (!s->idle_frames.empty()) {
      output.reset(s->idle_frames.back());
      s->idle_frames.pop_back();
      if (output->width == frame->width && output->height == frame->height &&
          output->p010 == p010)
        break;
      output.reset();
    }
    bool fresh = !output;
    if (fresh) {
      output = std::make_unique<WslFrame>(s);
      output->width = frame->width;
      output->height = frame->height;
      output->p010 = p010;
    }
    for (int i = 0; fresh && i < 2; i++) {
      D3D12_HEAP_PROPERTIES heap = {};
      heap.Type = D3D12_HEAP_TYPE_DEFAULT;
      D3D12_RESOURCE_DESC rd = {};
      rd.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
      rd.Width = i ? (frame->width + 1) / 2 : frame->width;
      rd.Height = i ? (frame->height + 1) / 2 : frame->height;
      rd.DepthOrArraySize = 1;
      rd.MipLevels = 1;
      rd.SampleDesc.Count = 1;
      rd.Format = p010 ? (i ? DXGI_FORMAT_R16G16_UNORM : DXGI_FORMAT_R16_UNORM)
                       : (i ? DXGI_FORMAT_R8G8_UNORM : DXGI_FORMAT_R8_UNORM);
      rd.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
      hr(s->device->CreateCommittedResource(
             &heap, D3D12_HEAP_FLAG_SHARED, &rd, D3D12_RESOURCE_STATE_COMMON,
             nullptr, IID_ID3D12Resource, output->planes[i].out()),
         "allocate shared plane");
      HANDLE handle = nullptr;
      hr(s->device->CreateSharedHandle(output->planes[i].p, nullptr,
                                       GENERIC_ALL, nullptr, &handle),
         "export shared plane");
      Fd fd;
      fd.value = (int)(intptr_t)handle;
      VkExternalMemoryImageCreateInfo external = {};
      external.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO;
      external.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
      VkImageCreateInfo image = {};
      image.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
      image.pNext = &external;
      image.imageType = VK_IMAGE_TYPE_2D;
      image.format = p010 ? (i ? VK_FORMAT_R16G16_UNORM : VK_FORMAT_R16_UNORM)
                          : (i ? VK_FORMAT_R8G8_UNORM : VK_FORMAT_R8_UNORM);
      image.extent = {(uint32_t)rd.Width, rd.Height, 1};
      image.mipLevels = 1;
      image.arrayLayers = 1;
      image.samples = VK_SAMPLE_COUNT_1_BIT;
      image.tiling = VK_IMAGE_TILING_OPTIMAL;
      image.usage = VK_IMAGE_USAGE_SAMPLED_BIT |
                    VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                    VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
      vr(vkCreateImage(s->vk, &image, nullptr, &output->images[i]),
         "create Vulkan shared plane");
      VkMemoryRequirements req;
      vkGetImageMemoryRequirements(s->vk, output->images[i], &req);
      uint32_t type = 0;
      while (type < s->memory.memoryTypeCount &&
             (!(req.memoryTypeBits & (1u << type)) ||
              !(s->memory.memoryTypes[type].propertyFlags &
                VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)))
        type++;
      if (type == s->memory.memoryTypeCount)
        throw std::runtime_error("No device-local plane memory");
      VkMemoryDedicatedAllocateInfo dedicated = {};
      dedicated.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO;
      dedicated.image = output->images[i];
      VkImportMemoryFdInfoKHR import = {};
      import.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR;
      import.pNext = &dedicated;
      import.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
      import.fd = fd.value;
      VkMemoryAllocateInfo allocation = {};
      allocation.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
      allocation.pNext = &import;
      allocation.allocationSize = req.size;
      allocation.memoryTypeIndex = type;
      vr(vkAllocateMemory(s->vk, &allocation, nullptr, &output->memory[i]),
         "import D3D12 plane into Vulkan");
      fd.value = -1;
      vr(vkBindImageMemory(s->vk, output->images[i], output->memory[i], 0),
         "bind shared plane");
    }
    if (fresh)
      transition(output.get(), VK_IMAGE_LAYOUT_UNDEFINED,
                 VK_IMAGE_LAYOUT_GENERAL, s->family, VK_QUEUE_FAMILY_EXTERNAL);
    hr(s->allocator->Reset(), "reset D3D12 allocator");
    hr(s->list->Reset(s->allocator.p, nullptr), "reset D3D12 list");
    for (int i = 0; i < 2; i++) {
      D3D12_RESOURCE_BARRIER barriers[2] = {};
      for (int j = 0; j < 2; j++) {
        auto &b = barriers[j];
        b.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
        b.Transition.pResource = j ? output->planes[i].p : source.p;
        b.Transition.Subresource = j ? 0 : i;
        b.Transition.StateBefore = D3D12_RESOURCE_STATE_COMMON;
        b.Transition.StateAfter = j ? D3D12_RESOURCE_STATE_COPY_DEST
                                    : D3D12_RESOURCE_STATE_COPY_SOURCE;
      }
      s->list->ResourceBarrier(2, barriers);
      D3D12_TEXTURE_COPY_LOCATION src = {};
      src.pResource = source.p;
      src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
      src.SubresourceIndex = i;
      D3D12_TEXTURE_COPY_LOCATION dst = {};
      dst.pResource = output->planes[i].p;
      dst.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
      D3D12_BOX box = {0,
                       0,
                       0,
                       (UINT)(i ? (frame->width + 1) / 2 : frame->width),
                       (UINT)(i ? (frame->height + 1) / 2 : frame->height),
                       1};
      s->list->CopyTextureRegion(&dst, 0, 0, 0, &src, &box);
      for (auto &b : barriers) {
        auto before = b.Transition.StateBefore;
        b.Transition.StateBefore = b.Transition.StateAfter;
        b.Transition.StateAfter = before;
      }
      s->list->ResourceBarrier(2, barriers);
    }
    hr(s->list->Close(), "close D3D12 copy");
    ID3D12CommandList *lists[] = {s->list.p};
    s->copy_queue->ExecuteCommandLists(1, lists);
    submitted = true;
    hr(s->copy_queue->Signal(s->copy_fence.p, ++s->serial),
       "signal D3D12 copy");
    while (s->copy_fence->GetCompletedValue() < s->serial) {
      hr(s->device->GetDeviceRemovedReason(), "D3D12 copy device");
      std::this_thread::sleep_for(std::chrono::microseconds(50));
    }
    completed = true;
    hr(s->device->GetDeviceRemovedReason(), "D3D12 copy completion");
    transition(output.get(), VK_IMAGE_LAYOUT_GENERAL,
               VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
               VK_QUEUE_FAMILY_EXTERNAL, s->family);
    for (int i = 0; i < 2; i++)
      images[i] = (uint64_t)output->images[i];
    return output.release();
  } catch (const std::exception &e) {
    // Keep local owners alive through cleanup. If queue signalling failed
    // after ExecuteCommandLists on a still-live device, completion is unknown.
    // Never turn that failure into a GPU use-after-free or allocator reuse.
    if (submitted && !completed &&
        SUCCEEDED(s->device->GetDeviceRemovedReason())) {
      s->poisoned = true;
      source.p = nullptr; // Retain the decoder allocation until process exit.
      retained.value = nullptr; // Prevent the decoder pool from reusing it.
      (void)output.release();
    } else if (output) {
      // Also covers a failed Vulkan transition after a successful D3D12 copy.
      vkDeviceWaitIdle(s->vk);
    }
    snprintf(error, error_size, "%s", e.what());
    return nullptr;
  }
}

extern "C" int erika_wsl_release(void *frame, char *error, size_t error_size) {
  std::unique_ptr<WslFrame> f(static_cast<WslFrame *>(frame));
  try {
    transition(f.get(), VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
               VK_IMAGE_LAYOUT_GENERAL, f->state->family,
               VK_QUEUE_FAMILY_EXTERNAL);
    WslState *state = f->state;
    if (state->idle_frames.size() < 3) {
      // Reserve before relinquishing ownership (allocation can throw).
      state->idle_frames.reserve(3);
      state->idle_frames.push_back(f.release());
    }
    return 0;
  } catch (const std::exception &e) {
    vkDeviceWaitIdle(f->state->vk);
    snprintf(error, error_size, "%s", e.what());
    return -1;
  }
}
