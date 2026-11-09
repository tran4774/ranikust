//! Red team of `prune --all` and the commands its refusals print (T4):
//! paths a shell may read otherwise, real tools that are not GNU `find`,
//! directories swapped while GNU `find` walks, system directories; as root
//! (T2c), entries of other ids, special files and links out, and a
//! filesystem mounted below. Of `prune --all --userns` (ADR-0146): its
//! refusals of root's default however spelt and of symlinks, and root's
//! command after it in hostile trees; as root, the `userns` quick case.

use std::mem::MaybeUninit;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use super::*;

/// The two commands root's refusal prints for `dir`, cut from its guidance
/// line as the error report writes it: the emptying, then the `rmdir`.
fn printed_for_root(dir: &Path) -> (String, String) {
    let refusal = prune_all(0, dir).unwrap_err();
    let mut out = Vec::new();
    crate::report_to(&mut out, &refusal.reason, &refusal.guidance).unwrap();
    let text = String::from_utf8(out).unwrap();
    let line = text
        .lines()
        .nth(1)
        .unwrap_or_else(|| panic!("no guidance: {text}"));
    let first = format!("( {ENV_CHECK} && CDPATH= cd -P -- ");
    let start = line
        .find(&first)
        .unwrap_or_else(|| panic!("no commands: {line}"));
    let then = line.rfind(&format!(", then {first}")).expect(line);
    let end = line
        .rfind("; Phase 5's ranikust prune will serve root too")
        .expect(line);
    (
        line[start..then].to_owned(),
        line[then + ", then ".len()..end].to_owned(),
    )
}

/// `command` run by the shell `sh` in `cwd`.
fn shell_run(sh: &[&str], command: &str, cwd: &Path) -> Output {
    Command::new(sh[0])
        .args(&sh[1..])
        .args(["-c", command])
        .current_dir(cwd)
        .output()
        .unwrap()
}

/// T4: root's printed commands for a path a shell must quote or may read
/// otherwise, run by each shell present as printed, empty and remove the
/// directory ranikust checked or change nothing; a marked directory where
/// the shell reads the path otherwise (dash's `$'…'`) is left as it is.
#[test]
fn red_team_printed_paths_act_only_on_the_checked_directory_in_any_shell() {
    let tmp = private_tempdir();
    let cases: [(&[u8], &[u8]); 4] = [
        (b"-p 'q' \"d\" $HOME \\ !x `id` *", b"-n"),
        (b"p", b"nl\nname"),
        (b"\xff\xfe", b"st"),
        (b"e\x1b]0;x\x07", b"-"),
    ];
    let shells = shells();
    assert!(!shells.is_empty());
    for (s, sh) in shells.iter().enumerate() {
        for (c, (p, n)) in cases.iter().enumerate() {
            let base = tmp.path().join(format!("{s}-{c}"));
            let cwd = base.join("cwd");
            let outside = base.join("outside");
            fs::create_dir_all(&cwd).unwrap();
            // Private whatever the umask, as a build requires.
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
            fs::create_dir_all(&outside).unwrap();
            fs::write(outside.join("keep"), b"x").unwrap();
            let state = base.join(OsStr::from_bytes(p)).join(OsStr::from_bytes(n));
            marked_state(&state);
            symlink(&outside, state.join("blobs/out")).unwrap();
            let (emptying, removal) = printed_for_root(&state);
            let word = shell_word(state.as_os_str());
            assert!(
                emptying.contains(&format!("cd -P -- {word} && ")),
                "{emptying}"
            );
            let read = shell_run(sh, &format!("printf %s {word}"), &cwd);
            assert!(read.status.success(), "{sh:?}: {read:?}");
            // Where this shell reads another path: a state directory there.
            let decoy = (read.stdout != state.as_os_str().as_bytes()).then(|| {
                let read = Path::new(OsStr::from_bytes(&read.stdout));
                // `starts_with` is lexical: a `..` would lead out of `cwd`.
                assert!(
                    !read.components().any(|c| c == Component::ParentDir),
                    "{sh:?} reads {word} as {read:?}"
                );
                let at = cwd.join(read);
                assert!(at.starts_with(&cwd), "{sh:?} reads {word} as {at:?}");
                marked_state(&at);
                at
            });
            let before = listing(&base);
            let emptied = shell_run(sh, &emptying, &cwd);
            let removed = shell_run(sh, &removal, &cwd);
            assert!(outside.join("keep").is_file(), "{sh:?} {c}");
            match decoy {
                Some(decoy) => {
                    assert!(!emptied.status.success(), "{sh:?} {c}: {emptied:?}");
                    assert!(!removed.status.success(), "{sh:?} {c}: {removed:?}");
                    assert_eq!(listing(&base), before, "{sh:?} {c}: {decoy:?}");
                }
                None => {
                    assert!(emptied.status.success(), "{sh:?} {c}: {emptied:?}");
                    assert!(removed.status.success(), "{sh:?} {c}: {removed:?}");
                    assert!(!state.exists(), "{sh:?} {c}");
                }
            }
            fs::remove_dir_all(&base).unwrap();
        }
    }
}

/// T4: with BusyBox's `sh`, `find`, `stat`, `grep`, `rm` and `mv`, as on
/// Alpine without findutils, each script stops before changing anything,
/// whether that `sh` runs its own applets first or looks on `PATH`; with
/// GNU `find` beside BusyBox's other tools and a `sh` that looks on `PATH`
/// (dash standing for Alpine's), as on Alpine with findutils, each empties
/// the directory, which root's `rmdir` then removes.
#[test]
fn red_team_busybox_tools_change_nothing_without_gnu_find() {
    let (Some(busybox), Some(find)) = (which("busybox"), which("find")) else {
        eprintln!("skipped: no busybox or no find on PATH");
        return;
    };
    let sh = which("dash")
        .or_else(|| which("bash"))
        .expect("dash or bash");
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    let owner = pin_of(tmp.path()).uid;
    for gnu in [false, true] {
        let bin = tmp.path().join(format!("bin-{gnu}"));
        fs::create_dir(&bin).unwrap();
        for applet in ["sh", "stat", "grep", "rm", "mv", "find"] {
            let to = match applet {
                "find" if gnu => &find,
                "sh" if gnu => &sh,
                _ => &busybox,
            };
            symlink(to, bin.join(applet)).unwrap();
        }
        for script in [
            ROOT_SCRIPT.to_owned(),
            root_script(&Owner::of(owner)),
            USER_SCRIPT.to_owned(),
        ] {
            marked_state(&state);
            let before = listing(tmp.path());
            let pin = pin_of(&state);
            let command = format!(
                "{} && {}",
                in_dir(&state, pin, &script),
                remove_dir(&state, pin)
            );
            let mut run = Command::new(bin.join("sh"));
            run.args(["-c", &command]).current_dir(tmp.path());
            bin_first(&mut run, &bin);
            let out = run.output().unwrap();
            if gnu {
                assert!(out.status.success(), "{command}: {out:?}");
                assert!(!state.exists(), "{command}");
            } else {
                assert!(!out.status.success(), "{command}");
                let err = String::from_utf8_lossy(&out.stderr);
                assert!(err.contains(NEEDS_GNU_FIND), "{err}");
                assert_eq!(listing(tmp.path()), before, "{command}");
                fs::remove_dir_all(&state).unwrap();
            }
        }
    }
}

/// T4: while GNU `find` empties the directory under each script, its owner
/// swaps every directory the walk listed and has not entered for a symlink
/// to a directory outside holding the same names, then moves the tree
/// being walked out and adds a file to it there: nothing outside goes.
/// Root's removal of what `prune --all --userns` left too, this test's ids
/// standing for the range's, whose filter takes the files outside.
#[test]
fn red_team_directories_swapped_during_the_walk_lead_nowhere_out() {
    use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
    const DIRS: usize = 10;
    const FILES: usize = 400;
    let tmp = private_tempdir();
    let me = pin_of(tmp.path()).uid;
    let scripts = [
        (ROOT_SCRIPT.to_owned(), rnk_store::STATE_MARKER),
        (root_script(&Owner::of(me)), rnk_store::STATE_MARKER),
        (USER_SCRIPT.to_owned(), rnk_store::STATE_MARKER),
        (userns_left_script(own_range()), rnk_store::PRUNING_MARKER),
    ];
    let names: Vec<String> = (0..DIRS).map(|d| format!("w{d:02}")).collect();
    let (outside, aside, state) = (
        tmp.path().join("outside"),
        tmp.path().join("aside"),
        tmp.path().join("state"),
    );
    let walked = state.join("caches/k");
    for (script, marker) in &scripts {
        // A round whose racer saw no deletion in time is run again.
        let mut swapped = 0;
        for _ in 0..3 {
            marked_state(&state);
            if *marker != rnk_store::STATE_MARKER {
                fs::rename(state.join(rnk_store::STATE_MARKER), state.join(marker)).unwrap();
            }
            fs::create_dir(&aside).unwrap();
            for name in &names {
                for dir in [walked.join(name), outside.join(name)] {
                    fs::create_dir_all(&dir).unwrap();
                    for f in 0..FILES {
                        fs::write(dir.join(format!("f{f:03}")), b"x").unwrap();
                    }
                }
            }
            let victims = listing(&outside);
            let watch = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).unwrap();
            let watched: Vec<(i32, String)> = names
                .iter()
                .map(|n| {
                    let wd = inotify::add_watch(&watch, walked.join(n), WatchFlags::DELETE);
                    (wd.unwrap(), n.clone())
                })
                .collect();
            let racer = {
                let (walked, outside, aside) = (walked.clone(), outside.clone(), aside.clone());
                std::thread::spawn(move || -> usize {
                    let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
                    let mut events = inotify::Reader::new(&watch, &mut buf);
                    let deadline = Instant::now() + Duration::from_secs(20);
                    let hit = loop {
                        match events.next() {
                            Ok(e) if e.events().contains(ReadFlags::DELETE) => break e.wd(),
                            Ok(_) => {}
                            Err(rustix::io::Errno::AGAIN) if Instant::now() < deadline => {
                                std::thread::yield_now();
                            }
                            Err(_) => return 0,
                        }
                    };
                    let mut swapped = 0;
                    for (wd, name) in &watched {
                        if *wd != hit && fs::rename(walked.join(name), aside.join(name)).is_ok() {
                            symlink(outside.join(name), walked.join(name)).unwrap();
                            swapped += 1;
                        }
                    }
                    if fs::rename(&walked, aside.join("k")).is_ok() {
                        fs::write(aside.join("k/added"), b"x").unwrap();
                        symlink(&outside, &walked).unwrap();
                    }
                    swapped
                })
            };
            let command = in_dir(&state, pin_of(&state), script);
            let ran = run_command(&command, tmp.path(), None);
            swapped = racer.join().unwrap();
            assert_eq!(listing(&outside), victims, "{ran}: {command}");
            if swapped > 0 {
                assert!(aside.join("k/added").is_file(), "{command}");
            }
            for dir in [&state, &aside, &outside] {
                fs::remove_dir_all(dir).unwrap();
            }
            if swapped > 0 {
                break;
            }
        }
        assert!(swapped > 0, "the race was never run: {script}");
    }
}

/// T4: the checks a user's `prune --all` runs before any user namespace,
/// which change nothing, refuse `/`, system directories and the home
/// directory, each that exists and holds anything (one missing or empty
/// is left as it is); root's refusal gives no commands for them.
#[test]
fn red_team_system_directories_and_the_home_get_nothing() {
    let mut dirs: Vec<PathBuf> = [
        "/", "/etc", "/home", "/usr", "/var", "/var/lib", "/tmp", "/root", "/proc", "/dev",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    dirs.extend(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute()),
    );
    for dir in &dirs {
        let found = statedir::check(dir);
        // One this process cannot list counts as holding something.
        let holds = fs::read_dir(dir).map_or(dir.exists(), |mut d| d.next().is_some());
        if holds {
            assert!(found.is_err(), "{}: {found:?}", dir.display());
        } else {
            assert!(
                !matches!(found, Ok(Found::Entries(_))),
                "{}: {found:?}",
                dir.display()
            );
        }
        let guidance = prune_all(0, dir).unwrap_err().guidance;
        assert!(!guidance.contains("cd -P"), "{}: {guidance}", dir.display());
    }
}

/// The uid whose state directory root's commands run on as root: the first
/// user of an Ubuntu image, whose first `/etc/subuid` range, where it has
/// one, the commands take as its subordinate ids.
const OWNER: u32 = 1000;

/// Gives `path`, not followed, the owner `uid`.
fn own(path: &Path, uid: u32) {
    std::os::unix::fs::lchown(path, Some(uid), Some(uid)).unwrap();
}

/// Every path below `dir`, never following a symlink, with its owner,
/// mode and content (a symlink's target), sorted.
fn owned(dir: &Path) -> Vec<(PathBuf, u32, u32, Vec<u8>)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            let content = if m.is_dir() {
                todo.push(p.clone());
                Vec::new()
            } else if m.is_symlink() {
                fs::read_link(&p)
                    .unwrap()
                    .into_os_string()
                    .into_encoded_bytes()
            } else if m.is_file() {
                fs::read(&p).unwrap()
            } else {
                Vec::new()
            };
            out.push((
                p.strip_prefix(dir).unwrap().to_owned(),
                m.uid(),
                m.mode(),
                content,
            ));
        }
    }
    out.sort();
    out
}

/// Runs `command` with bash in `cwd`.
fn bash(command: &str, cwd: &Path) -> Output {
    Command::new("bash")
        .args(["-c", command])
        .current_dir(cwd)
        .output()
        .unwrap()
}

/// In `state`, owned by [`OWNER`], what its builds could leave: a file
/// `f-<id>` of each of `ids`, a setuid file, a device node, a FIFO, a
/// socket, names a shell would read otherwise, a symlink to `outside/dir`
/// and a hardlink to `outside/owned`, both [`OWNER`]'s.
fn owners_state(state: &Path, ids: &[u32], outside: &Path) {
    let layer = state.join("layers/v3/sha256/x");
    fs::create_dir_all(&layer).unwrap();
    fs::set_permissions(state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(state.join("caches/c")).unwrap();
    fs::write(state.join(rnk_store::STATE_MARKER), b"").unwrap();
    for id in ids {
        fs::write(layer.join(format!("f-{id}")), b"x").unwrap();
    }
    for name in ["nl\nname", "-rf", "it's $(id)"] {
        fs::write(layer.join(name), b"x").unwrap();
    }
    fs::write(layer.join("suid"), b"x").unwrap();
    fs::set_permissions(layer.join("suid"), fs::Permissions::from_mode(0o4755)).unwrap();
    for (tool, args) in [
        ("mknod", &["null", "c", "1", "3"][..]),
        ("mkfifo", &["fifo"][..]),
    ] {
        let made = Command::new(tool)
            .args(args)
            .current_dir(&layer)
            .status()
            .unwrap();
        assert!(made.success(), "{tool}");
    }
    drop(std::os::unix::net::UnixListener::bind(layer.join("sock")).unwrap());
    symlink(outside.join("dir"), state.join("caches/c/link-out")).unwrap();
    fs::hard_link(outside.join("owned"), state.join("caches/c/hard-owner")).unwrap();
    let mut todo = vec![state.to_owned()];
    while let Some(d) = todo.pop() {
        own(&d, OWNER);
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                todo.push(p);
            } else if p != state.join("caches/c/hard-owner") {
                own(&p, OWNER);
            }
        }
    }
    for id in ids {
        own(&layer.join(format!("f-{id}")), *id);
    }
    // The device, the setuid file and the socket of subordinate ids.
    for (name, id) in ["null", "suid", "sock"]
        .iter()
        .zip(ids.iter().cycle().skip(1))
    {
        own(&layer.join(name), *id);
    }
    fs::set_permissions(layer.join("suid"), fs::Permissions::from_mode(0o4755)).unwrap();
}

/// T2c, T4: root's printed commands for a user's state directory, run by
/// root as printed, change nothing where entries of other ids are, saying
/// so, and the `rmdir` fails; with no other id's entry, they delete the
/// entries of its owner and of its subordinate ids, special files and
/// links out among them, and remove the directory. The hardlinked and
/// linked-to files outside are unchanged.
#[test]
#[ignore = "T2c: default: chown to other users and mknod need root"]
fn red_team_roots_commands_take_only_the_owners_ids_and_nothing_outside() {
    let tmp = private_tempdir();
    let owner = Owner::of(OWNER);
    let ours = |id: u32| {
        id == OWNER
            || owner
                .range
                .is_some_and(|r| id >= r.start && id - r.start < r.count)
    };
    let mut ids = vec![OWNER];
    let mut others = vec![0, 65_534];
    match owner.range {
        Some(r) => {
            ids.extend([r.start, r.start + (r.count - 1)]);
            others.extend([r.start - 1, r.start + r.count]);
        }
        None => others.push(OWNER + 1),
    }
    others.retain(|id| !ours(*id));
    let outside = tmp.path().join("outside");
    fs::create_dir_all(outside.join("dir")).unwrap();
    fs::write(outside.join("secret"), b"secret").unwrap();
    fs::set_permissions(outside.join("secret"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(outside.join("owned"), b"owned").unwrap();
    fs::write(outside.join("dir/keep"), b"keep").unwrap();
    for p in ["owned", "dir", "dir/keep"] {
        own(&outside.join(p), OWNER);
    }
    let before = owned(&outside);
    let nlink = |name: &str| fs::symlink_metadata(outside.join(name)).unwrap().nlink();
    let parent = tmp.path().join("p a'r\nent");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    // With entries of other ids too: of root, of nobody, just outside the
    // range; a hardlink to a file of root's; a directory of root's
    // holding one of the owner's.
    let state = parent.join("-st'a\nte");
    owners_state(&state, &ids, &outside);
    let kept = state.join("tmp/b");
    fs::create_dir_all(&kept).unwrap();
    own(&state.join("tmp"), OWNER);
    own(&kept, OWNER);
    for id in &others {
        let f = kept.join(format!("o-{id}"));
        fs::write(&f, b"x").unwrap();
        own(&f, *id);
    }
    fs::hard_link(outside.join("secret"), state.join("caches/c/hard-root")).unwrap();
    fs::create_dir_all(state.join("builds/x")).unwrap();
    own(&state.join("builds"), OWNER);
    fs::write(state.join("builds/x/mine"), b"x").unwrap();
    own(&state.join("builds/x/mine"), OWNER);
    let (emptying, removal) = printed_for_root(&state);
    assert!(emptying.contains(&format!(r"-uid {OWNER}")), "{emptying}");
    let inside = owned(&state);
    let emptied = bash(&emptying, tmp.path());
    let removed = bash(&removal, tmp.path());
    assert!(!emptied.status.success(), "{emptied:?}");
    let err = String::from_utf8_lossy(&emptied.stderr);
    assert!(
        err.contains(&format!(
            "nothing changed: entries of uids other than {OWNER}"
        )),
        "{err}"
    );
    assert!(!removed.status.success(), "{removed:?}");
    assert_eq!(owned(&state), inside, "{err}");
    // The unchanged directory keeps its hardlinks to both.
    assert_eq!(nlink("secret"), 2);
    assert_eq!(nlink("owned"), 2);
    assert_eq!(owned(&outside), before);
    // The owner's ids only: both commands remove it.
    let state = parent.join("second");
    let links = nlink("owned");
    owners_state(&state, &ids, &outside);
    let (emptying, removal) = printed_for_root(&state);
    let emptied = bash(&emptying, tmp.path());
    let removed = bash(&removal, tmp.path());
    assert!(emptied.status.success(), "{emptied:?}");
    assert!(removed.status.success(), "{removed:?}");
    assert!(!state.exists());
    assert_eq!(nlink("owned"), links);
    assert_eq!(owned(&outside), before);
}

/// T2c, T4: root's printed commands for its own state directory, with a
/// tmpfs mounted below it, leave the tmpfs and what it holds, and the
/// emptying and the `rmdir` then fail. In a mount namespace of the test's
/// own, every mount private.
#[test]
#[ignore = "T2c: sysadmin: mounts in a private mount namespace"]
fn red_team_roots_commands_leave_a_filesystem_mounted_below() {
    std::thread::spawn(|| {
        use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};
        // SAFETY: this thread shares nothing that the unshared filesystem
        // attributes or mount namespace would break; it ends with the test.
        unsafe {
            rustix::thread::unshare_unsafe(
                rustix::thread::UnshareFlags::FS | rustix::thread::UnshareFlags::NEWNS,
            )
        }
        .expect("unshare(CLONE_FS | CLONE_NEWNS)");
        rustix::mount::mount_change(
            "/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )
        .expect("make every mount private");
        let tmp = private_tempdir();
        let state = tmp.path().join("state");
        marked_state(&state);
        let below = state.join("caches/m");
        fs::create_dir_all(&below).unwrap();
        rustix::mount::mount("tmpfs", &below, "tmpfs", MountFlags::empty(), None).expect("tmpfs");
        fs::write(below.join("keep"), b"keep").unwrap();
        let (emptying, removal) = printed_for_root(&state);
        let emptied = bash(&emptying, tmp.path());
        let removed = bash(&removal, tmp.path());
        let kept = fs::read(below.join("keep"));
        rustix::mount::unmount(&below, UnmountFlags::DETACH).unwrap();
        assert_eq!(kept.unwrap(), b"keep");
        assert!(!emptied.status.success(), "{emptied:?}");
        assert!(!removed.status.success(), "{removed:?}");
        assert!(!state.join(rnk_store::STATE_MARKER).exists());
        assert!(!state.join("blobs").exists());
    })
    .join()
    .unwrap();
}

/// A state directory a `prune --all --userns` left unfinished: as
/// [`marked_state`], its marker renamed to `.ranikust-pruning`.
fn pruning_state(state: &Path) {
    marked_state(state);
    fs::rename(
        state.join(rnk_store::STATE_MARKER),
        state.join(rnk_store::PRUNING_MARKER),
    )
    .unwrap();
}

/// The command an unfinished `prune --all --userns` prints for root for
/// `dir`, this test's ids standing for the range's, cut from its guidance
/// line as the error report writes it.
fn printed_for_userns(dir: &Path) -> String {
    let guidance = userns_unfinished(dir, Ok(pin_of(dir)), own_range());
    let mut out = Vec::new();
    crate::report_to(&mut out, "left", &guidance).unwrap();
    let text = String::from_utf8(out).unwrap();
    let line = text
        .lines()
        .nth(1)
        .unwrap_or_else(|| panic!("no guidance: {text}"));
    let first = format!("( {ENV_CHECK} && CDPATH= cd -P -- ");
    let start = line
        .find(&first)
        .unwrap_or_else(|| panic!("no commands: {line}"));
    let end = line
        .rfind("; then run ranikust prune --all --userns --state-dir ")
        .expect(line);
    line[start..end].to_owned()
}

/// T4: root's command after an unfinished `prune --all --userns`, for a
/// path a shell must quote or may read otherwise, run by each shell
/// present as printed: all but the marker of the directory ranikust
/// checked goes, or, where the shell reads another path, nothing changes.
#[test]
fn red_team_userns_printed_paths_act_only_on_the_checked_directory_in_any_shell() {
    let tmp = private_tempdir();
    let cases: [(&[u8], &[u8]); 6] = [
        (b"-p 'q' \"d\" $HOME \\ !x `id` *", b"-n"),
        (b"p", b"nl\nname"),
        (b"\xff\xfe", b"st"),
        (b"e\x1b]0;x\x07", b"-"),
        (b"[ab]*?", b"{a,b}"),
        (b"-rf", b"--"),
    ];
    let shells = shells();
    assert!(!shells.is_empty());
    for (s, sh) in shells.iter().enumerate() {
        for (c, (p, n)) in cases.iter().enumerate() {
            let base = tmp.path().join(format!("{s}-{c}"));
            let cwd = base.join("cwd");
            let outside = base.join("outside");
            fs::create_dir_all(&cwd).unwrap();
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
            fs::create_dir_all(&outside).unwrap();
            fs::write(outside.join("keep"), b"x").unwrap();
            let state = base.join(OsStr::from_bytes(p)).join(OsStr::from_bytes(n));
            pruning_state(&state);
            symlink(&outside, state.join("blobs/out")).unwrap();
            let command = printed_for_userns(&state);
            let word = shell_word(state.as_os_str());
            assert!(
                command.contains(&format!("cd -P -- {word} && ")),
                "{command}"
            );
            let read = shell_run(sh, &format!("printf %s {word}"), &cwd);
            assert!(read.status.success(), "{sh:?}: {read:?}");
            let decoy = (read.stdout != state.as_os_str().as_bytes()).then(|| {
                let read = Path::new(OsStr::from_bytes(&read.stdout));
                assert!(
                    !read.components().any(|c| c == Component::ParentDir),
                    "{sh:?} reads {word} as {read:?}"
                );
                let at = cwd.join(read);
                assert!(at.starts_with(&cwd), "{sh:?} reads {word} as {at:?}");
                pruning_state(&at);
                at
            });
            let before = listing(&base);
            let ran = shell_run(sh, &command, &cwd);
            assert!(outside.join("keep").is_file(), "{sh:?} {c}");
            match decoy {
                Some(decoy) => {
                    assert!(!ran.status.success(), "{sh:?} {c}: {ran:?}");
                    assert_eq!(listing(&base), before, "{sh:?} {c}: {decoy:?}");
                }
                None => {
                    assert!(ran.status.success(), "{sh:?} {c}: {ran:?}");
                    let left: Vec<_> = fs::read_dir(&state)
                        .unwrap()
                        .map(|e| e.unwrap().file_name())
                        .collect();
                    assert_eq!(left, [OsStr::new(rnk_store::PRUNING_MARKER)], "{sh:?} {c}");
                }
            }
            fs::remove_dir_all(&base).unwrap();
        }
    }
}

/// Makes `depth` nested directories `d` below `top`, by descriptor (past
/// `PATH_MAX`), with `link`, a symlink to `to`, at the bottom.
fn deep_tree(top: &Path, depth: usize, to: &Path) {
    use rustix::fs::{Mode, OFlags};
    let how = OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut fd = rustix::fs::open(top, how, Mode::empty()).unwrap();
    for _ in 0..depth {
        rustix::fs::mkdirat(&fd, "d", Mode::from_raw_mode(0o700)).unwrap();
        fd = rustix::fs::openat(&fd, "d", how, Mode::empty()).unwrap();
    }
    rustix::fs::symlinkat(to, &fd, "link").unwrap();
}

/// T4: root's command after an unfinished `prune --all --userns`, this
/// test's ids standing for the range's, in a tree a `RUN` could leave: a
/// tree deeper than `PATH_MAX` with a symlink out at its bottom, a wide
/// directory, a FIFO, a socket, a setuid file, symlink loops, a hardlink
/// to a file outside, names `find` or a shell could read as options or
/// patterns, a `.ranikust-pruning` below the top: all but the marker
/// goes, nothing outside, without blocking. A FIFO as the marker: nothing
/// changes, without blocking.
#[test]
fn red_team_userns_leftovers_deep_wide_and_special_go_without_blocking() {
    let tmp = private_tempdir();
    let outside = tmp.path().join("outside");
    fs::create_dir_all(outside.join("dir")).unwrap();
    fs::write(outside.join("keep"), b"keep").unwrap();
    fs::write(outside.join("dir/keep"), b"keep").unwrap();
    let state = tmp.path().join("state");
    pruning_state(&state);
    fs::create_dir_all(state.join("builds/deep")).unwrap();
    deep_tree(&state.join("builds/deep"), 3000, &outside);
    let wide = state.join("caches/wide");
    fs::create_dir_all(&wide).unwrap();
    for i in 0..5000 {
        fs::write(wide.join(format!("f{i}")), b"").unwrap();
    }
    let odd = state.join("layers/odd");
    fs::create_dir_all(&odd).unwrap();
    let names = "-delete -exec ( ) ! \\ ; {} *?[a] -- - -L -xdev .ranikust-pruning";
    for name in names.split(' ').chain([" lead", "trail ", "\n", "\t"]) {
        fs::write(odd.join(name), b"x").unwrap();
    }
    fs::create_dir_all(state.join("tmp/.ranikust-pruning")).unwrap();
    fs::write(state.join("tmp/.ranikust-pruning/f"), b"x").unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        odd.join("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    drop(std::os::unix::net::UnixListener::bind(odd.join("sock")).unwrap());
    fs::write(odd.join("suid"), b"x").unwrap();
    fs::set_permissions(odd.join("suid"), fs::Permissions::from_mode(0o4755)).unwrap();
    symlink("loop", odd.join("loop")).unwrap();
    symlink("b", odd.join("a")).unwrap();
    symlink("a", odd.join("b")).unwrap();
    symlink(outside.join("dir"), odd.join("out")).unwrap();
    fs::hard_link(outside.join("keep"), odd.join("hard")).unwrap();
    let victims = listing(&outside);
    let script = userns_left_script(own_range());
    let started = Instant::now();
    let out = Command::new("timeout")
        .args([
            "300",
            "bash",
            "-c",
            &in_dir(&state, pin_of(&state), &script),
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    measured(&format!(
        "root's command after prune --all --userns, 3000 deep and 5000 wide: {:?}",
        started.elapsed()
    ));
    assert!(out.status.success(), "{out:?}");
    let left: Vec<_> = fs::read_dir(&state)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, [OsStr::new(rnk_store::PRUNING_MARKER)]);
    assert_eq!(listing(&outside), victims);
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");
    assert_eq!(
        fs::symlink_metadata(outside.join("keep")).unwrap().nlink(),
        1
    );
    // A FIFO where the marker is: refused, nothing opened or changed.
    fs::remove_file(state.join(rnk_store::PRUNING_MARKER)).unwrap();
    marked_state(&state);
    rustix::fs::mknodat(
        rustix::fs::CWD,
        state.join(rnk_store::PRUNING_MARKER),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    let before = listing(tmp.path());
    let out = Command::new("timeout")
        .args(["20", "bash", "-c", &in_dir(&state, pin_of(&state), &script)])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no regular .ranikust-pruning here"),
        "{out:?}"
    );
    assert_eq!(listing(tmp.path()), before);
}

/// T4: `prune --all --userns` refuses root's default, a path below it and
/// one above it however the path is spelt (`.`, repeated or trailing
/// slashes), before the range's namespace and changing nothing; a symlink
/// at the state directory's path, followed by a trailing `/` or `/.`, or on
/// it is refused, never followed.
#[test]
fn red_team_userns_prune_refuses_roots_default_however_spelt() {
    let user_ns = fs::read_link("/proc/self/ns/user").unwrap();
    let range = || Ok(range_from(100_000, 200_000));
    let below = "is below root's default /var/lib/ranikust";
    let above = "is above root's default /var/lib/ranikust";
    for (dir, how) in [
        ("/var/lib/ranikust/.", "is root's default"),
        ("/var/lib/ranikust/", "is root's default"),
        ("//var/lib/ranikust", "is root's default"),
        ("/var//lib/./ranikust", "is root's default"),
        ("/var/lib/ranikust/./userns/", below),
        ("/var/lib/ranikust//x/y", below),
        ("/./var/lib", above),
        ("/var/", above),
        ("/", above),
    ] {
        let refusal = userns_with(Path::new(dir), range).unwrap_err();
        assert_eq!(
            refusal.reason,
            format!("the state directory {dir} {how}"),
            "{refusal}"
        );
        assert_eq!(refusal.guidance, USERNS_ONLY, "{refusal}");
    }
    let euid = rustix::process::geteuid().as_raw();
    let egid = rustix::process::getegid().as_raw();
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    pruning_state(&state);
    symlink(&state, tmp.path().join("link")).unwrap();
    symlink(tmp.path(), tmp.path().join("up")).unwrap();
    let before = listing(tmp.path());
    for dir in [
        tmp.path().join("link"),
        tmp.path().join("link/"),
        tmp.path().join("link/."),
        tmp.path().join("up/state"),
    ] {
        let refusal = userns_with(&dir, || Ok(range_from(euid, egid))).unwrap_err();
        // ELOOP reads "Symbolic link loop" in musl and "Too many levels of
        // symbolic links" in glibc.
        assert!(
            refusal.reason.to_lowercase().contains("symbolic link"),
            "{refusal}"
        );
        assert_eq!(refusal.guidance, USERNS_ONLY, "{refusal}");
    }
    assert_eq!(listing(tmp.path()), before);
    assert_eq!(fs::read_link("/proc/self/ns/user").unwrap(), user_ns);
}
