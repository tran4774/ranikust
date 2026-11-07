//! The build's user namespace and the store opened in it (#75), and
//! `prune --all` emptying a state directory there (#76).

use std::fs;
use std::io::Read as _;
use std::os::fd::{AsFd as _, AsRawFd as _};
use std::os::unix::fs::chown;
use std::path::Path;

use super::*;

/// The ids, other than its own, a dead build leaves files of: in a ranged
/// map, the first ids after root.
const OWNERS: [u32; 2] = [1, 2];

/// A fresh temp dir no one else may write, whatever the umask.
fn private_tempdir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

/// Marks the test binary [`alone`] starts.
const ALONE: &str = "RNK_USERNS_TEST_ALONE";

/// Whether the test `name` of this module runs alone in its process; if
/// not, starts this test binary again for that test only, with one test
/// thread, asserts it passed and returns false. There, the only other
/// thread is libtest's main one, waiting for the test's result: at
/// [`in_child`]'s fork, no other thread is inside mimalloc, which has no
/// fork handler.
fn alone(name: &str) -> bool {
    if std::env::var_os(ALONE).is_some() {
        return true;
    }
    let module = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, m)| m);
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            &format!("{module}::{name}"),
            "--exact",
            "--include-ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(ALONE, "1")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Not printed when it passed: the lanes count each `test result` line.
    assert!(
        out.status.success() && text.contains("test result: ok. 1 passed"),
        "{text}"
    );
    false
}

/// This process's user and mount namespaces.
fn namespaces() -> [String; 2] {
    ["user", "mnt"].map(|ns| {
        fs::read_link(format!("/proc/self/ns/{ns}"))
            .map_or_else(|e| e.to_string(), |p| p.display().to_string())
    })
}

/// Whether both namespaces differ from `before` (`new`), neither does
/// (`same`), or only one (`partly`).
fn moved(before: &[String; 2]) -> &'static str {
    let now = namespaces();
    match (now[0] == before[0], now[1] == before[1]) {
        (false, false) => "new",
        (true, true) => "same",
        (true, false) | (false, true) => "partly",
    }
}

/// Runs `f` in a forked child, as `user` (uid, gid) when given; the child
/// is single-threaded, as the set-up must be, and the subreaper of what it
/// forks. Returns what `f` printed, or an `error:` line. Only in a test
/// that runs [`alone`].
fn in_child(user: Option<(u32, u32)>, f: impl FnOnce() -> String) -> String {
    assert!(
        std::env::var_os(ALONE).is_some(),
        "only in a test run alone"
    );
    let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
    let keep = u32::try_from(w.as_raw_fd()).unwrap();
    // SAFETY: the test runs alone ([`alone`]): the only other thread waits
    // for its result, holding no lock the child's allocations need. The
    // child ends with `_exit`, dropping no owner of a descriptor it closed.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: plain system calls in this child, which uses or drops no
        // owner of what they close. Best effort: a descriptor left open
        // stays open only until this child ends.
        unsafe {
            libc::syscall(libc::SYS_close_range, 3_u32, keep - 1, 0_u32);
            libc::syscall(libc::SYS_close_range, keep + 1, u32::MAX, 0_u32);
        }
        // SAFETY: plain system calls on this single-threaded child.
        let dropped = user.is_none_or(|(uid, gid)| unsafe {
            libc::setgroups(0, std::ptr::null()) == 0
                && libc::setgid(gid) == 0
                && libc::setuid(uid) == 0
        });
        let text = if !dropped {
            "error: cannot drop to the user".to_owned()
        } else if let Err(e) = rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
        {
            format!("error: PR_SET_CHILD_SUBREAPER: {e}")
        } else {
            // A panic must not unwind into the harness's copy here.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .unwrap_or_else(|_| "error: panicked".to_owned())
        };
        let mut rest = text.as_bytes();
        while let Ok(n @ 1..) = rustix::io::write(w.as_fd(), rest) {
            rest = &rest[n..];
        }
        // SAFETY: `_exit` ends this process at once.
        unsafe { libc::_exit(0) }
    }
    let pid = rustix::process::Pid::from_raw(pid).expect("fork");
    drop(w);
    let mut out = String::new();
    fs::File::from(r).read_to_string(&mut out).unwrap();
    let waited = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());
    let Ok(Some((_, status))) = waited else {
        panic!("waitpid: {waited:?}: {out}");
    };
    assert!(status.exited(), "{status:?}: {out}");
    out
}

/// Files and a read-only directory of [`OWNERS`] in `dir`, as a `RUN`
/// leaves them: only root of the namespace may remove them.
fn plant(dir: &Path) -> std::io::Result<()> {
    let file = dir.join("f");
    fs::write(&file, b"x")?;
    chown(&file, Some(OWNERS[0]), Some(OWNERS[0]))?;
    let ro = dir.join("ro");
    fs::create_dir(&ro)?;
    fs::write(ro.join("g"), b"y")?;
    chown(ro.join("g"), Some(OWNERS[1]), Some(OWNERS[1]))?;
    chown(&ro, Some(OWNERS[1]), Some(OWNERS[1]))?;
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555))
}

/// A build that enters the user namespace, opens the store there, leaves
/// [`plant`]'s files in its `tmp/` and `builds/` directories, and dies
/// without closing the store. `map\tid`, or `refused\t<refusal>`.
fn dead_build(state: &Path, user: Option<(u32, u32)>) -> String {
    dead_build_leaving(state, user, plant)
}

/// [`dead_build`], leaving what `plant` makes in each of its directories.
fn dead_build_leaving(
    state: &Path,
    user: Option<(u32, u32)>,
    plant: impl Fn(&Path) -> std::io::Result<()>,
) -> String {
    in_child(user, || {
        let before = namespaces();
        let set_up = match enter_user_namespace(state) {
            Ok(s) => s,
            Err(r) => return format!("refused\t{r}"),
        };
        let m = moved(&before);
        if m != "new" {
            return format!("error: the namespaces: {m}");
        }
        let id = set_up.store.build_id().to_owned();
        let dirs = [
            set_up.store.build_dir().to_owned(),
            set_up.store.state_dir().join("tmp").join(&id),
        ];
        for d in dirs {
            if let Err(e) = plant(&d) {
                return format!("error: plant in {}: {e}", d.display());
            }
        }
        // Dropped unclosed: its build lock goes with it, as with the process.
        format!("{}\t{id}", set_up.userns.map)
    })
}

/// The next build: opens the store in the namespace, whose start-up
/// clean-up removes the dead build `id`, then closes it. `gone`, or what
/// is left, or the refusal.
fn next_build(state: &Path, user: Option<(u32, u32)>, id: &str) -> String {
    let id = id.to_owned();
    in_child(user, move || {
        let before = namespaces();
        let set_up = match enter_user_namespace(state) {
            Ok(s) => s,
            Err(r) => return format!("refused\t{r}"),
        };
        let m = moved(&before);
        if m != "new" {
            return format!("error: the namespaces: {m}");
        }
        let left: Vec<String> = ["builds", "tmp"]
            .iter()
            .map(|d| set_up.store.state_dir().join(d).join(&id))
            .chain([set_up.store.state_dir().join(format!("builds/{id}.lock"))])
            .filter(|p| p.symlink_metadata().is_ok())
            .map(|p| p.display().to_string())
            .collect();
        if let Err(e) = set_up.store.close() {
            return format!("error: close: {e}");
        }
        if left.is_empty() {
            "gone".to_owned()
        } else {
            format!("left: {}", left.join(" "))
        }
    })
}

/// Whatever the machine allows: the state directory passes its checks
/// outside, then either the build's user namespace is entered and the
/// store opened in it, as root of the namespace, or the set-up is refused,
/// naming the maps it tried, before the store.
#[test]
fn the_store_opens_inside_the_user_namespace_or_not_at_all() {
    if !alone("the_store_opens_inside_the_user_namespace_or_not_at_all") {
        return;
    }
    let euid = rustix::process::geteuid().as_raw();
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    let out = in_child(None, || {
        let before = namespaces();
        match enter_user_namespace(&state) {
            Ok(s) => {
                let owner = fs::metadata(s.store.state_dir()).map(|m| m.uid());
                let changed = moved(&before);
                let line = format!(
                    "ok\t{}\t{changed}\t{}\t{owner:?}",
                    s.userns.map,
                    rustix::process::geteuid().as_raw()
                );
                match s.store.close() {
                    Ok(()) => line,
                    Err(e) => format!("error: close: {e}"),
                }
            }
            Err(r) => format!(
                "refused\t{}\t{}\t{:?}\t{}",
                r.reason,
                r.guidance,
                r.kind,
                moved(&before)
            ),
        }
    });
    let st = fs::metadata(&state).unwrap();
    assert_eq!((st.uid(), st.mode() & 0o7777), (euid, 0o700), "{out}");
    let marker = state.join(rnk_store::STATE_MARKER).exists();
    match out.split('\t').collect::<Vec<_>>()[..] {
        ["ok", _map, changed, inside, owner] => {
            assert_eq!((changed, inside, owner), ("new", "0", "Ok(0)"), "{out}");
            assert!(marker, "{out}");
        }
        ["refused", reason, guidance, kind, "same"] => {
            assert!(
                reason.starts_with("no user namespace for the build: "),
                "{out}"
            );
            let want = if euid == 0 { "" } else { NO_USER_NAMESPACE };
            assert_eq!((guidance, kind), (want, "Other"), "{out}");
            assert!(!marker, "the store opened outside the namespace: {out}");
        }
        _ => panic!("{out}"),
    }
}

/// T1, as root of a rootless Podman container, whose ids are a range of
/// the host user's: a build that died with files and a read-only directory
/// of other ids in its `tmp/` and `builds/` directories is removed by the
/// next build's store, opened in the build's user namespace.
#[test]
#[ignore = "T1: default: root in a rootless Podman container enters the build's user namespace"]
fn the_store_in_the_user_namespace_removes_a_dead_build_of_other_ids() {
    if !alone("the_store_in_the_user_namespace_removes_a_dead_build_of_other_ids") {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    let dead = dead_build(&state, None);
    let Some(("multi-range", id)) = dead.split_once('\t') else {
        panic!("{dead}");
    };
    for d in ["builds", "tmp"] {
        let dir = state.join(d).join(id);
        assert_eq!(fs::metadata(dir.join("f")).unwrap().uid(), OWNERS[0]);
        assert_eq!(fs::metadata(dir.join("ro/g")).unwrap().uid(), OWNERS[1]);
    }
    assert_eq!(next_build(&state, None, id), "gone");
    for d in ["builds", "tmp"] {
        assert!(!state.join(d).join(id).exists(), "{d}/{id}");
    }
}

/// T2: as the user who ran sudo (the VM's `rnk`), with the user's
/// subordinate ids. Where AppArmor restricts unprivileged user namespaces
/// (Ubuntu 24.04), no map passes and the store is never opened; elsewhere
/// (22.04) a dead build's files of subordinate ids, which the user cannot
/// remove outside, are removed by the next build's store in the namespace.
#[test]
#[ignore = "T2: drops from root to the sudo user to open the store in the build's user namespace"]
fn a_regular_user_s_store_removes_a_dead_build_of_subordinate_ids() {
    if !alone("a_regular_user_s_store_removes_a_dead_build_of_subordinate_ids") {
        return;
    }
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok());
    let (Some(uid), Some(gid)) = (var("SUDO_UID"), var("SUDO_GID")) else {
        panic!("run through sudo by a regular user, as the T2 unit suite does");
    };
    assert_ne!(uid, 0, "run through sudo by a regular user");
    let user = Some((uid, gid));
    let restricted = fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1");
    let tmp = private_tempdir();
    chown(tmp.path(), Some(uid), Some(gid)).unwrap();
    let state = tmp.path().join("state");
    let dead = dead_build(&state, user);
    if restricted {
        assert!(
            dead.starts_with("refused\tno user namespace for the build: "),
            "{dead}"
        );
        assert!(dead.ends_with(NO_USER_NAMESPACE), "{dead}");
        assert!(!state.join(rnk_store::STATE_MARKER).exists(), "{dead}");
        return;
    }
    let sub = crate::subids::lookup(uid)
        .unwrap()
        .expect("the VM's user has subordinate ids");
    let Some(("newuidmap", id)) = dead.split_once('\t') else {
        panic!("{dead}");
    };
    // Outside, the files are the subordinate ids', from the second one on
    // (the namespace's 0 is the user).
    let host = |ns: u32| sub.uids.start + ns - 1;
    for d in ["builds", "tmp"] {
        let dir = state.join(d).join(id);
        assert_eq!(fs::metadata(dir.join("f")).unwrap().uid(), host(OWNERS[0]));
        assert_eq!(
            fs::metadata(dir.join("ro/g")).unwrap().uid(),
            host(OWNERS[1])
        );
    }
    // The control: outside the namespace, the user cannot remove them.
    let g = state.join("tmp").join(id).join("ro/g");
    let outside = in_child(user, || match fs::remove_file(&g) {
        Ok(()) => "removed".to_owned(),
        Err(e) => format!("{:?}", e.kind()),
    });
    assert_eq!(outside, "PermissionDenied");
    assert_eq!(next_build(&state, user, id), "gone");
    for d in ["builds", "tmp"] {
        assert!(!state.join(d).join(id).exists(), "{d}/{id}");
    }
}

/// What [`plant_links_out`] aims at: files of the namespace's root and of
/// [`OWNERS`] in `outside`, beside the state directory.
fn plant_victims(outside: &Path) -> std::io::Result<()> {
    fs::create_dir(outside)?;
    fs::write(outside.join("own"), b"keep")?;
    fs::write(outside.join("sub"), b"keep")?;
    chown(outside.join("sub"), Some(OWNERS[0]), Some(OWNERS[0]))?;
    let dir = outside.join("dir");
    fs::create_dir(&dir)?;
    fs::write(dir.join("f"), b"keep")?;
    chown(dir.join("f"), Some(OWNERS[1]), Some(OWNERS[1]))?;
    chown(&dir, Some(OWNERS[1]), Some(OWNERS[1]))
}

/// What a `RUN` of a dead build may leave in `dir` (`<state>/<tmp or
/// builds>/<id>`), aimed at `outside`: symlinks out (absolute, climbing,
/// to a file, at the end of a deep tree), hardlinks to files of each
/// owner, a loop, a FIFO, a socket and a directory no one may search.
fn plant_links_out(dir: &Path, outside: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::symlink;
    symlink(outside, dir.join("abs"))?;
    symlink("../../../outside", dir.join("rel"))?;
    symlink(outside.join("own"), dir.join("file"))?;
    for (name, target) in [
        ("hard-own", "own"),
        ("hard-sub", "sub"),
        ("hard-f", "dir/f"),
    ] {
        fs::hard_link(outside.join(target), dir.join(name))?;
    }
    symlink("loop", dir.join("loop"))?;
    rustix::fs::mknodat(
        rustix::fs::CWD,
        dir.join("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )?;
    drop(std::os::unix::net::UnixListener::bind(dir.join("sock"))?);
    let deep: std::path::PathBuf = std::iter::once(dir.join("deep"))
        .chain(std::iter::repeat_n("d".into(), 100))
        .collect();
    fs::create_dir_all(&deep)?;
    symlink(outside, deep.join("out"))?;
    let shut = dir.join("shut");
    fs::create_dir(&shut)?;
    fs::write(shut.join("x"), b"x")?;
    chown(&shut, Some(OWNERS[1]), Some(OWNERS[1]))?;
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o000))
}

/// Ids of builds that never ran, as a dead build may leave them: `tmp/` and
/// `builds/` entries that are symlinks to `outside`, and a build whose lock
/// file is a symlink to a file there.
const GHOSTS: [&str; 2] = ["ghost-links", "ghost-lock"];

/// [`GHOSTS`] in `state`, aimed at `outside`.
fn plant_ghosts(state: &Path, outside: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::symlink;
    symlink(outside, state.join("tmp").join(GHOSTS[0]))?;
    symlink(outside.join("dir"), state.join("builds").join(GHOSTS[0]))?;
    fs::create_dir(state.join("tmp").join(GHOSTS[1]))?;
    symlink(
        outside.join("own"),
        state.join("builds").join(format!("{}.lock", GHOSTS[1])),
    )
}

/// One entry of [`tree`].
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    path: String,
    uid: u32,
    gid: u32,
    mode: u32,
    /// The contents of a regular file.
    body: Vec<u8>,
    links: u64,
}

/// Every entry below `dir`, never following a symlink.
fn tree(dir: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                todo.push(p.clone());
            }
            out.push(Entry {
                path: p.strip_prefix(dir).unwrap().display().to_string(),
                uid: m.uid(),
                gid: m.gid(),
                mode: m.mode(),
                body: if m.is_file() {
                    fs::read(&p).unwrap()
                } else {
                    Vec::new()
                },
                links: m.nlink(),
            });
        }
    }
    out.sort();
    out
}

/// The dead build of [`dead_build_leaving`] with [`plant_links_out`]'s
/// links and [`GHOSTS`], then the next build's clean-up as root of the
/// namespace: what lies outside the state directory keeps its contents,
/// owners and modes, and only the planted links to it go. `None` when the
/// set-up was refused (asserted by the caller), else the dead build's map.
fn links_out_stay_out(tmp: &Path, user: Option<(u32, u32)>) -> Option<String> {
    let state = tmp.join("state");
    let outside = tmp.join("outside");
    let dead = dead_build_leaving(&state, user, |d| {
        if !outside.exists() {
            plant_victims(&outside)?;
            plant_ghosts(&state, &outside)?;
        }
        plant(d)?;
        plant_links_out(d, &outside)
    });
    if dead.starts_with("refused\t") {
        return None;
    }
    let Some((map, id)) = dead.split_once('\t') else {
        panic!("{dead}");
    };
    let mut before = tree(&outside);
    let names: Vec<&str> = before.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(names, ["dir", "dir/f", "own", "sub"], "{dead}");
    // Each file outside has a link from both of the dead build's directories.
    for e in &mut before {
        if e.path != "dir" {
            assert_eq!(e.links, 3, "{e:?}");
            e.links = 1;
        }
    }
    assert_eq!(next_build(&state, user, id), "gone");
    assert_eq!(tree(&outside), before);
    assert_eq!(fs::read(outside.join("own")).unwrap(), b"keep");
    // The ghost whose entries are links: unlinked, never followed.
    for d in ["tmp", "builds"] {
        assert!(
            fs::symlink_metadata(state.join(d).join(GHOSTS[0])).is_err(),
            "{d}/{}",
            GHOSTS[0]
        );
    }
    Some(map.to_owned())
}

/// T1, as root of a rootless Podman container: a dead build's links and
/// hardlinks to files outside the state directory, of root and of other
/// ids, are removed by the next build's store in the user namespace; what
/// they lead to is left as it was (T4).
#[test]
#[ignore = "T1: default: root in a rootless Podman container enters the build's user namespace"]
fn red_team_a_dead_build_s_links_out_are_removed_and_never_followed() {
    if !alone("red_team_a_dead_build_s_links_out_are_removed_and_never_followed") {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let map = links_out_stay_out(tmp.path(), None).expect("the set-up was refused");
    assert_eq!(map, "multi-range");
}

/// T2, as the user who ran sudo, with the user's subordinate ids: the same
/// with files of subordinate ids, removed through `newuidmap`'s map.
/// Where AppArmor restricts unprivileged user namespaces (Ubuntu 24.04),
/// the set-up is refused and nothing outside is made.
#[test]
#[ignore = "T2: drops from root to the sudo user to open the store in the build's user namespace"]
fn red_team_a_regular_user_s_dead_build_links_out_are_removed_and_never_followed() {
    if !alone("red_team_a_regular_user_s_dead_build_links_out_are_removed_and_never_followed") {
        return;
    }
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok());
    let (Some(uid), Some(gid)) = (var("SUDO_UID"), var("SUDO_GID")) else {
        panic!("run through sudo by a regular user, as the T2 unit suite does");
    };
    assert_ne!(uid, 0, "run through sudo by a regular user");
    let restricted = fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1");
    let tmp = private_tempdir();
    chown(tmp.path(), Some(uid), Some(gid)).unwrap();
    match links_out_stay_out(tmp.path(), Some((uid, gid))) {
        None => {
            assert!(
                restricted,
                "refused where user namespaces are not restricted"
            );
            assert!(!tmp.path().join("outside").exists());
        }
        Some(map) => {
            assert!(!restricted, "{map}");
            assert_eq!(map, "newuidmap");
        }
    }
}

/// T1, as root of a rootless Podman container dropping to another uid: a
/// `$HOME` another user owns or may let others write (a `sudo -u` that kept
/// the caller's `$HOME`, a shared sticky directory, `/`) gives a default
/// state directory that is refused before anything is made in it (T4).
#[test]
#[ignore = "T1: default: root in a rootless Podman container makes homes of other ids"]
fn red_team_a_home_of_another_user_gives_no_state_directory() {
    if !alone("red_team_a_home_of_another_user_gives_no_state_directory") {
        return;
    }
    assert!(rustix::process::geteuid().is_root(), "run as root");
    const ME: u32 = 3000;
    const OTHER: u32 = 2000;
    let tmp = private_tempdir();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let home = |name: &str, owner: u32, mode: u32| {
        let h = tmp.path().join(name);
        fs::create_dir(&h).unwrap();
        chown(&h, Some(owner), Some(owner)).unwrap();
        fs::set_permissions(&h, fs::Permissions::from_mode(mode)).unwrap();
        h
    };
    let others = home("other", OTHER, 0o755);
    let local = others.join(".local");
    fs::create_dir(&local).unwrap();
    chown(&local, Some(OTHER), Some(OTHER)).unwrap();
    let sticky = home("sticky", OTHER, 0o1777);
    let my_sticky = home("my-sticky", ME, 0o1777);
    let group = home("group", ME, 0o775);
    let mine = home("mine", ME, 0o755);
    for (h, why) in [
        (others.as_path(), format!("is owned by uid {OTHER}")),
        (sticky.as_path(), format!("is owned by uid {OTHER}")),
        (
            my_sticky.as_path(),
            "writable by group or others".to_owned(),
        ),
        (group.as_path(), "writable by group or others".to_owned()),
        (
            Path::new("/"),
            "cannot create the state directory".to_owned(),
        ),
    ] {
        let state = crate::config::default_state_dir(ME, Some(h.as_os_str())).unwrap();
        let out = in_child(Some((ME, ME)), || match crate::statedir::prepare(&state) {
            Ok(()) => "made".to_owned(),
            Err(r) => r.0,
        });
        assert!(out.contains(&why), "{}: {out}", h.display());
        let made: Vec<_> = fs::read_dir(h)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let want: &[&str] = match h {
            _ if h == others => &[".local"],
            _ => &[],
        };
        if h != Path::new("/") {
            assert_eq!(made, want, "{}", h.display());
        }
    }
    assert!(!local.join("share").exists());
    // The control: the user's own home gets it, the user's and private.
    let state = crate::config::default_state_dir(ME, Some(mine.as_os_str())).unwrap();
    let out = in_child(Some((ME, ME)), || match crate::statedir::prepare(&state) {
        Ok(()) => "made".to_owned(),
        Err(r) => r.0,
    });
    assert_eq!(out, "made");
    let st = fs::metadata(&state).unwrap();
    assert_eq!((st.uid(), st.mode() & 0o7777), (ME, 0o700));
}

/// A rootless build as `user` (else as this process's user): enters the
/// build's user namespace, publishes a blob, and a layer and a cache mount
/// holding [`plant`]'s files of other ids; leaves the same in its `tmp/`
/// and `builds/` directories, with what `extra` makes there; dies without
/// closing the store. `map\tid`, or `refused\t<refusal>`.
fn rootless_build(
    state: &Path,
    user: Option<(u32, u32)>,
    extra: impl Fn(&Path) -> std::io::Result<()>,
) -> String {
    in_child(user, || {
        let set_up = match enter_user_namespace(state) {
            Ok(s) => s,
            Err(r) => return format!("refused\t{r}"),
        };
        let store = &set_up.store;
        let digest = |n: u8| rnk_oci::Digest::from_sha256_bytes(&[n; 32]);
        let blob = store.fill(&rnk_store::EntryKey::Blob(digest(1)), |e| {
            std::io::Write::write_all(&mut e.file().expect("a blob"), b"blob")?;
            Ok::<_, std::io::Error>(4)
        });
        if let Err(e) = blob {
            return format!("error: blob: {e}");
        }
        let layer = store.fill(&rnk_store::EntryKey::Layer(digest(2)), |e| {
            plant(e.path()).map(|()| 0)
        });
        if let Err(e) = layer {
            return format!("error: layer: {e}");
        }
        let cache = store
            .cache_lease("cache", rnk_store::CacheSharing::Shared, None, &|| false)
            .map_err(|e| e.to_string())
            .and_then(|lease| plant(lease.path()).map_err(|e| e.to_string()));
        if let Err(e) = cache {
            return format!("error: cache: {e}");
        }
        let id = store.build_id().to_owned();
        let dirs = [
            store.build_dir().to_owned(),
            store.state_dir().join("tmp").join(&id),
        ];
        for d in dirs {
            if let Err(e) = plant(&d).and_then(|()| extra(&d)) {
                return format!("error: plant in {}: {e}", d.display());
            }
        }
        // Dropped unclosed, as by a killed build.
        format!("{}\t{id}", set_up.userns.map)
    })
}

/// Runs `f` (a `prune`) on `state` in a child as `user`: `Emptied`,
/// `Empty`, `Missing` or `refused\t<reason>\t<guidance>\t<kind>`, then `\t`
/// and the processes left once it returned (the child is their
/// subreaper): `none` when nothing outlived it.
fn prune_in_child(
    state: &Path,
    user: Option<(u32, u32)>,
    f: impl FnOnce(&Path) -> Result<crate::prune::Pruned, Refusal>,
) -> String {
    in_child(user, || {
        let out = match f(state) {
            Ok(pruned) => format!("{pruned:?}"),
            Err(r) => format!("refused\t{}\t{}\t{:?}", r.reason, r.guidance, r.kind),
        };
        let left = match rustix::process::waitpid(None, rustix::process::WaitOptions::NOHANG) {
            Err(rustix::io::Errno::CHILD) => "none".to_owned(),
            other => format!("{other:?}"),
        };
        format!("{out}\t{left}")
    })
}

/// This process's mounts, to tell that a child left none.
fn mounts() -> String {
    fs::read_to_string("/proc/self/mountinfo").unwrap()
}

/// The owners of everything below `dir`.
fn owners_below(dir: &Path) -> std::collections::BTreeSet<u32> {
    tree(dir).into_iter().map(|e| e.uid).collect()
}

/// [`plant_links_out`] in `dir`, aimed at `outside`, made with its victims
/// ([`plant_victims`]) and [`GHOSTS`] on the first call: an `extra` for
/// [`rootless_build`].
fn links_out(state: &Path, outside: &Path, dir: &Path) -> std::io::Result<()> {
    if !outside.exists() {
        plant_victims(outside)?;
        plant_ghosts(state, outside)?;
    }
    plant_links_out(dir, outside)
}

/// [`tree`] of `outside` as it must be once the hardlinks [`links_out`]
/// made from both of a build's directories are gone.
fn outside_without_links(outside: &Path) -> Vec<Entry> {
    let mut entries = tree(outside);
    let names: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(names, ["dir", "dir/f", "own", "sub"]);
    for e in &mut entries {
        if e.path != "dir" {
            assert_eq!(e.links, 3, "{e:?}");
            e.links = 1;
        }
    }
    entries
}

/// Whatever the machine allows, as the test user: `prune --all` on a state
/// directory a build left either empties it in the build's user namespace,
/// marker included, or is refused before the namespace, saying what to do,
/// with the directory unchanged. Either way no process or mount is left.
#[test]
fn prune_all_empties_in_the_user_namespace_or_changes_nothing() {
    if !alone("prune_all_empties_in_the_user_namespace_or_changes_nothing") {
        return;
    }
    let euid = rustix::process::geteuid().as_raw();
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    // A build that ran in no namespace: the test user's own files.
    let store = Store::open(&BuildPaths::new(&state, &state)).unwrap();
    store
        .fill(
            &rnk_store::EntryKey::Blob(rnk_oci::Digest::from_sha256_bytes(&[3; 32])),
            |e| {
                std::io::Write::write_all(&mut e.file().expect("a blob"), b"blob")?;
                Ok::<_, std::io::Error>(4)
            },
        )
        .unwrap();
    store.close().unwrap();
    let before = tree(&state);
    let mounted = mounts();
    let out = prune_in_child(&state, None, |s| crate::prune::prune_all(euid, s));
    assert_eq!(mounts(), mounted);
    match out.split('\t').collect::<Vec<_>>()[..] {
        ["Emptied", "none"] => {
            assert_ne!(euid, 0, "{out}");
            assert!(tree(&state).is_empty(), "{out}");
        }
        ["refused", reason, guidance, "Other", "none"] => {
            if euid == 0 {
                assert!(reason.contains("refuses root"), "{out}");
            } else {
                assert!(
                    reason.starts_with("no user namespace for the build: "),
                    "{out}"
                );
                assert_eq!(guidance, crate::prune::no_user_namespace(&state), "{out}");
            }
            assert_eq!(tree(&state), before, "{out}");
        }
        _ => panic!("{out}"),
    }
}

/// T1, as root of a rootless Podman container, whose ids are a range of
/// the host user's: after a rootless build that left files and read-only
/// directories of other ids in a layer, a cache mount and its own
/// directories, with links out of the state directory, `prune --all`'s
/// emptying in the build's user namespace leaves the directory empty, to
/// be removed with `rmdir`; nothing outside it changes, and no process or
/// mount is left.
#[test]
#[ignore = "T1: default: root in a rootless Podman container enters the build's user namespace"]
fn prune_all_empties_a_state_directory_of_other_ids() {
    if !alone("prune_all_empties_a_state_directory_of_other_ids") {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    let outside = tmp.path().join("outside");
    let built = rootless_build(&state, None, |d| links_out(&state, &outside, d));
    let Some(("multi-range", _)) = built.split_once('\t') else {
        panic!("{built}");
    };
    let owners = owners_below(&state);
    assert!(owners.is_superset(&OWNERS.into()), "{owners:?}");
    let outside_after = outside_without_links(&outside);
    let mounted = mounts();
    // Below the root check, which refuses this container's root (the next
    // test): the namespace, then the store's emptying.
    let out = prune_in_child(&state, None, crate::prune::empty);
    assert_eq!(out, "Emptied\tnone");
    assert_eq!(mounts(), mounted);
    assert_eq!(tree(&outside), outside_after);
    assert!(tree(&state).is_empty());
    fs::remove_dir(&state).unwrap();
}

/// T1, as root of a rootless Podman container: euid 0 in a user namespace
/// is root all the same, so `prune --all` refuses it before it looks at
/// the state directory, naming Phase 5's `prune` (ADR-0105).
#[test]
#[ignore = "T1: default: root of a rootless Podman container is refused"]
fn prune_all_refuses_root_of_a_rootless_container() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let map = fs::read_to_string("/proc/self/uid_map").unwrap();
    let whole = map.split_whitespace().eq(["0", "0", "4294967295"]);
    assert!(!whole, "real root, not a container's: {map}");
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    Store::open(&BuildPaths::new(&state, &state))
        .unwrap()
        .close()
        .unwrap();
    let before = tree(&state);
    let euid = rustix::process::geteuid().as_raw();
    let refusal = crate::prune::prune_all(euid, &state).unwrap_err();
    assert!(refusal.reason.contains("refuses root"), "{refusal}");
    assert!(refusal.guidance.contains("Phase 5"), "{refusal}");
    assert_eq!(tree(&state), before);
}

/// T1, as root of a rootless Podman container: while a build holds the
/// store, `prune --all` in the build's user namespace removes nothing; once
/// the build ended, it empties the directory.
#[test]
#[ignore = "T1: default: root in a rootless Podman container enters the build's user namespace"]
fn prune_all_leaves_what_a_running_build_uses() {
    if !alone("prune_all_leaves_what_a_running_build_uses") {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    let built = rootless_build(&state, None, |_| Ok(()));
    let Some(("multi-range", _)) = built.split_once('\t') else {
        panic!("{built}");
    };
    // A build running beside, its store open in this process.
    let running = Store::open(&BuildPaths::new(&state, &state)).unwrap();
    fs::write(running.build_dir().join("in-use"), b"x").unwrap();
    let before = tree(&state);
    let out = prune_in_child(&state, None, crate::prune::empty);
    assert!(
        out.starts_with("refused\ta build is using the state directory "),
        "{out}"
    );
    assert!(out.ends_with("\tBroken\tnone"), "{out}");
    assert_eq!(tree(&state), before);
    running.close().unwrap();
    assert_eq!(
        prune_in_child(&state, None, crate::prune::empty),
        "Emptied\tnone"
    );
    assert!(tree(&state).is_empty());
}

/// T2: as the user who ran sudo (the VM's `rnk`), with the user's
/// subordinate ids through `newuidmap` (Ubuntu 22.04): after a rootless
/// build, `prune --all` empties the state directory of files of
/// subordinate ids the user cannot remove outside the namespace, and the
/// user then removes it with `rmdir`; nothing outside changes, and no
/// process or mount is left. Where AppArmor restricts unprivileged user
/// namespaces (Ubuntu 24.04), it is refused, naming what to do, and the
/// state directory stays as it is.
#[test]
#[ignore = "T2: drops from root to the sudo user to prune in the build's user namespace"]
fn a_regular_user_s_prune_all_empties_a_state_directory_of_subordinate_ids() {
    if !alone("a_regular_user_s_prune_all_empties_a_state_directory_of_subordinate_ids") {
        return;
    }
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok());
    let (Some(uid), Some(gid)) = (var("SUDO_UID"), var("SUDO_GID")) else {
        panic!("run through sudo by a regular user, as the T2 unit suite does");
    };
    assert_ne!(uid, 0, "run through sudo by a regular user");
    let user = Some((uid, gid));
    let restricted = fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1");
    let tmp = private_tempdir();
    chown(tmp.path(), Some(uid), Some(gid)).unwrap();
    let state = tmp.path().join("state");
    let outside = tmp.path().join("outside");
    // The whole command's checks, the root one included: the user is not
    // root.
    let prune = |s: &Path| crate::prune::prune_all(rustix::process::geteuid().as_raw(), s);
    let mounted = mounts();
    if restricted {
        // A state directory of the user's own files, made in no namespace.
        let made = in_child(user, || {
            match Store::open(&BuildPaths::new(&state, &state)).and_then(Store::close) {
                Ok(()) => "made".to_owned(),
                Err(e) => format!("error: {e}"),
            }
        });
        assert_eq!(made, "made");
        let before = tree(&state);
        let out = prune_in_child(&state, user, prune);
        assert!(
            out.starts_with("refused\tno user namespace for the build: "),
            "{out}"
        );
        let guidance = crate::prune::no_user_namespace(&state);
        assert!(
            out.ends_with(&format!("\t{guidance}\tOther\tnone")),
            "{out}"
        );
        assert_eq!(tree(&state), before);
        assert_eq!(mounts(), mounted);
        return;
    }
    let built = rootless_build(&state, user, |d| links_out(&state, &outside, d));
    let Some(("newuidmap", id)) = built.split_once('\t') else {
        panic!("{built}");
    };
    let sub = crate::subids::lookup(uid)
        .unwrap()
        .expect("the VM's user has subordinate ids");
    // Outside, the files are the subordinate ids', from the second one on
    // (the namespace's 0 is the user).
    let host = |ns: u32| sub.uids.start + ns - 1;
    let owners = owners_below(&state);
    assert!(
        owners.contains(&host(OWNERS[0])) && owners.contains(&host(OWNERS[1])),
        "{owners:?}"
    );
    // The control: outside the namespace, the user cannot remove them.
    let g = state.join("tmp").join(id).join("ro/g");
    let outside_try = in_child(user, || match fs::remove_file(&g) {
        Ok(()) => "removed".to_owned(),
        Err(e) => format!("{:?}", e.kind()),
    });
    assert_eq!(outside_try, "PermissionDenied");
    let outside_after = outside_without_links(&outside);
    assert_eq!(prune_in_child(&state, user, prune), "Emptied\tnone");
    assert_eq!(mounts(), mounted);
    assert_eq!(tree(&outside), outside_after);
    assert!(tree(&state).is_empty());
    let removed = in_child(user, || match fs::remove_dir(&state) {
        Ok(()) => "removed".to_owned(),
        Err(e) => e.to_string(),
    });
    assert_eq!(removed, "removed");
}
