//! CLI surface for `neomake`.
//!
//! Parsed with `clap` (derive):
//!
//! ```text
//! neomake [GLOBAL] run   [PATTERN]... [RUN OPTIONS]   tasks and tests, and their deps
//! neomake [GLOBAL] test  [PATTERN]... [RUN OPTIONS]   tests, and their deps
//! neomake [GLOBAL] list  [--tests]
//! neomake [GLOBAL] check
//! neomake [GLOBAL] log   [TASK] [--run ID]
//! neomake [GLOBAL] cache clean | status
//! ```

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Top-level CLI arguments.
#[derive(Debug, Parser)]
#[command(
    name = "neomake",
    version,
    about = "A TOML/TOMLX task runner and test harness."
)]
pub struct Cli {
    /// Path to the configuration file. If omitted, `neomake.toml` then
    /// `neomake.tomlx` are tried in the current directory.
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Maximum number of tasks that may run commands concurrently.
    #[arg(long, global = true)]
    pub concurrency: Option<usize>,

    /// Disable all cache lookups for this invocation.
    #[arg(long, global = true, default_value_t = false)]
    pub no_cache: bool,

    /// Show only the output of what fails (the same as `--output failures`).
    #[arg(short, long, global = true, default_value_t = false)]
    pub quiet: bool,

    /// Show more: every step as it passes; with `list`, requirements.
    #[arg(short, long, global = true, default_value_t = false)]
    pub verbose: bool,

    /// Subcommand.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run tasks and tests, with what they depend on. Without patterns,
    /// everything runs.
    Run {
        /// Task names, matrix names, or names with `*` and `?`.
        #[arg(value_name = "PATTERN")]
        patterns: Vec<String>,
        /// How the run behaves.
        #[command(flatten)]
        options: RunArgs,
    },
    /// Run tests, with what they depend on. Without patterns, every test
    /// runs.
    Test {
        /// Test names, matrix names, or names with `*` and `?`.
        #[arg(value_name = "PATTERN")]
        patterns: Vec<String>,
        /// How the run behaves.
        #[command(flatten)]
        options: RunArgs,
    },
    /// List tasks and tests in the order they run, with what they are
    /// for, their tags and dependencies.
    List {
        /// Only tests.
        #[arg(long)]
        tests: bool,
    },
    /// Check the configuration without running anything.
    Check,
    /// Print a task's log from the last run (or from `--run`). Without a
    /// task, list what the run logged.
    Log {
        /// The task or test.
        task: Option<String>,
        /// The run's id (its directory under `runs/`).
        #[arg(long, value_name = "ID")]
        run: Option<String>,
    },
    /// Cache management subcommands.
    #[command(subcommand)]
    Cache(CacheCommand),
}

/// How progress is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    /// Every line as it is printed, prefixed with its task.
    Stream,
    /// Each task's lines together, once it ends (the default on GitHub
    /// Actions, where each task is a collapsible group).
    Grouped,
    /// Only the last lines of what fails, at the end.
    Failures,
}

/// Options of `run` and `test`.
#[derive(Debug, Args, Clone)]
pub struct RunArgs {
    /// Only what has this tag (may be given more than once).
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Leave out what has this tag (may be given more than once).
    #[arg(long = "exclude-tag", value_name = "TAG")]
    pub exclude_tags: Vec<String>,

    /// Show what would run, be up to date or be skipped, and why,
    /// without running anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Stop at the first failure: running tasks are stopped and the rest
    /// are not started.
    #[arg(long)]
    pub fail_fast: bool,

    /// Multiply every timeout by this factor (for a slow host, say 2).
    #[arg(long, value_name = "FACTOR", default_value_t = 1.0)]
    pub timeout_scale: f64,

    /// Also write the run's report (as in the run directory's
    /// report.json) to this file.
    #[arg(long, value_name = "FILE")]
    pub json: Option<PathBuf>,

    /// Write JUnit XML of the tests to this file.
    #[arg(long, value_name = "FILE")]
    pub junit: Option<PathBuf>,

    /// How many of a failed task's last lines to show.
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub tail: usize,

    /// How progress is shown.
    #[arg(long, value_enum)]
    pub output: Option<OutputMode>,
}

/// `cache` subcommands.
#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Remove every entry from the cache directory.
    Clean,
    /// Print a one-line summary of the cache contents.
    Status,
}
