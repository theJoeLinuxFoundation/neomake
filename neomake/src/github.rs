//! GitHub Actions: the job summary.
//!
//! When `GITHUB_STEP_SUMMARY` names a file, a run appends a Markdown
//! table to it: every task that did not succeed or get cached, then a
//! count of what did. Progress groups and failure annotations are part
//! of the console output ([`crate::output`]).

use std::io::Write;

use neomake_core::duration::human;
use neomake_core::{RunReport, TaskStatus};

/// Whether this process runs under GitHub Actions.
pub fn active() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some_and(|v| v == "true")
}

/// Append the run's summary to `$GITHUB_STEP_SUMMARY`, if it is set.
pub fn write_summary(report: &RunReport, title: &str) {
    let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") else {
        return;
    };
    let mut md = format!("### {title}\n\n");
    let counts = report.counts();
    let line: Vec<String> = counts
        .iter()
        .map(|(k, v)| format!("{v} {}", k.replace('_', " ")))
        .collect();
    md.push_str(&format!("{}\n\n", line.join(", ")));
    let notable: Vec<_> = report
        .outcomes
        .iter()
        .filter(|o| !matches!(o.status, TaskStatus::Success | TaskStatus::Cached))
        .collect();
    if !notable.is_empty() {
        md.push_str("| | Task | Time | Why |\n|---|---|---|---|\n");
        for o in notable {
            let icon = match &o.status {
                TaskStatus::Skipped(_) => "⏭️",
                _ if o.allowed_failure => "⚠️",
                _ => "❌",
            };
            let why = match &o.status {
                TaskStatus::Skipped(r) => r.describe(),
                _ => o.error.clone().unwrap_or_default(),
            };
            md.push_str(&format!(
                "| {icon} | `{}` | {} | {} |\n",
                o.name,
                human(o.duration),
                why.replace('|', "\\|").replace('\n', " ")
            ));
        }
        md.push('\n');
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    {
        let _ = f.write_all(md.as_bytes());
    }
}
