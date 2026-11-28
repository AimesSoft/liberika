# Apple presenter owner tests

The identical owner source is packaged in all three pods. `check_owner_copies.py`
checks this contract and the shared RunnerTests source. Each native test stub
asserts that creation is off main, each live presenter has a distinct physical
thread, and every call/destruction uses its creating thread. It also rejects two
presenters attached to the same Metal layer at once. The stub does not decode
media or perform GPU work.

Run macOS tests without launching a GUI:

```sh
bash packages/erika_flutter/macos/Tests/run_headless_tests.sh /tmp/erika-owner-tests
```

Tests cover FIFO native calls and main-thread results, all presenter channel
commands, 20,000 merged ticks and rendering fairness, owned model bytes, native
capture, incremental configuration patches, events/subscription replacement,
disposal during blocked commands/rendering, surface transfer between busy
owners, and engine retirement with pending replies. Plugin/stub tests skip when
the stub is absent; scheduler/completion tests always run. Debug-only helpers
drive frame requests and create offscreen views; no window is displayed.

For an iOS or tvOS simulator, point the disposable harness at that platform's
Flutter engine (the standard iOS engine cannot be used for tvOS):

```sh
ruby packages/erika_flutter/macos/Tests/create_simulator_project.rb \
  tvos /path/to/tvos_debug_sim_arm64 /tmp/erika-owner-tvos
xcrun simctl boot SIMULATOR_ID  # do not open Simulator.app
xcodebuild test -project /tmp/erika-owner-tvos/AppleOwner.xcodeproj \
  -scheme AppleOwner -configuration Debug -parallel-testing-enabled NO \
  -destination 'platform=tvOS Simulator,id=SIMULATOR_ID'
```

Use `ios` and `platform=iOS Simulator` for iOS. The script requires CocoaPods'
`xcodeproj` Ruby gem; use the same Ruby/GEM_HOME as the installed `pod` command.
Both harnesses compile the actual plugin and link `NativeStub.c` through its
existing dynamic-symbol loader. No production native-library injection code or
public Dart API is added.

The example's iOS RunnerTests can also run against a stub static library built
with the iPhoneSimulator SDK and supplied using `ERIKA_IOS_CAPI_STATICLIB`.
For generic simulator builds include both arm64 and x86_64 archive slices.

Set `ERIKA_FLUTTER_FRAME_TRACE=1` in the application environment for physical
thread identity on create, commands (method name), ticks, polling, and destroy.
All presenter entries must report `main=false` and one stable thread per player.
Hardware playback, decoder joins, GPU presentation, HDR and screenshots of real
media still need device testing with the real native library.
