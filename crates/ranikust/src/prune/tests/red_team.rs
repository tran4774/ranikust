//! Red team of `prune --all` and the commands its refusals print (T4):
//! paths a shell may read otherwise, real tools that are not GNU `find`,
//! directories swapped while GNU `find` walks, system directories; as root
//! (T2c), entries of other ids, special files and links out, and a
//! filesystem mounted below.

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
    let start = line
        .find("( CDPATH= cd -P -- ")
        .unwrap_or_else(|| panic!("no commands: {line}"));
    let then = line.rfind(", then ( CDPATH= cd -P -- ").expect(line);
    let end = line
        .rfind("; Phase 5's ranikust prune will serve root too")
        .expect(line);
    (
        line[start..then].to_owned(),
        line[then + ", then ".len()..end].to_owned(),
    )
}

/// `program` found on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// The shells on this machine, each as its program and the arguments
/// before `-c`.
fn shells() -> Vec<Vec<&'static str>> {
    [
        vec!["bash"],
        vec!["zsh", "-f"],
        vec!["dash"],
        vec!["ksh"],
        vec!["mksh"],
        vec!["busybox", "sh"],
    ]
    .into_iter()
    .filter(|sh| {
        Command::new(sh[0])
            .args(&sh[1..])
            .args(["-c", "true"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
    .collect()
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
                let at = cwd.join(OsStr::from_bytes(&read.stdout));
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
            root_script(Owner::of(owner)),
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
#[test]
fn red_team_directories_swapped_during_the_walk_lead_nowhere_out() {
    use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
    const DIRS: usize = 10;
    const FILES: usize = 400;
    let tmp = private_tempdir();
    let me = pin_of(tmp.path()).uid;
    let scripts = [
        ROOT_SCRIPT.to_owned(),
        root_script(Owner::of(me)),
        USER_SCRIPT.to_owned(),
    ];
    let names: Vec<String> = (0..DIRS).map(|d| format!("w{d:02}")).collect();
    let (outside, aside, state) = (
        tmp.path().join("outside"),
        tmp.path().join("aside"),
        tmp.path().join("state"),
    );
    let walked = state.join("caches/k");
    for script in &scripts {
        // A round whose racer saw no deletion in time is run again.
        let mut swapped = 0;
        for _ in 0..3 {
            marked_state(&state);
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

/// T4: a user's `prune --all` refuses `/`, system directories and the
/// home directory before any user namespace; root's refusal gives no
/// commands for them.
#[test]
fn red_team_system_directories_and_the_home_get_nothing() {
    let user_ns = fs::read_link("/proc/self/ns/user").unwrap();
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
        match empty(dir) {
            Ok(pruned) => panic!("{}: {pruned:?}", dir.display()),
            Err(refusal) => assert_eq!(refusal.kind, Kind::Other, "{refusal:?}"),
        }
        let guidance = prune_all(0, dir).unwrap_err().guidance;
        assert!(!guidance.contains("cd -P"), "{}: {guidance}", dir.display());
    }
    assert_eq!(fs::read_link("/proc/self/ns/user").unwrap(), user_ns);
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

/// In `state`, owned by [`OWNER`], what its builds could leave: entries of
/// each of `ids`, a setuid file, a device node, a FIFO, a socket, names a
/// shell would read otherwise, a symlink to `outside/dir` and a hardlink
/// to `outside/owned`, both [`OWNER`]'s.
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
/// root as printed, delete the entries of its owner and of its subordinate
/// ids, special files and links out among them; entries of other ids stay,
/// and the hardlinked and linked-to files outside are unchanged; the
/// emptying then stops, saying so, and the `rmdir` fails. With no other
/// id's entry, both remove the directory.
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
    let emptied = bash(&emptying, tmp.path());
    let removed = bash(&removal, tmp.path());
    assert!(!emptied.status.success(), "{emptied:?}");
    let err = String::from_utf8_lossy(&emptied.stderr);
    assert!(
        err.contains(&format!("not emptied: entries of uids other than {OWNER}")),
        "{err}"
    );
    assert!(!removed.status.success(), "{removed:?}");
    let mut left: Vec<String> = owned(&state)
        .into_iter()
        .map(|(p, ..)| p.display().to_string())
        .collect();
    left.sort();
    let mut want: Vec<String> = [
        "builds",
        "builds/x",
        "caches",
        "caches/c",
        "caches/c/hard-root",
        "tmp",
        "tmp/b",
    ]
    .iter()
    .map(|p| (*p).to_owned())
    .chain(others.iter().map(|id| format!("tmp/b/o-{id}")))
    .collect();
    want.sort();
    assert_eq!(left, want, "{err}");
    assert_eq!(nlink("secret"), 2);
    assert_eq!(nlink("owned"), 1);
    assert_eq!(owned(&outside), before);
    // The owner's ids only: both commands remove it.
    let state = parent.join("second");
    owners_state(&state, &ids, &outside);
    let (emptying, removal) = printed_for_root(&state);
    let emptied = bash(&emptying, tmp.path());
    let removed = bash(&removal, tmp.path());
    assert!(emptied.status.success(), "{emptied:?}");
    assert!(removed.status.success(), "{removed:?}");
    assert!(!state.exists());
    assert_eq!(nlink("owned"), 1);
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
