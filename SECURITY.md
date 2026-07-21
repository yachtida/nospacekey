# セキュリティポリシー / Security Policy

IME は入力中の平文テキスト（パスワード欄を除く全打鍵）を扱うソフトウェアです。
脆弱性報告は最優先で扱います。

*An IME handles plaintext keystrokes. Vulnerability reports are treated with top priority.*

## 脆弱性の報告 / Reporting a Vulnerability

- **公開 Issue には書かないでください。** GitHub の **Private Vulnerability Reporting**
  （リポジトリの Security タブ → *Report a vulnerability*）から非公開で報告してください。
- 個人メンテナンスのプロジェクトのため応答に時間がかかる場合がありますが、報告は必ず確認します。
  修正が公開されるまで詳細の公開を控えていただけると助かります。

## 対象範囲 / Scope

- TIP DLL（TSF テキストサービス）、変換エンジン（engine host）、設定アプリ、インストーラ。
- 特に関心の高い領域:
  - パスワード欄検出の回避（パスワードが変換・学習経路に乗るケース）
  - 学習データ・ユーザー辞書の保存と消去
  - 名前付きパイプ IPC のアクセス制御（DACL）
  - インストーラ/アンインストーラの権限昇格まわり

## サポートバージョン / Supported Versions

最新リリースのみを修正対象とします。
