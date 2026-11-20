//! Steps: what a test does, one at a time.
//!
//! A test is either one `command` or a list of `steps`. Each step does
//! one thing (runs a command, starts a section, and, in a machine test,
//! drives the machine) and may check what came of it: the exit status,
//! and what the output contains, lacks, matches or equals. A step can
//! capture part of its output into a variable that later steps use as
//! `{{name}}`, and can be repeated until it passes (`wait = "30s"`).
//!
//! The first step that fails ends the test, and the failure names the
//! step, where it is written and what was expected; the rest are
//! recorded as not run.

pub mod host;
#[cfg(unix)]
pub mod pty;
pub mod vars;

use std::time::Duration;

use regex::Regex;

/// One step of a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The line of the test's file the step is written on.
    pub line: Option<usize>,
    /// A name for the step, in place of one made from what it does.
    pub label: Option<String>,
    /// What it does.
    pub action: Action,
    /// What it checks.
    pub check: Check,
    /// How long it may take.
    pub timeout: Option<Duration>,
    /// Repeat the step until its checks pass, for at most this long.
    pub wait: Option<Wait>,
}

/// How a step is repeated until it passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    /// Give up after this long.
    pub within: Duration,
    /// Pause between attempts.
    pub interval: Duration,
}

/// What a step does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start a new section of the test.
    Section(String),
    /// Run a command: in the machine, in a machine test; on the host
    /// otherwise.
    Run {
        /// The script.
        script: String,
        /// Run it on a pseudo-terminal.
        tty: Option<Tty>,
    },
    /// Run a command on the host.
    Host {
        /// The script.
        script: String,
        /// Run it on a pseudo-terminal.
        tty: Option<Tty>,
    },
    /// Wait for the machine's console to show a pattern.
    Expect {
        /// The regular expression.
        pattern: String,
        /// How many times it must appear, one after the other.
        times: u32,
        /// What its groups must be, in order (at the last appearance).
        groups: Vec<String>,
        /// Variables to set from its groups, in order.
        capture: Vec<String>,
    },
    /// Type text on the machine's console, as it is.
    Send(String),
    /// Start the machine and wait for it to boot.
    Boot,
    /// Start the machine without waiting.
    Start,
    /// Wait for a boot: the firmware's banner, then the prompt.
    WaitBoot,
    /// Wait for the prompt.
    WaitPrompt,
    /// Reboot cleanly, and wait for the next boot unless `wait` is false.
    Reboot {
        /// Wait for the next boot.
        wait: bool,
    },
    /// Pull the plug, start it again, and wait for the boot unless
    /// `wait` is false.
    PowerCut {
        /// Wait for the next boot.
        wait: bool,
    },
    /// Pull the plug.
    Kill,
    /// Power off cleanly, and wait for the machine to stop.
    Shutdown,
    /// Write a file in the machine.
    Write {
        /// Where.
        path: String,
        /// What.
        content: WriteSource,
        /// Its mode, as `chmod` takes it.
        mode: Option<String>,
    },
    /// Save what the machine's display shows, as a PNG file in the
    /// test's directory.
    Screenshot(String),
    /// Attach a medium, from the next start on.
    Attach(crate::machine::Media),
    /// Detach a medium, from the next start on.
    Detach(String),
}

/// What a `write` step writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteSource {
    /// This text.
    Inline(String),
    /// The contents of this file (relative to the project root).
    File(String),
}

impl Action {
    /// Whether the step runs a command whose output can be checked.
    pub fn runs_a_command(&self) -> bool {
        matches!(self, Action::Run { .. } | Action::Host { .. })
    }

    /// Whether the step needs a machine.
    pub fn needs_a_machine(&self) -> bool {
        !matches!(
            self,
            Action::Section(_) | Action::Run { .. } | Action::Host { .. }
        )
    }
}

/// What a step expects of the command it ran.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Check {
    /// The exit status.
    pub exit: ExitCheck,
    /// Which output the text checks look at.
    pub stream: StreamSel,
    /// Text the output must contain.
    pub contains: Vec<String>,
    /// Text the output must not contain.
    pub lacks: Vec<String>,
    /// Regular expressions (in multi-line mode, so `^` and `$` match at
    /// every line) the output must match.
    pub matches: Vec<String>,
    /// What the output must be, leading and trailing whitespace aside.
    pub equals: Option<String>,
    /// Variables to set from the output.
    pub captures: Vec<Capture>,
}

impl Check {
    /// `true` when the step checks nothing but a zero exit status.
    pub fn is_default(&self) -> bool {
        *self == Check::default()
    }
}

/// The exit status a step expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExitCheck {
    /// Exactly this.
    Code(i32),
    /// Anything but 0.
    Nonzero,
    /// Anything.
    Any,
    /// 0 (the default).
    #[default]
    Zero,
}

impl ExitCheck {
    fn accepts(&self, status: i32) -> bool {
        match self {
            ExitCheck::Zero => status == 0,
            ExitCheck::Code(c) => status == *c,
            ExitCheck::Nonzero => status != 0,
            ExitCheck::Any => true,
        }
    }

    fn describe(&self) -> String {
        match self {
            ExitCheck::Zero => "0".into(),
            ExitCheck::Code(c) => c.to_string(),
            ExitCheck::Nonzero => "non-zero".into(),
            ExitCheck::Any => "any".into(),
        }
    }
}

/// Which output the text checks look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamSel {
    /// Standard output (the default).
    #[default]
    Stdout,
    /// Standard error.
    Stderr,
    /// Both, interleaved as they arrived.
    Combined,
}

impl StreamSel {
    fn name(&self) -> &'static str {
        match self {
            StreamSel::Stdout => "stdout",
            StreamSel::Stderr => "stderr",
            StreamSel::Combined => "the output",
        }
    }
}

/// A variable set from a step's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    /// The variable.
    pub name: String,
    /// A regular expression; its first group (or else its whole match)
    /// is the value. Without one, the whole output, trimmed.
    pub pattern: Option<String>,
}

/// Running a step on a pseudo-terminal, for commands that behave
/// differently on one (questions, colour).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tty {
    /// Answers to give when the output shows a pattern.
    pub respond: Vec<Respond>,
    /// Patterns the output must never show.
    pub forbid: Vec<String>,
}

/// An answer typed when a pattern appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Respond {
    /// A regular expression to wait for.
    pub on: String,
    /// What to type.
    pub send: String,
}

/// What a command printed and how it ended.
#[derive(Debug, Clone, Default)]
pub struct Output {
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Both, in the order lines arrived.
    pub combined: String,
    /// The exit status (128 plus the signal number for a signal).
    pub status: i32,
}

impl Output {
    fn stream(&self, sel: StreamSel) -> &str {
        match sel {
            StreamSel::Stdout => &self.stdout,
            StreamSel::Stderr => &self.stderr,
            StreamSel::Combined => &self.combined,
        }
    }
}

impl Check {
    /// Check `out`. On success, the captured variables; on failure, the
    /// first expectation that did not hold, in words.
    pub fn verify(&self, out: &Output) -> Result<Vec<(String, String)>, String> {
        if !self.exit.accepts(out.status) {
            return Err(format!(
                "expected exit status {}, got {}",
                self.exit.describe(),
                out.status
            ));
        }
        let text = out.stream(self.stream);
        let where_ = self.stream.name();
        for needle in &self.contains {
            if !text.contains(needle.as_str()) {
                return Err(format!("expected {where_} to contain {needle:?}"));
            }
        }
        for needle in &self.lacks {
            if text.contains(needle.as_str()) {
                return Err(format!("expected {where_} not to contain {needle:?}"));
            }
        }
        for pattern in &self.matches {
            let re = compile(pattern)?;
            if !re.is_match(text) {
                return Err(format!("expected {where_} to match /{pattern}/"));
            }
        }
        if let Some(expected) = &self.equals {
            if text.trim() != expected.trim() {
                return Err(format!(
                    "expected {where_} to be {:?}, got {:?}",
                    expected.trim(),
                    text.trim()
                ));
            }
        }
        let mut captured = Vec::new();
        for capture in &self.captures {
            let value = match &capture.pattern {
                None => text.trim().to_string(),
                Some(pattern) => {
                    let re = compile(pattern)?;
                    let caps = re.captures(text).ok_or_else(|| {
                        format!(
                            "nothing in {where_} matches /{pattern}/ to capture `{}`",
                            capture.name
                        )
                    })?;
                    caps.get(1)
                        .or_else(|| caps.get(0))
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default()
                }
            };
            captured.push((capture.name.clone(), value));
        }
        Ok(captured)
    }
}

/// A pattern compiled the way every step check uses it: multi-line, so
/// `^` and `$` match at line boundaries.
pub fn compile(pattern: &str) -> Result<Regex, String> {
    Regex::new(&format!("(?m){pattern}")).map_err(|e| format!("invalid pattern /{pattern}/: {e}"))
}

impl Step {
    /// A short description of what the step does, for progress and
    /// reports.
    pub fn describe(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        match &self.action {
            Action::Section(title) => title.clone(),
            Action::Run { script, .. } => format!("run: {}", summarize(script)),
            Action::Host { script, .. } => format!("host: {}", summarize(script)),
            Action::Expect { pattern, times, .. } if *times > 1 => {
                format!("expect /{pattern}/ {times} times")
            }
            Action::Expect { pattern, .. } => format!("expect /{pattern}/"),
            Action::Send(text) => format!("send {:?}", summarize(text)),
            Action::Boot => "boot".into(),
            Action::Start => "start".into(),
            Action::WaitBoot => "wait for a boot".into(),
            Action::WaitPrompt => "wait for the prompt".into(),
            Action::Reboot { wait: true } => "reboot".into(),
            Action::Reboot { wait: false } => "reboot (without waiting)".into(),
            Action::PowerCut { wait: true } => "power cut".into(),
            Action::PowerCut { wait: false } => "power cut (without waiting)".into(),
            Action::Kill => "pull the plug".into(),
            Action::Shutdown => "shut down".into(),
            Action::Write { path, .. } => format!("write {path}"),
            Action::Screenshot(name) => format!("screenshot {name}"),
            Action::Attach(m) => format!("attach {} ({})", m.name, m.file),
            Action::Detach(name) => format!("detach {name}"),
        }
    }

    /// Every string of the step, for substitution.
    pub fn strings_mut(&mut self) -> Vec<&mut String> {
        let mut out: Vec<&mut String> = Vec::new();
        if let Some(label) = &mut self.label {
            out.push(label);
        }
        match &mut self.action {
            Action::Section(title) => out.push(title),
            Action::Run { script, tty } | Action::Host { script, tty } => {
                out.push(script);
                if let Some(tty) = tty {
                    for r in &mut tty.respond {
                        out.push(&mut r.on);
                        out.push(&mut r.send);
                    }
                    out.extend(tty.forbid.iter_mut());
                }
            }
            Action::Expect {
                pattern, groups, ..
            } => {
                out.push(pattern);
                out.extend(groups.iter_mut());
            }
            Action::Send(text) | Action::Screenshot(text) | Action::Detach(text) => out.push(text),
            Action::Write {
                path,
                content,
                mode,
            } => {
                out.push(path);
                match content {
                    WriteSource::Inline(s) | WriteSource::File(s) => out.push(s),
                }
                if let Some(m) = mode {
                    out.push(m);
                }
            }
            Action::Attach(m) => {
                out.push(&mut m.name);
                out.push(&mut m.file);
            }
            Action::Boot
            | Action::Start
            | Action::WaitBoot
            | Action::WaitPrompt
            | Action::Reboot { .. }
            | Action::PowerCut { .. }
            | Action::Kill
            | Action::Shutdown => {}
        }
        let c = &mut self.check;
        out.extend(c.contains.iter_mut());
        out.extend(c.lacks.iter_mut());
        out.extend(c.matches.iter_mut());
        if let Some(e) = &mut c.equals {
            out.push(e);
        }
        for cap in &mut c.captures {
            if let Some(p) = &mut cap.pattern {
                out.push(p);
            }
        }
        out
    }
}

/// The first line of `script`, shortened.
pub fn summarize(script: &str) -> String {
    let first = script.trim().lines().next().unwrap_or("");
    let more = script.trim().lines().count() > 1;
    let mut s: String = first.chars().take(72).collect();
    if more || first.chars().count() > 72 {
        s.push('…');
    }
    s
}

/// How one step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// Its checks held.
    Passed,
    /// It failed; the test ended here.
    Failed,
    /// An earlier step failed, or the run stopped.
    NotRun,
}

/// The record of one step.
#[derive(Debug, Clone)]
pub struct StepOutcome {
    /// Position, from 1.
    pub index: usize,
    /// What the step does.
    pub label: String,
    /// The section it belongs to.
    pub section: Option<String>,
    /// Where it is written.
    pub line: Option<usize>,
    /// How it ended.
    pub status: StepStatus,
    /// How long it took.
    pub duration: Duration,
    /// Why it failed.
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(stdout: &str, status: i32) -> Output {
        Output {
            stdout: stdout.into(),
            stderr: String::new(),
            combined: stdout.into(),
            status,
        }
    }

    #[test]
    fn verifies_exit_and_text() {
        let check = Check {
            contains: vec!["2 to install.".into()],
            lacks: vec!["error".into()],
            matches: vec![r"^  hello +1\.0-r1$".into()],
            ..Check::default()
        };
        let ok = out("plan:\n  hello   1.0-r1\n2 to install.\n", 0);
        assert!(check.verify(&ok).is_ok());
        assert_eq!(
            check.verify(&out("plan:\n", 0)).unwrap_err(),
            "expected stdout to contain \"2 to install.\""
        );
        assert!(check
            .verify(&out("  hello   1.0-r1\n2 to install.\nerror", 0))
            .unwrap_err()
            .contains("not to contain"));
        assert!(check
            .verify(&out("hello 1.0-r1\n2 to install.", 0))
            .unwrap_err()
            .contains("match"));
        assert_eq!(
            check.verify(&out("", 3)).unwrap_err(),
            "expected exit status 0, got 3"
        );
    }

    #[test]
    fn exit_expectations() {
        let any = Check {
            exit: ExitCheck::Nonzero,
            ..Check::default()
        };
        assert!(any.verify(&out("", 1)).is_ok());
        assert!(any.verify(&out("", 0)).is_err());
        let exact = Check {
            exit: ExitCheck::Code(77),
            ..Check::default()
        };
        assert!(exact.verify(&out("", 77)).is_ok());
    }

    #[test]
    fn equals_ignores_surrounding_whitespace() {
        let check = Check {
            equals: Some("hello v1".into()),
            ..Check::default()
        };
        assert!(check.verify(&out("hello v1\n", 0)).is_ok());
        assert!(check.verify(&out("hello v2\n", 0)).is_err());
    }

    #[test]
    fn captures_groups_or_everything() {
        let check = Check {
            captures: vec![
                Capture {
                    name: "kernel".into(),
                    pattern: Some(r"^linux (\S+)$".into()),
                },
                Capture {
                    name: "all".into(),
                    pattern: None,
                },
            ],
            ..Check::default()
        };
        let got = check
            .verify(&out("title joe\nlinux /joe/vmlinuz\n", 0))
            .unwrap();
        assert_eq!(got[0], ("kernel".into(), "/joe/vmlinuz".into()));
        assert_eq!(got[1].1, "title joe\nlinux /joe/vmlinuz");
        assert!(check.verify(&out("nothing", 0)).is_err());
    }

    #[test]
    fn describes_steps() {
        let step = Step {
            line: None,
            label: None,
            action: Action::Run {
                script: "dave install hello\nmore".into(),
                tty: None,
            },
            check: Check::default(),
            timeout: None,
            wait: None,
        };
        assert_eq!(step.describe(), "run: dave install hello…");
    }
}
