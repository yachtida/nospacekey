# ビルド版番号と配布物の同一性

## CI版の採番

以下はCI用の識別子と単体テストで維持する規則です。現在のworkflowでは通常push/PR/手動実行は軽いチェックだけを行い、インストーラを作りません。配布用ビルドは後述の正式タグpushに限定します。

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

## 正式タグによる配布用ビルドの予約

対応する製品版はWindows互換の `major.minor.patch` または `major.minor.patch-beta.N`（Nは正の整数）です。タグは必ずソースの製品版と完全に一致する `v<version>` にします。メタデータ、CI接尾辞、別版のタグ、注釈付きタグは受け付けません。

通常push/PR/手動workflow実行では版宣言・Nodeの予約/採番テスト・設定UIの静的/単体チェックのみ実行します。Windows製品ビルドは公開リポジトリへの正式な軽量タグの新規pushだけで開始します。全ジョブが `contents: read` のままで、GitHub APIにはGETのみを送ります。新たなsecret・認証やリポジトリ設定変更は不要です。

1. 版番号とRelease notesをソースに設定し、全製品宣言の一致と通常チェックを確認します。このworkflow変更を含む確定commitを使います。既存の予約版番号を再利用しません
2. 操作者がそのcommitを指す軽量タグ `v<version>` を新規pushします。commitを指すタグを使い、注釈付きタグは使いません。タグはcreate-onlyで扱い、更新・force push・削除・再作成はしません。起動したrunのIDとattemptを確認します
3. 同じソースcommitを唯一の親として `.github/release-reservation.json` だけを追加したreceipt commitを作ります。JSONはschema 1を維持し、版・リポジトリ名とID・ソースcommit・run ID・attemptを記録します。`release-identity.mjs` の `encodeReleaseReservation` が返す正規形を使います。context.refは `refs/tags/v<version>`、eventNameは `push` です
4. receipt commitを指す `release-reservations/v<version>` をcreate-onlyで新規作成します。既存の予約branchは同じcommitでも再予約成功とは扱いません。タグだけでは再実行や同時実行を防げず、この予約branchが唯一の所有者を決めます
5. 製品ビルドは高価なツール導入より先に、push eventが新規作成であり削除・force更新でないこと、checkout HEADがGITHUB_SHAであること、タグがそのソースcommitを直接指すことを確認します。予約を最大10分待ち、リポジトリID・全JSON項目・Git blob hash・唯一の親commit・receipt以外の変更がないことを検証します。最初に読んだreceipt commitを固定します
6. 版割当・包装前・署名後の確定・別VM検証・手動公開bundleの各境界で、同じreceipt commitとその所有run/attempt、同じソースを指すタグ、同名Releaseが未作成であることをGETで再確認します。読み取りの前後でもタグと予約refを確認し、移動・削除・不正応答は停止します。同時に起動した別runや、ビルドjobの新attemptは同じreceiptを利用できません
7. 予約された1つのrun・attemptだけが製品版でビルド・署名できます。予約後に失敗した版は永久に使用済みです。予約branchやタグを消す・動かす、receiptを書き換える、再署名することで再利用せず、新しい製品版と新しいタグ・予約を使います
8. 既存のRust・Swift・IPC・署名検証と、別のクリーンなWindows VMへのインストール・入力・変換・削除検証を行います。検証jobだけの再実行では、元のartifact ID・元のビルドattempt・同じ署名済みexeを使います
9. 全検証成功後だけ `nospacekey-release-ready-<run ID>-<attempt>` を作ります。同じ署名済みexe、チェックサム、手動公開用説明・manifest・Release notesをコピーし、再ビルドや再署名はしません。manifestの `target_ref` は既存タグ、`target_commit` は固定ソースcommitです

手動公開では `MANUAL-RELEASE.md` に従い、**既存タグ**を選択してソースcommitとSHA-256を確認します。新規タグや別のTarget branchを指定しません。公開添付はexeとチェックサムの2点だけです。ベータはプレリリース、安定版は通常Releaseに設定し、どちらも最新指定は自動で行いません。既存Release・添付の置換・削除・clobberはしません。公開途中の再試行は、同じ予約とハッシュを確認できる不足操作だけを続行します。

この方式は予約branchとタグを維持する運用を前提とし、管理者による削除・force更新や、チェック間の一時的なref移動まで技術的に禁止するものではありません。タグ保護・権限設定は変更しません。公開直前にも既存タグのcommit・予約・同名Releaseの有無を確認します。

移行前の `release-build/v<version>` で完了したビルドと予約はそのまま維持します。旧ソースのworkflowにはタグpushトリガーがないため、その旧commitへの後付けタグでは新方式は起動しません。新方式のrunから旧receiptを使おうとしてもrun IDが一致せず、同じ製品版を再ビルドできません。移行前の完了済みbundleは生成し直さず、元の手動公開手順を使います。

## 自動テスト

```sh
node --test scripts/tests/*.test.mjs
```

IDの精度・別run/attempt/repositoryの分離・不正値拒否・同時予約・包装の重複拒否・成果物の改変拒否・検証だけの再実行に加え、予約receiptの厳密検証・タグ/イベントの正負ケース・タグ移動/削除・安定版/ベータ版の手動公開用成果物の生成条件を確認します。PowerShell版同期、Authenticode署名、実installerの検証はWindows workflowの後続テストで確認します。

参考: [GitHub Actionsの変数](https://docs.github.com/en/actions/reference/workflows-and-actions/variables)、[Windows VERSIONINFO](https://learn.microsoft.com/en-us/windows/win32/menurc/versioninfo-resource)
