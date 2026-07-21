<div align="center">

# nospacekey

**Space を押さない。モードと戦わない。**

日本語入力の「めんどくさい」を根本から消しにいく、Windows 用のかな漢字変換 IME。

[![Release](https://img.shields.io/github/v/release/yachtida/nospacekey)](https://github.com/yachtida/nospacekey/releases/latest)
[![License: MIT](https://img.shields.io/github/license/yachtida/nospacekey)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%2011%20x64-blue)

[紹介ページ](https://yachtida.github.io/nospacekey/) ・ [ダウンロード](https://github.com/yachtida/nospacekey/releases/latest)

</div>

---

## こんな経験、ありませんか

### 「ターミナルに `ｇｉｔ　ｓｔａｔｕｓ`」

Slack に日本語で返信して、その手でターミナルに戻る。切り忘れた IME がコマンドを
全角で取ってしまい、Enter を押してから気づく——
普通の IME は「日本語モードに入れたら、切るのも自分の仕事」だからこうなります。

**nospacekey は逆です。** 普段は半角英数のまま(起動時から英数で始める設定もあります)。
日本語を打ちたくなったら <kbd>F8</kbd> — そこだけ日本語モードになり、
**確定した瞬間に自動で半角英数へ戻ります**(一時日本語モード)。
「モードを切り忘れる」という概念そのものがなくなるので、コミットメッセージに
一言日本語を入れても、次のキーはちゃんとショートカットとして働きます。

### 「Space、Space、Space……変換のたびに親指が忙しい」

nospacekey の変換は**ライブ変換**。打つそばから変換が追いかけてきて、
そのまま打ち続ければ文が確定していきます。Space を押すのは候補を選び直したい
ときだけ。「文節を区切って、変換して、確定して」というリズムから解放されます。
macOS のライブ変換に慣れた人が Windows で恋しくなる、あれです。候補が出るだけでなく
**確定まで自動で進む**のがポイントです。

### 「error: expected ';'」

コメントだけ日本語で書いて、次の行の `;` が「；」になっていた。全角スペースに
至っては diff でも見えません。nospacekey は普段が半角英数のままなので全角が
紛れ込む隙がなく、**日本語モードの最中でも記号は半角のまま**入ります
(全角にしたい場合は設定で選べます)。

### 「こんんいちは」

急いで打つと出る「同じキーの打ちすぎ」も、<kbd>Tab</kbd> 一発で修復候補が
出ます(修正変換)。確定すれば誤読みごと学習するので、次からは普通の変換で直ります。

## 機能

- **ライブ変換** — 打鍵に追随して自動で変換。従来の Space 変換もそのまま使えます
- **一時日本語モード** — <kbd>F8</kbd>(変更可)で入り、確定すると自動で半角英数へ復帰
- **モードレス再変換** — 半角英数で打ったローマ字を、選んで後から日本語化
- **修正変換(Tab)** — 打ち間違いを修復し、誤読みも学習
- **Shift+英字** — 文中に英単語を混ぜるときは Shift を押しながら打つだけ
- **Zenzai ニューラル変換(opt-in)** — GGUF モデルを置くと文脈を読む変換をローカル CPU で。無ければ古典 LOUDS 変換で軽快に動作
- **設定 GUI** — キーマップのリバインド、句読点・記号の全角半角、起動時の入力モードなど
- **プライバシーファースト** — 変換・学習・ログはすべてローカル完結、既定で外部送信ゼロ([PRIVACY.md](PRIVACY.md))。パスワード欄では変換も学習もしません

## インストール

1. [Releases](https://github.com/yachtida/nospacekey/releases/latest) から `nospacekey-setup-<version>.exe` をダウンロード
2. 実行してインストール(IME の登録はマシン単位のため、管理者権限を求められます)
3. <kbd>Win</kbd> + <kbd>Space</kbd> で「nospacekey」を選択

対応環境: Windows 11 x64

> [!NOTE]
> 現在の版は開発用の自己署名で配布しているため、初回実行時に SmartScreen の警告が表示されます。
> 「詳細情報」→「実行」で続行できます。ダウンロードしたファイルは各リリース添付の
> `SHA256SUMS.txt` で検証できます。

### Zenzai(ニューラル変換)を有効にする

Zenzai はオプトインです。設定画面のダウンロード機能を使うか、
[zenz-v3.1-small](https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf)(CC-BY-SA-4.0)の
`ggml-model-Q5_K_M.gguf` を `C:\Program Files\nospacekey\models\` に配置すると、
次回起動時から自動で有効になります。

### アンインストール

Windows の「設定 → アプリ」から「nospacekey」をアンインストールしてください。
IME の登録・配置ファイルはすべて除去されます。

## アーキテクチャ

TSF (Text Services Framework) 層を Rust、変換エンジンホストを Swift で実装し、
名前付きパイプの JSON IPC で接続する分離プロセス構成です。エンジンが落ちても
入力先のアプリを巻き込まず、劣化動作(読みのまま確定)にフォールバックします。

```
nospacekey\
├─ crates/
│  ├─ tip/          # TSF テキスト入力プロセッサ (Rust, COM) → nospacekey_tip.dll
│  ├─ ipc/          # JSON メッセージ型・フレーミング・パイプ client
│  ├─ settings/     # 設定の型と永続化
│  ├─ config/       # 設定 GUI (Tauri) → NospacekeyConfig.exe
│  └─ testbench/    # ヘッドレス受入シナリオ
├─ engine-host/     # 変換エンジンホスト (Swift) → NospacekeyEngineHost.exe
└─ installer/       # Inno Setup スクリプト
```

## コントリビューション

小規模な個人プロジェクトのため、現在 Pull Request は受け付けていません。
不具合報告や提案は [Issue](https://github.com/yachtida/nospacekey/issues) までお寄せください。

## 謝辞

nospacekey は次のオープンソースプロジェクトの成果の上に成り立っています。

- [AzooKeyKanaKanjiConverter](https://github.com/azooKey/AzooKeyKanaKanjiConverter) (MIT) — かな漢字変換エンジン
- [zenz-v3.1-small](https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf) (CC-BY-SA-4.0) — Zenzai ニューラル変換モデル
- [llama.cpp](https://github.com/ggml-org/llama.cpp) (MIT) — ローカル LLM 推論

本プロジェクトは azooKey プロジェクトとは独立した非公式プロジェクトです。
本プロジェクトに関するお問い合わせは、上流ではなく当リポジトリの Issue へお願いします。

## 免責事項

nospacekey は [MIT License](LICENSE) に基づき「**現状有姿(AS IS)**」で提供される無償の
ソフトウェアであり、明示・黙示を問わずいかなる保証も行いません。IME はシステム全体の
文字入力に関与するという性質上、不具合により入力不能・入力内容の欠落・アプリケーションの
異常終了などが発生する可能性があります。本ソフトウェアの使用または使用不能から生じる
いかなる損害(データの損失、業務の中断、逸失利益などを含みますが、これらに限りません)
についても、作者およびコントリビューターは一切の責任を負いません。
利用はご自身の判断と責任でお願いします。重要な作業の前にはデータの保存・バックアップを
推奨します。

## ライセンス

[MIT License](LICENSE)

同梱・静的リンクする第三者コンポーネントのライセンスと帰属は
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) にまとめています。
脆弱性の報告方法は [SECURITY.md](SECURITY.md) をご覧ください。
