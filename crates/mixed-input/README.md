# mixed-input — 混在入力の契約型・fixture・判別器

日本語・英字混在入力（ADR-0007 / 計画書 `docs/active/2026-09-24-mixed-input-plan.md`）
の純粋な Rust ライブラリ。TSF・IPC・Swift・Zenzai・ネットワークに依存しない。
TIP 側ワーカー（PR5 以降）とオフライン評価 CLI が同じ実装を呼ぶ。

## 構成

| モデル | 役割 |
|---|---|
| `source` / `projection` / `plan` / `position` | 元入力の由来と Plan 適用の契約（PR2） |
| `episode` / `validation` / `replay` / `fixtures/` | 契約 fixture と打鍵再生（PR1） |
| `classify/edge` | 候補区間辺（ローマ字日本語辺・辞書/未知 Literal 辺・由来の固定辺） |
| `classify/ngram` | 文字 1–5 gram の対数尤度（JA / 原文保持の両ラベル、バックオフ平滑化） |
| `classify/dp` | k-best 区間DP（固定タイブレーク、言語切替ペナルティ） |
| `classify/model` | 線形特徴重み・保留閾値・テキスト形式アーティファクト（checksum 検証） |
| `classify/synth` | 評価用 synthetic 生成（決定的 seed。表記揺れ・未知語・typo を含む） |
| `classify/tune` | n-gram 学習・validation での座標探索・評価指標 |

## 判別器の再現手順（PR4）

データはすべて決定的生成（seed 固定）。分割はテンプレート単位で
train / validation / frozen に固定され、表記揺れ・編集派生が別分割に散らない
（計画書 9.2）。

```powershell
# 1) 学習 + validation 調整でモデルアーティファクトを書く
& ./scripts/with-dev-env.ps1 @('cargo','run','-p','mixed-input','--bin','mixed-input-eval','--','train','--out','target/mixed-model.txt')

# 2) frozen 分割 + 契約 fixture のカテゴリ別評価
& ./scripts/with-dev-env.ps1 @('cargo','run','-p','mixed-input','--bin','mixed-input-eval','--','eval','--model','target/mixed-model.txt')

# 3) 1入力の解釈候補を見る
& ./scripts/with-dev-env.ps1 @('cargo','run','-p','mixed-input','--bin','mixed-input-eval','--','demo','githubnotukaikata','--model','target/mixed-model.txt')
```

## 辞書・モデル資産（計画書 9.3）

- `data/dictionary/general.txt` / `tech.txt`: 一般語と技術用語の別レイヤー。
  出典・ライセンスは各ファイルヘッダが正本（自作）。
- モデルはオフラインで作り、テキスト形式（n-gram 件数表 + 線形重み + 保留閾値）+
  FNV-1a checksum で配布する。学習依存を製品へ含めない。
- 未知語評価用の holdout 語彙は辞書に入れない（`classify/synth` 参照）。

## 品質の位置づけ

PR4 の完了条件は「学習済み重みで githubnotukaikata の内部を分割できること」と
「一般語・短語・純日本語・未知語を別々に評価できること」。`eval` の数値は現状の
記録であって品質ゲートではない。閾値・feature ablation・CRF 比較・実機測定は
PR7 で行い、自動モードの公開判定はそこで別々に行う。

## 候補試験（PR5）

設定ファイルの `mixed_input` は `off`（既定）/ `candidates` / `auto`。
現段階の `auto` は `candidates` と同じ制限で動く。設定UIでの公開は PR8。
設定を保存後に入力先アプリを開き直す。機能有効の composition は自動prefix確定を抑止する。

Space で通常の候補の後ろに最大2件の混在候補と「区間を修正」を表示する。
解析・モデル読込・変換は専用ワーカーで行い、待機中の編集で古い結果を破棄する。
候補選択は読みとProjectionを同時に切り替える。Escは選択前の入力へ戻る。
入力続行は選んだ解釈の読みから継続し、通常ライブ変換による再解釈を止める。

「区間を修正」では原入力を表示し、左右/Home/Endで移動、Shiftと併用して範囲を選ぶ。
上下で「英字のまま」「日本語として解釈」を選びEnterで修正候補を生成する。
元入力不明な範囲や合法境界でない範囲は変更できない。説明行は本文として確定しない。

`data/model.txt` は上記の train コマンドで作るオフライン学習済み資産。
TIPはワーカー上で形式とchecksumを検証し、不正なら通常変換へ戻る。
モデルと辞書の公開manifest、品質・速度の実測、実機受入の判定は PR7/PR8 で管理する。


## ライブ試験（PR6）

通常ビルドの `auto` は引き続き候補モードに制限する。開発用の
`cargo build -p nospacekey_tip --features mixed-input-live-trial` と `mixed_input=auto`
（ライブ変換ON）を組み合わせた場合だけ、候補窓を開かずに仮採用する。
Cargoはリポジトリの `scripts/with-dev-env.ps1` 経由で実行する。
試験は既知のASCII英字の末尾追加入力に限定し、未知source・Direct・記号・途中編集では自動再解釈しない。

採用閾値はモデルの `auto_margin`、既に採用したprefixを変更する閾値は `auto_margin + 2`。
この差は暫定であり、PR7のvalidation評価で扱う。時間経過だけでは採用しない。
カーソル・削除・明示変換・表記指定はcomposition内のUserLockedとし、古い応答を失効させる。
TSF本文ACK前は仮採用状態を確定せず、拒否時は読み・表示を戻す。

`live::commit_fence` は競合解釈、最終未完区間、編集範囲、source unit、Literal全体を考慮する。
`source_after_prefix` は消費前に残りsourceを準備し、InputModuleは本文反映成功後だけsuffixを残す。
**この段階では自動prefix確定の試験ガードを解除しない**。fenceは算出・契約検証するが、
公開時の自動確定許可は品質/実TSF受入が揃ってから別に判定する。


## 品質判定（PR7）

現行モデルはvalidationで採用閾値64・変更閾値66に調整済み。自動のfrozen Recallは0であり、自動対応の完成を意味しない。
候補の区間完全一致は16/25、128文字判別p95は約195msで、候補・自動とも一般公開ゲートを満たさない。
データの出自、件数、ablation、機材、全実測と再現コマンドは [PR7評価](../../docs/validation/mixed-input/README.md) にまとめる。
学習の `train` の後に `mixed-input-quality ... --tuned-out ...` を実行すると同梱モデルを再現できる。

## 同梱資産

形式・ライセンス・更新/削除・再生成は [data/README.md](data/README.md)、品質・公開制限は [PR7評価](../../docs/validation/mixed-input/README.md) を参照。通常ビルドでは既定OFF、候補のみが試験用に選択可能。
