from pathlib import Path

package = Path(__file__).resolve().parents[2]
canonical = (package / "macos/Classes/ErikaPresenterOwner.swift").read_bytes()
for platform in ("ios", "tvos"):
    assert (package / platform / "Classes/ErikaPresenterOwner.swift").read_bytes() == canonical, platform
assert (package / "example/ios/RunnerTests/RunnerTests.swift").read_bytes() == (
    package / "example/macos/RunnerTests/RunnerTests.swift"
).read_bytes(), "RunnerTests copies differ"
print("Apple owner and RunnerTests copies match")
