// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Runs the lifecycle machine against AVF.
//!
//! "AVF" here stands for the backend that runs the VM: AVF itself (`avf`), or this service
//! running crosvm directly (`direct`); "the AVF objects" are the backend's instance.
//!
//! Requests (from Binder threads) and events (from AVF callbacks, timers and probes) lock the
//! [`Machine`], apply one transition, and queue the resulting actions to one executor thread, so
//! the AVF objects of the instance are only created, stopped and released in order. Every
//! visible change is handed to the `notify` hook outside the lock.
//!
//! The executor holds the instance's AVF objects from a successful start until the reclaim check
//! begins; the check drops them and then waits up to the reclaim window for the instance's
//! processes to finish exiting.
//!
//! Persisted record: right after a start the executor writes the instance's crosvm processes to
//! `state_dir/instance.json` with the boot ID; a gone confirmation clears it. A restarted service
//! reads it (and scans for `crosvm_<name>`) before accepting a start, so it never starts a second
//! instance next to one an earlier run left behind.

use crate::machine::{Action, Event, Exit, Machine, Refused, Snapshot};
use crate::reclaim::{self, TrackedProcess};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

/// A started VM, as the executor holds it.
pub trait Instance: Send + Sync + 'static {
    /// The VM's vsock CID.
    fn cid(&self) -> i32;
    /// Asks AVF to stop the VM. The death is reported through the death hook.
    fn stop(&self) -> Result<(), String>;
    /// One attempt of the readiness handshake: whether a vsock connection to `port` succeeds.
    fn probe_ready(&self, port: u32) -> bool;
}

/// Creates instances.
pub trait Backend: Send + Sync + 'static {
    type Instance: Instance;
    /// Creates and starts the VM of `generation`. `on_death` must be called exactly once when AVF
    /// reports the VM dead or the virtualization manager serving it dies.
    fn create_and_start(
        &self,
        generation: u64,
        on_death: Box<dyn FnOnce(Exit) + Send>,
    ) -> Result<Self::Instance, String>;
}

/// The timing and identity the controller needs from the instance config.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `crosvm_<name>`.
    pub process_name: String,
    pub state_dir: PathBuf,
    pub proc_root: PathBuf,
    pub boot_id: String,
    pub stop_timeout: Duration,
    pub reclaim_window: Duration,
    /// Vsock port and deadline of the readiness handshake.
    pub readiness: Option<(u32, Duration)>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct InstanceRecord {
    boot_id: String,
    processes: Vec<TrackedProcess>,
}

const RECORD_FILE: &str = "instance.json";
const RECLAIM_POLL: Duration = Duration::from_millis(200);
const READY_POLL: Duration = Duration::from_secs(1);

type Preflight = Box<dyn Fn() -> Result<(), String> + Send + Sync>;
type Notify = Box<dyn Fn(Snapshot) + Send + Sync>;
type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

struct Inner<B: Backend> {
    machine: Mutex<Machine>,
    backend: B,
    settings: Settings,
    preflight: Preflight,
    notify: Notify,
    clock: Clock,
    actions: Mutex<Sender<Action>>,
    /// The current instance and its generation, while the executor holds it.
    instance: Mutex<Option<(u64, Arc<B::Instance>)>>,
    tracked: Mutex<Vec<TrackedProcess>>,
}

/// The controller of the one instance.
pub struct Controller<B: Backend> {
    inner: Arc<Inner<B>>,
}

impl<B: Backend> Clone for Controller<B> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<B: Backend> Controller<B> {
    /// Builds the controller and starts its executor. Reads the persisted record first: a
    /// process an earlier run left behind starts the machine in `ReclaimUnknown`.
    pub fn new(
        backend: B,
        settings: Settings,
        preflight: Preflight,
        notify: Notify,
        clock: Clock,
    ) -> Self {
        let restored = restore(&settings);
        if let Some(detail) = &restored.1 {
            warn!("found a leftover instance: {detail}");
        }
        let (tx, rx) = channel::<Action>();
        let inner = Arc::new(Inner {
            machine: Mutex::new(Machine::new(settings.readiness.is_some(), restored.1)),
            backend,
            settings,
            preflight,
            notify,
            clock,
            actions: Mutex::new(tx),
            instance: Mutex::new(None),
            tracked: Mutex::new(restored.0),
        });
        let weak = Arc::downgrade(&inner);
        thread::Builder::new()
            .name("xkvm-executor".into())
            .spawn(move || {
                for action in rx {
                    let Some(inner) = weak.upgrade() else { return };
                    inner.execute(action);
                }
            })
            .expect("can't spawn the executor");
        let controller = Self { inner };
        controller.refresh();
        controller
    }

    /// The current snapshot, after re-evaluating availability while no instance exists.
    pub fn status(&self) -> Snapshot {
        self.refresh();
        self.snapshot()
    }

    /// The current snapshot as it is. Never publishes, so it may be taken under a lock that
    /// publishing takes.
    pub fn snapshot(&self) -> Snapshot {
        self.inner.machine.lock().unwrap().snapshot()
    }

    /// Re-evaluates availability while no instance exists; publishes a change.
    pub fn refresh(&self) {
        let preflight = (self.inner.preflight)();
        let changed = {
            let mut machine = self.inner.machine.lock().unwrap();
            machine.refresh_availability(preflight).then(|| machine.snapshot())
        };
        if let Some(snapshot) = changed {
            (self.inner.notify)(snapshot);
        }
    }

    pub fn request_start(&self) -> Result<u64, Refused> {
        let preflight = (self.inner.preflight)();
        self.inner.transition(|m| m.request_start(preflight))
    }

    pub fn request_stop(&self) -> u64 {
        self.inner.transition(|m| Ok::<_, Refused>(m.request_stop())).unwrap_or(0)
    }
}

impl<B: Backend> Inner<B> {
    /// Applies one transition, queues its actions and notifies a visible change.
    fn transition<T>(
        &self,
        f: impl FnOnce(&mut Machine) -> Result<(T, Vec<Action>), Refused>,
    ) -> Result<T, Refused> {
        let (result, actions, snapshot) = {
            let mut machine = self.machine.lock().unwrap();
            let before = machine.snapshot().seq;
            let outcome = f(&mut machine);
            let snapshot = machine.snapshot();
            let changed = (snapshot.seq != before).then_some(snapshot);
            match outcome {
                Ok((value, actions)) => (Ok(value), actions, changed),
                Err(refused) => (Err(refused), Vec::new(), changed),
            }
        };
        self.queue(actions);
        if let Some(snapshot) = snapshot {
            (self.notify)(snapshot);
        }
        result
    }

    fn post(&self, event: Event) {
        info!("event {event:?}");
        let _ = self.transition(|m| Ok::<_, Refused>(((), m.on_event(event))));
    }

    fn queue(&self, actions: Vec<Action>) {
        let tx = self.actions.lock().unwrap();
        for action in actions {
            let _ = tx.send(action);
        }
    }

    fn current(&self, generation: u64) -> Option<Arc<B::Instance>> {
        match &*self.instance.lock().unwrap() {
            Some((g, instance)) if *g == generation => Some(instance.clone()),
            _ => None,
        }
    }

    fn execute(self: &Arc<Self>, action: Action) {
        info!("action {action:?}");
        match action {
            Action::CreateAndStart { generation } => self.create_and_start(generation),
            Action::Stop { generation } => match self.current(generation) {
                Some(instance) => {
                    if let Err(e) = instance.stop() {
                        // The VM may already be dead; its death report settles the stop.
                        warn!("stop of generation {generation} failed: {e}");
                    }
                }
                None => warn!("stop of generation {generation}: no instance held"),
            },
            Action::ArmStopDeadline { generation } => {
                let weak = Arc::downgrade(self);
                let timeout = self.settings.stop_timeout;
                spawn("xkvm-stop-deadline", move || {
                    thread::sleep(timeout);
                    if let Some(inner) = weak.upgrade() {
                        inner.post(Event::StopTimedOut { generation });
                    }
                });
            }
            Action::ProbeReady { generation } => self.probe_ready(generation),
            Action::CheckReclaim { generation } => self.check_reclaim(generation),
        }
    }

    fn create_and_start(self: &Arc<Self>, generation: u64) {
        let weak: Weak<Self> = Arc::downgrade(self);
        let on_death = Box::new(move |exit: Exit| {
            if let Some(inner) = weak.upgrade() {
                inner.post(Event::Died { generation, exit });
            }
        });
        match self.backend.create_and_start(generation, on_death) {
            Ok(instance) => {
                let cid = instance.cid();
                *self.instance.lock().unwrap() = Some((generation, Arc::new(instance)));
                let processes =
                    reclaim::find_by_arg0(&self.settings.proc_root, &self.settings.process_name);
                if processes.is_empty() {
                    warn!("no {} process found right after the start", self.settings.process_name);
                }
                self.record(&processes);
                *self.tracked.lock().unwrap() = processes;
                self.post(Event::Started { generation, cid, at_ms: (self.clock)() });
            }
            Err(reason) => {
                error!("start of generation {generation} failed: {reason}");
                self.post(Event::StartFailed { generation, reason });
            }
        }
    }

    fn probe_ready(self: &Arc<Self>, generation: u64) {
        let Some((port, timeout)) = self.settings.readiness else { return };
        let weak = Arc::downgrade(self);
        spawn("xkvm-ready-probe", move || {
            let deadline = Instant::now() + timeout;
            loop {
                let Some(inner) = weak.upgrade() else { return };
                // Hold the instance only for one attempt, so a reclaim check is never delayed.
                let Some(instance) = inner.current(generation) else { return };
                if instance.probe_ready(port) {
                    drop(instance);
                    inner.post(Event::Ready { generation });
                    return;
                }
                drop(instance);
                if Instant::now() >= deadline {
                    inner.post(Event::ReadyTimedOut { generation });
                    return;
                }
                drop(inner);
                thread::sleep(READY_POLL);
            }
        });
    }

    fn check_reclaim(self: &Arc<Self>, generation: u64) {
        // Release the AVF objects of this generation.
        {
            let mut slot = self.instance.lock().unwrap();
            if matches!(&*slot, Some((g, _)) if *g == generation) {
                *slot = None;
            }
        }
        let tracked = self.tracked.lock().unwrap().clone();
        let deadline = Instant::now() + self.settings.reclaim_window;
        loop {
            let left =
                reclaim::leftovers(&self.settings.proc_root, &self.settings.process_name, &tracked);
            if left.is_empty() {
                self.tracked.lock().unwrap().clear();
                self.record(&[]);
                self.post(Event::ReclaimChecked { generation, gone: true, detail: String::new() });
                return;
            }
            if Instant::now() >= deadline {
                let detail = left.join("; ");
                warn!("instance of generation {generation} not gone: {detail}");
                self.post(Event::ReclaimChecked { generation, gone: false, detail });
                return;
            }
            thread::sleep(RECLAIM_POLL);
        }
    }

    fn record(&self, processes: &[TrackedProcess]) {
        let path = self.settings.state_dir.join(RECORD_FILE);
        let result = if processes.is_empty() {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            let record = InstanceRecord {
                boot_id: self.settings.boot_id.clone(),
                processes: processes.to_vec(),
            };
            fs::create_dir_all(&self.settings.state_dir)
                .and_then(|()| fs::write(&path, serde_json::to_vec(&record).unwrap()))
        };
        if let Err(e) = result {
            error!("can't update {}: {e}", path.display());
        }
    }
}

/// Reads the persisted record of an earlier run. Returns the processes still to track and, when
/// any of them (or any `crosvm_<name>` process) is still there, what is left.
fn restore(settings: &Settings) -> (Vec<TrackedProcess>, Option<String>) {
    let path = settings.state_dir.join(RECORD_FILE);
    let record: Option<InstanceRecord> =
        fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let tracked = match record {
        // PIDs of another boot mean nothing in this one.
        Some(record) if record.boot_id == settings.boot_id => record.processes,
        Some(_) => {
            let _ = fs::remove_file(&path);
            Vec::new()
        }
        None => Vec::new(),
    };
    let left = reclaim::leftovers(&settings.proc_root, &settings.process_name, &tracked);
    if left.is_empty() {
        if !tracked.is_empty() {
            let _ = fs::remove_file(&path);
        }
        (Vec::new(), None)
    } else {
        (tracked, Some(format!("left by an earlier run: {}", left.join("; "))))
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = thread::Builder::new().name(name.into()).spawn(f) {
        error!("can't spawn {name}: {e}");
    }
}

/// The record path.
#[cfg(test)]
pub fn record_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join(RECORD_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::VmState;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Condvar;

    type DeathHook = Arc<Mutex<Option<Box<dyn FnOnce(Exit) + Send>>>>;

    /// A fake AVF: `start_ok` decides the start; a started VM "dies" when `stop` is called unless
    /// `hang_on_stop` is set; a fake /proc entry plays the crosvm process.
    struct FakeBackend {
        proc_root: PathBuf,
        start_ok: AtomicBool,
        hang_on_stop: AtomicBool,
        leave_process: AtomicBool,
        ready_after: AtomicU32,
        starts: AtomicU32,
        next_pid: AtomicU32,
        death: DeathHook,
    }

    struct FakeInstance {
        proc_root: PathBuf,
        pid: u32,
        hang_on_stop: bool,
        leave_process: bool,
        probes: AtomicU32,
        ready_after: u32,
        death: DeathHook,
    }

    fn write_process(root: &Path, pid: u32, argv0: &str, state: char) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cmdline"), format!("{argv0}\0run\0")).unwrap();
        let zeros = ["0"; 18].join(" ");
        fs::write(dir.join("stat"), format!("{pid} (crosvm) {state} {zeros} {pid}000 0\n"))
            .unwrap();
    }

    impl Instance for FakeInstance {
        fn cid(&self) -> i32 {
            2048
        }
        fn stop(&self) -> Result<(), String> {
            if !self.hang_on_stop {
                if !self.leave_process {
                    let _ = fs::remove_dir_all(self.proc_root.join(self.pid.to_string()));
                }
                if let Some(death) = self.death.lock().unwrap().take() {
                    death(Exit::Killed);
                }
            }
            Ok(())
        }
        fn probe_ready(&self, _port: u32) -> bool {
            self.probes.fetch_add(1, Ordering::SeqCst) + 1 >= self.ready_after
        }
    }

    impl Backend for FakeBackend {
        type Instance = FakeInstance;
        fn create_and_start(
            &self,
            _generation: u64,
            on_death: Box<dyn FnOnce(Exit) + Send>,
        ) -> Result<FakeInstance, String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if !self.start_ok.load(Ordering::SeqCst) {
                return Err("createVm: denied".into());
            }
            let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
            write_process(&self.proc_root, pid, "crosvm_xkernel_vm", 'S');
            *self.death.lock().unwrap() = Some(on_death);
            Ok(FakeInstance {
                proc_root: self.proc_root.clone(),
                pid,
                hang_on_stop: self.hang_on_stop.load(Ordering::SeqCst),
                leave_process: self.leave_process.load(Ordering::SeqCst),
                probes: AtomicU32::new(0),
                ready_after: self.ready_after.load(Ordering::SeqCst),
                death: self.death.clone(),
            })
        }
    }

    struct Harness {
        controller: Controller<FakeBackend>,
        seen: Arc<(Mutex<Vec<Snapshot>>, Condvar)>,
        root: PathBuf,
    }

    impl Harness {
        fn new(tag: &str, readiness: Option<(u32, Duration)>, setup: impl FnOnce(&Path)) -> Self {
            let root = std::env::temp_dir().join(format!("xkvm-ctl-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("proc")).unwrap();
            setup(&root);
            let backend = FakeBackend {
                proc_root: root.join("proc"),
                start_ok: AtomicBool::new(true),
                hang_on_stop: AtomicBool::new(false),
                leave_process: AtomicBool::new(false),
                ready_after: AtomicU32::new(2),
                starts: AtomicU32::new(0),
                next_pid: AtomicU32::new(1000),
                death: Arc::new(Mutex::new(None)),
            };
            let settings = Settings {
                process_name: "crosvm_xkernel_vm".into(),
                state_dir: root.join("state"),
                proc_root: root.join("proc"),
                boot_id: "boot-1".into(),
                stop_timeout: Duration::from_millis(300),
                reclaim_window: Duration::from_millis(300),
                readiness,
            };
            let seen = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
            let sink = seen.clone();
            let controller = Controller::new(
                backend,
                settings,
                Box::new(|| Ok(())),
                Box::new(move |s| {
                    sink.0.lock().unwrap().push(s);
                    sink.1.notify_all();
                }),
                Box::new(|| 77),
            );
            Harness { controller, seen, root }
        }

        fn backend(&self) -> &FakeBackend {
            &self.controller.inner.backend
        }

        fn wait_for(&self, state: VmState) -> Snapshot {
            let (lock, cv) = &*self.seen;
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut seen = lock.lock().unwrap();
            loop {
                if let Some(s) = seen.iter().rev().find(|s| s.state == state) {
                    return s.clone();
                }
                let now = Instant::now();
                assert!(now < deadline, "never reached {state:?}; saw {:?}", *seen);
                seen = cv.wait_timeout(seen, deadline - now).unwrap().0;
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn start_stop_and_start_again() {
        let h = Harness::new("cycle", None, |_| {});
        h.controller.request_start().unwrap();
        let running = h.wait_for(VmState::RunningNotReady);
        assert_eq!((running.cid, running.started_at_ms), (2048, 77));
        assert!(record_path(&h.root.join("state")).exists(), "processes recorded");

        h.controller.request_stop();
        let stopped = h.wait_for(VmState::Stopped);
        assert!(stopped.reclaim_confirmed);
        assert!(!record_path(&h.root.join("state")).exists(), "record cleared");

        h.seen.0.lock().unwrap().clear();
        h.controller.request_start().unwrap();
        h.wait_for(VmState::RunningNotReady);
        assert_eq!(h.backend().starts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn readiness_probe_reaches_ready() {
        let h = Harness::new("ready", Some((5000, Duration::from_secs(5))), |_| {});
        h.controller.request_start().unwrap();
        h.wait_for(VmState::Ready);
    }

    #[test]
    fn hanging_stop_is_reclaim_unknown_and_blocks_start() {
        let h = Harness::new("hang", None, |_| {});
        h.backend().hang_on_stop.store(true, Ordering::SeqCst);
        h.controller.request_start().unwrap();
        h.wait_for(VmState::RunningNotReady);
        h.controller.request_stop();
        let s = h.wait_for(VmState::ReclaimUnknown);
        assert!(s.reason.contains("stop deadline"), "{}", s.reason);
        assert!(matches!(h.controller.request_start(), Err(Refused::ReclaimPending(_))));
        assert_eq!(h.backend().starts.load(Ordering::SeqCst), 1, "no second instance");
    }

    #[test]
    fn leftover_process_after_death_is_reported() {
        let h = Harness::new("leftover", None, |_| {});
        h.backend().leave_process.store(true, Ordering::SeqCst);
        h.controller.request_start().unwrap();
        h.wait_for(VmState::RunningNotReady);
        h.controller.request_stop();
        let s = h.wait_for(VmState::ReclaimUnknown);
        assert!(s.reason.contains("pid 1000"), "{}", s.reason);
        // The process finally exits; a stop re-checks and confirms.
        fs::remove_dir_all(h.root.join("proc/1000")).unwrap();
        h.controller.request_stop();
        h.wait_for(VmState::Stopped);
    }

    #[test]
    fn start_failure_ends_failed_and_allows_retry() {
        let h = Harness::new("fail", None, |_| {});
        h.backend().start_ok.store(false, Ordering::SeqCst);
        h.controller.request_start().unwrap();
        let s = h.wait_for(VmState::Failed);
        assert!(s.reason.contains("denied") && s.reclaim_confirmed, "{s:?}");
        h.backend().start_ok.store(true, Ordering::SeqCst);
        h.controller.request_start().unwrap();
        h.wait_for(VmState::RunningNotReady);
    }

    #[test]
    fn restart_with_a_leftover_from_this_boot_is_reclaim_unknown() {
        let h = Harness::new("restore", None, |root| {
            write_process(&root.join("proc"), 4242, "crosvm_xkernel_vm", 'D');
            let record = InstanceRecord {
                boot_id: "boot-1".into(),
                processes: vec![TrackedProcess { pid: 4242, start_time: 4242000 }],
            };
            fs::create_dir_all(root.join("state")).unwrap();
            fs::write(record_path(&root.join("state")), serde_json::to_vec(&record).unwrap())
                .unwrap();
        });
        let s = h.controller.status();
        assert_eq!(s.state, VmState::ReclaimUnknown, "{s:?}");
        assert!(s.reason.contains("earlier run"));
        assert!(h.controller.request_start().is_err());
    }

    #[test]
    fn record_of_another_boot_is_discarded() {
        let h = Harness::new("oldboot", None, |root| {
            let record = InstanceRecord {
                boot_id: "boot-0".into(),
                processes: vec![TrackedProcess { pid: 1, start_time: 1 }],
            };
            fs::create_dir_all(root.join("state")).unwrap();
            fs::write(record_path(&root.join("state")), serde_json::to_vec(&record).unwrap())
                .unwrap();
        });
        assert_eq!(h.controller.status().state, VmState::Stopped);
        assert!(!record_path(&h.root.join("state")).exists());
    }
}
