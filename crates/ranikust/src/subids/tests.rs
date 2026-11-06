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
    assert_eq!(user_name(PASSWD_TEXT, 1000), Some("u"));
    assert_eq!(user_name(PASSWD_TEXT, 1001), Some("v"));
    assert_eq!(user_name(PASSWD_TEXT, 1002), None);
    assert_eq!(
        user_name(":x:1002:1002::/:/bin/sh\nbad:x:x:0::/:/\n", 1002),
        None
    );
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
        "u:4294967295:2\n",
        "u:4294967296:1\n",
        "#u:1:5\n",
        "\n",
    ] {
        assert_eq!(first_range(bad, Some("u"), 1000), None, "{bad:?}");
    }
    // The next well-formed one counts.
    assert_eq!(
        first_range("u:1:0\nu:4294967295:1\n", Some("u"), 1000),
        range(u32::MAX, 1)
    );
}

#[test]
fn both_files_must_give_a_range_and_root_needs_none() {
    let etc = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| fs::write(etc.path().join(name), text).unwrap();
    write(PASSWD, PASSWD_TEXT);
    write(SUBUID, "u:100000:65536\n");
    assert_eq!(lookup_in(etc.path(), 1000), None);
    write(SUBGID, "u:165536:1000\n");
    assert_eq!(
        lookup_in(etc.path(), 1000),
        Some(SubordinateIds {
            uids: IdRange {
                start: 100_000,
                count: 65_536
            },
            gids: IdRange {
                start: 165_536,
                count: 1000
            },
        })
    );
    write(SUBUID, "root:100000:65536\n");
    write(SUBGID, "root:100000:65536\n");
    assert_eq!(lookup_in(etc.path(), 0), None);
    assert_eq!(lookup_in(&etc.path().join("missing"), 1000), None);
}
