//! T2: deletion and pruning on an overlay's merged directory, where the
//! deletions below an ancestor copy it up before the walk climbs back to it.

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::{Detach, set_times};
use crate::delete::OPEN_DIRS;
use crate::test_util::snapshot;
use crate::{FileType, IgnoreList, RootPath, delete_contents, prune};

/// Levels of the overlay chains: past the descriptors a deletion walk
/// keeps, so ancestors are closed, copied up by the deletions below them,
/// then reopened through `..`.
const DEPTH: usize = 2 * OPEN_DIRS;
/// The seconds of the times every directory of the pruned chain is given.
const TIMES: i64 = 1_000_000;

/// Where an overlay's layers sit, which decides whether a merged
/// directory keeps its inode number across copy-up (the kernel's
/// overlayfs.rst, "Inode properties").
#[derive(Clone, Copy, Debug)]
enum Layers {
    /// Lower, upper and work on the temp dir's own filesystem.
    SameFs,
    /// Upper and work on a tmpfs of their own, lower on the temp dir's
    /// filesystem, with `xino` off or on.
    TmpfsUpper { xino: bool },
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Delete,
    Prune,
}

type Outcome = Result<(), String>;

/// Writes `measured: <line>` to stderr past libtest's capture, for the T2
/// log.
fn measured(line: &str) {
    use std::io::Write as _;
    // The line is for the log only; the assertions decide.
    let _ = writeln!(std::io::stderr().lock(), "measured: {line}");
}

/// The super options of the last mount at `at` in this thread's mount
/// namespace, without the layer paths.
fn super_options(at: &Path) -> String {
    // `/proc/self` would show the main thread's namespace, not this one's.
    let info = fs::read_to_string("/proc/thread-self/mountinfo").unwrap();
    let at = at.to_str().unwrap();
    let line = info
        .lines()
        .rfind(|l| l.split(' ').nth(4) == Some(at))
        .unwrap_or_default();
    let opts = line
        .split(" - ")
        .nth(1)
        .and_then(|l| l.split(' ').nth(2))
        .unwrap_or("?");
    opts.split(',')
        .filter(|o| {
            !["lowerdir=", "upperdir=", "workdir="]
                .iter()
                .any(|k| o.starts_with(k))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The type of the filesystem holding `p`, by name where known.
fn fs_name(p: &Path) -> String {
    let magic = format!("{:#x}", rustix::fs::statfs(p).unwrap().f_type);
    match magic.as_str() {
        "0xef53" => "ext4".to_owned(),
        "0x1021994" => "tmpfs".to_owned(),
        "0x58465342" => "xfs".to_owned(),
        "0x9123683e" => "btrfs".to_owned(),
        _ => magic,
    }
}

/// Makes [`DEPTH`] levels of directories `d` below `top`, each level but
/// the last holding the files `files`; returns the last level.
fn chain(top: &Path, files: &[&str]) -> PathBuf {
    let mut cur = top.to_owned();
    for _ in 0..DEPTH {
        for f in files {
            fs::write(cur.join(f), f.as_bytes()).unwrap();
        }
        cur.push("d");
        fs::create_dir(&cur).unwrap();
    }
    cur
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn mtime(p: &Path) -> (i64, i64) {
    let md = fs::symlink_metadata(p).unwrap();
    (md.mtime(), md.mtime_nsec())
}

fn ensure(ok: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what()) }
}

/// Builds a tree for `op` in the lower layer, mounts an overlay over it
/// with its upper layer where `layers` says, and runs `op` on the merged
/// directory. Returns how the layers were mounted, and what went wrong if
/// anything did.
fn on_overlay(layers: Layers, op: Op) -> (String, Outcome) {
    use rustix::mount::{MountFlags, mount};
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let lower = base.join("lower");
    fs::create_dir(&lower).unwrap();
    match op {
        Op::Delete => {
            let bottom = chain(&lower, &["a", "b", "z"]);
            fs::write(bottom.join("leaf"), b"leaf").unwrap();
        }
        Op::Prune => {
            let bottom = chain(&lower, &["keep", "rm"]);
            fs::write(bottom.join("leaf"), b"leaf").unwrap();
            fs::create_dir(bottom.join("gone")).unwrap();
            chain(&bottom.join("gone"), &["f"]);
            let mut cur = lower.join("d");
            for _ in 0..DEPTH {
                set_times(&cur, TIMES);
                cur.push("d");
            }
        }
    }
    let lower_before = snapshot(&lower);
    // Declared before the root, so that the root's descriptor goes first.
    let mut mounts = Detach(Vec::new());
    let upper_fs = match layers {
        Layers::SameFs => base.clone(),
        Layers::TmpfsUpper { .. } => {
            let t = base.join("tmpfs");
            fs::create_dir(&t).unwrap();
            mount("tmpfs", &t, "tmpfs", MountFlags::empty(), None).expect("tmpfs");
            mounts.0.push(t.clone());
            t
        }
    };
    let (upper, work, merged) = (
        upper_fs.join("upper"),
        upper_fs.join("work"),
        base.join("merged"),
    );
    for d in [&upper, &work, &merged] {
        fs::create_dir(d).unwrap();
    }
    let xino = match layers {
        Layers::SameFs => "",
        Layers::TmpfsUpper { xino: false } => ",xino=off",
        Layers::TmpfsUpper { xino: true } => ",xino=on",
    };
    let data = std::ffi::CString::new(format!(
        "lowerdir={},upperdir={},workdir={}{xino}",
        lower.display(),
        upper.display(),
        work.display()
    ))
    .unwrap();
    if let Err(e) = mount(
        "overlay",
        &merged,
        "overlay",
        MountFlags::empty(),
        data.as_c_str(),
    ) {
        return (format!("overlay{xino}"), Err(format!("mount: {e}")));
    }
    mounts.0.push(merged.clone());
    let setup = format!(
        "lower {}, upper {}, overlay {}",
        fs_name(&lower),
        fs_name(&upper),
        super_options(&merged)
    );
    let root = RootPath::open(&merged).unwrap();
    let r = match op {
        Op::Delete => deleted(&root, &merged),
        Op::Prune => pruned(&root, &merged),
    }
    .and_then(|()| {
        ensure(snapshot(&lower) == lower_before, || {
            "the lower layer changed".to_owned()
        })
    });
    drop(root);
    drop(mounts);
    (setup, r)
}

/// `delete_contents` on the merged directory leaves nothing in it.
fn deleted(root: &RootPath, merged: &Path) -> Outcome {
    delete_contents(root, &IgnoreList::new()).map_err(|e| format!("delete_contents: {e:?}"))?;
    let left = names(merged);
    ensure(left.is_empty(), || format!("left: {left:?}"))
}

/// `prune` on the merged directory takes `rm` from every level and the
/// chain `gone` at the bottom, and gives every level its times back.
fn pruned(root: &RootPath, merged: &Path) -> Outcome {
    let root_mtime = mtime(merged);
    let remove = |p: &Path, t: FileType| {
        let name = p.file_name().unwrap_or_default();
        (t == FileType::Regular && name == "rm") || (t == FileType::Directory && name == "gone")
    };
    let pruned = prune(root, &remove).map_err(|e| format!("prune: {e:?}"))?;
    ensure(!pruned.linked_outside, || "a link outside".to_owned())?;
    ensure(mtime(merged) == root_mtime, || {
        "the root's times".to_owned()
    })?;
    let mut cur = merged.to_owned();
    for level in 0..=DEPTH {
        let want: &[&str] = if level == DEPTH {
            &["leaf"]
        } else {
            &["d", "keep"]
        };
        let names = names(&cur);
        ensure(names == want, || format!("level {level}: {names:?}"))?;
        ensure(level == 0 || mtime(&cur) == (TIMES, 7), || {
            format!("level {level}: times {:?}", mtime(&cur))
        })?;
        cur.push("d");
    }
    Ok(())
}

/// On an overlay's merged directory, a deletion walk reopens each ancestor
/// through `..` after the deletions below it copied it up, and must find
/// the directory it left, whether the layers share a filesystem or not and
/// with `xino` off or on: everything goes, `prune` keeps what it should
/// with its times, and the lower layer is untouched. Each configuration
/// and its result go to the log as a `measured:` line, with the kernel;
/// the test fails once all ran if one did.
#[test]
#[ignore = "T2: mounts overlays and a tmpfs in a private mount namespace as real root; run in the test VM only"]
fn deep_trees_on_an_overlay_are_deleted_and_pruned_whatever_its_layers() {
    std::thread::spawn(|| {
        use rustix::mount::{MountPropagationFlags, mount_change};
        use rustix::thread::UnshareFlags;
        // SAFETY: this thread shares nothing that the unshared filesystem
        // attributes or mount namespace would break; it ends with the test.
        unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS | UnshareFlags::NEWNS) }
            .expect("unshare(CLONE_FS | CLONE_NEWNS)");
        mount_change(
            "/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )
        .expect("make every mount private");
        let kernel = rustix::system::uname()
            .release()
            .to_string_lossy()
            .into_owned();
        let mut failed = Vec::new();
        for layers in [
            Layers::SameFs,
            Layers::TmpfsUpper { xino: false },
            Layers::TmpfsUpper { xino: true },
        ] {
            for op in [Op::Delete, Op::Prune] {
                let (setup, r) = on_overlay(layers, op);
                let result = match &r {
                    Ok(()) => "ok".to_owned(),
                    Err(e) => format!("FAILED: {e}"),
                };
                measured(&format!(
                    "deep tree on an overlay: {layers:?}\t{op:?}\tkernel {kernel}\t{setup}\t{result}"
                ));
                if r.is_err() {
                    failed.push((layers, op, r));
                }
            }
        }
        assert!(failed.is_empty(), "{failed:?}");
    })
    .join()
    .unwrap();
}
