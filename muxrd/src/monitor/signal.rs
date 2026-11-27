//! signal — send a signal to a process, refusing anything but the process the caller saw.
//!
//! The caller names a process by `(pid, start time)`; the pid is looked up fresh and the start
//! time compared first, so a recycled pid is never signalled. The check and the `kill` are two
//! steps and cannot be made atomic from user space; the window is the few microseconds between
//! them.

use std::fmt;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// A signal the monitor may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    /// SIGTERM: ask the process to exit.
    Term,
    /// SIGKILL: end it.
    Kill,
    /// SIGINT: interrupt it.
    Int,
    /// SIGHUP: hang up (many daemons reload).
    Hup,
}

impl ProcessSignal {
    fn number(self) -> libc::c_int {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
            Self::Int => libc::SIGINT,
            Self::Hup => libc::SIGHUP,
        }
    }
}

/// Why a signal was not sent, or not delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalRefusal {
    /// No such process.
    NotFound,
    /// The pid exists but its start time differs from the expected one: a different process.
    Recycled,
    /// Pid 0, pid 1, a pid that is not a valid `pid_t`, or muxrd itself.
    Forbidden,
    /// The kernel refused (`EPERM` or another errno).
    NotPermitted(i32),
}

impl fmt::Display for SignalRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("no such process"),
            Self::Recycled => f.write_str("the pid now belongs to a different process"),
            Self::Forbidden => f.write_str("this process may not be signalled"),
            Self::NotPermitted(errno) => write!(f, "not permitted (errno {errno})"),
        }
    }
}

impl std::error::Error for SignalRefusal {}

/// Send `signal` to `pid`, provided it still started at `expected_start_unix_s`. Blocking: call
/// from `spawn_blocking`.
pub fn send_signal(
    pid: u32,
    expected_start_unix_s: i64,
    signal: ProcessSignal,
) -> Result<(), SignalRefusal> {
    // 0 and 1 are never ours to signal, and a pid above i32::MAX would turn negative: kill(2)
    // would then address a process group (or everything).
    let raw_pid = match i32::try_from(pid) {
        Ok(raw) if raw > 1 && pid != std::process::id() => raw,
        _ => return Err(SignalRefusal::Forbidden),
    };

    let Some(start) = start_time_of(pid) else {
        return Err(SignalRefusal::NotFound);
    };
    if start != expected_start_unix_s {
        return Err(SignalRefusal::Recycled);
    }

    // SAFETY: kill(2) takes plain integers and touches no memory of ours; the pid is a positive
    // pid_t that is neither 0, 1 nor this process, so it names exactly one other process.
    let rc = unsafe { libc::kill(raw_pid, signal.number()) };
    if rc == 0 {
        return Ok(());
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    Err(match errno {
        libc::ESRCH => SignalRefusal::NotFound,
        other => SignalRefusal::NotPermitted(other),
    })
}

/// The start time of `pid` right now, from a fresh single-pid refresh; `None` when it is gone.
fn start_time_of(pid: u32) -> Option<i64> {
    let mut system = match System::new() {
        Ok(system) => system,
        Err(err) => {
            log::warn!("monitor: could not open sysinfo to look up a pid: {err}");
            return None;
        }
    };
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system
        .process(pid)
        .map(|p| i64::try_from(p.start_time()).unwrap_or(i64::MAX))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    /// Kills and reaps the child if a test fails before it does.
    struct Sleeper(Child);

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn sleeper() -> Sleeper {
        Sleeper(
            Command::new("sleep")
                .arg("60")
                .spawn()
                .expect("spawn sleep"),
        )
    }

    #[test]
    fn a_wrong_start_time_is_refused_and_the_child_lives() {
        let mut child = sleeper();
        let pid = child.0.id();
        let start = start_time_of(pid).expect("the child is visible");
        assert_eq!(
            send_signal(pid, start + 1, ProcessSignal::Term),
            Err(SignalRefusal::Recycled)
        );
        assert!(child.0.try_wait().unwrap().is_none(), "still alive");
    }

    #[test]
    fn the_right_start_time_terminates_the_child() {
        let mut child = sleeper();
        let pid = child.0.id();
        let start = start_time_of(pid).expect("the child is visible");
        assert_eq!(send_signal(pid, start, ProcessSignal::Term), Ok(()));
        let status = child.0.wait().unwrap();
        assert!(!status.success());
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn pid_zero_pid_one_and_our_own_pid_are_forbidden() {
        for pid in [0, 1, std::process::id(), u32::MAX, 1 << 31] {
            assert_eq!(
                send_signal(pid, 0, ProcessSignal::Kill),
                Err(SignalRefusal::Forbidden),
                "pid {pid}"
            );
        }
    }

    #[test]
    fn a_missing_pid_is_not_found() {
        // Above any default pid_max (4194304), below i32::MAX.
        assert_eq!(
            send_signal(2_000_000_000, 0, ProcessSignal::Term),
            Err(SignalRefusal::NotFound)
        );
    }
}
