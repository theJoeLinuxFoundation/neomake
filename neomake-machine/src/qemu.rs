//! QEMU's command line for a machine.
//!
//! The command line is what joe Linux's harness and `image/run.sh` use: a
//! q35 machine with KVM (`-cpu host`) or without (`-cpu max`), UEFI from
//! pflash with a per-run copy of the variable store, raw virtio disks,
//! user-mode networking, and the serial console on standard input and
//! output. Media attach as a CD on q35's AHCI, or as a USB stick on an
//! xHCI controller, first in the boot order either way. A display adds a
//! virtio GPU, keyboard and tablet, with QMP for screenshots; shares add
//! a memfd memory backend and one vhost-user-fs device each.

use std::path::PathBuf;

use neomake_core::machine::{MachineSpec, Media, MediaKind};

/// Where a run's files for a machine are.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The disks, in order (the first is booted from).
    pub disks: Vec<PathBuf>,
    /// UEFI code and the run's copy of the variable store.
    pub firmware: Option<(PathBuf, PathBuf)>,
    /// Media attached at the next start, with their image paths.
    pub media: Vec<(Media, PathBuf)>,
    /// QMP's socket, when the machine has a display.
    pub qmp: Option<PathBuf>,
    /// virtiofsd sockets and the tags they are shared by.
    pub shares: Vec<(PathBuf, String)>,
    /// Use KVM.
    pub kvm: bool,
}

/// Where the serial console goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serial {
    /// Standard input and output, for a test to drive.
    Stdio,
    /// Standard input and output with QEMU's monitor multiplexed
    /// (Ctrl-A X quits), for a person.
    Interactive,
}

/// QEMU's arguments for `spec` laid out as `layout`.
pub fn args(spec: &MachineSpec, layout: &Layout, serial: Serial) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    if layout.kvm {
        push(&["-enable-kvm", "-cpu", "host"]);
    } else {
        push(&["-cpu", "max"]);
    }
    push(&[
        "-machine",
        "q35",
        "-m",
        &spec.memory,
        "-smp",
        &spec.cpus.to_string(),
    ]);
    if let Some((code, vars)) = &layout.firmware {
        push(&[
            "-drive",
            &format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
            "-drive",
            &format!("if=pflash,format=raw,file={}", vars.display()),
        ]);
    }
    for disk in &layout.disks {
        push(&[
            "-drive",
            &format!("file={},format=raw,if=virtio,cache=unsafe", disk.display()),
        ]);
    }
    let mut xhci = false;
    for (media, file) in &layout.media {
        let id = drive_id(&media.name);
        match media.kind {
            MediaKind::Cd => push(&[
                "-drive",
                &format!(
                    "if=none,id={id},format=raw,media=cdrom,readonly=on,file={}",
                    file.display()
                ),
                "-device",
                &format!("ide-cd,drive={id},bootindex=0"),
            ]),
            MediaKind::Usb => {
                if !xhci {
                    push(&["-device", "qemu-xhci,id=xhci"]);
                    xhci = true;
                }
                push(&[
                    "-drive",
                    &format!(
                        "if=none,id={id},format=raw,readonly=on,file={}",
                        file.display()
                    ),
                    "-device",
                    &format!("usb-storage,bus=xhci.0,drive={id},bootindex=0"),
                ]);
            }
        }
    }
    if spec.net {
        push(&[
            "-netdev",
            "user,id=n0",
            "-device",
            "virtio-net-pci,netdev=n0",
        ]);
    } else {
        push(&["-nic", "none"]);
    }
    if !layout.shares.is_empty() {
        push(&[
            "-object",
            &format!("memory-backend-memfd,id=mem,size={},share=on", spec.memory),
            "-numa",
            "node,memdev=mem",
        ]);
        for (i, (socket, tag)) in layout.shares.iter().enumerate() {
            push(&[
                "-chardev",
                &format!("socket,id=vfs{i},path={}", socket.display()),
                "-device",
                &format!("vhost-user-fs-pci,chardev=vfs{i},tag={tag}"),
            ]);
        }
    }
    if spec.display {
        push(&[
            "-device",
            "virtio-vga",
            "-device",
            "virtio-keyboard-pci",
            "-device",
            "virtio-tablet-pci",
        ]);
    }
    match serial {
        Serial::Stdio => {
            push(&["-display", "none", "-monitor", "none", "-serial", "stdio"]);
        }
        Serial::Interactive => {
            if spec.display {
                push(&["-display", "gtk", "-serial", "mon:stdio"]);
            } else {
                push(&["-nographic"]);
            }
        }
    }
    if let Some(qmp) = &layout.qmp {
        push(&[
            "-qmp",
            &format!("unix:{},server=on,wait=off", qmp.display()),
        ]);
    }
    a.extend(spec.extra_args.iter().cloned());
    a
}

/// A medium's name as a QEMU drive id.
fn drive_id(name: &str) -> String {
    let id: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("m_{id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use neomake_core::machine::{Console, Firmware, Kvm};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn spec() -> MachineSpec {
        MachineSpec {
            name: "m".into(),
            qemu: "qemu-system-x86_64".into(),
            disks: vec![],
            media: vec![],
            memory: "2G".into(),
            cpus: 4,
            kvm: Kvm::Require,
            firmware: Firmware::Uefi {
                code: None,
                vars: None,
            },
            display: false,
            shares: vec![],
            net: true,
            extra_args: vec!["-rtc".into(), "base=utc".into()],
            env: BTreeMap::new(),
            console: Console {
                prompt: "joe# ".into(),
                banner: None,
                setup: None,
                reboot: "systemctl reboot".into(),
                poweroff: "systemctl poweroff".into(),
            },
            boot_timeout: Duration::from_secs(1),
            command_timeout: Duration::from_secs(1),
            source: None,
        }
    }

    #[test]
    fn builds_the_harness_command_line() {
        let layout = Layout {
            disks: vec![PathBuf::from("/r/disk.img")],
            firmware: Some((PathBuf::from("/fw/CODE.fd"), PathBuf::from("/r/VARS.fd"))),
            media: vec![(
                Media {
                    name: "medium".into(),
                    file: "x".into(),
                    kind: MediaKind::Usb,
                },
                PathBuf::from("/i/live.iso"),
            )],
            qmp: None,
            shares: vec![(PathBuf::from("/s/vfs.sock"), "host".into())],
            kvm: true,
        };
        let line = args(&spec(), &layout, Serial::Stdio).join(" ");
        for part in [
            "-enable-kvm -cpu host -machine q35 -m 2G -smp 4",
            "-drive if=pflash,format=raw,readonly=on,file=/fw/CODE.fd",
            "-drive if=pflash,format=raw,file=/r/VARS.fd",
            "-drive file=/r/disk.img,format=raw,if=virtio,cache=unsafe",
            "-device qemu-xhci,id=xhci -drive if=none,id=m_medium,format=raw,readonly=on,file=/i/live.iso -device usb-storage,bus=xhci.0,drive=m_medium,bootindex=0",
            "-netdev user,id=n0 -device virtio-net-pci,netdev=n0",
            "-object memory-backend-memfd,id=mem,size=2G,share=on -numa node,memdev=mem",
            "-chardev socket,id=vfs0,path=/s/vfs.sock -device vhost-user-fs-pci,chardev=vfs0,tag=host",
            "-display none -monitor none -serial stdio",
        ] {
            assert!(line.contains(part), "{part:?} missing from\n{line}");
        }
        assert!(line.ends_with("-rtc base=utc"));
        let no_kvm = Layout {
            kvm: false,
            ..layout
        };
        assert!(args(&spec(), &no_kvm, Serial::Stdio)
            .join(" ")
            .starts_with("-cpu max"));
    }
}
