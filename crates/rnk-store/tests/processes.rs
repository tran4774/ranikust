//! Per-digest locks between processes: the test binary runs itself as
//! several builds sharing one store.
//!
//! A test binary of its own: while it spawns children, each fork holds a
//! copy of every descriptor until `exec` closes it, a lock included, so a
//! clean-up test running beside it could see a dead build as alive.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use rnk_oci::Digest;
use rnk_store::{BuildPaths, EntryKey, Store};

/// Environment variables of [`child_fill`].
const CHILD_STATE: &str = "RNK_STORE_TEST_CHILD_STATE";
const CHILD_LOG: &str = "RNK_STORE_TEST_CHILD_LOG";

fn names(dir: impl AsRef<Path>) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect()
}

/// Not a test on its own: the body of one process in
/// [`concurrent_fills_from_processes_run_the_filler_once`].
#[test]
fn child_fill() {
    let (Some(state), Some(log)) = (std::env::var_os(CHILD_STATE), std::env::var_os(CHILD_LOG))
    else {
        return;
    };
    let state = PathBuf::from(state);
    let store = Store::open(&BuildPaths::new(&state, &state)).unwrap();
    let key = EntryKey::Layer(Digest::from_sha256_bytes(&[5; 32]));
    let p = store
        .fill(&key, |e| -> io::Result<u64> {
            let mut l = fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&log)?;
            writeln!(l, "filled")?;
            thread::sleep(Duration::from_millis(200));
            e.root()
                .unwrap()
                .create_dir("done", 0o755)
                .map_err(io::Error::other)?;
            Ok(0)
        })
        .unwrap();
    assert!(p.join("done").is_dir());
    store.close().unwrap();
}

#[test]
fn concurrent_fills_from_processes_run_the_filler_once() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let log = dir.path().join("log");
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..4)
        .map(|_| {
            Command::new(&exe)
                .args(["--exact", "child_fill", "--test-threads=1", "--quiet"])
                .env(CHILD_STATE, &state)
                .env(CHILD_LOG, &log)
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    assert_eq!(fs::read_to_string(&log).unwrap(), "filled\n");
    // Every child closed its build: nothing is left in builds/ or tmp/.
    assert!(names(state.join("tmp")).is_empty());
    assert!(names(state.join("builds")).is_empty());
}

/// Environment variable of [`child_killed_mid_fill`].
const CHILD_HANG: &str = "RNK_STORE_TEST_CHILD_HANG";

/// Not a test on its own: a build that is killed while it fills a layer,
/// in [`a_build_killed_mid_fill_leaves_nothing_behind`].
#[test]
fn child_killed_mid_fill() {
    let (Some(state), Some(log)) = (std::env::var_os(CHILD_HANG), std::env::var_os(CHILD_LOG))
    else {
        return;
    };
    let state = PathBuf::from(state);
    let store = Store::open(&BuildPaths::new(&state, &state)).unwrap();
    fs::create_dir_all(store.build_dir().join("stages/0/diff-1/etc")).unwrap();
    let key = EntryKey::Layer(Digest::from_sha256_bytes(&[6; 32]));
    let _ = store.fill(&key, |e| -> io::Result<u64> {
        e.root()
            .unwrap()
            .create_dir_all("usr/lib", 0o755)
            .map_err(io::Error::other)?;
        fs::write(&log, "inside\n")?;
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    });
}

#[test]
fn a_build_killed_mid_fill_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let log = dir.path().join("log");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_killed_mid_fill",
            "--test-threads=1",
            "--quiet",
        ])
        .env(CHILD_HANG, &state)
        .env(CHILD_LOG, &log)
        .spawn()
        .unwrap();
    while fs::read_to_string(&log).unwrap_or_default() != "inside\n" {
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap(); // SIGKILL
    child.wait().unwrap();

    let store = Store::open(&BuildPaths::new(&state, &state)).unwrap();
    let id = store.build_id();
    let mut builds = names(state.join("builds"));
    builds.sort();
    let mut live = vec![id.to_owned(), format!("{id}.lock")];
    live.sort();
    assert_eq!(builds, live);
    assert_eq!(names(state.join("tmp")), [id]);
    // The killed fill published nothing; its digest's lock is free again.
    let key = EntryKey::Layer(Digest::from_sha256_bytes(&[6; 32]));
    assert_eq!(store.get(&key).unwrap(), None);
    store.fill(&key, |_| Ok::<_, io::Error>(0)).unwrap();
}
