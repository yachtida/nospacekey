//! Reproducible synthetic validation/frozen evaluation and standalone classifier measurements.
use mixed_input::{
    classify::{
        classify,
        edge::Dictionary,
        model::ClassifyModel,
        synth::{self, Split, SynthEpisode},
        tune::source_from_str,
        ScoredPlan,
    },
    live::LiveState,
    plan::{InterpretationPlan, SegmentKind},
    quality::{self, Evidence, Gates},
    selection::mixed_candidate_plans,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

#[derive(Default, Clone, Serialize)]
struct Counts {
    episodes: u64,
    mixed_episodes: u64,
    candidate_hits: u64,
    pure_episodes: u64,
    pure_false_episodes: u64,
    literal_correct: u64,
    literal_proposed: u64,
    literal_expected: u64,
    final_literal_correct: u64,
    reversals: u64,
}
impl Counts {
    fn add(&mut self, b: &Self) {
        self.episodes += b.episodes;
        self.mixed_episodes += b.mixed_episodes;
        self.candidate_hits += b.candidate_hits;
        self.pure_episodes += b.pure_episodes;
        self.pure_false_episodes += b.pure_false_episodes;
        self.literal_correct += b.literal_correct;
        self.literal_proposed += b.literal_proposed;
        self.literal_expected += b.literal_expected;
        self.final_literal_correct += b.final_literal_correct;
        self.reversals += b.reversals;
    }
}
#[derive(Serialize)]
struct Slice {
    category: String,
    counts: Counts,
    candidate_recall_95: Option<[f64; 2]>,
    pure_error_95: Option<[f64; 2]>,
}
#[derive(Serialize)]
struct Calibration {
    adopt_margin: f64,
    change_margin: f64,
    validation: Counts,
}
#[derive(Serialize)]
struct Ablation {
    name: String,
    frozen: Counts,
}
#[derive(Serialize)]
struct Latency {
    scalars: usize,
    samples: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
}
#[derive(Serialize)]
struct Memory {
    before_bytes: u64,
    loaded_bytes: u64,
    classified_bytes: u64,
    delta_bytes: u64,
    load_ms: f64,
}
#[derive(Serialize)]
struct Report {
    schema: u32,
    model: String,
    evaluated_model_checksum: String,
    model_bytes: usize,
    dictionary_bytes: usize,
    seed: u64,
    dataset_scope: String,
    interval_scope: String,
    measurement_scope: String,
    build: String,
    selected_adopt_margin: f64,
    selected_change_margin: f64,
    validation_grid: Vec<Calibration>,
    frozen: Counts,
    slices: Vec<Slice>,
    ablations: Vec<Ablation>,
    latency: Vec<Latency>,
    memory: Memory,
    gates: Gates,
}
struct Cached {
    episode: SynthEpisode,
    prefixes: Vec<Vec<ScoredPlan>>,
}
fn expected(episode: &SynthEpisode) -> InterpretationPlan {
    InterpretationPlan::build(&episode.source, &episode.expected).unwrap()
}
fn labels(plan: &InterpretationPlan) -> Vec<SegmentKind> {
    plan.spans
        .iter()
        .flat_map(|s| std::iter::repeat(s.kind).take(s.range.len() as usize))
        .collect()
}
fn literal_ranges(values: &[SegmentKind]) -> Vec<(usize, usize)> {
    let mut ranges = vec![];
    let mut start = None;
    for (i, kind) in values
        .iter()
        .chain(std::iter::once(&SegmentKind::Japanese))
        .enumerate()
    {
        if *kind == SegmentKind::Literal {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(begin) = start.take() {
            ranges.push((begin, i));
        }
    }
    ranges
}
fn cache(
    episodes: &[SynthEpisode],
    split: Split,
    model: &ClassifyModel,
    dictionary: &Dictionary,
) -> Vec<Cached> {
    episodes
        .iter()
        .filter(|e| e.split == split)
        .map(|episode| {
            let mut source = String::new();
            let prefixes = episode
                .source
                .chars()
                .map(|ch| {
                    source.push(ch);
                    classify(&source_from_str(&source), model, dictionary)
                })
                .collect();
            Cached {
                episode: episode.clone(),
                prefixes,
            }
        })
        .collect()
}
// Count each intended literal once per episode, even when it grows over many prefixes.
// Keep every distinct wrong adopted interval, including mistakes later corrected.
#[derive(Default)]
struct AdoptionAudit {
    correct: BTreeSet<(usize, usize)>,
    wrong: BTreeSet<(usize, usize)>,
}
impl AdoptionAudit {
    fn record(&mut self, proposed: &[SegmentKind], truth: &[(usize, usize)]) {
        for range in literal_ranges(proposed) {
            if let Some(full) = truth.iter().find(|&&(start, end)| {
                start < proposed.len() && range == (start, end.min(proposed.len()))
            }) {
                self.correct.insert(*full);
            } else {
                self.wrong.insert(range);
            }
        }
    }
}
fn evaluate(cached: &Cached, adopt: f64, change: f64) -> Counts {
    let truth = expected(&cached.episode);
    let truth_labels = labels(&truth);
    let expected_literals = literal_ranges(&truth_labels);
    let pure = expected_literals.is_empty();
    let candidate_hit = pure
        || cached
            .prefixes
            .last()
            .is_some_and(|results| mixed_candidate_plans(results).any(|p| *p == truth));
    let mut state = LiveState::default();
    let mut audit = AdoptionAudit::default();
    let mut pure_error = false;
    let mut reversals = 0;
    let mut displayed = vec![];
    let source_chars: Vec<_> = cached.episode.source.chars().collect();
    for (index, results) in cached.prefixes.iter().enumerate() {
        if !source_chars[index].is_ascii_alphabetic() {
            state.lock();
        }
        let mut results = results.clone();
        if let Some(best) = results.first_mut() {
            best.retained = best.margin.is_none_or(|m| m < adopt);
        }
        if let Some(plan) = state.proposal(&results, adopt, change).cloned() {
            if state.accepted.is_some() || plan.spans.iter().any(|s| s.kind == SegmentKind::Literal)
            {
                let next = labels(&plan);
                audit.record(&next, &expected_literals);
                if displayed.iter().zip(&next).any(|(a, b)| a != b) {
                    reversals += 1;
                }
                state.accept(plan);
            }
        }
        displayed = state.accepted.as_ref().map(labels).unwrap_or_default();
        displayed.resize(index + 1, SegmentKind::Japanese);
        pure_error |= pure && displayed.contains(&SegmentKind::Literal);
    }
    let proposed = literal_ranges(&displayed);
    Counts {
        episodes: 1,
        mixed_episodes: u64::from(!pure),
        candidate_hits: u64::from(!pure && candidate_hit),
        pure_episodes: u64::from(pure),
        pure_false_episodes: u64::from(pure_error),
        literal_correct: audit.correct.len() as u64,
        literal_proposed: (audit.correct.len() + audit.wrong.len()) as u64,
        final_literal_correct: proposed
            .iter()
            .filter(|r| expected_literals.contains(r))
            .count() as u64,
        literal_expected: expected_literals.len() as u64,
        reversals,
    }
}
fn totals(cases: &[Cached], adopt: f64, change: f64) -> Counts {
    let mut total = Counts::default();
    for case in cases {
        total.add(&evaluate(case, adopt, change));
    }
    total
}
fn percentile(times: &mut [f64], q: f64) -> f64 {
    times.sort_by(f64::total_cmp);
    times[((times.len() as f64 * q).ceil() as usize).saturating_sub(1)]
}
fn benchmark(model: &ClassifyModel, dictionary: &Dictionary) -> Vec<Latency> {
    [8, 16, 32, 64, 128]
        .into_iter()
        .map(|length| {
            let sources: Vec<_> = [
                "githubnotukaikata",
                "sokomadematte",
                "abcdefghijklmno",
                "aaaaaaaaaaaaaaaa",
                "meetingnoato",
            ]
            .into_iter()
            .map(|s| source_from_str(&s.chars().cycle().take(length).collect::<String>()))
            .collect();
            let mut times = vec![];
            for source in &sources {
                std::hint::black_box(classify(source, model, dictionary));
            }
            for _ in 0..20 {
                for source in &sources {
                    let start = Instant::now();
                    std::hint::black_box(classify(source, model, dictionary));
                    times.push(start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            Latency {
                scalars: length,
                samples: times.len(),
                p50_ms: percentile(&mut times, 0.5),
                p95_ms: percentile(&mut times, 0.95),
                p99_ms: percentile(&mut times, 0.99),
            }
        })
        .collect()
}
#[cfg(windows)]
fn rss() -> u64 {
    #[repr(C)]
    struct Counters {
        cb: u32,
        faults: u32,
        peak: usize,
        working: usize,
        paged_peak: usize,
        paged: usize,
        nonpaged_peak: usize,
        nonpaged: usize,
        pagefile: usize,
        pagefile_peak: usize,
        private: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut Counters,
            size: u32,
        ) -> i32;
    }
    let mut c: Counters = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<Counters>() as u32;
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } == 0 {
        0
    } else {
        c.working as u64
    }
}
#[cfg(not(windows))]
fn rss() -> u64 {
    0
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "mixed-input-quality <model> <report.toml> [--memory-only | --tuned-out model.txt]"
        );
        std::process::exit(2);
    }
    let before = rss();
    let start = Instant::now();
    let text = std::fs::read_to_string(&args[1]).expect("model read");
    let mut model = ClassifyModel::from_artifact(&text).expect("model validation");
    let dictionary = synth::default_dictionary();
    let load_ms = start.elapsed().as_secs_f64() * 1000.;
    let loaded = rss();
    if args.iter().any(|s| s == "--memory-only") {
        std::hint::black_box(classify(
            &source_from_str(&"a".repeat(128)),
            &model,
            &dictionary,
        ));
        let classified = rss();
        let memory = Memory {
            before_bytes: before,
            loaded_bytes: loaded,
            classified_bytes: classified,
            delta_bytes: classified.saturating_sub(before),
            load_ms,
        };
        std::fs::write(&args[2], toml::to_string_pretty(&memory).unwrap()).expect("memory report");
        std::thread::sleep(std::time::Duration::from_secs(3));
        std::hint::black_box(model);
        return;
    }
    let latency = benchmark(&model, &dictionary);
    let classified = rss();
    let memory = Memory {
        before_bytes: before,
        loaded_bytes: loaded,
        classified_bytes: classified,
        delta_bytes: classified.saturating_sub(before),
        load_ms,
    };
    let episodes = synth::generate(&synth::SynthConfig { seed: 42 });
    let validation = cache(&episodes, Split::Validation, &model, &dictionary);
    let mut grid = vec![];
    for adopt in [0., 1., 2., 4., 8., 16., 32., 64.] {
        grid.push(Calibration {
            adopt_margin: adopt,
            change_margin: adopt + 2.,
            validation: totals(&validation, adopt, adopt + 2.),
        });
    }
    // Select on validation only: no pure-Japanese episode error, then maximum exact literal recall,
    // then minimum false spans, then stricter adoption. Frozen is not read until after selection.
    let selected = grid
        .iter()
        .filter(|g| g.validation.pure_false_episodes == 0)
        .max_by_key(|g| {
            (
                g.validation.final_literal_correct,
                std::cmp::Reverse(g.validation.literal_proposed - g.validation.literal_correct),
                g.adopt_margin as u64,
            )
        });
    let adopt = selected.map_or(64., |g| g.adopt_margin);
    let change = adopt + 2.;
    model.thresholds.auto_margin = adopt;
    let evaluated_artifact = model.to_artifact();
    let tuned_path = args.iter().position(|s| s == "--tuned-out").map(|i| {
        args.get(i + 1)
            .expect("--tuned-out requires a path")
            .clone()
    });
    if let Some(path) = &tuned_path {
        std::fs::write(path, &evaluated_artifact).expect("tuned model write");
    }
    let frozen_cache = cache(&episodes, Split::Frozen, &model, &dictionary);
    let frozen = totals(&frozen_cache, adopt, change);
    let mut slices: BTreeMap<String, Counts> = BTreeMap::new();
    for case in &frozen_cache {
        slices
            .entry(case.episode.category.as_str().into())
            .or_default()
            .add(&evaluate(case, adopt, change));
    }
    let slices = slices
        .into_iter()
        .map(|(category, counts)| Slice {
            category,
            candidate_recall_95: quality::wilson(counts.candidate_hits, counts.mixed_episodes),
            pure_error_95: quality::wilson(counts.pure_false_episodes, counts.pure_episodes),
            counts,
        })
        .collect();
    let mut ablations = vec![];
    for name in [
        "dictionary_without_ngram",
        "ngram_without_dictionary",
        "without_transition_penalties",
    ] {
        let mut variant = model.clone();
        match name {
            "dictionary_without_ngram" => variant.weights.ngram = 0.,
            "ngram_without_dictionary" => {
                variant.weights.dict_general = 0.;
                variant.weights.dict_tech = 0.;
            }
            _ => {
                variant.weights.span_cost = 0.;
                variant.weights.switch_cost = 0.;
                variant.weights.ja_to_literal = 0.;
            }
        }
        // Fixed weights/thresholds for ablation: no tuning on frozen.
        ablations.push(Ablation {
            name: name.into(),
            frozen: totals(
                &cache(&episodes, Split::Frozen, &variant, &dictionary),
                adopt,
                change,
            ),
        });
    }
    let evidence = Evidence {
        candidate_hits: frozen.candidate_hits,
        mixed_episodes: frozen.mixed_episodes,
        pure_episodes: frozen.pure_episodes,
        pure_false_episodes: frozen.pure_false_episodes,
        literal_correct: frozen.literal_correct,
        literal_proposed: frozen.literal_proposed,
        literal_expected: frozen.literal_expected,
        final_literal_correct: frozen.final_literal_correct,
        // Human review, actual TSF, STA/multi-app timing and irreversible commit tests are not inferred from offline results.
        ..Default::default()
    };
    let report=Report {schema:2,model:tuned_path.unwrap_or_else(|| args[1].clone()),
        evaluated_model_checksum: evaluated_artifact.lines().find(|line| line.starts_with("checksum=")).unwrap_or("checksum unavailable").to_string(),
        model_bytes:evaluated_artifact.len(),
        dictionary_bytes:include_bytes!("../../data/dictionary/general.txt").len()+include_bytes!("../../data/dictionary/tech.txt").len(),seed:42,
        dataset_scope:"Self-authored synthetic; template-grouped train/validation/frozen; no human natural-input evidence".into(),
        interval_scope:"Wilson 95% descriptive intervals; generated variants are correlated and are not independent natural-input samples".into(),
        measurement_scope:"Standalone classifier process; memory delta includes allocator retention; excludes TSF/IPC/queue/STA and actual multi-app behavior".into(),
        build:if cfg!(debug_assertions) {"debug"} else {"release"}.into(),
        selected_adopt_margin:adopt,selected_change_margin:change,validation_grid:grid,frozen,slices,ablations,latency,memory,gates:quality::decide(&evidence)};
    std::fs::write(&args[2], toml::to_string_pretty(&report).unwrap()).expect("quality report");
    println!(
        "report={} candidates_public={} automatic_public={}",
        args[2], report.gates.candidates_public, report.gates.automatic_public
    );
    if (args.iter().any(|s| s == "--require-candidates") && !report.gates.candidates_public)
        || (args.iter().any(|s| s == "--require-auto") && !report.gates.automatic_public)
    {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn early_wrong_literal_is_counted_after_final_correction_without_prefix_inflation() {
        let ja = SegmentKind::Japanese;
        let lit = SegmentKind::Literal;
        let mut audit = AdoptionAudit::default();
        let truth = [(2, 6)]; // Japanese[ka] + Literal[made]
        audit.record(&[lit, lit], &truth);
        audit.record(&[lit, lit], &truth); // repeated wrong display is one interval
        audit.record(&[ja, ja, lit], &truth);
        audit.record(&[ja, ja, lit, lit], &truth);
        audit.record(&[ja, ja, lit, lit, lit, lit], &truth);
        assert_eq!(audit.correct.len(), 1);
        assert_eq!(audit.wrong.len(), 1);
        assert_eq!(
            audit.correct.len() as f64 / (audit.correct.len() + audit.wrong.len()) as f64,
            0.5
        );
    }
    #[test]
    fn candidate_metric_counts_only_two_mixed_slots_and_keeps_abstentions_in_denominator() {
        let episode = synth::generate(&synth::SynthConfig { seed: 42 })
            .into_iter()
            .find(|e| e.expected.iter().any(|(k, _)| *k == SegmentKind::Literal))
            .unwrap();
        let truth = expected(&episode);
        let text = episode.source.clone();
        let wrong =
            InterpretationPlan::build(&text, &[(SegmentKind::Literal, text.clone())]).unwrap();
        let results = vec![wrong.clone(), wrong, truth]
            .into_iter()
            .map(|plan| ScoredPlan {
                plan,
                score: 1.,
                margin: Some(0.),
                retained: true,
            })
            .collect();
        let c = evaluate(
            &Cached {
                episode,
                prefixes: vec![results],
            },
            2.,
            4.,
        );
        assert_eq!(c.mixed_episodes, 1);
        assert_eq!(c.candidate_hits, 0);
    }
}
