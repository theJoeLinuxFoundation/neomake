//! What a run prints on the terminal.
//!
//! Progress lines are tagged by status (`[RUNNING]`, `[OK     ]`, …),
//! the lines tasks print are prefixed with the task's name (`|` for
//! stdout, `!` for stderr), and a run ends with a summary that names
//! every task that did not succeed, its reason and its log, followed by
//! the last lines of each failure's output when they were not already
//! shown.

use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use neomake_core::duration::human;
use neomake_core::{ExecutionEvent, Reporter, RunReport, TaskStatus};

/// Settings of the console output.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    /// Use ANSI colours.
    pub color: bool,
    /// Do not echo the lines tasks print.
    pub quiet: bool,
}

/// The reporter that prints progress to stdout.
pub fn reporter(style: Style) -> Reporter {
    let stdout = Arc::new(Mutex::new(io::stdout()));
    Arc::new(move |event: ExecutionEvent| {
        let mut out = stdout.lock().unwrap_or_else(|e| e.into_inner());
        let c = |color: &str, text: &str| paint(style.color, color, text);
        let _ = match event {
            ExecutionEvent::Started { task, attempt } if attempt > 1 => {
                writeln!(
                    out,
                    "{} {task}  (attempt {attempt})",
                    c("cyan", "[RUNNING]")
                )
            }
            ExecutionEvent::Started { task, .. } => {
                writeln!(out, "{} {task}", c("cyan", "[RUNNING]"))
            }
            ExecutionEvent::Cached { task, key } => {
                let short: String = key.chars().take(12).collect();
                writeln!(out, "{} {task}  (key={short}…)", c("yellow", "[CACHED ]"))
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
                writeln!(
                    out,
                    "{} {task}  ({}{after})",
                    c("green", "[OK     ]"),
                    human(duration)
                )
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
                writeln!(out, "{tag} {task}  {message}{note}")
            }
            ExecutionEvent::Cancelled { task } => {
                writeln!(out, "{} {task}  stopped", c("magenta", "[STOPPED]"))
            }
            ExecutionEvent::Skipped { task, reason } => writeln!(
                out,
                "{} {task}  ({})",
                c("magenta", "[SKIPPED]"),
                reason.describe()
            ),
            ExecutionEvent::Stdout { task, line } if !style.quiet => {
                writeln!(out, "    {task} | {line}")
            }
            ExecutionEvent::Stderr { task, line } if !style.quiet => {
                writeln!(out, "    {task} ! {line}")
            }
            ExecutionEvent::Stdout { .. } | ExecutionEvent::Stderr { .. } => Ok(()),
        };
    })
}

/// The end-of-run summary.
pub fn summary(
    report: &RunReport,
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
        let _ = writeln!(out, "  {tag} {}: {reason}  {log}", o.name);
    }
    // Lines the run did not already show.
    if style.quiet && tail > 0 {
        for o in unsuccessful.iter().filter(|o| !o.tail.is_empty()) {
            let shown = &o.tail[o.tail.len().saturating_sub(tail)..];
            let _ = writeln!(out, "--- {} (last {} lines) ---", o.name, shown.len());
            for line in shown {
                let _ = writeln!(out, "    {line}");
            }
        }
    }
    if let Some(dir) = run_dir {
        let _ = writeln!(out, "logs: {}", rel(dir));
    }
}

fn paint(enabled: bool, color: &str, text: &str) -> String {
    if !enabled {
        return text.to_string();
    }
    let code = match color {
        "red" => "31",
        "green" => "32",
        "yellow" => "33",
        "cyan" => "36",
        "magenta" => "35",
        _ => return text.to_string(),
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}
