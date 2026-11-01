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

/// `covers` as it was before it bounded the prefixes it looks at: every
/// ancestor looked up. The reference for the property below.
fn covers_by_every_ancestor(l: &IgnoreList, p: &Path) -> bool {
    let Some(c) = path::clean(p) else {
        return false;
    };
    if c.as_os_str().is_empty() {
        return false;
    }
    let mut first = true;
    for a in c.ancestors() {
        if a.as_os_str().is_empty() {
            break;
        }
        if l.paths.contains(a) || (!first && l.below.contains(a)) {
            return true;
        }
        first = false;
    }
    let names: Vec<&OsStr> = c.iter().collect();
    l.globs.iter().any(|g| g.covers(&names))
}

proptest::proptest! {
    /// Bounding the prefixes by the longest literal entry changes no
    /// answer: random entries (literal, below-only, globs) and paths over a
    /// small alphabet, with `.`, `..`, doubled and trailing slashes.
    #[test]
    fn covers_agrees_with_looking_up_every_ancestor(
        lit in proptest::collection::vec("(/?(a|b|ab|\\.|\\.\\.)){1,4}/?", 0..4),
        below in proptest::collection::vec("(/?(a|b|ab)){1,4}", 0..3),
        globs in proptest::collection::vec("(/?(a|b|\\*|a\\*)){1,3}", 0..2),
        paths in proptest::collection::vec("(/{1,2}(a|b|ab|\\.|\\.\\.)){0,7}/?", 1..16),
    ) {
        let mut l = IgnoreList::new();
        for p in &lit {
            let _ = l.insert(p);
        }
        for p in &below {
            let _ = l.insert_below(p);
        }
        for p in &globs {
            let _ = l.insert_glob(p);
        }
        for p in &paths {
            let p = Path::new(p);
            proptest::prop_assert_eq!(l.covers(p), covers_by_every_ancestor(&l, p), "{}", p.display());
        }
    }
}

#[test]
fn a_deep_path_is_covered_by_a_shallow_entry_and_a_deep_one() {
    let deep: String = (0..3000).map(|i| format!("/d{i}")).collect();
    let l = list(&["d0/d1", &deep[1..]], &["d0/d1/d2/d3"], &[]);
    assert!(l.covers(format!("{deep}/x")));
    assert!(l.covers(&deep));
    assert!(l.covers("d0/d1/d2/d3/d4"));
    assert!(!l.covers("d0"));
    let only_below = list(&[], &["d0/d1/d2/d3"], &[]);
    assert!(!only_below.covers("d0/d1/d2/d3"));
    assert!(only_below.covers(format!("{deep}/x")));
}

/// Red team (criterion 8, resources): an `--ignore-path` glob is matched
/// against every name the walk, extraction and deletion reach, and those
/// names come from the image. The matcher backtracks over every split of a
/// name among the stars, so a pattern with a few stars meets a long name
/// in time exponential in the stars (measured on a release build: 120
/// characters, 4 stars 21 ms, 5 stars 0.46 s, 6 stars 9 s; 255
/// characters, 4 stars 14 s; 200 characters, 5 stars 135 s), per name. Go's
/// `filepath.Match`, which kaniko uses, takes each star's next chunk at
/// its first match and never backtracks across stars. A file named by a
/// hostile base image
/// or `RUN` then stalls a build whose operator passed such a pattern.
#[test]
fn red_team_finding_a_glob_with_stars_against_a_long_name_takes_linear_time() {
    let mut l = IgnoreList::new();
    l.insert_glob("/var/cache/*a*a*a*a*b").unwrap();
    l.insert_glob("/var/cache/*a*a*a*a*a*a*a*a*a*a*c").unwrap();
    // NAME_MAX: the longest name a layer or a `RUN` can make.
    let name = format!("var/cache/{}", "a".repeat(255));
    let started = std::time::Instant::now();
    for _ in 0..100 {
        assert!(!l.covers(&name));
    }
    let took = started.elapsed();
    // 100 rounds of 4 and 10 stars against 255 characters: microseconds
    // each, even in a debug build (was 14 s for one round of 4 stars).
    assert!(
        took < std::time::Duration::from_millis(500),
        "100 rounds took {took:?}"
    );
    // And a long name the stars do match.
    assert!(l.covers(format!("var/cache/{}b", "a".repeat(254))));
}

/// The matcher before the iterative algorithm: every split of the name
/// among the stars. Exponential, kept as the oracle of the property test.
fn match_by_every_split(toks: &[Tok], name: &[char]) -> bool {
    let Some((tok, rest)) = toks.split_first() else {
        return name.is_empty();
    };
    match tok {
        Tok::Star => (0..=name.len()).any(|k| match_by_every_split(rest, &name[k..])),
        _ => name
            .split_first()
            .is_some_and(|(h, n)| takes(tok, *h) && match_by_every_split(rest, n)),
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4000))]
    /// The iterative matcher answers as trying every split does, on random
    /// patterns (`*`, `?`, literals, classes, negated classes, escapes) and
    /// names over a small alphabet, where stars and repeats collide often.
    #[test]
    fn the_linear_matcher_agrees_with_every_split(
        pattern in "(a|b|\\*|\\?|\\[ab\\]|\\[\\^a\\]|\\[a-b\\]|\\\\\\*){0,8}",
        name in "[ab*]{0,10}",
    ) {
        let compiled = Pattern::compile(OsStr::new(&pattern)).expect("valid pattern");
        let chars: Vec<char> = name.chars().collect();
        proptest::prop_assert_eq!(
            match_from(&compiled.0, &chars),
            match_by_every_split(&compiled.0, &chars),
            "pattern {:?} name {:?}", pattern, name
        );
    }
}
