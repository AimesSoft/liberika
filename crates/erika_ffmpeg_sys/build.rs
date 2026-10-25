use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=ERIKA_NATIVE_PROFILE");
    println!("cargo:rerun-if-env-changed=ERIKA_NATIVE_TARGET");
    println!("cargo:rerun-if-env-changed=ERIKA_FFMPEG_DIR");
    println!("cargo:rerun-if-env-changed=ERIKA_ZLIB_DIR");
    println!("cargo:rerun-if-env-changed=ERIKA_DAV1D_DIR");
    println!("cargo:rerun-if-env-changed=ERIKA_ALLOW_LEGACY_FFMPEG");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");
    println!("cargo:rerun-if-env-changed=ANDROID_API_LEVEL");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_ROOT");
    println!("cargo:rerun-if-env-changed=ANDROID_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_SDK_ROOT");
    println!("cargo:rerun-if-env-changed=OHOS_NDK_HOME");
    println!("cargo:rerun-if-env-changed=OHOS_SDK_NATIVE");
    println!("cargo:rerun-if-env-changed=TVOS_DEPLOYMENT_TARGET");
    println!("cargo:rerun-if-env-changed=ERIKA_USE_SYSTEM_LIBS");

    let include_dirs = if use_system_libraries() {
        configure_system_ffmpeg()
    } else {
        configure_bundled_ffmpeg()
    };
    generate_bindings(&include_dirs);
}

fn use_system_libraries() -> bool {
    env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("ohos")
        && env::var("ERIKA_USE_SYSTEM_LIBS").as_deref() != Ok("0")
        && env::var_os("ERIKA_FFMPEG_DIR").is_none()
}

fn configure_system_ffmpeg() -> Vec<PathBuf> {
    let mut include_dirs = Vec::new();
    for (library, version) in [
        ("libavdevice", "62"),
        ("libavfilter", "11"),
        ("libavformat", "62"),
        ("libavcodec", "62"),
        ("libswresample", "6"),
        ("libswscale", "9"),
        ("libavutil", "60"),
    ] {
        let dependency = pkg_config::Config::new()
            .atleast_version(version)
            .statik(false)
            .probe(library)
            .unwrap_or_else(|error| {
                panic!("Linux requires FFmpeg 8 development libraries ({library} >= {version}). Install the libav*-dev/libsw*-dev packages or set ERIKA_USE_SYSTEM_LIBS=0 for a bundled build: {error}")
            });
        for path in dependency.include_paths {
            if !include_dirs.contains(&path) {
                include_dirs.push(path);
            }
        }
    }
    let include_dir = include_dirs
        .iter()
        .find(|path| path.join("libavutil/version.h").is_file())
        .expect("pkg-config did not provide the FFmpeg header directory");
    emit_ffmpeg_version_cfg(include_dir);
    include_dirs
}

fn configure_bundled_ffmpeg() -> Vec<PathBuf> {
    let dist_dir = ffmpeg_dist_dir();
    let zlib_dir = native_dep_dir("ERIKA_ZLIB_DIR", "zlib");
    let dav1d_dir = native_dep_dir("ERIKA_DAV1D_DIR", "dav1d");
    let include_dir = dist_dir.join("include");
    let lib_dir = dist_dir.join("lib");

    if !include_dir.join("libavformat/avformat.h").exists() {
        panic!(
            "FFmpeg headers were not found at {}. Run `{}` first, or set ERIKA_FFMPEG_DIR.",
            include_dir.display(),
            xtask_build_hint()
        );
    }

    if matches!(
        env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("android" | "macos" | "ios" | "tvos")
    ) {
        for archive in [
            "libavdevice.a",
            "libavfilter.a",
            "libavformat.a",
            "libavcodec.a",
            "libswresample.a",
            "libswscale.a",
            "libavutil.a",
        ] {
            let archive = lib_dir.join(archive);
            println!("cargo:rerun-if-changed={}", archive.display());
            if !archive.is_file() {
                panic!(
                    "Android FFmpeg archive was not found at {}. Run `{}` first.",
                    archive.display(),
                    xtask_build_hint()
                );
            }
        }
        let zlib_header = zlib_dir.join("include/zlib.h");
        let zlib_archive = zlib_dir.join("lib/libz.a");
        for path in [&zlib_header, &zlib_archive] {
            println!("cargo:rerun-if-changed={}", path.display());
            if !path.is_file() {
                panic!(
                    "Android zlib dependency was not found at {}. Run `{}` first.",
                    path.display(),
                    xtask_build_hint()
                );
            }
        }
    }
    let dav1d_header = dav1d_dir.join("include/dav1d/dav1d.h");
    let dav1d_archive = if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && dav1d_dir.join("lib/dav1d.lib").is_file()
    {
        dav1d_dir.join("lib/dav1d.lib")
    } else {
        dav1d_dir.join("lib/libdav1d.a")
    };
    for path in [&dav1d_header, &dav1d_archive] {
        println!("cargo:rerun-if-changed={}", path.display());
        if !path.is_file() {
            panic!(
                "dav1d dependency was not found at {}. Run `{}` first, or set ERIKA_DAV1D_DIR.",
                path.display(),
                xtask_build_hint()
            );
        }
    }

    let ffmpeg_version_major = emit_ffmpeg_version_cfg(&include_dir);
    enforce_bundled_ffmpeg_version(ffmpeg_version_major, &include_dir);

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!(
        "cargo:rustc-link-search=native={}",
        zlib_dir.join("lib").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        dav1d_dir.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=avdevice");
    println!("cargo:rustc-link-lib=static=avfilter");
    println!("cargo:rustc-link-lib=static=avformat");
    println!("cargo:rustc-link-lib=static=avcodec");
    println!("cargo:rustc-link-lib=static=swresample");
    println!("cargo:rustc-link-lib=static=swscale");
    println!("cargo:rustc-link-lib=static=avutil");
    println!("cargo:rustc-link-lib=static=dav1d");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=static=zlib");
    } else {
        println!("cargo:rustc-link-lib=static=z");
    }

    if matches!(
        env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("macos" | "ios" | "tvos")
    ) {
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=CoreMedia");
        println!("cargo:rustc-link-lib=framework=CoreVideo");
        println!("cargo:rustc-link-lib=framework=VideoToolbox");
        println!("cargo:rustc-link-lib=iconv");
        println!("cargo:rustc-link-lib=bz2");
        if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
            link_macos_compiler_runtime();
        }
    } else if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        for lib in [
            "bcrypt",
            "d3d11",
            "dxgi",
            "dxguid",
            "gdi32",
            "mf",
            "mfplat",
            "mfuuid",
            "mfreadwrite",
            "ole32",
            "secur32",
            "strmiids",
            "user32",
            "uuid",
            "ws2_32",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
    } else if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        for lib in [
            "android",
            "log",
            "camera2ndk",
            "mediandk",
            "atomic",
            "dl",
            "m",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }

    vec![include_dir]
}

/// FFmpeg's VideoToolbox availability checks call __isPlatformVersionAtLeast.
/// Clang/Swift link compiler-rt automatically, but Rust's -nodefaultlibs does
/// not. Carry this dependency with FFmpeg so standalone Rust hosts link too.
fn link_macos_compiler_runtime() {
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
    let output = std::process::Command::new("xcrun")
        .args([
            "--sdk",
            "macosx",
            "clang",
            "--print-file-name=libclang_rt.osx.a",
        ])
        .output()
        .expect("locate the macOS Clang runtime with xcrun");
    assert!(
        output.status.success(),
        "cannot locate the macOS Clang runtime: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let archive = PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("Clang runtime path is UTF-8")
            .trim(),
    );
    assert!(
        archive.is_file(),
        "macOS Clang runtime not found: {}",
        archive.display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        archive.parent().expect("Clang runtime directory").display()
    );
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
}

fn generate_bindings(include_dirs: &[PathBuf]) {
    ensure_libclang_path();

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .allowlist_function("av_.*")
        .allowlist_function("avio_.*")
        .allowlist_function("avcodec_.*")
        .allowlist_function("avsubtitle_.*")
        .allowlist_function("avformat_.*")
        .allowlist_function("swr_.*")
        .allowlist_function("sws_.*")
        .allowlist_type("AV.*")
        .allowlist_type("Swr.*")
        .allowlist_type("Sws.*")
        .allowlist_var("AV.*")
        .allowlist_var("FF_.*")
        .allowlist_var("SWS_.*")
        .allowlist_var("AVERROR.*")
        .blocklist_item("FP_.*")
        .generate_comments(false)
        .derive_debug(true)
        .derive_default(true);
    for include_dir in include_dirs {
        builder = builder.clang_arg(format!("-I{}", include_dir.display()));
    }
    for argument in target_bindgen_clang_args() {
        builder = builder.clang_arg(argument);
    }
    let bindings = builder.generate().expect("generate FFmpeg bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("write FFmpeg bindings");
}

fn emit_ffmpeg_version_cfg(include_dir: &Path) -> Option<u32> {
    println!("cargo:rustc-check-cfg=cfg(erika_ffmpeg_legacy_channel_layout)");
    let version_header = include_dir.join("libavutil/version.h");
    println!("cargo:rerun-if-changed={}", version_header.display());
    let Ok(contents) = fs::read_to_string(&version_header) else {
        return None;
    };
    let major = contents.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some("#define"), Some("LIBAVUTIL_VERSION_MAJOR"), Some(value)) => {
                value.parse::<u32>().ok()
            }
            _ => None,
        }
    });
    if matches!(major, Some(value) if value < 57) {
        println!("cargo:rustc-cfg=erika_ffmpeg_legacy_channel_layout");
    }
    major
}

fn enforce_bundled_ffmpeg_version(version_major: Option<u32>, include_dir: &Path) {
    let target_os = env::var("CARGO_CFG_TARGET_OS").ok();
    if env::var("ERIKA_ALLOW_LEGACY_FFMPEG").as_deref() == Ok("1") {
        return;
    }
    if matches!(version_major, Some(major) if major >= 60) {
        return;
    }
    panic!(
        "{} native core requires Erika's FFmpeg 8.x dependency bundle (libavutil >= 60), but found {:?} under {}. Run `{}` or set ERIKA_FFMPEG_DIR to that dist; set ERIKA_ALLOW_LEGACY_FFMPEG=1 only for local compatibility experiments.",
        target_os.as_deref().unwrap_or("target"),
        version_major,
        include_dir.display(),
        xtask_build_hint()
    );
}

fn ensure_libclang_path() {
    if env::var_os("LIBCLANG_PATH").is_some() {
        return;
    }
    let mut candidates = vec![
        PathBuf::from("C:/msys64/mingw64/bin"),
        PathBuf::from("C:/Program Files/LLVM/bin"),
    ];
    if let Some(prebuilt) = android_ndk_prebuilt_dir() {
        candidates.push(prebuilt.join("bin"));
        candidates.push(prebuilt.join("lib64"));
        candidates.push(prebuilt.join("lib"));
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("ohos")
        && let Some(native_root) = ohos_native_root()
    {
        candidates.push(native_root.join("llvm/lib"));
        candidates.push(native_root.join("llvm/bin"));
    }
    for path in candidates {
        if contains_libclang(&path) {
            // Build scripts are single-process setup code; set this before bindgen
            // loads libclang so Windows source builds work without a developer shell.
            unsafe {
                env::set_var("LIBCLANG_PATH", &path);
            }
            prepend_path_for_dlls(&path);
            if path.starts_with("C:/msys64") {
                prepend_path_for_dlls(Path::new("C:/msys64/usr/bin"));
            }
            return;
        }
    }
}

fn contains_libclang(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    if ["libclang.dll", "libclang.so", "libclang.dylib"]
        .into_iter()
        .any(|name| path.join(name).is_file())
    {
        return true;
    }
    fs::read_dir(path).is_ok_and(|entries| {
        entries.filter_map(|entry| entry.ok()).any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("libclang.so.")
        })
    })
}

fn target_bindgen_clang_args() -> Vec<String> {
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("ohos") {
        let native_root = ohos_native_root().expect(
            "OpenHarmony native SDK was not found for bindgen; set OHOS_NDK_HOME or OHOS_SDK_NATIVE",
        );
        let target = env::var("TARGET").expect("Cargo TARGET is set for OpenHarmony bindgen");
        assert!(
            target.ends_with("-ohos"),
            "unsupported OpenHarmony Rust target for bindgen: {target}"
        );
        return vec![
            format!("--target={target}"),
            format!("--sysroot={}", native_root.join("sysroot").display()),
        ];
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return Vec::new();
    }
    let target = env::var("TARGET").expect("Cargo TARGET is set for Android bindgen");
    let clang_triple = match target.as_str() {
        "aarch64-linux-android" => "aarch64-linux-android",
        "armv7-linux-androideabi" => "armv7a-linux-androideabi",
        "x86_64-linux-android" => "x86_64-linux-android",
        "i686-linux-android" => "i686-linux-android",
        other => panic!("unsupported Android Rust target for bindgen: {other}"),
    };
    let api_level = android_api_level();
    let prebuilt = android_ndk_prebuilt_dir()
        .expect("Android NDK was not found for bindgen; set ANDROID_NDK_HOME or ANDROID_NDK_ROOT");
    let sysroot = prebuilt.join("sysroot");
    if !sysroot.is_dir() {
        panic!("Android NDK sysroot was not found at {}", sysroot.display());
    }
    vec![
        format!("--target={clang_triple}{api_level}"),
        format!("--sysroot={}", sysroot.to_string_lossy().replace('\\', "/")),
    ]
}

fn ohos_native_root() -> Option<PathBuf> {
    for variable in ["OHOS_NDK_HOME", "OHOS_SDK_NATIVE"] {
        if let Some(value) = env::var_os(variable) {
            let path = PathBuf::from(value);
            if path.is_dir() {
                return Some(path);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let deveco = PathBuf::from(
            "/Applications/DevEco-Studio.app/Contents/sdk/default/openharmony/native",
        );
        if deveco.is_dir() {
            return Some(deveco);
        }
    }
    None
}

fn android_api_level() -> u32 {
    let api = env::var("ANDROID_API_LEVEL")
        .ok()
        .map(|value| {
            value
                .trim_start_matches("android-")
                .parse::<u32>()
                .unwrap_or_else(|_| panic!("ANDROID_API_LEVEL must be an integer, got `{value}`"))
        })
        .unwrap_or(26);
    assert!(api >= 26, "Erika Android builds require API 26 or newer");
    api
}

fn android_ndk_prebuilt_dir() -> Option<PathBuf> {
    let ndk_root = android_ndk_root()?;
    let prebuilt = ndk_root.join("toolchains/llvm/prebuilt");
    let preferred: &[&str] = if cfg!(windows) {
        &["windows-x86_64"]
    } else if cfg!(target_os = "macos") {
        &["darwin-aarch64", "darwin-x86_64"]
    } else {
        &["linux-x86_64"]
    };
    preferred
        .iter()
        .map(|tag| prebuilt.join(tag))
        .find(|path| path.is_dir())
}

fn android_ndk_root() -> Option<PathBuf> {
    let mut candidates = ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    for variable in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = env::var_os(variable) {
            add_sdk_ndk_candidates(&mut candidates, &PathBuf::from(root));
        }
    }
    if cfg!(windows)
        && let Some(local_app_data) = env::var_os("LOCALAPPDATA")
    {
        add_sdk_ndk_candidates(
            &mut candidates,
            &PathBuf::from(local_app_data).join("Android/Sdk"),
        );
    }
    candidates.into_iter().find(|path| {
        path.join("build/cmake/android.toolchain.cmake").is_file()
            && path.join("toolchains/llvm/prebuilt").is_dir()
    })
}

fn add_sdk_ndk_candidates(candidates: &mut Vec<PathBuf>, sdk_root: &Path) {
    if let Ok(entries) = fs::read_dir(sdk_root.join("ndk")) {
        let mut versions = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        versions.sort_by_key(|path| {
            path.file_name()
                .map(|name| version_key(&name.to_string_lossy()))
                .unwrap_or_default()
        });
        versions.reverse();
        candidates.extend(versions);
    }
    candidates.push(sdk_root.join("ndk-bundle"));
}

fn version_key(value: &str) -> Vec<u32> {
    value
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

fn prepend_path_for_dlls(path: &Path) {
    if !path.exists() {
        return;
    }
    let mut paths = vec![path.to_path_buf()];
    if let Some(current) = env::var_os("PATH") {
        paths.extend(env::split_paths(&current));
    }
    if let Ok(joined) = env::join_paths(paths) {
        unsafe {
            env::set_var("PATH", joined);
        }
    }
}

fn ffmpeg_dist_dir() -> PathBuf {
    if let Ok(path) = env::var("ERIKA_FFMPEG_DIR") {
        return PathBuf::from(path);
    }
    if let Ok(target) = env::var("ERIKA_NATIVE_TARGET") {
        return workspace_root()
            .join("third_party/dist")
            .join(target)
            .join(native_profile_dir())
            .join("ffmpeg");
    }
    native_dist_root().join(native_profile_dir()).join("ffmpeg")
}

fn native_dep_dir(env_name: &str, name: &str) -> PathBuf {
    if let Ok(path) = env::var(env_name) {
        return PathBuf::from(path);
    }
    if let Ok(target) = env::var("ERIKA_NATIVE_TARGET") {
        return workspace_root()
            .join("third_party/dist")
            .join(target)
            .join(native_profile_dir())
            .join(name);
    }
    native_dist_root().join(native_profile_dir()).join(name)
}

/// The dist directory `xtask deps build` filled for this target: the one
/// named after the target triple when it was built (`--target
/// aarch64-apple-darwin`), else the host build's.
fn native_dist_root() -> PathBuf {
    let dist = workspace_root().join("third_party/dist");
    if let Some(target) = inferred_native_target() {
        return dist.join(target);
    }
    if let Ok(target) = env::var("TARGET") {
        let built = dist.join(target);
        if built.join(native_profile_dir()).is_dir() {
            return built;
        }
    }
    dist
}

fn native_profile() -> String {
    env::var("ERIKA_NATIVE_PROFILE").unwrap_or_else(|_| "lgpl".to_string())
}

/// Rust links the MSVC C runtime statically (`-C target-feature=+crt-static`),
/// so the native libraries must too: `xtask deps build --crt static`.
fn static_crt() -> bool {
    env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
        && env::var("CARGO_CFG_TARGET_FEATURE")
            .is_ok_and(|features| features.split(',').any(|feature| feature == "crt-static"))
}

/// The dist directory of the profile, which a static C runtime build keeps
/// apart.
fn native_profile_dir() -> String {
    if static_crt() {
        format!("{}-crt-static", native_profile())
    } else {
        native_profile()
    }
}

fn xtask_build_hint() -> String {
    let profile = native_profile();
    let crt = if static_crt() { " --crt static" } else { "" };
    if let Some(target) = inferred_native_target() {
        format!("cargo run -p xtask -- deps build --profile {profile} --target {target}{crt}")
    } else {
        format!("cargo run -p xtask -- deps build --profile {profile}{crt}")
    }
}

fn inferred_native_target() -> Option<String> {
    let os = env::var("CARGO_CFG_TARGET_OS").ok()?;
    let arch = env::var("CARGO_CFG_TARGET_ARCH").ok()?;
    match (os.as_str(), arch.as_str()) {
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc".to_string()),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc".to_string()),
        ("android", "aarch64") => Some("aarch64-linux-android".to_string()),
        ("android", "arm") => Some("armv7-linux-androideabi".to_string()),
        ("android", "x86_64") => Some("x86_64-linux-android".to_string()),
        ("android", "x86") => Some("i686-linux-android".to_string()),
        ("ios", _) => Some("ios".to_string()),
        ("tvos", "aarch64") if env::var("CARGO_CFG_TARGET_ABI").as_deref() == Ok("sim") => {
            Some("aarch64-apple-tvos-sim".to_string())
        }
        ("tvos", "aarch64") => Some("aarch64-apple-tvos".to_string()),
        ("tvos", "x86_64") => Some("x86_64-apple-tvos".to_string()),
        _ => None,
    }
}

fn workspace_root() -> PathBuf {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    manifest_dir
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .expect("crate lives under workspace/crates/name")
}
