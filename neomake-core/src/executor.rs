//! DAG-aware, tokio-based task executor.
//!
//! # Concurrency model
//!
//! Each task in the [`Dag`] gets its own async task. A single
//! [`tokio::sync::watch`] channel is created per task; the sender is
//! given to the task's own future, and a clone of the receiver is given
//! to each of its dependents. A task starts by awaiting every
//! dependency's channel to produce an outcome, and runs only if all of
//! them succeeded (or were cached).
//!
//! Before its command runs, a task takes, in order: the named locks it
//! lists under `exclusive` (sorted, so two tasks can never wait on each
//! other), then a slot of the [`Semaphore`] that caps how many commands
//! run at once. An interactive task takes every slot, so it runs alone.
//! Cache hits take neither.
//!
//! Hashing inputs and outputs reads files, which is done on tokio's
//! blocking pool rather than on the threads that drive the run.
//!
//! # Stopping
//!
//! [`RunOptions::cancel`] stops the run: commands that are running are
//! stopped (see [`crate::process`]) and become `Cancelled`, and tasks not
//! yet started are skipped. `fail_fast` cancels it at the first failure.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::cache::Cache;
use crate::dag::Dag;
use crate::error::ExecError;
use crate::machine::{MachineEnv, MachineRunner};
use crate::process::{self, Invocation, Stream};
use crate::rundir;
use crate::services::{Service, ServiceEvent, Services};
use crate::shell;
use crate::steps::host::{self, Ending, HostEnv, Runner};
use crate::steps::vars::Vars;
use crate::steps::StepOutcome;
use crate::task::Task;

/// Why a task did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// A dependency did not succeed.
    Upstream {
        /// The dependency.
        task: String,
        /// Whether it failed (rather than being skipped itself).
        failed: bool,
    },
    /// Its requirements are not met on this host.
    Unmet(String),
    /// Its configuration says to skip it.
    Disabled(String),
    /// The run was stopped before it started.
    Stopped,
}

impl SkipReason {
    /// The reason in words.
    pub fn describe(&self) -> String {
        match self {
            SkipReason::Upstream { task, failed: true } => format!("upstream `{task}` failed"),
            SkipReason::Upstream {
                task,
                failed: false,
            } => format!("upstream `{task}` did not run"),
            SkipReason::Unmet(why) => format!("requires: {why}"),
            SkipReason::Disabled(why) => why.clone(),
            SkipReason::Stopped => "the run was stopped".into(),
        }
    }
}

/// Status of a single task after the run completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Task produced a fresh, successful result.
    Success,
    /// Task was skipped because the cache already held a valid result.
    Cached,
    /// Task's command returned a non-zero exit status or failed to start.
    Failed,
    /// Task's command was stopped because its timeout passed.
    TimedOut,
    /// Task's command was stopped because the run was cancelled.
    Cancelled,
    /// Task did not run.
    Skipped(SkipReason),
}

impl TaskStatus {
    /// Returns `true` if the status keeps dependents from running.
    pub fn is_blocking_failure(&self) -> bool {
        !matches!(self, Self::Success | Self::Cached)
    }

    /// Returns `true` for a task that ran and did not succeed.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed | Self::TimedOut)
    }

    /// A one-word name: `success`, `cached`, `failed`, `timed_out`,
    /// `cancelled` or `skipped`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Cached => "cached",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Skipped(_) => "skipped",
        }
    }
}

/// The outcome of a single task after the executor finishes it.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    /// Task name.
    pub name: String,
    /// Terminal status.
    pub status: TaskStatus,
    /// Cache key derived for this task (hex SHA-256).
    pub cache_key: Option<String>,
    /// Exit status of the last attempt, when the task ran (128 plus the
    /// signal number for a command killed by a signal).
    pub exit_code: Option<i32>,
    /// Time spent running the command, over every attempt; for cache hits
    /// the time spent checking the cache.
    pub duration: Duration,
    /// Human-readable error message, if any.
    pub error: Option<String>,
    /// How many times the command was started.
    pub attempts: u32,
    /// When the first attempt started.
    pub started_at: Option<SystemTime>,
    /// The task's log file, if one was written.
    pub log: Option<PathBuf>,
    /// The last lines the task printed, kept for failed tasks.
    pub tail: Vec<String>,
    /// The task failed, but `allow_failure` makes that not count.
    pub allowed_failure: bool,
    /// A test's steps, as they went.
    pub steps: Vec<StepOutcome>,
    /// The directory a test's files went to, if it has one.
    pub test_dir: Option<PathBuf>,
}

impl TaskOutcome {
    fn new(name: &str, status: TaskStatus) -> Self {
        Self {
            name: name.to_string(),
            status,
            cache_key: None,
            exit_code: None,
            duration: Duration::ZERO,
            error: None,
            attempts: 0,
            started_at: None,
            log: None,
            tail: Vec::new(),
            allowed_failure: false,
            steps: Vec::new(),
            test_dir: None,
        }
    }

    fn skipped(name: &str, reason: SkipReason) -> Self {
        let mut o = Self::new(name, TaskStatus::Skipped(reason.clone()));
        o.error = Some(reason.describe());
        o
    }
}

/// Aggregate report returned at the end of an executor run.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// Per-task outcomes in dependency order.
    pub outcomes: Vec<TaskOutcome>,
}

impl RunReport {
    /// `true` if a task failed or timed out, other than those allowed to.
    pub fn had_failure(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| o.status.is_failure() && !o.allowed_failure)
    }

    /// Look up the outcome for a specific task.
    pub fn get(&self, name: &str) -> Option<&TaskOutcome> {
        self.outcomes.iter().find(|o| o.name == name)
    }

    /// How many outcomes have each status label.
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for o in &self.outcomes {
            *counts.entry(o.status.label()).or_insert(0) += 1;
        }
        counts
    }
}

/// Events emitted by the executor while a run is in progress.
#[derive(Debug, Clone)]
pub enum ExecutionEvent {
    /// The task's command is starting.
    Started {
        /// Task name.
        task: String,
        /// Which attempt this is, from 1.
        attempt: u32,
    },
    /// Task resulted in a cache hit and will not run its command.
    Cached {
        /// Task name.
        task: String,
        /// Cache key (hex).
        key: String,
    },
    /// Task finished successfully.
    Finished {
        /// Task name.
        task: String,
        /// Time spent running.
        duration: Duration,
        /// How many attempts it took.
        attempts: u32,
    },
    /// An attempt failed (non-zero exit, timeout or failure to start).
    Failed {
        /// Task name.
        task: String,
        /// Exit status when one was produced.
        exit_code: Option<i32>,
        /// Human-readable error message.
        message: String,
        /// The attempt ran out of time.
        timed_out: bool,
        /// Another attempt follows.
        retrying: bool,
        /// The failure does not count (`allow_failure`).
        allowed: bool,
    },
    /// The task's command was stopped because the run was cancelled.
    Cancelled {
        /// Task name.
        task: String,
    },
    /// The task did not run.
    Skipped {
        /// Task name.
        task: String,
        /// Why.
        reason: SkipReason,
    },
    /// A service is ready.
    ServiceStarted {
        /// The service.
        name: String,
        /// Where it listens, or what it runs.
        detail: String,
    },
    /// A service could not start.
    ServiceFailed {
        /// The service.
        name: String,
        /// Why.
        message: String,
    },
    /// A service was stopped.
    ServiceStopped {
        /// The service.
        name: String,
    },
    /// A test started a new section.
    Section {
        /// Task name.
        task: String,
        /// The section's title.
        title: String,
    },
    /// A test's step ended.
    Step {
        /// Task name.
        task: String,
        /// Its position, from 1.
        index: usize,
        /// How many steps the test has.
        total: usize,
        /// What it does.
        label: String,
        /// Whether its checks held.
        passed: bool,
        /// How long it took.
        duration: Duration,
    },
    /// A single line of the task's stdout.
    Stdout {
        /// Task name.
        task: String,
        /// Captured line (without the trailing newline).
        line: String,
    },
    /// A single line of the task's stderr.
    Stderr {
        /// Task name.
        task: String,
        /// Captured line (without the trailing newline).
        line: String,
    },
}

/// Callback invoked for every [`ExecutionEvent`] emitted by the executor.
pub type Reporter = Arc<dyn Fn(ExecutionEvent) + Send + Sync>;

/// Configuration knobs for [`run`].
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Maximum number of tasks that may run their command concurrently.
    pub concurrency: usize,
    /// Project root — commands run here unless their `cwd` says otherwise.
    pub project_root: PathBuf,
    /// When `false`, every task is treated as a cache miss.
    pub use_cache: bool,
    /// Stop the run at the first failure.
    pub fail_fast: bool,
    /// Every timeout is multiplied by this (for slow or fast hosts).
    pub timeout_scale: f64,
    /// The timeout of tasks that set none.
    pub default_timeout: Option<Duration>,
    /// The shell of tasks that set none (else the platform's).
    pub default_shell: Option<Vec<String>>,
    /// Where each task's log is written, if anywhere.
    pub log_dir: Option<PathBuf>,
    /// Cancelling this stops the run.
    pub cancel: CancellationToken,
    /// How many of a failed task's last lines its outcome keeps.
    pub tail_lines: usize,
    /// The run's directory; tests keep their files under `tests/` in it.
    pub artifact_dir: Option<PathBuf>,
    /// The services tasks may use, by name.
    pub services: BTreeMap<String, Service>,
    /// What runs machine tests (none: they fail).
    pub machines: Option<Arc<dyn MachineRunner>>,
    /// How many machine tests may run at once.
    pub max_machines: usize,
}

impl RunOptions {
    /// Construct sensible defaults from a project root and concurrency.
    pub fn new(project_root: impl Into<PathBuf>, concurrency: usize) -> Self {
        Self {
            project_root: project_root.into(),
            concurrency: concurrency.max(1),
            use_cache: true,
            fail_fast: false,
            timeout_scale: 1.0,
            default_timeout: None,
            default_shell: None,
            log_dir: None,
            cancel: CancellationToken::new(),
            tail_lines: 40,
            artifact_dir: None,
            services: BTreeMap::new(),
            machines: None,
            max_machines: 2,
        }
    }
}

/// Compute the transitive dependency closure of `targets` within `dag`.
///
/// If `targets` is empty, returns every task.
pub fn select_tasks(dag: &Dag, targets: &[String]) -> Result<Vec<Task>, ExecError> {
    if targets.is_empty() {
        return Ok(dag.tasks().to_vec());
    }
    let all: BTreeMap<&str, &Task> = dag.tasks().iter().map(|t| (t.name.as_str(), t)).collect();

    for t in targets {
        if !all.contains_key(t.as_str()) {
            let suggestion = closest(t, all.keys().copied());
            return Err(ExecError::UnknownTask {
                name: t.clone(),
                suggestion,
            });
        }
    }

    let mut selected: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = targets.to_vec();
    while let Some(n) = stack.pop() {
        if !selected.insert(n.clone()) {
            continue;
        }
        for d in dag.deps_of(&n) {
            stack.push(d.clone());
        }
    }
    Ok(dag
        .tasks()
        .iter()
        .filter(|t| selected.contains(&t.name))
        .cloned()
        .collect())
}

/// What a run would do with a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Planned {
    /// Run it.
    Run,
    /// Nothing: the cache holds its result.
    UpToDate,
    /// Skip it.
    Skip(SkipReason),
}

/// One task of a [`plan`].
#[derive(Debug, Clone)]
pub struct PlanEntry {
    /// The task.
    pub name: String,
    /// What would happen to it.
    pub planned: Planned,
}

/// What [`run`] would do with `tasks`, without running anything. A task
/// is up to date only when the cache holds its result and everything it
/// depends on is up to date too.
pub fn plan(
    tasks: &[Task],
    cache: &Cache,
    options: &RunOptions,
) -> Result<Vec<PlanEntry>, ExecError> {
    let dag = Dag::build(tasks).map_err(|e| ExecError::Plan(e.to_string()))?;
    let mut keys: HashMap<String, String> = HashMap::new();
    let mut planned: HashMap<String, Planned> = HashMap::new();
    let mut out = Vec::new();
    for name in dag.topo_order() {
        let task = dag
            .tasks()
            .iter()
            .find(|t| &t.name == name)
            .expect("in the DAG");
        let deps = dag.deps_of(name);
        let upstream_skip = deps
            .iter()
            .find(|d| matches!(planned.get(*d), Some(Planned::Skip(_))));
        let p = if let Some(dep) = upstream_skip {
            Planned::Skip(SkipReason::Upstream {
                task: dep.clone(),
                failed: false,
            })
        } else if let Some(reason) = &task.skip {
            Planned::Skip(SkipReason::Disabled(reason.clone()))
        } else if let Some(reason) = task.requires.unmet_in(&options.project_root, &task.env) {
            Planned::Skip(SkipReason::Unmet(reason))
        } else if let Some(reason) = task
            .machine
            .as_ref()
            .zip(options.machines.as_ref())
            .and_then(|(machine, runner)| runner.unmet(machine))
        {
            Planned::Skip(SkipReason::Unmet(reason))
        } else {
            let dep_keys: Vec<String> = deps.iter().filter_map(|d| keys.get(d).cloned()).collect();
            let key = cache
                .compute_key(task, &dep_keys)
                .map_err(|e| ExecError::Plan(format!("`{name}`: {e}")))?;
            let deps_up_to_date = deps
                .iter()
                .all(|d| planned.get(d) == Some(&Planned::UpToDate));
            let hit = options.use_cache
                && task.cache
                && deps_up_to_date
                && matches!(cache.lookup(&key), Ok(Some(_)));
            keys.insert(name.clone(), key);
            if hit {
                Planned::UpToDate
            } else {
                Planned::Run
            }
        };
        planned.insert(name.clone(), p.clone());
        out.push(PlanEntry {
            name: name.clone(),
            planned: p,
        });
    }
    Ok(out)
}

/// The name in `names` closest to `wanted`, if one is close enough to be
/// a likely misspelling.
pub fn closest<'a>(wanted: &str, names: impl Iterator<Item = &'a str>) -> Option<String> {
    names
        .map(|n| (edit_distance(wanted, n), n))
        .filter(|(d, n)| *d <= (n.len().max(wanted.len()) / 3).max(2))
        .min()
        .map(|(_, n)| n.to_string())
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Shared state of one run.
struct Context {
    cache: Cache,
    opts: RunOptions,
    reporter: Reporter,
    sem: Arc<Semaphore>,
    locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    services: Services,
    machine_slots: Arc<Semaphore>,
}

/// Run `tasks` in parallel honoring their dependency order.
pub async fn run(
    tasks: &[Task],
    cache: &Cache,
    options: &RunOptions,
    reporter: Option<Reporter>,
) -> Result<RunReport, ExecError> {
    // Sub-DAG just for `tasks` (so `deps_of` only includes selected tasks).
    let dag = Dag::build(tasks).map_err(|e| ExecError::Plan(e.to_string()))?;

    let locks = tasks
        .iter()
        .flat_map(|t| t.exclusive.iter())
        .map(|name| (name.clone(), Arc::new(tokio::sync::Mutex::new(()))))
        .collect();
    let reporter = reporter.unwrap_or_else(|| Arc::new(|_| {}));
    let mut users: BTreeMap<String, usize> = BTreeMap::new();
    for t in tasks {
        for name in &t.services {
            *users.entry(name.clone()).or_insert(0) += 1;
        }
    }
    let service_reporter = Arc::clone(&reporter);
    let services = Services::new(
        &options.services,
        &users,
        &options.project_root,
        options.log_dir.as_deref(),
        default_shell(options),
        Arc::new(move |event| {
            service_reporter(match event {
                ServiceEvent::Started { name, detail } => {
                    ExecutionEvent::ServiceStarted { name, detail }
                }
                ServiceEvent::Failed { name, message } => {
                    ExecutionEvent::ServiceFailed { name, message }
                }
                ServiceEvent::Stopped { name } => ExecutionEvent::ServiceStopped { name },
            })
        }),
    );
    let ctx = Arc::new(Context {
        cache: cache.clone(),
        opts: RunOptions {
            concurrency: options.concurrency.max(1),
            ..options.clone()
        },
        reporter,
        sem: Arc::new(Semaphore::new(options.concurrency.max(1))),
        locks,
        services,
        machine_slots: Arc::new(Semaphore::new(options.max_machines.max(1))),
    });
    if let Some(dir) = &ctx.opts.log_dir {
        std::fs::create_dir_all(dir).map_err(|source| ExecError::Io {
            task: "<logs>".into(),
            source,
        })?;
    }

    let mut senders: HashMap<String, watch::Sender<Option<TaskOutcome>>> = HashMap::new();
    let mut receivers: HashMap<String, watch::Receiver<Option<TaskOutcome>>> = HashMap::new();
    for name in dag.topo_order() {
        let (tx, rx) = watch::channel(None);
        senders.insert(name.clone(), tx);
        receivers.insert(name.clone(), rx);
    }

    let mut join_set: JoinSet<TaskOutcome> = JoinSet::new();
    for task in dag.tasks().iter().cloned() {
        let dep_receivers: Vec<watch::Receiver<Option<TaskOutcome>>> = dag
            .deps_of(&task.name)
            .iter()
            .map(|d| receivers[d].clone())
            .collect();
        let my_sender = senders.remove(&task.name).expect("sender prepared");
        let ctx = Arc::clone(&ctx);
        join_set.spawn(async move {
            let services = task.services.clone();
            let outcome = run_single(task, dep_receivers, &ctx).await;
            for name in &services {
                ctx.services.release(name).await;
            }
            if ctx.opts.fail_fast && outcome.status.is_failure() && !outcome.allowed_failure {
                ctx.opts.cancel.cancel();
            }
            let _ = my_sender.send(Some(outcome.clone()));
            outcome
        });
    }

    let mut outcomes: HashMap<String, TaskOutcome> = HashMap::new();
    while let Some(joined) = join_set.join_next().await {
        let outcome = joined.map_err(|e| ExecError::Plan(format!("a task panicked: {e}")))?;
        outcomes.insert(outcome.name.clone(), outcome);
    }
    ctx.services.stop_all().await;
    if let Err(e) = ctx.cache.flush() {
        tracing::warn!(error = %e, "could not save the remembered input hashes");
    }

    let ordered = dag
        .topo_order()
        .iter()
        .filter_map(|n| outcomes.remove(n))
        .collect();
    Ok(RunReport { outcomes: ordered })
}

async fn run_single(
    task: Task,
    mut dep_receivers: Vec<watch::Receiver<Option<TaskOutcome>>>,
    ctx: &Context,
) -> TaskOutcome {
    let reporter = &ctx.reporter;
    let skip = |reason: SkipReason| {
        reporter(ExecutionEvent::Skipped {
            task: task.name.clone(),
            reason: reason.clone(),
        });
        TaskOutcome::skipped(&task.name, reason)
    };

    // Wait for every dep, collecting their cache keys for our own.
    let mut dep_keys: Vec<String> = Vec::with_capacity(dep_receivers.len());
    for rx in dep_receivers.iter_mut() {
        let outcome = match rx.wait_for(Option::is_some).await {
            Ok(v) => v.clone().expect("waited for Some"),
            Err(_) => {
                return skip(SkipReason::Upstream {
                    task: "<unknown>".into(),
                    failed: true,
                })
            }
        };
        if outcome.status.is_blocking_failure() {
            return skip(SkipReason::Upstream {
                failed: outcome.status.is_failure() || outcome.status == TaskStatus::Cancelled,
                task: outcome.name,
            });
        }
        if let Some(k) = outcome.cache_key {
            dep_keys.push(k);
        }
    }

    if ctx.opts.cancel.is_cancelled() {
        return skip(SkipReason::Stopped);
    }
    if let Some(reason) = &task.skip {
        return skip(SkipReason::Disabled(reason.clone()));
    }
    if let Some(reason) = task.requires.unmet_in(&ctx.opts.project_root, &task.env) {
        return skip(SkipReason::Unmet(reason));
    }
    if let Some(machine) = &task.machine {
        match &ctx.opts.machines {
            None => {
                return fail_early(
                    &task,
                    ctx,
                    "machine tests cannot run here: no machine support".into(),
                )
            }
            Some(runner) => {
                if let Some(reason) = runner.unmet(machine) {
                    return skip(SkipReason::Unmet(reason));
                }
            }
        }
    }

    let checking = Instant::now();
    let cache_key = {
        let cache = ctx.cache.clone();
        let t = task.clone();
        match tokio::task::spawn_blocking(move || cache.compute_key(&t, &dep_keys)).await {
            Ok(Ok(k)) => k,
            Ok(Err(e)) => {
                return fail_early(&task, ctx, format!("cache key derivation failed: {e}"))
            }
            Err(e) => return fail_early(&task, ctx, format!("cache key derivation panicked: {e}")),
        }
    };

    let caching = ctx.opts.use_cache && task.cache;
    if caching {
        let cache = ctx.cache.clone();
        let key = cache_key.clone();
        match tokio::task::spawn_blocking(move || cache.lookup(&key)).await {
            Ok(Ok(Some(_))) => {
                reporter(ExecutionEvent::Cached {
                    task: task.name.clone(),
                    key: cache_key.clone(),
                });
                let mut o = TaskOutcome::new(&task.name, TaskStatus::Cached);
                o.cache_key = Some(cache_key);
                o.exit_code = Some(0);
                o.duration = checking.elapsed();
                return o;
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                tracing::warn!(task = %task.name, error = %e, "cache lookup failed; treating as miss");
            }
            Err(e) => {
                tracing::warn!(task = %task.name, error = %e, "cache lookup panicked; treating as miss");
            }
        }
    }

    // Services it uses, started if they are not running yet.
    let mut service_vars = Vec::new();
    for name in &task.services {
        match ctx.services.acquire(name, &ctx.opts.cancel).await {
            Ok(vars) => service_vars.extend(vars),
            Err(_) if ctx.opts.cancel.is_cancelled() => return skip(SkipReason::Stopped),
            Err(message) => return fail_early(&task, ctx, message),
        }
    }

    // Locks, then a concurrency slot (every slot, for an interactive task).
    let mut names: Vec<&String> = task.exclusive.iter().collect();
    names.sort();
    names.dedup();
    let mut _guards = Vec::with_capacity(names.len());
    for name in names {
        let lock = Arc::clone(&ctx.locks[name]);
        tokio::select! {
            guard = lock.lock_owned() => _guards.push(guard),
            _ = ctx.opts.cancel.cancelled() => return skip(SkipReason::Stopped),
        }
    }
    let slots = if task.interactive {
        ctx.opts.concurrency as u32
    } else {
        1
    };
    let _machine_permit = if task.machine.is_some() {
        tokio::select! {
            permit = Arc::clone(&ctx.machine_slots).acquire_owned() => match permit {
                Ok(p) => Some(p),
                Err(_) => return fail_early(&task, ctx, "the scheduler shut down".into()),
            },
            _ = ctx.opts.cancel.cancelled() => return skip(SkipReason::Stopped),
        }
    } else {
        None
    };
    let _permit = tokio::select! {
        permit = Arc::clone(&ctx.sem).acquire_many_owned(slots) => match permit {
            Ok(p) => p,
            Err(_) => return fail_early(&task, ctx, "the scheduler shut down".into()),
        },
        _ = ctx.opts.cancel.cancelled() => return skip(SkipReason::Stopped),
    };

    let mut outcome = execute(&task, ctx, &service_vars).await;
    outcome.cache_key = Some(cache_key.clone());

    if outcome.status == TaskStatus::Success && caching {
        let cache = ctx.cache.clone();
        let t = task.clone();
        let key = cache_key;
        match tokio::task::spawn_blocking(move || cache.store(&t, &key, 0)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(task = %task.name, error = %e, "cache store failed"),
            Err(e) => tracing::warn!(task = %task.name, error = %e, "cache store panicked"),
        }
    }
    outcome
}

fn fail_early(task: &Task, ctx: &Context, message: String) -> TaskOutcome {
    (ctx.reporter)(ExecutionEvent::Failed {
        task: task.name.clone(),
        exit_code: None,
        message: message.clone(),
        timed_out: false,
        retrying: false,
        allowed: task.allow_failure,
    });
    let mut o = TaskOutcome::new(&task.name, TaskStatus::Failed);
    o.error = Some(message);
    o.allowed_failure = task.allow_failure;
    o
}

/// Where a task's output goes: its log, the tail kept for its outcome,
/// and the reporter.
pub struct Sink {
    task: String,
    log: Option<Mutex<std::io::BufWriter<std::fs::File>>>,
    tail: Mutex<VecDeque<String>>,
    tail_lines: usize,
    reporter: Reporter,
}

impl Sink {
    fn new(task: &Task, log: Option<&Path>, ctx: &Context) -> Self {
        Self {
            task: task.name.clone(),
            log: log
                .and_then(|p| std::fs::File::create(p).ok())
                .map(|f| Mutex::new(std::io::BufWriter::new(f))),
            tail: Mutex::new(VecDeque::with_capacity(ctx.opts.tail_lines)),
            tail_lines: ctx.opts.tail_lines,
            reporter: Arc::clone(&ctx.reporter),
        }
    }

    /// The task's name.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// A line for the log only.
    pub fn note(&self, line: String) {
        if let Some(log) = &self.log {
            let mut w = log.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writeln!(w, "{line}");
        }
    }

    /// A line the task printed.
    pub fn line(&self, stream: Stream, line: String) {
        self.note(line.clone());
        if self.tail_lines > 0 {
            let mut t = self.tail.lock().unwrap_or_else(|e| e.into_inner());
            if t.len() == self.tail_lines {
                t.pop_front();
            }
            t.push_back(line.clone());
        }
        let task = self.task.clone();
        (self.reporter)(match stream {
            Stream::Stdout => ExecutionEvent::Stdout { task, line },
            Stream::Stderr => ExecutionEvent::Stderr { task, line },
        });
    }

    /// An event for the reporter.
    pub fn event(&self, event: ExecutionEvent) {
        (self.reporter)(event);
    }

    fn take_tail(&self) -> Vec<String> {
        self.tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    fn flush(&self) {
        if let Some(log) = &self.log {
            let _ = log.lock().unwrap_or_else(|e| e.into_inner()).flush();
        }
    }
}

/// How one attempt went.
struct Attempt {
    status: TaskStatus,
    exit_code: Option<i32>,
    message: Option<String>,
    steps: Vec<StepOutcome>,
}

/// Run the task, as many times as its retries allow.
async fn execute(task: &Task, ctx: &Context, service_vars: &[(String, String)]) -> TaskOutcome {
    let log_path = ctx
        .opts
        .log_dir
        .as_ref()
        .map(|dir| dir.join(format!("{}.log", rundir::file_name_for(&task.name))));
    let sink = Sink::new(task, log_path.as_deref(), ctx);
    let test_dir = if task.is_test() {
        ctx.opts.artifact_dir.as_ref().and_then(|dir| {
            let d = dir.join("tests").join(rundir::file_name_for(&task.name));
            std::fs::create_dir_all(&d).ok().map(|_| d)
        })
    } else {
        None
    };

    let timeout = task
        .timeout
        .or(ctx.opts.default_timeout)
        .map(|t| t.mul_f64(ctx.opts.timeout_scale.max(0.0)));
    let started_at = SystemTime::now();
    let started = Instant::now();
    let max_attempts = task.retries + 1;
    let mut outcome = TaskOutcome::new(&task.name, TaskStatus::Failed);
    outcome.started_at = Some(started_at);
    outcome.log = log_path.clone();
    outcome.allowed_failure = task.allow_failure;
    outcome.test_dir = test_dir.clone();

    for attempt in 1..=max_attempts {
        outcome.attempts = attempt;
        if attempt > 1 {
            sink.note(format!("--- attempt {attempt} of {max_attempts} ---"));
        }
        sink.event(ExecutionEvent::Started {
            task: task.name.clone(),
            attempt,
        });

        let mut vars = base_vars(task, &ctx.opts, test_dir.as_deref());
        for (name, value) in service_vars {
            vars.set(name.clone(), value.clone());
        }
        let result = if task.steps.is_empty() {
            command_attempt(task, ctx, &sink, timeout, &vars).await
        } else {
            steps_attempt(task, ctx, &sink, timeout, test_dir.as_deref(), vars).await
        };
        outcome.status = result.status.clone();
        outcome.exit_code = result.exit_code;
        outcome.error = result.message.clone();
        outcome.steps = result.steps;

        match &result.status {
            TaskStatus::Success => break,
            TaskStatus::Cancelled => {
                sink.note("--- stopped: the run was cancelled ---".into());
                sink.event(ExecutionEvent::Cancelled {
                    task: task.name.clone(),
                });
                break;
            }
            _ => {}
        }
        let retrying = attempt < max_attempts && !ctx.opts.cancel.is_cancelled();
        let message = result.message.unwrap_or_else(|| "failed".into());
        sink.note(format!("--- {message} ---"));
        sink.event(ExecutionEvent::Failed {
            task: task.name.clone(),
            exit_code: result.exit_code,
            message,
            timed_out: result.status == TaskStatus::TimedOut,
            retrying,
            allowed: task.allow_failure,
        });
        if !retrying {
            break;
        }
    }

    outcome.duration = started.elapsed();
    sink.flush();
    if outcome.status == TaskStatus::Success {
        sink.event(ExecutionEvent::Finished {
            task: task.name.clone(),
            duration: outcome.duration,
            attempts: outcome.attempts,
        });
    } else {
        outcome.tail = sink.take_tail();
    }
    outcome
}

/// One attempt at a task's command.
async fn command_attempt(
    task: &Task,
    ctx: &Context,
    sink: &Sink,
    timeout: Option<Duration>,
    vars: &Vars,
) -> Attempt {
    let failed = |message: String| Attempt {
        status: TaskStatus::Failed,
        exit_code: None,
        message: Some(message),
        steps: Vec::new(),
    };
    let mut invocation = match invocation_for(task, &ctx.opts) {
        Ok(inv) => inv,
        Err(message) => return failed(message),
    };
    // A command's placeholders: services' addresses and the run's paths.
    if let Some(command) = invocation.args.last_mut() {
        *command = crate::steps::vars::substitute_known(command, &|n| vars.get(n));
    }
    let result = process::run(&invocation, timeout, &ctx.opts.cancel, |stream, line| {
        sink.line(stream, line)
    })
    .await;
    match result {
        Ok(exit) if exit.success() => Attempt {
            status: TaskStatus::Success,
            exit_code: Some(0),
            message: None,
            steps: Vec::new(),
        },
        Ok(exit) if exit.cancelled => Attempt {
            status: TaskStatus::Cancelled,
            exit_code: Some(exit.status()),
            message: Some("stopped: the run was cancelled".into()),
            steps: Vec::new(),
        },
        Ok(exit) if exit.timed_out => Attempt {
            status: TaskStatus::TimedOut,
            exit_code: Some(exit.status()),
            message: Some(format!(
                "timed out after {}",
                crate::duration::human(timeout.unwrap_or_default())
            )),
            steps: Vec::new(),
        },
        Ok(exit) => Attempt {
            status: TaskStatus::Failed,
            exit_code: Some(exit.status()),
            message: Some(exit.describe()),
            steps: Vec::new(),
        },
        Err(e) => failed(format!("could not start `{}`: {e}", invocation.program)),
    }
}

/// One attempt at a test's steps.
async fn steps_attempt(
    task: &Task,
    ctx: &Context,
    sink: &Sink,
    timeout: Option<Duration>,
    test_dir: Option<&Path>,
    vars: Vars,
) -> Attempt {
    let failed = |message: String| Attempt {
        status: TaskStatus::Failed,
        exit_code: None,
        message: Some(message),
        steps: Vec::new(),
    };
    let cwd = match cwd_for(task, &ctx.opts) {
        Ok(cwd) => cwd,
        Err(message) => return failed(message),
    };
    let mut env = task.env.clone();
    if let Some(dir) = test_dir {
        env.insert(
            "NEOMAKE_TEST_DIR".into(),
            absolute(dir).display().to_string(),
        );
    }
    // The prelude's placeholders are filled in like the steps'.
    let prelude = match task.prelude.as_deref().map(|p| vars.apply(p)).transpose() {
        Ok(p) => p,
        Err(message) => return failed(format!("prelude: {message}")),
    };
    let mut host_env = HostEnv {
        shell: shell_argv(task, &ctx.opts),
        cwd,
        env,
        env_clear: task.env_clear,
        prelude,
        deadline: timeout.map(|t| Instant::now() + t),
        cancel: &ctx.opts.cancel,
        vars,
    };
    let file = task.source.as_ref().map(|l| {
        l.file
            .strip_prefix(&ctx.opts.project_root)
            .unwrap_or(&l.file)
            .display()
            .to_string()
    });
    let mut result = if task.machine.is_some() {
        let (Some(runner), Some(dir)) = (&ctx.opts.machines, test_dir) else {
            return failed("a machine test needs machine support and a run directory".into());
        };
        let env = MachineEnv {
            host: host_env,
            dir: absolute(dir),
            root: absolute(&ctx.opts.project_root),
            file: file.clone(),
        };
        runner.run(task, env, sink).await
    } else {
        host::run(&task.steps, file.as_deref(), &mut host_env, sink).await
    };
    if result.ending != Ending::Passed {
        Runner::not_run_after(&mut result, &task.steps);
    }
    Attempt {
        status: match result.ending {
            Ending::Passed => TaskStatus::Success,
            Ending::Failed => TaskStatus::Failed,
            Ending::TimedOut => TaskStatus::TimedOut,
            Ending::Cancelled => TaskStatus::Cancelled,
        },
        exit_code: result.exit_code,
        message: result.message,
        steps: result.steps,
    }
}

/// The variables every test's placeholders can use.
fn base_vars(task: &Task, opts: &RunOptions, test_dir: Option<&Path>) -> Vars {
    let mut vars = Vars::default();
    vars.set("test.name", task.name.clone());
    vars.set(
        "project.root",
        absolute(&opts.project_root).display().to_string(),
    );
    if let Some(dir) = &opts.artifact_dir {
        vars.set("run.dir", absolute(dir).display().to_string());
    }
    if let Some(dir) = test_dir {
        vars.set("test.dir", absolute(dir).display().to_string());
    }
    vars
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let path = path.strip_prefix(".").unwrap_or(path);
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// The shell `task` runs its commands with.
fn shell_argv(task: &Task, opts: &RunOptions) -> Vec<String> {
    match &task.shell {
        Some(argv) => argv.clone(),
        None => default_shell(opts),
    }
}

/// The shell of tasks that set none.
fn default_shell(opts: &RunOptions) -> Vec<String> {
    match &opts.default_shell {
        Some(argv) => argv.clone(),
        None => {
            let s = shell::resolve();
            std::iter::once(s.program).chain(s.args).collect()
        }
    }
}

/// The directory `task` runs in, or why it cannot.
fn cwd_for(task: &Task, opts: &RunOptions) -> Result<PathBuf, String> {
    let cwd = match &task.cwd {
        Some(dir) => opts.project_root.join(dir),
        None => opts.project_root.clone(),
    };
    if cwd.is_dir() {
        Ok(cwd)
    } else {
        Err(format!(
            "the working directory {} does not exist",
            display_rel(&cwd)
        ))
    }
}

/// How `task` is started, or why it cannot be.
fn invocation_for(task: &Task, opts: &RunOptions) -> Result<Invocation, String> {
    let argv = shell_argv(task, opts);
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "the shell is empty".to_string())?;
    let cwd = cwd_for(task, opts)?;
    let mut args = args.to_vec();
    args.push(task.command.clone());
    Ok(Invocation {
        program: program.clone(),
        args,
        cwd,
        env: task.env.clone(),
        env_clear: task.env_clear,
        interactive: task.interactive,
    })
}

fn display_rel(path: &Path) -> String {
    path.strip_prefix("./")
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Cross-platform test command helpers. The default shell is `sh -c`
    // on Unix and PowerShell on Windows, so test commands must be
    // expressed in whichever dialect matches the host.

    #[cfg(windows)]
    fn sleep_cmd(ms: u64) -> String {
        format!("Start-Sleep -Milliseconds {ms}")
    }
    #[cfg(not(windows))]
    fn sleep_cmd(ms: u64) -> String {
        format!("sleep {}", ms as f64 / 1000.0)
    }

    #[cfg(windows)]
    fn fail_cmd() -> String {
        "exit 1".to_string()
    }
    #[cfg(not(windows))]
    fn fail_cmd() -> String {
        "false".to_string()
    }

    /// Write `body` to `path` (relative to the task's cwd) using a
    /// shell-native command. PowerShell 5.1's `>` redirects emit
    /// UTF-16 + BOM; we use `Set-Content -Encoding ascii` instead so
    /// the bytes match what `echo > file` produces under `sh`.
    #[cfg(windows)]
    fn write_cmd(path: &str, body: &str) -> String {
        format!("Set-Content -Encoding ascii -Path '{path}' -Value '{body}'")
    }
    #[cfg(not(windows))]
    fn write_cmd(path: &str, body: &str) -> String {
        format!("echo {body} > {path}")
    }

    fn mk(name: &str, cmd: &str, deps: &[&str]) -> Task {
        Task::new(name, cmd).with_deps(deps)
    }

    fn events_collector() -> (Reporter, Arc<Mutex<Vec<ExecutionEvent>>>) {
        let store: Arc<Mutex<Vec<ExecutionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let store_clone = Arc::clone(&store);
        let r: Reporter = Arc::new(move |e: ExecutionEvent| {
            store_clone.lock().unwrap().push(e);
        });
        (r, store)
    }

    async fn run_in(tmp: &Path, tasks: &[Task], opts: &RunOptions) -> RunReport {
        let cache = Cache::open(tmp).unwrap();
        run(tasks, &cache, opts, None).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn runs_independent_tasks_concurrently() {
        let tmp = tempfile::tempdir().unwrap();
        // PowerShell's per-invocation startup is ~0.5–1s on a fresh CI
        // runner, so the sleep must dominate spawn noise and the
        // threshold is sized as `sleep + spawn_budget`.
        let (sleep_ms, threshold_ms): (u64, u64) = if cfg!(windows) {
            (2000, 4500)
        } else {
            (300, 550)
        };
        let tasks = vec![
            mk("a", &sleep_cmd(sleep_ms), &[]),
            mk("b", &sleep_cmd(sleep_ms), &[]),
        ];
        let opts = RunOptions::new(tmp.path(), 4);
        let started = Instant::now();
        let report = run_in(tmp.path(), &tasks, &opts).await;
        let elapsed = started.elapsed();
        assert!(!report.had_failure());
        assert!(
            elapsed < Duration::from_millis(threshold_ms),
            "elapsed={elapsed:?} — tasks appear to have run serially"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failure_skips_dependents() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = vec![
            mk("a", &fail_cmd(), &[]),
            mk("b", "echo should-not-run", &["a"]),
        ];
        let report = run_in(tmp.path(), &tasks, &RunOptions::new(tmp.path(), 2)).await;
        assert_eq!(report.get("a").unwrap().status, TaskStatus::Failed);
        assert_eq!(
            report.get("b").unwrap().status,
            TaskStatus::Skipped(SkipReason::Upstream {
                task: "a".into(),
                failed: true
            })
        );
        assert!(report.had_failure());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_run_hits_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let mut task = Task::new("t", write_cmd("out.txt", "hi"));
        task.outputs = vec!["out.txt".into()];
        let opts = RunOptions::new(tmp.path(), 1);

        let c = Arc::clone(&counter);
        let reporter: Reporter = Arc::new(move |e| {
            if matches!(e, ExecutionEvent::Finished { .. }) {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        let r1 = run(
            std::slice::from_ref(&task),
            &cache,
            &opts,
            Some(reporter.clone()),
        )
        .await
        .unwrap();
        assert_eq!(r1.get("t").unwrap().status, TaskStatus::Success);

        let (rep2, store) = events_collector();
        let r2 = run(&[task], &cache, &opts, Some(rep2)).await.unwrap();
        assert_eq!(r2.get("t").unwrap().status, TaskStatus::Cached);
        let events = store.lock().unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, ExecutionEvent::Cached { .. })));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tasks_that_do_not_cache_always_run() {
        let tmp = tempfile::tempdir().unwrap();
        let mut task = Task::new("t", write_cmd("out.txt", "hi"));
        task.outputs = vec!["out.txt".into()];
        task.cache = false;
        let opts = RunOptions::new(tmp.path(), 1);
        run_in(tmp.path(), std::slice::from_ref(&task), &opts).await;
        let r2 = run_in(tmp.path(), &[task], &opts).await;
        assert_eq!(r2.get("t").unwrap().status, TaskStatus::Success);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn skips_for_unmet_requirements_and_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let mut needs = mk("needs", "echo x", &[]);
        needs.requires.commands = vec!["no-such-program-x9".into()];
        let after = mk("after", "echo y", &["needs"]);
        let mut off = mk("off", "echo z", &[]);
        off.skip = Some("not on this host".into());
        let report = run_in(
            tmp.path(),
            &[needs, after, off],
            &RunOptions::new(tmp.path(), 2),
        )
        .await;
        assert_eq!(
            report.get("needs").unwrap().status,
            TaskStatus::Skipped(SkipReason::Unmet("no-such-program-x9 not found".into()))
        );
        assert_eq!(
            report.get("after").unwrap().status,
            TaskStatus::Skipped(SkipReason::Upstream {
                task: "needs".into(),
                failed: false
            })
        );
        assert_eq!(
            report.get("off").unwrap().status,
            TaskStatus::Skipped(SkipReason::Disabled("not on this host".into()))
        );
        assert!(!report.had_failure(), "skipping is not failing");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeouts_retries_and_allowed_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let mut slow = mk("slow", "sleep 30", &[]);
        slow.timeout = Some(Duration::from_millis(200));
        // Fails twice, then succeeds.
        let mut flaky = mk(
            "flaky",
            "n=$(cat count 2>/dev/null || echo 0); echo $((n+1)) > count; [ $n -ge 2 ]",
            &[],
        );
        flaky.retries = 2;
        let mut allowed = mk("allowed", "exit 4", &[]);
        allowed.allow_failure = true;
        let report = run_in(
            tmp.path(),
            &[slow, flaky, allowed],
            &RunOptions::new(tmp.path(), 3),
        )
        .await;
        let slow = report.get("slow").unwrap();
        assert_eq!(slow.status, TaskStatus::TimedOut);
        assert!(slow.duration < Duration::from_secs(5));
        let flaky = report.get("flaky").unwrap();
        assert_eq!(flaky.status, TaskStatus::Success);
        assert_eq!(flaky.attempts, 3);
        let allowed = report.get("allowed").unwrap();
        assert_eq!(allowed.status, TaskStatus::Failed);
        assert_eq!(allowed.exit_code, Some(4));
        assert!(allowed.allowed_failure);
        // Only the timeout counts against the run.
        assert!(report.had_failure());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_scale_stretches_timeouts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut t = mk("t", "sleep 0.3", &[]);
        t.timeout = Some(Duration::from_millis(100));
        let mut opts = RunOptions::new(tmp.path(), 1);
        opts.timeout_scale = 10.0;
        let report = run_in(tmp.path(), &[t], &opts).await;
        assert_eq!(report.get("t").unwrap().status, TaskStatus::Success);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fail_fast_stops_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let fails = mk("fails", "sleep 0.1; false", &[]);
        let long = mk("long", "sleep 30", &[]);
        let later = mk("later", "echo late", &["long"]);
        let mut opts = RunOptions::new(tmp.path(), 4);
        opts.fail_fast = true;
        let started = Instant::now();
        let report = run_in(tmp.path(), &[fails, long, later], &opts).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(report.get("fails").unwrap().status, TaskStatus::Failed);
        assert_eq!(report.get("long").unwrap().status, TaskStatus::Cancelled);
        assert!(matches!(
            report.get("later").unwrap().status,
            TaskStatus::Skipped(SkipReason::Upstream { .. })
        ));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exclusive_tasks_do_not_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let script =
            "if [ -e held ]; then echo overlap >> errors; fi; touch held; sleep 0.2; rm held";
        let mut tasks = Vec::new();
        for name in ["a", "b", "c"] {
            let mut t = mk(name, script, &[]);
            t.exclusive = vec!["port-8642".into()];
            tasks.push(t);
        }
        let report = run_in(tmp.path(), &tasks, &RunOptions::new(tmp.path(), 3)).await;
        assert!(!report.had_failure());
        assert!(
            !tmp.path().join("errors").exists(),
            "two holders of one lock overlapped"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writes_logs_and_keeps_the_tail_of_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = RunOptions::new(tmp.path(), 1);
        opts.log_dir = Some(tmp.path().join("logs"));
        opts.tail_lines = 2;
        let mut t = mk("t[x=1]", "echo one; echo two; echo three; exit 2", &[]);
        t.cwd = Some("sub".into());
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let report = run_in(tmp.path(), &[t], &opts).await;
        let o = report.get("t[x=1]").unwrap();
        assert_eq!(o.tail.len(), 2);
        assert_eq!(o.tail[1], "three");
        let log = std::fs::read_to_string(o.log.as_ref().unwrap()).unwrap();
        assert!(log.contains("one\n"), "{log}");
        assert!(log.contains("--- exit code 2 ---"), "{log}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_working_directory_fails_the_task() {
        let tmp = tempfile::tempdir().unwrap();
        let mut t = mk("t", "echo x", &[]);
        t.cwd = Some("nowhere".into());
        let report = run_in(tmp.path(), &[t], &RunOptions::new(tmp.path(), 1)).await;
        let o = report.get("t").unwrap();
        assert_eq!(o.status, TaskStatus::Failed);
        assert!(o.error.as_ref().unwrap().contains("does not exist"));
    }

    #[test]
    fn suggests_close_names() {
        let names = ["build", "test", "deploy"];
        assert_eq!(
            closest("biuld", names.iter().copied()),
            Some("build".into())
        );
        assert_eq!(closest("zzzzzz", names.iter().copied()), None);
    }
}
