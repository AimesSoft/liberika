# liberika のリリース

[中文](releasing.zh.md) · [English](releasing.md) · [ドキュメント](README.md)

通常のリリースはコアの `vX.Y.Z` tag を一つ push して開始します。GitHub Actions がネイティブライブラリを公開し、Flutter、OHPM、Swift SDK を更新して公開します。

## リリース手順

1. 変更を `main` に merge し、[CHANGELOG](../CHANGELOG.md)、SDK README、関係する接続ガイドを更新します。C ABI の変更では header、各 platform binding、ネイティブバイナリも更新します。
2. 変更に応じたテストと platform build を実行します。基本チェック：

```sh
cargo fmt --all -- --check
cargo test --locked -p erika -p erika_capi
cargo clippy --locked -p erika --all-targets --all-features --no-deps
cd packages/erika_flutter
dart pub publish --dry-run
```

3. `Cargo.toml` の `[workspace.package].version` を更新し、`cargo update -w` 後の `Cargo.lock` も commit します。Release は Cargo version と異なる tag を拒否します。対象 commit で手動 `Release` を実行し、全 platform の build 成功を確認してから tag を付けます。以下は次回バージョンの例です。実際の番号に置き換えてください。

```sh
VERSION=0.3.0
git tag "v${VERSION}"
git push origin "v${VERSION}"
```

4. Actions の `Release`、`Release ecosystem packages` と各 SDK の公開結果を確認し、GitHub Release に機能と修正を記載します。OHPM は upload 後に審査があります。

## 自動公開の流れ

[release.yml](../.github/workflows/release.yml) は platform を並列 build します。iOS、tvOS、Android は architecture slice を個別に build して archive を組み立て、macOS arm64/x64 は universal に結合します。GitHub Release に 13 個の native/Flutter archive、Web archive と `SHA256SUMS` を公開します。

[release-ecosystem.yml](../.github/workflows/release-ecosystem.yml) が続けて実行します：

- 公開済みの checksum で Flutter/OHPM version と `native_artifacts.properties` を更新し、metadata を `main` に commit。
- 内部 tag `erika_flutter-vX.Y.Z` を作成し、[Flutter OIDC 公開](../.github/workflows/pub-publish.yml)を開始。
- ArkTS HAR を build し OHPM に upload。
- Swift XCFramework を組み立て、コア Release と `SHA256SUMS` に追加。liberika-swift に更新・テスト・同じ version の公開を通知。

Flutter、OHPM、Swift の公開は並列実行できます。メンテナーが作成するのはコア tag だけです。別 repository の tag と Swift 公開には secret `ERIKA_SWIFT_RELEASE_TOKEN` を使います。

`workflow_dispatch` による手動 `Release` は build を確認し、Actions Artifact を保存します。GitHub Release の公開は tag で開始します。

## 公開する成果物

完了したリリースは ZIP 15 個と `SHA256SUMS` を含みます。

| Platform | Archive 命名 | 数 |
|---|---|---|
| macOS arm64 / x64 / universal | `erika-capi-macos-{arm64,x64,universal}.zip` | 3 |
| Windows x64 / ARM64 | `erika-capi-windows-{x64,arm64}.zip` | 2 |
| iOS / tvOS | `erika-capi-{ios,tvos}.zip` | 2 |
| Android | `erika-capi-android.zip` | 1 |
| Flutter Android | `erika-flutter-android-{arm64-v8a,armeabi-v7a,x86_64,x86}.zip` | 4 |
| OpenHarmony arm64 | `erika-capi-openharmony-arm64.zip` | 1 |
| Web / WASM | `liberika-web-X.Y.Z.zip` | 1 |
| Swift | `erika-swift-core-X.Y.Z.xcframework.zip` | 1 |

C ABI bundle は library、`include/erika.h`、`LICENSE`、`THIRD_PARTY_NOTICES.md`、依存 license、tag/commit を記録した `MANIFEST.txt` を含みます。iOS/tvOS は device + simulator XCFramework、Android 統合 bundle は 4 ABI の static/shared library と対応する `libc++_shared.so` を含みます。Flutter Android bundle は各 ABI の shared runtime のみを含みます。

OpenHarmony は Native SDK 5.1.0、compatible SDK 18 を使います。Linux は現在ソースから build します。[Linux ガイド](linux.md)。

## OHPM upload timeout

[ohpm-package.yml](../.github/workflows/ohpm-package.yml) の公開コマンドは `--fetch_timeout 360000` を使います。これは CLI のリクエスト上限の 6 分です。公開 job 全体は 60 分が上限です。upload した version は OHPM の審査後に公開されます。

## ビルド済みランタイムの利用

Flutter は package に固定された native tag を download し、SHA-256 を検証します。アプリの設定は [Flutter README](../packages/erika_flutter/README.ja.md) を参照してください。ローカル開発・カスタム build の変数：

| 変数 | 用途 |
|---|---|
| `ERIKA_PREBUILT_TAG` | native version を指定。対応する checksum も設定。 |
| `ERIKA_PREBUILT_SHA256` | カスタム tag の archive checksum。 |
| `ERIKA_PREBUILT_SHA256_<ABI>` | 複数 ABI の Android は `ARM64_V8A`、`ARMEABI_V7A`、`X86_64`、`X86` を個別に設定。 |
| `ERIKA_FORCE_SOURCE_BUILD=1` | ローカル liberika source を build。 |
| `ERIKA_MACOS_ARCHS` | `universal`, `arm64`, `x86_64`, `arm64,x86_64` |

C/C++ は bundle を展開し、`lib/` を link、`include/erika.h` を include します。macOS dylib は `@rpath/liberika_capi.dylib` を使います。[ネイティブ接続](integration.ja.md)。

## ローカル packaging と license

[packaging/bundle.sh](../packaging/bundle.sh) はローカルと CI で共通です。

```sh
bash packaging/bundle.sh erika-capi-macos-universal \
  dist/erika-capi-macos-universal.zip out/liberika_capi.dylib out/liberika_capi.a
```

liberika は MPL-2.0、依存 library はそれぞれの license を維持します。公開 bundle に license、third-party notice、build source を残します。`lgpl` / `gpl-full` の設定は[ビルドガイド](building.ja.md#ライセンス-profile)で管理します。
