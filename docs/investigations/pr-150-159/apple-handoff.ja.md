# Apple 検証の引き継ぎ：#154、#155、#157

[中国語の記録](apple-handoff.zh.md)の翻訳です。基準は `4390c78f4467e2047e19690700fbead6a18d8e31`、PR の head SHA は `author-ci.json` を参照してください。調査機には Apple プラットフォームの実行環境がなく、以下の 3 項目は Windows / Linux の結果で代替できません。

## #154：CoreAudio 部分読み取り時の無音フラグ

目的は妥当です。ring が実サンプルを部分的に返し、残りをゼロ埋めするとき、バッファー全体を `OUTPUT_IS_SILENCE` にしてはいけません。`read_frames` が 0 のときだけフラグを設定します。この部分はレビュー用ブランチには取り込まれていません。

検証手順：

1. 1 回の callback 要求より少ない非ゼロ PCM を ring に入れ、実際の CoreAudio callback を発生させます。
2. 実サンプルの先頭部分が保持され、末尾がゼロで、`OUTPUT_IS_SILENCE` が設定されていないことを確認します。
3. 空の ring では全体がゼロ出力になり無音フラグが付くことを確認します。
4. 音声・映像を再生し、pause / resume、倍速、underrun を含めて音と音声クロックを確認します。

## #155：Metal SDR キャプチャと packed alpha

目的は妥当ですが、既存パッチはそのまま採用できません。BGRA8Unorm キャプチャターゲットには対応する pipeline が必要です。EDR 時の live drawable は RGBA16Float の場合があり、キャッシュした live pipeline を異なるターゲット形式にそのまま使えません。キャプチャ専用 pipeline / cache を用意し、SDR マッピングと alpha uniforms を明示的に選択します。

検証手順：

1. EDR を有効にした実際の Metal layer で HDR 動画を再生し、SDR PNG にキャプチャします。Metal validation で形式不一致がないことを確認します。
2. 明示的な SDR トーンマップ基準画像と中間調・ハイライトを比較します。PNG の生成やバイト数の確認だけでは不十分です。
3. side-by-side / top-bottom の packed-alpha 動画を使い、スクリーンショットの色、透明部分、乗算済み alpha を確認します。
4. 同一プレーヤーで EDR 再生、SDR キャプチャ、resize、再キャプチャを交互に行い、キャプチャが live 出力モードや pipeline に影響しないことを確認します。

## #157：先に再現し、その後に解放プロトコルを設計

本番コードの呼び出し箇所はすべて `AudioQueueDispose(queue, true)` を使用します。Apple の公式文書は true を同期 disposal と定め、復帰後は callback が実行されないとしています。

<https://developer.apple.com/documentation/audiotoolbox/audioqueuedispose(_:_:)>。

「既存 callback が dispose の復帰後も状態にアクセスする」という作者の前提には実行時の証拠が必要です。提案されたカウンターも、callback が `user_data` を読み取ってからカウントを増やすまでの隙間を保護できません。100 ms 待機してリークする可能性がある方法は完全な修正ではありません。この PR は採用していません。

検証手順：

1. iOS / tvOS で実際の AudioQueue callback 入口、全状態アクセス終了後、dispose の入口 / 復帰、Box drop におけるスレッドと単調時刻を記録します。
2. 同期 disposal で stop / configure / drop を高頻度に繰り返し、ASan または同等のメモリ診断を使用します。問題発生時の最短タイムラインを保存します。
3. 真の dispose 後 callback と、ホストが別経路で `user_data` を早期解放したケースを区別します。
4. 公式契約が実際のシステム動作をカバーできない場合、callback 入口を含めて保護する安定した owner / registry、または明示的なキャンセルプロトコルを設計します。

作者の fork CI は各 head の厳密な SHA ですべて成功し、iOS / tvOS のビルドも含みます。これはコンパイル範囲を示すものであり、競合の再現や修正を証明するものではありません。
