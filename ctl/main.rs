// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! `xkernel_vmctl`: a shell client of `IXKernelVmControl`, for tests and diagnosis.
//!
//! ```text
//! xkernel_vmctl status
//! xkernel_vmctl start [--wait SECONDS]   # waits for RUNNING_NOT_READY/READY or a failure
//! xkernel_vmctl stop  [--wait SECONDS]   # waits for STOPPED/FAILED/RECLAIM_UNKNOWN
//! xkernel_vmctl watch [--wait SECONDS]   # prints every change
//! ```
//!
//! The exit status is 0 when the awaited state is reached and 1 otherwise, so scripts can rely
//! on it.

use anyhow::{bail, Context, Result};
use binder::{BinderFeatures, Interface, ProcessState, Result as BinderResult, Strong};
use clap::{Parser, Subcommand};
use com_kylinos_xkernelvm::aidl::com::kylinos::xkernelvm::{
    IXKernelVmCallback::{BnXKernelVmCallback, IXKernelVmCallback},
    IXKernelVmControl::IXKernelVmControl,
    VmState::VmState,
    VmStatus::VmStatus,
};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SERVICE_NAME: &str = "com.kylinos.xkernelvm.IXKernelVmControl/default";

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status,
    Start {
        #[arg(long, default_value_t = 60)]
        wait: u64,
    },
    Stop {
        #[arg(long, default_value_t = 60)]
        wait: u64,
    },
    Watch {
        #[arg(long, default_value_t = 3600)]
        wait: u64,
    },
}

struct Callback(Mutex<Sender<VmStatus>>);

impl Interface for Callback {}

impl IXKernelVmCallback for Callback {
    fn onStatusChanged(&self, status: &VmStatus) -> BinderResult<()> {
        let _ = self.0.lock().unwrap().send(status.clone());
        Ok(())
    }
}

fn state_name(state: VmState) -> String {
    // The generated Debug spells the constant as a raw identifier.
    format!("{state:?}").trim_start_matches("r#").to_owned()
}

fn print(status: &VmStatus) {
    let uptime = if status.startedAtElapsedMs > 0 {
        let now = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME)
            .map(|t| t.tv_sec() * 1000 + t.tv_nsec() / 1_000_000)
            .unwrap_or(0);
        format!(" uptime={}s", (now - status.startedAtElapsedMs).max(0) / 1000)
    } else {
        String::new()
    };
    println!(
        "state={} seq={} op={} instance={} cid={} reclaim_confirmed={} protected={} without_firmware={} \
         dice={} memory_mib={} cpus={} readiness={}{} reason={:?} last_exit={:?} session={}",
        state_name(status.state),
        status.seq,
        status.operationId,
        status.instance,
        status.cid,
        status.reclaimConfirmed,
        status.protectedVm,
        status.withoutFirmware,
        status.diceAvailable,
        status.memoryMib,
        status.cpuCount,
        status.readinessConfigured,
        uptime,
        status.reason,
        status.lastExit,
        status.sessionId,
    );
}

/// Prints changes until `done` says the state is final; returns whether it was `wanted`.
fn follow(
    service: &Strong<dyn IXKernelVmControl>,
    wait: Duration,
    request: impl FnOnce() -> BinderResult<i64>,
    done: impl Fn(VmState) -> Option<bool>,
) -> Result<bool> {
    let (tx, rx) = channel();
    let callback =
        BnXKernelVmCallback::new_binder(Callback(Mutex::new(tx)), BinderFeatures::default());
    let initial = service.registerCallback(&callback).context("registerCallback")?;
    let mut seq = initial.seq;
    let session = initial.sessionId.clone();
    print(&initial);
    let operation = request().context("request")?;
    println!("operation={operation}");
    let deadline = Instant::now() + wait;
    let mut last = initial;
    loop {
        // The request may have left the state final already (e.g. a refused start).
        if let Some(result) = done(last.state) {
            if last.seq > 0 && (operation == 0 || last.operationId >= operation) {
                let _ = service.unregisterCallback(&callback);
                return Ok(result);
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let _ = service.unregisterCallback(&callback);
            println!("timed out");
            return Ok(false);
        }
        match rx.recv_timeout(left) {
            Ok(status) => {
                if status.sessionId != session {
                    bail!("the control service restarted");
                }
                if status.seq <= seq {
                    continue;
                }
                seq = status.seq;
                print(&status);
                last = status;
            }
            Err(_) => continue,
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    ProcessState::start_thread_pool();
    let service: Strong<dyn IXKernelVmControl> =
        binder::wait_for_interface(SERVICE_NAME).context("control service not found")?;
    let ok = match args.command {
        Command::Status => {
            print(&service.getStatus()?);
            true
        }
        Command::Start { wait } => follow(
            &service,
            Duration::from_secs(wait),
            || service.requestStart(),
            |s| match s {
                VmState::RUNNING_NOT_READY | VmState::READY => Some(true),
                VmState::FAILED | VmState::UNAVAILABLE | VmState::RECLAIM_UNKNOWN => Some(false),
                _ => None,
            },
        )?,
        Command::Stop { wait } => follow(
            &service,
            Duration::from_secs(wait),
            || service.requestStop(),
            |s| match s {
                VmState::STOPPED | VmState::UNAVAILABLE | VmState::FAILED => Some(true),
                VmState::RECLAIM_UNKNOWN => Some(false),
                _ => None,
            },
        )?,
        Command::Watch { wait } => follow(&service, Duration::from_secs(wait), || Ok(0), |_| None)?,
    };
    std::process::exit(if ok { 0 } else { 1 });
}
