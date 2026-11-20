//! Core library for `neomake` — a TOML/TOMLX-driven parallel build
//! orchestrator.
//!
//! `neomake-core` provides the pieces that do not care whether a config
//! was written in plain TOML or in the TOMLX superset: the task model,
//! the dependency-graph builder, the content-addressable cache, and the
//! tokio-based parallel executor. The binary crate and the TOMLX crate
//! are thin layers that feed this core.
//!
//! # Crates in the workspace
//!
//! - [`neomake-core`](crate) — this crate.
//! - `neomake-tomlx` — lexer/parser/evaluator for the TOMLX syntax.
//! - `neomake` — the `neomake` CLI binary.
//!
//! # Quick tour
//!
//! 1. Evaluate the configuration's documents (via `neomake-tomlx`) and
//!    build a [`config::Config`] with [`config::Config::from_sources`].
//! 2. Build a DAG with [`dag::Dag::build`].
//! 3. Open a [`cache::Cache`] in the project's state directory.
//! 4. Call [`executor::run`] with selected tasks, the cache, and [`executor::RunOptions`].

#![warn(missing_docs)]
#![deny(clippy::all)]

pub mod asset;
pub mod cache;
pub mod config;
pub mod dag;
pub mod duration;
pub mod error;
pub mod executor;
pub mod glob;
pub mod process;
pub mod report;
pub mod rundir;
pub mod shell;
pub mod task;

pub use cache::{Cache, CacheEntry, CacheStatus, OutputRecord};
pub use config::{Config, Settings};
pub use dag::Dag;
pub use error::{CacheError, ConfigError, CoreError, DagError, ExecError};
pub use executor::{
    run, select_tasks, ExecutionEvent, Reporter, RunOptions, RunReport, SkipReason, TaskOutcome,
    TaskStatus,
};
pub use rundir::RunDir;
pub use shell::{resolve as resolve_shell, ShellInvocation};
pub use task::{project_root_for, Location, Requirements, Task, TaskKind, TaskSet};
