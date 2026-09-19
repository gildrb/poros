use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;

static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: libc::c_int) {
    LAST_SIGNAL.store(signal, Ordering::SeqCst);
    // One byte wakes the reader thread; async-signal-safe.
    let byte = [1u8];
    let _ = unsafe {
        libc::write(
            SIGNAL_PIPE_WRITE.load(Ordering::Relaxed),
            byte.as_ptr().cast(),
            1,
        )
    };
}

static SIGNAL_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

/// The most recent signal number observed by a handler, or 0.
pub fn last_signal() -> i32 {
    LAST_SIGNAL.load(Ordering::SeqCst)
}

/// Installs handlers for SIGINT, SIGTERM, and SIGHUP and forwards each received
/// signal number through the returned channel.
pub fn install() -> Result<std::sync::mpsc::Receiver<i32>, String> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err("create signal pipe".to_string());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    set_non_blocking(read_fd);
    set_non_blocking(write_fd);
    SIGNAL_PIPE_WRITE.store(write_fd, Ordering::SeqCst);
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        if unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return Err("register signal handler".to_string());
        }
    }
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("poros-signals".to_string())
        .spawn(move || {
            let mut buffer = [0u8; 16];
            loop {
                let read = unsafe { libc::read(read_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                if read <= 0 {
                    if read == 0 {
                        return;
                    }
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::WouldBlock
                        && error.raw_os_error() != Some(libc::EINTR)
                    {
                        return;
                    }
                    continue;
                }
                for _ in 0..read {
                    let signal = LAST_SIGNAL.load(Ordering::SeqCst);
                    if sender.send(signal).is_err() {
                        return;
                    }
                }
            }
        })
        .map_err(|error| format!("start signal thread: {error}"))?;
    Ok(receiver)
}

fn set_non_blocking(fd: libc::c_int) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags >= 0 {
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
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
