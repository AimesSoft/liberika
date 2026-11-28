{
  description = "liberika Rust-first media player development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          darwinFrameworks = with pkgs.darwin.apple_sdk.frameworks; [
            AppKit
            AudioToolbox
            CoreAudio
            CoreFoundation
            CoreGraphics
            CoreMedia
            CoreVideo
            Foundation
            Metal
            QuartzCore
            VideoToolbox
          ];
          commonPackages = with pkgs; [
            autoconf
            automake
            bzip2
            cmake
            curl
            libiconv
            libtool
            llvmPackages.libclang
            meson
            nasm
            ninja
            pkg-config
            python3
            rustup
            xz
            yasm
            zlib
          ];
        in
        {
          default = pkgs.mkShell {
            packages = commonPackages ++ pkgs.lib.optionals pkgs.stdenv.isDarwin darwinFrameworks;

            shellHook = ''
              export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
              export BINDGEN_EXTRA_CLANG_ARGS="-I${pkgs.llvmPackages.libclang.lib}/lib/clang/${pkgs.llvmPackages.libclang.version}/include $BINDGEN_EXTRA_CLANG_ARGS"
              export ERIKA_NATIVE_PROFILE="''${ERIKA_NATIVE_PROFILE:-lgpl}"
              echo "Erika dev shell ready ($ERIKA_NATIVE_PROFILE)"
            '';
          };
        });
    };
}
