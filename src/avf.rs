// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The controller's backend on AVF.
//!
//! The first start spawns this service's virtualization manager (`vmclient`); later starts reuse
//! it, as every AVF client process does. It cannot be released per VM: once a client has
//! registered a VM callback, the RPC sessions between the two keep each other open until the
//! client process exits, so dropping the client's handle leaves an idle manager behind (measured
//! on the tablet 2026-10-15: one per start, about 10 MB each). An idle manager holds no VM
//! resources. A manager that died is replaced at the next start.
//!
//! Each start creates the VM from the fixed instance config as a raw config and starts it. The
//! returned instance holds the VM; dropping it releases the VM's AVF objects.
//!
//! A watcher thread waits for the VM's death. `vmclient` reports both AVF's death callback and the
//! death of the manager itself through the same wait, so one hook covers every way the VM ends.
//!
//! The serial console goes to this service's [`Console`] (crosvm gets the guest ends of its pipes;
//! the console of an instance ends with its death). The guest's log device goes to logcat
//! through the manager, which reads it when no file is given.

use crate::config::VmConfig;
use crate::console::Console;
use crate::controller::{Backend, Instance};
use crate::machine::Exit;
use android_system_virtualizationservice::aidl::android::system::virtualizationservice::{
    CpuOptions::{CpuOptions, CpuTopology::CpuTopology},
    DiskImage::DiskImage,
    IVirtualizationService::IVirtualizationService,
    VirtualMachineConfig::VirtualMachineConfig,
    VirtualMachineRawConfig::VirtualMachineRawConfig,
};
use android_system_virtualizationservice::binder::{IBinder, ParcelFileDescriptor, Strong};
use anyhow::{Context, Result};
use log::{info, warn};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use vmclient::{DeathReason, VirtualizationService, VmInstance};

/// The platform version this service's raw config is written for.
const PLATFORM_VERSION: &str = "~1.0";
const INSTANCE_ID_FILE: &str = "instance_id";
const INSTANCE_ID_SIZE: usize = 64;

pub struct AvfBackend {
    config: VmConfig,
    instance_id: [u8; INSTANCE_ID_SIZE],
    manager: Mutex<Option<Manager>>,
    console: Arc<Console>,
}

/// This service's virtualization manager and the connection to it.
struct Manager {
    connection: Strong<dyn IVirtualizationService>,
    _process: VirtualizationService,
}

pub struct AvfInstance {
    vm: Arc<VmInstance>,
}

impl AvfBackend {
    pub fn new(config: VmConfig, console: Arc<Console>) -> Result<Self> {
        let instance_id = load_or_create_instance_id(&config.state_dir)?;
        Ok(Self { config, instance_id, manager: Mutex::new(None), console })
    }

    /// The connection to the virtualization manager, spawning it on first use or after it died.
    fn connection(&self) -> Result<Strong<dyn IVirtualizationService>, String> {
        let mut manager = self.manager.lock().unwrap();
        if let Some(m) = manager.as_ref() {
            if m.connection.as_binder().ping_binder().is_ok() {
                return Ok(m.connection.clone());
            }
            warn!("the virtualization manager died; spawning a new one");
        }
        *manager = None;
        let process = VirtualizationService::new()
            .map_err(|e| format!("can't spawn the virtualization manager: {e}"))?;
        let connection = process
            .connect()
            .map_err(|e| format!("can't connect to the virtualization manager: {e}"))?;
        *manager = Some(Manager { connection: connection.clone(), _process: process });
        Ok(connection)
    }

    fn raw_config(&self) -> Result<VirtualMachineRawConfig> {
        let config = &self.config;
        let mut disks = Vec::new();
        for disk in &config.disks {
            if let Some(size_mib) = disk.create_size_mib {
                create_sparse_if_missing(&disk.path, size_mib)?;
            }
            disks.push(DiskImage {
                image: Some(open_pfd(&disk.path, disk.writable)?),
                writable: disk.writable,
                partitions: Vec::new(),
            });
        }
        Ok(VirtualMachineRawConfig {
            name: config.name.clone(),
            instanceId: self.instance_id,
            kernel: Some(open_pfd(&config.kernel, false)?),
            params: config.params.clone(),
            disks,
            protectedVm: config.protection.is_protected(),
            memoryMib: config.memory_mib.try_into().context("memory_mib")?,
            cpuOptions: CpuOptions {
                cpuTopology: CpuTopology::CpuCount(config.cpus.try_into().context("cpus")?),
            },
            platformVersion: PLATFORM_VERSION.to_owned(),
            consoleInputDevice: config.console_input_device.clone(),
            ..Default::default()
        })
    }
}

impl Backend for AvfBackend {
    type Instance = AvfInstance;

    fn create_and_start(
        &self,
        generation: u64,
        on_death: Box<dyn FnOnce(Exit) + Send>,
    ) -> Result<AvfInstance, String> {
        let raw = self.raw_config().map_err(|e| format!("{e:#}"))?;
        let connection = self.connection()?;
        let console =
            self.console.begin(generation).map_err(|e| format!("can't open the console: {e}"))?;
        // The guest ends go to the manager; this process keeps none of them.
        let vm = VmInstance::create(
            connection.as_ref(),
            &VirtualMachineConfig::RawConfig(raw),
            Some(console.output),
            Some(console.input),
            /* log= */ None,
            /* dump_dt= */ None,
        )
        .map_err(|e| format!("createVm: {e}"));
        let vm = match vm {
            Ok(vm) => Arc::new(vm),
            Err(e) => {
                self.console.end(generation);
                return Err(e);
            }
        };
        if let Err(e) = vm.start(None) {
            self.console.end(generation);
            return Err(format!("start: {e}"));
        }
        info!("started {} with CID {}", self.config.name, vm.cid());

        let watcher = vm.clone();
        let console = self.console.clone();
        thread::Builder::new()
            .name("xkvm-death-watch".into())
            .spawn(move || {
                let reason = watcher.wait_for_death();
                info!("VM died: {reason:?}");
                console.end(generation);
                drop(watcher);
                on_death(exit_of(reason));
            })
            .map_err(|e| format!("can't spawn the death watcher: {e}"))?;
        Ok(AvfInstance { vm })
    }
}

impl Instance for AvfInstance {
    fn cid(&self) -> i32 {
        self.vm.cid()
    }

    fn stop(&self) -> Result<(), String> {
        self.vm.stop().map_err(|e| format!("stop: {e}"))
    }

    fn probe_ready(&self, port: u32) -> bool {
        self.vm.connect_vsock(port).is_ok()
    }
}

/// How the controller sees AVF's death reason. A kill is what a requested stop produces; the
/// machine treats it as a failure only when no stop was asked for.
fn exit_of(reason: DeathReason) -> Exit {
    match reason {
        DeathReason::Shutdown => Exit::GuestShutdown,
        DeathReason::Killed => Exit::Killed,
        other => Exit::Abnormal(format!("{other:?}")),
    }
}

fn open_pfd(path: &Path, writable: bool) -> Result<ParcelFileDescriptor> {
    let file = OpenOptions::new()
        .read(true)
        .write(writable)
        .open(path)
        .with_context(|| format!("Can't open {}", path.display()))?;
    Ok(ParcelFileDescriptor::new(file))
}

fn create_sparse_if_missing(path: &Path, size_mib: u64) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("Can't create {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("Can't create {}", path.display()))?;
    file.set_len(size_mib * 1024 * 1024)?;
    info!("created sparse {} ({size_mib} MiB)", path.display());
    Ok(())
}

/// The instance ID is persistent per instance; it is random, as nothing derives a secret from it
/// without firmware.
fn load_or_create_instance_id(state_dir: &Path) -> Result<[u8; INSTANCE_ID_SIZE]> {
    let path = state_dir.join(INSTANCE_ID_FILE);
    let mut id = [0u8; INSTANCE_ID_SIZE];
    if let Ok(mut file) = File::open(&path) {
        if file.read_exact(&mut id).is_ok() {
            return Ok(id);
        }
        warn!("{} is damaged; creating a new instance ID", path.display());
    }
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut id))
        .context("Can't read /dev/urandom")?;
    fs::create_dir_all(state_dir)
        .with_context(|| format!("Can't create {}", state_dir.display()))?;
    File::create(&path)
        .and_then(|mut f| f.write_all(&id))
        .with_context(|| format!("Can't write {}", path.display()))?;
    Ok(id)
}
