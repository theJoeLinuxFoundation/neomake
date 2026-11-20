//! Running a machine test's steps.
//!
//! The machine is laid out in the test's directory (its disks copied),
//! started, and waited for until its shell's prompt appears, unless the
//! test says `autoboot = false`. Then each step runs: `run` in the
//! machine's shell, `host` on the host, and the machine's own steps on
//! the machine. When a step fails while the machine still runs, the
//! test's `on_failure` steps run to gather what shows why (their output
//! goes to the log; their own failures do not count). The machine is
//! stopped when the test ends, and the disk copies of a test that passed
//! are deleted; a failed test's are kept, with its console logs, for
//! looking into.

use std::time::{Duration, Instant};

use regex::bytes::Regex;

use neomake_core::executor::Sink;
use neomake_core::machine::{Kvm, MachineEnv};
use neomake_core::process::Stream;
use neomake_core::steps::host::{
    pause, resolve, run_host_step, step_deadline, Ending, Runner, StepFailure, StepsResult,
};
use neomake_core::steps::{Action, Output, Step, WriteSource};
use neomake_core::Task;

use crate::machine::{quote, ConsoleError, Machine};
use crate::{host, QemuRunner};

/// Run `task` in its machine.
pub async fn run_test(
    r: &QemuRunner,
    task: &Task,
    mut env: MachineEnv<'_>,
    sink: &Sink,
) -> StepsResult {
    let spec = task.machine.as_ref().expect("only machine tests come here");
    let kvm = host::use_kvm(spec, r.no_kvm);
    if !kvm && spec.kvm == Kvm::Prefer {
        sink.note("--- no KVM here: the machine is emulated, which is slow".into());
    }
    let mut m = match Machine::prepare(spec, &env.root, &env.dir, kvm) {
        Ok(m) => m,
        Err(message) => return StepsResult::before_steps(Ending::Failed, message),
    };
    m.on_boot = task.on_boot.clone();

    let vars = &mut env.host.vars;
    vars.set("machine.name", spec.name.clone());
    vars.set("machine.dir", m.dir.display().to_string());
    vars.set(
        "machine.console",
        m.dir.join("console.log").display().to_string(),
    );
    if let Some(first) = m.layout.disks.first() {
        vars.set("machine.disk", first.display().to_string());
    }
    for (disk, path) in spec.disks.iter().zip(&m.layout.disks) {
        if let Some(name) = &disk.name {
            vars.set(format!("machine.disks.{name}"), path.display().to_string());
        }
    }

    let cancel = env.host.cancel.clone();
    if task.autoboot {
        sink.note(format!("--- booting machine `{}`", spec.name));
        let boot = async {
            m.start().await.map_err(ConsoleError::Other)?;
            m.wait_boot(within(spec.boot_timeout, env.host.deadline))
                .await
        };
        let booted = tokio::select! {
            r = boot => r,
            _ = cancel.cancelled() => Err(ConsoleError::Other("the run was cancelled".into())),
        };
        if let Err(e) = booted {
            let ending = if cancel.is_cancelled() {
                Ending::Cancelled
            } else if past(env.host.deadline) {
                Ending::TimedOut
            } else {
                Ending::Failed
            };
            m.stop().await;
            return StepsResult::before_steps(
                ending,
                format!("machine `{}` did not boot: {}", spec.name, e.message()),
            );
        }
    }

    let file = env.file.clone();
    let mut runner = Runner::new(&task.steps, file.as_deref());
    let mut result = None;
    for (i, step) in task.steps.iter().enumerate() {
        if !runner.begin(i, step, &cancel, sink) {
            continue;
        }
        let started = Instant::now();
        let outcome = tokio::select! {
            r = step_in_machine(&mut m, step, &mut env, sink) => r,
            _ = cancel.cancelled() => Err(StepFailure {
                ending: Ending::Cancelled,
                message: "the run was cancelled".into(),
                exit_code: None,
            }),
        };
        if let Some(r) = runner.finish(step, started, outcome, sink) {
            result = Some(r);
            break;
        }
    }
    let result = result.unwrap_or_else(|| runner.done());

    if result.ending == Ending::Failed && m.running() && !task.on_failure.is_empty() {
        sink.note("--- the test failed; gathering what shows why (on_failure)".into());
        for step in &task.on_failure {
            let outcome = match &step.action {
                Action::Host { .. } => run_host_step(step, &mut env.host, sink).await,
                Action::Run { .. } => step_in_machine(&mut m, step, &mut env, sink).await,
                _ => Ok(()),
            };
            if let Err(e) = outcome {
                sink.note(format!(
                    "--- on_failure `{}`: {}",
                    step.describe(),
                    e.message
                ));
            }
        }
    }
    m.stop().await;
    if result.ending == Ending::Passed {
        m.discard_disks();
    }
    result
}

fn past(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// `default`, cut short by `deadline`.
fn within(default: Duration, deadline: Option<Instant>) -> Duration {
    match deadline {
        Some(d) => default.min(d.saturating_duration_since(Instant::now())),
        None => default,
    }
}

async fn step_in_machine(
    m: &mut Machine,
    step: &Step,
    env: &mut MachineEnv<'_>,
    sink: &Sink,
) -> Result<(), StepFailure> {
    if let Action::Host { .. } = step.action {
        return run_host_step(step, &mut env.host, sink).await;
    }
    let resolved = resolve(step, &env.host.vars)?;
    let test_deadline = env.host.deadline;
    let deadline = step_deadline(&resolved, test_deadline);
    let limit = |default: Duration| match resolved.timeout {
        Some(_) => deadline.map_or(default, |d| d.saturating_duration_since(Instant::now())),
        None => within(default, test_deadline),
    };
    let fail = |e: ConsoleError| StepFailure {
        ending: match e {
            ConsoleError::Timeout(_) if past(test_deadline) => Ending::TimedOut,
            _ => Ending::Failed,
        },
        message: e.message().to_string(),
        exit_code: None,
    };
    let command_timeout = m.spec.command_timeout;
    let boot_timeout = m.spec.boot_timeout;

    match &resolved.action {
        Action::Run { script, .. } => {
            let waiting = resolved.wait.map(|w| (Instant::now() + w.within, w));
            loop {
                let (out, status) = m.run(script, limit(command_timeout)).await.map_err(fail)?;
                for line in out.lines() {
                    sink.line(Stream::Stdout, line.to_string());
                }
                let output = Output {
                    stdout: out.clone(),
                    stderr: String::new(),
                    combined: out,
                    status,
                };
                match resolved.check.verify(&output) {
                    Ok(captured) => {
                        for (name, value) in captured {
                            env.host.vars.set(name, value);
                        }
                        return Ok(());
                    }
                    Err(message) => {
                        if let Some((until, wait)) = waiting {
                            if Instant::now() + wait.interval < until {
                                if !pause(wait.interval, env.host.cancel).await {
                                    return Err(StepFailure {
                                        ending: Ending::Cancelled,
                                        message: "the run was cancelled".into(),
                                        exit_code: None,
                                    });
                                }
                                continue;
                            }
                            return Err(StepFailure {
                                exit_code: Some(status),
                                ..StepFailure::failed(format!(
                                    "{message} (still, after {})",
                                    neomake_core::duration::human(wait.within)
                                ))
                            });
                        }
                        return Err(StepFailure {
                            exit_code: Some(status),
                            ..StepFailure::failed(message)
                        });
                    }
                }
            }
        }
        Action::Expect {
            pattern,
            times,
            groups,
            capture,
        } => {
            let re = Regex::new(&format!("(?m){pattern}"))
                .map_err(|e| StepFailure::failed(format!("invalid pattern /{pattern}/: {e}")))?;
            let mut got = Vec::new();
            for n in 1..=*times {
                got = m.expect(&re, limit(command_timeout)).await.map_err(|e| {
                    let mut f = fail(e);
                    if *times > 1 {
                        f.message = format!("appearance {n} of {times}: {}", f.message);
                    }
                    f
                })?;
                let shown = got[0].clone().unwrap_or_default();
                sink.note(format!("matched: {}", shown.trim_end()));
                if !groups.is_empty() {
                    let actual: Vec<String> = got
                        .iter()
                        .skip(1)
                        .map(|g| g.clone().unwrap_or_default())
                        .collect();
                    let expected: Vec<String> = groups.clone();
                    if actual.len() < expected.len() || actual[..expected.len()] != expected[..] {
                        return Err(StepFailure::failed(format!(
                            "expected the groups of /{pattern}/ to be {expected:?}, got {actual:?} in {:?}",
                            shown.trim_end()
                        )));
                    }
                }
            }
            for (i, name) in capture.iter().enumerate() {
                let value = got.get(i + 1).cloned().flatten().ok_or_else(|| {
                    StepFailure::failed(format!(
                        "/{pattern}/ has no group {} to capture `{name}`",
                        i + 1
                    ))
                })?;
                env.host.vars.set(name.clone(), value);
            }
            Ok(())
        }
        Action::Send(text) => m.send(text).await.map_err(fail),
        Action::Boot => {
            m.start().await.map_err(StepFailure::failed)?;
            m.wait_boot(limit(boot_timeout)).await.map_err(fail)
        }
        Action::Start => m.start().await.map_err(StepFailure::failed),
        Action::WaitBoot => m.wait_boot(limit(boot_timeout)).await.map_err(fail),
        Action::WaitPrompt => m.wait_prompt(limit(boot_timeout)).await.map_err(fail),
        Action::Reboot { wait } => {
            m.clear();
            let command = format!("{}\n", m.spec.console.reboot);
            m.send(&command).await.map_err(fail)?;
            if *wait {
                m.wait_boot(limit(boot_timeout)).await.map_err(fail)?;
            }
            Ok(())
        }
        Action::PowerCut { wait } => {
            m.stop().await;
            m.start().await.map_err(StepFailure::failed)?;
            if *wait {
                m.wait_boot(limit(boot_timeout)).await.map_err(fail)?;
            }
            Ok(())
        }
        Action::Kill => {
            m.stop().await;
            Ok(())
        }
        Action::Shutdown => m.shutdown(limit(boot_timeout)).await.map_err(fail),
        Action::Write {
            path,
            content,
            mode,
        } => {
            let bytes = match content {
                WriteSource::Inline(text) => text.clone().into_bytes(),
                WriteSource::File(file) => {
                    let host_path = env.root.join(file);
                    std::fs::read(&host_path).map_err(|e| {
                        StepFailure::failed(format!("cannot read {}: {e}", host_path.display()))
                    })?
                }
            };
            m.write_file(path, &bytes, limit(command_timeout))
                .await
                .map_err(fail)?;
            if let Some(mode) = mode {
                let (out, status) = m
                    .run(
                        &format!("chmod {mode} {}", quote(path)),
                        limit(command_timeout),
                    )
                    .await
                    .map_err(fail)?;
                if status != 0 {
                    return Err(StepFailure::failed(format!(
                        "chmod {mode} {path} exited with {status}: {}",
                        out.trim_end()
                    )));
                }
            }
            Ok(())
        }
        Action::Screenshot(name) => {
            let path = m.screenshot(name).await.map_err(fail)?;
            sink.note(format!("--- screenshot: {}", path.display()));
            Ok(())
        }
        Action::Attach(media) => {
            m.attach(media.clone());
            Ok(())
        }
        Action::Detach(name) => m.detach(name).map_err(StepFailure::failed),
        Action::Section(_) | Action::Host { .. } => Ok(()),
    }
}
