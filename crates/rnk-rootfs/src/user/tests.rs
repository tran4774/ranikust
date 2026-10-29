use std::fs;
use std::os::unix::fs::symlink;

use super::*;
use crate::test_util::Fixture;

const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
alice:x:1000:1000:Alice:/home/alice:/bin/sh
bob:x:1001:1002::/home/bob:/bin/sh\r
1234:x:4321:4321:a numeric name::/bin/sh
broken line
evil:x:notanumber:0::/:/bin/sh
huge:x:4294967295:0::/:/bin/sh
+nis
#comment:x:5:5::/:/bin/sh
";

const GROUP: &str = "\
root:x:0:
staff:x:50:alice,bob
wheel:x:10:alice
docker:x:999: alice ,
alice:x:1000:
bobs:x:1002:
dup:x:50:alice
";

fn fixture() -> Fixture {
    let f = Fixture::new();
    fs::create_dir(f.host("etc")).unwrap();
    fs::write(f.host("etc/passwd"), PASSWD).unwrap();
    fs::write(f.host("etc/group"), GROUP).unwrap();
    f
}

fn user(uid: u32, gid: u32, supp: &[u32], home: &str) -> User {
    User {
        uid,
        gid,
        supplementary_gids: supp.to_vec(),
        home: PathBuf::from(home),
    }
}

#[test]
fn user_by_name_with_supplementary_groups() {
    let f = fixture();
    assert_eq!(
        lookup_user(&f.root, "alice").unwrap(),
        user(1000, 1000, &[50, 10, 999], "/home/alice")
    );
    assert_eq!(
        lookup_user(&f.root, "bob").unwrap(),
        user(1001, 1002, &[50], "/home/bob")
    );
    assert_eq!(
        lookup_user(&f.root, "root").unwrap(),
        user(0, 0, &[], "/root")
    );
}

#[test]
fn user_with_explicit_group_has_no_supplementary_groups() {
    let f = fixture();
    assert_eq!(
        lookup_user(&f.root, "alice:wheel").unwrap(),
        user(1000, 10, &[], "/home/alice")
    );
    assert_eq!(
        lookup_user(&f.root, "alice:77").unwrap(),
        user(1000, 77, &[], "/home/alice")
    );
    assert_eq!(
        lookup_user(&f.root, "1000:1000").unwrap(),
        user(1000, 1000, &[], "/home/alice")
    );
}

#[test]
fn numeric_users() {
    let f = fixture();
    // By uid: the entry's group, home and memberships.
    assert_eq!(
        lookup_user(&f.root, "1000").unwrap(),
        user(1000, 1000, &[50, 10, 999], "/home/alice")
    );
    // Unknown uid: gid 0 and home `/`, as Docker.
    assert_eq!(
        lookup_user(&f.root, "5000").unwrap(),
        user(5000, 0, &[], "/")
    );
    // A name made of digits matches by name first; empty home is `/`.
    assert_eq!(
        lookup_user(&f.root, "1234").unwrap(),
        user(4321, 4321, &[], "/")
    );
}

#[test]
fn unknown_names_and_bad_specs_fail() {
    let f = fixture();
    for spec in [
        "nobody",
        "alice:nogroup",
        "evil",
        "huge",
        "",
        ":",
        "alice:",
        ":wheel",
        "4294967295",
        "2147483648",
        "alice:4294967295",
        "+nis",
        "#comment",
    ] {
        let e = lookup_user(&f.root, spec).unwrap_err();
        assert!(matches!(e, Error::User(_)), "{spec:?}: {e}");
    }
    assert_eq!(
        lookup_user(&f.root, "2147483647").unwrap().uid,
        2_147_483_647
    );
}

#[test]
fn chown_gid_defaults_to_uid() {
    let f = fixture();
    let o = |uid, gid| Owner { uid, gid };
    assert_eq!(lookup_owner(&f.root, "alice").unwrap(), o(1000, 1000));
    assert_eq!(lookup_owner(&f.root, "bob").unwrap(), o(1001, 1001));
    assert_eq!(lookup_owner(&f.root, "bob:bobs").unwrap(), o(1001, 1002));
    assert_eq!(lookup_owner(&f.root, "bob:staff").unwrap(), o(1001, 50));
    assert_eq!(lookup_owner(&f.root, "10:20").unwrap(), o(10, 20));
    assert_eq!(lookup_owner(&f.root, "5000").unwrap(), o(5000, 5000));
    assert_eq!(lookup_owner(&f.root, "1234").unwrap(), o(4321, 4321));
    for spec in ["nobody", "alice:nogroup", "", "alice:", "4294967295"] {
        let e = lookup_owner(&f.root, spec).unwrap_err();
        assert!(matches!(e, Error::User(_)), "{spec:?}: {e}");
    }
}

#[test]
fn missing_databases_allow_numeric_ids_only() {
    let f = Fixture::new();
    assert_eq!(lookup_user(&f.root, "0").unwrap(), user(0, 0, &[], "/"));
    assert_eq!(lookup_user(&f.root, "0:0").unwrap(), user(0, 0, &[], "/"));
    // The host has `root`; the root has no passwd at all.
    assert!(matches!(
        lookup_user(&f.root, "root").unwrap_err(),
        Error::User(_)
    ));
    assert!(matches!(
        lookup_owner(&f.root, "root").unwrap_err(),
        Error::User(_)
    ));
}

#[test]
fn databases_are_read_inside_the_root() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("usr/share")).unwrap();
    fs::create_dir(f.host("etc")).unwrap();
    fs::write(f.host("usr/share/passwd"), "carol:x:7:8::/c:/bin/sh\n").unwrap();
    symlink("/usr/share/passwd", f.host("etc/passwd")).unwrap();
    // A group file pointing at the host's: resolved inside, so missing.
    symlink("/proc/self/root/etc/group", f.host("etc/group")).unwrap();
    assert_eq!(
        lookup_user(&f.root, "carol").unwrap(),
        user(7, 8, &[], "/c")
    );
    assert!(lookup_user(&f.root, "carol:root").is_err());
    assert!(lookup_user(&f.root, "root").is_err());
}

#[test]
fn oversized_database_is_refused() {
    let f = Fixture::new();
    fs::create_dir(f.host("etc")).unwrap();
    let file = fs::File::create(f.host("etc/passwd")).unwrap();
    file.set_len(DB_LIMIT + 1).unwrap();
    let e = lookup_user(&f.root, "0").unwrap_err();
    assert!(
        matches!(e, Error::User(ref m) if m.contains("larger")),
        "{e}"
    );
}

/// A FIFO would hang the build and a device node would be opened on the
/// host; neither is opened. The 0/0 character device stands in for a real
/// device: an unprivileged user may create it.
#[test]
fn databases_that_are_not_regular_files_are_refused_unopened() {
    use rustix::fs::{CWD, FileType as Ft, Mode};
    let f = Fixture::new();
    fs::create_dir(f.host("etc")).unwrap();
    rustix::fs::mkfifoat(CWD, f.host("etc/passwd"), Mode::from_raw_mode(0o644)).unwrap();
    let e = lookup_user(&f.root, "root").unwrap_err();
    assert!(matches!(e, Error::InvalidPath { .. }), "{e}");
    fs::remove_file(f.host("etc/passwd")).unwrap();
    rustix::fs::mknodat(
        CWD,
        f.host("etc/passwd"),
        Ft::CharacterDevice,
        Mode::empty(),
        0,
    )
    .unwrap();
    let e = lookup_owner(&f.root, "root").unwrap_err();
    assert!(matches!(e, Error::InvalidPath { .. }), "{e}");
    fs::remove_file(f.host("etc/passwd")).unwrap();
    fs::create_dir(f.host("etc/passwd")).unwrap();
    let e = lookup_user(&f.root, "root").unwrap_err();
    assert!(matches!(e, Error::InvalidPath { .. }), "{e}");
}
