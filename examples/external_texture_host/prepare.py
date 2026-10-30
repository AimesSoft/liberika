#!/usr/bin/env python3
"""Prepare the pinned host HAL without modifying the global Cargo registry."""
import glob
import hashlib
import io
from pathlib import Path
import shutil
import subprocess
import tarfile
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
VERSION = "26.0.6"
CHECKSUM = "a8d0e67224cc7305b3b4eb2cc57ca4c4c3afc665c1d1bee162ea806e19c47bdd"
DEST = ROOT / "target/host-wgpu-hal-26"
PATCH = HERE / "wgpu-hal-26-initial-usage.patch"
stamp = hashlib.sha256(PATCH.read_bytes()).hexdigest()
if (DEST / ".erika-patch").is_file() and (DEST / ".erika-patch").read_text() == stamp:
    print(f"Host HAL is ready: {DEST}")
    raise SystemExit(0)
cached = glob.glob(str(Path.home() / f".cargo/registry/cache/*/wgpu-hal-{VERSION}.crate"))
data = Path(cached[0]).read_bytes() if cached else urllib.request.urlopen(
    f"https://static.crates.io/crates/wgpu-hal/wgpu-hal-{VERSION}.crate", timeout=60
).read()
if hashlib.sha256(data).hexdigest() != CHECKSUM:
    raise SystemExit("Pinned wgpu-hal archive checksum mismatch")
stage = DEST.with_name(DEST.name + ".preparing")
if stage.exists():
    shutil.rmtree(stage)
stage.mkdir(parents=True)
prefix = f"wgpu-hal-{VERSION}/"
with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
    for member in archive:
        if not member.isfile() or not member.name.startswith(prefix):
            continue
        relative = Path(member.name[len(prefix):])
        if relative.is_absolute() or ".." in relative.parts:
            raise SystemExit("Unsafe path in archive")
        target = stage / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(archive.extractfile(member).read())
subprocess.run(["patch", "-p1", "-i", str(PATCH)], cwd=stage, check=True)
(stage / ".erika-patch").write_text(stamp)
if DEST.exists():
    shutil.rmtree(DEST)
stage.rename(DEST)
print(f"Prepared host HAL: {DEST}")
