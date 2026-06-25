//! Serde adapter for `ruff_text_size::TextRange` so `Mutant` can round-trip
//! through JSON as a `[start, end]` tuple.

use ruff_text_size::TextRange;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub fn serialize<S: Serializer>(r: &TextRange, ser: S) -> Result<S::Ok, S::Error> {
    [u32::from(r.start()), u32::from(r.end())].serialize(ser)
}

pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<TextRange, D::Error> {
    let pair = <[u32; 2]>::deserialize(de)?;
    Ok(TextRange::new(pair[0].into(), pair[1].into()))
}
