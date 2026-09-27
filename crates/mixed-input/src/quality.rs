//! Release evidence is separate from classifier scores. Missing evidence never grants release.
use serde::Serialize;
#[derive(Clone, Debug, Default)]
pub struct Evidence {
    pub candidate_hits: u64,
    pub mixed_episodes: u64,
    pub pure_episodes: u64,
    pub pure_false_episodes: u64,
    pub literal_correct: u64,
    pub literal_proposed: u64,
    pub literal_expected: u64,
    pub final_literal_correct: u64,
    pub human_pure: u64,
    pub human_mixed: u64,
    pub safety_and_tsf_passed: bool,
    pub irreversible_errors: Option<u64>,
    pub performance_passed: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct Gates {
    pub candidates_public: bool,
    pub automatic_public: bool,
    pub missing: Vec<String>,
}
fn ratio(n: u64, d: u64) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64
    }
}
pub fn decide(e: &Evidence) -> Gates {
    let recall = e.mixed_episodes > 0 && ratio(e.candidate_hits, e.mixed_episodes) >= 0.95;
    let safety = e.safety_and_tsf_passed;
    let human = e.human_pure + e.human_mixed >= 1000;
    let candidates_public = safety && human && recall && e.performance_passed;
    let enough_auto = e.human_pure >= 5000 && e.human_mixed >= 1000;
    let automatic_public = candidates_public
        && enough_auto
        && e.pure_episodes > 0
        && ratio(e.pure_false_episodes, e.pure_episodes) <= 0.001
        && e.literal_proposed > 0
        && ratio(e.literal_correct, e.literal_proposed) >= 0.99
        && e.literal_expected > 0
        && ratio(e.final_literal_correct, e.literal_expected) >= 0.8
        && e.irreversible_errors == Some(0);
    let mut missing = vec![];
    if e.pure_episodes == 0 || ratio(e.pure_false_episodes, e.pure_episodes) > 0.001 {
        missing.push(
            "Pure-Japanese episode false literal rate exceeds 0.1% or has no evidence".into(),
        );
    }
    if e.literal_proposed == 0 || ratio(e.literal_correct, e.literal_proposed) < 0.99 {
        missing
            .push("Automatic exact literal precision below 99% or undefined (no adoption)".into());
    }
    if e.literal_expected == 0 || ratio(e.final_literal_correct, e.literal_expected) < 0.8 {
        missing.push("Automatic literal recall below 80% or no evidence".into());
    }
    if !safety {
        missing.push("Current model: safety/TSF acceptance evidence incomplete".into());
    }
    if !human {
        missing.push("Fewer than 1000 human-reviewed episodes; synthetic accuracy is not natural-input quality".into());
    }
    if !recall {
        missing.push(
            "Normal interpretation plus two Mixed interpretations: recall below 95% or no evidence"
                .into(),
        );
    }
    if !e.performance_passed {
        missing
            .push("Classifier/STA/multi-app performance evidence incomplete or over budget".into());
    }
    if !enough_auto {
        missing.push(
            "Automatic release needs 5000 pure and 1000 mixed human-reviewed episodes".into(),
        );
    }
    if e.irreversible_errors != Some(0) {
        missing.push("Irreversible commit regression evidence is missing or failed".into());
    }
    Gates {
        candidates_public,
        automatic_public,
        missing,
    }
}
/// Wilson interval describes this measured sample; generated variants are not independent human episodes.
pub fn wilson(success: u64, total: u64) -> Option<[f64; 2]> {
    if total == 0 || success > total {
        return None;
    }
    let n = total as f64;
    let p = success as f64 / n;
    let z2 = 1.96_f64.powi(2);
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let half = 1.96 * ((p * (1.0 - p) + z2 / (4.0 * n)) / n).sqrt() / denom;
    Some([(center - half).max(0.0), (center + half).min(1.0)])
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excellent_synthetic_scores_never_open_release_without_acceptance() {
        let mut e = Evidence {
            candidate_hits: 1000,
            mixed_episodes: 1000,
            pure_episodes: 5000,
            literal_correct: 1000,
            final_literal_correct: 1000,
            literal_expected: 1000,
            literal_proposed: 1000,
            performance_passed: true,
            ..Default::default()
        };
        assert!(!decide(&e).candidates_public);
        e.human_pure = 5000;
        e.human_mixed = 1000;
        e.safety_and_tsf_passed = true;
        assert!(decide(&e).candidates_public);
        assert!(!decide(&e).automatic_public);
        e.irreversible_errors = Some(0);
        assert!(decide(&e).automatic_public);
        e.literal_proposed = 1011; // Intermediate mistakes must fail even with perfect final recall.
        assert!(!decide(&e).automatic_public);
        e.literal_proposed = 1000;
        e.pure_false_episodes = 6;
        assert!(!decide(&e).automatic_public);
        e.candidate_hits = 949;
        assert!(!decide(&e).candidates_public);
    }
    #[test]
    fn zero_observed_errors_still_has_a_nonzero_upper_bound() {
        assert!(wilson(0, 25).unwrap()[1] > 0.1);
        assert!(wilson(0, 0).is_none());
        assert!(wilson(2, 1).is_none());
    }
}
