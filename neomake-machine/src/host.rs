//! What the host offers machines: QEMU, KVM, UEFI firmware, virtiofsd.

use std::path::{Path, PathBuf};

use neomake_core::machine::{Firmware, Kvm, MachineSpec};

/// Where distributions install OVMF, as (code, variables) pairs.
const OVMF: &[(&str, &str)] = &[
    // Arch
    (
        "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
        "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
    ),
    (
        "/usr/share/edk2/x64/OVMF_CODE.fd",
        "/usr/share/edk2/x64/OVMF_VARS.fd",
    ),
    // Debian, Ubuntu
    (
        "/usr/share/OVMF/OVMF_CODE_4M.fd",
        "/usr/share/OVMF/OVMF_VARS_4M.fd",
    ),
    (
        "/usr/share/OVMF/OVMF_CODE.fd",
        "/usr/share/OVMF/OVMF_VARS.fd",
    ),
    // Fedora
    (
        "/usr/share/edk2/ovmf/OVMF_CODE.fd",
        "/usr/share/edk2/ovmf/OVMF_VARS.fd",
    ),
    // NixOS's qemu, Homebrew's
    (
        "/run/libvirt/nix-ovmf/OVMF_CODE.fd",
        "/run/libvirt/nix-ovmf/OVMF_VARS.fd",
    ),
    (
        "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
        "/opt/homebrew/share/qemu/edk2-i386-vars.fd",
    ),
];

/// Where virtiofsd is installed when it is not on `PATH`.
const VIRTIOFSD: &[&str] = &[
    "/usr/lib/virtiofsd",
    "/usr/libexec/virtiofsd",
    "/usr/lib/qemu/virtiofsd",
];

/// A program by name (found on `PATH`) or path (relative to `root`).
pub fn program(name: &str, root: &Path) -> Option<PathBuf> {
    if name.contains('/') {
        let path = if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else {
            root.join(name)
        };
        return path.is_file().then_some(path);
    }
    which::which(name).ok()
}

/// Whether this process may use KVM.
pub fn kvm_usable() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

/// The UEFI firmware's code and variable store for `firmware`.
pub fn ovmf(firmware: &Firmware, root: &Path) -> Option<(PathBuf, PathBuf)> {
    let Firmware::Uefi { code, vars } = firmware else {
        return None;
    };
    let at = |p: &str| {
        if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            root.join(p)
        }
    };
    match (code, vars) {
        (Some(c), Some(v)) => Some((at(c), at(v))),
        _ => OVMF
            .iter()
            .map(|(c, v)| (PathBuf::from(c), PathBuf::from(v)))
            .find(|(c, v)| c.is_file() && v.is_file())
            .map(|(c, v)| {
                (
                    code.as_deref().map(at).unwrap_or(c),
                    vars.as_deref().map(at).unwrap_or(v),
                )
            }),
    }
}

/// virtiofsd, if it is installed.
pub fn virtiofsd() -> Option<PathBuf> {
    which::which("virtiofsd")
        .ok()
        .or_else(|| VIRTIOFSD.iter().map(PathBuf::from).find(|p| p.is_file()))
}

/// Why `spec` cannot run on this host, if it cannot. With `no_kvm`, KVM
/// is taken to be missing.
pub fn unmet(spec: &MachineSpec, root: &Path, no_kvm: bool) -> Option<String> {
    let mut missing = Vec::new();
    if program(&spec.qemu, root).is_none() {
        missing.push(format!("{} not found", spec.qemu));
    }
    if spec.kvm == Kvm::Require && (no_kvm || !kvm_usable()) {
        missing.push(format!(
            "machine `{}` needs KVM, and /dev/kvm {}",
            spec.name,
            if no_kvm {
                "is switched off (--no-kvm)"
            } else {
                "cannot be opened"
            }
        ));
    }
    if let Firmware::Uefi { code, vars } = &spec.firmware {
        match ovmf(&spec.firmware, root) {
            None => missing.push(
                "no UEFI firmware (OVMF) found; install it, or give the machine `firmware = { code = \"…\", vars = \"…\" }`".into(),
            ),
            Some((c, v)) => {
                for (given, path) in [(code, c), (vars, v)] {
                    if given.is_some() && !path.is_file() {
                        missing.push(format!("{} missing", path.display()));
                    }
                }
            }
        }
    }
    if spec.shares.iter().any(|s| !s.optional) && virtiofsd().is_none() {
        missing.push("virtiofsd not found (the machine shares a directory)".into());
    }
    (!missing.is_empty()).then(|| missing.join("; "))
}

/// Whether `spec` runs with KVM here.
pub fn use_kvm(spec: &MachineSpec, no_kvm: bool) -> bool {
    match spec.kvm {
        Kvm::Off => false,
        Kvm::Require | Kvm::Prefer => !no_kvm && kvm_usable(),
    }
}
