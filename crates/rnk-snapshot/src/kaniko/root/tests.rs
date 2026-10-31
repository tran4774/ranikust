use std::fs;
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

    /// The names in the state directory, as a `RUN` would list them.
    fn state_names(&self) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(self.host.join("state"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
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

    fn names(&self) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(&self.host)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
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

#[test]
fn a_wrong_diff_id_is_refused() {
    let f = Fixture::new();
    let good = f.layer("l1", &[("a", b"a")]);
    let bad = KanikoLayer {
        diff_id: Digest::from_sha256_bytes(&[0; 32]),
        blob: good.blob.clone(),
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
