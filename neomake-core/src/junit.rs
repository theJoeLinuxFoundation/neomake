//! JUnit XML, for CI systems that show test results.
//!
//! Every test that was selected is a `<testcase>`: passed, `<failure>`
//! (it failed or timed out), `<skipped>` (with the reason) or, when it
//! was stopped, `<error>`. A task that failed is reported too, as an
//! `<error>`, since the tests after it did not get to run; tasks that
//! succeeded are left out. A test's `classname` is the file it is
//! defined in, with `/` as `.` and no extension (`tests.cli`), and its
//! steps are listed in `<system-out>`, with the failing one marked.

use std::fmt::Write as _;

use crate::executor::{RunReport, TaskOutcome, TaskStatus};
use crate::report::RunMeta;
use crate::rundir::rfc3339;
use crate::steps::StepStatus;
use crate::task::Task;

/// The JUnit XML document for `run`.
pub fn render(run: &RunReport, tasks: &[Task], meta: &RunMeta) -> String {
    let mut cases = Vec::new();
    let (mut failures, mut errors, mut skipped) = (0, 0, 0);
    for outcome in &run.outcomes {
        let task = tasks.iter().find(|t| t.name == outcome.name);
        let is_test = task.is_some_and(Task::is_test);
        let failed = outcome.status.is_failure() || outcome.status == TaskStatus::Cancelled;
        if !is_test && !failed {
            continue;
        }
        let kind = match (&outcome.status, is_test) {
            (TaskStatus::Success | TaskStatus::Cached, _) => Kind::Passed,
            (TaskStatus::Failed | TaskStatus::TimedOut, true) if !outcome.allowed_failure => {
                failures += 1;
                Kind::Failure
            }
            (TaskStatus::Failed | TaskStatus::TimedOut, true) => {
                skipped += 1;
                Kind::Skipped
            }
            (TaskStatus::Skipped(_), _) => {
                skipped += 1;
                Kind::Skipped
            }
            _ => {
                errors += 1;
                Kind::Error
            }
        };
        cases.push(case(outcome, task, kind, meta));
    }
    let tests = cases.len();
    let time = meta.duration.as_secs_f64();
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        xml,
        "<testsuites name=\"neomake\" tests=\"{tests}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\" time=\"{time:.3}\">"
    );
    let _ = writeln!(
        xml,
        "  <testsuite name=\"neomake\" tests=\"{tests}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\" time=\"{time:.3}\" timestamp=\"{}\">",
        rfc3339(meta.started_at).trim_end_matches('Z')
    );
    for c in cases {
        xml.push_str(&c);
    }
    xml.push_str("  </testsuite>\n</testsuites>\n");
    xml
}

enum Kind {
    Passed,
    Failure,
    Error,
    Skipped,
}

fn case(outcome: &TaskOutcome, task: Option<&Task>, kind: Kind, meta: &RunMeta) -> String {
    let location = task.and_then(|t| t.source.as_ref()).map(|l| {
        let file = l.file.strip_prefix(&meta.root).unwrap_or(&l.file);
        (file.display().to_string(), l.line)
    });
    let classname = location
        .as_ref()
        .map(|(file, _)| {
            let stem = file.rsplit_once('.').map_or(file.as_str(), |(s, _)| s);
            stem.replace(['/', '\\'], ".")
        })
        .unwrap_or_else(|| "neomake".into());
    let mut attrs = format!(
        "name=\"{}\" classname=\"{}\" time=\"{:.3}\"",
        escape(&outcome.name),
        escape(&classname),
        outcome.duration.as_secs_f64()
    );
    if let Some((file, line)) = &location {
        let _ = write!(attrs, " file=\"{}\" line=\"{line}\"", escape(file));
    }
    let mut body = String::new();
    let message = outcome.error.clone().unwrap_or_default();
    let details = || {
        let mut d = String::new();
        if let Some(log) = &outcome.log {
            let _ = writeln!(
                d,
                "log: {}",
                log.strip_prefix(&meta.root).unwrap_or(log).display()
            );
        }
        for line in &outcome.tail {
            let _ = writeln!(d, "{line}");
        }
        d
    };
    match kind {
        Kind::Passed => {}
        Kind::Failure => {
            let _ = writeln!(
                body,
                "      <failure message=\"{}\" type=\"{}\">{}</failure>",
                escape(&message),
                outcome.status.label(),
                escape(&details())
            );
        }
        Kind::Error => {
            let _ = writeln!(
                body,
                "      <error message=\"{}\" type=\"{}\">{}</error>",
                escape(&message),
                outcome.status.label(),
                escape(&details())
            );
        }
        Kind::Skipped => {
            let _ = writeln!(body, "      <skipped message=\"{}\"/>", escape(&message));
        }
    }
    if !outcome.steps.is_empty() {
        let mut out = String::new();
        let mut section = None;
        for step in &outcome.steps {
            if step.section != section {
                if let Some(title) = &step.section {
                    let _ = writeln!(out, "== {title}");
                }
                section = step.section.clone();
            }
            let mark = match step.status {
                StepStatus::Passed => "ok  ",
                StepStatus::Failed => "FAIL",
                StepStatus::NotRun => "--  ",
            };
            let _ = write!(out, "{mark} {:>3}. {}", step.index, step.label);
            if let Some(m) = &step.message {
                let _ = write!(out, ": {m}");
            }
            out.push('\n');
        }
        let _ = writeln!(body, "      <system-out>{}</system-out>", escape(&out));
    }
    if body.is_empty() {
        format!("    <testcase {attrs}/>\n")
    } else {
        format!("    <testcase {attrs}>\n{body}    </testcase>\n")
    }
}

/// `text` as XML character data or an attribute value. Characters XML
/// cannot hold at all (most control characters) become `\u{…}`.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' | '\t' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{fffe}' || c == '\u{ffff}' => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::SkipReason;
    use crate::steps::StepOutcome;
    use crate::task::{Location, TaskKind};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn outcome(name: &str, status: TaskStatus) -> TaskOutcome {
        TaskOutcome {
            name: name.into(),
            status,
            cache_key: None,
            exit_code: None,
            duration: Duration::from_millis(1500),
            error: Some("step 2/2 (tests/cli.tomlx:9) `run: x`: expected \"<a>\"".into()),
            attempts: 1,
            started_at: None,
            log: None,
            tail: vec!["bad \u{1b}[31m output".into()],
            allowed_failure: false,
            steps: vec![StepOutcome {
                index: 1,
                label: "run: dave".into(),
                section: Some("help".into()),
                line: Some(8),
                status: StepStatus::Passed,
                duration: Duration::ZERO,
                message: None,
            }],
            test_dir: None,
        }
    }

    #[test]
    fn renders_tests_and_failed_tasks() {
        let mut test = Task::new("cli", "");
        test.kind = TaskKind::Test;
        test.source = Some(Location {
            file: PathBuf::from("/p/tests/cli.tomlx"),
            line: 3,
        });
        let mut skipped = Task::new("vm", "x");
        skipped.kind = TaskKind::Test;
        let tasks = vec![test, skipped, Task::new("build", "b"), Task::new("ok", "o")];
        let run = RunReport {
            outcomes: vec![
                outcome("build", TaskStatus::Failed),
                outcome("ok", TaskStatus::Success),
                outcome("cli", TaskStatus::Failed),
                outcome(
                    "vm",
                    TaskStatus::Skipped(SkipReason::Unmet("qemu not found".into())),
                ),
            ],
        };
        let meta = RunMeta {
            id: "x".into(),
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_secs(2),
            interrupted: false,
            root: PathBuf::from("/p"),
        };
        let xml = render(&run, &tasks, &meta);
        assert!(
            xml.contains("tests=\"3\" failures=\"1\" errors=\"1\" skipped=\"1\""),
            "{xml}"
        );
        assert!(xml.contains("classname=\"tests.cli\""), "{xml}");
        assert!(xml.contains("file=\"tests/cli.tomlx\" line=\"3\""), "{xml}");
        assert!(xml.contains("&quot;&lt;a&gt;&quot;"), "{xml}");
        assert!(xml.contains("\\u{1b}[31m"), "{xml}");
        assert!(xml.contains("== help\nok     1. run: dave"), "{xml}");
        assert!(
            !xml.contains("name=\"ok\""),
            "successful tasks are left out: {xml}"
        );
    }
}
