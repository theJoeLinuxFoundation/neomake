//! Reading `[machines.*]` and the `machine` of tests.
//!
//! A machine may build on another (`use = "base"`), taking every field
//! it does not set itself; `console` is merged field by field. A test
//! names a machine (`machine = "scaffold"`) or adjusts one
//! (`machine = { use = "scaffold", memory = "6G" }`), the same way. Since
//! a test's machine may come from its matrix
//! (`machine = "{{matrix.base}}"`), tests' machines are resolved after
//! matrices are expanded.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use super::RawDuration;
use crate::machine::{Console, Disk, Firmware, Kvm, MachineSpec, Media, MediaKind, Share};
use crate::task::Location;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawMachine {
    #[serde(default, rename = "use")]
    use_machine: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    #[serde(default)]
    qemu: Option<String>,
    #[serde(default)]
    disk: Option<String>,
    #[serde(default)]
    disks: Option<Vec<RawDisk>>,
    #[serde(default)]
    media: Option<Vec<RawMedia>>,
    #[serde(default)]
    memory: Option<String>,
    #[serde(default)]
    cpus: Option<u32>,
    #[serde(default)]
    kvm: Option<String>,
    #[serde(default)]
    firmware: Option<RawFirmware>,
    #[serde(default)]
    display: Option<bool>,
    #[serde(default)]
    shares: Option<Vec<RawShare>>,
    #[serde(default)]
    net: Option<bool>,
    #[serde(default)]
    extra_args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    console: Option<RawConsole>,
    #[serde(default)]
    boot_timeout: Option<RawDuration>,
    #[serde(default)]
    command_timeout: Option<RawDuration>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDisk {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    size: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawMedia {
    #[serde(default)]
    name: Option<String>,
    file: String,
    #[serde(default, rename = "as")]
    kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawFirmware {
    Kind(String),
    Uefi {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        vars: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShare {
    dir: String,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    optional: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConsole {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    setup: Option<String>,
    #[serde(default)]
    reboot: Option<String>,
    #[serde(default)]
    poweroff: Option<String>,
}

/// Every machine of the configuration.
#[derive(Default)]
pub(super) struct Machines {
    by_name: BTreeMap<String, (RawMachine, Location)>,
}

impl Machines {
    pub(super) fn add(
        &mut self,
        name: String,
        raw: RawMachine,
        at: Location,
    ) -> Result<(), String> {
        if let Some((_, first)) = self.by_name.get(&name) {
            return Err(format!("machine `{name}` is already defined at {first}"));
        }
        self.by_name.insert(name, (raw, at));
        Ok(())
    }

    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// `raw`, with every field it does not set taken from the machine it
    /// uses (and so on).
    fn flatten(&self, raw: &RawMachine, depth: usize) -> Result<RawMachine, String> {
        let Some(base_name) = &raw.use_machine else {
            return Ok(raw.clone());
        };
        if depth > 8 {
            return Err(format!(
                "machines use each other more than 8 deep (through `{base_name}`)"
            ));
        }
        let (base, _) =
            self.by_name.get(base_name).ok_or_else(|| {
                match crate::executor::closest(base_name, self.names()) {
                    Some(s) => format!("no machine named `{base_name}` (did you mean `{s}`?)"),
                    None => format!("no machine named `{base_name}`"),
                }
            })?;
        let base = self.flatten(base, depth + 1)?;
        let console = match (&raw.console, &base.console) {
            (Some(c), Some(b)) => Some(RawConsole {
                prompt: c.prompt.clone().or(b.prompt.clone()),
                banner: c.banner.clone().or(b.banner.clone()),
                setup: c.setup.clone().or(b.setup.clone()),
                reboot: c.reboot.clone().or(b.reboot.clone()),
                poweroff: c.poweroff.clone().or(b.poweroff.clone()),
            }),
            (c, b) => c.clone().or(b.clone()),
        };
        Ok(RawMachine {
            use_machine: None,
            description: raw.description.clone().or(base.description),
            qemu: raw.qemu.clone().or(base.qemu),
            // A machine that names its own disks replaces its base's.
            disk: if raw.disk.is_some() || raw.disks.is_some() {
                raw.disk.clone()
            } else {
                base.disk
            },
            disks: if raw.disk.is_some() || raw.disks.is_some() {
                raw.disks.clone()
            } else {
                base.disks
            },
            media: raw.media.clone().or(base.media),
            memory: raw.memory.clone().or(base.memory),
            cpus: raw.cpus.or(base.cpus),
            kvm: raw.kvm.clone().or(base.kvm),
            firmware: raw.firmware.clone().or(base.firmware),
            display: raw.display.or(base.display),
            shares: raw.shares.clone().or(base.shares),
            net: raw.net.or(base.net),
            extra_args: raw.extra_args.clone().or(base.extra_args),
            env: raw.env.clone().or(base.env),
            console,
            boot_timeout: raw.boot_timeout.clone().or(base.boot_timeout),
            command_timeout: raw.command_timeout.clone().or(base.command_timeout),
        })
    }

    /// The machine a test's `machine` value means.
    pub(super) fn resolve(
        &self,
        value: &toml::Value,
        at: &Location,
    ) -> Result<MachineSpec, String> {
        let raw = match value {
            toml::Value::String(name) => RawMachine {
                use_machine: Some(name.clone()),
                ..RawMachine::default()
            },
            other => other
                .clone()
                .try_into::<RawMachine>()
                .map_err(|e| format!("machine: {}", e.message().trim_end()))?,
        };
        let name = raw
            .use_machine
            .clone()
            .unwrap_or_else(|| "machine".to_string());
        let flat = self.flatten(&raw, 0)?;
        let source = self
            .by_name
            .get(&name)
            .map(|(_, l)| l.clone())
            .or_else(|| Some(at.clone()));
        build(name, flat, source)
    }
}

/// Every machine of `[machines.*]`, checked.
pub(super) fn check(
    machines: &Machines,
) -> Result<BTreeMap<String, MachineSpec>, (Location, String)> {
    let mut out = BTreeMap::new();
    for (name, (raw, at)) in &machines.by_name {
        let flat = machines.flatten(raw, 0).map_err(|m| (at.clone(), m))?;
        let spec = build(name.clone(), flat, Some(at.clone())).map_err(|m| (at.clone(), m))?;
        out.insert(name.clone(), spec);
    }
    Ok(out)
}

fn build(name: String, raw: RawMachine, source: Option<Location>) -> Result<MachineSpec, String> {
    let mut disks = Vec::new();
    if let Some(file) = raw.disk {
        disks.push(Disk {
            name: None,
            file: Some(file),
            size: None,
        });
    }
    for d in raw.disks.unwrap_or_default() {
        if d.file.is_none() && d.size.is_none() {
            return Err("a disk has a `file` to start from, or a `size` to start empty".into());
        }
        disks.push(Disk {
            name: d.name,
            file: d.file,
            size: d.size,
        });
    }
    let media = raw
        .media
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, m)| media(m, i))
        .collect::<Result<Vec<_>, _>>()?;
    if disks.is_empty() && media.is_empty() {
        return Err(format!(
            "machine `{name}` has nothing to boot from: give it a `disk`, `disks` or `media`"
        ));
    }
    let kvm = match raw.kvm.as_deref() {
        None | Some("require") => Kvm::Require,
        Some("prefer") => Kvm::Prefer,
        Some("off") => Kvm::Off,
        Some(other) => {
            return Err(format!(
                "`kvm` is \"require\", \"prefer\" or \"off\", not {other:?}"
            ))
        }
    };
    let firmware = match raw.firmware {
        None => Firmware::Uefi {
            code: None,
            vars: None,
        },
        Some(RawFirmware::Kind(k)) if k == "uefi" => Firmware::Uefi {
            code: None,
            vars: None,
        },
        Some(RawFirmware::Kind(k)) if k == "bios" => Firmware::Bios,
        Some(RawFirmware::Kind(other)) => {
            return Err(format!(
                "`firmware` is \"uefi\", \"bios\" or {{ code, vars }}, not {other:?}"
            ))
        }
        Some(RawFirmware::Uefi { code, vars }) => Firmware::Uefi { code, vars },
    };
    let console = raw.console.unwrap_or_default();
    let prompt = console.prompt.ok_or_else(|| {
        format!(
            "machine `{name}` needs `console.prompt`, the shell prompt its serial console shows"
        )
    })?;
    let duration = |d: Option<RawDuration>, what: &str, default: u64| -> Result<Duration, String> {
        Ok(d.map(|d| d.parse())
            .transpose()
            .map_err(|e| format!("{what}: {e}"))?
            .unwrap_or(Duration::from_secs(default)))
    };
    let shares = raw
        .shares
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, s)| Share {
            tag: s.tag.unwrap_or_else(|| {
                if i == 0 {
                    "host".to_string()
                } else {
                    format!("host{i}")
                }
            }),
            dir: s.dir,
            optional: s.optional,
        })
        .collect();
    Ok(MachineSpec {
        qemu: raw.qemu.unwrap_or_else(|| "qemu-system-x86_64".into()),
        disks,
        media,
        memory: raw.memory.unwrap_or_else(|| "2G".into()),
        cpus: raw.cpus.unwrap_or(2).max(1),
        kvm,
        firmware,
        display: raw.display.unwrap_or(false),
        shares,
        net: raw.net.unwrap_or(true),
        extra_args: raw.extra_args.unwrap_or_default(),
        env: raw.env.unwrap_or_default(),
        console: Console {
            prompt,
            banner: console.banner,
            setup: console.setup,
            reboot: console.reboot.unwrap_or_else(|| "systemctl reboot".into()),
            poweroff: console
                .poweroff
                .unwrap_or_else(|| "systemctl poweroff".into()),
        },
        boot_timeout: duration(raw.boot_timeout, "boot_timeout", 180)?,
        command_timeout: duration(raw.command_timeout, "command_timeout", 120)?,
        source,
        name,
    })
}

/// A medium of a machine or of an `attach` step.
pub(super) fn media(raw: RawMedia, index: usize) -> Result<Media, String> {
    let kind = match raw.kind.as_deref() {
        None | Some("cd") => MediaKind::Cd,
        Some("usb") => MediaKind::Usb,
        Some(other) => {
            return Err(format!(
                "a medium is attached `as` \"cd\" or \"usb\", not {other:?}"
            ))
        }
    };
    Ok(Media {
        name: raw.name.unwrap_or_else(|| {
            if index == 0 {
                "medium".to_string()
            } else {
                format!("medium{index}")
            }
        }),
        file: raw.file,
        kind,
    })
}

/// Replace, in every string of `value`, what `f` replaces.
pub(super) fn map_strings(value: &mut toml::Value, f: &mut dyn FnMut(&mut String)) {
    match value {
        toml::Value::String(s) => f(s),
        toml::Value::Array(items) => items.iter_mut().for_each(|v| map_strings(v, f)),
        toml::Value::Table(t) => t.iter_mut().for_each(|(_, v)| map_strings(v, f)),
        _ => {}
    }
}
