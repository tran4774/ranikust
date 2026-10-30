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
        let tmp = tempfile::tempdir().unwrap();
        let host = fs::canonicalize(tmp.path()).unwrap().join("root");
        fs::create_dir_all(host.join("state")).unwrap();
        fs::write(host.join("state/blob"), b"kept").unwrap();
        let mut ignore = IgnoreList::new();
        ignore.insert("/state").unwrap();
        let root = RootPath::open(&host).unwrap();
        let walk = WalkOptions {
            workers: NonZeroUsize::new(2).unwrap(),
            mode: SnapshotMode::Full,
        };
        let kaniko = Kaniko::new(root, host.clone(), ignore, walk);
        Self { tmp, host, kaniko }
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
    assert_eq!(f.names(), ["state"]);
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
    assert!(matches!(f.kaniko.empty(), Err(Error::State(_))));
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
