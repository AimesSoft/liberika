# Presenter threading checks

From the repository root:

```sh
c++ -std=c++17 -pthread -Wall -Wextra -Werror \
  packages/erika_flutter/linux/tests/presenter_worker_test.cpp \
  -o /tmp/presenter_worker_test
/tmp/presenter_worker_test
python3 packages/erika_flutter/linux/tests/check_worker_copies.py
```

The worker also has a standalone CMake/CTest target in this directory. The
canonical implementation is the portable Windows header, with an optional
monotonic clock. OHPM distributes exact copies because its packages build
independently of Flutter's other platform directories.

`flutter_smoke.dart` is a test-double integration app. Copy it into a generated
Flutter Linux runner that depends on this plugin, and build
`ohos/tests/presenter_stub.cpp` as `liberika_capi.so` with
`-Ipackages/erika_flutter/native/include`. Set `ERIKA_LIBRARY_DIR` to that library's
directory when building Flutter. The double asserts that every presenter call
runs on the creating worker, and sleeps during render to exercise coalescing.
The app displays a real Flutter texture and checks channel results/events,
surface replacement, capture, byte ownership and pending-result disposal.

Run with `ERIKA_FLUTTER_FRAME_TRACE=1`. For a Wayland runner, also set
`ERIKA_TEST_WAYLAND_OVERLAY=1` to check native overlay resize/hide/recreation and
joined disposal. Xvfb or a headless Weston compositor can host these checks.
This app requires the test double: **it does not test real playback or GPU work**.
