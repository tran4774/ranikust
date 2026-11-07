//! `ranikust prune --all` (ADR-0105): empties the state directory of a user
//! other than root inside the build's user namespace, where the files of
//! subordinate ids a rootless build leaves can be removed (the user outside
//! cannot). Phase 5's `prune`, by size and age (ADR-0032), grows from it.
//!
//! In order, each step before the next: without `--all`, a usage error
//! naming Phase 5; root refused (euid 0 in any user namespace, a
//! container's root included), naming Phase 5's `prune`; a path ending in
//! `..` refused, before anything is opened; then a build's checks of the
//! state directory, outside, which neither make it nor change its mode
//! ([`statedir::check`]): one that does not exist, or is empty, is left
//! so; then the build's user namespace
//! ([`setup::enter_build_userns`]); then, in it,
//! [`rnk_store::Store::prune_all`], which removes nothing while a build
//! holds the store, refuses a directory that is not the store's, and keeps
//! builds out of one it did not finish emptying. The commands a refusal
//! suggests name the state directory, quoted for a shell, and act only on
//! the directory ranikust checked ([`Pin`]).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};

use rnk_sandbox::{IdRange, SubordinateIds};
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

/// Why neither root's commands nor a user's `prune --all` take a path
/// that ends in `..` ([`ends_in_dotdot`]).
const DOTDOT: &str = "ends in ..: give the state directory's own path";

/// What the scripts need, as a literal: `concat!` takes no constant.
macro_rules! needs_gnu_find {
    () => {
        "needs GNU findutils (Alpine: apk add findutils; a BusyBox sh may run its own find applet)"
    };
}

/// What the printed guidance says the scripts need.
const NEEDS_GNU_FIND: &str = needs_gnu_find!();

/// `$rest` after the check both scripts start with: unless `find` is GNU
/// findutils' and lists `FTS(FTS_CWDFD)`, the walk they are written for,
/// the script stops there with nothing changed, saying what it needs
/// ([`NEEDS_GNU_FIND`]).
macro_rules! gnu_find_first {
    ($rest:expr) => {
        concat!(
            r#"find --version 2>/dev/null | grep -q "GNU findutils" && find --version 2>/dev/null | grep -qF "FTS(FTS_CWDFD)" || { echo ""#,
            needs_gnu_find!(),
            r#"" >&2; exit 1; }; "#,
            $rest
        )
    };
}

/// Root's emptying's step before any deletion: its regular marker,
/// `.ranikust-state` or else the `.ranikust-pruning` a prune that did not
/// finish left, removed first, so that what an interrupted removal leaves
/// is unmarked; the script stops with a message where there is neither
/// marker, and with another where `rm` fails.
macro_rules! root_marker_first {
    () => {
        r#"if test -f .ranikust-state && ! test -h .ranikust-state; then marker=.ranikust-state; elif test -f .ranikust-pruning && ! test -h .ranikust-pruning; then marker=.ranikust-pruning; else echo "no regular .ranikust-state or .ranikust-pruning here: not a state directory, or an emptying already started; finish it by hand" >&2; exit 1; fi; rm "$marker" || { echo "cannot remove $marker: nothing else changed" >&2; exit 1; }; "#
    };
}

/// Root's emptying of a state directory root owns, for [`in_dir`]: with
/// GNU `find` only ([`gnu_find_first`]), the marker first
/// ([`root_marker_first`]), then everything, with no top-level entry an
/// operand. `find -xdev` stops at a mount of another filesystem (`find`
/// then fails), not at a bind mount of the same one, whose contents go:
/// hence the guidance's "nothing is mounted below it". Root's store may
/// hold files of image ids that are host accounts' too, so every owner's
/// go.
const ROOT_SCRIPT: &str = gnu_find_first!(concat!(
    root_marker_first!(),
    "find . -xdev -mindepth 1 -delete"
));

/// The owner of a state directory, whose entries root's emptying deletes
/// ([`root_script`]): `uid`, and, for a user other than root, `range`, the
/// first `/etc/subuid` range of it, the only other uids its builds' user
/// namespace maps; `unread`, why that range could not be read.
#[derive(Clone, Debug)]
struct Owner {
    uid: u32,
    range: Option<IdRange>,
    unread: Option<String>,
}

impl Owner {
    /// `uid` with the range [`crate::subids::lookup`] reads for it.
    fn of(uid: u32) -> Self {
        Self::from_lookup(uid, crate::subids::lookup(uid))
    }

    /// `uid` with the range `lookup` found: none where there is none or it
    /// failed, whose error is kept for the guidance.
    fn from_lookup(uid: u32, lookup: Result<Option<SubordinateIds>, String>) -> Self {
        match lookup {
            Ok(ids) => Self::new(uid, ids.map(|ids| ids.uids)),
            // No range keeps more: only the owner's own uid's entries go.
            Err(e) => Self {
                unread: Some(e),
                ..Self::new(uid, None)
            },
        }
    }

    /// `uid` with `range`, but for one that holds root's uid or ends past
    /// the last uid, which is left out, its entries kept.
    fn new(uid: u32, range: Option<IdRange>) -> Self {
        let range =
            range.filter(|r| r.start > 0 && r.count > 0 && r.start.checked_add(r.count).is_some());
        Self {
            uid,
            range,
            unread: None,
        }
    }

    /// The range's first and last uids, as `first-last`.
    fn range_text(range: IdRange) -> String {
        format!("{}-{}", range.start, range.start + (range.count - 1))
    }
}

/// Root's emptying of the state directory it runs in, which `owner` owns,
/// for [`in_dir`]: [`ROOT_SCRIPT`] where root owns it; else the same steps
/// for only the entries of the owner's uid and of its range (`find`'s `+n`
/// and `-n` are strict), so that those of other uids are left in place:
/// where any is found, or a listing fails, the script stops, saying so,
/// before the marker goes; where any is left after the deletion, it stops
/// there, the directory not empty.
fn root_script(owner: &Owner) -> String {
    let uid = owner.uid;
    if uid == 0 {
        return ROOT_SCRIPT.to_owned();
    }
    let (or_range, and_range) = match owner.range {
        Some(range) => (
            format!(
                r" -o \( -uid +{} -uid -{} \)",
                range.start - 1,
                range.start + range.count
            ),
            format!(" and its subordinate range {}", Owner::range_text(range)),
        ),
        None => (String::new(), String::new()),
    };
    let ours = format!(r"\( -uid {uid}{or_range} \)");
    let mut script = String::from(gnu_find_first!(""));
    script.push_str(&format!(
        r#"test -z "$(find . -xdev -mindepth 1 ! {ours} -print -quit || echo failed)" || {{ echo "nothing changed: entries of uids other than {uid}{and_range} are here, which this emptying would leave in place, or find failed" >&2; exit 1; }}; "#
    ));
    script.push_str(root_marker_first!());
    script.push_str(&format!(
        r#"{{ find . -xdev -mindepth 1 {ours} -delete && test -z "$(find . -mindepth 1 -maxdepth 1 || echo failed)" || {{ echo "not emptied: entries of uids other than {uid}{and_range} are left in place, as is anything find names above" >&2; exit 1; }}; }}"#
    ));
    script
}

/// What [`root_script`] deletes in a directory `owner` owns, to follow "the
/// emptying needs GNU findutils": nothing to add for root.
fn owner_only(owner: &Owner) -> String {
    let uid = owner.uid;
    if uid == 0 {
        return String::new();
    }
    let range = owner.range.map_or(String::new(), |range| {
        format!(" and of its subordinate ids {},", Owner::range_text(range))
    });
    let unread = owner.unread.as_ref().map_or(String::new(), |e| {
        format!(" not of its subordinate ids, which ranikust could not read ({e}),")
    });
    format!(
        " and deletes only the entries of uid {uid}, the directory's owner,{range}{unread} \
         changing nothing where another uid owns any"
    )
}

/// A user's emptying of the state directory it runs in without a user
/// namespace, for [`in_dir`]: with GNU `find` only ([`gnu_find_first`]);
/// first a regular `.ranikust-pruning` an earlier run left is kept, or
/// else, where nothing has that name, the regular marker is renamed to it
/// (`mv -T`, never into a directory), which keeps builds out; then all but
/// it, as [`ROOT_SCRIPT`] does; it last, once a listing shows nothing else
/// left, since not every `find` fails when a removal does.
const USER_SCRIPT: &str = gnu_find_first!(
    r#"{ test -f .ranikust-pruning && ! test -h .ranikust-pruning || { ! test -e .ranikust-pruning && ! test -h .ranikust-pruning && test -f .ranikust-state && ! test -h .ranikust-state && mv -T .ranikust-state .ranikust-pruning; }; } && find . -xdev -mindepth 1 ! -path ./.ranikust-pruning -delete && test "$(find . -mindepth 1 -maxdepth 1 || echo failed)" = ./.ranikust-pruning && rm .ranikust-pruning"#
);

/// How root empties the state directory `dir`, pinned as `pin`, which
/// holds a marker, with ordinary tools, then removes it.
fn root_empties(dir: &Path, pin: Pin) -> String {
    let owner = Owner::of(pin.uid);
    format!(
        "root empties a state directory with ordinary tools while no build uses it and nothing \
         is mounted below it, its marker first so that no build takes what an interrupted \
         removal leaves, and nothing in a directory without one or other than the one ranikust \
         checked; the commands refuse a PATH with a relative or empty entry and unset \
         LD_LIBRARY_PATH, LD_PRELOAD, LD_AUDIT and GCONV_PATH; the emptying \
         {NEEDS_GNU_FIND}{}: {}, then {}",
        owner_only(&owner),
        in_dir(dir, pin, &root_script(&owner)),
        remove_dir(dir, pin)
    )
}

/// What root may do with the state directory `dir`, for root's refusal
/// and a build's: the commands of [`root_empties`] for a directory [`pin`]
/// checks at the path as given, a build would accept, and finds a marker
/// in, else none.
fn root_commands(dir: &Path) -> String {
    match pin(dir) {
        Ok((pin, true)) => root_empties(dir, pin),
        Ok((_, false)) => format!(
            "ranikust gives no commands for {}, which is empty: it gives commands only for a \
             directory that holds its marker",
            shell_word(dir.as_os_str())
        ),
        Err(why) => no_commands(dir, &why, true),
    }
}

/// What root does instead with the state directory `dir` (ADR-0105).
fn root_guidance(dir: &Path) -> String {
    format!(
        "{}; Phase 5's ranikust prune will serve root too",
        root_commands(dir)
    )
}

/// Why no command is given for `dir`, and what to do instead; `root` when
/// root reads it, who may be pruning a user's directory.
fn no_commands(dir: &Path, why: &NoPin, root: bool) -> String {
    let d = shell_word(dir.as_os_str());
    match why {
        NoPin::NoName => format!("ranikust gives no commands for {d}, which {DOTDOT}"),
        NoPin::Symlink(why) => {
            let user = if root {
                "; for a user's directory, run ranikust prune --all as that user"
            } else {
                ""
            };
            format!(
                "ranikust gives no commands for {d}, which it cannot open without following a \
                 symbolic link ({why}): give the path with no symbolic link on it{user}"
            )
        }
        NoPin::Refused(why) => format!("ranikust gives no commands for {d}: {why}"),
    }
}

/// What a user does with the state directory `dir`, pinned as `pin`,
/// where it gets no user namespace: its own files' emptying, which leaves
/// files of its subordinate ids, or root's `prune --all`, whose commands
/// empty the directory of those too (ADR-0133). Not the build's guidance,
/// whose "run ranikust as root" `prune --all` refuses.
pub(crate) fn no_user_namespace(dir: &Path, pin: Result<Pin, NoPin>) -> String {
    let what = match pin {
        Ok(pin) => format!(
            "while nothing is mounted below the state directory, {}, which {NEEDS_GNU_FIND}, \
             renames its marker, which keeps builds out, removes the user's own files, then the \
             marker once nothing else is left; files of subordinate ids stay, and the marker \
             with them, for a ranikust prune --all where a user namespace is allowed to finish; \
             or {}",
            in_dir(dir, pin, USER_SCRIPT),
            ask_root(dir)
        ),
        Err(why) => no_commands(dir, &why, false),
    };
    format!(
        "a user other than root gets a user namespace of its own only where \
         kernel.apparmor_restrict_unprivileged_userns is 0, or an AppArmor profile allows \
         ranikust one (Ubuntu 23.10 and later); without it, {what}"
    )
}

/// The way out through root of a user's state directory `dir` that the
/// user's own `prune --all` cannot empty: root's `prune --all`, which
/// takes the directory as root sees it and prints root's commands.
fn ask_root(dir: &Path) -> String {
    format!(
        "have root run ranikust prune --all --state-dir {}, which gives root's commands to \
         empty it in place",
        shell_word(dir.as_os_str())
    )
}

/// The ways out of the state directory `dir`, pinned as `pin`, that
/// `prune --all` could not finish emptying ([`rnk_store::Error::NotPruned`]),
/// which no build opens; root's is [`ask_root`].
pub(crate) fn unfinished(dir: &Path, pin: Result<Pin, NoPin>) -> String {
    let d = shell_word(dir.as_os_str());
    let commands = match pin {
        Ok(pin) => format!(
            "; or move the directory aside and remove it, {}; or {}",
            move_aside(dir, pin),
            ask_root(dir)
        ),
        Err(why) => format!("; {}", no_commands(dir, &why, false)),
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
        return root_commands(dir);
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

/// The state directory a printed command acts on, as `stat -c
/// %d:%i:%u:%a` prints it (GNU and BusyBox): each command checks it right
/// after entering a directory, before anything else, so one run after the
/// path changed, a parent swapped for a symlink included, or after its
/// owner or mode changed, fails there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pin {
    dev: u64,
    ino: u64,
    uid: u32,
    /// The permission bits, special ones included, as `%a` prints them in
    /// octal.
    mode: u32,
}

impl Pin {
    /// The pin of the state directory `dir` that `check` describes, for
    /// commands that name it: `dir` must end in a name, which they enter
    /// its parent to act on, and pass the build's rule with its owner as
    /// the user ([`statedir::check_for_commands`]), so that no command acts
    /// on a directory a build would refuse. [`pin`] and [`empty`] turn away
    /// a path ending in `..` first, so only `/`, which their checks refuse,
    /// has no name.
    pub(crate) fn of(dir: &Path, check: &rnk_store::PruneCheck) -> Result<Self, NoPin> {
        if dir.parent().is_none() || dir.file_name().is_none() {
            return Err(NoPin::NoName);
        }
        let mode = statedir::check_for_commands(dir, check).map_err(|r| NoPin::Refused(r.0))?;
        Ok(Self {
            dev: check.dev,
            ino: check.ino,
            uid: check.uid,
            mode,
        })
    }

    /// The check of the entry `entry`, one shell word, against the pin;
    /// piped rather than compared in a quoted command substitution, so an
    /// interactive shell's history expansion or quoting does not touch it.
    fn test(self, entry: &str) -> String {
        let Self {
            dev,
            ino,
            uid,
            mode,
        } = self;
        format!("stat -c %d:%i:%u:%a {entry} | grep -qxF {dev}:{ino}:{uid}:{mode:o}")
    }
}

/// Why a refusal gives no commands for a state directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoPin {
    /// Its path ends in `..`, not in its name, whose parent the commands
    /// enter to act on it.
    NoName,
    /// [`Store::check_prune`] could not open the path as given without
    /// following a symbolic link on it.
    Symlink(String),
    /// [`Store::check_prune`] refused it for any other reason, which says
    /// what to do.
    Refused(String),
}

/// Whether `dir` ends in `..`: whatever that leads to, the commands cannot
/// name it by its parent and its name.
fn ends_in_dotdot(dir: &Path) -> bool {
    dir.components().next_back() == Some(Component::ParentDir)
}

/// The state directory `dir` checked as [`Store::check_prune`] does, at
/// the path as given with no symlink followed on it, with its [`Pin`] and
/// whether it holds anything; else why no command can be given for it.
/// This process's user namespace must be the one the commands run in, for
/// the owner.
pub(crate) fn pin(dir: &Path) -> Result<(Pin, bool), NoPin> {
    if ends_in_dotdot(dir) {
        return Err(NoPin::NoName);
    }
    let check = Store::check_prune(dir).map_err(|e| {
        if e.is_symlink_on(dir) {
            NoPin::Symlink(e.to_string())
        } else {
            NoPin::Refused(e.to_string())
        }
    })?;
    Ok((Pin::of(dir, &check)?, check.entries))
}

/// What each printed command runs first, before it enters a directory,
/// so that the tools it runs there are the system's, loading the system's
/// libraries: a `PATH` with a relative or empty entry, or a loader
/// variable naming libraries or modules by path, could run code from the
/// directory entered. It stops, saying so, at such a `PATH`, and unsets
/// those variables, stopping where one cannot be unset. Single-quoted, for
/// `eval`, so that no interactive shell's history expansion reads its `!`.
pub(crate) const ENV_CHECK: &str = r#"eval 'case ":$PATH:" in *::*|*:[!/]*) echo "PATH holds a relative or empty entry: use absolute directories only" >&2; exit 1;; esac; unset LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT GCONV_PATH || { echo "cannot unset LD_LIBRARY_PATH, LD_PRELOAD, LD_AUDIT or GCONV_PATH: nothing changed" >&2; exit 1; }'"#;

/// `script` run by `sh` in the state directory `dir`, pinned as `pin`,
/// which it enters once ([`ENV_CHECK`] first, then `cd -P`, `CDPATH`
/// unset), checks against the pin, then names only as `.`: the lock, the
/// marker's check and the deletion all act on the directory checked,
/// whatever is put at its path meanwhile. The lock is the exclusive
/// `flock` [`Store::prune_all`] takes, so it fails while a build holds the
/// directory; at a missing path, `cd` fails and nothing is made. `script`
/// holds no single quote.
fn in_dir(dir: &Path, pin: Pin, script: &str) -> String {
    debug_assert!(!script.contains('\''), "{script}");
    let d = shell_word(dir.as_os_str());
    format!(
        "( {ENV_CHECK} && CDPATH= cd -P -- {d} && {} && exec flock -xn . sh -c '{script}' )",
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
/// parent, entered once after [`ENV_CHECK`]: never a symlink or another
/// directory put at its name.
fn remove_dir(dir: &Path, pin: Pin) -> String {
    let (parent, name) = parent_and_name(dir, "");
    format!(
        "( {ENV_CHECK} && CDPATH= cd -P -- {parent} && {} && rmdir -- {name} )",
        pin.test(&name)
    )
}

/// A user's way to move the state directory `dir`, pinned as `pin`, aside
/// and remove it in the user's namespace of `podman`: from its parent,
/// entered once after [`ENV_CHECK`], only the directory checked, never a
/// symlink.
fn move_aside(dir: &Path, pin: Pin) -> String {
    let (parent, name) = parent_and_name(dir, "");
    let (_, aside) = parent_and_name(dir, ".old");
    format!(
        "( {ENV_CHECK} && CDPATH= cd -P -- {parent} && test -d {name} && ! test -h {name} && \
         {} && mv -T -- {name} {aside} && podman unshare rm -rf --one-file-system -- {aside} )",
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
/// checks and that holds a marker; else [`empty`]. Call while single-threaded.
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
/// process enters for good: a path ending in `..` refused before anything
/// is opened, as root's commands refuse it; the state directory's checks
/// outside, which change nothing ([`statedir::check`]); nothing more when
/// it does not exist or is empty; else the namespace, then
/// [`Store::prune_all`] in it. Call while single-threaded.
///
/// # Errors
///
/// A [`Refusal`]: naming a path ending in `..` or the state directory's
/// check that failed; as
/// [`setup::enter_build_userns`]; or naming what [`Store::prune_all`]
/// refused or left, then of [`Kind::Broken`].
pub(crate) fn empty(state_dir: &Path) -> Result<Pruned, Refusal> {
    if ends_in_dotdot(state_dir) {
        return Err(Refusal {
            kind: Kind::Other,
            reason: format!("the state directory {} {DOTDOT}", state_dir.display()),
            guidance: "".into(),
        });
    }
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
    match Store::prune_all(state_dir) {
        Ok(true) => Ok(Pruned::Emptied),
        Ok(false) => Ok(Pruned::Empty),
        Err(e) => Err(store_refusal(&e, state_dir, pin)),
    }
}

/// [`Store::prune_all`]'s error `e` on `state_dir`, pinned outside as
/// `pin`, met in the build's user namespace: of [`Kind::Broken`], with the
/// ways out when entries stay.
fn store_refusal(e: &rnk_store::Error, state_dir: &Path, pin: Result<Pin, NoPin>) -> Refusal {
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
