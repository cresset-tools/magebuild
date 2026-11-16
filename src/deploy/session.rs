//! Keeping the agent alive when the runner goes away.
//!
//! The agent runs under ssh: stdout is the protocol, stderr is what the runner
//! shows. When the connection drops (CI cancelled or timed out, a network
//! blip), writes to either fail. Before the deploy is committed that should
//! stop it, and does: the store is untouched. After it is committed (an upgrade
//! has started, or the swap is next) stopping halfway is worse than finishing,
//! so from then on a lost runner is only logged:
//!
//! - SIGHUP is ignored, by the agent and, inherited across exec, by the PHP
//!   it starts;
//! - fd 2 is a pipe the agent drains itself, so PHP never writes to a dead
//!   ssh pipe (and dies of SIGPIPE), and `eprintln!` never fails;
//! - everything on it is copied to `.dep/last-deploy.log`, which says how a
//!   deploy ended when nobody was watching.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

/// Set once the deploy must run to the end even without a runner.
static COMMITTED: AtomicBool = AtomicBool::new(false);
/// Set when a write to the runner first fails.
static RUNNER_GONE: AtomicBool = AtomicBool::new(false);
static LOG: Mutex<Option<File>> = Mutex::new(None);
static FORWARDER: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();

/// From here on, losing the runner no longer stops the deploy.
pub fn commit() {
    COMMITTED.store(true, Ordering::SeqCst);
}

pub fn runner_gone() -> bool {
    RUNNER_GONE.load(Ordering::SeqCst)
}

/// Ignore SIGHUP and put a pipe behind fd 2 that a thread forwards to the
/// original stderr and the log. Best effort: on failure stderr stays as is.
pub fn detach() {
    // SAFETY: plain libc calls on fds this process owns; the new fds are
    // owned by the thread below or closed here.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        let mut fds = [0; 2];
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return;
        }
        let original = libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3);
        if original < 0 || libc::dup2(fds[1], 2) < 0 {
            libc::close(fds[0]);
            libc::close(fds[1]);
            if original >= 0 {
                libc::close(original);
            }
            return;
        }
        libc::close(fds[1]);
        let reader = File::from(OwnedFd::from_raw_fd(fds[0]));
        let original = File::from(OwnedFd::from_raw_fd(original));
        let handle = std::thread::spawn(move || forward(reader, original));
        let _ = FORWARDER.set(Mutex::new(Some(handle)));
    }
}

/// Before exiting: close our end of the stderr pipe and let the forwarder
/// pass on what is left, so the runner sees the last lines.
pub fn finish() {
    let Some(handle) = FORWARDER.get().and_then(|m| m.lock().ok()?.take()) else {
        return;
    };
    // SAFETY: points fd 2 at /dev/null, closing this process's write end of
    // the pipe; the forwarder then reads to EOF once children are done.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if null < 0 {
            return;
        }
        libc::dup2(null, 2);
        libc::close(null);
    }
    let _ = handle.join();
}

fn forward(mut from: File, mut to: File) {
    let mut buf = [0u8; 8192];
    let mut to_ok = true;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if to_ok && to.write_all(&buf[..n]).is_err() {
            to_ok = false;
            RUNNER_GONE.store(true, Ordering::SeqCst);
        }
        log_bytes(&buf[..n]);
    }
}

/// Start `.dep/last-deploy.log` under the deploy root, replacing the last one.
pub fn open_log(root: &Path) {
    if let Ok(f) = File::create(root.join(".dep/last-deploy.log"))
        && let Ok(mut log) = LOG.lock()
    {
        *log = Some(f);
    }
}

/// A line for the log only (the runner gets the same as a protocol event).
pub fn log_line(line: &str) {
    log_bytes(format!("{line}\n").as_bytes());
}

fn log_bytes(b: &[u8]) {
    if let Ok(mut log) = LOG.lock()
        && let Some(f) = log.as_mut()
    {
        let _ = f.write_all(b);
    }
}

/// The protocol side of stdout. Before the deploy is committed a failed write
/// is an error, as usual; after, it is noted once and the deploy carries on.
pub struct ToRunner<W: Write>(pub W);

impl<W: Write> ToRunner<W> {
    fn lost(&mut self, e: std::io::Error) -> std::io::Result<()> {
        if !COMMITTED.load(Ordering::SeqCst) {
            return Err(e);
        }
        if !RUNNER_GONE.swap(true, Ordering::SeqCst) {
            log_line(&format!(
                "agent: lost the runner ({e}); finishing the deploy on the host"
            ));
        }
        Ok(())
    }
}

impl<W: Write> Write for ToRunner<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if runner_gone() && COMMITTED.load(Ordering::SeqCst) {
            return Ok(buf.len());
        }
        match self.0.write(buf) {
            Ok(n) => Ok(n),
            Err(e) => self.lost(e).map(|()| buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if runner_gone() && COMMITTED.load(Ordering::SeqCst) {
            return Ok(());
        }
        match self.0.flush() {
            Ok(()) => Ok(()),
            Err(e) => self.lost(e),
        }
    }
}
