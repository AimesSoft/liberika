"""OHPM builds cannot include another platform/package's files: check copies."""
from pathlib import Path

packages = Path(__file__).resolve().parents[3]
canonical = (packages / "erika_flutter/windows/erika_presenter_worker.h").read_bytes()
for path in [
    packages / "erika_flutter/ohos/src/main/cpp/presenter_worker.h",
    packages / "erika_ohos/src/main/cpp/presenter_worker.h",
]:
    assert path.read_bytes() == canonical, f"Refresh packaged worker: {path}"
print("Both independently packaged OHPM worker headers match the shared Windows/Linux owner")
