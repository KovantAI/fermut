//! Source identity for the `coverage` cache mode (the default).
//!
//! A cached verdict holds while the code its tests run is unchanged. Coverage
//! says which code that is: each covering test executed some top-level
//! defs/classes, in any source file. A mutant's identity hash is the global
//! layer (import-time code of every source file) plus, per covering test, a
//! digest of the defs/classes that test executed. Editing `foo` re-runs only
//! the mutants whose tests ran `foo`, and an edit in another file reaches the
//! mutants whose tests call into it — which neither `file` nor `scope` mode
//! sees.
//!
//! Import-time code goes into one global layer, not per test: coverage credits
//! a module's import to whichever test file happened to import it first (a
//! collect context), not to the tests that read its constants. So an edit to
//! an import, a module constant or a decorator anywhere re-runs everything.
//!
//! Gap: a mutant that steers a test into code the baseline never ran, which
//! is then edited, keeps its stale verdict until the coverage DB is refreshed.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::ast_hash;
use crate::filter::coverage::CoverageContexts;
use crate::mutator::Mutant;

type LineTests = HashMap<u32, Vec<String>>;

pub struct CovIdentity {
    globals: String,
    /// Per test node id: digest of the top-level scopes it executed.
    tests: HashMap<String, String>,
}

impl CovIdentity {
    /// Parse every covered file and every mutated file once. `mutated` adds
    /// files the coverage map doesn't list (never executed) to the global layer.
    /// Files under `tests` are left out: test edits are the test
    /// fingerprint's job, and a new test file mustn't re-run everything.
    pub fn build<'a>(
        ctx: &CoverageContexts,
        tests: &Path,
        mutated: impl IntoIterator<Item = &'a Path>,
    ) -> Self {
        let root = ctx.node_root();
        let tests = tests.canonicalize().unwrap_or_else(|_| tests.to_path_buf());
        // Sorted by path relative to the node root: deterministic, and the
        // same on another checkout location (a CI cache restore).
        let mut files: BTreeMap<String, (PathBuf, Option<&LineTests>)> = BTreeMap::new();
        for (path, lines) in ctx.files().filter(|(p, _)| !p.starts_with(&tests)) {
            files.insert(rel(root, path), (path.to_path_buf(), Some(lines)));
        }
        for path in mutated {
            let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            files.entry(rel(root, &canon)).or_insert((canon, None));
        }

        let mut globals = Sha256::new();
        globals.update(b"cov-globals:v1\n");
        let mut ran: HashMap<&str, BTreeSet<(usize, usize)>> = HashMap::new();
        let mut scope_hashes: Vec<Vec<String>> = Vec::with_capacity(files.len());
        for (i, (name, (path, lines))) in files.iter().enumerate() {
            globals.update(name.as_bytes());
            globals.update(b"\n");
            let Some(layout) = ast_hash::scope_layout(path) else {
                // Unparseable: any byte change re-runs everything.
                if let Ok(bytes) = std::fs::read(path) {
                    globals.update(Sha256::digest(&bytes));
                }
                scope_hashes.push(Vec::new());
                continue;
            };
            globals.update(layout.globals.as_bytes());
            globals.update(b"\n");
            for (line, ids) in lines.iter().copied().flatten() {
                let Some(s) = layout.scope_at(*line) else {
                    continue; // module level: in the global layer
                };
                for id in ids {
                    ran.entry(id.as_str()).or_default().insert((i, s));
                }
            }
            scope_hashes.push(layout.scopes.into_iter().map(|s| s.2).collect());
        }

        let names: Vec<&String> = files.keys().collect();
        let tests = ran
            .into_iter()
            .map(|(id, set)| {
                let mut h = Sha256::new();
                for (f, s) in set {
                    h.update(names[f].as_bytes());
                    h.update(b"|");
                    h.update(scope_hashes[f][s].as_bytes());
                    h.update(b"\n");
                }
                (id.to_string(), hex::encode(h.finalize()))
            })
            .collect();
        Self {
            globals: hex::encode(globals.finalize()),
            tests,
        }
    }

    /// Identity hash for `mutant`, or `None` when coverage doesn't reach it
    /// (the caller falls back to the file hash).
    pub fn hash_for(&self, ctx: &CoverageContexts, mutant: &Mutant) -> Option<String> {
        let mut ids: Vec<&String> = ctx.tests_for_mutant(mutant)?.iter().collect();
        if ids.is_empty() {
            return None;
        }
        ids.sort();
        let mut h = Sha256::new();
        h.update(b"cov:v1\n");
        h.update(self.globals.as_bytes());
        for id in ids {
            h.update(b"\n");
            h.update(id.as_bytes());
            h.update(b"=");
            h.update(self.tests.get(id).map_or("", String::as_str).as_bytes());
        }
        Some(hex::encode(h.finalize()))
    }
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}
