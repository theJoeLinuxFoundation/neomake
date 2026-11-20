//! `neomake` CLI binary.
//!
//! A thin wrapper that:
//!
//! 1. Parses command-line arguments with [`cli`].
//! 2. Locates the config file (`--config`, else `neomake.toml`, else
//!    `neomake.tomlx`) and loads it, with everything it includes, through
//!    `neomake-tomlx`.
//! 3. Hands the resulting [`neomake_core::Config`] to the DAG executor,
//!    with a run directory for logs and the report.
//! 4. Prints progress as the run goes ([`output`]).
//!
//! Exit status: 0 when every selected task succeeded, was cached or was
//! skipped; 1 when one failed or timed out, or the configuration is
//! wrong; 130 when the run was interrupted.

mod cli;
mod output;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use clap::Parser;

use neomake_core::report::{Report, RunMeta};
use neomake_core::{executor, Cache, Config, Dag, RunDir, RunOptions};

use crate::cli::{CacheCommand, Cli, Command, RunArgs};

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(err) => {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "error: {err:#}");
            ExitCode::from(1)
        }
    }
}

/// A loaded configuration and where things go.
struct Project {
    config_path: PathBuf,
    config: Config,
}

impl Project {
    fn load(explicit: Option<&Path>) -> Result<Self> {
        let config_path = resolve_config(explicit)?;
        let loaded = neomake_tomlx::load_documents(&config_path)
            .with_context(|| format!("failed to load `{}`", config_path.display()))?;
        let sources: Vec<(PathBuf, String)> = loaded
            .documents
            .into_iter()
            .map(|d| (d.path, d.text))
            .collect();
        let config = Config::from_sources(&loaded.root, &sources)
            .with_context(|| format!("invalid configuration in `{}`", config_path.display()))?;
        Ok(Self {
            config_path,
            config,
        })
    }

    fn root(&self) -> &Path {
        &self.config.root
    }

    fn state_dir(&self) -> PathBuf {
        self.root().join(&self.config.settings.state_dir)
    }

    fn cache(&self) -> Result<Cache> {
        Ok(Cache::open_in(
            self.root(),
            self.state_dir().join("cache"),
            vec![self.config.settings.state_dir.clone()],
        )?)
    }
}

fn real_main() -> Result<ExitCode> {
    init_tracing();
    let cli = Cli::parse();

    match &cli.command {
        Command::Run { tasks, options } => run(&cli, tasks, options),
        Command::List => {
            let project = Project::load(cli.config.as_deref())?;
            let dag = Dag::build(project.config.tasks.tasks())?;
            println!(
                "tasks in `{}` (topological order):",
                project.config_path.display()
            );
            for name in dag.topo_order() {
                let deps = dag.deps_of(name);
                if deps.is_empty() {
                    println!("  {name}");
                } else {
                    println!("  {name}  (deps: {})", deps.join(", "));
                }
            }
            Ok(ExitCode::from(0))
        }
        Command::Cache(cmd) => {
            let cache = match Project::load(cli.config.as_deref()) {
                Ok(project) => project.cache()?,
                Err(_) => Cache::open(".")?,
            };
            match cmd {
                CacheCommand::Clean => {
                    cache.clean()?;
                    println!("cache cleaned: {}", cache.dir().display());
                }
                CacheCommand::Status => {
                    let status = cache.status()?;
                    println!(
                        "cache at {}: {} entries, {} bytes",
                        cache.dir().display(),
                        status.entries,
                        status.bytes
                    );
                }
            }
            Ok(ExitCode::from(0))
        }
    }
}

fn run(cli: &Cli, targets: &[String], args: &RunArgs) -> Result<ExitCode> {
    if !(args.timeout_scale.is_finite() && args.timeout_scale > 0.0) {
        return Err(anyhow!("--timeout-scale must be a positive number"));
    }
    let project = Project::load(cli.config.as_deref())?;
    let dag = Dag::build(project.config.tasks.tasks())?;
    let selected = executor::select_tasks(&dag, targets)?;
    let cache = project.cache()?;
    let state_dir = project.state_dir();
    let run_dir = RunDir::create(&state_dir)
        .with_context(|| format!("cannot create a run directory in {}", state_dir.display()))?;

    let settings = &project.config.settings;
    let mut options = RunOptions::new(
        project.root(),
        cli.concurrency.unwrap_or_else(num_cpus::get),
    );
    options.use_cache = !cli.no_cache;
    options.fail_fast = args.fail_fast;
    options.timeout_scale = args.timeout_scale;
    options.default_timeout = settings.default_timeout;
    options.default_shell = settings.shell.clone();
    options.log_dir = Some(run_dir.logs());

    let style = output::Style {
        color: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        quiet: cli.quiet,
    };
    let started_at = SystemTime::now();
    let started = Instant::now();
    let interrupted = Arc::new(AtomicBool::new(false));

    let runtime = tokio_runtime()?;
    let report = runtime.block_on(async {
        let cancel = options.cancel.clone();
        let flag = Arc::clone(&interrupted);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                flag.store(true, Ordering::SeqCst);
                eprintln!("\ninterrupted: stopping (Ctrl-C again to quit at once)");
                cancel.cancel();
                if tokio::signal::ctrl_c().await.is_ok() {
                    std::process::exit(130);
                }
            }
        });
        executor::run(&selected, &cache, &options, Some(output::reporter(style))).await
    })?;
    let elapsed = started.elapsed();
    let interrupted = interrupted.load(Ordering::SeqCst);

    let report_json = Report::new(
        &report,
        &selected,
        &RunMeta {
            id: run_dir.id.clone(),
            started_at,
            duration: elapsed,
            interrupted,
            root: project.root().to_path_buf(),
        },
    )
    .to_json();
    std::fs::write(run_dir.path.join("report.json"), &report_json)
        .with_context(|| format!("cannot write {}", run_dir.path.display()))?;
    if let Some(path) = &args.json {
        std::fs::write(path, &report_json)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    run_dir.mark_latest();
    RunDir::prune(&state_dir, settings.keep_runs);

    output::summary(
        &report,
        elapsed,
        Some(&run_dir.path),
        project.root(),
        style,
        args.tail,
    );
    Ok(if interrupted {
        ExitCode::from(130)
    } else if report.had_failure() {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    })
}

fn resolve_config(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            return Err(anyhow!("config file not found: {}", p.display()));
        }
        return Ok(p.to_path_buf());
    }
    let candidates = ["neomake.toml", "neomake.tomlx"];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "no configuration file found (looked for `neomake.toml` and `neomake.tomlx`)"
    ))
}

fn tokio_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start tokio runtime")
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("NEOMAKE_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(io::stderr)
        .compact()
        .try_init();
}
