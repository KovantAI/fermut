//! Schema-migration gate. Drops mutants in generated migration files unless
//! `--mutate-migrations` (or `mutate_migrations = true`) is set: Django
//! `migrations/0001_*.py` and Alembic `versions/*.py`. A test DB is built from
//! them once, so a survivor there means "no test reads this column's
//! `max_length`", not a logic gap, and they swamp the score (healthchecks:
//! 55 % of mutants, 76 % surviving). They also defeat schemata: `operations`
//! is a class attribute, run at import.

use std::path::Path;

use anyhow::Result;

use super::Filter;
use crate::mutator::Mutant;

/// Drops migration-file mutants unless `include` is set.
pub struct MigrationsFilter {
    include: bool,
}

impl MigrationsFilter {
    pub fn new(include: bool) -> Self {
        Self { include }
    }
}

impl Filter for MigrationsFilter {
    fn name(&self) -> &'static str {
        "migrations"
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        Ok(self.include || !is_migration(&mutant.file))
    }
}

/// Django: a digit-led module in a `migrations` package (`0001_initial.py`).
/// Alembic: any module in `versions/` next to the environment's `env.py`.
/// The name checks run first, so ordinary files never touch the filesystem.
fn is_migration(file: &Path) -> bool {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name().and_then(|n| n.to_str())) else {
        return false;
    };
    match dir.file_name().and_then(|n| n.to_str()) {
        Some("migrations") => {
            name.starts_with(|c: char| c.is_ascii_digit()) && dir.join("__init__.py").is_file()
        }
        Some("versions") => dir.parent().is_some_and(|p| p.join("env.py").is_file()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::TextRange;

    fn mutant_in(file: &Path) -> Mutant {
        Mutant {
            id: "x".into(),
            file: file.to_path_buf(),
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
    fn drops_django_and_alembic_migrations_only() {
        let d = tempfile::tempdir().unwrap();
        let touch = |rel: &str| {
            let p = d.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            p
        };
        touch("app/migrations/__init__.py");
        let django = touch("app/migrations/0001_initial.py");
        let helper = touch("app/migrations/utils.py");
        // A `migrations` dir that isn't a package (data, docs) is left alone.
        let loose = touch("docs/migrations/0001_notes.py");
        touch("alembic/env.py");
        let alembic = touch("alembic/versions/ae1027a6acf_add_users.py");
        let other_versions = touch("pkg/versions/v2.py");

        let off = MigrationsFilter::new(false);
        let admits = |p: &Path| off.admits(&mutant_in(p)).unwrap();
        assert!(!admits(&django));
        assert!(!admits(&alembic));
        assert!(admits(&helper));
        assert!(admits(&loose));
        assert!(admits(&other_versions));
        assert!(MigrationsFilter::new(true)
            .admits(&mutant_in(&django))
            .unwrap());
    }
}
