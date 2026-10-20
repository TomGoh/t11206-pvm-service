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
//!   "memory_mib": 256,
//!   "cpus": 2,
//!   "protection": "without_firmware",
//!   "disks": [
//!     { "path": "/system_ext/etc/xkernel_vm/dummy.img", "writable": false },
//!     { "path": "/data/misc/xkernel_vm/rootfs.img", "writable": true,
//!       "template": "/system_ext/etc/xkernel_vm/rootfs.img" }
//!   ],
//!   "state_dir": "/data/misc/xkernel_vm"
//! }
//! ```
//!
//! A writable disk that is missing is made right before a start: copied from `template` (the
//! product ships the guest's root file system read-only in system_ext; the first start of a new
//! device copies it to `/data`, sparsely, through a temporary file renamed into place), or
//! created empty and sparse with `create_size_mib`. Without either it must already exist.
//!
//! `protection` says how the instance is protected, so the UI can show the mode; through AVF it
//! also names the hypervisor property the preconditions need. AVF itself decides: an instance
//! with `"without_firmware"` only runs that way if the product declares it in
//! `/system_ext/etc/avf/protected_vms_without_firmware*.xml` for this service's executable.
//!
//! `backend` says who runs crosvm: `"direct"` runs a crosvm binary itself and needs no change to
//! AVF, which makes it the preferred one and the product config's choice (`etc/xkernel_vm.json`);
//! `"avf"` (`etc/xkernel_vm.avf.json`, and the value when `backend` is left out) creates the VM
//! through AVF, which needs the Virtualization module's support for declared firmware-less VMs.
//! The direct backend needs a `direct` section:
//!
//! ```json
//! "backend": "direct",
//! "direct": { "crosvm": "/apex/com.android.virt/bin/crosvm", "hypervisor": "xhyper", "cid": 1024 }
//! ```
//!
//! It does not use AVF's admission, so it checks neither virtmgr nor the AVF property; whether a
//! protected VM may run is then up to the hypervisor alone.
//!
//! `readiness` says how the service learns the guest is ready (state `Ready`): the product
//! config waits for the guest's shell prompt on the serial console (`console_marker`), which
//! needs nothing from the guest; a vsock port (`vsock_port`) needs a listener in the guest and,
//! under SELinux, AVF. Without it the instance stays `RunningNotReady`.
//!
//! The preconditions only read files and properties, so evaluating them never changes anything.

use crate::controller::Probe;
use anyhow::{bail, ensure, Context, Result};
use log::info;
use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
    /// The crosvm binary to run: a build with the hypervisor's backend. On T11206 that is the
    /// platform's own crosvm (`/apex/com.android.virt/bin/crosvm`), built with its `xhyper`
    /// feature.
    pub crosvm: PathBuf,
    /// crosvm's `--hypervisor`, the backend to run the VM on (`xhyper`). Without it crosvm picks
    /// one itself: KVM when /dev/kvm exists, otherwise XHyper when /dev/xhyper exists.
    #[serde(default)]
    pub hypervisor: Option<String>,
    /// `xkernel_crosvm_launcher`, which starts crosvm so that it dies with this service even
    /// though it runs in another SELinux domain (see `launcher/main.rs`). Without one crosvm is
    /// started directly; then only a start without a domain change keeps that guarantee.
    #[serde(default)]
    pub launcher: Option<PathBuf>,
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
    /// Copy this image when the disk is missing (writable disks only; not with
    /// `create_size_mib`).
    #[serde(default)]
    pub template: Option<PathBuf>,
}

/// The block size of the sparse copy: a block of zeros is skipped instead of written.
const COPY_BLOCK: usize = 1 << 20;

impl DiskConfig {
    /// Creates the image when it is missing and may be created; the backends call this right
    /// before a start (it is the one thing here that writes).
    pub fn create_if_missing(&self) -> Result<()> {
        if self.path.exists() {
            return Ok(());
        }
        if let Some(template) = &self.template {
            return self.copy_from(template);
        }
        let Some(size_mib) = self.create_size_mib else { return Ok(()) };
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

    /// Copies `template` to the disk's path, skipping blocks of zeros (the copy stays sparse),
    /// into a temporary file that is renamed into place once complete and synced: a copy cut
    /// short leaves no half image behind.
    fn copy_from(&self, template: &Path) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};
        let started = std::time::Instant::now();
        let mut src = fs::File::open(template)
            .with_context(|| format!("Can't open {}", template.display()))?;
        let tmp = self.path.with_extension("tmp");
        let _ = fs::remove_file(&tmp);
        let mut dst = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o660)
            .open(&tmp)
            .with_context(|| format!("Can't create {}", tmp.display()))?;
        // Not left to the umask: crosvm (same uid) and the system group read and write it.
        dst.set_permissions(fs::Permissions::from_mode(0o660))?;
        let mut block = vec![0u8; COPY_BLOCK];
        let (mut len, mut written) = (0u64, 0u64);
        loop {
            let mut n = 0;
            while n < block.len() {
                match src.read(&mut block[n..])? {
                    0 => break,
                    k => n += k,
                }
            }
            if n == 0 {
                break;
            }
            if block[..n].iter().any(|b| *b != 0) {
                dst.seek(SeekFrom::Start(len))?;
                dst.write_all(&block[..n])?;
                written += n as u64;
            }
            len += n as u64;
        }
        dst.set_len(len)?;
        dst.sync_all()?;
        drop(dst);
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("Can't move {} into place", tmp.display()))?;
        info!(
            "copied {} to {} ({} MiB, {} MiB written) in {:.1}s",
            template.display(),
            self.path.display(),
            len >> 20,
            written >> 20,
            started.elapsed().as_secs_f32()
        );
        Ok(())
    }
}

/// How the service learns that the guest is ready: exactly one of `vsock_port` and
/// `console_marker`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    /// The guest is ready once a vsock connection to this port succeeds (it listens there).
    /// Under SELinux only AVF's managers may connect over vsock, so this works through AVF only.
    #[serde(default)]
    pub vsock_port: Option<u32>,
    /// The guest is ready once its serial output contains this text, e.g. its shell prompt. The
    /// service keeps the console, so this asks nothing of the guest but what it prints anyway.
    #[serde(default)]
    pub console_marker: Option<String>,
    #[serde(default = "default_ready_timeout")]
    pub timeout_seconds: u64,
}

impl Readiness {
    pub fn probe(&self) -> Probe {
        match (&self.vsock_port, &self.console_marker) {
            (Some(port), _) => Probe::Vsock(*port),
            (None, Some(marker)) => Probe::ConsoleMarker(marker.as_bytes().to_vec()),
            // Refused by validate().
            (None, None) => Probe::ConsoleMarker(Vec::new()),
        }
    }
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
    /// The hypervisor driver's device node; its absence makes the instance unavailable, unless
    /// `driver` says to load the driver on a start.
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
    /// Load the hypervisor's host driver on a start when its device node is missing (see
    /// `driver.rs`); without this a missing node makes the instance unavailable.
    #[serde(default)]
    pub driver: Option<DriverConfig>,
}

/// Loading the hypervisor driver on demand.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DriverConfig {
    /// How long a start waits for the loader to report.
    #[serde(default = "default_driver_timeout")]
    pub timeout_seconds: u64,
}

fn default_driver_timeout() -> u64 {
    20
}

fn default_hypervisor_device() -> PathBuf {
    PathBuf::from("/dev/xhyper")
}

/// Whether `path` exists, asked with `access(F_OK)` instead of `stat`: platform policy forbids
/// this service, a core domain, even `getattr` on a VM manager device such as /dev/xhyper (only
/// crosvm may touch one), and the kernel asks SELinux nothing for an `F_OK` check.
pub fn exists_without_stat(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::F_OK) == 0 }
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
            if (disk.create_size_mib.is_some() || disk.template.is_some()) && !disk.writable {
                bail!("disk {}: only a writable disk can be created", disk.path.display());
            }
            if let Some(template) = &disk.template {
                ensure!(
                    disk.create_size_mib.is_none(),
                    "disk {}: template or create_size_mib",
                    disk.path.display()
                );
                ensure!(template.is_absolute(), "template {} must be absolute", template.display());
            }
        }
        if let Some(device) = &self.console_input_device {
            ensure!(device == "hvc0" || device == "ttyS0", "console_input_device {device:?}");
        }
        if let Some(readiness) = &self.readiness {
            match (readiness.vsock_port, &readiness.console_marker) {
                (Some(port), None) => ensure!(port > 0, "readiness.vsock_port must be positive"),
                (None, Some(marker)) => {
                    ensure!(!marker.is_empty(), "readiness.console_marker must not be empty")
                }
                _ => bail!("readiness needs exactly one of vsock_port and console_marker"),
            }
            ensure!(readiness.timeout_seconds > 0, "readiness.timeout_seconds must be positive");
        }
        ensure!(self.stop_timeout_seconds > 0, "stop_timeout_seconds must be positive");
        if let Some(driver) = &self.driver {
            ensure!(driver.timeout_seconds > 0, "driver.timeout_seconds must be positive");
        }
        ensure!(self.reclaim_window_seconds > 0, "reclaim_window_seconds must be positive");
        match (self.backend, &self.direct) {
            (BackendKind::Avf, None) => {}
            (BackendKind::Avf, Some(_)) => bail!("a direct section needs \"backend\": \"direct\""),
            (BackendKind::Direct, None) => bail!("\"backend\": \"direct\" needs a direct section"),
            (BackendKind::Direct, Some(direct)) => {
                ensure!(direct.crosvm.is_absolute(), "direct.crosvm must be an absolute path");
                if let Some(launcher) = &direct.launcher {
                    ensure!(launcher.is_absolute(), "direct.launcher must be an absolute path");
                }
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
                    ensure!(
                        self.readiness.as_ref().is_none_or(|r| r.vsock_port.is_none()),
                        "a vsock readiness handshake needs direct.cid"
                    );
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
        if self.driver.is_none() && !exists_without_stat(&self.hypervisor_device) {
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
                for program in std::iter::once(&direct.crosvm).chain(&direct.launcher) {
                    let executable = fs::metadata(program)
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
                    if !executable {
                        return Err(format!("{} is not executable", program.display()));
                    }
                }
            }
        }
        if !self.kernel.is_file() {
            return Err(format!("kernel image missing: {}", self.kernel.display()));
        }
        for disk in &self.disks {
            if disk.path.is_file() || disk.create_size_mib.is_some() {
                continue;
            }
            match &disk.template {
                Some(template) if template.is_file() => {}
                Some(template) => {
                    return Err(format!(
                        "disk image missing: {} (and its template {})",
                        disk.path.display(),
                        template.display()
                    ))
                }
                None => return Err(format!("disk image missing: {}", disk.path.display())),
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
    fn existence_is_checked_without_stat() {
        let dir = std::env::temp_dir().join(format!("xkvm-exists-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("xhyper");
        assert!(!exists_without_stat(&file));
        fs::write(&file, b"").unwrap();
        assert!(exists_without_stat(&file));
        assert!(!exists_without_stat(Path::new("/dev/null\0x")), "interior NUL");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_shipped_config_is_valid() {
        let c = VmConfig::parse(include_str!("../etc/xkernel_vm.json")).unwrap();
        assert_eq!(c.name, "xkernel_vm");
        assert_eq!(c.protection, Protection::WithoutFirmware);
        assert_eq!(c.crosvm_process_name(), "crosvm_xkernel_vm");
        assert_eq!(c.backend, BackendKind::Direct);
        let d = c.direct.as_ref().unwrap();
        assert!(d.cid.is_some_and(|cid| cid < AVF_FIRST_CID));
        assert!(d.kill_after_seconds < c.stop_timeout_seconds);
    }

    #[test]
    fn the_shipped_avf_config_is_the_same_instance() {
        let c = VmConfig::parse(include_str!("../etc/xkernel_vm.avf.json")).unwrap();
        assert_eq!(c.backend, BackendKind::Avf);
        let direct = VmConfig::parse(include_str!("../etc/xkernel_vm.json")).unwrap();
        assert_eq!(
            (c.name, c.memory_mib, c.cpus, c.disks),
            (direct.name, direct.memory_mib, direct.cpus, direct.disks)
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
        assert!(check().unwrap_err().contains("crosvm is not executable"));
        fs::write(dir.join("crosvm"), b"#!/bin/sh\n").unwrap();
        assert!(check().unwrap_err().contains("not executable"));
        fs::set_permissions(dir.join("crosvm"), fs::Permissions::from_mode(0o755)).unwrap();
        // Neither virtmgr nor the AVF property is needed.
        assert_eq!(check(), Ok(()));

        // A configured launcher must be there too.
        let launched = VmConfig::parse(&json.replace(
            r#"/crosvm" }"#,
            &format!(r#"/crosvm", "launcher": "{}/launcher" }}"#, dir.display()),
        ))
        .unwrap();
        let check = || launched.preflight(&dir.join("no-virtmgr"), &no_props);
        assert!(check().unwrap_err().contains("launcher is not executable"));
        fs::write(dir.join("launcher"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("launcher"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(check(), Ok(()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_driver_is_loaded_on_start_only_when_configured() {
        let dir = std::env::temp_dir().join(format!("xkvm-config-driver-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for f in ["kernel.bin", "dummy.img", "crosvm"] {
            fs::write(dir.join(f), b"x").unwrap();
        }
        fs::set_permissions(dir.join("crosvm"), fs::Permissions::from_mode(0o755)).unwrap();
        let json = example(&dir).replace(
            r#""state_dir""#,
            &format!(
                r#""backend": "direct", "direct": {{ "crosvm": "{}/crosvm" }}, "state_dir""#,
                dir.display()
            ),
        );
        let no_props = props(&[]);
        let without = VmConfig::parse(&json).unwrap();
        let err = without.preflight(&dir.join("no-virtmgr"), &no_props).unwrap_err();
        assert!(err.contains("hypervisor driver missing"), "{err}");
        let with = VmConfig::parse(
            &json.replace(r#""state_dir""#, r#""driver": { "timeout_seconds": 5 }, "state_dir""#),
        )
        .unwrap();
        assert_eq!(with.driver, Some(DriverConfig { timeout_seconds: 5 }));
        assert_eq!(with.preflight(&dir.join("no-virtmgr"), &no_props), Ok(()));
        assert!(VmConfig::parse(
            &json.replace(r#""state_dir""#, r#""driver": { "timeout_seconds": 0 }, "state_dir""#,)
        )
        .is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn readiness_by_console_marker_or_vsock() {
        let base = example(Path::new("/x"));
        let with =
            |readiness: &str| {
                VmConfig::parse(&base.replace(
                    r#""state_dir""#,
                    &format!(r#""readiness": {readiness}, "state_dir""#),
                ))
            };
        let c = with(r#"{ "console_marker": "kylin-x:~#" }"#).unwrap();
        assert_eq!(
            c.readiness.as_ref().unwrap().probe(),
            Probe::ConsoleMarker(b"kylin-x:~#".to_vec())
        );
        assert_eq!(c.readiness.unwrap().timeout_seconds, 60);
        let c = with(r#"{ "vsock_port": 5000, "timeout_seconds": 9 }"#).unwrap();
        assert_eq!(c.readiness.unwrap().probe(), Probe::Vsock(5000));
        for bad in [
            r#"{ "vsock_port": 5000, "console_marker": "$" }"#,
            r#"{ "timeout_seconds": 9 }"#,
            r#"{ "console_marker": "" }"#,
            r#"{ "vsock_port": 0 }"#,
            r#"{ "console_marker": "$", "timeout_seconds": 0 }"#,
        ] {
            assert!(with(bad).is_err(), "accepted {bad}");
        }
        // The direct backend without a CID has no vsock, but the console works.
        let direct = base.replace(
            r#""state_dir""#,
            r#""backend": "direct", "direct": { "crosvm": "/x/crosvm" }, "state_dir""#,
        );
        let direct_with =
            |readiness: &str| {
                VmConfig::parse(&direct.replace(
                    r#""state_dir""#,
                    &format!(r#""readiness": {readiness}, "state_dir""#),
                ))
            };
        assert!(direct_with(r#"{ "console_marker": "kylin-x:~#" }"#).is_ok());
        assert!(direct_with(r#"{ "vsock_port": 5000 }"#).is_err());
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
            (r#""writable": false }"#, r#""writable": false, "template": "/x/t.img" }"#),
            (r#""create_size_mib": 64 }"#, r#""create_size_mib": 64, "template": "/x/t.img" }"#),
            (r#""create_size_mib": 64 }"#, r#""template": "t.img" }"#),
            (r#""cpus": 1,"#, r#""cpus": 1, "surprise": 1,"#),
        ] {
            assert!(good.contains(from), "{from}");
            let bad = good.replace(from, to);
            assert!(VmConfig::parse(&bad).is_err(), "accepted {to}");
        }
    }

    #[test]
    fn a_missing_disk_is_copied_sparsely_from_its_template_once() {
        let dir = std::env::temp_dir().join(format!("xkvm-template-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 3 MiB + 5 bytes: data, a block of zeros, data, a short tail.
        let mut image = vec![0u8; 3 * COPY_BLOCK + 5];
        image[10] = 1;
        image[2 * COPY_BLOCK + 7] = 2;
        image[3 * COPY_BLOCK + 4] = 3;
        let template = dir.join("template.img");
        fs::write(&template, &image).unwrap();
        let disk = DiskConfig {
            path: dir.join("data/rootfs.img"),
            writable: true,
            create_size_mib: None,
            template: Some(template),
        };
        fs::create_dir_all(dir.join("data")).unwrap();
        fs::write(dir.join("data/rootfs.tmp"), b"left by a copy cut short").unwrap();
        disk.create_if_missing().unwrap();
        assert_eq!(fs::read(&disk.path).unwrap(), image);
        assert!(!dir.join("data/rootfs.tmp").exists());
        assert_eq!(fs::metadata(&disk.path).unwrap().permissions().mode() & 0o777, 0o660);
        // An existing disk is the guest's data: never copied over.
        fs::write(&disk.path, b"guest data").unwrap();
        disk.create_if_missing().unwrap();
        assert_eq!(fs::read(&disk.path).unwrap(), b"guest data");
        fs::remove_dir_all(&dir).unwrap();
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
