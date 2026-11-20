//! The machine-readable account of a run: `report.json`.
//!
//! The report is versioned (`"version": 1`) and lists every selected task
//! in dependency order with its status and why, how long it took, where
//! its log is, and, for tasks that failed, the last lines they printed.
//! Paths are relative to the project root.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::executor::{RunReport, TaskOutcome, TaskStatus};
use crate::rundir::rfc3339;
use crate::steps::StepStatus;
use crate::task::{Task, TaskKind};

/// What a report says about the run as a whole.
#[derive(Debug, Clone)]
pub struct RunMeta {
    /// The run's id (its directory's name).
    pub id: String,
    /// When the run started.
    pub started_at: SystemTime,
    /// How long it took.
    pub duration: Duration,
    /// Whether it was interrupted (Ctrl-C).
    pub interrupted: bool,
    /// The project root, which paths are given relative to.
    pub root: PathBuf,
}

/// `report.json`.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Format version.
    pub version: u32,
    /// The run's id.
    pub run: String,
    /// RFC 3339 start time, UTC.
    pub started_at: String,
    /// Seconds.
    pub duration: f64,
    /// No task failed or timed out (other than those allowed to), and the
    /// run was not interrupted.
    pub success: bool,
    /// The run was interrupted.
    pub interrupted: bool,
    /// How many tasks ended with each status.
    pub summary: std::collections::BTreeMap<&'static str, usize>,
    /// Every task, in dependency order.
    pub tasks: Vec<ReportTask>,
}

/// One task in [`Report`].
#[derive(Debug, Serialize)]
pub struct ReportTask {
    /// Name.
    pub name: String,
    /// `task` or `test`.
    pub kind: &'static str,
    /// Its description, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where it is defined (`file:line`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defined_at: Option<String>,
    /// `success`, `cached`, `failed`, `timed_out`, `cancelled` or `skipped`.
    pub status: &'static str,
    /// Why it failed or was skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The failure does not count against the run.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allowed_failure: bool,
    /// Exit status of the last attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// How many times it was started.
    pub attempts: u32,
    /// Seconds.
    pub duration: f64,
    /// RFC 3339 time its first attempt started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Its log file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// The last lines it printed, if it did not succeed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tail: Vec<String>,
    /// A test's directory of files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_dir: Option<String>,
    /// A test's steps.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<ReportStep>,
}

/// One step of a test in [`ReportTask`].
#[derive(Debug, Serialize)]
pub struct ReportStep {
    /// Position, from 1.
    pub index: usize,
    /// What it does.
    pub label: String,
    /// Its section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// The line it is written on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// `passed`, `failed` or `not_run`.
    pub status: &'static str,
    /// Seconds.
    pub duration: f64,
    /// Why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Report {
    /// The report of `run`, whose tasks are `tasks`.
    pub fn new(run: &RunReport, tasks: &[Task], meta: &RunMeta) -> Self {
        let tasks_out = run
            .outcomes
            .iter()
            .map(|o| task_entry(o, tasks.iter().find(|t| t.name == o.name), &meta.root))
            .collect();
        Self {
            version: 1,
            run: meta.id.clone(),
            started_at: rfc3339(meta.started_at),
            duration: secs(meta.duration),
            success: !run.had_failure() && !meta.interrupted,
            interrupted: meta.interrupted,
            summary: run.counts(),
            tasks: tasks_out,
        }
    }

    /// The report as pretty-printed JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a report always serializes")
    }
}

fn task_entry(o: &TaskOutcome, task: Option<&Task>, root: &Path) -> ReportTask {
    let reason = match &o.status {
        TaskStatus::Success | TaskStatus::Cached => None,
        TaskStatus::Skipped(why) => Some(why.describe()),
        _ => o.error.clone(),
    };
    ReportTask {
        name: o.name.clone(),
        kind: match task.map(|t| t.kind) {
            Some(TaskKind::Test) => "test",
            _ => "task",
        },
        description: task.and_then(|t| t.description.clone()),
        defined_at: task.and_then(|t| t.source.as_ref()).map(|l| {
            let file = l.file.strip_prefix(root).unwrap_or(&l.file);
            format!("{}:{}", file.display(), l.line)
        }),
        status: o.status.label(),
        reason,
        allowed_failure: o.allowed_failure,
        exit_code: o.exit_code,
        attempts: o.attempts,
        duration: secs(o.duration),
        started_at: o.started_at.map(rfc3339),
        log: o
            .log
            .as_ref()
            .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string()),
        tail: o.tail.clone(),
        test_dir: o
            .test_dir
            .as_ref()
            .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string()),
        steps: o
            .steps
            .iter()
            .map(|s| ReportStep {
                index: s.index,
                label: s.label.clone(),
                section: s.section.clone(),
                line: s.line,
                status: match s.status {
                    StepStatus::Passed => "passed",
                    StepStatus::Failed => "failed",
                    StepStatus::NotRun => "not_run",
                },
                duration: secs(s.duration),
                message: s.message.clone(),
            })
            .collect(),
    }
}

fn secs(d: Duration) -> f64 {
    (d.as_secs_f64() * 1000.0).round() / 1000.0
}
