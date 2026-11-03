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
            with self.subTest(path=bridge), tempfile.TemporaryDirectory() as temp:
                module = Path(temp) / "bridge.node"
                command = [os.environ["CXX"], "-std=c++17", "-pthread", "-fno-exceptions", "-fPIC",
                           "-shared", "-I", str(headers), "-I", str(tests / "host_include"),
                           "-I", str(ROOT / include), str(ROOT / bridge),
                           str(tests / "presenter_stub.cpp"), "-o", str(module)]
                if sys.platform == "darwin":
                    command[1:1] = ["-undefined", "dynamic_lookup"]
                subprocess.run(command, check=True)
                subprocess.run([node, str(tests / "bridge_test.cjs"), str(module)], check=True)

if __name__ == "__main__":
    unittest.main()
