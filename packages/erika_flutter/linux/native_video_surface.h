#pragma once
#include <flutter_linux/flutter_linux.h>
#include "erika.h"
#include <cstdint>
#include <memory>
#include <functional>

// A Wayland subsurface below the Flutter surface. Video never enters Flutter's
// RGBA8 texture compositor. All calls must run on GTK's platform thread.
class NativeVideoSurface {
 public:
  using Operation = std::function<void(ErikaPresenterHandle*)>;
  // post queues native operations; fence waits only for native work, never UI.
  NativeVideoSurface(FlView* view, std::function<void(Operation)> post,
                     std::function<void()> detach_fence);
  ~NativeVideoSurface();
  NativeVideoSurface(const NativeVideoSurface&) = delete;
  NativeVideoSurface& operator=(const NativeVideoSurface&) = delete;
  void update(double x, double y, double width, double height, bool visible);
  bool visible() const;
 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};
