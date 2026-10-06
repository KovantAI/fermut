//! Subcommand handlers. Each `Cmd` variant in `cli::mod` dispatches into one
//! of these modules; keeps the top-level dispatch readable.

pub mod autofix;
pub mod baseline;
pub mod clean;
pub mod completions;
pub mod coverage;
pub mod dashboard;
pub mod doctor;
pub mod explain;
pub mod hom;
pub mod init;
pub mod install_skills;
pub mod list;
pub mod mcp;
pub mod migrate;
pub mod next;
pub mod pr_comment;
pub mod run;
pub mod score;
pub mod show;
pub mod suggest;
pub mod trend;
