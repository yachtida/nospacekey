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

## Windows の脅威境界 / Windows Threat Boundary

- 対象には、ユーザー境界・セッション境界をまたぐアクセス、nospacekey の製品プロセス間の協調、昇格したインストーラ／アップデータなどによる権限・信頼境界をまたぐパス置換を含みます。
- 学習データの消去は nospacekey プロセス間で協調し、観測可能な unsafe なファイルシステムオブジェクトに遭遇した場合は fail-closed します。ただし、既に侵害された同一 Windows ユーザーのセッションがユーザー所有のアプリデータを直接変更できる状況から、そのデータを保護することを目的としません。
- 同一 Windows ユーザーとして既に実行され、ユーザー所有のアプリデータを直接変更できる任意コードは、それが権限・信頼境界を越える場合を除き、本ポリシーのセキュリティ境界外です。

The scope includes cross-user and cross-session access, coordination among nospacekey product processes, and path replacement across a privilege or trust boundary (for example, by an elevated installer or updater).
Learning-data clearing coordinates nospacekey processes and fails closed when observable unsafe filesystem objects are encountered. It is not intended to protect user-owned app data from arbitrary code already running as the same Windows user and able to modify that data directly.
Arbitrary code already running as that same Windows user is outside this security boundary unless its behavior crosses a privilege or trust boundary.

## サポートバージョン / Supported Versions

最新リリースのみを修正対象とします。
