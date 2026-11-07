//! Red team of base layers and the store in the build's user namespace
//! (#77): layers of another id map, and store directories another owner
//! planted, or planted as symlinks (T13).

use super::*;

/// One layer: a file of ids other than root's, and a device node a user
/// namespace leaves out.
fn layer() -> (Vec<u8>, Digest) {
    Tar::default()
        .dir("./", ROOT, 0o755)
        .file("f", (1, 2), 0o644, b"f")
        .dir("dev/", ROOT, 0o755)
        .node("dev/null", EntryType::Char, ROOT, 0o666, (1, 3))
        .finish()
}

/// [`layer`] extracted with `store` in this process's user namespace:
/// `<uid_map>\t<path>\t<names in its root>`, or an `error:` line. Closes
/// the store.
fn extract_with(store: Store) -> String {
    let (tar, id) = layer();
    let store = Arc::new(store);
    let overlay = match Overlay::new(Arc::clone(&store), CAPS) {
        Ok(o) => o,
        Err(e) => return format!("error: {e}"),
    };
    let layer = overlay.base_layer_from(&id, tar.as_slice(), &[]);
    drop(overlay);
    let closed = Arc::into_inner(store).map(Store::close);
    let (Ok(layer), Some(Ok(()))) = (&layer, closed) else {
        return format!("error: {layer:?}");
    };
    let mut names: Vec<String> = fs::read_dir(layer.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let map = fs::read_to_string("/proc/self/uid_map").unwrap_or_default();
    format!(
        "{}\t{}\t{}",
        map.split_whitespace().collect::<Vec<_>>().join(" "),
        layer.path().display(),
        names.join(",")
    )
}

/// Red team: a layer of one id map is never reused under another, nor one at its chain ID (T13).
#[test]
#[ignore = "T1: default: root in a rootless Podman container extracts one layer under two id maps"]
fn red_team_layers_extracted_under_one_map_are_never_used_under_another() {
    if !alone_in(
        module_path!(),
        "red_team_layers_extracted_under_one_map_are_never_used_under_another",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    let plain = || {
        in_child(None, || {
            match open_store(&state, &state, rustix::process::geteuid().as_raw()) {
                Ok(store) => extract_with(store),
                Err(r) => format!("error: {r}"),
            }
        })
    };
    let nested = || {
        in_child(None, || match enter_user_namespace(&state) {
            Ok(s) => extract_with(s.store),
            Err(r) => format!("refused\t{r}"),
        })
    };
    // A layer at the key a store outside any user namespace gives it.
    let (_, id) = layer();
    let opened = in_child(None, || {
        match open_store(&state, &state, rustix::process::geteuid().as_raw()) {
            Ok(s) => s
                .close()
                .map_or_else(|e| format!("error: {e}"), |()| "ok".into()),
            Err(r) => format!("error: {r}"),
        }
    });
    assert_eq!(opened, "ok");
    let chain = state.join("layers/v3/sha256").join(id.encoded());
    fs::create_dir_all(chain.join("planted")).unwrap();

    let split = |out: &str| -> [String; 3] {
        let v: Vec<&str> = out.split('\t').collect();
        assert_eq!(v.len(), 3, "{out}");
        [v[0].into(), v[1].into(), v[2].into()]
    };
    let a = plain();
    let b = nested();
    if b.starts_with("refused\t") {
        measured(&format!("no build user namespace here: {b}"));
        return;
    }
    let [map_a, path_a, names_a] = split(&a);
    let [map_b, path_b, names_b] = split(&b);
    // The control: the two maps differ, or the keys could not.
    assert_ne!(map_a, map_b);
    assert_ne!(path_a, path_b);
    for (p, names) in [(&path_a, &names_a), (&path_b, &names_b)] {
        assert_ne!(Path::new(p), chain, "{p}");
        assert_eq!(names, "dev,f", "{p}");
    }
    assert!(chain.join("planted").is_dir());
    // Each map reuses its own.
    assert_eq!(split(&plain())[1], path_a);
    assert_eq!(split(&nested())[1], path_b);
    let published = fs::read_dir(state.join("layers/v3/sha256"))
        .unwrap()
        .count();
    assert_eq!(published, 3);
}

/// An entry's owner and mode: what a `chown` or `chmod` would change.
fn owner(p: &Path) -> (u32, u32, u32) {
    let m = fs::symlink_metadata(p).unwrap();
    (m.uid(), m.gid(), m.mode() & 0o7777)
}

/// Another owner's id, mapped by the container and by the build's
/// namespace.
const OTHER: u32 = 4321;

/// Red team: a foreign or symlinked store directory is never changed in the namespace (T13).
#[test]
#[ignore = "T1: default: root in a rootless Podman container plants store directories of other owners"]
fn red_team_a_planted_store_directory_is_never_chowned_in_the_user_namespace() {
    if !alone_in(
        module_path!(),
        "red_team_a_planted_store_directory_is_never_chowned_in_the_user_namespace",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let dirs = ["blobs", "layers", "caches", "locks", "tmp", "builds"];
    for d in dirs {
        for plant in ["foreign", "symlink", "own"] {
            let tmp = private_tempdir();
            let state = tmp.path().join("state");
            let first = in_child(None, || match enter_user_namespace(&state) {
                Ok(s) => s
                    .store
                    .close()
                    .map_or_else(|e| format!("error: {e}"), |()| "ok".into()),
                Err(r) => format!("refused\t{r}"),
            });
            if first.starts_with("refused\t") {
                measured(&format!("no build user namespace here: {first}"));
                return;
            }
            assert_eq!(first, "ok");
            // Root's, with another group and the bit a store directory
            // loses: what a followed symlink would change.
            let outside = tmp.path().join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("file"), b"kept").unwrap();
            chown(&outside, Some(0), Some(OTHER)).unwrap();
            fs::set_permissions(&outside, fs::Permissions::from_mode(0o2770)).unwrap();
            let at = state.join(d);
            match plant {
                "foreign" => {
                    chown(&at, Some(OTHER), Some(OTHER)).unwrap();
                    fs::set_permissions(&at, fs::Permissions::from_mode(0o2770)).unwrap();
                }
                "symlink" => {
                    fs::remove_dir_all(&at).unwrap();
                    std::os::unix::fs::symlink(&outside, &at).unwrap();
                    // Its twin in the state directory, for the store to open.
                    let twin = state.join(outside.strip_prefix("/").unwrap());
                    fs::create_dir_all(twin).unwrap();
                }
                _ => {
                    chown(&at, Some(0), Some(OTHER)).unwrap();
                    fs::set_permissions(&at, fs::Permissions::from_mode(0o2700)).unwrap();
                }
            }
            let before = (owner(&outside), owner(&at));
            let out = in_child(None, || match enter_user_namespace(&state) {
                Ok(s) => {
                    let blob = s.store.fill(
                        &rnk_store::EntryKey::Blob(Digest::from_sha256_bytes(&[1; 32])),
                        |e| -> std::io::Result<u64> {
                            use std::io::Write as _;
                            e.file().expect("a blob is a file").write_all(b"x")?;
                            Ok(1)
                        },
                    );
                    let closed = s.store.close();
                    format!("open\t{blob:?}\t{closed:?}")
                }
                Err(r) => format!("refused\t{r}"),
            });
            let case = format!("{d} {plant}: {out}");
            assert_eq!(owner(&outside), before.0, "{case}");
            assert_eq!(fs::read(outside.join("file")).unwrap(), b"kept", "{case}");
            assert_eq!(fs::read_dir(&outside).unwrap().count(), 1, "{case}");
            match plant {
                "foreign" => assert_eq!(owner(&at), (OTHER, OTHER, 0o2770), "{case}"),
                "symlink" => assert!(fs::symlink_metadata(&at).unwrap().is_symlink(), "{case}"),
                // The control: the namespace may change a store directory.
                _ => {
                    assert!(out.starts_with("open\t"), "{case}");
                    assert_eq!(owner(&at), (0, 0, 0o700), "{case}");
                }
            }
        }
    }
}

/// The lower of [`merged`]: what the top layer's xattrs aim at.
fn aimed_at() -> (Vec<u8>, Digest) {
    Tar::default()
        .dir("./", ROOT, 0o755)
        .dir("d/", ROOT, 0o755)
        .file("d/keep", ROOT, 0o644, b"keep-d")
        .dir("etc/", ROOT, 0o755)
        .file("etc/secret", ROOT, 0o600, b"secret")
        .file("gone", ROOT, 0o644, b"gone")
        .dir("o/", ROOT, 0o755)
        .file("o/keep", ROOT, 0o644, b"keep-o")
        .dir("x/", ROOT, 0o755)
        .file("x/y", ROOT, 0o644, b"lower-y")
        .finish()
}

/// The upper of [`merged`]: overlay's own xattrs under both prefixes, as
/// a layer may name them: an opaque directory, a redirect, a metacopy, a
/// whiteout file in a directory of them; and a real whiteout and opaque
/// directory, the controls.
fn aiming() -> (Vec<u8>, Digest) {
    let both = |rest: &str| {
        [
            format!("user.overlay.{rest}"),
            format!("trusted.overlay.{rest}"),
        ]
    };
    let [uo, to] = both("opaque");
    let [ur, tr] = both("redirect");
    let [um, tm] = both("metacopy");
    let [uw, tw] = both("whiteout");
    let [uws, tws] = both("whiteouts");
    Tar::default()
        .dir("./", ROOT, 0o755)
        .xattrs(&[(&uo, b"y"), (&to, b"y")])
        .dir("d/", ROOT, 0o755)
        .xattrs(&[(&ur, b"/etc"), (&tr, b"/etc")])
        .dir("r/", ROOT, 0o755)
        .file("r/own", ROOT, 0o644, b"own")
        .xattrs(&[
            (&um, b""),
            (&tm, b""),
            (&ur, b"/etc/secret"),
            (&tr, b"/etc/secret"),
        ])
        .file("m", ROOT, 0o644, b"top-m")
        .xattrs(&[(&uws, b"y"), (&tws, b"y"), (&uo, b"x"), (&to, b"x")])
        .dir("x/", ROOT, 0o755)
        .xattrs(&[(&uw, b"y"), (&tw, b"y")])
        .file("x/y", ROOT, 0o644, b"")
        .file(".wh.gone", ROOT, 0o644, b"")
        .dir("o/", ROOT, 0o755)
        .file("o/.wh..wh..opq", ROOT, 0o644, b"")
        .finish()
}

/// What `root` shows of the names [`aiming`] aims at: each one's content,
/// or the entries of a directory, or `absent`.
fn seen(root: &Path) -> String {
    ["d", "d/keep", "gone", "m", "o", "r", "r/secret", "x/y"]
        .iter()
        .map(|name| {
            let p = root.join(name);
            let what = if p.is_dir() {
                let mut names: Vec<String> = fs::read_dir(&p)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                format!("[{}]", names.join(","))
            } else {
                fs::read(&p).map_or_else(
                    |e| match e.kind() {
                        std::io::ErrorKind::NotFound => "absent".to_owned(),
                        _ => format!("error {e}"),
                    },
                    |b| format!("{:?}", String::from_utf8_lossy(&b)),
                )
            };
            format!("{name} {what}\n")
        })
        .collect()
}

/// [`aimed_at`] and [`aiming`] extracted and mounted in the build's user
/// namespace, with the overlay options of every stage: [`seen`] in the
/// read-only view, then after a layer that writes in `d/` and to `m`,
/// which copies them up, in the next view.
fn merged(state: &Path) -> String {
    use rnk_snapshot::{Base, Snapshotter as _, StageRootfs as _};
    in_child(None, || {
        let s = match enter_user_namespace(state) {
            Ok(s) => s,
            Err(r) => return format!("refused\t{r}"),
        };
        let store = Arc::new(s.store);
        let run = || -> Result<String, Box<dyn std::error::Error>> {
            let overlay = Overlay::new(Arc::clone(&store), CAPS)?;
            let (b, b_id) = aimed_at();
            let (t, t_id) = aiming();
            let lower = overlay.base_layer_from(&b_id, b.as_slice(), &[])?;
            let upper =
                overlay.base_layer_from(&t_id, t.as_slice(), std::slice::from_ref(&lower))?;
            let mut stage = overlay.prepare(Base::Image(&[lower, upper]))?;
            stage.view()?;
            let root = stage.mount_point().to_owned();
            let first = seen(&root);
            stage.begin_layer()?;
            fs::write(root.join("d/new"), b"new")?;
            let mut m = fs::OpenOptions::new().append(true).open(root.join("m"))?;
            std::io::Write::write_all(&mut m, b"+")?;
            drop(m);
            let _diff = stage.end_layer()?;
            stage.view()?;
            let next = seen(&root);
            stage.release()?;
            Ok(format!("{first}--\n{next}"))
        };
        let out = run().unwrap_or_else(|e| format!("error: {e}"));
        match Arc::into_inner(store).map(Store::close) {
            Some(Ok(())) => out,
            other => format!("error: close: {other:?}: {out}"),
        }
    })
}

/// Red team: overlay xattr names in a layer never act in a stage's mount or a copy-up (T1, T15).
#[test]
#[ignore = "T1: privileged: mounts the layers of a hostile image in the build's user namespace"]
fn red_team_overlay_xattrs_from_a_layer_never_act_in_a_mount_of_the_user_namespace() {
    if !alone_in(
        module_path!(),
        "red_team_overlay_xattrs_from_a_layer_never_act_in_a_mount_of_the_user_namespace",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a privileged rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let out = merged(&tmp.path().join("state"));
    let view = "\
d [keep]
d/keep \"keep-d\"
gone absent
m \"top-m\"
o []
r [own]
r/secret absent
x/y \"\"
";
    let next = view
        .replace("d [keep]", "d [keep,new]")
        .replace("\"top-m\"", "\"top-m+\"");
    assert_eq!(out, format!("{view}--\n{next}"));
}
