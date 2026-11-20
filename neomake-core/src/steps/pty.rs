//! Running a command on a pseudo-terminal.
//!
//! Some behaviour exists only on a terminal: dave asks `Continue? [Y/n]`
//! only when its input and output are one, and colours its output only
//! then. A `tty` step runs its command as the session leader of a new
//! pseudo-terminal (80 columns by 24 rows), types the answers it is
//! given when their patterns appear, and fails if a forbidden pattern
//! appears. The terminal merges stdout and stderr; `\r\n` is read as
//! `\n`.
//!
//! This is a blocking function, run on tokio's blocking pool.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::{compile, Tty};
use crate::process::{Exit, Invocation, GRACE};

/// What a command on a terminal printed and how it ended.
pub struct PtyResult {
    /// Everything it printed (and the answers typed, which the terminal
    /// echoes).
    pub output: String,
    /// How it ended.
    pub exit: Exit,
    /// The forbidden pattern it showed, if any.
    pub forbidden: Option<String>,
}

/// Run `invocation` on a new pseudo-terminal, passing each line it prints
/// to `on_line`.
pub fn run(
    invocation: &Invocation,
    tty: &Tty,
    timeout: Option<Duration>,
    cancel: &CancellationToken,
    on_line: &mut dyn FnMut(String),
) -> io::Result<PtyResult> {
    let mut responses = tty
        .respond
        .iter()
        .map(|r| {
            Ok((
                compile(&r.on).map_err(io::Error::other)?,
                r.send.clone(),
                false,
            ))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let forbid = tty
        .forbid
        .iter()
        .map(|p| Ok((compile(p).map_err(io::Error::other)?, p.clone())))
        .collect::<io::Result<Vec<_>>>()?;

    let (master, slave) = open_pty()?;
    let mut master = master;

    let mut cmd = Command::new(&invocation.program);
    cmd.args(&invocation.args).current_dir(&invocation.cwd);
    if invocation.env_clear {
        cmd.env_clear();
    }
    cmd.envs(&invocation.env)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // The parent's copies of the terminal's other end must be closed, or
    // reading would never see it hang up.
    drop(cmd);
    let pid = child.id() as libc::pid_t;

    let started = Instant::now();
    let mut raw: Vec<u8> = Vec::new();
    let mut pending_line: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut status = None;
    let mut exited_at: Option<Instant> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut stop_sent: Option<Instant> = None;

    loop {
        if readable(master_fd(&master), 50)? {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    for &b in &buf[..n] {
                        if b == b'\n' {
                            on_line(clean_line(&pending_line));
                            pending_line.clear();
                        } else {
                            pending_line.push(b);
                        }
                    }
                    let text = String::from_utf8_lossy(&raw);
                    for (re, send, sent) in responses.iter_mut() {
                        if !*sent && re.is_match(&text) {
                            master.write_all(send.as_bytes())?;
                            *sent = true;
                        }
                    }
                }
                // The other end hung up: everything has been read.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if status.is_none() {
            status = child.try_wait()?;
            if status.is_some() {
                exited_at = Some(Instant::now());
            }
        }
        if let Some(at) = exited_at {
            // Something that escaped may hold the terminal open; give it
            // a moment, then stop reading.
            if at.elapsed() > Duration::from_secs(1) {
                break;
            }
            continue;
        }
        let over_time = timeout.is_some_and(|t| started.elapsed() >= t);
        if stop_sent.is_none() && (over_time || cancel.is_cancelled()) {
            timed_out = over_time;
            cancelled = !over_time;
            signal_group(pid, libc::SIGTERM);
            stop_sent = Some(Instant::now());
        }
        if stop_sent.is_some_and(|at| at.elapsed() > GRACE) {
            signal_group(pid, libc::SIGKILL);
        }
    }
    if !pending_line.is_empty() {
        on_line(clean_line(&pending_line));
    }
    let status = match status {
        Some(s) => s,
        None => child.wait()?,
    };
    signal_group(pid, libc::SIGKILL);

    let output = String::from_utf8_lossy(&raw).replace("\r\n", "\n");
    let forbidden = forbid
        .into_iter()
        .find(|(re, _)| re.is_match(&output))
        .map(|(_, p)| p);
    use std::os::unix::process::ExitStatusExt;
    Ok(PtyResult {
        output,
        exit: Exit {
            code: status.code(),
            signal: status.signal(),
            timed_out,
            cancelled,
        },
        forbidden,
    })
}

fn clean_line(bytes: &[u8]) -> String {
    let line = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(line).into_owned()
}

fn open_pty() -> io::Result<(File, File)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two descriptors; the name and termios
    // arguments may be null.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size as *mut libc::winsize,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just opened and are owned here.
    unsafe { Ok((File::from_raw_fd(master), File::from_raw_fd(slave))) }
}

fn master_fd(master: &File) -> RawFd {
    use std::os::fd::AsRawFd;
    master.as_raw_fd()
}

fn readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(e);
    }
    Ok(r > 0 && pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
}

fn signal_group(pid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill(2) has no memory-safety preconditions.
    unsafe {
        libc::kill(-pid, signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steps::Respond;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

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

    #[test]
    fn answers_questions_and_sees_a_terminal() {
        let tty = Tty {
            respond: vec![Respond {
                on: r"\[Y/n\]".into(),
                send: "n\n".into(),
            }],
            forbid: vec![],
        };
        let script = "[ -t 0 ] && [ -t 1 ] && echo on-a-terminal; printf 'Continue? [Y/n] '; read a; echo \"got $a\"";
        let mut lines = Vec::new();
        let r = run(
            &sh(script),
            &tty,
            Some(Duration::from_secs(10)),
            &CancellationToken::new(),
            &mut |l| lines.push(l),
        )
        .unwrap();
        assert!(r.exit.success(), "{}", r.output);
        assert!(r.output.contains("on-a-terminal\n"), "{}", r.output);
        assert!(r.output.contains("got n"), "{}", r.output);
        assert!(lines.iter().any(|l| l == "got n"), "{lines:?}");
    }

    #[test]
    fn reports_forbidden_output_and_timeouts() {
        let tty = Tty {
            respond: vec![],
            forbid: vec![r"\[Y/n\]".into()],
        };
        let r = run(
            &sh("echo 'Continue? [Y/n]'"),
            &tty,
            None,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r.forbidden.as_deref(), Some(r"\[Y/n\]"));

        let started = Instant::now();
        let r = run(
            &sh("sleep 30"),
            &Tty::default(),
            Some(Duration::from_millis(200)),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert!(r.exit.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
