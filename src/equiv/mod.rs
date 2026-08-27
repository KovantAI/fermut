//! Equivalent-mutant detection.
//!
//! A layered, precision-biased pipeline that classifies a `Survived` mutant
//! as **provably equivalent** (skip from the score denominator) or **likely
//! equivalent** (surface as a suggestion to the human).
//!
//! Layers shipped here:
//!
//! - [`bytecode::BytecodeIdentity`] — compile original + mutated source with
//!   CPython and compare code-object signatures. Catches text-different but
//!   bytecode-identical mutations (e.g. `return` vs `return None`).
//! - [`patterns::AstPatterns`] — pure-Rust pattern rules over the mutant's
//!   operator + adjacent tokens. Catches `x +/- 0` and `x */÷ 1` identities.
//!
//! - [`probe::HypothesisProbe`] — differential property test driven by
//!   `hypothesis.given(from_type(...))`. Pass-on-N gives a 0.80 confidence
//!   signal that must stack with another layer to clear the pipeline
//!   threshold. Opt-in; not in [`EquivPipeline::default_pipeline`].
//! - [`llm::LlmJudge`] — model-judged equivalence with a strict JSON
//!   contract and a calibrated confidence clamp. Also opt-in; runs last
//!   because it's the most expensive and the least precise.

pub mod bytecode;
pub mod llm;
pub mod patterns;
pub mod probe;

use crate::mutator::Mutant;

/// One detector's verdict on a single mutant.
#[derive(Clone, Debug, PartialEq)]
pub enum EquivVerdict {
    /// Detector has no opinion / mutant is not equivalent under this rule.
    NotEquivalent,
    /// Detector estimates equivalence with some confidence in `[0, 1]`.
    LikelyEquivalent {
        confidence: f32,
        reason: String,
        source: &'static str,
    },
    /// Detector has proven equivalence. Treat as ground truth.
    ProvablyEquivalent {
        reason: String,
        source: &'static str,
    },
}

impl EquivVerdict {
    pub fn is_equivalent_signal(&self) -> bool {
        !matches!(self, EquivVerdict::NotEquivalent)
    }
}

pub trait EquivDetector: Send + Sync {
    fn name(&self) -> &'static str;
    /// Inspect a mutant given the original (un-patched) source of the file
    /// it lives in. Detectors must be pure with respect to their inputs.
    fn detect(&self, mutant: &Mutant, source: &str) -> EquivVerdict;
}

/// Aggregator. Runs detectors cheap-first; short-circuits on
/// `ProvablyEquivalent`. Otherwise returns the highest-confidence
/// `LikelyEquivalent`, or `NotEquivalent` if nothing fired.
pub struct EquivPipeline {
    detectors: Vec<Box<dyn EquivDetector>>,
    /// Combined-confidence threshold for surfacing a suggestion. Below this,
    /// we treat the mutant as still-not-equivalent. 0.85 by default — one
    /// strong rule (0.85) is enough; two weak signals stack toward it.
    threshold: f32,
}

impl EquivPipeline {
    pub fn new(detectors: Vec<Box<dyn EquivDetector>>) -> Self {
        Self {
            detectors,
            threshold: 0.85,
        }
    }

    /// Default pipeline: patterns (cheap) before bytecode (Python subproc).
    pub fn default_pipeline() -> Self {
        Self::new(vec![
            Box::new(patterns::AstPatterns::new()),
            Box::new(bytecode::BytecodeIdentity::new()),
        ])
    }

    pub fn with_threshold(mut self, t: f32) -> Self {
        self.threshold = t;
        self
    }

    /// Append the hypothesis probe (Layer 3). Off by default; the probe
    /// spawns a Python subprocess and depends on `hypothesis` being
    /// installed, so callers opt in explicitly.
    pub fn with_probe(mut self, probe: probe::HypothesisProbe) -> Self {
        self.detectors.push(Box::new(probe));
        self
    }

    /// Append the LLM judge (Layer 4). Off by default; runs last by
    /// construction so cheaper layers short-circuit it for free.
    pub fn with_llm(mut self, judge: llm::LlmJudge) -> Self {
        self.detectors.push(Box::new(judge));
        self
    }

    pub fn classify(&self, mutant: &Mutant, source: &str) -> EquivVerdict {
        let mut best: Option<(f32, String, &'static str)> = None;
        for d in &self.detectors {
            match d.detect(mutant, source) {
                v @ EquivVerdict::ProvablyEquivalent { .. } => return v,
                EquivVerdict::LikelyEquivalent {
                    confidence,
                    reason,
                    source: src,
                } => {
                    let acc = best.as_ref().map(|(c, _, _)| *c).unwrap_or(0.0) + confidence;
                    let capped = acc.min(1.0);
                    best = Some((capped, reason, src));
                }
                EquivVerdict::NotEquivalent => {}
            }
        }
        match best {
            Some((conf, reason, src)) if conf >= self.threshold => EquivVerdict::LikelyEquivalent {
                confidence: conf,
                reason,
                source: src,
            },
            _ => EquivVerdict::NotEquivalent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::{Mutant, Operator};
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    struct AlwaysProvable;
    impl EquivDetector for AlwaysProvable {
        fn name(&self) -> &'static str {
            "always-provable"
        }
        fn detect(&self, _m: &Mutant, _s: &str) -> EquivVerdict {
            EquivVerdict::ProvablyEquivalent {
                reason: "test".into(),
                source: "test",
            }
        }
    }

    struct AlwaysLikely(f32);
    impl EquivDetector for AlwaysLikely {
        fn name(&self) -> &'static str {
            "always-likely"
        }
        fn detect(&self, _m: &Mutant, _s: &str) -> EquivVerdict {
            EquivVerdict::LikelyEquivalent {
                confidence: self.0,
                reason: "test".into(),
                source: "test",
            }
        }
    }

    struct Silent;
    impl EquivDetector for Silent {
        fn name(&self) -> &'static str {
            "silent"
        }
        fn detect(&self, _m: &Mutant, _s: &str) -> EquivVerdict {
            EquivVerdict::NotEquivalent
        }
    }

    fn mutant() -> Mutant {
        Mutant {
            id: "t".into(),
            file: PathBuf::from("t.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn provable_short_circuits() {
        let p = EquivPipeline::new(vec![Box::new(AlwaysProvable), Box::new(AlwaysLikely(0.9))]);
        let v = p.classify(&mutant(), "");
        assert!(matches!(v, EquivVerdict::ProvablyEquivalent { .. }));
    }

    #[test]
    fn likely_below_threshold_is_dropped() {
        let p = EquivPipeline::new(vec![Box::new(AlwaysLikely(0.5))]);
        assert_eq!(p.classify(&mutant(), ""), EquivVerdict::NotEquivalent);
    }

    #[test]
    fn likely_stacks_toward_threshold() {
        let p = EquivPipeline::new(vec![
            Box::new(AlwaysLikely(0.5)),
            Box::new(AlwaysLikely(0.5)),
        ]);
        let v = p.classify(&mutant(), "");
        match v {
            EquivVerdict::LikelyEquivalent { confidence, .. } => {
                assert!((confidence - 1.0).abs() < 1e-6);
            }
            other => panic!("expected LikelyEquivalent, got {other:?}"),
        }
    }

    #[test]
    fn silent_detector_yields_not_equivalent() {
        let p = EquivPipeline::new(vec![Box::new(Silent)]);
        assert_eq!(p.classify(&mutant(), ""), EquivVerdict::NotEquivalent);
    }
}
