#!/usr/bin/env bash
# Compile and link erika + wgpu from a separate workspace, where this repository's
# [patch.crates-io] and .cargo/config.toml are not inherited.
set -euo pipefail
ERIKA_CONSUMER_REPO="$(cd "$(dirname "$0")/.." && pwd)"
ERIKA_CONSUMER_DIR="$(mktemp -d "${TMPDIR:-/tmp}/erika-rust-consumer.XXXXXX")"
trap 'rm -rf "$ERIKA_CONSUMER_DIR"' EXIT
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ERIKA_CONSUMER_REPO/target}"
mkdir -p "$CARGO_TARGET_DIR"
CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"
export CARGO_TARGET_DIR

python3 - "$ERIKA_CONSUMER_REPO" "$ERIKA_CONSUMER_DIR" <<'PY'
import json
import pathlib
import sys

repo, consumer = map(pathlib.Path, sys.argv[1:])
(consumer / "src").mkdir()
(consumer / "src/main.rs").write_text(
    "fn main() {\n"
    "    let constructor: fn() -> _ = erika::renderer::wgpu::WgpuRenderer::new;\n"
    "    let _ = std::hint::black_box(constructor);\n"
    "}\n"
)
# soundtouch-ffi disables cc-rs' PIC default. These settings belong to the
# consumer too; inheriting the repository's .cargo directory would hide this.
(consumer / ".cargo").mkdir()
(consumer / ".cargo/config.toml").write_text('''[env]
CFLAGS_x86_64_unknown_linux_gnu = "-fPIC"
CXXFLAGS_x86_64_unknown_linux_gnu = "-fPIC"
CFLAGS_aarch64_unknown_linux_gnu = "-fPIC"
CXXFLAGS_aarch64_unknown_linux_gnu = "-fPIC"
''')
(consumer / "Cargo.toml").write_text(f'''[package]
name = "erika-external-rust-consumer"
version = "0.0.0"
edition = "2024"

[workspace]

[dependencies]
erika = {{ path = {json.dumps(str(repo / "crates/erika"))}, features = ["wgpu"] }}

[patch.crates-io]
wgpu-hal = {{ path = {json.dumps(str(repo / "third_party/wgpu-hal"))} }}
''')
PY

cd "$ERIKA_CONSUMER_DIR"
cargo metadata --format-version 1 > metadata.json
python3 - "$ERIKA_CONSUMER_REPO" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path("metadata.json").read_text())
packages = [p for p in metadata["packages"] if p["name"] == "wgpu-hal"]
expected = (pathlib.Path(sys.argv[1]) / "third_party/wgpu-hal/Cargo.toml").resolve()
assert len(packages) == 1, f"Expected a single patched wgpu-hal, got {packages}"
assert pathlib.Path(packages[0]["manifest_path"]).resolve() == expected, packages[0]
assert pathlib.Path(metadata["workspace_root"]).resolve() == pathlib.Path.cwd().resolve()
print(f"External consumer resolves wgpu-hal {packages[0]['version']} from {expected}")
PY
cargo build --locked -j "${ERIKA_BUILD_JOBS:-4}"
