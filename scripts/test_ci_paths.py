import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import ci_paths


SOURCE = "crates/erika/src/"
RENDERER = SOURCE + "renderer/"


class SelectionTest(unittest.TestCase):
    def assert_platforms(self, paths, expected):
        result = ci_paths.select(paths, pull_request=True)
        self.assertEqual({name for name in ci_paths.ALL if result[name]}, set(expected))
        self.assertEqual(result["quality"], bool(expected))

    def test_metal_and_d3d11(self):
        self.assert_platforms([RENDERER + "metal/apple.rs"], {"macos", "ios", "tvos"})
        self.assert_platforms([RENDERER + "d3d11.rs"], {"windows"})

    def test_documentation(self):
        self.assert_platforms(["README.md", "docs/guide.md", "packages/erika_ohos/README.md"], set())

    def test_shared_and_unknown(self):
        for path in (
            SOURCE + "core.rs", RENDERER + "metal/mod.rs", RENDERER + "metal/new.rs",
            "Cargo.lock", "examples/linux_native_demo/Cargo.toml", "xtask/src/main.rs",
            "third_party/patches/android/fix.patch", "docs/investigations/reproducer.sh",
            "scripts/ci_paths.py", "unrecognized.txt",
        ):
            with self.subTest(path=path):
                self.assert_platforms([path], ci_paths.ALL)

    def test_native_modules_and_platform_packages(self):
        for path, platform in (
            (RENDERER + "linux_wsl_d3d12.cpp", "linux"),
            (RENDERER + "vulkan_dmabuf_export.rs", "linux"),
            (RENDERER + "d3d11_sr/tests.rs", "windows"),
            (RENDERER + "d3d12_linalg/head.hlsl", "windows"),
            (RENDERER + "vk_coopmat/head.spv", "windows"),
            (RENDERER + "android_ahb.frag", "android"),
            (SOURCE + "ohos/gles.rs", "ohos"),
            (SOURCE + "web/media.c", "web"),
            ("packages/erika_flutter/ios/Classes/Player.swift", "ios"),
            ("packages/erika_ohos/Index.ets", "ohos"),
            ("packages/erika_web/worker.js", "web"),
        ):
            with self.subTest(path=path):
                self.assert_platforms([path], {platform})

    def test_workflows(self):
        for workflow, expected in (
            ("ci", {"macos", "ios", "tvos", "windows"}),
            ("linux", {"linux"}), ("android", {"android"}), ("ohos", {"ohos"}), ("web", {"web"}),
            ("release", ci_paths.ALL), ("ci-select", ci_paths.ALL), ("unknown", ci_paths.ALL),
        ):
            with self.subTest(workflow=workflow):
                self.assert_platforms([f".github/workflows/{workflow}.yml"], expected)

    def test_mixed_paths_union_and_shared_override(self):
        paths = [SOURCE + "android.rs", RENDERER + "metal/apple.rs", "docs/guide.md"]
        self.assert_platforms(paths, {"android", "macos", "ios", "tvos"})
        self.assert_platforms(paths + ["Cargo.lock"], ci_paths.ALL)

    def test_matrices(self):
        for pull_request in (True, False):
            with self.subTest(pull_request=pull_request):
                result = ci_paths.select(["README.md"], pull_request=pull_request)
                self.assertEqual(
                    [row["arch"] for row in result["macos_matrix"]["include"]],
                    ["arm64"] if pull_request else ["arm64", "x64"],
                )
                self.assertEqual(
                    [row["arch"] for row in result["windows_matrix"]["include"]],
                    ["x64"] if pull_request else ["x64", "arm64"],
                )
                self.assertEqual(
                    [(row["arch"], row["dependencies"]) for row in result["linux_matrix"]["include"]],
                    [("x64", "bundled")] if pull_request else [
                        ("x64", "bundled"), ("x64", "system"), ("arm64", "bundled"), ("arm64", "system"),
                    ],
                )
                self.assertEqual(
                    [row["abi"] for row in result["android_matrix"]["include"]],
                    ["arm64-v8a"] if pull_request else ["arm64-v8a", "armeabi-v7a", "x86_64", "x86"],
                )
                self.assertEqual({name for name in ci_paths.ALL if result[name]}, set())
                self.assertEqual(
                    {name for name in ci_paths.ALL if ci_paths.select(None, pull_request=pull_request)[name]},
                    ci_paths.ALL,
                )

    def test_missing_or_empty_diff(self):
        self.assert_platforms(None, ci_paths.ALL)
        self.assert_platforms([], ci_paths.ALL)
        self.assertIsNone(ci_paths.pull_request_paths(None, "head"))
        with patch("ci_paths.subprocess.run", return_value=subprocess.CompletedProcess([], 128)):
            self.assertIsNone(ci_paths.pull_request_paths("base", "head"))

    def test_nul_delimited_paths(self):
        self.assertEqual(ci_paths.decode_paths(b"docs/with space.md\0src/with\nnewline.rs\0"),
                         ["docs/with space.md", "src/with\nnewline.rs"])
        with self.assertRaises(ValueError):
            ci_paths.decode_paths(b"truncated")

    def test_github_outputs(self):
        for event in ("pull_request", "push", "workflow_dispatch"):
            with self.subTest(event=event), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "output"
                env = {"EVENT_NAME": event, "GITHUB_OUTPUT": str(output)}
                with patch.dict(os.environ, env), patch("sys.argv", ["ci_paths.py", "--github"]), \
                        patch("ci_paths.pull_request_paths", return_value=None) as diff, \
                        patch("sys.stdout", new_callable=io.StringIO) as stdout:
                    ci_paths.main()
                values = dict(line.split("=", 1) for line in output.read_text().splitlines())
                self.assertEqual(values["quality"], "true")
                self.assertTrue(all(values[name] == "true" for name in ci_paths.ALL))
                self.assertEqual(diff.called, event == "pull_request")
                self.assertEqual("::warning::" in stdout.getvalue(), event == "pull_request")

    def test_git_diff_uses_merge_base_and_keeps_both_sides_of_a_rename(self):
        with tempfile.TemporaryDirectory() as directory:
            def git(*args):
                return subprocess.check_output(
                    ["git", "-c", "user.name=CI", "-c", "user.email=ci@example.invalid",
                     "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
                    cwd=directory, text=True,
                ).strip()

            git("init", "-q", "-b", "base")
            old = RENDERER + "metal/apple.rs"
            new = RENDERER + "d3d11.rs"
            (Path(directory) / old).parent.mkdir(parents=True)
            (Path(directory) / old).write_text("renderer\n")
            git("add", ".")
            git("commit", "-qm", "Common ancestor")
            ancestor = git("rev-parse", "HEAD")
            (Path(directory) / "Cargo.lock").write_text("base-only change\n")
            git("add", "Cargo.lock")
            git("commit", "-qm", "Base-only change")
            base = git("rev-parse", "HEAD")
            git("checkout", "-qb", "pr", ancestor)
            git("mv", old, new)
            git("commit", "-qm", "Move renderer")
            head = git("rev-parse", "HEAD")
            run = subprocess.run
            with patch("ci_paths.subprocess.run", side_effect=lambda *args, **kw: run(*args, cwd=directory, **kw)):
                paths = ci_paths.pull_request_paths(base, head)
            self.assertCountEqual(paths, [old, new])
            self.assert_platforms(paths, {"macos", "ios", "tvos", "windows"})

    def test_forced_push_reports_what_the_rewritten_branch_dropped(self):
        with tempfile.TemporaryDirectory() as directory:
            def git(*args):
                return subprocess.check_output(
                    ["git", "-c", "user.name=CI", "-c", "user.email=ci@example.invalid",
                     "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
                    cwd=directory, text=True,
                ).strip()

            git("init", "-q", "-b", "main")
            windows = "crates/erika/src/windows.rs"
            (Path(directory) / windows).parent.mkdir(parents=True)
            (Path(directory) / "README.md").write_text("liberika\n")
            git("add", ".")
            git("commit", "-qm", "Common ancestor")
            ancestor = git("rev-parse", "HEAD")
            (Path(directory) / windows).write_text("windows\n")
            git("add", ".")
            git("commit", "-qm", "Windows change")
            before = git("rev-parse", "HEAD")
            # main is rewritten: the Windows commit is gone, a README edit took its place.
            git("reset", "-q", "--hard", ancestor)
            (Path(directory) / "README.md").write_text("liberika, rewritten\n")
            git("commit", "-qam", "README edit")
            after = git("rev-parse", "HEAD")
            run = subprocess.run
            with patch("ci_paths.subprocess.run", side_effect=lambda *args, **kw: run(*args, cwd=directory, **kw)):
                self.assertEqual(ci_paths.pull_request_paths(before, after), ["README.md"])
                paths = ci_paths.push_paths(before, after)
                unreachable = ci_paths.push_paths("1" * 40, after)
            self.assertCountEqual(paths, ["README.md", windows])
            self.assert_platforms(paths, {"windows"})
            self.assertIsNone(unreachable)

    def test_main_push_compares_trees_and_falls_back_to_every_platform(self):
        for paths, windows in ((["README.md"], "false"), ([SOURCE + "windows.rs"], "true"), (None, "true")):
            with self.subTest(paths=paths), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "output"
                env = {"EVENT_NAME": "push", "REF": "refs/heads/main", "BEFORE_SHA": "a" * 40,
                       "SHA": "b" * 40, "GITHUB_OUTPUT": str(output)}
                with patch.dict(os.environ, env), patch("sys.argv", ["ci_paths.py", "--github"]), \
                        patch("ci_paths.push_paths", return_value=paths) as diff, \
                        patch("ci_paths.pull_request_paths") as merge_base_diff, \
                        patch("sys.stdout", new_callable=io.StringIO) as stdout:
                    ci_paths.main()
                diff.assert_called_once_with("a" * 40, "b" * 40)
                merge_base_diff.assert_not_called()
                values = dict(line.split("=", 1) for line in output.read_text().splitlines())
                self.assertEqual(values["windows"], windows)
                self.assertEqual(values["linux"], "true" if paths is None else "false")
                # A push to main never narrows a matrix, whatever it selects.
                self.assertEqual(len(json.loads(values["linux_matrix"])["include"]), 4)
                self.assertEqual("::warning::" in stdout.getvalue(), paths is None)


if __name__ == "__main__":
    unittest.main()
