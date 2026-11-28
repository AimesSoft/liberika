#pragma once
#include <cstdint>
struct OHNativeWindow { uint64_t id; };
inline int32_t OH_NativeWindow_CreateNativeWindowFromSurfaceId(uint64_t id, OHNativeWindow** window) {
  *window = id ? new OHNativeWindow{id} : nullptr; return id ? 0 : -1;
}
inline void OH_NativeWindow_DestroyNativeWindow(OHNativeWindow* window) { delete window; }
