#!/bin/bash
# Build and run the real macOS plugin in an unhosted XCTest bundle. No NSApp,
# Flutter engine, window, or real media/GPU renderer is launched.
set -euo pipefail
test_dir="$(cd "$(dirname "$0")" && pwd)"
package_dir="$(cd "$test_dir/../.." && pwd)"
flutter_dir="${FLUTTER_ROOT:-$(cd "$(dirname "$(command -v flutter)")/.." && pwd)}"
engine_dir="$flutter_dir/bin/cache/artifacts/engine/darwin-x64/FlutterMacOS.xcframework/macos-arm64_x86_64"
developer_dir="$(xcode-select -p)"
xctest_dir="$developer_dir/Platforms/MacOSX.platform/Developer"
output_dir="${1:-$(mktemp -d /tmp/erika-apple-tests.XXXXXX)}"
mkdir -p "$output_dir/AppleOwnerTests.xctest/Contents/MacOS"
python3 "$test_dir/check_owner_copies.py"
clang -dynamiclib -I "$package_dir/native/include" "$test_dir/NativeStub.c" \
  -o "$output_dir/liberika_apple_stub.dylib"
xcrun swiftc -emit-library -emit-module -enable-testing -D DEBUG \
  -module-name erika_flutter -target arm64-apple-macos12.0 \
  -F "$engine_dir" -framework FlutterMacOS \
  -Xlinker -rpath -Xlinker "$engine_dir" \
  "$package_dir"/macos/Classes/*.swift -o "$output_dir/liberika_flutter.dylib"
xcrun swiftc -emit-library -D DEBUG -module-name AppleOwnerTests \
  -I "$output_dir" -L "$output_dir" -lerika_flutter \
  -I "$xctest_dir/usr/lib" -L "$xctest_dir/usr/lib" \
  -F "$xctest_dir/Library/Frameworks" -framework XCTest \
  -F "$engine_dir" -framework FlutterMacOS \
  -Xlinker -rpath -Xlinker "$output_dir" \
  -Xlinker -rpath -Xlinker "$engine_dir" \
  "$package_dir/example/macos/RunnerTests/RunnerTests.swift" \
  -o "$output_dir/AppleOwnerTests.xctest/Contents/MacOS/AppleOwnerTests"
ERIKA_CAPI_DYLIB="$output_dir/liberika_apple_stub.dylib" \
  ERIKA_FLUTTER_FRAME_TRACE="${ERIKA_FLUTTER_FRAME_TRACE:-1}" \
  xcrun xctest "$output_dir/AppleOwnerTests.xctest"
