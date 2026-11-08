use std::fs;

use super::*;

const PASSWD_TEXT: &str = "root:x:0:0:root:/root:/bin/sh\n\
     u:x:1000:1000::/home/u:/bin/sh\n\
     twin:x:1000:1000::/home/twin:/bin/sh\n\
     v:x:1001:1001::/home/v:/bin/sh\n";

fn range(start: u32, count: u32) -> Option<IdRange> {
    Some(IdRange { start, count })
}

#[test]
fn the_name_is_the_first_entry_of_the_uid() {
    let passwd = PASSWD_TEXT.as_bytes();
    assert_eq!(user_name(passwd, 1000), Some("u"));
    assert_eq!(user_name(passwd, 1001), Some("v"));
    assert_eq!(user_name(passwd, 1002), None);
    assert_eq!(
        user_name(b":x:1002:1002::/:/bin/sh\nbad:x:x:0::/:/\n", 1002),
        None
    );
    assert_eq!(user_name(b"plus:x:+1002:1002::/:/bin/sh\n", 1002), None);
    // The comment field need not be UTF-8; the name must be.
    assert_eq!(
        user_name(b"jose:x:1002:1002:Jos\xe9:/:/bin/sh\n", 1002),
        Some("jose")
    );
    assert_eq!(user_name(b"jos\xe9:x:1002:1002::/:/bin/sh\n", 1002), None);
}

#[test]
fn the_first_range_of_the_name_or_the_uid_is_taken() {
    let text = "v:200000:65536\nu:100000:65536\nu:300000:10\n";
    assert_eq!(first_range(text, Some("u"), 1000), range(100_000, 65_536));
    // By uid, where the name is unknown or written as a number.
    assert_eq!(
        first_range("1000:400000:5\n", None, 1000),
        range(400_000, 5)
    );
    assert_eq!(
        first_range("1000:400000:5\n", Some("u"), 1000),
        range(400_000, 5)
    );
    assert_eq!(first_range(text, Some("w"), 1002), None);
}

#[test]
fn a_malformed_or_empty_range_is_passed_over() {
    for bad in [
        "u:100000\n",
        "u:100000:65536:extra\n",
        "u:-1:5\n",
        "u:+1:5\n",
        "u: 1:5\n",
        "u:1:0\n",
        "u:x:5\n",
        "u:4294967295:1\n",
        "u:4294967294:2\n",
        "u:4294967296:1\n",
        "#u:1:5\n",
        "\n",
    ] {
        assert_eq!(first_range(bad, Some("u"), 1000), None, "{bad:?}");
    }
    // The next well-formed one counts; the last id the kernel maps is
    // 4294967294.
    assert_eq!(
        first_range("u:1:0\nu:4294967294:1\n", Some("u"), 1000),
        range(u32::MAX - 1, 1)
    );
}

#[test]
fn both_files_must_give_a_range_and_root_needs_none() {
    let etc = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| fs::write(etc.path().join(name), text).unwrap();
    write(PASSWD, PASSWD_TEXT);
    write(SUBUID, "u:100000:65536\n");
    assert_eq!(lookup_in(etc.path(), 1000), Ok(None));
    write(SUBGID, "u:165536:1000\n");
    assert_eq!(
        lookup_in(etc.path(), 1000),
        Ok(Some(SubordinateIds {
            uids: IdRange {
                start: 100_000,
                count: 65_536
            },
            gids: IdRange {
                start: 165_536,
                count: 1000
            },
        }))
    );
    write(SUBUID, "root:100000:65536\n");
    write(SUBGID, "root:100000:65536\n");
    assert_eq!(lookup_in(etc.path(), 0), Ok(None));
    assert_eq!(lookup_in(&etc.path().join("missing"), 1000), Ok(None));
}

/// A file that exists but cannot be read is an error, not "no range"; a
/// range line that is not UTF-8 is passed over, and a `/etc/passwd`
/// comment field that is not UTF-8 hides no name.
#[test]
fn an_unreadable_file_is_an_error_and_a_line_not_utf8_is_passed_over() {
    let etc = tempfile::tempdir().unwrap();
    let mut passwd = b"x:x:999:999:Jos\xe9:/:/bin/sh\n".to_vec();
    passwd.extend_from_slice(PASSWD_TEXT.as_bytes());
    passwd.extend_from_slice(b"jose:x:1003:1003:Jos\xe9:/:/bin/sh\n");
    fs::write(etc.path().join(PASSWD), passwd).unwrap();
    fs::write(
        etc.path().join(SUBUID),
        b"\xff:1:5\nu:100000:65536\njose:200000:65536\n",
    )
    .unwrap();
    fs::write(
        etc.path().join(SUBGID),
        "u:100000:65536\njose:200000:65536\n",
    )
    .unwrap();
    assert_eq!(
        lookup_in(etc.path(), 1000).map(|s| s.map(|s| s.uids)),
        Ok(range(100_000, 65_536))
    );
    // By name, though the user's own comment field is Latin-1.
    assert_eq!(
        lookup_in(etc.path(), 1003).map(|s| s.map(|s| s.uids)),
        Ok(range(200_000, 65_536))
    );
    // A directory where the file should be: read(2) fails with EISDIR,
    // which root's privileges do not lift, as they would a mode.
    fs::remove_file(etc.path().join(SUBGID)).unwrap();
    fs::create_dir(etc.path().join(SUBGID)).unwrap();
    let e = lookup_in(etc.path(), 1000).unwrap_err();
    assert!(
        e.starts_with(&format!("{}:", etc.path().join(SUBGID).display())),
        "{e}"
    );
}

/// Hostile id files (ADR-0098): many lines of other owners, binary and
/// overlong lines, NULs, the user's malformed ranges first, a second
/// entry of the uid: the user's first well-formed range only, never two
/// ranges joined or another owner's, and no panic.
#[test]
fn red_team_hostile_id_files_give_only_the_user_s_first_range() {
    const OTHERS: u32 = 100_000;
    let etc = tempfile::tempdir().unwrap();
    let mut passwd = Vec::new();
    let mut subuid = Vec::new();
    for n in 0..OTHERS {
        let uid = 2000 + n;
        passwd.extend_from_slice(format!("o{n}:x:{uid}:{uid}::/:/bin/sh\n").as_bytes());
        subuid.extend_from_slice(format!("o{n}:{}:65536\n", 200_000 + n).as_bytes());
    }
    let junk: [&[u8]; 8] = [
        b"\xff\xfe\x00\x01:\x00:\x00\n",
        b"\n\n\n",
        b":::::::\n",
        b"u:x\n",
        b"u:x:\n",
        b"u:x::1000::/:/bin/sh\n",
        b"u:x:4294967296:1000::/:/bin/sh\n",
        b"u:x:-1000:1000::/:/bin/sh\n",
    ];
    for j in junk {
        passwd.extend_from_slice(j);
        subuid.extend_from_slice(j);
    }
    passwd.extend(std::iter::repeat_n(b'a', 1 << 20));
    passwd.extend_from_slice(b"\nu:x:1000:1000::/home/u:/bin/sh\nw:x:1000:1000::/:/bin/sh\n");
    // Before the user's well-formed range: every way a range can be wrong
    // for it, and lines another user's name or uid owns.
    subuid.extend(std::iter::repeat_n(b'9', 1 << 20));
    subuid.extend_from_slice(
        b"\nu:1:0\nu:4294967295:1\nu:1:4294967295\nu:4294967294:2\nu:+5:5\nu: 5:5\n\
          u:5:5:5\nu:5\n\x75\xff:5:5\nu\x00:6:6\nw:7:7\n1001:8:8\n01000:9:9\nU:10:10\nu :11:11\n\
          u:300000:65536\nu:400000:65536\n1000:500000:65536\n",
    );
    let write = |name: &str, bytes: &[u8]| fs::write(etc.path().join(name), bytes).unwrap();
    write(PASSWD, &passwd);
    write(SUBUID, &subuid);
    write(SUBGID, &subuid);
    let want = IdRange {
        start: 300_000,
        count: 65_536,
    };
    assert_eq!(
        lookup_in(etc.path(), 1000),
        Ok(Some(SubordinateIds {
            uids: want,
            gids: want
        }))
    );
    // Another user of the files gets its own, by name.
    assert_eq!(
        lookup_in(etc.path(), 2000 + 7).map(|s| s.map(|s| s.uids)),
        Ok(range(200_007, 65_536))
    );
    // A uid without a name gets its line by number only; one without a
    // line of its own, nothing.
    assert_eq!(
        lookup_in(etc.path(), 1001).map(|s| s.map(|s| s.uids)),
        Ok(range(8, 8))
    );
    assert_eq!(lookup_in(etc.path(), 1002), Ok(None));
}

/// Root's range for `--userns` (ADR-0104): the first well-formed
/// `ranikust` line of each file, by that name only; none, one from 0 or a
/// range in one file only is refused.
#[test]
fn root_s_range_is_the_ranikust_entry_of_both_files_and_nothing_else() {
    let etc = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| fs::write(etc.path().join(name), text).unwrap();
    // Missing files: no range, saying which file; the guidance, what to add.
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains("subuid has no ranikust range"), "{why}");
    assert!(ROOT_RANGE_GUIDANCE.contains("ranikust:<start>:65536"));
    assert!(ROOT_RANGE_GUIDANCE.contains("/etc/subuid and /etc/subgid"));
    // Root's other entries, podman's and a uid are not taken; malformed
    // lines are passed over.
    write(
        SUBUID,
        "root:100000:65536\ncontainers:200000:65536\n0:300000:65536\nranikust:5\n\
         ranikust:1:0\nRanikust:7:7\nranikust :8:8\nranikust:600000:65536\nranikust:700000:5\n",
    );
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains("subgid has no ranikust range"), "{why}");
    write(SUBGID, "ranikust:800000:1000\n");
    assert_eq!(
        root_range_in(etc.path()),
        Ok(SubordinateIds {
            uids: IdRange {
                start: 600_000,
                count: 65_536
            },
            gids: IdRange {
                start: 800_000,
                count: 1000
            },
        })
    );
    // A range from 0 would map the host's root.
    write(SUBGID, "ranikust:0:65536\nranikust:800000:1000\n");
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains("starts at 0"), "{why}");
    // Root's own lookup maps its own ids, the ranikust entry or not.
    assert_eq!(lookup_in(etc.path(), 0), Ok(None));
}
