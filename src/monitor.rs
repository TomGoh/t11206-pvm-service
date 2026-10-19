// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The figures of the read-only monitoring interface (`IXKernelVmMonitor`, served in `service`):
//! the hypervisor's EL2 cycles per CPU and the CPU time of the x-kernel VM. Both are cumulative;
//! the client computes rates from two samples, so this side keeps no per-client state.
//!
//! **EL2 cycles** come from the host driver's [`XCORE_STATS`]. Writing `1` starts the hypervisor's
//! per-CPU cycle counters (counting EL2 only), reading lists `cpu_id cycles timestamp_ns` for each
//! online CPU whose counter is readable, writing `0` stops them and gives the PMU back to the Host
//! (while they run, the Host cannot count with simpleperf). [`El2Monitor`]:
//!
//! - starts the counters on the first read, and loads the driver first if the file is missing
//!   (the driver is loaded on demand, as for a VM start);
//! - writes `1` again when a CPU's line goes missing or the read fails, at most every
//!   [`RESTART_INTERVAL`]. The driver re-applies the policy only to CPUs in a power transition or
//!   with an error, so this is cheap, unlike writing `1` on every refresh, which also races with
//!   CPUs entering deep idle;
//! - stops the counters once no client has read for [`IDLE_STOP`] ([`El2Monitor::stop_if_idle`],
//!   run by a thread), and at service start ([`El2Monitor::reset`]) in case an earlier run of
//!   the service left them running.
//!
//! The cycles are counted at the CPU's clock of the moment; turning them into time needs the
//! frequency the CPU ran at, which the client takes from cpufreq's `time_in_state`.
//!
//! **VM CPU time** is utime + stime of the VM's crosvm processes (`/proc/<pid>/stat`, every
//! thread, [`vm_cpu_time`]), each counted only while its start time still matches. The processes
//! ([`VmProcesses`]) are those the controller recorded at the start plus every process whose
//! argv[0] is `crosvm_<name>`, rescanned at most every [`PROCESS_RESCAN`]: the record alone misses
//! them on the direct backend, whose start returns before the launcher has executed crosvm, and
//! crosvm runs the vCPUs in a child it forks after the start (the parent only supervises). The
//! x-kernel's vCPUs are scheduled by host threads (crosvm asks for `affinity = "proxy"`), so the
//! guest's run time is these threads' time; it also contains the hypervisor's handling of the
//! guest's exits and crosvm's own device emulation.

use crate::reclaim::{self, TrackedProcess};
use log::{info, warn};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

/// The host driver's EL2 cycle counter file.
pub const XCORE_STATS: &str = "/proc/xcore_stats";
/// The first line of [`XCORE_STATS`].
const HEADER: &str = "cpu_id cycles timestamp_ns";
/// Counters nobody has read for this long are stopped.
pub const IDLE_STOP: Duration = Duration::from_secs(15);
/// The least time between two restarts of the counters.
pub const RESTART_INTERVAL: Duration = Duration::from_secs(5);
/// How often the idle check runs.
const IDLE_CHECK: Duration = Duration::from_secs(3);
/// How long a scan for the VM's processes is reused.
pub const PROCESS_RESCAN: Duration = Duration::from_secs(5);

/// The EL2 cycle counter of one CPU, as [`XCORE_STATS`] lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CycleSample {
    pub cpu: u32,
    pub cycles: u64,
    pub timestamp_ns: u64,
}

/// One reading of the EL2 counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum El2Reading {
    Counting(Vec<CycleSample>),
    Unavailable(String),
}

/// Parses [`XCORE_STATS`]: the header, then `cpu cycles timestamp_ns` per line.
pub fn parse_xcore_stats(text: &str) -> Result<Vec<CycleSample>, String> {
    let mut lines = text.lines();
    match lines.next() {
        Some(first) if first.trim() == HEADER => {}
        other => return Err(format!("unexpected header {other:?}")),
    }
    lines
        .filter(|l| !l.trim().is_empty())
        .map(|line| parse_sample(line).ok_or_else(|| format!("unexpected line {line:?}")))
        .collect()
}

fn parse_sample(line: &str) -> Option<CycleSample> {
    let mut fields = line.split_whitespace();
    let sample = CycleSample {
        cpu: fields.next()?.parse().ok()?,
        cycles: fields.next()?.parse().ok()?,
        timestamp_ns: fields.next()?.parse().ok()?,
    };
    fields.next().is_none().then_some(sample)
}

/// The counter file, so that tests can stand in for the driver.
pub trait StatsFile: Send + Sync {
    fn exists(&self) -> bool;
    fn read(&self) -> io::Result<String>;
    fn write(&self, value: &str) -> io::Result<()>;
}

/// [`StatsFile`] on a real path.
pub struct ProcFile(pub PathBuf);

impl StatsFile for ProcFile {
    fn exists(&self) -> bool {
        self.0.exists()
    }

    fn read(&self) -> io::Result<String> {
        fs::read_to_string(&self.0)
    }

    fn write(&self, value: &str) -> io::Result<()> {
        // One write(2) per value: the driver parses each write on its own.
        fs::write(&self.0, value)
    }
}

/// Loads the host driver, as for a start (`driver::DriverLoader::ensure_loaded`).
pub type EnsureDriver = Box<dyn Fn() -> Result<(), String> + Send + Sync>;
/// A monotonic clock; only differences matter.
pub type Clock = Box<dyn Fn() -> Duration + Send + Sync>;

#[derive(Default)]
struct State {
    /// This service has started the counters and not stopped them since.
    running: bool,
    last_read: Duration,
    last_start: Duration,
    /// The most CPUs one reading listed since the counters started.
    most_cpus: usize,
}

/// Starts, reads and stops the EL2 counters for the monitoring interface.
pub struct El2Monitor {
    file: Box<dyn StatsFile>,
    ensure_driver: Option<EnsureDriver>,
    now: Clock,
    state: Mutex<State>,
}

impl El2Monitor {
    pub fn new(file: Box<dyn StatsFile>, ensure_driver: Option<EnsureDriver>, now: Clock) -> Self {
        Self { file, ensure_driver, now, state: Mutex::new(State::default()) }
    }

    /// Stops counters an earlier run of this service may have left running.
    pub fn reset(&self) {
        if self.file.exists() {
            match self.file.write("0\n") {
                Ok(()) => info!("stopped the EL2 counters at service start"),
                Err(e) => warn!("can't stop the EL2 counters at service start: {e}"),
            }
        }
    }

    /// Reads the counters, starting them (and loading the driver) first if needed.
    pub fn read(&self) -> El2Reading {
        let mut state = self.state.lock().unwrap();
        if !self.file.exists() {
            if let Some(ensure) = &self.ensure_driver {
                if let Err(e) = ensure() {
                    return El2Reading::Unavailable(e);
                }
            }
            if !self.file.exists() {
                return El2Reading::Unavailable(format!(
                    "the hypervisor driver offers no EL2 monitor ({XCORE_STATS} is missing)"
                ));
            }
            // A driver loaded just now starts with its counters stopped.
            state.running = false;
        }
        let now = (self.now)();
        state.last_read = now;
        if !state.running {
            if let Err(e) = self.file.write("1\n") {
                return El2Reading::Unavailable(format!("can't start the EL2 counters: {e}"));
            }
            info!("started the EL2 counters");
            *state = State { running: true, last_read: now, last_start: now, most_cpus: 0 };
        }
        let reading = self
            .file
            .read()
            .map_err(|e| format!("can't read {XCORE_STATS}: {e}"))
            .and_then(|text| parse_xcore_stats(&text));
        let complete = matches!(&reading, Ok(samples) if samples.len() >= state.most_cpus);
        if !complete && now.saturating_sub(state.last_start) >= RESTART_INTERVAL {
            // Re-applies the policy to a CPU whose counter failed; does nothing for a CPU that
            // is merely offline.
            match self.file.write("1\n") {
                Ok(()) => info!("restarted the EL2 counters: {reading:?}"),
                Err(e) => warn!("can't restart the EL2 counters: {e}"),
            }
            state.last_start = now;
        }
        match reading {
            Ok(samples) => {
                state.most_cpus = state.most_cpus.max(samples.len());
                El2Reading::Counting(samples)
            }
            Err(e) => El2Reading::Unavailable(e),
        }
    }

    /// Stops the counters if nobody has read them for [`IDLE_STOP`]; returns whether it did.
    pub fn stop_if_idle(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.running || (self.now)().saturating_sub(state.last_read) < IDLE_STOP {
            return false;
        }
        state.running = false;
        if let Err(e) = self.file.write("0\n") {
            warn!("can't stop the idle EL2 counters: {e}");
        } else {
            info!("stopped the EL2 counters: no client for {}s", IDLE_STOP.as_secs());
        }
        true
    }

    /// Runs [`Self::stop_if_idle`] every few seconds for as long as the monitor exists.
    pub fn spawn_idle_stopper(self: &Arc<Self>) {
        let weak: Weak<Self> = Arc::downgrade(self);
        thread::Builder::new()
            .name("xkvm-el2-idle".into())
            .spawn(move || loop {
                thread::sleep(IDLE_CHECK);
                let Some(monitor) = weak.upgrade() else { return };
                monitor.stop_if_idle();
            })
            .expect("can't spawn the EL2 idle check");
    }
}

/// The VM's crosvm processes, for [`vm_cpu_time`].
pub struct VmProcesses {
    proc_root: PathBuf,
    /// `crosvm_<name>`, the argv[0] of every crosvm process of the VM.
    arg0: String,
    now: Clock,
    /// The generation scanned for, when, and what was found.
    cache: Mutex<Option<(u64, Duration, Vec<TrackedProcess>)>>,
}

impl VmProcesses {
    pub fn new(proc_root: PathBuf, arg0: String, now: Clock) -> Self {
        Self { proc_root, arg0, now, cache: Mutex::new(None) }
    }

    /// `recorded` (the controller's record of `generation`) plus the processes a scan finds; the
    /// scan is reused for [`PROCESS_RESCAN`] within one generation.
    pub fn get(&self, generation: u64, recorded: &[TrackedProcess]) -> Vec<TrackedProcess> {
        let now = (self.now)();
        let mut cache = self.cache.lock().unwrap();
        let fresh = matches!(&*cache, Some((g, at, _))
            if *g == generation && now.saturating_sub(*at) < PROCESS_RESCAN);
        if !fresh {
            *cache = Some((generation, now, reclaim::find_by_arg0(&self.proc_root, &self.arg0)));
        }
        let mut all = recorded.to_vec();
        for p in &cache.as_ref().unwrap().2 {
            if !all.contains(p) {
                all.push(*p);
            }
        }
        all
    }
}

/// utime + stime (clock ticks) and start time of `/proc/<pid>/stat`. Fields are counted after the
/// last `)`, as the command name may hold spaces and parentheses.
pub fn parse_cpu_ticks(stat: &str) -> Option<(u64, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] is field 3 (state): utime is field 14, stime 15, start time 22.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((utime + stime, start_time))
}

/// The CPU time in nanoseconds of the recorded processes that still run, or None if none does.
/// A process whose start time differs is another process that reuses the PID, and is skipped.
pub fn vm_cpu_time(
    proc_root: &Path,
    processes: &[TrackedProcess],
    ticks_per_sec: u64,
) -> Option<u64> {
    let mut ticks = None;
    for process in processes {
        let path = proc_root.join(process.pid.to_string()).join("stat");
        let Some((t, start_time)) = fs::read_to_string(path).ok().and_then(|s| parse_cpu_ticks(&s))
        else {
            continue;
        };
        if start_time == process.start_time {
            ticks = Some(ticks.unwrap_or(0) + t);
        }
    }
    ticks.map(|t| t * (1_000_000_000 / ticks_per_sec.max(1)))
}

/// CLOCK_MONOTONIC in nanoseconds, the clock of the driver's timestamps.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec for the call to fill in.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// The clock-tick rate of `/proc/<pid>/stat` (USER_HZ).
pub fn ticks_per_sec() -> u64 {
    // SAFETY: sysconf has no memory-safety preconditions.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as u64
    } else {
        100
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A stand-in for the driver's file: records writes, serves a settable content.
    #[derive(Default)]
    struct FakeFile {
        exists: Mutex<bool>,
        content: Mutex<String>,
        writes: Mutex<Vec<String>>,
        fail_writes: Mutex<bool>,
    }

    impl StatsFile for Arc<FakeFile> {
        fn exists(&self) -> bool {
            *self.exists.lock().unwrap()
        }

        fn read(&self) -> io::Result<String> {
            Ok(self.content.lock().unwrap().clone())
        }

        fn write(&self, value: &str) -> io::Result<()> {
            if *self.fail_writes.lock().unwrap() {
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            self.writes.lock().unwrap().push(value.to_owned());
            Ok(())
        }
    }

    struct Rig {
        file: Arc<FakeFile>,
        clock: Arc<AtomicU64>,
        monitor: El2Monitor,
    }

    fn rig(exists: bool, ensure_driver: Option<EnsureDriver>) -> Rig {
        let file = Arc::new(FakeFile::default());
        *file.exists.lock().unwrap() = exists;
        let clock = Arc::new(AtomicU64::new(1000));
        let c = clock.clone();
        let monitor = El2Monitor::new(
            Box::new(file.clone()),
            ensure_driver,
            Box::new(move || Duration::from_secs(c.load(Ordering::SeqCst))),
        );
        Rig { file, clock, monitor }
    }

    impl Rig {
        fn set(&self, lines: &[(u32, u64, u64)]) {
            let mut text = format!("{HEADER}\n");
            for (cpu, cycles, ts) in lines {
                text += &format!("{cpu} {cycles} {ts}\n");
            }
            *self.file.content.lock().unwrap() = text;
        }

        fn advance(&self, secs: u64) {
            self.clock.fetch_add(secs, Ordering::SeqCst);
        }

        fn writes(&self) -> Vec<String> {
            self.file.writes.lock().unwrap().clone()
        }
    }

    #[test]
    fn parses_the_driver_format() {
        let text = "cpu_id cycles timestamp_ns\n0 10 100\n1 20 200\n\n";
        assert_eq!(
            parse_xcore_stats(text),
            Ok(vec![
                CycleSample { cpu: 0, cycles: 10, timestamp_ns: 100 },
                CycleSample { cpu: 1, cycles: 20, timestamp_ns: 200 },
            ])
        );
        assert_eq!(parse_xcore_stats("cpu_id cycles timestamp_ns\n"), Ok(vec![]));
        assert!(parse_xcore_stats("").is_err());
        assert!(parse_xcore_stats("0 1 2\n").is_err());
        assert!(parse_xcore_stats("cpu_id cycles timestamp_ns\n0 1\n").is_err());
        assert!(parse_xcore_stats("cpu_id cycles timestamp_ns\n0 x 2\n").is_err());
    }

    #[test]
    fn first_read_starts_the_counters_once() {
        let r = rig(true, None);
        r.set(&[(0, 5, 1), (1, 6, 2)]);
        assert!(matches!(r.monitor.read(), El2Reading::Counting(s) if s.len() == 2));
        r.advance(1);
        assert!(matches!(r.monitor.read(), El2Reading::Counting(_)));
        r.advance(10);
        assert!(matches!(r.monitor.read(), El2Reading::Counting(_)));
        assert_eq!(r.writes(), ["1\n"]);
    }

    #[test]
    fn a_missing_cpu_restarts_the_counters_at_most_every_interval() {
        let r = rig(true, None);
        r.set(&[(0, 5, 1), (1, 6, 2)]);
        r.monitor.read();
        r.set(&[(0, 7, 3)]);
        r.advance(1);
        r.monitor.read(); // too soon after the start
        assert_eq!(r.writes(), ["1\n"]);
        r.advance(RESTART_INTERVAL.as_secs());
        r.monitor.read();
        assert_eq!(r.writes(), ["1\n", "1\n"]);
        r.advance(1);
        r.monitor.read(); // too soon after the restart
        assert_eq!(r.writes().len(), 2);
        r.set(&[(0, 9, 4), (1, 9, 4)]);
        r.advance(RESTART_INTERVAL.as_secs());
        r.monitor.read(); // complete again: nothing to do
        assert_eq!(r.writes().len(), 2);
    }

    #[test]
    fn a_failed_read_restarts_and_reports() {
        let r = rig(true, None);
        *r.file.content.lock().unwrap() = "garbage".into();
        assert!(matches!(r.monitor.read(), El2Reading::Unavailable(e) if e.contains("header")));
        r.advance(RESTART_INTERVAL.as_secs());
        r.monitor.read();
        assert_eq!(r.writes(), ["1\n", "1\n"]);
    }

    #[test]
    fn idle_counters_stop_and_the_next_read_starts_them_again() {
        let r = rig(true, None);
        r.set(&[(0, 5, 1)]);
        r.monitor.read();
        r.advance(IDLE_STOP.as_secs() - 1);
        assert!(!r.monitor.stop_if_idle());
        r.advance(1);
        assert!(r.monitor.stop_if_idle());
        assert!(!r.monitor.stop_if_idle(), "stops only once");
        assert_eq!(r.writes(), ["1\n", "0\n"]);
        r.monitor.read();
        assert_eq!(r.writes(), ["1\n", "0\n", "1\n"]);
    }

    #[test]
    fn never_started_counters_are_not_stopped() {
        let r = rig(true, None);
        r.advance(100);
        assert!(!r.monitor.stop_if_idle());
        assert!(r.writes().is_empty());
    }

    #[test]
    fn reset_stops_leftover_counters_only_if_the_driver_is_there() {
        let r = rig(false, None);
        r.monitor.reset();
        assert!(r.writes().is_empty());
        *r.file.exists.lock().unwrap() = true;
        r.monitor.reset();
        assert_eq!(r.writes(), ["0\n"]);
    }

    #[test]
    fn a_missing_file_loads_the_driver_then_starts() {
        let file_for_loader: Arc<Mutex<Option<Arc<FakeFile>>>> = Arc::new(Mutex::new(None));
        let f = file_for_loader.clone();
        let r = rig(
            false,
            Some(Box::new(move || {
                *f.lock().unwrap().as_ref().unwrap().exists.lock().unwrap() = true;
                Ok(())
            })),
        );
        *file_for_loader.lock().unwrap() = Some(r.file.clone());
        r.set(&[(0, 5, 1)]);
        assert!(matches!(r.monitor.read(), El2Reading::Counting(_)));
        assert_eq!(r.writes(), ["1\n"]);
    }

    #[test]
    fn a_failed_driver_load_or_a_driver_without_monitor_is_reported() {
        let r = rig(false, Some(Box::new(|| Err("the hypervisor driver did not load: x".into()))));
        assert_eq!(
            r.monitor.read(),
            El2Reading::Unavailable("the hypervisor driver did not load: x".into())
        );
        let r = rig(false, Some(Box::new(|| Ok(()))));
        assert!(
            matches!(r.monitor.read(), El2Reading::Unavailable(e) if e.contains("no EL2 monitor"))
        );
        assert!(r.writes().is_empty());
    }

    #[test]
    fn a_refused_start_is_retried_on_the_next_read() {
        let r = rig(true, None);
        r.set(&[(0, 5, 1)]);
        *r.file.fail_writes.lock().unwrap() = true;
        assert!(
            matches!(r.monitor.read(), El2Reading::Unavailable(e) if e.contains("can't start"))
        );
        *r.file.fail_writes.lock().unwrap() = false;
        assert!(matches!(r.monitor.read(), El2Reading::Counting(_)));
        assert_eq!(r.writes(), ["1\n"]);
    }

    #[test]
    fn parses_cpu_ticks_with_awkward_comm() {
        // Fields 4..13 zero, utime 7, stime 5, fields 16..21 zero, start time 999.
        let stat =
            format!("42 (a) b (c)) S {} 7 5 {} 999 0 0", ["0"; 10].join(" "), ["0"; 6].join(" "));
        assert_eq!(parse_cpu_ticks(&stat), Some((12, 999)));
        assert_eq!(parse_cpu_ticks("1 (x) S 0 0"), None);
        assert_eq!(parse_cpu_ticks("garbage"), None);
    }

    #[test]
    fn vm_cpu_time_counts_only_recorded_processes_with_their_start_time() {
        let root = std::env::temp_dir().join(format!("xkvm-monitor-proc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let add = |pid: i32, utime: u64, stime: u64, start: u64| {
            let dir = root.join(pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            let stat = format!(
                "{pid} (xkernel_crosvm) S {} {utime} {stime} {} {start} 0 0\n",
                ["0"; 10].join(" "),
                ["0"; 6].join(" ")
            );
            fs::write(dir.join("stat"), stat).unwrap();
        };
        add(100, 30, 20, 500);
        add(101, 1000, 0, 777); // a reused PID
        let tracked = [
            TrackedProcess { pid: 100, start_time: 500 },
            TrackedProcess { pid: 101, start_time: 501 },
            TrackedProcess { pid: 102, start_time: 502 }, // gone
        ];
        assert_eq!(vm_cpu_time(&root, &tracked, 100), Some(500_000_000));
        assert_eq!(vm_cpu_time(&root, &tracked[1..], 100), None);
        assert_eq!(vm_cpu_time(&root, &[], 100), None);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn vm_processes_add_scanned_ones_and_rescan_after_a_while() {
        let root = std::env::temp_dir().join(format!("xkvm-monitor-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let add = |pid: i32, arg0: &str, start: u64| {
            let dir = root.join(pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("cmdline"), format!("{arg0}\0run\0")).unwrap();
            let stat = format!(
                "{pid} (x) S {} 1 1 {} {start} 0 0\n",
                ["0"; 10].join(" "),
                ["0"; 6].join(" ")
            );
            fs::write(dir.join("stat"), stat).unwrap();
        };
        let clock = Arc::new(AtomicU64::new(0));
        let c = clock.clone();
        let processes = VmProcesses::new(
            root.clone(),
            "crosvm_xkernel_vm".into(),
            Box::new(move || Duration::from_secs(c.load(Ordering::SeqCst))),
        );
        // The supervisor is there; the child that runs the vCPUs comes later.
        add(100, "crosvm_xkernel_vm", 500);
        add(300, "crosvm_other", 501);
        let recorded = [TrackedProcess { pid: 90, start_time: 400 }];
        assert_eq!(
            processes.get(1, &recorded),
            vec![recorded[0], TrackedProcess { pid: 100, start_time: 500 }]
        );
        add(101, "crosvm_xkernel_vm", 510);
        assert_eq!(processes.get(1, &[]).len(), 1, "the scan is reused for a while");
        clock.fetch_add(PROCESS_RESCAN.as_secs(), Ordering::SeqCst);
        assert_eq!(processes.get(1, &[]).len(), 2, "and then repeated");
        add(102, "crosvm_xkernel_vm", 520);
        assert_eq!(processes.get(2, &[]).len(), 3, "a new generation scans again");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn clocks_are_sane() {
        assert!(monotonic_ns() > 0);
        assert!(ticks_per_sec() >= 1);
    }
}
