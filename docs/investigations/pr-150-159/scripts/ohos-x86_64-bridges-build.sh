set -euo pipefail
sdk=/opt/erika-ohos-sdk-5.1.0/linux/native
"$sdk/build-tools/cmake/bin/cmake" -S /review-logs/ohos-x86_64-bridge-project -B /target/ohos-x86_64-bridges -DCMAKE_TOOLCHAIN_FILE="$sdk/build/cmake/ohos.toolchain.cmake" -DOHOS_ARCH=x86_64 -DCMAKE_BUILD_TYPE=Release
"$sdk/build-tools/cmake/bin/cmake" --build /target/ohos-x86_64-bridges -j4
"$sdk/llvm/bin/llvm-readelf" -h /target/ohos-x86_64-bridges/liberika_flutter.so
"$sdk/llvm/bin/llvm-readelf" -d /target/ohos-x86_64-bridges/liberika_ohos.so
sha256sum /target/x86_64-unknown-linux-ohos/release/liberika_capi.so /target/ohos-x86_64-bridges/liberika_flutter.so /target/ohos-x86_64-bridges/liberika_ohos.so
mkdir -p /review-logs/ohos-live-app/entry/libs/x86_64
cp /target/ohos-x86_64-bridges/liberika_flutter.so /target/ohos-x86_64-bridges/liberika_ohos.so /target/x86_64-unknown-linux-ohos/release/liberika_capi.so /review-logs/ohos-live-app/entry/libs/x86_64/
cp "$sdk/llvm/lib/x86_64-linux-ohos/libc++_shared.so" /review-logs/ohos-live-app/entry/libs/x86_64/
