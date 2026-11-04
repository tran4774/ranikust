//! ranikust runs again from a sealed copy of its binary (ADR-0101).
//!
//! Every process ranikust forks runs its binary until it executes
//! something else: a `RUN` child until `execve`, a full-sandbox `RUN`'s init
//! for good. Run from a copy no `RUN` can write, none of them hands a `RUN`
//! a binary it could change through `/proc/<pid>/exe`, whatever the kernel
//! does about writes to a running file. The copies are the first two of
//! runc's (`CloneBinary`, `libcontainer/dmz` in runc 1.2.0), without the
//! overlay runc tries before them or the named temporary file after.

use std::convert::Infallible;
use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, SealFlags};
use rustix::io::Errno;

/// Set for the copy's run: it names the way, and the copy never makes
/// another (no loop).
const MARKER: &str = "RANIKUST_SEALED_COPY";

/// The seals of a memfd copy: no write, no change of size, no seal taken
/// off.
const SEALS: SealFlags = SealFlags::SEAL
    .union(SealFlags::SHRINK)
    .union(SealFlags::GROW)
    .union(SealFlags::WRITE);

/// A way to make the copy, in the order [`SealedCopy::ALL`] tries them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealedCopy {
    /// A copy in a sealed memfd: no write, no change of size. It takes the
    /// binary's size in memory.
    Memfd,
    /// A copy in an `O_TMPFILE` file no path reaches, run read-only. Only
    /// the kernel's `ETXTBSY` keeps it from being written, as the installed
    /// file before, but a write would not reach the installed file.
    Tmpfile,
}

impl SealedCopy {
    /// Every way, in the order they are tried.
    pub const ALL: [Self; 2] = [Self::Memfd, Self::Tmpfile];

    fn name(self) -> &'static str {
        match self {
            Self::Memfd => "memfd",
            Self::Tmpfile => "tmpfile",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|way| way.name() == name)
    }
}

impl fmt::Display for SealedCopy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What [`run_from_sealed_copy`] found, when it returns.
#[derive(Debug)]
pub enum Sealing {
    /// This process runs from a copy made that way, checked.
    Sealed {
        /// The way the copy was made.
        way: SealedCopy,
        /// Why each way tried before it failed, `; ` between them; empty
        /// when it was the first tried.
        passed_over: String,
    },
    /// This process runs from the binary as installed, or from something
    /// that is not the copy its marker names: why.
    Unsealed(String),
}

/// Runs this process again from a sealed copy of its binary, trying `ways`
/// in order; returns in the copy's run, or when no way worked.
///
/// Call it while the process is single-threaded, before the build's work
/// starts: the copy starts over with the same arguments, environment and
/// descriptors, `RANIKUST_SEALED_COPY` added. In the copy's run (that
/// variable set) it makes no copy, checks `/proc/self/exe` is the copy the
/// variable names, and names the process after its `argv[0]` again. The
/// core-file limit [`forbid_core_dumps`](crate::forbid_core_dumps) recorded
/// is given back for the copy, which records it again.
///
/// # Errors
///
/// Only when a copy did not run and the core-file limit could not be set
/// back to 0: the process must stop, as when `forbid_core_dumps` fails.
pub fn run_from_sealed_copy(ways: &[SealedCopy]) -> rustix::io::Result<Sealing> {
    if let Some(value) = std::env::var_os(MARKER) {
        return Ok(copy_run(&value));
    }
    let run = |way, copy: OwnedFd, failed: &[(SealedCopy, io::Error)]| {
        execute(&copy, &marker(way, failed))
    };
    let failed = match try_in_order(ways, make_copy, run, core_limit_to_zero) {
        Search::Ran(never) => match never {},
        Search::NoneRan(failed) => failed,
        Search::Stopped(e) => return Err(e),
    };
    if failed.is_empty() {
        return Ok(Sealing::Unsealed("no way was tried".to_owned()));
    }
    Ok(Sealing::Unsealed(passed_over(&failed)))
}

/// Each failure with its way, `; ` between them.
fn passed_over(failed: &[(SealedCopy, io::Error)]) -> String {
    let why: Vec<String> = failed
        .iter()
        .map(|(way, e)| format!("{way}: {e}"))
        .collect();
    why.join("; ")
}

/// The marker's value for a copy made `way`: its name, then why the ways
/// before it failed, for the copy to log.
fn marker(way: SealedCopy, failed: &[(SealedCopy, io::Error)]) -> String {
    if failed.is_empty() {
        way.name().to_owned()
    } else {
        format!("{way}; {}", passed_over(failed))
    }
}

/// The way a marker names and why the ways before it failed.
fn parse_marker(value: &str) -> Option<(SealedCopy, &str)> {
    let (name, passed_over) = value.split_once(';').unwrap_or((value, ""));
    Some((SealedCopy::from_name(name)?, passed_over.trim_start()))
}

/// The copy's run: checks the copy, never makes another.
fn copy_run(value: &OsStr) -> Sealing {
    // The kernel named the process after `/proc/self/fd/<n>`; `ps`, `pgrep`
    // and `pkill -x` look for ranikust's own name.
    name_after_argv0();
    let Some((way, passed_over)) = value.to_str().and_then(parse_marker) else {
        return Sealing::Unsealed(format!(
            "{MARKER}={value:?} names no way to seal the binary"
        ));
    };
    match File::open("/proc/self/exe").and_then(|exe| check(way, exe.as_fd())) {
        Ok(()) => Sealing::Sealed {
            way,
            passed_over: passed_over.to_owned(),
        },
        Err(e) => Sealing::Unsealed(format!(
            "{MARKER}={way}, but /proc/self/exe is no such copy: {e}"
        )),
    }
}

fn name_after_argv0() {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    let base = Path::new(&argv0)
        .file_name()
        .map_or(b"ranikust".as_slice(), OsStrExt::as_bytes);
    if let Ok(name) = CString::new(base) {
        // Refused only for a bad name; the name is cosmetic.
        let _ = rustix::thread::set_name(&name);
    }
}

/// Whether `file` is a copy made `way`.
fn check(way: SealedCopy, file: BorrowedFd<'_>) -> io::Result<()> {
    let holds = match way {
        SealedCopy::Memfd => rustix::fs::fcntl_get_seals(file)?.contains(SEALS),
        SealedCopy::Tmpfile => rustix::fs::fstat(file)?.st_nlink == 0,
    };
    if holds {
        Ok(())
    } else {
        Err(io::Error::other(format!("not a sealed {way} copy")))
    }
}

/// How [`try_in_order`] ended.
#[derive(Debug)]
enum Search<T, E> {
    /// A copy ran (in the process, it never returns).
    Ran(T),
    /// No way ran: each failure with its way, in order.
    NoneRan(Vec<(SealedCopy, io::Error)>),
    /// A copy did not run and `recover` failed: no other way was tried.
    Stopped(E),
}

/// Makes a copy with each way in turn and runs it, giving `run` the
/// failures of the ways before, until one runs. After a copy that did not
/// run, `recover` must succeed for the next way to be tried.
fn try_in_order<T, E>(
    ways: &[SealedCopy],
    mut make: impl FnMut(SealedCopy) -> io::Result<OwnedFd>,
    mut run: impl FnMut(SealedCopy, OwnedFd, &[(SealedCopy, io::Error)]) -> io::Result<T>,
    mut recover: impl FnMut() -> Result<(), E>,
) -> Search<T, E> {
    let mut failed = Vec::new();
    for &way in ways {
        let copy = match make(way) {
            Ok(copy) => copy,
            Err(e) => {
                failed.push((way, e));
                continue;
            }
        };
        match run(way, copy, &failed) {
            Ok(done) => return Search::Ran(done),
            Err(e) => failed.push((way, e)),
        }
        if let Err(e) = recover() {
            return Search::Stopped(e);
        }
    }
    Search::NoneRan(failed)
}

fn make_copy(way: SealedCopy) -> io::Result<OwnedFd> {
    let exe = File::open("/proc/self/exe").map_err(context("open /proc/self/exe"))?;
    match way {
        SealedCopy::Memfd => memfd_copy(&exe),
        SealedCopy::Tmpfile => tmpfile_copy(&exe, &temp_dir()),
    }
}

/// Where the `O_TMPFILE` copy goes.
fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir();
    if dir.is_absolute() {
        dir
    } else {
        PathBuf::from("/tmp")
    }
}

/// A copy in a memfd sealed against writes and size changes.
fn memfd_copy(exe: &File) -> io::Result<OwnedFd> {
    use rustix::fs::MemfdFlags;
    let flags = MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING;
    // `MFD_EXEC` (Linux 6.3) keeps `vm.memfd_noexec=1` from making the copy
    // impossible to run; older kernels refuse the flag. At 2 the kernel
    // refuses it, and the next way is tried.
    let fd = match rustix::fs::memfd_create("ranikust", flags | MemfdFlags::EXEC) {
        Err(Errno::INVAL) => rustix::fs::memfd_create("ranikust", flags),
        made => made,
    }
    .map_err(context("memfd_create"))?;
    let mut copy = File::from(fd);
    copy_whole(exe, &mut copy)?;
    rustix::fs::fcntl_add_seals(&copy, SEALS).map_err(context("seal the memfd"))?;
    Ok(copy.into())
}

/// A copy in an `O_TMPFILE` file in `dir`, which can never get a name
/// (`O_EXCL`), open read-only: `execve` refuses a file open for writing.
fn tmpfile_copy(exe: &File, dir: &Path) -> io::Result<OwnedFd> {
    let fd = rustix::fs::open(
        dir,
        OFlags::TMPFILE | OFlags::RDWR | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RWXU,
    )
    .map_err(context(format!("O_TMPFILE in {}", dir.display())))?;
    let mut writer = File::from(fd);
    copy_whole(exe, &mut writer)?;
    let copy = rustix::fs::open(
        format!("/proc/self/fd/{}", writer.as_raw_fd()).as_str(),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(context("open the copy read-only"))?;
    drop(writer);
    Ok(copy)
}

fn copy_whole(exe: &File, to: &mut File) -> io::Result<()> {
    let size = exe.metadata()?.len();
    let mut from = exe;
    let copied = io::copy(&mut from, to).map_err(context("copy the binary"))?;
    if copied == size {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "copied {copied} bytes of the binary's {size}"
        )))
    }
}

/// Runs `copy` with this process's arguments and environment, the marker
/// set to `marker`. Returns only when the kernel refused it.
fn execute(copy: &OwnedFd, marker: &str) -> io::Result<Infallible> {
    let path = CString::new(format!("/proc/self/fd/{}", copy.as_raw_fd()))?;
    let args = std::env::args_os()
        .map(|arg| CString::new(arg.into_vec()))
        .collect::<Result<Vec<_>, _>>()?;
    let env = copy_environment(std::env::vars_os(), marker)?;
    let argv = null_terminated(&args);
    let envp = null_terminated(&env);
    // The copy records the limit `forbid_core_dumps` found, as this run
    // did, so every `RUN` still gets the original back.
    if let Some(limit) = crate::spec::original_core_limit() {
        rustix::process::setrlimit(rustix::process::Resource::Core, limit)
            .map_err(context("setrlimit(RLIMIT_CORE) for the copy"))?;
    }
    // Not `Command::exec`: on a refusal it would leave `SIGPIPE` at its
    // default and the signal mask cleared in this process, which goes on.
    // SAFETY: `path`, every string and both null-terminated arrays outlive
    // the call; `execve` returns only on failure, changing nothing.
    unsafe { libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    Err(io::Error::last_os_error())
}

/// The copy's environment: `vars` with `MARKER=marker` in place of any
/// marker among them.
fn copy_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    marker: &str,
) -> io::Result<Vec<CString>> {
    let mut env = vars
        .into_iter()
        .filter(|(key, _)| key != MARKER)
        .map(|(key, value)| {
            let mut pair = key.into_vec();
            pair.push(b'=');
            pair.extend(value.into_vec());
            CString::new(pair)
        })
        .collect::<Result<Vec<_>, _>>()?;
    env.push(CString::new(format!("{MARKER}={marker}"))?);
    Ok(env)
}

fn null_terminated(strings: &[CString]) -> Vec<*const libc::c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

/// After a copy did not run: the soft core-file limit back to 0.
fn core_limit_to_zero() -> rustix::io::Result<()> {
    if crate::spec::original_core_limit().is_none() {
        return Ok(());
    }
    crate::forbid_core_dumps()
}

fn context<E: Into<io::Error>>(what: impl fmt::Display) -> impl FnOnce(E) -> io::Error {
    move |e| {
        let e = e.into();
        io::Error::new(e.kind(), format!("{what}: {e}"))
    }
}

#[cfg(test)]
mod tests;
