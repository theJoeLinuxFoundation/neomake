//! QEMU machines for neomake's machine tests.
//!
//! [`QemuRunner`] is what the `neomake` binary gives the executor to run
//! tests that have a `machine` (see `neomake_core::machine`). It starts
//! QEMU with a per-run copy of the machine's disks ([`disk`]) and drives
//! its serial console ([`machine`]): waiting for output, running commands
//! and checking what they print, writing files, rebooting and cutting
//! the power. [`interactive`] boots a machine on the terminal instead,
//! for a person, which `neomake machine run` does.
//!
//! Nothing here needs root: the machine is QEMU with KVM, disks are
//! copies (reflinks on btrfs), and shared directories go through
//! virtiofsd running as the user.

#![warn(missing_docs)]
#![deny(clippy::all)]

pub mod disk;
pub mod host;
pub mod interactive;
pub mod machine;
pub mod qemu;
pub mod qmp;
mod runner;
pub mod virtiofs;

use std::path::PathBuf;

use neomake_core::executor::Sink;
use neomake_core::machine::{BoxFuture, MachineEnv, MachineRunner, MachineSpec};
use neomake_core::steps::host::StepsResult;
use neomake_core::Task;

/// Runs machine tests with QEMU.
#[derive(Debug, Clone)]
pub struct QemuRunner {
    root: PathBuf,
    no_kvm: bool,
}

impl QemuRunner {
    /// A runner for the project at `root`. With `no_kvm`, KVM is taken to
    /// be missing: machines that require it are skipped, and the rest are
    /// emulated.
    pub fn new(root: impl Into<PathBuf>, no_kvm: bool) -> Self {
        Self {
            root: root.into(),
            no_kvm,
        }
    }
}

impl MachineRunner for QemuRunner {
    fn unmet(&self, machine: &MachineSpec) -> Option<String> {
        host::unmet(machine, &self.root, self.no_kvm)
    }

    fn run<'a>(
        &'a self,
        task: &'a Task,
        env: MachineEnv<'a>,
        sink: &'a Sink,
    ) -> BoxFuture<'a, StepsResult> {
        Box::pin(runner::run_test(self, task, env, sink))
    }
}
