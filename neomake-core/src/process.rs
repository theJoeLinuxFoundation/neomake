//! Running one command: output streaming, timeouts and cancellation.
//!
//! On Unix every command is started in a process group of its own, and
//! the group is what gets signalled. When the command is stopped (its
//! timeout passed, or the run was cancelled) the group gets SIGTERM, then
//! SIGKILL after [`GRACE`]. When the command exits by itself, whatever it
//! left running in its group is killed: a task's processes do not outlive
//! it, and a background daemon left behind cannot hold the output pipes
//! open and stall the run.
//!
//! Output is read as bytes and split into lines, with invalid UTF-8
//! replaced rather than ending the stream.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How long a stopped command is given to exit after SIGTERM.
pub const GRACE: Duration = Duration::from_secs(5);

/// What to run, and how.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// The program.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Variables set on top of the inherited environment (or of an empty
    /// one, with `env_clear`).
    pub env: BTreeMap<String, String>,
    /// Start from an empty environment.
    pub env_clear: bool,
    /// Attach to the terminal: inherit stdin, stdout and stderr, and stay
    /// in neomake's process group so the terminal can be read.
    pub interactive: bool,
}

/// Which stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// Exit status, when it exited by itself.
    pub code: Option<i32>,
    /// The signal that ended it, on Unix.
    pub signal: Option<i32>,
    /// It was stopped because its timeout passed.
    pub timed_out: bool,
    /// It was stopped because the run was cancelled.
    pub cancelled: bool,
}

impl Exit {
    /// `true` for an exit status of 0.
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.cancelled
    }

    /// The status a shell would report: the exit code, or 128 plus the
    /// signal number.
    pub fn status(&self) -> i32 {
        match (self.code, self.signal) {
            (Some(c), _) => c,
            (None, Some(s)) => 128 + s,
            (None, None) => -1,
        }
    }

    /// The end described for a person: `exit code 3`, `killed by signal 9`.
    pub fn describe(&self) -> String {
        match (self.code, self.signal) {
            (Some(c), _) => format!("exit code {c}"),
            (None, Some(s)) => format!("killed by signal {s}"),
            (None, None) => "ended without a status".into(),
        }
    }
}

/// Run `invocation`, passing each line of output to `on_line`.
///
/// The command is stopped if `timeout` passes or `cancel` fires; the
/// returned [`Exit`] says which.
pub async fn run(
    invocation: &Invocation,
    timeout: Option<Duration>,
    cancel: &CancellationToken,
    mut on_line: impl FnMut(Stream, String) + Send,
) -> std::io::Result<Exit> {
    let mut cmd = tokio::process::Command::new(&invocation.program);
    cmd.args(&invocation.args).current_dir(&invocation.cwd);
    if invocation.env_clear {
        cmd.env_clear();
    }
    cmd.envs(&invocation.env);
    if invocation.interactive {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    } else {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        cmd.process_group(0);
    }
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let group = if invocation.interactive {
        None
    } else {
        child.id()
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<(Stream, String)>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(tokio::spawn(read_lines(out, Stream::Stdout, tx.clone())));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(tokio::spawn(read_lines(err, Stream::Stderr, tx.clone())));
    }
    drop(tx);

    let deadline = async {
        match timeout {
            Some(t) => tokio::time::sleep(t).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);

    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        tokio::select! {
            Some((stream, line)) = rx.recv() => on_line(stream, line),
            status = child.wait() => break status?,
            _ = &mut deadline, if !timed_out && !cancelled => {
                timed_out = true;
                stop(&mut child, group).await;
            }
            _ = cancel.cancelled(), if !timed_out && !cancelled => {
                cancelled = true;
                stop(&mut child, group).await;
            }
        }
    };

    // Anything still running in the group is an orphan of this command.
    kill_group(group, Signal::Kill);
    for reader in readers {
        // A reader can only be held up by a process that escaped the
        // group; don't wait for it forever.
        let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
    }
    while let Ok((stream, line)) = rx.try_recv() {
        on_line(stream, line);
    }

    Ok(Exit {
        code: status.code(),
        signal: signal_of(&status),
        timed_out,
        cancelled,
    })
}

async fn read_lines(
    reader: impl AsyncRead + Unpin,
    stream: Stream,
    tx: mpsc::UnboundedSender<(Stream, String)>,
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                let line = String::from_utf8_lossy(&buf).into_owned();
                if tx.send((stream, line)).is_err() {
                    break;
                }
            }
        }
    }
}

/// SIGTERM, then SIGKILL after [`GRACE`].
async fn stop(child: &mut tokio::process::Child, group: Option<u32>) {
    if group.is_some() && cfg!(unix) {
        kill_group(group, Signal::Term);
        if tokio::time::timeout(GRACE, child.wait()).await.is_ok() {
            return;
        }
        kill_group(group, Signal::Kill);
    }
    let _ = child.start_kill();
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn kill_group(group: Option<u32>, signal: Signal) {
    if let Some(pid) = group {
        let sig = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: kill(2) with a negative pid signals a process group;
        // it has no memory-safety preconditions.
        unsafe {
            libc::kill(-(pid as libc::pid_t), sig);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_group: Option<u32>, _signal: Signal) {}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sh(script: &str) -> Invocation {
        Invocation {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: PathBuf::from("."),
            env: BTreeMap::new(),
            env_clear: false,
            interactive: false,
        }
    }

    async fn collect(inv: &Invocation, timeout: Option<Duration>) -> (Exit, Vec<String>) {
        let mut lines = Vec::new();
        let exit = run(inv, timeout, &CancellationToken::new(), |_, l| {
            lines.push(l)
        })
        .await
        .unwrap();
        (exit, lines)
    }

    #[tokio::test]
    async fn streams_lines_and_reports_the_status() {
        let (exit, lines) = collect(
            &sh("echo one; echo two >&2; printf 'x\\377y'; exit 3"),
            None,
        )
        .await;
        assert_eq!(exit.code, Some(3));
        assert!(!exit.success());
        assert!(lines.contains(&"one".to_string()));
        assert!(lines.contains(&"two".to_string()));
        assert!(lines.contains(&"x\u{fffd}y".to_string()), "{lines:?}");
    }

    #[tokio::test]
    async fn a_timeout_stops_the_whole_group() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("survived");
        let script = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let started = Instant::now();
        let (exit, _) = collect(&sh(&script), Some(Duration::from_millis(300))).await;
        assert!(exit.timed_out);
        assert!(started.elapsed() < Duration::from_secs(4));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !marker.exists(),
            "a process of the group outlived the timeout"
        );
    }

    #[tokio::test]
    async fn background_processes_do_not_outlive_the_command() {
        let started = Instant::now();
        // The background sleep holds stdout open; without killing the
        // group, reading would wait for it.
        let (exit, lines) = collect(&sh("sleep 30 & echo done"), None).await;
        assert!(exit.success());
        assert_eq!(lines, vec!["done"]);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn cancellation_stops_the_command() {
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let exit = run(&sh("sleep 30"), None, &cancel, |_, _| {})
            .await
            .unwrap();
        assert!(exit.cancelled);
        assert_eq!(exit.signal, Some(libc::SIGTERM));
    }

    #[tokio::test]
    async fn clears_the_environment_when_asked() {
        let mut inv = sh("echo \"[${HOME:-}]\" \"$ONLY\"");
        inv.program = "/bin/sh".into();
        inv.env_clear = true;
        inv.env.insert("ONLY".into(), "this".into());
        let (_, lines) = collect(&inv, None).await;
        assert_eq!(lines, vec!["[] this"]);
    }
}
