//! 保存品詞と外部IMEの対応品詞を分離するTSV境界。

use super::{canonical_pos, normalize_key, UserDictEntry};

// Mozcの品詞表には絵文字がない。対応品詞とコメントの組で自アプリへの往復も保つ。
const EMOJI_COMMENT: &str = "nospacekey:pos=絵文字";

pub struct ExportOutput {
    pub tsv: String,
    pub written: usize,
    pub skipped_control: usize,
}

enum Format {
    Native,
    Google,
}

/// Google/Mozc対応TSVを返す。絵文字は顔文字として出力し、専用コメントで分類を退避する。
/// 未知品詞は従来どおり名詞へ寄せる。制御文字を含む読み・単語は出力しない。
pub fn to_google_tsv(entries: &[UserDictEntry]) -> ExportOutput {
    export(entries, Format::Google)
}

/// 復旧用TSVを返す。未知・空・未指定の品詞も保存時のまま残し、分類の欠落を防ぐ。
/// 読み・単語・保存品詞に制御文字がある行は出力せず、呼出側へ件数を返す。
pub fn to_native_tsv(entries: &[UserDictEntry]) -> ExportOutput {
    export(entries, Format::Native)
}

pub(super) fn imported_pos(pos: Option<&str>, comment: Option<&str>) -> Option<String> {
    match (pos, comment) {
        (Some("顔文字"), Some(EMOJI_COMMENT)) => Some("絵文字".into()),
        _ => pos.map(str::to_owned),
    }
}

fn export(entries: &[UserDictEntry], format: Format) -> ExportOutput {
    let mut sorted: Vec<&UserDictEntry> = entries.iter().collect();
    // 一覧と同じ順序: wordは正規化後ではなく表示値を使う。
    sorted.sort_by_cached_key(|e| (normalize_key(&e.ruby), e.word.clone()));
    let mut out = ExportOutput {
        tsv: String::new(),
        written: 0,
        skipped_control: 0,
    };
    for entry in sorted {
        let (pos, comment) = match format {
            Format::Native => (entry.pos.as_deref(), None),
            Format::Google => match canonical_pos(entry.pos.as_deref()) {
                "絵文字" => (Some("顔文字"), Some(EMOJI_COMMENT)),
                pos => (Some(pos), None),
            },
        };
        if has_control_char(&entry.ruby)
            || has_control_char(&entry.word)
            || pos.is_some_and(has_control_char)
        {
            out.skipped_control += 1;
            continue;
        }
        out.tsv.push_str(&entry.ruby);
        out.tsv.push('\t');
        out.tsv.push_str(&entry.word);
        if let Some(pos) = pos {
            out.tsv.push('\t');
            out.tsv.push_str(pos);
        }
        if let Some(comment) = comment {
            out.tsv.push('\t');
            out.tsv.push_str(comment);
        }
        out.tsv.push_str("\r\n");
        out.written += 1;
    }
    out
}

fn has_control_char(s: &str) -> bool {
    s.chars().any(|c| ('\u{0000}'..='\u{001F}').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_dictionary::parse_tsv;

    #[test]
    fn google_export_uses_mozc_supported_pos_and_restores_emoji() {
        // 正本: google/mozc a069a88d4cb5c011de0f9aebb6c149a1c808d904
        // src/data/rules/third_party_pos_map.def。ConvertEntryは表にない品詞を拒否する。
        let accepted = ["名詞", "人名", "姓", "名", "固有名詞", "組織", "地名", "数", "顔文字"];
        let entries: Vec<_> = accepted.iter().copied().chain(["絵文字", "未知品詞"]).map(|pos| {
            UserDictEntry { ruby: "てすと".into(), word: pos.into(), pos: Some(pos.into()) }
        }).collect();
        let out = to_google_tsv(&entries);
        assert_eq!((out.written, out.skipped_control), (entries.len(), 0));
        for row in out.tsv.lines() {
            assert!(accepted.contains(&row.split('\t').nth(2).unwrap()), "{row}");
        }
        assert!(out.tsv.contains("てすと\t絵文字\t顔文字\tnospacekey:pos=絵文字\r\n"));
        let rows = parse_tsv(out.tsv.as_bytes()).rows;
        assert_eq!(rows.iter().find(|e| e.word == "絵文字").unwrap().pos.as_deref(), Some("絵文字"));
        assert_eq!(rows.iter().find(|e| e.word == "未知品詞").unwrap().pos.as_deref(), Some("名詞"));
    }

    #[test]
    fn ordinary_comments_and_mismatched_marker_do_not_change_pos() {
        let rows = parse_tsv(concat!(
            "てすと\tA\t顔文字\tordinary comment\n",
            "てすと\tB\t名詞\tnospacekey:pos=絵文字\n",
            "てすと\tC\t顔文字\tnospacekey:pos=絵文字 extra\n",
        ).as_bytes()).rows;
        assert_eq!(rows.iter().map(|e| e.pos.as_deref()).collect::<Vec<_>>(),
            vec![Some("顔文字"), Some("名詞"), Some("顔文字")]);
    }

    #[test]
    fn native_roundtrip_preserves_raw_empty_and_missing_pos() {
        let entries: Vec<_> = [None, Some(""), Some("人名(姓)"), Some("絵文字"), Some("未知品詞")]
            .into_iter().enumerate().map(|(i, pos)| UserDictEntry {
                ruby: "てすと".into(), word: i.to_string(), pos: pos.map(str::to_owned),
            }).collect();
        assert_eq!(parse_tsv(to_native_tsv(&entries).tsv.as_bytes()).rows, entries);
    }

    #[test]
    fn native_export_reports_unrepresentable_pos_for_backup() {
        let entries = [UserDictEntry { ruby: "てすと".into(), word: "検証語".into(),
            pos: Some("名詞\tcomment".into()) }];
        let out = to_native_tsv(&entries);
        assert_eq!((out.written, out.skipped_control), (0, 1));
    }
}
