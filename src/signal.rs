use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);
static WAKE_WRITE: AtomicI32 = AtomicI32::new(-1);

/// Termination signals are recorded; every handled signal (including SIGCHLD
/// and SIGWINCH) writes one byte so a sleeping `Events::wait` returns at once.
extern "C" fn on_signal(signal: libc::c_int) {
    if signal != libc::SIGCHLD && signal != libc::SIGWINCH {
        LAST_SIGNAL.store(signal, Ordering::SeqCst);
        PENDING_SIGNAL.store(signal, Ordering::SeqCst);
    }
    let byte = [1u8];
    let _ = unsafe { libc::write(WAKE_WRITE.load(Ordering::Relaxed), byte.as_ptr().cast(), 1) };
}

/// The most recent termination signal observed by a handler, or 0.
pub fn last_signal() -> i32 {
    LAST_SIGNAL.load(Ordering::SeqCst)
}

/// Self-pipe woken by signal handlers. The owning thread sleeps in poll(2)
/// with no periodic wakeups, so an idle Poros costs no CPU.
pub struct Events {
    read_fd: libc::c_int,
}

/// Most descriptors `Events::wait_with` watches besides the signal pipe.
pub const MAX_EXTRA: usize = 2;

impl Events {
    /// Installs handlers for SIGINT, SIGTERM, SIGHUP, SIGCHLD, plus `extra`
    /// signals that should only wake the waiter.
    pub fn install(extra: &[libc::c_int]) -> Result<Self, String> {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(format!(
                "create signal pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        for fd in fds {
            set_flags(fd)?;
        }
        WAKE_WRITE.store(write_fd, Ordering::SeqCst);
        let base = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGCHLD];
        for &signal in base.iter().chain(extra) {
            install_handler(signal)?;
        }
        Ok(Self { read_fd })
    }

    /// Takes a pending termination signal, if any.
    pub fn take_signal(&self) -> Option<i32> {
        match PENDING_SIGNAL.swap(0, Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    /// Sleeps until a signal arrives or the timeout passes; `None` waits
    /// indefinitely.
    pub fn wait(&self, timeout: Option<Duration>) {
        self.wait_with(&[], timeout);
    }

    /// Like `wait`, but also returns when one of `extra` becomes readable
    /// (or hangs up). Negative descriptors are ignored. Entry `i` of the
    /// result reports `extra[i]`.
    ///
    /// # Panics
    /// When given more than `MAX_EXTRA` descriptors.
    pub fn wait_with(&self, extra: &[libc::c_int], timeout: Option<Duration>) -> [bool; MAX_EXTRA] {
        assert!(extra.len() <= MAX_EXTRA, "too many descriptors to wait on");
        let blank = libc::pollfd {
            fd: -1,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut fds = [blank; MAX_EXTRA + 1];
        fds[0].fd = self.read_fd;
        for (slot, fd) in fds[1..].iter_mut().zip(extra) {
            slot.fd = *fd;
        }
        let millis = match timeout {
            None => -1,
            Some(duration) => {
                let rounded = duration
                    .as_millis()
                    .saturating_add(u128::from(duration.subsec_nanos() % 1_000_000 != 0));
                libc::c_int::try_from(rounded).unwrap_or(libc::c_int::MAX)
            }
        };
        // Unused slots stay -1, which poll(2) skips.
        let count = fds.len() as libc::nfds_t;
        let mut ready = [false; MAX_EXTRA];
        // A non-positive result is a timeout, or EINTR from a handler whose
        // byte is still queued for the next call.
        if unsafe { libc::poll(fds.as_mut_ptr(), count, millis) } > 0 {
            if fds[0].revents != 0 {
                self.drain();
            }
            for (flag, slot) in ready.iter_mut().zip(&fds[1..]) {
                *flag = slot.revents != 0;
            }
        }
        ready
    }

    fn drain(&self) {
        let mut buffer = [0u8; 64];
        while unsafe { libc::read(self.read_fd, buffer.as_mut_ptr().cast(), buffer.len()) } > 0 {}
    }
}

fn set_flags(fd: libc::c_int) -> Result<(), String> {
    let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    let descriptor = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if status < 0
        || descriptor < 0
        || unsafe { libc::fcntl(fd, libc::F_SETFL, status | libc::O_NONBLOCK) } < 0
        || unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor | libc::FD_CLOEXEC) } < 0
    {
        return Err(format!(
            "configure signal pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn install_handler(signal: libc::c_int) -> Result<(), String> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
    // SA_RESTART keeps blocking I/O in relay threads uninterrupted.
    action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0
        || unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0
    {
        return Err(format!(
            "register signal handler: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Delivers a signal to the process group. Returns false when the group is gone.
pub fn signal_process_group(process_group_id: u32, signal: libc::c_int) -> bool {
    let Ok(pgid) = i32::try_from(process_group_id) else {
        return false;
    };
    unsafe { libc::kill(-pgid, signal) == 0 }
}

pub fn signal_pid(pid: u32, signal: libc::c_int) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    unsafe { libc::kill(pid, signal) == 0 }
}

/// The group is alive unless the kernel reports it no longer exists.
pub fn process_group_alive(process_group_id: u32) -> bool {
    let Ok(pgid) = i32::try_from(process_group_id) else {
        return false;
    };
    let result = unsafe { libc::kill(-pgid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
