//! ASCII sparkline rendering for the trend table / dashboard.

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
