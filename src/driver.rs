// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The VM owns its on-demand host driver; monitoring never asks this loader for anything.
//!
//! The existing init trigger starts `xkernel_vm_loader` on any value written to [`LOAD_PROPERTY`].
//! Requests are `load:<pid>.<sequence>` or `unload:<pid>.<sequence>`. The loader captures the
//! operation and request before acting, then writes `loaded <request>`, `unloaded <request>` or
//! `failed <request>: <reason>` to [`STATUS_PROPERTY`]. The full request, including its operation,
//! is echoed and must match exactly. Legacy numeric requests still mean load in the loader.
//!
//! One mutex serializes operations. Before writing, the service waits for init's oneshot loader
//! to leave running/restarting, so a result published just before process exit cannot lose the
//! next trigger. The whole exchange has the configured deadline. A VM start loads missing
//! modules on every request (a remaining device may belong to a partial driver); confirmed instance reclaim stops owned EL2 counters then requests unload. Startup
//! with no leftover instance also unloads. Cleanup errors are reported to the machine, which
//! releases the switch instead of wedging it; a later instance end retries.
//!
//! The service has no module capability: the existing dedicated loader domain does the work.
//! Only removable modules disappear; the permanent core and its helper remain until reboot.

use crate::config::exists_without_stat;
use crate::controller::Backend;
use crate::machine::Exit;
use crate::monitor::El2Monitor;
use log::info;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Requests either operation; the existing init trigger starts the loader on any write.
pub const LOAD_PROPERTY: &str = "xkernel_vm.load_driver";
/// Loader result, including the full operation-prefixed request.
pub const STATUS_PROPERTY: &str = "xkernel_vm.driver_status";

const POLL: Duration = Duration::from_millis(100);

type SetProperty = Box<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;
type GetProperty = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct DriverLoader {
    device: PathBuf,
    timeout: Duration,
    set: SetProperty,
    get: GetProperty,
    requests: AtomicU64,
    /// Held for a whole load, so that only one request is outstanding.
    loading: Mutex<()>,
}

impl DriverLoader {
    pub fn new(device: PathBuf, timeout: Duration, set: SetProperty, get: GetProperty) -> Self {
        Self { device, timeout, set, get, requests: AtomicU64::new(0), loading: Mutex::new(()) }
    }

    /// Checks every module on each start, including after a partial unload left the device behind.
    pub fn ensure_loaded(&self) -> Result<(), String> {
        self.operate("load")
    }

    /// Requests unload even if the main device is absent (a failed load may be partial).
    pub fn unload(&self) -> Result<(), String> {
        self.operate("unload")
    }

    fn operate(&self, operation: &str) -> Result<(), String> {
        let _loading = self.loading.lock().unwrap();
        let deadline = Instant::now() + self.timeout;
        while matches!(
            (self.get)("init.svc.xkernel_vm_loader").as_deref(),
            Some("running" | "restarting")
        ) {
            if Instant::now() >= deadline {
                return Err("the driver loader is still running at the deadline".into());
            }
            thread::sleep(POLL);
        }
        // A device can survive a partial unload while its leaf modules are missing. Always
        // let the loader check the full list, after the previous oneshot has really exited.
        let n = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        let request = format!("{operation}:{}.{n}", std::process::id());
        (self.set)(LOAD_PROPERTY, &request)
            .map_err(|e| format!("can't ask to {operation} the hypervisor driver: {e}"))?;
        info!("asked init to {operation} the hypervisor driver ({request})");
        let success =
            format!("{} {request}", if operation == "load" { "loaded" } else { "unloaded" });
        let failed = format!("failed {request}:");
        loop {
            match (self.get)(STATUS_PROPERTY) {
                Some(status) if status == success => {
                    let present = exists_without_stat(&self.device);
                    if (operation == "load") == present {
                        return Ok(());
                    }
                    return Err(format!(
                        "the driver loader reported success but {} {}",
                        self.device.display(),
                        if present { "still exists" } else { "is missing" }
                    ));
                }
                Some(status) if status.starts_with(&failed) => {
                    return Err(format!(
                        "the hypervisor driver did not {operation}: {}",
                        status[failed.len()..].trim()
                    ));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(format!("the hypervisor driver did not {operation} within {}s ({STATUS_PROPERTY} has no result for {request})", self.timeout.as_secs()));
            }
            thread::sleep(POLL);
        }
    }
}

/// A backend that makes sure the driver is loaded before the inner backend starts the VM.
pub struct WithDriver<B> {
    inner: B,
    loader: Option<Arc<DriverLoader>>,
    monitor: Arc<El2Monitor>,
}

impl<B> WithDriver<B> {
    pub fn new(inner: B, loader: Option<Arc<DriverLoader>>, monitor: Arc<El2Monitor>) -> Self {
        Self { inner, loader, monitor }
    }
}

impl<B: Backend> Backend for WithDriver<B> {
    type Instance = B::Instance;

    fn has_driver(&self) -> bool {
        self.loader.is_some()
    }

    fn unload_driver(&self) -> Result<(), String> {
        let stopped = self.monitor.stop_for_unload();
        let unloaded = self.loader.as_ref().map_or(Ok(()), |loader| loader.unload());
        match (stopped, unloaded) {
            (_, Err(reason)) => Err(reason),
            (Err(reason), Ok(())) => {
                log::warn!("{reason}; driver unload completed");
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn create_and_start(
        &self,
        generation: u64,
        on_death: Box<dyn FnOnce(Exit) + Send>,
    ) -> Result<B::Instance, String> {
        if let Some(loader) = &self.loader {
            loader.ensure_loaded()?;
        }
        self.monitor.allow();
        self.inner.create_and_start(generation, on_death)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    type Props = Arc<Mutex<HashMap<String, String>>>;

    struct Rig {
        dir: PathBuf,
        props: Props,
    }

    impl Rig {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("xkvm-driver-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir, props: Arc::default() }
        }

        fn device(&self) -> PathBuf {
            self.dir.join("xhyper")
        }

        /// A loader whose property writes go to `props`; `init` plays init plus the loader: it
        /// gets the device path, the props and the request just written.
        fn loader(
            &self,
            timeout_ms: u64,
            init: impl Fn(&PathBuf, &Props, &str) + Send + Sync + 'static,
        ) -> DriverLoader {
            let props = self.props.clone();
            let device = self.device();
            let set: SetProperty = Box::new(move |name, value| {
                props.lock().unwrap().insert(name.to_string(), value.to_string());
                if name == LOAD_PROPERTY {
                    init(&device, &props, value);
                }
                Ok(())
            });
            let props = self.props.clone();
            let get: GetProperty = Box::new(move |name| props.lock().unwrap().get(name).cloned());
            DriverLoader::new(self.device(), Duration::from_millis(timeout_ms), set, get)
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn status(props: &Props, value: String) {
        props.lock().unwrap().insert(STATUS_PROPERTY.to_string(), value);
    }

    #[test]
    fn a_present_device_still_requests_a_complete_module_check() {
        let rig = Rig::new("present");
        fs::write(rig.device(), b"").unwrap();
        let loader = rig.loader(500, |_, props, request| {
            assert!(request.starts_with("load:"));
            status(props, format!("loaded {request}"));
        });
        assert_eq!(loader.ensure_loaded(), Ok(()));
        assert!(rig.props.lock().unwrap()[LOAD_PROPERTY].starts_with("load:"));
    }

    #[test]
    fn a_partial_unload_with_a_remaining_device_is_repaired_on_next_start() {
        let rig = Rig::new("partial");
        fs::write(rig.device(), b"").unwrap();
        fs::write(rig.dir.join("vcpu"), b"").unwrap();
        let loader = rig.loader(500, |device, props, request| {
            let leaf = device.parent().unwrap().join("vcpu");
            if request.starts_with("unload:") {
                fs::remove_file(leaf).unwrap();
                status(props, format!("failed {request}: xhyper_rsc_mgr busy"));
            } else {
                fs::write(leaf, b"").unwrap();
                status(props, format!("loaded {request}"));
            }
        });
        assert!(loader.unload().is_err());
        assert!(rig.device().exists());
        assert!(!rig.dir.join("vcpu").exists());
        assert_eq!(loader.ensure_loaded(), Ok(()));
        assert!(rig.dir.join("vcpu").exists());
        assert!(rig.props.lock().unwrap()[LOAD_PROPERTY].starts_with("load:"));
    }

    #[test]
    fn a_missing_device_is_loaded_through_the_property() {
        let rig = Rig::new("load");
        let loader = rig.loader(2000, |device, props, request| {
            let (device, props, request) = (device.clone(), props.clone(), request.to_string());
            // The loader answers later, from another process.
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));
                fs::write(&device, b"").unwrap();
                status(&props, format!("loaded {request}"));
            });
        });
        assert_eq!(loader.ensure_loaded(), Ok(()));
        let props = rig.props.lock().unwrap();
        assert_eq!(props[LOAD_PROPERTY], format!("load:{}.1", std::process::id()));
    }

    #[test]
    fn a_failure_reports_the_loaders_reason() {
        let rig = Rig::new("fail");
        let loader = rig.loader(2000, |_, props, request| {
            status(props, format!("failed {request}: xhyper_rsc_mgr.ko: Invalid argument"));
        });
        let err = loader.ensure_loaded().unwrap_err();
        assert!(err.ends_with("did not load: xhyper_rsc_mgr.ko: Invalid argument"), "{err}");
    }

    #[test]
    fn an_older_result_does_not_count_and_silence_times_out() {
        let rig = Rig::new("stale");
        // A result of an earlier request (an earlier service) is still in the property.
        status(&rig.props, "loaded 1.1".to_string());
        let loader = rig.loader(300, |_, _, _| {});
        let started = Instant::now();
        let err = loader.ensure_loaded().unwrap_err();
        assert!(err.contains("did not load within"), "{err}");
        assert!(started.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn unload_uses_the_same_trigger_and_its_own_result() {
        let rig = Rig::new("unload");
        fs::write(rig.device(), b"").unwrap();
        let loader = rig.loader(1000, |device, props, request| {
            assert!(request.starts_with("unload:"));
            fs::remove_file(device).unwrap();
            status(props, format!("unloaded {request}"));
        });
        assert_eq!(loader.unload(), Ok(()));
        assert_eq!(
            rig.props.lock().unwrap()[LOAD_PROPERTY],
            format!("unload:{}.1", std::process::id())
        );
    }

    #[test]
    fn unload_failure_and_timeout_are_reported_even_without_a_device() {
        let rig = Rig::new("unloadfail");
        let loader =
            rig.loader(300, |_, props, request| status(props, format!("failed {request}: busy")));
        assert!(loader.unload().unwrap_err().ends_with("unload: busy"));
        let loader =
            rig.loader(100, |_, props, request| status(props, format!("loaded {request}")));
        assert!(loader.unload().unwrap_err().contains("did not unload within"));
    }

    #[test]
    fn concurrent_callers_serialize_complete_load_checks() {
        let rig = Rig::new("concurrent");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = requests.clone();
        let loader = Arc::new(rig.loader(1000, move |device, props, request| {
            assert!(!busy.swap(true, Ordering::SeqCst), "overlapping loader requests");
            seen.lock().unwrap().push(request.to_string());
            thread::sleep(Duration::from_millis(100));
            fs::write(device, b"").unwrap();
            status(props, format!("loaded {request}"));
            busy.store(false, Ordering::SeqCst);
        }));
        let both = Arc::new(std::sync::Barrier::new(2));
        let (other, gate) = (loader.clone(), both.clone());
        let waiter = thread::spawn(move || {
            gate.wait();
            other.ensure_loaded()
        });
        both.wait();
        assert_eq!(loader.ensure_loaded(), Ok(()));
        assert_eq!(waiter.join().unwrap(), Ok(()));
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn success_without_the_node_is_an_error_and_each_attempt_is_a_new_request() {
        let rig = Rig::new("nonode");
        let loader = rig.loader(1000, |_, props, request| {
            status(props, format!("loaded {request}"));
        });
        assert!(loader.ensure_loaded().unwrap_err().contains("is missing"));
        assert!(loader.ensure_loaded().is_err());
        let props = rig.props.lock().unwrap();
        assert_eq!(props[LOAD_PROPERTY], format!("load:{}.2", std::process::id()));
    }
    #[test]
    fn present_device_waits_for_a_timed_out_unloader_before_a_new_start() {
        let rig = Rig::new("unload-running");
        fs::write(rig.device(), b"").unwrap();
        rig.props.lock().unwrap().insert("init.svc.xkernel_vm_loader".into(), "running".into());
        let loader = Arc::new(rig.loader(500, |_, props, request| {
            status(props, format!("loaded {request}"));
        }));
        let other = loader.clone();
        let waiter = thread::spawn(move || other.ensure_loaded());
        thread::sleep(Duration::from_millis(150));
        assert!(!waiter.is_finished(), "must wait for the previous unload");
        assert!(!rig.props.lock().unwrap().contains_key(LOAD_PROPERTY));
        rig.props.lock().unwrap().insert("init.svc.xkernel_vm_loader".into(), "stopped".into());
        assert_eq!(waiter.join().unwrap(), Ok(()));
        rig.props.lock().unwrap().insert("init.svc.xkernel_vm_loader".into(), "running".into());
        let previous = rig.props.lock().unwrap()[LOAD_PROPERTY].clone();
        assert!(loader.ensure_loaded().unwrap_err().contains("still running"));
        assert_eq!(rig.props.lock().unwrap()[LOAD_PROPERTY], previous);
    }
}
