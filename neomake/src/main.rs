//! `neomake` CLI binary.
//!
//! A thin wrapper that:
//!
//! 1. Parses command-line arguments with [`cli`].
//! 2. Locates the config file (`--config`, else `neomake.toml`, else
//!    `neomake.tomlx`) and loads it, with everything it includes, through
//!    `neomake-tomlx`.
//! 3. Selects what to run and hands it to the DAG executor, with a run
//!    directory for logs, test files and the report.
//! 4. Prints progress as the run goes ([`output`]).
//!
//! Exit status: 0 when every selected task succeeded, was cached or was
//! skipped; 1 when one failed or timed out, or the configuration is
//! wrong; 130 when the run was interrupted.

mod cli;
mod github;
mod output;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use neomake_core::report::{Report, RunMeta};
use neomake_core::select::{select, Selection};
use neomake_core::{executor, junit, Cache, Config, Dag, Planned, RunDir, RunOptions};

use crate::cli::{CacheCommand, Cli, Command, MachineCommand, OutputMode, RunArgs};

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
    files: usize,
}

impl Project {
    fn load(explicit: Option<&Path>) -> Result<Self> {
        let config_path = resolve_config(explicit)?;
        let loaded = neomake_tomlx::load_documents(&config_path)
            .with_context(|| format!("failed to load `{}`", config_path.display()))?;
        let files = loaded.documents.len();
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
            files,
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

    fn dag(&self) -> Result<Dag> {
        Ok(Dag::build(self.config.tasks.tasks())?)
    }
}

fn real_main() -> Result<ExitCode> {
    init_tracing();
    let cli = Cli::parse();

    match &cli.command {
        Command::Run { patterns, options } => run(&cli, patterns, options, false),
        Command::Test { patterns, options } => run(&cli, patterns, options, true),
        Command::List { tests } => list(&cli, *tests),
        Command::Check => check(&cli),
        Command::Log { task, run } => log(&cli, task.as_deref(), run.as_deref()),
        Command::Machine(cmd) => machine(&cli, cmd),
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

fn options_for(cli: &Cli, project: &Project, args: &RunArgs) -> RunOptions {
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
    options.services = project.config.services.clone();
    options.machines = Some(Arc::new(neomake_machine::QemuRunner::new(
        project.root(),
        cli.no_kvm,
    )));
    options.max_machines = args.machines.unwrap_or(settings.max_machines).max(1);
    options
}

fn machine(cli: &Cli, cmd: &MachineCommand) -> Result<ExitCode> {
    let project = Project::load(cli.config.as_deref())?;
    match cmd {
        MachineCommand::List => {
            for m in project.config.machines.values() {
                let disks: Vec<String> = m
                    .disks
                    .iter()
                    .map(|d| d.file.clone().or(d.size.clone()).unwrap_or_default())
                    .collect();
                let state = neomake_machine::host::unmet(m, project.root(), cli.no_kvm)
                    .map(|r| format!("  (cannot run here: {r})"))
                    .unwrap_or_default();
                println!(
                    "  {}  {}, {} CPU{}  {}{state}",
                    m.name,
                    m.memory,
                    m.cpus,
                    if m.cpus == 1 { "" } else { "s" },
                    disks.join(", ")
                );
            }
            Ok(ExitCode::from(0))
        }
        MachineCommand::Run {
            name,
            persist,
            media,
        } => {
            let spec = project.config.machines.get(name).cloned().ok_or_else(|| {
                let names: Vec<&str> = project.config.machines.keys().map(String::as_str).collect();
                match executor::closest(name, names.into_iter()) {
                    Some(s) => anyhow!("no machine named `{name}` (did you mean `{s}`?)"),
                    None => anyhow!("no machine named `{name}`"),
                }
            })?;
            if let Some(reason) = neomake_machine::host::unmet(&spec, project.root(), cli.no_kvm) {
                bail!("machine `{name}` cannot run here: {reason}");
            }
            let options = neomake_machine::interactive::Options {
                persist: persist.clone(),
                scratch: project.state_dir().join("machines").join(name),
                media: media.clone(),
                no_kvm: cli.no_kvm,
            };
            let status = tokio_runtime()?
                .block_on(neomake_machine::interactive::run(
                    &spec,
                    project.root(),
                    &options,
                ))
                .map_err(|e| anyhow!(e))?;
            Ok(ExitCode::from(
                status.code().unwrap_or(1).clamp(0, 255) as u8
            ))
        }
    }
}

fn run(cli: &Cli, patterns: &[String], args: &RunArgs, tests_only: bool) -> Result<ExitCode> {
    if !(args.timeout_scale.is_finite() && args.timeout_scale > 0.0) {
        bail!("--timeout-scale must be a positive number");
    }
    let project = Project::load(cli.config.as_deref())?;
    let dag = project.dag()?;
    let selected = select(
        &dag,
        &Selection {
            patterns: patterns.to_vec(),
            tags: args.tags.clone(),
            exclude_tags: args.exclude_tags.clone(),
            tests_only,
        },
    )?;
    if selected.is_empty() {
        println!(
            "nothing to run: no {} matches",
            if tests_only { "test" } else { "task" }
        );
        return Ok(ExitCode::from(0));
    }
    let cache = project.cache()?;
    let mut options = options_for(cli, &project, args);

    if args.dry_run {
        return dry_run(&selected, &cache, &options);
    }

    let state_dir = project.state_dir();
    let run_dir = RunDir::create(&state_dir)
        .with_context(|| format!("cannot create a run directory in {}", state_dir.display()))?;
    options.log_dir = Some(run_dir.logs());
    options.artifact_dir = Some(run_dir.path.clone());

    let github = github::active();
    let mode = args.output.unwrap_or(if cli.quiet {
        OutputMode::Failures
    } else if github {
        OutputMode::Grouped
    } else {
        OutputMode::Stream
    });
    let style = output::Style {
        color: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        mode,
        verbose: cli.verbose,
        github,
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

    let meta = RunMeta {
        id: run_dir.id.clone(),
        started_at,
        duration: elapsed,
        interrupted,
        root: project.root().to_path_buf(),
    };
    let report_json = Report::new(&report, &selected, &meta).to_json();
    std::fs::write(run_dir.path.join("report.json"), &report_json)
        .with_context(|| format!("cannot write {}", run_dir.path.display()))?;
    if let Some(path) = &args.json {
        std::fs::write(path, &report_json)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    if let Some(path) = &args.junit {
        std::fs::write(path, junit::render(&report, &selected, &meta))
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    run_dir.mark_latest();
    RunDir::prune(&state_dir, project.config.settings.keep_runs);

    output::summary(
        &report,
        &selected,
        elapsed,
        Some(&run_dir.path),
        project.root(),
        style,
        args.tail,
    );
    if github {
        github::write_summary(
            &report,
            &format!("neomake: {}", project.config_path.display()),
        );
    }
    Ok(if interrupted {
        ExitCode::from(130)
    } else if report.had_failure() {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    })
}

fn dry_run(
    selected: &[neomake_core::Task],
    cache: &Cache,
    options: &RunOptions,
) -> Result<ExitCode> {
    let plan = executor::plan(selected, cache, options)?;
    let width = plan.iter().map(|p| p.name.len()).max().unwrap_or(0);
    for entry in plan {
        let task = selected
            .iter()
            .find(|t| t.name == entry.name)
            .expect("planned tasks are selected");
        let what = if !task.steps.is_empty() {
            let n = task
                .steps
                .iter()
                .filter(|s| !matches!(s.action, neomake_core::steps::Action::Section(_)))
                .count();
            format!("{n} step{}", if n == 1 { "" } else { "s" })
        } else {
            neomake_core::steps::summarize(&task.command)
        };
        let (verdict, why) = match &entry.planned {
            Planned::Run => ("would run ", what),
            Planned::UpToDate => ("up to date", String::new()),
            Planned::Skip(reason) => ("skip      ", reason.describe()),
        };
        println!("{verdict}  {:width$}  {why}", entry.name);
    }
    Ok(ExitCode::from(0))
}

fn list(cli: &Cli, tests_only: bool) -> Result<ExitCode> {
    let project = Project::load(cli.config.as_deref())?;
    let dag = project.dag()?;
    let tasks: Vec<_> = dag
        .topo_order()
        .iter()
        .filter_map(|n| project.config.tasks.get(n))
        .filter(|t| !tests_only || t.is_test())
        .collect();
    println!(
        "{} in `{}` (in the order they run):",
        if tests_only {
            "tests"
        } else {
            "tasks and tests"
        },
        project.config_path.display()
    );
    let width = tasks.iter().map(|t| t.name.len()).max().unwrap_or(0);
    for t in tasks {
        let kind = if t.is_test() { "test" } else { "task" };
        let mut line = format!("  {kind}  {:width$}", t.name);
        if let Some(d) = &t.description {
            line.push_str(&format!("  {d}"));
        }
        if !t.tags.is_empty() {
            line.push_str(&format!("  [{}]", t.tags.join(", ")));
        }
        let deps = dag.deps_of(&t.name);
        if !deps.is_empty() {
            line.push_str(&format!("  (deps: {})", deps.join(", ")));
        }
        println!("{}", line.trim_end());
        if cli.verbose {
            if let Some(loc) = &t.source {
                let file = loc.file.strip_prefix(project.root()).unwrap_or(&loc.file);
                println!("          defined at {}:{}", file.display(), loc.line);
            }
            if let Some(reason) = &t.skip {
                println!("          skipped: {reason}");
            } else if let Some(reason) = t.requires.unmet_in(project.root(), &t.env) {
                println!("          requires, and is missing: {reason}");
            } else if !t.requires.is_empty() {
                println!("          requirements met");
            }
        }
    }
    if !tests_only && !project.config.services.is_empty() {
        println!("services:");
        for service in project.config.services.values() {
            let what = match &service.kind {
                neomake_core::services::ServiceKind::Serve { dir, listeners } => format!(
                    "serves {dir} on {}",
                    listeners
                        .iter()
                        .map(|l| l.addr.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                neomake_core::services::ServiceKind::Command { command, .. } => {
                    neomake_core::steps::summarize(command)
                }
            };
            let about = service
                .description
                .as_ref()
                .map(|d| format!("  {d}"))
                .unwrap_or_default();
            println!("  {}  {what}{about}", service.name);
        }
    }
    Ok(ExitCode::from(0))
}

fn check(cli: &Cli) -> Result<ExitCode> {
    let project = Project::load(cli.config.as_deref())?;
    project.dag()?;
    let problems = project.config.problems();
    for p in &problems {
        println!("{p}");
    }
    if problems.is_empty() {
        let tasks = project.config.tasks.tasks();
        let tests = tasks.iter().filter(|t| t.is_test()).count();
        println!(
            "ok: {} tasks and {} tests in {} file{}",
            tasks.len() - tests,
            tests,
            project.files,
            if project.files == 1 { "" } else { "s" }
        );
        Ok(ExitCode::from(0))
    } else {
        Ok(ExitCode::from(1))
    }
}

fn log(cli: &Cli, task: Option<&str>, run: Option<&str>) -> Result<ExitCode> {
    let project = Project::load(cli.config.as_deref())?;
    let state_dir = project.state_dir();
    let run_dir = match run {
        Some(id) => state_dir.join("runs").join(id),
        None => RunDir::latest(&state_dir).ok_or_else(|| anyhow!("no run has been logged yet"))?,
    };
    if !run_dir.is_dir() {
        bail!("there is no run {}", run_dir.display());
    }
    let logs = run_dir.join("logs");
    match task {
        Some(name) => {
            let path = logs.join(format!("{}.log", neomake_core::rundir::file_name_for(name)));
            let text = std::fs::read_to_string(&path).map_err(|_| {
                anyhow!(
                    "`{name}` left no log in {} (it may not have run)",
                    run_dir.display()
                )
            })?;
            io::stdout().lock().write_all(text.as_bytes())?;
        }
        None => {
            println!("logs of run {}:", run_dir.display());
            let mut names: Vec<String> = std::fs::read_dir(&logs)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|e| e.file_name().to_str().map(str::to_string))
                        .filter_map(|n| n.strip_suffix(".log").map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            for n in names {
                println!("  {n}");
            }
        }
    }
    Ok(ExitCode::from(0))
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
