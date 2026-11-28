# 发布 liberika

[English](releasing.md) · [日本語](releasing.ja.md) · [文档目录](README.md)

正常发布只需推送一个核心 `vX.Y.Z` tag。GitHub Actions 先发布原生库，再自动更新并发布 Flutter、OHPM 和 Swift SDK。

## 发布步骤

1. 将本次变更合并到 `main`，更新 [CHANGELOG](../CHANGELOG.md)、SDK README 和受影响的接入文档。若修改 C ABI，同时更新头文件、各平台绑定和预编译内核。
2. 运行受影响的测试与平台构建。原生和 Flutter 包的基础检查：

```sh
cargo fmt --all -- --check
cargo test --locked -p erika -p erika_capi
cargo clippy --locked -p erika --all-targets --all-features --no-deps
cd packages/erika_flutter
dart pub publish --dry-run
```

3. 将 `Cargo.toml` 的 `[workspace.package].version` 改为发布版本，运行 `cargo update -w` 并一同提交 `Cargo.lock`。Release 会拒绝与 Cargo 版本不一致的 tag。直接在 `main` 的该提交上打核心 tag；tag 触发的 `Release` 会构建所有平台并发布，不需要另外试跑。以下版本号是下一次发布的示例，按实际版本替换：

```sh
VERSION=0.3.8
git tag "v${VERSION}"
git push origin "v${VERSION}"
```

4. 在 Actions 中确认 `Release`、`Release ecosystem packages` 和各 SDK 发布结果；在 Release 页面写本次功能和修复。OHPM 上传后还需等待平台审核。

## 自动发布流程

[release.yml](../.github/workflows/release.yml) 并行构建各平台。iOS、tvOS、Android 按架构分片构建，再合并归档；macOS arm64/x64 合成 universal。完成后创建 GitHub Release，上传 17 个原生/Flutter 归档、两个 Web 归档与 `SHA256SUMS`。

[release-ecosystem.yml](../.github/workflows/release-ecosystem.yml) 接着执行：

- 用已上传的校验值更新 Flutter 和 OHPM 的版本与 `native_artifacts.properties`，提交到 `main`。
- 创建内部 `erika_flutter-vX.Y.Z` tag，触发 [Flutter OIDC 发布](../.github/workflows/pub-publish.yml)。
- 构建 ArkTS HAR 并上传 OHPM。
- 组装 Swift XCFramework，追加到核心 Release 和 `SHA256SUMS`，通知 liberika-swift 更新、测试并发布对应版本。
- 发布 npm 包 `@aimessoft/liberika-web` 与 `@aimessoft/liberika-web-nofont`：使用 `Release` 在构建 Web 归档时打好的两个 tarball，版本取自 Cargo workspace。

Flutter、OHPM 和 Swift 发布可以并行进行。维护者只创建核心 tag；跨仓库 tag 和 Swift 发布使用仓库 secret `ERIKA_SWIFT_RELEASE_TOKEN`。

tag 触发构建和 GitHub Release 发布。手动的 `workflow_dispatch` 只用于排查构建失败，不是发布步骤。

### npm 首次设置

npm 发布使用 trusted publishing，不保存 token，仓库变量 `NPM_PUBLISH` 为 `true` 时才运行。trusted publisher 在包的设置页里配置，因此每个包的第一个版本需要手动发布一次：

1. 在 npmjs.com 创建 `aimessoft` 组织，账号启用 2FA。
2. 从一次 `Release` 运行下载 `liberika-web-npm` artifact，对其中两个 tarball 分别执行 `npm publish <tarball> --access public --registry https://registry.npmjs.org/`。本地也可在 `web build` 后用 `python3 scripts/web_npm_package.py` 和 `npm pack` 得到同样的 tarball。
3. 在两个包的 Settings → Trusted Publisher 选择 GitHub Actions，填写 `AimesSoft/liberika` 和 workflow 文件名 `release-ecosystem.yml`。
4. 在仓库 Settings → Variables 添加 `NPM_PUBLISH=true`。此后每次发布自动上传，已存在的版本会跳过。

## 发布产物

完整发布共 20 个 ZIP，另附 `SHA256SUMS`：

| 平台 | 归档命名 | 数量 |
|---|---|---|
| macOS arm64 / x64 / universal | `erika-capi-macos-{arm64,x64,universal}.zip` | 3 |
| Windows x64 / ARM64 | `erika-capi-windows-{x64,arm64}.zip` | 2 |
| iOS / tvOS | `erika-capi-{ios,tvos}.zip` | 2 |
| Android | `erika-capi-android.zip` | 1 |
| Flutter Android | `erika-flutter-android-{arm64-v8a,armeabi-v7a,x86_64,x86}.zip` | 4 |
| OpenHarmony arm64 | `erika-capi-openharmony-arm64.zip` | 1 |
| Web / WASM | `liberika-web-X.Y.Z.zip`、`liberika-web-nofont-X.Y.Z.zip` | 2 |
| Linux x64 / ARM64 | `erika-capi-linux-{x64,arm64}-{bundled,system}.zip` | 4 |
| Swift | `erika-swift-core-X.Y.Z.xcframework.zip` | 1 |

C ABI 包包含库、`include/erika.h`、`LICENSE`、`THIRD_PARTY_NOTICES.md`、依赖许可证及记录 tag/commit 的 `MANIFEST.txt`。iOS/tvOS 为 device + simulator XCFramework；Android 合并包包含四个 ABI 的静态库、动态库和匹配的 `libc++_shared.so`。Flutter Android 包只携带单个 ABI 的动态运行时。

OpenHarmony 使用 5.1.0 Native SDK、compatible SDK 18。Linux 四组合复用 [Linux workflow](../.github/workflows/linux.yml)，全部以 Ubuntu 22.04 / glibc 2.35 构建，发布等待干净运行环境验收完成。bundled 内嵌 FFmpeg/字幕栈；system 不分发 FFmpeg SDK，运行时可自行安装 ABI 兼容的 FFmpeg 8 共享库，许可取决于外部 FFmpeg 配置。两者都不是全静态，包内附依赖报告；Flutter Linux 使用 `ERIKA_LIBRARY_DIR`，不自动下载。详见 [Linux 接入](linux.zh.md)。

## OHPM 上传超时

[ohpm-package.yml](../.github/workflows/ohpm-package.yml) 的发布命令使用 `--fetch_timeout 360000`，即 CLI 允许的六分钟请求超时。发布 job 的上限为 60 分钟，两者分别控制请求和整个任务。上传成功后，公开版本要等 OHPM 审核通过。

## 使用预编译内核

Flutter 默认下载当前 package 固定的原生 tag 并校验 SHA-256。应用接入步骤以 [Flutter README](../packages/erika_flutter/README.zh.md) 为准；以下变量用于本地调试和自定义构建：

| 变量 | 用途 |
|---|---|
| `ERIKA_PREBUILT_TAG` | 覆盖原生版本，须同时提供对应校验值。 |
| `ERIKA_PREBUILT_SHA256` | 自定义 tag 的归档校验值。 |
| `ERIKA_PREBUILT_SHA256_<ABI>` | Android 多 ABI 分别设置 `ARM64_V8A`、`ARMEABI_V7A`、`X86_64`、`X86`。 |
| `ERIKA_FORCE_SOURCE_BUILD=1` | 使用本地 liberika 源码。 |
| `ERIKA_MACOS_ARCHS` | `universal`, `arm64`, `x86_64`, `arm64,x86_64` |

C/C++ 接入解压归档后链接 `lib/`，包含 `include/erika.h`；macOS 动态库使用 `@rpath/liberika_capi.dylib`。接入方式在 [原生接入指南](integration.zh.md)。

## 本地打包与许可证

[packaging/bundle.sh](../packaging/bundle.sh) 同时供本地与 CI 使用：

```sh
bash packaging/bundle.sh erika-capi-macos-universal \
  dist/erika-capi-macos-universal.zip out/liberika_capi.dylib out/liberika_capi.a
```

liberika 使用 MPL-2.0，第三方依赖保留各自许可证。发布包保留许可证、第三方声明和构建来源；`lgpl` / `gpl-full` 的依赖与配置在 [构建指南](building.zh.md#许可证-profile) 中维护。
