set -euo pipefail
sdk=/opt/erika-ohos-sdk-5.1.0/linux/native
export PATH="$sdk/build-tools/cmake/bin:$PATH"
build=/target/ohos-native-review
mkdir -p "$build"
cmake -S /work/packages/erika_flutter/ohos/src/main/cpp -B "$build/flutter" \
  -DCMAKE_TOOLCHAIN_FILE="$sdk/build/cmake/ohos.toolchain.cmake" \
  -DOHOS_ARCH=arm64-v8a -DOHOS_PLATFORM=OHOS -DCMAKE_BUILD_TYPE=Debug
cmake --build "$build/flutter" -j4
runtime=$(find "$build/flutter" -path '*/lib/liberika_capi.so' -print -quit)
test -n "$runtime"
cmake -S /work/packages/erika_ohos/src/main/cpp -B "$build/arkts" \
  -DCMAKE_TOOLCHAIN_FILE="$sdk/build/cmake/ohos.toolchain.cmake" \
  -DOHOS_ARCH=arm64-v8a -DOHOS_PLATFORM=OHOS -DCMAKE_BUILD_TYPE=Debug \
  -DERIKA_CAPI_SO="$runtime"
cmake --build "$build/arkts" -j4
"$sdk/llvm/bin/llvm-readelf" -h "$build/flutter/liberika_flutter.so"
"$sdk/llvm/bin/llvm-readelf" -d "$build/arkts/liberika_ohos.so"
"$sdk/llvm/bin/llvm-nm" -D "$build/flutter/liberika_flutter.so" | grep -E 'napi_register|erika_presenter_create'
