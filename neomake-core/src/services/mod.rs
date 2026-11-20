//! Services: long-running processes that tasks and tests use.
//!
//! A service is declared under `[services.NAME]`, and used by a task or
//! test that lists it in `services`. It is either a `command` that keeps
//! running (with a `ready` check: a TCP port that accepts connections, a
//! file that appears, a line in its output, or an HTTP URL that answers),
//! or the built-in static file server (`serve`, see [`serve`]).
//!
//! A service starts when the first task that uses it is about to run,
//! after the tasks the service itself depends on, and stops when the last
//! task of the run that uses it has ended, or when the run ends. Before
//! it starts, the address it will listen on is checked to be free, so a
//! leftover server (`make serve-repo` in another terminal, say) is an
//! error that names the address, not a test that fails later because the
//! wrong files were served. Each service's output goes to its own log,
//! `logs/service-NAME.log`. If a service does not start, every task that
//! uses it fails with the reason.
//!
//! Tasks find where a service listens with placeholders:
//! `{{services.NAME.port}}`, `.addr` (`127.0.0.1:8643`) and `.url`
//! (`http://127.0.0.1:8643`) for its first address, and
//! `{{services.NAME.LISTENER.port}}` and so on for a named one.

pub mod serve;

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::task::Location;

/// A service, as configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Its name.
    pub name: String,
    /// What it is for.
    pub description: Option<String>,
    /// What it runs.
    pub kind: ServiceKind,
    /// Tasks that must have finished before it starts.
    pub deps: Vec<String>,
    /// Working directory, relative to the project root.
    pub cwd: Option<String>,
    /// Variables set for its command.
    pub env: BTreeMap<String, String>,
    /// How it is known to be ready.
    pub ready: Option<Ready>,
    /// How long it has to become ready.
    pub ready_timeout: Duration,
    /// Where it is defined.
    pub source: Option<Location>,
}

/// What a service runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceKind {
    /// A command that keeps running.
    Command {
        /// The command.
        command: String,
        /// The shell's argv, if not the default.
        shell: Option<Vec<String>>,
    },
    /// The built-in static file server.
    Serve {
        /// The directory, relative to the project root.
        dir: String,
        /// Where it listens.
        listeners: Vec<Listener>,
    },
}

/// One address the file server listens on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    /// Its name, for placeholders.
    pub name: Option<String>,
    /// `host:port`; port 0 picks a free one.
    pub addr: String,
    /// HTTPS, with this certificate and key (PEM files, relative to the
    /// project root).
    pub tls: Option<(String, String)>,
}

/// How a command service is known to be ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ready {
    /// It accepts TCP connections at this address.
    Tcp(String),
    /// This file exists (relative to the service's directory).
    File(String),
    /// A line of its output matches this pattern.
    Log(String),
    /// An HTTP request to this URL gets any answer.
    Http(String),
}

impl Service {
    /// The placeholder names this service provides: an address for the
    /// file server's listeners, and for a command service that is ready
    /// when an address answers.
    pub fn var_names(&self) -> Vec<String> {
        let fields = |prefix: String| {
            ["port", "addr", "url"]
                .into_iter()
                .map(move |f| format!("{prefix}.{f}"))
        };
        let base = format!("services.{}", self.name);
        match &self.kind {
            ServiceKind::Serve { listeners, .. } => fields(base.clone())
                .chain(
                    listeners
                        .iter()
                        .filter_map(|l| l.name.as_ref())
                        .flat_map(|n| fields(format!("{base}.{n}"))),
                )
                .collect(),
            ServiceKind::Command { .. } => match &self.ready {
                Some(Ready::Tcp(_) | Ready::Http(_)) => fields(base).collect(),
                _ => Vec::new(),
            },
        }
    }
}

/// An event in a service's life, for the run's progress output.
#[derive(Debug, Clone)]
pub enum ServiceEvent {
    /// It is ready; `detail` says where.
    Started {
        /// The service.
        name: String,
        /// Where it listens, or what it runs.
        detail: String,
    },
    /// It could not start.
    Failed {
        /// The service.
        name: String,
        /// Why.
        message: String,
    },
    /// It was stopped.
    Stopped {
        /// The service.
        name: String,
    },
}

/// The services of one run.
pub struct Services {
    root: PathBuf,
    log_dir: Option<PathBuf>,
    default_shell: Vec<String>,
    slots: BTreeMap<String, tokio::sync::Mutex<Slot>>,
    on_event: Arc<dyn Fn(ServiceEvent) + Send + Sync>,
}

struct Slot {
    service: Service,
    /// Tasks of the run that use it and have not ended yet.
    users_left: usize,
    state: State,
}

enum State {
    Idle,
    Running {
        vars: Vec<(String, String)>,
        handle: Handle,
    },
    Failed(String),
    Done,
}

enum Handle {
    Child {
        // Boxed: a child process is large on Windows.
        child: Box<tokio::process::Child>,
        group: Option<u32>,
    },
    Serve {
        stop: CancellationToken,
    },
}

impl Services {
    /// The services `users` (how many tasks of the run use each) refer
    /// to. `log_dir` is where their logs go.
    pub fn new(
        defs: &BTreeMap<String, Service>,
        users: &BTreeMap<String, usize>,
        root: &Path,
        log_dir: Option<&Path>,
        default_shell: Vec<String>,
        on_event: Arc<dyn Fn(ServiceEvent) + Send + Sync>,
    ) -> Self {
        let slots = users
            .iter()
            .filter_map(|(name, count)| {
                defs.get(name).map(|service| {
                    (
                        name.clone(),
                        tokio::sync::Mutex::new(Slot {
                            service: service.clone(),
                            users_left: *count,
                            state: State::Idle,
                        }),
                    )
                })
            })
            .collect();
        Self {
            root: root.to_path_buf(),
            log_dir: log_dir.map(Path::to_path_buf),
            default_shell,
            slots,
            on_event,
        }
    }

    /// Start (if it is not running yet) the service `name`, and return
    /// its placeholders.
    pub async fn acquire(
        &self,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<(String, String)>, String> {
        let Some(slot) = self.slots.get(name) else {
            return Err(format!("there is no service `{name}`"));
        };
        let mut slot = slot.lock().await;
        match &slot.state {
            State::Running { vars, .. } => return Ok(vars.clone()),
            State::Failed(why) => return Err(why.clone()),
            State::Done => return Err(format!("service `{name}` has already been stopped")),
            State::Idle => {}
        }
        let started = tokio::select! {
            r = self.start(&slot.service) => r,
            _ = cancel.cancelled() => Err("the run was cancelled".into()),
        };
        match started {
            Ok((vars, handle, detail)) => {
                (self.on_event)(ServiceEvent::Started {
                    name: name.to_string(),
                    detail,
                });
                slot.state = State::Running {
                    vars: vars.clone(),
                    handle,
                };
                Ok(vars)
            }
            Err(message) => {
                let message = format!("service `{name}` did not start: {message}");
                (self.on_event)(ServiceEvent::Failed {
                    name: name.to_string(),
                    message: message.clone(),
                });
                slot.state = State::Failed(message.clone());
                Err(message)
            }
        }
    }

    /// A task of the run that uses `name` has ended; the last one stops it.
    pub async fn release(&self, name: &str) {
        let Some(slot) = self.slots.get(name) else {
            return;
        };
        let mut slot = slot.lock().await;
        slot.users_left = slot.users_left.saturating_sub(1);
        if slot.users_left == 0 {
            self.stop(name, &mut slot).await;
        }
    }

    /// Stop every service still running.
    pub async fn stop_all(&self) {
        for (name, slot) in &self.slots {
            let mut slot = slot.lock().await;
            self.stop(name, &mut slot).await;
        }
    }

    async fn stop(&self, name: &str, slot: &mut Slot) {
        if let State::Running { handle, .. } = std::mem::replace(&mut slot.state, State::Done) {
            match handle {
                Handle::Child { mut child, group } => {
                    signal(group, Sig::Term);
                    if tokio::time::timeout(crate::process::GRACE, child.wait())
                        .await
                        .is_err()
                    {
                        signal(group, Sig::Kill);
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                    }
                    signal(group, Sig::Kill);
                }
                Handle::Serve { stop } => stop.cancel(),
            }
            (self.on_event)(ServiceEvent::Stopped {
                name: name.to_string(),
            });
        }
    }

    fn log(&self, name: &str) -> Arc<Mutex<Option<std::fs::File>>> {
        let file = self.log_dir.as_ref().and_then(|d| {
            std::fs::File::create(d.join(format!(
                "service-{}.log",
                crate::rundir::file_name_for(name)
            )))
            .ok()
        });
        Arc::new(Mutex::new(file))
    }

    async fn start(
        &self,
        service: &Service,
    ) -> Result<(Vec<(String, String)>, Handle, String), String> {
        let log = self.log(&service.name);
        let write = {
            let log = Arc::clone(&log);
            move |line: String| {
                if let Some(f) = log.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    let _ = writeln!(f, "{line}");
                }
            }
        };
        let cwd = match &service.cwd {
            Some(d) => self.root.join(d),
            None => self.root.clone(),
        };
        match &service.kind {
            ServiceKind::Serve { dir, listeners } => {
                let dir_path = self.root.join(dir);
                if !dir_path.is_dir() {
                    return Err(format!("{dir} is not a directory"));
                }
                let stop = CancellationToken::new();
                let mut vars = Vec::new();
                let mut shown = Vec::new();
                for (i, l) in listeners.iter().enumerate() {
                    let listener = tokio::net::TcpListener::bind(&l.addr)
                        .await
                        .map_err(|e| format!("cannot listen on {}: {e}", l.addr))?;
                    let addr = listener.local_addr().map_err(|e| e.to_string())?;
                    let tls = match &l.tls {
                        Some((cert, key)) => Some(serve::tls_acceptor(
                            &self.root.join(cert),
                            &self.root.join(key),
                        )?),
                        None => None,
                    };
                    let scheme = if tls.is_some() { "https" } else { "http" };
                    let url = format!("{scheme}://{addr}");
                    let mut prefixes = Vec::new();
                    if i == 0 {
                        prefixes.push(format!("services.{}", service.name));
                    }
                    if let Some(n) = &l.name {
                        prefixes.push(format!("services.{}.{n}", service.name));
                    }
                    for p in prefixes {
                        vars.push((format!("{p}.port"), addr.port().to_string()));
                        vars.push((format!("{p}.addr"), addr.to_string()));
                        vars.push((format!("{p}.url"), url.clone()));
                    }
                    shown.push(url);
                    let log_fn: serve::LogFn = Arc::new(write.clone());
                    tokio::spawn(serve::serve(
                        listener,
                        dir_path.clone(),
                        tls,
                        log_fn,
                        stop.clone(),
                    ));
                }
                let detail = format!("{} ({dir})", shown.join(", "));
                Ok((vars, Handle::Serve { stop }, detail))
            }
            ServiceKind::Command { command, shell } => {
                if let Some(Ready::Tcp(addr)) = &service.ready {
                    if let Some(sock) = resolve(addr) {
                        if quick_connect(sock).await {
                            return Err(format!(
                                "something is already listening on {addr} (another run, or a server started by hand?)"
                            ));
                        }
                    }
                }
                let argv = shell.clone().unwrap_or_else(|| self.default_shell.clone());
                let (program, args) = argv.split_first().ok_or("the shell is empty")?;
                let mut cmd = tokio::process::Command::new(program);
                cmd.args(args)
                    .arg(command)
                    .current_dir(&cwd)
                    .envs(&service.env)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                #[cfg(unix)]
                cmd.process_group(0);
                let mut child = cmd
                    .spawn()
                    .map_err(|e| format!("could not start `{program}`: {e}"))?;
                let group = child.id();
                let recent: Arc<Mutex<VecDeque<String>>> = Arc::default();
                for out in [
                    child
                        .stdout
                        .take()
                        .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                    child
                        .stderr
                        .take()
                        .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                ]
                .into_iter()
                .flatten()
                {
                    let w = write.clone();
                    let recent = Arc::clone(&recent);
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(out);
                        let mut buf = Vec::new();
                        while let Ok(n) = reader.read_until(b'\n', &mut buf).await {
                            if n == 0 {
                                break;
                            }
                            let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                            buf.clear();
                            {
                                let mut r = recent.lock().unwrap_or_else(|e| e.into_inner());
                                if r.len() == 200 {
                                    r.pop_front();
                                }
                                r.push_back(line.clone());
                            }
                            w(line);
                        }
                    });
                }
                let deadline = Instant::now() + service.ready_timeout;
                let pattern = match &service.ready {
                    Some(Ready::Log(p)) => Some(crate::steps::compile(p)?),
                    _ => None,
                };
                loop {
                    if let Ok(Some(status)) = child.try_wait() {
                        signal(group, Sig::Kill);
                        return Err(format!(
                            "it exited ({status}) before it was ready{}",
                            last_lines(&recent)
                        ));
                    }
                    let ready = match &service.ready {
                        None => true,
                        Some(Ready::Tcp(addr)) => match resolve(addr) {
                            Some(sock) => quick_connect(sock).await,
                            None => return Err(format!("`{addr}` is not an address")),
                        },
                        Some(Ready::File(f)) => cwd.join(f).exists(),
                        Some(Ready::Log(_)) => {
                            let r = recent.lock().unwrap_or_else(|e| e.into_inner());
                            r.iter()
                                .any(|l| pattern.as_ref().is_some_and(|p| p.is_match(l)))
                        }
                        Some(Ready::Http(url)) => http_answers(url).await,
                    };
                    if ready {
                        break;
                    }
                    if Instant::now() >= deadline {
                        signal(group, Sig::Kill);
                        let _ = child.start_kill();
                        return Err(format!(
                            "it was not ready after {}{}",
                            crate::duration::human(service.ready_timeout),
                            last_lines(&recent)
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                let mut vars = Vec::new();
                let addr = match &service.ready {
                    Some(Ready::Tcp(a)) => Some(a.clone()),
                    Some(Ready::Http(u)) => u
                        .split_once("://")
                        .map(|(_, rest)| rest.split('/').next().unwrap_or("").to_string()),
                    _ => None,
                };
                if let Some(addr) = &addr {
                    let port = addr.rsplit(':').next().unwrap_or("").to_string();
                    let p = format!("services.{}", service.name);
                    vars.push((format!("{p}.port"), port));
                    vars.push((format!("{p}.addr"), addr.clone()));
                    vars.push((format!("{p}.url"), format!("http://{addr}")));
                }
                let detail = addr.unwrap_or_else(|| crate::steps::summarize(command));
                Ok((
                    vars,
                    Handle::Child {
                        child: Box::new(child),
                        group,
                    },
                    detail,
                ))
            }
        }
    }
}

fn last_lines(recent: &Mutex<VecDeque<String>>) -> String {
    let r = recent.lock().unwrap_or_else(|e| e.into_inner());
    let lines: Vec<&String> = r.iter().rev().take(5).collect();
    if lines.is_empty() {
        String::new()
    } else {
        let mut s = String::from("; it printed:");
        for l in lines.into_iter().rev() {
            s.push_str("\n    ");
            s.push_str(l);
        }
        s
    }
}

fn resolve(addr: &str) -> Option<SocketAddr> {
    addr.to_socket_addrs().ok()?.next()
}

async fn quick_connect(addr: SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect(addr)
        )
        .await,
        Ok(Ok(_))
    )
}

/// Whether a plain-HTTP request to `url` gets any answer.
async fn http_answers(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let Some(addr) = resolve(host) else {
        return false;
    };
    let attempt = async {
        let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
        s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n").as_bytes())
            .await
            .ok()?;
        let mut buf = [0u8; 12];
        let n = s.read(&mut buf).await.ok()?;
        Some(buf[..n].starts_with(b"HTTP/"))
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(2), attempt).await,
        Ok(Some(true))
    )
}

#[derive(Clone, Copy)]
enum Sig {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal(group: Option<u32>, sig: Sig) {
    if let Some(pid) = group {
        let s = match sig {
            Sig::Term => libc::SIGTERM,
            Sig::Kill => libc::SIGKILL,
        };
        // SAFETY: kill(2) has no memory-safety preconditions.
        unsafe {
            libc::kill(-(pid as libc::pid_t), s);
        }
    }
}

#[cfg(not(unix))]
fn signal(_group: Option<u32>, _sig: Sig) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn services(defs: Vec<Service>, root: &Path) -> Services {
        let users = defs.iter().map(|s| (s.name.clone(), 1)).collect();
        let defs = defs.into_iter().map(|s| (s.name.clone(), s)).collect();
        Services::new(
            &defs,
            &users,
            root,
            Some(root),
            vec!["sh".into(), "-c".into()],
            Arc::new(|_| {}),
        )
    }

    fn service(name: &str, kind: ServiceKind, ready: Option<Ready>) -> Service {
        Service {
            name: name.into(),
            description: None,
            kind,
            deps: vec![],
            cwd: None,
            env: BTreeMap::new(),
            ready,
            ready_timeout: Duration::from_secs(5),
            source: None,
        }
    }

    #[tokio::test]
    async fn the_file_server_publishes_its_ports() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("repo")).unwrap();
        std::fs::write(tmp.path().join("repo/repo.zon"), "x").unwrap();
        let s = services(
            vec![service(
                "repo",
                ServiceKind::Serve {
                    dir: "repo".into(),
                    listeners: vec![
                        Listener {
                            name: None,
                            addr: "127.0.0.1:0".into(),
                            tls: None,
                        },
                        Listener {
                            name: Some("second".into()),
                            addr: "127.0.0.1:0".into(),
                            tls: None,
                        },
                    ],
                },
                None,
            )],
            tmp.path(),
        );
        let vars = s.acquire("repo", &CancellationToken::new()).await.unwrap();
        let get = |k: &str| {
            vars.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        let url = get("services.repo.url");
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert_ne!(get("services.repo.port"), get("services.repo.second.port"));
        assert!(http_answers(&format!("{url}/repo.zon")).await);
        s.release("repo").await;
        assert!(
            !http_answers(&format!("{url}/repo.zon")).await,
            "stopped after its last user"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_services_wait_for_readiness_and_refuse_taken_ports() {
        let tmp = tempfile::tempdir().unwrap();
        let s = services(
            vec![
                service(
                    "ready-file",
                    ServiceKind::Command {
                        command: "sleep 0.3; touch up; echo started; sleep 30".into(),
                        shell: None,
                    },
                    Some(Ready::File("up".into())),
                ),
                service(
                    "dies",
                    ServiceKind::Command {
                        command: "echo cannot bind >&2; exit 3".into(),
                        shell: None,
                    },
                    Some(Ready::Log("never".into())),
                ),
            ],
            tmp.path(),
        );
        let cancel = CancellationToken::new();
        s.acquire("ready-file", &cancel).await.unwrap();
        assert!(tmp.path().join("up").exists());
        let err = s.acquire("dies", &cancel).await.unwrap_err();
        assert!(err.contains("exited"), "{err}");
        assert!(err.contains("cannot bind"), "{err}");
        // Failing once is remembered for the other users.
        assert_eq!(s.acquire("dies", &cancel).await.unwrap_err(), err);
        s.stop_all().await;

        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = taken.local_addr().unwrap().to_string();
        let s = services(
            vec![service(
                "clash",
                ServiceKind::Command {
                    command: "sleep 30".into(),
                    shell: None,
                },
                Some(Ready::Tcp(addr.clone())),
            )],
            tmp.path(),
        );
        let err = s.acquire("clash", &cancel).await.unwrap_err();
        assert!(
            err.contains(&format!("already listening on {addr}")),
            "{err}"
        );
    }
}
