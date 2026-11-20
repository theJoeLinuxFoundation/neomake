//! What a run prints on the terminal.
//!
//! Progress lines are tagged by status (`[RUNNING]`, `[OK     ]`, …).
//! What tasks print is shown in one of three ways ([`OutputMode`]): as it
//! comes, prefixed with the task's name (`|` for stdout, `!` for
//! stderr); grouped, each task's lines together once it ends; or only
//! the last lines of what failed, at the end. Tests also show their
//! sections and failing steps, and with `--verbose` every step.
//!
//! A run ends with a summary naming every task that did not succeed,
//! why, and where its log is.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use neomake_core::duration::human;
use neomake_core::steps::StepStatus;
use neomake_core::{ExecutionEvent, Reporter, RunReport, Task, TaskStatus};

use crate::cli::OutputMode;

/// Settings of the console output.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    /// Use ANSI colours.
    pub color: bool,
    /// How task output is shown.
    pub mode: OutputMode,
    /// Show every step, not only failing ones.
    pub verbose: bool,
    /// Running under GitHub Actions: groups and annotations.
    pub github: bool,
}

/// The reporter that prints progress to stdout.
pub fn reporter(style: Style) -> Reporter {
    let buffers: Arc<Mutex<HashMap<String, Vec<String>>>> = Arc::default();
    let stdout = Arc::new(Mutex::new(io::stdout()));
    Arc::new(move |event: ExecutionEvent| {
        let mut out = stdout.lock().unwrap_or_else(|e| e.into_inner());
        let c = |color: &str, text: &str| paint(style.color, color, text);
        // A line of a task's own output (or of its steps' progress).
        let mut task_line = |task: &str, text: String| match style.mode {
            OutputMode::Stream => {
                let _ = writeln!(out, "    {task} {text}");
            }
            OutputMode::Grouped => buffers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(task.to_string())
                .or_default()
                .push(text),
            OutputMode::Failures => {}
        };
        match event {
            ExecutionEvent::Stdout { task, line } => task_line(&task, format!("| {line}")),
            ExecutionEvent::Stderr { task, line } => task_line(&task, format!("! {line}")),
            ExecutionEvent::Section { task, title } => {
                let text = format!("== {title}");
                task_line(&task, c("cyan", &text))
            }
            ExecutionEvent::Step {
                task,
                index,
                total,
                label,
                passed,
                duration,
            } => {
                if !passed {
                    task_line(
                        &task,
                        format!("{} step {index}/{total} {label}", c("red", "✗")),
                    );
                } else if style.verbose {
                    task_line(
                        &task,
                        format!(
                            "{} step {index}/{total} {label}  ({})",
                            c("green", "✓"),
                            human(duration)
                        ),
                    );
                }
            }
            other => {
                drop(out);
                status_line(other, style, &buffers);
            }
        }
    })
}

fn status_line(event: ExecutionEvent, style: Style, buffers: &Mutex<HashMap<String, Vec<String>>>) {
    let mut out = io::stdout().lock();
    let c = |color: &str, text: &str| paint(style.color, color, text);
    let (task, line, ended) = match event {
        ExecutionEvent::Started { task, attempt } => {
            let line = if attempt > 1 {
                format!("{} {task}  (attempt {attempt})", c("cyan", "[RUNNING]"))
            } else {
                format!("{} {task}", c("cyan", "[RUNNING]"))
            };
            (task, line, false)
        }
        ExecutionEvent::Cached { task, key } => {
            let short: String = key.chars().take(12).collect();
            let line = format!("{} {task}  (key={short}…)", c("yellow", "[CACHED ]"));
            (task, line, false)
        }
        ExecutionEvent::Finished {
            task,
            duration,
            attempts,
        } => {
            let after = if attempts > 1 {
                format!(", passed on attempt {attempts}")
            } else {
                String::new()
            };
            let line = format!(
                "{} {task}  ({}{after})",
                c("green", "[OK     ]"),
                human(duration)
            );
            (task, line, true)
        }
        ExecutionEvent::Failed {
            task,
            message,
            timed_out,
            retrying,
            allowed,
            ..
        } => {
            let tag = if retrying {
                c("yellow", "[RETRY  ]")
            } else if timed_out {
                c("red", "[TIMEOUT]")
            } else {
                c("red", "[FAILED ]")
            };
            let note = if allowed && !retrying {
                " (allowed to fail)"
            } else {
                ""
            };
            let line = format!("{tag} {task}  {message}{note}");
            (task, line, !retrying)
        }
        ExecutionEvent::Cancelled { task } => {
            let line = format!("{} {task}  stopped", c("magenta", "[STOPPED]"));
            (task, line, true)
        }
        ExecutionEvent::ServiceStarted { name, detail } => {
            let line = format!("{} {name}  {detail}", c("cyan", "[SERVICE]"));
            (name, line, false)
        }
        ExecutionEvent::ServiceFailed { name, message } => {
            let line = format!("{} {name}  {message}", c("red", "[SERVICE]"));
            (name, line, false)
        }
        ExecutionEvent::ServiceStopped { name } => {
            let line = format!("{} {name}  stopped", c("dim", "[SERVICE]"));
            (name, line, false)
        }
        ExecutionEvent::Skipped { task, reason } => {
            let line = format!(
                "{} {task}  ({})",
                c("magenta", "[SKIPPED]"),
                reason.describe()
            );
            (task, line, false)
        }
        _ => return,
    };
    let _ = writeln!(out, "{line}");
    if ended && style.mode == OutputMode::Grouped {
        let lines = buffers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&task)
            .unwrap_or_default();
        if !lines.is_empty() {
            if style.github {
                let _ = writeln!(out, "::group::{task} output");
            }
            for l in lines {
                let _ = writeln!(out, "    {l}");
            }
            if style.github {
                let _ = writeln!(out, "::endgroup::");
            }
        }
    }
}

/// The end-of-run summary.
pub fn summary(
    report: &RunReport,
    tasks: &[Task],
    elapsed: Duration,
    run_dir: Option<&Path>,
    root: &Path,
    style: Style,
    tail: usize,
) {
    let mut out = io::stdout().lock();
    let counts = report.counts();
    let count = |label| counts.get(label).copied().unwrap_or(0);
    let mut parts = vec![
        format!("{} ok", count("success")),
        format!("{} cached", count("cached")),
        format!("{} failed", count("failed")),
    ];
    for (label, word) in [("timed_out", "timed out"), ("cancelled", "stopped")] {
        if count(label) > 0 {
            parts.push(format!("{} {word}", count(label)));
        }
    }
    parts.push(format!("{} skipped", count("skipped")));
    let _ = writeln!(out, "summary: {}  ({})", parts.join(", "), human(elapsed));

    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    let unsuccessful: Vec<_> = report
        .outcomes
        .iter()
        .filter(|o| o.status.is_failure() || o.status == TaskStatus::Cancelled)
        .collect();
    for o in &unsuccessful {
        let reason = o.error.as_deref().unwrap_or("failed");
        let log = o.log.as_deref().map(rel).unwrap_or_default();
        let tag = if o.allowed_failure {
            paint(style.color, "yellow", "allowed")
        } else {
            paint(style.color, "red", o.status.label())
        };
        let _ = writeln!(out, "  {tag} {}: {reason}", o.name);
        if !log.is_empty() {
            let _ = writeln!(out, "      log: {log}");
        }
        if let Some(dir) = &o.test_dir {
            let _ = writeln!(out, "      files: {}", rel(dir));
        }
    }
    // Lines the run did not already show.
    if style.mode == OutputMode::Failures && tail > 0 {
        for o in unsuccessful.iter().filter(|o| !o.tail.is_empty()) {
            let shown = &o.tail[o.tail.len().saturating_sub(tail)..];
            let _ = writeln!(out, "--- {} (last {} lines) ---", o.name, shown.len());
            for line in shown {
                let _ = writeln!(out, "    {line}");
            }
        }
    }
    if style.github {
        for o in unsuccessful.iter().filter(|o| !o.allowed_failure) {
            let task = tasks.iter().find(|t| t.name == o.name);
            let file = task
                .and_then(|t| t.source.as_ref())
                .map(|l| rel(&l.file))
                .unwrap_or_default();
            // The failing step's line, if a step failed.
            let line = o
                .steps
                .iter()
                .find(|s| s.status == StepStatus::Failed)
                .and_then(|s| s.line)
                .or_else(|| task.and_then(|t| t.source.as_ref()).map(|l| l.line))
                .unwrap_or(1);
            let message = o.error.as_deref().unwrap_or("failed");
            let _ = writeln!(
                out,
                "::error file={file},line={line},title={}::{}",
                escape_property(&o.name),
                escape_data(message)
            );
        }
    }
    if let Some(dir) = run_dir {
        let _ = writeln!(out, "logs: {}", rel(dir));
    }
}

/// GitHub's escaping of a workflow command's message.
fn escape_data(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// GitHub's escaping of a workflow command's property.
fn escape_property(s: &str) -> String {
    escape_data(s).replace(':', "%3A").replace(',', "%2C")
}

pub(crate) fn paint(enabled: bool, color: &str, text: &str) -> String {
    if !enabled {
        return text.to_string();
    }
    let code = match color {
        "red" => "31",
        "green" => "32",
        "yellow" => "33",
        "cyan" => "36",
        "magenta" => "35",
        "dim" => "2",
        _ => return text.to_string(),
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}
