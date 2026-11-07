//! `ranikust prune --all` (ADR-0105): empties the state directory of a user
//! other than root inside the build's user namespace, where the files of
//! subordinate ids a rootless build leaves can be removed (the user outside
//! cannot). Phase 5's `prune`, by size and age (ADR-0032), grows from it.
//!
//! In order, each step before the next: without `--all`, a usage error
//! naming Phase 5; root refused (euid 0 in any user namespace, a
//! container's root included), naming Phase 5's `prune`; then a build's
//! checks of the state directory, outside, which neither make it nor
//! change its mode ([`statedir::check`]): one that does not exist, or is
//! empty, is left so; then the build's user namespace
//! ([`setup::enter_build_userns`]); then, in it, `RLIMIT_NOFILE` raised and
//! [`rnk_store::Store::prune_all`], which removes nothing while a build
//! holds the store, refuses a directory that is not the store's, and keeps
//! builds out of one it did not finish emptying. The commands a refusal
//! suggests name the state directory, quoted for a shell.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rnk_store::Store;

use crate::cli::PruneArgs;
use crate::config;
use crate::setup::{self, Kind, Refusal};
use crate::statedir::{self, Found};

/// What `prune --all` found in the state directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pruned {
    /// It does not exist; nothing was made.
    Missing,
    /// It was empty, and stays so: no marker is made, its mode is kept.
    Empty,
    /// It was emptied, its marker last.
    Emptied,
}

/// Why `prune` did not run.
#[derive(Debug)]
pub(crate) enum Failed {
    /// Flags it does not take.
    Usage(String),
    /// The environment or the state directory refused it.
    Refused(Refusal),
}

/// `prune` without `--all`.
const NEEDS_ALL: &str = "ranikust prune removes by size and age only from Phase 5 on \
     (ADR-0032); for now it takes --all, which empties the state directory of a user other than \
     root (ADR-0105)";

/// What root does instead with the state directory `dir` (ADR-0105).
fn root_guidance(dir: &Path) -> String {
    let rm = locked(
        dir,
        r#"rm "$1/.ranikust-state" && rm -rf --one-file-system "$1""#,
    );
    format!(
        "root removes its state directory with ordinary tools while no build uses it, its marker \
         first so that no build takes what an interrupted removal leaves, and nothing in a \
         directory without one: {rm}; Phase 5's ranikust prune will serve root too"
    )
}

/// What a user does with the state directory `dir` where it gets no user
/// namespace: the build's guidance ends in "run ranikust as root", which
/// `prune --all` refuses.
pub(crate) fn no_user_namespace(dir: &Path) -> String {
    let rm = locked(
        dir,
        r#"mv "$1/.ranikust-state" "$1/.ranikust-pruning" && find "$1" -mindepth 1 -maxdepth 1 ! -name .ranikust-pruning -exec rm -rf --one-file-system {} + && rm "$1/.ranikust-pruning""#,
    );
    format!(
        "a user other than root gets a user namespace of its own only where \
         kernel.apparmor_restrict_unprivileged_userns is 0, or an AppArmor profile allows \
         ranikust one (Ubuntu 23.10 and later); without it, {rm} renames the marker, which keeps \
         builds out, removes the user's own files, then the marker once nothing else is left; \
         files of subordinate ids stay, and the marker with them, for a ranikust prune --all \
         where a user namespace is allowed to finish"
    )
}

/// The ways out of the state directory `dir` that `prune --all` did not
/// finish emptying, which no build opens: for its refusal and a build's.
pub(crate) fn unfinished(dir: &Path) -> String {
    let d = shell_word(dir.as_os_str());
    let mut aside = dir.as_os_str().to_owned();
    aside.push(".old");
    let aside = shell_word(&aside);
    format!(
        "ranikust prune --all leaves what is on another mount, and files of ids it cannot remove \
         (a rootless build's subordinate ids, where no user namespace maps them): unmount \
         anything below {d}; install newuidmap and give this user ranges in /etc/subuid and \
         /etc/subgid, then run ranikust prune --all again; or move the directory aside, mv -T \
         {d} {aside}, and remove that with podman unshare rm -rf --one-file-system {aside}, or \
         as root"
    )
}

/// `script` run by `sh` with the state directory `dir` as `$1`, under the
/// exclusive `flock` that [`Store::prune_all`] takes, so it fails while a
/// build holds the directory; `test -d` first, since `flock` creates a
/// file at a missing path. `script` holds no single quote.
fn locked(dir: &Path, script: &str) -> String {
    debug_assert!(!script.contains('\''), "{script}");
    let d = shell_word(dir.as_os_str());
    format!("test -d {d} && flock -xn {d} sh -c '{script}' sh {d}")
}

/// `word` as one shell word that stands for itself: as it is when made of
/// characters no shell treats specially; else single-quoted; else, when it
/// holds a byte that is not UTF-8 or a character a terminal acts on
/// ([`rnk_metrics::terminal_text`], which would escape it), `$'…'` with
/// each byte outside printable ASCII as `\xNN`, as bash, zsh and ksh read
/// it.
fn shell_word(word: &OsStr) -> String {
    let bytes = word.as_bytes();
    let plain = |b: &u8| b.is_ascii_alphanumeric() || b"/._-+,:@%=".contains(b);
    if !bytes.is_empty() && bytes.iter().all(plain) {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(text) if rnk_metrics::terminal_text(text) == text => {
            format!("'{}'", text.replace('\'', r"'\''"))
        }
        _ => {
            let mut out = String::from("$'");
            for &b in bytes {
                match b {
                    b'\'' | b'\\' => {
                        out.push('\\');
                        out.push(char::from(b));
                    }
                    0x20..=0x7e => out.push(char::from(b)),
                    _ => out.push_str(&format!("\\x{b:02x}")),
                }
            }
            out.push('\'');
            out
        }
    }
}

/// Runs `prune` with `args`: the state directory it looked at, and what it
/// found there.
///
/// # Errors
///
/// [`Failed::Usage`] without `--all`, or without a `--state-dir` where the
/// default is unknown; [`Failed::Refused`] as [`prune_all`].
pub(crate) fn run(args: &PruneArgs) -> Result<(PathBuf, Pruned), Failed> {
    if !args.all {
        return Err(Failed::Usage(NEEDS_ALL.to_owned()));
    }
    let euid = rustix::process::geteuid().as_raw();
    let state_dir = match &args.state_dir {
        Some(dir) => config::absolute(dir),
        None => config::default_state_dir(euid, std::env::var_os("HOME").as_deref())
            .map_err(|invalid| Failed::Usage(invalid.0))?,
    };
    let pruned = prune_all(euid, &state_dir).map_err(Failed::Refused)?;
    Ok((state_dir, pruned))
}

/// `prune --all` on `state_dir`, by the effective user `euid`: refused for
/// root before anything is looked at, else [`empty`]. Call while
/// single-threaded.
///
/// # Errors
///
/// A [`Refusal`] for root, naming Phase 5's `prune`; else as [`empty`].
pub(crate) fn prune_all(euid: u32, state_dir: &Path) -> Result<Pruned, Refusal> {
    if euid == 0 {
        return Err(Refusal {
            kind: Kind::Other,
            reason: "ranikust prune --all empties the state directory of a user other than root \
                     (ADR-0105), and refuses root, a container's root included"
                .to_owned(),
            guidance: root_guidance(state_dir).into(),
        });
    }
    empty(state_dir)
}

/// Empties `state_dir` as root of the build's user namespace, which this
/// process enters for good: the state directory's checks outside, which
/// change nothing ([`statedir::check`]); nothing more when it does not
/// exist or is empty; else the namespace, then [`Store::prune_all`] in it.
/// Call while single-threaded.
///
/// # Errors
///
/// A [`Refusal`]: naming the state directory's check that failed; as
/// [`setup::enter_build_userns`]; or naming what [`Store::prune_all`]
/// refused or left, then of [`Kind::Broken`].
pub(crate) fn empty(state_dir: &Path) -> Result<Pruned, Refusal> {
    let found = statedir::check(state_dir).map_err(|refused| Refusal {
        kind: Kind::Other,
        reason: refused.0,
        guidance: "".into(),
    })?;
    match found {
        Found::Missing => return Ok(Pruned::Missing),
        Found::Empty => return Ok(Pruned::Empty),
        Found::Entries => {}
    }
    setup::enter_build_userns(no_user_namespace(state_dir).into())?;
    // The deletion holds a descriptor per directory level, and a `RUN` may
    // have left a deep tree.
    match statedir::raise_nofile_limit() {
        Ok(limit) => tracing::debug!(?limit, "RLIMIT_NOFILE raised to its hard limit"),
        Err(e) => tracing::warn!("cannot raise RLIMIT_NOFILE to its hard limit: {e}"),
    }
    match Store::prune_all(state_dir) {
        Ok(true) => Ok(Pruned::Emptied),
        Ok(false) => Ok(Pruned::Empty),
        Err(e) => Err(store_refusal(&e)),
    }
}

/// [`Store::prune_all`]'s error `e`, met in the build's user namespace: of
/// [`Kind::Broken`], with the ways out when entries stay.
fn store_refusal(e: &rnk_store::Error) -> Refusal {
    setup::inside_userns(Refusal {
        kind: Kind::Other,
        reason: e.to_string(),
        guidance: match e {
            rnk_store::Error::NotPruned { path, .. } => unfinished(path).into(),
            _ => "".into(),
        },
    })
}

/// The line `prune` prints on success, for `state_dir`.
pub(crate) fn summary(state_dir: &Path, pruned: Pruned) -> String {
    let dir = state_dir.display();
    match pruned {
        Pruned::Missing => format!("nothing to prune: {dir} does not exist"),
        Pruned::Empty => format!("nothing to prune: {dir} is empty"),
        Pruned::Emptied => format!("emptied {dir}"),
    }
}

#[cfg(test)]
mod tests;
