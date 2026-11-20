//! Where a run keeps what it produced.
//!
//! Every `neomake run` gets a directory `<state_dir>/runs/<id>/`, where
//! the id is the UTC start time (`20261120-101500`). It holds one log per
//! task under `logs/`, the report (`report.json`), and anything tests
//! leave behind. `<state_dir>/runs/latest` names the newest run, and only
//! the newest few are kept, so the output of a failed run is still there
//! after the next one.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The directory of one run.
#[derive(Debug, Clone)]
pub struct RunDir {
    /// The run's id, which is also its directory's name.
    pub id: String,
    /// The directory.
    pub path: PathBuf,
}

impl RunDir {
    /// Create a new run directory under `<state_dir>/runs`.
    pub fn create(state_dir: &Path) -> std::io::Result<Self> {
        let runs = state_dir.join("runs");
        std::fs::create_dir_all(&runs)?;
        let base = compact_timestamp(SystemTime::now());
        let mut id = base.clone();
        let mut n = 1;
        loop {
            let path = runs.join(&id);
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    std::fs::create_dir_all(path.join("logs"))?;
                    return Ok(Self { id, path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    n += 1;
                    id = format!("{base}-{n}");
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Where the logs are.
    pub fn logs(&self) -> PathBuf {
        self.path.join("logs")
    }

    /// The log file of `task`.
    pub fn log_of(&self, task: &str) -> PathBuf {
        self.logs().join(format!("{}.log", file_name_for(task)))
    }

    /// Make this the run `latest` names.
    pub fn mark_latest(&self) {
        let Some(runs) = self.path.parent() else {
            return;
        };
        let link = runs.join("latest");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&self.id, &link);
        #[cfg(not(unix))]
        let _ = std::fs::write(&link, &self.id);
    }

    /// The newest run's directory, if there has been one.
    pub fn latest(state_dir: &Path) -> Option<PathBuf> {
        let runs = state_dir.join("runs");
        let link = runs.join("latest");
        let id = match std::fs::read_link(&link) {
            Ok(target) => target,
            Err(_) => PathBuf::from(std::fs::read_to_string(&link).ok()?.trim()),
        };
        let path = runs.join(id);
        path.is_dir().then_some(path)
    }

    /// Delete all but the newest `keep` runs.
    pub fn prune(state_dir: &Path, keep: usize) {
        let runs = state_dir.join("runs");
        let Ok(entries) = std::fs::read_dir(&runs) else {
            return;
        };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.as_bytes().first().is_some_and(u8::is_ascii_digit))
            .collect();
        // Ids sort by time; a `-N` suffix sorts after the plain id.
        ids.sort();
        let excess = ids.len().saturating_sub(keep.max(1));
        for id in &ids[..excess] {
            let _ = std::fs::remove_dir_all(runs.join(id));
        }
    }
}

/// A task name as a file name: characters that are not safe in one
/// (path separators, brackets of matrix variants, and so on) become `_`.
pub fn file_name_for(task: &str) -> String {
    task.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '=' | ',') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `20261120-101500` for the given time, in UTC.
pub fn compact_timestamp(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = civil(t);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// `2026-11-20T10:15:00Z` for the given time.
pub fn rfc3339(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = civil(t);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// The UTC calendar date and time of `t`.
fn civil(t: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_utc_times() {
        let t = UNIX_EPOCH + Duration::from_secs(1_795_428_900);
        assert_eq!(rfc3339(t), "2026-11-23T10:15:00Z");
        assert_eq!(compact_timestamp(t), "20261123-101500");
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(rfc3339(leap), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn makes_task_names_safe_for_files() {
        assert_eq!(file_name_for("live[media=cd]"), "live_media=cd_");
        assert_eq!(file_name_for("a/b c"), "a_b_c");
    }

    #[test]
    fn creates_marks_and_prunes_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let a = RunDir::create(tmp.path()).unwrap();
        let b = RunDir::create(tmp.path()).unwrap();
        assert_ne!(a.id, b.id, "two runs in one second get distinct ids");
        assert!(a.logs().is_dir());
        b.mark_latest();
        assert_eq!(RunDir::latest(tmp.path()), Some(b.path.clone()));
        let c = RunDir::create(tmp.path()).unwrap();
        RunDir::prune(tmp.path(), 2);
        assert!(!a.path.exists());
        assert!(b.path.exists() && c.path.exists());
    }
}
