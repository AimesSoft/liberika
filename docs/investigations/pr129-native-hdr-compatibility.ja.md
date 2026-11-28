# PR #129 の修正と検証記録

[中国語の記録](pr129-native-hdr-compatibility.zh.md)の翻訳です。

## 範囲

- 元の PR：AimesSoft/liberika#129、head `2c5e470fd33b4a7297fa5b15bb22cb65ace1899e`。
- 比較対象の main：`7b0fa9179d0cd3b85e28974844f213d0e00aa058`。
- ローカル修正ブランチ：`codex/pr129-preserve-native-hdr`。
- 通常再生は main のネイティブ Windows HWND / swapchain と HDR10 ネゴシエーション経路を引き続き使用し、Flutter texture で主経路を置き換えません。

## 修正結果

1. Windows の `ErikaVideoView` を `ErikaWindowOverlayVideoView` に戻し、blendMode、opacity、debugLabel、callback を渡します。既定の srcOver、auto / extendedLinear は暗黙に texture を作りません。
2. `ErikaTextureVideoView` のみが Windows SDR BGRA8 texture を明示的に選択します。srcOver は Flutter の透明度、クリッピング、フィルターをサポートし、overlay はネイティブ Composition に切り替わり、その他の Windows ブレンドモードは明示的にエラーになります。TextureLayer をまたいで機能しない canvas saveLayer ラッパーを削除しました。
3. PR の 2 入力 Composition effect graph と FittedBox の変換矩形修正は保持しました。
4. 新しい texture API をオプションでロードし、既存 surface の解除前に能力を確認します。旧 DLL に新しいシンボルがなくても通常のプレーヤーを作成できます。C ヘッダーの 2 つのコピーに新 API を同期して宣言しました。
5. GPU 出力はコピー完了後に二度と書き込まない共有スナップショットとして公開します。プラグインが保持するのは latest と raster callback で取得した sampled descriptor のみで、スキップされた resize スナップショットは回収できます。release_callback や固定フレーム数から Flutter の GPU サンプリング完了を推測できないため、「タイムアウトでスナップショットを強制削除する」提案は採用していません。
6. 画面内容または寸法が変化したときだけ新しいスナップショットを公開し、Flutter に通知します。内容判定には動画フレーム、seek generation、字幕 / HUD、弾幕、超解像モードを含みます。クロックの進行だけでは新しい画面としません。
7. プレーヤー破棄後に texture owner がクリアされない問題と、overlay HWND の置換前に旧オブジェクト参照が解除されない問題を修正しました。
8. グローバルな D3D11VA surface pool 拡張と上限のない retired-frame キューを撤回しました。`ffmpeg.rs` は上記 main と完全に一致します。追加の GPU handoff 待ちは Composition / texture のみに適用し、通常の不透明 HWND 再生には追加しません。

明示的な texture はデバイス間で安全に引き渡すため、画面の変化ごとに GPU スナップショットの割り当て、コピー、上限付き完了待ちが増えます。CPU ピクセルの読み戻しはありません。これらのコストは既定のネイティブ再生経路には入りません。

## 検証中に見つかった既存の問題

- C API の超解像状態テストは Android のみを特別扱いしていました。Windows D3D11 はデバイス未接続時に元から Building を報告する一方、テストは Inactive を期待していました。元の main でも同じ失敗を再現し、Windows のテスト期待値のみを変更しました。超解像の動作は変更していません。
- main の非同期弾幕 planner は破棄時に shutdown 通知を送るだけでスレッド終了を待ちませんでした。プラグインは最後のプレーヤー解放後に DLL をアンロードするため、まだ開始または終了していないスレッドがアンロード済みコードを実行する可能性があります。例外アドレスは Rust スレッド開始と割り当て関数に解決され、旧 v0.1.7 DLL のストレステストでも終了時例外を再現しました。修正は Windows の破棄時に限り弾幕 worker を保存して join し、待機前に共有状態ロックを解放します。通常の弾幕計画、再生、デコード、HDR 方針は変更せず、他プラットフォームの終了方針も従来のままです。

## 完了した検証

- `flutter test --no-pub --reporter expanded`：104/104。
- `flutter analyze --no-pub`：問題なし。
- `cargo test -p erika -p erika_capi --lib -- --test-threads=1`：コア 528/528、C API 41/41。合同実行では C API 依存の feature セットを有効にするため、コア単独の既定テストより件数が多くなります。
- `cargo build -p erika_capi`：DLL 生成成功。既存の unused / dead-code と LNK4098 警告は残りました。
- Windows プラグインを実際の Flutter SDK で完全にコンパイル、リンク。
- ネイティブ CTest：3/3（GPU publication、旧 runtime API 互換、新 runtime texture ライフサイクル）。
- 新 runtime の実プラグイン接続と DLL アンロードを連続 50/50 回通過。DLL を固定保持しません。
- GPU テストには WARP のピクセル読み戻しによるスナップショット不変性、スキップした resize 公開 100 回分の参照回収、10,000 回の並行公開 / 読み取り、idle / resize の内容無効化を含みます。
- `git diff --check`、`cargo fmt --all -- --check`：通過。

旧 runtime の互換テストは DLL を明示的に固定保持し、ロード、作成、能力確認、texture owner のクリーンアップのみを検証します。公開済みバイナリ内部のスレッド終了不具合が修正されたとは主張しません。ソース runtime テストでは実際に DLL をアンロードし、今回の終了競合修正を確認する必要があります。

ネイティブテストは独立して構成できます。

```powershell
cmake -S packages/erika_flutter/windows/tests -B target/windows-plugin-tests -A x64 `
  -DFLUTTER_ENGINE_DIR="<Flutter SDK>/bin/cache/artifacts/engine/windows-x64" `
  -DERIKA_COMPAT_DLL="<released v0.1.7>/erika_capi.dll" `
  -DERIKA_SOURCE_DLL="<this checkout>/target/debug/erika_capi.dll"
cmake --build target/windows-plugin-tests --config Debug
ctest --test-dir target/windows-plugin-tests -C Debug --output-on-failure
```

## リリースと受け入れの限界

- 当時の artifact manifest は v0.1.7 に固定され、ビルド済みライブラリには新 texture API と上記スレッド終了修正がありません。新機能には対応するソース runtime が必要です（リポジトリのソースビルドでは `ERIKA_FORCE_SOURCE_BUILD=1` を設定可能）。正式リリース前に対応する artifact とチェックサムをビルド・更新する必要があります。存在しないバージョンを捏造・指定していません。
- HDR ディスプレイでの実動画再生、NipaPlay 統合、実際の DWM Composition ストレステストは未実施です。ウィンドウなしの GPU / API テストはこれらの受け入れを意味しません。
- マージ前の推奨確認：既定の主コンポーネントによる通常 SDR / HDR10 再生と出力状態、pause / seek / 字幕 / 弾幕 / 超解像、ウィンドウ resize とディスプレイ間移動、透明な走査線 overlay、明示的 texture の pause 通知、クリッピング / 透明度、急速な resize、プレーヤーの切り替え / 終了。
- 変更は独立 worktree に保存し、ユーザーの main 作業領域にある既存変更には触れていません。元 PR ブランチへの push はユーザーの明示的な要求が必要であり、main にはマージしていません。

## 2026-09-07：Copilot 指摘に対する実質的な修正

- 確認済みの overlay 所有権エラーと CPU バッファーの重複コピーのみを処理しました。再現されていない `isTopmost=FALSE` フォールバックや診断文言の変更は追加していません。
- HWND の再生成では元 owner のバインドのみを復元し、順序のないコンテナーの最後の要素には依存しません。owner を明示的に切り替える際は旧 producer を先に解除します。旧プレーヤーから遅れて届いた detach / hide は現在の画面を隠したり共有 HWND を切り替えたりしてはいけません。owner が存在しない、または解除済みの場合、他プレーヤーを勝手に選択しません。
- 同じバインド済みプレーヤーを非表示後に再表示しても、一時的に owner マーカーがクリアされたことだけを理由に swapchain を再生成しません。既定のネイティブ HWND / HDR 経路、デコードプール、出力形式ネゴシエーションは変更していません。
- texture シーンのキャッシュは字幕 / HUD と弾幕それぞれの実際の内容に応じて更新し、新しい動画フレームによって未変更の CPU ピクセルバッファーやインスタンスリストを再度ディープコピーしません。正確な内容比較を維持し、更新漏れの可能性があるハッシュや動画フレーム番号だけの判定は導入していません。弾幕 atlas は既存の Arc 共有を引き続き使います。
- 新テストでは連続する動画 120 フレームで同一キャッシュ割り当てを再利用しつつ、字幕ピクセル、alpha、弾幕位置、クリア操作では正しく無効化されることを確認します。Windows ネイティブテストは隠し HWND と実プラグインの状態ロジックを用い、owner の保持、排他的な引き渡し、遅延解除、owner 不在、バインド失敗をカバーします。差し替えたのは surface attach / detach の C 入口のみで、実際の DWM 画像効果はテストしていません。
- 今回の完全な回帰：Flutter 104/104、analyze 問題なし、合同 Rust コア 529/529、C API 41/41、プラグインビルドと native CTest 3/3。
