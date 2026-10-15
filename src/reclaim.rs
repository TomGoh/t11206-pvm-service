// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Confirming that an instance's resources are gone.
//!
//! AVF's death report is not enough: it says crosvm stopped serving the VM, not that the host
//! driver has released it. The driver releases a VM when the last file of it is closed, and a
//! process closes its files on exit before it can be reaped, so "the VM is released" is observed
//! as "every crosvm process of the instance has finished exiting". A release that hangs keeps the
//! process in its exit path (alive, not a zombie), and the check then reports it.
//!
//! Two ways find the processes:
//!
//! - the processes recorded right after AVF started the VM, by PID and start time (the start
//!   time rules out a reused PID). This is the way that still works while a process hangs in
//!   its exit path: by then its memory is gone and `/proc/<pid>/cmdline` reads empty;
//! - any process whose argv[0] is `crosvm_<name>`, the name AVF gives the VM's crosvm. This finds
//!   a process the record missed, e.g. one started before this service restarted.
//!
//! Everything takes the `/proc` root as a parameter, so it is tested on fake trees.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// A process of the instance, as recorded when the VM started.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TrackedProcess {
    pub pid: i32,
    /// Field 22 of `/proc/<pid>/stat`: start time in clock ticks after boot.
    pub start_time: u64,
}

/// The state letter and start time of `/proc/<pid>/stat`. The command name (field 2) may hold
/// spaces and parentheses, so fields are counted after the last `)`.
pub fn parse_stat(stat: &str) -> Option<(char, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    // After the state come fields 4.. ; start time is field 22, i.e. 18 fields after the state.
    let start_time = fields.nth(18)?.parse().ok()?;
    Some((state, start_time))
}

fn read_stat(proc_root: &Path, pid: i32) -> Option<(char, u64)> {
    parse_stat(&fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?)
}

/// Whether a state letter means the process has finished exiting (its files are closed).
fn has_exited(state: char) -> bool {
    matches!(state, 'Z' | 'X' | 'x')
}

/// Every live process whose argv[0] is `arg0`, with its start time.
pub fn find_by_arg0(proc_root: &Path, arg0: &str) -> Vec<TrackedProcess> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let first = cmdline.split(|b| *b == 0).next().unwrap_or_default();
        if first != arg0.as_bytes() {
            continue;
        }
        if let Some((state, start_time)) = read_stat(proc_root, pid) {
            if !has_exited(state) {
                found.push(TrackedProcess { pid, start_time });
            }
        }
    }
    found.sort_by_key(|p| p.pid);
    found
}

/// What is left of an instance: one line per process that has not finished exiting. Empty means
/// the instance is gone.
pub fn leftovers(proc_root: &Path, arg0: &str, tracked: &[TrackedProcess]) -> Vec<String> {
    let mut left = Vec::new();
    let mut seen = Vec::new();
    for process in tracked {
        if let Some((state, start_time)) = read_stat(proc_root, process.pid) {
            if start_time == process.start_time && !has_exited(state) {
                left.push(format!("pid {} ({arg0}) is still in state {state}", process.pid));
                seen.push(process.pid);
            }
        }
    }
    for process in find_by_arg0(proc_root, arg0) {
        if !seen.contains(&process.pid) {
            left.push(format!("pid {} ({arg0}) is still running", process.pid));
        }
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fake `/proc` with processes given as (pid, argv, state, start time).
    struct FakeProc(PathBuf);

    impl FakeProc {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("xkvm-proc-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(root.join("self")).unwrap(); // a non-numeric entry
            FakeProc(root)
        }

        fn add(&self, pid: i32, argv: &[&str], comm: &str, state: char, start_time: u64) {
            let dir = self.0.join(pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            let mut cmdline = argv.join("\0").into_bytes();
            if !argv.is_empty() {
                cmdline.push(0);
            }
            fs::write(dir.join("cmdline"), cmdline).unwrap();
            // Fields 4..21 are zeros; field 22 is the start time.
            let zeros = ["0"; 18].join(" ");
            fs::write(
                dir.join("stat"),
                format!("{pid} ({comm}) {state} {zeros} {start_time} 0 0\n"),
            )
            .unwrap();
        }
    }

    impl Drop for FakeProc {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const NAME: &str = "crosvm_xkernel_vm";

    #[test]
    fn parses_stat_with_awkward_comm() {
        let zeros = ["0"; 18].join(" ");
        let stat = format!("42 (a) b (c)) S {zeros} 12345 0 0");
        assert_eq!(parse_stat(&stat), Some(('S', 12345)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S 0 0"), None);
    }

    #[test]
    fn finds_by_arg0_but_not_other_vms_or_zombies() {
        let p = FakeProc::new("find");
        p.add(100, &[NAME, "run", "--name"], "crosvm", 'S', 500);
        p.add(101, &["crosvm_other_vm", "run"], "crosvm", 'S', 501);
        p.add(102, &[NAME], "crosvm", 'Z', 502);
        p.add(103, &["/apex/com.android.virt/bin/crosvm"], "crosvm", 'S', 503);
        assert_eq!(find_by_arg0(&p.0, NAME), vec![TrackedProcess { pid: 100, start_time: 500 }]);
    }

    #[test]
    fn tracked_process_hung_in_exit_is_found_without_cmdline() {
        let p = FakeProc::new("hung");
        // Memory gone, so cmdline is empty, but the process is still in uninterruptible sleep.
        p.add(200, &[], "crosvm", 'D', 700);
        let tracked = [TrackedProcess { pid: 200, start_time: 700 }];
        let left = leftovers(&p.0, NAME, &tracked);
        assert_eq!(left.len(), 1);
        assert!(left[0].contains("pid 200") && left[0].contains("state D"), "{left:?}");
    }

    #[test]
    fn reused_pid_and_zombie_do_not_count() {
        let p = FakeProc::new("reuse");
        p.add(300, &["sh"], "sh", 'S', 9999); // PID reused by another process
        p.add(301, &[], "crosvm", 'Z', 800); // finished exiting, not reaped yet
        let tracked = [
            TrackedProcess { pid: 300, start_time: 800 },
            TrackedProcess { pid: 301, start_time: 800 },
            TrackedProcess { pid: 302, start_time: 800 }, // gone entirely
        ];
        assert!(leftovers(&p.0, NAME, &tracked).is_empty());
    }

    #[test]
    fn untracked_instance_process_is_reported_once() {
        let p = FakeProc::new("untracked");
        p.add(400, &[NAME, "run"], "crosvm", 'R', 900);
        p.add(401, &[NAME, "run"], "crosvm", 'S', 901);
        let tracked = [TrackedProcess { pid: 400, start_time: 900 }];
        let left = leftovers(&p.0, NAME, &tracked);
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(left.iter().any(|l| l.contains("pid 401")));
    }

    #[test]
    fn missing_proc_root_finds_nothing() {
        assert!(find_by_arg0(Path::new("/does/not/exist"), NAME).is_empty());
    }
}
