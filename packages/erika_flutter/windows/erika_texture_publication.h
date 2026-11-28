// Copyright (c) Aimes Soft and contributors.
// SPDX-License-Identifier: MIT
#pragma once

#include <d3d11.h>
#include <dxgi.h>
#include <flutter_texture_registrar.h>
#include <wrl/client.h>

#include <memory>
#include <mutex>
#include <utility>

namespace erika_flutter {

// Publish only completed, immutable GPU textures. The worker transfers an
// AddRef'd texture in a platform completion, guarded by its surface generation.
// The platform thread publishes latest_ under mutex_,
// while Flutter's raster callback pins sampled_ until its next invocation.
// The descriptor therefore also survives the engine's reads AFTER opening the
// handle. No frame-count timeout or release_callback is a lifetime guarantee.
// Destroy this object only after UnregisterTexture's completion callback.
class ErikaTexturePublication {
 public:
  HRESULT Update(void* raw_texture, bool& changed) {
    changed = false;
    Microsoft::WRL::ComPtr<IUnknown> unknown;
    unknown.Attach(static_cast<IUnknown*>(raw_texture));
    if (!unknown) {
      return E_POINTER;
    }
    Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
    HRESULT status = unknown.As(&texture);
    if (FAILED(status)) {
      return status;
    }
    std::lock_guard<std::mutex> lock(mutex_);
    if (latest_ && latest_->texture.Get() == texture.Get()) {
      return S_OK;
    }
    Microsoft::WRL::ComPtr<IDXGIResource> resource;
    status = texture.As(&resource);
    if (FAILED(status)) {
      return status;
    }
    HANDLE handle = nullptr;
    status = resource->GetSharedHandle(&handle);
    if (FAILED(status) || handle == nullptr) {
      return FAILED(status) ? status : E_HANDLE;
    }
    auto next = std::make_shared<Snapshot>();
    D3D11_TEXTURE2D_DESC desc{};
    texture->GetDesc(&desc);
    next->texture = std::move(texture);
    next->descriptor.struct_size = sizeof(FlutterDesktopGpuSurfaceDescriptor);
    next->descriptor.handle = handle;
    next->descriptor.width = desc.Width;
    next->descriptor.height = desc.Height;
    next->descriptor.visible_width = desc.Width;
    next->descriptor.visible_height = desc.Height;
    next->descriptor.format = kFlutterDesktopPixelFormatBGRA8888;
    latest_ = std::move(next);
    changed = true;
    return S_OK;
  }

  // Called serially on Flutter's raster thread. UI publication never mutates
  // sampled_ or its descriptor, including while Flutter is opening a handle.
  const FlutterDesktopGpuSurfaceDescriptor* AcquireDescriptor() {
    std::lock_guard<std::mutex> lock(mutex_);
    sampled_ = latest_;
    return sampled_ ? &sampled_->descriptor : nullptr;
  }

  void Clear() {
    std::lock_guard<std::mutex> lock(mutex_);
    latest_.reset();
  }

 private:
  struct Snapshot {
    Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
    FlutterDesktopGpuSurfaceDescriptor descriptor{};
  };
  std::mutex mutex_;
  std::shared_ptr<Snapshot> latest_;
  std::shared_ptr<Snapshot> sampled_;
};

}  // namespace erika_flutter
