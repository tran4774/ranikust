//! The build's user namespace and the store opened in it (#75).

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

/// Whether the test `name` of this module runs alone in its process. Else
/// runs it again so, in this test binary with one test thread, asserts it
/// passed and returns false. There, the only other
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
fn namespaces() -> String {
    ["user", "mnt"]
        .map(|ns| {
            fs::read_link(format!("/proc/self/ns/{ns}"))
                .map_or_else(|e| e.to_string(), |p| p.display().to_string())
        })
        .join(" ")
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
        // owner of what they close. Best effort: another test's pipe left
        // open here would only hold that test until this child ends.
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
    in_child(user, || {
        let before = namespaces();
        let set_up = match enter_user_namespace(state) {
            Ok(s) => s,
            Err(r) => return format!("refused\t{r}"),
        };
        if namespaces() == before {
            return "error: the namespaces did not change".to_owned();
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
        if namespaces() == before {
            return "error: the namespaces did not change".to_owned();
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
                let changed = if namespaces() == before {
                    "same"
                } else {
                    "new"
                };
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
                if namespaces() == before {
                    "same"
                } else {
                    "new"
                }
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
