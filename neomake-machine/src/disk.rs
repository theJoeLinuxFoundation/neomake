//! Disk images: per-run copies, empty disks, sizes.
//!
//! A machine never boots its configured image: each run gets its own
//! copy, so the image stays as built. On btrfs (which joe Linux's
//! `build/` must be on) the copy is a reflink and costs nothing; on other
//! filesystems the copy skips zeroed blocks, so a sparse 40 GB image
//! stays sparse.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Copy `src` to `dst`: a reflink where the filesystem has them, else a
/// copy that keeps holes.
pub fn copy(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(dst);
    if reflink(src, dst).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_file(dst);
    sparse_copy(src, dst)
}

#[cfg(target_os = "linux")]
fn reflink(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // FICLONE, from linux/fs.h.
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let from = File::open(src)?;
    let to = File::create(dst)?;
    // SAFETY: both descriptors are open for the duration of the call.
    let r = unsafe { libc::ioctl(to.as_raw_fd(), FICLONE as _, from.as_raw_fd()) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn reflink(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::other("no reflinks here"))
}

fn sparse_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let mut from = File::open(src)?;
    let mut to = File::create(dst)?;
    let len = from.metadata()?.len();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = read_full(&mut from, &mut buf)?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|&b| b == 0) {
            to.seek(SeekFrom::Current(n as i64))?;
        } else {
            to.write_all(&buf[..n])?;
        }
    }
    to.set_len(len)?;
    Ok(())
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Make `path` an empty (sparse) disk of `size` bytes.
pub fn blank(path: &Path, size: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = File::create(path)?;
    f.set_len(size)
}

/// Grow `path` to `size` bytes, if it is smaller.
pub fn grow(path: &Path, size: u64) -> io::Result<()> {
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    if f.metadata()?.len() < size {
        f.set_len(size)?;
    }
    Ok(())
}

/// A size as `truncate -s` takes it: a number of bytes, or with `K`, `M`,
/// `G` or `T` (powers of 1024).
pub fn parse_size(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let (digits, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len()));
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("`{text}` is not a size such as 40G"))?;
    let shift = match unit.trim_end_matches(['B', 'b', 'i']) {
        "" => 0,
        "K" | "k" => 10,
        "M" | "m" => 20,
        "G" | "g" => 30,
        "T" | "t" => 40,
        _ => return Err(format!("`{text}` is not a size such as 40G")),
    };
    n.checked_shl(shift)
        .filter(|v| v >> shift == n)
        .ok_or_else(|| format!("`{text}` is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("4K").unwrap(), 4096);
        assert_eq!(parse_size("40G").unwrap(), 40 << 30);
        assert_eq!(parse_size("2GiB").unwrap(), 2 << 30);
        assert!(parse_size("forty").is_err());
    }

    #[test]
    fn copies_keep_content_and_holes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.img");
        {
            let mut f = File::create(&src).unwrap();
            f.write_all(b"head").unwrap();
            f.seek(SeekFrom::Start(8 << 20)).unwrap();
            f.write_all(b"tail").unwrap();
        }
        let dst = tmp.path().join("copy/disk.img");
        copy(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dst).unwrap());
        sparse_copy(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dst).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let blocks = std::fs::metadata(&dst).unwrap().blocks();
            assert!(
                blocks * 512 < 4 << 20,
                "the hole was written out ({blocks} blocks)"
            );
        }
        blank(&tmp.path().join("blank.img"), 1 << 30).unwrap();
        assert_eq!(
            std::fs::metadata(tmp.path().join("blank.img"))
                .unwrap()
                .len(),
            1 << 30
        );
    }
}
