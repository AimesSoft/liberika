"""Focused API packaging checks. Run with Python; CXX enables native bridge tests."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def read(path):
    return (ROOT / path).read_text(encoding="utf-8")


class ApiAlignmentTests(unittest.TestCase):
    def test_header_copies(self):
        canonical = read("crates/erika_capi/include/erika.h")
        for path in ["packages/erika_flutter/native/include/erika.h",
                     "packages/erika_ohos/src/main/cpp/include/erika.h"]:
            with self.subTest(path=path):
                self.assertEqual(canonical, read(path))

    def test_open_options_mirrors_match_header(self):
        # Hosts that cannot include erika.h mirror ErikaOpenOptions; a mirror
        # or a write past the header's reserved slots corrupts the caller.
        sizes = {"const ErikaHttpHeader *": 8, "uintptr_t": 8, "uint64_t": 8,
                 "UnsafeRawPointer?": 8, "UInt": 8, "UInt64": 8}
        header = read("crates/erika_capi/include/erika.h")
        body = re.search(r"typedef struct ErikaOpenOptions \{(.*?)\} ErikaOpenOptions;", header, re.S)[1]
        header_bytes = 0
        for kind, name, count in re.findall(r"^\s*(const ErikaHttpHeader \*|\w+)\s*(\w+)(?:\[(\d+)\])?;", body, re.M):
            header_bytes += sizes[kind] * int(count or 1)
        reserved = int(re.search(r"uint64_t reserved\[(\d+)\];", body)[1])
        for platform in ["ios", "macos", "tvos"]:
            with self.subTest(platform=platform):
                swift = read(f"packages/erika_flutter/{platform}/Classes/ErikaFlutterPlugin.swift")
                mirror = re.search(r"private struct ErikaOpenOptions \{(.*?)\n\}", swift, re.S)[1]
                mirror_bytes = 0
                for kind in re.findall(r"var \w+: ([\w?]+|\([^)]*\))", mirror):
                    parts = [t.strip() for t in kind.strip("()").split(",")] if kind.startswith("(") else [kind]
                    mirror_bytes += sum(sizes[t] for t in parts)
                self.assertEqual(header_bytes, mirror_bytes)
        windows = read("packages/erika_flutter/windows/erika_flutter_plugin.cpp")
        for index in re.findall(r"options\.reserved\[(\d+)\]", windows):
            self.assertLess(int(index), reserved)

    def test_declared_and_exported_names(self):
        header = re.sub(r"/\*.*?\*/", "", read("crates/erika_capi/include/erika.h"), flags=re.S)
        declared = set(re.findall(r"\b(erika_\w+)\s*\(", header))
        exported = set()
        for path in (ROOT / "crates/erika_capi/src").glob("*.rs"):
            exported.update(re.findall(r'pub (?:unsafe )?extern "C" fn (erika_\w+)', path.read_text(encoding="utf-8")))
        self.assertEqual(declared, exported)

    def test_ohos_alpha_reaches_native_call(self):
        flutter = read("packages/erika_flutter/ohos/src/main/ets/components/plugin/ErikaFlutterPlugin.ets")
        self.assertIn("this.numberArg(args, 'videoAlphaMode', 0)", flutter)
        self.assertIn("nativeCreateAsync(outputMode, headroom, upscaler, videoAlphaMode)", flutter)
        sdk = read("packages/erika_ohos/src/main/ets/ErikaPlayer.ets")
        self.assertIn("config.videoAlphaMode ?? 0", sdk)
        self.assertIn("nativeCreate(outputMode, edrHeadroom, upscaler, videoAlphaMode)", sdk)
        self.assertIn("videoAlphaMode?: number", read("packages/erika_flutter/ohos/src/main/cpp/types/liberika_flutter/Index.d.ts"))

    def test_c_and_rust_function_signatures(self):
        # These entry points use scalar/named types and pointers, not callbacks.
        # Keep this deliberately strict: an unfamiliar signature must be reviewed.
        scalar = {"i32": "int32_t", "u32": "uint32_t", "i64": "int64_t",
                  "u64": "uint64_t", "u8": "uint8_t", "usize": "uintptr_t",
                  "f32": "float", "f64": "double", "c_char": "char",
                  "std::ffi::c_void": "void", "c_void": "void",
                  "ErikaPresenterHandle": "void"}

        def compact(value):
            # Unsupported-platform stubs use void* for this opaque handle.
            return re.sub(r"\s+", "", value).replace("ErikaPresenterHandle", "void")

        def c_type(value):
            value = value.strip()
            if value.startswith("*mut "):
                return c_type(value[5:]) + "*"
            if value.startswith("*const "):
                return "const" + c_type(value[7:]) + "*"
            return scalar.get(value, value)

        header = re.sub(r"/\*.*?\*/", "", read("crates/erika_capi/include/erika.h"), flags=re.S)
        declarations = {}
        for ret, name, args in re.findall(r"^([\w *]+?)\b(erika_\w+)\s*\((.*?)\);", header, re.M | re.S):
            params = [] if args.strip() == "void" else [compact(re.sub(r"\w+\s*$", "", arg.strip())) for arg in args.split(",")]
            declarations[name] = (compact(ret), params)
        checked = set()
        for path in (ROOT / "crates/erika_capi/src").glob("*.rs"):
            source = path.read_text(encoding="utf-8")
            for name, args, ret in re.findall(r'pub (?:unsafe )?extern "C" fn (erika_\w+)\((.*?)\)\s*(?:->\s*([^{}]+))?\s*\{', source, re.S):
                params = [c_type(arg.split(":", 1)[1]) for arg in args.split(",") if arg.strip()]
                with self.subTest(name=name):
                    self.assertEqual(declarations[name], (c_type(ret or "void"), params))
                checked.add(name)
        self.assertEqual(checked, set(declarations))

    def test_swift_presenter_config_and_subtitle_style_mirrors_match_header(self):
        header = re.sub(r"/\*.*?\*/", "", read("crates/erika_capi/include/erika.h"), flags=re.S)
        struct_pairs = [
            ("ErikaPresenterConfig", "ErikaPresenterConfigC"),
            ("ErikaSubtitleStyle", "ErikaSubtitleStyleC"),
        ]
        for c_name, swift_name in struct_pairs:
            c_body = re.search(rf"typedef struct {c_name} \{{(.*?)\}} {c_name};", header, re.S)[1]
            c_fields = [
                line.strip()
                for line in c_body.splitlines()
                if line.strip() and line.strip().endswith(";")
            ]
            for platform in ["ios", "macos", "tvos"]:
                with self.subTest(struct=c_name, platform=platform):
                    swift = read(f"packages/erika_flutter/{platform}/Classes/ErikaFlutterPlugin.swift")
                    swift_body = re.search(rf"private struct {swift_name} \{{(.*?)\n\}}", swift, re.S)[1]
                    swift_fields = re.findall(r"^\s*var\s+\w+\s*:", swift_body, re.M)
                    self.assertEqual(len(c_fields), len(swift_fields))

    def test_flutter_method_channel_coverage_and_json_bridges(self):
        presenter_json = read("crates/erika_capi/src/presenter_json.rs")
        json_table_block = re.search(
            r"pub\(crate\) static JSON_METHODS: &\[JsonMethodSpec\] = &\[(.*?)\n\];",
            presenter_json,
            re.S,
        )[1]
        json_methods = set(re.findall(r'method\(\s*"([^"]+)"', json_table_block))

        dart_files = sorted((ROOT / "packages/erika_flutter/lib/src").glob("*.dart"))
        dart_source = "\n".join(path.read_text(encoding="utf-8") for path in dart_files)
        dart_channel_methods = set(
            re.findall(
                r"(?:_channel\.invokeMethod(?:<[^>]+>)?|_invoke|_invokeForPlayer|_invokeOptional|_channelFrameExport(?:<[^>]+>)?)\(\s*'([^']+)'",
                dart_source,
            )
        )
        # attachOverlay is invoked through _channel.invokeMethod in erika_player.dart
        self.assertIn("invokeJson", dart_channel_methods)
        self.assertIn("getDiagnosticsSnapshot", dart_channel_methods)
        self.assertIn("getLogs", dart_channel_methods)

        # Every Dart invokeJson('<name>') target must exist in presenter_json.rs.
        dart_json_targets = set(re.findall(r"invokeJson\(\s*'([^']+)'", dart_source))
        self.assertTrue(dart_json_targets)
        self.assertEqual(dart_json_targets - json_methods, set())

        # Every OHPM ArkTS SDK invokeOrThrow('<name>') target must exist in presenter_json.rs.
        ohos_sdk = read("packages/erika_ohos/src/main/ets/ErikaPlayer.ets")
        ohos_sdk_targets = set(re.findall(r"invokeOrThrow\(\s*'([^']+)'", ohos_sdk))
        self.assertTrue(ohos_sdk_targets)
        self.assertEqual(ohos_sdk_targets - json_methods, set())
        self.assertIn("invokeJson(method: string", ohos_sdk)
        for required_ohos_method in [
            "setMuted",
            "volume",
            "tracks",
            "trackSelection",
            "chapters",
            "container",
            "setSubtitleStyle",
            "setSubtitleDelay",
            "subtitleDelay",
            "subtitleText",
            "loadDanmakuFile",
            "addDanmakuTrackJson",
            "setDanmakuConfig",
            "sendDanmaku",
            "exportGif",
            "exportFrame",
            "exportFrames",
            "getDiagnosticsSnapshot",
            "getLogs",
            "listMethods",
        ]:
            with self.subTest(ohos_sdk_method=required_ohos_method):
                self.assertIn(required_ohos_method, ohos_sdk_targets)

        self.assertEqual(
            read("packages/erika_flutter/ohos/src/main/cpp/presenter_bridge.h"),
            read("packages/erika_ohos/src/main/cpp/presenter_bridge.h"),
        )

        web_dts = read("packages/erika_web/index.d.ts")
        for token in [
            "setMuted(muted: boolean)",
            "getVolume():",
            "setSubtitleDelay(seconds: number)",
            "getSubtitleDelay(): number",
            "clearDanmaku(): Promise<void>",
            "blockWords?: string[]",
            "container?: ContainerInfo | null",
        ]:
            with self.subTest(web_dts_token=token):
                self.assertIn(token, web_dts)

        def swift_methods(platform):
            source = read(f"packages/erika_flutter/{platform}/Classes/ErikaFlutterPlugin.swift")
            handled = set()
            for line in re.findall(r"^\s*case\s+([^\n:]+):", source, re.M):
                handled.update(re.findall(r'"([^"]+)"', line))
            return handled

        windows_source = read("packages/erika_flutter/windows/erika_flutter_plugin.cpp")
        windows_methods = set(re.findall(r'method == "([^"]+)"', windows_source))

        android_source = read(
            "packages/erika_flutter/android/src/main/kotlin/dev/aimesoft/erika_flutter/ErikaFlutterPlugin.kt"
        )
        android_when = re.search(
            r"when \(call\.method\) \{(.*?)else -> result\.notImplemented\(\)",
            android_source,
            re.S,
        )[1]
        android_native = re.search(
            r"private val NATIVE_METHODS = setOf\((.*?)\)",
            android_source,
            re.S,
        )[1]
        android_methods = set(re.findall(r'"([^"]+)"', android_when)) | set(
            re.findall(r'"([^"]+)"', android_native)
        )

        linux_source = read("packages/erika_flutter/linux/erika_flutter_plugin.cc")
        linux_explicit = set(re.findall(r'method == "([^"]+)"', linux_source))
        linux_methods = linux_explicit | json_methods

        ohos_flutter_source = read(
            "packages/erika_flutter/ohos/src/main/ets/components/plugin/ErikaFlutterPlugin.ets"
        )
        ohos_explicit = set(re.findall(r"call\.method === '([^']+)'", ohos_flutter_source))
        ohos_native = re.search(
            r"private isNativePlayerMethod\(method: string\): boolean \{(.*?)\}",
            ohos_flutter_source,
            re.S,
        )[1]
        ohos_flutter_methods = ohos_explicit | set(re.findall(r"'([^']+)'", ohos_native))

        platform_handlers = {
            "macos": swift_methods("macos"),
            "ios": swift_methods("ios"),
            "tvos": swift_methods("tvos"),
            "windows": windows_methods,
            "android": android_methods,
            "linux": linux_methods,
            "ohos": ohos_flutter_methods,
        }

        # Every platform must expose the shared JSON pass-through and diagnostics.
        for platform, handled in platform_handlers.items():
            for required in ("invokeJson", "getDiagnosticsSnapshot", "getLogs"):
                with self.subTest(platform=platform, required=required):
                    self.assertIn(required, handled)

        # Platform-specific exemptions (e.g. FFI-based export off macOS,
        # texture-only vs overlay-only embedders, two-step startPosition).
        ffi_export_methods = {
            "exportGif",
            "exportFrame",
            "openFrameExportSession",
            "exportSessionFrame",
            "closeFrameExportSession",
        }
        texture_methods = {"createTexture", "resizeTexture", "releaseTexture"}
        overlay_methods = {"attachOverlay", "detachOverlay", "setOverlayFrame"}
        intentionally_unsupported = {
            "macos": set(),
            "ios": ffi_export_methods | texture_methods | {"openWithStartPosition"},
            "tvos": ffi_export_methods | texture_methods | {"openWithStartPosition"},
            "windows": ffi_export_methods,
            "android": ffi_export_methods | texture_methods | {"openWithStartPosition"},
            "linux": (ffi_export_methods - {"exportFrame", "exportGif"}) | {"openWithStartPosition"},
            "ohos": ffi_export_methods | overlay_methods | {"openWithStartPosition"},
        }

        for platform, handled in platform_handlers.items():
            exempt = intentionally_unsupported[platform]
            with self.subTest(platform=platform):
                missing = dart_channel_methods - handled - exempt
                self.assertEqual(missing, set(), f"{platform} missing Flutter channel handlers: {sorted(missing)}")
                stale_exemptions = exempt & handled
                self.assertEqual(stale_exemptions, set(), f"{platform} has stale exemptions: {sorted(stale_exemptions)}")

    @unittest.skipUnless(os.environ.get("CXX"), "Set CXX to a C++ compiler to execute NativeCreate with a fake N-API host")
    def test_native_create_preserves_config_and_legacy_default(self):
        # Load each package's actual N-API bridge into Node with an owner-checking
        # C API double (packages/erika_flutter/ohos/tests). Among the ownership
        # checks, the harness verifies that the whole config reaches the presenter,
        # that a 3-argument call keeps the opaque alpha default, and that a shorter
        # call creates nothing. No OHOS SDK/device is needed.
        if "CXX" not in os.environ:
            self.skipTest("Set CXX to a C++ compiler to run the OHOS bridges under Node")
        node = shutil.which("node")
        headers = Path(node).resolve().parents[1] / "include" / "node" if node else None
        if not headers or not (headers / "node_api.h").exists():
            self.skipTest("Node.js with its N-API headers is required for the OHOS bridge test")
        tests = ROOT / "packages/erika_flutter/ohos/tests"
        for bridge, include in [
            ("packages/erika_flutter/ohos/src/main/cpp/erika_flutter_plugin.cpp",
             "packages/erika_flutter/native/include"),
            ("packages/erika_ohos/src/main/cpp/erika_ohos.cpp",
             "packages/erika_ohos/src/main/cpp/include"),
        ]:
            for legacy in (False, True):
                with self.subTest(path=bridge, legacy=legacy), tempfile.TemporaryDirectory() as temp:
                    module = Path(temp) / "bridge.node"
                    command = [os.environ["CXX"], "-std=c++17", "-pthread", "-fno-exceptions", "-fPIC",
                               "-shared", "-I", str(headers), "-I", str(tests / "host_include"),
                               "-I", str(ROOT / include), str(ROOT / bridge),
                               str(tests / "presenter_stub.cpp"), "-o", str(module)]
                    if sys.platform == "darwin":
                        command[1:1] = ["-undefined", "dynamic_lookup"]
                    if legacy:
                        command.insert(1, "-DERIKA_TEST_LEGACY_RUNTIME")
                    subprocess.run(command, check=True)
                    subprocess.run([node, str(tests / "bridge_test.cjs"), str(module), "legacy" if legacy else "current"], check=True)

if __name__ == "__main__":
    unittest.main()
