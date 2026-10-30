# PR #150–#159：目的、判断と実行時の検証

[中国語原文](review.zh.md)の翻訳です。日付、コミット、環境ごとの検証範囲は原文を基準とします。

調査日：2026-10-05。HarmonyOS 実行検証の追加：2026-10-08。最新のメインブランチ統合：2026-10-13。<br>
リポジトリ：AimesSoft/liberika。作者：TsangAsuna。<br>
評価基準：`4390c78f4467e2047e19690700fbead6a18d8e31`。<br>
再生カーネルのレビューコミット：`004b118`。エミュレーターのビルド対応と実行証拠は、ブランチの後続コミットを参照。<br>
ブランチ：`codex/pr-150-159-reviewed`。

Erika は NipaPlay の組み込み再生カーネルで、デコード、タイミング、レンダリング、字幕、弾幕、音声出力を担当する。ここでは実際の再生の信頼性、ホストのライフサイクル、プラットフォーム間の一貫性、可観測性に基づいて貢献を評価する。PR の文体から生成モデルを特定できる証拠はなく、それによって変更の価値を判断することもできない。

**結論：8 件の PR の部分的な変更をマージ候補とした。#152 は実際に影響を受ける入力がなく、#157 は問題の前提がまだ成立していない。** #152 と #159 の現在の製品上の価値は比較的低い。後者は通常の外部字幕が通る FFmpeg 経路を変更しない。残すべき部分を独立したコミットに整理し、元の実装の不足も数か所修正した。統合 PR は [#164](https://github.com/AimesSoft/liberika/pull/164)。最新の検証は [2026-10-13 統合検証](merge-validation.zh.md)を参照。以下のコミット、件数、環境は初回レビューと 10 月 8 日の検証記録であり、最終的なメインブランチ統合の結果は最新の検証を基準とする。

## 各 PR の評価

| PR | 解決したい問題 | プロジェクトへの意義 | 対応 |
|---|---|---|---|
| [#150](https://github.com/AimesSoft/Erika/pull/150) | HTTP Range の過剰応答、不正な再開位置、不明な長さの繰り返し探査 | 高：メモリ浪費、キャッシュのずれ、重複リクエストを防止 | 採用。要求バイトを受信した後もサーバー応答の末尾を待つ問題も修正。 |
| [#151](https://github.com/AimesSoft/Erika/pull/151) | カバー画像を動画と誤認、decoder のタイムベース契約、破損ビットマップ字幕 | 高：メディアの正しい識別と破損データへの耐性 | 3 項目とも採用。実際の MP3 のカバー画像は動画トラックとして公開されなくなった。 |
| [#152](https://github.com/AimesSoft/Erika/pull/152) | 不合理な HDR mastering ピークがマッピングを誤らせる | 低〜中：妥当なメタデータのフォールバックだが、実際の不正な入力がない | 保留。予備レビューのフィルターは今回の統合から撤回。 |
| [#153](https://github.com/AimesSoft/Erika/pull/153) | 字幕や弾幕のタイムスタンプで panic | 高：入力データによって組み込みカーネルがクラッシュすべきではない | 実際に表現可能な範囲を検査。元の有限値チェックでは `1e100` を防げない。 |
| [#154](https://github.com/AimesSoft/Erika/pull/154) | 音声スレッド異常終了、倍速ブリッジのデバイスキュー漏れ、CoreAudio の部分サンプル誤消音 | 高。ただしプラットフォーム別の対応が必要 | WASAPI を完了し、キュー統計と回復予算を修正。CoreAudio は Apple 検証へ引き継ぎ。 |
| [#155](https://github.com/AimesSoft/Erika/pull/155) | GPU 再作成、インポート診断、HDR スクリーンショット、Linux 待機、OHOS フレーム落ち診断 | Windows 部分は高。他は一括して承認できない | D3D11 と OHOS trace を採用。Linux 待機は実際には時間制限されず、Metal capture は別途修正と検証が必要。 |
| [#156](https://github.com/AimesSoft/Erika/pull/156) | 一時停止中の seek、未実行の pause 意図、失われる resume コマンド | 高：ユーザー操作に直接影響 | 再現可能な状態修正 3 件を採用。FIFO 飽和時にメッセージを失う元の「補償」は不採用。 |
| [#157](https://github.com/AimesSoft/Erika/pull/157) | iOS AudioQueue の破棄とコールバック解放の競合 | 未証明：Apple の同期 disposal 契約と前提が矛盾 | 不採用。Apple での実際の再現が必要で、カウンター自体も入口の時間窓を保護しない。 |
| [#158](https://github.com/AimesSoft/Erika/pull/158) | 古い OHOS ID が新プレーヤーを指す、Flutter 遅延イベント、GIF 上限 | OHOS ID 修正は高。他の 2 件は根拠不足 | 両ブリッジの単調増加 ID を採用。既存 Dart のライフサイクルは安全で回帰テストを追加し、任意の GIF 制限は導入しない。 |
| [#159](https://github.com/AimesSoft/Erika/pull/159) | 空白・Tab 区切りの SRT/VTT を組み込みパーサーが結合 | 低：公開 Rust API の修正。通常再生は FFmpeg を使う | 小さな修正を採用。プレーヤーの外部字幕が修正されたとは主張しない。 |

### #150：HTTP 読み取りの契約

ベースラインでは 8 バイトを要求した際、過剰に返された応答から 64 バイトをキャッシュした。元の PR によるキャッシュサイズ制限には価値があるが、サーバーが要求分を送った後に応答を終了しない場合、なお末尾を待ち続ける。レビュー版は要求範囲を満たした時点で直ちに停止する。0 以外の再開オフセットに対する 206 応答では Content-Range の検証が必要。長さ不明の探査結果もキャッシュできるが、一時的なエラーはキャッシュしない。実際の localhost HTTP サーバーとベースライン／元 PR の比較を含む source テスト 71 件が通過した。Windows C ABI の完全な再生検証には、別途、実際の H.264 ファイルと Range サーバーを使用した。

コミット：`3123f52`。証拠：`pr150-baseline.log`、`pr150-original.log`、`pr150-reviewed.log`。

### #151：メディア識別と decoder の契約

ベースラインではカバー画像付き MP3 が mjpeg の動画トラックを公開した。レビュー版では実際の音声トラックのみを公開し、長さは 1 秒のまま。pkt_timebase の明示的な設定は契約の修正である。破損した bitmap rect は rect 単位でスキップし、同じパケット内の有効な字幕を残せる。FFmpeg テスト 45 件が通過。Windows の実 DLL と Linux の実 shared library はどちらもこのファイルを開き、トラックを確認した。Linux ネイティブ demo は別途、カバー画像付き 5 秒 MP3 から音声フレーム 144480 個を消費した。demo の終了時チェックは動画フレームを要求するため、この音声専用入力の終了コードは 1 であり、demo コマンド全体を成功と報告することはできない。

独立した Python C ABI ホストでは、1.5 秒の観測時間では消費が 0 だったが、4 秒に延長すると 48000 フレームを消費し、キューが空となり、音声エラー 0 で 1 秒 MP3 の実出力が完了した。短い観測時間だけを根拠に再生障害と断定してはならない。

コミット：`4d01f1b`。証拠：`pr151-baseline-probe.log`、`pr151-reviewed-probe.log`、`final-linux-cover-audio.log`。

### #152：HDR の妥当性判定

最大 mastering 輝度が 1 nit 未満である値は HDR ピークとして不適切だが、最低輝度が 1 nit 未満なのは正常であり保持しなければならない。frame/HDR テスト 35 件は通過したが、実際に異常な mastering 情報を持つファイルと HDR ディスプレイでの比較には代わらない。元 PR は不正なソースを示しておらず、0.0001 nit が「既知の sentinel」であるという主張にも検証可能な根拠がない。10 月 13 日に異常な mastering ファイルの生成を試みたが、FFmpeg が必要な輝度メタデータを公開せず、実際に影響を受ける経路は確認できなかった。そのため今回の統合では `0004667` で変更を撤回した。過去のテストログは残すが、マージの根拠にはしない。

コミット：`bf23517`。証拠：`pr152-reviewed.log`。

### #153：Duration で表現可能な範囲の確認

有限値であっても表現可能とは限らない。元 PR は有限の `1e100` で Duration の生成時に panic する。レビュー版は `Duration::try_from_secs_f64` を使い、弾幕の負数を 0 にする既存の意味を保ちながら、NaN、無限値、範囲外の値を拒否する。字幕の時間・分・秒の加算には checked arithmetic を使う。弾幕テスト 93 件と字幕テスト 54 件が通過した。実際の Windows C ABI で不正な弾幕を読み込んでもクラッシュしなかった。不正な外部 SRT はエラーを返したが、この経路では FFmpeg が拒否したため、組み込み字幕パーサー修正の実行証拠とはいえない。

コミット：`b6c36d2`。証拠：`pr153-original.log`、`pr153-reviewed-danmaku.log`、`pr153-reviewed-subtitle.log`。

### #154：WASAPI のキューと回復

元 PR は書き込み前の padding を記録していたため、実際のキューが 4800 フレームでも 1200 フレームと報告した。レビュー版では ReleaseBuffer 成功後に padding と今回の書き込み量の合計を報告する。reopen が成功するたびに回復予算をリセットしてもいけない。reopen は成功しても render が毎回失敗すれば、元の方式は無限ループする。現在は render の成功時にのみ回復完了とする。ネイティブ Windows COM インターフェイスの harness で実装の GetCurrentPadding／GetBuffer／ReleaseBuffer を呼び、4 項目が通過した。実 ABI とのやり取りではあるが、制御された endpoint を使っており、実際のサウンドカード障害の代わりにはならない。

初回レビュー時、このマシンのデフォルト音声 endpoint の問い合わせは `0x80070490` を返した。10 月 13 日に Remote Audio が利用可能な間、実際のデフォルト endpoint への送信、カバー画像付き MP3 の消費、WASAPI ループバック録音を完了した。その後 endpoint が切断され、再検証は失敗した。実デバイスの障害からの再接続は未検証。正確な結果と録音ハッシュは最新の統合検証を参照。CoreAudio については [Apple 向け引き継ぎ](apple-handoff.zh.md)を参照。

コミット：`84d62de`。証拠：`pr154-original-com.log`、`pr154-original-native.log`、`pr154-reviewed.log`。

### #155：プラットフォームごとに分けて評価

D3D11 は device removed の検出後、デバイスに結び付く状態をすべて消去し、既存の経路で新しいデバイスを作成する。インポート失敗のカウントも有用である。実際の WARP デバイスで無効化、新デバイス作成、Flutter target の再作成、ピクセルの読み戻しを実行した結果、リソースの identity は変わり、白いピクセルは一致した。D3D11 テスト 20 件が通過。実際の Windows D3D11VA 再生では別途 direct／zero-copy フレームを 289 個処理し、インポートと描画のエラーは 0 だった。実際の TDR は誘発していないため、実 GPU リセットからの復旧は未検証である。

Linux の 100 ms poll は、内部の release で行われる barrier、無期限 fence wait、DeviceWaitIdle を制限しないため、約束された時間制限は実現できない。一方、これだけで UAF とも断定できない。同一キューの barrier は解放前に待機する。Metal の BGRA8 capture target は EDR モードの RGBA16Float pipeline cache と一致しない可能性があり、専用 capture pipeline と alpha の検証が必要。OHOS の textureless diagnostic は小さく、保持できる可観測性の改善である。

コミット：`4adc1b4`。証拠：`pr155-reviewed-windows.log`、`final-windows-live-capi.log`。

### #156：実際の状態修正と無効な補償

ベースラインでは、一時停止中の seek が最終フレームを越えると Ended に変わる問題と、worker の実行中クロックが一時停止の意図を上書きする問題をそれぞれ再現した。レビュー版は Paused を維持し、decode-suspended 中には完了できないプレビューを放棄し、未実行の pause シーケンスにも停止したクロックを与える。Windows で実際のローカル／HTTP H.264 再生を使い、一時停止、10 秒の EOF への seek、3.5 秒への戻り、再開、倍速を実行した。EOF seek 後も公開状態は Paused のままで、一時停止中に decodedVideoFrames は増えなかった。

元 PR の FIFO 送信タイムアウトへの「補償」は、同じ満杯のキューで try_send するため、どちらの送信も失われ得る。決定的なキュー飽和検証で 2 件が失敗したため、この部分は採用せず、transition resume の飽和問題が解決したとも主張しない。playback のフィルターテスト 96 件、core のフィルターテスト 119 件、および最終的な Windows／Linux の関連完全スイートが通過した。プレビューテストは実際の経過時間による期限と yield を使い、密なポーリングで非同期 demux が飢餓状態になることを防いだ。

コミット：`e7cf59e`、`004b118`。証拠：`pr156-baseline-preview.log`、`pr156-baseline-clock.log`、`pr156-original-saturation.log`。

### #157：前提は未証明で、解決策も不完全

現在の呼び出し箇所はいずれも `AudioQueueDispose(queue, true)` を使う。[Apple の公式契約](https://developer.apple.com/documentation/audiotoolbox/audioqueuedispose(_:_:))では true は同期的 disposal を意味し、戻った後はコールバックが実行されない。作者はこの契約に反する再現例を示していない。仮にそのような実装上の不具合があっても、コールバックはカウンターを増やす前に user_data を参照するため、入口の時間窓はカウンターで保護されない。有限時間の待機と state のリークを行う変更は、この根拠だけではマージできない。検証要件は [Apple 向け引き継ぎ](apple-handoff.zh.md)を参照。

コードのコミットはない。作者の CI は iOS／tvOS をコンパイルしており、プラットフォームチェックがなかったと記すのは誤り。ただしコンパイルはこの競合の証明にはならない。

### #158：OHOS ID は実質的な改善だが、他の 2 件を同時に採用する根拠は不足

2 つの NAPI ブリッジは以前 presenter のアドレスを ID として使っていたため、アロケーターがアドレスを再利用すると、古いホストの呼び出しが新しいプレーヤーに届き得た。他のホストと同じ方向性で、プロセス内で単調増加する ID に変更した。host harness で presenter が毎回同じアドレス 0x10 を返すよう強制し、2 つの実装ファイルの NativeCreate、FindPlayer、NativeDestroy を実行した。ベースラインはどちらも失敗し、修正後は古い lookup／destroy が新オブジェクトに影響しなかった。packaging チェック 5 件はすべて C++ を実行しており、スキップはない。

SDK 5.1.0.107／API 18 をインストールした。このブランチの Rust C API runtime を ARM64 と x86_64 向けにビルドし、2 つの完全な実運用ブリッジをいずれもその runtime にリンクした。以前の v0.2.1 release にリンクしたビルドログは過去の証拠としてのみ残す。DevEco Studio 26.0.0.851 と公式 HarmonyOS 5.1.0.110 の携帯電話イメージをインストールし、ローカルの x86_64／API 18 エミュレーターを hdc で接続して、実際の HAP をインストール・実行した。

2 つの実運用ブリッジそれぞれで H.264＋AAC の再生、一時停止、一時停止中の EOF への seek、3.5 秒への seek、1.25 倍速での再開、Surface detach／attach、カバー画像付き MP3、破棄と再作成、古い ID の呼び出しを実行した。それぞれ実際に 64 回作成・破棄した後、古い ID 2 の stop は unknown player を返し、古い destroy は新しい ID 67 に影響しなかった。新しいプレーヤーはいずれも映像と音声を出力し続け、描画／インポート／音声エラーはすべて 0 で、スクリーンショットも確認した。これは ArkTS HAP が Flutter の完全な実運用 NAPI ブリッジを呼び出した検証であり、完全な Flutter エンジン／NipaPlay アプリケーションの実行を証明するものではない。

さらに、同じ現在の Rust runtime をリンクしたまま両ブリッジをベースラインに戻し、それぞれ実際の作成・破棄を 128 回試みたが、指定した古いアドレスは再利用されなかった。この結果を実環境におけるベースライン障害の再現と記すことはできない。アドレス再利用の欠陥についての決定的な証拠は、上記の強制再利用を行う実運用 C++ harness にある。詳しい件数と再現方法は [HarmonyOS 実行記録](ohos-runtime.zh.md)を参照。

Flutter の既存 Dart コードは、dispose の待機中に届くイベント、ストリームのクローズ、遅れて届く 100 件のイベント、未知の ID をすでに正しく処理する。113 テストが通過し、analysis に問題はなかった。StateError を一律に握りつぶす必要はない。GIF の 36000 フレーム固定上限は任意の製品制約であり、キャンセル、進捗、明示的な予算の設計に代わるものではない。

コミット：`627b846`。証拠：`pr158-baseline-ohos.log`、`pr158-reviewed-ohos.log`、`ohos-arkts-review.jsonl`、`ohos-flutter-review.jsonl`、`ohos-live-summary.json`。

### #159：公開パーサーの改善であり、プレーヤーへの効果は限定的

空白のみの行で SRT/VTT を分割する修正は公開 `parse_srt`／`parse_webvtt` API に適用され、スペース、Tab、LF／CRLF、複数行 cue をカバーする。ベースラインでは 2 つの cue が結合され、修正後は字幕テスト 56 件が通過した。ただし `ExternalSubtitleSession::open` は Demuxer＋FFmpeg SubtitleDecoder を使い、組み込みテキストパーサーはその経路に接続されていない。実際の Linux C ABI＋wgpu＋libass では、空白で区切られた SRT から 3.5 秒に「Second」を表示し、スクリーンショットを保存・確認した。この実行経路は #159 のパーサー変更の影響を受けない。

一時停止したまま seek したプレビューでは cue が表示されず、短く再開してから再度一時停止すると表示された。この現象は別途調査が必要であり、パーサーテストやピクセル差分を一時停止中の字幕プレビュー全般の成功とみなしてはならない。スクリーンショット比較には track switch に伴う動画更新も含まれるため、changed-pixel 数は字幕の面積ではない。

コミット：`28e949a`。証拠：`pr159-baseline.log`、`pr159-reviewed.log`、`final-linux-subtitle-pixels.log`。

## 初回レビューと 10 月 8 日の実行結果および検証範囲

| 環境 | 実際に実行した内容 | 結果と限界 |
|---|---|---|
| Windows の実 D3D11VA | 非表示 HWND＋実 DLL、実際の BBB をローカル／localhost HTTP Range で再生。一時停止／EOF seek／再開／倍速 | 289 direct／zero-copy、654 rendered、import／render errors は 0。クリップに音声トラックはない。 |
| Windows WARP | device の無効化、新 device と Flutter targets、ピクセルの読み戻し | 20 件通過。実際の TDR は誘発していない。 |
| Windows WASAPI | 実 COM ABI、制御された endpoint | 4 件通過。初回レビュー時はこのマシンにデフォルト endpoint がなく、実サウンドカードは未検証。 |
| Linux X11＋Mesa＋PulseAudio | 独立したネイティブウィンドウで 5 秒の音声・映像。独立した C ホストで一時停止、表示の再接続、seek、resize、再開、destroy | 262 video／144000 audio。C ホストは 328 video／103200 audio。エラー 0／0。 |
| Linux の実 H.264 | 実際の BBB ファイルを FFmpeg でデコードし wgpu で表示 | 295 rendered、エラー 0。ソフトウェア llvmpipe であり VAAPI／NVDEC のハードウェア証拠ではない。 |
| Linux 字幕 | 実 shared library＋Xlib＋libass、外部 SRT、seek、再開後の一時停止、RGBA capture | 画面の「Second」を目視確認。一時停止のみのプレビューは通過しなかった。 |
| Linux カバー画像付き MP3 | 独立 Python C ABI ホスト、実 MP3 デコード、PulseAudio 出力 | 48000 audio frames、キューは空、audio errors 0。null sink では実際の聴感を確認できない。 |
| Linux PulseAudio | 本番 native pause／resume／flush／reconfigure テスト＋null sink | 通過。null sink はスピーカーからの聴感を証明しない。 |
| OHOS 公式携帯電話エミュレーター | 現在の Rust runtime＋2 つの実運用ブリッジ、実 HAP／XComponent、H.264＋AAC、一時停止／EOF seek／倍速／Surface 再接続／カバー画像付き MP3／破棄と再作成 | ArkTS 518 rendered、Flutter 517 rendered。MP3 はそれぞれ 235190 音声フレームを消費。古い ID は拒否され、エラーはいずれも 0。システム decoder の出力は CPU フォールバックで表示され、実機でのゼロコピーの証拠ではない。 |
| Apple | このマシンでは未実行 | ユーザーが別途対応するため、完了とは記せない。 |

使用した BBB ファイルは 969201 バイト。SHA256：

`18b99ec25f32f6bd2223aa54e4b5632533328bf5cc81c283eba7604c42649f75`。

ダウンロード元：

<https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/720/Big_Buck_Bunny_720_10s_1MB.mp4>

元動画は Blender Foundation / Big Buck Bunny によるもので、メディアのメタデータには CC BY 3.0 と記されている。

## 初回レビューの回帰テスト概要

Windows：erika 単体テスト 702 件通過／1 ignored、integration 4＋6 件通過、C API 42 件通過。Linux：erika 単体テスト 677 件通過／2 ignored、integration 4＋6 件通過、C API 41 件通過。Flutter 113 件通過、flutter analyze に問題なし。packaging 5 件通過、Rust fmt 通過。HarmonyOS エミュレーターの依存関係ビルドに x86_64 target 対応を追加し、xtask 26 件が通過、ARM64／x86_64 の実ビルドも両方通過した。Windows clippy は完了したが既存 warnings があり、`-D warnings` では実行していない。

`--workspace` 全体は Apple-only demo の ObjC ビルドスクリプトで失敗し、後続の検証はコアと C API に限定した。Linux synthetic Vulkan direct-frame チェックは llvmpipe に synchronization2／external-semaphore の機能がないため失敗しており、この経路が検証に通過したわけではない。#155 の Linux コードは採用していない。

元作者の 10 件の PR は fork CI がすべて成功し、実行 SHA はレビュー対象の head と完全に一致した。詳細は [author-ci.json](author-ci.json)を参照。CI の成功は上記の機能面・実行面の判断の代わりにはならない。

## 再現資料

[evidence](evidence/) にはベースライン、元実装、レビュー版の対象ログと実行ログを保存する。[scripts](scripts/) には独立ホストと SDK のビルドコマンドを保存する。説明中のツール、メディア、パスに依存し、mock／コンパイルチェックをプラットフォーム実行テストとして扱わない。

このマシンの完全な元の証拠、メディア、ビルド成果物、SDK は `C:\\Users\\rgliang\\AppData\\Local\\Temp\\erika-pr-review-20261005` にあった。元のワークスペースは変更せず、すべてのコードは分離された worktree 内に置いた。
