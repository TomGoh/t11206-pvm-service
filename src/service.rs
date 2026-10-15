// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The Binder face of the controller: `IXKernelVmControl`.
//!
//! Callers must be uid 0 or 1000, or hold [`CONTROL_PERMISSION`] (a signature permission the
//! control app declares). Callbacks get every snapshot the controller publishes; registering one
//! returns the snapshot it starts from, under the same lock that publishing takes, so no change
//! falls between the two. A callback whose process dies is dropped.

use crate::config::{Protection, VmConfig};
use crate::controller::{Backend, Controller};
use crate::machine::{Refused, Snapshot, VmState};
use android_os_permissions_aidl::aidl::android::os::IPermissionController::IPermissionController;
use binder::{
    DeathRecipient, ExceptionCode, IBinder, Interface, Result as BinderResult, Status, Strong,
    ThreadState,
};
use com_kylinos_xkernelvm::aidl::com::kylinos::xkernelvm::{
    IXKernelVmCallback::IXKernelVmCallback,
    IXKernelVmControl::{IXKernelVmControl, ERROR_BUSY, ERROR_RECLAIM_PENDING, ERROR_UNAVAILABLE},
    VmState::VmState as AidlVmState,
    VmStatus::VmStatus,
};
use log::{info, warn};
use std::sync::{Arc, Mutex, Weak};

/// The servicemanager name of the control interface.
pub const SERVICE_NAME: &str = "com.kylinos.xkernelvm.IXKernelVmControl/default";
/// The permission that lets an app use the control interface.
pub const CONTROL_PERMISSION: &str = "com.kylinos.xkernelvm.permission.CONTROL_VM";

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
}

impl<B: Backend> ControlService<B> {
    pub fn new(controller: Controller<B>, callbacks: Arc<Callbacks>) -> Self {
        Self { controller, callbacks }
    }
}

impl<B: Backend> Interface for ControlService<B> {}

fn check_caller() -> BinderResult<()> {
    let uid = ThreadState::get_calling_uid();
    if uid == AID_ROOT || uid == AID_SYSTEM {
        return Ok(());
    }
    let pid = ThreadState::get_calling_pid();
    let permission: Strong<dyn IPermissionController> = binder::wait_for_interface("permission")?;
    if permission.checkPermission(CONTROL_PERMISSION, pid, uid as i32)? {
        Ok(())
    } else {
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("uid {uid} does not hold {CONTROL_PERMISSION}")),
        ))
    }
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
}
