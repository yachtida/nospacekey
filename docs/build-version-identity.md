# ビルド版番号と配布物の同一性

## CI版の採番

CIのインストール版は次の形式です。

- 安定版ソース: `1.6.0-ci.gha.<repository_id>.<run_id>.<run_attempt>`
- ベータ版ソース: `1.7.0-beta.8.ci.gha.<repository_id>.<run_id>.<run_attempt>`

`run_id` は同じリポジトリ内の別workflowも含めて異なり、再実行では `run_attempt` が増えます。リポジトリIDでfork等も分離します。`gha` は旧 `ci.<run_number>...` 形式と区別する固定の識別子です。現在の対象はGitHub.com上の1 workflow runにつき1つのWindows x64インストーラです。将来、同じrunで複数の製品構成を作る場合は、採番に構成の識別子を加えてから対応してください。

workflow単位の `run_number`、日時、短縮commit hashだけでは採番しません。IDは数値へ丸めず十進文字列のまま扱い、不足・不正値・80文字を超える版番号は停止します（実行時lease名と削除処理の制限に合わせます）。乱数による衝突確率にも依存しません。

Cargo、Tauri、Swift、installerの製品版番号とインストール先を同じ完全な版番号にそろえます。ファイル名だけを変えて同じインストール版を再利用しません。`+build-metadata` はSemVerの比較で無視されるため、今回の識別子には使用しません。

Windowsの数値 `FileVersion` は4個の16bit整数という制約があり、従来どおり `major.minor.patch.0` です。これは一意なビルド識別子ではありません。インストール先の完全な版番号、`BUILD-INFO.json` のソースcommit・run・attempt、setup.exeのSHA-256で配布物を特定します。版のコア部分が65535を超える場合は、丸めずにビルドを停止します。

## 同じ版を別の内容で作らない

1. `ci-version.ps1` は `ci-identity.mjs` で版を割り当て、create-onlyのローカルreceiptを作ってから版を同期します
2. `ci-package.ps1` は署名前に包装処理をcreate-onlyで予約します。既存の出力ディレクトリや包装receiptがあれば、空であっても停止します
3. 署名済みインストーラのSHA-256を計算し、チェックサムと `BUILD-INFO.json` をcreate-onlyで確定します
4. Actions artifactは上書きしません。検証jobにはビルドが返したartifact IDを渡し、同名の別の成果物を探索して代用しません
5. 検証jobはcommit・リポジトリ・run・元のattempt・版・ファイル名・SHA-256を確認してから、署名検証とインストールへ進みます

同時に2つの処理が同じcheckoutを使っても、予約に成功するのは1つだけです。包装の途中で失敗した場合もreceiptを消して同じ版を作り直さず、ビルドjobを再実行して新しいattemptの版を使います。receiptと出力ディレクトリはキャッシュ対象にしません。

検証jobだけの再実行では、新しい版を割り当てません。元のビルドのartifact ID、内部版番号、元のattempt、署名済みバイナリとSHA-256を使い続けます。検証レポートのartifact名には現在のrun IDとattemptを含め、前の記録を残します。

既存installerも、同じ完全版番号のディレクトリがある場合は上書きを拒否します。これを解除して衝突を回避することはしません。

## 正式なプレリリース版の予約

通常CIの採番は `v1.7.0-beta.8` のような正式Release番号の予約ではありません。通常CIは引き続きReleaseを作らず、`contents: read` のままです。

正式ベータ版は、通常CIと区別して次の順序で準備します。Actionsは全段階で `contents: read` のままで、GitHub APIにはGETのみを送ります。タグやReleaseの作成、公開添付は手動で行います。

1. 版番号をソースと全製品宣言に設定し、通常CIで変更を検証します
2. 別途承認された操作者が、検証済みのソースcommitから `release-build/v<version>` を新規作成します。このpushで起動したrunのIDとattemptを確認します
3. 操作者は、同じソースcommitを唯一の親として `.github/release-reservation.json` だけを追加したcommitを作ります。JSONにはschema、版、リポジトリ名とID、ソースcommit、run ID、attemptを記録します。`release-identity.mjs` の `encodeReleaseReservation` が返す正規形をそのまま使います
4. 操作者は完成済みのreceipt commitを指す `release-reservations/v<version>` をcreate-onlyで新規作成します。既存の予約branchは、同じcommitであっても再予約成功とは扱いません。候補番号の一覧確認やタグの有無の確認だけでは原子的な予約になりません
5. Actionsは予約を最大10分待ち、リポジトリID・全JSON項目・Git blob hash・親commit・receipt以外の変更がないことを検証します。最初に読んだ予約commitを固定し、以後の読み取りでもbranchの移動を拒否します。既存の同名タグ・Releaseがあれば停止します
6. 予約された1つのrun・attemptだけが、CI接尾辞のない完全版番号でビルドできます。予約後にビルドが失敗した版は永久に使用済みとし、予約branchを消す・動かすなどして再利用しません。新しい版で新しい予約を作ります
7. 別のクリーンなWindows VMが、最終署名済みexeそのものをインストールして検証します。検証jobだけの再実行は元のartifact IDと元のビルドattemptを使います。ビルドjobの再実行は、同じ正式版の予約では拒否されます
8. 全検証の成功後だけ `nospacekey-release-ready-<run ID>-<attempt>` が作られます。同じexeと `SHA256SUMS.txt`、手動公開用の説明・確認manifest・Release notesが入ります。再ビルドや再署名はしません

手動公開では `MANUAL-RELEASE.md` に従い、指定のタグ、ソースcommit、Target branch、SHA-256を確認します。公開添付はexeとチェックサムの2点だけです。ベータReleaseをプレリリースとし、最新の安定版としては指定しません。既存Releaseや添付の置換・削除・clobber、タグや予約branchのforce更新はしません。公開途中の再試行は、同じ予約とハッシュを確認できる不足操作だけを続行します。

この方式は予約branchを維持する運用を前提とし、管理者による手動削除・force更新まで禁止するものではありません。手動公開までの間に別の操作者が同名タグやReleaseを作ることもあるため、公開直前にも有無を確認します。Actionsに公開権限や追加のsecretを与える必要はありません。

## 自動テスト

```sh
node --test scripts/tests/*.test.mjs
```

IDの精度・別run/attempt/repositoryの分離・不正値拒否・同時予約・包装の重複拒否・成果物の改変拒否・検証だけの再実行に加え、予約receiptの厳密検証と手動公開用成果物の生成条件を確認します。PowerShell版同期、Authenticode署名、実installerの検証はWindows workflowの後続テストで確認します。

参考: [GitHub Actionsの変数](https://docs.github.com/en/actions/reference/workflows-and-actions/variables)、[Windows VERSIONINFO](https://learn.microsoft.com/en-us/windows/win32/menurc/versioninfo-resource)
