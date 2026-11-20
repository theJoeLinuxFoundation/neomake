//! Machines: what a machine test runs its steps in.
//!
//! A machine is a QEMU virtual machine, described under
//! `[machines.NAME]` ([`MachineSpec`]) and used by a test that names it
//! in `machine`. The test's `run` steps then run in the machine's shell,
//! over its serial console, and further steps drive the machine itself:
//! wait for output, reboot, cut the power, take a screenshot, change
//! media (see [`crate::steps::Action`]).
//!
//! The core knows machines only as data and through [`MachineRunner`],
//! which the `neomake-machine` crate implements, so that the engine does
//! not depend on QEMU.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use crate::executor::Sink;
use crate::steps::host::{HostEnv, StepsResult};
use crate::task::{Location, Task};

/// A machine, as configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSpec {
    /// Its name.
    pub name: String,
    /// The QEMU program.
    pub qemu: String,
    /// Its disks; the first is the one it boots from.
    pub disks: Vec<Disk>,
    /// Removable media attached when it starts.
    pub media: Vec<Media>,
    /// Memory, as QEMU's `-m` takes it (`2G`).
    pub memory: String,
    /// Processors.
    pub cpus: u32,
    /// Whether it uses KVM.
    pub kvm: Kvm,
    /// Its firmware.
    pub firmware: Firmware,
    /// Give it a display (a virtio GPU, keyboard and tablet), which
    /// screenshots are taken of.
    pub display: bool,
    /// Host directories shared with it over virtiofs.
    pub shares: Vec<Share>,
    /// User-mode networking (the host is 10.0.2.2).
    pub net: bool,
    /// Further QEMU arguments.
    pub extra_args: Vec<String>,
    /// Variables set for QEMU.
    pub env: BTreeMap<String, String>,
    /// How its serial console is driven.
    pub console: Console,
    /// How long a boot may take.
    pub boot_timeout: Duration,
    /// How long a command may take, unless its step says otherwise.
    pub command_timeout: Duration,
    /// Where it is defined.
    pub source: Option<Location>,
}

/// One disk of a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// A name, for placeholders (`{{machine.disks.NAME}}`).
    pub name: Option<String>,
    /// The image it starts as (copied for each run, relative to the
    /// project root); without one, the disk starts empty.
    pub file: Option<String>,
    /// Its size, as `truncate -s` takes it (`40G`): an empty disk's size,
    /// or how far a copied one is grown.
    pub size: Option<String>,
}

/// A removable medium.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    /// Its name, which `detach` uses.
    pub name: String,
    /// The image (relative to the project root).
    pub file: String,
    /// How it is attached.
    pub kind: MediaKind,
}

/// How a medium is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// A CD, the first thing the firmware tries.
    Cd,
    /// A USB stick.
    Usb,
}

/// A host directory shared with the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    /// The directory (relative to the project root).
    pub dir: String,
    /// The tag the guest mounts it by.
    pub tag: String,
    /// Run without it when virtiofsd is not installed.
    pub optional: bool,
}

/// Whether a machine uses KVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kvm {
    /// Only with KVM: without it, the test is skipped.
    Require,
    /// With KVM when it can be had, else emulated (slowly).
    Prefer,
    /// Never.
    Off,
}

/// A machine's firmware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Firmware {
    /// UEFI (OVMF); without paths, the host's are found.
    Uefi {
        /// The code image.
        code: Option<String>,
        /// The variable store it starts with (copied for each run).
        vars: Option<String>,
    },
    /// QEMU's BIOS.
    Bios,
}

/// How a machine's serial console is driven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Console {
    /// The shell prompt, which ends every command's output.
    pub prompt: String,
    /// What the firmware prints as it starts a boot; a boot is waited
    /// for by this, then the prompt.
    pub banner: Option<String>,
    /// Commands run whenever the prompt first appears after a boot.
    pub setup: Option<String>,
    /// The command that reboots.
    pub reboot: String,
    /// The command that powers off.
    pub poweroff: String,
}

/// The placeholders a machine test has, besides every test's.
pub const MACHINE_VARS: &[&str] = &[
    "machine.name",
    "machine.dir",
    "machine.disk",
    "machine.console",
];

/// What a machine test runs with.
pub struct MachineEnv<'a> {
    /// The test's steps' host environment (for `host` steps), with the
    /// test's variables.
    pub host: HostEnv<'a>,
    /// The test's directory: the machine's disks, console log and
    /// screenshots go here.
    pub dir: PathBuf,
    /// The project root.
    pub root: PathBuf,
    /// The test's file, for messages.
    pub file: Option<String>,
}

/// A boxed future, as [`MachineRunner`] returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs machine tests; implemented by the `neomake-machine` crate.
pub trait MachineRunner: Send + Sync + std::fmt::Debug {
    /// Why `machine` cannot run on this host (QEMU or KVM or firmware
    /// missing), if it cannot.
    fn unmet(&self, machine: &MachineSpec) -> Option<String>;

    /// Run `task`'s steps in its machine.
    fn run<'a>(
        &'a self,
        task: &'a Task,
        env: MachineEnv<'a>,
        sink: &'a Sink,
    ) -> BoxFuture<'a, StepsResult>;
}
