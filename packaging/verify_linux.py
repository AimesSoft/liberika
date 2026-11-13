#!/usr/bin/env python3
"""Validate a Linux C ABI binary, then load it with immediate symbol resolution."""
import argparse
import ctypes
import os
from pathlib import Path
import re
import subprocess

MACHINES = {"x64": "Advanced Micro Devices X86-64", "arm64": "AArch64"}
FFMPEG_ABI = {
    "libavdevice": 62, "libavfilter": 11, "libavformat": 62,
    "libavcodec": 62, "libswresample": 6, "libswscale": 9, "libavutil": 60,
}
VERSION_LIMITS = {"GLIBC": (2, 35), "GLIBCXX": (3, 4, 30), "CXXABI": (1, 3, 13)}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def inspect_elf(header, dynamic, versions, symbols, api_header, arch, mode):
    """Pure validation, shared with fixture-based tests on non-Linux hosts."""
    require(re.search(r"Class:\s+ELF64", header), "Expected ELF64")
    require(re.search(r"Machine:\s+" + re.escape(MACHINES[arch]), header), "Wrong ELF architecture")
    require(re.search(r"\(SONAME\).*\[liberika_capi\.so\]", dynamic), "Missing C API SONAME")
    require(not re.search(r"\((?:RPATH|RUNPATH)\)", dynamic), "Unexpected embedded library search path")
    needed = sorted(set(re.findall(r"\(NEEDED\).*\[([^\]]+)\]", dynamic)))
    require(needed, "No dynamic dependencies reported")
    require(not any(n.startswith("libass.so") for n in needed), "Patched libass must remain embedded")
    if mode == "bundled":
        forbidden = tuple(FFMPEG_ABI) + ("libdav1d", "libfreetype", "libharfbuzz", "libfribidi", "libz.so")
        require(not any(n.startswith(forbidden) for n in needed), "Bundled binary directly imports a bundled dependency")
    else:
        # --as-needed may drop unused FFmpeg components (e.g. avdevice).
        for library in ("libavformat", "libavcodec", "libavutil", "libswscale", "libswresample"):
            require(f"{library}.so.{FFMPEG_ABI[library]}" in needed, f"Missing FFmpeg 8 ABI: {library}")
        for name in needed:
            for library, major in FFMPEG_ABI.items():
                if name.startswith(library + ".so"):
                    require(name == f"{library}.so.{major}", f"Wrong FFmpeg ABI: {name}")
    requirements = []
    for namespace, limit in VERSION_LIMITS.items():
        numbers = [tuple(map(int, v.split("."))) for v in re.findall(r"\b" + namespace + r"_([0-9]+(?:\.[0-9]+)+)\b", versions)]
        if numbers:
            maximum = max(numbers)
            require(maximum <= limit, f"{namespace} requirement {maximum} exceeds baseline {limit}")
            requirements.append(namespace + "_" + ".".join(map(str, maximum)))
    require("GLIBC_PRIVATE" not in versions and "GLIBC_ABI_" not in versions, "Unsupported glibc ABI extension")
    required_api = set(re.findall(r"\b(erika_\w+)\s*\(", api_header))
    require({"erika_create", "erika_destroy"} <= required_api, "Invalid API header")
    exported = set()
    for line in symbols.splitlines():
        columns = line.split()
        if len(columns) >= 8 and columns[6] != "UND":
            exported.add(columns[7].split("@")[0])
    missing = required_api - exported
    require(not missing, "Missing C ABI exports: " + ", ".join(sorted(missing)))
    return needed, requirements


def command(*args):
    return subprocess.check_output(args, text=True, env={**os.environ, "LC_ALL": "C"}, stderr=subprocess.STDOUT)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("library", type=Path)
    parser.add_argument("--header", type=Path, required=True)
    parser.add_argument("--arch", choices=MACHINES, required=True)
    parser.add_argument("--mode", choices=("bundled", "system"), required=True)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    library = args.library.resolve(strict=True)
    needed, requirements = inspect_elf(
        command("readelf", "-hW", str(library)), command("readelf", "-dW", str(library)),
        command("readelf", "-VW", str(library)), command("readelf", "--dyn-syms", "-W", str(library)),
        args.header.read_text(), args.arch, args.mode,
    )
    if args.archive:
        headers = command("readelf", "-hW", str(args.archive))
        machines = set(re.findall(r"Machine:\s+([^\n]+)", headers))
        require(machines == {MACHINES[args.arch]}, "Wrong architecture in static archive")
    resolved = command("ldd", "-r", str(library))
    require("not found" not in resolved and "undefined symbol" not in resolved, "Unresolved runtime dependencies:\n" + resolved)
    lib = ctypes.CDLL(str(library), mode=os.RTLD_NOW | os.RTLD_LOCAL)
    lib.erika_create.argtypes = []
    lib.erika_create.restype = ctypes.c_void_p
    lib.erika_destroy.argtypes = [ctypes.c_void_p]
    lib.erika_destroy.restype = None
    handle = lib.erika_create()
    require(handle, "erika_create returned NULL")
    lib.erika_destroy(handle)
    report = (
        f"Linux {args.arch} / {args.mode}\n"
        "Compatibility ceiling: GLIBC_2.35, GLIBCXX_3.4.30, CXXABI_1.3.13\n"
        "Direct DT_NEEDED (driver/fontconfig transitive dependencies are not bundled):\n"
        + "".join(f"  {n}\n" for n in needed)
        + "Required symbol versions: " + ", ".join(requirements) + "\n"
        + "Verified: architecture, SONAME, public C API, no rpath, ldd -r, RTLD_NOW, create/destroy.\n"
    )
    if args.report:
        args.report.write_text(report)
    print(report, end="")


if __name__ == "__main__":
    main()
