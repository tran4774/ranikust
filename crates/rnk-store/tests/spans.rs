//! The `rnk_store.*` spans the metrics report reads (ADR-0025).
//!
//! A test binary of its own, and every test creates its [`Capture`] before
//! it reaches a span: tracing caches whether a span is wanted once per
//! process, and a test reaching it with no subscriber alive could race a
//! `Capture` into caching "never".

use std::fs;
use std::io::{self, Write as _};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

use rnk_metrics::Capture;
use rnk_oci::Digest;
use rnk_store::{BuildPaths, EntryKey, Store};

fn store(dir: &std::path::Path) -> Store {
    Store::open(&BuildPaths::new(dir, dir.join("state"))).unwrap()
}

#[test]
fn fill_counts_written_and_reused_bytes() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    let store = capture.run(|| store(dir.path()));
    let key = EntryKey::Blob(Digest::from_sha256_bytes(&[1; 32]));
    capture.run(|| {
        for _ in 0..2 {
            store
                .fill(&key, |e| -> io::Result<u64> {
                    e.file().unwrap().write_all(b"12345")?;
                    Ok(5)
                })
                .unwrap();
        }
    });
    let report = capture.report();
    let fill = &report["phases"]["rnk_store.fill"];
    assert_eq!(fill["count"], 2);
    assert_eq!(fill["reused"], 1);
    assert_eq!(fill["bytes_written"], 5);
    assert_eq!(fill["bytes_reused"], 5);
    let lock = &report["phases"]["rnk_store.lock"];
    assert_eq!(lock["count"], 2);
    assert_eq!(lock["contended"], 0);
    assert_eq!(report["phases"]["rnk_store.cleanup"]["count"], 1);
}

#[test]
fn a_waiting_lock_is_contended() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store(dir.path()));
    let key = EntryKey::Layer(Digest::from_sha256_bytes(&[2; 32]));
    let (inside, entered) = mpsc::channel();
    let release = Arc::new(Barrier::new(2));
    let holder = {
        let (store, key, release) = (store.clone(), key.clone(), release.clone());
        thread::spawn(move || {
            store
                .fill(&key, |_| -> io::Result<u64> {
                    inside.send(()).unwrap();
                    release.wait();
                    Ok(0)
                })
                .unwrap();
        })
    };
    entered.recv().unwrap();
    let waiter = {
        let (store, key, dispatch) = (store.clone(), key.clone(), capture.dispatch().clone());
        thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                store
                    .fill(&key, |_| -> io::Result<u64> { panic!("filled twice") })
                    .unwrap()
            })
        })
    };
    thread::sleep(Duration::from_millis(100));
    release.wait();
    holder.join().unwrap();
    let p = waiter.join().unwrap();
    assert!(fs::metadata(p).unwrap().is_dir());
    let report = capture.report();
    assert_eq!(report["phases"]["rnk_store.lock"]["contended"], 1);
    assert_eq!(report["phases"]["rnk_store.fill"]["reused"], 1);
}

#[test]
fn cleanup_counts_removed_builds() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    let dead = store(dir.path());
    fs::create_dir_all(dead.build_dir().join("stages/0")).unwrap();
    drop(dead);
    capture.run(|| store(dir.path()));
    let cleanup = &capture.report()["phases"]["rnk_store.cleanup"];
    // The dead build's directory in builds/ and in tmp/.
    assert_eq!(cleanup["entries_removed"], 2);
}
