use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // Without an ELF SONAME, CMake records this cdylib's absolute build
        // path in the N-API bridge's DT_NEEDED entry, which cannot resolve on
        // device after HAR packaging.
        println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,liberika_capi.so");
    }

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && env::var_os("CARGO_FEATURE_LIBASS").is_some()
    {
        println!("cargo:rustc-link-lib=dwrite");
    }
}
