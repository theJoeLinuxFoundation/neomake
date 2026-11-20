//! CLI surface for `neomake`.
//!
//! Parsed with `clap` (derive):
//!
//! ```text
//! neomake [-c FILE] [--concurrency N] [--no-cache] [-q] run [TASK]... [run options]
//! neomake [-c FILE]                                     list
//! neomake [-c FILE]                                     cache clean | cache status
//! ```

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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

    /// Quiet mode — suppress per-task stdout/stderr forwarding.
    #[arg(short, long, global = true, default_value_t = false)]
    pub quiet: bool,

    /// Subcommand.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one or more tasks (and their dependencies). If no task name
    /// is given, every task is run.
    Run {
        /// Names of tasks to run.
        #[arg(value_name = "TASK")]
        tasks: Vec<String>,
        /// How the run behaves.
        #[command(flatten)]
        options: RunArgs,
    },
    /// List every task in topological order with its dependencies.
    List,
    /// Cache management subcommands.
    #[command(subcommand)]
    Cache(CacheCommand),
}

/// Options of `run`.
#[derive(Debug, Args, Clone)]
pub struct RunArgs {
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

    /// How many of a failed task's last lines to show.
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub tail: usize,
}

/// `cache` subcommands.
#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Remove every entry from the cache directory.
    Clean,
    /// Print a one-line summary of the cache contents.
    Status,
}
