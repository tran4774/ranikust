//! The `rnk_rootfs.*` spans the metrics report reads (ADR-0025,
//! `design/metrics.md`).
//!
//! A test binary of its own, and every test creates its [`Capture`] before
//! it reaches a span: tracing caches whether a span is wanted once per
//! process, and a test reaching it with no subscriber alive could race a
//! `Capture` into caching "never".

use std::fs;
use std::path::PathBuf;

use rnk_metrics::Capture;
use rnk_rootfs::{CopyOpts, IgnoreList, RootPath, Whiteouts, copy, delete_contents, extract};

/// A fresh root below a temp dir, removed on drop.
fn root() -> (tempfile::TempDir, PathBuf, RootPath) {
    let tmp = tempfile::tempdir().unwrap();
    let host = fs::canonicalize(tmp.path()).unwrap();
    let root = RootPath::open(&host).unwrap();
    (tmp, host, root)
}

/// A layer with two files (5 and 3 bytes), a directory, a hardlink and a
/// whiteout.
fn layer() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut add = |name: &str, t: tar::EntryType, data: &[u8], link: Option<&str>| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(t);
        h.set_mode(0o755);
        h.set_uid(rustix::process::getuid().as_raw().into());
        h.set_gid(rustix::process::getgid().as_raw().into());
        h.set_mtime(0);
        h.set_size(data.len() as u64);
        if let Some(l) = link {
            h.set_link_name(l).unwrap();
        }
        b.append_data(&mut h, name, data).unwrap();
    };
    add("d/", tar::EntryType::Directory, b"", None);
    add("d/a", tar::EntryType::Regular, b"hello", None);
    add("b", tar::EntryType::Regular, b"abc", None);
    add("d/h", tar::EntryType::Link, b"", Some("d/a"));
    add("d/.wh.gone", tar::EntryType::Regular, b"", None);
    b.into_inner().unwrap()
}

#[test]
fn extract_counts_entries_kinds_and_bytes() {
    let capture = Capture::new();
    let (_tmp, _host, root) = root();
    let tar = layer();
    capture.run(|| extract(&root, tar.as_slice(), Whiteouts::Overlay).unwrap());
    let report = capture.report();
    let x = &report["phases"]["rnk_rootfs.extract"];
    assert_eq!(x["count"], 1);
    assert_eq!(x["entries"], 5);
    assert_eq!(x["files"], 2);
    assert_eq!(x["hardlinks"], 1);
    assert_eq!(x["whiteouts"], 1);
    assert_eq!(x["bytes_written"], 8);
    // Every header and data block, up to the end-of-archive marker.
    let bytes_in = x["bytes_in"].as_u64().unwrap();
    assert!(bytes_in >= 5 * 512 + 2 * 512, "{bytes_in}");
    assert!(bytes_in <= tar.len() as u64, "{bytes_in}");
}

#[test]
fn copy_counts_files_dirs_and_bytes() {
    let capture = Capture::new();
    let (_s, shost, src) = root();
    let (_d, _dhost, dst) = root();
    fs::create_dir_all(shost.join("a/sub")).unwrap();
    fs::write(shost.join("a/one"), b"12345").unwrap();
    fs::write(shost.join("a/sub/two"), b"123").unwrap();
    capture.run(|| copy(&src, "a", &dst, "x/app", &CopyOpts::default()).unwrap());
    let report = capture.report();
    let c = &report["phases"]["rnk_rootfs.copy"];
    assert_eq!(c["count"], 1);
    assert_eq!(c["files"], 2);
    // `x` and `app` created, `sub` copied.
    assert_eq!(c["dirs"], 3);
    assert_eq!(c["bytes"], 8);
}

#[test]
fn delete_counts_entries() {
    let capture = Capture::new();
    let (_tmp, host, root) = root();
    fs::create_dir_all(host.join("a/b")).unwrap();
    fs::write(host.join("a/b/f"), b"").unwrap();
    fs::write(host.join("g"), b"").unwrap();
    capture.run(|| delete_contents(&root, &IgnoreList::new()).unwrap());
    let report = capture.report();
    let d = &report["phases"]["rnk_rootfs.delete"];
    assert_eq!(d["count"], 1);
    assert_eq!(d["entries"], 4);
}
