// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! `xkernel_vm_loader`: loads or unloads the host driver for the VM lifecycle.
//!
//! The existing init property trigger starts this oneshot on `xkernel_vm.load_driver=*`.
//! It captures the request before acting: `load:<pid>.<sequence>` loads and prepares nodes,
//! `unload:<pid>.<sequence>` removes modules in reverse `modules.load` order (see `loader.rs`).
//! Legacy unprefixed requests still load. Results echo that exact request as `loaded <request>`,
//! `unloaded <request>` or `failed <request>: <reason>` on `xkernel_vm.driver_status`.
//! Unload uses only the existing module capability, module-directory existence and data-node
//! attributes; it never inspects module reference counts or reads a global module listing.
//!
//! Before reporting, it also makes the driver's EL2 cycle counter file, `/proc/xcore_stats`,
//! 0660 root system, so that the control service (uid system, no capabilities) can start, read
//! and stop the counters for the monitoring interface. The driver creates it 0644 root root. The
//! monitor is optional: a driver without it, or a failure here, is logged and the load still
//! succeeds.
//!
//! Usage: `xkernel_vm_loader [DIR [DEVICE]]`, by default `/system_ext/etc/xkernel_vm/modules`
//! and `/dev/xhyper`.

mod loader;

use loader::{
    clip, load_all, operation, parse_list, unload_all, Kernel, Operation, LOAD_PROPERTY,
    STATUS_PROPERTY,
};
use log::{error, info, warn, LevelFilter};
use rustutils::system_properties;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const DEFAULT_DIR: &str = "/system_ext/etc/xkernel_vm/modules";
const DEFAULT_DEVICE: &str = "/dev/xhyper";
/// The driver's EL2 cycle counters, written and read by the control service.
const MONITOR_FILE: &str = "/proc/xcore_stats";
const AID_SYSTEM: libc::gid_t = 1000;
const NODE_TIMEOUT: Duration = Duration::from_secs(5);
/// A system property value holds at most 91 bytes.
const PROPERTY_VALUE_MAX: usize = 91;

struct System {
    device: PathBuf,
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

impl Kernel for System {
    fn is_loaded(&self, name: &str) -> bool {
        // access(F_OK) needs only search on the sysfs directories, which every domain has.
        let Ok(path) = c_path(&Path::new("/sys/module").join(name)) else { return false };
        // SAFETY: access only reads the NUL-terminated path.
        unsafe { libc::access(path.as_ptr(), libc::F_OK) == 0 }
    }

    fn load(&self, path: &Path, params: &str) -> io::Result<()> {
        let file = File::open(path)?;
        let params =
            CString::new(params).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: finit_module reads the open file and the NUL-terminated parameters only.
        let r =
            unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), params.as_ptr(), 0) };
        if r == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn unload(&self, name: &str) -> io::Result<()> {
        let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: delete_module reads the NUL-terminated name; O_NONBLOCK forbids waiting for references.
        let result =
            unsafe { libc::syscall(libc::SYS_delete_module, name.as_ptr(), libc::O_NONBLOCK) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn monitor_exists(&self) -> bool {
        // getattr is already allowed for the counter file; no file contents are opened.
        std::fs::symlink_metadata(MONITOR_FILE).is_ok()
    }

    fn node_exists(&self) -> bool {
        let Ok(path) = c_path(&self.device) else { return false };
        // SAFETY: access only reads the NUL-terminated path.
        unsafe { libc::access(path.as_ptr(), libc::F_OK) == 0 }
    }

    fn prepare_node(&self) -> io::Result<()> {
        let path = c_path(&self.device)?;
        // SAFETY: chown only reads the NUL-terminated path.
        if unsafe { libc::chown(path.as_ptr(), 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: chmod only reads the NUL-terminated path.
        if unsafe { libc::chmod(path.as_ptr(), 0o666) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Makes [`MONITOR_FILE`] 0660 root system; returns whether the driver offers it.
fn prepare_monitor() -> io::Result<bool> {
    let path = c_path(Path::new(MONITOR_FILE))?;
    // SAFETY: access only reads the NUL-terminated path.
    if unsafe { libc::access(path.as_ptr(), libc::F_OK) } != 0 {
        return Ok(false);
    }
    // SAFETY: chown only reads the NUL-terminated path.
    if unsafe { libc::chown(path.as_ptr(), 0, AID_SYSTEM) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: chmod only reads the NUL-terminated path.
    if unsafe { libc::chmod(path.as_ptr(), 0o660) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

fn run(dir: &Path, device: PathBuf, operation: Operation) -> Result<Vec<String>, String> {
    let list = dir.join("modules.load");
    let text = std::fs::read_to_string(&list).map_err(|e| format!("{}: {e}", list.display()))?;
    let modules = parse_list(&text)?;
    let kernel = System { device };
    if operation == Operation::Unload {
        return unload_all(&modules, &kernel);
    }
    let loaded = load_all(dir, &modules, &kernel, NODE_TIMEOUT)?;
    match prepare_monitor() {
        Ok(true) => info!("{MONITOR_FILE} made 0660 root system"),
        Ok(false) => info!("the driver offers no {MONITOR_FILE}"),
        Err(e) => warn!("can't give {MONITOR_FILE} to the control service: {e}"),
    }
    Ok(loaded)
}

fn main() -> ExitCode {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("xkernel_vm_loader")
            .with_max_level(LevelFilter::Info),
    );
    let mut args = std::env::args_os().skip(1);
    let dir = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
    let device = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_DEVICE));
    let request = system_properties::read(LOAD_PROPERTY).ok().flatten().unwrap_or_default();
    let op = operation(&request);
    let result = op.as_ref().map_err(Clone::clone).and_then(|op| run(&dir, device, *op));
    let (status, code) = match &result {
        Ok(loaded) => {
            let verb = if op == Ok(Operation::Unload) { "unloaded" } else { "loaded" };
            info!("hypervisor driver {verb} ({loaded:?} from {})", dir.display());
            (format!("{verb} {request}"), ExitCode::SUCCESS)
        }
        Err(reason) => {
            error!("hypervisor driver operation failed: {reason}");
            (format!("failed {request}: {reason}"), ExitCode::FAILURE)
        }
    };
    if let Err(e) = system_properties::write(STATUS_PROPERTY, clip(&status, PROPERTY_VALUE_MAX)) {
        error!("can't write {STATUS_PROPERTY}: {e:#}");
        return ExitCode::FAILURE;
    }
    code
}
