//! Stub mount points, their journal, and the per-`RUN` scratch files.
//!
//! An image may lack a mount target (`alpine` has no `/etc/resolv.conf`).
//! The empty file or directory created as the target is a real entry of the
//! root: it would reach the layer, and it survives a killed build. So, as in
//! BuildKit's `executor/stubs.go`, each `RUN` with mounts has a journal,
//! `<state>/sandbox/runs/<id>.journal`, locked with `flock` while the `RUN`
//! lives, next to `<id>/`, its writable `/etc` copies. One record per line,
//! tab-separated, paths in hex:
//!
//! - `root <path>`: the host path of the root, first.
//! - `stub <d|f> <path> <atime> <mtime>`: written and synced *before* the
//!   stub exists, with the times its parent had before.
//! - `placed <path> <mtime>`: the parent's mtime just after; all `placed`
//!   records are synced before the `RUN` starts.
//!
//! Stubs are removed after the `RUN`, newest first, while still empty; the
//! parent's times go back to the old ones only if its mtime still is the
//! `placed` one (no `placed` record: the build died before the `RUN`). At
//! start-up, journals no live process holds are replayed the same way.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Component, Path, PathBuf};

use rnk_rootfs::{FileTime, FileType, Meta, OpenHow, RootPath};
use rustix::fs::FlockOperation;
use rustix::io::Errno;

use crate::error::{Error, Result, io_err};
use crate::spec::MountKind;

/// Symlinks followed while placing one target, as the kernel's limit.
const MAX_SYMLINKS: u32 = 40;

/// Extension of a journal.
const JOURNAL_EXT: &str = "journal";
/// Extension of a journal being created, before it is locked and renamed.
const NEW_EXT: &str = "journal-new";

/// Scratch files of a run, removed by name, never recursively.
pub(crate) const SCRATCH_FILES: [&str; 3] = ["resolv.conf", "hosts", "hostname"];

/// One stub as journaled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StubRecord {
    pub(crate) kind: MountKind,
    /// Absolute path inside the root.
    pub(crate) path: PathBuf,
    /// The parent's times before the stub was created.
    pub(crate) parent_atime: FileTime,
    pub(crate) parent_mtime: FileTime,
    /// The parent's mtime right after, once recorded.
    pub(crate) placed_mtime: Option<FileTime>,
}

/// The journal and scratch directory of one `RUN`.
#[derive(Debug)]
pub(crate) struct Journal {
    file: File,
    path: PathBuf,
    scratch: PathBuf,
    root: RootPath,
    stubs: Vec<StubRecord>,
    /// `placed` records written since the last sync.
    unsynced: bool,
}

impl Journal {
    /// Creates and locks `<runs_dir>/<id>.journal` for the root `root_dir`,
    /// and the empty scratch directory `<runs_dir>/<id>/`. The journal is
    /// written and locked under a temporary name and renamed afterwards, so
    /// a replay never sees it unlocked.
    pub(crate) fn create(
        runs_dir: &Path,
        id: &str,
        root_dir: &Path,
        root: RootPath,
    ) -> Result<Self> {
        let path = runs_dir.join(format!("{id}.{JOURNAL_EXT}"));
        let new = runs_dir.join(format!("{id}.{NEW_EXT}"));
        let file = File::options()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&new)
            .map_err(|e| io_err("create the stub journal", &new, e))?;
        rustix::fs::flock(&file, FlockOperation::LockExclusive)
            .map_err(|e| io_err("lock the stub journal", &new, e.into()))?;
        if path.exists() {
            let _ = fs::remove_file(&new);
            return Err(io_err(
                "create the stub journal",
                &path,
                io::Error::from(io::ErrorKind::AlreadyExists),
            ));
        }
        fs::rename(&new, &path).map_err(|e| io_err("publish the stub journal", &path, e))?;
        File::open(runs_dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| io_err("sync", runs_dir, e))?;
        let mut journal = Self {
            file,
            scratch: runs_dir.join(id),
            path,
            root,
            stubs: Vec::new(),
            unsynced: false,
        };
        journal.append(&format!("root\t{}\n", hex(root_dir.as_os_str().as_bytes())))?;
        fs::create_dir(&journal.scratch)
            .map_err(|e| io_err("create the run directory", &journal.scratch, e))?;
        Ok(journal)
    }

    /// The directory for the run's scratch files.
    pub(crate) fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// Stubs placed so far.
    pub(crate) fn stub_count(&self) -> usize {
        self.stubs.len()
    }

    /// Makes `target` exist in the root, as a directory or an empty file,
    /// creating each missing component as a journaled stub. Symlinks on the
    /// way, dangling ones included, are followed inside the root, so the stub
    /// lands where the mount will resolve the target.
    pub(crate) fn ensure(&mut self, target: &Path, kind: MountKind) -> Result<usize> {
        let mut created = 0;
        let mut cur = PathBuf::from("/");
        let mut rest = components(target);
        let mut hops = 0;
        while let Some(name) = rest.pop_front() {
            if name == ".." {
                cur.pop();
                continue;
            }
            let next = cur.join(&name);
            let last = rest.is_empty();
            match self.root.statx(&next) {
                Ok(st) => match st.file_type() {
                    FileType::Symlink => {
                        hops += 1;
                        if hops > MAX_SYMLINKS {
                            return Err(io_err(
                                "place a mount target",
                                &next,
                                io::Error::from_raw_os_error(libc::ELOOP),
                            ));
                        }
                        let link = self.root.read_link(&next)?;
                        if link.is_absolute() {
                            cur = PathBuf::from("/");
                        }
                        for c in components(&link).into_iter().rev() {
                            rest.push_front(c);
                        }
                    }
                    FileType::Directory => cur = next,
                    // An existing non-directory target: the mount decides.
                    _ if last => cur = next,
                    _ => {
                        return Err(io_err(
                            "place a mount target",
                            &next,
                            io::Error::from_raw_os_error(libc::ENOTDIR),
                        ));
                    }
                },
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                    let kind = if last { kind } else { MountKind::Directory };
                    self.place(&cur, &next, kind)?;
                    created += 1;
                    cur = next;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(created)
    }

    fn place(&mut self, parent: &Path, path: &Path, kind: MountKind) -> Result<()> {
        let before = self.root.statx(parent)?;
        let (code, mode) = match kind {
            MountKind::Directory => ('d', 0o755),
            MountKind::File => ('f', 0o644),
        };
        self.append(&format!(
            "stub\t{code}\t{}\t{}\t{}\n",
            hex(path.as_os_str().as_bytes()),
            time(before.atime),
            time(before.mtime),
        ))?;
        self.sync()?;
        match kind {
            MountKind::Directory => self.root.create_dir(path, mode)?,
            MountKind::File => drop(
                self.root
                    .open_at(path, &OpenHow::write().create_new(mode))?,
            ),
        }
        let mut record = StubRecord {
            kind,
            path: path.to_owned(),
            parent_atime: before.atime,
            parent_mtime: before.mtime,
            placed_mtime: None,
        };
        // The record is complete before anything else can fail, so cleanup
        // finds the stub.
        self.stubs.push(record.clone());
        // The umask must not decide whether a non-root `USER` can traverse a
        // stub directory.
        self.root.set_meta(
            path,
            &Meta {
                mode: Some(mode),
                ..Meta::default()
            },
        )?;
        let after = self.root.statx(parent)?;
        self.append(&format!(
            "placed\t{}\t{}\n",
            hex(path.as_os_str().as_bytes()),
            time(after.mtime)
        ))?;
        record.placed_mtime = Some(after.mtime);
        if let Some(last) = self.stubs.last_mut() {
            *last = record;
        }
        self.unsynced = true;
        Ok(())
    }

    /// Syncs the `placed` records; call before the `RUN` starts.
    pub(crate) fn commit(&mut self) -> Result<()> {
        if self.unsynced {
            self.sync()?;
            self.unsynced = false;
        }
        Ok(())
    }

    /// Removes the stubs and the scratch files, then the journal. On error the
    /// journal stays for the next start-up.
    pub(crate) fn finish(self) -> Result<usize> {
        let removed = remove_stubs(&self.root, &self.stubs)?;
        remove_scratch(&self.scratch)?;
        fs::remove_file(&self.path)
            .map_err(|e| io_err("remove the stub journal", &self.path, e))?;
        Ok(removed)
    }

    fn append(&mut self, line: &str) -> Result<()> {
        self.file
            .write_all(line.as_bytes())
            .map_err(|e| io_err("write the stub journal", &self.path, e))
    }

    fn sync(&mut self) -> Result<()> {
        self.file
            .sync_data()
            .map_err(|e| io_err("sync the stub journal", &self.path, e))
    }
}

/// What a start-up replay did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Replayed {
    /// Journals of dead runs processed and removed.
    pub(crate) journals: usize,
    /// Stubs removed from their roots.
    pub(crate) stubs: usize,
}

/// Replays every journal in `runs_dir` that no live process holds.
///
/// The state directory is not trusted further than its owner: a journal is
/// opened without following a symlink, its root must lie below `confine`
/// (the state directory, where every root of a build lives), and its scratch
/// directory is removed through descriptors, never through a symlink. A
/// journal that fails these checks is left in place with a warning.
pub(crate) fn replay(runs_dir: &Path, confine: &Path) -> Result<Replayed> {
    let mut done = Replayed::default();
    let entries = match fs::read_dir(runs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(done),
        Err(e) => return Err(io_err("list the stub journals", runs_dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err("list the stub journals", runs_dir, e))?;
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        if ext == NEW_EXT {
            // Its creator died before renaming it, or is about to: removed
            // only when nobody holds the lock.
            remove_unlocked(&path);
            continue;
        }
        if ext != JOURNAL_EXT {
            continue;
        }
        match replay_one(&path, confine) {
            Ok(Some(stubs)) => {
                done.journals += 1;
                done.stubs += stubs;
            }
            Ok(None) => {}
            // One bad journal must not stop the build; it stays for later.
            Err(e) => tracing::warn!("rnk-sandbox: cannot replay {}: {e}", path.display()),
        }
    }
    Ok(done)
}

/// Removes a journal that was never published, unless it is locked.
fn remove_unlocked(path: &Path) {
    let Ok(file) = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return;
    };
    if rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive).is_ok() {
        let _ = fs::remove_file(path);
    }
}

/// Replays one journal; `None` when a live run still holds it.
fn replay_one(path: &Path, confine: &Path) -> Result<Option<usize>> {
    let mut file = File::options()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| io_err("open the stub journal", path, e))?;
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK) => return Ok(None),
        Err(e) => return Err(io_err("lock the stub journal", path, e.into())),
    }
    // Its owner removed it between our open and our lock: already clean.
    let st =
        rustix::fs::fstat(&file).map_err(|e| io_err("stat the stub journal", path, e.into()))?;
    if st.st_nlink == 0 {
        return Ok(None);
    }
    let mut text = Vec::new();
    file.read_to_end(&mut text)
        .map_err(|e| io_err("read the stub journal", path, e))?;
    let (root_dir, stubs) = parse(&text);
    let mut removed = 0;
    if let Some(root_dir) = root_dir
        // `None`: the root is gone, and its stubs with it.
        && let Some(root_dir) = confined_root(&root_dir, confine)?
    {
        match RootPath::open(&root_dir) {
            Ok(root) => removed = remove_stubs(&root, &stubs)?,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
            Err(e) => return Err(e.into()),
        }
    }
    remove_scratch(&path.with_extension(""))?;
    fs::remove_file(path).map_err(|e| io_err("remove the stub journal", path, e))?;
    Ok(Some(removed))
}

/// Removes the stubs newest first and restores their parents' times.
fn remove_stubs(root: &RootPath, stubs: &[StubRecord]) -> Result<usize> {
    let mut removed = 0;
    for stub in stubs.iter().rev() {
        let parent = stub.path.parent().unwrap_or(Path::new("/"));
        let parent_mtime = root.statx(parent).ok().map(|st| st.mtime);
        let gone = match stub.kind {
            MountKind::Directory => ok_unless(
                root.remove_dir(&stub.path),
                &[libc::ENOENT, libc::ENOTEMPTY, libc::ENOTDIR, libc::EEXIST],
            )?,
            MountKind::File => match root.statx(&stub.path) {
                Ok(st) if st.file_type() == FileType::Regular && st.size == 0 => {
                    ok_unless(root.remove_file(&stub.path), &[libc::ENOENT])?
                }
                // Gone, or no longer the empty file placed: leave it.
                _ => false,
            },
        };
        if !gone {
            continue;
        }
        removed += 1;
        let restore = match stub.placed_mtime {
            Some(placed) => parent_mtime == Some(placed),
            // Never placed: the build died before the `RUN` started.
            None => true,
        };
        if restore {
            root.set_meta(
                parent,
                &Meta {
                    atime: Some(stub.parent_atime),
                    mtime: Some(stub.parent_mtime),
                    ..Meta::default()
                },
            )?;
        }
    }
    Ok(removed)
}

/// `Ok(true)` on success, `Ok(false)` for an errno in `expected`.
fn ok_unless(r: rnk_rootfs::Result<()>, expected: &[i32]) -> Result<bool> {
    match r {
        Ok(()) => Ok(true),
        Err(e) if e.raw_os_error().is_some_and(|n| expected.contains(&n)) => Ok(false),
        Err(e) => Err(Error::Rootfs(e)),
    }
}

/// The journal's root, resolved, if it still exists and lies strictly below
/// `confine`; an error for a root elsewhere, which a journal planted in the
/// state directory could name to remove host files.
fn confined_root(root_dir: &Path, confine: &Path) -> Result<Option<PathBuf>> {
    let root = match root_dir.canonicalize() {
        Ok(root) => root,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("resolve the journaled root", root_dir, e)),
    };
    let base = confine
        .canonicalize()
        .map_err(|e| io_err("resolve the state directory", confine, e))?;
    if root == base || !root.starts_with(&base) {
        return Err(io_err(
            "replay a journal whose root is outside the state directory",
            root_dir,
            io::Error::from(io::ErrorKind::PermissionDenied),
        ));
    }
    Ok(Some(root))
}

/// Removes the scratch files of the run directory `dir` by name, then the
/// directory, through descriptors: `dir` is opened below its parent without
/// following a symlink, so a planted `runs/<id> -> /etc` removes nothing.
fn remove_scratch(dir: &Path) -> Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(io_err(
            "remove the run directory",
            dir,
            io::Error::from(io::ErrorKind::InvalidInput),
        ));
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let runs = rustix::fs::open(parent, flags, Mode::empty())
        .map_err(|e| io_err("open the runs directory", parent, e.into()))?;
    let run = match rustix::fs::openat(&runs, name, flags, Mode::empty()) {
        Ok(run) => run,
        Err(Errno::NOENT) => return Ok(()),
        Err(e) => return Err(io_err("open the run directory", dir, e.into())),
    };
    for file in SCRATCH_FILES {
        match rustix::fs::unlinkat(&run, file, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(e) => return Err(io_err("remove a run file", dir.join(file), e.into())),
        }
    }
    match rustix::fs::unlinkat(&runs, name, AtFlags::REMOVEDIR) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(io_err("remove the run directory", dir, e.into())),
    }
}

/// The root and the stubs a journal names; lines that do not parse (a torn
/// last write) are skipped.
fn parse(text: &[u8]) -> (Option<PathBuf>, Vec<StubRecord>) {
    let mut root = None;
    let mut stubs: Vec<StubRecord> = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["root", path] => root = unhex(path).map(path_from),
            ["stub", kind, path, atime, mtime] => {
                let kind = match *kind {
                    "d" => MountKind::Directory,
                    "f" => MountKind::File,
                    _ => continue,
                };
                let (Some(path), Some(parent_atime), Some(parent_mtime)) =
                    (unhex(path), parse_time(atime), parse_time(mtime))
                else {
                    continue;
                };
                stubs.push(StubRecord {
                    kind,
                    path: path_from(path),
                    parent_atime,
                    parent_mtime,
                    placed_mtime: None,
                });
            }
            ["placed", path, mtime] => {
                let (Some(path), Some(mtime)) = (unhex(path), parse_time(mtime)) else {
                    continue;
                };
                let path = path_from(path);
                if let Some(stub) = stubs.iter_mut().rev().find(|s| s.path == path) {
                    stub.placed_mtime = Some(mtime);
                }
            }
            _ => {}
        }
    }
    (root, stubs)
}

/// The normal components of a path inside the root, `..` kept, as a queue.
fn components(p: &Path) -> VecDeque<OsString> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

fn path_from(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0xf)]));
    }
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |c: u8| {
                char::from(c)
                    .to_digit(16)
                    .and_then(|d| u8::try_from(d).ok())
            };
            Some((digit(pair[0])? << 4) | digit(pair[1])?)
        })
        .collect()
}

fn time(t: FileTime) -> String {
    format!("{}.{:09}", t.secs, t.nanos)
}

fn parse_time(s: &str) -> Option<FileTime> {
    let (secs, nanos) = s.split_once('.')?;
    let nanos: u32 = nanos.parse().ok()?;
    if nanos >= 1_000_000_000 {
        return None;
    }
    Some(FileTime {
        secs: secs.parse().ok()?,
        nanos,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        root_dir: PathBuf,
        runs: PathBuf,
    }

    impl Fixture {
        /// The state directory the roots lie in.
        fn base(&self) -> &Path {
            self.dir.path()
        }
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root_dir = dir.path().join("root");
        let runs = dir.path().join("runs");
        fs::create_dir_all(root_dir.join("etc")).unwrap();
        fs::create_dir(&runs).unwrap();
        Fixture {
            root_dir: root_dir.canonicalize().unwrap(),
            runs,
            dir,
        }
    }

    /// Host files a planted journal aims at, outside the state directory.
    fn victim() -> tempfile::TempDir {
        let victim = tempfile::tempdir().unwrap();
        for name in SCRATCH_FILES {
            fs::write(victim.path().join(name), b"host").unwrap();
        }
        victim
    }

    fn assert_untouched(victim: &tempfile::TempDir) {
        for name in SCRATCH_FILES {
            assert_eq!(
                fs::read(victim.path().join(name)).unwrap(),
                b"host",
                "{name}"
            );
        }
    }

    #[test]
    fn replay_does_not_follow_a_planted_run_directory_symlink() {
        let f = fixture();
        let victim = victim();
        fs::write(f.runs.join("x.journal"), b"").unwrap();
        std::os::unix::fs::symlink(victim.path(), f.runs.join("x")).unwrap();
        replay(&f.runs, f.base()).unwrap();
        assert_untouched(&victim);
        // Left for a human: the journal could not be replayed.
        assert!(f.runs.join("x.journal").exists());
    }

    #[test]
    fn replay_refuses_a_journaled_root_outside_the_state_directory() {
        let f = fixture();
        let victim = victim();
        let text = format!(
            "root\t{}\nstub\tf\t{}\t1.0\t2.0\n",
            hex(victim.path().as_os_str().as_bytes()),
            hex(b"/hosts"),
        );
        fs::write(f.runs.join("x.journal"), text.as_bytes()).unwrap();
        fs::write(victim.path().join("empty"), b"").unwrap();
        let text = format!(
            "root\t{}\nstub\tf\t{}\t1.0\t2.0\n",
            hex(victim.path().as_os_str().as_bytes()),
            hex(b"/empty"),
        );
        fs::write(f.runs.join("y.journal"), text.as_bytes()).unwrap();
        // Also through a symlink inside the state directory.
        std::os::unix::fs::symlink(victim.path(), f.base().join("link")).unwrap();
        let text = format!(
            "root\t{}\nstub\tf\t{}\t1.0\t2.0\n",
            hex(f.base().join("link").as_os_str().as_bytes()),
            hex(b"/empty"),
        );
        fs::write(f.runs.join("z.journal"), text.as_bytes()).unwrap();
        assert_eq!(replay(&f.runs, f.base()).unwrap(), Replayed::default());
        assert_untouched(&victim);
        assert!(victim.path().join("empty").exists());
        for j in ["x", "y", "z"] {
            assert!(f.runs.join(format!("{j}.journal")).exists(), "{j}");
        }
    }

    #[test]
    fn replay_does_not_follow_a_journal_symlink() {
        let f = fixture();
        let victim = victim();
        let target = victim.path().join("hosts");
        std::os::unix::fs::symlink(&target, f.runs.join("x.journal")).unwrap();
        std::os::unix::fs::symlink(&target, f.runs.join(format!("y.{NEW_EXT}"))).unwrap();
        replay(&f.runs, f.base()).unwrap();
        assert_untouched(&victim);
    }

    fn set_old_times(root: &RootPath, p: &str) -> FileTime {
        let t = FileTime::from_secs(1_000_000);
        root.set_meta(
            p,
            &Meta {
                atime: Some(t),
                mtime: Some(t),
                ..Meta::default()
            },
        )
        .unwrap();
        t
    }

    #[test]
    fn stubs_are_journaled_removed_and_times_restored() {
        let f = fixture();
        let root = RootPath::open(&f.root_dir).unwrap();
        let old = set_old_times(&root, "/etc");
        let root_old = set_old_times(&root, "/");
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        assert_eq!(
            j.ensure(Path::new("/etc/resolv.conf"), MountKind::File)
                .unwrap(),
            1
        );
        assert_eq!(
            j.ensure(Path::new("/etc/hosts"), MountKind::File).unwrap(),
            1
        );
        assert_eq!(
            j.ensure(Path::new("/proc"), MountKind::Directory).unwrap(),
            1
        );
        assert_eq!(
            j.ensure(Path::new("/var/cache/apt"), MountKind::Directory)
                .unwrap(),
            3
        );
        // Already there: nothing to place.
        assert_eq!(
            j.ensure(Path::new("/etc/hosts"), MountKind::File).unwrap(),
            0
        );
        j.commit().unwrap();
        assert_eq!(j.stub_count(), 6);
        assert!(j.scratch().is_dir());
        let meta = fs::metadata(f.root_dir.join("var/cache")).unwrap();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
            0o755
        );

        let text = fs::read(f.runs.join("1.journal")).unwrap();
        let (root_dir, stubs) = parse(&text);
        assert_eq!(root_dir.as_deref(), Some(f.root_dir.as_path()));
        assert_eq!(stubs.len(), 6);
        assert!(stubs.iter().all(|s| s.placed_mtime.is_some()));
        assert_eq!(stubs[0].path, Path::new("/etc/resolv.conf"));
        assert_eq!(stubs[0].parent_mtime, old);

        assert_eq!(j.finish().unwrap(), 6);
        let root = RootPath::open(&f.root_dir).unwrap();
        assert!(root.statx("/etc/hosts").is_err());
        assert!(root.statx("/var").is_err());
        assert_eq!(root.statx("/etc").unwrap().mtime, old);
        assert_eq!(root.statx("/").unwrap().mtime, root_old);
        assert_eq!(fs::read_dir(&f.runs).unwrap().count(), 0);
    }

    #[test]
    fn dangling_symlinks_are_followed_inside_the_root() {
        let f = fixture();
        std::os::unix::fs::symlink(
            "/run/systemd/resolve/stub-resolv.conf",
            f.root_dir.join("etc/resolv.conf"),
        )
        .unwrap();
        // A relative link climbing above the root is clamped to it.
        std::os::unix::fs::symlink("../../../../../hosts.real", f.root_dir.join("etc/hosts"))
            .unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        assert_eq!(
            j.ensure(Path::new("/etc/resolv.conf"), MountKind::File)
                .unwrap(),
            4
        );
        assert_eq!(
            j.ensure(Path::new("/etc/hosts"), MountKind::File).unwrap(),
            1
        );
        assert!(
            f.root_dir
                .join("run/systemd/resolve/stub-resolv.conf")
                .is_file()
        );
        assert!(f.root_dir.join("hosts.real").is_file());
        assert!(j.finish().is_ok());
        assert!(!f.root_dir.join("run").exists());
        assert!(!f.root_dir.join("hosts.real").exists());
        assert!(f.root_dir.join("etc/resolv.conf").is_symlink());
    }

    #[test]
    fn symlink_loops_and_files_in_the_way_are_refused() {
        let f = fixture();
        std::os::unix::fs::symlink("b", f.root_dir.join("a")).unwrap();
        std::os::unix::fs::symlink("a", f.root_dir.join("b")).unwrap();
        fs::write(f.root_dir.join("file"), b"x").unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        assert!(j.ensure(Path::new("/a"), MountKind::File).is_err());
        assert!(j.ensure(Path::new("/file/x"), MountKind::File).is_err());
        assert_eq!(j.stub_count(), 0);
    }

    #[test]
    fn written_stubs_and_runs_files_are_kept() {
        let f = fixture();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        j.ensure(Path::new("/data/x"), MountKind::File).unwrap();
        // The `RUN` wrote into the stub directory: it is content now.
        fs::write(f.root_dir.join("data/new"), b"y").unwrap();
        fs::write(f.root_dir.join("data/x"), b"z").unwrap();
        assert_eq!(j.finish().unwrap(), 0);
        assert!(f.root_dir.join("data/new").is_file());
        assert!(f.root_dir.join("data/x").is_file());
    }

    #[test]
    fn replay_removes_stubs_of_dead_runs_only() {
        let f = fixture();
        let root = RootPath::open(&f.root_dir).unwrap();
        let old = set_old_times(&root, "/etc");
        let mut dead = Journal::create(&f.runs, "dead", &f.root_dir, root).unwrap();
        dead.ensure(Path::new("/etc/hostname"), MountKind::File)
            .unwrap();
        fs::write(dead.scratch().join("hosts"), b"h").unwrap();
        // A crash: the journal is never finished and its lock goes away.
        let Journal { file, .. } = dead;
        drop(file);

        let root = RootPath::open(&f.root_dir).unwrap();
        let mut live = Journal::create(&f.runs, "live", &f.root_dir, root).unwrap();
        live.ensure(Path::new("/srv"), MountKind::Directory)
            .unwrap();

        let done = replay(&f.runs, f.base()).unwrap();
        assert_eq!(
            done,
            Replayed {
                journals: 1,
                stubs: 1
            }
        );
        assert!(!f.root_dir.join("etc/hostname").exists());
        let root = RootPath::open(&f.root_dir).unwrap();
        assert_eq!(root.statx("/etc").unwrap().mtime, old);
        assert!(!f.runs.join("dead.journal").exists());
        assert!(!f.runs.join("dead").exists());
        // The live run's journal and stub are untouched.
        assert!(f.root_dir.join("srv").is_dir());
        assert!(f.runs.join("live.journal").exists());
        assert_eq!(live.finish().unwrap(), 1);
    }

    #[test]
    fn unpublished_journals_are_removed_unless_locked() {
        let f = fixture();
        let dead = f.runs.join(format!("dead.{NEW_EXT}"));
        fs::write(&dead, b"").unwrap();
        let live = f.runs.join(format!("live.{NEW_EXT}"));
        let held = File::create(&live).unwrap();
        rustix::fs::flock(&held, FlockOperation::LockExclusive).unwrap();
        assert_eq!(replay(&f.runs, f.base()).unwrap(), Replayed::default());
        assert!(!dead.exists());
        assert!(live.exists());
        // Published names never collide.
        let root = RootPath::open(&f.root_dir).unwrap();
        let j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        assert!(Journal::create(&f.runs, "1", &f.root_dir, root).is_err());
        assert!(f.runs.join("1.journal").exists());
        assert!(!f.runs.join(format!("1.{NEW_EXT}")).exists());
        j.finish().unwrap();
    }

    #[test]
    fn parse_skips_torn_lines_and_unknown_records() {
        let text = format!(
            "root\t{}\nstub\tf\t{}\t1.000000002\t3.000000004\nplaced\t{}\t5.000000006\nstub\td\t2f78\t1.0\nbogus\n",
            hex(b"/r"),
            hex(b"/etc/hosts"),
            hex(b"/etc/hosts"),
        );
        let (root, stubs) = parse(text.as_bytes());
        assert_eq!(root.as_deref(), Some(Path::new("/r")));
        assert_eq!(
            stubs,
            [StubRecord {
                kind: MountKind::File,
                path: "/etc/hosts".into(),
                parent_atime: FileTime { secs: 1, nanos: 2 },
                parent_mtime: FileTime { secs: 3, nanos: 4 },
                placed_mtime: Some(FileTime { secs: 5, nanos: 6 }),
            }]
        );
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("2f"), Some(b"/".to_vec()));
        assert_eq!(parse_time("-1.5"), Some(FileTime { secs: -1, nanos: 5 }));
        assert_eq!(parse_time("1.1000000000"), None);
    }
}
