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
//! Not a pipeline layer: [`llm`], the LLM equivalence judge. A model's
//! opinion can't prove equivalence, so it never touches the score; `autofix`
//! and `suggest` use it to triage survivors (`--triage`).

pub mod bytecode;
pub mod llm;
pub mod patterns;

use crate::mutator::Mutant;

/// One detector's verdict on a single mutant.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::enum_variant_names)] // `*Equivalent` reads as a scale, not noise
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

pub trait EquivDetector: Send + Sync {
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
        fn detect(&self, _m: &Mutant, _s: &str) -> EquivVerdict {
            EquivVerdict::ProvablyEquivalent {
                reason: "test".into(),
                source: "test",
            }
        }
    }

    struct AlwaysLikely(f32);
    impl EquivDetector for AlwaysLikely {
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
            site: None,
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
