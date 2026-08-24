//! per-user Scheduled Task の定義と lifecycle。
//!
//! Task Scheduler の XML を純関数として組み立てることで、Windows 実機なしでも
//! principal/trigger/policy を回帰テストできる。登録は `schtasks /XML` を用い、
//! password を保存せず InteractiveToken と LeastPrivilege を固定する。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub const TASK_FOLDER: &str = r"\nospacekey";
pub const TASK_NAME_PREFIX: &str = "UpdateCheck-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIdentity {
    pub sid: String,
}

impl TaskIdentity {
    pub fn name(&self) -> String {
        format!("{TASK_FOLDER}\\{TASK_NAME_PREFIX}{}", self.sid)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutput {
    success: bool,
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

trait ProcessHandle {
    fn try_wait(&mut self) -> std::io::Result<Option<bool>>;
    fn kill(&mut self) -> std::io::Result<()>;
    fn reap(&mut self) -> std::io::Result<CommandOutput>;
}

struct ChildHandle(Option<Child>);

impl ProcessHandle for ChildHandle {
    fn try_wait(&mut self) -> std::io::Result<Option<bool>> {
        self.0
            .as_mut()
            .ok_or_else(|| std::io::Error::other("child already reaped"))?
            .try_wait()
            .map(|status| status.map(|status| status.success()))
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.0
            .as_mut()
            .ok_or_else(|| std::io::Error::other("child already reaped"))?
            .kill()
    }

    fn reap(&mut self) -> std::io::Result<CommandOutput> {
        let child = self
            .0
            .take()
            .ok_or_else(|| std::io::Error::other("child already reaped"))?;
        let output = child.wait_with_output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

fn poll_process<P: ProcessHandle>(process: &mut P) -> Result<CommandOutput, String> {
    poll_process_with(process, COMMAND_TIMEOUT, Instant::now, std::thread::sleep)
}

fn poll_process_with<P, N, S>(
    process: &mut P,
    timeout: Duration,
    mut now: N,
    mut sleep: S,
) -> Result<CommandOutput, String>
where
    P: ProcessHandle,
    N: FnMut() -> Instant,
    S: FnMut(Duration),
{
    let deadline = now().checked_add(timeout).unwrap_or_else(Instant::now);
    loop {
        match process.try_wait() {
            Ok(Some(_)) => {
                return process
                    .reap()
                    .map_err(|error| format!("command output collection failed: {error}"));
            }
            Ok(None) if now() >= deadline => {
                let kill_error = process.kill().err();
                let output = process
                    .reap()
                    .map_err(|error| format!("command timed out and reap failed: {error}"))?;
                let detail = output_detail(&output);
                let kill_detail = kill_error
                    .map(|error| format!("; kill failed: {error}"))
                    .unwrap_or_default();
                return Err(format!(
                    "command timed out after {} ms{kill_detail}{detail}",
                    timeout.as_millis()
                ));
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(now());
                sleep(remaining.min(COMMAND_POLL_INTERVAL));
            }
            Err(error) => {
                let kill_error = process.kill().err();
                let output = process.reap().map_err(|reap_error| {
                    format!("command status polling failed: {error}; reap failed: {reap_error}")
                })?;
                let detail = output_detail(&output);
                let kill_detail = kill_error
                    .map(|kill_error| format!("; kill failed: {kill_error}"))
                    .unwrap_or_default();
                return Err(format!(
                    "command status polling failed: {error}{kill_detail}{detail}"
                ));
            }
        }
    }
}

fn output_detail(output: &CommandOutput) -> String {
    let text = if !output.stderr.is_empty() {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    } else {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

fn run_command(program: &str, args: &[String]) -> Result<CommandOutput, String> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("command spawn failed: {error}"))?;
    poll_process(&mut ChildHandle(Some(child)))
}

const TASK_NOT_FOUND_HRESULT: u32 = 0x8007_0002;

fn is_task_not_found(output: &CommandOutput) -> bool {
    !output.success && output.code.map(|code| code as u32) == Some(TASK_NOT_FOUND_HRESULT)
}

fn command_failed(action: &str, output: &CommandOutput) -> String {
    if output.success {
        return format!("{action} failed");
    }
    let detail = output_detail(output);
    if detail.is_empty() {
        format!("{action} failed (non-zero exit)")
    } else {
        format!("{action} failed{detail}")
    }
}

fn valid_sid(value: &str) -> bool {
    let mut parts = value.split('-');
    if parts.next() != Some("S") || parts.next() != Some("1") {
        return false;
    }
    let mut count = 0;
    for part in parts {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        count += 1;
    }
    count > 0
}

pub(crate) fn parse_sid_csv(text: &str) -> Result<String, String> {
    text.split(',')
        .map(|value| value.trim().trim_matches('"'))
        .find(|value| valid_sid(value))
        .map(str::to_string)
        .ok_or_else(|| "current user SID was not returned".to_string())
}

pub fn current_user_sid() -> Result<String, String> {
    let output = run_command(
        "whoami",
        &["/user".into(), "/fo".into(), "csv".into(), "/nh".into()],
    )?;
    if !output.success {
        return Err(command_failed("whoami", &output));
    }
    parse_sid_csv(&String::from_utf8_lossy(&output.stdout))
}

pub fn task_identity(sid: impl Into<String>) -> TaskIdentity {
    TaskIdentity { sid: sid.into() }
}

pub fn task_xml(identity: &TaskIdentity, checker_path: &Path) -> String {
    let sid = xml_escape(&identity.sid);
    let command = xml_escape(&checker_path.to_string_lossy());
    let working = xml_escape(
        checker_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_string_lossy()
            .as_ref(),
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Author>{sid}</Author></RegistrationInfo>
  <Triggers><LogonTrigger><Repetition><Interval>PT6H</Interval><StopAtDurationEnd>false</StopAtDurationEnd></Repetition><UserId>{sid}</UserId><Delay>PT30M</Delay></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Enabled>true</Enabled><Hidden>true</Hidden><WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT2M</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author"><Exec><Command>{command}</Command><WorkingDirectory>{working}</WorkingDirectory></Exec></Actions>
</Task>"#
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn register_or_update(checker_path: &Path) -> Result<TaskIdentity, String> {
    if !checker_path.is_absolute() {
        return Err("checker path must be absolute".into());
    }
    let identity = task_identity(current_user_sid()?);
    let dir = std::env::temp_dir().join(format!("nospacekey-task-{}.xml", std::process::id()));
    write_utf16(&dir, &task_xml(&identity, checker_path)).map_err(|error| error.to_string())?;
    let result = run_command(
        "schtasks.exe",
        &[
            "/Create".into(),
            "/TN".into(),
            identity.name(),
            "/XML".into(),
            dir.to_string_lossy().into_owned(),
            "/F".into(),
        ],
    );
    let _ = std::fs::remove_file(&dir);
    result.and_then(|output| {
        if !output.success {
            return Err(command_failed("Task Scheduler registration", &output));
        }
        Ok(identity)
    })
}

fn write_utf16(path: &Path, text: &str) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(2 + text.len() * 2);
    bytes.extend_from_slice(&[0xff, 0xfe]);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

pub fn run_now(identity: &TaskIdentity) -> Result<(), String> {
    let output = run_command(
        "schtasks.exe",
        &["/Run".into(), "/TN".into(), identity.name()],
    )?;
    if output.success {
        Ok(())
    } else {
        Err(command_failed("Task Scheduler run", &output))
    }
}

pub fn delete(identity: &TaskIdentity) -> Result<(), String> {
    delete_with_runner(identity, run_command)
}

/// Delete a task idempotently without depending on localized `/Delete` text.
/// A failed delete is considered already complete only when a follow-up query
/// returns the documented Task Scheduler not-found HRESULT.  Any query error,
/// successful query (task still exists), or other HRESULT preserves the
/// original delete failure.
fn delete_with_runner<F>(identity: &TaskIdentity, mut run: F) -> Result<(), String>
where
    F: FnMut(&str, &[String]) -> Result<CommandOutput, String>,
{
    let delete_output = run(
        "schtasks.exe",
        &["/Delete".into(), "/TN".into(), identity.name(), "/F".into()],
    )?;
    if delete_output.success {
        return Ok(());
    }
    let delete_error = command_failed("Task Scheduler delete", &delete_output);
    let query = run(
        "schtasks.exe",
        &[
            "/Query".into(),
            "/TN".into(),
            identity.name(),
            "/HResult".into(),
        ],
    );
    if query.as_ref().is_ok_and(is_task_not_found) {
        Ok(())
    } else {
        Err(delete_error)
    }
}

pub fn checker_path_from_config(config_exe: &Path) -> PathBuf {
    config_exe
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("NospacekeyUpdateChecker.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProcess {
        wait: Option<std::io::Result<Option<bool>>>,
        killed: bool,
        reaped: bool,
        output: CommandOutput,
    }

    impl ProcessHandle for FakeProcess {
        fn try_wait(&mut self) -> std::io::Result<Option<bool>> {
            self.wait.take().unwrap_or(Ok(Some(self.output.success)))
        }

        fn kill(&mut self) -> std::io::Result<()> {
            self.killed = true;
            Ok(())
        }

        fn reap(&mut self) -> std::io::Result<CommandOutput> {
            self.reaped = true;
            Ok(self.output.clone())
        }
    }

    fn output(success: bool) -> CommandOutput {
        CommandOutput {
            success,
            code: Some(if success { 0 } else { 1 }),
            stdout: b"stdout".to_vec(),
            stderr: b"stderr".to_vec(),
        }
    }

    fn output_with_code(success: bool, code: Option<i32>) -> CommandOutput {
        CommandOutput {
            code,
            ..output(success)
        }
    }

    #[test]
    fn delete_treats_task_not_found_hresult_as_idempotent_success() {
        let identity = task_identity("S-1-5-21-1001");
        let mut calls = 0;
        let result = delete_with_runner(&identity, |_program, args| {
            calls += 1;
            assert_eq!(args[2], identity.name());
            if calls == 1 {
                Ok(output_with_code(false, Some(1)))
            } else {
                assert_eq!(args[0], "/Query");
                assert_eq!(args[3], "/HResult");
                Ok(output_with_code(false, Some(TASK_NOT_FOUND_HRESULT as i32)))
            }
        });
        assert_eq!(result, Ok(()));
        assert_eq!(calls, 2);
    }

    #[test]
    fn delete_preserves_original_error_when_query_finds_task_or_fails() {
        let identity = task_identity("S-1-5-21-1001");
        for query in [
            Ok(output(true)),
            Err("query timed out".to_string()),
            Ok(output_with_code(false, Some(0x8004_130f_u32 as i32))),
        ] {
            let mut calls = 0;
            let result = delete_with_runner(&identity, |_program, _args| {
                calls += 1;
                if calls == 1 {
                    Ok(output_with_code(false, Some(1)))
                } else {
                    query.clone()
                }
            });
            assert!(result
                .expect_err("non-absent query must preserve delete failure")
                .contains("Task Scheduler delete"));
            assert_eq!(calls, 2);
        }
    }

    #[test]
    fn delete_does_not_query_when_delete_runner_fails() {
        let identity = task_identity("S-1-5-21-1001");
        let mut calls = 0;
        let result = delete_with_runner(&identity, |_program, _args| {
            calls += 1;
            Err("delete timed out".to_string())
        });
        assert_eq!(result, Err("delete timed out".to_string()));
        assert_eq!(calls, 1);
    }

    #[test]
    fn runner_reaps_immediate_process_and_captures_output() {
        let mut process = FakeProcess {
            wait: Some(Ok(Some(true))),
            killed: false,
            reaped: false,
            output: output(true),
        };
        let result =
            poll_process_with(&mut process, Duration::from_secs(3), Instant::now, |_| {}).unwrap();
        assert_eq!(result, output(true));
        assert!(!process.killed);
        assert!(process.reaped);
    }

    #[test]
    fn runner_kills_and_reaps_after_timeout() {
        let start = Instant::now();
        let mut process = FakeProcess {
            wait: Some(Ok(None)),
            killed: false,
            reaped: false,
            output: output(false),
        };
        let mut calls = 0;
        let result = poll_process_with(
            &mut process,
            Duration::from_secs(3),
            || {
                calls += 1;
                if calls == 1 {
                    start
                } else {
                    start + Duration::from_secs(4)
                }
            },
            |_| {},
        )
        .unwrap_err();
        assert!(result.contains("timed out"));
        assert!(result.contains("stderr"));
        assert!(process.killed);
        assert!(process.reaped);
    }

    #[test]
    fn runner_kills_and_reaps_after_try_wait_error() {
        let mut process = FakeProcess {
            wait: Some(Err(std::io::Error::other("poll failed"))),
            killed: false,
            reaped: false,
            output: output(false),
        };
        let result = poll_process_with(&mut process, Duration::from_secs(3), Instant::now, |_| {})
            .unwrap_err();
        assert!(result.contains("status polling failed"));
        assert!(result.contains("poll failed"));
        assert!(process.killed);
        assert!(process.reaped);
    }

    #[test]
    fn sid_parser_accepts_csv_and_rejects_malformed_values() {
        assert_eq!(
            parse_sid_csv(r#""alice, example","S-1-5-21-1-2-3-1001""#).unwrap(),
            "S-1-5-21-1-2-3-1001"
        );
        assert!(parse_sid_csv(r#""alice","not-a-sid""#).is_err());
        assert!(parse_sid_csv(r#""alice","S-1-foo""#).is_err());
    }

    #[test]
    fn task_definition_is_per_user_and_non_elevated() {
        let id = task_identity("S-1-5-21-1-2-3-1001");
        let xml = task_xml(
            &id,
            Path::new(r"C:\Program Files\nospacekey\NospacekeyUpdateChecker.exe"),
        );
        assert_eq!(id.name(), r"\nospacekey\UpdateCheck-S-1-5-21-1-2-3-1001");
        assert!(xml.contains("InteractiveToken"));
        assert!(xml.contains("LeastPrivilege"));
        assert!(xml.contains("PT30M"));
        assert!(xml.contains("PT6H"));
        let repetition = xml.find("<Repetition>").unwrap();
        let user_id = xml.find("<UserId>S-1-5-21-1-2-3-1001</UserId>").unwrap();
        let delay = xml.find("<Delay>").unwrap();
        assert!(repetition < user_id);
        assert!(user_id < delay);
        let trigger_end = xml.find("</LogonTrigger>").unwrap();
        let trigger = &xml[..trigger_end];
        assert!(trigger.contains("<UserId>S-1-5-21-1-2-3-1001</UserId>"));
        assert!(xml.contains("PT2M"));
        assert!(xml.contains("IgnoreNew"));
        assert!(!xml.contains("Password"));
        assert!(xml.contains("DisallowStartIfOnBatteries>false"));
        assert!(xml.contains("WakeToRun>false"));
    }

    #[test]
    fn xml_escapes_paths() {
        let id = task_identity("S-1-5-21-&");
        let xml = task_xml(&id, Path::new(r"C:\a&b\checker.exe"));
        assert!(xml.contains("S-1-5-21-&amp;"));
        assert!(xml.contains("a&amp;b"));
    }
}
