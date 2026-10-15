// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! `xkernel_vmctl`: a shell client of `IXKernelVmControl`, for tests and diagnosis.
//!
//! ```text
//! xkernel_vmctl status
//! xkernel_vmctl start [--wait SECONDS]   # waits for RUNNING_NOT_READY/READY or a failure
//! xkernel_vmctl stop  [--wait SECONDS]   # waits for STOPPED/FAILED/RECLAIM_UNKNOWN
//! xkernel_vmctl watch [--wait SECONDS]   # prints every change
//! xkernel_vmctl console [--new-only] [--send TEXT [--raw]] [--follow SECONDS]
//! ```
//!
//! `console` prints the serial console output the service kept (unless `--new-only`), then types
//! TEXT into the guest (with a newline unless `--raw`), then prints new output for SECONDS.
//!
//! The exit status is 0 when the awaited state is reached (for `console`: the input was taken)
//! and 1 otherwise, so scripts can rely on it.

use anyhow::{bail, Context, Result};
use binder::{BinderFeatures, Interface, ProcessState, Result as BinderResult, Strong};
use clap::{Parser, Subcommand};
use com_kylinos_xkernelvm::aidl::com::kylinos::xkernelvm::{
    ConsoleOutput::ConsoleOutput,
    IXKernelVmCallback::{BnXKernelVmCallback, IXKernelVmCallback},
    IXKernelVmConsoleCallback::{BnXKernelVmConsoleCallback, IXKernelVmConsoleCallback},
    IXKernelVmControl::IXKernelVmControl,
    VmState::VmState,
    VmStatus::VmStatus,
};
use std::io::Write;
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
    Console {
        /// Skip the output kept from before.
        #[arg(long)]
        new_only: bool,
        /// Text to type into the guest.
        #[arg(long)]
        send: Option<String>,
        /// Do not add a newline to the text.
        #[arg(long)]
        raw: bool,
        /// Seconds to print new output for.
        #[arg(long, default_value_t = 0)]
        follow: u64,
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

struct ConsoleCallback(Mutex<Sender<ConsoleOutput>>);

impl Interface for ConsoleCallback {}

impl IXKernelVmConsoleCallback for ConsoleCallback {
    fn onConsoleOutput(&self, output: &ConsoleOutput) -> BinderResult<()> {
        let _ = self.0.lock().unwrap().send(output.clone());
        Ok(())
    }
}

/// Prints the console as `console` in the module documentation describes.
fn console(
    service: &Strong<dyn IXKernelVmControl>,
    new_only: bool,
    send: Option<String>,
    raw: bool,
    follow: Duration,
) -> Result<bool> {
    let (tx, rx) = channel();
    let callback = BnXKernelVmConsoleCallback::new_binder(
        ConsoleCallback(Mutex::new(tx)),
        BinderFeatures::default(),
    );
    let history = service.attachConsole(&callback).context("attachConsole")?;
    let mut out = std::io::stdout().lock();
    if !new_only {
        out.write_all(&history.data)?;
    }
    out.flush()?;
    let mut instance = history.instance;
    let mut end = history.offset + history.data.len() as i64;
    let mut taken = true;
    if let Some(text) = send {
        let mut bytes = text.into_bytes();
        if !raw {
            bytes.push(b'\n');
        }
        if let Err(e) = service.writeConsole(&bytes) {
            eprintln!("input not taken: {e}");
            taken = false;
        }
    }
    let deadline = Instant::now() + follow;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Ok(piece) = rx.recv_timeout(left) else { break };
        if piece.instance != instance {
            if piece.offset != 0 {
                continue;
            }
            instance = piece.instance;
            end = 0;
            writeln!(out, "\n--- console of instance {instance}")?;
        }
        let piece_end = piece.offset + piece.data.len() as i64;
        if piece_end <= end {
            continue;
        }
        if piece.offset > end {
            writeln!(out, "\n--- {} bytes of console output lost", piece.offset - end)?;
            end = piece.offset;
        }
        out.write_all(&piece.data[(end - piece.offset) as usize..])?;
        out.flush()?;
        end = piece_end;
    }
    let _ = service.detachConsole(&callback);
    Ok(taken)
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
        Command::Console { new_only, send, raw, follow } => {
            console(&service, new_only, send, raw, Duration::from_secs(follow))?
        }
    };
    std::process::exit(if ok { 0 } else { 1 });
}
