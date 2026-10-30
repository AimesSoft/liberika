# Releasing liberika

[中文](releasing.zh.md) · [日本語](releasing.ja.md) · [Documentation](README.md)

A normal release starts with one core `vX.Y.Z` tag. GitHub Actions publishes the native libraries, then updates and publishes the Flutter, OHPM, and Swift SDKs.

## Release steps

1. Merge the changes into `main`, update the [CHANGELOG](../CHANGELOG.md), SDK READMEs, and affected integration guides. C ABI changes also require matching headers, platform bindings, and native binaries.
2. Run the affected tests and platform builds. Basic native and Flutter package checks:

```sh
cargo fmt --all -- --check
cargo test --locked -p erika -p erika_capi
cargo clippy --locked -p erika --all-targets --all-features --no-deps
cd packages/erika_flutter
dart pub publish --dry-run
```

3. Set `[workspace.package].version` in `Cargo.toml` to the release version and run `cargo update -w`; commit `Cargo.lock` with it. The Release workflow rejects tags that differ from the Cargo version. Run `Release` manually on this commit and confirm all platform builds pass before tagging. Tag the release commit. The version below is an example for the next release; replace it with the intended version:

```sh
VERSION=0.4.0
git tag "v${VERSION}"
git push origin "v${VERSION}"
```

4. Check `Release`, `Release ecosystem packages`, and each SDK publish result in Actions. Write the features and fixes in the GitHub Release. OHPM review follows a successful upload.

## Automated pipeline

[release.yml](../.github/workflows/release.yml) builds platforms in parallel. iOS, tvOS, and Android build independent architecture slices and then assemble their archives; macOS arm64/x64 outputs form the universal bundle. The workflow creates a GitHub Release with 13 native/Flutter archives, the Web archive, and `SHA256SUMS`.

[release-ecosystem.yml](../.github/workflows/release-ecosystem.yml) then:

- Updates Flutter/OHPM versions and `native_artifacts.properties` with the uploaded checksums, committing the metadata to `main`.
- Creates the internal `erika_flutter-vX.Y.Z` tag to trigger [Flutter OIDC publishing](../.github/workflows/pub-publish.yml).
- Builds the ArkTS HAR and uploads it to OHPM.
- Assembles the Swift XCFramework, appends it to the core Release and `SHA256SUMS`, and dispatches liberika-swift to update, test, and publish its matching version.

Flutter, OHPM, and Swift publishing can run in parallel. Maintainers create only the core tag. Cross-repository tags and Swift publishing use the repository secret `ERIKA_SWIFT_RELEASE_TOKEN`.

A manual `Release` run through `workflow_dispatch` checks the builds and stores Actions Artifacts. A tag triggers the GitHub Release publish step.

## Release artifacts

A complete release contains 15 ZIPs plus `SHA256SUMS`:

| Platform | Archive naming | Count |
|---|---|---|
| macOS arm64 / x64 / universal | `erika-capi-macos-{arm64,x64,universal}.zip` | 3 |
| Windows x64 / ARM64 | `erika-capi-windows-{x64,arm64}.zip` | 2 |
| iOS / tvOS | `erika-capi-{ios,tvos}.zip` | 2 |
| Android | `erika-capi-android.zip` | 1 |
| Flutter Android | `erika-flutter-android-{arm64-v8a,armeabi-v7a,x86_64,x86}.zip` | 4 |
| OpenHarmony arm64 | `erika-capi-openharmony-arm64.zip` | 1 |
| Web / WASM | `liberika-web-X.Y.Z.zip` | 1 |
| Swift | `erika-swift-core-X.Y.Z.xcframework.zip` | 1 |

C ABI bundles include the library, `include/erika.h`, `LICENSE`, `THIRD_PARTY_NOTICES.md`, dependency license texts, and a `MANIFEST.txt` with the tag/commit. iOS/tvOS contain device + simulator XCFramework slices. The combined Android bundle contains static/shared libraries and matching `libc++_shared.so` for four ABIs. Flutter Android bundles contain one ABI's shared runtime each.

OpenHarmony uses the 5.1.0 Native SDK with compatible SDK 18. Linux currently builds from source; use the [Linux guide](linux.md).

## OHPM upload timeout

The publish command in [ohpm-package.yml](../.github/workflows/ohpm-package.yml) uses `--fetch_timeout 360000`, the CLI's six-minute request limit. The publish job allows 60 minutes for the whole task. The uploaded version becomes public after OHPM approval.

## Consuming prebuilt runtimes

Flutter downloads its package-pinned native tag and verifies SHA-256 by default. Application setup belongs in the [Flutter README](../packages/erika_flutter/README.md). These variables support local development and custom builds:

| Variable | Purpose |
|---|---|
| `ERIKA_PREBUILT_TAG` | Override the native version; also supply matching checksums. |
| `ERIKA_PREBUILT_SHA256` | Archive checksum for a custom tag. |
| `ERIKA_PREBUILT_SHA256_<ABI>` | Set `ARM64_V8A`, `ARMEABI_V7A`, `X86_64`, and `X86` separately for multi-ABI Android. |
| `ERIKA_FORCE_SOURCE_BUILD=1` | Build the local liberika source. |
| `ERIKA_MACOS_ARCHS` | `universal`, `arm64`, `x86_64`, `arm64,x86_64` |

C/C++ consumers link `lib/` and include `include/erika.h` after extracting a bundle. The macOS dylib uses `@rpath/liberika_capi.dylib`. Use the [native integration guide](integration.md).

## Local packaging and licenses

[packaging/bundle.sh](../packaging/bundle.sh) is shared by local builds and CI:

```sh
bash packaging/bundle.sh erika-capi-macos-universal \
  dist/erika-capi-macos-universal.zip out/liberika_capi.dylib out/liberika_capi.a
```

liberika uses MPL-2.0; third-party dependencies retain their own licenses. Preserve license texts, third-party notices, and build provenance in published bundles. The [build guide](building.md#license-profiles) maintains the `lgpl` / `gpl-full` dependency configuration.
