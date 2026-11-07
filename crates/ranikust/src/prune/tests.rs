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

/// What a refusal that gives no commands says to do instead.
const OTHERWISE: &str = "give the path with no symbolic link on it; for a user's directory, run \
                         ranikust prune --all as that user";

/// The [`Pin`] of the directory at `dir`, from the standard library's
/// stat(2), whatever is there.
fn pin_of(dir: &Path) -> Pin {
    use std::os::unix::fs::MetadataExt as _;
    let m = fs::symlink_metadata(dir).unwrap();
    Pin {
        dev: m.dev(),
        ino: m.ino(),
        uid: m.uid(),
    }
}

/// Puts `bin` first on `run`'s `PATH`, the machine's own as `REAL_PATH`.
fn bin_first(run: &mut std::process::Command, bin: &Path) {
    let path = std::env::var_os("PATH").expect("PATH");
    let mut first = bin.as_os_str().to_owned();
    first.push(":");
    first.push(&path);
    run.env("PATH", first).env("REAL_PATH", path);
}

/// `command` run by bash from `cwd`, with `bin` first on `PATH` (the
/// machine's own as `REAL_PATH`) when given.
fn run_command(command: &str, cwd: &Path, bin: Option<&Path>) -> std::process::ExitStatus {
    let mut run = std::process::Command::new("bash");
    run.args(["-c", command]).current_dir(cwd);
    if let Some(bin) = bin {
        bin_first(&mut run, bin);
    }
    run.status().unwrap()
}

/// A `podman` that runs `podman unshare`'s command as it is, in no user
/// namespace, in a new `bin` below `at`.
fn podman_shim(at: &Path) -> PathBuf {
    let bin = at.join("podman-bin");
    fs::create_dir(&bin).unwrap();
    let shim = bin.join("podman");
    fs::write(
        &shim,
        "#!/bin/sh\ntest \"$1\" = unshare || exit\nshift\nexec \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[test]
fn root_is_refused_naming_phase_5_with_nothing_changed() {
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    marked_state(&state);
    let before = listing(&state);
    let empty_dir = tmp.path().join("empty");
    fs::create_dir(&empty_dir).unwrap();
    let missing = tmp.path().join("missing");
    for dir in [&state, &empty_dir, &missing] {
        let refusal = prune_all(0, dir).unwrap_err();
        assert!(refusal.reason.contains("refuses root"), "{refusal}");
        assert!(
            refusal.reason.contains("a container's root included"),
            "{refusal}"
        );
        assert!(refusal.guidance.contains("Phase 5"), "{refusal}");
        assert_eq!(refusal.kind, Kind::Other);
    }
    // In the directory entered once and checked to be the one ranikust
    // checked, under the lock builds take, the marker first: what an
    // interrupted removal leaves is unmarked, and a directory without one
    // stays.
    let refusal = prune_all(0, &state).unwrap_err();
    let Pin { dev, ino, uid } = pin_of(&state);
    let (d, p) = (
        shell_word(state.as_os_str()),
        shell_word(tmp.path().as_os_str()),
    );
    let marker_first = r#"'find --version 2>/dev/null | grep -q "GNU findutils" && find --version 2>/dev/null | grep -qF "FTS(FTS_CWDFD)" || { echo "needs GNU findutils (Alpine: apk add findutils)" >&2; exit 1; }; { test -f .ranikust-state && ! test -h .ranikust-state && rm .ranikust-state || { test -f .ranikust-pruning && ! test -h .ranikust-pruning && rm .ranikust-pruning; }; } && "#;
    // In a directory a user owns, only the entries of its uid and of its
    // subordinate range go.
    let (only, emptying) = match (uid, Owner::of(uid).range) {
        (0, _) => (String::new(), "find . -xdev -mindepth 1 -delete".to_owned()),
        (_, None) => (
            format!(
                " and deletes only the entries of uid {uid}, the directory's owner, stopping \
                 where another uid owns any"
            ),
            format!(
                r#"{{ find . -xdev -mindepth 1 \( -uid {uid} \) -delete && test -z "$(find . -mindepth 1 -maxdepth 1)" || {{ echo "not emptied: entries of uids other than {uid} are left in place, as is anything find names above" >&2; exit 1; }}; }}"#
            ),
        ),
        (_, Some(IdRange { start, count })) => (
            format!(
                " and deletes only the entries of uid {uid}, the directory's owner, and of its \
                 subordinate ids {start}-{}, stopping where another uid owns any",
                start + count - 1
            ),
            format!(
                r#"{{ find . -xdev -mindepth 1 \( -uid {uid} -o \( -uid +{} -uid -{} \) \) -delete && test -z "$(find . -mindepth 1 -maxdepth 1)" || {{ echo "not emptied: entries of uids other than {uid} and its subordinate range {start}-{} are left in place, as is anything find names above" >&2; exit 1; }}; }}"#,
                start - 1,
                start + count,
                start + count - 1
            ),
        ),
    };
    let script = format!("{marker_first}{emptying}'");
    let want = format!(
        "; the emptying needs GNU findutils (Alpine: apk add findutils){only}: ( CDPATH= cd -P -- {d} && stat -c %d:%i:%u . | grep -qxF {dev}:{ino}:{uid} && exec flock -xn . sh -c {script} ), then ( CDPATH= cd -P -- {p} && stat -c %d:%i:%u ./state | grep -qxF {dev}:{ino}:{uid} && rmdir -- ./state )"
    );
    assert!(refusal.guidance.contains(&want), "{refusal}");
    assert!(refusal.guidance.contains("nothing is mounted below it"));
    // An empty directory, which holds no marker, gets no `rmdir`: it may
    // be one the system needs, named by mistake.
    let refusal = prune_all(0, &empty_dir).unwrap_err();
    let want = format!(
        "ranikust gives no commands for {}, which is empty: it gives commands only for a \
         directory that holds its marker",
        shell_word(empty_dir.as_os_str())
    );
    assert!(refusal.guidance.contains(&want), "{refusal}");
    assert!(!refusal.guidance.contains("rmdir"), "{refusal}");
    // A refusal of the store's checks other than for a symlink gives its
    // own reason: no symlink advice, no `prune --all` to run.
    let odd = tmp.path().join("odd");
    marked_state(&odd);
    fs::create_dir(odd.join(".ranikust-pruning")).unwrap();
    let unmarked = tmp.path().join("unmarked");
    fs::create_dir(&unmarked).unwrap();
    fs::write(unmarked.join("keep"), b"x").unwrap();
    for (dir, why) in [
        (&odd, "not a regular file"),
        (&unmarked, "has no regular file .ranikust-state"),
        (&missing, "No such file or directory"),
    ] {
        let refusal = prune_all(0, dir).unwrap_err();
        let none = format!(
            "ranikust gives no commands for {}: ",
            shell_word(dir.as_os_str())
        );
        assert!(
            refusal.guidance.starts_with(&none) && refusal.guidance.contains(why),
            "{refusal}"
        );
        for not in ["symbolic link", "as that user", "again", "cd -P"] {
            assert!(!refusal.guidance.contains(not), "{not}: {refusal}");
        }
    }
    fs::remove_dir_all(&odd).unwrap();
    fs::remove_dir_all(&unmarked).unwrap();
    // A path that ends in `..`, not in the directory's name, whether it
    // leads to a state directory, to another directory or nowhere.
    for up in [state.join("blobs/.."), state.join(".."), missing.join("..")] {
        let refusal = prune_all(0, &up).unwrap_err();
        assert!(
            refusal.guidance.starts_with(&format!(
                "ranikust gives no commands for {}, which ends in ..: give the state directory's \
                 own path;",
                shell_word(up.as_os_str())
            )),
            "{refusal}"
        );
    }
    // The path the user gave, quoted.
    let quoted = tmp.path().join("the 'state'");
    let refusal = prune_all(0, &quoted).unwrap_err();
    assert!(
        refusal
            .guidance
            .starts_with("ranikust gives no commands for '")
            && refusal.guidance.contains(r"/the '\''state'\''': "),
        "{refusal}"
    );
    assert_eq!(listing(&state), before);
    assert_eq!(listing(&empty_dir), Vec::new());
    assert!(!missing.exists());
}

/// Root's path with a symlink on it, to a directory holding a regular
/// marker, gets no commands, as one whose check fails; nothing is removed
/// there.
#[test]
fn root_gets_no_commands_for_a_path_through_a_symlink() {
    let tmp = private_tempdir();
    let real = tmp.path().join("real");
    marked_state(&real.join("state"));
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let before = listing(tmp.path());
    let typed = link.join("state");
    let refusal = prune_all(0, &typed).unwrap_err();
    let cut = cut_short(&typed, 0);
    for guidance in [&*refusal.guidance, &cut] {
        assert!(!guidance.contains("cd -P"), "{guidance}");
        assert!(
            guidance.contains(&format!(
                "ranikust gives no commands for {}, which it cannot open without following a \
                 symbolic link (",
                shell_word(typed.as_os_str())
            )),
            "{guidance}"
        );
        assert!(guidance.contains(OTHERWISE), "{guidance}");
        assert!(!guidance.contains("owner"), "{guidance}");
    }
    assert_eq!(listing(tmp.path()), before);
}

/// The pin ranikust takes is what `stat -c %d:%i:%u` prints for the
/// directory, so each printed command runs with it: root's emptying, of a
/// directory root owns and of one its owner's, and `rmdir`, the user's,
/// and the move aside.
#[test]
fn the_printed_commands_run_on_the_directory_ranikust_checked() {
    let tmp = private_tempdir();
    let state = tmp.path().join("my 'state' $x");
    marked_state(&state);
    let (taken, entries) = pin(&state).unwrap();
    assert!(entries);
    assert_eq!(taken, pin_of(&state));
    let out = std::process::Command::new("stat")
        .args(["-c", "%d:%i:%u"])
        .arg(&state)
        .output()
        .unwrap();
    let Pin { dev, ino, uid } = taken;
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{dev}:{ino}:{uid}\n")
    );
    let scripts = [
        ROOT_SCRIPT.to_owned(),
        root_script(Owner::of(uid)),
        USER_SCRIPT.to_owned(),
    ];
    for (i, script) in scripts.iter().enumerate() {
        if i > 0 {
            marked_state(&state);
        }
        let (taken, _) = pin(&state).unwrap();
        let command = format!(
            "{} && {}",
            in_dir(&state, taken, script),
            remove_dir(&state, taken)
        );
        assert!(
            run_command(&command, tmp.path(), None).success(),
            "{command}"
        );
        assert!(!state.exists(), "{command}");
    }
    marked_state(&state);
    let (taken, _) = pin(&state).unwrap();
    let bin = podman_shim(tmp.path());
    let command = move_aside(&state, taken);
    assert!(
        run_command(&command, tmp.path(), Some(&bin)).success(),
        "{command}"
    );
    assert!(!state.exists(), "{command}");
    assert!(!tmp.path().join("my 'state' $x.old").exists(), "{command}");
}

/// Each printed command checks, right after entering a directory, that it
/// is the one ranikust checked: with any part of the pin wrong, or with a
/// parent swapped for a symlink after the commands were printed, to a
/// directory holding that name, each fails before it changes anything.
#[test]
fn the_printed_commands_change_nothing_in_another_directory() {
    let tmp = private_tempdir();
    let bin = podman_shim(tmp.path());
    let parent = tmp.path().join("parent");
    let state = parent.join("state");
    let other = tmp.path().join("other");
    type Printed = fn(&Path, Pin) -> String;
    let commands: [(&str, Printed); 5] = [
        ("root", |d, p| in_dir(d, p, ROOT_SCRIPT)),
        ("root's for the owner", |d, p| {
            in_dir(d, p, &root_script(Owner::of(p.uid)))
        }),
        ("user", |d, p| in_dir(d, p, USER_SCRIPT)),
        ("rmdir", remove_dir),
        ("move aside", move_aside),
    ];
    for (name, command) in commands {
        // `rmdir` only removes an empty directory.
        let make = |dir: &Path| {
            if name == "rmdir" {
                fs::create_dir_all(dir).unwrap();
            } else {
                marked_state(dir);
            }
        };
        make(&state);
        let right = pin_of(&state);
        let wrong = [
            Pin {
                dev: right.dev + 1,
                ..right
            },
            Pin {
                ino: right.ino + 1,
                ..right
            },
            Pin {
                uid: right.uid + 1,
                ..right
            },
        ];
        let before = listing(tmp.path());
        for pin in wrong {
            let command = command(&state, pin);
            assert!(
                !run_command(&command, tmp.path(), Some(&bin)).success(),
                "{name}: {command}"
            );
            assert_eq!(listing(tmp.path()), before, "{name}: {command}");
        }
        // The commands printed for the parent, which is then moved away
        // and replaced by a symlink to another directory holding a
        // `state` of its own.
        let printed = command(&state, right);
        make(&other.join("state"));
        let moved = tmp.path().join("parent.moved");
        fs::rename(&parent, &moved).unwrap();
        std::os::unix::fs::symlink(&other, &parent).unwrap();
        let before = listing(tmp.path());
        assert!(
            !run_command(&printed, tmp.path(), Some(&bin)).success(),
            "{name}: {printed}"
        );
        assert_eq!(listing(tmp.path()), before, "{name}: {printed}");
        fs::remove_file(&parent).unwrap();
        fs::remove_dir_all(&moved).unwrap();
        fs::remove_dir_all(&other).unwrap();
    }
}

/// Each script stops, saying what it needs, before it changes anything
/// unless `find` is GNU findutils': with one that answers `--version` as
/// BusyBox's does, or prints nothing, and otherwise runs the machine's,
/// the marker and every entry stay.
#[test]
fn the_scripts_change_nothing_without_gnu_findutils() {
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    let owner = pin_of(tmp.path()).uid;
    let shims = [
        (
            "busybox",
            "#!/bin/sh\nif test \"$1\" = --version; then\n  echo \"find: unrecognized option: \
             --version\" >&2\n  echo \"BusyBox v1.36.1 (2024-06-10 07:11:47 UTC) multi-call \
             binary.\" >&2\n  exit 1\nfi\nexec env PATH=\"$REAL_PATH\" find \"$@\"\n",
        ),
        (
            "silent",
            "#!/bin/sh\ntest \"$1\" = --version && exit 0\nexec env PATH=\"$REAL_PATH\" find \
             \"$@\"\n",
        ),
        (
            "gnu-without-fts",
            "#!/bin/sh\nif test \"$1\" = --version; then\n  echo \"find (GNU findutils) \
             4.4.2\"\n  echo \"Features enabled: D_TYPE O_NOFOLLOW(enabled) \
             LEAF_OPTIMISATION\"\n  exit 0\nfi\nexec env PATH=\"$REAL_PATH\" find \"$@\"\n",
        ),
    ];
    for (name, shim) in shims {
        let bin = tmp.path().join(format!("bin-{name}"));
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("find"), shim).unwrap();
        fs::set_permissions(bin.join("find"), fs::Permissions::from_mode(0o755)).unwrap();
        for script in [
            ROOT_SCRIPT.to_owned(),
            root_script(Owner::of(owner)),
            USER_SCRIPT.to_owned(),
        ] {
            marked_state(&state);
            let before = listing(tmp.path());
            let command = in_dir(&state, pin_of(&state), &script);
            let mut run = std::process::Command::new("bash");
            run.args(["-c", &command]).current_dir(tmp.path());
            bin_first(&mut run, &bin);
            let out = run.output().unwrap();
            assert!(!out.status.success(), "{name}: {command}");
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(err.contains(NEEDS_GNU_FIND), "{name}: {err}");
            assert_eq!(listing(tmp.path()), before, "{name}: {command}");
            fs::remove_dir_all(&state).unwrap();
        }
    }
}

#[test]
fn without_a_user_namespace_the_marker_is_renamed_before_anything_goes() {
    // Builds refuse the directory meanwhile; its marker goes once nothing
    // else is left, or a later prune finishes it.
    let dir = Path::new("/home/u/my state");
    let pin = Pin {
        dev: 1,
        ino: 2,
        uid: 3,
    };
    let guidance = no_user_namespace(dir, Ok(pin));
    assert!(
        guidance.contains(
            r#"( CDPATH= cd -P -- '/home/u/my state' && stat -c %d:%i:%u . | grep -qxF 1:2:3 && exec flock -xn . sh -c 'find --version 2>/dev/null | grep -q "GNU findutils" && find --version 2>/dev/null | grep -qF "FTS(FTS_CWDFD)" || { echo "needs GNU findutils (Alpine: apk add findutils)" >&2; exit 1; }; { "#
        ),
        "{guidance}"
    );
    assert!(
        guidance.contains(
            "' ), which needs GNU findutils (Alpine: apk add findutils), renames its marker"
        ),
        "{guidance}"
    );
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
    // No command for a directory ranikust could not pin; the user is not
    // told to run `prune --all` as a user.
    let guidance = no_user_namespace(dir, Err(NoPin::Symlink("why".to_owned())));
    assert!(
        guidance.ends_with(
            "without it, ranikust gives no commands for '/home/u/my state', which it cannot \
             open without following a symbolic link (why): give the path with no symbolic link \
             on it"
        ),
        "{guidance}"
    );
    let guidance = no_user_namespace(dir, Err(NoPin::Refused("why".to_owned())));
    assert!(
        guidance.ends_with("ranikust gives no commands for '/home/u/my state': why"),
        "{guidance}"
    );
    let guidance = no_user_namespace(dir, Err(NoPin::NoName));
    assert!(
        guidance.ends_with("which ends in ..: give the state directory's own path"),
        "{guidance}"
    );
    assert!(!guidance.contains("cd -P"), "{guidance}");
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
        .args(["-c", &in_dir(state, pin_of(state), script)])
        .current_dir(cwd)
        .envs(env.iter().copied());
    if let Some(bin) = bin {
        bin_first(&mut command, bin);
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
    let owner = pin_of(tmp.path()).uid;
    let scripts = [
        (ROOT_SCRIPT.to_owned(), "rm"),
        (root_script(Owner::of(owner)), "rm"),
        (USER_SCRIPT.to_owned(), "mv"),
    ];
    for (i, (script, tool)) in scripts.iter().enumerate() {
        let bin = tmp.path().join(format!("bin-{i}-{tool}"));
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
    let owner = pin_of(tmp.path()).uid;
    for (script, renamed_before) in [
        (ROOT_SCRIPT.to_owned(), false),
        (ROOT_SCRIPT.to_owned(), true),
        (root_script(Owner::of(owner)), false),
        (root_script(Owner::of(owner)), true),
        (USER_SCRIPT.to_owned(), false),
        (USER_SCRIPT.to_owned(), true),
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
        assert!(
            run_script(&script, tmp.path(), &state).success(),
            "{script}"
        );
        assert_eq!(listing(&state), Vec::new(), "{script}");
        assert!(outside.join("keep").is_file(), "{script}");
        fs::remove_dir(&state).unwrap();
        // Unmarked, or marked only by a symlink: refused before anything
        // goes.
        for marker in [".ranikust-state", ".ranikust-pruning"] {
            fs::create_dir(&state).unwrap();
            fs::write(state.join("keep"), b"x").unwrap();
            assert!(
                !run_script(&script, tmp.path(), &state).success(),
                "{script}"
            );
            assert!(state.join("keep").is_file(), "{script}");
            std::os::unix::fs::symlink(outside.join("keep"), state.join(marker)).unwrap();
            assert!(
                !run_script(&script, tmp.path(), &state).success(),
                "{script} {marker}"
            );
            assert!(state.join("keep").is_file(), "{script} {marker}");
            assert!(state.join(marker).is_symlink(), "{script} {marker}");
            assert!(outside.join("keep").is_file(), "{script} {marker}");
            fs::remove_dir_all(&state).unwrap();
        }
    }
    // The user's marker is never moved into a `.ranikust-pruning` that is
    // not a regular file, there before the run or made just before `mv`.
    let state = tmp.path().join("state");
    let bin = tmp.path().join("mkdir-bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("mv"),
        "#!/bin/sh\nPATH=$REAL_PATH mkdir .ranikust-pruning || exit\nPATH=$REAL_PATH mv \"$@\"\n\
         s=$?\necho $s > ../mv-ran\nexit $s\n",
    )
    .unwrap();
    let mv_ran = tmp.path().join("mv-ran");
    fs::set_permissions(bin.join("mv"), fs::Permissions::from_mode(0o755)).unwrap();
    for made_by_mv in [false, true] {
        fill(&state);
        if !made_by_mv {
            fs::create_dir(state.join(".ranikust-pruning")).unwrap();
        }
        let shim = made_by_mv.then_some(bin.as_path());
        let ran = run_script_with(USER_SCRIPT, tmp.path(), &state, shim, &[]).success();
        assert!(!ran, "made by mv: {made_by_mv}");
        assert!(
            state.join(".ranikust-state").is_file(),
            "made by mv: {made_by_mv}"
        );
        assert!(
            state.join(".ranikust-pruning").is_dir(),
            "made by mv: {made_by_mv}"
        );
        assert!(
            listing(&state.join(".ranikust-pruning")).is_empty(),
            "made by mv: {made_by_mv}"
        );
        assert!(state.join("blobs").is_dir(), "made by mv: {made_by_mv}");
        // The machine's `mv` ran after the shim's `mkdir`, and failed.
        let status = match fs::read_to_string(&mv_ran) {
            Ok(status) => Some(status),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("{}: {e}", mv_ran.display()),
        };
        let want = made_by_mv.then(|| "1\n".to_owned());
        assert_eq!(status, want, "made by mv: {made_by_mv}: mv's status");
        if made_by_mv {
            fs::remove_file(&mv_ran).unwrap();
        }
        fs::remove_dir_all(&state).unwrap();
    }
    // Nor beside a symlink of that name, to a regular file or to nothing.
    for target in [outside.join("keep"), tmp.path().join("nothing")] {
        fill(&state);
        let link = state.join(".ranikust-pruning");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let ran = run_script(USER_SCRIPT, tmp.path(), &state).success();
        let to = target.display();
        assert!(!ran, "to {to}");
        assert!(link.is_symlink(), "to {to}");
        assert!(state.join(".ranikust-state").is_file(), "to {to}");
        assert!(state.join("blobs").is_dir(), "to {to}");
        assert!(outside.join("keep").is_file(), "to {to}");
        fs::remove_dir_all(&state).unwrap();
    }
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

/// Root's emptying deletes, in a directory a user owns, only the entries
/// of that user's uid and of its subordinate range, `find` given the
/// owner's uid and the range's bounds; with no range, or one holding root's
/// uid, the owner's uid alone. In a directory root owns, every entry goes.
#[test]
fn roots_emptying_of_a_users_directory_deletes_only_that_users_entries() {
    let range = IdRange {
        start: 100_000,
        count: 65_536,
    };
    assert_eq!(root_script(Owner::new(0, Some(range))), ROOT_SCRIPT);
    assert!(!ROOT_SCRIPT.contains("-uid"));
    let ranged = root_script(Owner::new(1, Some(range)));
    assert!(
        ranged.contains(r" \( -uid 1 -o \( -uid +99999 -uid -165536 \) \) -delete && "),
        "{ranged}"
    );
    assert!(
        ranged.contains(
            r#" || { echo "not emptied: entries of uids other than 1 and its subordinate range 100000-165535 are left in place, as is anything find names above" >&2; exit 1; }; }"#
        ),
        "{ranged}"
    );
    let holds_root = IdRange { start: 0, count: 5 };
    for range in [None, Some(holds_root)] {
        let alone = root_script(Owner::new(1, range));
        assert!(alone.contains(r" \( -uid 1 \) -delete && "), "{alone}");
        assert!(
            alone.contains(
                r#"echo "not emptied: entries of uids other than 1 are left in place, as"#
            ),
            "{alone}"
        );
        assert_eq!(
            owner_only(Owner::new(1, range)),
            " and deletes only the entries of uid 1, the directory's owner, stopping where another \
             uid owns any"
        );
    }
    assert_eq!(
        owner_only(Owner::new(1, Some(range))),
        " and deletes only the entries of uid 1, the directory's owner, and of its subordinate \
         ids 100000-165535, stopping where another uid owns any"
    );
    assert_eq!(owner_only(Owner::new(0, Some(range))), "");
    let tmp = private_tempdir();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"x").unwrap();
    let state = tmp.path().join("my 'state' $x");
    let fill = |state: &Path| {
        marked_state(state);
        fs::create_dir_all(state.join("builds/b/deep")).unwrap();
        fs::write(state.join("builds/b/deep/f"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, state.join("builds/b/link")).unwrap();
    };
    // A `find` that writes the arguments of each run, one a line, to
    // `$ARGS`, then runs the machine's.
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("find"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" -- >> \"$ARGS\"\nexec env PATH=\"$REAL_PATH\" find \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("find"), fs::Permissions::from_mode(0o755)).unwrap();
    let args = tmp.path().join("args");
    let run = |owner: Owner| {
        let pin = pin_of(&state);
        let command = format!(
            "{} && {}",
            in_dir(&state, pin, &root_script(owner)),
            remove_dir(&state, pin)
        );
        let mut bash = std::process::Command::new("bash");
        bash.args(["-c", &command])
            .current_dir(tmp.path())
            .env("ARGS", &args);
        bin_first(&mut bash, &bin);
        let out = bash.output().unwrap();
        let runs = fs::read_to_string(&args).unwrap();
        fs::remove_file(&args).unwrap();
        let deleting: Vec<String> = runs
            .split("--\n")
            .filter(|run| run.ends_with("-delete\n"))
            .map(|run| run.lines().collect::<Vec<_>>().join(" "))
            .collect();
        (out, deleting)
    };
    // The test's own files, run for their owner.
    let me = pin_of(tmp.path()).uid;
    let far = IdRange {
        start: me + 10,
        count: 5,
    };
    let mut cases = vec![(Owner::new(0, None), "-delete".to_owned())];
    if me != 0 {
        cases.push((Owner::new(me, None), format!("( -uid {me} ) -delete")));
        cases.push((
            Owner::new(me, Some(far)),
            format!(
                "( -uid {me} -o ( -uid +{} -uid -{} ) ) -delete",
                me + 9,
                me + 15
            ),
        ));
    }
    for (owner, predicate) in cases {
        fill(&state);
        let (out, deleting) = run(owner);
        assert!(out.status.success(), "{owner:?}: {out:?}");
        assert!(!state.exists(), "{owner:?}");
        assert!(outside.join("keep").is_file(), "{owner:?}");
        assert_eq!(
            deleting,
            [format!(". -xdev -mindepth 1 {predicate}")],
            "{owner:?}"
        );
    }
    // Run for another owner, with the test's uid at each edge of the
    // range: just before it, its first, its last, just after it. Outside,
    // nothing but the marker goes, and the script stops, saying so, before
    // `rmdir`.
    if me < 6 {
        eprintln!("skipped: the edges of a range around uid {me}");
        return;
    }
    let other = me + 100;
    for (start, deleted) in [(me + 1, false), (me, true), (me - 4, true), (me - 5, false)] {
        let range = IdRange { start, count: 5 };
        fill(&state);
        let mut before = listing(&state);
        before.retain(|(name, _)| name != Path::new(rnk_store::STATE_MARKER));
        let (out, deleting) = run(Owner::new(other, Some(range)));
        assert_eq!(
            deleting,
            [format!(
                ". -xdev -mindepth 1 ( -uid {other} -o ( -uid +{} -uid -{} ) ) -delete",
                start - 1,
                start + 5
            )],
            "{start}"
        );
        assert!(outside.join("keep").is_file(), "{start}");
        if deleted {
            assert!(out.status.success(), "{start}: {out:?}");
            assert!(!state.exists(), "{start}");
            continue;
        }
        assert!(!out.status.success(), "{start}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        let why = format!(
            "not emptied: entries of uids other than {other} and its subordinate range {start}-{} \
             are left in place",
            start + 4
        );
        assert!(err.contains(&why), "{start}: {err}");
        assert_eq!(listing(&state), before, "{start}");
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
/// shared: run while no store is open, refused while one is.
#[test]
fn the_suggested_commands_wait_for_no_build() {
    let tmp = private_tempdir();
    let state = tmp.path().join("my 'state'");
    marked_state(&state);
    let ran = state.join("ran");
    let command = in_dir(&state, pin_of(&state), "touch ran");
    let run = || {
        std::process::Command::new("bash")
            .args(["-c", &command])
            .status()
            .unwrap()
    };
    assert!(run().success(), "{command}");
    assert!(ran.exists());
    fs::remove_file(&ran).unwrap();
    // A closed store's lock outlives it in a child another test spawns
    // meanwhile, so nothing that needs the lock runs after it.
    let open = Store::open(&rnk_store::BuildPaths::new(&state, &state)).unwrap();
    assert!(!run().success(), "{command}");
    assert!(!ran.exists());
    open.close().unwrap();
    // Nothing is made at a missing path, where `flock` would make a file.
    let missing = tmp.path().join("missing");
    let command = in_dir(&missing, pin_of(&state), "true");
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
    let pin = Pin {
        dev: 1,
        ino: 2,
        uid: 3,
    };
    let ways = unfinished(dir, Ok(pin));
    let owner = Owner::of(3);
    let root = format!(
        "; or, as root, empty it in place, which needs GNU findutils (Alpine: apk add \
         findutils){}, {}, then ",
        owner_only(owner),
        in_dir(dir, pin, &root_script(owner))
    );
    assert!(ways.contains(&root), "{ways}");
    for want in [
        "on another mount",
        "subordinate ids",
        "unmount anything below '/home/u/my state'",
        "with newuidmap installed and ranges for this user in /etc/subuid and /etc/subgid",
        r#"; or move the directory aside and remove it, ( CDPATH= cd -P -- /home/u && test -d './my state' && ! test -h './my state' && stat -c %d:%i:%u './my state' | grep -qxF 1:2:3 && mv -T -- './my state' './my state.old' && podman unshare rm -rf --one-file-system -- './my state.old' ); or, as root, empty it in place, which needs GNU findutils (Alpine: apk add findutils) and deletes only the entries of uid 3, the directory's owner,"#,
        r#"( CDPATH= cd -P -- '/home/u/my state' && stat -c %d:%i:%u . | grep -qxF 1:2:3 && exec flock -xn . sh -c 'find --version "#,
        r#" \( -uid 3"#,
        r#"), then ( CDPATH= cd -P -- /home/u && stat -c %d:%i:%u './my state' | grep -qxF 1:2:3 && rmdir -- './my state' )"#,
    ] {
        assert!(ways.contains(want), "{want}: {ways}");
    }
    // No command for a directory ranikust could not pin.
    let none = unfinished(dir, Err(NoPin::Symlink("why".to_owned())));
    assert!(
        none.ends_with(
            "/etc/subgid; ranikust gives no commands for '/home/u/my state', which it cannot \
             open without following a symbolic link (why): give the path with no symbolic link \
             on it"
        ),
        "{none}"
    );
    let not_pruned = rnk_store::Error::NotPruned {
        path: dir.to_owned(),
        left: "\"x\"".to_owned(),
    };
    let refusal = store_refusal(&not_pruned, dir, Ok(pin));
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
        assert_eq!(store_refusal(&e, dir, Ok(pin)).guidance, "", "{e}");
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
    assert!(matches!(statedir::check(&dir).unwrap(), Found::Entries(_)));
    assert_eq!(listing(tmp.path()), before, "modes included");
}

/// A user's `prune --all`, as root's commands, takes no path that ends in
/// `..`, whatever it leads to: refused before anything is opened, so
/// neither as nothing to prune nor with the check's reason.
#[test]
fn a_users_path_ending_in_dotdot_is_refused_before_anything_is_opened() {
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    marked_state(&state);
    let missing = tmp.path().join("missing");
    let before = listing(tmp.path());
    let user_ns = fs::read_link("/proc/self/ns/user").unwrap();
    // Not one that leads to the state directory: that one would reach the
    // namespace, which this test process, which has threads, cannot enter.
    for up in [missing.join(".."), state.join("..")] {
        let refusal = prune_all(1000, &up).unwrap_err();
        assert_eq!(
            refusal.reason,
            format!(
                "the state directory {} ends in ..: give the state directory's own path",
                up.display()
            )
        );
        assert_eq!(refusal.kind, Kind::Other);
    }
    assert_eq!(listing(tmp.path()), before);
    assert_eq!(fs::read_link("/proc/self/ns/user").unwrap(), user_ns);
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
    assert!(matches!(statedir::check(&dir).unwrap(), Found::Entries(_)));
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

mod red_team;
