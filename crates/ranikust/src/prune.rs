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
//! suggests name the state directory, quoted for a shell, and act only on
//! the directory ranikust checked ([`Pin`]).

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

/// Root's emptying of the state directory it runs in, for [`in_dir`]: its
/// regular marker first, `.ranikust-state` or else the `.ranikust-pruning`
/// a prune that did not finish left, so that what an interrupted removal
/// leaves is unmarked, and nothing where there is neither; then
/// everything, with no top-level entry an operand and `find -xdev`
/// entering no other filesystem, so a mount point is left (GNU `find` then
/// fails).
const ROOT_SCRIPT: &str = "{ test -f .ranikust-state && ! test -h .ranikust-state && rm .ranikust-state || { test -f .ranikust-pruning && ! test -h .ranikust-pruning && rm .ranikust-pruning; }; } && find . -xdev -mindepth 1 -delete";

/// A user's emptying of the state directory it runs in without a user
/// namespace, for [`in_dir`]: first a regular `.ranikust-pruning` an
/// earlier run left is kept, or else, where nothing has that name, the
/// regular marker is renamed to it (`mv -T`, never into a directory),
/// which keeps builds out; then all but it, as [`ROOT_SCRIPT`] does; it
/// last, once a listing shows nothing else left, since not every `find`
/// fails when a removal does.
const USER_SCRIPT: &str = r#"{ test -f .ranikust-pruning && ! test -h .ranikust-pruning || { ! test -e .ranikust-pruning && ! test -h .ranikust-pruning && test -f .ranikust-state && ! test -h .ranikust-state && mv -T .ranikust-state .ranikust-pruning; }; } && find . -xdev -mindepth 1 ! -path ./.ranikust-pruning -delete && test "$(find . -mindepth 1 -maxdepth 1)" = ./.ranikust-pruning && rm .ranikust-pruning"#;

/// How root empties the state directory `dir`, pinned as `pin`, with
/// ordinary tools; `entries` when it holds anything, else it is only
/// removed.
fn root_empties(dir: &Path, pin: Pin, entries: bool) -> String {
    let rmdir = remove_dir(dir, pin);
    if !entries {
        return format!("root removes the empty state directory with {rmdir}");
    }
    let rm = in_dir(dir, pin, ROOT_SCRIPT);
    format!(
        "root empties a state directory with ordinary tools while no build uses it and nothing \
         is mounted below it, its marker first so that no build takes what an interrupted \
         removal leaves, and nothing in a directory without one or other than the one ranikust \
         checked: {rm}, then {rmdir}"
    )
}

/// What root does instead with the state directory `dir` (ADR-0105): the
/// commands of [`root_empties`] for the directory [`pin`] checks at the
/// path as given, else none.
fn root_guidance(dir: &Path) -> String {
    let what = match pin(dir) {
        Ok((pin, entries)) => root_empties(dir, pin, entries),
        Err(why) => no_commands(dir, &why),
    };
    format!("{what}; Phase 5's ranikust prune will serve root too")
}

/// Why no command is given for `dir`, and what to do instead.
fn no_commands(dir: &Path, why: &str) -> String {
    format!(
        "ranikust gives no commands for {}, which it cannot check ({why}): give the path with \
         no symbolic link on it; for a user's directory, run ranikust prune --all as that user",
        shell_word(dir.as_os_str())
    )
}

/// What a user does with the state directory `dir`, pinned as `pin`,
/// where it gets no user namespace: the build's guidance ends in "run
/// ranikust as root", which `prune --all` refuses.
pub(crate) fn no_user_namespace(dir: &Path, pin: Result<Pin, String>) -> String {
    let what = match pin {
        Ok(pin) => format!(
            "while nothing is mounted below the state directory, {} renames its marker, which \
             keeps builds out, removes the user's own files, then the marker once nothing else \
             is left; files of subordinate ids stay, and the marker with them, for a ranikust \
             prune --all where a user namespace is allowed to finish",
            in_dir(dir, pin, USER_SCRIPT)
        ),
        Err(why) => no_commands(dir, &why),
    };
    format!(
        "a user other than root gets a user namespace of its own only where \
         kernel.apparmor_restrict_unprivileged_userns is 0, or an AppArmor profile allows \
         ranikust one (Ubuntu 23.10 and later); without it, {what}"
    )
}

/// The ways out of the state directory `dir`, pinned as `pin`, that
/// `prune --all` could not finish emptying ([`rnk_store::Error::NotPruned`]),
/// which no build opens.
pub(crate) fn unfinished(dir: &Path, pin: Result<Pin, String>) -> String {
    let d = shell_word(dir.as_os_str());
    let commands = match pin {
        Ok(pin) => format!(
            "; or move the directory aside and remove it, {}; or, as root, empty it in place, {}, \
             then {}",
            move_aside(dir, pin),
            in_dir(dir, pin, ROOT_SCRIPT),
            remove_dir(dir, pin)
        ),
        Err(why) => format!("; {}", no_commands(dir, &why)),
    };
    format!(
        "ranikust prune --all leaves what is on another mount, and files of ids it cannot remove \
         (a rootless build's subordinate ids, where no user namespace maps them): unmount \
         anything below {d} first; then run ranikust prune --all again, with newuidmap \
         installed and ranges for this user in /etc/subuid and /etc/subgid{commands}"
    )
}

/// What a build does with the state directory `dir` that a `prune --all`
/// did not finish emptying ([`rnk_store::Error::PruneCutShort`]), for the
/// effective uid `euid` outside any user namespace of the build: a user
/// other than root runs `prune --all` again, which root may not.
pub(crate) fn cut_short(dir: &Path, euid: u32) -> String {
    if euid == 0 {
        return match pin(dir) {
            Ok((pin, entries)) => root_empties(dir, pin, entries),
            Err(why) => no_commands(dir, &why),
        };
    }
    format!(
        "ranikust prune --all finishes emptying it or says what stops it: run ranikust prune \
         --all --state-dir {}",
        shell_word(dir.as_os_str())
    )
}

/// Which prune left the store error `e` behind, with the state directory
/// it names: in one place, for the guidance of `prune --all` and a build.
pub(crate) fn unfinished_prune(e: &rnk_store::Error) -> Option<(Unfinished, &Path)> {
    use rnk_store::Error;
    match e {
        Error::NotPruned { path, .. } => Some((Unfinished::NotPruned, path)),
        Error::PruneCutShort { path } => Some((Unfinished::CutShort, path)),
        Error::NotAStateDir { .. }
        | Error::NoFlock { .. }
        | Error::Busy { .. }
        | Error::InUse { .. }
        | Error::StateNotEmpty { .. }
        | Error::UsedOnSlash { .. }
        | Error::NotEmptied { .. }
        | Error::Unmarked { .. }
        | Error::Io { .. }
        | Error::BadEntry { .. }
        | Error::Cancelled
        | Error::Anonymous { .. }
        | Error::Random(_)
        | Error::AnonymousFile { .. }
        | Error::NameOutlivesUnlink { .. }
        | Error::Rootfs(_)
        | Error::Fill { .. } => None,
    }
}

/// How a `prune --all` left a state directory unfinished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unfinished {
    /// This prune could not remove everything.
    NotPruned,
    /// An earlier one was cut short or failed.
    CutShort,
}

/// The state directory a printed command acts on, as `stat -c %d:%i:%u`
/// prints it (GNU and BusyBox): each command checks it right after
/// entering a directory, before anything else, so one run after the path
/// changed, a parent swapped for a symlink included, fails there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pin {
    dev: u64,
    ino: u64,
    uid: u32,
}

impl Pin {
    /// The pin of the state directory `dir` that `check` describes, for
    /// commands that name it: `dir` must end in a name, which they enter
    /// its parent to act on.
    pub(crate) fn of(dir: &Path, check: &rnk_store::PruneCheck) -> Result<Self, String> {
        match (dir.parent(), dir.file_name()) {
            (Some(_), Some(_)) => Ok(Self {
                dev: check.dev,
                ino: check.ino,
                uid: check.uid,
            }),
            _ => Err(format!("{} does not end in a name", dir.display())),
        }
    }

    /// The check of the entry `entry` against the pin, for `test`.
    fn test(self, entry: &str) -> String {
        let Self { dev, ino, uid } = self;
        format!(r#"test "$(stat -c %d:%i:%u {entry})" = {dev}:{ino}:{uid}"#)
    }
}

/// The state directory `dir` checked as [`Store::check_prune`] does, at
/// the path as given with no symlink followed on it, with its [`Pin`] and
/// whether it holds anything; else why no command can be given for it.
/// This process's user namespace must be the one the commands run in, for
/// the owner.
pub(crate) fn pin(dir: &Path) -> Result<(Pin, bool), String> {
    let check = Store::check_prune(dir).map_err(|e| e.to_string())?;
    Ok((Pin::of(dir, &check)?, check.entries))
}

/// `script` run by `sh` in the state directory `dir`, pinned as `pin`,
/// which it enters once (`cd -P`, `CDPATH` unset), checks against the pin,
/// then names only as `.`: the lock, the marker's check and the deletion
/// all act on the directory checked, whatever is put at its path
/// meanwhile. The lock is the exclusive `flock` [`Store::prune_all`]
/// takes, so it fails while a build holds the directory; at a missing
/// path, `cd` fails and nothing is made. `script` holds no single quote.
fn in_dir(dir: &Path, pin: Pin, script: &str) -> String {
    debug_assert!(!script.contains('\''), "{script}");
    let d = shell_word(dir.as_os_str());
    format!(
        "( CDPATH= cd -P -- {d} && {} && exec flock -xn . sh -c '{script}' )",
        pin.test(".")
    )
}

/// The parent of `dir`, entered once, and `dir`'s name in it, as `./name`
/// (no word starting with `=` or `-`), with `suffix` added.
fn parent_and_name(dir: &Path, suffix: &str) -> (String, String) {
    let parent = dir.parent().unwrap_or(Path::new("/"));
    let mut name = std::ffi::OsString::from("./");
    name.push(dir.file_name().unwrap_or(dir.as_os_str()));
    name.push(suffix);
    (shell_word(parent.as_os_str()), shell_word(&name))
}

/// Removes the empty state directory `dir`, pinned as `pin`, from its
/// parent, entered once: never a symlink or another directory put at its
/// name.
fn remove_dir(dir: &Path, pin: Pin) -> String {
    let (parent, name) = parent_and_name(dir, "");
    format!(
        "( CDPATH= cd -P -- {parent} && {} && rmdir -- {name} )",
        pin.test(&name)
    )
}

/// A user's way to move the state directory `dir`, pinned as `pin`, aside
/// and remove it in the user's namespace of `podman`: from its parent,
/// entered once, only the directory checked, never a symlink.
fn move_aside(dir: &Path, pin: Pin) -> String {
    let (parent, name) = parent_and_name(dir, "");
    let (_, aside) = parent_and_name(dir, ".old");
    format!(
        "( CDPATH= cd -P -- {parent} && test -d {name} && ! test -h {name} && {} && mv -T -- \
         {name} {aside} && podman unshare rm -rf --one-file-system -- {aside} )",
        pin.test(&name)
    )
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
/// root, changing nothing, with commands only for a directory [`pin`]
/// checks; else [`empty`]. Call while single-threaded.
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
    let check = match found {
        Found::Missing => return Ok(Pruned::Missing),
        Found::Empty => return Ok(Pruned::Empty),
        Found::Entries(check) => check,
    };
    // Outside the namespace, where the commands run: the owner as they see
    // it.
    let pin = Pin::of(state_dir, &check);
    setup::enter_build_userns(no_user_namespace(state_dir, pin.clone()).into())?;
    // The deletion holds a descriptor per directory level, and a `RUN` may
    // have left a deep tree.
    match statedir::raise_nofile_limit() {
        Ok(limit) => tracing::debug!(?limit, "RLIMIT_NOFILE raised to its hard limit"),
        Err(e) => tracing::warn!("cannot raise RLIMIT_NOFILE to its hard limit: {e}"),
    }
    match Store::prune_all(state_dir) {
        Ok(true) => Ok(Pruned::Emptied),
        Ok(false) => Ok(Pruned::Empty),
        Err(e) => Err(store_refusal(&e, state_dir, pin)),
    }
}

/// [`Store::prune_all`]'s error `e` on `state_dir`, pinned outside as
/// `pin`, met in the build's user namespace: of [`Kind::Broken`], with the
/// ways out when entries stay.
fn store_refusal(e: &rnk_store::Error, state_dir: &Path, pin: Result<Pin, String>) -> Refusal {
    setup::inside_userns(Refusal {
        kind: Kind::Other,
        reason: e.to_string(),
        guidance: match unfinished_prune(e) {
            Some((Unfinished::NotPruned, _)) => unfinished(state_dir, pin).into(),
            Some((Unfinished::CutShort, _)) | None => "".into(),
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
