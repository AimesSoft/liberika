use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const FFMPEG_VERSION: &str = "8.1.2";
const GAS_PREPROCESSOR_REVISION: &str = "d09971fad329d32df19f5bbafe88cf2f0ed04ed7";
const GAS_PREPROCESSOR_PATCH_VERSION: &str = "erika-tbnz-v1";
const GAS_PREPROCESSOR_URL: &str = "https://raw.githubusercontent.com/libav/gas-preprocessor/d09971fad329d32df19f5bbafe88cf2f0ed04ed7/gas-preprocessor.pl";
const DAV1D_VERSION: &str = "1.5.1";
const LIBASS_VERSION: &str = "0.17.5";
const HARFBUZZ_VERSION: &str = "14.2.1";
const FREETYPE_VERSION: &str = "2.14.3";
const FRIBIDI_VERSION: &str = "1.0.16";
const ZLIB_VERSION: &str = "1.3.2";
const DEFAULT_ANDROID_API_LEVEL: u32 = 26;

const FFMPEG_ARCHIVE: &str = "ffmpeg-n8.1.2.tar.gz";
const FFMPEG_DIR: &str = "FFmpeg-n8.1.2";
const FFMPEG_URLS: &[&str] = &["https://codeload.github.com/FFmpeg/FFmpeg/tar.gz/refs/tags/n8.1.2"];
const FFMPEG_SHA256: &str = "9fd092511605bbebafe095ea6d38d9e40f34d12f7386e1258372df8be0576eb7";
const FFMPEG_PATCHSET_VERSION: &str = "erika-android-mediacodec-v4";
const FFMPEG_PATCHES: &[&str] = &[
    "third_party/patches/ffmpeg-8.1.2/0001-erika-mediacodec-bounded-receive.patch",
    "third_party/patches/ffmpeg-8.1.2/0002-windows-msvc-awk-backslash.patch",
];

const DAV1D_ARCHIVE: &str = "dav1d-1.5.1.tar.gz";
const DAV1D_DIR: &str = "dav1d-1.5.1";
const DAV1D_URLS: &[&str] = &["https://codeload.github.com/videolan/dav1d/tar.gz/refs/tags/1.5.1"];

const LIBASS_ARCHIVE: &str = "libass-0.17.5.tar.xz";
const LIBASS_DIR: &str = "libass-0.17.5";
const LIBASS_URLS: &[&str] = &[
    "https://github.com/libass/libass/releases/download/0.17.5/libass-0.17.5.tar.xz",
    "https://codeload.github.com/libass/libass/tar.gz/refs/tags/0.17.5",
];
const LIBASS_PATCHSET_VERSION: &str = "erika-ordered-font-fallback-v1";
const LIBASS_BUILD_VERSION: &str = "0.17.5+erika-ordered-font-fallback-v1";
const LIBASS_PATCHES: &[&str] =
    &["third_party/patches/libass-0.17.5/0001-erika-ordered-default-font-families.patch"];

const HARFBUZZ_ARCHIVE: &str = "harfbuzz-14.2.1.tar.xz";
const HARFBUZZ_DIR: &str = "harfbuzz-14.2.1";
const HARFBUZZ_URLS: &[&str] = &[
    "https://github.com/harfbuzz/harfbuzz/releases/download/14.2.1/harfbuzz-14.2.1.tar.xz",
    "https://codeload.github.com/harfbuzz/harfbuzz/tar.gz/refs/tags/14.2.1",
];

const FREETYPE_ARCHIVE: &str = "freetype-VER-2-14-3.tar.gz";
const FREETYPE_DIR: &str = "freetype-VER-2-14-3";
const FREETYPE_URLS: &[&str] =
    &["https://codeload.github.com/freetype/freetype/tar.gz/refs/tags/VER-2-14-3"];

const FRIBIDI_ARCHIVE: &str = "fribidi-1.0.16.tar.xz";
const FRIBIDI_DIR: &str = "fribidi-1.0.16";
const FRIBIDI_URLS: &[&str] = &[
    "https://github.com/fribidi/fribidi/releases/download/v1.0.16/fribidi-1.0.16.tar.xz",
    "https://codeload.github.com/fribidi/fribidi/tar.gz/refs/tags/v1.0.16",
];

const ZLIB_ARCHIVE: &str = "zlib-1.3.2.tar.gz";
const ZLIB_DIR: &str = "zlib-1.3.2";
const ZLIB_URLS: &[&str] = &["https://codeload.github.com/madler/zlib/tar.gz/refs/tags/v1.3.2"];

fn main() -> Result<()> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        print_help();
        return Ok(());
    }

    match args.remove(0).as_str() {
        "deps" => deps(args),
        "web" => {
            if args.first().map(String::as_str) != Some("build") {
                bail!("usage: cargo run -p xtask -- web build [--skip-native]");
            }
            let root = workspace_root()?;
            run(Command::new("python3")
                .arg(root.join("scripts/build_web.py"))
                .args(&args[1..])
                .current_dir(root))
        }
        "pkg-config-shim" => pkg_config_shim(args),
        "check" => check(args),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        command => bail!("unknown xtask command: {command}"),
    }
}

fn check(mut args: Vec<String>) -> Result<()> {
    if args.is_empty() {
        bail!("missing check subcommand: cargo-patches or license");
    }
    match args.remove(0).as_str() {
        "cargo-patches" => check_cargo_patches(),
        "license" => check_license_policy(),
        other => bail!("unknown check subcommand: {other}"),
    }
}

fn check_cargo_patches() -> Result<()> {
    let root = workspace_root()?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--all-features",
        ])
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .context("run cargo metadata for patch verification")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed while verifying patches ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse cargo metadata")?;
    let packages = metadata["packages"]
        .as_array()
        .context("cargo metadata did not contain a packages array")?;
    let resolved = packages
        .iter()
        .filter(|package| package["name"].as_str() == Some("wgpu-hal"))
        .collect::<Vec<_>>();
    if resolved.len() != 1 {
        let versions = resolved
            .iter()
            .map(|package| {
                format!(
                    "{} ({})",
                    package["version"].as_str().unwrap_or("unknown version"),
                    package["source"].as_str().unwrap_or("path")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "expected exactly one resolved wgpu-hal package from the vendored patch, found {}: [{}]",
            resolved.len(),
            versions
        );
    }

    let package = resolved[0];
    if !package["source"].is_null() {
        bail!(
            "vendored wgpu-hal patch is inactive; Cargo resolved {} from {}",
            package["version"].as_str().unwrap_or("unknown version"),
            package["source"].as_str().unwrap_or("an unknown source")
        );
    }
    let actual_manifest = package["manifest_path"]
        .as_str()
        .context("resolved wgpu-hal package has no manifest_path")?;
    let actual_manifest = fs::canonicalize(actual_manifest)
        .with_context(|| format!("canonicalize resolved wgpu-hal manifest {actual_manifest}"))?;
    let expected_manifest = fs::canonicalize(root.join("third_party/wgpu-hal/Cargo.toml"))
        .context("canonicalize vendored wgpu-hal manifest")?;
    if actual_manifest != expected_manifest {
        bail!(
            "vendored wgpu-hal patch is inactive; expected {}, resolved {}",
            expected_manifest.display(),
            actual_manifest.display()
        );
    }

    println!(
        "cargo patch ok: wgpu-hal {} resolves from {}",
        package["version"].as_str().unwrap_or("unknown version"),
        expected_manifest.display()
    );
    Ok(())
}

fn deps(mut args: Vec<String>) -> Result<()> {
    if args.is_empty() {
        bail!("missing deps subcommand: plan, fetch, status, smoke-ffmpeg-make, or build");
    }
    let subcommand = args.remove(0);
    let options = DepsOptions::parse(&args)?;
    match subcommand.as_str() {
        "plan" => {
            print_dependency_plan(options.profile, options.target, options.crt);
            Ok(())
        }
        "fetch" => {
            print_dependency_plan(options.profile, options.target, options.crt);
            let layout = workspace_layout(options.profile, options.target, options.crt)?;
            fetch_dependency_sources(&layout, options.all)?;
            write_profile_metadata(&layout, options.profile, options.target)
        }
        "status" => print_dependency_status(&workspace_layout(
            options.profile,
            options.target,
            options.crt,
        )?),
        "smoke-ffmpeg-make" => smoke_ffmpeg_make(options),
        "build" => {
            print_dependency_plan(options.profile, options.target, options.crt);
            build_dependencies(options)
        }
        other => bail!("unknown deps subcommand: {other}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeDependencyProfile {
    Lgpl,
    GplFull,
}

/// The MSVC C runtime the Windows libraries link against: the DLL (`/MD`,
/// as Rust links by default) or the static one (`/MT`, for hosts built with
/// `-C target-feature=+crt-static`). A static build goes to its own
/// `<profile>-crt-static` directory, where `erika_ffmpeg_sys` looks when
/// Rust links the CRT statically. Other targets ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsCrt {
    Dynamic,
    Static,
}

impl WindowsCrt {
    fn ffmpeg_cflag(self) -> &'static str {
        match self {
            Self::Dynamic => "--extra-cflags=-MD",
            Self::Static => "--extra-cflags=-MT",
        }
    }

    fn dir_suffix(self) -> &'static str {
        match self {
            Self::Dynamic => "",
            Self::Static => "-crt-static",
        }
    }
}

impl NativeDependencyProfile {
    fn ffmpeg_configure_flags(self) -> &'static [&'static str] {
        match self {
            Self::Lgpl => &[
                "--disable-gpl",
                "--enable-version3",
                "--enable-static",
                "--disable-shared",
                "--disable-programs",
                "--disable-doc",
                "--disable-network",
                "--disable-autodetect",
                "--enable-zlib",
                "--enable-libdav1d",
                "--enable-decoder=libdav1d",
                "--enable-protocol=file",
                "--enable-demuxer=asf,mov,matroska,mpegts,mpegps,mpegvideo,avi,flv,h264,hevc,av1,ivf,mp3,aac,flac,wav,ogg,ac3,eac3,dts,truehd,mlp,mjpeg,vc1,ass,srt,webvtt",
                "--enable-parser=hevc,h264,av1,vp9,aac,ac3,dca,mlp,opus,vorbis,flac,mpegaudio,mpegvideo,mpeg4video,mjpeg,vc1,dvdsub,dvbsub",
                "--enable-decoder=wmv1,wmv2,wmv3,vc1,wmav1,wmav2,wmapro,wmalossless,hevc,h264,av1,vp8,vp9,mpeg1video,mpeg2video,mpeg4,mjpeg,flv,theora,aac,ac3,eac3,dca,truehd,mlp,opus,vorbis,flac,mp3,pcm_s16le,pcm_s24le,pcm_s32le,ass,srt,webvtt,pgssub,dvdsub,dvbsub",
                "--enable-encoder=gif",
                "--enable-muxer=gif",
            ],
            Self::GplFull => &[
                "--enable-gpl",
                "--enable-version3",
                "--enable-static",
                "--disable-shared",
                "--disable-programs",
                "--disable-doc",
                "--disable-network",
                "--disable-autodetect",
                "--enable-zlib",
                "--enable-libdav1d",
                "--enable-decoder=libdav1d",
                "--enable-protocol=file",
                "--enable-demuxer=asf,mov,matroska,mpegts,mpegps,mpegvideo,avi,flv,h264,hevc,av1,ivf,mp3,aac,flac,wav,ogg,ac3,eac3,dts,truehd,mlp,mjpeg,vc1,ass,srt,webvtt",
                "--enable-parser=hevc,h264,av1,vp9,aac,ac3,dca,mlp,opus,vorbis,flac,mpegaudio,mpegvideo,mpeg4video,mjpeg,vc1,dvdsub,dvbsub",
                "--enable-decoder=wmv1,wmv2,wmv3,vc1,wmav1,wmav2,wmapro,wmalossless,hevc,h264,av1,vp8,vp9,mpeg1video,mpeg2video,mpeg4,mjpeg,flv,theora,aac,ac3,eac3,dca,truehd,mlp,opus,vorbis,flac,mp3,pcm_s16le,pcm_s24le,pcm_s32le,ass,srt,webvtt,pgssub,dvdsub,dvbsub",
                "--enable-encoder=gif",
                "--enable-muxer=gif",
            ],
        }
    }

    #[cfg(test)]
    fn ffmpeg_configure_flags_for_target(self, target: NativeTarget) -> Vec<&'static str> {
        self.ffmpeg_configure_flags_for(target, WindowsCrt::Dynamic)
    }

    fn ffmpeg_configure_flags_for(
        self,
        target: NativeTarget,
        crt: WindowsCrt,
    ) -> Vec<&'static str> {
        let mut flags = self.ffmpeg_configure_flags().to_vec();
        if target.is_windows() {
            // Match the Meson/CMake dependencies and the CRT Rust links,
            // including configure's link probes against static dav1d. Keep
            // this in the recorded flags so old builds are invalidated.
            flags.extend(["--enable-d3d11va", "--enable-dxva2", crt.ffmpeg_cflag()]);
        } else if target.is_android() {
            flags.extend([
                "--enable-jni",
                "--enable-mediacodec",
                "--enable-decoder=h264_mediacodec,hevc_mediacodec,mpeg2_mediacodec,mpeg4_mediacodec,vp8_mediacodec,vp9_mediacodec,av1_mediacodec,libdav1d",
            ]);
            // FFmpeg's 32-bit external and inline x86 assembly still emits
            // absolute R_386_32 relocations even with CONFIG_PIC enabled.
            // Android has rejected text relocations since API 23, so keep the
            // complete C decoder set while disabling the incompatible asm
            // acceleration paths for this legacy ABI.
            if matches!(target, NativeTarget::I686Android) {
                flags.push("--disable-asm");
            }
        } else if target.is_apple() {
            flags.push("--enable-videotoolbox");
        }
        if target.is_android() {
            assert_android_software_decoder_fallbacks(&flags);
        }
        flags
    }
}

fn assert_android_software_decoder_fallbacks(flags: &[&str]) {
    let enabled = flags
        .iter()
        .filter_map(|flag| flag.strip_prefix("--enable-decoder="))
        .flat_map(|decoders| decoders.split(','))
        .collect::<std::collections::HashSet<_>>();
    for (hardware, software) in [
        ("h264_mediacodec", "h264"),
        ("hevc_mediacodec", "hevc"),
        ("mpeg2_mediacodec", "mpeg2video"),
        ("mpeg4_mediacodec", "mpeg4"),
        ("vp8_mediacodec", "vp8"),
        ("vp9_mediacodec", "vp9"),
        ("av1_mediacodec", "libdav1d"),
    ] {
        assert!(
            !enabled.contains(hardware) || enabled.contains(software),
            "Android FFmpeg enables {hardware} without required software fallback {software}"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeTarget {
    Host,
    Aarch64Macos,
    X86_64Macos,
    Aarch64Ios,
    Aarch64IosSimulator,
    X86_64IosSimulator,
    Aarch64Tvos,
    Aarch64TvosSimulator,
    X86_64TvosSimulator,
    X86_64WindowsMsvc,
    Aarch64WindowsMsvc,
    Aarch64Android,
    Armv7Android,
    X86_64Android,
    I686Android,
    Aarch64Ohos,
    X86_64Ohos,
}

impl NativeTarget {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "host" => Ok(Self::Host),
            "aarch64-apple-darwin" => Ok(Self::Aarch64Macos),
            "x86_64-apple-darwin" => Ok(Self::X86_64Macos),
            "aarch64-apple-ios" => Ok(Self::Aarch64Ios),
            "aarch64-apple-ios-sim" => Ok(Self::Aarch64IosSimulator),
            "x86_64-apple-ios" => Ok(Self::X86_64IosSimulator),
            "aarch64-apple-tvos" => Ok(Self::Aarch64Tvos),
            "aarch64-apple-tvos-sim" => Ok(Self::Aarch64TvosSimulator),
            "x86_64-apple-tvos" => Ok(Self::X86_64TvosSimulator),
            "x86_64-pc-windows-msvc" | "windows-x64" => Ok(Self::X86_64WindowsMsvc),
            "aarch64-pc-windows-msvc" | "windows-arm64" => Ok(Self::Aarch64WindowsMsvc),
            "aarch64-linux-android" | "arm64-v8a" => Ok(Self::Aarch64Android),
            "armv7-linux-androideabi" | "armeabi-v7a" => Ok(Self::Armv7Android),
            "x86_64-linux-android" | "android-x64" => Ok(Self::X86_64Android),
            "i686-linux-android" | "x86" => Ok(Self::I686Android),
            "aarch64-unknown-linux-ohos" | "ohos-arm64" => Ok(Self::Aarch64Ohos),
            "x86_64-unknown-linux-ohos" | "ohos-x64" => Ok(Self::X86_64Ohos),
            other => bail!("unknown native target: {other}"),
        }
    }

    fn triple(self) -> Option<&'static str> {
        match self {
            Self::Host => None,
            Self::Aarch64Macos => Some("aarch64-apple-darwin"),
            Self::X86_64Macos => Some("x86_64-apple-darwin"),
            Self::Aarch64Ios => Some("aarch64-apple-ios"),
            Self::Aarch64IosSimulator => Some("aarch64-apple-ios-sim"),
            Self::X86_64IosSimulator => Some("x86_64-apple-ios"),
            Self::Aarch64Tvos => Some("aarch64-apple-tvos"),
            Self::Aarch64TvosSimulator => Some("aarch64-apple-tvos-sim"),
            Self::X86_64TvosSimulator => Some("x86_64-apple-tvos"),
            Self::X86_64WindowsMsvc => Some("x86_64-pc-windows-msvc"),
            Self::Aarch64WindowsMsvc => Some("aarch64-pc-windows-msvc"),
            Self::Aarch64Android => Some("aarch64-linux-android"),
            Self::Armv7Android => Some("armv7-linux-androideabi"),
            Self::X86_64Android => Some("x86_64-linux-android"),
            Self::I686Android => Some("i686-linux-android"),
            Self::Aarch64Ohos => Some("aarch64-unknown-linux-ohos"),
            Self::X86_64Ohos => Some("x86_64-unknown-linux-ohos"),
        }
    }

    fn sdk(self) -> Option<&'static str> {
        match self {
            Self::Host => None,
            Self::Aarch64Macos | Self::X86_64Macos => Some("macosx"),
            Self::Aarch64Ios => Some("iphoneos"),
            Self::Aarch64IosSimulator | Self::X86_64IosSimulator => Some("iphonesimulator"),
            Self::Aarch64Tvos => Some("appletvos"),
            Self::Aarch64TvosSimulator | Self::X86_64TvosSimulator => Some("appletvsimulator"),
            Self::X86_64WindowsMsvc
            | Self::Aarch64WindowsMsvc
            | Self::Aarch64Android
            | Self::Armv7Android
            | Self::X86_64Android
            | Self::I686Android
            | Self::Aarch64Ohos
            | Self::X86_64Ohos => None,
        }
    }

    fn ffmpeg_arch(self) -> Option<&'static str> {
        match self {
            Self::Host => None,
            Self::Aarch64Macos
            | Self::Aarch64Ios
            | Self::Aarch64IosSimulator
            | Self::Aarch64Tvos
            | Self::Aarch64TvosSimulator => Some("arm64"),
            Self::X86_64Macos | Self::X86_64IosSimulator | Self::X86_64TvosSimulator => {
                Some("x86_64")
            }
            Self::X86_64WindowsMsvc => Some("x86_64"),
            Self::Aarch64WindowsMsvc => Some("aarch64"),
            Self::Aarch64Android => Some("aarch64"),
            Self::Armv7Android => Some("arm"),
            Self::X86_64Android => Some("x86_64"),
            Self::I686Android => Some("x86"),
            Self::Aarch64Ohos => Some("aarch64"),
            Self::X86_64Ohos => Some("x86_64"),
        }
    }

    fn meson_cpu_family(self) -> Option<&'static str> {
        match self {
            Self::Host => None,
            Self::Aarch64Macos
            | Self::Aarch64Ios
            | Self::Aarch64IosSimulator
            | Self::Aarch64Tvos
            | Self::Aarch64TvosSimulator => Some("aarch64"),
            Self::X86_64Macos | Self::X86_64IosSimulator | Self::X86_64TvosSimulator => {
                Some("x86_64")
            }
            Self::X86_64WindowsMsvc => Some("x86_64"),
            Self::Aarch64WindowsMsvc => Some("aarch64"),
            Self::Aarch64Android => Some("aarch64"),
            Self::Armv7Android => Some("arm"),
            Self::X86_64Android => Some("x86_64"),
            Self::I686Android => Some("x86"),
            Self::Aarch64Ohos => Some("aarch64"),
            Self::X86_64Ohos => Some("x86_64"),
        }
    }

    fn meson_cpu(self) -> Option<&'static str> {
        match self {
            Self::Host => None,
            Self::Aarch64Macos
            | Self::Aarch64Ios
            | Self::Aarch64IosSimulator
            | Self::Aarch64Tvos
            | Self::Aarch64TvosSimulator => Some("arm64"),
            Self::X86_64Macos | Self::X86_64IosSimulator | Self::X86_64TvosSimulator => {
                Some("x86_64")
            }
            Self::X86_64WindowsMsvc => Some("x86_64"),
            Self::Aarch64WindowsMsvc => Some("arm64"),
            Self::Aarch64Android => Some("aarch64"),
            Self::Armv7Android => Some("armv7"),
            Self::X86_64Android => Some("x86_64"),
            Self::I686Android => Some("i686"),
            Self::Aarch64Ohos => Some("aarch64"),
            Self::X86_64Ohos => Some("x86_64"),
        }
    }

    fn android_abi(self) -> Option<&'static str> {
        match self {
            Self::Aarch64Android => Some("arm64-v8a"),
            Self::Armv7Android => Some("armeabi-v7a"),
            Self::X86_64Android => Some("x86_64"),
            Self::I686Android => Some("x86"),
            _ => None,
        }
    }

    fn android_clang_triple(self) -> Option<&'static str> {
        match self {
            Self::Aarch64Android => Some("aarch64-linux-android"),
            Self::Armv7Android => Some("armv7a-linux-androideabi"),
            Self::X86_64Android => Some("x86_64-linux-android"),
            Self::I686Android => Some("i686-linux-android"),
            _ => None,
        }
    }

    fn is_ios(self) -> bool {
        matches!(
            self,
            Self::Aarch64Ios | Self::Aarch64IosSimulator | Self::X86_64IosSimulator
        )
    }

    fn is_tvos(self) -> bool {
        matches!(
            self,
            Self::Aarch64Tvos | Self::Aarch64TvosSimulator | Self::X86_64TvosSimulator
        )
    }

    fn is_windows(self) -> bool {
        matches!(self, Self::X86_64WindowsMsvc | Self::Aarch64WindowsMsvc)
            || (matches!(self, Self::Host) && cfg!(windows))
    }

    fn is_apple(self) -> bool {
        matches!(
            self,
            Self::Aarch64Macos
                | Self::X86_64Macos
                | Self::Aarch64Ios
                | Self::Aarch64IosSimulator
                | Self::X86_64IosSimulator
                | Self::Aarch64Tvos
                | Self::Aarch64TvosSimulator
                | Self::X86_64TvosSimulator
        ) || (matches!(self, Self::Host) && cfg!(target_vendor = "apple"))
    }

    fn is_android(self) -> bool {
        matches!(
            self,
            Self::Aarch64Android | Self::Armv7Android | Self::X86_64Android | Self::I686Android
        ) || (matches!(self, Self::Host) && cfg!(target_os = "android"))
    }

    fn is_ohos(self) -> bool {
        matches!(self, Self::Aarch64Ohos | Self::X86_64Ohos)
            || (matches!(self, Self::Host) && cfg!(target_env = "ohos"))
    }

    fn ohos_abi(self) -> Option<&'static str> {
        match self {
            Self::Aarch64Ohos => Some("arm64-v8a"),
            Self::X86_64Ohos => Some("x86_64"),
            _ => None,
        }
    }

    fn deployment_target(self) -> Option<(String, &'static str)> {
        match self {
            Self::Host => None,
            Self::Aarch64Macos | Self::X86_64Macos => Some((
                env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".to_string()),
                "-mmacosx-version-min",
            )),
            Self::Aarch64Ios => Some((
                env::var("IPHONEOS_DEPLOYMENT_TARGET").unwrap_or_else(|_| "13.0".to_string()),
                "-miphoneos-version-min",
            )),
            Self::Aarch64IosSimulator | Self::X86_64IosSimulator => Some((
                env::var("IPHONEOS_DEPLOYMENT_TARGET").unwrap_or_else(|_| "13.0".to_string()),
                "-mios-simulator-version-min",
            )),
            Self::Aarch64Tvos => Some((
                env::var("TVOS_DEPLOYMENT_TARGET").unwrap_or_else(|_| "13.0".to_string()),
                "-mtvos-version-min",
            )),
            Self::Aarch64TvosSimulator | Self::X86_64TvosSimulator => Some((
                env::var("TVOS_DEPLOYMENT_TARGET").unwrap_or_else(|_| "13.0".to_string()),
                "-mtvos-simulator-version-min",
            )),
            Self::X86_64WindowsMsvc
            | Self::Aarch64WindowsMsvc
            | Self::Aarch64Android
            | Self::Armv7Android
            | Self::X86_64Android
            | Self::I686Android
            | Self::Aarch64Ohos
            | Self::X86_64Ohos => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DepsOptions {
    profile: NativeDependencyProfile,
    target: NativeTarget,
    crt: WindowsCrt,
    force: bool,
    all: bool,
    jobs: Option<usize>,
}

impl DepsOptions {
    fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self {
            profile: NativeDependencyProfile::Lgpl,
            target: NativeTarget::Host,
            crt: WindowsCrt::Dynamic,
            force: false,
            all: false,
            jobs: None,
        };
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--profile" => {
                    let value = args.get(index + 1).context("--profile requires a value")?;
                    options.profile = match value.as_str() {
                        "lgpl" => NativeDependencyProfile::Lgpl,
                        "gpl-full" => NativeDependencyProfile::GplFull,
                        other => bail!("unknown dependency profile: {other}"),
                    };
                    index += 2;
                }
                "--target" => {
                    let value = args.get(index + 1).context("--target requires a value")?;
                    options.target = NativeTarget::parse(value)?;
                    index += 2;
                }
                "--crt" => {
                    let value = args.get(index + 1).context("--crt requires a value")?;
                    options.crt = match value.as_str() {
                        "dynamic" => WindowsCrt::Dynamic,
                        "static" => WindowsCrt::Static,
                        other => bail!("unknown C runtime: {other} (dynamic or static)"),
                    };
                    index += 2;
                }
                "--force" => {
                    options.force = true;
                    index += 1;
                }
                "--all" => {
                    options.all = true;
                    index += 1;
                }
                "--jobs" => {
                    let value = args.get(index + 1).context("--jobs requires a value")?;
                    options.jobs =
                        Some(value.parse().context("--jobs must be a positive integer")?);
                    index += 2;
                }
                other => bail!("unknown deps option: {other}"),
            }
        }
        if options.crt == WindowsCrt::Static && !options.target.is_windows() {
            bail!("--crt static applies to Windows targets only");
        }
        Ok(options)
    }
}

#[derive(Debug)]
struct WorkspaceLayout {
    root: PathBuf,
    target: NativeTarget,
    cache_dir: PathBuf,
    source_dir: PathBuf,
    build_dir: PathBuf,
    dist_dir: PathBuf,
    ffmpeg_source_dir: PathBuf,
    ffmpeg_build_dir: PathBuf,
    ffmpeg_build_marker: PathBuf,
    ffmpeg_prefix: PathBuf,
    dav1d_source_dir: PathBuf,
    dav1d_build_dir: PathBuf,
    dav1d_build_marker: PathBuf,
    dav1d_prefix: PathBuf,
    libass_source_dir: PathBuf,
    libass_build_dir: PathBuf,
    libass_build_marker: PathBuf,
    libass_prefix: PathBuf,
    harfbuzz_source_dir: PathBuf,
    harfbuzz_build_dir: PathBuf,
    harfbuzz_build_marker: PathBuf,
    harfbuzz_prefix: PathBuf,
    freetype_source_dir: PathBuf,
    freetype_build_dir: PathBuf,
    freetype_build_marker: PathBuf,
    freetype_prefix: PathBuf,
    fribidi_source_dir: PathBuf,
    fribidi_build_dir: PathBuf,
    fribidi_build_marker: PathBuf,
    fribidi_prefix: PathBuf,
    zlib_source_dir: PathBuf,
    zlib_build_dir: PathBuf,
    zlib_build_marker: PathBuf,
    zlib_prefix: PathBuf,
    python_tools_dir: PathBuf,
}

fn workspace_layout(
    profile: NativeDependencyProfile,
    target: NativeTarget,
    crt: WindowsCrt,
) -> Result<WorkspaceLayout> {
    let root = workspace_root()?;
    let cache_dir = root.join("third_party/cache");
    let source_dir = root.join("third_party/src");
    let profile_dir = format!("{}{}", profile_name(profile), crt.dir_suffix());
    let (build_dir, dist_dir) = if let Some(triple) = target.triple() {
        (
            root.join("third_party/build")
                .join(triple)
                .join(&profile_dir),
            root.join("third_party/dist")
                .join(triple)
                .join(&profile_dir),
        )
    } else {
        (
            root.join("third_party/build").join(&profile_dir),
            root.join("third_party/dist").join(&profile_dir),
        )
    };
    let ffmpeg_source_dir = source_dir.join(FFMPEG_DIR);
    let ffmpeg_build_dir = build_dir.join("ffmpeg");
    let ffmpeg_build_marker = ffmpeg_build_dir.join("ffmpeg-built.txt");
    let ffmpeg_prefix = dist_dir.join("ffmpeg");
    let dav1d_source_dir = source_dir.join(DAV1D_DIR);
    let dav1d_build_dir = build_dir.join("dav1d");
    let dav1d_build_marker = dav1d_build_dir.join("dav1d-built.txt");
    let dav1d_prefix = dist_dir.join("dav1d");
    let libass_source_dir = source_dir.join(LIBASS_DIR);
    let libass_build_dir = build_dir.join("libass");
    let libass_build_marker = libass_build_dir.join("libass-built.txt");
    let libass_prefix = dist_dir.join("libass");
    let harfbuzz_source_dir = source_dir.join(HARFBUZZ_DIR);
    let harfbuzz_build_dir = build_dir.join("harfbuzz");
    let harfbuzz_build_marker = harfbuzz_build_dir.join("harfbuzz-built.txt");
    let harfbuzz_prefix = dist_dir.join("harfbuzz");
    let freetype_source_dir = source_dir.join(FREETYPE_DIR);
    let freetype_build_dir = build_dir.join("freetype");
    let freetype_build_marker = freetype_build_dir.join("freetype-built.txt");
    let freetype_prefix = dist_dir.join("freetype");
    let fribidi_source_dir = source_dir.join(FRIBIDI_DIR);
    let fribidi_build_dir = build_dir.join("fribidi");
    let fribidi_build_marker = fribidi_build_dir.join("fribidi-built.txt");
    let fribidi_prefix = dist_dir.join("fribidi");
    let zlib_source_dir = source_dir.join(ZLIB_DIR);
    let zlib_build_dir = build_dir.join("zlib");
    let zlib_build_marker = zlib_build_dir.join("zlib-built.txt");
    let zlib_prefix = dist_dir.join("zlib");
    let python_tools_dir = build_dir.join("python-tools");
    Ok(WorkspaceLayout {
        root,
        target,
        cache_dir,
        source_dir,
        build_dir,
        dist_dir,
        ffmpeg_source_dir,
        ffmpeg_build_dir,
        ffmpeg_build_marker,
        ffmpeg_prefix,
        dav1d_source_dir,
        dav1d_build_dir,
        dav1d_build_marker,
        dav1d_prefix,
        libass_source_dir,
        libass_build_dir,
        libass_build_marker,
        libass_prefix,
        harfbuzz_source_dir,
        harfbuzz_build_dir,
        harfbuzz_build_marker,
        harfbuzz_prefix,
        freetype_source_dir,
        freetype_build_dir,
        freetype_build_marker,
        freetype_prefix,
        fribidi_source_dir,
        fribidi_build_dir,
        fribidi_build_marker,
        fribidi_prefix,
        zlib_source_dir,
        zlib_build_dir,
        zlib_build_marker,
        zlib_prefix,
        python_tools_dir,
    })
}

fn print_dependency_plan(profile: NativeDependencyProfile, target: NativeTarget, crt: WindowsCrt) {
    println!("Erika native dependency plan");
    println!("profile: {}", profile_name(profile));
    println!("target: {}", target.triple().unwrap_or("host"));
    if let Some(abi) = target.android_abi() {
        println!("android ABI: {abi}");
        match android_api_level() {
            Ok(api_level) => println!("android API level: {api_level}"),
            Err(error) => println!("android API level: invalid ({error})"),
        }
    }
    println!("ffmpeg: {FFMPEG_VERSION} ({})", FFMPEG_URLS[0]);
    if target.is_android() {
        println!("ffmpeg patch set: {FFMPEG_PATCHSET_VERSION}");
    }
    println!("dav1d: {DAV1D_VERSION} ({})", DAV1D_URLS[0]);
    println!("libass: {LIBASS_VERSION} ({})", LIBASS_URLS[0]);
    println!("harfbuzz: {HARFBUZZ_VERSION} ({})", HARFBUZZ_URLS[0]);
    println!("freetype: {FREETYPE_VERSION} ({})", FREETYPE_URLS[0]);
    println!("fribidi: {FRIBIDI_VERSION} ({})", FRIBIDI_URLS[0]);
    println!("zlib: {ZLIB_VERSION} ({})", ZLIB_URLS[0]);
    println!("ffmpeg configure flags:");
    for flag in profile.ffmpeg_configure_flags_for(target, crt) {
        println!("  {flag}");
    }
    println!(
        "text/subtitle dependencies are source-fetched in v0 and linked when libass rendering lands"
    );
}

fn fetch_dependency_sources(layout: &WorkspaceLayout, all: bool) -> Result<()> {
    fs::create_dir_all(&layout.cache_dir)
        .with_context(|| format!("create {}", layout.cache_dir.display()))?;
    fs::create_dir_all(&layout.source_dir)
        .with_context(|| format!("create {}", layout.source_dir.display()))?;

    fetch_and_extract(
        layout,
        FFMPEG_URLS,
        FFMPEG_ARCHIVE,
        FFMPEG_DIR,
        Some(FFMPEG_SHA256),
    )?;
    apply_ffmpeg_patches(layout)?;
    fetch_and_extract(layout, ZLIB_URLS, ZLIB_ARCHIVE, ZLIB_DIR, None)?;
    fetch_and_extract(layout, DAV1D_URLS, DAV1D_ARCHIVE, DAV1D_DIR, None)?;
    if all {
        fetch_and_extract(layout, LIBASS_URLS, LIBASS_ARCHIVE, LIBASS_DIR, None)?;
        apply_libass_patches(layout)?;
        fetch_and_extract(layout, HARFBUZZ_URLS, HARFBUZZ_ARCHIVE, HARFBUZZ_DIR, None)?;
        fetch_and_extract(layout, FREETYPE_URLS, FREETYPE_ARCHIVE, FREETYPE_DIR, None)?;
        fetch_and_extract(layout, FRIBIDI_URLS, FRIBIDI_ARCHIVE, FRIBIDI_DIR, None)?;
    } else {
        println!(
            "skip text/subtitle source fetch; pass --all when preparing libass/HarfBuzz/FreeType work"
        );
    }
    Ok(())
}

fn build_dependencies(options: DepsOptions) -> Result<()> {
    let layout = workspace_layout(options.profile, options.target, options.crt)?;
    ensure_required_tools(options, &layout)?;
    prepare_dependency_dirs(&layout)?;
    fetch_dependency_sources(&layout, options.all)?;
    build_zlib(&layout, options)?;
    build_dav1d(&layout, options)?;
    build_ffmpeg(&layout, options)?;
    if options.all {
        build_text_dependencies(&layout, options)?;
    }
    write_profile_metadata(&layout, options.profile, options.target)?;
    println!(
        "\nNative dependencies are ready at {}",
        layout.dist_dir.display()
    );
    Ok(())
}

fn print_dependency_status(layout: &WorkspaceLayout) -> Result<()> {
    println!("Erika native dependency status");
    println!("target: {}", layout.target.triple().unwrap_or("host"));
    println!("workspace: {}", layout.root.display());
    println!("cache dir: {}", layout.cache_dir.display());
    println!("source dir: {}", layout.source_dir.display());
    println!("dist dir: {}", layout.dist_dir.display());
    println!(
        "ffmpeg source: {}",
        status_word(layout.ffmpeg_source_dir.exists())
    );
    println!(
        "ffmpeg dist: {}",
        status_word(native_static_lib_exists(&layout.ffmpeg_prefix, "avformat"))
    );
    println!(
        "dav1d source: {}",
        status_word(layout.dav1d_source_dir.exists())
    );
    println!(
        "dav1d dist: {}",
        status_word(native_static_lib_exists(&layout.dav1d_prefix, "dav1d"))
    );
    println!(
        "zlib source: {}",
        status_word(layout.zlib_source_dir.exists())
    );
    println!(
        "zlib dist: {}",
        status_word(
            native_static_lib_exists(&layout.zlib_prefix, "z")
                || native_static_lib_exists(&layout.zlib_prefix, "zlib")
        )
    );
    println!(
        "libass source: {}",
        status_word(layout.libass_source_dir.exists())
    );
    println!(
        "harfbuzz source: {}",
        status_word(layout.harfbuzz_source_dir.exists())
    );
    println!(
        "freetype source: {}",
        status_word(layout.freetype_source_dir.exists())
    );
    println!(
        "fribidi source: {}",
        status_word(layout.fribidi_source_dir.exists())
    );
    println!(
        "freetype dist: {}",
        status_word(native_static_lib_exists(
            &layout.freetype_prefix,
            "freetype"
        ))
    );
    println!(
        "harfbuzz dist: {}",
        status_word(native_static_lib_exists(
            &layout.harfbuzz_prefix,
            "harfbuzz"
        ))
    );
    println!(
        "fribidi dist: {}",
        status_word(native_static_lib_exists(&layout.fribidi_prefix, "fribidi"))
    );
    println!(
        "libass dist: {}",
        status_word(native_static_lib_exists(&layout.libass_prefix, "ass"))
    );
    if layout.dist_dir.join("erika-native-deps.txt").exists() {
        println!(
            "metadata: {}",
            layout.dist_dir.join("erika-native-deps.txt").display()
        );
    } else {
        println!("metadata: missing");
    }
    Ok(())
}

fn prepare_dependency_dirs(layout: &WorkspaceLayout) -> Result<()> {
    fs::create_dir_all(&layout.build_dir)
        .with_context(|| format!("create {}", layout.build_dir.display()))?;
    fs::create_dir_all(&layout.ffmpeg_build_dir)
        .with_context(|| format!("create {}", layout.ffmpeg_build_dir.display()))?;
    fs::create_dir_all(&layout.dist_dir)
        .with_context(|| format!("create {}", layout.dist_dir.display()))?;
    println!("workspace: {}", layout.root.display());
    println!("cache dir: {}", layout.cache_dir.display());
    println!("source dir: {}", layout.source_dir.display());
    println!("build dir: {}", layout.build_dir.display());
    println!("dist dir: {}", layout.dist_dir.display());
    Ok(())
}

fn ensure_required_tools(options: DepsOptions, layout: &WorkspaceLayout) -> Result<()> {
    for tool in ["tar"] {
        if which(tool).is_none() {
            bail!("required build tool `{tool}` was not found in PATH");
        }
    }

    if options.target.is_windows() {
        let _ = windows_msvc_environment(options.target)?;
        if posix_shell().is_none() {
            bail!(
                "required POSIX shell was not found; install Git for Windows or MSYS2 so FFmpeg configure can run"
            );
        }
        if gnu_make().is_none() {
            bail!("required GNU make was not found; install MSYS2 make or MinGW mingw32-make");
        }
        if ((ffmpeg_requires_nasm(options.target)
            && !ffmpeg_build_marker_is_current(layout, options))
            || (dav1d_requires_nasm(options.target)
                && !dav1d_build_marker_is_current(layout, options)))
            && which("nasm").is_none()
        {
            bail!(
                "required build tool `nasm` was not found for Windows x86_64 FFmpeg/dav1d assembly"
            );
        }
        if cmake_tool().is_none() {
            bail!("required CMake was not found; install the Visual Studio CMake component");
        }
        if python_tool().is_none() {
            bail!("required Python with venv support was not found in PATH");
        }
        let _ = ensure_pkg_config_shim(layout)?;
        return Ok(());
    }

    if options.target.is_android() {
        let toolchain = android_toolchain(options.target)?
            .context("an explicit Android target requires an Android NDK toolchain")?;
        println!("Android NDK: {}", toolchain.ndk_root.display());
        println!("Android toolchain: {}", toolchain.bin_dir.display());
        if cfg!(windows) && posix_shell().is_none() {
            bail!(
                "required POSIX shell was not found; install Git for Windows so FFmpeg configure can run"
            );
        }
        if cfg!(windows) {
            let _ = windows_msvc_environment(windows_host_target())?;
        }
        if gnu_make().is_none() {
            bail!(
                "required GNU make was not found; install make or use an NDK distribution that includes prebuilt make"
            );
        }
        if dav1d_requires_nasm(options.target)
            && (!ffmpeg_build_marker_is_current(layout, options)
                || !dav1d_build_marker_is_current(layout, options))
            && which("nasm").is_none()
        {
            bail!("required build tool `nasm` was not found for Android x86_64 FFmpeg/dav1d");
        }
        if cmake_tool().is_none() {
            bail!("required CMake was not found; install CMake or Android SDK CMake");
        }
        if python_tool().is_none() && (which("meson").is_none() || which("ninja").is_none()) {
            bail!(
                "required Python with venv support was not found; Android dav1d needs Meson/Ninja and xtask cannot provision them"
            );
        }
        let _ = ensure_meson_tools(layout)?;
        let _ = ensure_pkg_config_shim(layout)?;
        let _ = host_c_compiler()?;
        let _ = host_cxx_compiler()?;
        return Ok(());
    }

    if options.target.is_ohos() {
        let toolchain = ohos_toolchain(options.target)?
            .context("an explicit OpenHarmony target requires the native SDK toolchain")?;
        println!(
            "OpenHarmony native SDK: {}",
            toolchain.native_root.display()
        );
        println!("OpenHarmony toolchain: {}", toolchain.bin_dir.display());
        if gnu_make().is_none() {
            bail!("required GNU make was not found for the OpenHarmony FFmpeg build");
        }
        if cmake_tool().is_none() {
            bail!("required CMake was not found for the OpenHarmony dependency build");
        }
        if python_tool().is_none() {
            bail!("required Python with venv support was not found for OpenHarmony dav1d");
        }
        let _ = ensure_pkg_config_shim(layout)?;
        return Ok(());
    }

    if dav1d_requires_nasm(options.target)
        && (!ffmpeg_build_marker_is_current(layout, options)
            || !dav1d_build_marker_is_current(layout, options))
        && which("nasm").is_none()
    {
        bail!("required build tool `nasm` was not found for x86_64 Apple dav1d assembly");
    }

    let compiler = "clang";
    for tool in ["make", compiler, "cmake", "pkg-config"] {
        if which(tool).is_none() {
            bail!("required build tool `{tool}` was not found in PATH");
        }
    }
    if python_tool().is_none() {
        bail!("required Python with venv support was not found in PATH");
    }
    Ok(())
}

fn build_text_dependencies(layout: &WorkspaceLayout, options: DepsOptions) -> Result<()> {
    build_freetype(layout, options)?;
    build_harfbuzz(layout, options)?;
    build_fribidi(layout, options)?;
    build_libass(layout, options)?;
    Ok(())
}

fn build_zlib(layout: &WorkspaceLayout, options: DepsOptions) -> Result<()> {
    if marker_has_version(
        &layout.zlib_build_marker,
        "zlib",
        ZLIB_VERSION,
        options.target,
    ) && !options.force
        && (native_static_lib_exists(&layout.zlib_prefix, "z")
            || native_static_lib_exists(&layout.zlib_prefix, "zlib")
            || native_static_lib_exists(&layout.zlib_prefix, "zs"))
    {
        println!(
            "reuse zlib build marker {}",
            layout.zlib_build_marker.display()
        );
        ensure_windows_link_aliases(
            options.target,
            &layout.zlib_prefix,
            &[("zs.lib", "z.lib"), ("z.lib", "zlib.lib")],
        )?;
        ensure_windows_zlib_static_alias(options.target, &layout.zlib_prefix)?;
        ensure_windows_zlib_header_compat(options.target, &layout.zlib_prefix)?;
        return Ok(());
    }
    reset_build_and_prefix(&layout.zlib_build_dir, &layout.zlib_prefix)?;
    fs::create_dir_all(&layout.zlib_build_dir)
        .with_context(|| format!("create {}", layout.zlib_build_dir.display()))?;
    fs::create_dir_all(&layout.zlib_prefix)
        .with_context(|| format!("create {}", layout.zlib_prefix.display()))?;

    println!("configure zlib");
    let mut configure = cmake_command(options.target)?;
    configure
        .arg("-S")
        .arg(&layout.zlib_source_dir)
        .arg("-B")
        .arg(&layout.zlib_build_dir)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg("-DBUILD_SHARED_LIBS=OFF")
        .arg("-DZLIB_BUILD_SHARED=OFF")
        .arg("-DZLIB_BUILD_STATIC=ON")
        .arg("-DZLIB_INSTALL=ON")
        .arg("-DZLIB_BUILD_TESTING=OFF")
        .arg(format!(
            "-DCMAKE_INSTALL_PREFIX={}",
            layout.zlib_prefix.display()
        ));
    apply_cmake_target(&mut configure, options.target)?;
    apply_cmake_crt(&mut configure, options);
    run(&mut configure)?;
    cmake_build_install(&layout.zlib_build_dir, options.jobs, options.target)?;
    ensure_windows_link_aliases(
        options.target,
        &layout.zlib_prefix,
        &[("zs.lib", "z.lib"), ("z.lib", "zlib.lib")],
    )?;
    ensure_windows_zlib_static_alias(options.target, &layout.zlib_prefix)?;
    ensure_windows_zlib_header_compat(options.target, &layout.zlib_prefix)?;
    write_marker(
        &layout.zlib_build_marker,
        "zlib",
        ZLIB_VERSION,
        &layout.zlib_prefix,
        options.target,
    )
}

fn build_dav1d(layout: &WorkspaceLayout, options: DepsOptions) -> Result<()> {
    if dav1d_build_marker_is_current(layout, options) && !options.force {
        println!(
            "reuse dav1d build marker {}",
            layout.dav1d_build_marker.display()
        );
        return Ok(());
    }

    // The marker includes the target API and assembly policy. Never mix an
    // older cross configuration with the current build.
    for path in [&layout.dav1d_build_dir, &layout.dav1d_prefix] {
        if path.exists() {
            fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    fs::create_dir_all(&layout.dav1d_prefix)
        .with_context(|| format!("create {}", layout.dav1d_prefix.display()))?;

    let meson = ensure_meson_tools(layout)?;
    let asm_enabled = dav1d_asm_enabled(options.target);
    println!(
        "configure dav1d for {} (asm={asm_enabled})",
        options.target.triple().unwrap_or("host")
    );
    let mut setup = meson_command(&meson);
    setup
        .arg("setup")
        .arg(&layout.dav1d_build_dir)
        .arg(&layout.dav1d_source_dir)
        .arg(format!("--prefix={}", layout.dav1d_prefix.display()))
        .arg("--libdir=lib")
        .arg("--default-library=static")
        .arg("--buildtype=release")
        .arg("-Dbitdepths=8,16")
        .arg(format!("-Denable_asm={asm_enabled}"))
        .arg("-Denable_tools=false")
        .arg("-Denable_examples=false")
        .arg("-Denable_tests=false")
        .arg("-Denable_docs=false")
        .arg("-Dlogging=true");
    apply_meson_target(&mut setup, layout, options.target, "dav1d")?;
    apply_meson_crt(&mut setup, options);
    apply_windows_target_env(&mut setup, options.target)?;
    if matches!(options.target, NativeTarget::Aarch64WindowsMsvc) {
        let gaspp = ensure_gas_preprocessor(layout)?;
        prepend_path_to_command(
            &mut setup,
            gaspp.parent().context("gas-preprocessor directory")?,
        );
    }
    run(&mut setup)?;
    meson_compile_install(
        &meson,
        &layout.dav1d_build_dir,
        options.jobs,
        options.target,
    )?;

    ensure_windows_link_aliases(
        options.target,
        &layout.dav1d_prefix,
        &[("libdav1d.a", "dav1d.lib"), ("libdav1d.lib", "dav1d.lib")],
    )?;
    let archive = layout.dav1d_prefix.join(if options.target.is_windows() {
        "lib/dav1d.lib"
    } else {
        "lib/libdav1d.a"
    });
    let pkg_config = layout.dav1d_prefix.join("lib/pkgconfig/dav1d.pc");
    for path in [&archive, &pkg_config] {
        if !path.is_file() {
            bail!("dav1d install did not produce {}", path.display());
        }
    }
    fs::write(
        &layout.dav1d_build_marker,
        format!(
            "dav1d={DAV1D_VERSION}\ntarget={}\nandroid_api={}\ndeployment_target={}\nasm={asm_enabled}\nprefix={}\n",
            options.target.triple().unwrap_or("host"),
            if options.target.is_android() {
                android_api_level()?.to_string()
            } else {
                "n/a".to_string()
            },
            native_deployment_target(options.target),
            layout.dav1d_prefix.display(),
        ),
    )
    .with_context(|| format!("write {}", layout.dav1d_build_marker.display()))
}

fn dav1d_asm_enabled(target: NativeTarget) -> bool {
    // Match the FFmpeg policy for 32-bit Android x86: omit assembly that can
    // introduce text relocations into the final shared library.
    !matches!(target, NativeTarget::I686Android)
}

fn dav1d_requires_nasm(target: NativeTarget) -> bool {
    matches!(
        target,
        NativeTarget::X86_64Macos
            | NativeTarget::X86_64IosSimulator
            | NativeTarget::X86_64TvosSimulator
            | NativeTarget::X86_64Android
            | NativeTarget::X86_64WindowsMsvc
    ) || (matches!(target, NativeTarget::Host) && cfg!(target_arch = "x86_64"))
}

fn ffmpeg_requires_nasm(target: NativeTarget) -> bool {
    matches!(
        target,
        NativeTarget::X86_64Macos
            | NativeTarget::X86_64IosSimulator
            | NativeTarget::X86_64TvosSimulator
            | NativeTarget::X86_64WindowsMsvc
            | NativeTarget::X86_64Android
    ) || (matches!(target, NativeTarget::Host) && cfg!(target_arch = "x86_64"))
}

fn build_freetype(layout: &WorkspaceLayout, options: DepsOptions) -> Result<()> {
    if marker_has_version(
        &layout.freetype_build_marker,
        "freetype",
        FREETYPE_VERSION,
        options.target,
    ) && !options.force
    {
        println!(
            "reuse FreeType build marker {}",
            layout.freetype_build_marker.display()
        );
        return Ok(());
    }
    reset_build_and_prefix(&layout.freetype_build_dir, &layout.freetype_prefix)?;
    fs::create_dir_all(&layout.freetype_build_dir)
        .with_context(|| format!("create {}", layout.freetype_build_dir.display()))?;
    fs::create_dir_all(&layout.freetype_prefix)
        .with_context(|| format!("create {}", layout.freetype_prefix.display()))?;

    println!("configure FreeType");
    let mut configure = cmake_command(options.target)?;
    configure
        .arg("-S")
        .arg(&layout.freetype_source_dir)
        .arg("-B")
        .arg(&layout.freetype_build_dir)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg("-DBUILD_SHARED_LIBS=OFF")
        .arg(format!(
            "-DCMAKE_INSTALL_PREFIX={}",
            layout.freetype_prefix.display()
        ))
        .arg("-DFT_DISABLE_ZLIB=TRUE")
        .arg("-DFT_DISABLE_BZIP2=TRUE")
        .arg("-DFT_DISABLE_PNG=TRUE")
        .arg("-DFT_DISABLE_HARFBUZZ=TRUE")
        .arg("-DFT_DISABLE_BROTLI=TRUE");
    apply_cmake_target(&mut configure, options.target)?;
    apply_cmake_crt(&mut configure, options);
    run(&mut configure)?;
    cmake_build_install(&layout.freetype_build_dir, options.jobs, options.target)?;
    write_marker(
        &layout.freetype_build_marker,
        "freetype",
        FREETYPE_VERSION,
        &layout.freetype_prefix,
        options.target,
    )
}

fn build_harfbuzz(layout: &WorkspaceLayout, options: DepsOptions) -> Result<()> {
    if marker_has_version(
        &layout.harfbuzz_build_marker,
        "harfbuzz",
        HARFBUZZ_VERSION,
        options.target,
    ) && !options.force
    {
        println!(
            "reuse HarfBuzz build marker {}",
            layout.harfbuzz_build_marker.display()
        );
        return Ok(());
    }
    reset_build_and_prefix(&layout.harfbuzz_build_dir, &layout.harfbuzz_prefix)?;
    fs::create_dir_all(&layout.harfbuzz_build_dir)
        .with_context(|| format!("create {}", layout.harfbuzz_build_dir.display()))?;
    fs::create_dir_all(&layout.harfbuzz_prefix)
        .with_context(|| format!("create {}", layout.harfbuzz_prefix.display()))?;

    println!("configure HarfBuzz");
    let mut configure = cmake_command(options.target)?;
    configure
        .arg("-S")
        .arg(&layout.harfbuzz_source_dir)
        .arg("-B")
        .arg(&layout.harfbuzz_build_dir)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg("-DBUILD_SHARED_LIBS=OFF")
        .arg(format!(
            "-DCMAKE_INSTALL_PREFIX={}",
            layout.harfbuzz_prefix.display()
        ))
        .arg("-DHB_HAVE_FREETYPE=OFF")
        .arg("-DHB_HAVE_GLIB=OFF")
        .arg("-DHB_HAVE_GOBJECT=OFF")
        .arg("-DHB_HAVE_ICU=OFF")
        .arg("-DHB_HAVE_CAIRO=OFF")
        .arg("-DHB_BUILD_UTILS=OFF")
        .arg("-DHB_BUILD_SUBSET=OFF");
    if options.target.is_windows() {
        configure
            .arg("-DHB_HAVE_CORETEXT=OFF")
            .arg("-DHB_HAVE_DIRECTWRITE=ON");
    } else if options.target.is_apple() {
        configure
            .arg("-DHB_HAVE_CORETEXT=ON")
            .arg("-DHB_HAVE_DIRECTWRITE=OFF");
    } else {
        configure
            .arg("-DHB_HAVE_CORETEXT=OFF")
            .arg("-DHB_HAVE_DIRECTWRITE=OFF");
    }
    apply_cmake_target(&mut configure, options.target)?;
    apply_cmake_crt(&mut configure, options);
    run(&mut configure)?;
    cmake_build_install(&layout.harfbuzz_build_dir, options.jobs, options.target)?;
    write_marker(
        &layout.harfbuzz_build_marker,
        "harfbuzz",
        HARFBUZZ_VERSION,
        &layout.harfbuzz_prefix,
        options.target,
    )
}

fn build_fribidi(layout: &WorkspaceLayout, options: DepsOptions) -> Result<()> {
    if marker_has_version(
        &layout.fribidi_build_marker,
        "fribidi",
        FRIBIDI_VERSION,
        options.target,
    ) && !options.force
    {
        println!(
            "reuse FriBidi build marker {}",
            layout.fribidi_build_marker.display()
        );
        ensure_windows_link_aliases(
            options.target,
            &layout.fribidi_prefix,
            &[("libfribidi.a", "fribidi.lib")],
        )?;
        return Ok(());
    }
    patch_fribidi_meson_native_compiler(layout)?;
    let meson = ensure_meson_tools(layout)?;
    reset_build_and_prefix(&layout.fribidi_build_dir, &layout.fribidi_prefix)?;
    fs::create_dir_all(&layout.fribidi_prefix)
        .with_context(|| format!("create {}", layout.fribidi_prefix.display()))?;
    println!("configure FriBidi");
    let mut setup = meson_command(&meson);
    setup
        .arg("setup")
        .arg(&layout.fribidi_build_dir)
        .arg(&layout.fribidi_source_dir)
        .arg(format!("--prefix={}", layout.fribidi_prefix.display()))
        .arg("--default-library=static")
        .arg("--buildtype=release")
        .arg("-Ddocs=false")
        .arg("-Dtests=false");
    apply_meson_target(&mut setup, layout, options.target, "fribidi")?;
    apply_meson_crt(&mut setup, options);
    apply_windows_target_env(&mut setup, options.target)?;
    run(&mut setup)?;
    meson_compile_install(
        &meson,
        &layout.fribidi_build_dir,
        options.jobs,
        options.target,
    )?;
    ensure_windows_link_aliases(
        options.target,
        &layout.fribidi_prefix,
        &[("libfribidi.a", "fribidi.lib")],
    )?;
    write_marker(
        &layout.fribidi_build_marker,
        "fribidi",
        FRIBIDI_VERSION,
        &layout.fribidi_prefix,
        options.target,
    )
}

fn patch_fribidi_meson_native_compiler(layout: &WorkspaceLayout) -> Result<()> {
    let path = layout.fribidi_source_dir.join("gen.tab/meson.build");
    let contents = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let original = "native_cc = meson.get_compiler('c')";
    let replacement = "native_cc = meson.get_compiler('c', native: true)";
    if contents.contains(replacement) {
        return Ok(());
    }
    if !contents.contains(original) {
        bail!(
            "FriBidi native compiler declaration was not found in {}; update the Erika patch for this FriBidi version",
            path.display()
        );
    }
    fs::write(&path, contents.replacen(original, replacement, 1))
        .with_context(|| format!("patch {}", path.display()))?;
    println!("patched FriBidi Meson generators to probe headers with the build-machine compiler");
    Ok(())
}

fn build_libass(layout: &WorkspaceLayout, options: DepsOptions) -> Result<()> {
    if marker_has_version(
        &layout.libass_build_marker,
        "libass",
        LIBASS_BUILD_VERSION,
        options.target,
    ) && !options.force
    {
        println!(
            "reuse libass build marker {}",
            layout.libass_build_marker.display()
        );
        ensure_windows_link_aliases(
            options.target,
            &layout.libass_prefix,
            &[("libass.a", "ass.lib")],
        )?;
        return Ok(());
    }
    let meson = ensure_meson_tools(layout)?;
    reset_build_and_prefix(&layout.libass_build_dir, &layout.libass_prefix)?;
    fs::create_dir_all(&layout.libass_prefix)
        .with_context(|| format!("create {}", layout.libass_prefix.display()))?;

    let pkg_config_path = pkg_config_path([
        &layout.freetype_prefix,
        &layout.harfbuzz_prefix,
        &layout.fribidi_prefix,
    ]);
    let pkg_config = ensure_pkg_config_shim(layout)?;
    println!("configure libass");
    let mut setup = meson_command(&meson);
    setup
        .arg("setup")
        .arg(&layout.libass_build_dir)
        .arg(&layout.libass_source_dir)
        .arg(format!("--prefix={}", layout.libass_prefix.display()))
        .arg("--default-library=static")
        .arg("--buildtype=release")
        .arg("-Dtest=disabled")
        .arg("-Dprofile=disabled")
        .arg("-Dfontconfig=disabled")
        .arg("-Dasm=disabled")
        .arg("-Dlibunibreak=disabled")
        .env("PKG_CONFIG_PATH", &pkg_config_path)
        .env("PKG_CONFIG", &pkg_config)
        .env("ERIKA_PKG_CONFIG_RELATIVE_BASE", &layout.libass_build_dir);
    if options.target.is_windows() {
        setup
            .arg("-Dcoretext=disabled")
            .arg("-Ddirectwrite=enabled");
    } else if options.target.is_apple() {
        setup
            .arg("-Dcoretext=enabled")
            .arg("-Ddirectwrite=disabled");
    } else {
        setup
            .arg("-Dcoretext=disabled")
            .arg("-Ddirectwrite=disabled")
            .arg("-Drequire-system-font-provider=false");
    }
    apply_meson_target(&mut setup, layout, options.target, "libass")?;
    apply_meson_crt(&mut setup, options);
    apply_windows_target_env(&mut setup, options.target)?;
    run(&mut setup)?;

    let mut compile = meson_command(&meson);
    compile
        .arg("compile")
        .arg("-C")
        .arg(&layout.libass_build_dir)
        .env("PKG_CONFIG_PATH", &pkg_config_path)
        .env("PKG_CONFIG", &pkg_config)
        .env("ERIKA_PKG_CONFIG_RELATIVE_BASE", &layout.libass_build_dir);
    if let Some(jobs) = options.jobs {
        compile.arg(format!("-j{jobs}"));
    }
    apply_windows_target_env(&mut compile, options.target)?;
    apply_android_host_env(&mut compile, options.target)?;
    run(&mut compile)?;
    let mut install = meson_command(&meson);
    install
        .arg("install")
        .arg("-C")
        .arg(&layout.libass_build_dir)
        .env("PKG_CONFIG_PATH", &pkg_config_path)
        .env("PKG_CONFIG", &pkg_config)
        .env("ERIKA_PKG_CONFIG_RELATIVE_BASE", &layout.libass_build_dir);
    apply_windows_target_env(&mut install, options.target)?;
    apply_android_host_env(&mut install, options.target)?;
    run(&mut install)?;
    ensure_windows_link_aliases(
        options.target,
        &layout.libass_prefix,
        &[("libass.a", "ass.lib")],
    )?;

    write_marker(
        &layout.libass_build_marker,
        "libass",
        LIBASS_BUILD_VERSION,
        &layout.libass_prefix,
        options.target,
    )
}

fn cmake_build_install(
    build_dir: &std::path::Path,
    jobs: Option<usize>,
    target: NativeTarget,
) -> Result<()> {
    let mut build = cmake_command(target)?;
    build
        .arg("--build")
        .arg(build_dir)
        .arg("--config")
        .arg("Release");
    if let Some(jobs) = jobs {
        build.arg("--parallel").arg(jobs.to_string());
    }
    apply_windows_target_env(&mut build, target)?;
    run(&mut build)?;
    let mut install = cmake_command(target)?;
    install
        .arg("--install")
        .arg(build_dir)
        .arg("--config")
        .arg("Release");
    apply_windows_target_env(&mut install, target)?;
    run(&mut install)
}

#[derive(Debug, Clone)]
struct MesonTools {
    meson: PathBuf,
    bin_dir: PathBuf,
}

fn ensure_meson_tools(layout: &WorkspaceLayout) -> Result<MesonTools> {
    if let Some(meson) = which("meson") {
        if which("ninja").is_some() {
            let bin_dir = meson.parent().unwrap_or(Path::new("")).to_path_buf();
            return Ok(MesonTools { meson, bin_dir });
        }
    }

    let venv = layout.python_tools_dir.join("venv");
    let bin_dir = venv_bin_dir(&venv);
    let meson = executable_in_dir(&bin_dir, "meson");
    let ninja = executable_in_dir(&bin_dir, "ninja");
    if meson.exists() && ninja.exists() {
        return Ok(MesonTools { meson, bin_dir });
    }

    fs::create_dir_all(&layout.python_tools_dir)
        .with_context(|| format!("create {}", layout.python_tools_dir.display()))?;
    let python = python_tool().context("required Python was not found in PATH")?;
    println!("bootstrap local meson/ninja tools");
    run(Command::new(python).arg("-m").arg("venv").arg(&venv))?;
    run(Command::new(executable_in_dir(&bin_dir, "python"))
        .arg("-m")
        .arg("pip")
        .arg("install")
        .arg("--upgrade")
        .arg("pip")
        .arg("meson==1.8.5")
        .arg("ninja==1.13.0"))?;
    Ok(MesonTools { meson, bin_dir })
}

fn meson_command(meson: &MesonTools) -> Command {
    let mut command = Command::new(&meson.meson);
    prepend_path(&mut command, &meson.bin_dir);
    // MSVC prints in the system code page. Python's UTF-8 mode would make
    // Meson fail to read `cl`'s output on, for example, Chinese Windows.
    if cfg!(windows) {
        command.env_remove("PYTHONUTF8");
    }
    command
}

fn cmake_command(target: NativeTarget) -> Result<Command> {
    let _ = target;
    let cmake = cmake_tool().context("required CMake was not found")?;
    Ok(Command::new(cmake))
}

fn apply_cmake_target(command: &mut Command, target: NativeTarget) -> Result<()> {
    apply_cmake_apple_target(command, target)?;
    if let Some(config) = android_toolchain(target)? {
        command
            .arg(format!(
                "-DCMAKE_TOOLCHAIN_FILE={}",
                config.cmake_toolchain_file.display()
            ))
            .arg(format!("-DANDROID_ABI={}", config.abi))
            .arg(format!("-DANDROID_PLATFORM=android-{}", config.api_level))
            .arg("-DANDROID_STL=c++_shared")
            .arg("-DCMAKE_POSITION_INDEPENDENT_CODE=ON");
        if let Some(ninja) = ninja_tool() {
            command
                .arg("-G")
                .arg("Ninja")
                .arg(format!("-DCMAKE_MAKE_PROGRAM={}", ninja.display()));
        }
    }
    if let Some(config) = ohos_toolchain(target)? {
        command
            .arg(format!(
                "-DCMAKE_TOOLCHAIN_FILE={}",
                config.cmake_toolchain_file.display()
            ))
            .arg(format!("-DOHOS_ARCH={}", config.abi))
            .arg("-DOHOS_STL=c++_shared")
            .arg(format!(
                "-DOHOS_COMPATIBLE_SDK_VERSION={}",
                config.compatible_sdk_version
            ))
            .arg("-DCMAKE_POSITION_INDEPENDENT_CODE=ON");
        if let Some(ninja) = ninja_tool() {
            command
                .arg("-G")
                .arg("Ninja")
                .arg(format!("-DCMAKE_MAKE_PROGRAM={}", ninja.display()));
        }
    }
    if target.is_windows() {
        if let Some(ninja) = ninja_tool() {
            command
                .arg("-G")
                .arg("Ninja")
                .arg(format!("-DCMAKE_MAKE_PROGRAM={}", ninja.display()));
        }
        apply_windows_target_env(command, target)?;
    }
    Ok(())
}

fn apply_cmake_apple_target(command: &mut Command, target: NativeTarget) -> Result<()> {
    let Some(config) = apple_toolchain(target)? else {
        return Ok(());
    };
    command
        .arg(format!("-DCMAKE_C_COMPILER={}", config.clang.display()))
        .arg(format!("-DCMAKE_CXX_COMPILER={}", config.clangxx.display()))
        .arg(format!("-DCMAKE_AR={}", config.ar.display()))
        .arg(format!("-DCMAKE_RANLIB={}", config.ranlib.display()))
        .arg(format!("-DCMAKE_OSX_SYSROOT={}", config.sdk_root.display()))
        .arg(format!("-DCMAKE_OSX_ARCHITECTURES={}", config.arch))
        .arg(format!("-DCMAKE_SYSTEM_PROCESSOR={}", config.arch))
        .arg(format!(
            "-DCMAKE_OSX_DEPLOYMENT_TARGET={}",
            config.deployment_target
        ));
    if target.is_ios() {
        command.arg("-DCMAKE_SYSTEM_NAME=iOS");
    } else if target.is_tvos() {
        command.arg("-DCMAKE_SYSTEM_NAME=tvOS");
    }
    apply_apple_target_env(command, target)
}

/// The static C runtime for a Windows CMake build, which otherwise uses the
/// DLL. The policy lets projects that require an older CMake honour it.
fn apply_cmake_crt(command: &mut Command, options: DepsOptions) {
    if options.target.is_windows() && options.crt == WindowsCrt::Static {
        command
            .arg("-DCMAKE_POLICY_DEFAULT_CMP0091=NEW")
            .arg("-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded");
    }
}

/// The static C runtime for a Windows Meson build, which otherwise uses the
/// DLL.
fn apply_meson_crt(command: &mut Command, options: DepsOptions) {
    if options.target.is_windows() && options.crt == WindowsCrt::Static {
        command.arg("-Db_vscrt=mt");
    }
}

fn apply_meson_target(
    command: &mut Command,
    layout: &WorkspaceLayout,
    target: NativeTarget,
    name: &str,
) -> Result<()> {
    let Some(cross_file) = meson_cross_file(layout, target, name)? else {
        return Ok(());
    };
    command.arg("--cross-file").arg(cross_file);
    // Cross builds (e.g. iOS) compile native generator tools such as FriBidi's
    // gen.tab on the build machine. Provide an explicit build-machine compiler
    // pinned to the macOS SDK so the iOS SDKROOT we export below does not make
    // those native tools target iOS and fail to run.
    let native_file = meson_native_file(layout, target, name)?;
    command.arg("--native-file").arg(native_file);
    apply_apple_target_env(command, target)?;
    apply_android_host_env(command, target)?;
    apply_windows_target_env(command, target)
}

fn meson_native_file(
    layout: &WorkspaceLayout,
    target: NativeTarget,
    name: &str,
) -> Result<PathBuf> {
    let path = layout.build_dir.join(format!("{name}-meson-native.ini"));
    let content = if target.is_apple() {
        let sdk_root = xcrun("macosx", &["--show-sdk-path"])?;
        let clang = xcrun("macosx", &["-f", "clang"])?;
        let clangxx = xcrun("macosx", &["-f", "clang++"])?;
        // SDKROOT belongs to the target build. Pin native generators back to macOS.
        let arch = match env::consts::ARCH {
            "aarch64" => "arm64",
            other => other,
        };
        let host_target = format!("{arch}-apple-macos");
        format!(
            "[binaries]\nc = [{}, '-target', {}, '-isysroot', {}]\ncpp = [{}, '-target', {}, '-isysroot', {}]\n",
            meson_string(&clang),
            meson_string(&host_target),
            meson_string(&sdk_root),
            meson_string(&clangxx),
            meson_string(&host_target),
            meson_string(&sdk_root),
        )
    } else {
        format!(
            "[binaries]\nc = {}\ncpp = {}\n",
            meson_string(&host_c_compiler()?.display().to_string()),
            meson_string(&host_cxx_compiler()?.display().to_string()),
        )
    };
    fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn meson_cross_file(
    layout: &WorkspaceLayout,
    target: NativeTarget,
    name: &str,
) -> Result<Option<PathBuf>> {
    let path = layout.build_dir.join(format!("{name}-meson-cross.ini"));
    let content = if let Some(config) = apple_toolchain(target)? {
        let pkg_config = which("pkg-config").unwrap_or_else(|| PathBuf::from("pkg-config"));
        let arch_flags = apple_arch_flags(&config);
        format!(
            "[binaries]\nc = {}\ncpp = {}\nar = {}\nstrip = {}\npkg-config = {}\n\n[built-in options]\nc_args = {}\ncpp_args = {}\nc_link_args = {}\ncpp_link_args = {}\n\n[host_machine]\nsystem = 'darwin'\ncpu_family = {}\ncpu = {}\nendian = 'little'\n",
            meson_string(&config.clang.display().to_string()),
            meson_string(&config.clangxx.display().to_string()),
            meson_string(&config.ar.display().to_string()),
            meson_string(&config.strip.display().to_string()),
            meson_string(&pkg_config.display().to_string()),
            meson_array(&arch_flags),
            meson_array(&arch_flags),
            meson_array(&arch_flags),
            meson_array(&arch_flags),
            meson_string(
                target
                    .meson_cpu_family()
                    .context("explicit Apple target must have a Meson CPU family")?,
            ),
            meson_string(
                target
                    .meson_cpu()
                    .context("explicit Apple target must have a Meson CPU")?,
            ),
        )
    } else if let Some(config) = android_toolchain(target)? {
        let pkg_config = ensure_pkg_config_shim(layout)?;
        let pic_flags = vec!["-fPIC".to_string()];
        format!(
            "[binaries]\nc = {}\ncpp = {}\nar = {}\nstrip = {}\npkg-config = {}\n\n[built-in options]\nc_args = {}\ncpp_args = {}\nc_link_args = {}\ncpp_link_args = {}\n\n[properties]\nneeds_exe_wrapper = true\n\n[host_machine]\nsystem = 'android'\ncpu_family = {}\ncpu = {}\nendian = 'little'\n",
            meson_string(&config.clang.display().to_string()),
            meson_string(&config.clangxx.display().to_string()),
            meson_string(&config.ar.display().to_string()),
            meson_string(&config.strip.display().to_string()),
            meson_string(&pkg_config.display().to_string()),
            meson_array(&pic_flags),
            meson_array(&pic_flags),
            meson_array(&pic_flags),
            meson_array(&pic_flags),
            meson_string(
                target
                    .meson_cpu_family()
                    .context("explicit Android target must have a Meson CPU family")?,
            ),
            meson_string(
                target
                    .meson_cpu()
                    .context("explicit Android target must have a Meson CPU")?,
            ),
        )
    } else if let Some(config) = ohos_toolchain(target)? {
        let pkg_config = ensure_pkg_config_shim(layout)?;
        ohos_meson_cross_contents(&config, &pkg_config)
    } else if matches!(target, NativeTarget::Aarch64WindowsMsvc) {
        let tools = windows_msvc_environment(target)?;
        let compiler = msvc_tool_path(tools, "cl.exe")?;
        let archiver = msvc_tool_path(tools, "lib.exe")?;
        format!(
            "[binaries]\nc = {}\ncpp = {}\nar = {}\n\n[properties]\nneeds_exe_wrapper = true\n\n[host_machine]\nsystem = 'windows'\ncpu_family = {}\ncpu = {}\nendian = 'little'\n",
            meson_string(&compiler.display().to_string()),
            meson_string(&compiler.display().to_string()),
            meson_string(&archiver.display().to_string()),
            meson_string(
                target
                    .meson_cpu_family()
                    .context("Windows ARM64 target must have a Meson CPU family")?,
            ),
            meson_string(
                target
                    .meson_cpu()
                    .context("Windows ARM64 target must have a Meson CPU")?,
            ),
        )
    } else {
        return Ok(None);
    };
    fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(Some(path))
}

fn ohos_meson_cross_contents(config: &OhosToolchain, pkg_config: &Path) -> String {
    let flags = vec![
        format!("--target={}", config.triple),
        format!("--sysroot={}", config.sysroot.display()),
        "-fPIC".to_string(),
    ];
    format!(
        "[binaries]\nc = {}\ncpp = {}\nar = {}\nstrip = {}\npkg-config = {}\n\n[built-in options]\nc_args = {}\ncpp_args = {}\nc_link_args = {}\ncpp_link_args = {}\n\n[properties]\nneeds_exe_wrapper = true\n\n[host_machine]\nsystem = 'linux'\ncpu_family = {}\ncpu = {}\nendian = 'little'\n",
        meson_string(&config.clang.display().to_string()),
        meson_string(&config.clangxx.display().to_string()),
        meson_string(&config.ar.display().to_string()),
        meson_string(&config.strip.display().to_string()),
        meson_string(&pkg_config.display().to_string()),
        meson_array(&flags),
        meson_array(&flags),
        meson_array(&flags),
        meson_array(&flags),
        meson_string(config.arch),
        meson_string(config.arch),
    )
}

fn apply_apple_target_env(command: &mut Command, target: NativeTarget) -> Result<()> {
    let Some(config) = apple_toolchain(target)? else {
        return Ok(());
    };
    clear_apple_deployment_target_env(command);
    command.env("SDKROOT", &config.sdk_root);
    if target.is_ios() {
        command.env("IPHONEOS_DEPLOYMENT_TARGET", &config.deployment_target);
    } else if target.is_tvos() {
        command.env("TVOS_DEPLOYMENT_TARGET", &config.deployment_target);
    } else {
        command.env("MACOSX_DEPLOYMENT_TARGET", &config.deployment_target);
    }
    Ok(())
}

fn clear_apple_deployment_target_env(command: &mut Command) {
    for name in [
        "MACOSX_DEPLOYMENT_TARGET",
        "IPHONEOS_DEPLOYMENT_TARGET",
        "TVOS_DEPLOYMENT_TARGET",
    ] {
        command.env_remove(name);
    }
}

fn apple_arch_flags(config: &AppleToolchain) -> Vec<String> {
    vec![
        "-arch".to_string(),
        config.arch.to_string(),
        "-isysroot".to_string(),
        config.sdk_root.display().to_string(),
        format!("{}={}", config.deployment_flag, config.deployment_target),
    ]
}

fn meson_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| meson_string(value))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn meson_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn prepend_path(command: &mut Command, dir: &Path) {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&path));
    }
    command.env(
        "PATH",
        env::join_paths(paths).expect("PATH entries are valid"),
    );
}

fn meson_compile_install(
    meson: &MesonTools,
    build_dir: &std::path::Path,
    jobs: Option<usize>,
    target: NativeTarget,
) -> Result<()> {
    let mut compile = meson_command(meson);
    compile.arg("compile").arg("-C").arg(build_dir);
    if let Some(jobs) = jobs {
        compile.arg(format!("-j{jobs}"));
    }
    apply_windows_target_env(&mut compile, target)?;
    apply_android_host_env(&mut compile, target)?;
    run(&mut compile)?;
    let mut install = meson_command(meson);
    install.arg("install").arg("-C").arg(build_dir);
    apply_windows_target_env(&mut install, target)?;
    apply_android_host_env(&mut install, target)?;
    run(&mut install)
}

// Only reached once the build marker was rejected, so the build directory holds
// a CMake or Meson configuration for a version we no longer pin. Never reuse it:
// both generators refuse to reconfigure against a different source directory,
// and the prefix would keep headers and archives from the old version.
fn reset_build_and_prefix(build_dir: &std::path::Path, prefix: &std::path::Path) -> Result<()> {
    for path in [prefix, build_dir] {
        if path.exists() {
            fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn write_marker(
    path: &std::path::Path,
    name: &str,
    version: &str,
    prefix: &std::path::Path,
    target: NativeTarget,
) -> Result<()> {
    fs::write(
        path,
        format!(
            "{name}={version}\ndeployment_target={}\nprefix={}\n",
            native_deployment_target(target),
            prefix.display()
        ),
    )
    .with_context(|| format!("write {}", path.display()))
}

fn marker_has_version(
    path: &std::path::Path,
    name: &str,
    version: &str,
    target: NativeTarget,
) -> bool {
    fs::read_to_string(path)
        .map(|marker| {
            marker
                .lines()
                .any(|line| line == format!("{name}={version}"))
                && marker.contains(&format!(
                    "deployment_target={}\n",
                    native_deployment_target(target)
                ))
        })
        .unwrap_or(false)
}

fn native_deployment_target(target: NativeTarget) -> String {
    target
        .deployment_target()
        .map(|(deployment_target, _)| deployment_target)
        .unwrap_or_else(|| "n/a".to_string())
}

fn pkg_config_path<'a>(prefixes: impl IntoIterator<Item = &'a PathBuf>) -> String {
    env::join_paths(
        prefixes
            .into_iter()
            .map(|prefix| prefix.join("lib/pkgconfig")),
    )
    .expect("pkg-config path entries are valid")
    .to_string_lossy()
    .into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnifiedPatchHunk {
    old_start: usize,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnifiedPatchFile {
    path: PathBuf,
    hunks: Vec<UnifiedPatchHunk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchApplication {
    Applied,
    AlreadyApplied,
}

fn apply_ffmpeg_patches(layout: &WorkspaceLayout) -> Result<()> {
    validate_generated_ffmpeg_source_path(layout)?;
    let patchset = ffmpeg_patchset_id(&layout.root)?;
    let application = match apply_ffmpeg_patch_files(layout) {
        Ok(application) => application,
        Err(first_error) => {
            // third_party/src is generated from the pinned archive. If an older
            // liberika patch set or an interrupted partial application left it in
            // an unknown state, refresh it automatically instead of requiring
            // users to repair the vendored source tree by hand.
            println!(
                "FFmpeg source does not match patch set {patchset}; refresh the pinned source ({first_error:#})"
            );
            refresh_ffmpeg_source(layout)?;
            apply_ffmpeg_patch_files(layout).with_context(|| {
                format!(
                    "apply FFmpeg patch set {patchset} after refreshing {}",
                    layout.ffmpeg_source_dir.display()
                )
            })?
        }
    };
    fs::write(
        layout.ffmpeg_source_dir.join(".erika-patchset"),
        format!("{patchset}\n"),
    )
    .with_context(|| {
        format!(
            "write FFmpeg patch stamp in {}",
            layout.ffmpeg_source_dir.display()
        )
    })?;
    match application {
        PatchApplication::Applied => println!("applied FFmpeg patch set {patchset}"),
        PatchApplication::AlreadyApplied => println!("reuse FFmpeg patch set {patchset}"),
    }
    Ok(())
}

fn apply_ffmpeg_patch_files(layout: &WorkspaceLayout) -> Result<PatchApplication> {
    let mut application = PatchApplication::AlreadyApplied;
    for relative_path in FFMPEG_PATCHES {
        let patch_path = layout.root.join(relative_path);
        let patch = fs::read_to_string(&patch_path)
            .with_context(|| format!("read FFmpeg patch {}", patch_path.display()))?;
        if apply_unified_patch(&layout.ffmpeg_source_dir, &patch)
            .with_context(|| format!("apply FFmpeg patch {}", patch_path.display()))?
            == PatchApplication::Applied
        {
            application = PatchApplication::Applied;
        }
    }
    Ok(application)
}

fn apply_libass_patches(layout: &WorkspaceLayout) -> Result<()> {
    let mut application = PatchApplication::AlreadyApplied;
    for relative_path in LIBASS_PATCHES {
        let patch_path = layout.root.join(relative_path);
        let patch = fs::read_to_string(&patch_path)
            .with_context(|| format!("read libass patch {}", patch_path.display()))?;
        if apply_unified_patch(&layout.libass_source_dir, &patch)
            .with_context(|| format!("apply libass patch {}", patch_path.display()))?
            == PatchApplication::Applied
        {
            application = PatchApplication::Applied;
        }
    }
    fs::write(
        layout.libass_source_dir.join(".erika-patchset"),
        format!("{LIBASS_PATCHSET_VERSION}\n"),
    )
    .with_context(|| {
        format!(
            "write libass patch stamp in {}",
            layout.libass_source_dir.display()
        )
    })?;
    match application {
        PatchApplication::Applied => {
            println!("applied libass patch set {LIBASS_PATCHSET_VERSION}")
        }
        PatchApplication::AlreadyApplied => {
            println!("reuse libass patch set {LIBASS_PATCHSET_VERSION}")
        }
    }
    Ok(())
}

fn refresh_ffmpeg_source(layout: &WorkspaceLayout) -> Result<()> {
    validate_generated_ffmpeg_source_path(layout)?;
    let archive_path = layout.cache_dir.join(FFMPEG_ARCHIVE);
    if layout.ffmpeg_source_dir.exists() {
        fs::remove_dir_all(&layout.ffmpeg_source_dir)
            .with_context(|| format!("remove {}", layout.ffmpeg_source_dir.display()))?;
    }
    extract_archive(&archive_path, &layout.source_dir, FFMPEG_DIR)?;
    fs::write(
        layout.ffmpeg_source_dir.join(".erika-extracted"),
        format!(
            "archive={FFMPEG_ARCHIVE}\nsha256={}\n",
            archive_sha256(&archive_path)?
        ),
    )
    .with_context(|| format!("mark {} extracted", layout.ffmpeg_source_dir.display()))
}

fn validate_generated_ffmpeg_source_path(layout: &WorkspaceLayout) -> Result<()> {
    let expected_source = layout.source_dir.join(FFMPEG_DIR);
    if layout.ffmpeg_source_dir != expected_source {
        bail!(
            "refuse to modify unexpected FFmpeg source path {}",
            layout.ffmpeg_source_dir.display()
        );
    }
    let canonical_root = fs::canonicalize(&layout.root)
        .with_context(|| format!("resolve workspace root {}", layout.root.display()))?;
    let canonical_source_dir = fs::canonicalize(&layout.source_dir)
        .with_context(|| format!("resolve source root {}", layout.source_dir.display()))?;
    if !canonical_source_dir.starts_with(&canonical_root) {
        bail!(
            "refuse to modify FFmpeg source root outside the workspace: {}",
            canonical_source_dir.display()
        );
    }
    if layout.ffmpeg_source_dir.exists() {
        let canonical_ffmpeg = fs::canonicalize(&layout.ffmpeg_source_dir).with_context(|| {
            format!(
                "resolve FFmpeg source path {}",
                layout.ffmpeg_source_dir.display()
            )
        })?;
        let canonical_expected = canonical_source_dir.join(FFMPEG_DIR);
        if canonical_ffmpeg != canonical_expected {
            bail!(
                "refuse to modify redirected FFmpeg source path: {}",
                canonical_ffmpeg.display()
            );
        }
    }
    Ok(())
}

fn ffmpeg_patchset_id(root: &Path) -> Result<String> {
    let mut hash = 0xcbf29ce484222325_u64;
    fn update(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }

    update(&mut hash, FFMPEG_PATCHSET_VERSION.as_bytes());
    for relative_path in FFMPEG_PATCHES {
        update(&mut hash, &[0]);
        update(&mut hash, relative_path.as_bytes());
        update(&mut hash, &[0]);
        let patch_path = root.join(relative_path);
        let patch = fs::read(&patch_path)
            .with_context(|| format!("read FFmpeg patch {}", patch_path.display()))?;
        update(&mut hash, &patch);
    }
    Ok(format!("{FFMPEG_PATCHSET_VERSION}-{hash:016x}"))
}

fn apply_unified_patch(source_root: &Path, patch: &str) -> Result<PatchApplication> {
    let files = parse_unified_patch(patch)?;
    let mut pending_writes = Vec::new();
    for file in files {
        if file.path.is_absolute()
            || file
                .path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            bail!("unsafe path in unified patch: {}", file.path.display());
        }
        let path = source_root.join(&file.path);
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("read patch target {}", path.display()))?;
        let (updated, changed) = apply_patch_hunks(&contents, &file.hunks)
            .with_context(|| format!("patch {}", path.display()))?;
        if changed {
            pending_writes.push((path, updated));
        }
    }

    for (path, contents) in pending_writes.iter() {
        fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(if pending_writes.is_empty() {
        PatchApplication::AlreadyApplied
    } else {
        PatchApplication::Applied
    })
}

fn parse_unified_patch(patch: &str) -> Result<Vec<UnifiedPatchFile>> {
    let lines = patch.lines().collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some(path) = lines[index].strip_prefix("+++ ") else {
            index += 1;
            continue;
        };
        let path = path
            .split_whitespace()
            .next()
            .context("unified patch +++ line has no path")?;
        let path = path
            .strip_prefix("b/")
            .context("unified patch target must use a b/ path")?;
        let mut file = UnifiedPatchFile {
            path: PathBuf::from(path),
            hunks: Vec::new(),
        };
        index += 1;

        while index < lines.len() && !lines[index].starts_with("diff --git ") {
            if !lines[index].starts_with("@@ ") {
                index += 1;
                continue;
            }
            let (old_start, old_count, new_count) = parse_unified_hunk_header(lines[index])?;
            index += 1;
            let mut old_lines = Vec::with_capacity(old_count);
            let mut new_lines = Vec::with_capacity(new_count);
            while old_lines.len() < old_count || new_lines.len() < new_count {
                let line = lines.get(index).with_context(|| {
                    format!("truncated unified patch hunk at old line {old_start}")
                })?;
                index += 1;
                if line.starts_with('\\') {
                    continue;
                }
                let (prefix, text) = line.split_at(1);
                match prefix {
                    " " => {
                        old_lines.push(text.to_string());
                        new_lines.push(text.to_string());
                    }
                    "-" => old_lines.push(text.to_string()),
                    "+" => new_lines.push(text.to_string()),
                    _ => bail!("invalid unified patch hunk line: {line}"),
                }
                if old_lines.len() > old_count || new_lines.len() > new_count {
                    bail!("unified patch hunk contains more lines than its header declares");
                }
            }
            file.hunks.push(UnifiedPatchHunk {
                old_start,
                old_lines,
                new_lines,
            });
        }
        if file.hunks.is_empty() {
            bail!(
                "unified patch contains no hunks for {}",
                file.path.display()
            );
        }
        files.push(file);
    }
    if files.is_empty() {
        bail!("unified patch contains no target files");
    }
    Ok(files)
}

fn parse_unified_hunk_header(header: &str) -> Result<(usize, usize, usize)> {
    let ranges = header
        .strip_prefix("@@")
        .and_then(|rest| rest.split_once("@@").map(|(ranges, _)| ranges.trim()))
        .with_context(|| format!("invalid unified patch hunk header: {header}"))?;
    let mut ranges = ranges.split_whitespace();
    let (old_start, old_count) = parse_unified_range(
        ranges
            .next()
            .context("unified patch hunk has no old range")?,
        '-',
    )?;
    let (_, new_count) = parse_unified_range(
        ranges
            .next()
            .context("unified patch hunk has no new range")?,
        '+',
    )?;
    Ok((old_start, old_count, new_count))
}

fn parse_unified_range(range: &str, prefix: char) -> Result<(usize, usize)> {
    let range = range
        .strip_prefix(prefix)
        .with_context(|| format!("unified patch range must start with {prefix}: {range}"))?;
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    let start = start
        .parse::<usize>()
        .with_context(|| format!("invalid unified patch range start: {range}"))?;
    let count = count
        .parse::<usize>()
        .with_context(|| format!("invalid unified patch range count: {range}"))?;
    if start == 0 || count == 0 {
        bail!("zero-length unified patch ranges are not supported: {range}");
    }
    Ok((start, count))
}

fn apply_patch_hunks(contents: &str, hunks: &[UnifiedPatchHunk]) -> Result<(String, bool)> {
    let trailing_newline = contents.ends_with('\n');
    let mut lines = contents.lines().map(str::to_string).collect::<Vec<_>>();
    let mut line_shift = 0_isize;
    let mut changed = false;
    for hunk in hunks {
        let expected = isize::try_from(hunk.old_start - 1)
            .context("unified patch line number does not fit isize")?
            + line_shift;
        if expected < 0 {
            bail!("unified patch hunk resolves before the start of the file");
        }
        let expected = usize::try_from(expected).expect("non-negative patch line index");
        if patch_lines_match(&lines, expected, &hunk.old_lines) {
            lines.splice(
                expected..expected + hunk.old_lines.len(),
                hunk.new_lines.iter().cloned(),
            );
            changed = true;
        } else if !patch_lines_match(&lines, expected, &hunk.new_lines) {
            bail!(
                "hunk at source line {} matches neither the pinned FFmpeg source nor the already-patched source",
                hunk.old_start
            );
        }
        line_shift += hunk.new_lines.len() as isize - hunk.old_lines.len() as isize;
    }

    let mut updated = lines.join("\n");
    if trailing_newline {
        updated.push('\n');
    }
    Ok((updated, changed))
}

fn patch_lines_match(lines: &[String], start: usize, expected: &[String]) -> bool {
    start
        .checked_add(expected.len())
        .filter(|end| *end <= lines.len())
        .is_some_and(|end| lines[start..end] == *expected)
}

fn fetch_and_extract(
    layout: &WorkspaceLayout,
    urls: &[&str],
    archive_name: &str,
    source_dir_name: &str,
    expected_sha256: Option<&str>,
) -> Result<()> {
    let archive_path = layout.cache_dir.join(archive_name);
    let partial_path = layout.cache_dir.join(format!("{archive_name}.part"));
    if archive_path.exists()
        && expected_sha256
            .is_some_and(|expected| !archive_checksum_matches(&archive_path, expected))
    {
        println!("discard invalid archive {}", archive_path.display());
        fs::remove_file(&archive_path)
            .with_context(|| format!("remove {}", archive_path.display()))?;
    }
    if !archive_path.exists() {
        download_archive(urls, &partial_path, &archive_path, expected_sha256)?;
    } else {
        println!("reuse {}", archive_path.display());
    }

    let source_path = layout.source_dir.join(source_dir_name);
    let extraction_marker = source_path.join(".erika-extracted");
    let mut archive_digest = archive_sha256(&archive_path)?;
    let mut expected_marker = format!("archive={archive_name}\nsha256={archive_digest}\n");
    if source_path.exists()
        && fs::read_to_string(&extraction_marker).is_ok_and(|marker| marker == expected_marker)
    {
        println!("reuse {}", source_path.display());
        return Ok(());
    }
    if source_path.exists() {
        fs::remove_dir_all(&source_path)
            .with_context(|| format!("remove incomplete source {}", source_path.display()))?;
    }
    println!("extract {}", archive_path.display());
    if let Err(error) = extract_archive(&archive_path, &layout.source_dir, source_dir_name) {
        println!(
            "extract failed for {}, downloading a fresh archive",
            archive_path.display()
        );
        fs::remove_file(&archive_path)
            .with_context(|| format!("remove {}", archive_path.display()))?;
        download_archive(urls, &partial_path, &archive_path, expected_sha256)
            .with_context(|| format!("recover after extraction failure: {error:#}"))?;
        archive_digest = archive_sha256(&archive_path)?;
        expected_marker = format!("archive={archive_name}\nsha256={archive_digest}\n");
        extract_archive(&archive_path, &layout.source_dir, source_dir_name)?;
    }
    fs::write(&extraction_marker, expected_marker)
        .with_context(|| format!("write {}", extraction_marker.display()))?;
    Ok(())
}

fn extract_archive(archive_path: &Path, destination: &Path, source_dir_name: &str) -> Result<()> {
    let temporary_dir = destination.join(format!(
        ".erika-extract-{source_dir_name}-{}",
        std::process::id()
    ));
    if temporary_dir.exists() {
        fs::remove_dir_all(&temporary_dir)
            .with_context(|| format!("remove {}", temporary_dir.display()))?;
    }
    fs::create_dir_all(&temporary_dir)
        .with_context(|| format!("create {}", temporary_dir.display()))?;
    let result = run(Command::new("tar")
        .arg("-xf")
        .arg(archive_path)
        .arg("-C")
        .arg(&temporary_dir));
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&temporary_dir);
        return Err(error);
    }
    let extracted_source = temporary_dir.join(source_dir_name);
    if !extracted_source.is_dir() {
        let _ = fs::remove_dir_all(&temporary_dir);
        bail!(
            "archive {} did not contain expected directory {source_dir_name}",
            archive_path.display()
        );
    }
    let source_path = destination.join(source_dir_name);
    fs::rename(&extracted_source, &source_path).with_context(|| {
        format!(
            "rename {} to {}",
            extracted_source.display(),
            source_path.display()
        )
    })?;
    fs::remove_dir_all(&temporary_dir)
        .with_context(|| format!("remove {}", temporary_dir.display()))
}

fn download_archive(
    urls: &[&str],
    partial_path: &PathBuf,
    archive_path: &PathBuf,
    expected_sha256: Option<&str>,
) -> Result<()> {
    let mut last_error = None;
    let agent = download_agent();
    for url in urls {
        for attempt in 1..=4 {
            println!("download {url} (attempt {attempt}/4)");
            if partial_path.exists() {
                fs::remove_file(partial_path)
                    .with_context(|| format!("remove {}", partial_path.display()))?;
            }
            match download_url(&agent, url, partial_path).and_then(|()| {
                if expected_sha256
                    .is_none_or(|expected| archive_checksum_matches(partial_path, expected))
                {
                    Ok(())
                } else {
                    bail!("checksum mismatch for {}", partial_path.display())
                }
            }) {
                Ok(()) => {
                    fs::rename(partial_path, archive_path).with_context(|| {
                        format!(
                            "rename {} to {}",
                            partial_path.display(),
                            archive_path.display()
                        )
                    })?;
                    return Ok(());
                }
                Err(error) => {
                    last_error = Some(error);
                    let _ = fs::remove_file(partial_path);
                    if attempt < 4 {
                        let delay = Duration::from_secs(2_u64.pow(attempt));
                        println!("download failed, retrying in {} seconds", delay.as_secs());
                        std::thread::sleep(delay);
                    }
                }
            }
        }
        println!("download failed, trying next source if available");
    }
    match last_error {
        Some(error) => Err(error).context("all download sources failed"),
        None => bail!(
            "no download sources configured for {}",
            archive_path.display()
        ),
    }
}

fn archive_checksum_matches(path: &Path, expected: &str) -> bool {
    archive_sha256(path).is_ok_and(|actual| actual.eq_ignore_ascii_case(expected))
}

fn archive_sha256(path: &Path) -> Result<String> {
    let Ok(mut file) = File::open(path) else {
        bail!("open {}", path.display());
    };
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn download_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(Duration::from_secs(300)))
        .max_redirects(10)
        .build()
        .into()
}

fn download_url(agent: &ureq::Agent, url: &str, partial_path: &Path) -> Result<()> {
    let mut response = agent
        .get(url)
        .header("User-Agent", "erika-xtask")
        .call()
        .with_context(|| format!("download {url}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut output =
        File::create(partial_path).with_context(|| format!("create {}", partial_path.display()))?;
    io::copy(&mut reader, &mut output)
        .with_context(|| format!("write {}", partial_path.display()))?;
    Ok(())
}

fn build_ffmpeg(layout: &WorkspaceLayout, options: DepsOptions) -> Result<()> {
    let marker_is_current = ffmpeg_build_marker_is_current(layout, options);
    if marker_is_current && !options.force {
        println!(
            "reuse FFmpeg build marker {}",
            layout.ffmpeg_build_marker.display()
        );
        ensure_windows_link_aliases(
            options.target,
            &layout.ffmpeg_prefix,
            &[
                ("libavdevice.a", "avdevice.lib"),
                ("libavfilter.a", "avfilter.lib"),
                ("libavformat.a", "avformat.lib"),
                ("libavcodec.a", "avcodec.lib"),
                ("libswresample.a", "swresample.lib"),
                ("libswscale.a", "swscale.lib"),
                ("libavutil.a", "avutil.lib"),
            ],
        )?;
        return Ok(());
    }

    // A changed target/configuration marker must never reuse objects or static
    // archive members produced by the previous FFmpeg configuration.
    if layout.ffmpeg_prefix.exists() {
        fs::remove_dir_all(&layout.ffmpeg_prefix)
            .with_context(|| format!("remove {}", layout.ffmpeg_prefix.display()))?;
    }
    if layout.ffmpeg_build_dir.exists() {
        fs::remove_dir_all(&layout.ffmpeg_build_dir)
            .with_context(|| format!("remove {}", layout.ffmpeg_build_dir.display()))?;
    }
    fs::create_dir_all(&layout.ffmpeg_build_dir)
        .with_context(|| format!("create {}", layout.ffmpeg_build_dir.display()))?;
    fs::create_dir_all(&layout.ffmpeg_prefix)
        .with_context(|| format!("create {}", layout.ffmpeg_prefix.display()))?;
    if uses_windows_posix_ffmpeg(options.target)
        && !layout.ffmpeg_build_dir.join("configure").exists()
    {
        println!("copy FFmpeg source for Windows-hosted in-tree build");
        copy_dir_all(&layout.ffmpeg_source_dir, &layout.ffmpeg_build_dir)?;
    }

    let mut configure = if uses_windows_posix_ffmpeg(options.target) {
        let mut command = Command::new(
            posix_shell().context("required POSIX shell was not found for FFmpeg configure")?,
        );
        command.arg("configure");
        command
    } else {
        Command::new(layout.ffmpeg_source_dir.join("configure"))
    };
    configure.current_dir(&layout.ffmpeg_build_dir);
    configure.arg(format!(
        "--prefix={}",
        path_to_forward_slashes(&layout.ffmpeg_prefix)
    ));
    let pkg_config = ensure_ffmpeg_pkg_config_shim(layout)?;
    let dav1d_pkg_config_dir = layout.dav1d_prefix.join("lib/pkgconfig");
    configure
        .arg(format!(
            "--pkg-config={}",
            ffmpeg_flag_path_arg(&pkg_config)
        ))
        .arg("--pkg-config-flags=--static")
        .env("PKG_CONFIG_PATH", &dav1d_pkg_config_dir)
        .env("PKG_CONFIG_LIBDIR", &dav1d_pkg_config_dir)
        .env("ERIKA_PKG_CONFIG_RELATIVE_BASE", &layout.ffmpeg_build_dir);
    let mut extra_cflags = if options.target.is_windows() {
        Vec::new()
    } else {
        vec!["-fPIC".to_string()]
    };
    let mut extra_ldflags = Vec::new();
    let gas_preprocessor_dir = if matches!(options.target, NativeTarget::Aarch64WindowsMsvc) {
        ensure_gas_preprocessor(layout)?
            .parent()
            .map(Path::to_path_buf)
    } else {
        None
    };
    if let Some(config) = apple_toolchain(options.target)? {
        configure.arg(format!("--cc={}", config.clang.display()));
        configure.arg(format!("--ar={}", config.ar.display()));
        configure.arg(format!("--ranlib={}", config.ranlib.display()));
        configure.arg(format!("--strip={}", config.strip.display()));
        configure.arg("--target-os=darwin");
        configure.arg("--enable-cross-compile");
        configure.arg(format!("--arch={}", config.arch));
        configure.arg(format!("--sysroot={}", config.sdk_root.display()));
        extra_cflags.push(format!("-arch {}", config.arch));
        extra_cflags.push(format!("-isysroot {}", config.sdk_root.display()));
        extra_cflags.push(format!(
            "{}={}",
            config.deployment_flag, config.deployment_target
        ));
        extra_ldflags.push(format!("-arch {}", config.arch));
        extra_ldflags.push(format!("-isysroot {}", config.sdk_root.display()));
        extra_ldflags.push(format!(
            "{}={}",
            config.deployment_flag, config.deployment_target
        ));
        extra_cflags.push(format!(
            "-I{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("include"))
        ));
        extra_ldflags.push(format!(
            "-L{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("lib"))
        ));
        if options.target.is_apple() {
            extra_cflags.push(format!(
                "-I{}",
                ffmpeg_flag_path_arg(&layout.dav1d_prefix.join("include"))
            ));
            extra_ldflags.push(format!(
                "-L{}",
                ffmpeg_flag_path_arg(&layout.dav1d_prefix.join("lib"))
            ));
        }
        clear_apple_deployment_target_env(&mut configure);
        configure.env("SDKROOT", &config.sdk_root);
        match options.target {
            NativeTarget::Aarch64Macos | NativeTarget::X86_64Macos => {
                configure.env("MACOSX_DEPLOYMENT_TARGET", &config.deployment_target);
            }
            NativeTarget::Aarch64Ios
            | NativeTarget::Aarch64IosSimulator
            | NativeTarget::X86_64IosSimulator => {
                configure.env("IPHONEOS_DEPLOYMENT_TARGET", &config.deployment_target);
            }
            NativeTarget::Aarch64Tvos
            | NativeTarget::Aarch64TvosSimulator
            | NativeTarget::X86_64TvosSimulator => {
                configure.env("TVOS_DEPLOYMENT_TARGET", &config.deployment_target);
            }
            NativeTarget::Host
            | NativeTarget::X86_64WindowsMsvc
            | NativeTarget::Aarch64WindowsMsvc
            | NativeTarget::Aarch64Android
            | NativeTarget::Armv7Android
            | NativeTarget::X86_64Android
            | NativeTarget::I686Android
            | NativeTarget::Aarch64Ohos
            | NativeTarget::X86_64Ohos => {}
        }
    } else if let Some(config) = android_toolchain(options.target)? {
        configure.arg(format!("--cc={}", ffmpeg_flag_path_arg(&config.clang)));
        configure.arg(format!("--cxx={}", ffmpeg_flag_path_arg(&config.clangxx)));
        configure.arg(format!("--ar={}", ffmpeg_flag_path_arg(&config.ar)));
        configure.arg(format!("--ranlib={}", ffmpeg_flag_path_arg(&config.ranlib)));
        configure.arg(format!("--strip={}", ffmpeg_flag_path_arg(&config.strip)));
        configure.arg(format!("--nm={}", ffmpeg_flag_path_arg(&config.nm)));
        configure.arg("--target-os=android");
        configure.arg("--enable-cross-compile");
        configure.arg(format!("--arch={}", config.arch));
        configure.arg(format!(
            "--sysroot={}",
            path_to_forward_slashes(&config.sysroot)
        ));
        if let Some(host_cc) = ffmpeg_android_host_cc(&config)? {
            configure.arg(format!("--host-cc={host_cc}"));
        }
        extra_cflags.push(format!(
            "-I{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("include"))
        ));
        extra_cflags.push(format!(
            "-I{}",
            ffmpeg_flag_path_arg(&layout.dav1d_prefix.join("include"))
        ));
        extra_ldflags.push(format!(
            "-L{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("lib"))
        ));
        extra_ldflags.push(format!(
            "-L{}",
            ffmpeg_flag_path_arg(&layout.dav1d_prefix.join("lib"))
        ));
        configure.env("ANDROID_NDK_HOME", &config.ndk_root);
    } else if let Some(config) = ohos_toolchain(options.target)? {
        configure.arg(format!("--cc={}", ffmpeg_flag_path_arg(&config.clang)));
        configure.arg(format!("--cxx={}", ffmpeg_flag_path_arg(&config.clangxx)));
        configure.arg(format!("--ar={}", ffmpeg_flag_path_arg(&config.ar)));
        configure.arg(format!("--ranlib={}", ffmpeg_flag_path_arg(&config.ranlib)));
        configure.arg(format!("--strip={}", ffmpeg_flag_path_arg(&config.strip)));
        configure.arg(format!("--nm={}", ffmpeg_flag_path_arg(&config.nm)));
        configure.arg("--target-os=linux");
        configure.arg("--enable-cross-compile");
        configure.arg(format!("--arch={}", config.arch));
        configure.arg(format!(
            "--sysroot={}",
            path_to_forward_slashes(&config.sysroot)
        ));
        extra_cflags.push(format!(
            "-I{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("include"))
        ));
        extra_ldflags.push(format!(
            "-L{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("lib"))
        ));
    } else if options.target.is_windows() {
        configure.arg("--target-os=win64");
        configure.arg(format!(
            "--arch={}",
            options
                .target
                .ffmpeg_arch()
                .context("Windows target must have an FFmpeg architecture")?
        ));
        if matches!(options.target, NativeTarget::Aarch64WindowsMsvc) {
            configure.arg("--enable-cross-compile");
            apply_ffmpeg_assembler_path(
                &mut configure,
                options.target,
                gas_preprocessor_dir.as_deref(),
            );
        }
        configure.arg("--toolchain=msvc");
        extra_cflags.push(format!(
            "-I{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("include"))
        ));
        extra_ldflags.push(format!(
            "-libpath:{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("lib"))
        ));
        apply_windows_target_env(&mut configure, options.target)?;
    } else {
        configure.arg("--cc=clang");
        extra_cflags.push(format!(
            "-I{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("include"))
        ));
        extra_ldflags.push(format!(
            "-L{}",
            ffmpeg_flag_path_arg(&layout.zlib_prefix.join("lib"))
        ));
    }
    if !options.target.is_android() && !options.target.is_apple() {
        extra_cflags.push(format!(
            "-I{}",
            ffmpeg_flag_path_arg(&layout.dav1d_prefix.join("include"))
        ));
        extra_ldflags.push(format!(
            "{}{}",
            if options.target.is_windows() {
                "-libpath:"
            } else {
                "-L"
            },
            ffmpeg_flag_path_arg(&layout.dav1d_prefix.join("lib")),
        ));
    }
    apply_windows_posix_shell(&mut configure, options.target);
    append_windows_posix_paths(&mut configure);
    apply_android_host_env(&mut configure, options.target)?;
    if !extra_cflags.is_empty() {
        configure.arg(format!("--extra-cflags={}", extra_cflags.join(" ")));
    }
    if !extra_ldflags.is_empty() {
        configure.arg(format!("--extra-ldflags={}", extra_ldflags.join(" ")));
    }
    for flag in options
        .profile
        .ffmpeg_configure_flags_for(options.target, options.crt)
    {
        configure.arg(flag);
    }

    println!("configure FFmpeg");
    if let Err(error) = run(&mut configure) {
        let log = layout.ffmpeg_build_dir.join("ffbuild/config.log");
        if let Ok(contents) = fs::read_to_string(&log) {
            let lines = contents.lines().collect::<Vec<_>>();
            eprintln!("FFmpeg configure diagnostics ({}):", log.display());
            for line in &lines[lines.len().saturating_sub(100)..] {
                eprintln!("{line}");
            }
        }
        return Err(error);
    }
    if cfg!(windows) && options.target.is_windows() {
        fix_ffmpeg_msvc_archive_response_file_creation(&layout.ffmpeg_build_dir)?;
    }
    if cfg!(windows) && options.target.is_android() {
        enable_ffmpeg_archive_response_files(&layout.ffmpeg_build_dir)?;
    }

    let jobs = options.jobs.unwrap_or_else(default_job_count);
    println!("build FFmpeg with {jobs} jobs");
    let make = gnu_make().context("required GNU make was not found")?;
    let build_args = [format!("-j{jobs}")];
    let mut build =
        ffmpeg_make_command(&make, &layout.ffmpeg_build_dir, &build_args, options.target)?;
    apply_windows_target_env(&mut build, options.target)?;
    apply_ffmpeg_assembler_path(&mut build, options.target, gas_preprocessor_dir.as_deref());
    apply_android_host_env(&mut build, options.target)?;
    apply_windows_posix_shell(&mut build, options.target);
    append_windows_posix_paths(&mut build);
    run(&mut build)?;
    let install_args = ["install".to_string()];
    let mut install = ffmpeg_make_command(
        &make,
        &layout.ffmpeg_build_dir,
        &install_args,
        options.target,
    )?;
    apply_windows_target_env(&mut install, options.target)?;
    apply_ffmpeg_assembler_path(
        &mut install,
        options.target,
        gas_preprocessor_dir.as_deref(),
    );
    apply_android_host_env(&mut install, options.target)?;
    apply_windows_posix_shell(&mut install, options.target);
    append_windows_posix_paths(&mut install);
    run(&mut install)?;
    ensure_windows_link_aliases(
        options.target,
        &layout.ffmpeg_prefix,
        &[
            ("libavdevice.a", "avdevice.lib"),
            ("libavfilter.a", "avfilter.lib"),
            ("libavformat.a", "avformat.lib"),
            ("libavcodec.a", "avcodec.lib"),
            ("libswresample.a", "swresample.lib"),
            ("libswscale.a", "swscale.lib"),
            ("libavutil.a", "avutil.lib"),
        ],
    )?;

    let ffmpeg_patchset = ffmpeg_patchset_id(&layout.root)?;
    fs::write(
        &layout.ffmpeg_build_marker,
        format!(
            "ffmpeg={FFMPEG_VERSION}\npatchset={}\nzlib={ZLIB_VERSION}\ndav1d={}\nprofile={}\ntarget={}\nandroid_api={}\ndeployment_target={}\nprefix={}\nflags={}\n",
            ffmpeg_patchset,
            DAV1D_VERSION,
            profile_name(options.profile),
            options.target.triple().unwrap_or("host"),
            if options.target.is_android() {
                android_api_level()?.to_string()
            } else {
                "n/a".to_string()
            },
            native_deployment_target(options.target),
            layout.ffmpeg_prefix.display(),
            options
                .profile
                .ffmpeg_configure_flags_for(options.target, options.crt)
                .join(" ")
        ),
    )
    .with_context(|| format!("write {}", layout.ffmpeg_build_marker.display()))?;
    Ok(())
}

fn ensure_gas_preprocessor(layout: &WorkspaceLayout) -> Result<PathBuf> {
    let tools_dir = layout.cache_dir.join("tools");
    let destination = tools_dir.join("gas-preprocessor.pl");
    let marker = tools_dir.join("gas-preprocessor.revision");
    let current = fs::read_to_string(&marker).ok().is_some_and(|value| {
        value.trim() == format!("{GAS_PREPROCESSOR_REVISION}-{GAS_PREPROCESSOR_PATCH_VERSION}")
    });
    if destination.is_file() && current {
        ensure_gas_preprocessor_executable(&destination)?;
        return Ok(destination);
    }
    fs::create_dir_all(&tools_dir).with_context(|| format!("create {}", tools_dir.display()))?;
    let partial = tools_dir.join("gas-preprocessor.pl.part");
    if partial.exists() {
        fs::remove_file(&partial).with_context(|| format!("remove {}", partial.display()))?;
    }
    println!("download {GAS_PREPROCESSOR_URL}");
    download_url(&download_agent(), GAS_PREPROCESSOR_URL, &partial)?;
    patch_gas_preprocessor_for_armasm64(&partial)?;
    fs::rename(&partial, &destination)
        .with_context(|| format!("rename {} to {}", partial.display(), destination.display()))?;
    fs::write(
        &marker,
        format!("{GAS_PREPROCESSOR_REVISION}-{GAS_PREPROCESSOR_PATCH_VERSION}\n"),
    )
    .with_context(|| format!("write {}", marker.display()))?;
    ensure_gas_preprocessor_executable(&destination)?;
    Ok(destination)
}

fn patch_gas_preprocessor_for_armasm64(path: &Path) -> Result<()> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let branch_pattern = "(cbn?z|adr|tbz)";
    let patched_branch_pattern = "(cbn?z|adr|tbn?z)";
    let register_workaround = "if ($instr eq \"tbz\" and";
    let patched_register_workaround = "if ($instr =~ /^tbn?z$/ and";
    if !contents.contains(branch_pattern) || !contents.contains(register_workaround) {
        bail!(
            "gas-preprocessor ARMASM branch pattern was not found in {}",
            path.display()
        );
    }
    let patched = contents
        .replacen(branch_pattern, patched_branch_pattern, 1)
        .replacen(register_workaround, patched_register_workaround, 1);
    fs::write(path, patched).with_context(|| format!("write patched {}", path.display()))
}

fn ensure_gas_preprocessor_executable(path: &Path) -> Result<()> {
    if !cfg!(windows) {
        return Ok(());
    }
    let shell = posix_shell().context("required POSIX shell was not found")?;
    run(Command::new(shell).arg("-lc").arg(format!(
        "chmod +x {}",
        shell_quote(&path_to_forward_slashes(path))
    )))
}

fn apply_ffmpeg_assembler_path(command: &mut Command, target: NativeTarget, dir: Option<&Path>) {
    if matches!(target, NativeTarget::Aarch64WindowsMsvc) {
        if let Some(dir) = dir {
            prepend_path_to_command(command, dir);
        }
    }
}

fn fix_ffmpeg_msvc_archive_response_file_creation(build_dir: &Path) -> Result<()> {
    let makefile = build_dir.join("ffbuild/library.mak");
    let contents =
        fs::read_to_string(&makefile).with_context(|| format!("read {}", makefile.display()))?;
    let original = "\t$(Q)echo $^ > $@.objs";
    let replacement = "\t$(file >$@.objs,$^)";
    if contents.contains(replacement) {
        return Ok(());
    }
    if !contents.contains(original) {
        bail!(
            "FFmpeg MSVC response-file recipe was not found in {}",
            makefile.display()
        );
    }
    fs::write(&makefile, contents.replacen(original, replacement, 1))
        .with_context(|| format!("write {}", makefile.display()))?;
    println!("patched FFmpeg MSVC archive response-file creation");
    Ok(())
}

fn enable_ffmpeg_archive_response_files(build_dir: &Path) -> Result<()> {
    let makefile = build_dir.join("ffbuild/library.mak");
    let contents =
        fs::read_to_string(&makefile).with_context(|| format!("read {}", makefile.display()))?;
    let original = "\t$(AR) $(ARFLAGS) $(AR_O) $^";
    let replacement = concat!(
        "\t$(file >$@.rsp,$(ARFLAGS) $(AR_O) $^)\n",
        "\t$(AR) @$@.rsp\n",
        "\t$(RM) $@.rsp"
    );
    if contents.contains(replacement) {
        return Ok(());
    }
    if !contents.contains(original) {
        bail!(
            "FFmpeg archive recipe was not found in {}; cannot enable Windows response files",
            makefile.display()
        );
    }
    let patched = contents.replacen(original, replacement, 1);
    fs::write(&makefile, patched).with_context(|| {
        format!(
            "write response-file archive recipe to {}",
            makefile.display()
        )
    })?;
    println!("patched FFmpeg static archive recipe to use llvm-ar response files on Windows");
    Ok(())
}

struct AppleToolchain {
    clang: PathBuf,
    clangxx: PathBuf,
    ar: PathBuf,
    ranlib: PathBuf,
    strip: PathBuf,
    sdk_root: PathBuf,
    arch: &'static str,
    deployment_flag: &'static str,
    deployment_target: String,
}

#[derive(Debug, Clone)]
struct AndroidToolchain {
    ndk_root: PathBuf,
    bin_dir: PathBuf,
    sysroot: PathBuf,
    cmake_toolchain_file: PathBuf,
    clang: PathBuf,
    clangxx: PathBuf,
    ar: PathBuf,
    ranlib: PathBuf,
    strip: PathBuf,
    nm: PathBuf,
    arch: &'static str,
    abi: &'static str,
    api_level: u32,
}

#[derive(Debug, Clone)]
struct OhosToolchain {
    native_root: PathBuf,
    bin_dir: PathBuf,
    sysroot: PathBuf,
    cmake_toolchain_file: PathBuf,
    clang: PathBuf,
    clangxx: PathBuf,
    ar: PathBuf,
    ranlib: PathBuf,
    strip: PathBuf,
    nm: PathBuf,
    compatible_sdk_version: String,
    triple: &'static str,
    arch: &'static str,
    abi: &'static str,
}

fn apple_toolchain(target: NativeTarget) -> Result<Option<AppleToolchain>> {
    let Some(sdk) = target.sdk() else {
        return Ok(None);
    };
    let sdk_root = PathBuf::from(xcrun(sdk, &["--show-sdk-path"])?);
    let (deployment_target, deployment_flag) = target
        .deployment_target()
        .context("explicit Apple target must have a deployment target")?;
    Ok(Some(AppleToolchain {
        clang: PathBuf::from(xcrun(sdk, &["-f", "clang"])?),
        clangxx: PathBuf::from(xcrun(sdk, &["-f", "clang++"])?),
        ar: PathBuf::from(xcrun(sdk, &["-f", "ar"])?),
        ranlib: PathBuf::from(xcrun(sdk, &["-f", "ranlib"])?),
        strip: PathBuf::from(xcrun(sdk, &["-f", "strip"])?),
        sdk_root,
        arch: target
            .ffmpeg_arch()
            .context("explicit Apple target must have an FFmpeg arch")?,
        deployment_flag,
        deployment_target,
    }))
}

fn android_toolchain(target: NativeTarget) -> Result<Option<AndroidToolchain>> {
    if !target.is_android() || matches!(target, NativeTarget::Host) {
        return Ok(None);
    }
    let ndk_root = android_ndk_root()?;
    let host_tag = android_ndk_host_tag(&ndk_root)?;
    let bin_dir = ndk_root
        .join("toolchains/llvm/prebuilt")
        .join(&host_tag)
        .join("bin");
    let sysroot = ndk_root
        .join("toolchains/llvm/prebuilt")
        .join(&host_tag)
        .join("sysroot");
    let cmake_toolchain_file = ndk_root.join("build/cmake/android.toolchain.cmake");
    let api_level = android_api_level()?;
    let clang_triple = target
        .android_clang_triple()
        .context("explicit Android target must have a Clang triple")?;
    let clang = required_executable_in_dir(
        &bin_dir,
        &format!("{clang_triple}{api_level}-clang"),
        "Android NDK C compiler",
    )?;
    let clangxx = required_executable_in_dir(
        &bin_dir,
        &format!("{clang_triple}{api_level}-clang++"),
        "Android NDK C++ compiler",
    )?;
    let ar = required_executable_in_dir(&bin_dir, "llvm-ar", "Android NDK archiver")?;
    let ranlib = required_executable_in_dir(&bin_dir, "llvm-ranlib", "Android NDK ranlib")?;
    let strip = required_executable_in_dir(&bin_dir, "llvm-strip", "Android NDK strip")?;
    let nm = required_executable_in_dir(&bin_dir, "llvm-nm", "Android NDK nm")?;
    if !sysroot.is_dir() || !cmake_toolchain_file.is_file() {
        bail!(
            "Android NDK at {} is incomplete (missing sysroot or android.toolchain.cmake)",
            ndk_root.display()
        );
    }
    Ok(Some(AndroidToolchain {
        ndk_root,
        bin_dir,
        sysroot,
        cmake_toolchain_file,
        clang,
        clangxx,
        ar,
        ranlib,
        strip,
        nm,
        arch: target
            .ffmpeg_arch()
            .context("explicit Android target must have an FFmpeg arch")?,
        abi: target
            .android_abi()
            .context("explicit Android target must have an ABI")?,
        api_level,
    }))
}

fn ohos_toolchain(target: NativeTarget) -> Result<Option<OhosToolchain>> {
    if !target.is_ohos() || matches!(target, NativeTarget::Host) {
        return Ok(None);
    }
    let native_root = ohos_native_root()?;
    let bin_dir = native_root.join("llvm/bin");
    let sysroot = native_root.join("sysroot");
    let cmake_toolchain_file = native_root.join("build/cmake/ohos.toolchain.cmake");
    let triple = target.triple().context("OHOS target must have a triple")?;
    let clang = required_executable_in_dir(
        &bin_dir,
        &format!("{triple}-clang"),
        "OpenHarmony C compiler",
    )?;
    let clangxx = required_executable_in_dir(
        &bin_dir,
        &format!("{triple}-clang++"),
        "OpenHarmony C++ compiler",
    )?;
    let ar = required_executable_in_dir(&bin_dir, "llvm-ar", "OpenHarmony archiver")?;
    let ranlib = required_executable_in_dir(&bin_dir, "llvm-ranlib", "OpenHarmony ranlib")?;
    let strip = required_executable_in_dir(&bin_dir, "llvm-strip", "OpenHarmony strip")?;
    let nm = required_executable_in_dir(&bin_dir, "llvm-nm", "OpenHarmony nm")?;
    if !sysroot.is_dir() || !cmake_toolchain_file.is_file() {
        bail!(
            "OpenHarmony native SDK at {} is incomplete (missing sysroot or ohos.toolchain.cmake)",
            native_root.display()
        );
    }
    let compatible_sdk_version =
        env::var("OHOS_COMPATIBLE_SDK_VERSION").unwrap_or_else(|_| "18".to_string());
    Ok(Some(OhosToolchain {
        native_root,
        bin_dir,
        sysroot,
        cmake_toolchain_file,
        clang,
        clangxx,
        ar,
        ranlib,
        strip,
        nm,
        compatible_sdk_version,
        triple,
        arch: target
            .ffmpeg_arch()
            .context("OHOS target must have an arch")?,
        abi: target.ohos_abi().context("OHOS target must have an ABI")?,
    }))
}

fn ohos_native_root() -> Result<PathBuf> {
    for variable in ["OHOS_NDK_HOME", "OHOS_SDK_NATIVE"] {
        if let Some(value) = env::var_os(variable) {
            let path = PathBuf::from(value);
            if path.is_dir() {
                return Ok(path);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let deveco = PathBuf::from(
            "/Applications/DevEco-Studio.app/Contents/sdk/default/openharmony/native",
        );
        if deveco.is_dir() {
            return Ok(deveco);
        }
    }

    bail!(
        "OpenHarmony native SDK was not found; set OHOS_NDK_HOME or OHOS_SDK_NATIVE to the SDK native directory"
    )
}

fn ffmpeg_android_host_cc(config: &AndroidToolchain) -> Result<Option<String>> {
    if !cfg!(windows) {
        return Ok(None);
    }
    let clang =
        required_executable_in_dir(&config.bin_dir, "clang", "Android NDK host Clang compiler")?;
    Ok(Some(format!(
        "{} --target={} -fuse-ld=link",
        ffmpeg_flag_path_arg(&clang),
        windows_host_target()
            .triple()
            .context("Windows host target must have a Rust triple")?
    )))
}

fn android_api_level() -> Result<u32> {
    let value = env::var("ANDROID_API_LEVEL")
        .ok()
        .map(|value| {
            value
                .trim_start_matches("android-")
                .parse::<u32>()
                .with_context(|| format!("ANDROID_API_LEVEL must be an integer, got `{value}`"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_ANDROID_API_LEVEL);
    if value < DEFAULT_ANDROID_API_LEVEL {
        bail!(
            "Android API level {value} is unsupported; Erika requires API {} or newer",
            DEFAULT_ANDROID_API_LEVEL
        );
    }
    Ok(value)
}

fn android_ndk_root() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    for variable in ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT"] {
        if let Some(path) = env::var_os(variable) {
            candidates.push(PathBuf::from(path));
        }
    }
    for variable in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(path) = env::var_os(variable) {
            add_android_sdk_ndk_candidates(&mut candidates, &PathBuf::from(path));
        }
    }
    if cfg!(windows) {
        if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
            add_android_sdk_ndk_candidates(
                &mut candidates,
                &PathBuf::from(local_app_data).join("Android/Sdk"),
            );
        }
    }
    if let Some(path) = candidates.into_iter().find(|path| {
        path.join("build/cmake/android.toolchain.cmake").is_file()
            && path.join("toolchains/llvm/prebuilt").is_dir()
    }) {
        return Ok(path);
    }
    bail!(
        "Android NDK was not found. Set ANDROID_NDK_HOME/ANDROID_NDK_ROOT, or install a side-by-side NDK under ANDROID_HOME/ndk"
    )
}

fn add_android_sdk_ndk_candidates(candidates: &mut Vec<PathBuf>, sdk_root: &Path) {
    let ndk_dir = sdk_root.join("ndk");
    if let Ok(entries) = fs::read_dir(&ndk_dir) {
        let mut versions = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        versions.sort_by_key(|path| {
            path.file_name()
                .map(|name| android_ndk_version_key(&name.to_string_lossy()))
                .unwrap_or_default()
        });
        versions.reverse();
        candidates.extend(versions);
    }
    candidates.push(sdk_root.join("ndk-bundle"));
}

fn android_ndk_version_key(value: &str) -> Vec<u32> {
    value
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

fn android_ndk_host_tag(ndk_root: &Path) -> Result<String> {
    let prebuilt = ndk_root.join("toolchains/llvm/prebuilt");
    let preferred: &[&str] = if cfg!(windows) {
        &["windows-x86_64"]
    } else if cfg!(target_os = "macos") {
        &["darwin-aarch64", "darwin-x86_64"]
    } else {
        &["linux-x86_64"]
    };
    for tag in preferred {
        if prebuilt.join(tag).is_dir() {
            return Ok((*tag).to_string());
        }
    }
    let available = fs::read_dir(&prebuilt)
        .with_context(|| format!("read Android NDK prebuilt directory {}", prebuilt.display()))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    bail!(
        "Android NDK at {} has no toolchain for this host (available: {})",
        ndk_root.display(),
        available.join(", ")
    )
}

fn required_executable_in_dir(dir: &Path, name: &str, description: &str) -> Result<PathBuf> {
    let executable = executable_in_dir(dir, name);
    if executable.is_file() {
        Ok(executable)
    } else {
        bail!("{description} was not found under {}", dir.display())
    }
}

fn ffmpeg_flag_path_arg(path: &Path) -> String {
    shell_escape(&path.to_string_lossy().replace('\\', "/"))
}

fn ffmpeg_build_marker_is_current(layout: &WorkspaceLayout, options: DepsOptions) -> bool {
    let Ok(marker) = fs::read_to_string(&layout.ffmpeg_build_marker) else {
        return false;
    };
    let android_api_is_current = if options.target.is_android() {
        android_api_level().is_ok_and(|api| marker.contains(&format!("android_api={api}\n")))
    } else {
        marker.contains("android_api=n/a\n")
    };
    let patchset_is_current = ffmpeg_patchset_id(&layout.root)
        .is_ok_and(|patchset| ffmpeg_build_marker_has_current_patchset(&marker, &patchset));
    marker.contains(&format!("ffmpeg={FFMPEG_VERSION}\n"))
        && marker.contains(&format!("profile={}\n", profile_name(options.profile)))
        && marker.contains(&format!(
            "target={}\n",
            options.target.triple().unwrap_or("host")
        ))
        && marker.contains(&format!("zlib={ZLIB_VERSION}\n"))
        && marker.contains(&format!("dav1d={}\n", DAV1D_VERSION))
        && ffmpeg_build_marker_has_current_flags(
            &marker,
            options.profile,
            options.target,
            options.crt,
        )
        && patchset_is_current
        && android_api_is_current
        && marker.contains(&format!(
            "deployment_target={}\n",
            native_deployment_target(options.target)
        ))
        && ffmpeg_install_is_complete(layout, options.target)
}

fn ffmpeg_install_is_complete(layout: &WorkspaceLayout, target: NativeTarget) -> bool {
    let library_extension = if target.is_windows() { "lib" } else { "a" };
    layout
        .ffmpeg_prefix
        .join("include/libavformat/avformat.h")
        .is_file()
        && [
            "avdevice",
            "avfilter",
            "avformat",
            "avcodec",
            "swresample",
            "swscale",
            "avutil",
        ]
        .iter()
        .all(|library| {
            layout
                .ffmpeg_prefix
                .join("lib")
                .join(format!("{library}.{library_extension}"))
                .is_file()
                || layout
                    .ffmpeg_prefix
                    .join("lib")
                    .join(format!("lib{library}.a"))
                    .is_file()
        })
}

fn dav1d_build_marker_is_current(layout: &WorkspaceLayout, options: DepsOptions) -> bool {
    let Ok(marker) = fs::read_to_string(&layout.dav1d_build_marker) else {
        return false;
    };
    let api_is_current = if options.target.is_android() {
        android_api_level().is_ok_and(|api| marker.contains(&format!("android_api={api}\n")))
    } else {
        marker.contains("android_api=n/a\n")
    };
    api_is_current && {
        native_static_lib_exists(&layout.dav1d_prefix, "dav1d")
            && layout.dav1d_prefix.join("include/dav1d/dav1d.h").is_file()
            && layout.dav1d_prefix.join("lib/pkgconfig/dav1d.pc").is_file()
            && marker.contains(&format!("dav1d={DAV1D_VERSION}\n"))
            && marker.contains(&format!(
                "target={}\n",
                options.target.triple().unwrap_or("host")
            ))
            && marker.contains(&format!(
                "deployment_target={}\n",
                native_deployment_target(options.target)
            ))
            && marker.contains(&format!("asm={}\n", dav1d_asm_enabled(options.target)))
    }
}

fn ffmpeg_build_marker_has_current_flags(
    marker: &str,
    profile: NativeDependencyProfile,
    target: NativeTarget,
    crt: WindowsCrt,
) -> bool {
    let expected = profile.ffmpeg_configure_flags_for(target, crt).join(" ");
    marker
        .lines()
        .find_map(|line| line.strip_prefix("flags="))
        .is_some_and(|flags| flags == expected)
}

fn ffmpeg_build_marker_has_current_patchset(marker: &str, expected: &str) -> bool {
    marker
        .lines()
        .find_map(|line| line.strip_prefix("patchset="))
        .is_some_and(|patchset| patchset == expected)
}

fn shell_escape(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | ':' | '.' | '_' | '-'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write_profile_metadata(
    layout: &WorkspaceLayout,
    profile: NativeDependencyProfile,
    target: NativeTarget,
) -> Result<()> {
    fs::create_dir_all(&layout.dist_dir)
        .with_context(|| format!("create {}", layout.dist_dir.display()))?;
    let ffmpeg_patchset = ffmpeg_patchset_id(&layout.root)?;
    fs::write(
        layout.dist_dir.join("erika-native-deps.txt"),
        format!(
            "profile={}\ntarget={}\nandroid_api={}\nffmpeg={}\nffmpeg_patchset={}\nffmpeg_dist={}\ndav1d={}\ndav1d_dist={}\nzlib={}\nzlib_dist={}\nlibass={}\nlibass_source={}\nharfbuzz={}\nharfbuzz_source={}\nfreetype={}\nfreetype_source={}\nfribidi={}\nfribidi_source={}\n",
            profile_name(profile),
            target.triple().unwrap_or("host"),
            if target.is_android() {
                android_api_level()?.to_string()
            } else {
                "n/a".to_string()
            },
            FFMPEG_VERSION,
            ffmpeg_patchset,
            layout.ffmpeg_prefix.display(),
            DAV1D_VERSION,
            layout.dav1d_prefix.display(),
            ZLIB_VERSION,
            layout.zlib_prefix.display(),
            LIBASS_VERSION,
            source_state(&layout.libass_source_dir),
            HARFBUZZ_VERSION,
            source_state(&layout.harfbuzz_source_dir),
            FREETYPE_VERSION,
            source_state(&layout.freetype_source_dir),
            FRIBIDI_VERSION,
            source_state(&layout.fribidi_source_dir)
        ),
    )
    .with_context(|| format!("write metadata in {}", layout.dist_dir.display()))?;
    Ok(())
}

fn check_license_policy() -> Result<()> {
    let root = workspace_root()?;
    let manifest = fs::read_to_string(root.join("crates/erika_ffmpeg_sys/Cargo.toml"))
        .context("read erika_ffmpeg_sys manifest")?;
    if !manifest.contains("default = [\"lgpl\"]") {
        bail!("erika_ffmpeg_sys default feature must be exactly lgpl");
    }
    if !NativeDependencyProfile::Lgpl
        .ffmpeg_configure_flags()
        .contains(&"--disable-gpl")
    {
        bail!("LGPL profile must pass --disable-gpl");
    }
    if NativeDependencyProfile::Lgpl
        .ffmpeg_configure_flags()
        .contains(&"--enable-gpl")
    {
        bail!("LGPL profile must not pass --enable-gpl");
    }
    if !NativeDependencyProfile::GplFull
        .ffmpeg_configure_flags()
        .contains(&"--enable-gpl")
    {
        bail!("gpl-full profile must explicitly pass --enable-gpl");
    }
    let notices = fs::read_to_string(root.join("packaging/THIRD_PARTY_NOTICES.md"))
        .context("read third-party notices")?;
    if !notices.contains("| dav1d | 1.5.x | BSD 2-Clause |")
        || !root.join("packaging/LICENSE.dav1d").is_file()
    {
        bail!("dav1d BSD 2-Clause attribution must ship with release bundles");
    }
    println!("license policy ok: default=lgpl, gpl-full is opt-in, dav1d BSD notice present");
    Ok(())
}

fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(PathBuf::from)
        .context("xtask manifest has no parent")
}

fn profile_name(profile: NativeDependencyProfile) -> &'static str {
    match profile {
        NativeDependencyProfile::Lgpl => "lgpl",
        NativeDependencyProfile::GplFull => "gpl-full",
    }
}

fn default_job_count() -> usize {
    std::thread::available_parallelism()
        .map_or(4, usize::from)
        .max(1)
}

fn status_word(ok: bool) -> &'static str {
    if ok { "ready" } else { "missing" }
}

fn source_state(path: &std::path::Path) -> &'static str {
    status_word(path.exists())
}

fn native_static_lib_exists(prefix: &Path, name: &str) -> bool {
    let lib_dir = prefix.join("lib");
    [
        format!("lib{name}.a"),
        format!("{name}.lib"),
        format!("lib{name}.lib"),
    ]
    .into_iter()
    .any(|file| lib_dir.join(file).exists())
}

fn smoke_ffmpeg_make(options: DepsOptions) -> Result<()> {
    if !options.target.is_windows() {
        bail!("deps smoke-ffmpeg-make only applies to the Windows FFmpeg/MSYS make path");
    }

    let make = gnu_make().context("required GNU make was not found")?;
    let smoke_dir = env::temp_dir().join(format!("erika-ffmpeg-make-smoke-{}", std::process::id()));
    if smoke_dir.exists() {
        fs::remove_dir_all(&smoke_dir)
            .with_context(|| format!("remove {}", smoke_dir.display()))?;
    }
    fs::create_dir_all(&smoke_dir).with_context(|| format!("create {}", smoke_dir.display()))?;
    let root = workspace_root()?;
    fs::write(
        smoke_dir.join("header_smoke.cpp"),
        "#include \"crates/erika_capi/include/erika.h\"\nint main() { return 0; }\n",
    )
    .with_context(|| format!("write {}", smoke_dir.join("header_smoke.cpp").display()))?;
    fs::write(
        smoke_dir.join("Makefile"),
        format!(
            "all:\n\t@echo cwd=$(CURDIR)\n\t@test -f Makefile\n\t@command -v cl.exe >/dev/null\n\t@awk 'BEGIN {{ bs = sprintf(\"%c\", 92); s = \"C:\" bs \"tmp\" bs \"file\"; gsub(bs bs, \"/\", s); if (s != \"C:/tmp/file\") exit 42 }}'\n\t@cl.exe /nologo /TP /std:c++17 /I\"{}\" /c header_smoke.cpp\n\t@echo erika-ffmpeg-make-smoke-ok\n",
            path_to_forward_slashes(&root)
        ),
    )
    .with_context(|| format!("write {}", smoke_dir.join("Makefile").display()))?;

    let args = ["all".to_string()];
    let mut command = ffmpeg_make_command(&make, &smoke_dir, &args, options.target)?;
    apply_windows_target_env(&mut command, options.target)?;
    apply_windows_posix_shell(&mut command, options.target);
    append_windows_posix_paths(&mut command);
    let result = run(&mut command);
    let _ = fs::remove_dir_all(&smoke_dir);
    result
}

fn ensure_windows_link_aliases(
    target: NativeTarget,
    prefix: &Path,
    aliases: &[(&str, &str)],
) -> Result<()> {
    if !target.is_windows() {
        return Ok(());
    }
    let lib_dir = prefix.join("lib");
    for (source, alias) in aliases {
        let source = lib_dir.join(source);
        let alias = lib_dir.join(alias);
        if alias.exists() || !source.exists() {
            continue;
        }
        fs::copy(&source, &alias)
            .with_context(|| format!("copy {} to {}", source.display(), alias.display()))?;
    }
    Ok(())
}

fn ensure_windows_zlib_static_alias(target: NativeTarget, prefix: &Path) -> Result<()> {
    if !target.is_windows() {
        return Ok(());
    }
    let lib_dir = prefix.join("lib");
    let import_lib = lib_dir.join("zlib.lib");
    if import_lib.exists() {
        return Ok(());
    }
    for source in ["zs.lib", "zlibs.lib", "zlibstatic.lib", "z.lib"] {
        let static_lib = lib_dir.join(source);
        if static_lib.exists() {
            fs::copy(&static_lib, &import_lib).with_context(|| {
                format!("copy {} to {}", static_lib.display(), import_lib.display())
            })?;
            break;
        }
    }
    Ok(())
}

fn ensure_windows_zlib_header_compat(target: NativeTarget, prefix: &Path) -> Result<()> {
    if !target.is_windows() {
        return Ok(());
    }
    let header = prefix.join("include").join("zconf.h");
    if !header.exists() {
        return Ok(());
    }
    let content =
        fs::read_to_string(&header).with_context(|| format!("read {}", header.display()))?;
    if content.contains("#if defined(HAVE_UNISTD_H) && !defined(_WIN32)") {
        return Ok(());
    }

    let updated = content.replace(
        "#ifdef HAVE_UNISTD_H    /* may be set to #if 1 by ./configure */",
        "#if defined(HAVE_UNISTD_H) && !defined(_WIN32)    /* may be set to #if 1 by ./configure */",
    );
    if updated == content {
        println!(
            "warning: zlib header compatibility patch not applied to {}",
            header.display()
        );
        return Ok(());
    }
    fs::write(&header, updated).with_context(|| format!("write {}", header.display()))?;
    Ok(())
}

fn ensure_pkg_config_shim(layout: &WorkspaceLayout) -> Result<PathBuf> {
    if !cfg!(windows) {
        return which("pkg-config").context("required pkg-config was not found in PATH");
    }
    let dir = layout.build_dir.join("pkg-config-shim");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let exe = env::current_exe().context("resolve current xtask executable")?;
    let shim = dir.join("pkg-config.cmd");
    let root_from_shim = windows_cmd_parent_traversal(&layout.root, &dir)?;
    let dist_from_root = windows_cmd_path_under_root(&layout.root, &layout.dist_dir)?;
    let exe_command = if let Ok(exe_from_root) = exe.strip_prefix(&layout.root) {
        format!("\"%ERIKA_ROOT%\\{}\"", windows_cmd_path(exe_from_root))
    } else {
        format!("\"{}\"", exe.display())
    };
    fs::write(
        &shim,
        format!(
            "@echo off\r\n\
             setlocal\r\n\
             for %%I in (\"%~dp0{}\") do set \"ERIKA_ROOT=%%~fI\"\r\n\
             set \"ERIKA_DIST_DIR=%ERIKA_ROOT%\\{}\"\r\n\
             set \"ERIKA_PKG_CONFIG_PATH=%ERIKA_DIST_DIR%\\dav1d\\lib\\pkgconfig;%ERIKA_DIST_DIR%\\ffmpeg\\lib\\pkgconfig;%ERIKA_DIST_DIR%\\freetype\\lib\\pkgconfig;%ERIKA_DIST_DIR%\\harfbuzz\\lib\\pkgconfig;%ERIKA_DIST_DIR%\\fribidi\\lib\\pkgconfig;%ERIKA_DIST_DIR%\\libass\\lib\\pkgconfig\"\r\n\
             if defined PKG_CONFIG_PATH (\r\n\
             \tset \"PKG_CONFIG_PATH=%ERIKA_PKG_CONFIG_PATH%;%PKG_CONFIG_PATH%\"\r\n\
             ) else (\r\n\
             \tset \"PKG_CONFIG_PATH=%ERIKA_PKG_CONFIG_PATH%\"\r\n\
             )\r\n\
             {} pkg-config-shim %*\r\n\
             exit /b %ERRORLEVEL%\r\n",
            root_from_shim, dist_from_root, exe_command
        ),
    )
    .with_context(|| format!("write {}", shim.display()))?;
    Ok(shim)
}

fn ensure_ffmpeg_pkg_config_shim(layout: &WorkspaceLayout) -> Result<PathBuf> {
    if !cfg!(windows) {
        return ensure_pkg_config_shim(layout);
    }
    // FFmpeg invokes pkg-config from a POSIX shell. Passing its version
    // operators through a .cmd wrapper makes cmd.exe interpret `>=` as
    // redirection. Keep the arguments in the shell until the native executable.
    let dir = layout.build_dir.join("pkg-config-shim");
    fs::create_dir_all(&dir)?;
    let shim = dir.join("pkg-config.sh");
    let exe = env::current_exe()?;
    fs::write(
        &shim,
        ffmpeg_pkg_config_shell_contents(&exe, &layout.dav1d_prefix.join("lib/pkgconfig")),
    )?;
    Ok(shim)
}

fn ffmpeg_pkg_config_shell_contents(exe: &Path, pkg_config_dir: &Path) -> String {
    format!(
        "#!/bin/sh\nexport PKG_CONFIG_PATH={}\nexec {} pkg-config-shim \"$@\"\n",
        shell_escape(&path_to_forward_slashes(pkg_config_dir)),
        shell_escape(&path_to_forward_slashes(exe)),
    )
}

fn windows_cmd_parent_traversal(root: &Path, dir: &Path) -> Result<String> {
    let rel = dir
        .strip_prefix(root)
        .with_context(|| format!("{} is not under {}", dir.display(), root.display()))?;
    let depth = rel.components().count();
    if depth == 0 {
        Ok(".".to_string())
    } else {
        Ok(std::iter::repeat_n("..", depth)
            .collect::<Vec<_>>()
            .join("\\"))
    }
}

fn windows_cmd_path_under_root(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("{} is not under {}", path.display(), root.display()))?;
    Ok(windows_cmd_path(rel))
}

fn windows_cmd_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("\\")
}

fn pkg_config_shim(args: Vec<String>) -> Result<()> {
    let query = PkgConfigQuery::parse(args);
    if query.version {
        println!("2.0.0-erika");
        return Ok(());
    }
    if query.packages.is_empty() {
        return Ok(());
    }

    let mut visited = HashSet::new();
    let mut output = Vec::new();
    for package in &query.packages {
        let pc = load_pc_file(package)?;
        if query.exists {
            continue;
        }
        if query.modversion {
            output.push(pc.value("Version"));
        }
        if let Some(variable) = &query.variable {
            output.push(pc.variable(variable));
        }
        if query.cflags {
            collect_pc_flags(
                &pc,
                PkgFlagKind::Cflags,
                query.static_link,
                query.msvc_syntax,
                &mut visited,
                &mut output,
            )?;
        }
        if query.libs {
            collect_pc_flags(
                &pc,
                PkgFlagKind::Libs,
                query.static_link,
                query.msvc_syntax,
                &mut visited,
                &mut output,
            )?;
        }
    }

    if !output.is_empty() {
        println!("{}", output.join(" "));
    }
    Ok(())
}

#[derive(Debug, Default)]
struct PkgConfigQuery {
    version: bool,
    exists: bool,
    modversion: bool,
    cflags: bool,
    libs: bool,
    static_link: bool,
    msvc_syntax: bool,
    variable: Option<String>,
    packages: Vec<String>,
}

impl PkgConfigQuery {
    fn parse(args: Vec<String>) -> Self {
        let mut query = Self::default();
        for arg in args {
            match arg.as_str() {
                "--version" => query.version = true,
                "--exists" => query.exists = true,
                "--modversion" => query.modversion = true,
                "--cflags" | "--cflags-only-I" | "--cflags-only-other" => query.cflags = true,
                "--libs" | "--libs-only-L" | "--libs-only-l" | "--libs-only-other" => {
                    query.libs = true
                }
                "--static" => query.static_link = true,
                "--msvc-syntax" => query.msvc_syntax = true,
                "--print-errors" | "--silence-errors" | "--short-errors" | "--errors-to-stdout" => {
                }
                _ if arg.starts_with("--variable=") => {
                    query.variable = Some(arg["--variable=".len()..].to_string());
                }
                _ if arg.starts_with("--") => {}
                ">" | ">=" | "=" | "<=" | "<" => {}
                value if looks_like_version(value) => {}
                value => query.packages.extend(
                    value
                        .split_whitespace()
                        .filter(|token| !matches!(*token, ">" | ">=" | "=" | "<=" | "<"))
                        .filter(|token| !looks_like_version(token))
                        .map(str::to_string),
                ),
            }
        }
        if !query.exists
            && !query.modversion
            && !query.cflags
            && !query.libs
            && query.variable.is_none()
            && !query.packages.is_empty()
        {
            query.cflags = true;
            query.libs = true;
        }
        query
    }
}

#[derive(Debug, Clone)]
struct PcFile {
    name: String,
    variables: HashMap<String, String>,
    fields: HashMap<String, String>,
}

impl PcFile {
    fn value(&self, key: &str) -> String {
        self.fields
            .get(key)
            .map(|value| substitute_pc_vars(value, &self.variables))
            .unwrap_or_default()
    }

    fn variable(&self, key: &str) -> String {
        self.variables.get(key).cloned().unwrap_or_default()
    }

    fn flag_tokens(&self, key: &str) -> Vec<String> {
        self.fields
            .get(key)
            .into_iter()
            .flat_map(|field| split_pc_field_tokens(field))
            .map(|token| unescape_pc_whitespace(&substitute_pc_vars(&token, &self.variables)))
            .filter(|token| !token.is_empty())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PkgFlagKind {
    Cflags,
    Libs,
}

fn collect_pc_flags(
    pc: &PcFile,
    kind: PkgFlagKind,
    static_link: bool,
    msvc_syntax: bool,
    visited: &mut HashSet<String>,
    output: &mut Vec<String>,
) -> Result<()> {
    let visit_key = format!("{}:{kind:?}", pc.name);
    if !visited.insert(visit_key) {
        return Ok(());
    }

    let mut fields = match kind {
        PkgFlagKind::Cflags => vec!["Cflags"],
        PkgFlagKind::Libs => vec!["Libs"],
    };
    if kind == PkgFlagKind::Libs && static_link {
        fields.push("Libs.private");
    }
    for field in fields {
        output.extend(
            pc.flag_tokens(field)
                .into_iter()
                .map(|token| format_pkg_config_token(token, msvc_syntax)),
        );
    }

    for required in pc_requirements(pc, static_link) {
        let required_pc = load_pc_file(&required)?;
        collect_pc_flags(
            &required_pc,
            kind,
            static_link,
            msvc_syntax,
            visited,
            output,
        )?;
    }
    Ok(())
}

fn pc_requirements(pc: &PcFile, static_link: bool) -> Vec<String> {
    let mut fields = vec![pc.value("Requires")];
    if static_link {
        fields.push(pc.value("Requires.private"));
    }
    fields
        .into_iter()
        .flat_map(|field| {
            field
                .split(',')
                .filter_map(|entry| entry.split_whitespace().next().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

fn load_pc_file(package: &str) -> Result<PcFile> {
    let pkg_config_path = env::var_os("PKG_CONFIG_PATH").context("PKG_CONFIG_PATH is not set")?;
    for dir in env::split_paths(&pkg_config_path) {
        let path = dir.join(format!("{package}.pc"));
        if path.exists() {
            return parse_pc_file(package, &path);
        }
    }
    bail!("pkg-config package `{package}` was not found")
}

fn parse_pc_file(package: &str, path: &Path) -> Result<PcFile> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut variables = HashMap::new();
    let mut fields = HashMap::new();
    variables.insert(
        "pcfiledir".to_string(),
        path.parent().unwrap_or(Path::new("")).display().to_string(),
    );
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let equals = line.find('=');
        let colon = line.find(':');
        if let Some(index) = colon.filter(|index| equals.is_none_or(|equals| *index < equals)) {
            let (key, value) = line.split_at(index);
            fields.insert(key.trim().to_string(), value[1..].trim().to_string());
        } else if let Some((key, value)) = line.split_once('=') {
            let value = unescape_pc_whitespace(&substitute_pc_vars(value.trim(), &variables));
            variables.insert(key.trim().to_string(), value);
        }
    }
    Ok(PcFile {
        name: package.to_string(),
        variables,
        fields,
    })
}

fn substitute_pc_vars(value: &str, variables: &HashMap<String, String>) -> String {
    let mut output = value.to_string();
    for _ in 0..8 {
        let Some(start) = output.find("${") else {
            break;
        };
        let Some(end) = output[start + 2..].find('}') else {
            break;
        };
        let end = start + 2 + end;
        let name = &output[start + 2..end];
        let replacement = variables.get(name).cloned().unwrap_or_default();
        output.replace_range(start..=end, &replacement);
    }
    output
}

fn split_pc_field_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn unescape_pc_whitespace(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            output.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            output.push(ch);
        }
    }
    if escaped {
        output.push('\\');
    }
    output
}

fn format_pkg_config_token(token: String, msvc_syntax: bool) -> String {
    let token = if let Some(path) = token.strip_prefix("-I") {
        let path = pkg_config_output_path(path);
        if msvc_syntax {
            format!("/I{path}")
        } else {
            format!("-I{path}")
        }
    } else if let Some(path) = token.strip_prefix("-L") {
        let path = pkg_config_output_path(path);
        if msvc_syntax {
            format!("/libpath:{path}")
        } else {
            format!("-L{path}")
        }
    } else if msvc_syntax {
        msvc_pkg_config_token(token)
    } else {
        token
    };
    escape_pkg_config_token(&token)
}

fn pkg_config_output_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    if let Some(relative) = relative_pkg_config_path(&path) {
        return relative;
    }
    path
}

fn relative_pkg_config_path(path: &str) -> Option<String> {
    let base = env::var_os("ERIKA_PKG_CONFIG_RELATIVE_BASE")?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return None;
    }
    relative_path(Path::new(&base), &path).map(|path| path_to_forward_slashes(&path))
}

fn relative_path(base: &Path, path: &Path) -> Option<PathBuf> {
    let base_components = base.components().collect::<Vec<_>>();
    let path_components = path.components().collect::<Vec<_>>();
    let mut common = 0;
    while common < base_components.len()
        && common < path_components.len()
        && windows_component_eq(base_components[common], path_components[common])
    {
        common += 1;
    }
    if common == 0 {
        return None;
    }
    let mut relative = PathBuf::new();
    for component in &base_components[common..] {
        if matches!(component, std::path::Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &path_components[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn windows_component_eq(left: std::path::Component<'_>, right: std::path::Component<'_>) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn escape_pkg_config_token(token: &str) -> String {
    token
        .chars()
        .flat_map(|ch| {
            if ch.is_whitespace() {
                vec!['\\', ch]
            } else {
                vec![ch]
            }
        })
        .collect()
}

fn msvc_pkg_config_token(token: String) -> String {
    if let Some(path) = token.strip_prefix("-I") {
        format!("/I{path}")
    } else if let Some(path) = token.strip_prefix("-L") {
        format!("/libpath:{path}")
    } else if let Some(name) = token.strip_prefix("-l") {
        format!("{name}.lib")
    } else {
        token
    }
}

fn looks_like_version(value: &str) -> bool {
    value.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

static WINDOWS_X64_MSVC_ENV: OnceLock<std::result::Result<Vec<(OsString, OsString)>, String>> =
    OnceLock::new();
static WINDOWS_ARM64_MSVC_ENV: OnceLock<std::result::Result<Vec<(OsString, OsString)>, String>> =
    OnceLock::new();

fn apply_windows_target_env(command: &mut Command, target: NativeTarget) -> Result<()> {
    if !target.is_windows() {
        return Ok(());
    }
    apply_msvc_environment(command, target)
}

fn apply_android_host_env(command: &mut Command, target: NativeTarget) -> Result<()> {
    if !cfg!(windows) || !target.is_android() {
        return Ok(());
    }
    apply_msvc_environment(command, windows_host_target())
}

fn windows_host_target() -> NativeTarget {
    if cfg!(all(windows, target_arch = "aarch64")) {
        NativeTarget::Aarch64WindowsMsvc
    } else {
        NativeTarget::X86_64WindowsMsvc
    }
}

fn windows_host_arch() -> &'static str {
    if matches!(windows_host_target(), NativeTarget::Aarch64WindowsMsvc) {
        "arm64"
    } else {
        "x64"
    }
}

fn apply_msvc_environment(command: &mut Command, target: NativeTarget) -> Result<()> {
    let existing_path = command_env_path(command);
    for (key, value) in windows_msvc_environment(target)? {
        command.env(key, value);
    }
    if let Some(existing_path) = existing_path {
        let existing_dirs = env::split_paths(&existing_path).collect::<Vec<_>>();
        // Keep VSDevCmd's PATH first so MSVC link.exe wins over POSIX tools such as MSYS link.exe.
        append_paths_to_command(command, existing_dirs.iter().map(PathBuf::as_path));
    }
    Ok(())
}

fn windows_msvc_environment(target: NativeTarget) -> Result<&'static [(OsString, OsString)]> {
    let cache = match target {
        NativeTarget::Aarch64WindowsMsvc => &WINDOWS_ARM64_MSVC_ENV,
        _ => &WINDOWS_X64_MSVC_ENV,
    };
    match cache.get_or_init(|| load_windows_msvc_environment(target).map_err(|e| e.to_string())) {
        Ok(values) => Ok(values.as_slice()),
        Err(message) => bail!("{message}"),
    }
}

fn load_windows_msvc_environment(target: NativeTarget) -> Result<Vec<(OsString, OsString)>> {
    let devcmd = vs_dev_cmd().context("Visual Studio Developer Command Prompt was not found")?;
    let arch = match target {
        NativeTarget::Aarch64WindowsMsvc => "arm64",
        _ => "x64",
    };
    let host_arch = windows_host_arch();
    let script_path = env::temp_dir().join(format!("erika-vsdevcmd-{arch}-env.cmd"));
    fs::write(
        &script_path,
        format!(
            "@echo off\r\ncall \"{}\" -arch={} -host_arch={} >nul\r\nset\r\n",
            devcmd.display(),
            arch,
            host_arch
        ),
    )
    .with_context(|| format!("write {}", script_path.display()))?;
    let output = Command::new("cmd.exe")
        .arg("/d")
        .arg("/c")
        .arg(&script_path)
        .output()
        .context("spawn Visual Studio Developer Command Prompt")?;
    let _ = fs::remove_file(&script_path);
    if !output.status.success() {
        bail!(
            "Visual Studio Developer Command Prompt failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut values = Vec::new();
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        values.push((OsString::from(key), OsString::from(value)));
    }
    if !values.iter().any(|(key, _)| {
        key.to_string_lossy()
            .eq_ignore_ascii_case("VCToolsInstallDir")
    }) {
        bail!("Visual Studio C++ tools are not installed in the Build Tools instance");
    }
    Ok(values)
}

fn msvc_tool_path(tools: &[(OsString, OsString)], executable: &str) -> Result<PathBuf> {
    let path = tools
        .iter()
        .find_map(|(key, value)| {
            key.to_string_lossy()
                .eq_ignore_ascii_case("PATH")
                .then_some(value)
        })
        .context("Visual Studio environment did not provide PATH")?;
    env::split_paths(path)
        .map(|dir| dir.join(executable))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("Visual Studio environment did not provide {executable}"))
}

fn vs_dev_cmd() -> Option<PathBuf> {
    let vswhere = which("vswhere")
        .or_else(|| {
            program_files_path(
                "ProgramFiles(x86)",
                "Microsoft Visual Studio/Installer/vswhere.exe",
            )
        })
        .or_else(|| {
            program_files_path(
                "ProgramFiles",
                "Microsoft Visual Studio/Installer/vswhere.exe",
            )
        })?;
    let output = Command::new(vswhere)
        .args(["-latest", "-products", "*", "-property", "installationPath"])
        .output()
        .ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            let devcmd = PathBuf::from(path).join("Common7/Tools/VsDevCmd.bat");
            if devcmd.exists() {
                return Some(devcmd);
            }
        }
    }
    ["Enterprise", "BuildTools", "Community", "Professional"]
        .into_iter()
        .flat_map(|edition| {
            ["ProgramFiles", "ProgramFiles(x86)"]
                .into_iter()
                .filter_map(move |variable| {
                    program_files_path(
                        variable,
                        &format!(
                            "Microsoft Visual Studio/2022/{edition}/Common7/Tools/VsDevCmd.bat"
                        ),
                    )
                })
        })
        .next()
}

fn program_files_path(variable: &str, relative: &str) -> Option<PathBuf> {
    env::var_os(variable)
        .map(PathBuf::from)
        .map(|root| root.join(relative))
        .filter(|path| path.is_file())
}

fn cmake_tool() -> Option<PathBuf> {
    which("cmake")
        .or_else(|| android_sdk_cmake_tool("cmake"))
        .or_else(|| {
            existing_path(
            "C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe",
        )
        })
}

fn ninja_tool() -> Option<PathBuf> {
    which("ninja")
        .or_else(|| android_sdk_cmake_tool("ninja"))
        .or_else(|| {
            existing_path(
            "C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja/ninja.exe",
        )
        })
}

fn android_sdk_cmake_tool(tool: &str) -> Option<PathBuf> {
    let mut sdk_roots = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if cfg!(windows) {
        if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
            sdk_roots.push(PathBuf::from(local_app_data).join("Android/Sdk"));
        }
    }
    for sdk_root in sdk_roots {
        let Ok(entries) = fs::read_dir(sdk_root.join("cmake")) else {
            continue;
        };
        let mut versions = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        versions.sort_by_key(|path| {
            path.file_name()
                .map(|name| android_ndk_version_key(&name.to_string_lossy()))
                .unwrap_or_default()
        });
        versions.reverse();
        for version in versions {
            let candidate = executable_in_dir(&version.join("bin"), tool);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn posix_shell() -> Option<PathBuf> {
    msys2_usr_bin()
        .map(|dir| dir.join("sh.exe"))
        .filter(|path| path.is_file())
        .or_else(|| which("sh"))
        .or_else(|| {
            which("bash").filter(|path| {
                !cfg!(windows)
                    || !path
                        .to_string_lossy()
                        .eq_ignore_ascii_case("C:\\Windows\\System32\\bash.exe")
            })
        })
        .or_else(|| {
            windows_posix_bin_dirs()
                .into_iter()
                .flat_map(|dir| [dir.join("sh.exe"), dir.join("bash.exe")])
                .find(|path| path.is_file())
        })
}

fn gnu_make() -> Option<PathBuf> {
    msys2_usr_bin()
        .map(|dir| dir.join("make.exe"))
        .filter(|path| path.is_file())
        .or_else(|| which("make"))
        .or_else(|| which("gmake"))
        .or_else(|| which("mingw32-make"))
        .or_else(|| {
            windows_posix_bin_dirs()
                .into_iter()
                .flat_map(|dir| [dir.join("make.exe"), dir.join("mingw32-make.exe")])
                .find(|path| path.is_file())
        })
        .or_else(android_ndk_make)
}

fn msys2_usr_bin() -> Option<PathBuf> {
    env::var_os("MSYS2_ROOT")
        .map(PathBuf::from)
        .map(|root| root.join("usr/bin"))
        .filter(|path| path.is_dir())
        .or_else(|| existing_path("C:/msys64/usr/bin"))
}

fn windows_posix_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(root) = env::var_os("MSYS2_ROOT") {
        let root = PathBuf::from(root);
        dirs.push(root.join("usr/bin"));
        dirs.push(root.join("ucrt64/bin"));
        dirs.push(root.join("mingw64/bin"));
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = env::var_os(variable) {
            let root = PathBuf::from(root);
            dirs.push(root.join("Git/usr/bin"));
            dirs.push(root.join("Git/bin"));
        }
    }
    dirs.extend([
        PathBuf::from("C:/msys64/usr/bin"),
        PathBuf::from("C:/msys64/ucrt64/bin"),
        PathBuf::from("C:/mingw64/bin"),
    ]);
    dirs
}

fn android_ndk_make() -> Option<PathBuf> {
    let ndk_root = android_ndk_root().ok()?;
    let host_tag = android_ndk_host_tag(&ndk_root).ok()?;
    for bin_dir in [
        ndk_root.join("prebuilt").join(&host_tag).join("bin"),
        ndk_root
            .join("toolchains/llvm/prebuilt")
            .join(&host_tag)
            .join("bin"),
    ] {
        let candidate = executable_in_dir(&bin_dir, "make");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn ffmpeg_make_command(
    make: &Path,
    build_dir: &Path,
    args: &[String],
    target: NativeTarget,
) -> Result<Command> {
    if uses_windows_posix_ffmpeg(target) {
        let shell = posix_shell().context("required POSIX shell was not found for FFmpeg make")?;
        let mut command = Command::new(shell);
        let make_line = std::iter::once(format!(
            "cd {}",
            shell_quote(&path_to_forward_slashes(build_dir))
        ))
        .chain(std::iter::once(
            std::iter::once(shell_quote(&path_to_forward_slashes(make)))
                .chain(args.iter().map(|arg| shell_quote(arg)))
                .collect::<Vec<_>>()
                .join(" "),
        ))
        .collect::<Vec<_>>()
        .join(" && ");
        command
            .arg("-lc")
            .arg(make_line)
            .env("MSYS2_PATH_TYPE", "inherit")
            .env("MSYS2_ARG_CONV_EXCL", "*");
        return Ok(command);
    }

    let mut command = Command::new(make);
    command.current_dir(build_dir).args(args);
    Ok(command)
}

fn python_tool() -> Option<PathBuf> {
    ["python3", "python", "py"]
        .into_iter()
        .filter_map(which)
        .find(|path| python_candidate_is_usable(path))
}

fn python_candidate_is_usable(path: &Path) -> bool {
    if cfg!(windows)
        && path
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("\\windowsapps\\")
    {
        return false;
    }
    Command::new(path)
        .arg("-c")
        .arg("import venv")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn append_windows_posix_paths(command: &mut Command) {
    if !cfg!(windows) {
        return;
    }
    if let Some(msys2_bin) = msys2_usr_bin() {
        prepend_path_to_command(command, &msys2_bin);
        return;
    }
    let dirs = windows_posix_bin_dirs();
    append_paths_to_command(command, dirs.iter().map(PathBuf::as_path));
}

fn prepend_path_to_command(command: &mut Command, dir: &Path) {
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(
        command_env_path(command)
            .or_else(|| env::var_os("PATH"))
            .map(|path| env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    command.env(
        "PATH",
        env::join_paths(paths).expect("PATH entries are valid"),
    );
}

fn apply_windows_posix_shell(command: &mut Command, target: NativeTarget) {
    if !uses_windows_posix_ffmpeg(target) {
        return;
    }
    let Some(shell) = posix_shell() else {
        return;
    };
    command.env("CONFIG_SHELL", &shell);
    command.env("SHELL", &shell);
    command.env("MAKESHELL", &shell);
}

fn uses_windows_posix_ffmpeg(target: NativeTarget) -> bool {
    cfg!(windows) && (target.is_windows() || target.is_android())
}

fn host_c_compiler() -> Result<PathBuf> {
    host_compiler("CC_FOR_BUILD", &["cc", "clang", "gcc", "cl"])
        .context("a host C compiler is required for Meson build-machine generators")
}

fn host_cxx_compiler() -> Result<PathBuf> {
    host_compiler("CXX_FOR_BUILD", &["c++", "clang++", "g++", "cl"])
        .context("a host C++ compiler is required for Meson build-machine generators")
}

fn host_compiler(variable: &str, candidates: &[&str]) -> Option<PathBuf> {
    env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            if cfg!(windows) && candidates.contains(&"cl") {
                windows_host_cl_compiler()
            } else {
                None
            }
        })
        .or_else(|| candidates.iter().find_map(|candidate| which(candidate)))
}

fn windows_host_cl_compiler() -> Option<PathBuf> {
    let tools = windows_msvc_environment(windows_host_target()).ok()?;
    msvc_tool_path(tools, "cl.exe").ok()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn append_paths_to_command<'a>(command: &mut Command, dirs: impl IntoIterator<Item = &'a Path>) {
    let mut paths = command_env_path(command)
        .or_else(|| env::var_os("PATH"))
        .map(|base_path| env::split_paths(&base_path).collect::<Vec<_>>())
        .unwrap_or_default();
    paths.extend(
        dirs.into_iter()
            .filter(|path| path.exists())
            .map(Path::to_path_buf),
    );
    if !paths.is_empty() {
        command.env(
            "PATH",
            env::join_paths(paths).expect("PATH entries are valid"),
        );
    }
}

fn command_env_path(command: &Command) -> Option<OsString> {
    command.get_envs().find_map(|(key, value)| {
        if key.to_string_lossy().eq_ignore_ascii_case("PATH") {
            value.map(OsString::from)
        } else {
            None
        }
    })
}

fn venv_bin_dir(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts")
    } else {
        venv.join("bin")
    }
}

fn executable_in_dir(dir: &Path, name: &str) -> PathBuf {
    if cfg!(windows) {
        for extension in ["exe", "cmd", "bat"] {
            let candidate = dir.join(format!("{name}.{extension}"));
            if candidate.exists() {
                return candidate;
            }
        }
    }
    dir.join(name)
}

fn existing_path(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.exists().then_some(path)
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination).with_context(|| format!("create {}", destination.display()))?;
    for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_all(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "copy {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn which(tool: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return Some(candidate);
        }
        if cfg!(windows) && Path::new(tool).extension().is_none() {
            for extension in ["exe", "cmd", "bat"] {
                let candidate = dir.join(format!("{tool}.{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn run(command: &mut Command) -> Result<()> {
    let display = command_display(command);
    let status = command
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("spawn {display}"))?;
    if !status.success() {
        bail!("command failed ({status}): {display}");
    }
    Ok(())
}

fn xcrun(sdk: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("xcrun")
        .arg("--sdk")
        .arg(sdk)
        .args(args)
        .output()
        .with_context(|| format!("spawn xcrun --sdk {sdk} {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "command failed ({}): xcrun --sdk {sdk} {}",
            output.status,
            args.join(" ")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn command_display(command: &Command) -> String {
    let mut parts = Vec::new();
    parts.push(command.get_program().to_string_lossy().into_owned());
    parts.extend(
        command
            .get_args()
            .map(OsStr::to_string_lossy)
            .map(String::from),
    );
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_child_commands_remove_inherited_deployment_targets() {
        let mut command = Command::new("tool");
        command
            .env("MACOSX_DEPLOYMENT_TARGET", "11.0")
            .env("IPHONEOS_DEPLOYMENT_TARGET", "13.0")
            .env("TVOS_DEPLOYMENT_TARGET", "13.0");

        clear_apple_deployment_target_env(&mut command);

        for name in [
            "MACOSX_DEPLOYMENT_TARGET",
            "IPHONEOS_DEPLOYMENT_TARGET",
            "TVOS_DEPLOYMENT_TARGET",
        ] {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| { key == OsStr::new(name) && value.is_none() })
            );
        }
    }

    #[test]
    fn windows_targets_map_to_rust_and_native_architectures() {
        let cases = [
            (
                "x86_64-pc-windows-msvc",
                NativeTarget::X86_64WindowsMsvc,
                "x86_64",
                "x86_64",
            ),
            (
                "aarch64-pc-windows-msvc",
                NativeTarget::Aarch64WindowsMsvc,
                "aarch64",
                "arm64",
            ),
        ];
        for (triple, expected, ffmpeg_arch, meson_cpu) in cases {
            let target = NativeTarget::parse(triple).unwrap();
            assert_eq!(target, expected);
            assert!(target.is_windows());
            assert_eq!(target.triple(), Some(triple));
            assert_eq!(target.ffmpeg_arch(), Some(ffmpeg_arch));
            assert_eq!(target.meson_cpu(), Some(meson_cpu));
        }
        assert_eq!(
            NativeTarget::parse("windows-arm64").unwrap(),
            NativeTarget::Aarch64WindowsMsvc
        );
    }

    #[test]
    fn tvos_targets_map_to_rust_sdks_and_native_architectures() {
        let cases = [
            (
                "aarch64-apple-tvos",
                NativeTarget::Aarch64Tvos,
                "appletvos",
                "arm64",
                "arm64",
            ),
            (
                "aarch64-apple-tvos-sim",
                NativeTarget::Aarch64TvosSimulator,
                "appletvsimulator",
                "arm64",
                "arm64",
            ),
            (
                "x86_64-apple-tvos",
                NativeTarget::X86_64TvosSimulator,
                "appletvsimulator",
                "x86_64",
                "x86_64",
            ),
        ];

        for (triple, expected, sdk, ffmpeg_arch, meson_cpu) in cases {
            let target = NativeTarget::parse(triple).unwrap();
            assert_eq!(target, expected);
            assert!(target.is_tvos());
            assert!(target.is_apple());
            assert_eq!(target.triple(), Some(triple));
            assert_eq!(target.sdk(), Some(sdk));
            assert_eq!(target.ffmpeg_arch(), Some(ffmpeg_arch));
            assert_eq!(target.meson_cpu(), Some(meson_cpu));
        }
    }

    #[test]
    fn x86_64_targets_keep_ffmpeg_assembly_enabled() {
        for target in [
            NativeTarget::X86_64Macos,
            NativeTarget::X86_64IosSimulator,
            NativeTarget::X86_64TvosSimulator,
            NativeTarget::X86_64WindowsMsvc,
            NativeTarget::X86_64Android,
        ] {
            let flags = NativeDependencyProfile::Lgpl.ffmpeg_configure_flags_for_target(target);
            assert!(!flags.contains(&"--disable-x86asm"));
            assert!(!flags.contains(&"--disable-asm"));
            assert!(ffmpeg_requires_nasm(target));
        }
        assert!(!ffmpeg_requires_nasm(NativeTarget::Aarch64WindowsMsvc));
    }

    #[test]
    fn ohos_dav1d_cross_file_uses_target_compilers_and_sysroot() {
        let root = PathBuf::from("/sdk with spaces/native");
        let bin = root.join("llvm/bin");
        let config = OhosToolchain {
            native_root: root.clone(),
            bin_dir: bin.clone(),
            sysroot: root.join("sysroot"),
            cmake_toolchain_file: root.join("build/cmake/ohos.toolchain.cmake"),
            clang: bin.join("aarch64-unknown-linux-ohos-clang"),
            clangxx: bin.join("aarch64-unknown-linux-ohos-clang++"),
            ar: bin.join("llvm-ar"),
            ranlib: bin.join("llvm-ranlib"),
            strip: bin.join("llvm-strip"),
            nm: bin.join("llvm-nm"),
            compatible_sdk_version: "18".to_string(),
            triple: "aarch64-unknown-linux-ohos",
            arch: "aarch64",
            abi: "arm64-v8a",
        };
        let content = ohos_meson_cross_contents(&config, Path::new("/usr/bin/pkg-config"));
        assert!(content.contains(&format!(
            "c = {}",
            meson_string(&config.clang.display().to_string())
        )));
        assert!(content.contains(&format!(
            "cpp = {}",
            meson_string(&config.clangxx.display().to_string())
        )));
        let sysroot_flag = meson_string(&format!("--sysroot={}", config.sysroot.display()));
        assert_eq!(content.matches(&sysroot_flag).count(), 4);
        assert_eq!(
            content
                .matches("'--target=aarch64-unknown-linux-ohos'")
                .count(),
            4
        );
        assert_eq!(content.matches("'-fPIC'").count(), 4);
        assert!(content.contains("system = 'linux'"));
        assert!(content.contains("cpu_family = 'aarch64'"));
        assert!(content.contains("needs_exe_wrapper = true"));
    }

    #[test]
    fn android_targets_map_to_rust_abi_and_clang() {
        let cases = [
            (
                "aarch64-linux-android",
                "arm64-v8a",
                "aarch64-linux-android",
            ),
            (
                "armv7-linux-androideabi",
                "armeabi-v7a",
                "armv7a-linux-androideabi",
            ),
            ("x86_64-linux-android", "x86_64", "x86_64-linux-android"),
            ("i686-linux-android", "x86", "i686-linux-android"),
        ];
        for (triple, abi, clang) in cases {
            let target = NativeTarget::parse(triple).unwrap();
            assert!(target.is_android());
            assert_eq!(target.triple(), Some(triple));
            assert_eq!(target.android_abi(), Some(abi));
            assert_eq!(target.android_clang_triple(), Some(clang));
        }
    }

    #[test]
    fn every_platform_includes_windows_media_and_av1_software_decoders() {
        for profile in [
            NativeDependencyProfile::Lgpl,
            NativeDependencyProfile::GplFull,
        ] {
            for target in [
                NativeTarget::Host,
                NativeTarget::Aarch64Macos,
                NativeTarget::X86_64Macos,
                NativeTarget::Aarch64Ios,
                NativeTarget::Aarch64Tvos,
                NativeTarget::X86_64WindowsMsvc,
                NativeTarget::Aarch64WindowsMsvc,
                NativeTarget::Aarch64Android,
                NativeTarget::Armv7Android,
                NativeTarget::X86_64Android,
                NativeTarget::I686Android,
                NativeTarget::Aarch64Ohos,
                NativeTarget::X86_64Ohos,
            ] {
                let flags = profile.ffmpeg_configure_flags_for_target(target);
                let enabled = |kind: &str, name: &str| {
                    flags
                        .iter()
                        .filter_map(|flag| flag.strip_prefix(kind))
                        .flat_map(|names| names.split(','))
                        .any(|candidate| candidate == name)
                };
                assert!(enabled("--enable-demuxer=", "asf"), "{target:?}");
                assert!(flags.contains(&"--enable-libdav1d"), "{target:?}");
                assert!(enabled("--enable-decoder=", "libdav1d"), "{target:?}");
                for decoder in [
                    "wmv1",
                    "wmv2",
                    "wmv3",
                    "vc1",
                    "wmav1",
                    "wmav2",
                    "wmapro",
                    "wmalossless",
                ] {
                    assert!(
                        enabled("--enable-decoder=", decoder),
                        "{target:?}: {decoder}"
                    );
                }
            }
        }
    }

    #[test]
    fn android_ffmpeg_plan_enables_mediacodec_without_videotoolbox() {
        for profile in [
            NativeDependencyProfile::Lgpl,
            NativeDependencyProfile::GplFull,
        ] {
            let flags = profile.ffmpeg_configure_flags_for_target(NativeTarget::X86_64Android);
            assert!(flags.contains(&"--enable-jni"));
            assert!(flags.contains(&"--enable-mediacodec"));
            assert!(flags.contains(&"--enable-libdav1d"));
            assert!(flags.iter().any(|flag| {
                flag.contains("h264_mediacodec")
                    && flag.contains("vp8_mediacodec")
                    && flag.contains("av1_mediacodec")
                    && flag.contains("libdav1d")
            }));
            assert!(flags.iter().any(|flag| {
                flag.strip_prefix("--enable-decoder=")
                    .is_some_and(|decoders| decoders.split(',').any(|decoder| decoder == "vp8"))
            }));
            assert!(!flags.contains(&"--enable-videotoolbox"));
        }
    }

    #[test]
    fn every_ffmpeg_profile_enables_headless_gif_export() {
        for profile in [
            NativeDependencyProfile::Lgpl,
            NativeDependencyProfile::GplFull,
        ] {
            let flags = profile.ffmpeg_configure_flags();
            assert!(flags.contains(&"--enable-encoder=gif"));
            assert!(flags.contains(&"--enable-muxer=gif"));
        }
    }

    #[test]
    fn apple_ffmpeg_plan_enables_videotoolbox_with_dav1d_fallback() {
        for target in [
            NativeTarget::Aarch64Macos,
            NativeTarget::Aarch64Ios,
            NativeTarget::Aarch64Tvos,
        ] {
            let flags = NativeDependencyProfile::Lgpl.ffmpeg_configure_flags_for_target(target);
            assert!(flags.contains(&"--enable-videotoolbox"));
            assert!(flags.contains(&"--enable-libdav1d"));
            assert!(flags.contains(&"--enable-decoder=libdav1d"));
        }
    }

    #[test]
    fn android_i686_ffmpeg_disables_non_pic_x86_assembly() {
        for profile in [
            NativeDependencyProfile::Lgpl,
            NativeDependencyProfile::GplFull,
        ] {
            let i686 = profile.ffmpeg_configure_flags_for_target(NativeTarget::I686Android);
            assert!(i686.contains(&"--disable-asm"));
            assert!(!dav1d_asm_enabled(NativeTarget::I686Android));

            let x86_64 = profile.ffmpeg_configure_flags_for_target(NativeTarget::X86_64Android);
            assert!(!x86_64.contains(&"--disable-asm"));
            assert!(dav1d_asm_enabled(NativeTarget::X86_64Android));
        }
    }

    #[test]
    fn x86_64_dav1d_targets_require_nasm() {
        assert!(dav1d_requires_nasm(NativeTarget::X86_64Macos));
        assert!(dav1d_requires_nasm(NativeTarget::X86_64IosSimulator));
        assert!(dav1d_requires_nasm(NativeTarget::X86_64TvosSimulator));
        assert!(dav1d_requires_nasm(NativeTarget::X86_64Android));
        assert!(dav1d_requires_nasm(NativeTarget::X86_64WindowsMsvc));
        assert!(!dav1d_requires_nasm(NativeTarget::Aarch64Macos));
        assert!(!dav1d_requires_nasm(NativeTarget::Aarch64Ios));
    }

    #[test]
    fn windows_arm64_ffmpeg_commands_include_gas_preprocessor_path() {
        let tools_dir = env::temp_dir().join("erika-xtask-gas-preprocessor-path-test");
        let existing_dir = env::temp_dir().join("erika-xtask-existing-path-test");
        let mut command = Command::new("tool");
        command.env("PATH", env::join_paths([&existing_dir]).unwrap());

        apply_ffmpeg_assembler_path(
            &mut command,
            NativeTarget::Aarch64WindowsMsvc,
            Some(&tools_dir),
        );

        let merged = command_env_path(&command).unwrap();
        let paths = env::split_paths(&merged).collect::<Vec<_>>();
        assert_eq!(paths, [tools_dir, existing_dir]);
    }

    #[test]
    fn dav1d_assembler_path_preserves_msvc_linker_precedence() {
        let root = env::temp_dir();
        let msvc = root.join("Visual Studio/MSVC/bin");
        let git = root.join("Git/usr/bin");
        let gaspp = root.join("gas-preprocessor");
        let mut command = Command::new("meson");
        command.env("PATH", env::join_paths([&msvc, &git]).unwrap());
        prepend_path_to_command(&mut command, &gaspp);
        assert_eq!(
            env::split_paths(&command_env_path(&command).unwrap()).collect::<Vec<_>>(),
            [gaspp, msvc, git]
        );
    }

    #[test]
    fn ffmpeg_pkg_config_accepts_split_and_quoted_version_requirements() {
        for args in [
            vec!["--exists", "--print-errors", "dav1d", ">=", "1.0.0"],
            vec!["--exists", "--print-errors", "dav1d >= 1.0.0"],
        ] {
            let query = PkgConfigQuery::parse(args.into_iter().map(str::to_string).collect());
            assert!(query.exists);
            assert_eq!(query.packages, ["dav1d"]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn ffmpeg_shell_shim_preserves_version_operator_arguments() {
        let root = env::temp_dir().join(format!("erika pkg-config {}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let exe = root.join("capture args");
        fs::write(&exe, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new("sh")
            .arg("-c")
            .arg(ffmpeg_pkg_config_shell_contents(&exe, &root))
            .args(["pkg-config", "--exists", "dav1d", ">=", "1.0.0"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "pkg-config-shim\n--exists\ndav1d\n>=\n1.0.0\n"
        );
        assert!(!root.join("=1.0.0").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gas_preprocessor_patch_adds_armasm64_tbnz_support() {
        let path = env::temp_dir().join(format!(
            "erika-gas-preprocessor-patch-{}.pl",
            std::process::id()
        ));
        fs::write(
            &path,
            "elsif ($line =~ /(cbn?z|adr|tbz)/) {}\nif ($instr eq \"tbz\" and $reg =~ /w\\d+/) {}\n",
        )
        .unwrap();

        patch_gas_preprocessor_for_armasm64(&path).unwrap();

        let patched = fs::read_to_string(&path).unwrap();
        assert!(patched.contains("(cbn?z|adr|tbn?z)"));
        assert!(patched.contains("if ($instr =~ /^tbn?z$/ and"));
        assert!(!patched.contains("(cbn?z|adr|tbz)"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn msvc_archive_response_file_is_written_by_make() {
        let build_dir = env::temp_dir().join(format!(
            "erika-ffmpeg-msvc-response-file-{}",
            std::process::id()
        ));
        let makefile = build_dir.join("ffbuild/library.mak");
        fs::create_dir_all(makefile.parent().unwrap()).unwrap();
        fs::write(
            &makefile,
            "ifeq ($(RESPONSE_FILES),yes)\n\t$(Q)echo $^ > $@.objs\n\t$(AR) $(ARFLAGS) $(AR_O) @$@.objs\nendif\n",
        )
        .unwrap();

        fix_ffmpeg_msvc_archive_response_file_creation(&build_dir).unwrap();

        let patched = fs::read_to_string(&makefile).unwrap();
        assert!(patched.contains("\t$(file >$@.objs,$^)"));
        assert!(!patched.contains("\t$(Q)echo $^ > $@.objs"));
        fs::remove_dir_all(build_dir).unwrap();
    }

    #[test]
    fn dependency_markers_require_current_versions() {
        let path = std::env::temp_dir().join(format!("erika-xtask-marker-{}", std::process::id()));
        fs::write(&path, "zlib=1.3.1\ndeployment_target=n/a\nprefix=/tmp\n").unwrap();

        assert!(!marker_has_version(
            &path,
            "zlib",
            "1.3.2",
            NativeTarget::Host
        ));
        assert!(marker_has_version(
            &path,
            "zlib",
            "1.3.1",
            NativeTarget::Host
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn windows_ffmpeg_markers_require_dynamic_crt() {
        for target in [
            NativeTarget::X86_64WindowsMsvc,
            NativeTarget::Aarch64WindowsMsvc,
        ] {
            for profile in [
                NativeDependencyProfile::Lgpl,
                NativeDependencyProfile::GplFull,
            ] {
                let flags = profile.ffmpeg_configure_flags_for_target(target);
                assert!(flags.contains(&"--extra-cflags=-MD"));
                let current = format!("flags={}\n", flags.join(" "));
                assert!(ffmpeg_build_marker_has_current_flags(
                    &current,
                    profile,
                    target,
                    WindowsCrt::Dynamic
                ));
                let stale = current.replace(" --extra-cflags=-MD", "");
                assert!(!ffmpeg_build_marker_has_current_flags(
                    &stale,
                    profile,
                    target,
                    WindowsCrt::Dynamic
                ));
                // A static runtime build is a different build.
                assert!(!ffmpeg_build_marker_has_current_flags(
                    &current,
                    profile,
                    target,
                    WindowsCrt::Static
                ));
            }
        }
    }

    #[test]
    fn windows_static_crt_builds_ffmpeg_with_mt_in_its_own_directory() {
        let target = NativeTarget::X86_64WindowsMsvc;
        let flags =
            NativeDependencyProfile::Lgpl.ffmpeg_configure_flags_for(target, WindowsCrt::Static);
        assert!(flags.contains(&"--extra-cflags=-MT"));
        assert!(!flags.contains(&"--extra-cflags=-MD"));
        let layout =
            workspace_layout(NativeDependencyProfile::Lgpl, target, WindowsCrt::Static).unwrap();
        assert!(
            layout
                .dist_dir
                .ends_with("x86_64-pc-windows-msvc/lgpl-crt-static")
        );
        let options = DepsOptions::parse(&[
            "--target".into(),
            "x86_64-pc-windows-msvc".into(),
            "--crt".into(),
            "static".into(),
        ])
        .unwrap();
        assert_eq!(options.crt, WindowsCrt::Static);
        assert!(
            DepsOptions::parse(&[
                "--crt".into(),
                "static".into(),
                "--target".into(),
                "aarch64-apple-darwin".into()
            ])
            .is_err()
        );
    }

    #[test]
    fn ffmpeg_marker_invalidates_when_android_decoder_flags_change() {
        let profile = NativeDependencyProfile::Lgpl;
        let target = NativeTarget::X86_64Android;
        let current = format!(
            "flags={}\n",
            profile.ffmpeg_configure_flags_for_target(target).join(" ")
        );
        assert!(ffmpeg_build_marker_has_current_flags(
            &current,
            profile,
            target,
            WindowsCrt::Dynamic
        ));

        let stale = current.replace(",vp8,vp9", ",vp9");
        assert!(!ffmpeg_build_marker_has_current_flags(
            &stale,
            profile,
            target,
            WindowsCrt::Dynamic
        ));
    }

    #[test]
    fn ffmpeg_markers_require_current_patchset_revision() {
        let patchset = "erika-android-mediacodec-v1-deadbeef";
        assert!(!ffmpeg_build_marker_has_current_patchset(
            &format!("ffmpeg={FFMPEG_VERSION}\n"),
            patchset
        ));
        assert!(!ffmpeg_build_marker_has_current_patchset(
            &format!("ffmpeg={FFMPEG_VERSION}\npatchset=stale\n"),
            patchset
        ));
        assert!(ffmpeg_build_marker_has_current_patchset(
            &format!("ffmpeg={FFMPEG_VERSION}\npatchset={patchset}\n"),
            patchset
        ));
    }

    #[test]
    fn unified_patch_application_is_idempotent() {
        let patch = concat!(
            "--- a/example.c\n",
            "+++ b/example.c\n",
            "@@ -2,2 +2,3 @@\n",
            " beta\n",
            "+bounded\n",
            " gamma\n",
        );
        let parsed = parse_unified_patch(patch).unwrap();
        assert_eq!(parsed.len(), 1);

        let original = "alpha\nbeta\ngamma\n";
        let (patched, changed) = apply_patch_hunks(original, &parsed[0].hunks).unwrap();
        assert!(changed);
        assert_eq!(patched, "alpha\nbeta\nbounded\ngamma\n");

        let (reapplied, changed) = apply_patch_hunks(&patched, &parsed[0].hunks).unwrap();
        assert!(!changed);
        assert_eq!(reapplied, patched);
    }

    #[test]
    fn ffmpeg_patch_is_opt_in_and_uses_zero_timeout_dequeues() {
        let root = workspace_root().unwrap();
        let patch = fs::read_to_string(root.join(FFMPEG_PATCHES[0])).unwrap();
        assert_eq!(parse_unified_patch(&patch).unwrap().len(), 3);
        assert!(patch.contains("\"erika_nonblocking\""));
        assert!(patch.contains("OFFSET(erika_nonblocking), AV_OPT_TYPE_BOOL, {.i64 = 0}"));
        assert!(patch.contains("s->ctx->erika_nonblocking = s->erika_nonblocking"));
        assert!(patch.contains("wait && !s->erika_nonblocking"));
        assert!(patch.contains("&null_pkt, !s->erika_nonblocking"));
        assert_eq!(patch.matches("frame, !s->erika_nonblocking").count(), 3);
        assert!(patch.contains("if (s->erika_nonblocking)"));
        assert!(patch.contains("output_dequeue_timeout_us = 0"));
        assert!(patch.contains("s->draining && !s->erika_nonblocking"));
        assert!(patch.contains("return AVERROR(EAGAIN);"));
        assert!(patch.contains("Erika nonblocking MediaCodec receive: yielding"));
        assert!(patch.contains("EAGAIN after zero-timeout input/output starvation"));
        assert!(patch.contains("Erika nonblocking MediaCodec drain: yielding EAGAIN"));
    }

    #[test]
    fn android_ffmpeg_flags_require_dav1d_for_av1_fallback() {
        for target in [
            NativeTarget::Aarch64Android,
            NativeTarget::Armv7Android,
            NativeTarget::X86_64Android,
            NativeTarget::I686Android,
        ] {
            let flags = NativeDependencyProfile::Lgpl.ffmpeg_configure_flags_for_target(target);
            let decoders = flags
                .iter()
                .filter_map(|flag| flag.strip_prefix("--enable-decoder="))
                .flat_map(|value| value.split(','))
                .collect::<HashSet<_>>();
            assert!(decoders.contains("av1_mediacodec"));
            assert!(decoders.contains("libdav1d"));
            assert!(flags.contains(&"--enable-libdav1d"));
        }
    }

    #[test]
    fn android_ndk_versions_sort_numerically() {
        let mut versions = ["9.0.0", "29.0.14206865", "27.2.12479018"];
        versions.sort_by_key(|value| android_ndk_version_key(value));
        assert_eq!(versions, ["9.0.0", "27.2.12479018", "29.0.14206865"]);
    }

    #[cfg(windows)]
    #[test]
    fn appending_paths_keeps_existing_command_path_first() {
        let temp = env::temp_dir().join("erika-xtask-path-order-test");
        let vs_bin = temp.join("VS/VC/bin");
        let system_bin = temp.join("Windows/System32");
        let msys_bin = temp.join("msys64/usr/bin");
        fs::create_dir_all(&vs_bin).unwrap();
        fs::create_dir_all(&system_bin).unwrap();
        fs::create_dir_all(&msys_bin).unwrap();

        let mut command = Command::new("tool");
        let vs_path = env::join_paths([&vs_bin, &system_bin]).unwrap();
        command.env("PATH", vs_path);

        append_paths_to_command(&mut command, [msys_bin.as_path()]);

        let merged = command_env_path(&command).unwrap();
        let paths = env::split_paths(&merged).collect::<Vec<_>>();
        assert_eq!(paths[0], vs_bin);
        assert_eq!(paths[1], system_bin);
        assert!(paths.iter().any(|path| path == &msys_bin));
    }

    #[cfg(windows)]
    #[test]
    fn prepending_path_places_posix_tools_before_existing_entries() {
        let temp = env::temp_dir().join("erika-xtask-posix-path-order-test");
        let msys_bin = temp.join("msys64/usr/bin");
        let mingw_bin = temp.join("mingw64/bin");
        fs::create_dir_all(&msys_bin).unwrap();
        fs::create_dir_all(&mingw_bin).unwrap();

        let mut command = Command::new("tool");
        command.env("PATH", env::join_paths([&mingw_bin]).unwrap());
        prepend_path_to_command(&mut command, &msys_bin);

        let merged = command_env_path(&command).unwrap();
        let paths = env::split_paths(&merged).collect::<Vec<_>>();
        assert_eq!(paths[0], msys_bin);
        assert_eq!(paths[1], mingw_bin);
    }
}

fn print_help() {
    println!("Erika xtask");
    println!("  cargo run -p xtask -- web build [--skip-native]");
    println!("  cargo run -p xtask -- deps plan --profile lgpl");
    println!("  cargo run -p xtask -- deps fetch --profile lgpl [--all]");
    println!("  cargo run -p xtask -- deps status --profile lgpl");
    println!(
        "  cargo run -p xtask -- deps build --profile lgpl [--target host|aarch64-apple-darwin|x86_64-apple-darwin|aarch64-apple-ios|aarch64-apple-ios-sim|x86_64-apple-ios|aarch64-apple-tvos|aarch64-apple-tvos-sim|x86_64-apple-tvos|x86_64-pc-windows-msvc|aarch64-pc-windows-msvc|aarch64-linux-android|armv7-linux-androideabi|x86_64-linux-android|i686-linux-android|aarch64-unknown-linux-ohos|x86_64-unknown-linux-ohos] [--force] [--jobs N]"
    );
    println!("  cargo run -p xtask -- check cargo-patches");
    println!("  cargo run -p xtask -- check license");
}
