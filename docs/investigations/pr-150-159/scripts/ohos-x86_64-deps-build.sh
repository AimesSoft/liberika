set -euo pipefail
cd /work
sdk=/opt/erika-ohos-sdk-5.1.0/linux/native
export RUSTUP_HOME=/opt/erika-rustup
export CARGO_HOME=/root/.cargo
export PATH="$sdk/build-tools/cmake/bin:/opt/erika-rust-cargo/bin:$PATH"
export OHOS_NDK_HOME="$sdk"
export OHOS_SDK_NATIVE="$sdk"
export OHOS_COMPATIBLE_SDK_VERSION=18
export ERIKA_NATIVE_PROFILE=lgpl
export ERIKA_NATIVE_TARGET=x86_64-unknown-linux-ohos
export CARGO_TARGET_DIR=/target
export LIBCLANG_PATH="$sdk/llvm/lib"
cxx_include="$sdk/llvm/include/libcxx-ohos/include/c++/v1"
if [[ ! -f "$cxx_include/vector" ]]; then cxx_include="$sdk/llvm/include/c++/v1"; fi
export BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_linux_ohos="--target=x86_64-unknown-linux-ohos --sysroot=$sdk/sysroot -isystem $cxx_include"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_OHOS_LINKER="$sdk/llvm/bin/x86_64-unknown-linux-ohos-clang"
export CC_x86_64_unknown_linux_ohos="$sdk/llvm/bin/x86_64-unknown-linux-ohos-clang"
export CXX_x86_64_unknown_linux_ohos="$sdk/llvm/bin/x86_64-unknown-linux-ohos-clang++"
export AR_x86_64_unknown_linux_ohos="$sdk/llvm/bin/llvm-ar"
export CFLAGS_x86_64_unknown_linux_ohos=-fPIC
export CXXFLAGS_x86_64_unknown_linux_ohos=-fPIC
cargo run --locked -p xtask -- deps build --profile lgpl --target x86_64-unknown-linux-ohos --jobs 8
