// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The lifecycle of the one x-kernel VM instance this service owns.
//!
//! The machine is pure: it takes requests and events, changes its state, and returns the actions
//! the executor must carry out (create and start the VM through AVF, stop it, probe its
//! readiness, confirm that its resources are gone). Nothing here touches Binder, AVF or the file
//! system, so every rule is tested on the host.
//!
//! "AVF" below stands for the backend that runs the VM: AVF itself, or this service running
//! crosvm directly (see `direct`); the rules are the same for both.
//!
//! What a state means (the UI shows exactly these):
//!
//! | State | Meaning |
//! |---|---|
//! | `Unavailable` | No instance, and a precondition fails (driver, AVF, image, protection). |
//! | `Stopped` | No instance, and the last one is confirmed gone. |
//! | `Starting` | The instance is being created and started through AVF. |
//! | `RunningNotReady` | AVF runs the VM; the guest has not completed the readiness handshake. |
//! | `Ready` | The guest completed the readiness handshake. |
//! | `Stopping` | A stop or a confirmation that the instance is gone is in progress. |
//! | `ReclaimUnknown` | The instance did not go away in time, or its process is still there. |
//! | `Failed` | The instance failed or died unexpectedly; it is confirmed gone. |
//!
//! Rules:
//!
//! - There is at most one instance. A start while one is starting or running is merged into the
//!   current operation; a start while stopping is refused as busy; a start while the last
//!   instance is not confirmed gone is refused.
//! - "Gone" is confirmed only by the executor's reclaim check (the VM process has exited, see
//!   `reclaim`), never by AVF's death notification alone and never by a timeout.
//! - A stop that does not see the VM die within the stop deadline moves to `ReclaimUnknown`; a
//!   later death still confirms and moves on. Nothing restarts the VM on its own.
//! - A stop while starting is remembered and carried out once the start has an outcome.
//! - A stop in `ReclaimUnknown` re-runs the reclaim check. `Failed` is only entered once the
//!   instance is confirmed gone, so a start may follow it directly and a stop has nothing to do.
//! - Every instance has a generation; events of an older generation are ignored.
//! - Every visible change bumps `seq`, so a client can drop stale snapshots.

use std::fmt;

/// The visible state of the instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmState {
    Unavailable,
    Stopped,
    Starting,
    RunningNotReady,
    Ready,
    Stopping,
    ReclaimUnknown,
    Failed,
}

/// How the last instance ended, as far as AVF reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The guest shut itself down.
    GuestShutdown,
    /// crosvm was killed: what a requested stop does, and also what an outside kill does.
    Killed,
    /// Anything else: crash, reboot request, virtualization manager death, … with AVF's reason.
    Abnormal(String),
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::GuestShutdown => write!(f, "guest shut down"),
            Exit::Killed => write!(f, "killed"),
            Exit::Abnormal(reason) => write!(f, "{reason}"),
        }
    }
}

/// `Snapshot::last_exit` of an instance that ended because a stop asked for it.
pub const LAST_EXIT_STOPPED: &str = "stopped on request";

/// Something that happened to the instance, reported by the executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// AVF created and started the VM.
    Started { generation: u64, cid: i32, at_ms: u64 },
    /// Creating or starting the VM failed.
    StartFailed { generation: u64, reason: String },
    /// The guest completed the readiness handshake.
    Ready { generation: u64 },
    /// The readiness handshake did not complete in time.
    ReadyTimedOut { generation: u64 },
    /// AVF reports the VM dead (or the virtualization manager serving it died).
    Died { generation: u64, exit: Exit },
    /// The VM did not die within the stop deadline.
    StopTimedOut { generation: u64 },
    /// The reclaim check finished: `gone` says whether every process of the instance has exited;
    /// `detail` says what is left otherwise.
    ReclaimChecked { generation: u64, gone: bool, detail: String },
}

/// What the executor must do. Each action carries the generation it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Create and start a new instance through AVF.
    CreateAndStart { generation: u64 },
    /// Ask AVF to stop the instance.
    Stop { generation: u64 },
    /// Arm the stop deadline.
    ArmStopDeadline { generation: u64 },
    /// Probe the guest's readiness until it answers or the deadline passes.
    ProbeReady { generation: u64 },
    /// Release the instance's AVF objects and check that its processes are gone.
    CheckReclaim { generation: u64 },
}

/// Why a request is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A precondition fails; the instance cannot start.
    Unavailable(String),
    /// The opposite operation is in progress.
    Busy(String),
    /// The last instance is not confirmed gone.
    ReclaimPending(String),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Unavailable(m) | Refused::Busy(m) | Refused::ReclaimPending(m) => {
                write!(f, "{m}")
            }
        }
    }
}

/// A consistent view of the instance, as clients see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub state: VmState,
    pub seq: u64,
    /// The operation in progress, or the last one.
    pub operation: u64,
    /// The generation of the current or last instance (0 before the first).
    pub generation: u64,
    /// Detail for `Unavailable`, `Failed`, `ReclaimUnknown` and a timed-out readiness handshake.
    pub reason: String,
    /// The last instance is confirmed gone (always true in `Stopped`).
    pub reclaim_confirmed: bool,
    /// The VM's vsock CID while it runs, else -1.
    pub cid: i32,
    /// Boot-time milliseconds when AVF started the VM, 0 when none runs.
    pub started_at_ms: u64,
    /// How the last instance ended, empty before the first ended.
    pub last_exit: String,
}

/// What happens once the instance is confirmed gone.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Stopped,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopPhase {
    /// Waiting for AVF to report the death.
    WaitingDeath,
    /// Waiting for the reclaim check.
    CheckingReclaim,
}

/// The state machine. See the module header.
#[derive(Debug)]
pub struct Machine {
    state: VmState,
    seq: u64,
    operation: u64,
    generation: u64,
    reason: String,
    reclaim_confirmed: bool,
    cid: i32,
    started_at_ms: u64,
    last_exit: String,
    readiness_configured: bool,
    stop_after_start: bool,
    stop_phase: StopPhase,
    outcome: Outcome,
}

impl Machine {
    /// A machine with no instance. `restored_leftover` is set when the service starts and finds
    /// a process of an instance an earlier run of the service started: it begins in
    /// `ReclaimUnknown` instead of `Stopped`.
    pub fn new(readiness_configured: bool, restored_leftover: Option<String>) -> Self {
        let mut machine = Self {
            state: VmState::Stopped,
            seq: 1,
            operation: 0,
            generation: 0,
            reason: String::new(),
            reclaim_confirmed: true,
            cid: -1,
            started_at_ms: 0,
            last_exit: String::new(),
            readiness_configured,
            stop_after_start: false,
            stop_phase: StopPhase::CheckingReclaim,
            outcome: Outcome::Stopped,
        };
        if let Some(detail) = restored_leftover {
            machine.state = VmState::ReclaimUnknown;
            machine.reclaim_confirmed = false;
            machine.reason = detail;
        }
        machine
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            state: self.state,
            seq: self.seq,
            operation: self.operation,
            generation: self.generation,
            reason: self.reason.clone(),
            reclaim_confirmed: self.reclaim_confirmed,
            cid: self.cid,
            started_at_ms: self.started_at_ms,
            last_exit: self.last_exit.clone(),
        }
    }

    fn changed(&mut self) {
        self.seq += 1;
    }

    fn next_operation(&mut self) -> u64 {
        self.operation += 1;
        self.operation
    }

    /// Re-evaluates `Stopped` against `Unavailable` while no instance exists. `preflight` is the
    /// result of the side-effect-free precondition check. Returns whether the snapshot changed.
    pub fn refresh_availability(&mut self, preflight: Result<(), String>) -> bool {
        match (self.state, preflight) {
            (VmState::Stopped, Err(reason)) => {
                self.state = VmState::Unavailable;
                self.reason = reason;
            }
            (VmState::Unavailable, Err(reason)) if reason != self.reason => {
                self.reason = reason;
            }
            (VmState::Unavailable, Ok(())) => {
                self.state = VmState::Stopped;
                self.reason.clear();
            }
            _ => return false,
        }
        self.changed();
        true
    }

    /// A request to start the instance. Returns the operation that serves it.
    pub fn request_start(
        &mut self,
        preflight: Result<(), String>,
    ) -> Result<(u64, Vec<Action>), Refused> {
        match self.state {
            VmState::Starting | VmState::RunningNotReady | VmState::Ready => {
                // Merged into the current instance; a stop remembered during the start is
                // cancelled by this newer request.
                if self.stop_after_start {
                    self.stop_after_start = false;
                    self.changed();
                }
                Ok((self.operation, Vec::new()))
            }
            VmState::Stopping => Err(Refused::Busy("the instance is stopping".into())),
            VmState::ReclaimUnknown => Err(Refused::ReclaimPending(format!(
                "the last instance is not confirmed gone: {}",
                self.reason
            ))),
            VmState::Stopped | VmState::Unavailable | VmState::Failed => {
                if let Err(reason) = preflight {
                    if self.state != VmState::Unavailable || self.reason != reason {
                        self.state = VmState::Unavailable;
                        self.reason = reason.clone();
                        self.changed();
                    }
                    return Err(Refused::Unavailable(reason));
                }
                let operation = self.next_operation();
                self.generation += 1;
                self.state = VmState::Starting;
                self.reason.clear();
                self.reclaim_confirmed = false;
                self.stop_after_start = false;
                self.changed();
                Ok((operation, vec![Action::CreateAndStart { generation: self.generation }]))
            }
        }
    }

    /// A request to stop the instance, or to re-check that a failed one is gone. Returns the
    /// operation that serves it; operation 0 means there was nothing to do.
    pub fn request_stop(&mut self) -> (u64, Vec<Action>) {
        let generation = self.generation;
        match self.state {
            VmState::Stopped | VmState::Unavailable | VmState::Failed => (0, Vec::new()),
            VmState::Starting => {
                let operation = self.next_operation();
                self.stop_after_start = true;
                self.changed();
                (operation, Vec::new())
            }
            VmState::RunningNotReady | VmState::Ready => {
                let operation = self.next_operation();
                self.begin_stop();
                (
                    operation,
                    vec![Action::Stop { generation }, Action::ArmStopDeadline { generation }],
                )
            }
            VmState::Stopping => (self.operation, Vec::new()),
            VmState::ReclaimUnknown => {
                let operation = self.next_operation();
                // The outcome stays what it was when the confirmation was missed: "stopped" after
                // a stop, the failure after an unexpected death.
                self.state = VmState::Stopping;
                self.stop_phase = StopPhase::CheckingReclaim;
                self.changed();
                (operation, vec![Action::CheckReclaim { generation }])
            }
        }
    }

    fn begin_stop(&mut self) {
        self.state = VmState::Stopping;
        self.stop_phase = StopPhase::WaitingDeath;
        self.outcome = Outcome::Stopped;
        self.changed();
    }

    /// An event from the executor. Events of another generation are ignored.
    pub fn on_event(&mut self, event: Event) -> Vec<Action> {
        let current = self.generation;
        match event {
            Event::Started { generation, cid, at_ms } if generation == current => {
                if self.state != VmState::Starting {
                    return Vec::new();
                }
                self.cid = cid;
                self.started_at_ms = at_ms;
                if self.stop_after_start {
                    self.stop_after_start = false;
                    self.begin_stop();
                    return vec![
                        Action::Stop { generation },
                        Action::ArmStopDeadline { generation },
                    ];
                }
                self.state = VmState::RunningNotReady;
                self.changed();
                if self.readiness_configured {
                    vec![Action::ProbeReady { generation }]
                } else {
                    Vec::new()
                }
            }
            Event::StartFailed { generation, reason } if generation == current => {
                if self.state != VmState::Starting {
                    return Vec::new();
                }
                self.stop_after_start = false;
                self.state = VmState::Stopping;
                self.stop_phase = StopPhase::CheckingReclaim;
                self.outcome = Outcome::Failed(format!("start failed: {reason}"));
                self.changed();
                vec![Action::CheckReclaim { generation }]
            }
            Event::Ready { generation } if generation == current => {
                if self.state == VmState::RunningNotReady {
                    self.state = VmState::Ready;
                    self.reason.clear();
                    self.changed();
                }
                Vec::new()
            }
            Event::ReadyTimedOut { generation } if generation == current => {
                if self.state == VmState::RunningNotReady {
                    self.reason = "the readiness handshake timed out".into();
                    self.changed();
                }
                Vec::new()
            }
            Event::Died { generation, exit } if generation == current => {
                let requested = self.state == VmState::Stopping
                    || (self.state == VmState::ReclaimUnknown && self.outcome == Outcome::Stopped);
                let waiting = match self.state {
                    VmState::RunningNotReady | VmState::Ready => {
                        // Nobody asked for a stop: only the guest's own shutdown is a normal end.
                        self.outcome = match &exit {
                            Exit::GuestShutdown => Outcome::Stopped,
                            Exit::Killed => Outcome::Failed("the VM was killed".into()),
                            Exit::Abnormal(reason) => Outcome::Failed(reason.clone()),
                        };
                        true
                    }
                    VmState::Stopping => self.stop_phase == StopPhase::WaitingDeath,
                    // A stop that timed out: the death resolves it; the outcome stays "stopped".
                    VmState::ReclaimUnknown => true,
                    // The VM died before AVF said it started.
                    VmState::Starting => {
                        self.stop_after_start = false;
                        self.outcome = Outcome::Failed(format!("died while starting: {exit}"));
                        true
                    }
                    _ => false,
                };
                if !waiting {
                    return Vec::new();
                }
                // A death that a stop asked for reads as such, whatever AVF calls it.
                self.last_exit =
                    if requested { LAST_EXIT_STOPPED.to_owned() } else { exit.to_string() };
                self.cid = -1;
                self.started_at_ms = 0;
                self.state = VmState::Stopping;
                self.stop_phase = StopPhase::CheckingReclaim;
                self.changed();
                vec![Action::CheckReclaim { generation }]
            }
            Event::StopTimedOut { generation } if generation == current => {
                if self.state == VmState::Stopping && self.stop_phase == StopPhase::WaitingDeath {
                    self.state = VmState::ReclaimUnknown;
                    self.reclaim_confirmed = false;
                    self.reason = "the VM did not exit within the stop deadline".into();
                    self.changed();
                }
                Vec::new()
            }
            Event::ReclaimChecked { generation, gone, detail } if generation == current => {
                if !(self.state == VmState::Stopping
                    && self.stop_phase == StopPhase::CheckingReclaim)
                {
                    return Vec::new();
                }
                if gone {
                    self.reclaim_confirmed = true;
                    match std::mem::replace(&mut self.outcome, Outcome::Stopped) {
                        Outcome::Stopped => {
                            self.state = VmState::Stopped;
                            self.reason.clear();
                        }
                        Outcome::Failed(reason) => {
                            self.state = VmState::Failed;
                            self.reason = reason;
                        }
                    }
                } else {
                    self.state = VmState::ReclaimUnknown;
                    self.reclaim_confirmed = false;
                    self.reason = detail;
                }
                self.changed();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(m: &mut Machine) -> u64 {
        let (_, actions) = m.request_start(Ok(())).unwrap();
        let generation = m.snapshot().generation;
        assert_eq!(actions, vec![Action::CreateAndStart { generation }]);
        m.on_event(Event::Started { generation, cid: 2048, at_ms: 10 });
        generation
    }

    fn confirm_gone(m: &mut Machine, generation: u64) {
        m.on_event(Event::ReclaimChecked { generation, gone: true, detail: String::new() });
    }

    #[test]
    fn starts_into_running_not_ready_without_a_probe() {
        let mut m = Machine::new(false, None);
        let generation = started(&mut m);
        let s = m.snapshot();
        assert_eq!(s.state, VmState::RunningNotReady);
        assert_eq!((s.cid, s.started_at_ms, s.generation), (2048, 10, generation));
        assert!(!s.reclaim_confirmed);
    }

    #[test]
    fn probe_moves_to_ready_and_timeout_only_annotates() {
        let mut m = Machine::new(true, None);
        m.request_start(Ok(())).unwrap();
        let g = m.snapshot().generation;
        let actions = m.on_event(Event::Started { generation: g, cid: 3, at_ms: 1 });
        assert_eq!(actions, vec![Action::ProbeReady { generation: g }]);
        m.on_event(Event::ReadyTimedOut { generation: g });
        assert_eq!(m.snapshot().state, VmState::RunningNotReady);
        assert!(!m.snapshot().reason.is_empty());
        m.on_event(Event::Ready { generation: g });
        assert_eq!(m.snapshot().state, VmState::Ready);
        assert!(m.snapshot().reason.is_empty());
    }

    #[test]
    fn duplicate_start_is_merged() {
        let mut m = Machine::new(false, None);
        let (op, _) = m.request_start(Ok(())).unwrap();
        let (op2, actions) = m.request_start(Ok(())).unwrap();
        assert_eq!((op, actions.len()), (op2, 0));
        let g = m.snapshot().generation;
        m.on_event(Event::Started { generation: g, cid: 3, at_ms: 1 });
        let (op3, actions) = m.request_start(Ok(())).unwrap();
        assert_eq!((op3, actions.len()), (op, 0));
        assert_eq!(m.snapshot().generation, g, "no second instance");
    }

    #[test]
    fn stop_confirms_before_stopped() {
        let mut m = Machine::new(false, None);
        let g = started(&mut m);
        let (op, actions) = m.request_stop();
        assert!(op > 0);
        assert_eq!(
            actions,
            vec![Action::Stop { generation: g }, Action::ArmStopDeadline { generation: g }]
        );
        assert_eq!(m.snapshot().state, VmState::Stopping);
        let actions = m.on_event(Event::Died { generation: g, exit: Exit::Killed });
        assert_eq!(actions, vec![Action::CheckReclaim { generation: g }]);
        assert_eq!(m.snapshot().state, VmState::Stopping, "death alone is not reclaim");
        confirm_gone(&mut m, g);
        let s = m.snapshot();
        assert_eq!((s.state, s.reclaim_confirmed, s.cid), (VmState::Stopped, true, -1));
        assert_eq!(s.last_exit, LAST_EXIT_STOPPED);
    }

    #[test]
    fn start_is_refused_until_gone() {
        let mut m = Machine::new(false, None);
        let g = started(&mut m);
        m.request_stop();
        assert!(matches!(m.request_start(Ok(())), Err(Refused::Busy(_))));
        m.on_event(Event::Died { generation: g, exit: Exit::Killed });
        assert!(matches!(m.request_start(Ok(())), Err(Refused::Busy(_))));
        confirm_gone(&mut m, g);
        assert!(m.request_start(Ok(())).is_ok());
        assert_eq!(m.snapshot().generation, g + 1);
    }

    #[test]
    fn stop_timeout_then_late_death() {
        let mut m = Machine::new(false, None);
        let g = started(&mut m);
        m.request_stop();
        m.on_event(Event::StopTimedOut { generation: g });
        assert_eq!(m.snapshot().state, VmState::ReclaimUnknown);
        assert!(matches!(m.request_start(Ok(())), Err(Refused::ReclaimPending(_))));
        let actions = m.on_event(Event::Died { generation: g, exit: Exit::Killed });
        assert_eq!(actions, vec![Action::CheckReclaim { generation: g }]);
        confirm_gone(&mut m, g);
        assert_eq!(m.snapshot().state, VmState::Stopped);
    }

    #[test]
    fn leftover_process_keeps_reclaim_unknown_and_recheck_resolves() {
        let mut m = Machine::new(false, None);
        let g = started(&mut m);
        m.request_stop();
        m.on_event(Event::Died { generation: g, exit: Exit::Killed });
        m.on_event(Event::ReclaimChecked { generation: g, gone: false, detail: "pid 7".into() });
        let s = m.snapshot();
        assert_eq!((s.state, s.reason.as_str()), (VmState::ReclaimUnknown, "pid 7"));
        let (op, actions) = m.request_stop();
        assert!(op > 0);
        assert_eq!(actions, vec![Action::CheckReclaim { generation: g }]);
        confirm_gone(&mut m, g);
        assert_eq!(m.snapshot().state, VmState::Stopped);
    }

    #[test]
    fn unexpected_death_is_failed_after_confirmation() {
        let mut m = Machine::new(false, None);
        let g = started(&mut m);
        m.on_event(Event::Died { generation: g, exit: Exit::Abnormal("crash".into()) });
        assert_eq!(m.snapshot().state, VmState::Stopping);
        assert!(matches!(m.request_start(Ok(())), Err(Refused::Busy(_))));
        confirm_gone(&mut m, g);
        let s = m.snapshot();
        assert_eq!(
            (s.state, s.reason.as_str(), s.reclaim_confirmed),
            (VmState::Failed, "crash", true)
        );
        assert_eq!(s.last_exit, "crash");
        // A confirmed failure may be retried.
        assert!(m.request_start(Ok(())).is_ok());
    }

    #[test]
    fn outside_kill_while_running_is_failed() {
        let mut m = Machine::new(false, None);
        let g = started(&mut m);
        m.on_event(Event::Died { generation: g, exit: Exit::Killed });
        confirm_gone(&mut m, g);
        let s = m.snapshot();
        assert_eq!((s.state, s.last_exit.as_str()), (VmState::Failed, "killed"));
    }

    #[test]
    fn guest_shutdown_ends_stopped() {
        let mut m = Machine::new(false, None);
        let g = started(&mut m);
        m.on_event(Event::Died { generation: g, exit: Exit::GuestShutdown });
        confirm_gone(&mut m, g);
        let s = m.snapshot();
        assert_eq!((s.state, s.last_exit.as_str()), (VmState::Stopped, "guest shut down"));
    }

    #[test]
    fn start_failure_is_confirmed_then_failed() {
        let mut m = Machine::new(false, None);
        m.request_start(Ok(())).unwrap();
        let g = m.snapshot().generation;
        let actions = m.on_event(Event::StartFailed { generation: g, reason: "denied".into() });
        assert_eq!(actions, vec![Action::CheckReclaim { generation: g }]);
        confirm_gone(&mut m, g);
        let s = m.snapshot();
        assert_eq!(s.state, VmState::Failed);
        assert!(s.reason.contains("denied"));
    }

    #[test]
    fn stop_during_start_runs_after_start() {
        let mut m = Machine::new(false, None);
        m.request_start(Ok(())).unwrap();
        let g = m.snapshot().generation;
        let (op, actions) = m.request_stop();
        assert!(op > 0 && actions.is_empty());
        assert_eq!(m.snapshot().state, VmState::Starting);
        let actions = m.on_event(Event::Started { generation: g, cid: 3, at_ms: 1 });
        assert_eq!(
            actions,
            vec![Action::Stop { generation: g }, Action::ArmStopDeadline { generation: g }]
        );
        assert_eq!(m.snapshot().state, VmState::Stopping);
    }

    #[test]
    fn start_after_stop_during_start_cancels_the_stop() {
        let mut m = Machine::new(false, None);
        m.request_start(Ok(())).unwrap();
        let g = m.snapshot().generation;
        m.request_stop();
        m.request_start(Ok(())).unwrap();
        let actions = m.on_event(Event::Started { generation: g, cid: 3, at_ms: 1 });
        assert!(actions.is_empty());
        assert_eq!(m.snapshot().state, VmState::RunningNotReady);
    }

    #[test]
    fn stop_during_start_then_start_failure() {
        let mut m = Machine::new(false, None);
        m.request_start(Ok(())).unwrap();
        let g = m.snapshot().generation;
        m.request_stop();
        m.on_event(Event::StartFailed { generation: g, reason: "x".into() });
        confirm_gone(&mut m, g);
        assert_eq!(m.snapshot().state, VmState::Failed);
    }

    #[test]
    fn stale_generation_events_are_ignored() {
        let mut m = Machine::new(false, None);
        let g = started(&mut m);
        m.request_stop();
        m.on_event(Event::Died { generation: g, exit: Exit::Killed });
        confirm_gone(&mut m, g);
        let g2 = started(&mut m);
        assert_eq!(g2, g + 1);
        let before = m.snapshot();
        for event in [
            Event::Died { generation: g, exit: Exit::Abnormal("old".into()) },
            Event::StopTimedOut { generation: g },
            Event::ReclaimChecked { generation: g, gone: false, detail: "old".into() },
            Event::Ready { generation: g },
        ] {
            assert!(m.on_event(event).is_empty());
        }
        assert_eq!(m.snapshot(), before);
    }

    #[test]
    fn stop_deadline_after_death_is_ignored() {
        let mut m = Machine::new(false, None);
        let g = started(&mut m);
        m.request_stop();
        m.on_event(Event::Died { generation: g, exit: Exit::Killed });
        m.on_event(Event::StopTimedOut { generation: g });
        assert_eq!(m.snapshot().state, VmState::Stopping);
        confirm_gone(&mut m, g);
        assert_eq!(m.snapshot().state, VmState::Stopped);
    }

    #[test]
    fn preflight_failure_is_unavailable_and_recovers() {
        let mut m = Machine::new(false, None);
        assert!(matches!(
            m.request_start(Err("no /dev/xhyper".into())),
            Err(Refused::Unavailable(_))
        ));
        assert_eq!(m.snapshot().state, VmState::Unavailable);
        assert!(m.refresh_availability(Ok(())));
        assert_eq!(m.snapshot().state, VmState::Stopped);
        assert!(m.refresh_availability(Err("no kernel".into())));
        assert_eq!(m.snapshot().reason, "no kernel");
        assert!(!m.refresh_availability(Err("no kernel".into())), "no change, no seq bump");
        // Availability never touches a running instance.
        m.refresh_availability(Ok(()));
        started(&mut m);
        assert!(!m.refresh_availability(Err("gone".into())));
        assert_eq!(m.snapshot().state, VmState::RunningNotReady);
    }

    #[test]
    fn restored_leftover_starts_reclaim_unknown() {
        let mut m = Machine::new(false, Some("pid 9 from an earlier run".into()));
        assert_eq!(m.snapshot().state, VmState::ReclaimUnknown);
        assert!(matches!(m.request_start(Ok(())), Err(Refused::ReclaimPending(_))));
        let (_, actions) = m.request_stop();
        assert_eq!(actions, vec![Action::CheckReclaim { generation: 0 }]);
        confirm_gone(&mut m, 0);
        assert_eq!(m.snapshot().state, VmState::Stopped);
    }

    #[test]
    fn seq_increases_on_every_visible_change() {
        let mut m = Machine::new(false, None);
        let mut last = m.snapshot().seq;
        let mut step = |m: &mut Machine| {
            let seq = m.snapshot().seq;
            assert!(seq > last, "seq did not increase");
            last = seq;
        };
        m.request_start(Ok(())).unwrap();
        step(&mut m);
        let g = m.snapshot().generation;
        m.on_event(Event::Started { generation: g, cid: 1, at_ms: 1 });
        step(&mut m);
        m.request_stop();
        step(&mut m);
        m.on_event(Event::Died { generation: g, exit: Exit::Killed });
        step(&mut m);
        confirm_gone(&mut m, g);
        step(&mut m);
    }

    #[test]
    fn stop_with_nothing_running_is_a_no_op() {
        let mut m = Machine::new(false, None);
        let before = m.snapshot();
        assert_eq!(m.request_stop(), (0, Vec::new()));
        assert_eq!(m.snapshot(), before);
    }
}
