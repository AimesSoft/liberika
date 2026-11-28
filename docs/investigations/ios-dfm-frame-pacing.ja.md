# iOS DFM+ 表示タイミング

[中文](ios-dfm-frame-pacing.zh.md) · [English](ios-dfm-frame-pacing.md)

liberika は既に CADisplayLink 駆動のシリアルワーカーで描画していましたが、DFM+ の位置は音動画の pump 後にワーカーの現在時刻で共有再生クロックをサンプリングしていました。そのため、キュー/pump の可変遅延が表示フレーム間のスクロール移動量を変えていました。描画がキューにある間は、最新の表示目標を含む後続コールバックをすべて破棄していました。

iOS ドライバーは現在、上限付きで最新要求を保持する mailbox を通して CADisplayLink.targetTimestamp を渡します。同時に存在するのは最大 1 つのワーカーと 1 つの保留要求です。ワーカーが多忙な場合、古いフレームを再生せず最新目標を消費します。ワーカーは一度に 1 呼び出しの間だけ player を保持し、持続的負荷が破棄を妨げないようにします。ドライバー変更時は保留要求をキャンセルし、既存のバックグラウンド音声専用パスを維持します。

新しい任意の C 入口 `erika_presenter_render_tick_with_timing` は、呼び出しの単調時刻から表示目標までの null 許容の符号付き遅延を受け取ります。Rust は presenter ロック取得やメディア pump 前に Instant へ変換します。弾幕、字幕、描画コンテキストはその時刻の同じ PlaybackSnapshot をサンプリングします。再生クロック本体、音声マスターの制御、seek generation、速度、オフセット、レイアウト/衝突ポリシーは変更しません。

従来の入口は null 目標を渡し、元のサンプリング動作を維持します。Swift プラグインは新入口を任意に検索するため、古いビルド済みライブラリでも従来タイミングで動作します。この最適化を完全に使うにはソースビルドのライブラリか、将来このシンボルを含むビルド済みライブラリが必要です。新たな必須リンクシンボルや公開構造体サイズの変更はありません。

## 検証、2026-09-29

ネイティブ実行に使用したのは iPhone 17 Pro / iOS 26.5 シミュレーターのみです。Rust は release profile、Flutter smoke shell は debug でビルドしました。Android エミュレーターは使用していません。

- iOS アプリはローカルビルドの静的ライブラリでコンパイル・実行しました。`nm` で静的リンク後のタイミングシンボルを確認し、実ドライバーは `mainThread=false displayTarget=true` を記録しました。
- iOS で Rust テスト 11 件を実行：新しい presenter 目標時刻テスト、再生クロック 8 件、既存のレイアウト generation と古いプランの無効化テスト。新テストは 60/120 Hz 目標サンプリング、繰り返し目標、一時停止、長短の seek、0.5/1/2x を対象とします。
- Swift mailbox 実行ファイルが iOS で合格：集約、多忙時のコールバック、古い/重複/非有限値の拒否、ドライバー変更。
- iOS Dart のスケジューラ/中断契約 3 件が合格。smoke harness の静的解析とヘッダー/API 整合性チェックも合格（CXX 未設定のため任意 C++ チェック 1 件をスキップ）。
- Flutter プラグインとネイティブ presenter trace を通した実再生 12 ケースが合格。一時停止中の 20、8、8.1、8 秒への seek はメディア時刻と generation の両方を更新しました。一時停止は固定され、オフセット、非表示 seek、空シーン、連続 seek も動作しました。約 1.51 秒間に 0.5/1/2x でメディア時刻はそれぞれ 0.759/1.492/3.005 秒進みました。
- 8 → 8.1 秒のネイティブ出力キャプチャは左に約 38 物理ピクセル移動しました。8 秒に戻すとピクセル単位で同一の画像になり、短い seek が trace データだけでなく描画出力に到達したことを確認しました。
- 回帰実行の render、import、audio failure カウンタはゼロでした。

機械可読の機能証拠は `ios-dfm-functional.json` にあります。

この修正は特定されたタイミング要因を解消しますが、Titan と同等の滑らかさや CPU/GPU 削減率を実証するものではありません。trace 実行は性能ベンチマークではありません。その後のバックグラウンド/オーバーヘッド測定はホストのロックにより中断され、対象外です。実機のリフレッシュ周期、バッテリー消費、バックグラウンド音声の回復は未測定です。

同じリフレッシュレートでは一定サイズのタイムスタンプ/mailbox 処理を追加し、追加の描画パスはありません。飽和時に利用可能な最新フレームを破棄せず描画すると使用率が上がる場合があります。一時停止は既存の画面リフレッシュ動作に従い、このパッチはアイドル/一時停止時の電力を最適化しません。

## 再現

リポジトリの通常のソースビルド手順で iOS シミュレーター用ネイティブ依存関係と release 静的ライブラリをビルドします。仮の Flutter iOS アプリを作成し、`packages/erika_flutter` を path 依存にして、`packages/erika_flutter/tool/ios_danmaku_smoke.dart` を `lib/main.dart` にコピーします。仮アプリの pubspec で `assets/test.mp4` を宣言します。ローカルテスト素材：

```sh
ffmpeg -f lavfi -i color=black:s=1280x720:r=24 \
  -f lavfi -i anullsrc=r=48000:cl=stereo -t 90 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac assets/test.mp4
```

Apple Silicon で arm64 のみの静的ライブラリを使う場合、仮アプリのシミュレータービルドと Pod ターゲットから `i386 x86_64` を除外します。アプリビルド時に `ERIKA_IOS_CAPI_STATICLIB` を新しい `liberika_capi.a` に設定してください。そうしないとプラグインが古いリリース済みバイナリを選ぶ場合があります。

機能確認では `ERIKA_DANMAKU_TRACE=1` と書き込み可能な `ERIKA_DANMAKU_TRACE_FILE` を設定して起動します。`simctl launch` では `SIMCTL_CHILD_` プレフィックスを使用します。harness は 8 秒で一時停止した状態から開始します。Flutter debug VM 接続後：

```sh
python3 packages/erika_flutter/tool/verify_ios_danmaku.py <VM-Service-URL> \
  --trace <native-trace-file> --output /tmp/erika-functional.json
```

trace は presenter 時刻とグリフ数を記録し、物理表示遅延は記録しません。スクリーンショットは別途確認します。今後の CPU/GPU 測定では trace ログを無効にし、新しいプロセスを使い、ホストをロックしないでください。この release ネイティブレンダラーと音動画負荷を NipaPlay の独立 debug overlay や密度の異なる Titan と直接比較しないでください。

起動中の iOS シミュレーターで mailbox テストを直接実行：

```sh
xcrun --sdk iphonesimulator swiftc -target arm64-apple-ios13.0-simulator \
  packages/erika_flutter/ios/Classes/ErikaTickMailbox.swift \
  packages/erika_flutter/test/ios_tick_mailbox/main.swift \
  -o /tmp/erika-ios-tick-mailbox-test
xcrun simctl spawn <UDID> /tmp/erika-ios-tick-mailbox-test
```
