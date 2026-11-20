//! Running a test's steps on the host.
//!
//! Every command step is its own shell process, started in the test's
//! working directory with the test's environment; the test's `prelude`
//! (shell functions and variables its steps share) is put in front of
//! each one. A step's output is collected per stream for its checks and,
//! like any task's, goes to the log and the terminal.
//!
//! The test's timeout is a deadline for all its steps together; a
//! step's own timeout can only shorten it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::vars::Vars;
use super::{Action, Output, Step, StepOutcome, StepStatus, Tty};
use crate::duration::human;
use crate::executor::{ExecutionEvent, Sink};
use crate::process::{self, Invocation, Stream};

/// Everything a test's steps run with.
pub struct HostEnv<'a> {
    /// The shell's argv; each step's script is appended.
    pub shell: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Variables set for every step.
    pub env: std::collections::BTreeMap<String, String>,
    /// Start from an empty environment.
    pub env_clear: bool,
    /// Shell text put in front of every step.
    pub prelude: Option<String>,
    /// When the test must be done.
    pub deadline: Option<Instant>,
    /// The run's cancellation.
    pub cancel: &'a CancellationToken,
    /// Placeholders' values.
    pub vars: Vars,
}

/// How an attempt at a task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Everything passed.
    Passed,
    /// A step failed.
    Failed,
    /// The deadline passed.
    TimedOut,
    /// The run was cancelled.
    Cancelled,
}

/// The result of running a test's steps.
#[derive(Debug, Clone)]
pub struct StepsResult {
    /// How it ended.
    pub ending: Ending,
    /// Why, if it did not pass.
    pub message: Option<String>,
    /// The exit status of the failing step's command, if it had one.
    pub exit_code: Option<i32>,
    /// Every step's record.
    pub steps: Vec<StepOutcome>,
    /// Where in the steps the run stopped: the first not yet recorded.
    pub(crate) resume: usize,
    /// The number of the last step recorded.
    pub(crate) number: usize,
    /// The section the run stopped in.
    pub(crate) section: Option<String>,
}

impl StepsResult {
    /// A test that ended before its first step (its machine did not
    /// boot, say), for `message`.
    pub fn before_steps(ending: Ending, message: impl Into<String>) -> Self {
        Self {
            ending,
            message: Some(message.into()),
            exit_code: None,
            steps: Vec::new(),
            resume: 0,
            number: 0,
            section: None,
        }
    }
}

/// Why one step did not pass.
#[derive(Debug, Clone)]
pub struct StepFailure {
    /// How it ends the test.
    pub ending: Ending,
    /// What went wrong.
    pub message: String,
    /// The exit status of its command, if it had one.
    pub exit_code: Option<i32>,
}

impl StepFailure {
    /// A step that failed for `message`.
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            ending: Ending::Failed,
            message: message.into(),
            exit_code: None,
        }
    }
}

/// Run `steps` on the host. `file` names where they are written, for
/// failure messages.
pub async fn run(
    steps: &[Step],
    file: Option<&str>,
    env: &mut HostEnv<'_>,
    sink: &Sink,
) -> StepsResult {
    let mut runner = Runner::new(steps, file);
    for (i, step) in steps.iter().enumerate() {
        if !runner.begin(i, step, env.cancel, sink) {
            continue;
        }
        let started = Instant::now();
        let result = if step.action.runs_a_command() {
            run_host_step(step, env, sink).await
        } else {
            Err(StepFailure::failed("this step needs a machine"))
        };
        if let Some(result) = runner.finish(step, started, result, sink) {
            return result;
        }
    }
    runner.done()
}

/// The bookkeeping every kind of step runner shares: numbering,
/// sections, progress events and outcomes. Sections are not steps: they
/// are not numbered and have no outcome, but name the section of the
/// steps after them.
pub struct Runner<'f> {
    total: usize,
    number: usize,
    position: usize,
    file: Option<&'f str>,
    section: Option<String>,
    outcomes: Vec<StepOutcome>,
    stopped: Option<StepsResult>,
}

impl<'f> Runner<'f> {
    /// Bookkeeping for `steps`, written in `file`.
    pub fn new(steps: &[Step], file: Option<&'f str>) -> Self {
        Self {
            total: steps
                .iter()
                .filter(|s| !matches!(s.action, Action::Section(_)))
                .count(),
            number: 0,
            position: 0,
            file,
            section: None,
            outcomes: Vec::new(),
            stopped: None,
        }
    }

    /// Note the start of `steps[i]`. `false` when there is nothing to
    /// run: a section, or a stopped run (whose result [`Self::done`]
    /// then returns).
    pub fn begin(
        &mut self,
        i: usize,
        step: &Step,
        cancel: &CancellationToken,
        sink: &Sink,
    ) -> bool {
        if self.stopped.is_some() {
            return false;
        }
        self.position = i;
        if let Action::Section(title) = &step.action {
            self.section = Some(title.clone());
            sink.note(format!("=== {title}"));
            sink.event(ExecutionEvent::Section {
                task: sink.task().to_string(),
                title: title.clone(),
            });
            return false;
        }
        self.number += 1;
        if cancel.is_cancelled() {
            let result = self.end(
                StepFailure {
                    ending: Ending::Cancelled,
                    message: "the run was cancelled".into(),
                    exit_code: None,
                },
                None,
                step,
            );
            self.stopped = Some(result);
            return false;
        }
        sink.note(format!(
            "--- step {}/{}{}: {}",
            self.number,
            self.total,
            self.at(step),
            step.describe()
        ));
        true
    }

    /// Record how the step begun last ended; `Some` ends the run.
    pub fn finish(
        &mut self,
        step: &Step,
        started: Instant,
        result: Result<(), StepFailure>,
        sink: &Sink,
    ) -> Option<StepsResult> {
        let duration = started.elapsed();
        sink.event(ExecutionEvent::Step {
            task: sink.task().to_string(),
            index: self.number,
            total: self.total,
            label: step.describe(),
            passed: result.is_ok(),
            duration,
        });
        match result {
            Ok(()) => {
                self.outcomes.push(StepOutcome {
                    index: self.number,
                    label: step.describe(),
                    section: self.section.clone(),
                    line: step.line,
                    status: StepStatus::Passed,
                    duration,
                    message: None,
                });
                None
            }
            Err(failure) => {
                sink.note(format!(
                    "--- step {} failed: {}",
                    self.number, failure.message
                ));
                Some(self.end(failure, Some(duration), step))
            }
        }
    }

    fn at(&self, step: &Step) -> String {
        match (self.file, step.line) {
            (Some(f), Some(l)) => format!(" ({f}:{l})"),
            (None, Some(l)) => format!(" (line {l})"),
            _ => String::new(),
        }
    }

    /// The step begun last ended the run with `failure`; `duration` is
    /// how long it ran, if it ran at all.
    pub fn end(
        &mut self,
        failure: StepFailure,
        duration: Option<Duration>,
        step: &Step,
    ) -> StepsResult {
        let message = format!(
            "step {}/{}{} `{}`: {}",
            self.number,
            self.total,
            self.at(step),
            step.describe(),
            failure.message
        );
        let mut outcomes = std::mem::take(&mut self.outcomes);
        outcomes.push(StepOutcome {
            index: self.number,
            label: step.describe(),
            section: self.section.clone(),
            line: step.line,
            status: if duration.is_some() {
                StepStatus::Failed
            } else {
                StepStatus::NotRun
            },
            duration: duration.unwrap_or_default(),
            message: Some(failure.message.clone()),
        });
        StepsResult {
            ending: failure.ending,
            message: Some(message),
            exit_code: failure.exit_code,
            steps: outcomes,
            resume: self.position + 1,
            number: self.number,
            section: self.section.clone(),
        }
    }

    /// Mark the steps after the one that ended the run as not run.
    pub fn not_run_after(result: &mut StepsResult, steps: &[Step]) {
        let mut section = result.section.clone();
        let mut number = result.number;
        for step in steps.iter().skip(result.resume) {
            if let Action::Section(title) = &step.action {
                section = Some(title.clone());
                continue;
            }
            number += 1;
            result.steps.push(StepOutcome {
                index: number,
                label: step.describe(),
                section: section.clone(),
                line: step.line,
                status: StepStatus::NotRun,
                duration: Duration::ZERO,
                message: None,
            });
        }
        result.resume = steps.len();
    }

    /// The result of a run that got past every step (or was stopped).
    pub fn done(&mut self) -> StepsResult {
        if let Some(stopped) = self.stopped.take() {
            return stopped;
        }
        StepsResult {
            ending: Ending::Passed,
            message: None,
            exit_code: None,
            steps: std::mem::take(&mut self.outcomes),
            resume: usize::MAX,
            number: self.number,
            section: self.section.clone(),
        }
    }
}

/// `step` with every placeholder replaced.
pub fn resolve(step: &Step, vars: &Vars) -> Result<Step, StepFailure> {
    let mut step = step.clone();
    for s in step.strings_mut() {
        *s = vars.apply(s).map_err(StepFailure::failed)?;
    }
    Ok(step)
}

/// How long a step may run: the test's deadline, shortened by the
/// step's own timeout.
pub fn step_deadline(step: &Step, deadline: Option<Instant>) -> Option<Instant> {
    let own = step.timeout.map(|t| Instant::now() + t);
    match (deadline, own) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Pause before repeating a waiting step; `false` when it is cancelled.
pub async fn pause(interval: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(interval) => true,
        _ = cancel.cancelled() => false,
    }
}

/// Run a `run` or `host` step on the host, with its checks.
pub async fn run_host_step(
    step: &Step,
    env: &mut HostEnv<'_>,
    sink: &Sink,
) -> Result<(), StepFailure> {
    let resolved = resolve(step, &env.vars)?;
    let (script, tty) = match &resolved.action {
        Action::Run { script, tty } | Action::Host { script, tty } => (script.clone(), tty.clone()),
        _ => return Err(StepFailure::failed("this step runs no command")),
    };
    let full = match &env.prelude {
        Some(p) => format!("{p}\n{script}"),
        None => script,
    };
    let deadline = step_deadline(&resolved, env.deadline);
    let waiting = resolved.wait.map(|w| (Instant::now() + w.within, w));

    loop {
        let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let (out, ended) = run_once(&full, tty.as_ref(), timeout, env, sink).await?;
        match ended {
            Ended::Cancelled => {
                return Err(StepFailure {
                    ending: Ending::Cancelled,
                    message: "the run was cancelled".into(),
                    exit_code: None,
                })
            }
            Ended::TimedOut => {
                return Err(StepFailure {
                    ending: Ending::TimedOut,
                    message: format!("timed out after {}", human(timeout.unwrap_or_default())),
                    exit_code: Some(out.status),
                })
            }
            Ended::Forbidden(pattern) => {
                return Err(StepFailure {
                    exit_code: Some(out.status),
                    ..StepFailure::failed(format!(
                        "the output shows /{pattern}/, which it must not"
                    ))
                })
            }
            Ended::Exited => {}
        }
        match resolved.check.verify(&out) {
            Ok(captured) => {
                for (name, value) in captured {
                    env.vars.set(name, value);
                }
                return Ok(());
            }
            Err(message) => {
                if let Some((until, wait)) = waiting {
                    if Instant::now() + wait.interval < until {
                        if !pause(wait.interval, env.cancel).await {
                            return Err(StepFailure {
                                ending: Ending::Cancelled,
                                message: "the run was cancelled".into(),
                                exit_code: None,
                            });
                        }
                        continue;
                    }
                    return Err(StepFailure {
                        exit_code: Some(out.status),
                        ..StepFailure::failed(format!(
                            "{message} (still, after {})",
                            human(wait.within)
                        ))
                    });
                }
                return Err(StepFailure {
                    exit_code: Some(out.status),
                    ..StepFailure::failed(message)
                });
            }
        }
    }
}

enum Ended {
    Exited,
    TimedOut,
    Cancelled,
    // Only a terminal step (Unix only) can show forbidden output.
    #[cfg_attr(not(unix), allow(dead_code))]
    Forbidden(String),
}

async fn run_once(
    script: &str,
    tty: Option<&Tty>,
    timeout: Option<Duration>,
    env: &HostEnv<'_>,
    sink: &Sink,
) -> Result<(Output, Ended), StepFailure> {
    let (program, args) = env
        .shell
        .split_first()
        .ok_or_else(|| StepFailure::failed("the shell is empty"))?;
    let mut args = args.to_vec();
    args.push(script.to_string());
    let invocation = Invocation {
        program: program.clone(),
        args,
        cwd: env.cwd.clone(),
        env: env.env.clone(),
        env_clear: env.env_clear,
        interactive: false,
    };

    if let Some(tty) = tty {
        return run_on_tty(invocation, tty.clone(), timeout, env.cancel.clone(), sink).await;
    }

    let mut out = Output::default();
    let exit = process::run(&invocation, timeout, env.cancel, |stream, line| {
        let target = match stream {
            Stream::Stdout => &mut out.stdout,
            Stream::Stderr => &mut out.stderr,
        };
        target.push_str(&line);
        target.push('\n');
        out.combined.push_str(&line);
        out.combined.push('\n');
        sink.line(stream, line);
    })
    .await
    .map_err(|e| StepFailure::failed(format!("could not start `{program}`: {e}")))?;
    out.status = exit.status();
    let ended = if exit.cancelled {
        Ended::Cancelled
    } else if exit.timed_out {
        Ended::TimedOut
    } else {
        Ended::Exited
    };
    Ok((out, ended))
}

#[cfg(unix)]
async fn run_on_tty(
    invocation: Invocation,
    tty: Tty,
    timeout: Option<Duration>,
    cancel: CancellationToken,
    sink: &Sink,
) -> Result<(Output, Ended), StepFailure> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let worker = tokio::task::spawn_blocking(move || {
        super::pty::run(&invocation, &tty, timeout, &cancel, &mut |line| {
            let _ = tx.send(line);
        })
    });
    while let Some(line) = rx.recv().await {
        sink.line(Stream::Stdout, line);
    }
    let result = worker
        .await
        .map_err(|e| StepFailure::failed(format!("the terminal worker panicked: {e}")))?
        .map_err(|e| StepFailure::failed(format!("could not run on a terminal: {e}")))?;
    let out = Output {
        stdout: result.output.clone(),
        stderr: String::new(),
        combined: result.output,
        status: result.exit.status(),
    };
    let ended = if result.exit.cancelled {
        Ended::Cancelled
    } else if result.exit.timed_out {
        Ended::TimedOut
    } else if let Some(pattern) = result.forbidden {
        Ended::Forbidden(pattern)
    } else {
        Ended::Exited
    };
    Ok((out, ended))
}

#[cfg(not(unix))]
async fn run_on_tty(
    _invocation: Invocation,
    _tty: Tty,
    _timeout: Option<Duration>,
    _cancel: CancellationToken,
    _sink: &Sink,
) -> Result<(Output, Ended), StepFailure> {
    Err(StepFailure::failed(
        "steps on a terminal (`tty`) need a Unix host",
    ))
}
