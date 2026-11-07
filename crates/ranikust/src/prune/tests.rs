//! `prune --all` at tier T0: what is refused before the user namespace.
//! Emptying in the namespace is in `setup/tests/userns.rs` (T0 whatever the
//! machine allows, T1, T2).

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use super::*;
use crate::cli::Command;

/// A fresh temp dir no one else may write, whatever the umask.
fn private_tempdir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

/// Every name below `dir` with its mode, sorted.
fn listing(dir: &Path) -> Vec<(PathBuf, u32)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                todo.push(p.clone());
            }
            out.push((
                p.strip_prefix(dir).unwrap().to_owned(),
                m.permissions().mode(),
            ));
        }
    }
    out.sort();
    out
}

/// A state directory a build left: the store's marker and a blob.
fn marked_state(state: &Path) {
    let store = Store::open(&rnk_store::BuildPaths::new(state, state)).unwrap();
    store
        .fill(
            &rnk_store::EntryKey::Blob(rnk_oci::Digest::from_sha256_bytes(&[7; 32])),
            |e| {
                std::io::Write::write_all(&mut e.file().expect("a blob"), b"blob")?;
                Ok::<_, std::io::Error>(4)
            },
        )
        .unwrap();
    store.close().unwrap();
}

fn parse(args: &[&str]) -> PruneArgs {
    let argv = std::iter::once("ranikust").chain(args.iter().copied());
    match crate::cli::parse(argv).unwrap().cli.into_command() {
        Command::Prune(args) => args,
        Command::Build(_) | Command::Version => panic!("not prune: {args:?}"),
    }
}

#[test]
fn prune_takes_all_and_a_state_directory() {
    let args = parse(&["prune", "--all", "--state-dir", "/s"]);
    assert!(args.all);
    assert_eq!(args.state_dir.as_deref(), Some(Path::new("/s")));
    let args = parse(&["prune"]);
    assert!(!args.all);
    assert_eq!(args.state_dir, None);
    // kaniko's name for the state directory is refused here as for a build.
    let argv = ["ranikust", "prune", "--all", "--kaniko-dir=/s"];
    assert!(matches!(
        crate::cli::parse(argv),
        Err(crate::cli::ParseError::Refused(_))
    ));
}

#[test]
fn prune_without_all_is_refused_naming_phase_5() {
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    let args = PruneArgs {
        all: false,
        state_dir: Some(state.clone()),
    };
    match run(&args) {
        Err(Failed::Usage(message)) => {
            assert!(message.contains("Phase 5"), "{message}");
            assert!(message.contains("--all"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!state.exists());
}

#[test]
fn root_is_refused_naming_phase_5_before_anything() {
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    marked_state(&state);
    let before = listing(&state);
    let missing = tmp.path().join("missing");
    for dir in [&state, &missing] {
        let refusal = prune_all(0, dir).unwrap_err();
        assert!(refusal.reason.contains("refuses root"), "{refusal}");
        assert!(
            refusal.reason.contains("a container's root included"),
            "{refusal}"
        );
        assert!(refusal.guidance.contains("Phase 5"), "{refusal}");
        // The marker first: what an interrupted removal leaves is unmarked.
        let marker = refusal
            .guidance
            .find("rm -f /var/lib/ranikust/.ranikust-state");
        let rest = refusal.guidance.find("rm -rf --one-file-system");
        assert!(
            matches!((marker, rest), (Some(m), Some(r)) if m < r),
            "{refusal}"
        );
        assert_eq!(refusal.kind, Kind::Other);
    }
    assert_eq!(listing(&state), before);
    assert!(!missing.exists());
}

#[test]
fn a_missing_state_directory_is_left_missing() {
    let tmp = private_tempdir();
    for dir in [
        tmp.path().join("state"),
        tmp.path().join("no/such/parent/state"),
    ] {
        assert_eq!(empty(&dir).unwrap(), Pruned::Missing);
        assert!(!dir.exists());
    }
    assert_eq!(listing(tmp.path()), Vec::new());
}

#[test]
fn a_directory_without_the_marker_is_refused_before_the_namespace() {
    let tmp = private_tempdir();
    let dir = tmp.path().join("precious");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.join("keep"), b"x").unwrap();
    let before = listing(tmp.path());
    let user_ns = fs::read_link("/proc/self/ns/user").unwrap();
    // Refused by the checks run outside, before any namespace: this test
    // process, which has threads, could not enter one anyway.
    let refusal = empty(&dir).unwrap_err();
    assert!(
        refusal
            .reason
            .contains("is not empty and has no regular file .ranikust-state"),
        "{refusal}"
    );
    assert_eq!(refusal.kind, Kind::Other);
    assert_eq!(
        listing(tmp.path()),
        before,
        "nothing changed, modes included"
    );
    assert_eq!(fs::read_link("/proc/self/ns/user").unwrap(), user_ns);
}

#[test]
fn the_summary_names_the_state_directory() {
    let dir = Path::new("/home/u/.local/share/ranikust");
    assert_eq!(
        summary(dir, Pruned::Emptied),
        "emptied /home/u/.local/share/ranikust"
    );
    assert!(summary(dir, Pruned::Missing).contains("does not exist"));
    assert!(summary(dir, Pruned::Empty).contains("is empty"));
}
