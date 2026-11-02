#ifndef ERIKA_OHOS_H_
#define ERIKA_OHOS_H_

#include <stdint.h>

#if defined(__cplusplus)
extern "C" {
#endif

// Deprecated compatibility symbol; returns nullptr. Presenters cannot leave
// their native owner thread. Use the serialized ArkTS/N-API entry points.
__attribute__((visibility("default"))) void *ErikaOhosPlayerFromId(int64_t player_id);

#if defined(__cplusplus)
}
#endif

#endif
