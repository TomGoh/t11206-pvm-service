//! The crosvm launcher as built, on Linux: a crosvm it starts dies with the launcher's parent even
//! when that parent armed nothing, and it refuses to start one for a parent that is already gone.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const LAUNCHER: &str = env!("CARGO_BIN_EXE_xkernel_crosvm_launcher");

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xkvm-launcher-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    // A crosvm that records its PID and argv[0], then waits.
    let fake = d.join("crosvm");
    fs::write(&fake, "#!/bin/sh\necho \"$$ $0\" > \"$PIDFILE\"\nexec sleep 30\n").unwrap();
    Command::new("chmod").arg("755").arg(&fake).status().unwrap();
    d
}

fn alive(pid: i32) -> bool {
    // A zombie is gone for this purpose: it has released everything.
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| !s.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
        .unwrap_or(false)
}

/// Runs `script` in a shell that is the launcher's parent and exits after half a second; returns
/// the fake crosvm's PID and argv[0] as it recorded them.
fn run_parent(d: &Path, script: &str) -> (i32, String) {
    let pidfile = d.join("pid");
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{script} & sleep 0.5; exit 0"))
        .env("PIDFILE", &pidfile)
        .env("LAUNCHER", LAUNCHER)
        .env("FAKE", d.join("crosvm"))
        .status()
        .unwrap();
    assert!(status.success());
    let line = fs::read_to_string(&pidfile).expect("the fake crosvm ran");
    let (pid, argv0) = line.trim().split_once(' ').unwrap();
    (pid.parse().unwrap(), argv0.to_string())
}

fn gone_within(pid: i32, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn crosvm_dies_with_the_launchers_parent() {
    let d = dir("dies");
    let (pid, argv0) = run_parent(&d, r#""$LAUNCHER" $$ "$FAKE" crosvm_xkernel_vm"#);
    assert!(argv0.ends_with("crosvm") || argv0 == "crosvm_xkernel_vm", "argv0 {argv0}");
    assert!(gone_within(pid, Duration::from_secs(3)), "crosvm {pid} outlived its parent");
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn control_without_the_launcher_crosvm_outlives_its_parent() {
    // Proves the test above can fail: the same fake started directly is reparented and lives on.
    let d = dir("control");
    let (pid, _) = run_parent(&d, r#""$FAKE""#);
    assert!(!gone_within(pid, Duration::from_millis(1500)), "the control died by itself");
    // SAFETY: kill only sends a signal to the control's own fake crosvm.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn refuses_when_the_parent_is_not_the_service() {
    let out = Command::new(LAUNCHER).args(["1", "/bin/true", "crosvm"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is gone"));
}
