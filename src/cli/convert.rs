//! CLI value-enum types and their conversions into domain types.
//!
//! clap `ValueEnum`s can't derive `From` for the crate's runtime enums
//! directly (they live in other modules), so each CLI-facing enum here owns a
//! hand-written `From` into its domain counterpart. Kept out of `cli::mod` so
//! the parser file stays a parser file.

use clap::ValueEnum;

use crate::report::ReportFormat;

use super::subcmd::{
    autofix::AutofixFormat,
    baseline::BaselineFormat,
    explain::ExplainFormat,
    migrate::MigrateSource,
    next::NextFormat,
    score::ScoreFormat,
    suggest::SuggestFormat,
    trend::{TrendFormat, TrendGroupBy, TrendScale},
};

/// Generic human/JSON output selector shared by most subcommands.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum Format {
    Human,
    Json,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum TrendFormatCli {
    Human,
    Json,
}

impl From<TrendFormatCli> for TrendFormat {
    fn from(f: TrendFormatCli) -> Self {
        match f {
            TrendFormatCli::Human => Self::Human,
            TrendFormatCli::Json => Self::Json,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum TrendScaleCli {
    Fixed,
    Auto,
}

impl From<TrendScaleCli> for TrendScale {
    fn from(s: TrendScaleCli) -> Self {
        match s {
            TrendScaleCli::Fixed => Self::Fixed,
            TrendScaleCli::Auto => Self::Auto,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum TrendGroupByCli {
    File,
}

impl From<TrendGroupByCli> for TrendGroupBy {
    fn from(g: TrendGroupByCli) -> Self {
        match g {
            TrendGroupByCli::File => Self::File,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum RunnerCli {
    Pytest,
    Rstest,
    Unittest,
}

impl From<RunnerCli> for crate::config::RunnerKind {
    fn from(r: RunnerCli) -> Self {
        match r {
            RunnerCli::Pytest => Self::Pytest,
            RunnerCli::Rstest => Self::Rstest,
            RunnerCli::Unittest => Self::Unittest,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum MigrateSourceCli {
    Mutmut,
    #[value(alias = "cosmic_ray")]
    CosmicRay,
}

impl From<MigrateSourceCli> for MigrateSource {
    fn from(s: MigrateSourceCli) -> Self {
        match s {
            MigrateSourceCli::Mutmut => Self::Mutmut,
            MigrateSourceCli::CosmicRay => Self::CosmicRay,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum IsolationCli {
    Auto,
    Copy,
    Hardlink,
    Reflink,
}

impl From<IsolationCli> for crate::config::IsolationMode {
    fn from(m: IsolationCli) -> Self {
        match m {
            IsolationCli::Auto => Self::Auto,
            IsolationCli::Copy => Self::Copy,
            IsolationCli::Hardlink => Self::Hardlink,
            IsolationCli::Reflink => Self::Reflink,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum CacheScopeCli {
    File,
    Scope,
}

impl From<CacheScopeCli> for crate::config::CacheScope {
    fn from(c: CacheScopeCli) -> Self {
        match c {
            CacheScopeCli::File => Self::File,
            CacheScopeCli::Scope => Self::Scope,
        }
    }
}

impl From<Format> for ReportFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => ReportFormat::Human,
            Format::Json => ReportFormat::Json,
        }
    }
}

impl From<Format> for ExplainFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => ExplainFormat::Human,
            Format::Json => ExplainFormat::Json,
        }
    }
}

impl From<Format> for SuggestFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => SuggestFormat::Human,
            Format::Json => SuggestFormat::Json,
        }
    }
}

impl From<Format> for ScoreFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => ScoreFormat::Human,
            Format::Json => ScoreFormat::Json,
        }
    }
}

impl From<Format> for NextFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => NextFormat::Human,
            Format::Json => NextFormat::Json,
        }
    }
}

impl From<Format> for BaselineFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => BaselineFormat::Human,
            Format::Json => BaselineFormat::Json,
        }
    }
}

impl From<Format> for AutofixFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => AutofixFormat::Human,
            Format::Json => AutofixFormat::Json,
        }
    }
}
