//! Original input is available only at complete composer-unit boundaries.
use ipc::clause::ReadingPosition;

#[derive(Clone, Debug, PartialEq, Eq)]
struct InputUnit {
    start: ReadingPosition,
    end: ReadingPosition,
    original: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InputJournal { units: Vec<InputUnit> }

impl InputJournal {
    pub fn clear(&mut self) { self.units.clear(); }

    pub fn replace_single_unit_original(&mut self, end: ReadingPosition, original: char) {
        if let Some(unit) = self.units.iter_mut().find(|unit| unit.end == end && unit.end.0 - unit.start.0 == 1) {
            unit.original = original.to_string();
        }
    }

    pub fn append(&mut self, start: ReadingPosition, end: ReadingPosition, original: &str) {
        if start >= end || original.is_empty() { return; }
        self.retain_prefix(start);
        self.units.push(InputUnit { start, end, original: original.to_owned() });
    }

    /// A boundary through e.g. kyo -> きょ has no original-input substring.
    pub fn original(&self, start: ReadingPosition, end: ReadingPosition) -> Option<String> {
        if start >= end { return None; }
        let mut next = start;
        let mut original = String::new();
        for unit in self.units.iter().skip_while(|unit| unit.end <= start) {
            if unit.start != next || unit.end > end { return None; }
            original.push_str(&unit.original);
            next = unit.end;
            if next == end { return Some(original); }
        }
        None
    }

    pub fn retain_prefix(&mut self, end: ReadingPosition) {
        self.units.truncate(self.units.partition_point(|unit| unit.end <= end));
    }

    pub fn remove_prefix(&mut self, count: ReadingPosition) {
        self.units.retain(|unit| unit.start >= count);
        for unit in &mut self.units {
            unit.start.0 -= count.0;
            unit.end.0 -= count.0;
        }
    }

    /// Before an edit at this position, separate later complete units. A unit
    /// crossing the edit point cannot retain its original-input mapping.
    pub fn detach_suffix(&mut self, start: ReadingPosition) -> Self {
        let index = self.units.partition_point(|unit| unit.start < start);
        let mut suffix = Self { units: self.units.split_off(index) };
        self.retain_prefix(start);
        suffix.remove_prefix(start);
        suffix
    }

    pub fn append_suffix(&mut self, mut suffix: Self, start: ReadingPosition) -> bool {
        if suffix.units.last().is_some_and(|unit| unit.end.0.checked_add(start.0).is_none()) { return false; }
        for unit in &mut suffix.units {
            unit.start.0 += start.0;
            unit.end.0 += start.0;
        }
        self.units.extend(suffix.units);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(value: u32) -> ReadingPosition { ReadingPosition(value) }

    #[test]
    fn completed_units_extract_only_exact_covered_ranges() {
        let mut journal = InputJournal::default();
        journal.append(p(0), p(2), "kyo");
        journal.append(p(2), p(3), "u");
        journal.append(p(3), p(4), "A");
        assert_eq!(journal.original(p(0), p(4)).as_deref(), Some("kyouA"));
        assert_eq!(journal.original(p(2), p(4)).as_deref(), Some("uA"));
        assert!(journal.original(p(0), p(1)).is_none());
        assert!(journal.original(p(1), p(4)).is_none());
        journal.retain_prefix(p(1));
        assert!(journal.original(p(0), p(1)).is_none());
        journal.append(p(1), p(2), "ka");
        assert_eq!(journal.original(p(1), p(2)).as_deref(), Some("ka"));
        assert!(journal.original(p(0), p(2)).is_none());
    }

    #[test]
    fn prefix_commit_preserves_only_whole_retained_units() {
        let mut journal = InputJournal::default();
        journal.append(p(0), p(2), "kyo");
        journal.append(p(2), p(3), "u");
        journal.remove_prefix(p(1));
        assert!(journal.original(p(0), p(2)).is_none());
        assert_eq!(journal.original(p(1), p(2)).as_deref(), Some("u"));
        journal.remove_prefix(p(1));
        assert_eq!(journal.original(p(0), p(1)).as_deref(), Some("u"));
    }
}
