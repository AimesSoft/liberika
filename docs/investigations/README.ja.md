[中文](README.md) | [English](README.en.md) | 日本語

# 調査・検証記録

各記録は、その日付、コミット、使用デバイスを踏まえて読んでください。現在の導入手順は[ドキュメント索引](../README.ja.md)にあります。以下のリンクには元の記録の言語を示しています。ログ、スクリーンショットなどの元の証拠はそのまま保持しています。

| 記録 | 範囲 |
|---|---|
| [Windows D3D11 スクリーンショットと 4K 再生（中文）](windows-d3d11-capture-2026-11-16/README.md) | 2026-11-16、WARP / Intel UHD 770 画素検証、RTX 5060 Ti の実際の 4K HEVC 出力と HWND/Composition ライフサイクル |
| [#197 surface なしの字幕出力（中文）](issue-197-headless-subtitles-2026-11-16/README.md) | 2026-11-16、Apple Metal：1080p ASS 再生から 640×360 ネイティブテクスチャ出力。main/修正版の比較、画素と字幕レイヤー呼び出しの検証 |
| [#205 HTTP キャンセルと連続 seek（中文）](issue-205-http-seek-2026-11-16/README.md) | 2026-11-16、macOS / Windows：実際の HTTP Range、12 回の読み取り中断、旧 main の失敗と修正版の自然終了 |
| [Issue #213：Rust 利用側と Windows host の検証（English）](issue-213-rust-consumer-2026-11-16/README.md) | 2026-11-16。明示的な HAL patch、Linux のコンパイル・リンク、Windows xtask/C API/ネイティブ検証 |
| [Vulkan fp16 空間ヘッド最適化（中文）](sr-gpu-head-2026-11-03/README.md) | 2026-11-03、RTX 5060 Ti。配置・精度検証と約 7–8% のネットワーク GPU 時間短縮 |
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
