//! Local roman-to-kana composer that owns the TIP's canonical reading.

use unicode_segmentation::UnicodeSegmentation;
use mixed_input::roman::{is_roman_prefix, lookup_roman, pending_step, RomanStep};

/// How a typed character participates in a composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputStyle {
    Kana,
    Direct,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaySegment {
    pub text: String,
    pub style: InputStyle,
}

/// Incrementally composes the reading that local kana conversion would produce.
#[derive(Clone, Default)]
pub struct LocalKanaComposer {
    stable: String,
    stable_segments: Vec<ReplaySegment>,
    // Byte offsets of ASCII literals recoverable on deletion: roman parsing
    // rejections and cursor-frozen unfinished roman. Offsets are relative to
    // `stable`. Explicit Direct keys (push_with_resolver) clear both lists and
    // seal prior literals.
    automatic_literals: Vec<usize>,
    // Same bookkeeping for literals living in `suffix`, relative to its start.
    // set_cursor re-splits move literals between the two regions and remap both.
    suffix_literals: Vec<usize>,
    pending: String,
    /// `pending` と1:1で並ぶ打鍵原文字（CapsLock 大文字など、読み合成用に正規化した
    /// `pending` とは別の実打鍵）。未指定の打鍵は `pending` と同じ文字。unit 化する
    /// とき journal の original になり、CompositionSource の Typed が大小文字を保存する。
    pending_originals: String,
    reading: String,
    journal: crate::input_journal::InputJournal,
    suffix: String,
    suffix_segments: Vec<ReplaySegment>,
}

impl LocalKanaComposer {
    /// Adds one character using the supplied input style.
    pub fn push(&mut self, ch: char, style: InputStyle) {
        self.push_with_original(ch, style, None);
    }

    /// 打鍵原文字を明示する push。`original` は読み合成用の `ch` と違う実打鍵
    /// （CapsLock 大文字のかな経路など）のときだけ指定する。
    pub(crate) fn push_with_original(
        &mut self,
        ch: char,
        style: InputStyle,
        original: Option<char>,
    ) {
        self.push_with_resolver(ch, style, original, lookup_roman);
    }

    fn push_with_resolver(
        &mut self,
        ch: char,
        style: InputStyle,
        original: Option<char>,
        resolve: fn(&str) -> Option<(&'static str, &'static str)>,
    ) {
        let suffix_journal = self.journal.detach_suffix(self.cursor());
        match style {
            InputStyle::Kana => {
                self.pending.push(ch);
                // 原文字不明の pending（reopen 部分削除等で対応を崩した範囲）へ新しい
                // 原文字を継ぎ足すと位置がずれ、後続の unit 化で誤った原文字を消費
                // する。対応が取れているときだけ積む。
                if self.pending_originals.chars().count() == self.pending.chars().count() - 1 {
                    self.pending_originals.push(original.unwrap_or(ch));
                }
                self.advance_pending(resolve);
            }
            InputStyle::Direct => {
                self.automatic_literals.clear();
                self.suffix_literals.clear();
                // pending を Kana のまま stable へ送ると、状態保持エンジンが roman2kana で
                // この文字を後続入力と再結合し、ローカル読みと分岐する。ローカル側はこの境界で
                // 結合を終えているため、Direct リテラルとして凍結して送る。
                let pending = std::mem::take(&mut self.pending);
                let pending_originals = std::mem::take(&mut self.pending_originals);
                let flushed_original = pending_originals_aligned(&pending, &pending_originals)
                    .then_some(pending_originals)
                    .unwrap_or_default();
                self.append_input_unit(&pending, InputStyle::Direct, &flushed_original);
                let original = original.unwrap_or(ch);
                self.append_input_unit(&ch.to_string(), InputStyle::Direct, &original.to_string());
            }
        }
        self.journal.append_suffix(suffix_journal, self.cursor());
        self.refresh_reading();
    }

    /// Removes the most recently typed character, if present.
    pub fn backspace(&mut self) {
        self.delete_before_cursor(false);
    }

    /// P6 reading edits use C2 legal scalar positions. Keep the pinned engine's
    /// legacy grapheme deletion above for its shared differential fixture.
    pub fn backspace_at_cursor(&mut self) {
        self.delete_before_cursor(true);
    }

    fn delete_before_cursor(&mut self, reading_boundary: bool) {
        let suffix_journal = self.journal.detach_suffix(self.cursor());
        if self.pending.pop().is_some() {
            self.pending_originals.pop();
            self.reopen_roman_tail();
            self.journal.append_suffix(suffix_journal, self.cursor());
            self.refresh_reading();
            return;
        }
        let end = self.cursor();
        let retained_len = if reading_boundary {
            let previous = ipc::clause::legal_boundaries(&self.stable).ok()
                .and_then(|positions| positions.into_iter().rev().find(|position| *position < end))
                .unwrap_or(ipc::clause::ReadingPosition(0));
            self.stable.char_indices().nth(previous.0 as usize).map_or(self.stable.len(), |(index, _)| index)
        } else {
            self.stable.graphemes(true).next_back().map_or(0, |cluster| self.stable.len() - cluster.len())
        };
        self.truncate_stable(retained_len);
        self.reopen_roman_tail();
        self.journal.append_suffix(suffix_journal, self.cursor());
        self.refresh_reading();
    }

    /// Clears the current composition.
    pub fn clear(&mut self) {
        self.journal.clear();
        self.suffix.clear();
        self.suffix_segments.clear();
        self.automatic_literals.clear();
        self.suffix_literals.clear();
        self.stable.clear();
        self.stable_segments.clear();
        self.pending.clear();
        self.pending_originals.clear();
        self.reading.clear();
    }

    /// Replaces the input after the engine has partially committed a prefix.
    pub fn reseed_reading(&mut self, reading: &str) {
        self.journal.clear();
        self.suffix.clear();
        self.suffix_segments.clear();
        self.automatic_literals.clear();
        self.suffix_literals.clear();
        self.stable.clear();
        self.stable_segments.clear();
        self.append_stable(reading, InputStyle::Kana);
        self.pending.clear();
        self.pending_originals.clear();
        self.refresh_reading();
    }

    pub fn retain_prefix(&mut self, prefix: &str) -> bool {
        if !self.reading.starts_with(prefix) { return false; }
        if !self.suffix.is_empty() { self.set_cursor(ipc::clause::ReadingPosition(self.reading.chars().count() as u32)); }
        if prefix.len() <= self.stable.len() {
            self.truncate_stable(prefix.len());
            self.pending.clear();
            self.pending_originals.clear();
            self.refresh_reading();
        } else {
            // A converted interval has literal reading; deleted roman suffixes
            // must not reappear or combine with the next composition's input.
            let suffix = prefix[self.stable.len()..].to_owned();
            // 保持する pending 部分の原文字は、対応する文字数だけ pending_originals
            // から切り出す。読みをそのまま original にすると CapsLock 大文字を
            // 小文字へ書き換えてしまう。取り出せない（対応が崩れている）ときは
            // 空文字で unit 登録を除外し、残りも消して次の打鍵へ混入させない。
            let original =
                take_originals_prefix(&mut self.pending_originals, suffix.chars().count())
                    .unwrap_or_default();
            self.pending.clear();
            self.pending_originals.clear();
            self.append_input_unit(&suffix, InputStyle::Direct, &original);
            self.refresh_reading();
        }
        true
    }

    /// Retains a visible suffix after a prefix commit without resolving an unfinished roman tail.
    pub fn retain_suffix(&mut self, suffix: &str) -> bool {
        if !self.suffix.is_empty() {
            if !self.can_retain_suffix(suffix) { return false; }
            self.set_cursor(ipc::clause::ReadingPosition(self.reading.chars().count() as u32));
        }
        let Some((stable_suffix, stable_segments)) = self.retained_stable_suffix(suffix) else {
            return false;
        };
        let removed = self.stable.len() - stable_suffix.len();
        self.journal.remove_prefix(ipc::clause::ReadingPosition(self.stable[..removed].chars().count() as u32));
        self.automatic_literals = self
            .automatic_literals
            .iter()
            .filter_map(|offset| offset.checked_sub(removed))
            .collect();
        self.stable = stable_suffix.to_owned();
        self.stable_segments = stable_segments;
        self.refresh_reading();
        true
    }

    /// Reports whether a suffix can be retained without changing the composition.
    pub fn can_retain_suffix(&self, suffix: &str) -> bool {
        if !self.suffix.is_empty() {
            return !suffix.is_empty() && self.reading.ends_with(suffix)
                && is_grapheme_boundary(&self.reading, self.reading.len() - suffix.len());
        }
        self.retained_stable_suffix(suffix).is_some()
    }

    /// Returns styled segments that replay the current visible reading.
    pub fn replay_segments(&self) -> Vec<ReplaySegment> {
        let mut segments = self.stable_segments.clone();
        // Interior pending input must not combine with the existing suffix on replay.
        append_segment(&mut segments, &self.pending, self.pending_input_style());
        for segment in &self.suffix_segments { append_segment(&mut segments, &segment.text, segment.style); }
        segments
    }

    /// Live pending の由来スタイル。suffix がある（カーソルが途中）ときは
    /// Kana 由来としても再結合を許すと canonical reading が変わるため Direct
    /// として凍結済み扱いにする。replay_segments と CompositionSource 導出の
    /// 共通規則。
    fn pending_input_style(&self) -> InputStyle {
        if self.suffix.is_empty() {
            InputStyle::Kana
        } else {
            InputStyle::Direct
        }
    }

    /// Returns the current canonical reading.
    pub fn reading(&self) -> &str {
        &self.reading
    }

    pub fn cursor(&self) -> ipc::clause::ReadingPosition {
        ipc::clause::ReadingPosition((self.stable.chars().count() + self.pending.chars().count()) as u32)
    }

    pub fn cursor_at_end(&self) -> bool { self.suffix.is_empty() }

    pub fn finalize_pending_n(&mut self) -> bool {
        if self.pending != "n" { return false; }
        let suffix = self.journal.detach_suffix(self.cursor());
        self.pending.clear();
        // 原文字不明の pending（対応が崩れている）なら空文字にして unit 登録を
        // 除外する（ResolvedKana を維持）。
        let original = take_originals_prefix(&mut self.pending_originals, 1).unwrap_or_default();
        self.append_input_unit("ん", InputStyle::Kana, &original);
        self.journal.append_suffix(suffix, self.cursor());
        self.refresh_reading();
        true
    }

    pub fn preserve_last_literal_original(&mut self, original: char) {
        if self.pending.is_empty() { self.journal.replace_single_unit_original(self.cursor(), original); }
    }

    pub fn adopt_conversion_reading(&mut self, reading: &str) -> bool {
        if self.reading == reading { return true; }
        let normalized: String = self.reading.chars().map(|ch| match ch as u32 {
            0x30A1..=0x30F6 => char::from_u32(ch as u32 - 0x60).unwrap(), _ => ch,
        }).collect();
        let final_n = self.suffix.is_empty() && self.pending == "n";
        let resolved_n = final_n.then(|| format!("{}ん", normalized.strip_suffix('n').unwrap()));
        if normalized != reading && resolved_n.as_deref() != Some(reading) { return false; }
        let end = ipc::clause::ReadingPosition(self.reading.chars().count() as u32);
        self.set_cursor(ipc::clause::ReadingPosition(0));
        self.set_cursor(end);
        self.stable.clear();
        self.stable_segments.clear();
        // 採用した読みで stable を作り直すため、旧 stable のバイト offset 簿記は参照先を
        // 失う。engine 由来のリテラル読みに再結合可能なローマ字は存在しないので破棄する。
        self.automatic_literals.clear();
        self.suffix_literals.clear();
        self.append_stable(reading, InputStyle::Direct);
        self.refresh_reading();
        true
    }

    pub fn move_cursor(&mut self, direction: i32) -> bool {
        let current = self.cursor();
        let Ok(positions) = ipc::clause::legal_boundaries(&self.reading) else { return false; };
        let next = if direction > 0 { positions.into_iter().find(|position| *position > current) }
            else if direction < 0 { positions.into_iter().rev().find(|position| *position < current) }
            else { None };
        next.is_some_and(|position| self.set_cursor(position))
    }

    /// Moving the caret preserves original units. Only a subsequent edit can
    /// invalidate a unit cut by the new caret. Unfinished roman input is frozen
    /// at the old caret so typing elsewhere cannot recombine with it; deletion
    /// back to the frozen tail reopens it into pending (Issue #9) because the
    /// caret is editing at that position again.
    pub fn set_cursor(&mut self, requested: ipc::clause::ReadingPosition) -> bool {
        let Ok(positions) = ipc::clause::legal_boundaries(&self.reading) else { return false; };
        let next = positions.into_iter().rev().find(|position| *position <= requested)
            .unwrap_or(ipc::clause::ReadingPosition(0));
        if next == self.cursor() { return false; }
        let mut frozen: Option<(usize, String)> = None;
        if !self.pending.is_empty() {
            let current = self.cursor();
            let suffix = self.journal.detach_suffix(current);
            let pending = std::mem::take(&mut self.pending);
            let pending_originals = std::mem::take(&mut self.pending_originals);
            let start = self.stable.len();
            // 原文字不明の pending は空文字で unit 登録を除外する（ResolvedKana 維持）。
            let frozen_original = pending_originals_aligned(&pending, &pending_originals)
                .then_some(pending_originals)
                .unwrap_or_default();
            self.append_input_unit(&pending, InputStyle::Direct, &frozen_original);
            self.journal.append_suffix(suffix, current);
            frozen = Some((start, pending));
        }
        // 未確定ローマ字の凍結はここでは純 ASCII（is_roman_prefix を満たす間だけ pending に
        // 残る不変条件）。凍結分も再結合可能な offset 簿記に預け、下の再 split で
        // stable⇔suffix 間の移動を含めて一括して再配置する（Issue #9: 削除で caret が
        // 凍結末尾へ戻ったら pending へ戻す）。明示 Direct 打鍵は push_with_resolver 側の
        // clear でこれらも封印する（自動凍結と同一生命周期）。
        if let Some((start, text)) = &frozen {
            for offset in *start..start + text.len() {
                self.automatic_literals.push(offset);
            }
        }
        let split = self.reading.char_indices().nth(next.0 as usize).map_or(self.reading.len(), |(index, _)| index);
        let (before, after) = split_segments(self.replay_segments(), split);
        let stable_len = self.stable.len();
        self.stable = self.reading[..split].to_owned();
        self.suffix = self.reading[split..].to_owned();
        self.stable_segments = before;
        self.suffix_segments = after;
        // 再 split 前の offset（stable 相対・suffix 相対）を新しい領域へ写す。
        // stable 相対の offset はそのまま reading 相対でもある。suffix 相対は
        // stable_len を足すと reading 相対になる。reading 相対が split 未満なら新しい
        // stable、以降なら新しい suffix（開始 split 引き）へ移る。
        let mut stable_offsets: Vec<usize> = Vec::new();
        let mut suffix_offsets: Vec<usize> = Vec::new();
        for reading_offset in self.automatic_literals.iter().copied()
            .chain(self.suffix_literals.iter().map(|o| o + stable_len))
        {
            if reading_offset < split {
                stable_offsets.push(reading_offset);
            } else {
                suffix_offsets.push(reading_offset - split);
            }
        }
        stable_offsets.sort_unstable();
        suffix_offsets.sort_unstable();
        self.automatic_literals = stable_offsets;
        self.suffix_literals = suffix_offsets;
        true
    }

    pub fn delete_forward(&mut self) -> bool {
        let current = self.cursor();
        let Some(next) = ipc::clause::legal_boundaries(&self.reading).ok()
            .and_then(|positions| positions.into_iter().find(|position| *position > current)) else { return false; };
        let count = (next.0 - current.0) as usize;
        let bytes = self.suffix.char_indices().nth(count).map_or(self.suffix.len(), |(index, _)| index);
        let suffix_journal = self.journal.detach_suffix(next);
        self.journal.retain_prefix(current);
        self.journal.append_suffix(suffix_journal, current);
        self.suffix.drain(..bytes);
        self.suffix_literals = self
            .suffix_literals
            .iter()
            .filter_map(|offset| offset.checked_sub(bytes))
            .collect();
        let (_, remaining) = split_segments(std::mem::take(&mut self.suffix_segments), bytes);
        self.suffix_segments = remaining;
        self.refresh_reading();
        true
    }

    pub fn original_input(&self, start: ipc::clause::ReadingPosition, end: ipc::clause::ReadingPosition) -> Option<String> {
        if self.pending.is_empty() { return self.journal.original(start, end); }
        let stable_end = ipc::clause::ReadingPosition(self.stable.chars().count() as u32);
        let mut journal = self.journal.clone();
        let suffix = journal.detach_suffix(self.cursor());
        // 原文字不明の pending は空文字で unit 登録を除外し、復元を None にする。
        let pending_original = self.pending_original_text();
        journal.append(
            stable_end,
            self.cursor(),
            &pending_original,
            self.pending_input_style(),
        );
        journal.append_suffix(suffix, self.cursor());
        journal.original(start, end)
    }

    /// pending の元打鍵文字列。対応が崩れている（原文字不明の）ときは空文字。
    fn pending_original_text(&self) -> String {
        if pending_originals_aligned(&self.pending, &self.pending_originals) {
            self.pending_originals.clone()
        } else {
            String::new()
        }
    }

    /// 元入力由来の実効 unit 全体（pending も original_input と同じ規則で合成）を
    /// reading 座標の昇順で返す。CompositionSource（PR2）の導出元。journal が保持
    /// するのは打鍵時の由来で、adopt_conversion_reading が replay を Direct 化して
    /// も書き変わらない。
    pub(crate) fn effective_input_units(&self) -> Vec<crate::input_journal::InputUnit> {
        let mut journal = self.journal.clone();
        if !self.pending.is_empty() {
            let stable_end = ipc::clause::ReadingPosition(self.stable.chars().count() as u32);
            let cursor = self.cursor();
            let suffix = journal.detach_suffix(cursor);
            let pending_original = self.pending_original_text();
            journal.append(
                stable_end,
                cursor,
                &pending_original,
                self.pending_input_style(),
            );
            journal.append_suffix(suffix, cursor);
        }
        journal.unit_list()
    }

    /// 現在の未確定入力の元入力ソース视图（混在入力 PR2）。journal が完全 unit と
    /// して保持する範囲だけが Typed になり、unit を失った範囲は ResolvedKana。
    /// source_revision は呼出側の composition revision（元入力の世代）。
    pub(crate) fn composition_source(
        &self,
        revision: u64,
    ) -> mixed_input::source::CompositionSource {
        crate::composition_source::build(self, revision)
    }

    /// 混在 Plan 採用後の stable/journal の再構築（PR3）。unit 列の読みを stable へ
    /// 載せ、journal は kind ごとに登録する（Literal=文字ごとの Direct、Kana=Kana、
    /// Direct=Direct、Unknown=登録なし=ResolvedKana 維持）。
    /// 未完・再開可能性は読み文字列の一致で推測せず、採用前の編集状態と unit の
    /// source 座標（採用前ソース上の対応範囲）で引き継ぐ:
    /// - 未完: 採用前 pending の source 範囲 [ps, pe) で採用 unit を stable 側 /
    ///   pending / suffix 側へ三方分割する（pending は読み上で stable と suffix の
    ///   間にあり、末尾とは限らない）。pending 部分の連結読みが roman prefix を
    ///   満たすときだけ未完として成立し、原文字は採用前 pending_originals が整合
    ///   しているときだけ対応位置から切り出し、不明（空）のまま維持する。
    /// - 再開可能（削除で未完へ戻る）: 採用前の automatic_literals（stable 側）と
    ///   suffix_literals（カーソル後方）を **読み座標** に統合し、その範囲を
    ///   被覆する journal unit の source 範囲として採る。採用 unit の source が
    ///   この範囲に含まれるときだけ継承する（同文の別位置を誤って継承しない。
    ///   unit 分割が変わっても包含で追従する）。
    pub fn rebuild_units(&mut self, units: &[mixed_input::projection::AdoptionUnit]) {
        let context = self.adoption_context();
        // 三方分割の詳細は `AdoptionContext::partition` の doc comment を参照。
        let (stable_parts, pending_parts, suffix_parts, pending_ok) =
            context.partition(units);
        let (stable_parts, pending_parts, suffix_parts) = if pending_ok {
            (stable_parts, pending_parts, suffix_parts)
        } else {
            // 未完不成立: 全 unit を位置どおり stable/suffix へ（pending 空）。
            let mut stable = Vec::new();
            let mut suffix = Vec::new();
            for (index, unit) in units.iter().enumerate() {
                // pending を保持しないので cursor は末尾: pending 範囲の後ろにある
                // unit だけが suffix に相当し、残り（境界をまたぐものを含む）は
                // stable へ置く。未完が元々ない（pending_source が空区間）ときは
                // すべて stable。
                if context.pending_nonempty && unit.source.start.get() >= context.pending_source_end
                {
                    suffix.push((index, None));
                } else {
                    stable.push((index, None));
                }
            }
            (stable, Vec::new(), suffix)
        };
        self.journal.clear();
        self.suffix.clear();
        self.suffix_segments.clear();
        self.automatic_literals.clear();
        self.suffix_literals.clear();
        self.stable.clear();
        self.stable_segments.clear();
        self.pending.clear();
        self.pending_originals.clear();
        // ① stable 側を stable/journal へ。
        for (index, clip_to) in &stable_parts {
            self.emit_stable_unit(&units[*index], *clip_to, &context);
        }
        // ② pending 部分。原文字は採用前 pending_originals が整合しているときだけ
        // 対応位置から切り出し、不明（空・不整合）のときは空のままにして捏造しない。
        if let Some(aligned) = context.pending_originals_aligned.clone() {
            for (index, from, to) in &pending_parts {
                let unit = &units[*index];
                let reading = unit_source_reading(unit, *from, *to);
                self.pending.push_str(&reading);
                let offset = from.saturating_sub(context.pending_source_start) as usize;
                let length = (*to - *from) as usize;
                let original: String = aligned.chars().skip(offset).take(length).collect();
                self.pending_originals.push_str(&original);
            }
        } else {
            for (index, from, to) in &pending_parts {
                let unit = &units[*index];
                let reading = unit_source_reading(unit, *from, *to);
                self.pending.push_str(&reading);
            }
        }
        // ③ suffix 側を suffix バッファと journal（reading 座標は stable+pending の
        // 後ろ）へ。再開可能は suffix_literals（suffix 内 byte offset）へ。
        // frozen 判定は clip 後の suffix 部分 [base, unit 末端) で行う（base は
        // clip_from、無ければ unit 先頭）。reading は clip 済みなので文字 index も
        // base 基準で数える。
        let mut reading_at = (self.stable.chars().count() + self.pending.chars().count()) as u32;
        for (index, clip_from) in &suffix_parts {
            let unit = &units[*index];
            let (reading, original) = clip_suffix_parts(unit, *clip_from);
            let style = unit_style(unit);
            append_segment(&mut self.suffix_segments, &reading, style);
            self.suffix.push_str(&reading);
            let suffix_byte_start = self.suffix.len() - reading.len();
            let base = clip_from.unwrap_or(unit.source.start.get());
            let unit_end = unit.source.end.get();
            match unit.kind {
                mixed_input::projection::AdoptionKind::Unknown => {
                    // 元打鍵不明: suffix へは載せるが journal へ登録しない
                    // （composition_source はこの範囲を ResolvedKana にする）。
                    // 再開可能の継承は凍結 source 範囲との交差部分だけ
                    // （stable 側と同じ規則）。
                    reading_at += reading.chars().count() as u32;
                    for (frozen_start, frozen_end) in &context.frozen_source_ranges {
                        let from = base.max(*frozen_start);
                        let to = unit_end.min(*frozen_end);
                        if from >= to {
                            continue;
                        }
                        let char_from = (from - base) as usize;
                        let char_to = (to - base) as usize;
                        let mut offset = suffix_byte_start;
                        for (index2, ch) in reading.chars().enumerate() {
                            if index2 >= char_from && index2 < char_to {
                                self.suffix_literals.push(offset);
                            }
                            offset += ch.len_utf8();
                        }
                    }
                }
                mixed_input::projection::AdoptionKind::Literal => {
                    // journal へは stable の Literal と同じく文字ごとの Direct unit。
                    // 1文字削除（DeleteForward）が unit 全体を落として未編集文字の
                    // 原文字対応まで失わない。採用 Literal は削除で再開しない（封印）。
                    for ch in original.chars() {
                        let text = ch.to_string();
                        self.journal.append(
                            ipc::clause::ReadingPosition(reading_at),
                            ipc::clause::ReadingPosition(reading_at + 1),
                            &text,
                            InputStyle::Direct,
                        );
                        reading_at += 1;
                    }
                }
                _ => {
                    self.journal.append(
                        ipc::clause::ReadingPosition(reading_at),
                        ipc::clause::ReadingPosition(reading_at + reading.chars().count() as u32),
                        &original,
                        style,
                    );
                    reading_at += reading.chars().count() as u32;
                    // 再開可能の継承: suffix 部分 [base, unit_end) が凍結範囲に
                    // 含まれるときだけ（stable 側の frozen_range_of は clip を
                    // 末端解釈するので、ここは suffix 用に先頭 base で判定する）。
                    let contained = context
                        .frozen_source_ranges
                        .iter()
                        .any(|(start, end)| base >= *start && unit_end <= *end);
                    if contained {
                        let mut offset = suffix_byte_start;
                        for ch in reading.chars() {
                            self.suffix_literals.push(offset);
                            offset += ch.len_utf8();
                        }
                    }
                }
            }
        }
        self.refresh_reading();
    }

    /// 採用前の編集状態のうち継承に必要な情報。すべて採用前の読み/source 座標で持つ。
    /// 採用 unit の stable 側部分を stable/journal へ載せる。`clip_to` は pending
    /// 境界をまたぐ unit の先頭側だけ載せるときの source 終端。再開可能の継承は
    /// stable に載せた部分に対してだけ行う。
    fn emit_stable_unit(
        &mut self,
        unit: &mixed_input::projection::AdoptionUnit,
        clip_to: Option<u32>,
        context: &AdoptionContext,
    ) {
        use mixed_input::projection::AdoptionKind;
        match unit.kind {
            AdoptionKind::Literal => {
                // journal へは文字ごとの Direct unit。1文字削除が境界をまたぐ
                // unit 全体を落とさない（残りの原入力対応を保つ）。採用 Literal は
                // 削除で再開しない（封印）。
                let (reading, _) = clip_stable_parts(unit, clip_to);
                for ch in reading.chars() {
                    let text = ch.to_string();
                    self.append_input_unit(&text, InputStyle::Direct, &text);
                }
            }
            AdoptionKind::Kana | AdoptionKind::Direct => {
                let style = if matches!(unit.kind, AdoptionKind::Direct) {
                    InputStyle::Direct
                } else {
                    InputStyle::Kana
                };
                let (reading, original) = clip_stable_parts(unit, clip_to);
                let start = self.stable.len();
                self.append_input_unit(&reading, style, &original);
                let mut inherited = unit.clone();
                inherited.reading = reading;
                inherited.original = original;
                inherited.source = mixed_input::position::SourceRange::new(
                    unit.source.start.get(),
                    clip_to.unwrap_or(unit.source.end.get()),
                );
                self.inherit_reopenable(&inherited, start, context);
            }
            AdoptionKind::Unknown => {
                // 元打鍵不明: stable へは載せるが journal へ登録しない
                // （composition_source はこの範囲を ResolvedKana にする）。
                // 再開可能性は（journal とは独立に）継承できる。登録は文字開始の
                // byte 位置だけにする（多バイト文字の内部を登録しない）。凍結 source
                // 範囲との**交差部分**（1:1 の文字対応）だけを登録する。
                let (reading, _) = clip_stable_parts(unit, clip_to);
                let start = self.stable.len();
                self.append_stable(&reading, InputStyle::Kana);
                let clip_end = clip_to.unwrap_or(unit.source.end.get());
                for (frozen_start, frozen_end) in &context.frozen_source_ranges {
                    let from = unit.source.start.get().max(*frozen_start);
                    let to = clip_end.min(*frozen_end);
                    if from >= to {
                        continue;
                    }
                    let char_from = (from - unit.source.start.get()) as usize;
                    let char_to = (to - unit.source.start.get()) as usize;
                    let mut offset = start;
                    for (index, ch) in reading.chars().enumerate() {
                        if index >= char_from && index < char_to {
                            self.automatic_literals.push(offset);
                        }
                        offset += ch.len_utf8();
                    }
                }
            }
        }
    }

    /// 採用前の編集状態を採る。`rebuild_units` の先頭で（まだ何も消す前に）呼ぶ。
    /// source 座標は composition_source と同じレイアウト歩行（journal unit の
    /// original と、journal のない読み範囲=ResolvedKana の読み、pending の原文字）
    /// から累積する — journal だけで累積すると原文字不明の範囲の分だけ後続が
    /// 前方へずれる。凍結位置は stable 側（automatic_literals）とカーソル後方
    /// （suffix_literals。読み座標は stable+pending の長さを加算）を読み scalar の
    /// 集合へ統合し、1:1 対応の区間（ギャップ、および読み長==原文字長の unit）に
    /// 限って source 範囲へ写す。journal がなくても再開可能性は継承できる
    /// （原文字不明と再開可能は独立した状態）。
    fn adoption_context(&self) -> AdoptionContext {
        let scalar_at_byte = |text: &str, byte: usize| -> u32 {
            text[..byte.min(text.len())].chars().count() as u32
        };
        let stable_scalar_len = self.stable.chars().count() as u32;
        let pending_scalar_len = self.pending.chars().count() as u32;
        let mut frozen_reading: Vec<u32> = Vec::new();
        for offset in &self.automatic_literals {
            frozen_reading.push(scalar_at_byte(&self.stable, *offset));
        }
        for offset in &self.suffix_literals {
            frozen_reading.push(
                stable_scalar_len + pending_scalar_len + scalar_at_byte(&self.suffix, *offset),
            );
        }
        frozen_reading.sort_unstable();
        frozen_reading.dedup();
        let frozen = |scalar: u32| frozen_reading.binary_search(&scalar).is_ok();

        // composition_source と同じ歩行。連続する凍結 run を source 範囲へ写す
        // （1:1 でない unit の内部は写さず run を分断する）。あわせて読み区間 →
        // source 区間の対応を記録し、pending（読み上で stable の後ろ・suffix の前）
        // の source 範囲を求める（末尾とは限らない）。
        let units = self.effective_input_units();
        let reading_len = self.reading.chars().count() as u32;
        let mut frozen_source_ranges = Vec::new();
        let mut segments: Vec<(u32, u32, u32, bool)> = Vec::new(); // (reading_from, reading_to, source_from, one_to_one)
        let mut source_at = 0u32;
        let mut covered_to = 0u32;
        let mut run_source_start: Option<u32> = None;
        let mut walk = |from: u32, to: u32, base_source: u32, one_to_one: bool,
                        run_source_start: &mut Option<u32>| {
            let mut scalar = from;
            while scalar < to {
                if one_to_one && frozen(scalar) {
                    if run_source_start.is_none() {
                        *run_source_start = Some(base_source + (scalar - from));
                    }
                } else if let Some(start) = run_source_start.take() {
                    frozen_source_ranges.push((start, base_source + (scalar - from)));
                }
                scalar += 1;
            }
        };
        for unit in &units {
            let (unit_start, unit_end) = (unit.start.0, unit.end.0);
            if unit_start > covered_to {
                let gap_end = unit_start.min(reading_len);
                segments.push((covered_to, gap_end, source_at, true));
                walk(covered_to, gap_end, source_at, true, &mut run_source_start);
                source_at += gap_end - covered_to;
                covered_to = gap_end;
            }
            let original_len = mixed_input::position::scalar_len(&unit.original);
            let one_to_one = original_len == unit_end.saturating_sub(unit_start);
            segments.push((unit_start, unit_end, source_at, one_to_one));
            walk(unit_start, unit_end, source_at, one_to_one, &mut run_source_start);
            source_at += original_len;
            covered_to = unit_end;
        }
        if covered_to < reading_len {
            segments.push((covered_to, reading_len, source_at, true));
            walk(covered_to, reading_len, source_at, true, &mut run_source_start);
            source_at += reading_len - covered_to;
        }
        if let Some(start) = run_source_start.take() {
            frozen_source_ranges.push((start, source_at));
        }

        // pending の source 範囲。読み上では stable の直後（suffix の前）なので
        // 末尾とは限らない — 歩行で記録した読み→source 対応から求める。
        // 原文字の整合は文字数でだけ判定し、不整合のときは空のまま維持する。
        let pending_reading = (stable_scalar_len, stable_scalar_len + pending_scalar_len);
        let map_source = |reading: u32| -> Option<u32> {
            segments.iter().rev().find_map(|(from, to, source, _)| {
                // 境界値（区間の終端）も対応させる。
                (*from <= reading && reading <= *to).then(|| source + (reading - from))
            })
        };
        let pending_source = if pending_scalar_len == 0 {
            (0, 0)
        } else {
            match (map_source(pending_reading.0), map_source(pending_reading.1)) {
                (Some(from), Some(to)) => (from, to),
                // 対応が取れない（1:1 でない unit が pending 内をまたぐ等）ときは
                // 未完の引き継ぎを諦める（封印。捏造しない）。
                _ => (u32::MAX, u32::MAX),
            }
        };
        let pending_aligned =
            (self.pending_originals.chars().count() as u32) == pending_scalar_len;
        AdoptionContext {
            frozen_source_ranges,
            pending_source_start: pending_source.0,
            pending_source_end: pending_source.1,
            pending_nonempty: pending_scalar_len > 0 && pending_source.0 != u32::MAX,
            pending_originals_aligned: pending_aligned.then(|| self.pending_originals.clone()),
        }
    }

    /// 採用 unit の source 範囲が採用前の再開可能 source 範囲に含まれるとき、
    /// この unit の stable byte offset を automatic_literals へ登録する（削除で
    /// 再開できる）。含まれない（明示 Direct・採用 Literal・別位置の同文）は封印。
    fn inherit_reopenable(
        &mut self,
        unit: &mixed_input::projection::AdoptionUnit,
        stable_start: usize,
        context: &AdoptionContext,
    ) {
        let contained = context.frozen_source_ranges.iter().any(|(start, end)| {
            unit.source.start.get() >= *start && unit.source.end.get() <= *end
        });
        if contained {
            // 登録は文字開始の byte 位置だけ（多バイト文字の内部を登録しない）。
            let mut offset = stable_start;
            for ch in unit.reading.chars() {
                self.automatic_literals.push(offset);
                offset += ch.len_utf8();
            }
        }
    }

    /// Returns the reading split at the unfinished-roman boundary. Direct and
    /// already-frozen ASCII live in `stable`, so only true roman pending is
    /// reported as pending — callers must not guess this from ASCII suffixes.
    pub(crate) fn reading_parts(&self) -> (&str, &str) {
        (&self.stable, &self.pending)
    }

    fn advance_pending(&mut self, resolve: fn(&str) -> Option<(&'static str, &'static str)>) {
        loop {
            if self.pending.is_empty() {
                return;
            }
            match pending_step(&self.pending, resolve) {
                RomanStep::Hold => return,
                RomanStep::Unit {
                    original,
                    kana,
                    drain,
                } => {
                    // journal の original には表のローマ字（original）でなく実打鍵
                    // （pending_originals の同じ文字数）を残す。CapsLock 大文字など
                    // 読み合成文字と原文字が違う打鍵の大小文字を保存するため。
                    // 取り出せない（reopen 部分削除等で対応が崩れている）ときは空
                    // 文字にし、unit を登録しない（CompositionSource は ResolvedKana）。
                    let typed = take_originals_prefix(
                        &mut self.pending_originals,
                        original.chars().count(),
                    )
                    .unwrap_or_default();
                    self.append_input_unit(kana, InputStyle::Kana, &typed);
                    self.pending.drain(..drain);
                }
                RomanStep::Literal { ch, alphabetic } => {
                    // Replaying a rejected ASCII letter as Kana would let a later key
                    // combine with it again, even though this composer has already
                    // frozen it as a literal.
                    let literal_style = if alphabetic {
                        InputStyle::Direct
                    } else {
                        InputStyle::Kana
                    };
                    if literal_style == InputStyle::Direct {
                        self.automatic_literals.push(self.stable.len());
                    }
                    let typed =
                        take_originals_prefix(&mut self.pending_originals, 1).unwrap_or_default();
                    self.append_input_unit(&ch.to_string(), literal_style, &typed);
                    self.pending.drain(..ch.len_utf8());
                }
            }
        }
    }

    fn refresh_reading(&mut self) {
        self.reading.clone_from(&self.stable);
        append_pending(&mut self.reading, &self.pending);
        self.reading.push_str(&self.suffix);
    }

    fn reopen_roman_tail(&mut self) {
        let mut start = self.stable.len();
        let mut reopen = None;
        for &offset in self.automatic_literals.iter().rev() {
            if offset + 1 != start {
                break;
            }
            start = offset;
            let candidate = format!("{}{}", &self.stable[start..], self.pending);
            if is_roman_prefix(&candidate) {
                reopen = Some((start, candidate));
            }
        }
        if let Some((start, pending)) = reopen {
            // reopen する stable 範囲の原文字は、truncate_stable が journal を落とす
            // 前に回収する。読み文字を原文字の代わりに使うと CapsLock 大文字を別の
            // 文字（小文字）として既知の原入力へ登録してしまう。
            // journal.original は開始境界・連続性・終端境界を検証するため、これで
            // 対象範囲が unit 境界から完全に被覆されていることを確認する。境界を
            // またぐ unit の余計な原文字と部分削除の欠落が文字数で相殺されるのを
            // 防ぐ（例: SH と WH を別々に凍結して末尾を削った reopen）。残り pending
            // 側の対応も別に確認し、どちらかでも証明できなければ reopen 全体を
            // 原文字不明として空にする。後続の unit 化は空文字で登録を除外し、
            // ResolvedKana／original() == None を維持する。
            let reopen_reading_at =
                ipc::clause::ReadingPosition(self.stable[..start].chars().count() as u32);
            let stable_end = ipc::clause::ReadingPosition(self.stable.chars().count() as u32);
            let covered = self.journal.original(reopen_reading_at, stable_end);
            let tail_aligned = pending_originals_aligned(&self.pending, &self.pending_originals);
            let mut originals: String = match (covered, tail_aligned) {
                (Some(mut covered), true) => {
                    covered.push_str(&self.pending_originals);
                    covered
                }
                _ => String::new(),
            };
            self.truncate_stable(start);
            self.pending = pending;
            if !originals.is_empty() && originals.chars().count() != self.pending.chars().count() {
                originals.clear();
            }
            self.pending_originals = originals;
        }
    }

    fn append_input_unit(&mut self, text: &str, style: InputStyle, original: &str) {
        let start = self.stable.chars().count() as u32;
        self.append_stable(text, style);
        self.journal.append(
            ipc::clause::ReadingPosition(start),
            ipc::clause::ReadingPosition(start + text.chars().count() as u32),
            original,
            style,
        );
    }

    fn append_stable(&mut self, text: &str, style: InputStyle) {
        self.stable.push_str(text);
        append_segment(&mut self.stable_segments, text, style);
    }

    fn truncate_stable(&mut self, len: usize) {
        self.automatic_literals.retain(|offset| *offset < len);
        self.stable.truncate(len);
        self.journal.retain_prefix(ipc::clause::ReadingPosition(self.stable.chars().count() as u32));
        let mut remaining = len;
        for segment in &mut self.stable_segments {
            if remaining == 0 {
                segment.text.clear();
            } else if segment.text.len() > remaining {
                segment.text.truncate(remaining);
                remaining = 0;
            } else {
                remaining -= segment.text.len();
            }
        }
        self.stable_segments
            .retain(|segment| !segment.text.is_empty());
    }

    fn stable_suffix_segments(&self, suffix_len: usize) -> Option<Vec<ReplaySegment>> {
        if suffix_len == 0 {
            return Some(Vec::new());
        }
        let start = self.stable.len().checked_sub(suffix_len)?;
        if !self.stable.is_char_boundary(start) || !is_grapheme_boundary(&self.stable, start) {
            return None;
        }

        let mut offset = 0;
        let mut retained = Vec::new();
        for segment in &self.stable_segments {
            let end = offset + segment.text.len();
            if end > start {
                let segment_start = start.saturating_sub(offset);
                if !segment.text.is_char_boundary(segment_start) {
                    return None;
                }
                append_segment(&mut retained, &segment.text[segment_start..], segment.style);
            }
            offset = end;
        }
        (offset == self.stable.len()).then_some(retained)
    }

    fn retained_stable_suffix<'a>(&self, suffix: &'a str) -> Option<(&'a str, Vec<ReplaySegment>)> {
        if suffix.is_empty() || !self.reading.ends_with(suffix) {
            return None;
        }
        let stable_suffix = if self.pending.is_empty() {
            suffix
        } else {
            suffix.strip_suffix(&self.pending)?
        };
        if !self.stable.ends_with(stable_suffix) {
            return None;
        }
        let stable_segments = self.stable_suffix_segments(stable_suffix.len())?;
        Some((stable_suffix, stable_segments))
    }
}

fn is_grapheme_boundary(text: &str, index: usize) -> bool {
    index == 0
        || index == text.len()
        || text.grapheme_indices(true).any(|(start, _)| start == index)
}

/// `pending_originals` が `pending` と文字数で 1:1 に並んでいるか。reopen の部分
/// 削除等で原文字を証明できなくなった範囲は `pending_originals` を空にして対応を
/// 崩し、unit 登録の除外（= ResolvedKana）で扱う。
fn pending_originals_aligned(pending: &str, pending_originals: &str) -> bool {
    pending.chars().count() == pending_originals.chars().count()
}

/// `pending_originals` の先頭 `count` 文字を取り出して返す。不足（対応が崩れて
/// いる）のときは None を返し、残りも空にして後続の取り出しに誤った対応を
/// 持ち越さない（`String::drain` はバイト範囲のため、文字数から境界を作る）。
fn take_originals_prefix(originals: &mut String, count: usize) -> Option<String> {
    if originals.chars().count() < count {
        originals.clear();
        return None;
    }
    let end = originals
        .char_indices()
        .nth(count)
        .map_or(originals.len(), |(index, _)| index);
    Some(originals.drain(..end).collect())
}

/// 採用 unit の stable 側部分（分割時は `clip_to` の source 位置まで = 先頭側）。
/// 戻り値は (reading, original)。clip_to が None のときは全体。
fn clip_stable_parts(
    unit: &mixed_input::projection::AdoptionUnit,
    clip_to: Option<u32>,
) -> (String, String) {
    match clip_to {
        None => (unit.reading.clone(), unit.original.clone()),
        Some(to) => {
            let keep = (to - unit.source.start.get()) as usize;
            (
                unit.reading.chars().take(keep).collect(),
                unit.original.chars().take(keep).collect(),
            )
        }
    }
}

/// 採用 unit の suffix 側部分（分割時は `clip_from` の source 位置から = 末尾側）。
fn clip_suffix_parts(
    unit: &mixed_input::projection::AdoptionUnit,
    clip_from: Option<u32>,
) -> (String, String) {
    match clip_from {
        None => (unit.reading.clone(), unit.original.clone()),
        Some(from) => {
            let skip = (from - unit.source.start.get()) as usize;
            (
                unit.reading.chars().skip(skip).collect(),
                unit.original.chars().skip(skip).collect(),
            )
        }
    }
}

/// unit の source 範囲 [from, to) に対応する読み（1:1 対応の前提）。
fn unit_source_reading(
    unit: &mixed_input::projection::AdoptionUnit,
    from: u32,
    to: u32,
) -> String {
    let skip = (from - unit.source.start.get()) as usize;
    let take = (to - from) as usize;
    unit.reading.chars().skip(skip).take(take).collect()
}

/// unit の journal 登録スタイル（Literal も suffix では文字単位分割しない）。
fn unit_style(unit: &mixed_input::projection::AdoptionUnit) -> InputStyle {
    use mixed_input::projection::AdoptionKind;
    match unit.kind {
        AdoptionKind::Kana | AdoptionKind::Unknown => InputStyle::Kana,
        AdoptionKind::Direct | AdoptionKind::Literal => InputStyle::Direct,
    }
}

/// 混在 Plan 採用の継承に必要な、採用前の編集状態。すべて採用前の source/読み座標で持つ。
struct AdoptionContext {
    /// 再開可能（削除で未完へ戻る）とされる採用前の source 範囲列。
    /// automatic_literals（stable 側）と suffix_literals（カーソル後方）を読み座標に
    /// 統合し、composition_source と同じ歩行で source へ写す。原文字不明
    /// （ResolvedKana・原文字空の pending）の範囲も含む（原文字不明と再開可能は独立）。
    frozen_source_ranges: Vec<(u32, u32)>,
    /// 採用前 pending の source 範囲。`pending_nonempty` が false のときは空区間。
    pending_source_start: u32,
    pending_source_end: u32,
    pending_nonempty: bool,
    /// 採用前 pending_originals（pending と文字数が整合しているときだけ Some。
    /// 不明・不整合は None = 空を維持し捏造しない）。
    pending_originals_aligned: Option<String>,
}

impl AdoptionContext {
    /// 採用 unit 列を採用前 pending の source 範囲 [ps, pe) で三方分割する。
    /// 戻り値:
    /// - stable_parts: (unit index, 末尾側 clip 位置)。stable へ載せる部分
    ///   （clip は pending 境界をまたぐ unit の先頭側）。
    /// - pending_parts: (unit index, from, to)。pending へ戻す source 部分。
    /// - suffix_parts: (unit index, 先頭側 clip 位置)。suffix 側へ置く部分。
    /// - pending_ok: pending へ戻す部分があるか。
    ///
    /// 再構築は stable → pending → suffix の順に出力するため、pending へ戻す
    /// body は source 順で **連続する1区間（run）** でなければならない。run より
    /// 前の body は stable、後ろは suffix へ置く（sealed unit を stable へ戻すと
    /// source 順が逆転する）。run は「連続する pendingable body のうち、連結読みが
    /// roman prefix になる最右の区間」（必要なら左側を刈る）を選ぶ。
    ///
    /// 分割は文字数一致（1:1）の unit だけで行う。非1:1 の完成済み unit
    /// （nn→ん 等）は pending に完全に収まるときだけ全体で採り、境界をまたぐ
    /// ときは分割せず stable 側へ置く（元打鍵を欠落させない）。pending に戻せる
    /// のは Kana/Unknown と、pending source 範囲内の Direct。カーソルが途中
    /// （suffix がある）の live pending は composition_source 上で Direct 凍結
    /// 扱いになる（`pending_input_style`）ため、採用 unit も Direct で戻ってくる。
    /// 範囲外の Direct（明示打鍵・凍結済み）と Literal は未完へ戻さない（封印）。
    fn partition(
        &self,
        units: &[mixed_input::projection::AdoptionUnit],
    ) -> (
        Vec<(usize, Option<u32>)>,
        Vec<(usize, u32, u32)>,
        Vec<(usize, Option<u32>)>,
        bool,
    ) {
        use mixed_input::projection::AdoptionKind;
        let (ps, pe) = (self.pending_source_start, self.pending_source_end);
        let mut stable_parts: Vec<(usize, Option<u32>)> = Vec::new();
        let mut pending_parts: Vec<(usize, u32, u32)> = Vec::new();
        let mut suffix_parts: Vec<(usize, Option<u32>)> = Vec::new();
        if !self.pending_nonempty {
            // 未完がない: すべて stable へ（カーソルは末尾）。
            for (index, _unit) in units.iter().enumerate() {
                stable_parts.push((index, None));
            }
            return (stable_parts, pending_parts, suffix_parts, false);
        }

        // ① pending 範囲 [ps, pe) と重なる unit の body（範囲内部分）を列挙する。
        //    head（ps 未満）/ tail（pe 超過）は run 選択の後に clip として確定する。
        struct Body {
            from: u32,
            to: u32,
            pendingable: bool,
        }
        // unit index → body（重ならない unit は None）。
        let mut bodies: Vec<Option<Body>> = units.iter().map(|_| None).collect();
        for (index, unit) in units.iter().enumerate() {
            let (us, ue) = (unit.source.start.get(), unit.source.end.get());
            let from = us.max(ps);
            let to = ue.min(pe);
            if from >= to {
                continue;
            }
            let eligible = matches!(
                unit.kind,
                AdoptionKind::Kana | AdoptionKind::Unknown | AdoptionKind::Direct
            );
            let one_to_one = unit.original.chars().count() == unit.reading.chars().count();
            // 未完へ戻せるのは対応が取れる body だけ（1:1、または unit 全体が
            // 範囲内。非1:1 の完成済み unit は分割すると元打鍵を欠落させる）。
            let pendingable = eligible && (one_to_one || (us >= ps && ue <= pe));
            bodies[index] = Some(Body {
                from,
                to,
                pendingable,
            });
        }

        // ② run 選択: 連続する pendingable body のうち、連結読みが roman prefix に
        //    なる最右の区間（必要なら左側を刈る）。再構築は stable → pending →
        //    suffix の順に出力するため、run より前の body は stable、後ろは suffix
        //    へ置く。sealed unit を stable へ「戻す」と source 順が逆転する
        //    （P1: pending 内 Japanese → Literal の分割）。
        let ordered: Vec<usize> = bodies
            .iter()
            .enumerate()
            .filter_map(|(index, body)| body.as_ref().map(|_| index))
            .collect();
        // unit index → ordered 内の位置（run 帰属の判定用）。
        let mut slot_of: Vec<Option<usize>> = units.iter().map(|_| None).collect();
        for (slot, &index) in ordered.iter().enumerate() {
            slot_of[index] = Some(slot);
        }
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut scan = 0;
        while scan < ordered.len() {
            let pendingable = |slot: usize| {
                bodies[ordered[slot]]
                    .as_ref()
                    .is_some_and(|b| b.pendingable)
            };
            if pendingable(scan) {
                let start = scan;
                while scan < ordered.len() && pendingable(scan) {
                    scan += 1;
                }
                runs.push((start, scan));
            } else {
                scan += 1;
            }
        }
        let run_text = |start: usize, end: usize| -> String {
            ordered[start..end]
                .iter()
                .map(|&index| {
                    let body = bodies[index].as_ref().expect("run 内は body がある");
                    unit_source_reading(&units[index], body.from, body.to)
                })
                .collect()
        };
        let mut chosen: Option<(usize, usize)> = None;
        'choose: for &(start, end) in runs.iter().rev() {
            for drop in 0..(end - start) {
                let run_start = start + drop;
                if mixed_input::roman::is_roman_prefix(&run_text(run_start, end)) {
                    chosen = Some((run_start, end));
                    break 'choose;
                }
            }
        }

        // ③ 仕分け（unit index 順に1パスで。stable/suffix への clip は「unit の
        //    先頭側から / 末尾側まで」を意味する）。
        for (index, unit) in units.iter().enumerate() {
            let Some(slot) = slot_of[index] else {
                // pending 範囲と重ならない: 位置で stable/suffix へ。
                if unit.source.start.get() >= pe {
                    suffix_parts.push((index, None));
                } else {
                    stable_parts.push((index, None));
                }
                continue;
            };
            let body = bodies[index].as_ref().expect("slot_of は body 持ちだけ");
            let (us, ue) = (unit.source.start.get(), unit.source.end.get());
            let in_run = chosen.is_some_and(|(start, end)| slot >= start && slot < end);
            if in_run {
                if us < body.from {
                    stable_parts.push((index, Some(body.from)));
                }
                pending_parts.push((index, body.from, body.to));
                if ue > body.to {
                    suffix_parts.push((index, Some(body.to)));
                }
            } else if chosen.map(|(start, _)| slot < start).unwrap_or(false) {
                // run より前 → stable（unit 末尾まで含む）。
                stable_parts.push((index, (body.to < ue).then_some(body.to)));
            } else if chosen.is_some() {
                // run より後 → suffix（unit 先頭から含む）。
                suffix_parts.push((index, (body.from > us).then_some(body.from)));
            } else {
                // 未完不成立: 重なる unit も位置どおり stable へ（cursor は末尾）。
                stable_parts.push((index, None));
            }
        }
        let pending_ok = !pending_parts.is_empty();
        (stable_parts, pending_parts, suffix_parts, pending_ok)
    }
}

fn append_segment(segments: &mut Vec<ReplaySegment>, text: &str, style: InputStyle) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = segments.last_mut().filter(|last| last.style == style) {
        last.text.push_str(text);
    } else {
        segments.push(ReplaySegment {
            text: text.to_owned(),
            style,
        });
    }
}

fn append_pending(output: &mut String, pending: &str) {
    output.push_str(pending);
}

fn split_segments(segments: Vec<ReplaySegment>, mut bytes: usize) -> (Vec<ReplaySegment>, Vec<ReplaySegment>) {
    let mut before = Vec::new();
    let mut after = Vec::new();
    for segment in segments {
        let split = bytes.min(segment.text.len());
        append_segment(&mut before, &segment.text[..split], segment.style);
        append_segment(&mut after, &segment.text[split..], segment.style);
        bytes -= split;
    }
    (before, after)
}

#[cfg(test)]
pub(crate) fn mismatch_diagnostic(local: &str, azookey: &str) -> Option<String> {
    (local != azookey).then(|| {
        format!(
            "ev=local_kana_mismatch local_utf16={} azookey_utf16={}",
            local.encode_utf16().count(),
            azookey.encode_utf16().count()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{lookup_roman, mismatch_diagnostic, InputStyle, LocalKanaComposer, ReplaySegment};
    use crate::text_service::{
        arm_deferred_work_timer, observe_shadow_compare, ShadowMismatchAggregate,
    };
    use std::cell::RefCell;
    use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

    #[test]
    fn conversion_reading_adoption_preserves_original_n_unit() {
        let mut composer = LocalKanaComposer::default();
        for ch in "nihon".chars() { composer.push(ch, InputStyle::Kana); }
        assert!(composer.adopt_conversion_reading("にほん"));
        assert_eq!(composer.reading(), "にほん");
        assert_eq!(composer.original_input(ipc::clause::ReadingPosition(0), ipc::clause::ReadingPosition(3)).as_deref(), Some("nihon"));
        assert!(!composer.adopt_conversion_reading("日本"));
        assert_eq!(composer.reading(), "にほん");

        // 同じ読みの再採用は早期 return で状態を変えない。直後の打鍵で pending と
        // 結合できることまで固定する。
        let mut pending = LocalKanaComposer::default();
        pending.push('n', InputStyle::Kana);
        assert!(pending.adopt_conversion_reading("n"));
        pending.push('a', InputStyle::Kana);
        assert_eq!(pending.reading(), "な");
    }

    #[test]
    fn pending_n_finalization_keeps_cursor_suffix_and_original_units() {
        use ipc::clause::ReadingPosition as P;
        for interior in [false, true] {
            let mut composer = LocalKanaComposer::default();
            for ch in "ai".chars() { composer.push(ch, InputStyle::Kana); }
            if interior { composer.set_cursor(P(1)); }
            composer.push('n', InputStyle::Kana);
            assert!(composer.finalize_pending_n());
            assert_eq!(composer.reading(), if interior { "あんい" } else { "あいん" });
            assert_eq!(composer.cursor(), P(if interior { 2 } else { 3 }));
            assert_eq!(composer.original_input(P(0), P(3)).as_deref(), Some(if interior { "ani" } else { "ain" }));
            assert!(!composer.finalize_pending_n());
        }
        let mut direct = LocalKanaComposer::default();
        direct.push('n', InputStyle::Direct);
        assert!(!direct.finalize_pending_n());
        assert_eq!(direct.reading(), "n");
    }

    #[test]
    fn partial_commit_at_interior_cursor_retains_suffix_once() {
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "aa".chars() { composer.push(ch, InputStyle::Kana); }
        composer.set_cursor(P(1));
        assert!(composer.retain_suffix("あ"));
        assert_eq!(composer.reading(), "あ");
        assert_eq!(composer.original_input(P(0), P(1)).as_deref(), Some("a"));
        composer.push('i', InputStyle::Kana);
        assert_eq!(composer.reading(), "あい");
    }

    #[test]
    fn cursor_backspace_uses_reading_boundaries_instead_of_display_graphemes() {
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        composer.reseed_reading("e\u{301}ゃ");
        composer.set_cursor(P(2));
        composer.backspace_at_cursor();
        assert_eq!(composer.reading(), "eゃ");
        assert_eq!(composer.cursor(), P(1));
        composer.set_cursor(P(2));
        composer.backspace_at_cursor();
        assert_eq!(composer.reading(), "e");
    }

    #[test]
    fn cursor_insert_preserves_suffix_styles_and_unit_sources() {
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "kyou".chars() { composer.push(ch, InputStyle::Kana); }
        composer.push('A', InputStyle::Direct);
        assert!(composer.set_cursor(P(2)));
        assert_eq!(composer.original_input(P(0), P(4)).as_deref(), Some("kyouA"));
        for ch in "ka".chars() { composer.push(ch, InputStyle::Kana); }
        assert_eq!(composer.reading(), "きょかうA");
        assert_eq!(composer.cursor(), P(3));
        assert_eq!(composer.original_input(P(0), P(5)).as_deref(), Some("kyokauA"));
        assert_eq!(composer.replay_segments().last(), Some(&ReplaySegment { text: "A".into(), style: InputStyle::Direct }));
        composer.backspace();
        assert_eq!(composer.reading(), "きょうA");
        assert_eq!(composer.cursor(), P(2));
        assert!(composer.delete_forward());
        assert_eq!(composer.reading(), "きょA");
        assert_eq!(composer.original_input(P(0), P(3)).as_deref(), Some("kyoA"));
    }

    #[test]
    fn cursor_crossing_unit_does_not_invalidate_until_it_is_edited() {
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "kyou".chars() { composer.push(ch, InputStyle::Kana); }
        composer.set_cursor(P(1));
        assert_eq!(composer.original_input(P(0), P(3)).as_deref(), Some("kyou"));
        composer.set_cursor(P(3));
        assert_eq!(composer.original_input(P(0), P(3)).as_deref(), Some("kyou"));
        composer.set_cursor(P(1));
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "きあょう");
        assert!(composer.original_input(P(0), P(4)).is_none());
        assert_eq!(composer.original_input(P(1), P(2)).as_deref(), Some("a"));
        assert_eq!(composer.original_input(P(3), P(4)).as_deref(), Some("u"));
    }

    #[test]
    fn cursor_freezes_unfinished_input_and_uses_legal_reading_boundaries() {
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "aky".chars() { composer.push(ch, InputStyle::Kana); }
        composer.set_cursor(P(1));
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "ああky");
        composer.set_cursor(P(4));
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "ああkyあ");
        composer.reseed_reading("か\u{3099}ゃ😀う");
        assert!(composer.set_cursor(P(1))); // clamps before the base+dakuten unit
        assert_eq!(composer.cursor(), P(0));
        assert!(composer.move_cursor(1));
        assert_eq!(composer.cursor(), P(2));
        assert!(composer.move_cursor(1));
        assert_eq!(composer.cursor(), P(3));
        assert!(composer.delete_forward());
        assert_eq!(composer.reading(), "か\u{3099}ゃう");
        composer.backspace();
        assert_eq!(composer.reading(), "か\u{3099}う");
        composer.backspace();
        assert_eq!(composer.reading(), "う");
        assert_eq!(composer.cursor(), P(0));
        composer.backspace();
        assert_eq!(composer.reading(), "う");
    }

    #[test]
    fn original_input_tracks_completed_units_through_deletion_and_prefix_commit() {
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "kyounn".chars() { composer.push(ch, InputStyle::Kana); }
        composer.push('A', InputStyle::Direct);
        assert_eq!(composer.reading(), "きょうんA");
        assert_eq!(composer.original_input(P(0), P(5)).as_deref(), Some("kyounnA"));
        assert_eq!(composer.original_input(P(2), P(4)).as_deref(), Some("unn"));
        assert!(composer.original_input(P(1), P(4)).is_none());
        assert!(composer.retain_suffix("ょうんA"));
        assert!(composer.original_input(P(0), P(4)).is_none());
        assert_eq!(composer.original_input(P(1), P(4)).as_deref(), Some("unnA"));
        composer.backspace();
        assert_eq!(composer.original_input(P(1), P(3)).as_deref(), Some("unn"));
        assert!(composer.retain_prefix("ょう"));
        composer.push('k', InputStyle::Kana);
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "ょうか");
        assert_eq!(composer.original_input(P(1), P(3)).as_deref(), Some("uka"));
    }

    #[test]
    fn original_input_reopens_automatic_romaji_but_never_invents_reseeded_keys() {
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "adhy".chars() { composer.push(ch, InputStyle::Kana); }
        composer.backspace();
        composer.backspace();
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "あだ");
        assert_eq!(composer.original_input(P(0), P(2)).as_deref(), Some("ada"));
        composer.reseed_reading("きょう");
        assert!(composer.original_input(P(0), P(3)).is_none());
        for ch in "ky".chars() { composer.push(ch, InputStyle::Kana); }
        assert_eq!(composer.original_input(P(3), P(5)).as_deref(), Some("ky"));
        assert!(composer.original_input(P(3), P(4)).is_none());
        composer.clear();
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.original_input(P(0), P(1)).as_deref(), Some("a"));
    }

    #[test]
    fn retained_prefix_preserves_styles_and_does_not_recombine_removed_suffix() {
        let mut composer = LocalKanaComposer::default();
        for ch in "kya".chars() { composer.push(ch, InputStyle::Kana); }
        composer.push('A', InputStyle::Direct);
        for ch in "ky".chars() { composer.push(ch, InputStyle::Kana); }
        assert!(!composer.retain_prefix("きゅ"));
        assert_eq!(composer.reading(), "きゃAky");
        assert!(composer.retain_prefix("きゃAk"));
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "きゃAkあ");
        assert!(composer.retain_prefix("きゃA"));
        composer.push('i', InputStyle::Kana);
        assert_eq!(composer.reading(), "きゃAい");
        assert!(composer.retain_prefix(""));
        composer.push('u', InputStyle::Kana);
        assert_eq!(composer.reading(), "う");
    }

    #[test]
    fn completed_roman_sequence_has_the_pinned_azookey_reading() {
        let mut composer = LocalKanaComposer::default();
        for ch in "nihongo".chars() {
            composer.push(ch, InputStyle::Kana);
        }

        assert_eq!(composer.reading(), "にほんご");
    }

    #[test]
    fn legal_unfinished_roman_suffixes_remain_visible() {
        let mut consonant = LocalKanaComposer::default();
        consonant.push('k', InputStyle::Kana);
        assert_eq!(consonant.reading(), "k");

        let mut digraph = LocalKanaComposer::default();
        for ch in "ky".chars() {
            digraph.push(ch, InputStyle::Kana);
        }
        assert_eq!(digraph.reading(), "ky");

        let mut nasal = LocalKanaComposer::default();
        nasal.push('n', InputStyle::Kana);
        assert_eq!(nasal.reading(), "n");
    }

    #[test]
    fn differential_corpus_matches_pinned_azookey_readings() {
        // These literals were recorded from the pinned AzooKeyKanaKanjiConverter revision.
        let cases = [
            ("konnichiha", "こんいちは"),
            ("gakkou", "がっこう"),
            ("ryokou", "りょこう"),
            ("xya", "ゃ"),
            ("watashi。", "わたし。"),
            ("caceco", "かせこ"),
            ("qwe", "くぇ"),
            ("sye", "しぇ"),
            ("wyi", "ゐ"),
            ("xn", "ん"),
            ("zl", "→"),
            ("n。", "ん。"),
            ("nn", "ん"),
        ];

        for (input, expected) in cases {
            let mut composer = LocalKanaComposer::default();
            for ch in input.chars() {
                composer.push(ch, InputStyle::Kana);
            }
            assert_eq!(composer.reading(), expected, "input={input:?}");
        }
    }

    #[test]
    fn direct_input_and_backspace_match_pinned_azookey_readings() {
        let mut composer = LocalKanaComposer::default();
        for ch in "kyou".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.push('A', InputStyle::Direct);
        assert_eq!(composer.reading(), "きょうA");

        composer.backspace();
        assert_eq!(composer.reading(), "きょう");

        for ch in "sha".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.backspace();
        assert_eq!(composer.reading(), "きょうし");

        for ch in "kaki".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.backspace();
        assert_eq!(composer.reading(), "きょうしか");
    }

    #[test]
    fn pinned_n_rules_prefer_concrete_mappings() {
        let mut nn = LocalKanaComposer::default();
        for ch in "nn".chars() {
            nn.push(ch, InputStyle::Kana);
        }
        assert_eq!(nn.reading(), "ん");

        let mut ny = LocalKanaComposer::default();
        for ch in "ny".chars() {
            ny.push(ch, InputStyle::Kana);
        }
        assert_eq!(ny.reading(), "ny");
    }

    #[test]
    fn replay_segments_preserve_kana_pending_and_direct_styles() {
        let mut composer = LocalKanaComposer::default();
        for ch in "ka".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.push('A', InputStyle::Direct);
        composer.push('k', InputStyle::Kana);

        assert_eq!(
            composer.replay_segments(),
            vec![
                ReplaySegment {
                    text: "か".into(),
                    style: InputStyle::Kana,
                },
                ReplaySegment {
                    text: "A".into(),
                    style: InputStyle::Direct,
                },
                ReplaySegment {
                    text: "k".into(),
                    style: InputStyle::Kana,
                },
            ]
        );
    }

    #[test]
    fn issue9_deletion_back_to_cursor_frozen_tail_reopens_pending_romaji() {
        // Issue #9: カーソル移動で Direct 凍結された未確定ローマ字（こんn）も、
        // 削除で caret が末尾へ戻った時点で pending へ戻り、後続打鍵と再結合する。
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "konn".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.push('n', InputStyle::Kana);
        assert_eq!(composer.reading(), "こんn");
        assert!(composer.set_cursor(P(0)));
        assert!(composer.set_cursor(P(3)), "caret を末尾へ戻しても凍結は維持");
        composer.push('i', InputStyle::Kana);
        assert_eq!(
            composer.reading(),
            "こんnい",
            "削除せずに打った i は凍結 n と結合しない（凍結の設計どおり）"
        );
        composer.backspace();
        assert_eq!(composer.reading(), "こんn");
        composer.push('i', InputStyle::Kana);
        assert_eq!(
            composer.reading(),
            "こんに",
            "削除で戻った n はローマ字入力として再認識され n+i=ni で再結合する"
        );
    }

    #[test]
    fn issue9_reading_cursor_navigation_variant_reopens_frozen_tail() {
        // InputModule の MoveReading/ReadingEnd と同じ move_cursor/set_cursor 経路で
        // 凍結→復帰した場合も backspace_at_cursor の再 open が働くことを固定する。
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "konnn".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        assert!(composer.move_cursor(-1));
        assert!(composer.set_cursor(P(u32::MAX)));
        assert_eq!(composer.reading(), "こんn");
        composer.push('i', InputStyle::Kana);
        assert_eq!(composer.reading(), "こんnい");
        composer.backspace_at_cursor();
        assert_eq!(composer.reading(), "こんn");
        // reopen した pending "n" は末尾 Kana ランへマージされる（replay_segments の
        // 通常形状）。Direct 凍結のままなら Kana ランに溶け込まない。
        assert_eq!(
            composer.replay_segments(),
            vec![ReplaySegment { text: "こんn".into(), style: InputStyle::Kana }]
        );
        composer.push('i', InputStyle::Kana);
        assert_eq!(composer.reading(), "こんに");
    }

    #[test]
    fn issue9_deletion_reopens_multi_char_frozen_romaji_as_a_roman_prefix() {
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "konnky".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        assert_eq!(composer.reading(), "こんky");
        assert!(composer.set_cursor(P(0)));
        assert!(composer.set_cursor(P(4)), "caret を末尾へ戻しても凍結は維持");
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "こんkyあ");
        composer.backspace();
        assert_eq!(composer.reading(), "こんky");
        composer.push('a', InputStyle::Kana);
        assert_eq!(
            composer.reading(),
            "こんきゃ",
            "ky 全体が pending へ戻り kya=きゃ として再結合する"
        );
    }

    #[test]
    fn explicit_direct_key_seals_cursor_frozen_romaji_against_reopening() {
        // push(Direct) は automatic_literals を clear する — カーソル凍結分も
        // 明示 Direct 打鍵以降は封印され、削除で戻っても再結合しない。
        use ipc::clause::ReadingPosition as P;
        let mut composer = LocalKanaComposer::default();
        for ch in "konn".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.push('n', InputStyle::Kana);
        assert!(composer.set_cursor(P(0)));
        assert!(composer.set_cursor(P(3)));
        composer.push('A', InputStyle::Direct);
        assert_eq!(composer.reading(), "こんnA");
        composer.backspace();
        composer.backspace();
        assert_eq!(composer.reading(), "こん");
        composer.push('i', InputStyle::Kana);
        assert_eq!(composer.reading(), "こんい", "封印された n は再結合しない");
    }

    #[test]
    fn backspace_keeps_nasal_pending_for_replay_and_recombination() {
        let mut composer = LocalKanaComposer::default();
        for ch in "ny".chars() {
            composer.push(ch, InputStyle::Kana);
        }

        composer.backspace();
        assert_eq!(composer.reading(), "n");
        assert_eq!(
            composer.replay_segments(),
            vec![ReplaySegment {
                text: "n".into(),
                style: InputStyle::Kana,
            }]
        );

        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "な");
        assert_eq!(
            composer.replay_segments(),
            vec![ReplaySegment {
                text: "な".into(),
                style: InputStyle::Kana,
            }]
        );
    }

    #[test]
    fn backspaced_n_recombines_with_y_and_resolves_before_a_consonant() {
        let mut recombiner = LocalKanaComposer::default();
        for ch in "ny".chars() {
            recombiner.push(ch, InputStyle::Kana);
        }
        recombiner.backspace();
        for ch in "yu".chars() {
            recombiner.push(ch, InputStyle::Kana);
        }
        assert_eq!(recombiner.reading(), "にゅ");

        let mut resolver = LocalKanaComposer::default();
        for ch in "ny".chars() {
            resolver.push(ch, InputStyle::Kana);
        }
        resolver.backspace();
        resolver.push('k', InputStyle::Kana);
        assert_eq!(resolver.reading(), "んk");
    }

    #[test]
    fn replay_reopens_an_automatically_frozen_ascii_literal_after_deletion() {
        let mut composer = LocalKanaComposer::default();
        for ch in "kq".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.backspace();

        let mut replayed = LocalKanaComposer::default();
        for segment in composer.replay_segments() {
            for ch in segment.text.chars() {
                replayed.push(ch, segment.style);
            }
        }
        composer.push('a', InputStyle::Kana);
        replayed.push('a', InputStyle::Kana);

        assert_eq!(composer.reading(), "か");
        assert_eq!(replayed.reading(), composer.reading());
    }

    #[test]
    fn automatic_reopening_respects_explicit_direct_boundaries() {
        let mut composer = LocalKanaComposer::default();
        composer.push('A', InputStyle::Direct);
        for ch in "dhy".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.backspace();
        composer.backspace();
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "Aだ");

        composer.clear();
        for ch in "dq".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.push('A', InputStyle::Direct);
        composer.backspace();
        composer.backspace();
        composer.push('a', InputStyle::Kana);
        assert_eq!(
            composer.reading(),
            "dあ",
            "an explicit Direct key seals prior literals"
        );
    }

    #[test]
    fn direct_combining_grapheme_backspace_removes_the_whole_grapheme_from_replay() {
        let mut composer = LocalKanaComposer::default();
        for ch in ['x', 'e', '\u{301}'] {
            composer.push(ch, InputStyle::Direct);
        }

        composer.backspace();
        assert_eq!(composer.reading(), "x");
        assert_eq!(
            composer.replay_segments(),
            vec![ReplaySegment {
                text: "x".into(),
                style: InputStyle::Direct,
            }]
        );
    }

    #[test]
    fn retaining_suffix_does_not_split_a_direct_grapheme() {
        let mut composer = LocalKanaComposer::default();
        for ch in ['x', 'e', '\u{301}'] {
            composer.push(ch, InputStyle::Direct);
        }
        let before_segments = composer.replay_segments();

        assert!(!composer.can_retain_suffix("\u{301}"));
        assert!(!composer.retain_suffix("\u{301}"));
        assert_eq!(composer.replay_segments(), before_segments);
        assert_eq!(composer.reading(), "xe\u{301}");
    }

    #[test]
    fn retaining_a_direct_suffix_keeps_its_style() {
        let mut composer = LocalKanaComposer::default();
        composer.push('a', InputStyle::Kana);
        for ch in ['x', 'e', '\u{301}'] {
            composer.push(ch, InputStyle::Direct);
        }

        assert!(composer.retain_suffix("xe\u{301}"));
        assert_eq!(
            composer.replay_segments(),
            vec![ReplaySegment {
                text: "xe\u{301}".into(),
                style: InputStyle::Direct,
            }]
        );
    }

    #[test]
    fn retaining_a_suffix_that_cuts_pending_leaves_replay_state_unchanged() {
        let mut composer = LocalKanaComposer::default();
        for ch in "ky".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        let before_reading = composer.reading().to_owned();
        let before_segments = composer.replay_segments();

        assert!(!composer.can_retain_suffix("y"));
        assert!(!composer.retain_suffix("y"));
        assert_eq!(composer.reading(), before_reading);
        assert_eq!(composer.replay_segments(), before_segments);
    }

    #[test]
    fn retaining_pending_suffix_keeps_pending_for_the_next_roman_key() {
        let mut composer = LocalKanaComposer::default();
        for ch in "an".chars() {
            composer.push(ch, InputStyle::Kana);
        }

        assert!(composer.retain_suffix("n"));
        assert_eq!(composer.reading(), "n");
        composer.push('y', InputStyle::Kana);
        composer.push('u', InputStyle::Kana);
        assert_eq!(composer.reading(), "にゅ");
    }

    #[derive(Debug)]
    struct FixtureCase {
        name: String,
        operations: Vec<FixtureOperation>,
        trajectory: Vec<String>,
    }

    #[derive(Debug)]
    enum FixtureOperation {
        Kana(String),
        Direct(String),
        Backspace,
    }

    fn parse_fixture(fixture: &str) -> Result<Vec<FixtureCase>, String> {
        let mut cases = Vec::new();
        for (line_number, line) in fixture.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 3 || fields.iter().any(|field| field.is_empty()) {
                return Err(format!(
                    "fixture line {} must have three non-empty columns",
                    line_number + 1
                ));
            }
            let operations = fields[1]
                .split(',')
                .map(|operation| match operation {
                    "B" => Ok(FixtureOperation::Backspace),
                    operation
                        if operation
                            .strip_prefix("K:")
                            .is_some_and(|payload| !payload.is_empty()) =>
                    {
                        Ok(FixtureOperation::Kana(operation[2..].to_owned()))
                    }
                    operation
                        if operation
                            .strip_prefix("D:")
                            .is_some_and(|payload| !payload.is_empty()) =>
                    {
                        Ok(FixtureOperation::Direct(operation[2..].to_owned()))
                    }
                    _ => Err(format!(
                        "fixture line {} has unknown operation {operation:?}",
                        line_number + 1
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let trajectory: Vec<_> = fields[2].split('|').map(str::to_owned).collect();
            if trajectory.iter().any(String::is_empty) {
                return Err(format!(
                    "fixture line {} has an empty trajectory reading",
                    line_number + 1
                ));
            }
            let event_count: usize = operations
                .iter()
                .map(|operation| match operation {
                    FixtureOperation::Kana(payload) | FixtureOperation::Direct(payload) => {
                        payload.chars().count()
                    }
                    FixtureOperation::Backspace => 1,
                })
                .sum();
            if trajectory.len() != event_count {
                return Err(format!(
                    "fixture line {} has {} events but {} readings",
                    line_number + 1,
                    event_count,
                    trajectory.len()
                ));
            }
            cases.push(FixtureCase {
                name: fields[0].to_owned(),
                operations,
                trajectory,
            });
        }
        if cases.is_empty() {
            return Err("fixture has no cases".to_owned());
        }
        Ok(cases)
    }

    fn run_fixture(
        fixture: &str,
        test_resolver: Option<fn(&str) -> Option<(&'static str, &'static str)>>,
    ) -> Result<(), String> {
        for case in parse_fixture(fixture)? {
            let mut composer = LocalKanaComposer::default();
            let mut event = 0;
            for operation in &case.operations {
                match operation {
                    FixtureOperation::Kana(payload) => {
                        for ch in payload.chars() {
                            if let Some(resolve) = test_resolver {
                                composer.push_with_resolver(ch, InputStyle::Kana, None, resolve);
                            } else {
                                composer.push(ch, InputStyle::Kana);
                            }
                            assert_trajectory_reading(&case, event, &composer)?;
                            event += 1;
                        }
                    }
                    FixtureOperation::Direct(payload) => {
                        for ch in payload.chars() {
                            if let Some(resolve) = test_resolver {
                                composer.push_with_resolver(ch, InputStyle::Direct, None, resolve);
                            } else {
                                composer.push(ch, InputStyle::Direct);
                            }
                            assert_trajectory_reading(&case, event, &composer)?;
                            event += 1;
                        }
                    }
                    FixtureOperation::Backspace => {
                        composer.backspace();
                        assert_trajectory_reading(&case, event, &composer)?;
                        event += 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn assert_trajectory_reading(
        case: &FixtureCase,
        event: usize,
        composer: &LocalKanaComposer,
    ) -> Result<(), String> {
        let actual = composer.reading();
        let expected = &case.trajectory[event];
        if actual != expected {
            return Err(format!(
                "fixture={} event={} actual={actual:?} expected={expected:?}",
                case.name,
                event + 1
            ));
        }
        let replayed = replay_segments_reading(composer);
        (replayed == actual).then_some(()).ok_or_else(|| {
            format!(
                "fixture={} event={} replayed={replayed:?} actual={actual:?}",
                case.name,
                event + 1
            )
        })
    }

    fn replay_segments_reading(composer: &LocalKanaComposer) -> String {
        let mut replayed = LocalKanaComposer::default();
        for segment in composer.replay_segments() {
            for ch in segment.text.chars() {
                replayed.push(ch, segment.style);
            }
        }
        replayed.reading().to_owned()
    }

    #[test]
    fn shared_pinned_azookey_fixture_matches_local_composer_trajectory() {
        run_fixture(
            include_str!("../../../fixtures/local-kana-parity.tsv"),
            None,
        )
        .unwrap();
    }

    #[test]
    fn fixture_rejects_malformed_non_comment_rows() {
        for malformed in [
            "missing\tK:a\n",
            "empty\tK:a\t\n",
            "unknown\tQ:a\ta\n",
            "# metadata\n",
        ] {
            assert!(parse_fixture(malformed).is_err(), "fixture={malformed:?}");
        }
    }

    #[test]
    fn shared_fixture_gate_rejects_an_injected_go_to_bo_rule() {
        assert!(run_fixture(
            include_str!("../../../fixtures/local-kana-parity.tsv"),
            Some(|input| {
                lookup_roman(input).map(|(roman, kana)| {
                    if roman == "go" {
                        (roman, "ぼ")
                    } else {
                        (roman, kana)
                    }
                })
            }),
        )
        .is_err());
    }

    #[test]
    #[ignore = "run with: cargo test --release -p nospacekey_tip local_kana_composer::tests::release_ten_thousand_key_performance_gate -- --ignored"]
    fn release_ten_thousand_key_performance_gate() {
        assert!(
            !cfg!(debug_assertions),
            "performance gate requires --release"
        );

        warm_observed_key_path();
        warm_timer_arm_path();
        let samples = run_ten_thousand_observed_keys();
        assert_p99_under_one_millisecond(&samples[..100], "short");
        assert_p99_under_one_millisecond(&samples[4_950..5_050], "medium");
        assert_p99_under_one_millisecond(&samples[9_900..], "long");
        assert_p99_under_one_millisecond(&run_timer_arm_samples(), "timer arm");
    }

    fn warm_observed_key_path() {
        let mut composer = LocalKanaComposer::default();
        let matches = RefCell::new(ShadowMismatchAggregate::default());
        let mismatches = RefCell::new(ShadowMismatchAggregate::default());
        for _ in 0..50 {
            composer.push('k', InputStyle::Kana);
            let _ = observe_shadow_compare(&matches, composer.reading(), composer.reading());
            composer.push('a', InputStyle::Kana);
            let _ = observe_shadow_compare(&mismatches, composer.reading(), "");
        }
    }

    fn warm_timer_arm_path() {
        for _ in 0..10 {
            let aggregate = RefCell::new(ShadowMismatchAggregate::default());
            let timer = std::cell::Cell::new(0);
            assert!(observe_shadow_compare(&aggregate, "local", "azookey"));
            assert!(arm_deferred_work_timer(&timer, true, || unsafe {
                SetTimer(None, 0, 1, None)
            }));
            unsafe { KillTimer(None, timer.get()) }.unwrap();
        }
    }

    fn run_ten_thousand_observed_keys() -> Vec<std::time::Duration> {
        let mut composer = LocalKanaComposer::default();
        let matches = RefCell::new(ShadowMismatchAggregate::default());
        let mismatches = RefCell::new(ShadowMismatchAggregate::default());
        let mut samples = Vec::with_capacity(10_000);
        for event in 0..10_000 {
            let started = std::time::Instant::now();
            composer.push(if event % 2 == 0 { 'k' } else { 'a' }, InputStyle::Kana);
            if event % 2 == 0 {
                assert!(!observe_shadow_compare(
                    &matches,
                    composer.reading(),
                    composer.reading()
                ));
            } else {
                let arm = observe_shadow_compare(&mismatches, composer.reading(), "");
                assert!(arm);
            }
            samples.push(started.elapsed());
        }
        assert_eq!(composer.reading().chars().count(), 5_000);
        assert!(mismatches.borrow().has_pending());
        samples
    }

    fn run_timer_arm_samples() -> Vec<std::time::Duration> {
        let mut samples = Vec::with_capacity(100);
        for _ in 0..100 {
            let aggregate = RefCell::new(ShadowMismatchAggregate::default());
            let timer = std::cell::Cell::new(0);
            let started = std::time::Instant::now();
            let arm = observe_shadow_compare(&aggregate, "local", "azookey");
            assert!(arm);
            assert!(arm_deferred_work_timer(&timer, arm, || unsafe {
                SetTimer(None, 0, 1, None)
            }));
            unsafe { KillTimer(None, timer.get()) }.unwrap();
            samples.push(started.elapsed());
        }
        samples
    }

    fn assert_p99_under_one_millisecond(samples: &[std::time::Duration], bucket: &str) {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let p99 = sorted[98];
        assert!(
            p99 < std::time::Duration::from_millis(1),
            "{bucket} reading key-path p99 {p99:?} exceeded the 1ms Issue #30 gate"
        );
    }

    #[test]
    fn mismatch_diagnostic_excludes_input_bodies() {
        let event =
            mismatch_diagnostic("にほんご", "にぼんご").expect("different readings log once");

        assert_eq!(
            event,
            "ev=local_kana_mismatch local_utf16=4 azookey_utf16=4"
        );
        assert!(!event.contains("にほんご"));
        assert!(!event.contains("にぼんご"));
    }

    #[test]
    fn rebuild_units_keeps_suffix_literals_of_a_straddling_unknown_unit() {
        // pending s（source [0,1)）と再開可能な凍結 suffix k（source [1,2)）が
        // 1 つの Unknown unit [0,2) にまとまって戻るケース。suffix 側は clip 位置
        // （source 1）以降の部分で frozen 判定するので、k の suffix_literals が
        // 失われない（unit 先頭基準の判定だと交差が空になり落ちる）。
        let mut composer = LocalKanaComposer::default();
        for ch in "ky".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(0)));
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(2)));
        composer.backspace(); // y を削る（unit 破壊 → 原文字不明の k が残る）
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(0)));
        composer.push('s', InputStyle::Kana);
        assert_eq!(composer.reading(), "sk");
        assert!(
            !composer.suffix_literals.is_empty(),
            "前提: 凍結 suffix k は再開可能"
        );
        let units = vec![mixed_input::projection::AdoptionUnit {
            reading: "sk".to_string(),
            original: "sk".to_string(),
            kind: mixed_input::projection::AdoptionKind::Unknown,
            source: mixed_input::position::SourceRange::new(0, 2),
        }];
        composer.rebuild_units(&units);
        assert_eq!(composer.reading(), "sk");
        assert_eq!(composer.pending, "s");
        assert_eq!(composer.suffix, "k");
        assert!(
            !composer.suffix_literals.is_empty(),
            "straddling Unknown の suffix 側も再開可能を継承する"
        );
    }

    #[test]
    fn rebuild_units_registers_suffix_literals_per_character() {
        // pending s の後ろに Literal("ky") を suffix として採用する。suffix の
        // Literal は stable と同じく文字ごとの Direct unit で journal 登録する
        // （1文字の前方削除で未編集の y まで原文字対応を落とさない）。
        let mut composer = LocalKanaComposer::default();
        for ch in "ky".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(0)));
        composer.push('s', InputStyle::Kana);
        assert_eq!(composer.reading(), "sky");
        let units = vec![
            mixed_input::projection::AdoptionUnit {
                reading: "s".to_string(),
                original: "s".to_string(),
                kind: mixed_input::projection::AdoptionKind::Kana,
                source: mixed_input::position::SourceRange::new(0, 1),
            },
            mixed_input::projection::AdoptionUnit {
                reading: "ky".to_string(),
                original: "ky".to_string(),
                kind: mixed_input::projection::AdoptionKind::Literal,
                source: mixed_input::position::SourceRange::new(1, 3),
            },
        ];
        composer.rebuild_units(&units);
        assert_eq!(composer.reading(), "sky");
        assert!(composer.delete_forward(), "カーソル直後の k を前方削除");
        assert_eq!(composer.reading(), "sy");
        let source = composer.composition_source(0);
        let y_element = source
            .elements()
            .iter()
            .rev()
            .find(|element| element.source_text.contains('y'))
            .expect("y の要素がある");
        assert!(
            matches!(
                y_element.provenance,
                mixed_input::source::Provenance::Typed {
                    style: mixed_input::source::SourceStyle::Direct
                }
            ),
            "未編集の y は原文字対応を保つ: {:?}",
            y_element
        );
    }
}
