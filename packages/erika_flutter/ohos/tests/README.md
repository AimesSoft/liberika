# Host bridge checks

On macOS with Node headers installed (adjust the include path if needed):

```sh
c++ -std=c++17 -pthread -fno-exceptions -Wall -Wextra -Werror \
  -shared -undefined dynamic_lookup -I/opt/homebrew/include/node \
  -Ipackages/erika_flutter/ohos/tests/host_include \
  -Ipackages/erika_flutter/native/include \
  packages/erika_flutter/ohos/src/main/cpp/erika_flutter_plugin.cpp \
  packages/erika_flutter/ohos/tests/presenter_stub.cpp -o /tmp/erika_flutter.node
node packages/erika_flutter/ohos/tests/bridge_test.cjs /tmp/erika_flutter.node
```

To check the standalone OHPM bridge, substitute its `erika_ohos.cpp` and
`src/main/cpp/include` directory. The test uses real host N-API/TSFN execution
and fake NativeWindow/hilog/Erika implementations. It verifies ownership and
dispatch, **not** NDK compatibility, ArkTS compilation, or device playback.

The portable clock/coalescing test is in `linux/tests/presenter_worker_test.cpp`.
Compile with `-DERIKA_WORKER_HEADER='"/absolute/path/to/presenter_worker.h"'`
to exercise either OHOS package's independently packaged copy.
