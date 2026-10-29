//! Process set-up, once per build, while the process is single-threaded.

use rustix::io::Errno;
use rustix::mount::MountPropagationFlags;
use rustix::thread::UnshareFlags;

use crate::error::{Error, Result, sys_err};

/// Moves the process into a private mount namespace whose `/` is a slave of
/// the original: mounts propagate in, none made by the build propagate out,
/// and all of them disappear with the process.
///
/// Call before any thread exists (before tokio or rayon start): in a
/// multi-threaded process `unshare(CLONE_NEWNS)` silently moves only the
/// calling thread.
///
/// # Errors
///
/// [`Error::NotSingleThreaded`] when other threads exist,
/// [`Error::Privilege`] without `CAP_SYS_ADMIN`, [`Error::Sys`] otherwise.
pub fn enter_build_namespace() -> Result<()> {
    let threads = thread_count()?;
    if threads != 1 {
        return Err(Error::NotSingleThreaded { threads });
    }
    // SAFETY: `NEWNS` does not unshare the descriptor table, the only flag
    // `unshare_unsafe` is unsafe for; the process is single-threaded.
    match unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) } {
        Ok(()) => {}
        Err(Errno::PERM) => return Err(Error::Privilege("a private mount namespace")),
        Err(e) => return Err(sys_err("unshare(CLONE_NEWNS)", e)),
    }
    // `DOWNSTREAM` is `MS_SLAVE`.
    rustix::mount::mount_change(
        c"/",
        MountPropagationFlags::DOWNSTREAM | MountPropagationFlags::REC,
    )
    .map_err(|e| sys_err("make / a recursive slave mount", e))
}

/// Threads of this process, from `/proc/self/status`.
fn thread_count() -> Result<u64> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| crate::error::io_err("read", "/proc/self/status", e))?;
    parse_threads(&status).ok_or_else(|| {
        sys_err(
            "read the thread count",
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "no Threads: line in /proc/self/status",
            ),
        )
    })
}

fn parse_threads(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_line_is_parsed() {
        let status = "Name:\tranikust\nPid:\t1\nThreads:\t3\nVmRSS:\t1 kB\n";
        assert_eq!(parse_threads(status), Some(3));
        assert_eq!(parse_threads("Name:\tx\n"), None);
    }

    #[test]
    fn a_test_process_is_multi_threaded_and_refused() {
        // The test harness runs tests on worker threads: refused before any
        // system call that could change the process.
        // A thread that lives until the check is done, whatever the harness
        // does with threads.
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let spare = std::thread::spawn(move || {
            let _ = wait.recv();
        });
        let r = enter_build_namespace();
        drop(done);
        spare.join().unwrap();
        assert!(
            matches!(r, Err(Error::NotSingleThreaded { threads }) if threads > 1),
            "{r:?}"
        );
    }
}
