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

/// Root's range for `--userns` shares no id with the host's accounts or
/// other ranges (ADR-0139): a uid range holding a uid of `/etc/passwd`, a
/// gid range holding a gid of `/etc/group` or a primary gid of
/// `/etc/passwd`, or either overlapping another line of its file, its
/// numbers read as `strtoul` reads them, is refused, naming the id or the
/// line; ranges that only touch, and lines that give no id, are not.
#[test]
fn root_s_range_shares_no_id_with_an_account_or_another_range() {
    let etc = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| fs::write(etc.path().join(name), text).unwrap();
    let why = || root_range_in(etc.path()).unwrap_err();
    let uids = "u:100000:65536\nranikust:600000:65536\nv:700000:10\n";
    write(SUBUID, uids);
    write(SUBGID, "u:100000:65536\nranikust:800000:65536\n");
    // Accounts and groups outside the ranges, and ranges that touch them.
    write(
        PASSWD,
        "root:x:0:0::/:/bin/sh\nedge:x:599999:865536::/:/bin/sh\n",
    );
    write(GROUP, "root:x:0:\nedge:x:865536:\n");
    assert!(root_range_in(etc.path()).is_ok());

    write(
        PASSWD,
        "root:x:0:0::/:/bin/sh\nsvc:x:665535:10::/:/bin/sh\n",
    );
    let w = why();
    assert!(w.contains("uid 665535 of svc in"), "{w}");
    assert!(w.contains("passwd"), "{w}");
    assert!(w.contains("ranikust range 600000:65536 of"), "{w}");
    // A primary gid with no group line is a gid in use too.
    write(
        PASSWD,
        "root:x:0:0::/:/bin/sh\nsvc:x:10:800000::/:/bin/sh\n",
    );
    let w = why();
    assert!(w.contains("gid 800000 of svc in"), "{w}");
    write(PASSWD, "root:x:0:0::/:/bin/sh\n");
    write(GROUP, "root:x:0:\nstaff:x:865535:\n");
    let w = why();
    assert!(w.contains("gid 865535 of staff in"), "{w}");
    assert!(w.contains("group"), "{w}");
    write(GROUP, "root:x:0:\n");

    // Another range: by any overlap, not only the same start.
    write(
        SUBUID,
        "u:100000:65536\nranikust:600000:65536\nv:665535:10\n",
    );
    let w = why();
    assert!(w.contains("overlaps line 3 of"), "{w}");
    assert!(w.contains("subuid, v:665535:10"), "{w}");
    write(SUBUID, "w:500000:100001\nranikust:600000:65536\n");
    let w = why();
    assert!(w.contains("overlaps line 1 of"), "{w}");
    write(SUBUID, uids);
    write(SUBGID, "ranikust:800000:65536\nranikust:800001:1\n");
    let w = why();
    assert!(w.contains("overlaps line 2 of"), "{w}");
    assert!(w.contains("subgid, ranikust:800001:1"), "{w}");
    // A sign or spaces, which `strtoul` takes, still name an id in use.
    write(SUBGID, "ranikust:800000:65536\nx: +800001: 1\n");
    let w = why();
    assert!(w.contains("overlaps line 2 of"), "{w}");
    write(SUBGID, "ranikust:800000:65536\n");
    write(
        PASSWD,
        "root:x:0:0::/:/bin/sh\nplus:x: +600001:0::/:/bin/sh\n",
    );
    let w = why();
    assert!(w.contains("uid 600001 of plus in"), "{w}");
    // Lines that give no range or id are passed over: fewer than three
    // fields, an empty range, an id glibc does not read in base 10.
    write(SUBGID, "ranikust:800000:65536\nx:800001\nx:800001:0\n");
    write(
        PASSWD,
        "root:x:0:0::/:/bin/sh\nneg:x:-600001:0::/:/bin/sh\nhex:x:0x927c1:0::/:/bin/sh\n",
    );
    assert!(root_range_in(etc.path()).is_ok());
    // A field more is counted, and a sign refuses.
    write(SUBGID, "ranikust:800000:65536\nx:800001:1:2\n");
    let w = why();
    assert!(w.contains("overlaps line 2 of"), "{w}");
    write(SUBGID, "ranikust:800000:65536\nx:-1:5\n");
    let w = why();
    assert!(w.contains("line 2 of"), "{w}");
    assert!(w.contains("(a sign)"), "{w}");
}

/// `newuidmap` and `newgidmap` give a line to the account its owner names,
/// so an account named `ranikust` in `/etc/passwd` refuses root's range,
/// whichever its ids; a group of that name, or another name, does not.
#[test]
fn an_account_named_ranikust_refuses_root_s_range() {
    let etc = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| fs::write(etc.path().join(name), text).unwrap();
    write(SUBUID, "ranikust:600000:65536\n");
    write(SUBGID, "ranikust:800000:65536\n");
    write(GROUP, "root:x:0:\nranikust:x:1500:\n");
    write(
        PASSWD,
        "root:x:0:0::/:/bin/sh\nranikustx:x:1501:1501::/:/bin/sh\nxranikust:x:1502:1502::/:/bin/sh\n",
    );
    assert!(root_range_in(etc.path()).is_ok());
    write(
        PASSWD,
        "root:x:0:0::/:/bin/sh\nranikust:x:1500:1500::/home/ranikust:/bin/sh\n",
    );
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains("line 2 of"), "{why}");
    assert!(why.contains("passwd"), "{why}");
    assert!(why.contains("account named ranikust"), "{why}");
    assert!(ROOT_RANGE_GUIDANCE.contains("no account named ranikust"));
    // Its fields need not be well-formed, and glibc passes over the white
    // space before its name.
    for passwd in ["ranikust\n", " \tranikust:x:1500:1500::/:/bin/sh\n"] {
        write(PASSWD, passwd);
        let why = root_range_in(etc.path()).unwrap_err();
        assert!(why.contains("account named ranikust"), "{why}");
    }
}

/// Lines shadow's versions read differently are counted (ADR-0139): the
/// first three fields of a line with more, a start or count in another
/// base, a count past 32 bits.
#[test]
fn lines_shadow_reads_as_overlapping_are_counted() {
    let etc = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| fs::write(etc.path().join(name), text).unwrap();
    write(PASSWD, "root:x:0:0::/:/bin/sh\n");
    write(GROUP, "root:x:0:\n");
    write(SUBGID, "ranikust:800000:65536\n");
    let mut accepted = Vec::new();
    for other in [
        "u:0x927c0:10",
        "u:02223700:10",
        "u:600000:10:junk",
        "u:600000:10:",
        "u:500000:4294967296",
        "u:0:99999999999",
    ] {
        write(SUBUID, &format!("ranikust:600000:65536\n{other}\n"));
        if root_range_in(etc.path()).is_ok() {
            accepted.push(other);
        }
    }
    assert!(
        accepted.is_empty(),
        "taken beside an overlapping line: {accepted:?}"
    );
}

/// Each reading of a line's numbers counts: `strtoul`'s in base 0 (C
/// white space and `+` before, `0x` hex, a leading 0 octal) and, for a
/// leading 0, the decimal one; a count of 0 from 0, which shadow's
/// arithmetic makes every id, and a count up to 64 bits. A line whose
/// numbers are not read so is refused, naming it; a trailing `\r` is
/// trimmed and the line counted.
#[test]
fn every_reading_of_a_line_s_numbers_counts() {
    let etc = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &[u8]| fs::write(etc.path().join(name), text).unwrap();
    write(PASSWD, b"root:x:0:0::/:/bin/sh\n");
    write(GROUP, b"root:x:0:\n");
    write(SUBGID, b"ranikust:800000:65536\n");
    let with = |ours: &str, other: &[u8]| {
        let mut text = format!("{ours}\n").into_bytes();
        text.extend_from_slice(other);
        text.push(b'\n');
        write(SUBUID, &text);
        root_range_in(etc.path())
    };
    let ours = "ranikust:600000:65536";
    for overlapping in [
        &b"u:0X927C0:1"[..],
        b"u:+0x927c1:1",
        b"u: \t\x0b\x0c\r600001:1",
        b"u:600001:+1",
        b"u:0:0",
        b"u:00:0",
        b"u:1:18446744073709551615",
        b"u:600001:1\r",
        b"#u:600001:1",
        b"-u:600001:1",
        b":600001:1",
        b"u\xff:600001:1",
    ] {
        let why = with(ours, overlapping).unwrap_err();
        assert!(
            why.contains("overlaps line 2 of"),
            "{}: {why}",
            String::from_utf8_lossy(overlapping)
        );
    }
    // A leading 0: octal for shadow, decimal for others; both count.
    assert!(with("ranikust:100000:65536", b"u:0100000:1").is_err());
    assert!(with("ranikust:32768:1000", b"u:0100000:1").is_err());
    assert!(with("ranikust:40000:1000", b"u:0100000:1").is_ok());
    for apart in [
        &b"u:0x10:5"[..],
        b"u:5:0",
        b"u:600000:0",
        b"u:665536:1",
        b"u:900000:1\r",
        b"u:0",
        b"u",
        b"",
    ] {
        assert!(
            with(ours, apart).is_ok(),
            "{}",
            String::from_utf8_lossy(apart)
        );
    }
    for (unread, why) in [
        (&b"u:-600001:1"[..], "(a sign)"),
        (b"u:600001:-0", "(a sign)"),
        (b"u:600001:1x", "(not a number)"),
        (b"u:600001:1 ", "(not a number)"),
        (b"u:600001:1\r\r", "(not a number)"),
        (b"u:0x:1", "(not a number)"),
        (b"u:0x927c0g:1", "(not a number)"),
        (b"u:0800000:1", "(not a number)"),
        (b"u:+ 600001:1", "(not a number)"),
        (b"u::1", "(not a number)"),
        (b"u: :1", "(not a number)"),
        (b":::", "(not a number)"),
        (b"# old u:900000:1 line", "(not a number)"),
        (b"u:1:18446744073709551616", "(past 64 bits)"),
        (b"u:0x10000000000000000:1", "(past 64 bits)"),
        (b"u:02000000000000000000000:1", "(past 64 bits)"),
        (b"u\x00:900000:1", "NUL byte"),
        (b"u:9\x00", "NUL byte"),
    ] {
        let w = with(ours, unread).unwrap_err();
        assert!(
            w.contains("line 2 of") && w.contains("subuid") && w.contains(why),
            "{}: {w}",
            String::from_utf8_lossy(unread)
        );
    }
    // In `/etc/subgid` too.
    write(SUBUID, b"ranikust:600000:65536\n");
    write(SUBGID, b"u:0xc3500:1\nranikust:800000:65536\n");
    let w = root_range_in(etc.path()).unwrap_err();
    assert!(w.contains("overlaps line 1 of"), "{w}");
    assert!(w.contains("subgid"), "{w}");
}

/// The module `/etc/nsswitch.conf` names for subordinate ids, as libsubid
/// finds it: the first token of the first `subid:` line that has one, at
/// column 0, in any case, `#` lines passed over; `files` is none.
#[test]
fn the_subid_module_is_libsubid_s_reading_of_nsswitch() {
    for (text, module) in [
        (&b""[..], None),
        (b"passwd: files sss\ngroup: files\n", None),
        (b"subid: sss\n", Some("sss")),
        (b"SubId:\t  sss files\n", Some("sss")),
        (b"subid: files sss\n", None),
        (b"subid:\nsubid: freeipa\n", Some("freeipa")),
        (b"#subid: sss\n", None),
        (b" subid: sss\n", None),
        (b"subids: sss\n", None),
        (b"subid: sss", Some("sss")),
        (b"subid: s\xffs\n", Some("s\u{fffd}s")),
        (b"subid:\x0b\x0c\r sss\n", Some("sss")),
    ] {
        assert_eq!(
            subid_module(text).as_deref(),
            module,
            "{}",
            String::from_utf8_lossy(text)
        );
    }
}

/// Red team (ADR-0104, ADR-0139): a `ranikust` line whose end passes
/// `u32`, whose count is 0 or that is padded, signed, hex, commented,
/// CRLF-ended or has a field more is never taken, so root's range stays
/// missing and `--userns` is refused; one ending at the last id the
/// kernel maps is taken, and an id or line at that end still overlaps.
#[test]
fn red_team_root_s_range_takes_no_overflowing_or_padded_ranikust_line() {
    let etc = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &[u8]| fs::write(etc.path().join(name), text).unwrap();
    write(PASSWD, b"root:x:0:0::/:/bin/sh\n");
    write(GROUP, b"root:x:0:\n");
    write(SUBGID, b"ranikust:800000:65536\n");
    for bad in [
        &b"ranikust:4294967295:1"[..],
        b"ranikust:4294901761:65536",
        b"ranikust:1:4294967295",
        b"ranikust:99999999999:1",
        b"ranikust:600000:4294967296",
        b"ranikust:600000:18446744073709551617",
        b"ranikust:600000:0",
        b"ranikust:-600000:65536",
        b"ranikust:+600000:65536",
        b"ranikust: 600000:65536",
        b"ranikust:600000:65536 ",
        b"ranikust:600000:65536\r",
        b" ranikust:600000:65536",
        b"#ranikust:600000:65536",
        b"ranikust:0x927c0:65536",
        b"ranikust:600000:65536:",
        b"ranikust::600000:65536",
        b"\xef\xbb\xbfranikust:600000:65536",
        b"ranikust\x00:600000:65536",
        b"ranikust:600000\x00:65536",
        b"ranikust:600000:65536\xff",
    ] {
        let mut text = bad.to_vec();
        text.push(b'\n');
        write(SUBUID, &text);
        let why = root_range_in(etc.path()).unwrap_err();
        assert!(
            why.contains("subuid has no ranikust range"),
            "{}: {why}",
            String::from_utf8_lossy(bad)
        );
    }
    // The last range the kernel maps: ids up to 4294967294.
    write(SUBUID, b"ranikust:4294901759:65536\n");
    assert_eq!(
        root_range_in(etc.path()).map(|s| s.uids),
        Ok(IdRange {
            start: 4_294_901_759,
            count: 65_536
        })
    );
    write(SUBUID, b"ranikust:4294901759:65536\nx:4294967294:1\n");
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains("overlaps line 2 of"), "{why}");
    write(SUBUID, b"ranikust:4294901759:65536\n");
    write(
        PASSWD,
        b"root:x:0:0::/:/bin/sh\ntop:x:4294967294:0::/:/bin/sh\n",
    );
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains("uid 4294967294 of top in"), "{why}");
}

/// Red team (ADR-0139): a second `ranikust` line, the same or not, a
/// commented or non-UTF-8 line and one whose count reaches every id all
/// count as another range where their numbers overlap root's, and a line
/// with a NUL byte or with words where a number goes is refused, so
/// `--userns` is refused, naming the line; a line with fewer than three
/// fields does not.
#[test]
fn red_team_duplicates_comments_and_binary_lines_still_overlap_root_s_range() {
    let etc = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &[u8]| fs::write(etc.path().join(name), text).unwrap();
    write(PASSWD, b"root:x:0:0::/:/bin/sh\n");
    write(GROUP, b"root:x:0:\n");
    write(SUBGID, b"ranikust:800000:65536\n");
    for (other, line) in [
        (&b"ranikust:600000:65536"[..], 2),
        (b"ranikust:665535:1", 2),
        (b"#x:600001:1", 2),
        (b"#:600001:1", 2),
        (b"\xff\xfe:600001:1", 2),
        (b"x:0:4294967295", 2),
        (b"x:1:4294967295", 2),
        (b"\x00:600001:1", 2),
        (b"x:\t600001:1", 2),
    ] {
        let mut text = b"ranikust:600000:65536\n".to_vec();
        text.extend_from_slice(other);
        text.push(b'\n');
        write(SUBUID, &text);
        let why = root_range_in(etc.path()).unwrap_err();
        assert!(
            why.contains(&format!("line {line} of")),
            "{}: {why}",
            String::from_utf8_lossy(other)
        );
    }
    // The other range before root's: the line number is its own.
    write(SUBUID, b"x:600001:1\nranikust:600000:65536\n");
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains("overlaps line 1 of"), "{why}");
    for (other, line) in [(&b"# old ranikust:600000:65536 line"[..], 2), (b":::", 2)] {
        let mut text = b"ranikust:600000:65536\n".to_vec();
        text.extend_from_slice(other);
        text.push(b'\n');
        write(SUBUID, &text);
        let why = root_range_in(etc.path()).unwrap_err();
        assert!(
            why.contains(&format!("line {line} of")),
            "{}: {why}",
            String::from_utf8_lossy(other)
        );
    }
    write(
        SUBUID,
        b"ranikust:600000:65536\n#\n\nx:600001\n# ranikust\n",
    );
    assert!(root_range_in(etc.path()).is_ok());
}

/// Red team (ADR-0098, ADR-0139): huge id files, 200000 accounts, groups
/// and ranges with an 8 MiB line of junk among them, are read in full
/// without a panic; root's range, last, is taken, and an account or a
/// range at the very end that holds one of its ids still refuses it. The
/// ranges' junk line is refused where it has NULs or fields of words.
#[test]
fn red_team_huge_id_files_are_read_whole_and_fail_closed() {
    const N: u32 = 200_000;
    let etc = tempfile::tempdir().unwrap();
    let (mut passwd, mut group, mut sub) = (Vec::new(), Vec::new(), Vec::new());
    for n in 0..N {
        let id = 1000 + n;
        passwd.extend_from_slice(format!("a{n}:x:{id}:{id}::/:/bin/sh\n").as_bytes());
        group.extend_from_slice(format!("g{n}:x:{id}:\n").as_bytes());
        // Ten ids each, from 1000000, below root's range.
        sub.extend_from_slice(format!("a{n}:{}:10\n", 1_000_000 + 10 * n).as_bytes());
    }
    let junk: Vec<u8> = (0..(8 << 20)).map(|i| b"9:\x00\xff"[i % 4]).collect();
    let mut refused = sub.clone();
    for f in [&mut passwd, &mut group, &mut refused] {
        f.extend_from_slice(&junk);
        f.push(b'\n');
    }
    refused.extend_from_slice(b"ranikust:600000000:65536\n");
    // One field: no range.
    sub.extend(
        junk.iter()
            .map(|b| if *b == b':' || *b == 0 { b'9' } else { *b }),
    );
    sub.push(b'\n');
    sub.extend_from_slice(b"ranikust:600000000:65536\n");
    let write = |name: &str, text: &[u8]| fs::write(etc.path().join(name), text).unwrap();
    write(PASSWD, &passwd);
    write(GROUP, &group);
    write(SUBUID, &sub);
    write(SUBGID, &sub);
    let want = IdRange {
        start: 600_000_000,
        count: 65_536,
    };
    assert_eq!(
        root_range_in(etc.path()),
        Ok(SubordinateIds {
            uids: want,
            gids: want
        })
    );
    let mut late = sub.clone();
    late.extend_from_slice(b"z:600065535:1\n");
    write(SUBUID, &late);
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains("subuid, z:600065535:1"), "{why}");
    write(SUBUID, &sub);
    let mut late = group.clone();
    late.extend_from_slice(b"last:x:600000000:\n");
    write(GROUP, &late);
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains("gid 600000000 of last in"), "{why}");
    write(GROUP, &group);
    write(SUBUID, &refused);
    let why = root_range_in(etc.path()).unwrap_err();
    assert!(why.contains(&format!("line {} of", N + 1)), "{why}");
}
