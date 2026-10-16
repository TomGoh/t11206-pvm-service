// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The controller's backend without AVF: this service runs crosvm itself.
//!
//! For products that may add services and apps but not change AVF. Everything above the backend
//! (the lifecycle machine, the reclaim check, the console, the Binder interface) is the same as
//! with AVF; only who starts and stops crosvm differs.
//!
//! The command line is the one virtmgr builds for this instance (measured on the T11206 tablet on
//! 2026-10-15, record `t11206-avf-nofw-impl-20261015`): the same devices, memory layout and kernel
//! parameters, so the guest sees the same machine either way. Left out are what only AVF has:
//! its device-tree overlay, its log and failure pipes (sinks here) and its CID allocation (the
//! CID is configured, below AVF's range).
//!
//! The process: crosvm is a child of this service, with argv[0] `crosvm_<name>` as under AVF, so
//! the reclaim check finds it the same way. When this service dies its VM must die too, as AVF's
//! VM dies with its virtualization manager. On a product init does that: it kills the process
//! group of a service that exits, crosvm included. The parent-death signal (SIGKILL) covers a
//! service started some other way, but the kernel clears it when SELinux moves the exec into
//! another domain, so under the product policy (crosvm runs in the crosvm domain) it only holds
//! without that transition (tablet trial, 2026-10-16). That signal follows the thread that
//! forked, so crosvm is only ever started from the controller's executor thread, which lives as
//! long as the service. Killing and reaping the child happen under one lock, so a kill never
//! reaches a reused PID.
//!
//! The console: crosvm's stdin and stdout are the guest ends of the console pipes, and its serial
//! devices open them through `/proc/self/fd/{0,1}`, as they open AVF's pipes. crosvm's own log
//! (stderr) goes to `state_dir/crosvm.log`, rewritten at each start and headed by the command
//! line.
//!
//! Stopping: the service asks crosvm to exit through its control socket (`crosvm stop`). If that
//! fails, or crosvm is still there after `kill_after_seconds`, the service kills it. Dropping the
//! instance (the reclaim check does) kills a crosvm that is still there, as releasing AVF's
//! objects does.
//!
//! With `--extended-status` the exit code says how the VM ended; see [`exit_of`].

use crate::config::{DirectConfig, Protection, VmConfig};
use crate::console::Console;
use crate::controller::{Backend, Instance};
use crate::machine::Exit;
use log::{info, warn};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SOCKET_FILE: &str = "crosvm.sock";
const LOG_FILE: &str = "crosvm.log";
/// What virtmgr passes for this instance on aarch64 (protected VMs get a bounce buffer for virtio
/// and the memory size grows by it).
const SWIOTLB_MIB: u32 = 6;
const PCI_WINDOWS: &str =
    "mem=[start=0x2c000000,size=0x2000000],cam=[start=0x2e000000,size=0x1000000]";
const CONSOLE_QUEUES: &str = "max-queue-sizes=[2,32]";
/// The guest ends of the console pipes, as crosvm sees them.
const CONSOLE_OUT: &str = "/proc/self/fd/1";
const CONSOLE_IN: &str = "/proc/self/fd/0";
const STOP_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_POLL: Duration = Duration::from_millis(100);

pub struct DirectBackend {
    config: VmConfig,
    direct: DirectConfig,
    console: Arc<Console>,
}

/// The crosvm child and how it ended.
struct Process {
    state: Mutex<(Child, Option<ExitStatus>)>,
    exited: Condvar,
}

impl Process {
    /// Kills crosvm unless it has exited (and been reaped) already.
    fn kill(&self) {
        let mut state = self.state.lock().unwrap();
        if state.1.is_none() {
            if let Err(e) = state.0.kill() {
                warn!("can't kill crosvm {}: {e}", state.0.id());
            } else {
                info!("killed crosvm {}", state.0.id());
            }
        }
    }

    /// Waits up to `timeout` for crosvm to exit; returns whether it did.
    fn wait_exit(&self, timeout: Duration) -> bool {
        let state = self.state.lock().unwrap();
        let (state, _) = self.exited.wait_timeout_while(state, timeout, |s| s.1.is_none()).unwrap();
        state.1.is_some()
    }

    /// Reaps crosvm once it exits, polling so the lock is never held across a blocking wait.
    fn reap(&self) -> ExitStatus {
        loop {
            {
                let mut state = self.state.lock().unwrap();
                match state.0.try_wait() {
                    Ok(Some(status)) => {
                        state.1 = Some(status);
                        self.exited.notify_all();
                        return status;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("waiting for crosvm failed: {e}");
                        let status = ExitStatus::from_raw(libc::SIGKILL);
                        state.1 = Some(status);
                        self.exited.notify_all();
                        return status;
                    }
                }
            }
            thread::sleep(EXIT_POLL);
        }
    }
}

pub struct DirectInstance {
    process: Arc<Process>,
    crosvm: PathBuf,
    socket: PathBuf,
    cid: Option<u32>,
    kill_after: Duration,
}

impl DirectBackend {
    pub fn new(config: VmConfig, console: Arc<Console>) -> anyhow::Result<Self> {
        let direct = config
            .direct
            .clone()
            .ok_or_else(|| anyhow::anyhow!("the direct backend needs a direct section"))?;
        fs::create_dir_all(&config.state_dir)?;
        Ok(Self { config, direct, console })
    }

    fn socket(&self) -> PathBuf {
        self.config.state_dir.join(SOCKET_FILE)
    }
}

/// crosvm's arguments for the instance, after argv[0].
pub fn crosvm_args(config: &VmConfig, direct: &DirectConfig, socket: &Path) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |s: &str| a.push(s.to_owned());
    push("--extended-status");
    push("--log-level");
    push(&direct.log_level);
    push("--no-syslog");
    push("run");
    push("--disable-sandbox");
    if let Some(cid) = direct.cid {
        push("--cid");
        push(&cid.to_string());
    }
    push("--name");
    push(&config.crosvm_process_name());
    let protected = config.protection.is_protected();
    if config.protection == Protection::WithoutFirmware {
        push("--protected-vm-without-firmware");
    }
    if protected {
        push("--unmap-guest-memory-on-fork");
    }
    push(&config.kernel.to_string_lossy());
    if let Some(params) = &config.params {
        push("--params");
        push(params);
    }
    push("--params");
    push(&format!("hostname={}", config.name));
    push("--cpus");
    push(&format!("num-cores={},sve=[auto=true]", config.cpus));
    push("--pci");
    push(PCI_WINDOWS);
    push("--no-pmu");
    push("--mem");
    push(&(config.memory_mib + if protected { SWIOTLB_MIB } else { 0 }).to_string());
    if protected {
        push("--swiotlb");
        push(&SWIOTLB_MIB.to_string());
    }
    push("--params");
    push("coherent_pool=4096");
    push("--params");
    push("rodata=on");
    let input = config.console_input_device.as_deref().unwrap_or("hvc0");
    let input_on = |device: &str| {
        if input == device {
            format!(",input={CONSOLE_IN}")
        } else {
            String::new()
        }
    };
    // ttyS0 and hvc0 carry the console, as under AVF; AVF's log (hvc2), failure (second UART)
    // and ramdump (hvc1) devices stay, connected to nothing.
    let ttys0 = input_on("ttyS0");
    let hvc0 = input_on("hvc0");
    push(&format!("--serial=type=file,path={CONSOLE_OUT}{ttys0},hardware=serial,num=1"));
    push(&format!(
        "--serial=type=file,path={CONSOLE_OUT}{hvc0},hardware=virtio-console,num=1,{CONSOLE_QUEUES}"
    ));
    push(&format!("--serial=type=sink,hardware=virtio-console,num=3,{CONSOLE_QUEUES}"));
    push("--serial=type=sink,hardware=serial,num=2");
    push(&format!("--serial=type=sink,hardware=virtio-console,num=2,{CONSOLE_QUEUES}"));
    for disk in &config.disks {
        push("--block");
        push(&format!("path={},ro={},lock=false", disk.path.display(), !disk.writable));
    }
    push("--no-usb");
    push("--socket");
    push(&socket.to_string_lossy());
    a
}

/// How the controller sees crosvm's exit. With `--extended-status`, crosvm exits 0 or 33 when
/// the VM stopped (a guest power-off, or `crosvm stop`), 32 when the guest reset (x-kernel resets
/// on panic with `panic=-1`), 34 when crosvm crashed, 35 on a guest panic, 36 on bad arguments and
/// 37 on a watchdog reset. A signal is a kill: what an escalated stop does, and also an outside
/// kill; the machine treats it as a failure only when no stop was asked for. FIXME: the
/// crosvm-xhyper build in use exited 33 after a vCPU error it logged as "exiting with crash"
/// (tablet, 2026-10-16), so a stop without a request may hide a crash; crosvm.log tells.
pub fn exit_of(status: ExitStatus) -> Exit {
    if status.signal().is_some() {
        return Exit::Killed;
    }
    match status.code() {
        Some(0) | Some(33) => Exit::GuestShutdown,
        Some(32) => Exit::Abnormal("the guest reset".into()),
        Some(34) => Exit::Abnormal("crosvm crashed".into()),
        Some(35) => Exit::Abnormal("the guest panicked".into()),
        Some(36) => Exit::Abnormal("crosvm refused its arguments".into()),
        Some(37) => Exit::Abnormal("the guest watchdog reset the VM".into()),
        Some(code) => Exit::Abnormal(format!("crosvm exited with status {code}")),
        None => Exit::Abnormal(format!("crosvm ended: {status}")),
    }
}

impl Backend for DirectBackend {
    type Instance = DirectInstance;

    fn create_and_start(
        &self,
        generation: u64,
        on_death: Box<dyn FnOnce(Exit) + Send>,
    ) -> Result<DirectInstance, String> {
        for disk in &self.config.disks {
            disk.create_if_missing().map_err(|e| format!("{e:#}"))?;
        }
        let socket = self.socket();
        if socket.exists() {
            fs::remove_file(&socket)
                .map_err(|e| format!("can't remove {}: {e}", socket.display()))?;
        }
        let args = crosvm_args(&self.config, &self.direct, &socket);
        let log_path = self.config.state_dir.join(LOG_FILE);
        let mut log = File::create(&log_path)
            .map_err(|e| format!("can't create {}: {e}", log_path.display()))?;
        let _ = writeln!(log, "{} {}", self.direct.crosvm.display(), args.join(" "));

        let console =
            self.console.begin(generation).map_err(|e| format!("can't open the console: {e}"))?;
        let mut command = Command::new(&self.direct.crosvm);
        command
            .arg0(self.config.crosvm_process_name())
            .args(&args)
            .stdin(Stdio::from(console.input))
            .stdout(Stdio::from(console.output))
            .stderr(Stdio::from(log));
        set_parent_death_signal(&mut command);
        let spawned = command.spawn();
        // The command holds the guest ends; this process keeps none of them.
        drop(command);
        let child = match spawned {
            Ok(child) => child,
            Err(e) => {
                self.console.end(generation);
                return Err(format!("can't run {}: {e}", self.direct.crosvm.display()));
            }
        };
        info!("started {} as crosvm {} (log {})", self.config.name, child.id(), log_path.display());

        let process =
            Arc::new(Process { state: Mutex::new((child, None)), exited: Condvar::new() });
        let watcher = process.clone();
        let console = self.console.clone();
        let spawned = thread::Builder::new().name("xkvm-crosvm-wait".into()).spawn(move || {
            let status = watcher.reap();
            info!("crosvm exited: {status}");
            console.end(generation);
            on_death(exit_of(status));
        });
        if let Err(e) = spawned {
            process.kill();
            return Err(format!("can't spawn the exit watcher: {e}"));
        }
        Ok(DirectInstance {
            process,
            crosvm: self.direct.crosvm.clone(),
            socket,
            cid: self.direct.cid,
            kill_after: Duration::from_secs(self.direct.kill_after_seconds),
        })
    }
}

/// crosvm gets SIGKILL when the forking thread (the executor) or this process dies.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_parent_death_signal(command: &mut Command) {
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: prctl and getppid are async-signal-safe and touch no memory of the parent.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent may have died before the signal was armed.
            if libc::getppid() != parent {
                return Err(std::io::Error::other("the service died while starting crosvm"));
            }
            Ok(())
        });
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_parent_death_signal(_command: &mut Command) {}

/// Runs `crosvm stop SOCKET` and returns whether crosvm accepted it.
fn request_stop(crosvm: &Path, socket: &Path) -> Result<(), String> {
    let mut child = Command::new(crosvm)
        .arg("--no-syslog")
        .arg("stop")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("can't run crosvm stop: {e}"))?;
    let deadline = Instant::now() + STOP_COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("crosvm stop: {status}")),
            Ok(None) if Instant::now() < deadline => thread::sleep(EXIT_POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("crosvm stop did not return".into());
            }
            Err(e) => return Err(format!("crosvm stop: {e}")),
        }
    }
}

impl Instance for DirectInstance {
    fn cid(&self) -> i32 {
        self.cid.map_or(-1, |cid| cid as i32)
    }

    fn stop(&self) -> Result<(), String> {
        match request_stop(&self.crosvm, &self.socket) {
            Ok(()) => {
                let process = self.process.clone();
                let kill_after = self.kill_after;
                thread::Builder::new()
                    .name("xkvm-kill-after".into())
                    .spawn(move || {
                        if !process.wait_exit(kill_after) {
                            warn!("crosvm did not exit {kill_after:?} after the stop request");
                            process.kill();
                        }
                    })
                    .map_err(|e| format!("can't spawn the stop escalation: {e}"))?;
            }
            Err(e) => {
                warn!("{e}; killing crosvm");
                self.process.kill();
            }
        }
        Ok(())
    }

    fn probe_ready(&self, port: u32) -> bool {
        self.cid.is_some_and(|cid| vsock_connects(cid, port))
    }
}

impl Drop for DirectInstance {
    fn drop(&mut self) {
        self.process.kill();
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn vsock_connects(cid: u32, port: u32) -> bool {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SAFETY: a fresh socket owned by `fd`, and a zeroed sockaddr_vm filled in before use.
    unsafe {
        let raw = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if raw < 0 {
            return false;
        }
        let fd = OwnedFd::from_raw_fd(raw);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = cid;
        addr.svm_port = port;
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        ) == 0
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn vsock_connects(_cid: u32, _port: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::{Listener, Output, SendError};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc::{channel, Receiver, Sender};

    /// The instance of the tablet's development config, as the direct backend would run it.
    fn tablet_config(dir: &Path) -> VmConfig {
        VmConfig::parse(&format!(
            r#"{{
              "name": "xkernel_vm",
              "kernel": "/data/local/tmp/xkernel-pvm/vm/kernel.bin",
              "params": "panic=-1 console=ttyS0 loglevel=8",
              "memory_mib": 256,
              "cpus": 2,
              "protection": "without_firmware",
              "disks": [
                {{ "path": "/data/local/tmp/xkernel-pvm/vm/dummy.img", "writable": true }},
                {{ "path": "/data/local/tmp/xkernel-pvm/vm/rootfs.img", "writable": true }}
              ],
              "state_dir": "{d}",
              "console_input_device": "ttyS0",
              "backend": "direct",
              "direct": {{ "crosvm": "{d}/crosvm", "cid": 1024, "kill_after_seconds": 1 }}
            }}"#,
            d = dir.display()
        ))
        .unwrap()
    }

    #[test]
    fn command_line_is_the_one_virtmgr_builds() {
        let c = tablet_config(Path::new("/s"));
        let args = crosvm_args(&c, c.direct.as_ref().unwrap(), Path::new("/s/crosvm.sock"));
        // virtmgr's command line for this instance (board/18-smp-logcat.txt), with its file
        // descriptors replaced by paths, its device-tree overlay dropped, its log and failure
        // pipes replaced by sinks and the CID configured; plus --no-syslog and the log level.
        let want = [
            "--extended-status", "--log-level", "info", "--no-syslog", "run", "--disable-sandbox",
            "--cid", "1024", "--name", "crosvm_xkernel_vm", "--protected-vm-without-firmware",
            "--unmap-guest-memory-on-fork", "/data/local/tmp/xkernel-pvm/vm/kernel.bin",
            "--params", "panic=-1 console=ttyS0 loglevel=8", "--params", "hostname=xkernel_vm",
            "--cpus", "num-cores=2,sve=[auto=true]",
            "--pci", "mem=[start=0x2c000000,size=0x2000000],cam=[start=0x2e000000,size=0x1000000]",
            "--no-pmu", "--mem", "262", "--swiotlb", "6",
            "--params", "coherent_pool=4096", "--params", "rodata=on",
            "--serial=type=file,path=/proc/self/fd/1,input=/proc/self/fd/0,hardware=serial,num=1",
            "--serial=type=file,path=/proc/self/fd/1,hardware=virtio-console,num=1,max-queue-sizes=[2,32]",
            "--serial=type=sink,hardware=virtio-console,num=3,max-queue-sizes=[2,32]",
            "--serial=type=sink,hardware=serial,num=2",
            "--serial=type=sink,hardware=virtio-console,num=2,max-queue-sizes=[2,32]",
            "--block", "path=/data/local/tmp/xkernel-pvm/vm/dummy.img,ro=false,lock=false",
            "--block", "path=/data/local/tmp/xkernel-pvm/vm/rootfs.img,ro=false,lock=false",
            "--no-usb", "--socket", "/s/crosvm.sock",
        ];
        assert_eq!(args, want);
    }

    #[test]
    fn a_read_only_disk_and_hvc0_input() {
        let mut c = tablet_config(Path::new("/s"));
        c.disks[0].writable = false;
        c.console_input_device = Some("hvc0".into());
        let args = crosvm_args(&c, c.direct.as_ref().unwrap(), Path::new("/s/crosvm.sock"));
        assert!(args
            .contains(&"path=/data/local/tmp/xkernel-pvm/vm/dummy.img,ro=true,lock=false".into()));
        assert!(
            args.contains(&"--serial=type=file,path=/proc/self/fd/1,hardware=serial,num=1".into())
        );
        assert!(args
            .iter()
            .any(|a| a.contains("input=/proc/self/fd/0,hardware=virtio-console,num=1")));
    }

    #[test]
    fn exit_codes() {
        let code = |c: i32| exit_of(ExitStatus::from_raw(c << 8));
        assert_eq!(code(0), Exit::GuestShutdown);
        assert_eq!(code(33), Exit::GuestShutdown);
        assert_eq!(code(32), Exit::Abnormal("the guest reset".into()));
        assert_eq!(code(35), Exit::Abnormal("the guest panicked".into()));
        assert_eq!(code(1), Exit::Abnormal("crosvm exited with status 1".into()));
        assert_eq!(exit_of(ExitStatus::from_raw(libc::SIGKILL)), Exit::Killed);
    }

    // A stand-in for crosvm: `run` writes its arguments next to the socket, greets on stdout,
    // echoes stdin lines, and exits 33 once a stop request arrives (unless told to hang);
    // `stop SOCKET` files that request (or fails, if told to).
    const FAKE_CROSVM: &str = r#"#!/bin/bash
if [ "$2" = stop ]; then
    [ -e "$3.stopfail" ] && exit 1
    touch "$3.stopreq"; exit 0
fi
prev=; for a in "$@"; do [ "$prev" = --socket ] && sock=$a; prev=$a; done
printf '%s\n' "$@" > "$sock.argv"
echo "Welcome to the fake x-kernel"
while :; do
    if [ -e "$sock.stopreq" ] && [ ! -e "$sock.hang" ]; then exit 33; fi
    if read -r -t 1 line; then echo "echo: $line"; fi
done
"#;

    struct Collect(Mutex<Sender<Output>>);

    impl Listener for Collect {
        fn send(&self, output: &Output) -> Result<(), SendError> {
            self.0.lock().unwrap().send(output.clone()).map_err(|_| SendError::Gone)
        }
    }

    struct Rig {
        dir: PathBuf,
        backend: DirectBackend,
        console: Arc<Console>,
        output: Receiver<Output>,
        deaths: Receiver<Exit>,
        death_tx: Sender<Exit>,
    }

    impl Rig {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("xkvm-direct-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("crosvm"), FAKE_CROSVM).unwrap();
            fs::set_permissions(dir.join("crosvm"), fs::Permissions::from_mode(0o755)).unwrap();
            let console = Console::new();
            let (tx, output) = channel();
            console.attach(Box::new(Collect(Mutex::new(tx))));
            let backend = DirectBackend::new(tablet_config(&dir), console.clone()).unwrap();
            let (death_tx, deaths) = channel();
            Rig { dir, backend, console, output, deaths, death_tx }
        }

        fn start(&self, generation: u64) -> DirectInstance {
            let tx = Mutex::new(self.death_tx.clone());
            self.backend
                .create_and_start(
                    generation,
                    Box::new(move |exit| tx.lock().unwrap().send(exit).unwrap()),
                )
                .unwrap()
        }

        /// Console text until `needle` shows up (or 5 s pass).
        fn wait_for(&self, needle: &str) -> String {
            let mut text = String::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !text.contains(needle) {
                let left = deadline.saturating_duration_since(Instant::now());
                let Ok(o) = self.output.recv_timeout(left) else { break };
                text.push_str(&String::from_utf8_lossy(&o.data));
            }
            text
        }

        fn death(&self, within: Duration) -> Option<Exit> {
            self.deaths.recv_timeout(within).ok()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn runs_crosvm_with_the_console_and_stops_it_on_request() {
        let rig = Rig::new("stop");
        let instance = rig.start(1);
        assert!(rig.wait_for("Welcome").contains("Welcome to the fake x-kernel"));
        let argv = fs::read_to_string(rig.dir.join("crosvm.sock.argv")).unwrap();
        let c = tablet_config(&rig.dir);
        let want = crosvm_args(&c, c.direct.as_ref().unwrap(), &rig.dir.join("crosvm.sock"));
        assert_eq!(argv.lines().collect::<Vec<_>>(), want);
        rig.console.write(b"nproc\n").unwrap();
        assert!(rig.wait_for("echo: nproc").contains("echo: nproc"));
        let log = fs::read_to_string(rig.dir.join("crosvm.log")).unwrap();
        assert!(log.starts_with(&format!("{}/crosvm --extended-status", rig.dir.display())));

        instance.stop().unwrap();
        assert_eq!(rig.death(Duration::from_secs(5)), Some(Exit::GuestShutdown));
        assert!(rig.console.write(b"x").is_err(), "the console ended with crosvm");
    }

    #[test]
    fn a_crosvm_that_ignores_the_stop_request_is_killed() {
        let rig = Rig::new("hang");
        fs::write(rig.dir.join("crosvm.sock.hang"), b"").unwrap();
        let instance = rig.start(1);
        rig.wait_for("Welcome");
        let asked = Instant::now();
        instance.stop().unwrap();
        assert_eq!(rig.death(Duration::from_secs(5)), Some(Exit::Killed));
        // kill_after_seconds is 1 in this config.
        assert!(asked.elapsed() >= Duration::from_millis(900), "{:?}", asked.elapsed());
    }

    #[test]
    fn a_failed_stop_request_kills_at_once() {
        let rig = Rig::new("stopfail");
        fs::write(rig.dir.join("crosvm.sock.stopfail"), b"").unwrap();
        let instance = rig.start(1);
        rig.wait_for("Welcome");
        let asked = Instant::now();
        instance.stop().unwrap();
        assert_eq!(rig.death(Duration::from_secs(5)), Some(Exit::Killed));
        assert!(asked.elapsed() < Duration::from_millis(900), "{:?}", asked.elapsed());
    }

    #[test]
    fn dropping_the_instance_kills_crosvm() {
        let rig = Rig::new("drop");
        let instance = rig.start(1);
        rig.wait_for("Welcome");
        drop(instance);
        assert_eq!(rig.death(Duration::from_secs(5)), Some(Exit::Killed));
    }

    #[test]
    fn an_outside_kill_is_reported_and_a_restart_works() {
        let rig = Rig::new("outside");
        let instance = rig.start(1);
        rig.wait_for("Welcome");
        let pid = instance.process.state.lock().unwrap().0.id();
        // SAFETY: the child is not reaped yet (the instance is alive), so the PID is ours.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
        assert_eq!(rig.death(Duration::from_secs(5)), Some(Exit::Killed));
        drop(instance);
        let _again = rig.start(2);
        assert!(rig.wait_for("Welcome").contains("Welcome"));
    }

    /// The parent-death signal is armed: it follows the forking thread, so a crosvm started from a
    /// thread that ends is killed. (This is why only the long-lived executor starts crosvm.)
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn crosvm_dies_with_the_thread_that_started_it() {
        let rig = Rig::new("pdeathsig");
        let (backend, tx) = (&rig.backend, Mutex::new(rig.death_tx.clone()));
        let instance = thread::scope(|s| {
            s.spawn(|| {
                let on_death = Box::new(move |exit| tx.lock().unwrap().send(exit).unwrap());
                backend.create_and_start(1, on_death).unwrap()
            })
            .join()
            .unwrap()
        });
        assert_eq!(rig.death(Duration::from_secs(5)), Some(Exit::Killed));
        drop(instance);
    }

    #[test]
    fn a_missing_crosvm_fails_the_start_and_ends_the_console() {
        let rig = Rig::new("missing");
        fs::remove_file(rig.dir.join("crosvm")).unwrap();
        let tx = Mutex::new(rig.death_tx.clone());
        let r =
            rig.backend.create_and_start(1, Box::new(move |e| tx.lock().unwrap().send(e).unwrap()));
        assert!(r.err().unwrap().contains("can't run"));
        assert!(rig.console.write(b"x").is_err());
        assert_eq!(rig.death(Duration::from_millis(300)), None);
    }
}
