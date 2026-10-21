//! Unicode block sparkline rendering for score sequences.

/// Render a Unicode block-based sparkline for a sequence of values
/// against an explicit `[lo, hi]` range. Values outside the range are
/// clamped. Empty input → empty string. If `hi <= lo` the range
/// collapses and every value renders as the lowest block.
pub fn sparkline_scaled<I: IntoIterator<Item = f64>>(values: I, lo: f64, hi: f64) -> String {
    const BLOCKS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let span = hi - lo;
    let mut out = String::new();
    for v in values {
        let idx = if span <= 0.0 {
            0
        } else {
            let norm = ((v - lo) / span).clamp(0.0, 1.0);
            (norm * (BLOCKS.len() - 1) as f64).round() as usize
        };
        out.push(BLOCKS[idx.min(BLOCKS.len() - 1)]);
    }
    out
}

/// Render a sparkline against the fixed `[0, 100]` mutation-score range.
/// Used by the Markdown report so two reports remain visually
/// comparable; `fermut trend` can opt into auto-scaling via `--scale auto`.
pub fn sparkline<I: IntoIterator<Item = f64>>(values: I) -> String {
    sparkline_scaled(values, 0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_renders_8_buckets() {
        let s = sparkline([0.0, 12.5, 25.0, 50.0, 75.0, 100.0]);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars.first(), Some(&'▁'));
        assert_eq!(chars.last(), Some(&'█'));
        assert_eq!(chars.len(), 6);
    }

    #[test]
    fn sparkline_handles_empty_input() {
        assert!(sparkline(std::iter::empty::<f64>()).is_empty());
    }

    #[test]
    fn sparkline_clamps_out_of_range_values() {
        let s = sparkline([-50.0, 200.0]);
        assert_eq!(s.chars().collect::<Vec<_>>(), vec!['▁', '█']);
    }

    #[test]
    fn sparkline_scaled_fills_height_for_narrow_window() {
        // Fixed-scale [0,100] crushes 85→95 into the same bucket; scaled
        // to its own range it spans floor→top.
        let s = sparkline_scaled([85.0, 95.0], 85.0, 95.0);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars.first(), Some(&'▁'));
        assert_eq!(chars.last(), Some(&'█'));
    }

    #[test]
    fn sparkline_scaled_clamps_to_explicit_bounds() {
        let s = sparkline_scaled([50.0, 200.0, -10.0], 80.0, 90.0);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars, vec!['▁', '█', '▁']);
    }

    #[test]
    fn sparkline_scaled_collapses_zero_span_safely() {
        let s = sparkline_scaled([42.0, 42.0, 42.0], 42.0, 42.0);
        assert_eq!(s.chars().collect::<Vec<_>>(), vec!['▁', '▁', '▁']);
    }
}
