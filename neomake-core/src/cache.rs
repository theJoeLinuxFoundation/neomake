//! The cache: whether a task's result is already up to date.
//!
//! # Cache-key derivation
//!
//! A task's cache key is the SHA-256 of a deterministic byte stream,
//! **format version 3** (any change to the layout silently invalidates
//! every existing entry, so it is kept explicit):
//!
//! ```text
//! "neomake-cache-v3\n"
//! "name:"       <task name>                    "\n"
//! "command:"    <command-bytes>                "\n"
//! "prelude:"    <prelude-bytes>                "\n"
//! "steps:"      <the steps, as Rust debug text> "\n"
//! "shell:"      for each argv element:         <arg> "\n"     then ".\n"
//! "cwd:"        <cwd or empty>                 "\n"
//! "env_clear:"  "0" | "1"                      "\n"
//! "env:"        for each (K=V) in BTreeMap:    K "=" V "\n"   then ".\n"
//! "deps:"       for each upstream dep_key:     <hex> "\n"     then ".\n"
//! "outputs:"    for each output pattern:       <pattern> "\n" then ".\n"
//! "inputs:"     for each (relpath, sha256):    relpath ":" <hex> "\n" then ".\n"
//! ```
//!
//! The terminating `.` lines keep the encoding unambiguous.
//!
//! The per-input hash is the SHA-256 of the file's contents. Hashes are
//! remembered with each file's size, modification and change times and
//! inode, so an unchanged multi-gigabyte disk image is not read again on
//! every run; any change to those re-hashes it. An input pattern that
//! names exactly a directory `git()` or `fetch()` produced contributes
//! that asset's commit or content hash instead.
//!
//! # Entries on disk
//!
//! A successful run writes `<dir>/<hex>.json` recording the outputs it
//! produced. On lookup every recorded output must still exist and hash
//! to the recorded value, or the hit is downgraded to a miss.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::CacheError;
use crate::shell;
use crate::task::Task;

/// Cache-layout version. Bump when the key or the on-disk format changes.
///
/// **v3** adds the task name, its shell, working directory, `env_clear`
/// and output patterns to the key, so two tasks that run the same
/// command no longer share an entry.
pub const CACHE_FORMAT_VERSION: &str = "neomake-cache-v3";

/// File name of the remembered input hashes, inside the cache directory.
const HASHES_FILE: &str = "hashes.json";

/// The cache of one project.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
    dir: PathBuf,
    /// Directories (relative to the root) glob walks never enter.
    skip: Vec<PathBuf>,
    hashes: Arc<Mutex<HashMemo>>,
}

/// Remembered file hashes, keyed by path.
#[derive(Debug, Default, Serialize, Deserialize)]
struct HashMemo {
    files: HashMap<String, Remembered>,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Remembered {
    stamp: Stamp,
    sha256: String,
}

/// What must be unchanged for a remembered hash to still hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
    inode: u64,
}

impl Cache {
    /// Open (or lazily create) the cache at `<project>/.neomake/cache/`.
    pub fn open(project_root: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let root = project_root.into();
        let dir = root.join(".neomake").join("cache");
        Self::open_in(root, dir, Vec::new())
    }

    /// Open (or lazily create) a cache for `root` kept in `dir`. Glob
    /// walks for inputs and outputs never enter `skip` (relative to
    /// `root`), which should include the state directory.
    pub fn open_in(
        root: impl Into<PathBuf>,
        dir: impl Into<PathBuf>,
        skip: Vec<PathBuf>,
    ) -> Result<Self, CacheError> {
        let root = root.into();
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| CacheError::Io {
            path: dir.clone(),
            source: e,
        })?;
        let memo = std::fs::read(dir.join(HASHES_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<HashMemo>(&bytes).ok())
            .unwrap_or_default();
        Ok(Self {
            root,
            dir,
            skip,
            hashes: Arc::new(Mutex::new(memo)),
        })
    }

    /// Project root the cache was opened against.
    pub fn project_root(&self) -> &Path {
        &self.root
    }

    /// Directory where cache entries are stored.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Derive the cache key for `task`, given already-computed upstream
    /// dep keys (in dependency declaration order).
    ///
    /// This reads the task's input files; call it off the async runtime.
    pub fn compute_key(&self, task: &Task, dep_keys: &[String]) -> Result<String, CacheError> {
        let mut hasher = Sha256::new();
        let mut line = |parts: &[&[u8]]| {
            for p in parts {
                hasher.update(p);
            }
            hasher.update(b"\n");
        };
        line(&[CACHE_FORMAT_VERSION.as_bytes()]);
        line(&[b"name:", task.name.as_bytes()]);
        line(&[b"command:", task.command.as_bytes()]);
        line(&[
            b"prelude:",
            task.prelude.as_deref().unwrap_or("").as_bytes(),
        ]);
        line(&[b"steps:", format!("{:?}", task.steps).as_bytes()]);
        line(&[b"shell:"]);
        let shell_argv = task.shell.clone().unwrap_or_else(|| {
            let default = shell::resolve();
            std::iter::once(default.program)
                .chain(default.args)
                .collect()
        });
        for arg in &shell_argv {
            line(&[arg.as_bytes()]);
        }
        line(&[b"."]);
        line(&[b"cwd:", task.cwd.as_deref().unwrap_or("").as_bytes()]);
        line(&[b"env_clear:", if task.env_clear { b"1" } else { b"0" }]);
        line(&[b"env:"]);
        for (k, v) in &task.env {
            line(&[k.as_bytes(), b"=", v.as_bytes()]);
        }
        line(&[b"."]);
        line(&[b"deps:"]);
        for dk in dep_keys {
            line(&[dk.as_bytes()]);
        }
        line(&[b"."]);
        line(&[b"outputs:"]);
        for pattern in &task.outputs {
            line(&[pattern.as_bytes()]);
        }
        line(&[b"."]);

        line(&[b"inputs:"]);
        let manifest = crate::asset::AssetManifest::load(&self.root);
        let mut patterns = Vec::new();
        for pattern in &task.inputs {
            match asset_hash(&manifest, &self.root, pattern) {
                Some(hash) => line(&[b"asset ", pattern.as_bytes(), b":", hash.as_bytes()]),
                None => patterns.push(pattern.clone()),
            }
        }
        for (rel, abs) in self.expand(&patterns)? {
            let content_hash = self.hash_file(&abs)?;
            line(&[rel.as_bytes(), b":", content_hash.as_bytes()]);
        }
        line(&[b"."]);

        Ok(hex::encode(hasher.finalize()))
    }

    /// Attempt a cache lookup for `task` with the given computed key.
    ///
    /// Returns `Ok(Some(_))` only when a persisted entry exists *and*
    /// every declared output file still matches its recorded hash.
    pub fn lookup(&self, key: &str) -> Result<Option<CacheEntry>, CacheError> {
        let path = self.entry_path(key);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(CacheError::Io {
                    path: path.clone(),
                    source: e,
                })
            }
        };
        let entry: CacheEntry =
            serde_json::from_slice(&bytes).map_err(|e| CacheError::Corrupt {
                path: path.clone(),
                message: e.to_string(),
            })?;

        for out in &entry.outputs {
            let abs = self.root.join(&out.path);
            match self.hash_file(&abs) {
                Ok(h) if h == out.sha256 => {}
                _ => return Ok(None),
            }
        }
        Ok(Some(entry))
    }

    /// Persist a cache entry for `task` after a successful run.
    pub fn store(&self, task: &Task, key: &str, exit_code: i32) -> Result<CacheEntry, CacheError> {
        let outputs = self.expand(&task.outputs)?;
        let mut records = Vec::with_capacity(outputs.len());
        for (rel, abs) in outputs {
            let sha256 = self.hash_file(&abs)?;
            let size = std::fs::metadata(&abs)
                .map_err(|e| CacheError::Io {
                    path: abs.clone(),
                    source: e,
                })?
                .len();
            records.push(OutputRecord {
                path: rel,
                sha256,
                size,
            });
        }
        let entry = CacheEntry {
            key: key.to_string(),
            task: task.name.clone(),
            outputs: records,
            exit_code,
            finished_at: now_unix(),
        };
        let path = self.entry_path(key);
        let json = serde_json::to_vec_pretty(&entry).map_err(|e| CacheError::Corrupt {
            path: path.clone(),
            message: e.to_string(),
        })?;
        write_atomically(&path, &json)?;
        Ok(entry)
    }

    /// Write the remembered input hashes back to disk, if any changed.
    pub fn flush(&self) -> Result<(), CacheError> {
        let mut memo = self.hashes.lock().unwrap_or_else(|e| e.into_inner());
        if !memo.dirty {
            return Ok(());
        }
        // Forget files that are gone.
        memo.files.retain(|path, _| Path::new(path).exists());
        let json = serde_json::to_vec(&*memo).map_err(|e| CacheError::Corrupt {
            path: self.dir.join(HASHES_FILE),
            message: e.to_string(),
        })?;
        write_atomically(&self.dir.join(HASHES_FILE), &json)?;
        memo.dirty = false;
        Ok(())
    }

    /// Delete every entry in the cache.
    pub fn clean(&self) -> Result<(), CacheError> {
        if !self.dir.exists() {
            return Ok(());
        }
        for entry in read_dir(&self.dir)? {
            let p = entry.path();
            if p.is_file() {
                std::fs::remove_file(&p).map_err(|e| CacheError::Io {
                    path: p.clone(),
                    source: e,
                })?;
            }
        }
        let mut memo = self.hashes.lock().unwrap_or_else(|e| e.into_inner());
        memo.files.clear();
        memo.dirty = false;
        Ok(())
    }

    /// Summarize cache contents.
    pub fn status(&self) -> Result<CacheStatus, CacheError> {
        let mut entries = 0usize;
        let mut bytes = 0u64;
        if !self.dir.exists() {
            return Ok(CacheStatus { entries, bytes });
        }
        for entry in read_dir(&self.dir)? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json")
                && path.file_name().and_then(|s| s.to_str()) != Some(HASHES_FILE)
            {
                entries += 1;
                bytes += entry
                    .metadata()
                    .map(|m| m.len())
                    .map_err(|e| CacheError::Io {
                        path: entry.path(),
                        source: e,
                    })?;
            }
        }
        Ok(CacheStatus { entries, bytes })
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// Every file matching `patterns`, as sorted, de-duplicated
    /// `(relative, absolute)` pairs.
    fn expand(&self, patterns: &[String]) -> Result<Vec<(String, PathBuf)>, CacheError> {
        let mut found = std::collections::BTreeMap::new();
        for pattern in patterns {
            let matches = crate::glob::expand(&self.root, pattern, &self.skip).map_err(|e| {
                CacheError::Corrupt {
                    path: self.root.clone(),
                    message: e,
                }
            })?;
            for rel in matches {
                let abs = self.root.join(&rel);
                found.insert(rel, abs);
            }
        }
        Ok(found.into_iter().collect())
    }

    fn hash_file(&self, path: &Path) -> Result<String, CacheError> {
        let io = |e| CacheError::Io {
            path: path.to_path_buf(),
            source: e,
        };
        let meta = std::fs::metadata(path).map_err(io)?;
        let stamp = stamp_of(&meta);
        let key = path.to_string_lossy().into_owned();
        {
            let memo = self.hashes.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(known) = memo.files.get(&key) {
                if known.stamp == stamp {
                    return Ok(known.sha256.clone());
                }
            }
        }
        let sha256 = hash_contents(path)?;
        let mut memo = self.hashes.lock().unwrap_or_else(|e| e.into_inner());
        memo.files.insert(
            key,
            Remembered {
                stamp,
                sha256: sha256.clone(),
            },
        );
        memo.dirty = true;
        Ok(sha256)
    }
}

/// The asset hash for an input pattern naming a `git()`/`fetch()`
/// destination exactly.
fn asset_hash(
    manifest: &crate::asset::AssetManifest,
    root: &Path,
    pattern: &str,
) -> Option<String> {
    let wanted = normalize(pattern);
    manifest.assets.iter().find_map(|(path, record)| {
        let rel = path.strip_prefix(root).unwrap_or(path);
        (normalize(&rel.to_string_lossy()) == wanted).then(|| match &record.kind {
            crate::asset::AssetKind::Git { commit_sha } => commit_sha.clone(),
            crate::asset::AssetKind::Fetch { content_sha256 } => content_sha256.clone(),
        })
    })
}

fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    path.trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

#[cfg(unix)]
fn stamp_of(meta: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    Stamp {
        size: meta.len(),
        mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        ctime_ns: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
        inode: meta.ino(),
    }
}

#[cfg(not(unix))]
fn stamp_of(meta: &std::fs::Metadata) -> Stamp {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i128);
    Stamp {
        size: meta.len(),
        mtime_ns,
        ctime_ns: 0,
        inode: 0,
    }
}

fn read_dir(dir: &Path) -> Result<Vec<std::fs::DirEntry>, CacheError> {
    let io = |e| CacheError::Io {
        path: dir.to_path_buf(),
        source: e,
    };
    std::fs::read_dir(dir)
        .map_err(io)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let io = |e| CacheError::Io {
        path: path.to_path_buf(),
        source: e,
    };
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// Summary returned by [`Cache::status`].
#[derive(Debug, Clone, Copy)]
pub struct CacheStatus {
    /// Number of entries currently on disk.
    pub entries: usize,
    /// Total size of those entries in bytes.
    pub bytes: u64,
}

/// A persisted cache record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Content-addressed key (hex SHA-256).
    pub key: String,
    /// Task name this entry was produced by.
    pub task: String,
    /// Record of every output file that was present after the task ran.
    pub outputs: Vec<OutputRecord>,
    /// Exit code reported by the task's command.
    pub exit_code: i32,
    /// Seconds since the Unix epoch at which the entry was written.
    pub finished_at: u64,
}

/// One output file captured in a [`CacheEntry`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputRecord {
    /// Path relative to the project root.
    pub path: String,
    /// Hex SHA-256 of the file's contents.
    pub sha256: String,
    /// File size in bytes.
    pub size: u64,
}

fn hash_contents(path: &Path) -> Result<String, CacheError> {
    use std::io::Read;
    let io = |e| CacheError::Io {
        path: path.to_path_buf(),
        source: e,
    };
    let mut f = std::fs::File::open(path).map_err(io)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn mk_task(name: &str, command: &str, inputs: &[&str], outputs: &[&str]) -> Task {
        let mut t = Task::new(name, command);
        t.inputs = inputs.iter().map(|s| s.to_string()).collect();
        t.outputs = outputs.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn key_is_stable_across_calls() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cat a.txt", &["a.txt"], &[]);
        let k1 = cache.compute_key(&task, &[]).unwrap();
        let k2 = cache.compute_key(&task, &[]).unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn key_changes_when_input_changes() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cat a.txt", &["a.txt"], &[]);
        let k1 = cache.compute_key(&task, &[]).unwrap();
        fs::write(tmp.path().join("a.txt"), b"world!").unwrap();
        let k2 = cache.compute_key(&task, &[]).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn remembered_hashes_follow_same_size_rewrites() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        fs::write(&file, b"hello").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cat a.txt", &["a.txt"], &[]);
        let k1 = cache.compute_key(&task, &[]).unwrap();
        cache.flush().unwrap();
        // Same size, new content: the change time moves even if the
        // modification time were preserved.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&file, b"jello").unwrap();
        let reopened = Cache::open(tmp.path()).unwrap();
        assert_ne!(reopened.compute_key(&task, &[]).unwrap(), k1);
    }

    #[test]
    fn key_changes_with_every_part_of_the_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let base = mk_task("t", "echo hi", &[], &[]);
        let k = |t: &Task| cache.compute_key(t, &[]).unwrap();
        let k0 = k(&base);
        let mut variants = Vec::new();
        let mut t = base.clone();
        t.command = "echo HI".into();
        variants.push(t);
        let mut t = base.clone();
        t.env.insert("X".into(), "1".into());
        variants.push(t);
        let mut t = base.clone();
        t.name = "other".into();
        variants.push(t);
        let mut t = base.clone();
        t.outputs = vec!["out".into()];
        variants.push(t);
        let mut t = base.clone();
        t.cwd = Some("sub".into());
        variants.push(t);
        let mut t = base.clone();
        t.shell = Some(vec!["bash".into(), "-c".into()]);
        variants.push(t);
        let mut t = base.clone();
        t.env_clear = true;
        variants.push(t);
        for v in &variants {
            assert_ne!(k(v), k0, "{v:?}");
        }
    }

    #[test]
    fn key_propagates_upstream_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let t = mk_task("t", "echo", &[], &[]);
        let k1 = cache.compute_key(&t, &["aaaa".into()]).unwrap();
        let k2 = cache.compute_key(&t, &["bbbb".into()]).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn inputs_skip_the_state_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("build/state")).unwrap();
        fs::write(tmp.path().join("build/a.txt"), b"a").unwrap();
        let cache = Cache::open_in(
            tmp.path(),
            tmp.path().join("build/state/cache"),
            vec![PathBuf::from("build/state")],
        )
        .unwrap();
        let task = mk_task("t", "x", &["build/**"], &[]);
        let k1 = cache.compute_key(&task, &[]).unwrap();
        fs::write(tmp.path().join("build/state/noise.txt"), b"n").unwrap();
        assert_eq!(cache.compute_key(&task, &[]).unwrap(), k1);
    }

    #[test]
    fn store_and_lookup_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("out.txt"), b"result").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cp src out.txt", &[], &["out.txt"]);
        let key = cache.compute_key(&task, &[]).unwrap();
        cache.store(&task, &key, 0).unwrap();
        let hit = cache.lookup(&key).unwrap();
        assert!(hit.is_some());
    }

    #[test]
    fn lookup_miss_when_output_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("out.txt"), b"r").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cp", &[], &["out.txt"]);
        let key = cache.compute_key(&task, &[]).unwrap();
        cache.store(&task, &key, 0).unwrap();
        fs::remove_file(tmp.path().join("out.txt")).unwrap();
        let hit = cache.lookup(&key).unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn lookup_miss_when_output_mutated() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("out.txt"), b"r").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cp", &[], &["out.txt"]);
        let key = cache.compute_key(&task, &[]).unwrap();
        cache.store(&task, &key, 0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(tmp.path().join("out.txt"), b"tampered").unwrap();
        let hit = cache.lookup(&key).unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn clean_removes_entries() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("o"), b"x").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let t = mk_task("t", "echo", &[], &["o"]);
        let k = cache.compute_key(&t, &[]).unwrap();
        cache.store(&t, &k, 0).unwrap();
        cache.flush().unwrap();
        assert!(cache.status().unwrap().entries >= 1);
        cache.clean().unwrap();
        assert_eq!(cache.status().unwrap().entries, 0);
    }
}
