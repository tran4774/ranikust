//! An image's owners checked against the build's user namespace before any
//! of its layers is extracted (#80, ADR-0098): a map of one id, a user's
//! without `uidmap` or without ranges, refuses an image with several
//! owners; a map of ranges, an owner past them. The check passes exactly
//! where the extraction would.

use rnk_snapshot::Snapshotter as _;

use super::*;

/// A uid and gid with no subordinate ids and no name.
const NO_RANGES: (u32, u32) = (4243, 4243);

/// An image's name and its layers' tars and diff IDs.
type Image = (&'static str, Vec<(Vec<u8>, Digest)>);

/// An image of root's alone, one with several owners (a group's file and
/// a user's home, each in a layer of its own), one with an owner no map
/// holds.
fn images() -> [Image; 3] {
    [
        (
            "root",
            vec![
                Tar::default()
                    .dir("./", ROOT, 0o755)
                    .file("f", ROOT, 0o644, b"f")
                    .finish(),
            ],
        ),
        (
            "owned",
            vec![
                Tar::default()
                    .dir("./", ROOT, 0o755)
                    .dir("etc/", ROOT, 0o755)
                    .file("etc/shadow", (0, 42), 0o640, b"")
                    .finish(),
                Tar::default()
                    .dir("home/", ROOT, 0o755)
                    .dir("home/app/", USER, 0o755)
                    .finish(),
            ],
        ),
        (
            "far",
            vec![
                Tar::default()
                    .dir("./", ROOT, 0o755)
                    .file("far", FAR, 0o644, b"far")
                    .finish(),
            ],
        ),
    ]
}

/// In the build's user namespace, entered with the state directory
/// `state`, each of [`images`] written to `blobs`, checked, then extracted
/// layer by layer all the same: `map\t<map>`, then for each image
/// `<name> check\t<ok or the refusal>`, `<name> ways\t<guidance>` and
/// `<name> extract\t<ok or the error>`, then `published\t<layers>`.
fn check_in_namespace(state: &Path, blobs: &Path) -> String {
    let set_up = match enter_user_namespace(state) {
        Ok(s) => s,
        Err(r) => return format!("refused\t{r}"),
    };
    let mut out = vec![format!("map\t{}", set_up.userns.map)];
    let store = Arc::new(set_up.store);
    let overlay = Overlay::new(Arc::clone(&store), CAPS).unwrap();
    fs::create_dir(blobs).unwrap();
    for (name, image) in images() {
        let layers: Vec<(Digest, PathBuf)> = image
            .into_iter()
            .enumerate()
            .map(|(n, (tar, id))| {
                let path = blobs.join(format!("{name}-{n}"));
                fs::write(&path, tar).unwrap();
                (id, path)
            })
            .collect();
        let (check, ways) = match overlay.check_owners(&layers) {
            Ok(()) => ("ok".to_owned(), String::new()),
            Err(e) => (
                e.to_string(),
                crate::owners::find(&e).map_or_else(
                    || "none".to_owned(),
                    |found| crate::owners::guidance(found, true),
                ),
            ),
        };
        let mut below = Vec::new();
        let mut extract = "ok".to_owned();
        for (id, path) in &layers {
            match overlay.base_layer(id, path, &below) {
                Ok(layer) => below.push(layer),
                Err(e) => {
                    extract = e.to_string();
                    break;
                }
            }
        }
        out.extend([
            format!("{name} check\t{check}"),
            format!("{name} ways\t{ways}"),
            format!("{name} extract\t{extract}"),
        ]);
    }
    drop(overlay);
    let closed = Arc::into_inner(store).map(Store::close);
    let layers = state.join("layers/v3/sha256");
    out.push(format!(
        "published\t{}",
        fs::read_dir(&layers).map_or(0, Iterator::count)
    ));
    if !matches!(closed, Some(Ok(()))) {
        out.push(format!("error\tclosing the store: {closed:?}"));
    }
    out.join("\n")
}

/// The value of `key` in [`check_in_namespace`]'s output.
fn field<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('\t'))
        .unwrap_or_else(|| panic!("no {key}: {out}"))
}

/// The refusal of the image with several owners in a one-id map.
const OWNED_ONE_ID: &str = "owners this user namespace does not map: uid 1000 (/home/app), gid \
                            42 (/etc/shadow), gid 1000 (/home/app); it maps uid 0 and gid 0";

/// Where the extraction fails on an owner a map lacks.
const UNMAPPED: &str = ": an id this user namespace does not map";

/// `out` as a one-id map gives it: the image of root's passes and is
/// extracted; the other two are refused by the check, which names their
/// owners and says to install `uidmap` and add ranges, and so would be by
/// their extraction.
fn check_one_id(out: &str) {
    assert!(!out.contains("error\t"), "{out}");
    assert_eq!(field(out, "map"), "one uid", "{out}");
    assert_eq!(field(out, "root check"), "ok", "{out}");
    assert_eq!(field(out, "root extract"), "ok", "{out}");
    assert_eq!(field(out, "owned check"), OWNED_ONE_ID, "{out}");
    assert!(
        field(out, "far check").starts_with(
            "owners this user namespace does not map: uid 3000000000 (/far), gid 3000000000 \
             (/far); it maps uid 0 and gid 0"
        ),
        "{out}"
    );
    for image in ["owned", "far"] {
        let ways = field(out, &format!("{image} ways"));
        for part in [
            "install uidmap",
            "/etc/subuid and /etc/subgid",
            "as root",
            "in a container",
        ] {
            assert!(ways.contains(part), "{image}: {part}: {out}");
        }
        assert!(
            field(out, &format!("{image} extract")).ends_with(UNMAPPED),
            "{out}"
        );
    }
    assert_eq!(field(out, "published"), "1", "{out}");
}

/// Hides `newuidmap` and `newgidmap` from this forked child and what it
/// forks: a mount namespace of its own, with `/dev/null` bound over each,
/// which the build's user namespace takes for no tool (not a regular
/// file).
fn hide_uidmap() -> Result<(), String> {
    let tools = [c"/usr/bin/newuidmap", c"/usr/bin/newgidmap"];
    // SAFETY: plain system calls on this single-threaded forked child, with
    // NUL-terminated paths that outlive them.
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            return Err(format!("unshare: {}", io::Error::last_os_error()));
        }
        let none = std::ptr::null();
        if libc::mount(
            none,
            c"/".as_ptr(),
            none,
            libc::MS_REC | libc::MS_PRIVATE,
            none.cast(),
        ) != 0
        {
            return Err(format!("private /: {}", io::Error::last_os_error()));
        }
        for tool in tools {
            if libc::mount(
                c"/dev/null".as_ptr(),
                tool.as_ptr(),
                none,
                libc::MS_BIND,
                none.cast(),
            ) != 0
            {
                return Err(format!(
                    "bind over {tool:?}: {}",
                    io::Error::last_os_error()
                ));
            }
        }
    }
    Ok(())
}

/// A private temp dir of `user`'s, and its state directory's path.
fn user_dir(user: (u32, u32)) -> (tempfile::TempDir, PathBuf) {
    let tmp = private_tempdir();
    chown(tmp.path(), Some(user.0), Some(user.1)).unwrap();
    let state = tmp.path().join("state");
    (tmp, state)
}

/// T1, as root of a rootless Podman container: the build's user namespace
/// mirrors the container's ranges; an image of several owners in them
/// passes, one with an owner past them is refused before any layer is
/// extracted, naming it and saying to widen the container's ranges.
#[test]
#[ignore = "T1: default: root in a rootless Podman container checks an image's owners against the build's user namespace"]
fn an_owner_past_the_container_s_ranges_is_refused_before_extraction() {
    if !alone_in(
        module_path!(),
        "an_owner_past_the_container_s_ranges_is_refused_before_extraction",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let out = in_child(None, || {
        check_in_namespace(&tmp.path().join("state"), &tmp.path().join("blobs"))
    });
    assert!(!out.contains("error\t"), "{out}");
    assert_eq!(field(&out, "map"), "multi-range", "{out}");
    for image in ["root", "owned"] {
        assert_eq!(field(&out, &format!("{image} check")), "ok", "{out}");
        assert_eq!(field(&out, &format!("{image} extract")), "ok", "{out}");
    }
    assert!(
        field(&out, "far check").starts_with(
            "owners this user namespace does not map: uid 3000000000 (/far), gid 3000000000 \
             (/far); it maps uids 0-"
        ),
        "{out}"
    );
    let ways = field(&out, "far ways");
    assert!(
        ways.contains("does not map uid 3000000000 and gid 3000000000: widen the container's"),
        "{out}"
    );
    assert!(field(&out, "far extract").ends_with(UNMAPPED), "{out}");
    assert_eq!(field(&out, "published"), "3", "{out}");
}

/// T2: as the user who ran sudo, with `newuidmap` and `newgidmap` hidden
/// (uidmap not installed, Ubuntu's default), the build's user namespace
/// maps one id: an image with several owners is refused before any layer
/// is extracted. Where AppArmor restricts unprivileged user namespaces
/// (Ubuntu 24.04), the set-up is refused first.
#[test]
#[ignore = "T2: hides uidmap in a mount namespace and checks an image's owners as the sudo user"]
fn a_user_without_uidmap_is_refused_an_image_of_several_owners() {
    if !alone_in(
        module_path!(),
        "a_user_without_uidmap_is_refused_an_image_of_several_owners",
    ) {
        return;
    }
    let (user, restricted) = sudo_user();
    assert!(
        Path::new("/usr/bin/newuidmap").is_file(),
        "the VM has uidmap, which the test hides"
    );
    assert!(
        crate::subids::lookup(user.0).unwrap().is_some(),
        "the VM's user has subordinate ids"
    );
    let (tmp, state) = user_dir(user);
    let blobs = tmp.path().join("blobs");
    let out = in_child(None, || match hide_uidmap() {
        Ok(()) => in_child(Some(user), || check_in_namespace(&state, &blobs)),
        Err(e) => format!("error\t{e}"),
    });
    if restricted {
        assert!(
            out.starts_with("refused\tno user namespace for the build: "),
            "{out}"
        );
        return;
    }
    check_one_id(&out);
}

/// T2: as a user with no subordinate ids, the build's user namespace maps
/// one id: an image with several owners is refused before any layer is
/// extracted. Where AppArmor restricts unprivileged user namespaces
/// (Ubuntu 24.04), the set-up is refused first.
#[test]
#[ignore = "T2: checks an image's owners as a user without subordinate ids"]
fn a_user_without_ranges_is_refused_an_image_of_several_owners() {
    if !alone_in(
        module_path!(),
        "a_user_without_ranges_is_refused_an_image_of_several_owners",
    ) {
        return;
    }
    let (_, restricted) = sudo_user();
    assert_eq!(crate::subids::lookup(NO_RANGES.0), Ok(None));
    let (tmp, state) = user_dir(NO_RANGES);
    let blobs = tmp.path().join("blobs");
    let out = in_child(Some(NO_RANGES), || check_in_namespace(&state, &blobs));
    if restricted {
        assert!(
            out.starts_with("refused\tno user namespace for the build: "),
            "{out}"
        );
        return;
    }
    check_one_id(&out);
}

/// T2: as the user who ran sudo, with `newuidmap` and ranges, an image of
/// several owners in them passes; one with an owner past them is refused
/// before any layer is extracted, naming it and saying to widen the
/// user's ranges. Where AppArmor restricts unprivileged user namespaces
/// (Ubuntu 24.04), the set-up is refused first.
#[test]
#[ignore = "T2: checks an image's owners against the sudo user's ranges"]
fn an_owner_past_a_user_s_ranges_is_refused_before_extraction() {
    if !alone_in(
        module_path!(),
        "an_owner_past_a_user_s_ranges_is_refused_before_extraction",
    ) {
        return;
    }
    let (user, restricted) = sudo_user();
    let (tmp, state) = user_dir(user);
    let blobs = tmp.path().join("blobs");
    let out = in_child(Some(user), || check_in_namespace(&state, &blobs));
    if restricted {
        assert!(
            out.starts_with("refused\tno user namespace for the build: "),
            "{out}"
        );
        return;
    }
    assert!(!out.contains("error\t"), "{out}");
    assert_eq!(field(&out, "map"), "newuidmap", "{out}");
    for image in ["root", "owned"] {
        assert_eq!(field(&out, &format!("{image} check")), "ok", "{out}");
        assert_eq!(field(&out, &format!("{image} extract")), "ok", "{out}");
    }
    assert!(
        field(&out, "far check").starts_with(
            "owners this user namespace does not map: uid 3000000000 (/far), gid 3000000000 \
             (/far); it maps uids 0-"
        ),
        "{out}"
    );
    assert!(
        field(&out, "far ways").contains("widen them so that it maps uid 3000000000 and gid"),
        "{out}"
    );
    assert!(field(&out, "far extract").ends_with(UNMAPPED), "{out}");
    assert_eq!(field(&out, "published"), "3", "{out}");
}
