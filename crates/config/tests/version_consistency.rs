// ⑤ 版整合の自動検証: Cargo(workspace) / tauri.conf.json / installer/version.iss /
// BuildInfo.swift が一致しないと fail。将来の release.ps1 はこのテストと
// scripts/sync-version.ps1 -Check をフェイルファストに使う。
use std::fs;
use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
} // crates/config

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel)).unwrap()
}

/// ルート Cargo.toml の [workspace.package] 節から version を素朴に抜く(toml crate 不要)。
fn workspace_version() -> String {
    let s = read("../../Cargo.toml");
    let sect = s
        .split("[workspace.package]")
        .nth(1)
        .expect("[workspace.package] 節がない");
    sect.lines()
        .take_while(|l| !l.trim_start().starts_with('[')) // 次の節見出しで走査を打ち切る
        .find_map(|l| {
            l.trim()
                .strip_prefix("version")
                .and_then(|r| r.split('"').nth(1))
                .map(str::to_string)
        })
        .expect("workspace.package.version がない")
}

fn windows_file_version(version: &str) -> String {
    format!("{}.0", version.split('-').next().unwrap())
}

#[test]
fn all_version_declarations_match_workspace_package() {
    let ws = workspace_version();
    // config crate 自身が workspace 継承している証明(= 全 crate の代表)
    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        ws,
        "crate が version.workspace=true を継承していない"
    );
    // tauri.conf.json
    let conf: serde_json::Value = serde_json::from_str(&read("tauri.conf.json")).unwrap();
    assert_eq!(
        conf["version"].as_str().unwrap(),
        ws,
        "tauri.conf.json の version 不一致"
    );
    // installer/version.iss
    assert!(
        read("../../installer/version.iss").contains(&format!("#define MyAppVersion \"{ws}\"")),
        "version.iss 不一致"
    );
    assert!(
        read("../../installer/version.iss").contains(&format!(
            "#define MyAppFileVersion \"{}\"",
            windows_file_version(&ws)
        )),
        "version.iss の Windows ファイルバージョン不一致"
    );
    // engine-host BuildInfo.swift
    assert!(
        read("../../engine-host/Sources/NospacekeyEngineCore/BuildInfo.swift")
            .contains(&format!("version = \"{ws}\"")),
        "BuildInfo.swift 不一致"
    );
}

#[test]
fn nospacekey_iss_has_no_hardcoded_version() {
    // .iss 本体に版リテラルが復活しないこと(#include "version.iss" 経由のみ)
    let iss = read("../../installer/nospacekey.iss");
    assert!(iss.contains("#include \"version.iss\""));
    assert!(iss.contains("VersionInfoVersion={#MyAppFileVersion}"));
    assert!(
        !iss.contains("#define MyAppVersion \""),
        "nospacekey.iss に版の直書きが復活している"
    );
}

#[test]
fn installer_keeps_loaded_pairs_in_versioned_directories() {
    let iss = read("../../installer/nospacekey.iss");
    let tip_registration = read("../tip/src/register.rs");
    let versioned = r#"{app}\versions\{#MyAppVersion}"#;
    assert!(
        iss.contains(&format!(
            r#"Source: "..\dist\nospacekey_tip.dll"; DestDir: "{versioned}"; Flags: 64bit onlyifdoesntexist"#
        )),
        "TIP must be copied without Inno's non-transactional regserver flag"
    );
    assert!(!iss.contains("Flags: regserver"));
    assert!(
        iss.contains(&format!(
            r#"Source: "..\dist\NospacekeyEngineHost.exe"; DestDir: "{versioned}"; Flags: 64bit onlyifdoesntexist"#
        )),
        "the matching EngineHost must be beside its TIP"
    );
    assert!(
        !iss.contains("taskkill /F /IM NospacekeyEngineHost.exe"),
        "upgrade/uninstall must not kill an EngineHost still serving a loaded old TIP"
    );
    assert!(
        !iss.contains("restartreplace") && !iss.contains("uninsrestartdelete"),
        "versioned binaries must never be overwritten or deleted at reboot"
    );
    assert!(!iss.contains("ignoreversion"));
    assert!(iss.contains("function PrepareToInstall(var NeedsRestart: Boolean): String;"));
    assert!(iss.contains("if DirExists(ExpandConstant('{app}\\versions\\{#MyAppVersion}')) then"));
    assert!(iss.contains(
        "Result := '同じバージョンの製品ファイルがすでに存在するため、安全に上書きできません。"
    ));
    assert!(iss.contains("--repair-update-task"));
    assert!(iss.contains("waituntilterminated runasoriginaluser"));
    let config_main = read("src/main.rs");
    let repair_fast_path = config_main
        .find("LaunchIntent::RepairUpdateTask")
        .expect("repair fast path must exist");
    let singleton_setup = config_main
        .find("tauri::Builder::default()")
        .expect("normal singleton setup must exist");
    assert!(
        repair_fast_path < singleton_setup,
        "installer task repair must exit before acquiring the Config singleton"
    );
    assert!(!iss.contains("CleanupInactiveVersions"));
    assert!(!iss.contains("DelTree(Candidate"));
    assert!(iss.contains("reclamation is tracked separately in #45"));
    assert!(
        iss.contains("アプリを再起動すると新版に切り替わります")
            && iss.contains("Windows の再起動は不要です"),
        "upgrade must explain the application-only restart boundary"
    );
    assert!(iss.contains("学習履歴は安全のためバージョンごとに分離"));
    assert!(iss.contains("function CapturePriorTipPath(): String;"));
    assert!(
        iss.contains("SetupMutex=nospacekey-tip-registration,Global\\nospacekey-tip-registration")
    );
    assert!(iss.contains("UsePreviousAppDir=no"));
    assert!(iss.contains("DisableDirPage=yes"));
    assert!(iss.contains("function IsExpectedInstallPath(): Boolean;"));
    assert!(iss.contains("function LoadPreviousInstallRoot(): String;"));
    assert!(iss.contains("'InstallLocation', Candidate"));
    assert!(iss.contains("Legacy := PreviousInstallRoot + '\\nospacekey_tip.dll';"));
    assert!(iss.contains("VersionedRoot := PreviousInstallRoot + '\\versions\\';"));
    assert!(iss.contains("(not PreviousInstall) or (PreviousInstallRoot = '')"));
    assert!(iss.contains("function QueryOriginalUserClsidOverlay(): Integer;"));
    assert!(iss.contains("ExecAsOriginalUser("));
    assert!(iss.contains("[Microsoft.Win32.RegistryView]::Registry64"));
    assert!(iss.contains("$base.Dispose(); exit 0 } catch { exit 20 }"));
    assert!(iss.contains("procedure RegisterTipTransaction();"));
    assert!(iss.contains("procedure RollbackTipRegistration();"));
    assert!(iss.contains("PriorTipValidationFailed := True;"));
    assert!(iss.contains("if PriorTipValidationFailed then"));
    assert!(iss.contains("if NewTipRegistered and (not InstallCompleted) then"));
    assert!(iss.contains("function TipRegistrationMatches(const ExpectedPath: String): Boolean;"));
    assert!(iss.contains("ThreadingModel") && iss.contains("Apartment"));
    assert!(iss.contains(
        "if RunTrustedTipRestore(PriorTipPath) and TipRegistrationMatches(PriorTipPath) then"
    ));
    assert!(iss.contains("(not TipRegistrationMatches(NewTipPath())) then"));
    assert!(iss.contains("RunTrustedTipRestore(PriorTipPath)"));
    assert!(!iss.contains("RunTipRegistration(PriorTipPath"));
    assert!(!iss.contains("Exec(PriorTipPath"));
    assert!(!iss.contains("Params := '/s \"' + PriorTipPath"));
    assert!(iss.contains("' \"' + NewTipPath() + '\"'"));
    assert_eq!(iss.matches("IsRetainedTipPath(").count(), 3);
    assert!(iss.contains("/n /i:restore-utf16hex="));
    for restore_case in [
        r#"D:\custom nospacekey\nospacekey_tip.dll"#,
        r#"D:\custom nospacekey\versions\1.2.3-beta.4+sha.abc\nospacekey_tip.dll"#,
        r#"D:\custom nospacekey\versions\v1.2.3\nospacekey_tip.dll"#,
        r#"D:\custom nospacekey\versions\1.2.3\nospacekey_tip.dll:evil"#,
        r#"D:\foreign\nospacekey_tip.dll"#,
    ] {
        assert!(tip_registration.contains(restore_case), "{restore_case}");
    }
    assert!(iss.contains("UninstallTipWasActive := IsCurrentTipActive();"));
    assert!(iss.contains("function InitializeUninstall(): Boolean;"));
    assert!(iss.contains("function RecoverInterruptedUninstallClaim(): Boolean;"));
    assert!(iss.contains("-RecoverUninstallClaim"));
    assert!(iss.contains("function CommitUninstallTasks(): Boolean;"));
    assert!(iss.contains("function FinalizeUninstallTasks(): Boolean;"));
    assert!(iss.contains("(not UninstallResumeDeleting) and (not CommitUninstallTasks())"));
    assert!(iss.contains("-ValidateDeletingUninstall -UninstallBuild ''{#MyAppVersion}''"));
    assert!(iss.contains("function ValidateDeletingUninstallResume(): Integer;"));
    assert_eq!(iss.matches("ValidateDeletingUninstallResume()").count(), 3);
    let uninstall_step = iss
        .find("if CurUninstallStep = usUninstall then begin")
        .expect("uninstall deletion callback must exist");
    let immediate_resume_validation = iss
        .find("UninstallResumeDeleting and (ValidateDeletingUninstallResume() <> 0)")
        .expect("deleting resume must be revalidated immediately before removal");
    let task_commit = iss
        .find("(not UninstallResumeDeleting) and (not CommitUninstallTasks())")
        .expect("normal uninstall task commit must exist");
    assert!(
        uninstall_step < immediate_resume_validation && immediate_resume_validation < task_commit
    );
    assert!(iss.contains(
        "#define CleanupScriptSHA256 GetSHA256OfFile(\"..\\scripts\\version-cleanup.ps1\")"
    ));
    assert_eq!(iss.matches("RunTrustedCleanupScript(").count(), 10);
    assert!(!iss.contains("ExecutionPolicy Bypass -File"));
    assert!(iss.contains("CreateFileW@kernel32.dll"));
    assert!(iss.contains("CleanupOpenReparsePoint"));
    assert!(iss.contains("BuildCleanupBootstrapCommand"));
    assert!(iss.contains("Get-FileHash -InputStream $pin -Algorithm SHA256"));
    assert!(iss.contains("Test-SemanticallyProtectedDirectory") || iss.contains("function safe($p)"));
    assert!(iss.contains(
        "Parameters: \"{code:DeferredCleanupParameters}\"; Flags: runhidden nowait; Check: ValidateCurrentCleanupPayload"
    ));
    assert!(iss.contains(
        "if (CurStep = ssPostInstall) and (not ValidateCurrentCleanupPayload()) then"
    ));
    let trusted_runner = iss
        .find("function RunTrustedCleanupScript(")
        .expect("trusted cleanup bootstrap must exist");
    let non_reparse = iss[trusted_runner..]
        .find("TestNonReparsePath(Root, True)")
        .expect("trusted cleanup bootstrap must inspect ancestors");
    let embedded_hash = iss[trusted_runner..]
        .find("CompareText(GetSHA256OfFile(ScriptPath), '{#CleanupScriptSHA256}')")
        .expect("trusted cleanup bootstrap must pin script bytes");
    let script_pin = iss[trusted_runner..]
        .find("OpenPinnedCleanupScript(ScriptPath)")
        .expect("trusted cleanup bootstrap must pin the non-reparse script");
    let elevated_exec = iss[trusted_runner..]
        .find("Result := Exec(ExpandConstant('{sys}\\WindowsPowerShell")
        .expect("trusted cleanup bootstrap must own the elevated launch");
    assert!(
        non_reparse < script_pin
            && script_pin < embedded_hash
            && embedded_hash < elevated_exec
    );
    let powershell_hash = iss
        .find("Get-FileHash -InputStream $pin -Algorithm SHA256")
        .expect("elevated bootstrap must revalidate the script hash");
    let powershell_pin = iss
        .find("[IO.FileShare]::Read")
        .expect("elevated bootstrap must deny concurrent script writes and deletes");
    let powershell_script_acl = iss
        .find("-not (safe $script)")
        .expect("elevated bootstrap must reject a writable installed script");
    let powershell_invoke = iss
        .find("& $script -InstallRoot $root")
        .expect("elevated bootstrap must invoke the validated script");
    assert!(
        powershell_script_acl < powershell_pin
            && powershell_pin < powershell_hash
            && powershell_hash < powershell_invoke
    );
    assert!(iss.contains("OpenPinnedCleanupScript(ScriptPath)"));
    assert!(iss.contains("if AllowMissingTree then"));
    assert!(iss.contains("if ResultCode = 3 then begin"));
    assert!(iss.contains("InterruptedDeletingDetected := True;"));
    assert!(iss.contains(
        "中断した同じバージョンのアンインストーラーを再実行して、アンインストールを完了してください。"
    ));
    assert!(iss.contains("UninstallResumeDeleting := True;"));
    assert!(iss.contains("if IsCurrentTipActive() then begin"));
    let finalizer = iss
        .find("if not FinalizeUninstallTasks() then")
        .expect("usDone finalizer must exist");
    let finalized = iss
        .find("UninstallFinalized := True")
        .expect("successful finalization marker must exist");
    assert!(finalizer < finalized);
    assert!(iss.contains("if not UninstallFinalized then"));
    assert!(iss.contains("-ValidateFinalUninstallArtifacts"));
    assert!(iss.contains("DeleteFile(RecoveryPath)"));
    assert!(iss.contains("DeleteFile(ExpandConstant('{app}\\.nospacekey-task-transaction'))"));
    assert!(iss.contains("RemoveDir(ExpandConstant('{app}\\.nospacekey-uninstall'))"));
    let recovery_delete = iss
        .find("DeleteFile(RecoveryPath)")
        .expect("recovery script cleanup must exist");
    let dependency_delete = iss
        .find("RemoveDir(ExpandConstant('{app}\\.nospacekey-uninstall'))")
        .expect("journal cleanup must exist");
    assert!(
        recovery_delete < dependency_delete,
        "a failed recovery-script deletion must retain its complete dependency set"
    );
    assert!(iss.contains(".nospacekey-uninstall-recovery.ps1"));
    assert!(iss.contains("-RecoverInterruptedUninstalls"));
    assert!(iss.contains("-FinalizeUninstallTasks"));
    assert!(iss.contains(".nospacekey-task-transaction"));
    assert!(iss.contains("uninsneveruninstall"));
    assert!(!iss.contains(
        "Type: files; Name: \"{app}\\versions\\{#MyAppVersion}\\.nospacekey-lifetime.uninstalling\""
    ));
    assert!(iss.contains("Type: dirifempty; Name: \"{app}\\versions\\{#MyAppVersion}\""));
    let initialize_uninstall = iss
        .split("function InitializeUninstall(): Boolean;")
        .nth(1)
        .unwrap()
        .split("function CommitUninstallTasks(): Boolean;")
        .next()
        .unwrap();
    assert!(!initialize_uninstall.contains("-CommitUninstallTasks"));
    assert!(!initialize_uninstall.contains("Get-ScheduledTask"));
    assert!(!initialize_uninstall.contains("Unregister-ScheduledTask"));
    assert!(iss.contains("if UninstallClaimed and (not UninstallStarted) and"));
    assert!(iss.contains("function RestoreUninstallClaim(): Boolean;"));
    assert!(iss.contains("not RestoreUninstallClaim()"));
    assert!(iss.contains("UninstallSentinelRestored: Boolean;"));
    assert!(iss.contains("(not IsCurrentTipActive())"));
    assert!(config_main
        .find("VersionLease::acquire()")
        .is_some_and(|lease| lease < config_main.find("LaunchIntent::RepairUpdateTask").unwrap()));
    assert_eq!(
        iss.matches("'/u /s \"' + ExpandConstant").count(),
        1,
        "uninstall must unregister the active TIP once"
    );
}

#[test]
fn rust_and_swift_wire_versions_match() {
    let swift = read("../../engine-host/Sources/NospacekeyEngineCore/Protocol.swift");
    assert!(
        swift.contains(&format!(
            "static let current: UInt32 = {}",
            ipc::protocol::PROTO_VERSION
        )),
        "Swift wire version must match Rust protocol::PROTO_VERSION"
    );
}

#[test]
fn all_crates_inherit_workspace_version() {
    // env!(CARGO_PKG_VERSION) は config 1 crate の継承しか証明しない。
    // workspace crate 全部の Cargo.toml を読み、[package] 節(次の [ 行まで)に
    // version.workspace = true があることを assert — cargo test 単独で全所在が閉じる。
    // (sync-version.ps1 -Check の同種検査は release.ps1 用の冗長系)
    for c in [
        "tip",
        "ipc",
        "ids",
        "settings",
        "testbench",
        "config",
        "update",
    ] {
        let s = read(&format!("../{c}/Cargo.toml"));
        let pkg: Vec<&str> = s
            .split("[package]")
            .nth(1)
            .unwrap_or_else(|| panic!("{c}: [package] 節がない"))
            .lines()
            .take_while(|l| !l.trim_start().starts_with('[')) // 依存テーブルは対象外
            .collect();
        assert!(
            pkg.iter().any(|l| l.trim() == "version.workspace = true"),
            "{c}/Cargo.toml の [package] が version.workspace = true でない"
        );
        assert!(
            !pkg.iter()
                .any(|l| l.trim_start().starts_with("version = \"")),
            "{c}/Cargo.toml の [package] に version 直書きが残っている"
        );
    }
}
