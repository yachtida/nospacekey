//! Unicode 15.1 extended graphemes (UAX #29 revision 43).
//!
//! unicode-segmentation 1.11 supplies the 15.1 data and all rules except GB9c.
//! Suppress its extra boundaries in Indic conjuncts using the fixed InCB data.
use unicode_segmentation::UnicodeSegmentation;

#[path = "grapheme_incb_15_1.rs"]
mod incb;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InCb { None, Consonant, Linker, Extend }

fn property(c: char) -> InCb {
    let scalar = c as u32;
    let index = incb::RANGES.partition_point(|&(_, end, _)| end < scalar);
    incb::RANGES.get(index).filter(|&&(start, _, _)| start <= scalar)
        .map_or(InCb::None, |&(_, _, value)| value)
}

fn indic_conjunct(text: &str, boundary: usize) -> bool {
    if text[boundary..].chars().next().map(property) != Some(InCb::Consonant) {
        return false;
    }
    let mut linker = false;
    for c in text[..boundary].chars().rev() {
        match property(c) {
            InCb::Linker => linker = true,
            InCb::Extend => {}
            InCb::Consonant => return linker,
            InCb::None => return false,
        }
    }
    false
}

pub(crate) fn last_grapheme_start(text: &str) -> Option<usize> {
    text.grapheme_indices(true).rev().map(|(start, _)| start)
        .find(|&start| !indic_conjunct(text, start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incb_table_matches_fixed_unicode_data() {
        let fixture = include_str!("../../../docs/design/clause-navigation-p5/DerivedCoreProperties-15.1.0-InCB.txt");
        let mut expected = Vec::new();
        for line in fixture.lines() {
            let data = line.split('#').next().unwrap().trim();
            if data.is_empty() { continue; }
            let fields: Vec<_> = data.split(';').map(str::trim).collect();
            assert_eq!(fields[1], "InCB");
            let range: Vec<_> = fields[0].split("..").collect();
            let start = u32::from_str_radix(range[0], 16).unwrap();
            let end = u32::from_str_radix(range.last().unwrap(), 16).unwrap();
            let value = match fields[2] {
                "Consonant" => InCb::Consonant, "Linker" => InCb::Linker,
                "Extend" => InCb::Extend, other => panic!("unexpected InCB: {other}"),
            };
            expected.push((start, end, value));
        }
        expected.sort_by_key(|&(start, _, _)| start);
        assert_eq!(incb::RANGES, expected);
        for &(start, end, value) in &expected {
            for scalar in start..=end { assert_eq!(property(char::from_u32(scalar).unwrap()), value); }
        }
    }
}
