//! virtiofsd: sharing a host directory with a machine, unprivileged.
//!
//! The daemon runs as the user running neomake and squashes ownership
//! both ways, as joe Linux's harness does: everything the guest's root
//! writes belongs to the user on the host, and the user's files look like
//! root's in the guest.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// A running virtiofsd.
pub struct Daemon {
    child: tokio::process::Child,
}

impl Daemon {
    /// Share `dir` on `socket`, logging to `log`.
    pub async fn start(
        binary: &Path,
        dir: &Path,
        socket: &Path,
        log: &Path,
    ) -> Result<Self, String> {
        let (uid, gid) = ids();
        let log_file = std::fs::File::create(log).map_err(|e| e.to_string())?;
        let err_file = log_file.try_clone().map_err(|e| e.to_string())?;
        let child = tokio::process::Command::new(binary)
            .arg("--socket-path")
            .arg(socket)
            .arg("--shared-dir")
            .arg(dir)
            .args([
                "--translate-uid",
                "squash-guest:0:0:4294967295",
                "--translate-gid",
                "squash-guest:0:0:4294967295",
                "--translate-uid",
                &format!("squash-host:0:{uid}:4294967295"),
                "--translate-gid",
                &format!("squash-host:0:{gid}:4294967295"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(err_file))
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", binary.display()))?;
        let mut daemon = Self { child };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            if let Ok(Some(status)) = daemon.child.try_wait() {
                return Err(format!(
                    "virtiofsd exited ({status}); see {}",
                    log.display()
                ));
            }
            if Instant::now() > deadline {
                daemon.stop().await;
                return Err(format!("virtiofsd did not start; see {}", log.display()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(daemon)
    }

    /// Stop it: SIGTERM, then SIGKILL after five seconds.
    pub async fn stop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // SAFETY: kill(2) has no memory-safety preconditions.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }
}

#[cfg(unix)]
fn ids() -> (u32, u32) {
    // SAFETY: getuid and getgid cannot fail.
    unsafe { (libc::getuid(), libc::getgid()) }
}

#[cfg(not(unix))]
fn ids() -> (u32, u32) {
    (1000, 1000)
}
