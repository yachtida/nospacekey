use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONINFORMATION, MB_SETFOREGROUND, MB_YESNO,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManualEvidence {
    shown: usize,
    accepted: usize,
    dismissed: usize,
}

impl ManualEvidence {
    fn passed(self) -> bool {
        self.shown >= 2 && self.accepted >= 1 && self.dismissed >= 1
    }
}

fn log_line_pid(line: &str) -> Option<u32> {
    line.strip_prefix("[pid ")?.split_once(']')?.0.parse().ok()
}

fn evidence_from_log(log: &str, allowed_pids: &HashSet<u32>) -> ManualEvidence {
    let relevant = log
        .lines()
        .filter(|line| log_line_pid(line).is_some_and(|pid| allowed_pids.contains(&pid)));
    let mut evidence = ManualEvidence {
        shown: 0,
        accepted: 0,
        dismissed: 0,
    };
    for line in relevant {
        evidence.shown += usize::from(line.contains("ev=prediction_show"));
        evidence.accepted += usize::from(line.contains("ev=prediction_accept"));
        evidence.dismissed += usize::from(line.contains("ev=prediction_dismiss"));
    }
    evidence
}

fn tasklist_pids_for_image_from_output(output: &str, image_name: &str) -> HashSet<u32> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line
                .trim_start_matches('\u{feff}')
                .trim()
                .trim_matches('"')
                .split("\",\"");
            let name = fields.next()?;
            let pid = fields.next()?.parse().ok()?;
            name.eq_ignore_ascii_case(image_name).then_some(pid)
        })
        .collect()
}

fn tasklist_pids_for_image(image_name: &str) -> Result<HashSet<u32>, String> {
    let output = Command::new("tasklist")
        .args([
            "/FI",
            &format!("IMAGENAME eq {image_name}"),
            "/FO",
            "CSV",
            "/NH",
        ])
        .output()
        .map_err(|error| format!("tasklist failed for {image_name}: {error}"))?;
    if !output.status.success() {
        return Err(format!("tasklist failed for {image_name}"));
    }
    Ok(tasklist_pids_for_image_from_output(
        &String::from_utf8_lossy(&output.stdout),
        image_name,
    ))
}

fn empty_evidence() -> ManualEvidence {
    ManualEvidence {
        shown: 0,
        accepted: 0,
        dismissed: 0,
    }
}

fn manual_notepad_executable(_current_executable: &Path) -> PathBuf {
    PathBuf::from(r"C:\Windows\System32\notepad.exe")
}

fn first_existing_path(candidates: impl IntoIterator<Item = PathBuf>) -> PathBuf {
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_default()
}

fn tasklist_shows_image(output: &str, image_name: &str) -> bool {
    output.lines().any(|line| {
        line.trim_start_matches('\u{feff}')
            .trim_start()
            .trim_start_matches('"')
            .split('"')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(image_name))
    })
}

fn image_is_running(image_name: &str) -> bool {
    Command::new("tasklist")
        .args([
            "/FI",
            &format!("IMAGENAME eq {image_name}"),
            "/FO",
            "CSV",
            "/NH",
        ])
        .output()
        .is_ok_and(|output| {
            tasklist_shows_image(&String::from_utf8_lossy(&output.stdout), image_name)
        })
}

struct ManualApp {
    name: &'static str,
    executable: PathBuf,
    image_name: &'static str,
    arguments: Vec<String>,
}

struct ManualAppResult {
    name: &'static str,
    launched: bool,
    unavailable: bool,
    confirmed: bool,
    evidence: ManualEvidence,
}

fn manual_settings() -> settings::Settings {
    let mut configured = settings::Settings::default();
    configured.inline_prediction.enabled = true;
    configured
}

fn gate_status(results: &[ManualAppResult]) -> &'static str {
    if results
        .iter()
        .any(|result| result.name != "Word" && !result.passed())
    {
        return "FAIL";
    }
    match results.iter().find(|result| result.name == "Word") {
        Some(result) if result.passed() => "PASS",
        Some(result) if result.unavailable => "PENDING",
        _ => "FAIL",
    }
}

fn prepare_manual_profile(scratch: &Path) -> Result<(), String> {
    let model_dir = std::env::var_os("NOSPACEKEY_PREDICTION_MODEL_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "NOSPACEKEY_PREDICTION_MODEL_DIR is not set".to_string())?;
    for required in [
        "llm-jp-3-150m-q8_0-c060ca9.gguf",
        "tokenizer.json",
        "VERIFIED",
    ] {
        if !model_dir.join(required).is_file() {
            return Err(format!("prediction artifact is missing: {required}"));
        }
    }

    let local_app_data = scratch.join("local-app-data");
    std::fs::create_dir_all(&local_app_data)
        .map_err(|error| format!("create isolated LOCALAPPDATA: {error}"))?;
    std::env::set_var("LOCALAPPDATA", &local_app_data);
    settings::save(&manual_settings())
        .map_err(|error| format!("save isolated inline-prediction settings: {error}"))
}

impl ManualAppResult {
    fn passed(&self) -> bool {
        self.launched && self.confirmed && self.evidence.passed()
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn confirm_app(name: &str) -> bool {
    let message = format!(
        "{name} で次を確認してください。\n\n\
         1. Win+Space で nospacekey を選ぶ\n\
         2. 長めの日本語を明示確定し、灰色の予測を表示する\n\
         3. 右矢印で受理し、本文の欠落や重複がないことを確認する\n\
         4. もう一度予測を出し、Escで却下して残留しないことを確認する\n\
         5. 通常入力が続けられることを確認する\n\
         6. 予測表示中にこのダイアログへAlt+Tabし、戻ったとき消えていることを確認する\n\n\
         すべて通れば「はい」、一つでも失敗なら「いいえ」を押してください。"
    );
    let title = format!("Nospacekey インライン予測 手動受入 — {name}");
    let message = wide(&message);
    let title = wide(&title);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONINFORMATION | MB_SETFOREGROUND,
        ) == IDYES
    }
}

fn tip_log_path() -> PathBuf {
    std::env::var_os("TEMP")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("nospacekey-tip.log")
}

fn read_log(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(log) => Ok(log),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("read TIP log: {error}")),
    }
}

fn appended_log<'a>(before: &str, after: &'a str) -> Result<&'a str, String> {
    after
        .strip_prefix(before)
        .ok_or_else(|| "TIP log rotated or changed during manual acceptance".to_string())
}

fn app_cleanup_arguments(image_name: &str) -> [&str; 4] {
    ["/IM", image_name, "/T", "/F"]
}

fn stop_app(child: &mut Child, image_name: &str) {
    let _ = Command::new("taskkill")
        .args(app_cleanup_arguments(image_name))
        .status();
    let _ = child.wait();
}

fn run_app(app: &ManualApp, log_path: &Path) -> ManualAppResult {
    let before = match read_log(log_path) {
        Ok(log) => log,
        Err(error) => {
            eprintln!("manual app infrastructure failure: {} ({error})", app.name);
            return ManualAppResult {
                name: app.name,
                launched: false,
                unavailable: false,
                confirmed: false,
                evidence: empty_evidence(),
            };
        }
    };
    let mut child = match Command::new(&app.executable).args(&app.arguments).spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("manual app unavailable: {} ({error})", app.name);
            return ManualAppResult {
                name: app.name,
                launched: false,
                unavailable: true,
                confirmed: false,
                evidence: empty_evidence(),
            };
        }
    };
    std::thread::sleep(Duration::from_secs(4));
    let mut allowed_pids = match tasklist_pids_for_image(app.image_name) {
        Ok(pids) => pids,
        Err(error) => {
            eprintln!("manual app infrastructure failure: {} ({error})", app.name);
            stop_app(&mut child, app.image_name);
            return ManualAppResult {
                name: app.name,
                launched: true,
                unavailable: false,
                confirmed: false,
                evidence: empty_evidence(),
            };
        }
    };
    allowed_pids.insert(child.id());
    let confirmed = confirm_app(app.name);
    std::thread::sleep(Duration::from_millis(500));
    let latest_pids = tasklist_pids_for_image(app.image_name);
    let evidence = latest_pids
        .map(|latest| allowed_pids.extend(latest))
        .and_then(|()| read_log(log_path))
        .and_then(|after| appended_log(&before, &after).map(str::to_owned))
        .map(|delta| evidence_from_log(&delta, &allowed_pids))
        .unwrap_or_else(|error| {
            eprintln!("manual app infrastructure failure: {} ({error})", app.name);
            empty_evidence()
        });
    stop_app(&mut child, app.image_name);
    ManualAppResult {
        name: app.name,
        launched: true,
        unavailable: false,
        confirmed,
        evidence,
    }
}

fn run_apps(scratch: PathBuf, apps: Vec<ManualApp>) -> i32 {
    if let Some(app) = apps.iter().find(|app| image_is_running(app.image_name)) {
        eprintln!(
            "manual inline-app host already has {} running; close it before the isolated acceptance run",
            app.name
        );
        return 2;
    }
    let _ = std::fs::create_dir_all(&scratch);
    let text_file = scratch.join("acceptance.txt");
    let _ = std::fs::write(&text_file, "");
    crate::driver::kill_engine_processes();
    if let Err(error) = prepare_manual_profile(&scratch) {
        eprintln!("manual inline-app profile setup failed: {error}");
        return 2;
    }
    let log_path = tip_log_path();
    let results: Vec<_> = apps.iter().map(|app| run_app(app, &log_path)).collect();
    for result in &results {
        println!(
            "manual_app name={} launched={} unavailable={} confirmed={} shown={} accepted={} dismissed={} passed={}",
            result.name, result.launched, result.unavailable, result.confirmed,
            result.evidence.shown, result.evidence.accepted, result.evidence.dismissed,
            result.passed(),
        );
    }
    let status = gate_status(&results);
    println!(
        "manual_inline_apps : {status} (notepad={} edge={} vscode={} word={} word_unavailable={})",
        results[0].passed(),
        results[1].passed(),
        results[2].passed(),
        results[3].passed(),
        results[3].unavailable,
    );
    if status == "PASS" {
        0
    } else {
        1
    }
}

pub(crate) fn run() -> i32 {
    let desktop = PathBuf::from(r"C:\Users\WDAGUtilityAccount\Desktop");
    let scratch = desktop.join("manual-inline-acceptance");
    let text_file = scratch.join("acceptance.txt");
    let current_executable = std::env::current_exe().unwrap_or_default();
    let apps = vec![
        ManualApp {
            name: "Notepad",
            executable: manual_notepad_executable(&current_executable),
            image_name: "notepad.exe",
            arguments: vec![text_file.to_string_lossy().into_owned()],
        },
        ManualApp {
            name: "Edge",
            executable: desktop.join(r"manual-edge\msedge.exe"),
            image_name: "msedge.exe",
            arguments: vec![
                "--no-first-run".into(),
                format!("--user-data-dir={}", scratch.join("edge-data").display()),
                "--app=data:text/html,%3Cmeta%20charset%3Dutf-8%3E%3Ctextarea%20autofocus%20style%3D%22width%3A90vw%3Bheight%3A80vh%3Bfont-size%3A24px%22%3E%3C%2Ftextarea%3E".into(),
            ],
        },
        ManualApp {
            name: "VS Code",
            executable: desktop.join(r"manual-vscode\Code.exe"),
            image_name: "Code.exe",
            arguments: vec![
                "--disable-extensions".into(),
                format!("--user-data-dir={}", scratch.join("vscode-data").display()),
                format!("--extensions-dir={}", scratch.join("vscode-extensions").display()),
                "--new-window".into(),
                text_file.to_string_lossy().into_owned(),
            ],
        },
        ManualApp {
            name: "Word",
            executable: desktop.join(r"manual-word\WINWORD.EXE"),
            image_name: "WINWORD.EXE",
            arguments: vec!["/q".into()],
        },
    ];

    run_apps(scratch, apps)
}

pub(crate) fn run_host() -> i32 {
    let scratch = std::env::temp_dir().join(format!(
        "nospacekey-manual-inline-host-{}",
        std::process::id()
    ));
    let text_file = scratch.join("acceptance.txt");
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default();
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    let program_files_x86 = std::env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));

    let apps = vec![
        ManualApp {
            name: "Notepad",
            executable: PathBuf::from(r"C:\Windows\System32\notepad.exe"),
            image_name: "notepad.exe",
            arguments: vec![text_file.to_string_lossy().into_owned()],
        },
        ManualApp {
            name: "Edge",
            executable: first_existing_path([
                program_files_x86.join(r"Microsoft\Edge\Application\msedge.exe"),
                program_files.join(r"Microsoft\Edge\Application\msedge.exe"),
            ]),
            image_name: "msedge.exe",
            arguments: vec![
                "--no-first-run".into(),
                format!("--user-data-dir={}", scratch.join("edge-data").display()),
                "--app=data:text/html,%3Cmeta%20charset%3Dutf-8%3E%3Ctextarea%20autofocus%20style%3D%22width%3A90vw%3Bheight%3A80vh%3Bfont-size%3A24px%22%3E%3C%2Ftextarea%3E".into(),
            ],
        },
        ManualApp {
            name: "VS Code",
            executable: first_existing_path([
                local_app_data.join(r"Programs\Microsoft VS Code\Code.exe"),
                program_files.join(r"Microsoft VS Code\Code.exe"),
            ]),
            image_name: "Code.exe",
            arguments: vec![
                "--disable-extensions".into(),
                format!("--user-data-dir={}", scratch.join("vscode-data").display()),
                format!("--extensions-dir={}", scratch.join("vscode-extensions").display()),
                "--new-window".into(),
                text_file.to_string_lossy().into_owned(),
            ],
        },
        ManualApp {
            name: "Word",
            executable: first_existing_path([
                program_files.join(r"Microsoft Office\root\Office16\WINWORD.EXE"),
                program_files_x86.join(r"Microsoft Office\root\Office16\WINWORD.EXE"),
            ]),
            image_name: "WINWORD.EXE",
            arguments: vec!["/q".into()],
        },
    ];

    run_apps(scratch, apps)
}

#[cfg(test)]
mod tests {
    use super::{
        app_cleanup_arguments, appended_log, evidence_from_log, gate_status,
        manual_notepad_executable, manual_settings, tasklist_pids_for_image_from_output,
        tasklist_shows_image, ManualAppResult, ManualEvidence,
    };
    use std::collections::HashSet;

    #[test]
    fn notepad_uses_only_the_fixed_guest_system_launcher() {
        let executable =
            manual_notepad_executable(std::path::Path::new(r"C:\guest\bin\testbench.exe"));
        assert_eq!(
            executable,
            std::path::PathBuf::from(r"C:\Windows\System32\notepad.exe")
        );
    }

    #[test]
    fn app_cleanup_never_targets_a_stale_process_id() {
        let arguments = app_cleanup_arguments("msedge.exe");
        assert_eq!(arguments, ["/IM", "msedge.exe", "/T", "/F"]);
        assert!(!arguments.iter().any(|argument| *argument == "/PID"));
    }

    #[test]
    fn tasklist_image_detection_requires_an_exact_csv_image_name() {
        let output = "\"msedge.exe\",\"123\",\"Console\",\"1\",\"20,000 K\"\r\n";
        assert!(tasklist_shows_image(output, "MSEdge.exe"));
        assert!(!tasklist_shows_image(output, "edge.exe"));
        assert!(!tasklist_shows_image(
            "INFO: No tasks are running",
            "msedge.exe"
        ));
    }

    #[test]
    fn tasklist_pid_parser_collects_only_the_target_image() {
        let output = "\"msedge.exe\",\"123\",\"Console\",\"1\",\"20,000 K\"\r\n\
                      \"other.exe\",\"456\",\"Console\",\"1\",\"10,000 K\"\r\n";
        assert_eq!(
            tasklist_pids_for_image_from_output(output, "MSEdge.exe"),
            HashSet::from([123])
        );
    }

    #[test]
    fn manual_profile_enables_inline_prediction() {
        let configured = manual_settings();
        assert!(configured.inline_prediction.enabled);
        assert!(settings::resolve_env_map(&configured, None, |_| None)
            .iter()
            .any(|(key, value)| key == "NOSPACEKEY_INLINE_PREDICTION" && value == "1"));
    }

    #[test]
    fn unavailable_word_is_pending_but_required_app_failure_is_fail() {
        let passed = |name| ManualAppResult {
            name,
            launched: true,
            unavailable: false,
            confirmed: true,
            evidence: ManualEvidence {
                shown: 2,
                accepted: 1,
                dismissed: 1,
            },
        };
        let pending_word = ManualAppResult {
            name: "Word",
            launched: false,
            unavailable: true,
            confirmed: false,
            evidence: ManualEvidence {
                shown: 0,
                accepted: 0,
                dismissed: 0,
            },
        };
        assert_eq!(
            gate_status(&[
                passed("Notepad"),
                passed("Edge"),
                passed("VS Code"),
                pending_word,
            ]),
            "PENDING"
        );

        let missing_edge = ManualAppResult {
            name: "Edge",
            launched: false,
            unavailable: true,
            confirmed: false,
            evidence: ManualEvidence {
                shown: 0,
                accepted: 0,
                dismissed: 0,
            },
        };
        assert_eq!(
            gate_status(&[
                passed("Notepad"),
                missing_edge,
                passed("VS Code"),
                passed("Word"),
            ]),
            "FAIL"
        );
    }

    #[test]
    fn manual_app_evidence_requires_show_accept_and_dismiss() {
        let allowed = HashSet::from([1]);
        let complete = evidence_from_log(
            "[pid 1] ev=prediction_show\n\
             [pid 1] ev=prediction_accept\n\
             [pid 1] ev=prediction_show\n\
             [pid 1] ev=prediction_dismiss\n",
            &allowed,
        );
        assert!(complete.passed());
        assert_eq!(
            (complete.shown, complete.accepted, complete.dismissed),
            (2, 1, 1)
        );

        assert!(!evidence_from_log("[pid 1] ev=prediction_show\n", &allowed).passed());
        assert!(!evidence_from_log(
            "[pid 1] ev=prediction_show\n[pid 1] ev=prediction_accept\n",
            &allowed,
        )
        .passed());
        assert!(!evidence_from_log(
            "[pid 2] ev=prediction_show\n[pid 2] ev=prediction_accept\n\
             [pid 2] ev=prediction_show\n[pid 2] ev=prediction_dismiss\n",
            &allowed,
        )
        .passed());
    }

    #[test]
    fn log_delta_fails_closed_on_rotation() {
        assert_eq!(appended_log("old", "old\nnew").unwrap(), "\nnew");
        assert!(appended_log("old", "new").is_err());
    }
}
