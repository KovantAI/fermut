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

/// Every declared dependency must be referenced from `src/` or `examples/`.
/// An unused dep (e.g. `thiserror`, declared for months with zero uses) costs
/// build time and dep-tree surface while signalling an error-handling story
/// the code doesn't have. A lightweight stand-in for `cargo-machete`: a crate
/// counts as used when its identifier appears as `ident::` or `use ident`.
#[test]
fn every_dependency_is_referenced() {
    // Declared but not textually referenced, with the reason it stays.
    const ALLOWED_UNREFERENCED: &[&str] = &[
        // Unreferenced in source; ruff git deps move as a set, so dropping it
        // is its own change. Remove from this list once that is decided.
        "ty_python_semantic",
    ];

    let root = env!("CARGO_MANIFEST_DIR");
    let manifest = std::fs::read_to_string(format!("{root}/Cargo.toml")).unwrap();
    let doc: toml::Value = toml::from_str(&manifest).unwrap();

    let mut deps: Vec<String> = doc["dependencies"]
        .as_table()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
        for t in targets.values() {
            if let Some(d) = t.get("dependencies").and_then(toml::Value::as_table) {
                deps.extend(d.keys().cloned());
            }
        }
    }

    let mut source = String::new();
    for dir in ["src", "examples"] {
        collect_rs(&std::path::Path::new(root).join(dir), &mut source);
    }

    let unused: Vec<&String> = deps
        .iter()
        .filter(|d| !ALLOWED_UNREFERENCED.contains(&d.as_str()))
        .filter(|d| {
            let ident = d.replace('-', "_");
            !source.contains(&format!("{ident}::")) && !source.contains(&format!("use {ident}"))
        })
        .collect();
    assert!(
        unused.is_empty(),
        "dependencies declared in Cargo.toml but never referenced: {unused:?}",
    );
}

fn collect_rs(dir: &std::path::Path, out: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push_str(&std::fs::read_to_string(&path).unwrap());
            out.push('\n');
        }
    }
}
