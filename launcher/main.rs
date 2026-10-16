// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Starts crosvm for the control service's direct backend so that it dies with the service.
//!
//! The service arms a parent-death signal (SIGKILL) on the child it forks for crosvm, but the
//! kernel clears that signal when SELinux moves an exec into another domain, and crosvm must run
//! in the crosvm domain (platform policy lets only that domain open the hypervisor's devices).
//! This program is the entry point into that domain: it arms the signal again, checks that its
//! parent is still the service (otherwise the service died before the signal was armed and
//! nobody would kill crosvm), and then execs crosvm in the same domain, which keeps the signal.
//!
//! Usage: `xkernel_crosvm_launcher <service-pid> <crosvm> <argv0> [crosvm arguments...]`.
//! crosvm runs with `argv0` as argv[0] (`crosvm_<name>`, which the reclaim check looks for) and
//! inherits the launcher's file descriptors (the console pipes and the log).

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(parent), Some(crosvm), Some(argv0)) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: xkernel_crosvm_launcher <service-pid> <crosvm> <argv0> [args...]");
        return ExitCode::from(2);
    };
    let Some(parent) = parent.to_str().and_then(|p| p.parse::<libc::pid_t>().ok()) else {
        eprintln!("xkernel_crosvm_launcher: bad service pid {parent:?}");
        return ExitCode::from(2);
    };
    if let Err(e) = arm_parent_death_signal() {
        eprintln!("xkernel_crosvm_launcher: PR_SET_PDEATHSIG: {e}");
        return ExitCode::from(1);
    }
    // After arming: if the parent changed, the service is already gone and the signal was never
    // going to come.
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } != parent {
        eprintln!(
            "xkernel_crosvm_launcher: the service (pid {parent}) is gone; not starting crosvm"
        );
        return ExitCode::from(1);
    }
    let rest: Vec<OsString> = args.collect();
    let error = Command::new(&crosvm).arg0(&argv0).args(&rest).exec();
    eprintln!("xkernel_crosvm_launcher: cannot run {crosvm:?}: {error}");
    ExitCode::from(127)
}

/// SIGKILL when the parent dies.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn arm_parent_death_signal() -> std::io::Result<()> {
    // SAFETY: prctl with these arguments only changes this process's parent-death signal.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Only Linux has the signal; the host tests build this file elsewhere too.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn arm_parent_death_signal() -> std::io::Result<()> {
    Err(std::io::Error::other("not supported on this system"))
}
