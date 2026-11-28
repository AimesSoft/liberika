set -euo pipefail
sdk=/opt/erika-ohos-sdk-5.1.0/linux/native
"$sdk/build-tools/cmake/bin/cmake" -S /review-logs/ohos-current-bridge-project -B /target/ohos-current-bridges -DCMAKE_TOOLCHAIN_FILE="$sdk/build/cmake/ohos.toolchain.cmake" -DOHOS_ARCH=arm64-v8a -DCMAKE_BUILD_TYPE=Release
"$sdk/build-tools/cmake/bin/cmake" --build /target/ohos-current-bridges -j4
"$sdk/llvm/bin/llvm-readelf" -h /target/ohos-current-bridges/liberika_flutter.so
"$sdk/llvm/bin/llvm-readelf" -d /target/ohos-current-bridges/liberika_ohos.so
sha256sum /target/aarch64-unknown-linux-ohos/release/liberika_capi.so /target/ohos-current-bridges/liberika_flutter.so /target/ohos-current-bridges/liberika_ohos.so
