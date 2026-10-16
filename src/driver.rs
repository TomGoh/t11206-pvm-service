// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Loading the hypervisor's host driver on the first start after boot.
//!
//! The driver's modules are not loaded at boot, so the device behaves as before until someone
//! starts the VM. On a start whose device node is missing, the service asks init to run
//! `xkernel_vm_loader` by writing a new request number to [`LOAD_PROPERTY`] (the init script
//! starts the loader on any write). The loader loads the modules listed in its directory in
//! order, waits for the node, gives it its mode, and writes `loaded <request>` or
//! `failed <request>: <reason>` to [`STATUS_PROPERTY`]; the start goes on once the result for
//! this request is there, or fails with the loader's reason or a timeout.
//!
//! The service never loads modules itself: platform policy gives module loading only to
//! dedicated domains, and a service that answers apps should not hold that right. The property
//! names are also in `loader/main.rs` and the init script.

use crate::config::exists_without_stat;
use crate::controller::Backend;
use crate::machine::Exit;
use log::info;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Written by the service to ask for a load; init starts the loader on any write.
pub const LOAD_PROPERTY: &str = "xkernel_vm.load_driver";
/// Written by the loader: `loaded <request>` or `failed <request>: <reason>`.
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
}

impl DriverLoader {
    pub fn new(device: PathBuf, timeout: Duration, set: SetProperty, get: GetProperty) -> Self {
        Self { device, timeout, set, get, requests: AtomicU64::new(0) }
    }

    /// Returns once the device node is there, asking for a load if it is not.
    pub fn ensure_loaded(&self) -> Result<(), String> {
        if exists_without_stat(&self.device) {
            return Ok(());
        }
        // Unique within a boot: a restarted service has another PID.
        let n = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        let request = format!("{}.{n}", std::process::id());
        (self.set)(LOAD_PROPERTY, &request)
            .map_err(|e| format!("can't ask for the hypervisor driver: {e}"))?;
        info!(
            "{} is missing; asked init to load the hypervisor driver ({request})",
            self.device.display()
        );
        let loaded = format!("loaded {request}");
        let failed = format!("failed {request}");
        let deadline = Instant::now() + self.timeout;
        loop {
            match (self.get)(STATUS_PROPERTY) {
                Some(status) if status == loaded => {
                    return if exists_without_stat(&self.device) {
                        info!("hypervisor driver loaded ({request})");
                        Ok(())
                    } else {
                        Err(format!(
                            "the driver loader reported success but {} is missing",
                            self.device.display()
                        ))
                    };
                }
                Some(status) if status.starts_with(&failed) => {
                    let reason = status[failed.len()..].trim_start_matches(':').trim();
                    return Err(format!("the hypervisor driver did not load: {reason}"));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the hypervisor driver did not load within {}s ({STATUS_PROPERTY} has no result for {request})",
                    self.timeout.as_secs()
                ));
            }
            thread::sleep(POLL);
        }
    }
}

/// A backend that makes sure the driver is loaded before the inner backend starts the VM.
pub struct WithDriver<B> {
    inner: B,
    loader: Option<DriverLoader>,
}

impl<B> WithDriver<B> {
    pub fn new(inner: B, loader: Option<DriverLoader>) -> Self {
        Self { inner, loader }
    }
}

impl<B: Backend> Backend for WithDriver<B> {
    type Instance = B::Instance;

    fn create_and_start(
        &self,
        generation: u64,
        on_death: Box<dyn FnOnce(Exit) + Send>,
    ) -> Result<B::Instance, String> {
        if let Some(loader) = &self.loader {
            loader.ensure_loaded()?;
        }
        self.inner.create_and_start(generation, on_death)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::{Arc, Mutex};

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
    fn a_present_device_asks_nothing() {
        let rig = Rig::new("present");
        fs::write(rig.device(), b"").unwrap();
        let loader = rig.loader(500, |_, _, _| panic!("asked for a load"));
        assert_eq!(loader.ensure_loaded(), Ok(()));
        assert!(rig.props.lock().unwrap().is_empty());
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
        assert_eq!(props[LOAD_PROPERTY], format!("{}.1", std::process::id()));
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
    fn success_without_the_node_is_an_error_and_each_attempt_is_a_new_request() {
        let rig = Rig::new("nonode");
        let loader = rig.loader(1000, |_, props, request| {
            status(props, format!("loaded {request}"));
        });
        assert!(loader.ensure_loaded().unwrap_err().contains("is missing"));
        assert!(loader.ensure_loaded().is_err());
        let props = rig.props.lock().unwrap();
        assert_eq!(props[LOAD_PROPERTY], format!("{}.2", std::process::id()));
    }
}
