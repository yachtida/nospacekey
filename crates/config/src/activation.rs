//! Config 起動引数と Toast protocol intent の exact parser。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchIntent {
    OpenUpdate,
    StopEngine,
    RepairUpdateTask,
    ParentHwnd(isize),
    None,
}

pub fn parse_argument(value: &str) -> LaunchIntent {
    match value {
        "--open-update" | "nospacekey://update" | "nospacekey://update/" => {
            LaunchIntent::OpenUpdate
        }
        "--stop-engine" => LaunchIntent::StopEngine,
        "--repair-update-task" => LaunchIntent::RepairUpdateTask,
        _ => value
            .parse::<isize>()
            .map(LaunchIntent::ParentHwnd)
            .unwrap_or(LaunchIntent::None),
    }
}

pub fn parse_args<I>(args: I) -> LaunchIntent
where
    I: IntoIterator<Item = String>,
{
    let mut parent = LaunchIntent::None;
    for argument in args {
        match parse_argument(&argument) {
            LaunchIntent::StopEngine => return LaunchIntent::StopEngine,
            LaunchIntent::RepairUpdateTask => return LaunchIntent::RepairUpdateTask,
            LaunchIntent::OpenUpdate => return LaunchIntent::OpenUpdate,
            LaunchIntent::ParentHwnd(hwnd) => parent = LaunchIntent::ParentHwnd(hwnd),
            LaunchIntent::None => {}
        }
    }
    parent
}

#[derive(Default)]
pub struct PendingIntent(pub std::sync::Mutex<bool>);

impl PendingIntent {
    pub fn new(pending: bool) -> Self {
        Self(std::sync::Mutex::new(pending))
    }
    pub fn set(&self) {
        if let Ok(mut pending) = self.0.lock() {
            *pending = true;
        }
    }
    pub fn consume(&self) -> bool {
        self.0
            .lock()
            .map(|mut pending| {
                let result = *pending;
                *pending = false;
                result
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_update_intents_are_accepted() {
        assert_eq!(parse_argument("--open-update"), LaunchIntent::OpenUpdate);
        assert_eq!(
            parse_argument("nospacekey://update"),
            LaunchIntent::OpenUpdate
        );
        assert_eq!(
            parse_argument("nospacekey://update/"),
            LaunchIntent::OpenUpdate
        );
        assert_eq!(
            parse_argument("nospacekey://update?x=1"),
            LaunchIntent::None
        );
        assert_eq!(
            parse_argument("nospacekey://update/extra"),
            LaunchIntent::None
        );
        assert_eq!(
            parse_argument("nospacekey://update#fragment"),
            LaunchIntent::None
        );
        assert_eq!(parse_argument("nospacekey://evil"), LaunchIntent::None);
        assert_eq!(parse_argument("https://example.com"), LaunchIntent::None);
    }

    #[test]
    fn installer_repair_task_intent_is_exact() {
        assert_eq!(
            parse_argument("--repair-update-task"),
            LaunchIntent::RepairUpdateTask
        );
        assert_eq!(parse_argument("--repair-update-task=x"), LaunchIntent::None);
    }

    #[test]
    fn stop_engine_wins_and_numeric_hwnd_is_ignored_beyond_parsing() {
        assert_eq!(
            parse_args(vec!["123".into()]),
            LaunchIntent::ParentHwnd(123)
        );
        assert_eq!(
            parse_args(vec!["123".into(), "--stop-engine".into()]),
            LaunchIntent::StopEngine
        );
        assert_eq!(
            parse_args(vec!["--open-update".into(), "999".into()]),
            LaunchIntent::OpenUpdate
        );
    }

    #[test]
    fn pending_intent_is_replayable_once() {
        let pending = PendingIntent::new(true);
        assert!(pending.consume());
        assert!(!pending.consume());
        pending.set();
        assert!(pending.consume());
    }

    #[test]
    fn protocol_registry_passes_the_original_uri_to_the_exact_parser() {
        let iss = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../installer/nospacekey.iss"),
        )
        .unwrap();
        let command = iss
            .lines()
            .find(|line| line.contains("nospacekey\\shell\\open\\command"))
            .expect("protocol command registration");
        assert!(command.contains("%1"));
        assert!(!command.contains("--open-update"));
    }
}
