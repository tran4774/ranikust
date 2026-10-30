//! The tar writer: layout, goldens, parents, hardlinks and errors.

use std::collections::HashMap;
use std::io::{self, Write};

use rnk_layer::{EntryKind, Error, LayerWriter};

use crate::support::{
    MemSource, ReadBack, Tree, entry, long_component, path, plain, read_tar, sample_tree,
    sha256_hex,
};

/// sha256 of the sample tree as a plain tar. Listed with GNU tar 1.35
/// (`tar --xattrs -tvvf`) when it was pinned: names, modes, owners, times,
/// links, devices and the xattr all read back. A change here changes every
/// layer digest.
const SAMPLE_TAR_SHA256: &str = "4c9b5fae31d8b4b308b2a35af3345654ed5ac2079361f45dcb261b75e62602d2";

#[test]
fn sample_tree_matches_its_golden_digest() {
    let bytes = sample_tree().encode(plain()).unwrap();
    assert_eq!(sha256_hex(&bytes), SAMPLE_TAR_SHA256);
}

#[test]
fn sample_tree_is_sorted_with_parents_and_markers() {
    let long = long_component();
    let bytes = sample_tree().encode(plain()).unwrap();
    let names: Vec<String> = read_tar(&bytes).iter().map(ReadBack::name).collect();
    let expected = [
        "deep/".to_owned(),
        "deep/link".to_owned(),
        format!("deep/{long}/"),
        format!("deep/{long}/file"),
        "dev/".to_owned(),
        "dev/null".to_owned(),
        "dev/sda".to_owned(),
        "etc/".to_owned(),
        "etc/hostname".to_owned(),
        "etc/localtime".to_owned(),
        "home/".to_owned(),
        "home/big/".to_owned(),
        "home/big/owned".to_owned(),
        "opt/".to_owned(),
        "opt/app/".to_owned(),
        "opt/app/.wh..wh..opq".to_owned(),
        "opt/app/new".to_owned(),
        "root/".to_owned(),
        "root/.wh..cache".to_owned(),
        "run/".to_owned(),
        "run/fifo".to_owned(),
        "tmp/".to_owned(),
        "usr/".to_owned(),
        "usr/bin/".to_owned(),
        "usr/bin/ping".to_owned(),
        "usr/bin/ping6".to_owned(),
    ];
    assert_eq!(names, expected);
}

#[test]
fn sample_tree_headers_read_back_with_the_tar_crate() {
    let long = long_component();
    let bytes = sample_tree().encode(plain()).unwrap();
    let entries = read_tar(&bytes);
    let by_name: HashMap<String, &ReadBack> = entries.iter().map(|e| (e.name(), e)).collect();
    let get = |name: &str| {
        *by_name
            .get(name)
            .unwrap_or_else(|| panic!("{name} missing"))
    };

    // Raw mode bits survive, including sticky and setuid; type bits do not
    // leak into the mode field.
    assert_eq!((get("tmp/").typeflag, get("tmp/").mode), (b'5', 0o1777));
    assert_eq!(
        get("root/").mode,
        0o700,
        "parent metadata comes from the source"
    );
    let ping = get("usr/bin/ping");
    assert_eq!((ping.typeflag, ping.mode), (b'0', 0o4755));
    assert_eq!(ping.content, b"\x7fELF ping");
    assert_eq!(ping.xattrs[&b"security.capability".to_vec()].len(), 20);

    let ping6 = get("usr/bin/ping6");
    assert_eq!(ping6.typeflag, b'1');
    assert_eq!(ping6.link.as_deref(), Some(&b"usr/bin/ping"[..]));
    assert!(ping6.content.is_empty());

    for marker in ["root/.wh..cache", "opt/app/.wh..wh..opq"] {
        let m = get(marker);
        assert_eq!(
            (m.typeflag, m.mode, m.uid, m.gid),
            (b'0', 0, 0, 0),
            "{marker}"
        );
        assert_eq!(m.mtime, 1_700_000_000);
        assert!(m.content.is_empty());
    }

    assert_eq!(
        (get("dev/null").typeflag, get("dev/null").device),
        (b'3', (1, 3))
    );
    assert_eq!(
        (get("dev/sda").typeflag, get("dev/sda").device),
        (b'4', (8, 0))
    );
    assert_eq!(get("run/fifo").typeflag, b'6');
    let localtime = get("etc/localtime");
    assert_eq!(localtime.typeflag, b'2');
    assert_eq!(
        localtime.link.as_deref(),
        Some(&b"/usr/share/zoneinfo/UTC"[..])
    );
    let link = get("deep/link");
    assert_eq!(
        link.link.as_deref(),
        Some(format!("/deep/{long}/file").as_bytes())
    );
    assert_eq!(get(&format!("deep/{long}/file")).content, b"deep\n");
    assert_eq!(get(&format!("deep/{long}/")).mode, 0o750);

    let owned = get("home/big/owned");
    assert_eq!(
        (owned.uid, owned.gid, owned.mtime),
        (3_000_000, 3_000_001, -1)
    );
    assert_eq!(get("etc/hostname").content, b"ranikust\n");
}

#[test]
fn parents_are_looked_up_once_outermost_first() {
    let mut t = sample_tree();
    t.encode(plain()).unwrap();
    let long = long_component();
    let expected: Vec<_> = [
        "deep",
        &format!("deep/{long}"),
        "home",
        "home/big",
        "opt",
        "root",
        "usr",
        "usr/bin",
    ]
    .iter()
    .map(|p| path(p))
    .collect();
    assert_eq!(t.source.lookups, expected);
}

#[test]
fn input_order_does_not_change_the_bytes() {
    let mut t = sample_tree();
    let forward = t.encode(plain()).unwrap();
    t.entries.reverse();
    assert_eq!(t.encode(plain()).unwrap(), forward);
    t.entries.rotate_left(7);
    assert_eq!(t.encode(plain()).unwrap(), forward);
}

#[test]
fn reproducible_mtime_clamps_later_timestamps() {
    // As BuildKit's rewrite-timestamp: later times become the epoch,
    // earlier ones (the sample's -1) are kept.
    let mut opts = plain();
    opts.mtime = Some(1);
    let mut t = sample_tree();
    let bytes = t.encode(opts).unwrap();
    for e in read_tar(&bytes) {
        assert!(e.mtime <= 1, "{}", e.name());
    }
    assert!(read_tar(&bytes).iter().any(|e| e.mtime == -1));
    // Any later time gives the same bytes.
    for e in t.entries.iter_mut().filter(|e| e.mtime > 1) {
        e.mtime += 1000;
    }
    for p in t.source.parents.values_mut() {
        p.mtime = 5;
    }
    assert_eq!(t.encode(opts).unwrap(), bytes);
}

#[test]
fn an_empty_layer_is_the_end_of_archive_marker() {
    assert_eq!(Tree::default().encode(plain()).unwrap(), vec![0; 1024]);
}

#[test]
fn a_lone_inode_is_written_as_a_regular_file() {
    // A hardlink whose other name is in a lower layer.
    let mut t = Tree::default();
    t.file("x", b"data").inode = Some((3, 9));
    let e = read_tar(&t.encode(plain()).unwrap());
    assert_eq!(
        (e[0].typeflag, e[0].content.as_slice()),
        (b'0', &b"data"[..])
    );
}

#[test]
fn files_above_8_gib_get_a_pax_size() {
    // Streaming and hashing 8 GiB takes minutes in a debug build, so the
    // source gives 256 KiB and then fails; the header is out by then.
    /// Keeps the first bytes written.
    struct Head(Vec<u8>);
    impl Write for Head {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let room = 4096_usize.saturating_sub(self.0.len()).min(buf.len());
            self.0.extend_from_slice(&buf[..room]);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    struct Fails;
    impl io::Read for Fails {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("stop"))
        }
    }
    struct ZerosThenFail;
    impl rnk_layer::EntrySource for ZerosThenFail {
        fn open(&mut self, _: &rnk_layer::Entry) -> io::Result<Box<dyn io::Read + '_>> {
            Ok(Box::new(io::Read::chain(
                io::Read::take(io::repeat(0), 256 << 10),
                Fails,
            )))
        }
        fn parent(&mut self, _: &rnk_layer::EntryPath) -> io::Result<Option<rnk_layer::Entry>> {
            Ok(None)
        }
    }

    let size = (8 << 30) + 1;
    let mut writer = LayerWriter::new(plain()).unwrap();
    writer.add(entry("big", EntryKind::File { size })).unwrap();
    let mut head = Head(Vec::new());
    let err = writer.finish(&mut ZerosThenFail, &mut head).unwrap_err();
    assert!(matches!(err, Error::Source { .. }), "{err}");

    let mut archive = tar::Archive::new(&head.0[..]);
    let mut entries = archive.entries().unwrap();
    let first = entries.next().unwrap().unwrap();
    assert_eq!(first.path_bytes().as_ref(), b"big");
    assert_eq!(first.size(), size);
    assert_eq!(first.header().size().unwrap(), 0, "the ustar field holds 0");
}

#[test]
fn a_parent_unknown_to_the_source_is_an_error() {
    let mut t = Tree::default();
    t.file("a/b", b"x");
    assert!(matches!(t.encode(plain()), Err(Error::MissingParent(p)) if p == path("a")));
}

#[test]
fn a_parent_that_is_not_a_directory_is_an_error() {
    let mut t = Tree::default();
    t.file("a", b"x");
    t.file("a/b", b"x");
    let err = t.encode(plain()).unwrap_err();
    assert!(
        matches!(err, Error::ParentNotDirectory { ref parent, .. } if *parent == path("a")),
        "{err}"
    );

    let mut t = Tree::default();
    t.add(entry("a", EntryKind::Whiteout));
    t.file("a/b", b"x");
    assert!(matches!(
        t.encode(plain()),
        Err(Error::ParentNotDirectory { .. })
    ));
}

#[test]
fn the_parent_lookup_must_give_a_directory() {
    let mut t = Tree::default();
    t.file("a/b", b"x");
    t.source
        .parents
        .insert(path("a"), entry("a", EntryKind::Fifo));
    assert!(matches!(t.encode(plain()), Err(Error::InvalidEntry { .. })));
}

#[test]
fn a_whiteout_and_an_entry_cannot_share_a_path() {
    let mut writer = LayerWriter::new(plain()).unwrap();
    writer.add(entry("a", EntryKind::Whiteout)).unwrap();
    let err = writer.add(entry("a", EntryKind::Dir)).unwrap_err();
    assert!(matches!(err, Error::DuplicatePath(p) if p == path("a")));
    assert_eq!(writer.len(), 1);
}

#[test]
fn names_reserved_for_whiteouts_are_rejected() {
    let mut writer = LayerWriter::new(plain()).unwrap();
    let err = writer
        .add(entry("d/.wh.x", EntryKind::File { size: 0 }))
        .unwrap_err();
    assert!(matches!(err, Error::InvalidEntry { .. }), "{err}");
    let mut bad = entry("f", EntryKind::File { size: 0 });
    bad.xattrs.insert(b"user.a=b".to_vec(), Vec::new());
    assert!(writer.add(bad).is_err());
    assert!(
        writer
            .add(entry("l", EntryKind::Symlink(Vec::new())))
            .is_err()
    );
    assert!(writer.is_empty());
}

#[test]
fn content_must_match_the_announced_size() {
    let mut t = Tree::default();
    t.file("f", b"abc");
    t.entries[0].kind = EntryKind::File { size: 5 };
    let err = t.encode(plain()).unwrap_err();
    assert!(
        matches!(
            err,
            Error::ContentShorter {
                expected: 5,
                actual: 3,
                ..
            }
        ),
        "{err}"
    );
    t.entries[0].kind = EntryKind::File { size: 2 };
    let err = t.encode(plain()).unwrap_err();
    assert!(
        matches!(err, Error::ContentLonger { expected: 2, .. }),
        "{err}"
    );
}

#[test]
fn source_errors_name_the_entry() {
    let mut t = Tree::default();
    t.add(entry("f", EntryKind::File { size: 1 }));
    t.source = MemSource::default();
    let err = t.encode(plain()).unwrap_err();
    assert!(
        matches!(err, Error::Source { ref path, .. } if *path == crate::support::path("f")),
        "{err}"
    );
}
