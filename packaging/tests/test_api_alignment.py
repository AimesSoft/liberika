"""Focused API packaging checks. Run with Python; CXX enables native bridge tests."""
import os
from pathlib import Path
import re
import subprocess
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
        self.assertIn("nativeCreate(outputMode, headroom, upscaler, videoAlphaMode)", flutter)
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
        # Execute the actual bridge function with a fake N-API argument source
        # and capture what reaches the presenter. No OHOS SDK/device is needed.
        for path in ["packages/erika_flutter/ohos/src/main/cpp/erika_flutter_plugin.cpp",
                     "packages/erika_ohos/src/main/cpp/erika_ohos.cpp"]:
            with self.subTest(path=path), tempfile.TemporaryDirectory() as temp:
                source = read(path)
                start = source.index("napi_value NativeCreate(")
                end = source.index("\nnapi_value NativeLastError", start)
                harness = r'''
#include <cassert>
#include <cstdint>
#include <map>
#include <atomic>
#include "erika.h"
using napi_env = void*;
using napi_callback_info = void*;
using napi_value = double;
size_t supplied;
void napi_get_cb_info(napi_env, napi_callback_info, size_t* n, napi_value* args, void*, void*) {
  double values[] = {2, 4.0, 3, 1};
  *n = supplied < *n ? supplied : *n;
  for (size_t i = 0; i < *n; ++i) args[i] = values[i];
}
int GetInt32(napi_env, napi_value v) { return static_cast<int>(v); }
double GetDouble(napi_env, napi_value v) { return v; }
napi_value Int64(napi_env, int64_t v) { return static_cast<double>(v); }
struct OhosPlayer { ErikaPresenterHandle* presenter; void* window; };
std::map<int64_t, OhosPlayer> g_players;
std::atomic<int64_t> g_next_player_id{1};
ErikaPresenterConfig captured;
int creates = 0;
extern "C" ErikaPresenterHandle* erika_presenter_create_with_config(ErikaPresenterConfig c) {
  captured = c; ++creates;
  return reinterpret_cast<ErikaPresenterHandle*>(16);
}
'''
                harness += source[start:end]
                harness += r'''
int main() {
  for (size_t count : {size_t(3), size_t(4)}) {
    supplied = count;
    const int64_t previous_id = g_next_player_id.load();
    const int64_t player_id = NativeCreate(nullptr, nullptr);
    // Successful creates hand out the process-wide monotonic id, not the
    // presenter pointer, so a destroyed player's reused address cannot be
    // mistaken for a live id.
    assert(player_id > 0 && player_id == previous_id);
    assert(captured.output_mode == 2 && captured.edr_headroom == 4.0f);
    assert(captured.luma_upscaler == 3);
    assert(captured.video_alpha_mode == (count == 4 ? 1 : 0));
  }
  supplied = 2;
  assert(NativeCreate(nullptr, nullptr) == 0);
  assert(creates == 2);
}
'''
                cpp = Path(temp) / "bridge.cpp"
                cpp.write_text(harness, encoding="utf-8")
                exe = Path(temp) / ("bridge.exe" if os.name == "nt" else "bridge")
                subprocess.run([os.environ["CXX"], "-std=c++17", "-I", str(ROOT / "crates/erika_capi/include"), str(cpp), "-o", str(exe)], check=True)
                subprocess.run([str(exe)], check=True)


if __name__ == "__main__":
    unittest.main()
