//! Arbitrary Python source → mutant generation → splice.
//!
//! Invariants: the visitor never panics, every mutant's range is in bounds and
//! on UTF-8 boundaries, `original` is the text it replaces, the mutation is not
//! a no-op, and the patched source still parses.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(mutants) = fermut::collect_mutants(Path::new("fuzz.py"), src) else {
        return; // not valid Python
    };
    for m in mutants {
        let (start, end) = (usize::from(m.range.start()), usize::from(m.range.end()));
        assert!(start <= end && end <= src.len(), "{m:?} out of bounds");
        assert!(
            src.is_char_boundary(start) && src.is_char_boundary(end),
            "{m:?} splits a codepoint"
        );
        assert_eq!(&src[start..end], m.original, "{m:?} original != source slice");
        assert_ne!(m.original, m.replacement, "{m:?} is a no-op");
        let patched = fermut::patch_source(src, m.range, &m.replacement);
        assert!(
            ruff_python_parser::parse_module(&patched).is_ok(),
            "{m:?} doesn't parse:\n{patched}"
        );
    }
});
