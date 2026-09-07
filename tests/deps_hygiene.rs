//! Guards dependency-hygiene decisions that regress silently — invisible at
//! runtime but they re-bloat the build and widen the dep tree.

/// `ureq` must stay on a minimal feature set. The Anthropic client makes one
/// stateless JSON POST, so cookie storage and gzip decompression — and the
/// `cookie_store` / `time` / `flate2` / `zlib-rs` trees they drag in — are dead
/// weight. Dropping `default-features = false` (as the ureq 2 → 3 bump briefly
/// did) pulls them all back in, so pin the intent at the manifest level.
#[test]
fn ureq_stays_on_minimal_features() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let doc: toml::Value = toml::from_str(&manifest).unwrap();

    let ureq = doc["dependencies"]["ureq"]
        .as_table()
        .expect("ureq should be a table dependency");

    assert_eq!(
        ureq.get("default-features").and_then(toml::Value::as_bool),
        Some(false),
        "ureq must set default-features = false (drops cookies + gzip)",
    );

    let feats: Vec<&str> = ureq["features"]
        .as_array()
        .expect("ureq features should be an array")
        .iter()
        .map(|v| v.as_str().expect("feature is a string"))
        .collect();

    for banned in ["cookies", "gzip", "brotli"] {
        assert!(
            !feats.contains(&banned),
            "ureq feature `{banned}` re-introduces dep bloat for a stateless JSON POST",
        );
    }
    assert!(feats.contains(&"rustls"), "ureq still needs rustls for TLS");
    assert!(
        feats.contains(&"json"),
        "ureq still needs json for send/read_json"
    );
}
