#include "include/erika_ohos.h"
#include "presenter_bridge.h"

NAPI_MODULE(erika_ohos, erika_ohos::Init)

// Legacy symbol retained for binary loading compatibility. Raw presenter
// pointers cannot escape the serialized native owner. Use the ArkTS/N-API API.
extern "C" void* ErikaOhosPlayerFromId(int64_t) { return nullptr; }
