// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The figures of the read-only monitoring interface (`IXKernelVmMonitor`, served in `service`):
//! the hypervisor's EL2 cycles per CPU and the CPU time of the x-kernel VM. Both are cumulative;
//! the client computes rates from two samples, so this side keeps no per-client state.
//!
//! **EL2 cycles** come from the host driver's [`XCORE_STATS`]. Writing `1` starts the hypervisor's
//! per-CPU cycle counters (counting EL2 only), reading lists `cpu_id cycles timestamp_ns` for each
//! online CPU whose counter is readable, writing `0` stops them and gives the PMU back to the Host
//! (while they run, the Host cannot count with simpleperf). A stopped monitor still lists every
//! CPU, with the counts frozen; a restart continues from them. [`El2Monitor`]:
//!
//! - starts the counters on the first read, and loads the driver first if the file is missing
//!   (the driver is loaded on demand, as for a VM start). The load runs without the monitor's
//!   lock: it can take the loader's whole timeout. If the start is refused for permission (a
//!   driver loaded by something else leaves the file 0644 root root), the loader is asked once
//!   more; it skips the loaded modules and gives the file to this service.
//! - remembers a failed load for [`LOAD_BACKOFF`] and a failed start for [`START_BACKOFF`], and
//!   answers with the remembered reason meanwhile, so a polling client does not make the loader
//!   run, or the driver issue global controls, at its polling rate.
//! - writes `1` again, at most every [`RESTART_INTERVAL`], only when the counters need it: an
//!   online CPU's line is missing in [`MISSING_READINGS`] readings in a row (its counter failed,
//!   e.g. a refused freeze around deep idle), the read fails, or no listed CPU's count moved for
//!   [`FROZEN_AFTER`] (someone else wrote `0`, or the driver was reloaded). A CPU that is offline
//!   is not missing. Writing `1` while running is not free: the driver re-applies the global
//!   control whenever any online CPU is inside a deep-idle bracket, which on an idle device is
//!   almost always, and that wakes every CPU and adds EL2 time to the figure being measured.
//! - stops the counters once no client has read for [`IDLE_STOP`] ([`El2Monitor::stop_if_idle`],
//!   run by a thread; a failed stop is retried up to [`STOP_ATTEMPTS`] times), and at service
//!   start ([`El2Monitor::reset`]) in case an earlier run of the service left them running.
//!
//! The cycles are counted at the CPU's clock of the moment, so a share of time needs a frequency;
//! which one is the client's choice. The driver numbers CPUs by the hypervisor's CPU id; on T11206 it equals Linux's CPU number (pinning Linux
//! CPUs 0-3 changed exactly the EL2 rates of ids 0-3), and this module and its clients assume so.
//!
//! **VM CPU time** is utime + stime of the VM's crosvm processes (`/proc/<pid>/stat`, every
//! thread, [`vm_cpu_time`]), each counted only while its start time still matches. The processes
//! ([`VmProcesses`]) are those the controller recorded at the start plus every process whose
//! argv[0] is `crosvm_<name>`, rescanned at most every [`PROCESS_RESCAN`]: the record alone misses
//! them on the direct backend, whose start returns before the launcher has executed crosvm, and
//! crosvm runs the vCPUs in a child it forks after the start (the parent only supervises). The
//! set of processes counted is reported as a fingerprint, so a client re-baselines when it
//! changes instead of reading the time a newly found process had used so far as one interval's.
//! The x-kernel's vCPUs are scheduled by host threads (crosvm asks for `affinity = "proxy"`), so
//! the guest's run time is these threads' time; it also contains the hypervisor's handling of the
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
/// The CPUs Linux has online.
const CPU_ONLINE: &str = "/sys/devices/system/cpu/online";
/// The first line of [`XCORE_STATS`].
const HEADER: &str = "cpu_id cycles timestamp_ns";
/// Counters nobody has read for this long are stopped.
pub const IDLE_STOP: Duration = Duration::from_secs(15);
/// The least time between two restarts of the counters.
pub const RESTART_INTERVAL: Duration = Duration::from_secs(5);
/// How long a failed start is answered from memory.
pub const START_BACKOFF: Duration = RESTART_INTERVAL;
/// How long a failed driver load is answered from memory.
pub const LOAD_BACKOFF: Duration = Duration::from_secs(30);
/// Readings in a row with an online CPU missing before the counters are restarted.
pub const MISSING_READINGS: u32 = 2;
/// Counts that did not move for this long mean the counters are not running.
pub const FROZEN_AFTER: Duration = Duration::from_secs(1);
/// Failed idle stops before giving up.
pub const STOP_ATTEMPTS: u32 = 5;
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

/// Parses a CPU list such as `0-3,5,7-8` (sysfs `online`).
pub fn parse_cpu_list(text: &str) -> Option<Vec<u32>> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((first, last)) => {
                let (first, last): (u32, u32) = (first.parse().ok()?, last.parse().ok()?);
                if first > last {
                    return None;
                }
                cpus.extend(first..=last);
            }
            None => cpus.push(part.parse().ok()?),
        }
    }
    Some(cpus)
}

/// The CPUs Linux has online, or None if that cannot be read.
pub fn online_cpus() -> Option<Vec<u32>> {
    parse_cpu_list(&fs::read_to_string(CPU_ONLINE).ok()?)
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

/// Loads the host driver as for a start (`driver::DriverLoader`); with `true`, asks the loader to
/// run even if the driver is loaded, to prepare [`XCORE_STATS`] again.
pub type EnsureDriver = Box<dyn Fn(bool) -> Result<(), String> + Send + Sync>;
/// The CPUs Linux has online ([`online_cpus`]).
pub type OnlineCpus = Box<dyn Fn() -> Option<Vec<u32>> + Send + Sync>;
/// A monotonic clock; only differences matter.
pub type Clock = Box<dyn Fn() -> Duration + Send + Sync>;

#[derive(Default)]
struct State {
    /// This service has started the counters and not stopped them since.
    running: bool,
    last_read: Duration,
    last_start: Duration,
    /// Readings in a row in which an online CPU's line was missing.
    missing: u32,
    /// An earlier reading to tell whether the counts move, and when it was taken.
    reference: Option<(Duration, Vec<CycleSample>)>,
    load_failure: Option<(Duration, String)>,
    start_failure: Option<(Duration, String)>,
    stop_failures: u32,
}

/// The remembered reason, while it is younger than `backoff`.
fn recent(
    failure: &Option<(Duration, String)>,
    now: Duration,
    backoff: Duration,
) -> Option<String> {
    failure
        .as_ref()
        .filter(|(at, _)| now.saturating_sub(*at) < backoff)
        .map(|(_, reason)| reason.clone())
}

/// Starts, reads and stops the EL2 counters for the monitoring interface.
pub struct El2Monitor {
    file: Box<dyn StatsFile>,
    ensure_driver: Option<EnsureDriver>,
    online: OnlineCpus,
    now: Clock,
    state: Mutex<State>,
}

impl El2Monitor {
    pub fn new(
        file: Box<dyn StatsFile>,
        ensure_driver: Option<EnsureDriver>,
        online: OnlineCpus,
        now: Clock,
    ) -> Self {
        Self { file, ensure_driver, online, now, state: Mutex::new(State::default()) }
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
        let now = (self.now)();
        if let Err(reason) = self.ensure_file(now) {
            return El2Reading::Unavailable(reason);
        }
        let start = {
            let mut state = self.state.lock().unwrap();
            state.last_read = now;
            if !state.running {
                if let Some(reason) = recent(&state.start_failure, now, START_BACKOFF) {
                    return El2Reading::Unavailable(reason);
                }
            }
            !state.running
        };
        if start {
            // Not under the lock: a refused start may wait for the loader.
            let result = self.start();
            let mut state = self.state.lock().unwrap();
            match result {
                Ok(()) => {
                    info!("started the EL2 counters");
                    *state = State {
                        running: true,
                        last_read: now,
                        last_start: now,
                        ..State::default()
                    };
                }
                Err(e) => {
                    warn!("{e}");
                    state.start_failure = Some((now, e.clone()));
                    return El2Reading::Unavailable(e);
                }
            }
        }
        let reading = self
            .file
            .read()
            .map_err(|e| format!("can't read {XCORE_STATS}: {e}"))
            .and_then(|text| parse_xcore_stats(&text));
        let mut state = self.state.lock().unwrap();
        let why = match &reading {
            Err(e) => Some(e.clone()),
            Ok(samples) => self.restart_reason(&mut state, now, samples),
        };
        if let Some(why) = why {
            if state.running && now.saturating_sub(state.last_start) >= RESTART_INTERVAL {
                match self.file.write("1\n") {
                    Ok(()) => info!("restarted the EL2 counters: {why}"),
                    Err(e) => {
                        // The next read goes through the start, with its backoff and its
                        // second chance for a file the service may not write.
                        warn!("can't restart the EL2 counters ({why}): {e}");
                        state.running = false;
                    }
                }
                state.last_start = now;
                state.missing = 0;
                state.reference = None;
            }
        }
        match reading {
            Ok(samples) => El2Reading::Counting(samples),
            Err(e) => El2Reading::Unavailable(e),
        }
    }

    /// Makes sure the counter file exists, loading the driver if not; failures are remembered
    /// for [`LOAD_BACKOFF`].
    fn ensure_file(&self, now: Duration) -> Result<(), String> {
        if self.file.exists() {
            return Ok(());
        }
        if let Some(reason) = recent(&self.state.lock().unwrap().load_failure, now, LOAD_BACKOFF) {
            return Err(reason);
        }
        // Not under the lock: a load can take the loader's whole timeout. The loader handles one
        // request at a time, so concurrent readers wait for the same load.
        let loaded = match &self.ensure_driver {
            Some(ensure) => ensure(false),
            None => Ok(()),
        };
        let result = loaded.and_then(|()| {
            self.file.exists().then_some(()).ok_or_else(|| {
                format!("the hypervisor driver offers no EL2 monitor ({XCORE_STATS} is missing)")
            })
        });
        let mut state = self.state.lock().unwrap();
        match result {
            Ok(()) => {
                // A driver loaded just now starts with its counters stopped.
                state.running = false;
                state.load_failure = None;
                Ok(())
            }
            Err(e) => {
                state.load_failure = Some((now, e.clone()));
                Err(e)
            }
        }
    }

    /// Writes `1`; on a permission error asks the loader once to prepare the file.
    fn start(&self) -> Result<(), String> {
        let error = match self.file.write("1\n") {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let Some(ensure) = self.ensure_driver.as_ref() else {
            return Err(format!("can't start the EL2 counters: {error}"));
        };
        if error.kind() != io::ErrorKind::PermissionDenied {
            return Err(format!("can't start the EL2 counters: {error}"));
        }
        info!("{XCORE_STATS} refused the start ({error}): asking the loader to prepare it");
        ensure(true).map_err(|e| format!("can't start the EL2 counters ({error}): {e}"))?;
        self.file.write("1\n").map_err(|e| format!("can't start the EL2 counters: {e}"))
    }

    /// Why a complete reading still asks for a restart, if it does.
    fn restart_reason(
        &self,
        state: &mut State,
        now: Duration,
        samples: &[CycleSample],
    ) -> Option<String> {
        let missing: Vec<u32> = (self.online)()
            .unwrap_or_default()
            .into_iter()
            .filter(|cpu| !samples.iter().any(|s| s.cpu == *cpu))
            .collect();
        state.missing = if missing.is_empty() { 0 } else { state.missing + 1 };
        let frozen = match &state.reference {
            Some((at, earlier)) if now.saturating_sub(*at) >= FROZEN_AFTER => {
                let frozen = !samples.is_empty()
                    && samples
                        .iter()
                        .all(|s| earlier.iter().any(|e| e.cpu == s.cpu && e.cycles == s.cycles));
                state.reference = Some((now, samples.to_vec()));
                frozen
            }
            Some(_) => false,
            None => {
                state.reference = Some((now, samples.to_vec()));
                false
            }
        };
        if state.missing >= MISSING_READINGS {
            Some(format!("online CPU(s) {missing:?} missing in {} readings", state.missing))
        } else if frozen {
            Some(format!("no count moved for {}s: stopped from outside", FROZEN_AFTER.as_secs()))
        } else {
            None
        }
    }

    /// Stops the counters if nobody has read them for [`IDLE_STOP`]; returns whether they are
    /// stopped now (or given up on after [`STOP_ATTEMPTS`] failed stops).
    pub fn stop_if_idle(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.running || (self.now)().saturating_sub(state.last_read) < IDLE_STOP {
            return false;
        }
        match self.file.write("0\n") {
            Ok(()) => info!("stopped the EL2 counters: no client for {}s", IDLE_STOP.as_secs()),
            Err(_) if !self.file.exists() => info!("the EL2 counters are gone with the driver"),
            Err(e) => {
                state.stop_failures += 1;
                if state.stop_failures < STOP_ATTEMPTS {
                    warn!(
                        "can't stop the idle EL2 counters ({} of {STOP_ATTEMPTS}): {e}",
                        state.stop_failures
                    );
                    return false;
                }
                warn!("can't stop the idle EL2 counters, giving up: {e}");
            }
        }
        state.running = false;
        state.stop_failures = 0;
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

/// The CPU time of the VM's processes, and which processes it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmCpu {
    pub cpu_time_ns: u64,
    /// A fingerprint of the processes counted (PIDs and start times); it changes when one comes
    /// or goes, and the time then jumps or drops by that process's total.
    pub process_set: u64,
}

/// The CPU time of the given processes that still run, or None if none does. A process whose
/// start time differs is another process that reuses the PID, and is skipped.
pub fn vm_cpu_time(
    proc_root: &Path,
    processes: &[TrackedProcess],
    ticks_per_sec: u64,
) -> Option<VmCpu> {
    let mut ticks = 0;
    let mut counted = Vec::new();
    for process in processes {
        let path = proc_root.join(process.pid.to_string()).join("stat");
        let Some((t, start_time)) = fs::read_to_string(path).ok().and_then(|s| parse_cpu_ticks(&s))
        else {
            continue;
        };
        if start_time == process.start_time && !counted.contains(process) {
            ticks += t;
            counted.push(*process);
        }
    }
    if counted.is_empty() {
        return None;
    }
    counted.sort_by_key(|p| (p.pid, p.start_time));
    // FNV-1a over the sorted (pid, start time) pairs.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for p in &counted {
        for byte in p.pid.to_le_bytes().into_iter().chain(p.start_time.to_le_bytes()) {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    Some(VmCpu { cpu_time_ns: ticks * (1_000_000_000 / ticks_per_sec.max(1)), process_set: hash })
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
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    /// A stand-in for the driver: `1` starts and `0` stops the counters; while they run, every
    /// read adds 1000 cycles to each listed CPU. Writes are recorded; a write can be made to fail.
    #[derive(Default)]
    struct FakeDriver {
        exists: bool,
        running: bool,
        /// The CPUs whose lines are listed, with their counts.
        cycles: BTreeMap<u32, u64>,
        garbage: bool,
        /// errno of the next writes, while set.
        fail: Option<i32>,
        attempts: u32,
        writes: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<FakeDriver>>);

    impl StatsFile for Fake {
        fn exists(&self) -> bool {
            self.0.lock().unwrap().exists
        }

        fn read(&self) -> io::Result<String> {
            let mut d = self.0.lock().unwrap();
            if d.garbage {
                return Ok("garbage".into());
            }
            let running = d.running;
            let mut text = format!("{HEADER}\n");
            for (cpu, cycles) in d.cycles.iter_mut() {
                if running {
                    *cycles += 1000;
                }
                text += &format!("{cpu} {cycles} 1\n");
            }
            Ok(text)
        }

        fn write(&self, value: &str) -> io::Result<()> {
            let mut d = self.0.lock().unwrap();
            d.attempts += 1;
            if let Some(errno) = d.fail {
                return Err(io::Error::from_raw_os_error(errno));
            }
            d.writes.push(value.to_owned());
            d.running = value == "1\n";
            Ok(())
        }
    }

    struct Rig {
        fake: Fake,
        online: Arc<Mutex<Option<Vec<u32>>>>,
        clock: Arc<AtomicU64>,
        monitor: Arc<El2Monitor>,
    }

    /// CPUs 0 and 1 listed and online; the clock counts tenths of a second.
    fn rig(exists: bool, ensure_driver: Option<EnsureDriver>) -> Rig {
        let fake = Fake::default();
        {
            let mut d = fake.0.lock().unwrap();
            d.exists = exists;
            d.cycles = BTreeMap::from([(0, 0), (1, 0)]);
        }
        let online = Arc::new(Mutex::new(Some(vec![0, 1])));
        let clock = Arc::new(AtomicU64::new(10_000));
        let (o, c) = (online.clone(), clock.clone());
        let monitor = Arc::new(El2Monitor::new(
            Box::new(fake.clone()),
            ensure_driver,
            Box::new(move || o.lock().unwrap().clone()),
            Box::new(move || Duration::from_millis(c.load(Ordering::SeqCst) * 100)),
        ));
        Rig { fake, online, clock, monitor }
    }

    impl Rig {
        fn driver(&self) -> std::sync::MutexGuard<'_, FakeDriver> {
            self.fake.0.lock().unwrap()
        }

        fn advance_ms(&self, ms: u64) {
            self.clock.fetch_add(ms / 100, Ordering::SeqCst);
        }

        fn advance(&self, secs: u64) {
            self.advance_ms(secs * 1000);
        }

        fn writes(&self) -> Vec<String> {
            self.driver().writes.clone()
        }

        /// Reads every `step_ms` for `secs` seconds.
        fn poll(&self, secs: u64, step_ms: u64) {
            for _ in 0..secs * 1000 / step_ms {
                self.advance_ms(step_ms);
                self.monitor.read();
            }
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
    fn parses_cpu_lists() {
        assert_eq!(parse_cpu_list("0-7\n"), Some((0..8).collect()));
        assert_eq!(parse_cpu_list("0-3,5,7-8"), Some(vec![0, 1, 2, 3, 5, 7, 8]));
        assert_eq!(parse_cpu_list("0"), Some(vec![0]));
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("x"), None);
    }

    #[test]
    fn running_counters_are_started_once_however_often_they_are_read() {
        let r = rig(true, None);
        r.poll(60, 500);
        assert_eq!(r.writes(), ["1\n"]);
    }

    #[test]
    fn an_offline_cpu_is_not_missing() {
        let r = rig(true, None);
        r.poll(2, 500);
        // CPU 1 goes offline: the driver stops listing it.
        r.driver().cycles.remove(&1);
        *r.online.lock().unwrap() = Some(vec![0]);
        r.poll(60, 500);
        assert_eq!(r.writes(), ["1\n"]);
    }

    #[test]
    fn an_online_cpu_missing_twice_in_a_row_restarts_at_most_every_interval() {
        let r = rig(true, None);
        r.poll(6, 1000);
        // CPU 1 is online but its line is gone (its counter failed).
        r.driver().cycles.remove(&1);
        r.advance(1);
        r.monitor.read();
        assert_eq!(r.writes().len(), 1, "one missing reading is not enough");
        r.advance(1);
        r.monitor.read();
        assert_eq!(r.writes(), ["1\n", "1\n"]);
        r.poll(4, 1000);
        assert_eq!(r.writes().len(), 2, "not again within the interval");
        r.poll(1, 1000);
        assert_eq!(r.writes().len(), 3);
        // Back again: nothing more to do.
        r.driver().cycles.insert(1, 0);
        r.poll(20, 1000);
        assert_eq!(r.writes().len(), 3);
    }

    #[test]
    fn a_single_missing_reading_does_not_restart() {
        let r = rig(true, None);
        r.poll(6, 1000);
        r.driver().cycles.remove(&1);
        r.poll(1, 1000);
        r.driver().cycles.insert(1, 0);
        r.poll(10, 1000);
        assert_eq!(r.writes(), ["1\n"]);
    }

    #[test]
    fn counters_stopped_from_outside_are_started_again() {
        let r = rig(true, None);
        r.poll(6, 500);
        // Someone writes 0 (e.g. el2-monitor.sh stop); the readings stay complete but frozen.
        r.driver().running = false;
        r.poll(3, 500);
        assert_eq!(r.writes(), ["1\n", "1\n"]);
        assert!(r.driver().running);
        r.poll(30, 500);
        assert_eq!(r.writes().len(), 2);
    }

    #[test]
    fn a_failed_read_restarts_and_reports() {
        let r = rig(true, None);
        r.driver().garbage = true;
        assert!(matches!(r.monitor.read(), El2Reading::Unavailable(e) if e.contains("header")));
        r.advance(5);
        r.monitor.read();
        assert_eq!(r.writes(), ["1\n", "1\n"]);
    }

    #[test]
    fn idle_counters_stop_and_the_next_read_starts_them_again() {
        let r = rig(true, None);
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
    fn a_failed_idle_stop_is_retried_then_given_up() {
        let r = rig(true, None);
        r.monitor.read();
        r.advance(IDLE_STOP.as_secs());
        r.driver().fail = Some(libc::EIO);
        for _ in 1..STOP_ATTEMPTS {
            assert!(!r.monitor.stop_if_idle());
        }
        assert!(r.monitor.stop_if_idle(), "gives up after the last attempt");
        assert_eq!(r.driver().attempts, 1 + STOP_ATTEMPTS);
        r.driver().fail = None;
        assert!(!r.monitor.stop_if_idle(), "no longer running");
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
        r.driver().exists = true;
        r.monitor.reset();
        assert_eq!(r.writes(), ["0\n"]);
    }

    /// A loader that records its calls and runs `effect` on the fake driver.
    fn loader(
        fake: &Arc<Mutex<Option<Fake>>>,
        calls: &Arc<Mutex<Vec<bool>>>,
        effect: impl Fn(&mut FakeDriver, bool) -> Result<(), String> + Send + Sync + 'static,
    ) -> EnsureDriver {
        let (fake, calls) = (fake.clone(), calls.clone());
        Box::new(move |force| {
            calls.lock().unwrap().push(force);
            let fake = fake.lock().unwrap().clone().unwrap();
            let mut d = fake.0.lock().unwrap();
            effect(&mut d, force)
        })
    }

    fn rig_with_loader(
        exists: bool,
        effect: impl Fn(&mut FakeDriver, bool) -> Result<(), String> + Send + Sync + 'static,
    ) -> (Rig, Arc<Mutex<Vec<bool>>>) {
        let slot = Arc::new(Mutex::new(None));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let r = rig(exists, Some(loader(&slot, &calls, effect)));
        *slot.lock().unwrap() = Some(r.fake.clone());
        (r, calls)
    }

    #[test]
    fn a_missing_file_loads_the_driver_then_starts() {
        let (r, calls) = rig_with_loader(false, |d, _| {
            d.exists = true;
            Ok(())
        });
        assert!(matches!(r.monitor.read(), El2Reading::Counting(_)));
        assert_eq!(r.writes(), ["1\n"]);
        assert_eq!(*calls.lock().unwrap(), [false]);
    }

    #[test]
    fn a_failed_driver_load_is_answered_from_memory_for_a_while() {
        let (r, calls) = rig_with_loader(false, |_, _| Err("did not load: x".into()));
        assert_eq!(r.monitor.read(), El2Reading::Unavailable("did not load: x".into()));
        r.poll(LOAD_BACKOFF.as_secs() - 1, 500);
        assert_eq!(calls.lock().unwrap().len(), 1);
        r.advance(1);
        r.monitor.read();
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert!(r.writes().is_empty());
    }

    #[test]
    fn a_driver_without_the_monitor_is_reported() {
        let (r, _) = rig_with_loader(false, |_, _| Ok(()));
        assert!(
            matches!(r.monitor.read(), El2Reading::Unavailable(e) if e.contains("no EL2 monitor"))
        );
        assert!(r.writes().is_empty());
    }

    #[test]
    fn a_refused_start_is_tried_once_per_interval() {
        let r = rig(true, None);
        r.driver().fail = Some(libc::EBUSY);
        assert!(
            matches!(r.monitor.read(), El2Reading::Unavailable(e) if e.contains("can't start"))
        );
        r.poll(10, 500);
        assert_eq!(r.driver().attempts, 3, "at 0, 5 and 10 s");
        r.driver().fail = None;
        r.poll(5, 500);
        assert_eq!(r.writes(), ["1\n"]);
    }

    #[test]
    fn a_file_the_service_may_not_write_asks_the_loader_again() {
        // Loaded by someone else: the file is still root-only until the loader prepares it.
        let (r, calls) = rig_with_loader(true, |d, force| {
            assert!(force);
            d.fail = None;
            Ok(())
        });
        r.driver().fail = Some(libc::EACCES);
        assert!(matches!(r.monitor.read(), El2Reading::Counting(_)));
        assert_eq!(*calls.lock().unwrap(), [true]);
        assert_eq!(r.writes(), ["1\n"]);
    }

    #[test]
    fn a_driver_load_does_not_hold_the_monitor() {
        let (r, _) = rig_with_loader(false, |d, _| {
            thread::sleep(Duration::from_millis(400));
            d.exists = true;
            Ok(())
        });
        let monitor = r.monitor.clone();
        let loading = thread::spawn(move || monitor.read());
        thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        r.monitor.stop_if_idle();
        assert!(started.elapsed() < Duration::from_millis(200), "waited for the load");
        assert!(matches!(loading.join().unwrap(), El2Reading::Counting(_)));
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
        add(103, 10, 0, 503);
        let p100 = TrackedProcess { pid: 100, start_time: 500 };
        let p103 = TrackedProcess { pid: 103, start_time: 503 };
        let tracked = [
            p100,
            TrackedProcess { pid: 101, start_time: 501 },
            TrackedProcess { pid: 102, start_time: 502 }, // gone
        ];
        let one = vm_cpu_time(&root, &tracked, 100).unwrap();
        assert_eq!(one.cpu_time_ns, 500_000_000);
        assert_eq!(vm_cpu_time(&root, &tracked[1..], 100), None);
        assert_eq!(vm_cpu_time(&root, &[], 100), None);
        // A process found later changes the set; the order and duplicates do not.
        let two = vm_cpu_time(&root, &[p100, p103], 100).unwrap();
        assert_eq!(two.cpu_time_ns, 600_000_000);
        assert_ne!(two.process_set, one.process_set);
        assert_eq!(vm_cpu_time(&root, &[p103, p100, p103], 100), Some(two));
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
