// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The Binder face of the controller: `IXKernelVmControl`.
//!
//! Callers must be uid 0 or 1000, or hold [`CONTROL_PERMISSION`] (a signature permission the
//! control app declares). Callbacks get every snapshot the controller publishes; registering one
//! returns the snapshot it starts from, under the same lock that publishing takes, so no change
//! falls between the two. A callback whose process dies is dropped.
//!
//! The serial console (see `console`) is served to the same callers: attaching returns the kept
//! output and subscribes the client to the rest, and writing types into the running guest. The
//! console gives full control of the guest's shell, which is why it needs the same permission as
//! starting and stopping. Console contents are never logged.
//!
//! `IXKernelVmMonitor` is registered separately and is read-only: the hypervisor's EL2 cycles per
//! CPU and the VM's CPU time (see `monitor`). Monitor apps get it without control: callers must
//! be uid 0 or 1000, or hold [`READ_STATS_PERMISSION`] or [`CONTROL_PERMISSION`]. The figures are
//! system-wide CPU accounting, which platform policy keeps from ordinary apps (`/proc/stat`), so
//! the interface is not open to every app either.

use crate::config::{BackendKind, Protection, VmConfig};
use crate::console::{self, Console, InputError, Listener, SendError, MAX_INPUT};
use crate::controller::{Backend, Controller};
use crate::machine::{Refused, Snapshot, VmState};
use crate::monitor::{self, El2Monitor, El2Reading, VmProcesses};
use android_os_permissions_aidl::aidl::android::os::IPermissionController::IPermissionController;
use binder::{
    DeathRecipient, ExceptionCode, IBinder, Interface, Result as BinderResult, SpIBinder, Status,
    StatusCode, Strong, ThreadState,
};
use com_kylinos_xkernelvm::aidl::com::kylinos::xkernelvm::{
    ConsoleOutput::ConsoleOutput,
    IXKernelVmCallback::IXKernelVmCallback,
    IXKernelVmConsoleCallback::IXKernelVmConsoleCallback,
    El2Counters::El2Counters,
    El2CycleSample::El2CycleSample,
    IXKernelVmControl::{
        IXKernelVmControl, ERROR_BUSY, ERROR_NOT_RUNNING, ERROR_RECLAIM_PENDING, ERROR_UNAVAILABLE,
    },
    IXKernelVmMonitor::{IXKernelVmMonitor, EL2_COUNTING, EL2_UNAVAILABLE},
    VmCpuTime::VmCpuTime,
    VmState::VmState as AidlVmState,
    VmStatus::VmStatus,
};
use log::{info, warn};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

/// The servicemanager name of the control interface.
pub const SERVICE_NAME: &str = "com.kylinos.xkernelvm.IXKernelVmControl/default";
/// The permission that lets an app use the control interface.
pub const CONTROL_PERMISSION: &str = "com.kylinos.xkernelvm.permission.CONTROL_VM";
/// The servicemanager name of the monitoring interface.
pub const MONITOR_SERVICE_NAME: &str = "com.kylinos.xkernelvm.IXKernelVmMonitor/default";
/// The permission that lets an app read the monitoring interface (and nothing else).
pub const READ_STATS_PERMISSION: &str = "com.kylinos.xkernelvm.permission.READ_VM_STATS";

const AID_ROOT: u32 = 0;
const AID_SYSTEM: u32 = 1000;

/// The fields of `VmStatus` that come from the fixed config and the service run.
#[derive(Clone)]
struct StaticFields {
    session_id: String,
    memory_mib: i32,
    cpu_count: i32,
    protected_vm: bool,
    without_firmware: bool,
    readiness_configured: bool,
    via_avf: bool,
}

struct Entry {
    id: u64,
    callback: Strong<dyn IXKernelVmCallback>,
    _death: DeathRecipient,
}

/// The registered callbacks. Publishing and registering take the same lock.
pub struct Callbacks {
    fields: StaticFields,
    entries: Mutex<(u64, Vec<Entry>)>,
}

impl Callbacks {
    pub fn new(config: &VmConfig, session_id: String) -> Arc<Self> {
        Arc::new(Self {
            fields: StaticFields {
                session_id,
                memory_mib: config.memory_mib as i32,
                cpu_count: config.cpus as i32,
                protected_vm: config.protection.is_protected(),
                without_firmware: config.protection == Protection::WithoutFirmware,
                readiness_configured: config.readiness.is_some(),
                via_avf: config.backend == BackendKind::Avf,
            },
            entries: Mutex::new((0, Vec::new())),
        })
    }

    fn status(&self, snapshot: &Snapshot) -> VmStatus {
        let f = &self.fields;
        VmStatus {
            state: aidl_state(snapshot.state),
            sessionId: f.session_id.clone(),
            seq: snapshot.seq as i64,
            operationId: snapshot.operation as i64,
            instance: snapshot.generation as i64,
            reason: snapshot.reason.clone(),
            reclaimConfirmed: snapshot.reclaim_confirmed,
            cid: snapshot.cid,
            startedAtElapsedMs: snapshot.started_at_ms as i64,
            lastExit: snapshot.last_exit.clone(),
            memoryMib: f.memory_mib,
            cpuCount: f.cpu_count,
            protectedVm: f.protected_vm,
            withoutFirmware: f.without_firmware,
            diceAvailable: false,
            readinessConfigured: f.readiness_configured,
            viaAvf: f.via_avf,
        }
    }

    /// Delivers a snapshot to every callback; drops the ones that cannot be reached.
    pub fn publish(&self, snapshot: Snapshot) {
        let status = self.status(&snapshot);
        let mut entries = self.entries.lock().unwrap();
        entries.1.retain(|entry| match entry.callback.onStatusChanged(&status) {
            Ok(()) => true,
            Err(e) => {
                warn!("dropping callback {}: {e}", entry.id);
                false
            }
        });
    }

    fn register(
        self: &Arc<Self>,
        callback: &Strong<dyn IXKernelVmCallback>,
        current: impl FnOnce() -> Snapshot,
    ) -> BinderResult<VmStatus> {
        let mut entries = self.entries.lock().unwrap();
        entries.0 += 1;
        let id = entries.0;
        let weak: Weak<Self> = Arc::downgrade(self);
        let mut death = DeathRecipient::new(move || {
            if let Some(callbacks) = weak.upgrade() {
                callbacks.entries.lock().unwrap().1.retain(|entry| entry.id != id);
                info!("callback {id} died");
            }
        });
        callback.as_binder().link_to_death(&mut death)?;
        entries.1.push(Entry { id, callback: callback.clone(), _death: death });
        // Taken under the lock, so a change published after it reaches this callback too.
        Ok(self.status(&current()))
    }

    fn unregister(&self, callback: &Strong<dyn IXKernelVmCallback>) {
        let binder = callback.as_binder();
        self.entries.lock().unwrap().1.retain(|entry| entry.callback.as_binder() != binder);
    }
}

fn aidl_output(output: console::Output) -> ConsoleOutput {
    ConsoleOutput {
        instance: output.instance as i64,
        offset: output.offset as i64,
        data: output.data,
    }
}

struct ConsoleListener(Strong<dyn IXKernelVmConsoleCallback>);

impl Listener for ConsoleListener {
    fn send(&self, output: &console::Output) -> Result<(), SendError> {
        match self.0.onConsoleOutput(&aidl_output(output.clone())) {
            Ok(()) => Ok(()),
            Err(e) if e.transaction_error() == StatusCode::DEAD_OBJECT => Err(SendError::Gone),
            Err(e) => Err(SendError::Lost(e.to_string())),
        }
    }
}

struct Attached {
    binder: SpIBinder,
    id: u64,
    _death: DeathRecipient,
}

/// The console clients, by Binder object. The attached list is locked before the console.
pub struct ConsoleClients {
    console: Arc<Console>,
    attached: Mutex<Vec<Attached>>,
}

impl ConsoleClients {
    pub fn new(console: Arc<Console>) -> Arc<Self> {
        Arc::new(Self { console, attached: Mutex::new(Vec::new()) })
    }

    fn attach(
        self: &Arc<Self>,
        callback: &Strong<dyn IXKernelVmConsoleCallback>,
    ) -> BinderResult<ConsoleOutput> {
        let mut binder = callback.as_binder();
        let mut attached = self.attached.lock().unwrap();
        // Attaching again resynchronises: the earlier registration goes first.
        if let Some(pos) = attached.iter().position(|a| a.binder == binder) {
            self.console.detach(attached.remove(pos).id);
        }
        let (id, history) = self.console.attach(Box::new(ConsoleListener(callback.clone())));
        let weak: Weak<Self> = Arc::downgrade(self);
        let mut death = DeathRecipient::new(move || {
            if let Some(clients) = weak.upgrade() {
                clients.forget(id);
                info!("console client {id} died");
            }
        });
        if let Err(e) = binder.link_to_death(&mut death) {
            self.console.detach(id);
            return Err(e.into());
        }
        attached.push(Attached { binder, id, _death: death });
        info!("console client {id} attached at {}+{}", history.offset, history.data.len());
        Ok(aidl_output(history))
    }

    fn forget(&self, id: u64) {
        self.attached.lock().unwrap().retain(|a| a.id != id);
        self.console.detach(id);
    }

    fn detach(&self, callback: &Strong<dyn IXKernelVmConsoleCallback>) {
        let binder = callback.as_binder();
        let mut attached = self.attached.lock().unwrap();
        attached.retain(|a| {
            if a.binder == binder {
                self.console.detach(a.id);
                false
            } else {
                true
            }
        });
    }

    fn write(&self, data: &[u8]) -> BinderResult<()> {
        self.console.write(data).map_err(|e| match e {
            InputError::NotRunning => {
                Status::new_service_specific_error_str(ERROR_NOT_RUNNING, Some("no instance runs"))
            }
            InputError::Full { written } => Status::new_service_specific_error_str(
                ERROR_BUSY,
                Some(format!(
                    "the guest has not read the earlier input ({written} of {} bytes taken)",
                    data.len()
                )),
            ),
            InputError::TooLarge => Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some(format!("at most {MAX_INPUT} bytes per write")),
            ),
            InputError::Io(e) => Status::new_service_specific_error_str(
                ERROR_NOT_RUNNING,
                Some(format!("the console input is closed: {e}")),
            ),
        })
    }
}

fn aidl_state(state: VmState) -> AidlVmState {
    match state {
        VmState::Unavailable => AidlVmState::UNAVAILABLE,
        VmState::Stopped => AidlVmState::STOPPED,
        VmState::Starting => AidlVmState::STARTING,
        VmState::RunningNotReady => AidlVmState::RUNNING_NOT_READY,
        VmState::Ready => AidlVmState::READY,
        VmState::Stopping => AidlVmState::STOPPING,
        VmState::ReclaimUnknown => AidlVmState::RECLAIM_UNKNOWN,
        VmState::Failed => AidlVmState::FAILED,
    }
}

/// The `IXKernelVmControl` implementation.
pub struct ControlService<B: Backend> {
    controller: Controller<B>,
    callbacks: Arc<Callbacks>,
    console: Arc<ConsoleClients>,
}

impl<B: Backend> ControlService<B> {
    pub fn new(
        controller: Controller<B>,
        callbacks: Arc<Callbacks>,
        console: Arc<ConsoleClients>,
    ) -> Self {
        Self { controller, callbacks, console }
    }
}

impl<B: Backend> Interface for ControlService<B> {}

fn check_caller() -> BinderResult<()> {
    check_permission(&[CONTROL_PERMISSION])
}

/// Admits uid 0 and 1000, and callers holding one of `permissions`.
fn check_permission(permissions: &[&str]) -> BinderResult<()> {
    let uid = ThreadState::get_calling_uid();
    if uid == AID_ROOT || uid == AID_SYSTEM {
        return Ok(());
    }
    let pid = ThreadState::get_calling_pid();
    let permission: Strong<dyn IPermissionController> = binder::wait_for_interface("permission")?;
    for name in permissions {
        if permission.checkPermission(name, pid, uid as i32)? {
            return Ok(());
        }
    }
    Err(Status::new_exception_str(
        ExceptionCode::SECURITY,
        Some(format!("uid {uid} does not hold {}", permissions.join(" or "))),
    ))
}

fn refused(refused: Refused) -> Status {
    let code = match refused {
        Refused::Unavailable(_) => ERROR_UNAVAILABLE,
        Refused::Busy(_) => ERROR_BUSY,
        Refused::ReclaimPending(_) => ERROR_RECLAIM_PENDING,
    };
    Status::new_service_specific_error_str(code, Some(refused.to_string()))
}

impl<B: Backend> IXKernelVmControl for ControlService<B> {
    fn getStatus(&self) -> BinderResult<VmStatus> {
        check_caller()?;
        Ok(self.callbacks.status(&self.controller.status()))
    }

    fn requestStart(&self) -> BinderResult<i64> {
        check_caller()?;
        info!("requestStart from uid {}", ThreadState::get_calling_uid());
        self.controller.request_start().map(|op| op as i64).map_err(refused)
    }

    fn requestStop(&self) -> BinderResult<i64> {
        check_caller()?;
        info!("requestStop from uid {}", ThreadState::get_calling_uid());
        Ok(self.controller.request_stop() as i64)
    }

    fn registerCallback(
        &self,
        callback: &Strong<dyn IXKernelVmCallback>,
    ) -> BinderResult<VmStatus> {
        check_caller()?;
        let controller = &self.controller;
        // Publishing takes the callbacks lock, so availability is refreshed before it is held.
        controller.refresh();
        self.callbacks.register(callback, || controller.snapshot())
    }

    fn unregisterCallback(&self, callback: &Strong<dyn IXKernelVmCallback>) -> BinderResult<()> {
        check_caller()?;
        self.callbacks.unregister(callback);
        Ok(())
    }

    fn attachConsole(
        &self,
        callback: &Strong<dyn IXKernelVmConsoleCallback>,
    ) -> BinderResult<ConsoleOutput> {
        check_caller()?;
        self.console.attach(callback)
    }

    fn detachConsole(&self, callback: &Strong<dyn IXKernelVmConsoleCallback>) -> BinderResult<()> {
        check_caller()?;
        self.console.detach(callback);
        Ok(())
    }

    fn writeConsole(&self, data: &[u8]) -> BinderResult<()> {
        check_caller()?;
        self.console.write(data)
    }
}

/// The `IXKernelVmMonitor` implementation. Read-only: it never changes the VM, and it takes only
/// the controller's short-held locks.
pub struct MonitorService<B: Backend> {
    controller: Controller<B>,
    el2: Arc<El2Monitor>,
    processes: VmProcesses,
    proc_root: PathBuf,
    ticks_per_sec: u64,
}

impl<B: Backend> MonitorService<B> {
    pub fn new(
        controller: Controller<B>,
        el2: Arc<El2Monitor>,
        processes: VmProcesses,
        proc_root: PathBuf,
    ) -> Self {
        Self { controller, el2, processes, proc_root, ticks_per_sec: monitor::ticks_per_sec() }
    }
}

impl<B: Backend> Interface for MonitorService<B> {}

fn check_reader() -> BinderResult<()> {
    check_permission(&[READ_STATS_PERMISSION, CONTROL_PERMISSION])
}

impl<B: Backend> IXKernelVmMonitor for MonitorService<B> {
    fn getEl2Counters(&self) -> BinderResult<El2Counters> {
        check_reader()?;
        Ok(match self.el2.read() {
            El2Reading::Counting(samples) => El2Counters {
                status: EL2_COUNTING,
                reason: String::new(),
                samples: samples
                    .iter()
                    .map(|s| El2CycleSample {
                        cpu: s.cpu as i32,
                        cycles: s.cycles as i64,
                        timestampNs: s.timestamp_ns as i64,
                    })
                    .collect(),
            },
            El2Reading::Unavailable(reason) => {
                El2Counters { status: EL2_UNAVAILABLE, reason, samples: Vec::new() }
            }
        })
    }

    fn getVmCpuTime(&self) -> BinderResult<VmCpuTime> {
        check_reader()?;
        // Recorded processes belong to the latest instance: a new start needs the last one
        // confirmed gone, which clears them.
        let instance = self.controller.snapshot().generation;
        let processes = self.processes.get(instance, &self.controller.processes());
        Ok(match monitor::vm_cpu_time(&self.proc_root, &processes, self.ticks_per_sec) {
            Some(cpu_time_ns) => VmCpuTime {
                running: true,
                instance: instance as i64,
                cpuTimeNs: cpu_time_ns as i64,
                timestampNs: monitor::monotonic_ns() as i64,
            },
            None => VmCpuTime::default(),
        })
    }
}
