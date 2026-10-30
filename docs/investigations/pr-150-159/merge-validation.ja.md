# 2026-10-13：main ブランチ統合と実行検証

[中文](merge-validation.zh.md) · [English](merge-validation.en.md)

統合 PR：[AimesSoft/liberika #164](https://github.com/AimesSoft/liberika/pull/164)。ベースライン：`14c1d5511a0a16c34cf2c5f5fe9132d1f17fce65`。最終製品コード：`211f347acf9d67fb23424d98de436d87931f9318`。後続コミットは検証ホストと証拠のみ更新しました。Windows/Linux の実ビルドは `051bfa4` からで、`211f347` は x86_64 OHOS 専用の拡張除外のみを追加しました。元のワークスペースを保持し、すべての変更とビルドはレビュー worktree で実施しました。

## 採用範囲とプロジェクト目標

#150、#151、#153、#154、#155、#156、#158、#159 の有効な部分を採用し、HTTP 読み取り、メディア識別、タイムスタンプ入力への耐性、WASAPI 出力、D3D11 リソース再作成、一時停止/シーク意図、OHOS オブジェクト寿命、公開字幕解析をそれぞれ改善しました。いずれも組み込み再生コアの信頼性に関わります。採用コードのコミットには元の作者を明記しました。

#152 の HDR フィルタは撤回しました。生成した実ファイルは必要な異常マスタリング輝度を FFmpeg 経由で公開せず、対象経路を実証できなかったためです。#157 は不採用です。同期 AudioQueueDispose の競合の前提と修正の完全性のいずれも実証されていません。CoreAudio、Metal など Apple の実行検証はユーザーに委ねました。無効な Linux タイムアウト、FIFO 上の無効な補償、重複する Dart ライフサイクル修正、任意の GIF フレーム数上限も統合していません。各根拠は [元レビュー](review.zh.md) を参照。main に既存の、非ゼロオフセットに対する HTTP 200 全ファイル応答のプレフィックス読み飛ばしロジックは保持しました。

## 実行結果

以下のホストは製品版ランタイム、デバイス API、ウィンドウシステムを呼び出しました。ユニットテスト数は別に記載しており、これらの実行の代わりにはなりません。

| 環境 | 実行内容と結果 | 証拠 |
|---|---|---|
| Windows / D3D11VA | 実 DLL、HWND、ローカル BBB、HTTP 206、Range 無視の全ファイル 200。64 KB キャッシュで非ゼロオフセット要求を強制。一時停止、EOF シーク、後方シーク、再開、倍速、不正入力。最終 463 direct/zero-copy フレーム、990 回表示、import/render エラーはともに 0 | `merge-pinned-windows-live.log` |
| Windows / WASAPI | Remote Audio 利用時に製品版既定 endpoint への送信確認が成功。カバー付き MP3 は 30720 フレームを実消費（ホストは 1.5 倍速を継続）、音声エラー 0。その後実 WASAPI ループバックで非ゼロ PCM を取得 | `merge-wasapi-endpoint.log`、`merge-windows-live.log`、`merge-wasapi-loopback.wav` |
| Linux / X11、Mesa、PulseAudio | ネイティブ demo が 278 video / 144000 audio frames を出力。独立 C ABI ホストで表示再接続、シーク、リサイズ、再開、破棄を実行。実 BBB を 287 フレーム表示し、エラーはすべて 0 | `merge-final-linux-desktop.log` |
| Linux / libass | 実際の外部 SRT はシーク後に短く再開してから再停止し、スクリーンショットに 2 番目の字幕 “Second” が表示。カバー付き MP3 は音声トラックのみ公開し、48000 フレームを消費、キューは空、音声エラー 0 | `merge-final-linux-subtitle.log`、`merge-linux-subtitle-second.png` |
| OHOS / 公式 API 18 エミュレーター | 実 HAP、XComponent、現行 Rust runtime、2 つの完全な製品版 NAPI ブリッジで以下の全手順を完了し、厳密に検査した結果を出力 | `merge-api18-ohos-live-summary.json`、2 つの JSONL、スクリーンショット |
| OHOS / 公式 API 24 エミュレーター | 同じ最終 runtime と HAP で新しい公式イメージ上の同じ手順を完了。修正前の初期化クラッシュログも保存 | `merge-api24-ohos-live-summary.json`、2 つの JSONL、スクリーンショット、`merge-api24-cppcrash.log` |

OHOS の両ブリッジで H.264+AAC 再生、Paused 確認後の連続カウンタ観測、一時停止中の EOF シーク、3.5 秒への復帰、1.25 倍速での再開、実 NativeWindow の detach/attach、5 秒のカバー付き MP3、64 回の実作成/破棄、旧 ID の呼び出しと破棄、新インスタンスでの再生を実施しました。集計器は全段階の完了、一時停止中のデコード/音声消費の安定、EOF シーク後も Paused、再開後のデコード継続、MP3 に動画フレームがなく音声キューが空、旧 ID の拒否、新インスタンスの音動画出力継続、全エラーカウント 0 を要求しました。4 組のブリッジ/イメージ実行はいずれも MP3 を 235190 フレーム消費しました。生カウントは JSON 集計にあります。

## 実行によって判明した修正とホストの訂正

1. API 18 エミュレーターの GLES アダプタは inter-stage variables が 7 個までで、WebGL2 ベースラインの 15 個の要求により実際の nativeCreate が 0 を返しました。GL パスはアダプタの上限で要求し、現行動画シェーダーは作成・表示できます。
2. 新しい API 24 イメージの Vulkan loader は cooperative matrix 拡張を宣言しつつ、プロパティ問い合わせ入口で SIGABRT しました。x86_64 OHOS のみで任意拡張の問い合わせと公開を抑止しました。動画描画では使用せず、ARM64 は影響を受けません。
3. 64 回の device/renderer 割り当てが旧ホストの全体再生期限を超えることがありました。新インスタンスの 5 秒観測期間は実際の open 完了後から測定します。
4. 一時停止は非同期制御で、停止前のスナップショットからコマンド実行までにデバイスがさらに消費する場合があります。ホストは Paused 確認後にサンプリングし、さらに 1 秒観測してデコードと音声読み取りカウントの安定を要求します。一時停止の検証は削除していません。早期収集や途中停止の実行は最終合格証拠に含めません。

## 成果物と録音

| 成果物 | SHA256 |
|---|---|
| Windows 最終実行 DLL | `1589cdf8f8e6a62afbff9886a8cf914d4d1df1e7afbdcec96cac7dc36736722f` |
| Linux 実行 shared library | `90a54b9d7d2fae8d87b74ce85ca938c1770421c5674b760582669e6052c92766` |
| 両 OHOS イメージで実行した Rust runtime | `61048133599712676166b807456799ae172cb8077cb6df05f53c716f0743561b` |
| OHOS Flutter 製品版ブリッジ | `f1329a2f9405a8f894491a72c54aecb2f4f303e9ceced786124704dd1b090ecb` |
| OHOS ArkTS 製品版ブリッジ | `6a8737bc254916797683935d60e9099a5f617e9158e5d145f2b588192b40e7f2` |
| 最終の両イメージで使用した HAP | `f70b1cdda011e6fde7ce95f394b4775d5ca2abf0dbfefed8e818af24ad0030c0` |
| WASAPI ループバック WAV | `4166e50512f96f3c03d7b9273a55948258826e4d3f1833ce5b80617c280e036b` |

ループバック WAV は 48 kHz / ステレオ / 16-bit / 96000 フレームです。再読み取り時の RMS は `0.01962782635`、ピークは `0.09189123203`。最初の float キャプチャ出力は RMS `0.01963214017`、ピーク `0.09189660847`、音声消費 54176 フレーム、エラー 0 でした。この成功テキストは後の失敗した再実行で上書きされたため、float 値は当時のツール出力に基づき、WAV が保存された原証拠です。録音ホストはブロッキング取得中に動画 tick を処理せず D3D デコーダーフォールバック警告が出たため、実音声出力の証明にのみ使用します。最終 D3D 動画検証は別の継続 tick ホストを使用しました。

## 回帰チェックと失敗した試行

Windows：712 core + 4/6 integration + 42 C API が成功、1 ignored。Linux：687 core + 4/6 integration + 41 C API + 26 xtask が成功、2 ignored。Flutter は 113 成功、analyze に問題なし。packaging は製品版 C++ ブリッジの実行を含む 5 項目が成功し、format と license チェックも成功しました。最終リモート CI は [PR #164 のチェック](https://github.com/AimesSoft/liberika/pull/164/checks) を正とします。

Windows の初回既定並列スイートは P010 ピクセル比較差（60 vs 65）を示しましたが、main の該当ユニットテストと本ブランチの Vulkan シングルスレッド完全スイートは成功しました。初回失敗ログを保存し、修正済みの製品欠陥として扱っていません。HDR ファイルのプローブ失敗ログも保存しています。後の WASAPI 再実行は既定 endpoint 消失により `0x80070490` で失敗し、最後の DLL 動画実行は明示的に `--video-only` を使用しました。成功した WASAPI 製品コードはその後変更されていませんが、endpoint がない再実行を音声成功には数えていません。

## 検証範囲の限界

- Apple の実行証拠はありません。プラットフォームのコンパイル成功はユーザーの Apple 検証の代わりにはなりません。
- Windows で実際の TDR を誘発せず、実障害デバイスの再接続も未検証。D3D11 は任意の C API screenshot を実装していません。
- Linux は Mesa llvmpipe / Pulse null sink を使用し、物理 VAAPI/NVDEC やスピーカー聴感の証拠はありません。
- OHOS は公式 x86_64 エミュレーターで、システム decoder は CPU fallback で表示しました。実機ゼロコピーの証拠はありません。ArkTS HAP は Flutter の完全な製品版 NAPI ブリッジを呼び、完全な Flutter エンジンや NipaPlay は実行していません。
- 一時停止状態のみでの字幕シークプレビューは別途未解決の観測です。成功した画面は短い再開を含み、その問題が修正されたとは言えません。
- #159 は公開 Rust テキスト解析 API を変更しました。通常の外部字幕は FFmpeg を通るため、実字幕再生はその経路が壊れていないことだけを示します。

完全な生証拠と成果物はローカル `C:\Users\rgliang\AppData\Local\Temp\erika-pr-review-20261005` にあります。リポジトリの [evidence](evidence/) は検証可能なログ、JSONL、スクリーンショット、ループバック録音を保存します。再現手順は [scripts](scripts/README.md) を参照してください。
