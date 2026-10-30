//! The exporter: one instruction's upper directory, read against its lower
//! stack, fed to `rnk-layer`'s writer as an OCI layer
//! (`design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer`, as
//! BuildKit's `WriteUpperdir`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use rnk_layer::{
    EncodeOptions, Entry, EntryKind, EntryPath, EntrySource, LayerDigests, LayerWriter,
};
use rnk_rootfs::{FileType, OpenHow, RootPath, Stat};

use crate::error::{Error, Result};
use crate::lowers::{self, Found, Lowers, OPAQUE, REDIRECT};

/// One instruction's diff: the upper directory `diff-<k>` over the lower
/// stack it was mounted on. Holds paths only; the directories stay on disk
/// until the stage is released, so a layer can be exported later, or
/// never (lazy export).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerDiff {
    pub(crate) upper: PathBuf,
    /// Topmost first.
    pub(crate) lowers: Vec<PathBuf>,
}

/// What [`LayerDiff::export`] found, besides the layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffCounts {
    /// Entries fed to the writer other than whiteouts: files, directories,
    /// links, nodes.
    pub files_added: u64,
    /// Whiteouts written (`.wh.<name>`).
    pub whiteouts: u64,
    /// Opaque directories written (`.wh..wh..opq`): always 0 since an
    /// opaque directory is written as whiteouts of what it hides, as
    /// BuildKit (ADR-0034).
    pub opaque_dirs: u64,
    /// Bytes of regular file content.
    pub bytes: u64,
    /// Entries the lower stack also shows and found unchanged, so left out
    /// (BuildKit's `sameDirent`, ADR-0034).
    pub unchanged: u64,
    /// Bytes read from both sides to compare contents.
    pub bytes_compared: u64,
}

impl LayerDiff {
    /// The diff of the upper directory `upper` over `lowers` (topmost
    /// first), in the overlay form with `userxattr`, as an unmounted
    /// overlay leaves them. [`StageRootfs::end_layer`] makes these; this is
    /// for diffs kept elsewhere.
    ///
    /// [`StageRootfs::end_layer`]: crate::StageRootfs::end_layer
    #[must_use]
    pub fn new(upper: PathBuf, lowers: Vec<PathBuf>) -> Self {
        Self { upper, lowers }
    }

    /// The upper directory, for messages.
    #[must_use]
    pub fn upper(&self) -> &Path {
        &self.upper
    }

    /// Whether the upper directory holds no entry at all: the instruction
    /// changed nothing on disk.
    ///
    /// # Errors
    ///
    /// [`Error::Rootfs`] when the upper directory cannot be read.
    pub fn is_empty(&self) -> Result<bool> {
        let upper = RootPath::open(&self.upper)?;
        Ok(list(&upper, Path::new(""))?.is_empty())
    }

    /// Writes the diff as a layer to `out` and returns its digests and
    /// counts. The bytes depend only on the diff and on `options` other
    /// than its thread count.
    ///
    /// Whiteouts are converted before hardlinks are found, so overlay's
    /// whiteouts, which share one inode, never become hardlinks. A whiteout
    /// is written only where the lower stack shows the name. An entry
    /// copied up unchanged is not written (BuildKit's `sameDirent`,
    /// ADR-0034); a directory left out this way still comes back from the
    /// writer as the parent of a changed entry. An opaque directory over a
    /// lower one is written as whiteouts of the names it hides, never as
    /// `.wh..wh..opq`, as BuildKit's differ and containerd's writer do.
    /// Of the xattrs, only `security.capability` is kept, as BuildKit
    /// (ADR-0032, Q-025); overlay's own never reach a layer.
    ///
    /// # Errors
    ///
    /// [`Error::Export`] for a redirect (never made with
    /// `redirect_dir=nofollow`) or an entry of unknown type;
    /// [`Error::Layer`] when encoding fails; [`Error::Rootfs`] when a
    /// directory cannot be read.
    pub fn export<W: Write>(
        &self,
        options: EncodeOptions,
        out: W,
    ) -> Result<(LayerDigests, DiffCounts)> {
        let span = tracing::info_span!(
            "rnk_snapshot.export",
            entries = tracing::field::Empty,
            bytes_read = tracing::field::Empty,
        );
        let _entered = span.enter();
        let upper = RootPath::open(&self.upper)?;
        let lower_roots = self
            .lowers
            .iter()
            .map(RootPath::open)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let lowers = Lowers {
            layers: &lower_roots,
        };
        let (entries, counts) = {
            let diff = tracing::info_span!(
                "rnk_snapshot.diff",
                files_added = tracing::field::Empty,
                whiteouts = tracing::field::Empty,
                opaque_dirs = tracing::field::Empty,
                bytes = tracing::field::Empty,
                unchanged = tracing::field::Empty,
                bytes_compared = tracing::field::Empty,
            );
            let _entered = diff.enter();
            let mut walk = Walk::new(&upper, &lowers);
            walk.run()?;
            diff.record("files_added", walk.counts.files_added);
            diff.record("whiteouts", walk.counts.whiteouts);
            diff.record("opaque_dirs", walk.counts.opaque_dirs);
            diff.record("bytes", walk.counts.bytes);
            diff.record("unchanged", walk.counts.unchanged);
            diff.record("bytes_compared", walk.counts.bytes_compared);
            (walk.entries, walk.counts)
        };
        let mut writer = LayerWriter::new(options)?;
        for e in entries {
            writer.add(e)?;
        }
        span.record("entries", writer.len());
        let mut source = Source {
            upper: &upper,
            lowers: &lowers,
            bytes_read: 0,
        };
        let digests = writer.finish(&mut source, out)?;
        span.record("bytes_read", source.bytes_read);
        Ok((digests, counts))
    }

    /// The entries [`LayerDiff::export`] feeds the writer, sorted by path;
    /// for tests and for callers that only want to look.
    ///
    /// # Errors
    ///
    /// As [`LayerDiff::export`].
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let upper = RootPath::open(&self.upper)?;
        let lower_roots = self
            .lowers
            .iter()
            .map(RootPath::open)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let lowers = Lowers {
            layers: &lower_roots,
        };
        let mut walk = Walk::new(&upper, &lowers);
        walk.run()?;
        let mut entries = walk.entries;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }
}

/// The names in the directory `dir` of `root`, without `.` and `..`,
/// sorted. Opened without following a final symlink.
pub(crate) fn list(root: &RootPath, dir: &Path) -> Result<Vec<OsString>> {
    let at = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let fd = root.open_at(at, &OpenHow::read().directory().no_follow())?;
    let mut names = Vec::new();
    let io_err = |e: rustix::io::Errno| Error::Io {
        op: "read directory",
        path: dir.to_owned(),
        source: e.into(),
    };
    for item in rustix::fs::Dir::read_from(&fd).map_err(io_err)? {
        let item = item.map_err(io_err)?;
        let name = item.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    }
    names.sort();
    Ok(names)
}

struct Walk<'a> {
    upper: &'a RootPath,
    lowers: &'a Lowers<'a>,
    entries: Vec<Entry>,
    counts: DiffCounts,
    /// Unchanged names under an opaque directory whose inode has other
    /// links (continuity's `ChangeKindUnmodified`): written after the walk,
    /// and only as links to a changed name of the same inode.
    linked: Vec<Entry>,
}

/// The longest path below a layer root the exporter walks to (`PATH_MAX`
/// less the NUL): every entry is resolved again from the root, so a deeper
/// tree, which a `RUN` can make, is refused with this error rather than an
/// `ENAMETOOLONG` from the kernel.
const PATH_MAX: usize = 4095;

/// The buffer size of the content comparison, as BuildKit's `bufPool`.
const COMPARE_BUF: usize = 32 * 1024;

impl<'a> Walk<'a> {
    fn new(upper: &'a RootPath, lowers: &'a Lowers<'a>) -> Self {
        Self {
            upper,
            lowers,
            entries: Vec::new(),
            counts: DiffCounts::default(),
            linked: Vec::new(),
        }
    }

    /// Walks the whole upper directory with an explicit stack of the
    /// directories still to list, so a deep tree costs memory, never stack.
    /// Each directory carries whether the lower stack shows it as a
    /// directory: below one it does not, the lower stack shows nothing
    /// either (overlay's lookup stops there), so new trees cost no lookup.
    fn run(&mut self) -> Result<()> {
        let mut todo = vec![(PathBuf::new(), !self.lowers.layers.is_empty())];
        while let Some((dir, below)) = todo.pop() {
            for name in list(self.upper, &dir)? {
                let p = child(&dir, &name)?;
                let st = self.upper.statx(&p)?;
                if let Some(below) = self.entry(&p, &st, below)? {
                    todo.push((p, below));
                }
            }
        }
        self.link_unchanged();
        Ok(())
    }

    /// Classifies one entry and feeds it, or not; `below` is whether the
    /// lower stack shows its parent as a directory. Returns, for a
    /// directory the walk must go into (fed or an unchanged copy-up),
    /// whether the lower stack shows it as a directory too.
    /// As BuildKit's overlay differ (`util/overlay/overlay_linux.go`
    /// `Changes`): an entry the lower stack also shows is left out when
    /// `sameDirent` finds it unchanged, and an opaque directory over a
    /// lower entry is walked against it instead of written as opaque.
    fn entry(&mut self, p: &Path, st: &Stat, below: bool) -> Result<Option<bool>> {
        let lower = if below { self.lowers.lookup(p)? } else { None };
        if lowers::is_whiteout(st) {
            if lower.is_some() {
                self.whiteout(p)?;
            }
            return Ok(None);
        }
        let all = self.upper.xattrs(p)?;
        if all
            .iter()
            .any(|(n, _)| n == REDIRECT || n == "trusted.overlay.redirect")
        {
            return Err(Error::Export {
                path: p.to_owned(),
                reason: "an overlay redirect, which redirect_dir=nofollow never creates",
            });
        }
        let opaque = all.iter().any(|(n, v)| n == OPAQUE && v.as_slice() == b"y");
        let xattrs = lowers::layer_xattrs(all);
        let same = match &lower {
            Some(l) => self.same_dirent(st, &xattrs, l, p)?,
            None => false,
        };
        let is_dir = st.file_type() == FileType::Directory;
        let lower_dir = lower
            .as_ref()
            .is_some_and(|l| l.stat.file_type() == FileType::Directory);
        if opaque && is_dir && lower.is_some() {
            if same {
                self.counts.unchanged += 1;
            } else {
                self.push(p, st, xattrs)?;
            }
            self.opaque_dir(p)?;
            return Ok(None);
        }
        if same {
            self.counts.unchanged += 1;
        } else if !self.push(p, st, xattrs)? {
            return Ok(None);
        }
        Ok(is_dir.then_some(lower_dir))
    }

    /// The children of the opaque directory `dir` against what the lower
    /// stack shows there, as continuity's double-walking differ that
    /// BuildKit runs on it: a name only below is a whiteout (one for a
    /// whole directory), a name only in the upper is added with all it
    /// holds, a name in both is compared with `sameDirent` and, when both
    /// are directories, walked the same way. Whiteouts in the upper are
    /// not names here, as in BuildKit's whiteout-free view of the upper.
    fn opaque_dir(&mut self, dir: &Path) -> Result<()> {
        let mut todo = vec![dir.to_owned()];
        while let Some(d) = todo.pop() {
            let mut upper = BTreeMap::new();
            for name in list(self.upper, &d)? {
                let p = child(&d, &name)?;
                let st = self.upper.statx(&p)?;
                if !lowers::is_whiteout(&st) {
                    upper.insert(name, st);
                }
            }
            let below = self.lowers.list(&d)?;
            for name in below.iter().filter(|n| !upper.contains_key(*n)) {
                self.whiteout(&child(&d, name)?)?;
            }
            for (name, st) in upper {
                let p = child(&d, &name)?;
                let lower = if below.contains(&name) {
                    self.lowers.lookup(&p)?
                } else {
                    None
                };
                let Some(lower) = lower else {
                    self.add_tree(&p, &st)?;
                    continue;
                };
                let xattrs = lowers::layer_xattrs(self.upper.xattrs(&p)?);
                let lower_dir = lower.stat.file_type() == FileType::Directory;
                let is_dir = st.file_type() == FileType::Directory;
                if self.same_dirent(&st, &xattrs, &lower, &p)? {
                    self.counts.unchanged += 1;
                    if is_dir {
                        todo.push(p);
                    } else if st.nlink > 1
                        && let Some(e) = self.make_entry(&p, &st, xattrs)?
                    {
                        self.linked.push(e);
                    }
                } else if is_dir && lower_dir {
                    self.push(&p, &st, xattrs)?;
                    todo.push(p);
                } else if is_dir {
                    self.add_tree(&p, &st)?;
                } else {
                    // A directory below replaced by a non-directory goes
                    // with all it held: no whiteout for its children.
                    self.push(&p, &st, xattrs)?;
                }
            }
        }
        Ok(())
    }

    /// Feeds `p` and, for a directory, everything below it in the upper,
    /// whiteouts left out: names the lower stack does not show there.
    fn add_tree(&mut self, p: &Path, st: &Stat) -> Result<()> {
        let xattrs = lowers::layer_xattrs(self.upper.xattrs(p)?);
        if !self.push(p, st, xattrs)? {
            return Ok(());
        }
        let mut todo = vec![p.to_owned()];
        while let Some(d) = todo.pop() {
            for name in list(self.upper, &d)? {
                let q = child(&d, &name)?;
                let st = self.upper.statx(&q)?;
                if lowers::is_whiteout(&st) {
                    continue;
                }
                let xattrs = lowers::layer_xattrs(self.upper.xattrs(&q)?);
                if self.push(&q, &st, xattrs)? {
                    todo.push(q);
                }
            }
        }
        Ok(())
    }

    /// Writes the unchanged hardlinked names whose inode a changed name
    /// brought into the layer, so the writer makes them links to it, as
    /// containerd's `ChangeWriter` does with `inodeRefs`.
    fn link_unchanged(&mut self) {
        let written: BTreeSet<_> = self.entries.iter().filter_map(|e| e.inode).collect();
        for e in std::mem::take(&mut self.linked) {
            if e.inode.is_some_and(|i| written.contains(&i)) {
                self.counts.files_added += 1;
                self.entries.push(e);
            }
        }
    }

    /// A whiteout for `p`. Its time is zero, as containerd 2's
    /// `ChangeWriter` writes whiteouts whatever the build's epoch.
    fn whiteout(&mut self, p: &Path) -> Result<()> {
        self.counts.whiteouts += 1;
        self.entries.push(Entry {
            path: entry_path(p)?,
            kind: EntryKind::Whiteout,
            mode: 0,
            uid: 0,
            gid: 0,
            mtime: 0,
            xattrs: BTreeMap::new(),
            inode: None,
        });
        Ok(())
    }

    /// Feeds `p` and returns whether it is a directory to go into.
    fn push(&mut self, p: &Path, st: &Stat, xattrs: Vec<(OsString, Vec<u8>)>) -> Result<bool> {
        let Some(e) = self.make_entry(p, st, xattrs)? else {
            return Ok(false);
        };
        if let EntryKind::File { size } = e.kind {
            self.counts.bytes += size;
        }
        let is_dir = e.kind.is_dir();
        self.counts.files_added += 1;
        self.entries.push(e);
        Ok(is_dir)
    }

    /// The entry for `p`; `None` for a socket, which is no layer content
    /// (as kaniko and BuildKit).
    fn make_entry(
        &self,
        p: &Path,
        st: &Stat,
        xattrs: Vec<(OsString, Vec<u8>)>,
    ) -> Result<Option<Entry>> {
        let kind = match st.file_type() {
            FileType::Directory => EntryKind::Dir,
            FileType::Regular => EntryKind::File { size: st.size },
            FileType::Symlink => {
                EntryKind::Symlink(self.upper.read_link(p)?.into_os_string().into_vec())
            }
            FileType::CharDevice => EntryKind::Char(st.rdev_major, st.rdev_minor),
            FileType::BlockDevice => EntryKind::Block(st.rdev_major, st.rdev_minor),
            FileType::Fifo => EntryKind::Fifo,
            FileType::Socket => return Ok(None),
            FileType::Unknown => {
                return Err(Error::Export {
                    path: p.to_owned(),
                    reason: "an entry of unknown type",
                });
            }
        };
        let is_dir = kind.is_dir();
        Ok(Some(Entry {
            path: entry_path(p)?,
            inode: (!is_dir && st.nlink > 1).then_some((dev(st), st.ino)),
            kind,
            mode: st.mode,
            uid: u64::from(st.uid),
            gid: u64::from(st.gid),
            mtime: st.mtime.secs,
            xattrs: xattrs.into_iter().map(|(n, v)| (n.into_vec(), v)).collect(),
        }))
    }

    /// BuildKit's `sameDirent` (`buildkit@cd8ee244
    /// util/overlay/overlay_linux.go:348-387`, from continuity): the upper
    /// entry `st`, whose layer xattrs are `xattrs`, against what the lower
    /// stack shows at `p`. The same inode; else the same type, permission
    /// bits, owner, device numbers and `security.capability`, and for a
    /// non-directory the same size and modification second. When both
    /// times have zero nanoseconds, as a file from a tar does, the
    /// nanoseconds prove nothing, so a symlink's target or a file's content
    /// decides (an empty file is equal); otherwise the nanoseconds must
    /// match and the content is not read.
    fn same_dirent(
        &mut self,
        st: &Stat,
        xattrs: &[(OsString, Vec<u8>)],
        lower: &Found<'_>,
        p: &Path,
    ) -> Result<bool> {
        let l = &lower.stat;
        if (st.dev_major, st.dev_minor, st.ino) == (l.dev_major, l.dev_minor, l.ino) {
            return Ok(true);
        }
        let lower_xattrs = lowers::layer_xattrs(lower.layer.xattrs(p)?);
        if !meta_equal(st, l, xattrs, &lower_xattrs) {
            return Ok(false);
        }
        if st.file_type() == FileType::Directory {
            return Ok(true);
        }
        if st.size != l.size || st.mtime.secs != l.mtime.secs {
            return Ok(false);
        }
        if st.mtime.nanos != 0 || l.mtime.nanos != 0 {
            return Ok(st.mtime.nanos == l.mtime.nanos);
        }
        if st.file_type() == FileType::Symlink {
            return Ok(self.upper.read_link(p)? == lower.layer.read_link(p)?);
        }
        if st.size == 0 {
            return Ok(true);
        }
        self.same_content(lower.layer, p)
    }

    /// Whether the regular file `p` has the same bytes in the upper and in
    /// `lower`, read through both roots in 32 KiB blocks and stopped at the
    /// first difference (continuity's `compareFileContent`). Both were
    /// checked to have the same size, so this reads at most that much from
    /// each, unless a file grows meanwhile.
    fn same_content(&mut self, lower: &RootPath, p: &Path) -> Result<bool> {
        let io_err = |source: io::Error| Error::Io {
            op: "compare file contents",
            path: p.to_owned(),
            source,
        };
        let mut a = File::from(self.upper.open_regular(p)?);
        let mut b = File::from(lower.open_regular(p)?);
        let mut buf_a = vec![0; COMPARE_BUF];
        let mut buf_b = vec![0; COMPARE_BUF];
        loop {
            let n = read_full(&mut a, &mut buf_a).map_err(io_err)?;
            let m = read_full(&mut b, &mut buf_b).map_err(io_err)?;
            self.counts.bytes_compared += (n + m) as u64;
            if n != m || buf_a[..n] != buf_b[..m] {
                return Ok(false);
            }
            if n < COMPARE_BUF {
                return Ok(true);
            }
        }
    }
}

/// `dir/name`, refused past [`PATH_MAX`].
fn child(dir: &Path, name: &OsStr) -> Result<PathBuf> {
    let p = dir.join(name);
    if p.as_os_str().len() > PATH_MAX {
        return Err(Error::Export {
            path: dir.to_owned(),
            reason: "an entry's path is longer than PATH_MAX",
        });
    }
    Ok(p)
}

/// Fills `buf` from `r` until it is full or at the end; the bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// continuity's `compareSysStat` and `compareCapabilities`: the raw
/// `st_mode` (type and permission bits), owner, device numbers, and the
/// layer xattrs, which hold only `security.capability`.
fn meta_equal(
    a: &Stat,
    b: &Stat,
    a_xattrs: &[(OsString, Vec<u8>)],
    b_xattrs: &[(OsString, Vec<u8>)],
) -> bool {
    (a.mode, a.uid, a.gid, a.rdev_major, a.rdev_minor)
        == (b.mode, b.uid, b.gid, b.rdev_major, b.rdev_minor)
        && a_xattrs == b_xattrs
}

fn dev(st: &Stat) -> u64 {
    (u64::from(st.dev_major) << 32) | u64::from(st.dev_minor)
}

fn entry_path(p: &Path) -> Result<EntryPath> {
    Ok(EntryPath::new(p.as_os_str().as_bytes())?)
}

/// File contents and parent directories for the writer: the upper
/// directory first, which holds every parent of a changed entry (overlay
/// copies parents up), then the lower stack.
struct Source<'a> {
    upper: &'a RootPath,
    lowers: &'a Lowers<'a>,
    bytes_read: u64,
}

impl EntrySource for Source<'_> {
    fn open(&mut self, entry: &Entry) -> io::Result<Box<dyn Read + '_>> {
        let p = Path::new(OsStr::from_bytes(entry.path.as_bytes()));
        let fd = self.upper.open_regular(p).map_err(io::Error::other)?;
        if let EntryKind::File { size } = entry.kind {
            self.bytes_read += size;
        }
        Ok(Box::new(File::from(fd)))
    }

    fn parent(&mut self, path: &EntryPath) -> io::Result<Option<Entry>> {
        let p = Path::new(OsStr::from_bytes(path.as_bytes()));
        let (st, xattrs) = match self.upper.statx(p) {
            Ok(st) => (st, self.upper.xattrs(p).map_err(io::Error::other)?),
            // Only a parent the upper does not have comes from the lowers;
            // any other error is an error.
            Err(e) if lowers::is_missing(&e) => {
                match self.lowers.lookup(p).map_err(io::Error::other)? {
                    Some(f) => (f.stat, f.layer.xattrs(p).map_err(io::Error::other)?),
                    None => return Ok(None),
                }
            }
            Err(e) => return Err(io::Error::other(e)),
        };
        if st.file_type() != FileType::Directory {
            return Ok(None);
        }
        Ok(Some(Entry {
            path: path.clone(),
            kind: EntryKind::Dir,
            mode: st.mode,
            uid: u64::from(st.uid),
            gid: u64::from(st.gid),
            mtime: st.mtime.secs,
            xattrs: lowers::layer_xattrs(xattrs)
                .into_iter()
                .map(|(n, v)| (n.into_vec(), v))
                .collect(),
            inode: None,
        }))
    }
}

#[cfg(test)]
mod tests;
