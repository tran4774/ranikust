//! The canary of a state directory and id map (#183, ADR-0128): a build
//! nested in a namespace whose range moved keeps its map relative to that
//! namespace, and so the keys of the layers it extracted; the canary
//! refuses them before anything changes.

use std::io::Write as _;
use std::os::unix::fs::lchown;

use super::*;

/// Where the enclosing namespace's ids from 1 start in the container: the
/// first range, one moved past it, one overlapping it.
const FIRST: u32 = 1000;
const MOVED: u32 = 5000;
const OVERLAPPING: u32 = 500;
/// How many ids from 1 the enclosing namespace maps.
const COUNT: u32 = 1000;
/// How far a `chown` on disk moves the first range's ids.
const CHOWNED: u32 = 3000;

/// A layer of a file owned by [`USER`], the last id of the range.
fn layer() -> (Vec<u8>, Digest) {
    Tar::default()
        .dir("./", ROOT, 0o755)
        .file("owned", USER, 0o644, b"owned")
        .finish()
}

/// Runs `f` in a forked grandchild, root of a user namespace that maps 0
/// to root's 0, and 1 to [`COUNT`] to the container's ids from `first`: an
/// enclosing container whose subordinate range starts there. What `f`
/// returns, or an `error:` line. Only in [`in_child`]'s single-threaded
/// child.
fn under_range(first: u32, f: impl FnOnce() -> String) -> String {
    let pipe = || rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
    let ((ready_r, ready_w), (go_r, go_w), (out_r, out_w)) = (pipe(), pipe(), pipe());
    // SAFETY: this process is single-threaded ([`in_child`]); the child
    // ends with `_exit`.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        drop((ready_r, go_w, out_r));
        // SAFETY: plain system calls on this single-threaded child; the
        // map is written from the parent, which opens its `/proc` files.
        let unshared = unsafe {
            libc::prctl(libc::PR_SET_DUMPABLE, 1) == 0 && libc::unshare(libc::CLONE_NEWUSER) == 0
        };
        let text = if unshared {
            send(ready_w.as_fd(), "u");
            drop(ready_w);
            if rustix::io::read(&go_r, &mut [0; 1]) == Ok(1) {
                // A panic must not unwind into the harness's copy here.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                    .unwrap_or_else(|_| "error: panicked".to_owned())
            } else {
                "error: no map was written".to_owned()
            }
        } else {
            format!("error: unshare: {}", std::io::Error::last_os_error())
        };
        send(out_w.as_fd(), &text);
        // SAFETY: `_exit` ends this process at once.
        unsafe { libc::_exit(0) }
    }
    drop((ready_w, go_r, out_w));
    let map = format!("0 0 1\n1 {first} {COUNT}\n");
    let written = match rustix::io::read(&ready_r, &mut [0; 1]) {
        Ok(1) => ["uid_map", "gid_map"].iter().try_for_each(|name| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(format!("/proc/{pid}/{name}"))?;
            // The kernel takes a map in one write.
            match file.write(map.as_bytes())? {
                n if n == map.len() => Ok(()),
                _ => Err(std::io::Error::other("short write")),
            }
        }),
        _ => Ok(()),
    };
    if written.is_ok() {
        send(go_w.as_fd(), "g");
    }
    drop(go_w);
    let mut out = String::new();
    fs::File::from(out_r).read_to_string(&mut out).unwrap();
    let pid = rustix::process::Pid::from_raw(pid).expect("fork");
    let waited = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());
    assert!(matches!(waited, Ok(Some(_))), "waitpid: {waited:?}: {out}");
    match written {
        Ok(()) => out,
        Err(e) => format!("error: write the map {map:?}: {e}: {out}"),
    }
}

/// In the build's user namespace, entered with the state directory `state`:
/// its map, then [`layer`] extracted, its path, inode and how many layers
/// its directory holds; or `refused\t<refusal>`.
fn extract(state: &Path) -> String {
    let s = match enter_user_namespace(state) {
        Ok(s) => s,
        Err(r) => return format!("refused\t{r}"),
    };
    let store = Arc::new(s.store);
    let overlay = Overlay::new(Arc::clone(&store), CAPS).unwrap();
    let (tar, id) = layer();
    let out = match overlay.base_layer_from(&id, tar.as_slice(), &[]) {
        Ok(l) => {
            let p = l.path();
            let ino = fs::metadata(p).map_or_else(|e| e.to_string(), |m| m.ino().to_string());
            let layers = fs::read_dir(p.parent().unwrap()).unwrap().count();
            format!("{}\n{}\t{ino}\t{layers}", s.userns.map, p.display())
        }
        Err(e) => format!("error: {e}"),
    };
    drop(overlay);
    match Arc::into_inner(store).map(Store::close) {
        Some(Ok(())) => out,
        Some(Err(e)) => format!("error: close: {e}"),
        None => "error: the store is still shared".to_owned(),
    }
}

/// Rootless overlay mode's plan ([`crate::setup::rootless::plan`]), which
/// checks and holds the state directory, then probes in it, before the
/// build's user namespace exists: `planned`, or `refused\t<refusal>`, with
/// `warns\t` before the refusal where root's `--mode=auto` would log it as a
/// warning.
fn plan(state: &Path) -> String {
    match crate::setup::rootless::plan(state, false, false, None) {
        Ok(_) => "planned".to_owned(),
        Err(n) => {
            let warns = if n.refusal.warns_on_fallback() {
                "warns\t"
            } else {
                ""
            };
            format!("refused\t{warns}{}", n.refusal)
        }
    }
}

/// Why a state directory is refused for its canary.
const CHANGED: &str = "holds layers extracted under other ids";

/// The owner a namespace sees for an entry of an unmapped uid and the gid
/// 0: the kernel's `overflowuid`, not always 65534.
fn unmapped_uid() -> String {
    let uid = fs::read_to_string("/proc/sys/kernel/overflowuid").unwrap();
    format!("{}:0", uid.trim())
}

/// Asserts that rootless overlay mode's plan under the range from `first`,
/// before the build's user namespace exists, and the store's open in that
/// namespace each refuse the state directory `state` for its canary's
/// `uid-1` owned by `owner`, with the way out, the plan's refusal marked to
/// warn on fallback, and that `state` is still as it was.
fn assert_refused(
    run: impl Fn(u32, fn(&Path) -> String) -> String,
    first: u32,
    owner: &str,
    state: &Path,
) {
    let before = tree(state);
    let planned = run(first, plan);
    assert!(planned.starts_with("refused\twarns\t"), "{planned}");
    for out in [planned, run(first, extract)] {
        assert!(out.starts_with("refused\t"), "{out}");
        assert!(out.contains(CHANGED), "{out}");
        assert!(
            out.contains(&format!("/uid-1 is owned by {owner}, not 1:0")),
            "{out}"
        );
        assert!(out.contains("ranikust prune --all"), "{out}");
        assert_eq!(tree(state), before, "{out}");
    }
}

/// T1, as root of a rootless Podman container: the base layer a build
/// extracted, nested in a namespace whose range starts at [`FIRST`], is
/// reused by the next build there; once the range moved past or into the
/// first, or once the state directory was chowned on disk, rootless
/// overlay mode is refused there before anything changes, by its plan and
/// by the store, with the way out.
#[test]
#[ignore = "T1: default: root in a rootless Podman container moves the range of a namespace enclosing the build's"]
fn layers_extracted_under_a_moved_range_are_refused_and_reused_under_the_same() {
    if !alone_in(
        module_path!(),
        "layers_extracted_under_a_moved_range_are_refused_and_reused_under_the_same",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    let run = |first, f: fn(&Path) -> String| in_child(None, || under_range(first, || f(&state)));
    let out = run(FIRST, extract);
    assert!(out.starts_with("multi-range\n"), "{out}");
    let canaries = state.join("layers/v3/canary");
    assert_eq!(fs::read_dir(&canaries).unwrap().count(), 1, "{out}");
    assert_refused(run, MOVED, &unmapped_uid(), &state);
    // The first range's uid 1 is the overlapping one's 501.
    let shifted = format!("{}:0", FIRST - OVERLAPPING + 1);
    assert_refused(run, OVERLAPPING, &shifted, &state);
    // The plan passes the canary under the same range (its probe may stop
    // later, as it does where `/proc` is masked).
    let planned = run(FIRST, plan);
    assert!(!planned.contains(CHANGED), "{planned}");
    // The same path and inode: the layer is reused, not extracted again.
    assert_eq!(run(FIRST, extract), out);
    assert_eq!(fs::read_dir(&canaries).unwrap().count(), 1, "{out}");
    let range = FIRST..FIRST + COUNT;
    let moved = |id: u32| {
        if range.contains(&id) {
            id + CHOWNED
        } else {
            id
        }
    };
    for e in tree(&state) {
        if range.contains(&e.uid) || range.contains(&e.gid) {
            lchown(state.join(&e.path), Some(moved(e.uid)), Some(moved(e.gid))).unwrap();
        }
    }
    assert_refused(run, FIRST, &unmapped_uid(), &state);
}
