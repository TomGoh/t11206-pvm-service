// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! `xkernel_vm_loader`: loads the hypervisor's host driver when the control service asks.
//!
//! init starts it (a oneshot service of the init script) whenever the service writes a request
//! number to `xkernel_vm.load_driver`, i.e. on the first start of the VM after boot. It loads the
//! modules of its directory in the order of `modules.load` (see `loader.rs`), waits for
//! `/dev/xhyper`, makes it 0666 root root like AOSP's `/dev/kvm` (SELinux lets only the crosvm
//! domain open it), and writes `loaded <request>` or `failed <request>: <reason>` to
//! `xkernel_vm.driver_status`, `<request>` being the newest value of the request property.
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

use loader::{clip, load_all, parse_list, Kernel, LOAD_PROPERTY, STATUS_PROPERTY};
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

fn run(dir: &Path, device: PathBuf) -> Result<Vec<String>, String> {
    let list = dir.join("modules.load");
    let text = std::fs::read_to_string(&list).map_err(|e| format!("{}: {e}", list.display()))?;
    let loaded = load_all(dir, &parse_list(&text)?, &System { device }, NODE_TIMEOUT)?;
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
    let result = run(&dir, device);
    // Answer the newest request: one written while this ran is served by this load too.
    let request = system_properties::read(LOAD_PROPERTY).ok().flatten().unwrap_or_default();
    let (status, code) = match &result {
        Ok(loaded) => {
            info!("hypervisor driver ready (loaded {loaded:?} from {})", dir.display());
            (format!("loaded {request}"), ExitCode::SUCCESS)
        }
        Err(reason) => {
            error!("hypervisor driver not loaded: {reason}");
            (format!("failed {request}: {reason}"), ExitCode::FAILURE)
        }
    };
    if let Err(e) = system_properties::write(STATUS_PROPERTY, clip(&status, PROPERTY_VALUE_MAX)) {
        error!("can't write {STATUS_PROPERTY}: {e:#}");
        return ExitCode::FAILURE;
    }
    code
}
