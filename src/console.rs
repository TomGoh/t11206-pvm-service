// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! The serial console of the x-kernel VM, kept by this service for its clients.
//!
//! Each instance gets two pipes: crosvm writes the guest's serial output into one and reads the
//! guest's serial input from the other. The service keeps the last [`HISTORY_CAPACITY`] bytes of
//! the output of the latest instance, so a client that attaches late (or again, after the app was
//! closed) still sees what came before, and forwards every new piece to the attached listeners.
//!
//! Output is addressed by (instance, offset): the instance is the controller's generation, the
//! offset counts the bytes of that instance's output from 0. Attaching returns the kept history
//! and registers the listener under one lock that delivering also takes, so nothing falls between
//! the two; a client that sees a gap in the offsets attaches again. A new instance starts with an
//! empty piece at offset 0, which tells clients to clear their view.
//!
//! Input goes to the running instance only. The input pipe is non-blocking: when the guest does
//! not read, a write is refused as full instead of blocking the caller's Binder thread.
//!
//! The output reader of an instance stops when the output pipe reaches end of file or when the
//! instance ends; at the end it first reads what is left in the pipe (the guest's last words).

use log::{info, warn};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The output kept per instance.
pub const HISTORY_CAPACITY: usize = 64 * 1024;
/// The largest input accepted per write.
pub const MAX_INPUT: usize = 4096;
/// Output that arrives within this time of the previous piece is delivered together, so a
/// byte-by-byte writer does not cost one callback per byte.
const COALESCE: Duration = Duration::from_millis(15);
const READ_CHUNK: usize = 4096;

/// A piece of output of one instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub instance: u64,
    pub offset: u64,
    pub data: Vec<u8>,
}

/// Why a delivery failed.
#[derive(Debug)]
pub enum SendError {
    /// The listener is gone and is dropped.
    Gone,
    /// This piece was lost; the listener stays and sees a gap in the offsets.
    Lost(String),
}

/// A client that receives the output.
pub trait Listener: Send {
    fn send(&self, output: &Output) -> Result<(), SendError>;
}

/// Why input was not taken.
#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
    /// No instance runs.
    NotRunning,
    /// The guest has not read the earlier input; nothing or only a prefix was written.
    Full {
        written: usize,
    },
    TooLarge,
    Io(String),
}

/// The output of the latest instance, bounded to its last `capacity` bytes.
struct History {
    capacity: usize,
    instance: u64,
    /// Offset of `data[0]`.
    start: u64,
    data: VecDeque<u8>,
}

impl History {
    fn new(capacity: usize) -> Self {
        Self { capacity, instance: 0, start: 0, data: VecDeque::new() }
    }

    fn begin(&mut self, instance: u64) {
        self.instance = instance;
        self.start = 0;
        self.data.clear();
    }

    fn end_offset(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    /// Appends output and returns the offset it starts at.
    fn append(&mut self, bytes: &[u8]) -> u64 {
        let offset = self.end_offset();
        self.data.extend(bytes);
        let excess = self.data.len().saturating_sub(self.capacity);
        if excess > 0 {
            self.data.drain(..excess);
            self.start += excess as u64;
        }
        offset
    }

    fn snapshot(&self) -> Output {
        Output {
            instance: self.instance,
            offset: self.start,
            data: self.data.iter().copied().collect(),
        }
    }
}

struct Running {
    instance: u64,
    input: File,
    /// Dropping it wakes the reader to drain and exit.
    _stop: OwnedFd,
}

struct Inner {
    history: History,
    running: Option<Running>,
    listeners: Vec<(u64, Box<dyn Listener>)>,
    next_id: u64,
}

/// The guest-side ends of an instance's pipes, for crosvm.
pub struct GuestEnds {
    /// crosvm writes the serial output here.
    pub output: File,
    /// crosvm reads the serial input from here.
    pub input: File,
}

pub struct Console {
    inner: Mutex<Inner>,
}

impl Console {
    pub fn new() -> Arc<Self> {
        Self::with_capacity(HISTORY_CAPACITY)
    }

    fn with_capacity(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                history: History::new(capacity),
                running: None,
                listeners: Vec::new(),
                next_id: 0,
            }),
        })
    }

    /// Starts the console of a new instance: clears the history, tells the listeners, and
    /// returns the pipe ends crosvm uses. The output reader runs until [`Console::end`] or end
    /// of file.
    pub fn begin(self: &Arc<Self>, instance: u64) -> io::Result<GuestEnds> {
        let (out_read, out_write) = pipe()?;
        let (in_read, in_write) = pipe()?;
        let (stop_read, stop_write) = pipe()?;
        set_nonblocking(&out_read)?;
        set_nonblocking(&in_write)?;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.history.begin(instance);
            inner.running =
                Some(Running { instance, input: File::from(in_write), _stop: stop_write });
            deliver(&mut inner, &Output { instance, offset: 0, data: Vec::new() });
        }
        let console = Arc::downgrade(self);
        thread::Builder::new().name("xkvm-console".into()).spawn(move || {
            read_output(File::from(out_read), stop_read, |bytes| {
                if let Some(console) = console.upgrade() {
                    console.append(instance, bytes);
                }
            });
            info!("console reader of instance {instance} ended");
        })?;
        Ok(GuestEnds { output: File::from(out_write), input: File::from(in_read) })
    }

    /// Ends the console of `instance`: input is refused from now on, and the reader delivers
    /// what is left in the pipe and exits. The history stays until the next instance.
    pub fn end(&self, instance: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.running.as_ref().is_some_and(|r| r.instance == instance) {
            inner.running = None;
        }
    }

    fn append(&self, instance: u64, bytes: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        if inner.history.instance != instance {
            // A reader that outlived its instance; its output belongs to no console now.
            return;
        }
        let offset = inner.history.append(bytes);
        deliver(&mut inner, &Output { instance, offset, data: bytes.to_vec() });
    }

    /// Registers a listener and returns the kept output it starts from, and the listener's id.
    pub fn attach(&self, listener: Box<dyn Listener>) -> (u64, Output) {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.listeners.push((id, listener));
        (id, inner.history.snapshot())
    }

    pub fn detach(&self, id: u64) {
        self.inner.lock().unwrap().listeners.retain(|(i, _)| *i != id);
    }

    /// Writes input to the running instance.
    pub fn write(&self, bytes: &[u8]) -> Result<(), InputError> {
        if bytes.len() > MAX_INPUT {
            return Err(InputError::TooLarge);
        }
        let mut inner = self.inner.lock().unwrap();
        let Some(running) = inner.running.as_mut() else {
            return Err(InputError::NotRunning);
        };
        let mut written = 0;
        while written < bytes.len() {
            match running.input.write(&bytes[written..]) {
                Ok(0) => return Err(InputError::Full { written }),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(InputError::Full { written })
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(InputError::Io(e.to_string())),
            }
        }
        Ok(())
    }
}

fn deliver(inner: &mut Inner, output: &Output) {
    inner.listeners.retain(|(id, listener)| match listener.send(output) {
        Ok(()) => true,
        Err(SendError::Gone) => {
            info!("console listener {id} is gone");
            false
        }
        Err(SendError::Lost(e)) => {
            warn!("console output at {} lost for listener {id}: {e}", output.offset);
            true
        }
    });
}

/// Reads `out` until end of file or until `stop` is readable (its write end closed); then reads
/// what is left. Pieces that follow each other within [`COALESCE`] are handed over together.
fn read_output(mut out: File, stop: OwnedFd, mut emit: impl FnMut(&[u8])) {
    let mut piece = Vec::with_capacity(READ_CHUNK);
    let mut buf = [0u8; READ_CHUNK];
    loop {
        let timeout = if piece.is_empty() { None } else { Some(COALESCE) };
        let (out_ready, stopped) = match wait(&out, &stop, timeout) {
            Ok(r) => r,
            Err(e) => {
                warn!("console wait failed: {e}");
                break;
            }
        };
        if !out_ready && !stopped {
            // The coalescing window passed.
            emit(&piece);
            piece.clear();
            continue;
        }
        if out_ready {
            match out.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    piece.extend_from_slice(&buf[..n]);
                    if piece.len() >= READ_CHUNK {
                        emit(&piece);
                        piece.clear();
                    }
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    warn!("console read failed: {e}");
                    break;
                }
            }
        }
        if stopped {
            // Drain: the instance ended, so whatever it wrote is in the pipe already.
            loop {
                match out.read(&mut buf) {
                    Ok(n) if n > 0 => piece.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    _ => break,
                }
            }
            break;
        }
    }
    if !piece.is_empty() {
        emit(&piece);
    }
}

/// Waits until `out` has data or end of file, `stop` is readable, or the timeout passes.
fn wait(out: &File, stop: &OwnedFd, timeout: Option<Duration>) -> io::Result<(bool, bool)> {
    let mut fds = [
        libc::pollfd { fd: out.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    let timeout_ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
    loop {
        // SAFETY: `fds` is a valid array of two pollfd for the duration of the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if n >= 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    let ready =
        |revents: libc::c_short| revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
    Ok((ready(fds[0].revents), ready(fds[1].revents)))
}

/// A close-on-exec pipe: (read end, write end).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe() writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe() succeeded, so both are open descriptors that nothing else owns.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read, &write] {
        // SAFETY: fcntl on a descriptor this function owns.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor the caller owns.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::time::Instant;

    struct Collect(Mutex<Sender<Output>>);

    impl Listener for Collect {
        fn send(&self, output: &Output) -> Result<(), SendError> {
            self.0.lock().unwrap().send(output.clone()).map_err(|_| SendError::Gone)
        }
    }

    fn collector() -> (Box<dyn Listener>, Receiver<Output>) {
        let (tx, rx) = channel();
        (Box::new(Collect(Mutex::new(tx))), rx)
    }

    /// Collects output until `want` bytes (after `from`) arrived or a second passed.
    fn gather(rx: &Receiver<Output>, from: u64, want: usize) -> Vec<u8> {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        while got.len() < want {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(o) = rx.recv_timeout(left) else { break };
            assert_eq!(o.offset, from + got.len() as u64, "offsets are contiguous");
            got.extend(o.data);
        }
        got
    }

    #[test]
    fn history_keeps_the_last_bytes_with_their_offsets() {
        let mut h = History::new(8);
        h.begin(3);
        assert_eq!(h.append(b"hello"), 0);
        assert_eq!(h.append(b" world"), 5);
        let s = h.snapshot();
        assert_eq!((s.instance, s.offset, s.data.as_slice()), (3, 3, &b"lo world"[..]));
        h.begin(4);
        let s = h.snapshot();
        assert_eq!((s.instance, s.offset, s.data.len()), (4, 0, 0));
    }

    #[test]
    fn output_reaches_listeners_and_history() {
        let console = Console::new();
        let (listener, rx) = collector();
        console.attach(listener);
        let mut ends = console.begin(1).unwrap();
        let reset = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(reset, Output { instance: 1, offset: 0, data: Vec::new() });
        ends.output.write_all(b"Welcome\r\n# ").unwrap();
        assert_eq!(gather(&rx, 0, 11), b"Welcome\r\n# ");
        // A late client starts from the history.
        let (late, late_rx) = collector();
        let (_, history) = console.attach(late);
        assert_eq!(history, Output { instance: 1, offset: 0, data: b"Welcome\r\n# ".to_vec() });
        ends.output.write_all(b"ls").unwrap();
        assert_eq!(gather(&late_rx, 11, 2), b"ls");
        assert_eq!(gather(&rx, 11, 2), b"ls");
    }

    #[test]
    fn input_goes_to_the_running_instance_only() {
        let console = Console::new();
        assert_eq!(console.write(b"x"), Err(InputError::NotRunning));
        let mut ends = console.begin(1).unwrap();
        console.write(b"nproc\n").unwrap();
        let mut got = [0u8; 6];
        ends.input.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"nproc\n");
        console.end(1);
        assert_eq!(console.write(b"x"), Err(InputError::NotRunning));
        assert_eq!(console.write(&vec![b'a'; MAX_INPUT + 1]), Err(InputError::TooLarge));
    }

    #[test]
    fn full_input_is_refused_not_blocked() {
        let console = Console::new();
        let _ends = console.begin(1).unwrap();
        let chunk = vec![b'a'; MAX_INPUT];
        let mut refused = None;
        // A pipe holds far less than 64 MiB; nobody reads it.
        for _ in 0..(64 * 1024 * 1024 / MAX_INPUT) {
            if let Err(e) = console.write(&chunk) {
                refused = Some(e);
                break;
            }
        }
        assert!(matches!(refused, Some(InputError::Full { .. })), "{refused:?}");
    }

    #[test]
    fn ending_drains_what_is_left_and_stops_the_reader() {
        let console = Console::new();
        let (listener, rx) = collector();
        console.attach(listener);
        let mut ends = console.begin(1).unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        ends.output.write_all(b"panic: last words").unwrap();
        // The guest side stays open (as if a manager still held a copy): only end() stops it.
        console.end(1);
        assert_eq!(gather(&rx, 0, 17), b"panic: last words");
        thread::sleep(Duration::from_millis(100));
        // Nobody reads the pipe any more: the write fails (EPIPE) or is never delivered.
        let _ = ends.output.write_all(b"late");
        thread::sleep(Duration::from_millis(100));
        assert!(rx.try_recv().is_err(), "the reader stopped");
        assert_eq!(console.attach(collector().0).1.data, b"panic: last words");
    }

    #[test]
    fn end_of_file_stops_the_reader_and_a_new_instance_clears() {
        let console = Console::new();
        let (listener, rx) = collector();
        console.attach(listener);
        let mut ends = console.begin(1).unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        ends.output.write_all(b"bye").unwrap();
        drop(ends);
        assert_eq!(gather(&rx, 0, 3), b"bye");
        console.end(1);
        let _ends2 = console.begin(2).unwrap();
        let reset = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(reset, Output { instance: 2, offset: 0, data: Vec::new() });
        let (_, history) = console.attach(collector().0);
        assert_eq!((history.instance, history.data.len()), (2, 0));
    }

    #[test]
    fn a_gone_listener_is_dropped_and_others_stay() {
        struct GoneAfterFirst(Mutex<u32>);
        impl Listener for GoneAfterFirst {
            fn send(&self, _: &Output) -> Result<(), SendError> {
                let mut n = self.0.lock().unwrap();
                *n += 1;
                if *n > 1 {
                    Err(SendError::Gone)
                } else {
                    Ok(())
                }
            }
        }
        let console = Console::new();
        console.attach(Box::new(GoneAfterFirst(Mutex::new(0))));
        let (listener, rx) = collector();
        let (id, _) = console.attach(listener);
        let mut ends = console.begin(1).unwrap();
        ends.output.write_all(b"a").unwrap();
        assert_eq!(gather(&rx, 0, 1), b"a");
        assert_eq!(console.inner.lock().unwrap().listeners.len(), 1);
        console.detach(id);
        assert!(console.inner.lock().unwrap().listeners.is_empty());
    }
}
