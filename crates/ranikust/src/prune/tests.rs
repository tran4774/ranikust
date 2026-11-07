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

/// A state directory a build left: the store's marker and a blob. By
/// hand: a store opened in this process would leave its lock with a child
/// another test spawns meanwhile, until that child runs its program.
fn marked_state(state: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state.join("blobs/sha256"))
        .unwrap();
    fs::write(state.join("blobs/sha256").join("07".repeat(32)), b"blob").unwrap();
    fs::write(state.join(rnk_store::STATE_MARKER), b"").unwrap();
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
        // In the directory entered once, under the lock builds take, the
        // marker first: what an interrupted removal leaves is unmarked, and
        // a directory without one stays.
        let d = shell_word(dir.as_os_str());
        let script = "'{ test -f .ranikust-state && ! test -h .ranikust-state && rm .ranikust-state || { test -f .ranikust-pruning && ! test -h .ranikust-pruning && rm .ranikust-pruning; }; } && find . -xdev -mindepth 1 -delete'";
        let want =
            format!("( CDPATH= cd -P -- {d} && exec flock -xn . sh -c {script} ), then rmdir {d}");
        assert!(refusal.guidance.contains(&want), "{refusal}");
        assert!(refusal.guidance.contains("nothing is mounted below it"));
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
            .contains(r"( CDPATH= cd -P -- '/srv/the '\''state'\''' && exec flock -xn . "),
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
        guidance.contains("( CDPATH= cd -P -- '/home/u/my state' && exec flock -xn . sh -c '{ "),
        "{guidance}"
    );
    assert!(guidance.contains("' ) renames its marker"), "{guidance}");
    let at = |what: &str| guidance.find(what);
    let renamed = at(
        "! test -e .ranikust-pruning && ! test -h .ranikust-pruning && test -f .ranikust-state \
         && ! test -h .ranikust-state && mv -T .ranikust-state .ranikust-pruning; }; } && ",
    );
    let removed = at(r#"find . -xdev -mindepth 1 ! -path ./.ranikust-pruning -delete && "#);
    let marker = at(
        r#"test "$(find . -mindepth 1 -maxdepth 1)" = ./.ranikust-pruning && rm .ranikust-pruning'"#,
    );
    assert!(
        matches!((renamed, removed, marker), (Some(m), Some(r), Some(l)) if m < r && r < l),
        "{guidance}"
    );
    assert!(guidance.contains("nothing is mounted below the state directory"));
    assert!(guidance.contains("subordinate ids stay"));
}

/// `script` on the state directory `state` as the guidance gives it, run
/// by bash from `cwd`; with `bin` first on `PATH` (the machine's own as
/// `REAL_PATH`) when given, and `env` set.
fn run_script_with(
    script: &str,
    cwd: &Path,
    state: &Path,
    bin: Option<&Path>,
    env: &[(&str, &Path)],
) -> std::process::ExitStatus {
    let mut command = std::process::Command::new("bash");
    command
        .args(["-c", &pinned(state, script)])
        .current_dir(cwd)
        .envs(env.iter().copied());
    if let Some(bin) = bin {
        let path = std::env::var_os("PATH").expect("PATH");
        let mut first = bin.as_os_str().to_owned();
        first.push(":");
        first.push(&path);
        command.env("PATH", first).env("REAL_PATH", path);
    }
    command.status().unwrap()
}

/// [`run_script_with`] on the machine's tools.
fn run_script(script: &str, cwd: &Path, state: &Path) -> std::process::ExitStatus {
    run_script_with(script, cwd, state, None, &[])
}

/// Puts in `bin` a `tool` that runs the machine's, then, the first time,
/// moves the directory `$SWAP` to `$SWAP.moved` and puts a symlink to
/// `$TO` at its path: what another process could do between two steps.
fn swap_after(bin: &Path, tool: &str) {
    fs::create_dir(bin).unwrap();
    let shim = bin.join(tool);
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nPATH=$REAL_PATH {tool} \"$@\" || exit\ntest -e \"$SWAP.moved\" && exit\n\
             PATH=$REAL_PATH mv \"$SWAP\" \"$SWAP.moved\" && PATH=$REAL_PATH ln -s \"$TO\" \"$SWAP\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The commands act on the directory they locked: one swapped for a
/// symlink between two of their steps is emptied where it now is, and the
/// symlink's target is left as it is.
#[test]
fn the_suggested_scripts_stay_in_the_directory_they_locked() {
    let tmp = private_tempdir();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"x").unwrap();
    for (script, tool) in [(ROOT_SCRIPT, "rm"), (USER_SCRIPT, "mv")] {
        let bin = tmp.path().join(format!("bin-{tool}"));
        swap_after(&bin, tool);
        let state = tmp.path().join("my 'state' $x");
        let moved = tmp.path().join("my 'state' $x.moved");
        marked_state(&state);
        let env = [("SWAP", state.as_path()), ("TO", outside.as_path())];
        let ran = run_script_with(script, tmp.path(), &state, Some(&bin), &env);
        assert!(outside.join("keep").is_file(), "{tool}");
        assert!(
            fs::symlink_metadata(&state).unwrap().is_symlink(),
            "{tool}: not swapped"
        );
        assert!(ran.success(), "{tool}");
        assert_eq!(listing(&moved), Vec::new(), "{tool}");
        fs::remove_file(&state).unwrap();
        fs::remove_dir(&moved).unwrap();
    }
}

/// Gives a directory made read-only back its mode, even when the test
/// fails, so the temp dir can be removed.
struct Writable<'a>(&'a Path);

impl Drop for Writable<'_> {
    fn drop(&mut self) {
        if let Err(e) = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755)) {
            eprintln!("restoring the mode of {}: {e}", self.0.display());
        }
    }
}

/// The scripts the guidance gives, run as they are on a path a shell must
/// quote: each empties a marked state directory, not following a symlink,
/// the user's its marker last and again after a run cut short; neither
/// removes anything from a directory without the marker.
#[test]
fn the_suggested_scripts_empty_only_a_marked_state_directory() {
    let tmp = private_tempdir();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"x").unwrap();
    let fill = |state: &Path| {
        marked_state(state);
        fs::create_dir_all(state.join("builds/b/deep/er")).unwrap();
        fs::write(state.join("builds/b/deep/er/f"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, state.join("builds/b/link")).unwrap();
    };
    for (script, renamed_before) in [
        (ROOT_SCRIPT, false),
        (ROOT_SCRIPT, true),
        (USER_SCRIPT, false),
        (USER_SCRIPT, true),
    ] {
        let state = tmp.path().join("my 'state' $x");
        fill(&state);
        if renamed_before {
            fs::rename(
                state.join(".ranikust-state"),
                state.join(".ranikust-pruning"),
            )
            .unwrap();
        }
        assert!(run_script(script, tmp.path(), &state).success(), "{script}");
        assert_eq!(listing(&state), Vec::new(), "{script}");
        assert!(outside.join("keep").is_file(), "{script}");
        fs::remove_dir(&state).unwrap();
        // Unmarked, or marked only by a symlink: refused before anything
        // goes.
        for marker in [".ranikust-state", ".ranikust-pruning"] {
            fs::create_dir(&state).unwrap();
            fs::write(state.join("keep"), b"x").unwrap();
            assert!(
                !run_script(script, tmp.path(), &state).success(),
                "{script}"
            );
            assert!(state.join("keep").is_file(), "{script}");
            std::os::unix::fs::symlink(outside.join("keep"), state.join(marker)).unwrap();
            assert!(
                !run_script(script, tmp.path(), &state).success(),
                "{script} {marker}"
            );
            assert!(state.join("keep").is_file(), "{script} {marker}");
            assert!(state.join(marker).is_symlink(), "{script} {marker}");
            assert!(outside.join("keep").is_file(), "{script} {marker}");
            fs::remove_dir_all(&state).unwrap();
        }
    }
    // The user's marker is never moved into a `.ranikust-pruning` that is
    // not a regular file.
    let state = tmp.path().join("state");
    fill(&state);
    fs::create_dir(state.join(".ranikust-pruning")).unwrap();
    assert!(!run_script(USER_SCRIPT, tmp.path(), &state).success());
    assert!(state.join(".ranikust-state").is_file());
    assert!(state.join("blobs").is_dir());
    fs::remove_dir_all(&state).unwrap();
    // What the user may not remove stays, and the marker with it, whether
    // or not `find` fails when a removal does; root may remove it.
    if rustix::process::geteuid().as_raw() == 0 {
        return;
    }
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("find"),
        "#!/bin/sh\nPATH=$REAL_PATH find \"$@\"\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("find"), fs::Permissions::from_mode(0o755)).unwrap();
    for find_fails in [true, false] {
        let state = tmp.path().join("my 'state' $x");
        fill(&state);
        let kept = state.join("builds/b/deep");
        let writable = Writable(&kept);
        fs::set_permissions(&kept, fs::Permissions::from_mode(0o555)).unwrap();
        let shim = (!find_fails).then_some(bin.as_path());
        let ran = run_script_with(USER_SCRIPT, tmp.path(), &state, shim, &[]).success();
        drop(writable);
        assert!(!ran, "find fails: {find_fails}");
        assert!(!state.join("blobs").exists(), "find fails: {find_fails}");
        assert!(kept.join("er").is_dir(), "find fails: {find_fails}");
        assert!(
            state.join(".ranikust-pruning").is_file(),
            "find fails: {find_fails}"
        );
        fs::remove_dir_all(&state).unwrap();
    }
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
    let command = pinned(&state, "touch ran");
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
    let command = pinned(&missing, "true");
    let status = std::process::Command::new("bash")
        .args(["-c", &command])
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(!missing.exists());
}

/// A prune that could not finish names its causes and the ways out, with
/// the state directory quoted; no other refusal of the store gets them.
#[test]
fn a_prune_that_could_not_finish_names_the_ways_out() {
    let dir = Path::new("/home/u/my state");
    let ways = unfinished(dir);
    for want in [
        "on another mount",
        "subordinate ids",
        "unmount anything below '/home/u/my state'",
        "with newuidmap installed and ranges for this user in /etc/subuid and /etc/subgid",
        "( CDPATH= cd -P -- /home/u && mv -T -- 'my state' 'my state.old' && podman unshare rm \
         -rf --one-file-system -- 'my state.old' ), or the same as root",
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
    for e in [
        rnk_store::Error::InUse {
            path: dir.to_owned(),
        },
        rnk_store::Error::PruneCutShort {
            path: dir.to_owned(),
        },
    ] {
        assert_eq!(store_refusal(&e).guidance, "", "{e}");
    }
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
    // Prune's advice, not a build's.
    assert!(
        refusal.reason.contains("empties only a state directory"),
        "{refusal}"
    );
    assert!(!refusal.reason.contains("--state-dir"), "{refusal}");
    assert_eq!(refusal.kind, Kind::Other);
    assert_eq!(
        listing(tmp.path()),
        before,
        "nothing changed, modes included"
    );
    assert_eq!(fs::read_link("/proc/self/ns/user").unwrap(), user_ns);
}

/// A marked directory holding a name the store does not make is refused
/// before the namespace, as the store refuses it, so no guidance for a
/// user without one is given for it either.
#[test]
fn a_name_the_store_does_not_make_is_refused_before_the_namespace() {
    let tmp = private_tempdir();
    let dir = tmp.path().join("state");
    marked_state(&dir);
    fs::write(dir.join("notes"), b"x").unwrap();
    let before = listing(tmp.path());
    let user_ns = fs::read_link("/proc/self/ns/user").unwrap();
    let refusal = empty(&dir).unwrap_err();
    assert!(
        refusal
            .reason
            .contains(r#"holds "notes", which ranikust does not make there"#),
        "{refusal}"
    );
    assert_eq!(refusal.guidance, "");
    assert_eq!(listing(tmp.path()), before);
    assert_eq!(fs::read_link("/proc/self/ns/user").unwrap(), user_ns);
    // Nor a `.ranikust-pruning` that is not a regular file, beside a marker
    // that is.
    fs::remove_file(dir.join("notes")).unwrap();
    fs::create_dir(dir.join(".ranikust-pruning")).unwrap();
    let before = listing(tmp.path());
    let refusal = empty(&dir).unwrap_err();
    assert!(
        refusal
            .reason
            .contains(".ranikust-pruning, which is not a regular file"),
        "{refusal}"
    );
    assert_eq!(refusal.guidance, "");
    assert_eq!(refusal.kind, Kind::Other);
    assert_eq!(listing(tmp.path()), before);
    assert_eq!(fs::read_link("/proc/self/ns/user").unwrap(), user_ns);
    fs::remove_dir(dir.join(".ranikust-pruning")).unwrap();
    // Every name the store makes is taken.
    fs::create_dir_all(dir.join(rnk_store::SANDBOX_DIR).join("runs")).unwrap();
    assert_eq!(statedir::check(&dir).unwrap(), Found::Entries);
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

/// What `prune --all` refuses outside the user namespace is what the store
/// refuses in it: on each directory, [`statedir::check`] and
/// [`Store::prune_all`] both take it or both refuse it.
#[test]
fn the_checks_outside_refuse_what_the_store_refuses() {
    use std::os::unix::fs::symlink;
    let tmp = private_tempdir();
    let marker = |d: &Path| d.join(rnk_store::STATE_MARKER);
    let pruning = |d: &Path| d.join(rnk_store::PRUNING_MARKER);
    type Case<'a> = (&'a str, bool, &'a dyn Fn(&Path));
    let cases: [Case<'_>; 9] = [
        ("empty", true, &|_| {}),
        ("marked", true, &|d| marked_state(d)),
        ("cut short", true, &|d| {
            marked_state(d);
            fs::rename(marker(d), pruning(d)).unwrap();
        }),
        ("marker and .ranikust-building", false, &|d| {
            marked_state(d);
            fs::write(d.join(rnk_store::BUILDING_MARKER), b"").unwrap();
        }),
        ("anonymous names only", false, &|d| {
            fs::write(d.join(format!("{}x", rnk_store::ANON_PREFIX)), b"").unwrap();
        }),
        ("marker and a .ranikust-pruning directory", false, &|d| {
            marked_state(d);
            fs::create_dir(pruning(d)).unwrap();
        }),
        // To a regular file in the directory, which either check would
        // take if it followed the link.
        ("symlinked marker", false, &|d| {
            fs::create_dir(d.join("blobs")).unwrap();
            fs::write(d.join("blobs/x"), b"").unwrap();
            symlink("blobs/x", marker(d)).unwrap();
        }),
        ("symlinked .ranikust-pruning", false, &|d| {
            fs::create_dir(d.join("blobs")).unwrap();
            fs::write(d.join("blobs/x"), b"").unwrap();
            symlink("blobs/x", pruning(d)).unwrap();
        }),
        ("foreign name", false, &|d| {
            marked_state(d);
            fs::write(d.join("notes"), b"x").unwrap();
        }),
    ];
    for (name, taken, make) in cases {
        let dir = tmp.path().join(name.replace(' ', "-"));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        make(&dir);
        let outside = statedir::check(&dir);
        let store = Store::prune_all(&dir);
        assert_eq!(outside.is_ok(), taken, "{name}: {outside:?}");
        assert_eq!(store.is_ok(), taken, "{name}: {store:?}");
        if let (Err(outside), Err(store)) = (outside, store) {
            assert_eq!(outside.0, store.to_string(), "{name}");
        } else {
            assert!(listing(&dir).is_empty(), "{name}");
        }
    }
}
