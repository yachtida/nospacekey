<div align="center">

# nospacekey

日本語も、コードも。入力の流れを止めない。

打ちながら変換。必要なときだけ、日本語へ。
Windows 11 向けの、ローカルで動く日本語 IME。

[Windows 版をダウンロード](https://github.com/yachtida/nospacekey/releases/latest) · [使いはじめる](#使いはじめる) · [基本操作](#基本操作)

</div>

<!-- 実機の録画を用意できたら、ここに短いデモを1本だけ追加する。
     ライブ変換 → 候補の修正 → F8による一時かな入力、の操作を見せる。
     実際の表示・速度を使い、未確認の動画パスは公開しない。 -->

## 打つそばから、日本語に。

ライブ変換が、入力中の読みをかな漢字交じりの文章へ変えていきます。
変換結果が合っていれば、そのまま続きを。候補を選び直したいときだけ、<kbd>Space</kbd> を使います。

```
日本語を打つ ──→ 入力中に変換 ──→ Enter で確定
                       │
                 候補を変えたいときは Space
```

長文入力では、先頭の文節を順次自動確定します。最後に残った入力は <kbd>Enter</kbd> で確定。
ライブ変換をオフにして、Space で変換する使い方もできます。

## 日本語は、必要なときだけ。

コードやコマンドの合間に、日本語のコメントをひと言。
一時かな入力なら、半角英数モードで <kbd>F8</kbd> を押して日本語を入力し、確定すると自動で半角英数へ戻ります。

```
半角英数 ── F8 ──→ 日本語を入力 ── Enter で確定 ──→ 半角英数
                                                  戻す操作は不要
```

日本語を続けて書くときは、通常の日本語モードも使えます。
一時かな入力は、いつもの入力方法を置き換えるのではなく、選べるもう一つの使い方です。

## 使いはじめる

対応環境：Windows 11 x64。基本のかな漢字変換に、ニューラル変換用の GPU やモデルは不要です。

1. [最新リリース](https://github.com/yachtida/nospacekey/releases/latest)の Assets から、`nospacekey-setup-` で始まる .exe をダウンロードします。
2. インストーラを実行します。IME の登録には管理者権限が必要です。
3. <kbd>Win</kbd> + <kbd>Space</kbd> で nospacekey を選びます。

> [!IMPORTANT]
> ファイル名に `-devsigned` を含む版は、開発用証明書で署名されています。Windows SmartScreen の警告が表示される場合があります。
> 配布元がこのリポジトリの Releases であることを確認し、実行するか判断してください。ダウンロードしたファイルは、リリース添付の `SHA256SUMS.txt` と照合できます。

### 最初に試すこと

まずは日本語を入力し、ライブ変換を試してみてください。ライブ変換は既定でオンです。

英数を中心に使う場合は、言語バーの「設定」から 一般 → 新しいアプリの入力モード → 半角英数で始める をオンにします。新しいアプリを開き、<kbd>F8</kbd> で一時かな入力を試せます。

「半角英数で始める」は既定ではオフです。日本語中心の使い方なら、変更する必要はありません。

<details>
<summary>アンインストールするには</summary>

Windows の「設定 → アプリ」から nospacekey をアンインストールしてください。使用中のアプリに IME が読み込まれている場合は、Windows の再起動が必要になることがあります。

</details>

## 基本操作

以下は既定のキー設定です。

| やりたいこと | 操作 |
| --- | --- |
| 日本語・半角英数を切り替える | 入力していないときに <kbd>半角/全角</kbd>、または <kbd>Alt</kbd> + <kbd>;</kbd> |
| 半角英数から、一時的に日本語を入力する | 入力開始前に <kbd>F8</kbd>。確定すると半角英数へ戻ります |
| 入力を確定する | <kbd>Enter</kbd> |
| 手動で変換・候補を選び直す | <kbd>Space</kbd> で文節を変換。もう一度押すと候補一覧を表示 |
| 変換する文節を移動する | 変換中に <kbd>←</kbd> / <kbd>→</kbd> |
| 文節の区切りを変える | 変換中に <kbd>Shift</kbd> + <kbd>←</kbd> / <kbd>→</kbd>。変更した区間は <kbd>Space</kbd> で再変換 |
| 打ち間違いの修復候補を出す | 読みの入力中に <kbd>Tab</kbd> |

F8 の役割は入力状態で変わります。上の一時かな入力は未確定文字列がないときの操作で、変換中の F8 は半角カナへの表記変換です。機能キーの割り当ては、設定の「キー設定」から変更できます。

<details>
<summary>もっと便利に使う：再変換・英字・記号・辞書・外観</summary>

| 機能 | できること |
| --- | --- |
| モードレス再変換 | 半角英数で打ったローマ字を選択し、<kbd>変換</kbd> または <kbd>Alt</kbd> + <kbd>/</kbd> で後から日本語化 |
| 英字の入力 | 日本語入力中に <kbd>Shift</kbd> + 英字で、英語入力モードへ。既定では確定まで英字入力が続きます |
| 記号の幅 | ; や : などの記号は既定で半角。全角にする記号も個別に選択できます。数字・句読点の幅は別設定です |
| 読みモニタ | ライブ変換中も、打っている読みを小窓で確認できます |
| 学習・ユーザー辞書 | 変換履歴の学習や単語登録に対応。学習の無効化・履歴の消去もできます |
| 外観 | ライト／ダーク、フォント、配色などを調整できます |

</details>

## 文脈を読む変換も、ローカルで。

Zenzai は、文脈を考慮して候補を選ぶニューラル変換です。対応する Vulkan GPU・ドライバ・実行環境とモデルがそろった場合に利用できます。

| 変換方式 | 必要なもの |
| --- | --- |
| 通常のかな漢字変換 | 本体のインストールのみ。追加モデルは不要 |
| Zenzai ニューラル変換 | Vulkan 対応 GPU・ドライバ・実行環境と GGUF モデル |

Zenzai を利用できない環境では、通常のかな漢字変換で動作します。

インストーラの「Zenzai（ニューラル変換）を使用する」は既定で選択されています。不要なら解除してください。モデルは、インストール後に設定画面から取得することもできます。

<details>
<summary>インライン予測を試す（アルファ版・既定オフ）</summary>

明示的に確定した文章の続きを、薄い文字で提案する機能です。<kbd>→</kbd> または <kbd>End</kbd> で受け入れ、<kbd>Esc</kbd> で閉じます。

専用モデルの取得と Vulkan 対応 GPU が必要です。推論はローカルの GPU で行い、GPU を利用できない場合は予測を無効にして通常変換を続けます。設定画面から有効にできます。

アルファ版のため、動作や仕様は今後変更される可能性があります。

</details>

## 入力内容は、あなたの PC の中に。

変換・学習・予測はローカルで処理し、入力内容を外部 API へ送りません。テレメトリやクラッシュレポートの自動送信もありません。

モデルの取得や更新確認では通信が発生します。自動アップデート確認は既定でオフで、有効にしても更新を自動でダウンロード・インストールすることはありません。

パスワード／PIN 入力欄を検出した場合は、変換・学習・予測を行いません。詳しくは[プライバシー方針](PRIVACY.md)をご覧ください。

## 不具合報告・開発について

不具合や機能の提案は [Issues](https://github.com/yachtida/nospacekey/issues) へ。Windows のバージョン、nospacekey のバージョン、入力先アプリ、再現手順があると調査の助けになります。入力例に個人情報や機密情報を含めないでください。

現在、Pull Request は受け付けていません。脆弱性の報告方法は [SECURITY.md](SECURITY.md) をご覧ください。

<details>
<summary>内部構成</summary>

Windows の入力処理を担う TSF 層を Rust、変換エンジンホストを Swift で実装しています。両者は別プロセスで動作し、名前付きパイプの JSON IPC で通信します。

```
入力先アプリ
    │
    └─ TSF / TIP（Rust）
              │ 名前付きパイプ / JSON
              └─ 変換エンジンホスト（Swift）
                      └─ かな漢字変換 / Zenzai

設定アプリ：Tauri
インストーラ：Inno Setup
```

| ディレクトリ | 役割 |
| --- | --- |
| `crates/tip` | TSF テキスト入力プロセッサ |
| `crates/ipc` | IPC メッセージとパイプ通信 |
| `crates/settings` | 設定の型・保存 |
| `crates/config` | 設定 GUI |
| `crates/testbench` | ヘッドレス受入シナリオ |
| `engine-host` | 変換エンジンホスト |
| `installer` | インストーラ |

</details>

## 謝辞・ライセンス

nospacekey は [MIT License](LICENSE) で公開しています。次のプロジェクトの成果を利用しています。

- [AzooKeyKanaKanjiConverter](https://github.com/azooKey/AzooKeyKanaKanjiConverter) — かな漢字変換エンジン（MIT）
- [zenz-v3.1-small](https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf) — Zenzai ニューラル変換モデル（CC-BY-SA-4.0）
- [llama.cpp](https://github.com/ggml-org/llama.cpp) — ローカル推論（MIT）

第三者コンポーネントのライセンス・帰属は [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) にまとめています。

本プロジェクトは azooKey プロジェクトとは独立した非公式プロジェクトです。お問い合わせは、上流ではなく当リポジトリの Issues へお願いします。

## コントリビューター

機能要望・不具合報告を通じて、ご協力いただいた方々：

<!-- ALL-CONTRIBUTORS-LIST:START - Do not remove or modify this section. -->
<!-- prettier-ignore-start -->
<!-- markdownlint-disable -->
<table>
  <tbody>
    <tr>
      <td align="center" valign="top" width="14.28%"><a href="https://github.com/waigoma"><img src="https://avatars.githubusercontent.com/u/48357625?v=4?s=100" width="100px;" alt="waichi"/><br /><sub><b>waichi</b></sub></a><br /><a href="#ideas-waigoma" title="Ideas, Planning, & Feedback">🤔</a></td>
    </tr>
  </tbody>
</table>

<!-- markdownlint-restore -->
<!-- prettier-ignore-end -->
<!-- ALL-CONTRIBUTORS-LIST:END -->

<details>
<summary>免責事項</summary>

nospacekey は [MIT License](LICENSE) に基づき「現状有姿(AS IS)」で提供される無償の
ソフトウェアであり、明示・黙示を問わずいかなる保証も行いません。IME はシステム全体の
文字入力に関与するという性質上、不具合により入力不能・入力内容の欠落・アプリケーションの
異常終了などが発生する可能性があります。本ソフトウェアの使用または使用不能から生じる
いかなる損害(データの損失、業務の中断、逸失利益などを含みますが、これらに限りません)
についても、作者およびコントリビューターは一切の責任を負いません。
利用はご自身の判断と責任でお願いします。重要な作業の前にはデータの保存・バックアップを
推奨します。

</details>
