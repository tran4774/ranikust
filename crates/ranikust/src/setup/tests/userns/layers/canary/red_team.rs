//! Red team of the canary (ADR-0128, T13) as root of a rootless Podman
//! container: builds under enclosing ranges racing on one state directory,
//! and the checks of kaniko mode off `/`, root's overlay mode and
//! `--userns`, which refuse a changed canary before they change anything.

use std::time::Duration;

use rnk_store::EntryKey;

use super::*;

/// Runs `f(state)` under [`under_range`] of each of `firsts`, each in a
/// child of its own, all at once: each `f` starts once the children had
/// time to enter their namespaces. Their outputs, in order. Only in
/// [`in_child`]'s single-threaded child.
fn racing(firsts: &[u32], f: fn(&Path) -> String, state: &Path) -> Vec<String> {
    let pipe = || rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
    let (go_r, go_w) = pipe();
    let mut racers = Vec::new();
    for &first in firsts {
        let (out_r, out_w) = pipe();
        // SAFETY: this process is single-threaded ([`in_child`]); the child
        // ends with `_exit`.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            drop((out_r, go_w));
            let text = under_range(first, || {
                // One byte each, written for every racer at once.
                let _ = rustix::io::read(&go_r, &mut [0; 1]);
                f(state)
            });
            send(out_w.as_fd(), &text);
            // SAFETY: `_exit` ends this process at once.
            unsafe { libc::_exit(0) }
        }
        drop(out_w);
        racers.push((pid, fs::File::from(out_r)));
    }
    drop(go_r);
    std::thread::sleep(Duration::from_millis(500));
    send(go_w.as_fd(), &"g".repeat(firsts.len()));
    drop(go_w);
    racers
        .into_iter()
        .map(|(pid, mut r)| {
            let mut out = String::new();
            r.read_to_string(&mut out).unwrap();
            let pid = rustix::process::Pid::from_raw(pid).expect("fork");
            let waited = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());
            assert!(matches!(waited, Ok(Some(_))), "waitpid: {waited:?}: {out}");
            out
        })
        .collect()
}

/// Between [`racing`]'s outputs, joined in one.
const APART: &str = "\n----\n";

/// T1, red team (T13): builds nested in namespaces of ranges that start at
/// [`FIRST`], [`MOVED`] and [`OVERLAPPING`], racing on one new state
/// directory for the same base layer: one of them extracts it and makes
/// the canary, every other one is refused for the canary, the layer is
/// there once, and later builds reuse it under the winner's range alone.
#[test]
#[ignore = "T1: default: root in a rootless Podman container races builds under namespaces of three ranges on one state directory"]
fn red_team_builds_under_ranges_racing_on_one_state_directory_never_share_a_layer() {
    if !alone_in(
        module_path!(),
        "red_team_builds_under_ranges_racing_on_one_state_directory_never_share_a_layer",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let ranges = [FIRST, MOVED, OVERLAPPING];
    for round in 0..6 {
        let tmp = private_tempdir();
        let state = tmp.path().join("state");
        // Made here: the racers share it from their first step.
        Store::open(&BuildPaths::new(&state, &state))
            .unwrap()
            .close()
            .unwrap();
        let mut order = ranges;
        order.rotate_left(round % ranges.len());
        let outs = in_child(None, || racing(&order, extract, &state).join(APART));
        let outs: Vec<&str> = outs.split(APART).collect();
        assert_eq!(outs.len(), order.len(), "round {round}: {outs:?}");
        let won: Vec<_> = (0..order.len())
            .filter(|&i| outs[i].starts_with("multi-range\n"))
            .collect();
        assert_eq!(won.len(), 1, "round {round}: {outs:?}");
        let winner = won[0];
        for (i, out) in outs.iter().enumerate() {
            if i != winner {
                assert!(out.contains(CHANGED), "round {round}: {}: {out}", order[i]);
            }
        }
        let count = |d: &str| fs::read_dir(state.join(d)).unwrap().count();
        assert_eq!(count("layers/v3/canary"), 1, "round {round}: {outs:?}");
        assert_eq!(count("layers/v3/sha256"), 1, "round {round}: {outs:?}");
        let run = |first| in_child(None, || under_range(first, || extract(&state)));
        // The same path and inode: reused under the winner's range alone.
        assert_eq!(run(order[winner]), outs[winner], "round {round}");
        for (i, &first) in order.iter().enumerate() {
            if i != winner {
                let out = run(first);
                assert!(out.contains(CHANGED), "round {round}: {first}: {out}");
            }
        }
        assert_eq!(count("layers/v3/sha256"), 1, "round {round}");
    }
}

/// The ids of the `--userns` range of [`red_team_a_changed_canary_is_refused_unchanged_by_each_check_before_a_build`]:
/// mapped in the container, as root's `ranikust` range is on a host.
fn userns_range() -> rnk_sandbox::SubordinateIds {
    let range = rnk_sandbox::IdRange {
        start: FIRST,
        count: COUNT,
    };
    rnk_sandbox::SubordinateIds {
        uids: range,
        gids: range,
    }
}

/// A layer published by `store`, which makes the canary of its maps.
fn publish(store: &Store) -> Result<(), String> {
    let digest = rnk_oci::Digest::from_sha256_bytes(&[7; 32]);
    store
        .fill(&EntryKey::Layer(digest), |_| Ok::<_, std::io::Error>(0))
        .map(drop)
        .map_err(|e| e.to_string())
}

/// The entry `name` of the only canary in `state`.
fn canary_entry(state: &Path, name: &str) -> PathBuf {
    let canaries: Vec<_> = fs::read_dir(state.join("layers/v3/canary"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(canaries.len(), 1, "{canaries:?}");
    canaries[0].join(name)
}

/// The mode of `dir` itself.
fn mode_of(dir: &Path) -> u32 {
    fs::symlink_metadata(dir).unwrap().mode() & 0o7777
}

/// Gives a directory its mode back when dropped, a panic included.
struct ModeBack(PathBuf, u32);

impl Drop for ModeBack {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
    }
}

/// Asserts that `check` refuses `state`, whose canary's `entry` was given
/// the owner `moved`, for the canary, with the way out and changing
/// nothing; then, the owner given back to `was`, takes it, and, where
/// `private`, makes it so.
fn refused_then_taken(
    what: &str,
    state: &Path,
    entry: &Path,
    (was, moved): ((u32, u32), (u32, u32)),
    private: bool,
    check: &dyn Fn() -> Result<(), Refusal>,
) {
    lchown(entry, Some(moved.0), Some(moved.1)).unwrap();
    fs::set_permissions(state, fs::Permissions::from_mode(0o755)).unwrap();
    let before = (tree(state), mode_of(state));
    let refusal = check().map(|()| "taken").unwrap_err();
    assert_eq!(refusal.kind, Kind::IdMapChanged, "{what}: {refusal}");
    let shown = refusal.to_string();
    assert!(shown.contains(CHANGED), "{what}: {shown}");
    assert!(shown.contains("ranikust prune --all"), "{what}: {shown}");
    assert_eq!((tree(state), mode_of(state)), before, "{what}: {shown}");
    // The control: the owner back, taken and made private.
    lchown(entry, Some(was.0), Some(was.1)).unwrap();
    check().unwrap();
    let mode = if private { 0o700 } else { 0o755 };
    assert_eq!(mode_of(state), mode, "{what}");
}

/// T1, red team (T13): a canary whose entry was given another owner, as a
/// `chown` of the state directory on disk gives it, is refused before
/// anything in the state directory changes, its mode included: by
/// `--userns`'s check from outside (a range mapped in the container) and
/// by the store in its namespace, and, for the container's own maps, by
/// kaniko mode off `/`, root's overlay mode and the store opened here. Each
/// takes it again once the owner is back.
#[test]
#[ignore = "T1: default: root in a rootless Podman container gives a canary's entry another owner and checks the state directory as each mode does"]
fn red_team_a_changed_canary_is_refused_unchanged_by_each_check_before_a_build() {
    if !alone_in(
        module_path!(),
        "red_team_a_changed_canary_is_refused_unchanged_by_each_check_before_a_build",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    // The range's first ids search it.
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o711)).unwrap();
    // `--userns` refuses a parent others may write, as the lane's /work (1777).
    let parent = tmp.path().parent().unwrap().to_owned();
    let _back = ModeBack(parent.clone(), mode_of(&parent));
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    let on_overlayfs = || unreachable!("the T1 container's /work is a tmpfs");

    // `--userns`: the state directory is the range's first ids'.
    let sub = userns_range();
    let state = tmp.path().join("userns");
    fs::create_dir(&state).unwrap();
    lchown(&state, Some(FIRST), Some(FIRST)).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let in_range = |f: fn(&Store) -> Result<(), String>| {
        let state = state.clone();
        in_child(None, move || match rnk_sandbox::enter_build_user_namespace(
            rnk_sandbox::BuildIds::Shifted(sub),
        ) {
            Ok(_) => match Store::open(&BuildPaths::new(&state, &state)) {
                Ok(store) => f(&store).map_or_else(
                    |e| format!("error: {e}"),
                    |()| {
                        store
                            .close()
                            .map_or_else(|e| format!("error: close: {e}"), |()| "ok".into())
                    },
                ),
                Err(e) => format!("refused\t{e}"),
            },
            Err(e) => format!("error: the namespace: {e}"),
        })
    };
    assert_eq!(in_range(publish), "ok");
    let last = FIRST + COUNT - 1;
    let entry = canary_entry(&state, &format!("uid-{}", COUNT - 1));
    let children: Vec<_> = rnk_sandbox::BuildIds::Shifted(sub)
        .maps()
        .iter()
        .filter_map(|(u, g)| rnk_store::IdMaps::parse(u, g))
        .collect();
    assert_eq!(children.len(), 1, "{children:?}");
    let owner = crate::statedir::Owner::Mapped {
        uid: FIRST,
        gid: FIRST,
    };
    let userns = || hold_state_dir(&state, 0, owner, &children, on_overlayfs).map(drop);
    refused_then_taken(
        "--userns",
        &state,
        &entry,
        ((last, FIRST), (last - 1, FIRST)),
        true,
        &userns,
    );
    lchown(&entry, Some(last - 1), Some(FIRST)).unwrap();
    let before = tree(&state);
    let inside = in_range(|_| Ok(()));
    assert!(inside.starts_with("refused\t"), "{inside}");
    assert!(inside.contains(CHANGED), "{inside}");
    assert_eq!(tree(&state), before, "{inside}");
    lchown(&entry, Some(last), Some(FIRST)).unwrap();
    assert_eq!(in_range(|_| Ok(())), "ok");

    // The container's own maps, which kaniko mode and root's overlay mode
    // build under.
    let state = tmp.path().join("own");
    let store = Store::open(&BuildPaths::new(&state, &state)).unwrap();
    publish(&store).unwrap();
    store.close().unwrap();
    let entry = canary_entry(&state, "uid-1");
    let ids = ((1, 0), (2, 0));
    let kaniko = || kaniko_state_dir(&state, false, 0).map(drop);
    refused_then_taken("kaniko mode off /", &state, &entry, ids, true, &kaniko);
    let me = crate::statedir::Owner::me();
    let overlay = || hold_state_dir(&state, 0, me, &[], on_overlayfs).map(drop);
    refused_then_taken("root's overlay mode", &state, &entry, ids, true, &overlay);
    let store = || {
        Store::open(&BuildPaths::new(&state, &state))
            .and_then(Store::close)
            .map_err(|e| store_refusal(&e, &state, 0))
    };
    refused_then_taken("the store", &state, &entry, ids, false, &store);
}

/// Where the canaries of a state directory are.
const CANARIES: &str = "layers/v3/canary";

/// `mount(2)` of `src` at `at`, of type `fstype` when given.
fn mount(src: &Path, at: &Path, fstype: Option<&str>, flags: libc::c_ulong) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let c = |b: &[u8]| std::ffi::CString::new(b).unwrap();
    let (src, at) = (c(src.as_os_str().as_bytes()), c(at.as_os_str().as_bytes()));
    let fstype = fstype.map(|t| c(t.as_bytes()));
    let fstype = fstype.as_ref().map_or(std::ptr::null(), |t| t.as_ptr());
    // SAFETY: each pointer is null or a NUL-terminated string that outlives
    // the call.
    let r = unsafe { libc::mount(src.as_ptr(), at.as_ptr(), fstype, flags, std::ptr::null()) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// A bind mount of `src` at `at`.
fn bind(src: &Path, at: &Path) -> std::io::Result<()> {
    mount(src, at, None, libc::MS_BIND)
}

/// An empty tmpfs at `at`.
fn tmpfs(at: &Path) -> std::io::Result<()> {
    mount(Path::new("tmpfs"), at, Some("tmpfs"), 0)
}

/// Gives this child a mount namespace of its own, every mount private, so
/// that what it mounts goes with it. Only in [`in_child`]'s child.
fn private_mounts() -> std::io::Result<()> {
    // SAFETY: this child is single-threaded ([`in_child`]) and ends with
    // its case.
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    mount(
        Path::new("none"),
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
    )
}

/// The canary `from` again at `to`: its entries, modes and owners.
fn copy_canary(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let m = fs::symlink_metadata(e.path())?;
        let made = to.join(e.file_name());
        fs::File::create(&made)?;
        lchown(&made, Some(m.uid()), Some(m.gid()))?;
        fs::set_permissions(&made, m.permissions())?;
    }
    let m = fs::symlink_metadata(from)?;
    lchown(to, Some(m.uid()), Some(m.gid()))?;
    fs::set_permissions(to, m.permissions())
}

/// What a check of `state` gave, as set-up shows it.
fn checked(r: Result<(), rnk_store::Error>, state: &Path) -> String {
    match r {
        Ok(()) => "taken".into(),
        Err(e) => {
            let r = store_refusal(&e, state, 0);
            format!("refused {:?}: {r}", r.kind)
        }
    }
}

/// The state directory's check from outside and the store's open, as
/// [`checked`] shows them, each on its own line.
fn both_checks(state: &Path) -> String {
    let outside = Store::check_state_dir_for(state, &[]).map(drop);
    let inside = Store::open(&BuildPaths::new(state, state)).and_then(Store::close);
    format!("{}\n{}", checked(outside, state), checked(inside, state))
}

/// How a canary of another mount takes the place of the state directory's
/// own, given the state directory with a good canary, the one checked and
/// the canary's name.
type Mounting = fn(&Path, &Path, &str) -> std::io::Result<()>;

/// T1, red team (T13): a canary, an entry or a `layers/v3` of the right
/// ids mounted over a wrong canary is refused by the check from outside and
/// by the store, changing nothing; an empty tmpfs that hides the canary
/// lets neither a layer be reused nor one be published.
#[test]
#[ignore = "T1: privileged: mounts a good canary over a state directory's wrong one in a mount namespace of its own"]
fn red_team_a_canary_mounted_over_the_state_directory_s_own_is_refused() {
    if !alone_in(
        module_path!(),
        "red_team_a_canary_mounted_over_the_state_directory_s_own_is_refused",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a privileged rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let made = |name: &str| {
        let state = tmp.path().join(name);
        let store = Store::open(&BuildPaths::new(&state, &state)).unwrap();
        publish(&store).unwrap();
        store.close().unwrap();
        state
    };
    let (good, state) = (made("good"), made("state"));
    let names = |d: &Path| -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    };
    let hex = names(&state.join(CANARIES)).remove(0);
    assert_eq!(names(&good.join(CANARIES)), std::slice::from_ref(&hex));
    lchown(canary_entry(&state, "uid-1"), Some(2), Some(0)).unwrap();
    // The controls: the good canary taken, the wrong one refused.
    assert_eq!(both_checks(&good), "taken\ntaken");
    let wrong = both_checks(&state);
    assert_eq!(wrong.matches("refused IdMapChanged").count(), 2, "{wrong}");

    let cases: [(&str, Mounting); 4] = [
        ("a good canary bound over it", |good, state, hex| {
            bind(
                &good.join(CANARIES).join(hex),
                &state.join(CANARIES).join(hex),
            )
        }),
        ("a good entry bound over one", |good, state, hex| {
            let entry = |s: &Path| s.join(CANARIES).join(hex).join("uid-1");
            bind(&entry(good), &entry(state))
        }),
        (
            "a tmpfs of a good canary at layers/v3/canary",
            |good, state, hex| {
                tmpfs(&state.join(CANARIES))?;
                copy_canary(
                    &good.join(CANARIES).join(hex),
                    &state.join(CANARIES).join(hex),
                )
            },
        ),
        ("a good layers/v3 bound over it", |good, state, _| {
            bind(&good.join("layers/v3"), &state.join("layers/v3"))
        }),
    ];
    for (case, how) in cases {
        let out = in_child(None, || {
            let run = || -> std::io::Result<String> {
                private_mounts()?;
                how(&good, &state, &hex)?;
                let before = tree(&state);
                let out = both_checks(&state);
                let same = if tree(&state) == before {
                    "unchanged"
                } else {
                    "changed"
                };
                Ok(format!("{out}\n{same}"))
            };
            run().unwrap_or_else(|e| format!("error: {e}"))
        });
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{case}: {out}");
        for line in &lines[..2] {
            assert!(line.starts_with("refused IdMapChanged: "), "{case}: {out}");
            assert!(
                line.contains("the state directory's mount"),
                "{case}: {out}"
            );
            assert!(line.contains("ranikust prune --all"), "{case}: {out}");
        }
        assert_eq!(lines[2], "unchanged", "{case}: {out}");
    }

    // The canary hidden: the open takes it, as a state directory of no
    // canary, yet its layer is not reused and no other one is published.
    let out = in_child(None, || {
        let run = || -> std::io::Result<String> {
            private_mounts()?;
            tmpfs(&state.join(CANARIES))?;
            let outside = Store::check_state_dir_for(&state, &[]).map(drop);
            let mut out = vec![checked(outside, &state)];
            let store = match Store::open(&BuildPaths::new(&state, &state)) {
                Ok(store) => store,
                Err(e) => return Ok(format!("{}\n{}", out[0], checked(Err(e), &state))),
            };
            let key = |b| EntryKey::Layer(rnk_oci::Digest::from_sha256_bytes(&[b; 32]));
            let shown = |r: Result<PathBuf, rnk_store::Error>| match r {
                Ok(p) => format!("given {}", p.display()),
                Err(e) => format!("error: {e}"),
            };
            out.push(shown(store.get(&key(7)).and_then(|p| {
                p.ok_or_else(|| rnk_store::Error::Io {
                    op: "get",
                    path: state.clone(),
                    source: std::io::ErrorKind::NotFound.into(),
                })
            })));
            out.push(shown(store.fill(&key(8), |_| Ok::<_, std::io::Error>(0))));
            out.push(checked(store.close(), &state));
            out.push(format!("canaries: {:?}", names(&state.join(CANARIES))));
            out.push(format!(
                "layers: {}",
                names(&state.join("layers/v3/sha256")).len()
            ));
            Ok(out.join("\n"))
        };
        run().unwrap_or_else(|e| format!("error: {e}"))
    });
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 6, "{out}");
    assert_eq!(lines[0], "taken", "{out}");
    assert!(
        lines[1].starts_with("error: ") && lines[1].contains("is missing"),
        "{out}"
    );
    assert!(lines[2].starts_with("error: "), "{out}");
    assert_eq!(lines[3..], ["taken", "canaries: []", "layers: 1"], "{out}");
}
