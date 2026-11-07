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
        // Under the lock builds take, the marker first: what an interrupted
        // removal leaves is unmarked, and a directory without it stays.
        let d = shell_word(dir.as_os_str());
        let script = r#"'rm "$1/.ranikust-state" && rm -rf --one-file-system "$1"'"#;
        let want = format!("test -d {d} && flock -xn {d} sh -c {script} sh {d}");
        assert!(refusal.guidance.contains(&want), "{refusal}");
        assert_eq!(refusal.kind, Kind::Other);
    }
    assert_eq!(listing(&state), before);
    assert!(!missing.exists());
    // The path the user gave, quoted.
    let odd = Path::new("/srv/the 'state'");
    let refusal = prune_all(0, odd).unwrap_err();
    assert!(
        refusal
            .guidance
            .contains(r"test -d '/srv/the '\''state'\''' && flock -xn '/srv/the '\''state'\'''"),
        "{refusal}"
    );
}

#[test]
fn without_a_user_namespace_the_marker_is_renamed_before_anything_goes() {
    // Builds refuse the directory meanwhile; its marker goes once nothing
    // else is left, or a later prune finishes it.
    let dir = Path::new("/home/u/my state");
    let guidance = no_user_namespace(dir);
    assert!(
        guidance.contains("test -d '/home/u/my state' && flock -xn '/home/u/my state' sh -c 'mv "),
        "{guidance}"
    );
    assert!(
        guidance.contains("' sh '/home/u/my state' renames the marker"),
        "{guidance}"
    );
    let at = |what: &str| guidance.find(what);
    let renamed = at(r#"mv "$1/.ranikust-state" "$1/.ranikust-pruning" && "#);
    let removed = at(
        r#"find "$1" -mindepth 1 -maxdepth 1 ! -name .ranikust-pruning -exec rm -rf --one-file-system {} + && "#,
    );
    let marker = at(r#"rm "$1/.ranikust-pruning"'"#);
    assert!(
        matches!((renamed, removed, marker), (Some(m), Some(r), Some(l)) if m < r && r < l),
        "{guidance}"
    );
    assert!(guidance.contains("subordinate ids stay"));
}

/// A state directory's path, as one word bash reads back byte for byte;
/// nothing in the word is left for a terminal to act on.
#[test]
fn a_path_is_quoted_for_a_shell() {
    use std::os::unix::ffi::OsStrExt as _;
    let cases: [&[u8]; 9] = [
        b"/var/lib/ranikust",
        b"/home/u/.local/share/ranikust",
        b"/srv/a b",
        b"/srv/it's",
        b"/srv/$HOME/`id`/*/!x;y|z&",
        b"/srv/\xc3\xa9t\xc3\xa9",
        b"/srv/\x1b]0;x\x07\n",
        b"/srv/\xff\\'",
        "/srv/\u{202e}x".as_bytes(),
    ];
    for bytes in cases {
        let path = OsStr::from_bytes(bytes);
        let word = shell_word(path);
        assert_eq!(&*rnk_metrics::terminal_text(&word), word, "{word}");
        let out = std::process::Command::new("bash")
            .args(["-c", &format!("printf %s {word}")])
            .output()
            .unwrap();
        assert!(out.status.success(), "{word}: {out:?}");
        assert_eq!(out.stdout, bytes, "{word}");
    }
    assert_eq!(
        shell_word(OsStr::new("/var/lib/ranikust")),
        "/var/lib/ranikust"
    );
    assert_eq!(shell_word(OsStr::new("/srv/it's")), r"'/srv/it'\''s'");
    assert_eq!(shell_word(OsStr::from_bytes(b"/a\nb")), r"$'/a\x0ab'");
}

/// The commands the guidance gives run under the `flock` a build holds
/// shared: refused while a store is open, run once it is closed.
#[test]
fn the_suggested_commands_wait_for_no_build() {
    let tmp = private_tempdir();
    let state = tmp.path().join("my 'state'");
    marked_state(&state);
    let ran = state.join("ran");
    let command = locked(&state, r#"touch "$1/ran""#);
    let run = || {
        std::process::Command::new("bash")
            .args(["-c", &command])
            .status()
            .unwrap()
    };
    let open = Store::open(&rnk_store::BuildPaths::new(&state, &state)).unwrap();
    assert!(!run().success(), "{command}");
    assert!(!ran.exists());
    open.close().unwrap();
    assert!(run().success(), "{command}");
    assert!(ran.exists());
    // Nothing is made at a missing path, where `flock` would make a file.
    let missing = tmp.path().join("missing");
    let command = locked(&missing, "true");
    let status = std::process::Command::new("bash")
        .args(["-c", &command])
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(!missing.exists());
}

/// A prune that did not finish names its causes and the ways out, with the
/// state directory quoted: in `prune --all`'s refusal and a build's.
#[test]
fn a_prune_that_did_not_finish_names_the_ways_out() {
    let dir = Path::new("/home/u/my state");
    let ways = unfinished(dir);
    for want in [
        "on another mount",
        "subordinate ids",
        "unmount anything below '/home/u/my state'",
        "install newuidmap and give this user ranges in /etc/subuid and /etc/subgid",
        "mv -T '/home/u/my state' '/home/u/my state.old'",
        "podman unshare rm -rf --one-file-system '/home/u/my state.old', or as root",
    ] {
        assert!(ways.contains(want), "{want}: {ways}");
    }
    let refusal = store_refusal(&rnk_store::Error::NotPruned {
        path: dir.to_owned(),
        left: "\"x\"".to_owned(),
    });
    assert_eq!(refusal.guidance, ways);
    assert_eq!(refusal.kind, Kind::Broken);
    assert!(
        refusal.to_string().ends_with(&format!("\n  {ways}")),
        "{refusal}"
    );
    let refusal = store_refusal(&rnk_store::Error::InUse {
        path: dir.to_owned(),
    });
    assert_eq!(refusal.guidance, "");
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

/// An empty state directory is left as it is, before any namespace: no
/// marker, no `builds/`, its mode kept; and the checks that let `prune`
/// on to the namespace change no mode either.
#[test]
fn an_empty_state_directory_is_left_as_it_is() {
    let tmp = private_tempdir();
    let dir = tmp.path().join("state");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let before = listing(tmp.path());
    let user_ns = fs::read_link("/proc/self/ns/user").unwrap();
    assert_eq!(empty(&dir).unwrap(), Pruned::Empty);
    assert_eq!(listing(tmp.path()), before, "modes included");
    assert_eq!(fs::read_link("/proc/self/ns/user").unwrap(), user_ns);
    marked_state(&dir);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let before = listing(tmp.path());
    assert_eq!(statedir::check(&dir).unwrap(), Found::Entries);
    assert_eq!(listing(tmp.path()), before, "modes included");
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
