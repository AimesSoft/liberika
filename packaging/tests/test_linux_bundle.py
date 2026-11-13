#!/usr/bin/env python3
"""Portable Linux packaging/ABI contract tests; no native build or downloads."""
import os
from pathlib import Path
import runpy
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

ROOT = Path(__file__).resolve().parents[2]
VERIFY = runpy.run_path(str(ROOT / "packaging/verify_linux.py"))
HEADER = "void *erika_create(void); void erika_destroy(void *handle);"
SYMBOLS = "1: 0 1 FUNC GLOBAL DEFAULT 12 erika_create\n2: 0 1 FUNC GLOBAL DEFAULT 12 erika_destroy"


def dynamic(libraries):
    return "(SONAME) Library soname: [liberika_capi.so]\n" + "\n".join(
        f"(NEEDED) Shared library: [{lib}]" for lib in libraries
    )


class ElfContract(unittest.TestCase):
    def verify(self, mode="bundled", arch="x64", libraries=None, **overrides):
        args = dict(
            header=f"Class: ELF64\nMachine: {VERIFY['MACHINES'][arch]}\n",
            dynamic=dynamic(libraries or ["libc.so.6", "libfontconfig.so.1", "libpulse.so.0"]),
            versions="GLIBC_2.35 GLIBCXX_3.4.30 CXXABI_1.3.13",
            symbols=SYMBOLS, api_header=HEADER, arch=arch, mode=mode,
        )
        args.update(overrides)
        return VERIFY["inspect_elf"](**args)

    def test_both_architectures(self):
        for arch in ("x64", "arm64"):
            self.verify(arch=arch)

    def test_system_ffmpeg_8(self):
        libraries = [f"{name}.so.{major}" for name, major in VERIFY["FFMPEG_ABI"].items()]
        for arch in ("x64", "arm64"):
            self.verify(mode="system", arch=arch, libraries=libraries)

    def test_wrong_architecture(self):
        with self.assertRaisesRegex(ValueError, "architecture"):
            self.verify(arch="arm64", header="Class: ELF64\nMachine: Advanced Micro Devices X86-64")

    def test_wrong_class(self):
        with self.assertRaisesRegex(ValueError, "ELF64"):
            self.verify(header="Class: ELF32")

    def test_missing_soname(self):
        with self.assertRaisesRegex(ValueError, "SONAME"):
            self.verify(dynamic="(NEEDED) [libc.so.6]")

    def test_rpath_and_runpath_rejected(self):
        for tag in ("RPATH", "RUNPATH"):
            with self.assertRaisesRegex(ValueError, "search path"):
                self.verify(dynamic=dynamic(["libc.so.6"]) + f"\n({tag}) [/build/sdk/lib]")

    def test_bundled_leaks(self):
        for name in ("libavcodec.so.62", "libavutil.so.60", "libass.so.9", "libfreetype.so.6", "libharfbuzz.so.0", "libfribidi.so.0", "libdav1d.so.7", "libz.so.1"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.verify(libraries=["libc.so.6", name])

    def test_system_old_or_missing_ffmpeg_rejected(self):
        with self.assertRaisesRegex(ValueError, "FFmpeg 8"):
            self.verify(mode="system", libraries=["libavcodec.so.58"])
        libs = [f"{name}.so.{major}" for name, major in VERIFY["FFMPEG_ABI"].items()]
        with self.assertRaisesRegex(ValueError, "Wrong FFmpeg"):
            self.verify(mode="system", libraries=libs + ["libavcodec.so.61"])

    def test_new_glibc_and_cxx_rejected(self):
        for version in ("GLIBC_2.36", "GLIBCXX_3.4.31", "CXXABI_1.3.14", "GLIBC_PRIVATE", "GLIBC_ABI_DT_RELR"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                self.verify(versions=version)

    def test_older_versions_accepted(self):
        self.verify(versions="GLIBC_2.17 GLIBCXX_3.4.21 CXXABI_1.3.9")

    def test_undefined_is_not_an_export(self):
        with self.assertRaisesRegex(ValueError, "erika_destroy"):
            self.verify(symbols=SYMBOLS.replace("12 erika_destroy", "UND erika_destroy"))


    def test_preprocessed_header_removes_inactive_declarations_and_comments(self):
        with tempfile.TemporaryDirectory() as temporary:
            header = Path(temporary) / "api.h"
            header.write_text(HEADER + """
/* Example: erika_documentation_example(); */
#if defined(_WIN32)
void erika_windows_only(void);
#else
void erika_platform_export(void);
#endif
""")
            with patch.dict(os.environ, CC="cc -U_WIN32"):
                api = VERIFY["preprocess_api_header"](header)
            self.assertNotIn("erika_windows_only", api)
            self.assertNotIn("erika_documentation_example", api)
            with self.assertRaisesRegex(ValueError, "erika_platform_export"):
                self.verify(api_header=api)
            self.verify(api_header=api, symbols=SYMBOLS + "\n3: 0 1 FUNC GLOBAL DEFAULT 12 erika_platform_export")

    def test_public_header_platform_contract(self):
        header = ROOT / "crates/erika_capi/include/erika.h"
        windows_only = {
            "erika_presenter_windows_composition_swapchain_iunknown",
            "erika_presenter_windows_flutter_texture_iunknown",
        }
        with patch.dict(os.environ, CC="cc -U_WIN32"):
            native = VERIFY["preprocess_api_header"](header)
        with patch.dict(os.environ, CC="cc -D_WIN32"):
            windows = VERIFY["preprocess_api_header"](header)
        names = lambda text: set(re.findall(r"\b(erika_\w+)\s*\(", text))
        self.assertEqual(names(windows) - names(native), windows_only)
        self.assertFalse(names(native) - names(windows))
        symbols = "\n".join(f"{i}: 0 1 FUNC GLOBAL DEFAULT 12 {name}" for i, name in enumerate(sorted(names(native))))
        self.verify(api_header=native, symbols=symbols)
        with self.assertRaisesRegex(ValueError, "Missing C ABI exports:.*erika_presenter_windows"):
            self.verify(api_header=windows, symbols=symbols)

    def test_preprocessor_errors_are_not_ignored(self):
        with tempfile.TemporaryDirectory() as temporary:
            header = Path(temporary) / "api.h"
            header.write_text("#error broken public header\n" + HEADER)
            with self.assertRaises(subprocess.CalledProcessError):
                VERIFY["preprocess_api_header"](header)


class BundleContract(unittest.TestCase):
    def test_four_archives_and_other_platform_compatibility(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            so, archive = work / "liberika_capi.so", work / "liberika_capi.a"
            so.write_bytes(b"fixture-only-not-an-ELF")
            archive.write_bytes(b"fixture-only-not-an-archive")
            metadata = work / "metadata"
            (metadata / "licenses").mkdir(parents=True)
            (metadata / "DEPENDENCIES.txt").write_text("verified ELF fixture\n")
            (metadata / "README.md").write_text("runtime instructions\n")
            (metadata / "licenses/headers.txt").write_text("header notices\n")
            for arch, mode in [(a, m) for a in ("x64", "arm64") for m in ("bundled", "system")]:
                name = f"erika-capi-linux-{arch}-{mode}"
                output = work / f"{name}.zip"
                env = {**os.environ, "ERIKA_LINUX_ARCH": arch, "ERIKA_LINUX_DEPENDENCIES": mode,
                       "ERIKA_BUNDLE_METADATA_DIR": str(metadata), "ERIKA_NATIVE_PROFILE": "lgpl"}
                # A previous zip must not retain stale SDK files when regenerated.
                with zipfile.ZipFile(output, "w") as old:
                    old.writestr(name + "/lib/libavcodec.so.62", "stale")
                subprocess.run(["bash", str(ROOT / "packaging/bundle.sh"), name, str(output), str(so), str(archive)],
                               env=env, check=True, capture_output=True)
                with zipfile.ZipFile(output) as bundle:
                    names = bundle.namelist()
                    files = {n[len(name) + 1:] for n in names if not n.endswith("/")}
                    self.assertEqual({n for n in files if n.startswith("lib/")}, {"lib/liberika_capi.so", "lib/liberika_capi.a"})
                    for expected in ("include/erika.h", "LICENSE", "README.md", "DEPENDENCIES.txt", "licenses/headers.txt", "licenses/LICENSE.libass"):
                        self.assertIn(expected, files)
                    manifest = bundle.read(name + "/MANIFEST.txt").decode()
                    self.assertIn(f"arch: {arch}", manifest)
                    self.assertIn(f"dependencies: {mode}", manifest)
                    self.assertIn("glibc 2.35", manifest)
                    self.assertIn("profile: system" if mode == "system" else "profile: lgpl", manifest)
            env = {k: v for k, v in os.environ.items() if not k.startswith("ERIKA_")}
            subprocess.run(["bash", str(ROOT / "packaging/bundle.sh"), "other-platform", str(work / "other.zip"), str(archive)],
                           env=env, check=True, capture_output=True)
            with zipfile.ZipFile(work / "other.zip") as bundle:
                self.assertNotIn(b"dependencies:", bundle.read("other-platform/MANIFEST.txt"))

    def test_environment_modes_and_native_host_guard(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            rustc = work / "rustc"
            rustc.write_text("#!/bin/sh\nprintf 'host: %s\\n' \"$FAKE_RUST_HOST\"\n")
            rustc.chmod(0o755)
            base = {k: v for k, v in os.environ.items() if not k.startswith(("ERIKA_", "CARGO_", "LD_LIBRARY_PATH", "PKG_CONFIG_"))}
            base.update(PATH=str(work) + os.pathsep + base["PATH"], CARGO_TARGET_DIR=str(work / "target"))
            script = 'source scripts/linux_build_env.sh; printf "%s\\n" "$ERIKA_LINUX_ARCH" "$ERIKA_USE_SYSTEM_LIBS" "$CARGO_BUILD_TARGET" "$PKG_CONFIG_PATH" "${LD_LIBRARY_PATH:-}"'
            for host, arch in (("x86_64-unknown-linux-gnu", "x64"), ("aarch64-unknown-linux-gnu", "arm64")):
                for mode in ("bundled", "system"):
                    env = {**base, "FAKE_RUST_HOST": host, "ERIKA_LINUX_DEPENDENCIES": mode}
                    result = subprocess.run(["bash", "-c", script], cwd=ROOT, env=env, check=True, capture_output=True, text=True)
                    lines = result.stdout.splitlines()
                    self.assertEqual(lines[:3], [arch, "0" if mode == "bundled" else "1", host])
                    self.assertEqual("lgpl/ffmpeg/lib" in lines[4], mode == "system")
            for extra in ({"FAKE_RUST_HOST": "aarch64-apple-darwin"}, {"ERIKA_LINUX_DEPENDENCIES": "invalid"},
                          {"CARGO_BUILD_TARGET": "aarch64-unknown-linux-gnu"}, {"ERIKA_FFMPEG_DIR": "/unexpected"}):
                env = {**base, "FAKE_RUST_HOST": "x86_64-unknown-linux-gnu", **extra}
                result = subprocess.run(["bash", "-c", script], cwd=ROOT, env=env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)

    def test_ci_runner_keeps_arguments_and_failure_status(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            gdb = work / "gdb"
            gdb.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\nexit "$TEST_EXIT_CODE"\n')
            gdb.chmod(0o755)
            arguments = ["/test binary", "--test-threads=2", "filter with spaces"]
            for code in (0, 101, 255):
                env = {**os.environ, "PATH": str(work) + os.pathsep + os.environ["PATH"],
                       "TEST_EXIT_CODE": str(code)}
                result = subprocess.run(["bash", str(ROOT / "scripts/linux_test_runner.sh"), *arguments],
                                        env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, code)
                passed = result.stdout.splitlines()
                self.assertIn("--return-child-result", passed)
                self.assertIn("thread apply all bt 32", passed)
                self.assertEqual(passed[passed.index("--args") + 1:], arguments)
        workflow = (ROOT / ".github/workflows/linux.yml").read_text()
        self.assertIn('ERIKA_LINUX_TEST_BACKTRACE: "1"', workflow)
        script = (ROOT / "scripts/check_linux.sh").read_text()
        for arch in ("X86_64", "AARCH64"):
            self.assertIn(f'CARGO_TARGET_{arch}_UNKNOWN_LINUX_GNU_RUNNER="bash scripts/linux_test_runner.sh"', script)
        self.assertIn("--lib -- --test-threads=2", script)

    def test_release_gates_on_four_combinations_and_clean_runtime(self):
        workflow = (ROOT / ".github/workflows/linux.yml").read_text()
        self.assertIn("workflow_call:", workflow)
        self.assertEqual(workflow.count("arch: [x64, arm64]"), 2)
        self.assertEqual(workflow.count("dependencies: [bundled, system]"), 2)
        self.assertEqual(workflow.count("container: ubuntu:22.04"), 2)
        self.assertNotIn("ubuntu:26.04", workflow)
        self.assertIn("runner: ubuntu-24.04-arm", workflow)
        self.assertIn("needs: native", workflow)
        self.assertIn("bash scripts/check_linux_bundle.sh", workflow)
        release = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("uses: ./.github/workflows/linux.yml", release)
        self.assertRegex(release, r"needs: \[[^\n]*\blinux\]")
        self.assertIn("pattern: erika-capi-*", release)
        self.assertNotIn("pattern: linux-test-sdk", release)


if __name__ == "__main__":
    unittest.main()
