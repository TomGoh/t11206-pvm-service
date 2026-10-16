// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! `xkernel_vm_service`: the only owner of the x-kernel VM instance.
//!
//! It registers `IXKernelVmControl` and waits. Nothing is created until a client asks for a
//! start: no virtualization manager, no crosvm, no guest memory. The virtualization manager the
//! first start spawns stays (idle, without VM resources) for later starts; see `avf`. A started
//! VM is created through AVF from the fixed instance config (see `config`); its lifecycle is the
//! machine in `machine`, run by `controller` on the AVF backend in `avf`; the Binder interface
//! is in `service`.
//!
//! The VM belongs to this service, not to its clients: a client that exits or dies changes
//! nothing. If this service itself dies, the virtualization manager it spawned exits and takes
//! the VM with it; the next run finds anything left over before accepting a start (see
//! `controller` and `reclaim`).
//!
//! The VM's serial console is held here too (see `console`), so clients can read and type into it
//! however often they come and go.
//!
//! The instance config chooses who runs crosvm: AVF (`avf`), or this service itself (`direct`),
//! for products that may not change AVF. Everything else is the same for both.

mod avf;
mod config;
mod console;
mod controller;
mod direct;
mod driver;
mod machine;
mod reclaim;
mod service;

use crate::avf::AvfBackend;
use crate::config::{BackendKind, VmConfig, VIRTMGR_PATH};
use crate::console::Console;
use crate::controller::{Backend, Controller, Settings};
use crate::direct::DirectBackend;
use crate::driver::{DriverLoader, WithDriver};
use crate::service::{Callbacks, ConsoleClients, ControlService, SERVICE_NAME};
use anyhow::{Context, Result};
use binder::{BinderFeatures, ProcessState};
use clap::Parser;
use com_kylinos_xkernelvm::aidl::com::kylinos::xkernelvm::IXKernelVmControl::BnXKernelVmControl;
use log::{error, info, LevelFilter};
use nix::time::{clock_gettime, ClockId};
use rustutils::system_properties;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_CONFIG: &str = "/system_ext/etc/xkernel_vm/xkernel_vm.json";

#[derive(Parser)]
#[command(about = "Owns the x-kernel VM instance and serves IXKernelVmControl.")]
struct Args {
    /// The instance config.
    #[arg(long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,
}

/// CLOCK_BOOTTIME in milliseconds, the clock of SystemClock.elapsedRealtime().
fn boot_time_ms() -> u64 {
    clock_gettime(ClockId::CLOCK_BOOTTIME)
        .map(|t| t.tv_sec() as u64 * 1000 + t.tv_nsec() as u64 / 1_000_000)
        .unwrap_or(0)
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn try_main() -> Result<()> {
    let args = Args::parse();
    let config = VmConfig::load(&args.config)?;
    ProcessState::start_thread_pool();
    info!("instance config {}: {config:?}", args.config.display());

    let console = Console::new();
    // Loads the hypervisor driver on a start that finds its device node missing.
    let driver = config.driver.as_ref().map(|d| {
        DriverLoader::new(
            config.hypervisor_device.clone(),
            Duration::from_secs(d.timeout_seconds),
            Box::new(|name, value| system_properties::write(name, value).map_err(|e| format!("{e:#}"))),
            Box::new(|name| system_properties::read(name).ok().flatten()),
        )
    });
    match config.backend {
        BackendKind::Avf => {
            let backend = AvfBackend::new(config.clone(), console.clone())
                .context("Can't prepare the AVF backend")?;
            serve(config, WithDriver::new(backend, driver), console)
        }
        BackendKind::Direct => {
            let backend = DirectBackend::new(config.clone(), console.clone())
                .context("Can't prepare the direct backend")?;
            serve(config, WithDriver::new(backend, driver), console)
        }
    }
}

fn serve<B: Backend>(config: VmConfig, backend: B, console: std::sync::Arc<Console>) -> Result<()> {
    let boot_id = boot_id();
    let session_id = format!("{boot_id}/{}/{}", std::process::id(), boot_time_ms());
    let callbacks = Callbacks::new(&config, session_id);

    let settings = Settings {
        process_name: config.crosvm_process_name(),
        state_dir: config.state_dir.clone(),
        proc_root: PathBuf::from("/proc"),
        boot_id,
        stop_timeout: Duration::from_secs(config.stop_timeout_seconds),
        reclaim_window: Duration::from_secs(config.reclaim_window_seconds),
        readiness: config
            .readiness
            .as_ref()
            .map(|r| (r.vsock_port, Duration::from_secs(r.timeout_seconds))),
    };
    let preflight = {
        let config = config.clone();
        Box::new(move || {
            config.preflight(Path::new(VIRTMGR_PATH), &|name| {
                system_properties::read(name).ok().flatten()
            })
        })
    };
    let publish = {
        let callbacks = callbacks.clone();
        Box::new(move |snapshot| callbacks.publish(snapshot))
    };
    let controller = Controller::new(backend, settings, preflight, publish, Box::new(boot_time_ms));
    info!("initial status: {:?}", controller.status());

    let service = BnXKernelVmControl::new_binder(
        ControlService::new(controller, callbacks, ConsoleClients::new(console)),
        BinderFeatures::default(),
    );
    binder::add_service(SERVICE_NAME, service.as_binder())
        .with_context(|| format!("Can't register {SERVICE_NAME}"))?;
    info!("registered {SERVICE_NAME}");
    ProcessState::join_thread_pool();
    anyhow::bail!("the Binder thread pool ended")
}

fn main() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("xkernel_vm_service")
            .with_max_level(LevelFilter::Info),
    );
    if let Err(e) = try_main() {
        error!("{e:#}");
        std::process::exit(1);
    }
}
