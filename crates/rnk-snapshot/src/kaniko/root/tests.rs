use std::fs;
use std::io::Read as _;
use std::num::NonZeroUsize;

use rnk_layer::Compression;
use sha2::{Digest as _, Sha256};
use tar::{EntryType, Header};

use super::*;
use crate::kaniko::SnapshotMode;

struct Fixture {
    tmp: tempfile::TempDir,
    host: PathBuf,
    kaniko: Kaniko,
}

impl Fixture {
    /// A root with `state/` ignored, as the state directory inside it.
    fn new() -> Self {
        Self::at(None)
    }

    /// As [`Fixture::new`], the snapshotter told its root is `path` (only
    /// `/` changes anything); the root itself is always a temp directory.
    fn at(path: Option<&str>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let host = fs::canonicalize(tmp.path()).unwrap().join("root");
        fs::create_dir_all(host.join("state")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(host.join("state/blob"), b"kept").unwrap();
        let mut ignore = IgnoreList::new();
        ignore.insert("/state").unwrap();
        ignore.insert("/ranikust").unwrap();
        let root = RootPath::open(&host).unwrap();
        let walk = WalkOptions {
            workers: NonZeroUsize::new(2).unwrap(),
            mode: SnapshotMode::Full,
        };
        let shown = path.map_or_else(|| host.clone(), PathBuf::from);
        // The keep area sits beside the root, as `builds/<id>/kept` does.
        let kept_dir = host.with_file_name("kept");
        fs::create_dir(&kept_dir).unwrap();
        let kept = RootPath::open(&kept_dir).unwrap();
        let kaniko = Kaniko::new(root, shown, ignore, walk, kept);
        Self { tmp, host, kaniko }
    }

    /// As [`Fixture::at`] with `/`, keeping stages as tars in an anonymous
    /// store on the state directory, which is also the keep area, as on
    /// `/` (ADR-0056).
    fn tars() -> (Self, Arc<Store>) {
        let f = Self::at(Some("/"));
        let state = f.host.join("state");
        let store = Arc::new(Store::open_anonymous(&state).unwrap());
        let kept = RootPath::open(&state).unwrap();
        let mut ignore = IgnoreList::new();
        ignore.insert("/state").unwrap();
        ignore.insert("/ranikust").unwrap();
        let walk = WalkOptions {
            workers: NonZeroUsize::new(2).unwrap(),
            mode: SnapshotMode::Full,
        };
        let kaniko = Kaniko::new(
            RootPath::open(&f.host).unwrap(),
            PathBuf::from("/"),
            ignore,
            walk,
            kept,
        );
        kaniko.keep_in_anonymous_files(Arc::clone(&store)).unwrap();
        (Self { kaniko, ..f }, store)
    }

    /// The names in the state directory, as a `RUN` would list them, but
    /// the marker of a build in progress, which holds nothing.
    fn state_names(&self) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(self.host.join("state"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != ".ranikust-building")
            .collect();
        v.sort();
        v
    }

    fn kept_entries(&self) -> usize {
        fs::read_dir(self.host.with_file_name("kept"))
            .unwrap()
            .count()
    }

    /// A base layer of `files` (path, content), as a blob file.
    fn layer(&self, name: &str, files: &[(&str, &[u8])]) -> KanikoLayer {
        let mut out = Vec::new();
        for (path, data) in files {
            let mut h = Header::new_gnu();
            h.set_path(path).unwrap();
            h.set_entry_type(EntryType::Regular);
            h.set_mode(0o644);
            h.set_uid(u64::from(rustix::process::geteuid().as_raw()));
            h.set_gid(u64::from(rustix::process::getegid().as_raw()));
            h.set_mtime(1_000_000);
            h.set_size(u64::try_from(data.len()).unwrap());
            h.set_cksum();
            out.extend_from_slice(h.as_bytes());
            out.extend_from_slice(data);
            out.extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
        }
        out.extend(std::iter::repeat_n(0, 1024));
        let hash: [u8; 32] = Sha256::digest(&out).into();
        let blob = self.tmp.path().join(name);
        fs::write(&blob, &out).unwrap();
        self.kaniko
            .base_layer(&Digest::from_sha256_bytes(&hash), &blob, &[])
            .unwrap()
    }

    /// The names in the root but `ranikust`, where a build on `/` keeps its
    /// marker (ADR-0052, D1).
    fn names(&self) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(&self.host)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != "ranikust")
            .collect();
        v.sort();
        v
    }
}

fn options() -> EncodeOptions {
    EncodeOptions {
        compression: Compression::None,
        level: 0,
        threads: NonZeroUsize::MIN,
        mtime: None,
    }
}

/// The tar's entry names.
fn names_in(diff: KanikoDiff) -> Vec<String> {
    let mut out = Vec::new();
    diff.export(options(), &mut out).unwrap();
    tar::Archive::new(out.as_slice())
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn an_image_stage_empties_the_root_extracts_and_walks_only_when_needed() {
    let f = Fixture::new();
    fs::write(f.host.join("leftover"), b"x").unwrap();
    let base = f.layer("l1", &[("etc-release", b"v1\n")]);
    let mut root = f.kaniko.prepare(Base::Image(&[base])).unwrap();
    assert_eq!(f.names(), ["etc-release", "state"], "emptied, state kept");
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::metadata(&f.host).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o755, "a workdir root starts as an image root");
    assert_eq!(fs::read(f.host.join("state/blob")).unwrap(), b"kept");
    root.set_layers_needed(true);

    root.begin_layer().unwrap();
    fs::write(f.host.join("new"), b"n").unwrap();
    fs::write(f.host.join("state/more"), b"s").unwrap();
    let diff = root.end_layer().unwrap();
    assert_eq!(
        names_in(diff),
        ["new"],
        "the state directory is never walked"
    );

    // An unchanged layer is an empty one.
    root.begin_layer().unwrap();
    let diff = root.end_layer().unwrap();
    assert!(diff.diff().is_empty());
    assert_eq!(names_in(diff), Vec::<String>::new());

    root.release().unwrap();
    assert_eq!(
        f.names(),
        ["etc-release", "new", "state"],
        "release keeps the root"
    );
    assert!(f.kaniko.finish().unwrap());
    assert_eq!(f.names(), ["state"]);
    let mode = fs::metadata(&f.host).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o700, "the workdir's own mode is given back");
}

#[test]
fn a_stage_whose_layers_are_not_needed_is_never_walked() {
    let f = Fixture::new();
    let mut root = f.kaniko.prepare(Base::Scratch).unwrap();
    root.set_layers_needed(false);
    root.begin_layer().unwrap();
    fs::write(f.host.join("a"), b"a").unwrap();
    let diff = root.end_layer().unwrap();
    assert!(diff.diff().is_empty());
    assert_eq!(diff.diff().stats.entries, 0, "no walk");
    assert!(f.kaniko.shared.hold().walker.is_none());
    root.release().unwrap();
}

#[test]
fn a_squashed_child_goes_on_from_its_parent_and_baselines_when_needed() {
    let f = Fixture::new();
    let mut parent = f.kaniko.prepare(Base::Scratch).unwrap();
    parent.set_layers_needed(false);
    parent.begin_layer().unwrap();
    fs::write(f.host.join("from-parent"), b"p").unwrap();
    drop(parent.end_layer().unwrap());

    let mut child = f.kaniko.prepare(Base::Stage(&parent)).unwrap();
    // The parent handed the root over: releasing it keeps everything.
    parent.release().unwrap();
    assert!(f.host.join("from-parent").exists());
    child.set_layers_needed(true);
    child.begin_layer().unwrap();
    fs::write(f.host.join("from-child"), b"c").unwrap();
    let diff = child.end_layer().unwrap();
    assert_eq!(names_in(diff), ["from-child"]);
    child.release().unwrap();
    assert!(f.kaniko.finish().unwrap());
    assert_eq!(f.names(), ["state"]);
}

#[test]
fn the_shared_root_has_one_holder() {
    let f = Fixture::new();
    let first = f.kaniko.prepare(Base::Scratch).unwrap();
    assert!(matches!(
        f.kaniko.prepare(Base::Scratch),
        Err(Error::State(_))
    ));
    assert!(matches!(f.kaniko.finish(), Err(Error::State(_))));
    let other = f.kaniko.prepare(Base::Stage(&first)).unwrap();
    // The first stage no longer holds it.
    assert!(matches!(
        f.kaniko.prepare(Base::Stage(&first)),
        Err(Error::State(_))
    ));
    other.release().unwrap();
    first.release().unwrap();
    assert!(matches!(other_view_after_release(&f), Err(Error::State(_))));
}

fn other_view_after_release(f: &Fixture) -> Result<()> {
    let mut a = f.kaniko.prepare(Base::Scratch)?;
    let b = f.kaniko.prepare(Base::Stage(&a))?;
    b.release()?;
    // `a` handed the root to `b`, which released it.
    a.view().map(drop)
}

/// A streamed source of the bytes of `blob`, failing at their end when
/// `fail_at_end` (a digest the source found wrong); counts its opens.
#[derive(Debug)]
struct Streamed {
    blob: PathBuf,
    fail_at_end: bool,
    opens: std::sync::atomic::AtomicUsize,
}

impl LayerOpen for Streamed {
    fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        let bytes = std::io::Cursor::new(fs::read(&self.blob)?);
        if !self.fail_at_end {
            return Ok(Box::new(bytes));
        }
        Ok(Box::new(bytes.chain(FailingReader)))
    }
}

/// Fails every read: the source's verdict at the end of the body.
struct FailingReader;

impl std::io::Read for FailingReader {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("digest mismatch"))
    }
}

#[test]
fn a_streamed_base_layer_is_read_anew_for_each_stage_and_never_kept() {
    let f = Fixture::new();
    let kept = f.layer("l1", &[("a", b"from the registry")]);
    let source = Arc::new(Streamed {
        blob: kept.blob().unwrap().to_owned(),
        fail_at_end: false,
        opens: std::sync::atomic::AtomicUsize::new(0),
    });
    let layer = f
        .kaniko
        .streamed_layer(kept.diff_id(), Arc::clone(&source) as Arc<dyn LayerOpen>)
        .unwrap();
    assert_eq!(layer.blob(), None, "a streamed layer has no file");
    let root = f
        .kaniko
        .prepare(Base::Image(std::slice::from_ref(&layer)))
        .unwrap();
    assert_eq!(fs::read(f.host.join("a")).unwrap(), b"from the registry");
    root.release().unwrap();
    f.kaniko.prepare(Base::Image(&[layer])).unwrap();
    assert_eq!(source.opens.load(Ordering::Relaxed), 2);
}

#[test]
fn a_streamed_base_layer_whose_source_fails_at_its_end_fails_the_stage() {
    let f = Fixture::new();
    let kept = f.layer("l1", &[("a", b"tampered")]);
    let source = Arc::new(Streamed {
        blob: kept.blob().unwrap().to_owned(),
        fail_at_end: true,
        opens: std::sync::atomic::AtomicUsize::new(0),
    });
    let layer = f
        .kaniko
        .streamed_layer(kept.diff_id(), source as Arc<dyn LayerOpen>)
        .unwrap();
    let e = f.kaniko.prepare(Base::Image(&[layer])).unwrap_err();
    assert!(format!("{e:?}").contains("digest mismatch"), "{e:?}");
    // The failed layer's entries reached the root before its digest was
    // checked; the next stage's root holds none of them.
    let good = f.layer("l2", &[("b", b"b")]);
    let root = f.kaniko.prepare(Base::Image(&[good])).unwrap();
    assert_eq!(f.names(), ["b", "state"]);
    root.release().unwrap();
}

#[test]
fn a_wrong_diff_id_is_refused() {
    let f = Fixture::new();
    let good = f.layer("l1", &[("a", b"a")]);
    let bad = KanikoLayer {
        diff_id: Digest::from_sha256_bytes(&[0; 32]),
        source: good.source.clone(),
    };
    let e = f.kaniko.prepare(Base::Image(&[bad])).unwrap_err();
    assert!(matches!(e, Error::DiffIdMismatch { .. }), "{e}");
}

#[test]
fn a_build_that_never_prepared_a_base_leaves_the_root_alone() {
    let f = Fixture::new();
    fs::write(f.host.join("precious"), b"x").unwrap();
    assert!(!f.kaniko.finish().unwrap());
    assert!(f.host.join("precious").exists());
}

#[test]
fn on_slash_the_first_base_goes_over_the_root_and_the_end_leaves_it() {
    let f = Fixture::at(Some("/"));
    fs::write(f.host.join("context"), b"x").unwrap();
    let base = f.layer("l1", &[("a", b"a")]);
    let root = f
        .kaniko
        .prepare(Base::Image(std::slice::from_ref(&base)))
        .unwrap();
    assert_eq!(
        f.names(),
        ["a", "context", "state"],
        "extracted over, as kaniko"
    );
    root.release().unwrap();
    // The next stage starts from an empty root, as after kaniko's
    // DeleteFilesystem between stages.
    let root = f.kaniko.prepare(Base::Image(&[base])).unwrap();
    assert_eq!(f.names(), ["a", "state"]);
    root.release().unwrap();
    assert!(
        !f.kaniko.finish().unwrap(),
        "no --cleanup: / is left as it is"
    );
    assert_eq!(f.names(), ["a", "state"]);
}

/// The snapshotter of a build on `/` (the fixture's root), with
/// `/ranikust` on the ignore list as `kaniko::ignore` puts it.
fn at_slash(f: &Fixture) -> Kaniko {
    let mut ignore = IgnoreList::new();
    ignore.insert("/state").unwrap();
    ignore.insert("/ranikust").unwrap();
    Kaniko::new(
        RootPath::open(&f.host).unwrap(),
        PathBuf::from("/"),
        ignore,
        WalkOptions {
            workers: NonZeroUsize::new(2).unwrap(),
            mode: SnapshotMode::Full,
        },
        RootPath::open(f.host.with_file_name("kept")).unwrap(),
    )
}

/// ADR-0052, D1: a build on `/` marks it under `/ranikust` at its first
/// base; the next build in the same container finds the marker and empties
/// `/` (the ignore list kept) before its first base, so nothing of the
/// first build is inherited.
#[test]
fn a_second_build_on_slash_does_not_inherit_the_first() {
    let f = Fixture::at(Some("/"));
    fs::write(f.host.join("tool"), b"the container's own").unwrap();
    let first = at_slash(&f);
    let base = f.layer("l1", &[("a", b"a")]);
    let root = first
        .prepare(Base::Image(std::slice::from_ref(&base)))
        .unwrap();
    assert!(f.host.join(BUILT_MARKER).is_file());
    assert_eq!(
        f.names(),
        ["a", "state", "tool"],
        "no earlier build: extracted over"
    );
    fs::write(f.host.join("left-by-run"), b"secret").unwrap();
    root.release().unwrap();
    assert!(!first.finish().unwrap());
    assert!(f.host.join("left-by-run").exists(), "/ is left as it is");

    let second = at_slash(&f);
    let root = second.prepare(Base::Image(&[base])).unwrap();
    assert_eq!(f.names(), ["a", "state"]);
    assert!(f.host.join(BUILT_MARKER).is_file(), "the marker stays");
    root.release().unwrap();
}

/// A build that stops before its first base (a typo, a failed pull) leaves
/// no marker; off `/` nothing is marked.
#[test]
fn only_a_base_on_slash_marks_it() {
    let f = Fixture::at(Some("/"));
    assert!(!at_slash(&f).finish().unwrap());
    assert!(!f.host.join("ranikust").exists());
    let f = Fixture::new();
    f.kaniko.prepare(Base::Scratch).unwrap().release().unwrap();
    assert!(!f.host.join("ranikust").exists());
}

#[test]
fn the_marker_needs_a_real_directory_and_keeps_it_private() {
    // A symlink at /ranikust is refused, and nothing is written through it.
    let f = Fixture::at(Some("/"));
    fs::create_dir(f.host.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink("elsewhere", f.host.join("ranikust")).unwrap();
    let e = at_slash(&f).prepare(Base::Scratch).unwrap_err();
    assert!(matches!(e, Error::State(_)), "{e}");
    assert_eq!(fs::read_dir(f.host.join("elsewhere")).unwrap().count(), 0);

    // An existing /ranikust becomes 0700; a symlink at the marker's name
    // counts as a marker and is not followed.
    let f = Fixture::at(Some("/"));
    fs::create_dir(f.host.join("ranikust")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(f.host.join("ranikust"), fs::Permissions::from_mode(0o777)).unwrap();
    }
    std::os::unix::fs::symlink("/nowhere", f.host.join(BUILT_MARKER)).unwrap();
    fs::write(f.host.join("tool"), b"x").unwrap();
    at_slash(&f)
        .prepare(Base::Scratch)
        .unwrap()
        .release()
        .unwrap();
    assert_eq!(f.names(), ["state"], "the marker was found: / emptied");
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(f.host.join("ranikust"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o700);
    }
}

/// `/ranikust` on another mount (a volume shared between containers) is
/// refused, and so is a directory swapped between look-up and opening.
#[test]
fn the_marker_directory_must_be_on_slashs_own_mount() {
    let f = Fixture::at(Some("/"));
    fs::create_dir(f.host.join("ranikust")).unwrap();
    let root = RootPath::open(&f.host).unwrap();
    let made = root.statx("ranikust").unwrap();
    check_marker_dir_name(&made, root.mnt_id()).unwrap();
    let mut other = made;
    other.mnt_id += 1;
    assert!(matches!(
        check_marker_dir_name(&other, root.mnt_id()),
        Err(Error::State(_))
    ));
    let mut point = made;
    point.mount_root = true;
    assert!(matches!(
        check_marker_dir_name(&point, root.mnt_id()),
        Err(Error::State(_))
    ));
    let opened = root.sub_root("ranikust").unwrap();
    let st = opened.statx("/").unwrap();
    check_marker_dir_opened(&made, &st, root.mnt_id(), opened.mnt_id()).unwrap();
    assert!(matches!(
        check_marker_dir_opened(&made, &st, root.mnt_id(), opened.mnt_id() + 1),
        Err(Error::State(_))
    ));
    let mut swapped = st;
    swapped.ino += 1;
    assert!(matches!(
        check_marker_dir_opened(&made, &swapped, root.mnt_id(), opened.mnt_id()),
        Err(Error::State(_))
    ));
}

fn read(view: &RootPath, p: &str) -> String {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut File::from(view.open_regular(p).unwrap()), &mut s).unwrap();
    s
}

#[test]
fn a_kept_stage_is_read_after_the_root_is_reused_and_deleted_on_release() {
    let f = Fixture::new();
    let mut a = f.kaniko.prepare(Base::Scratch).unwrap();
    a.set_layers_needed(false);
    a.begin_layer().unwrap();
    fs::create_dir_all(f.host.join("app/dist")).unwrap();
    fs::write(f.host.join("app/dist/main.js"), b"js").unwrap();
    fs::write(f.host.join("app/src.ts"), b"ts").unwrap();
    std::os::unix::fs::symlink("app/dist", f.host.join("out")).unwrap();
    drop(a.end_layer().unwrap());
    a.keep(&Keep::Paths(vec![
        PathBuf::from("out"),
        PathBuf::from("app/dist"),
    ]))
    .unwrap();
    // The root is given up: the next stage empties it.
    let b = f.kaniko.prepare(Base::Scratch).unwrap();
    assert_eq!(f.names(), ["state"]);
    let view = a.view().unwrap();
    assert_eq!(view.read_link("out").unwrap(), PathBuf::from("app/dist"));
    assert_eq!(read(view, "out/main.js"), "js");
    assert!(
        view.statx("app/src.ts").is_err(),
        "only the paths asked for"
    );
    // Keeping again after the root was reused is refused.
    assert!(matches!(a.keep(&Keep::All), Err(Error::State(_))));
    b.release().unwrap();
    assert_eq!(f.kept_entries(), 1);
    a.release().unwrap();
    assert_eq!(f.kept_entries(), 0, "the copy is deleted with its stage");
}

#[test]
fn a_stage_built_from_a_kept_one_starts_from_its_copy() {
    let f = Fixture::new();
    let base = f.layer("l1", &[("etc-release", b"v1\n")]);
    let mut a = f.kaniko.prepare(Base::Image(&[base])).unwrap();
    a.set_layers_needed(false);
    a.begin_layer().unwrap();
    fs::write(f.host.join("built"), b"b").unwrap();
    drop(a.end_layer().unwrap());
    a.keep(&Keep::All).unwrap();
    // Kept whole, without the ignored state directory.
    let mut kept: Vec<_> = fs::read_dir(f.host.with_file_name("kept"))
        .unwrap()
        .flat_map(|d| fs::read_dir(d.unwrap().path()).unwrap())
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    kept.sort();
    assert_eq!(kept, ["built", "etc-release"]);
    // Another stage takes the root, then gives it up.
    let other = f.kaniko.prepare(Base::Scratch).unwrap();
    fs::write(f.host.join("other"), b"o").unwrap();
    other.release().unwrap();

    let mut child = f.kaniko.prepare(Base::Stage(&a)).unwrap();
    assert_eq!(f.names(), ["built", "etc-release", "state"]);
    child.set_layers_needed(true);
    child.begin_layer().unwrap();
    fs::write(f.host.join("child"), b"c").unwrap();
    let diff = child.end_layer().unwrap();
    assert_eq!(names_in(diff), ["child"], "the copy is the baseline");
    child.release().unwrap();
    a.release().unwrap();
}

/// The `/` layout: the keep area inside the root, below `state/`, which
/// the ignore list holds when `ignored`.
fn kept_inside(ignored: bool) -> (tempfile::TempDir, PathBuf, Kaniko) {
    let tmp = tempfile::tempdir().unwrap();
    let host = fs::canonicalize(tmp.path()).unwrap().join("root");
    fs::create_dir_all(host.join("state/kept")).unwrap();
    fs::write(host.join("state/blob"), b"store").unwrap();
    let mut ignore = IgnoreList::new();
    ignore.insert("/ranikust").unwrap();
    if ignored {
        ignore.insert("/state").unwrap();
    }
    let kaniko = Kaniko::new(
        RootPath::open(&host).unwrap(),
        PathBuf::from("/"),
        ignore,
        WalkOptions::default(),
        RootPath::open(host.join("state/kept")).unwrap(),
    );
    (tmp, host, kaniko)
}

#[test]
fn a_keep_area_inside_the_root_is_left_out_of_the_copy() {
    let (_tmp, host, k) = kept_inside(true);
    let mut a = k.prepare(Base::Scratch).unwrap();
    fs::write(host.join("built"), b"b").unwrap();
    a.keep(&Keep::All).unwrap();
    let mut copies: Vec<_> = fs::read_dir(host.join("state/kept")).unwrap().collect();
    assert_eq!(copies.len(), 1);
    let copy = copies.pop().unwrap().unwrap().path();
    let mut names: Vec<_> = fs::read_dir(&copy)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["built"],
        "the ignored state directory is not copied"
    );
    a.release().unwrap();
}

#[test]
fn a_keep_area_inside_the_root_never_copies_itself() {
    let (_tmp, host, k) = kept_inside(false);
    let mut a = k.prepare(Base::Scratch).unwrap();
    fs::write(host.join("built"), b"b").unwrap();
    // Without the ignore entry the copy meets its own destination: refused,
    // never a recursion, and the partial copy is removed.
    let e = a.keep(&Keep::All).unwrap_err();
    assert!(
        matches!(
            e,
            Error::Rootfs(rnk_rootfs::Error::InvalidPath { reason, .. })
                if reason.contains("destination lies inside the source")
        ),
        "{e}"
    );
    assert_eq!(fs::read_dir(host.join("state/kept")).unwrap().count(), 0);
    a.release().unwrap();
}

#[test]
fn a_kept_name_taken_by_a_link_is_never_followed() {
    let f = Fixture::new();
    let kept = f.host.with_file_name("kept");
    // The next keep area name is taken by a link to the area itself.
    let id = f.kaniko.shared.next_id.load(Ordering::Relaxed);
    std::os::unix::fs::symlink(".", kept.join(format!("k{id}"))).unwrap();
    let e = make_dir(&f.kaniko.shared, &format!("k{id}")).unwrap_err();
    assert!(matches!(e, Error::Rootfs(_)), "{e}");
}

#[test]
fn a_copy_from_image_is_unpacked_beside_the_root_while_a_stage_holds_it() {
    let f = Fixture::new();
    let holder = f.kaniko.prepare(Base::Scratch).unwrap();
    let image = f.layer("img", &[("bin-tool", b"tool")]);
    let mut source = f.kaniko.prepare_source(&[image]).unwrap();
    assert_eq!(f.names(), ["state"], "the shared root is not touched");
    assert_eq!(read(source.view().unwrap(), "bin-tool"), "tool");
    source.release().unwrap();
    assert_eq!(f.kept_entries(), 0);
    holder.release().unwrap();
}

#[test]
fn on_slash_a_kept_stage_is_a_tar_with_no_name_unpacked_only_for_its_reader() {
    let (f, store) = Fixture::tars();
    let mut a = f.kaniko.prepare(Base::Scratch).unwrap();
    a.set_layers_needed(false);
    a.begin_layer().unwrap();
    fs::create_dir_all(f.host.join("app/dist")).unwrap();
    fs::write(f.host.join("app/dist/main.js"), b"js").unwrap();
    fs::write(f.host.join("app/src.ts"), b"ts").unwrap();
    std::os::unix::fs::symlink("app/dist", f.host.join("out")).unwrap();
    drop(a.end_layer().unwrap());
    a.keep(&Keep::Paths(vec![
        PathBuf::from("out"),
        PathBuf::from("app/dist"),
    ]))
    .unwrap();
    // Nothing with a name: the tar is an anonymous file.
    assert_eq!(f.state_names(), ["blob"]);
    let mut b = f.kaniko.prepare(Base::Scratch).unwrap();
    assert_eq!(f.names(), ["state"]);
    // The reader's view is unpacked in the state directory...
    let view = a.view().unwrap();
    assert_eq!(view.read_link("out").unwrap(), PathBuf::from("app/dist"));
    assert_eq!(read(view, "out/main.js"), "js");
    assert!(
        view.statx("app/src.ts").is_err(),
        "only the paths asked for"
    );
    assert_eq!(f.state_names().len(), 2, "the unpacked directory");
    // ... asked for before the reading instruction began, still there
    // through it, and gone at its end, before any RUN.
    b.set_layers_needed(false);
    b.begin_layer().unwrap();
    assert_eq!(read(a.view().unwrap(), "out/main.js"), "js");
    drop(b.end_layer().unwrap());
    assert_eq!(f.state_names(), ["blob"]);
    b.begin_layer().unwrap();
    let run = store.enter_run().unwrap();
    // A reader asking again during a RUN on this thread fails closed.
    assert!(a.view().is_err());
    drop(run);
    assert_eq!(read(a.view().unwrap(), "out/main.js"), "js");
    drop(b.end_layer().unwrap());
    b.release().unwrap();
    assert_eq!(f.state_names(), ["blob"], "release deletes the view too");
    a.release().unwrap();
}

#[test]
fn on_slash_a_stage_built_from_a_kept_tar_starts_from_it() {
    let (f, _store) = Fixture::tars();
    let base = f.layer("l1", &[("etc-release", b"v1\n")]);
    let mut a = f.kaniko.prepare(Base::Image(&[base])).unwrap();
    a.set_layers_needed(false);
    a.begin_layer().unwrap();
    fs::write(f.host.join("built"), b"b").unwrap();
    drop(a.end_layer().unwrap());
    a.keep(&Keep::All).unwrap();
    assert_eq!(f.state_names(), ["blob"]);
    let other = f.kaniko.prepare(Base::Scratch).unwrap();
    other.release().unwrap();
    let mut child = f.kaniko.prepare(Base::Stage(&a)).unwrap();
    assert_eq!(f.names(), ["built", "etc-release", "state"]);
    assert_eq!(fs::read(f.host.join("built")).unwrap(), b"b");
    assert_eq!(fs::read(f.host.join("state/blob")).unwrap(), b"kept");
    child.set_layers_needed(true);
    child.begin_layer().unwrap();
    fs::write(f.host.join("child"), b"c").unwrap();
    assert_eq!(names_in(child.end_layer().unwrap()), ["child"]);
    child.release().unwrap();
    a.release().unwrap();
}

#[test]
fn on_slash_a_copy_from_image_stays_blobs_until_read() {
    let (f, _store) = Fixture::tars();
    let holder = f.kaniko.prepare(Base::Scratch).unwrap();
    let image = f.layer("img", &[("bin-tool", b"tool")]);
    let mut source = f.kaniko.prepare_source(&[image]).unwrap();
    assert_eq!(f.state_names(), ["blob"], "nothing unpacked yet");
    assert_eq!(read(source.view().unwrap(), "bin-tool"), "tool");
    assert_eq!(f.state_names().len(), 2);
    source.release().unwrap();
    assert_eq!(f.state_names(), ["blob"]);
    holder.release().unwrap();
}

#[test]
fn on_slash_keep_all_leaves_the_ignored_state_out_of_the_tar() {
    let (f, _store) = Fixture::tars();
    let mut a = f.kaniko.prepare(Base::Scratch).unwrap();
    a.set_layers_needed(false);
    a.begin_layer().unwrap();
    fs::write(f.host.join("built"), b"b").unwrap();
    drop(a.end_layer().unwrap());
    a.keep(&Keep::All).unwrap();
    let b = f.kaniko.prepare(Base::Scratch).unwrap();
    b.release().unwrap();
    let view = a.view().unwrap();
    assert_eq!(read(view, "built"), "b");
    assert!(
        view.statx("state").is_err(),
        "the state directory is not kept"
    );
    a.release().unwrap();
}

#[test]
fn on_slash_a_kept_name_like_a_whiteout_fails_the_keep_never_vanishes() {
    let (f, _store) = Fixture::tars();
    let mut a = f.kaniko.prepare(Base::Scratch).unwrap();
    a.set_layers_needed(false);
    a.begin_layer().unwrap();
    fs::create_dir(f.host.join("d")).unwrap();
    fs::write(f.host.join("d/x"), b"x").unwrap();
    fs::write(f.host.join("d/.wh.x"), b"not a whiteout").unwrap();
    drop(a.end_layer().unwrap());
    // The tar is written as a layer is: a `.wh.` name cannot be written
    // (Q-053), so the keep fails rather than lose `d/x` on the way back.
    let e = a.keep(&Keep::All).unwrap_err();
    assert!(e.to_string().contains(".wh."), "{e}");
    a.release().unwrap();
}

#[test]
fn on_slash_a_view_swapped_for_a_link_is_never_emptied_through_it() {
    let (f, _store) = Fixture::tars();
    let mut a = f.kaniko.prepare(Base::Scratch).unwrap();
    a.set_layers_needed(false);
    a.begin_layer().unwrap();
    fs::write(f.host.join("built"), b"b").unwrap();
    drop(a.end_layer().unwrap());
    a.keep(&Keep::All).unwrap();
    let mut b = f.kaniko.prepare(Base::Scratch).unwrap();
    fs::write(f.host.join("precious"), b"p").unwrap();
    assert_eq!(read(a.view().unwrap(), "built"), "b");
    // Something swaps the view for a link to the root.
    let state = f.host.join("state");
    let name = f
        .state_names()
        .into_iter()
        .find(|n| n.starts_with(rnk_store::ANON_PREFIX))
        .unwrap();
    fs::rename(state.join(&name), state.join("moved")).unwrap();
    std::os::unix::fs::symlink(&f.host, state.join(&name)).unwrap();
    b.set_layers_needed(false);
    b.begin_layer().unwrap();
    assert!(matches!(b.end_layer(), Err(Error::State(_))));
    assert_eq!(fs::read(f.host.join("precious")).unwrap(), b"p");
}

/// A streamed layer of `files` in `f`'s snapshotter, failing at the end of
/// its body when `fail_at_end`.
fn streamed(f: &Fixture, name: &str, files: &[(&str, &[u8])], fail_at_end: bool) -> KanikoLayer {
    let kept = f.layer(name, files);
    let source = Arc::new(Streamed {
        blob: kept.blob().unwrap().to_owned(),
        fail_at_end,
        opens: std::sync::atomic::AtomicUsize::new(0),
    });
    f.kaniko
        .streamed_layer(kept.diff_id(), source as Arc<dyn LayerOpen>)
        .unwrap()
}

#[test]
fn on_slash_a_streamed_layer_is_verified_before_any_entry_reaches_the_root() {
    let (f, _store) = Fixture::tars();
    let good = streamed(&f, "l1", &[("a", b"a")], false);
    let root = f.kaniko.prepare(Base::Image(&[good])).unwrap();
    assert_eq!(f.names(), ["a", "state"]);
    assert_eq!(f.state_names(), ["blob"], "the spool has no name");
    root.release().unwrap();
    // A layer whose digest turns out wrong at the end of its body: none of
    // its entries is in `/`, which nothing empties.
    let bad = streamed(&f, "l2", &[("unverified", b"x")], true);
    let e = f.kaniko.prepare(Base::Image(&[bad])).unwrap_err();
    assert!(format!("{e:?}").contains("digest mismatch"), "{e:?}");
    assert!(!f.host.join("unverified").exists());
    assert_eq!(f.state_names(), ["blob"]);
}

#[test]
fn on_slash_a_streamed_layer_with_a_wrong_diff_id_never_reaches_the_root() {
    let (f, _store) = Fixture::tars();
    let good = streamed(&f, "l1", &[("unverified", b"x")], false);
    let bad = KanikoLayer {
        diff_id: Digest::from_sha256_bytes(&[0; 32]),
        source: good.source.clone(),
    };
    let e = f.kaniko.prepare(Base::Image(&[bad])).unwrap_err();
    assert!(matches!(e, Error::DiffIdMismatch { .. }), "{e}");
    assert!(!f.host.join("unverified").exists());
}

#[test]
fn on_slash_a_streamed_copy_from_image_is_verified_before_it_is_unpacked() {
    let (f, _store) = Fixture::tars();
    let holder = f.kaniko.prepare(Base::Scratch).unwrap();
    let bad = streamed(&f, "img", &[("bin-tool", b"tool")], true);
    let mut source = f.kaniko.prepare_source(&[bad]).unwrap();
    assert!(source.view().is_err());
    // No directory was made for it: the check came first.
    assert_eq!(f.state_names(), ["blob"]);
    source.release().unwrap();
    let good = streamed(&f, "img", &[("bin-tool", b"tool")], false);
    let mut source = f.kaniko.prepare_source(&[good]).unwrap();
    assert_eq!(read(source.view().unwrap(), "bin-tool"), "tool");
    source.release().unwrap();
    assert_eq!(f.state_names(), ["blob"]);
    holder.release().unwrap();
}

#[test]
fn on_slash_a_streamed_gzip_layer_is_spooled_compressed_and_extracted() {
    use std::io::Write as _;
    let (f, _store) = Fixture::tars();
    let plain = f.layer("l1", &[("a", b"gzipped")]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&fs::read(plain.blob().unwrap()).unwrap())
        .unwrap();
    let gzipped = f.tmp.path().join("l1.gz");
    fs::write(&gzipped, gz.finish().unwrap()).unwrap();
    let source = Arc::new(Streamed {
        blob: gzipped,
        fail_at_end: false,
        opens: std::sync::atomic::AtomicUsize::new(0),
    });
    let layer = f
        .kaniko
        .streamed_layer(plain.diff_id(), Arc::clone(&source) as Arc<dyn LayerOpen>)
        .unwrap();
    let root = f.kaniko.prepare(Base::Image(&[layer])).unwrap();
    assert_eq!(fs::read(f.host.join("a")).unwrap(), b"gzipped");
    // Downloaded once: the second decompression reads the spool.
    assert_eq!(source.opens.load(Ordering::Relaxed), 1);
    root.release().unwrap();
}

// Red team, ADR-0052 D1 and ADR-0058 (threat model T1, T3, T4): hostile
// `/ranikust` and `.built` planted by an image, a `RUN` or a daemon left
// by an earlier build; a second build's emptying of `/` against the real
// ignore list; diffs the builder must never take for "changed nothing".

/// An entry of a hostile base layer.
enum Raw<'a> {
    File(&'a str, &'a [u8]),
    Link(&'a str, &'a str),
}

/// A base layer of raw entries (whiteouts are files named `.wh.*`), as a
/// blob file.
fn raw_layer(f: &Fixture, name: &str, entries: &[Raw<'_>]) -> KanikoLayer {
    let mut b = tar::Builder::new(Vec::new());
    for e in entries {
        let mut h = Header::new_gnu();
        h.set_uid(u64::from(rustix::process::geteuid().as_raw()));
        h.set_gid(u64::from(rustix::process::getegid().as_raw()));
        h.set_mtime(1_000_000);
        match e {
            Raw::File(p, data) => {
                h.set_entry_type(EntryType::Regular);
                h.set_mode(0o644);
                h.set_size(u64::try_from(data.len()).unwrap());
                b.append_data(&mut h, p, *data).unwrap();
            }
            Raw::Link(p, target) => {
                h.set_entry_type(EntryType::Symlink);
                h.set_mode(0o777);
                h.set_size(0);
                b.append_link(&mut h, p, target).unwrap();
            }
        }
    }
    let out = b.into_inner().unwrap();
    let hash: [u8; 32] = Sha256::digest(&out).into();
    let blob = f.tmp.path().join(name);
    fs::write(&blob, &out).unwrap();
    f.kaniko
        .base_layer(&Digest::from_sha256_bytes(&hash), &blob, &[])
        .unwrap()
}

/// Runs `f` on another thread, failing after 30 s: a FIFO opened would
/// block forever.
fn within_time<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::channel();
        s.spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .expect("blocked: a FIFO was opened")
    })
}

fn mkfifo(p: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        p,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
}

fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    fs::symlink_metadata(p).unwrap().mode() & 0o7777
}

/// A directory beside the fixture's root, outside it, with one file, mode
/// `0755`.
fn outside_dir(f: &Fixture) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let out = f.host.with_file_name("outside");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("secret"), b"HOST").unwrap();
    fs::set_permissions(&out, fs::Permissions::from_mode(0o755)).unwrap();
    out
}

fn assert_outside_untouched(out: &Path) {
    let names: Vec<_> = fs::read_dir(out)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["secret"], "nothing added outside the root");
    assert_eq!(fs::read(out.join("secret")).unwrap(), b"HOST");
    assert_eq!(mode(out), 0o755, "no chmod outside the root");
}

/// `/ranikust` planted as anything but a directory of `/`'s mount (a file,
/// a FIFO, a socket, a dangling link, a link out of the root, into the
/// ignored state directory, to `/`, a loop) is refused before `/` changes:
/// the base is not extracted, nothing is emptied, nothing is opened,
/// written or chmodded through it.
#[test]
fn a_marker_directory_of_another_type_is_refused_before_slash_changes() {
    for kind in [
        "file", "fifo", "socket", "dangling", "outside", "state", "slash", "loop",
    ] {
        let f = Fixture::at(Some("/"));
        let out = outside_dir(&f);
        fs::write(f.host.join("tool"), b"own").unwrap();
        let p = f.host.join("ranikust");
        match kind {
            "file" => fs::write(&p, b"x").unwrap(),
            "fifo" => mkfifo(&p),
            "socket" => drop(std::os::unix::net::UnixListener::bind(&p).unwrap()),
            "dangling" => std::os::unix::fs::symlink("/nowhere", &p).unwrap(),
            "outside" => std::os::unix::fs::symlink(&out, &p).unwrap(),
            "state" => std::os::unix::fs::symlink("state", &p).unwrap(),
            "slash" => std::os::unix::fs::symlink("/", &p).unwrap(),
            _ => std::os::unix::fs::symlink("ranikust", &p).unwrap(),
        }
        let planted = fs::symlink_metadata(&p).unwrap().file_type();
        let base = f.layer("l1", &[("a", b"a")]);
        let k = at_slash(&f);
        let r = within_time(|| {
            k.prepare(Base::Image(std::slice::from_ref(&base)))
                .map(drop)
        });
        assert!(matches!(r, Err(Error::State(_))), "{kind}: {r:?}");
        assert_eq!(f.names(), ["state", "tool"], "{kind}: / changed");
        assert_eq!(f.state_names(), ["blob"], "{kind}");
        assert_eq!(fs::read(f.host.join("state/blob")).unwrap(), b"kept");
        assert_outside_untouched(&out);
        assert_eq!(
            fs::symlink_metadata(&p).unwrap().file_type(),
            planted,
            "{kind}: the planted entry was replaced"
        );
    }
}

/// Anything planted at `/ranikust/.built` (a FIFO, a socket, a directory,
/// a link out of the root, a dangling link, a hardlink to a file outside)
/// counts as a marker, so the later build empties `/`; it is never opened,
/// followed or written, and nothing outside the root changes.
#[test]
fn a_planted_marker_of_any_type_counts_and_is_never_opened() {
    for kind in ["fifo", "socket", "dir", "outside", "dangling", "hardlink"] {
        let f = Fixture::at(Some("/"));
        let out = outside_dir(&f);
        fs::write(f.host.join("left-by-run"), b"x").unwrap();
        fs::create_dir(f.host.join("ranikust")).unwrap();
        let p = f.host.join(BUILT_MARKER);
        match kind {
            "fifo" => mkfifo(&p),
            "socket" => drop(std::os::unix::net::UnixListener::bind(&p).unwrap()),
            "dir" => fs::create_dir(&p).unwrap(),
            "outside" => std::os::unix::fs::symlink(out.join("secret"), &p).unwrap(),
            "dangling" => std::os::unix::fs::symlink("/nowhere/x", &p).unwrap(),
            _ => fs::hard_link(out.join("secret"), &p).unwrap(),
        }
        let before = fs::symlink_metadata(&p).unwrap().file_type();
        let base = f.layer("l1", &[("a", b"a")]);
        let k = at_slash(&f);
        let root = within_time(|| k.prepare(Base::Image(std::slice::from_ref(&base))))
            .unwrap_or_else(|e| panic!("{kind}: {e}"));
        assert_eq!(f.names(), ["a", "state"], "{kind}: / was not emptied");
        assert_eq!(fs::symlink_metadata(&p).unwrap().file_type(), before);
        assert_outside_untouched(&out);
        root.release().unwrap();
        if kind == "hardlink" {
            // The link shares the outside file's inode: still one name more,
            // never truncated.
            assert_eq!(fs::read(&p).unwrap(), b"HOST");
        }
    }
}

/// A daemon left by an earlier build swaps `/ranikust` with a link (to a
/// directory of `/`, and to the host directory outside the root) as fast as
/// it can while the marker is looked for: whatever `mark_slash` returns,
/// it never makes the marker in, nor chmods, the link's target.
#[test]
fn a_marker_directory_swapped_for_a_link_never_marks_its_target() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = Fixture::at(Some("/"));
    let out = outside_dir(&f);
    fs::create_dir(f.host.join("ranikust")).unwrap();
    fs::create_dir(f.host.join("elsewhere")).unwrap();
    fs::set_permissions(f.host.join("elsewhere"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("elsewhere", f.host.join("swap-in")).unwrap();
    std::os::unix::fs::symlink(&out, f.host.join("swap-out")).unwrap();
    let real = File::open(f.host.join("ranikust")).unwrap();
    let host_dir = File::open(&f.host).unwrap();
    let root = RootPath::open(&f.host).unwrap();
    let (mut marked, mut refused) = (0, 0);
    for link in ["swap-in", "swap-out"] {
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    if rustix::fs::renameat_with(
                        &host_dir,
                        "ranikust",
                        &host_dir,
                        link,
                        rustix::fs::RenameFlags::EXCHANGE,
                    )
                    .is_err()
                    {
                        break;
                    }
                }
            });
            // At least 3000 tries, more until both outcomes were seen (a
            // slow or emulated machine may start the swapper late).
            let (m0, r0) = (marked, refused);
            let mut tries = 0;
            while tries < 3000 || ((marked == m0 || refused == r0) && tries < 200_000) {
                tries += 1;
                match mark_slash(&root) {
                    Ok(_) => marked += 1,
                    Err(_) => refused += 1,
                }
                let _ = rustix::fs::unlinkat(&real, ".built", rustix::fs::AtFlags::empty());
            }
            stop.store(true, Ordering::Relaxed);
        });
        // Put the real directory back under its name.
        if fs::symlink_metadata(f.host.join("ranikust"))
            .unwrap()
            .is_symlink()
        {
            rustix::fs::renameat_with(
                &host_dir,
                "ranikust",
                &host_dir,
                link,
                rustix::fs::RenameFlags::EXCHANGE,
            )
            .unwrap();
        }
    }
    assert_eq!(fs::read_dir(f.host.join("elsewhere")).unwrap().count(), 0);
    assert_eq!(mode(&f.host.join("elsewhere")), 0o755);
    assert_outside_untouched(&out);
    assert!(f.host.join("ranikust").is_dir());
    assert!(
        marked > 0 && refused > 0,
        "no race: {marked} marked, {refused} refused"
    );
}

/// A base layer aimed at the marker, the container's markers and its mounts
/// (`/var/run -> ../run` as alpine has it, a secret mounted below `/run`,
/// a volume, `/etc/resolv.conf`): whiteouts, an opaque `/` and `/run`,
/// links replacing `/ranikust` and `/var/run`, files through `var/run`.
fn hostile_base(f: &Fixture) -> KanikoLayer {
    raw_layer(
        f,
        "hostile",
        &[
            Raw::File(".wh..wh..opq", b""),
            Raw::File(".wh.ranikust", b""),
            Raw::File("ranikust/.wh..built", b""),
            Raw::Link("ranikust", "/workspace"),
            Raw::File(".wh..dockerenv", b""),
            Raw::File("var/run/secrets/token", b"EVIL"),
            Raw::File("var/run/.containerenv", b"EVIL"),
            Raw::File("run/.wh..wh..opq", b""),
            Raw::File("run/.wh.secrets", b""),
            Raw::Link("run/secrets", "/tmp"),
            Raw::File("workspace/.wh..wh..opq", b""),
            Raw::File("workspace/src", b"EVIL"),
            Raw::File("etc/resolv.conf", b"nameserver 6.6.6.6"),
            Raw::Link("var/run", "/proc/self"),
            Raw::File("var/run/environ", b"EVIL"),
            Raw::File("image-file", b"i"),
        ],
    )
}

/// ADR-0052 D1 with the real ignore list (`kaniko::ignore_list`, mounts
/// from a container's mountinfo): two builds on `/`, each on a hostile
/// base. The first extracts over `/`, the second empties it first; neither
/// touches the container's markers (`/.dockerenv`, `/run/.containerenv`),
/// its mounts (a secret below `/run`, reached through `/var/run`, a volume,
/// `/etc/resolv.conf`), the state directory or the marker, and the second
/// keeps nothing the first left.
#[test]
fn a_second_build_empties_slash_but_never_the_containers_markers_or_mounts() {
    let f = Fixture::at(Some("/"));
    let h = f.host.clone();
    let out = outside_dir(&f);
    fs::write(h.join(".dockerenv"), b"").unwrap();
    fs::create_dir_all(h.join("run/secrets")).unwrap();
    fs::write(h.join("run/secrets/token"), b"SECRET").unwrap();
    fs::write(h.join("run/.containerenv"), b"engine=podman").unwrap();
    fs::create_dir_all(h.join("var")).unwrap();
    std::os::unix::fs::symlink("../run", h.join("var/run")).unwrap();
    fs::create_dir_all(h.join("workspace")).unwrap();
    fs::write(h.join("workspace/src"), b"SRC").unwrap();
    fs::create_dir_all(h.join("etc")).unwrap();
    fs::write(h.join("etc/resolv.conf"), b"nameserver 10.0.0.1").unwrap();
    fs::create_dir_all(h.join("proc/self")).unwrap();
    let hp = h.display();
    let mountinfo = format!(
        "1 0 0:1 / / rw - overlay overlay rw\n\
         2 1 0:2 / {hp}/run/secrets ro - tmpfs tmpfs rw\n\
         3 1 0:3 /vol {hp}/workspace rw - ext4 /dev/x rw\n\
         4 1 0:3 /c/resolv.conf {hp}/etc/resolv.conf rw - ext4 /dev/x rw\n"
    );
    let mounts = crate::kaniko::parse_mountinfo(mountinfo.as_bytes()).unwrap();
    let state = h.join("state");
    let ignore = crate::kaniko::ignore_list(
        &crate::kaniko::IgnoreSpec {
            root: &h,
            state_dir: Some(&state),
            ignore_paths: &[],
        },
        &mounts,
    )
    .unwrap();
    let kaniko = || {
        Kaniko::new(
            RootPath::open(&h).unwrap(),
            PathBuf::from("/"),
            ignore.clone(),
            WalkOptions {
                workers: NonZeroUsize::new(2).unwrap(),
                mode: SnapshotMode::Full,
            },
            RootPath::open(h.with_file_name("kept")).unwrap(),
        )
    };
    let intact = |when: &str| {
        assert_eq!(fs::read(h.join(".dockerenv")).unwrap(), b"", "{when}");
        assert_eq!(fs::read(h.join("run/secrets/token")).unwrap(), b"SECRET");
        assert_eq!(fs::read_dir(h.join("run/secrets")).unwrap().count(), 1);
        assert_eq!(
            fs::read(h.join("run/.containerenv")).unwrap(),
            b"engine=podman",
            "{when}"
        );
        assert_eq!(fs::read(h.join("workspace/src")).unwrap(), b"SRC", "{when}");
        assert_eq!(fs::read_dir(h.join("workspace")).unwrap().count(), 1);
        assert_eq!(
            fs::read(h.join("etc/resolv.conf")).unwrap(),
            b"nameserver 10.0.0.1",
            "{when}"
        );
        assert!(h.join("ranikust").is_dir(), "{when}");
        assert!(h.join(BUILT_MARKER).is_file(), "{when}");
        assert_eq!(f.state_names(), ["blob"], "{when}");
        assert!(!h.join("proc/self/environ").exists(), "{when}");
        assert_outside_untouched(&out);
    };

    let first = kaniko();
    let base = hostile_base(&f);
    let root = first
        .prepare(Base::Image(std::slice::from_ref(&base)))
        .unwrap();
    intact("the first build's base");
    assert!(h.join("image-file").exists());
    // What the first build's `RUN`s leave.
    fs::write(h.join("left-by-run"), b"x").unwrap();
    fs::create_dir_all(h.join("usr/local/bin")).unwrap();
    fs::write(h.join("usr/local/bin/tool"), b"x").unwrap();
    root.release().unwrap();
    assert!(!first.finish().unwrap());

    let second = kaniko();
    let root = second.prepare(Base::Image(&[base])).unwrap();
    intact("the second build's base");
    assert!(!h.join("left-by-run").exists());
    assert!(!h.join("usr").exists());
    assert!(h.join("image-file").exists());
    root.release().unwrap();
}

/// The SHA-256 of `diff` encoded as a plain tar: its diff ID.
fn diff_id(diff: KanikoDiff) -> [u8; 32] {
    let mut out = Vec::new();
    diff.export(options(), &mut out).unwrap();
    Sha256::digest(&out).into()
}

/// ADR-0058: the builder drops a layer whose diff ID is the empty tar's.
/// A step that only deletes (a file, an empty directory), replaces a
/// directory by an empty one, or only changes a mode or an mtime must
/// never encode to it; a step that changes nothing, or undoes what it did,
/// does.
#[test]
fn only_a_step_that_changes_nothing_encodes_to_the_empty_layer() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = Fixture::new();
    let empty: [u8; 32] = Sha256::digest([0_u8; 1024]).into();
    let base = f.layer(
        "l1",
        &[
            ("a", b"a"),
            ("d/x", b"x"),
            ("d/y", b"y"),
            ("m", b"m"),
            ("t", b"t"),
            ("e/keep", b"k"),
        ],
    );
    let mut root = f.kaniko.prepare(Base::Image(&[base])).unwrap();
    root.set_layers_needed(true);
    let h = f.host.clone();
    fs::create_dir(h.join("empty-dir")).unwrap();
    // The walker's baseline includes `empty-dir`.
    root.begin_layer().unwrap();
    drop(root.end_layer().unwrap());
    let mut step = |what: &str, change: &dyn Fn()| {
        root.begin_layer().unwrap();
        change();
        let id = diff_id(root.end_layer().unwrap());
        (what.to_owned(), id)
    };
    let steps = [
        step("rm a", &|| fs::remove_file(h.join("a")).unwrap()),
        step("rmdir empty-dir", &|| {
            fs::remove_dir(h.join("empty-dir")).unwrap();
        }),
        step("rm -r d && mkdir d", &|| {
            fs::remove_dir_all(h.join("d")).unwrap();
            fs::create_dir(h.join("d")).unwrap();
        }),
        step("chmod 600 m", &|| {
            fs::set_permissions(h.join("m"), fs::Permissions::from_mode(0o600)).unwrap();
        }),
        step("touch -m t", &|| {
            let t = fs::File::options().write(true).open(h.join("t")).unwrap();
            t.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5))
                .unwrap();
        }),
        step("rm e/keep", &|| fs::remove_file(h.join("e/keep")).unwrap()),
    ];
    for (what, id) in &steps {
        assert_ne!(*id, empty, "{what}: taken for a step that changed nothing");
    }
    let (_, nothing) = step("true", &|| {});
    assert_eq!(nothing, empty);
    let (_, undone) = step("touch n && rm n", &|| {
        fs::write(h.join("n"), b"n").unwrap();
        fs::remove_file(h.join("n")).unwrap();
    });
    assert_eq!(undone, empty);
    root.release().unwrap();
}
