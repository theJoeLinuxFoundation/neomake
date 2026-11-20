//! A running machine and its serial console.
//!
//! QEMU's serial console is its standard input and output. Everything the
//! machine prints is appended to `console.log` (as it is) and to
//! `console.timed.log` (each line with the seconds since QEMU started),
//! and kept in a buffer that [`Machine::expect`] searches: a match
//! consumes the buffer up to its end, so the next search starts after it.
//!
//! [`Machine::run`] runs a command in the console's shell and returns its
//! output and exit status. It types
//!
//! ```text
//! echo __NM_<n>_B__; COMMAND; echo __NM_<n>_E_$?__
//! ```
//!
//! and takes what comes between the two markers, where `<n>` is new for
//! every command, so output can never be mistaken for a marker, and the
//! terminal's echo of the typed line does not match either (the first
//! marker is followed there by `;`, the second by `$?`). A command that
//! spans lines, is long, or holds a `#` (which would comment out the end
//! marker) is first written to a file in `/tmp` and then sourced, so the
//! functions and variables it defines stay in the shell until the next
//! boot. Files are written the same way: base64, a few hundred
//! characters per line, decoded in the machine.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use regex::bytes::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use neomake_core::machine::{Firmware, MachineSpec, Media};

use crate::disk;
use crate::host;
use crate::qemu::{self, Layout, Serial};
use crate::virtiofs::Daemon;

/// How much of the console's latest output errors quote.
const TAIL: usize = 2000;

/// Why a console operation did not succeed.
#[derive(Debug, Clone)]
pub enum ConsoleError {
    /// What was waited for did not come in time.
    Timeout(String),
    /// QEMU exited.
    Stopped(String),
    /// Anything else.
    Other(String),
}

impl ConsoleError {
    /// The error in words.
    pub fn message(&self) -> &str {
        match self {
            ConsoleError::Timeout(m) | ConsoleError::Stopped(m) | ConsoleError::Other(m) => m,
        }
    }
}

enum Event {
    Data(Vec<u8>),
    Closed,
}

/// A machine of a test run.
pub struct Machine {
    /// What it is.
    pub spec: MachineSpec,
    /// Its directory: disks, firmware variables, logs.
    pub dir: PathBuf,
    root: PathBuf,
    sockets: PathBuf,
    /// Where its files are.
    pub layout: Layout,
    qemu: PathBuf,
    child: Option<tokio::process::Child>,
    stdin: Option<tokio::process::ChildStdin>,
    events: Option<mpsc::UnboundedReceiver<Event>>,
    buffer: Vec<u8>,
    recent: Arc<Mutex<VecDeque<u8>>>,
    qemu_errors: Arc<Mutex<VecDeque<u8>>>,
    daemons: Vec<Daemon>,
    nonce: u64,
    /// Commands run whenever the shell is ready after a boot.
    pub on_boot: Option<String>,
    started: Instant,
}

static SOCKET_DIRS: AtomicU64 = AtomicU64::new(0);

impl Machine {
    /// Lay `spec` out in `dir`: copy its disks and firmware variables.
    pub fn prepare(spec: &MachineSpec, root: &Path, dir: &Path, kvm: bool) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let at = |p: &str| {
            if Path::new(p).is_absolute() {
                PathBuf::from(p)
            } else {
                root.join(p)
            }
        };
        let mut disks = Vec::new();
        for (i, d) in spec.disks.iter().enumerate() {
            let name = match (&d.name, i) {
                (Some(n), _) => format!("disk-{n}.img"),
                (None, 0) => "disk.img".to_string(),
                (None, i) => format!("disk-{i}.img"),
            };
            let path = dir.join(name);
            let size = d.size.as_deref().map(disk::parse_size).transpose()?;
            match (&d.file, size) {
                (Some(file), size) => {
                    let src = at(file);
                    if !src.is_file() {
                        return Err(format!(
                            "machine `{}`: its disk {} does not exist (is the task that builds it among the test's deps?)",
                            spec.name,
                            src.display()
                        ));
                    }
                    disk::copy(&src, &path).map_err(|e| {
                        format!("cannot copy {} to {}: {e}", src.display(), path.display())
                    })?;
                    if let Some(size) = size {
                        disk::grow(&path, size)
                            .map_err(|e| format!("cannot grow {}: {e}", path.display()))?;
                    }
                }
                (None, Some(size)) => disk::blank(&path, size)
                    .map_err(|e| format!("cannot create {}: {e}", path.display()))?,
                (None, None) => unreachable!("checked when the configuration is read"),
            }
            disks.push(path);
        }
        let firmware = match &spec.firmware {
            Firmware::Bios => None,
            f @ Firmware::Uefi { .. } => {
                let (code, vars) = host::ovmf(f, root).ok_or("no UEFI firmware (OVMF) found")?;
                let copy = dir.join("OVMF_VARS.fd");
                std::fs::copy(&vars, &copy)
                    .map_err(|e| format!("cannot copy {}: {e}", vars.display()))?;
                Some((code, copy))
            }
        };
        let sockets = socket_dir()?;
        let media = spec
            .media
            .iter()
            .map(|m| (m.clone(), at(&m.file)))
            .collect();
        let qemu =
            host::program(&spec.qemu, root).ok_or_else(|| format!("{} not found", spec.qemu))?;
        Ok(Self {
            layout: Layout {
                disks,
                firmware,
                media,
                qmp: spec.display.then(|| sockets.join("qmp.sock")),
                shares: Vec::new(),
                kvm,
            },
            spec: spec.clone(),
            dir: dir.to_path_buf(),
            root: root.to_path_buf(),
            sockets,
            qemu,
            child: None,
            stdin: None,
            events: None,
            buffer: Vec::new(),
            recent: Arc::default(),
            qemu_errors: Arc::default(),
            daemons: Vec::new(),
            nonce: 0,
            on_boot: None,
            started: Instant::now(),
        })
    }

    /// Whether QEMU is running.
    pub fn running(&mut self) -> bool {
        matches!(self.child.as_mut().map(|c| c.try_wait()), Some(Ok(None)))
    }

    /// Start QEMU (and the virtiofsd of each share).
    pub async fn start(&mut self) -> Result<(), String> {
        if self.running() {
            return Err("the machine is already running".into());
        }
        self.stop().await;
        for (media, file) in &self.layout.media {
            if !file.is_file() {
                return Err(format!(
                    "medium `{}`: {} does not exist",
                    media.name,
                    file.display()
                ));
            }
        }
        self.layout.shares.clear();
        for (i, share) in self.spec.shares.clone().iter().enumerate() {
            let Some(binary) = host::virtiofsd() else {
                if share.optional {
                    continue;
                }
                return Err("virtiofsd not found".into());
            };
            let dir = self.root.join(&share.dir);
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            let socket = self.sockets.join(format!("vfs{i}.sock"));
            let _ = std::fs::remove_file(&socket);
            let log = self.dir.join(format!("virtiofsd-{i}.log"));
            self.daemons
                .push(Daemon::start(&binary, &dir, &socket, &log).await?);
            self.layout.shares.push((socket, share.tag.clone()));
        }

        let args = qemu::args(&self.spec, &self.layout, Serial::Stdio);
        let mut cmd = tokio::process::Command::new(&self.qemu);
        cmd.args(&args)
            .envs(&self.spec.env)
            .current_dir(&self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        {
            let mut f = append(&self.dir.join("qemu.log"));
            let _ = writeln!(f, "$ {} {}", self.qemu.display(), args.join(" "));
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", self.qemu.display()))?;
        self.started = Instant::now();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let recent = Arc::clone(&self.recent);
        let mut raw = append(&self.dir.join("console.log"));
        let mut timed = append(&self.dir.join("console.timed.log"));
        let started = self.started;
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            let mut line_start = true;
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Event::Closed);
                        break;
                    }
                    Ok(n) => {
                        let data = &buf[..n];
                        let _ = raw.write_all(data);
                        let _ = raw.flush();
                        for &b in data {
                            if line_start {
                                let _ = write!(timed, "[{:9.3}] ", started.elapsed().as_secs_f64());
                                line_start = false;
                            }
                            if b != b'\r' {
                                let _ = timed.write_all(&[b]);
                            }
                            if b == b'\n' {
                                line_start = true;
                            }
                        }
                        let _ = timed.flush();
                        {
                            let mut r = recent.lock().unwrap_or_else(|e| e.into_inner());
                            r.extend(data);
                            while r.len() > TAIL * 2 {
                                r.pop_front();
                            }
                        }
                        if tx.send(Event::Data(data.to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let errors = Arc::clone(&self.qemu_errors);
        let mut log = append(&self.dir.join("qemu.log"));
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let _ = log.write_all(&buf[..n]);
                let mut e = errors.lock().unwrap_or_else(|e| e.into_inner());
                e.extend(&buf[..n]);
                while e.len() > 4096 {
                    e.pop_front();
                }
            }
        });
        self.stdin = child.stdin.take();
        self.events = Some(rx);
        self.child = Some(child);
        self.buffer.clear();
        Ok(())
    }

    /// Stop QEMU at once (the power cut) and the share daemons.
    pub async fn stop(&mut self) {
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // SAFETY: kill(2) has no memory-safety preconditions.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.events = None;
        for mut d in self.daemons.drain(..) {
            d.stop().await;
        }
    }

    /// Remove what the run no longer needs: the disk copies and sockets.
    pub fn discard_disks(&self) {
        for d in &self.layout.disks {
            let _ = std::fs::remove_file(d);
        }
        if let Some((_, vars)) = &self.layout.firmware {
            let _ = std::fs::remove_file(vars);
        }
        let _ = std::fs::remove_dir_all(&self.sockets);
    }

    /// The console's latest output, for messages.
    pub fn tail(&self) -> String {
        let r = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let bytes: Vec<u8> = r.iter().rev().take(TAIL).rev().copied().collect();
        String::from_utf8_lossy(&bytes).replace("\r\n", "\n")
    }

    fn stopped(&mut self) -> ConsoleError {
        let status = self
            .child
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten())
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        let errors = {
            let e = self.qemu_errors.lock().unwrap_or_else(|e| e.into_inner());
            String::from_utf8_lossy(&e.iter().copied().collect::<Vec<u8>>())
                .trim()
                .to_string()
        };
        let mut msg = format!("QEMU exited{status}");
        if !errors.is_empty() {
            msg.push_str(&format!("; it said:\n{errors}"));
        }
        msg.push_str(&self.quote_tail());
        ConsoleError::Stopped(msg)
    }

    fn quote_tail(&self) -> String {
        let tail = self.tail();
        if tail.trim().is_empty() {
            String::new()
        } else {
            format!("\nthe console's last output:\n{}", tail.trim_end())
        }
    }

    /// Wait for more output until `deadline`; `Ok(false)` when the time
    /// is up.
    async fn more(&mut self, deadline: Instant) -> Result<bool, ConsoleError> {
        let Some(events) = self.events.as_mut() else {
            return Err(ConsoleError::Other("the machine is not running".into()));
        };
        let wait = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(wait, events.recv()).await {
            Err(_) => Ok(false),
            Ok(Some(Event::Data(d))) => {
                self.buffer.extend(d);
                Ok(true)
            }
            Ok(Some(Event::Closed)) | Ok(None) => {
                // Let QEMU finish exiting, for its status.
                if let Some(c) = self.child.as_mut() {
                    let _ = tokio::time::timeout(Duration::from_secs(2), c.wait()).await;
                }
                self.events = None;
                Err(self.stopped())
            }
        }
    }

    /// Wait for `re` to match the console's output, consuming it up to
    /// the match; returns the match's groups (group 0 first).
    pub async fn expect(
        &mut self,
        re: &Regex,
        timeout: Duration,
    ) -> Result<Vec<Option<String>>, ConsoleError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(caps) = re.captures(&self.buffer) {
                let groups = caps
                    .iter()
                    .map(|g| g.map(|m| String::from_utf8_lossy(m.as_bytes()).into_owned()))
                    .collect();
                let end = caps.get(0).expect("group 0").end();
                self.buffer.drain(..end);
                return Ok(groups);
            }
            if !self.more(deadline).await? {
                return Err(ConsoleError::Timeout(format!(
                    "nothing matched /{}/ within {}{}",
                    pattern_of(re),
                    neomake_core::duration::human(timeout),
                    self.quote_tail()
                )));
            }
        }
    }

    /// Type `text` on the console.
    pub async fn send(&mut self, text: &str) -> Result<(), ConsoleError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(ConsoleError::Other("the machine is not running".into()));
        };
        if stdin.write_all(text.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            return Err(self.stopped());
        }
        Ok(())
    }

    /// Forget output not yet searched (before a reboot, say).
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Wait for the prompt, then run the console's setup and `on_boot`.
    pub async fn wait_prompt(&mut self, timeout: Duration) -> Result<(), ConsoleError> {
        let deadline = Instant::now() + timeout;
        let prompt =
            Regex::new(&regex::escape(&self.spec.console.prompt)).expect("an escaped literal");
        self.expect(&prompt, timeout).await?;
        if let Some(setup) = self.spec.console.setup.clone() {
            self.run(&setup, remaining(deadline)).await?;
        }
        if let Some(on_boot) = self.on_boot.clone() {
            let (out, status) = self.run(&on_boot, remaining(deadline)).await?;
            if status != 0 {
                return Err(ConsoleError::Other(format!(
                    "on_boot exited with {status}:\n{}",
                    out.trim_end()
                )));
            }
        }
        Ok(())
    }

    /// Wait for a boot: the firmware's banner (if the console has one),
    /// then the prompt.
    pub async fn wait_boot(&mut self, timeout: Duration) -> Result<(), ConsoleError> {
        let deadline = Instant::now() + timeout;
        if let Some(banner) = self.spec.console.banner.clone() {
            let re = Regex::new(&regex::escape(&banner)).expect("an escaped literal");
            self.expect(&re, timeout).await?;
        }
        self.wait_prompt(remaining(deadline)).await
    }

    fn next_nonce(&mut self) -> String {
        self.nonce += 1;
        format!("{:x}{:x}", std::process::id(), self.nonce)
    }

    /// Run `command` in the console's shell; its output (with `\r\n` as
    /// `\n`) and exit status.
    pub async fn run(
        &mut self,
        command: &str,
        timeout: Duration,
    ) -> Result<(String, i32), ConsoleError> {
        let deadline = Instant::now() + timeout;
        if command.contains('\n') || command.contains('#') || command.len() > 900 {
            let n = self.next_nonce();
            let path = format!("/tmp/.neomake-{n}.sh");
            self.write_file(&path, command.as_bytes(), remaining(deadline))
                .await?;
            let wrapped = format!(". {path}; __nm=$?; rm -f {path}; (exit $__nm)");
            return self.run_line(&wrapped, remaining(deadline)).await;
        }
        self.run_line(command, timeout).await
    }

    async fn run_line(
        &mut self,
        command: &str,
        timeout: Duration,
    ) -> Result<(String, i32), ConsoleError> {
        let deadline = Instant::now() + timeout;
        let n = self.next_nonce();
        self.send(&format!(
            "echo __NM_{n}_B__; {command}; echo __NM_{n}_E_$?__\n"
        ))
        .await?;
        let begin = Regex::new(&format!(r"__NM_{n}_B__\r?\n")).expect("a fixed pattern");
        let end =
            Regex::new(&format!(r"(?s)(.*?)__NM_{n}_E_(\d+)__\r?\n")).expect("a fixed pattern");
        self.expect(&begin, remaining(deadline)).await?;
        let groups = self.expect(&end, remaining(deadline)).await?;
        let output = groups[1].clone().unwrap_or_default().replace("\r\n", "\n");
        let status: i32 = groups[2].as_deref().unwrap_or("0").parse().unwrap_or(-1);
        let prompt =
            Regex::new(&regex::escape(&self.spec.console.prompt)).expect("an escaped literal");
        let wait = remaining(deadline)
            .max(Duration::from_secs(1))
            .min(Duration::from_secs(30));
        self.expect(&prompt, wait).await?;
        Ok((output, status))
    }

    /// Write `bytes` to `path` in the machine.
    pub async fn write_file(
        &mut self,
        path: &str,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<(), ConsoleError> {
        let deadline = Instant::now() + timeout;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let n = self.next_nonce();
        let tmp = format!("/tmp/.neomake-{n}.b64");
        let check = |r: (String, i32), what: &str| {
            if r.1 == 0 {
                Ok(())
            } else {
                Err(ConsoleError::Other(format!(
                    "writing {path}: {what} exited with {}: {}",
                    r.1,
                    r.0.trim_end()
                )))
            }
        };
        let r = self
            .run_line(&format!(": > {tmp}"), remaining(deadline))
            .await?;
        check(r, "creating its temporary file")?;
        for chunk in encoded.as_bytes().chunks(700) {
            let chunk = std::str::from_utf8(chunk).expect("base64 is ASCII");
            let r = self
                .run_line(
                    &format!("printf %s '{chunk}' >> {tmp}"),
                    remaining(deadline),
                )
                .await?;
            check(r, "printf")?;
        }
        let r = self
            .run_line(
                &format!(
                    "base64 -d {tmp} > {}; __nm=$?; rm -f {tmp}; (exit $__nm)",
                    quote(path)
                ),
                remaining(deadline),
            )
            .await?;
        check(r, "base64 -d")
    }

    /// Power off cleanly and wait for QEMU to exit.
    pub async fn shutdown(&mut self, timeout: Duration) -> Result<(), ConsoleError> {
        let command = format!("{}\n", self.spec.console.poweroff);
        self.send(&command).await?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.more(deadline).await {
                Ok(true) => continue,
                Ok(false) => {
                    return Err(ConsoleError::Timeout(format!(
                        "the machine did not power off within {}{}",
                        neomake_core::duration::human(timeout),
                        self.quote_tail()
                    )))
                }
                Err(ConsoleError::Stopped(_)) => break,
                Err(e) => return Err(e),
            }
        }
        self.stop().await;
        Ok(())
    }

    /// Save what the display shows as `name` in the machine's directory.
    pub async fn screenshot(&mut self, name: &str) -> Result<PathBuf, ConsoleError> {
        let Some(socket) = self.layout.qmp.clone() else {
            return Err(ConsoleError::Other(format!(
                "machine `{}` has no display (`display = true`)",
                self.spec.name
            )));
        };
        let path = self.dir.join(name);
        crate::qmp::screenshot(&socket, &path)
            .await
            .map_err(ConsoleError::Other)?;
        Ok(path)
    }

    /// Attach `media` from the next start on.
    pub fn attach(&mut self, media: Media) {
        let file = if Path::new(&media.file).is_absolute() {
            PathBuf::from(&media.file)
        } else {
            self.root.join(&media.file)
        };
        self.layout.media.retain(|(m, _)| m.name != media.name);
        self.layout.media.push((media, file));
    }

    /// Detach the medium `name` from the next start on.
    pub fn detach(&mut self, name: &str) -> Result<(), String> {
        let before = self.layout.media.len();
        self.layout.media.retain(|(m, _)| m.name != name);
        if self.layout.media.len() == before {
            let names: Vec<&str> = self
                .layout
                .media
                .iter()
                .map(|(m, _)| m.name.as_str())
                .collect();
            return Err(format!(
                "no medium `{name}` is attached ({})",
                if names.is_empty() {
                    "none is".to_string()
                } else {
                    names.join(", ")
                }
            ));
        }
        Ok(())
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn pattern_of(re: &Regex) -> String {
    re.as_str().trim_start_matches("(?m)").to_string()
}

fn append(path: &Path) -> std::fs::File {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|_| std::fs::File::create("/dev/null").expect("/dev/null"))
}

/// `s` in single quotes for a shell.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A short directory for sockets, whose paths are limited to about 100
/// bytes: under `$XDG_RUNTIME_DIR`, else `/tmp`.
fn socket_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let n = SOCKET_DIRS.fetch_add(1, Ordering::SeqCst);
    let dir = base.join(format!("nm-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(quote("/usr/local/bin/x"), "'/usr/local/bin/x'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
