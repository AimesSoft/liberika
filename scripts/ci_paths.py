"""Select CI platforms from repo-relative, NUL-delimited Git paths on stdin."""

import argparse
import json
import os
import subprocess
import sys


ALL = frozenset({"macos", "ios", "tvos", "windows", "linux", "android", "ohos", "web"})
APPLE = frozenset({"macos", "ios", "tvos"})
DOC_ROOTS = (
    "docs/", "readme/", "packages/erika_flutter/", "packages/erika_ohos/", "packages/erika_web/",
)
WORKFLOWS = {
    "ci.yml": APPLE | {"windows"},
    "linux.yml": {"linux"},
    "android.yml": {"android"},
    "ohos.yml": {"ohos"},
    "web.yml": {"web"},
    "android-flutter-assets.yml": {"android"},
    "flutter-package.yml": APPLE | {"windows", "android", "ohos"},
    "ohpm-package.yml": {"ohos"},
    "swift-sdk-binary.yml": APPLE,
}
RENDERER_FILES = {
    "d3d11.rs": "windows",
    "d3d11_d3d12_linalg.rs": "windows",
    "d3d11_sr.rs": "windows",
    "d3d11_sr_select.rs": "windows",
    "d3d11_vk_coopmat.rs": "windows",
    "d3d11_vsr.rs": "windows",
    "linux_vulkan.rs": "linux",
    "linux_vulkan.c": "linux",
    "linux_vulkan_test.c": "linux",
    "linux_wsl_d3d12.cpp": "linux",
    "vulkan_dmabuf_export.rs": "linux",
    "vulkan_export.rs": "linux",
    "android_vulkan.rs": "android",
    "android_ahb.frag": "android",
    "android_ahb.vert": "android",
    "ohos_vulkan.rs": "ohos",
    "ohos_native_buffer.frag": "ohos",
    "ohos_native_buffer.frag.spv": "ohos",
    "ohos_native_buffer.vert": "ohos",
    "ohos_native_buffer.vert.spv": "ohos",
}

MACOS = [
    dict(arch="arm64", target="aarch64-apple-darwin", runner="macos-26"),
    dict(arch="x64", target="x86_64-apple-darwin", runner="macos-26"),
]
WINDOWS = [
    dict(arch="x64", host_arch="AMD64", target="x86_64-pc-windows-msvc",
         runner="windows-latest", msys2=True),
    dict(arch="arm64", host_arch="ARM64", target="aarch64-pc-windows-msvc",
         runner="windows-11-arm", msys2=False),
]
LINUX = [
    dict(arch=arch, runner=runner, target=target, dependencies=dependencies)
    for arch, runner, target in (
        ("x64", "ubuntu-24.04", "x86_64-unknown-linux-gnu"),
        ("arm64", "ubuntu-24.04-arm", "aarch64-unknown-linux-gnu"),
    )
    for dependencies in ("bundled", "system")
]
ANDROID = [
    dict(abi=abi, rust_target=rust_target, clang_target=clang_target,
         cxx_runtime_target=cxx_runtime_target)
    for abi, rust_target, clang_target, cxx_runtime_target in (
        ("arm64-v8a", "aarch64-linux-android", "aarch64-linux-android", "aarch64-linux-android"),
        ("armeabi-v7a", "armv7-linux-androideabi", "armv7a-linux-androideabi", "arm-linux-androideabi"),
        ("x86_64", "x86_64-linux-android", "x86_64-linux-android", "x86_64-linux-android"),
        ("x86", "i686-linux-android", "i686-linux-android", "i686-linux-android"),
    )
]


def platforms_for_path(path):
    """Match verified modules and packages; new or shared paths default to all."""
    if path.startswith(".github/workflows/"):
        return WORKFLOWS.get(path.removeprefix(".github/workflows/"), ALL)
    if path.rsplit("/", 1)[-1] in {"Cargo.toml", "Cargo.lock"} or path.endswith(".patch"):
        return ALL
    if path.endswith(".md") and ("/" not in path or path.startswith(DOC_ROOTS)):
        return set()

    flutter = path.removeprefix("packages/erika_flutter/")
    if flutter != path:
        platform, separator, _ = flutter.partition("/")
        if separator and platform in ALL:
            return {platform}
    for prefix, platform in (
        ("packages/erika_ohos/", "ohos"),
        ("packages/erika_web/", "web"),
        ("examples/linux_native_demo/", "linux"),
        ("examples/ohos_arkts/", "ohos"),
        ("examples/web_player/", "web"),
    ):
        if path.startswith(prefix):
            return {platform}
    if path in {"scripts/build_web.py", "scripts/serve_web.py", "scripts/web_test_fixtures.py"}:
        return {"web"}

    source = path.removeprefix("crates/erika/src/")
    if source == path:
        return ALL
    for platform in ("android", "ohos", "web"):
        if source == f"{platform}.rs" or source.startswith(f"{platform}/"):
            return {platform}
    if source in {"linux.rs", "linux_pulse.c"}:
        return {"linux"}
    if source == "windows.rs":
        return {"windows"}
    renderer = source.removeprefix("renderer/")
    if renderer == source:
        return ALL
    if renderer in RENDERER_FILES:
        return {RENDERER_FILES[renderer]}
    if renderer.startswith(("d3d11/", "d3d11_sr/", "d3d12_linalg/", "vk_coopmat/")):
        return {"windows"}
    if renderer.startswith("metal/apple/") or renderer in {
        "metal/apple.rs", "metal/upscaler.rs", "metal/upscaler_fsr.rs",
        "metal/upscaler_matmul.rs", "metal/upscaler_metalfx.rs",
        "metal/upscaler_network.rs", "metal/upscaler_vt.rs",
    }:
        return APPLE
    return ALL


def select(paths, pull_request):
    """Missing or empty diffs are conservative; only PRs narrow the matrices."""
    impacted = ALL
    if pull_request and paths:
        impacted = set()
        for path in paths:
            impacted.update(platforms_for_path(path))
            if impacted == ALL:
                break
    outputs = {platform: platform in impacted for platform in sorted(ALL)}
    outputs["quality"] = bool(impacted)
    for platform, rows in (("macos", MACOS), ("windows", WINDOWS), ("linux", LINUX), ("android", ANDROID)):
        outputs[f"{platform}_matrix"] = {"include": rows[:1] if pull_request else rows}
    return outputs


def decode_paths(raw):
    if raw and not raw.endswith(b"\0"):
        raise ValueError("Expected NUL-terminated paths")
    return [os.fsdecode(path) for path in raw.split(b"\0")[:-1]]


def pull_request_paths(base, head):
    if not base or not head:
        return None
    diff = subprocess.run(
        ["git", "diff", "--no-renames", "--name-only", "-z", f"{base}...{head}", "--"],
        stdout=subprocess.PIPE,
        check=False,
    )
    if diff.returncode:
        return None
    return decode_paths(diff.stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--github", action="store_true", help="use the caller event and write GitHub outputs")
    args = parser.parse_args()
    if not args.github:
        print(json.dumps(select(decode_paths(sys.stdin.buffer.read()), pull_request=True)))
        return

    pull_request = os.environ["EVENT_NAME"] == "pull_request"
    paths = None
    if pull_request:
        paths = pull_request_paths(os.environ.get("BASE_SHA"), os.environ.get("HEAD_SHA"))
        if paths is None:
            print("::warning::PR diff unavailable; selecting every platform")
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        for name, value in select(paths, pull_request).items():
            print(f"{name}={json.dumps(value, separators=(',', ':'))}", file=output)


if __name__ == "__main__":
    main()
