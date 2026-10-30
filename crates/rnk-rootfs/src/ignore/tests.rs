use super::*;

fn list(lit: &[&str], below: &[&str], globs: &[&str]) -> IgnoreList {
    let mut l = IgnoreList::new();
    for p in lit {
        l.insert(p).unwrap();
    }
    for p in below {
        l.insert_below(p).unwrap();
    }
    for p in globs {
        l.insert_glob(p).unwrap();
    }
    l
}

#[test]
fn entries_are_cleaned_and_confined() {
    let mut keep = IgnoreList::new();
    assert!(keep.is_empty());
    for bad in ["", "/", ".", "..", "a/../..", "/../etc"] {
        assert!(
            matches!(keep.insert(bad), Err(Error::InvalidPath { .. })),
            "{bad:?}"
        );
        assert!(keep.insert_below(bad).is_err(), "{bad:?}");
        assert!(keep.insert_glob(bad).is_err(), "{bad:?}");
    }
    assert!(keep.is_empty());
}

#[test]
fn a_literal_covers_itself_and_below_only() {
    let l = list(&["/proc", "ranikust/state/"], &[], &[]);
    for p in [
        "proc",
        "/./proc/",
        "/proc/self/fd",
        "/ranikust/x/../state/a",
    ] {
        assert!(l.covers(p), "{p}");
    }
    for p in ["/", "", "/procs", "/ranikust", "/ranikust/stat", "/../proc"] {
        assert!(!l.covers(p), "{p}");
    }
    assert!(l.contains("/proc"));
    assert!(!l.contains("/proc/self"));
    assert!(l.has_below("/ranikust"));
    assert!(l.has_below("/"));
    assert!(!l.has_below("/proc"));
    assert!(!l.has_below("/ranikust/state"));
}

#[test]
fn a_below_only_entry_keeps_the_directory_itself() {
    // kaniko's `/tmp/apt-key-gpghome`, `PrefixMatchOnly: true`.
    let l = list(&[], &["/tmp/apt-key-gpghome"], &[]);
    assert!(!l.covers("/tmp/apt-key-gpghome"));
    assert!(l.covers("/tmp/apt-key-gpghome/pubring.kbx"));
    assert!(l.covers("/tmp/apt-key-gpghome/a/b"));
    assert!(!l.covers("/tmp"));
    assert!(l.has_below("/tmp"));
    assert_eq!(l.len(), 1);
}

#[test]
fn globs_match_per_component_as_go_filepath_match() {
    let l = list(
        &[],
        &[],
        &["/var/cache/*", "/opt/app-?/log", "/srv/[a-c]x", "/h/[^ab]"],
    );
    assert!(!l.covers("/var/cache"));
    assert!(l.covers("/var/cache/apt"));
    assert!(l.covers("/var/cache/apt/archives/x.deb"));
    assert!(l.covers("/opt/app-1/log"));
    assert!(l.covers("/opt/app-1/log/today"));
    assert!(!l.covers("/opt/app-10/log"));
    assert!(l.covers("/srv/bx"));
    assert!(!l.covers("/srv/dx"));
    assert!(l.covers("/h/c"));
    assert!(!l.covers("/h/a"));
    assert!(l.has_below("/var/cache"));
    assert!(l.has_below("/opt/app-2"));
    assert!(!l.has_below("/opt/app-22"));
}

#[test]
fn a_glob_without_pattern_characters_is_literal() {
    let l = list(&[], &[], &["/data/keep"]);
    assert!(l.contains("/data/keep"));
    assert!(l.covers("/data/keep/x"));
}

#[test]
fn escapes_and_stars_match_as_in_go() {
    let m = |p: &str, n: &str| {
        Pattern::compile(OsStr::new(p))
            .unwrap()
            .matches(OsStr::new(n))
    };
    assert!(m("*", ""));
    assert!(m("*", "abc"));
    assert!(m("a*c", "abbbc"));
    assert!(m("a*c*", "acx"));
    assert!(!m("a*c", "abd"));
    assert!(m("\\*", "*"));
    assert!(!m("\\*", "a"));
    assert!(m("[\\]]", "]"));
    assert!(m("ä?", "äb"));
    assert!(m("[^0-9]x", "ax"));
    assert!(!m("[^0-9]x", "5x"));
}

#[test]
fn malformed_patterns_are_refused() {
    for bad in [
        "[", "[]", "[a", "a\\", "[-a]", "[a-]", "[z-a]", "[]a]", "[a-c-]",
    ] {
        assert_eq!(Pattern::compile(OsStr::new(bad)), None, "{bad}");
        let mut l = IgnoreList::new();
        let e = l.insert_glob(format!("/x/{bad}")).unwrap_err();
        assert!(matches!(e, Error::InvalidPath { .. }), "{bad}: {e}");
    }
}
