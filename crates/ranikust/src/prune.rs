//! `ranikust prune --all` (ADR-0105): empties the state directory of a user
//! other than root inside the build's user namespace, where the files of
//! subordinate ids a rootless build leaves can be removed (the user outside
//! cannot). Phase 5's `prune`, by size and age (ADR-0032), grows from it.
//!
//! In order, each step before the next: without `--all`, a usage error
//! naming Phase 5; root refused (euid 0 in any user namespace, a
//! container's root included), naming Phase 5's `prune`; a state directory
//! that does not exist left so, nothing made; then, as a build, the state
//! directory's checks outside and the build's user namespace
//! ([`setup::enter_user_namespace_for`]); then, in it,
//! [`rnk_store::Store::prune_all`], which removes nothing while a build
//! holds the store and refuses a directory without the store's marker.

use std::path::{Path, PathBuf};

use rnk_store::Store;

use crate::cli::PruneArgs;
use crate::config;
use crate::setup::{self, Kind, Refusal};

/// What `prune --all` found in the state directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pruned {
    /// It does not exist; nothing was made.
    Missing,
    /// It was empty, and stays so: no marker is made.
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

/// What root does instead (ADR-0105).
const ROOT_GUIDANCE: &str = "root removes its state directory with ordinary tools while no \
     build uses it (rm -rf /var/lib/ranikust, or the --state-dir it built with); Phase 5's \
     ranikust prune will serve root too";

/// What a user does where it gets no user namespace: the build's guidance,
/// [`setup::NO_USER_NAMESPACE`], ends in "run ranikust as root", which
/// `prune --all` refuses.
pub(crate) const NO_USER_NAMESPACE: &str = "a user other than root gets a user namespace of its \
     own only where kernel.apparmor_restrict_unprivileged_userns is 0, or an AppArmor profile \
     allows ranikust one (Ubuntu 23.10 and later); without it, the user's own files in the state \
     directory go with rm -rf while no build uses it, and files of subordinate ids stay";

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
            guidance: ROOT_GUIDANCE,
        });
    }
    empty(state_dir)
}

/// Empties `state_dir` as root of the build's user namespace, which this
/// process enters for good: nothing when it does not exist, else the
/// state directory's checks outside, the namespace, then
/// [`Store::prune_all`] in it. Call while single-threaded.
///
/// # Errors
///
/// A [`Refusal`]: as [`setup::enter_user_namespace_for`], or naming what
/// [`Store::prune_all`] refused or left, then of [`Kind::Broken`].
pub(crate) fn empty(state_dir: &Path) -> Result<Pruned, Refusal> {
    match std::fs::symlink_metadata(state_dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Pruned::Missing),
        Err(e) => {
            return Err(Refusal {
                kind: Kind::Other,
                reason: format!(
                    "cannot look up the state directory {}: {e}",
                    state_dir.display()
                ),
                guidance: "",
            });
        }
    }
    setup::enter_user_namespace_for(state_dir).map_err(|r| {
        if r.guidance == setup::NO_USER_NAMESPACE {
            Refusal {
                guidance: NO_USER_NAMESPACE,
                ..r
            }
        } else {
            r
        }
    })?;
    match Store::prune_all(state_dir) {
        Ok(true) => Ok(Pruned::Emptied),
        Ok(false) => Ok(Pruned::Empty),
        Err(e) => Err(setup::inside_userns(Refusal {
            kind: Kind::Other,
            reason: e.to_string(),
            guidance: "",
        })),
    }
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
