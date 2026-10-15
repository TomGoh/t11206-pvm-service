// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The instance configuration and the start preconditions.
//!
//! The configuration is a fixed product file (`/system_ext/etc/xkernel_vm/xkernel_vm.json` on a
//! product build); no client chooses memory, CPUs, images or the protection mode. Example:
//!
//! ```json
//! {
//!   "name": "xkernel_vm",
//!   "kernel": "/system_ext/etc/xkernel_vm/kernel.bin",
//!   "params": "panic=-1 console=ttyS0 loglevel=8",
//!   "memory_mib": 128,
//!   "cpus": 1,
//!   "protection": "without_firmware",
//!   "disks": [
//!     { "path": "/system_ext/etc/xkernel_vm/dummy.img", "writable": false },
//!     { "path": "/data/misc/xkernel_vm/rootfs.img", "writable": true }
//!   ],
//!   "state_dir": "/data/misc/xkernel_vm"
//! }
//! ```
//!
//! `protection` says which AVF admission the instance relies on, so the preconditions can name
//! the hypervisor property it needs and the UI can show the mode. AVF itself decides: an instance
//! with `"without_firmware"` only runs that way if the product declares it in
//! `/system_ext/etc/avf/protected_vms_without_firmware*.xml` for this service's executable.
//!
//! `backend` says who runs crosvm: `"avf"` (the default) creates the VM through AVF, which needs
//! the Virtualization module's support for declared firmware-less VMs; `"direct"` runs a crosvm
//! binary itself, for products that may not change AVF (see `direct`). The direct backend needs a
//! `direct` section:
//!
//! ```json
//! "backend": "direct",
//! "direct": { "crosvm": "/system_ext/bin/xkernel_crosvm", "cid": 1024 }
//! ```
//!
//! It does not use AVF's admission, so it checks neither virtmgr nor the AVF property; whether a
//! protected VM may run is then up to the hypervisor alone.
//!
//! The preconditions only read files and properties, so evaluating them never changes anything.

use anyhow::{bail, ensure, Context, Result};
use log::info;
use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Where the virtualization manager AVF spawns for each client lives.
pub const VIRTMGR_PATH: &str = "/apex/com.android.virt/bin/virtmgr";

/// The boot properties behind each protection mode.
pub const PROP_VM: &str = "ro.boot.hypervisor.vm.supported";
pub const PROP_PROTECTED_VM: &str = "ro.boot.hypervisor.protected_vm.supported";
pub const PROP_PROTECTED_VM_WITHOUT_FIRMWARE: &str =
    "ro.boot.hypervisor.protected_vm_without_firmware.supported";

/// The AVF admission an instance relies on.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Protection {
    /// A non-protected VM.
    None,
    /// A protected VM booted through pvmfw.
    WithFirmware,
    /// A protected VM the product declares to run without guest firmware.
    WithoutFirmware,
}

impl Protection {
    /// The boot property AVF requires for this mode.
    pub fn property(self) -> &'static str {
        match self {
            Protection::None => PROP_VM,
            Protection::WithFirmware => PROP_PROTECTED_VM,
            Protection::WithoutFirmware => PROP_PROTECTED_VM_WITHOUT_FIRMWARE,
        }
    }

    pub fn is_protected(self) -> bool {
        self != Protection::None
    }
}

/// Who runs the VM's crosvm.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// AVF creates and runs the VM.
    #[default]
    Avf,
    /// This service runs crosvm itself.
    Direct,
}

/// The settings of the direct backend.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectConfig {
    /// The crosvm binary to run: a build with the hypervisor's backend.
    pub crosvm: PathBuf,
    /// The vsock CID. It must stay below [`AVF_FIRST_CID`] so it never collides with a VM AVF
    /// runs. Without one the VM has no vsock device and no readiness handshake.
    #[serde(default)]
    pub cid: Option<u32>,
    /// crosvm's `--log-level`.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// How long a requested stop waits for crosvm to exit before killing it; less than the stop
    /// deadline, so a killed VM is still a normal stop.
    #[serde(default = "default_kill_after")]
    pub kill_after_seconds: u64,
}

/// The first CID AVF hands out; the direct backend's CID stays below it.
pub const AVF_FIRST_CID: u32 = 2048;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiskConfig {
    pub path: PathBuf,
    pub writable: bool,
    /// Create the image as a sparse file of this size when it is missing (writable disks only).
    #[serde(default)]
    pub create_size_mib: Option<u64>,
}

impl DiskConfig {
    /// Creates the image when it is missing and may be created; the backends call this right
    /// before a start (it is the one thing here that writes).
    pub fn create_if_missing(&self) -> Result<()> {
        let Some(size_mib) = self.create_size_mib else { return Ok(()) };
        if self.path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Can't create {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&self.path)
            .with_context(|| format!("Can't create {}", self.path.display()))?;
        file.set_len(size_mib * 1024 * 1024)?;
        info!("created sparse {} ({size_mib} MiB)", self.path.display());
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    /// The guest is ready once a vsock connection to this port succeeds.
    pub vsock_port: u32,
    #[serde(default = "default_ready_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    /// The VM name passed to AVF (`crosvm_<name>` becomes the crosvm process name).
    pub name: String,
    pub kernel: PathBuf,
    #[serde(default)]
    pub params: Option<String>,
    pub memory_mib: u32,
    pub cpus: u32,
    pub protection: Protection,
    #[serde(default)]
    pub disks: Vec<DiskConfig>,
    /// Where the service keeps the instance ID and the record of running processes.
    pub state_dir: PathBuf,
    /// The hypervisor driver's device node; its absence makes the instance unavailable.
    #[serde(default = "default_hypervisor_device")]
    pub hypervisor_device: PathBuf,
    /// Serial device for console input, `"hvc0"` (AVF's default) or `"ttyS0"`.
    #[serde(default)]
    pub console_input_device: Option<String>,
    /// The readiness handshake; without one the instance never reports `Ready`.
    #[serde(default)]
    pub readiness: Option<Readiness>,
    /// How long a stop waits for AVF to report the VM dead.
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout_seconds: u64,
    /// How long the reclaim check waits for the VM's processes to exit after AVF's death report.
    #[serde(default = "default_reclaim_window")]
    pub reclaim_window_seconds: u64,
    #[serde(default)]
    pub backend: BackendKind,
    /// Required with `"backend": "direct"`, refused otherwise.
    #[serde(default)]
    pub direct: Option<DirectConfig>,
}

fn default_hypervisor_device() -> PathBuf {
    PathBuf::from("/dev/xhyper")
}
fn default_ready_timeout() -> u64 {
    60
}
fn default_stop_timeout() -> u64 {
    30
}
fn default_reclaim_window() -> u64 {
    10
}
fn default_log_level() -> String {
    "info".to_owned()
}
fn default_kill_after() -> u64 {
    10
}

/// The largest vCPU count accepted; the hypervisor's own limit applies below it.
const MAX_CPUS: u32 = 8;

/// AVF's rule for VM names (`is_valid_vm_name` in virtmgr): letters, digits, `_` and `-`.
fn is_valid_vm_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl VmConfig {
    pub fn parse(json: &str) -> Result<Self> {
        let config: VmConfig = serde_json::from_str(json).context("Invalid instance config")?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let json =
            fs::read_to_string(path).with_context(|| format!("Can't read {}", path.display()))?;
        Self::parse(&json).with_context(|| format!("In {}", path.display()))
    }

    fn validate(&self) -> Result<()> {
        ensure!(is_valid_vm_name(&self.name), "Invalid VM name {:?}", self.name);
        ensure!(self.memory_mib > 0, "memory_mib must be positive");
        ensure!((1..=MAX_CPUS).contains(&self.cpus), "cpus must be 1..={MAX_CPUS}");
        ensure!(self.kernel.is_absolute(), "kernel must be an absolute path");
        ensure!(self.state_dir.is_absolute(), "state_dir must be an absolute path");
        ensure!(self.hypervisor_device.is_absolute(), "hypervisor_device must be absolute");
        for disk in &self.disks {
            ensure!(disk.path.is_absolute(), "disk {} must be absolute", disk.path.display());
            if disk.create_size_mib.is_some() && !disk.writable {
                bail!("disk {}: only a writable disk can be created", disk.path.display());
            }
        }
        if let Some(device) = &self.console_input_device {
            ensure!(device == "hvc0" || device == "ttyS0", "console_input_device {device:?}");
        }
        if let Some(readiness) = &self.readiness {
            ensure!(readiness.vsock_port > 0, "readiness.vsock_port must be positive");
            ensure!(readiness.timeout_seconds > 0, "readiness.timeout_seconds must be positive");
        }
        ensure!(self.stop_timeout_seconds > 0, "stop_timeout_seconds must be positive");
        ensure!(self.reclaim_window_seconds > 0, "reclaim_window_seconds must be positive");
        match (self.backend, &self.direct) {
            (BackendKind::Avf, None) => {}
            (BackendKind::Avf, Some(_)) => bail!("a direct section needs \"backend\": \"direct\""),
            (BackendKind::Direct, None) => bail!("\"backend\": \"direct\" needs a direct section"),
            (BackendKind::Direct, Some(direct)) => {
                ensure!(direct.crosvm.is_absolute(), "direct.crosvm must be an absolute path");
                ensure!(
                    self.protection != Protection::WithFirmware,
                    "the direct backend runs no guest firmware"
                );
                if let Some(cid) = direct.cid {
                    ensure!(
                        (3..AVF_FIRST_CID).contains(&cid),
                        "direct.cid must be 3..{AVF_FIRST_CID}"
                    );
                } else {
                    ensure!(self.readiness.is_none(), "readiness needs direct.cid (vsock)");
                }
                ensure!(
                    (1..self.stop_timeout_seconds).contains(&direct.kill_after_seconds),
                    "direct.kill_after_seconds must be 1..stop_timeout_seconds"
                );
            }
        }
        Ok(())
    }

    /// The argv[0] AVF gives the VM's crosvm process; the direct backend uses the same.
    pub fn crosvm_process_name(&self) -> String {
        format!("crosvm_{}", self.name)
    }

    /// Checks every start precondition and returns the first that fails, in the words the UI
    /// shows. `virtmgr` is [`VIRTMGR_PATH`] on a device; `property` reads a system property.
    pub fn preflight(
        &self,
        virtmgr: &Path,
        property: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), String> {
        if !self.hypervisor_device.exists() {
            return Err(format!(
                "hypervisor driver missing: {} does not exist",
                self.hypervisor_device.display()
            ));
        }
        match &self.direct {
            None => {
                if !virtmgr.exists() {
                    return Err(format!("AVF missing: {} does not exist", virtmgr.display()));
                }
                let required = self.protection.property();
                if !matches!(property(required).as_deref(), Some("1") | Some("true")) {
                    return Err(format!("hypervisor does not report {required}"));
                }
            }
            Some(direct) => {
                let executable = fs::metadata(&direct.crosvm)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
                if !executable {
                    return Err(format!(
                        "crosvm missing: {} is not executable",
                        direct.crosvm.display()
                    ));
                }
            }
        }
        if !self.kernel.is_file() {
            return Err(format!("kernel image missing: {}", self.kernel.display()));
        }
        for disk in &self.disks {
            if !disk.path.is_file() && disk.create_size_mib.is_none() {
                return Err(format!("disk image missing: {}", disk.path.display()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn example(dir: &Path) -> String {
        format!(
            r#"{{
              "name": "xkernel_vm",
              "kernel": "{d}/kernel.bin",
              "params": "panic=-1 console=ttyS0 loglevel=8",
              "memory_mib": 128,
              "cpus": 1,
              "protection": "without_firmware",
              "disks": [
                {{ "path": "{d}/dummy.img", "writable": false }},
                {{ "path": "{d}/rootfs.img", "writable": true, "create_size_mib": 64 }}
              ],
              "state_dir": "{d}/state",
              "hypervisor_device": "{d}/xhyper"
            }}"#,
            d = dir.display()
        )
    }

    fn props(entries: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn parses_the_example_with_defaults() {
        let c = VmConfig::parse(&example(Path::new("/x"))).unwrap();
        assert_eq!(c.protection, Protection::WithoutFirmware);
        assert_eq!((c.stop_timeout_seconds, c.reclaim_window_seconds), (30, 10));
        assert_eq!(c.readiness, None);
        assert_eq!(c.crosvm_process_name(), "crosvm_xkernel_vm");
        assert_eq!(c.protection.property(), PROP_PROTECTED_VM_WITHOUT_FIRMWARE);
    }

    #[test]
    fn the_shipped_config_is_valid() {
        let c = VmConfig::parse(include_str!("../etc/xkernel_vm.json")).unwrap();
        assert_eq!(c.name, "xkernel_vm");
        assert_eq!(c.protection, Protection::WithoutFirmware);
        assert_eq!(c.crosvm_process_name(), "crosvm_xkernel_vm");
    }

    #[test]
    fn the_shipped_direct_config_is_valid() {
        let c = VmConfig::parse(include_str!("../etc/xkernel_vm.direct.json")).unwrap();
        assert_eq!(c.backend, BackendKind::Direct);
        let d = c.direct.as_ref().unwrap();
        assert!(d.cid.is_some_and(|cid| cid < AVF_FIRST_CID));
        assert!(d.kill_after_seconds < c.stop_timeout_seconds);
        // The same instance as the AVF config, only run differently.
        let avf = VmConfig::parse(include_str!("../etc/xkernel_vm.json")).unwrap();
        assert_eq!(
            (c.name, c.memory_mib, c.cpus, c.disks),
            (avf.name, avf.memory_mib, avf.cpus, avf.disks)
        );
    }

    #[test]
    fn rejects_invalid_direct_configs() {
        let avf = example(Path::new("/x"));
        let direct = avf.replace(
            r#""state_dir""#,
            r#""backend": "direct", "direct": { "crosvm": "/x/crosvm", "cid": 1024 }, "state_dir""#,
        );
        assert_eq!(VmConfig::parse(&direct).unwrap().backend, BackendKind::Direct);
        for (from, to) in [
            (r#""backend": "direct", "#, ""),
            (
                r#""backend": "direct", "direct": { "crosvm": "/x/crosvm", "cid": 1024 }, "#,
                r#""backend": "direct", "#,
            ),
            (r#""crosvm": "/x/crosvm""#, r#""crosvm": "crosvm""#),
            (r#""cid": 1024"#, r#""cid": 2048"#),
            (r#""cid": 1024"#, r#""cid": 2"#),
            (r#""cid": 1024"#, r#""cid": 1024, "kill_after_seconds": 30"#),
            (r#""protection": "without_firmware""#, r#""protection": "with_firmware""#),
            (r#", "cid": 1024 }"#, r#" }, "readiness": { "vsock_port": 5000 }"#),
        ] {
            assert!(direct.contains(from), "{from}");
            assert!(VmConfig::parse(&direct.replace(from, to)).is_err(), "accepted {to}");
        }
    }

    #[test]
    fn direct_preflight_needs_crosvm_not_avf() {
        let dir = std::env::temp_dir().join(format!("xkvm-config-direct-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let json = example(&dir).replace(
            r#""state_dir""#,
            &format!(
                r#""backend": "direct", "direct": {{ "crosvm": "{}/crosvm" }}, "state_dir""#,
                dir.display()
            ),
        );
        let c = VmConfig::parse(&json).unwrap();
        for f in ["xhyper", "kernel.bin", "dummy.img"] {
            fs::write(dir.join(f), b"x").unwrap();
        }
        let no_props = props(&[]);
        let check = || c.preflight(&dir.join("no-virtmgr"), &no_props);
        assert!(check().unwrap_err().contains("crosvm missing"));
        fs::write(dir.join("crosvm"), b"#!/bin/sh\n").unwrap();
        assert!(check().unwrap_err().contains("not executable"));
        fs::set_permissions(dir.join("crosvm"), fs::Permissions::from_mode(0o755)).unwrap();
        // Neither virtmgr nor the AVF property is needed.
        assert_eq!(check(), Ok(()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_invalid_configs() {
        let good = example(Path::new("/x"));
        for (from, to) in [
            (r#""name": "xkernel_vm""#, r#""name": "x kernel""#),
            (r#""memory_mib": 128"#, r#""memory_mib": 0"#),
            (r#""cpus": 1"#, r#""cpus": 9"#),
            (r#""kernel": "/x/kernel.bin""#, r#""kernel": "kernel.bin""#),
            (r#""protection": "without_firmware""#, r#""protection": "maybe""#),
            (r#""writable": false }"#, r#""writable": false, "create_size_mib": 1 }"#),
            (r#""cpus": 1,"#, r#""cpus": 1, "surprise": 1,"#),
        ] {
            assert!(good.contains(from), "{from}");
            let bad = good.replace(from, to);
            assert!(VmConfig::parse(&bad).is_err(), "accepted {to}");
        }
    }

    #[test]
    fn preflight_names_the_first_missing_piece() {
        let dir = std::env::temp_dir().join(format!("xkvm-config-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let c = VmConfig::parse(&example(&dir)).unwrap();
        let ok_props = props(&[(PROP_PROTECTED_VM_WITHOUT_FIRMWARE, "1")]);

        let virtmgr = dir.join("virtmgr");
        let check = |p: &dyn Fn(&str) -> Option<String>| c.preflight(&virtmgr, p);

        assert!(check(&ok_props).unwrap_err().contains("hypervisor driver"));
        fs::write(dir.join("xhyper"), b"").unwrap();
        assert!(check(&ok_props).unwrap_err().contains("AVF"));
        fs::write(&virtmgr, b"").unwrap();
        // The standard protected property does not stand in for the firmware-less one.
        let r = check(&props(&[(PROP_PROTECTED_VM, "1")]));
        assert!(r.unwrap_err().contains(PROP_PROTECTED_VM_WITHOUT_FIRMWARE));
        assert!(check(&props(&[(PROP_PROTECTED_VM_WITHOUT_FIRMWARE, "0")])).is_err());
        assert!(check(&ok_props).unwrap_err().contains("kernel"));
        fs::write(dir.join("kernel.bin"), b"k").unwrap();
        assert!(check(&ok_props).unwrap_err().contains("dummy.img"));
        fs::write(dir.join("dummy.img"), b"d").unwrap();
        // rootfs.img is missing but may be created.
        assert_eq!(check(&ok_props), Ok(()));
        assert_eq!(check(&props(&[(PROP_PROTECTED_VM_WITHOUT_FIRMWARE, "true")])), Ok(()));
        fs::remove_dir_all(&dir).unwrap();
    }
}
