//! A machine on the terminal, for a person.
//!
//! `neomake machine run NAME` boots a machine with its serial console on
//! the terminal (and its display in a window, if it has one); Ctrl-A X
//! leaves it. By default it works on a fresh copy of the machine's disks
//! in the state directory, as a test would; with a persistent directory
//! the copy is made once and kept, so what is done in the machine
//! remains for next time. This is what joe Linux's `image/run.sh` did.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use neomake_core::machine::{Disk, Firmware, MachineSpec, Media};

use crate::disk;
use crate::host;
use crate::qemu::{self, Layout, Serial};
use crate::virtiofs::Daemon;

/// How to run a machine interactively.
#[derive(Debug, Clone)]
pub struct Options {
    /// Keep the machine's disks here, and reuse them next time.
    pub persist: Option<PathBuf>,
    /// Where a fresh copy goes otherwise.
    pub scratch: PathBuf,
    /// Further media to attach (as CDs).
    pub media: Vec<PathBuf>,
    /// Take KVM to be missing.
    pub no_kvm: bool,
}

/// Boot `spec` on the terminal and wait for QEMU to exit.
pub async fn run(spec: &MachineSpec, root: &Path, options: &Options) -> Result<ExitStatus, String> {
    let (dir, keep) = match &options.persist {
        Some(dir) => (dir.clone(), true),
        None => (options.scratch.clone(), false),
    };
    if !keep {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let at = |p: &str| {
        if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            root.join(p)
        }
    };

    let mut disks = Vec::new();
    for (i, d) in spec.disks.iter().enumerate() {
        let path = dir.join(disk_name(d, i));
        if !path.exists() {
            let size = d.size.as_deref().map(disk::parse_size).transpose()?;
            match &d.file {
                Some(file) => {
                    let src = at(file);
                    if !src.is_file() {
                        return Err(format!("{} does not exist; build it first", src.display()));
                    }
                    println!("copying {} to {}", src.display(), path.display());
                    disk::copy(&src, &path).map_err(|e| e.to_string())?;
                    if let Some(size) = size {
                        disk::grow(&path, size).map_err(|e| e.to_string())?;
                    }
                }
                None => disk::blank(&path, size.unwrap_or(0)).map_err(|e| e.to_string())?,
            }
        }
        disks.push(path);
    }
    let firmware = match &spec.firmware {
        Firmware::Bios => None,
        f @ Firmware::Uefi { .. } => {
            let (code, vars) = host::ovmf(f, root).ok_or("no UEFI firmware (OVMF) found")?;
            let copy = dir.join("OVMF_VARS.fd");
            if !copy.exists() {
                std::fs::copy(&vars, &copy).map_err(|e| e.to_string())?;
            }
            Some((code, copy))
        }
    };
    let mut media: Vec<(Media, PathBuf)> = spec
        .media
        .iter()
        .map(|m| (m.clone(), at(&m.file)))
        .collect();
    for (i, file) in options.media.iter().enumerate() {
        media.push((
            Media {
                name: format!("extra{i}"),
                file: file.display().to_string(),
                kind: neomake_core::machine::MediaKind::Cd,
            },
            file.clone(),
        ));
    }

    let mut daemons = Vec::new();
    let mut shares = Vec::new();
    for (i, share) in spec.shares.iter().enumerate() {
        let Some(binary) = host::virtiofsd() else {
            if share.optional {
                continue;
            }
            return Err("virtiofsd not found".into());
        };
        let socket = std::env::temp_dir().join(format!("nm-share-{}-{i}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let shared = at(&share.dir);
        std::fs::create_dir_all(&shared).map_err(|e| e.to_string())?;
        daemons.push(
            Daemon::start(
                &binary,
                &shared,
                &socket,
                &dir.join(format!("virtiofsd-{i}.log")),
            )
            .await?,
        );
        shares.push((socket, share.tag.clone()));
    }

    let layout = Layout {
        disks,
        firmware,
        media,
        qmp: None,
        shares,
        kvm: host::use_kvm(spec, options.no_kvm),
    };
    let qemu_bin =
        host::program(&spec.qemu, root).ok_or_else(|| format!("{} not found", spec.qemu))?;
    let args = qemu::args(spec, &layout, Serial::Interactive);
    println!(
        "machine `{}` in {} (Ctrl-A X quits)",
        spec.name,
        dir.display()
    );
    let status = tokio::process::Command::new(qemu_bin)
        .args(&args)
        .envs(&spec.env)
        .status()
        .await
        .map_err(|e| format!("could not start {}: {e}", spec.qemu))?;
    for mut d in daemons {
        d.stop().await;
    }
    Ok(status)
}

fn disk_name(d: &Disk, i: usize) -> String {
    match (&d.name, i) {
        (Some(n), _) => format!("disk-{n}.img"),
        (None, 0) => "disk.img".to_string(),
        (None, i) => format!("disk-{i}.img"),
    }
}
