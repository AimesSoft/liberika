[中文](README.md) | [English](README.en.md) | 日本語

# 調査・検証記録

各記録は、その日付、コミット、使用デバイスを踏まえて読んでください。現在の導入手順は[ドキュメント索引](../README.ja.md)にあります。以下のリンクには元の記録の言語を示しています。ログ、スクリーンショットなどの元の証拠はそのまま保持しています。

| 記録 | 範囲 |
|---|---|
| [PR #150–#159 の目的、トレードオフと実行検証](pr-150-159/review.ja.md) · [中文](pr-150-159/review.zh.md) · [English](pr-150-159/review.en.md) | 2026-10-05 の調査、10-08 の OHOS 追記、10-13 の main 統合。HTTP、メディア識別、タイムスタンプ、音声、GPU、ライフサイクル |
| [main 統合と実行検証](pr-150-159/merge-validation.ja.md) · [中文](pr-150-159/merge-validation.zh.md) · [English](pr-150-159/merge-validation.en.md) | 2026-10-13、PR #164 の統合と Windows / Linux / OHOS の実機・実行確認 |
| [OHOS エミュレーターの実行記録](pr-150-159/ohos-runtime.ja.md) · [中文](pr-150-159/ohos-runtime.zh.md) · [English](pr-150-159/ohos-runtime.en.md) | 2026-10-08、完全な Rust runtime、NAPI ブリッジとエミュレーター |
| [Apple 検証の引き継ぎ](pr-150-159/apple-handoff.ja.md) · [中文](pr-150-159/apple-handoff.zh.md) · [English](pr-150-159/apple-handoff.en.md) | PR #154、#155、#157 の Apple 検証範囲と残作業 |
| [ネイティブチェックの再現手順](pr-150-159/scripts/README.ja.md) · [中文](pr-150-159/scripts/README.zh.md) · [English](pr-150-159/scripts/README.md) | Windows、Linux、OHOS、Apple の検証スクリプトと証拠 |
| [Doctor-X の音声調査](doctorx-audio-2026-10-26.ja.md) · [中文](doctorx-audio-2026-10-26.zh.md) · [English](doctorx-audio-2026-10-26.en.md) | 2026-10-26、macOS で断続的に発生する音割れ |
| [v0.1.8 API の整合性](v0.1.8-api-alignment.ja.md) · [中文](v0.1.8-api-alignment.md) · [English](v0.1.8-api-alignment.en.md) | 0.1.8 時点のバインディング、ABI、SDK の相違 |
| [Windows ネイティブ HDR とテクスチャーの互換性](pr129-native-hdr-compatibility.ja.md) · [中文](pr129-native-hdr-compatibility.zh.md) · [English](pr129-native-hdr-compatibility.en.md) | PR #129 のプラットフォーム経路と実験 |
| [HTTP リソースのライフサイクル](issue-125-verification.ja.md) · [中文](issue-125-verification.zh.md) · [English](issue-125-verification.en.md) | 2026-09-28、issue #125 のキャンセル、再生し直し、終了 |
| [iOS 弾幕の表示タイミング](ios-dfm-frame-pacing.ja.md) · [中文](ios-dfm-frame-pacing.zh.md) · [English](ios-dfm-frame-pacing.md) | PR #146 の表示目標時刻のサンプリングと mailbox 設計 |
| [Linux 検証](linux-validation.ja.md) · [中文](linux-validation.zh.md) · [English](linux-validation.en.md) | 2026-09-29、WSLg、GPU、Flutter、C ABI |
