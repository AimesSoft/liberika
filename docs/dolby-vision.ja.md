# Dolby Vision HDR マッピング

[English](dolby-vision.md) · [中文](dolby-vision.zh.md) · [文書一覧](README.md)

この文書は liberika の Dolby Vision RPU（Reference Processing Unit）メタデータ処理と色マッピングを説明します。実装は mpv の Dolby Vision 対応に使われるレンダラー [libplacebo](https://github.com/haasn/libplacebo) を参考にしています。

## 対応 Profile とデコード

| Profile | 形式 | デコード方式 | RPU |
|---|---|---|---|
| 5 | 単一レイヤー、後方互換なし（IPTPQc2） | VideoToolbox / D3D11VA でハードウェア、モバイルではソフトウェアにフォールバック | 対応 |
| 8 | 後方互換ベースを持つ単一レイヤー（8.1 HDR10、8.4 HLG など） | ハードウェアデコード可能 | 対応 |

Profile 8 のベースレイヤーは HDR10 PQ や HLG の標準信号を持ち、RPU は NAL 単位で挿入されます。Profile 7 は UHD Blu-ray などで使われる二層形式（ベースレイヤーと FEL/MEL 拡張レイヤー）です。

macOS VideoToolbox と Windows D3D11VA はハードウェアテクスチャとともに `AV_FRAME_DATA_DOVI_METADATA` フレームサイドデータを保持し、GPU shader でフレームごとの変換に使えます。MediaCodec や汎用 AvCodec などのモバイル・組み込みバックエンドは RPU を出力しない場合があります。Profile 5 は通常の YCbCr と後方互換性がないため、そのようなバックエンドでは FFmpeg `avcodec` のソフトウェアデコードでメタデータを取得します。デスクトップでは Profile 5 もハードウェアデコードを維持します。Profile 8 は RPU が得られなくてもベースレイヤーを HDR10/HLG として表示できます。`crates/erika/src/playback.rs` の `dolby_vision_decode_fallback()` を参照してください。

## 処理パイプライン

1. MP4/MKV コンテナの `dvcC`/`dvvC` が Profile 5 または 8 を示します。
2. FFmpeg が HEVC をデコードし、NAL の RPU を解析して `AV_FRAME_DATA_DOVI_METADATA` を出力します。
3. `ffmpeg.rs` の `frame_dovi_metadata_result` が `AVDOVIMetadata` を読み、ベースレイヤーのビット深度で pivot を正規化し、係数を `2^(-coef_log2_denom)` でスケールして `DoviSourceMetadata` を生成します。
4. `pipeline.rs` が多項式・MMR 係数を vec4 アラインの `DoviUniforms`（約 3 KB）に格納し、アップロードのビット深度に合わせて信号オフセットを補正します。
5. WGSL、Metal、HLSL shader が成分ごとの再整形、RPU `ycc_to_rgb` 非線形行列、PQ EOTF による線形化、HPE 逆行列と `rgb_to_lms` を組み合わせた LMS→RGB、ディスプレイ向けトーンマッピングを行います。

## 再整形と色変換

各 Y、Cb、Cr 成分は pivot と係数を使って区分的に再整形されます。入力 `s` が `p[i]` と `p[i+1]` の間にある多項式区間では次を計算します。

```text
output = (c2 · s + c1) · s + c0
```

MMR（多変量多項式回帰）は三成分を混合します。一次には `R`、`G`、`B` と `R·G`、`R·B`、`G·B`、`R·G·B` を含み、二次・三次はそれらの二乗・三乗を加えます。shader の `dovi_reshaped_signal()` を参照してください。

再整形後、`nonlinear_offset` を減算し、RPU の `ycc_to_rgb` を適用（まだ PQ 符号化）、PQ を線形光に変換し、`(HPE⁻¹ × rgb_to_lms) × linearized` で RGB に戻して HDR 色域・トーンマッピングに進みます。

## トーンマッピング

libplacebo の `pl_shader_color_map` に合わせ、ソース原色 RGB（絶対 nits）を HPE-LMS と PQ 符号化を経て IPT に変換し、IPT の強度軸に曲線を適用します。libplacebo の `hull` 三次色度規則で彩度を保ち、ターゲット原色に戻す際に原色変換も行います。

| コード | `ToneMapOperator` | `ToneMapConfig::curve_param` |
|---|---|---|
| 0 | Clip | — |
| 1 | Reinhard | コントラスト、既定 0.5 |
| 2 | Mobius | 線形 knee、既定 0.3 |
| 3 | BT.2390（既定） | knee オフセット、既定 1.0 |
| 4 | Spline | 傾きのコントラスト、既定 0.30 |
| 5 | BT.2446 method A | — |
| 6 | SMPTE ST 2094-10 | knee 適応、既定 0.70；係数は CPU でフレームごとに求める |

黒点補正は `target black = target peak / contrast`（`contrast_ratio`、SDR では自動 1000:1、HDR/EDR ターゲットでは 0）を使い、SDR→SDR には適用しません。`[black, peak]` を `[0, 1]` に符号化するため、補正後の下限はコード 0 に戻ります。

トーンマップ、黒点補正、知覚色域 LUT の有効・無効は**静的な**マスターディスプレイピーク L0 で決まり、暗いシーンでフレーム単位に切り替わりません。動的 L1 ピークは曲線のソースピーク（`tone_map_extra`/`nits.x`）を設定します。ST 2094-10 は PQ 領域で knee を選び、絶対 nits で曲線を求めます。これは libplacebo の `pl_hdr_rescale` の往復に対応します。

## シーン適応 HDR10

HDR10 は通常、静的 ST.2086 メタデータのみを持ちます。ソフトウェアデコードされた PQ フレームでは `crates/erika/src/luma_stats.rs` が NV12/P010 輝度面を疎にサンプリングし、線形化して PQ に再符号化した値を知覚領域で平均し、libplacebo の IIR フィルター（`coeff = 1 - exp(-1/20)`）で平滑化します。結果は nits 単位で `PlayerVideoFrame::scene_avg_nits` と `SourceColorState.measured_scene_avg_nits` に入り、`tone_map_extra.y` が L1 平均より優先するため、spline の pivot がシーンに追従します。

サンプルはフレームの色範囲で正規化します。有限範囲は 16..235 / 64..940 を展開し、shader の `expand_ycbcr_range` と一致させます。VideoToolbox、D3D11VA、MediaCodec のハードウェアフレームには CPU 輝度面がないため静的メタデータを使用します。generation 変更時に推定器をリセットし、seek 後に前のシーンの輝度を持ち越しません。

## 知覚色域マッピング

HDR ソースを小さい色域（BT.2020→BT.709）にトーンマップする場合、レンダラーは CPU 生成の 48 × 32 × 256 IPT LUT（`renderer::gamut`）を使います。テクセルは `(I, P + 0.5, T + 0.5)` を保持し、I 軸はターゲット `[black, peak]` の PQ コードを覆います。配置とインデックスは libplacebo に従いますが、色度ロールオフは色相ごとの完全な境界探索ではなく、簡略化したデッドゾーン混合と Möbius ソフトクリップです。そのため高彩度の BT.2020 色は mpv と多少異なる場合があります。

shader はソース原色の RGB を再構成し、LMS-PQ-IPT を往復し、ICh 空間で LUT をサンプリングしてターゲット原色に戻します。LUT はソース、ターゲット、ターゲット黒点とピークごとにキャッシュし、WGSL slot 4、Metal texture 2、D3D11 t2 にバインドします。高速パスでもレイアウトを維持するため 1×1×1 のダミーを使用します。

## フレームごとの L1 輝度と Profile 7

RPU の動的 DM 拡張ブロック L1 は `min_pq`、`max_pq`、`avg_pq` を 12 ビット PQ コードで保持します。FFmpeg が `AVDOVIMetadata` の `ext_block_offset` 領域へコピーし、`frame_dovi_level1` が検証して取り出します。有効な L1 がある場合、`max_pq` はトーンマップのソースピーク、`avg_pq` はシーンの pivot を設定します。静的 L0 は引き続き出力モードとマッピング/LUT の有効化を決めます。L1 がない、すべてゼロ、または範囲が逆転している場合は静的 `source_max_pq` にフォールバックし、平均値が不明なら knee は 40% に固定します。L1 の不備だけで RPU 全体を拒否しません。

Profile 7 の拡張レイヤーは別の HEVC レイヤーであり、現在のレンダラーはデコードしないため、`disable_residual_flag == 0` が要求する残差を合成できません。一方、ベースレイヤーの再整形曲線、色行列、L1 調整は適用されます。`Frame::dovi_el_status()` は `DoviElStatus`（`residual_requested`、`nlq_nontrivial`）を返し、再生時にはストリームごとに頻度を抑えて `dovi_el_not_composed` を通知します。NLQ は報告のために解析され、shader には渡されません。結果は mpv/libplacebo と同様ですが、拡張レイヤーの追加ディテールはありません。

## BT.2020/PQ の強制

Profile 5/8 の VUI タグは信頼できない場合があるため、`pipeline.rs` の `SourceColorState::dovi()` は BT.2020 原色と PQ 伝達関数を強制します。これにより伝達関数が未指定の場合に PQ サンプルを sRGB gamma で誤ってデコードすることを防ぎます。

## テストと制限

単体テストはサイドデータの解析、uniform パッキング、タグ欠落時の PQ 強制、L1 解析とフォールバック、動的ピークと静的出力判定、FEL ベースレイヤーのマッピング、Profile 5/8 のハードウェア・ソフトウェアデコード方針を検証します。サンプルを必要とする統合テストでは `ERIKA_DV_SAMPLE`（Profile 5）と `ERIKA_DV_PROFILE_8_SAMPLE`（Profile 8）を設定します。

現在の制限：Profile 7 FEL/MEL の拡張レイヤー残差は合成しません。モバイルハードウェアデコーダーが RPU を出力できない場合、Profile 5 はソフトウェアデコードを使います。約 3 KB の uniform buffer は古いモバイル GPU の制限を超える可能性があります。ソフトウェアデコード面には CPU から GPU へのアップロードが必要です。

## 実装と参考資料

- `crates/erika/src/ffmpeg.rs`：コンテナ・フレームメタデータ、`playback.rs`：Profile ごとのデコードフォールバック。
- `renderer/pipeline.rs`：データ構造と uniform、`renderer/gamut.rs`：知覚色域 LUT、`luma_stats.rs`：HDR10 シーン輝度測定。
- `renderer/wgpu_video.wgsl`、`renderer/metal/apple.rs`、`renderer/d3d11.rs`：各 shader 実装。
- [Dolby Vision](https://professional.dolby.com/dolby-vision/) · [libplacebo dovi_reshape](https://github.com/haasn/libplacebo/blob/master/src/shaders/dovi.c) · [FFmpeg AVDOVIMetadata](https://ffmpeg.org/doxygen/trunk/structAVDOVIMetadata.html) · [BT.2100 PQ EOTF](https://www.itu.int/rec/R-REC-BT.2100)
