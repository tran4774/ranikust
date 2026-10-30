//! Wildcards in `COPY`/`ADD` sources: Go's `path/filepath.Match`, which
//! BuildKit applies one path component at a time.
//!
//! [open: Q-080] Interim: BuildKit's rules (a pattern matches the entries
//! at its own depth; `*` and `?` never match `/`); kaniko matches the same
//! way with `filepath.Match` over the whole path.

/// Whether `s` has a wildcard character Go's `filepath.Match` reads.
pub(crate) fn has_wildcard(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Go's `filepath.Match` for one component (no `/` in `name`).
///
/// # Errors
///
/// `ErrBadPattern`'s cases: an unclosed or empty class, or a trailing `\`.
pub(crate) fn matches(pattern: &str, name: &str) -> Result<bool, BadPattern> {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    match_from(&p, &n)
}

/// A pattern Go's `filepath.Match` refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BadPattern;

fn match_from(p: &[char], n: &[char]) -> Result<bool, BadPattern> {
    let (mut pi, mut ni) = (0, 0);
    // Where to retry after a `*`: the pattern after it, and the next name
    // position it may swallow up to.
    let mut star: Option<(usize, usize)> = None;
    loop {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star = Some((pi + 1, ni));
                    pi += 1;
                    continue;
                }
                '?' if ni < n.len() && n[ni] != '/' => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                '[' if ni < n.len() => {
                    let (ok, next) = class(p, pi, n[ni])?;
                    if ok {
                        pi = next;
                        ni += 1;
                        continue;
                    }
                }
                '\\' => {
                    let c = *p.get(pi + 1).ok_or(BadPattern)?;
                    if ni < n.len() && n[ni] == c {
                        pi += 2;
                        ni += 1;
                        continue;
                    }
                }
                c if c != '?' && c != '[' && ni < n.len() && n[ni] == c => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                _ => {}
            }
        } else if ni == n.len() {
            return Ok(true);
        }
        // Mismatch: let the last `*` swallow one more character, unless that
        // character is a separator.
        match star {
            Some((sp, sn)) if sn < n.len() && n[sn] != '/' => {
                star = Some((sp, sn + 1));
                pi = sp;
                ni = sn + 1;
            }
            _ => {
                // Go still reports a malformed rest of the pattern.
                check_rest(&p[pi.min(p.len())..])?;
                return Ok(false);
            }
        }
    }
}

/// Matches `c` against the class starting at `p[at] == '['`; returns the
/// result and the index after `]`.
fn class(p: &[char], at: usize, c: char) -> Result<(bool, usize), BadPattern> {
    let mut i = at + 1;
    let negated = matches!(p.get(i), Some('^'));
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let lo = match p.get(i) {
            None => return Err(BadPattern),
            Some(']') if !first => {
                i += 1;
                break;
            }
            Some('\\') => {
                i += 1;
                *p.get(i).ok_or(BadPattern)?
            }
            // Go's `getEsc`: an unescaped `-` or `]` cannot start a range.
            Some('-' | ']') => return Err(BadPattern),
            Some(&ch) => ch,
        };
        i += 1;
        first = false;
        let hi = if p.get(i) == Some(&'-') && p.get(i + 1).is_some_and(|&x| x != ']') {
            i += 1;
            let h = match p.get(i) {
                Some('\\') => {
                    i += 1;
                    *p.get(i).ok_or(BadPattern)?
                }
                Some('-') | None => return Err(BadPattern),
                Some(&h) => h,
            };
            i += 1;
            h
        } else {
            lo
        };
        if lo <= c && c <= hi {
            matched = true;
        }
    }
    Ok((matched != negated, i))
}

/// Checks the syntax of what remains of a pattern after a mismatch.
fn check_rest(p: &[char]) -> Result<(), BadPattern> {
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '\\' => {
                if i + 1 >= p.len() {
                    return Err(BadPattern);
                }
                i += 2;
            }
            '[' => {
                // Any character works to check the syntax.
                let (_, next) = class(p, i, '\0')?;
                i = next;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_filepath_match_cases() {
        // From Go's `path/filepath/match_test.go`.
        let cases: &[(&str, &str, Result<bool, BadPattern>)] = &[
            ("abc", "abc", Ok(true)),
            ("*", "abc", Ok(true)),
            ("*c", "abc", Ok(true)),
            ("a*", "a", Ok(true)),
            ("a*", "abc", Ok(true)),
            ("a*", "ab/c", Ok(false)),
            ("a*/b", "abc/b", Ok(true)),
            ("a*/b", "a/c/b", Ok(false)),
            ("a*b*c*d*e*/f", "axbxcxdxe/f", Ok(true)),
            ("a*b*c*d*e*/f", "axbxcxdxexxx/f", Ok(true)),
            ("a*b*c*d*e*/f", "axbxcxdxe/xxx/f", Ok(false)),
            ("a*b*c*d*e*/f", "axbxcxdxexxx/fff", Ok(false)),
            ("a*b?c*x", "abxbbxdbxebxczzx", Ok(true)),
            ("a*b?c*x", "abxbbxdbxebxczzy", Ok(false)),
            ("ab[c]", "abc", Ok(true)),
            ("ab[b-d]", "abc", Ok(true)),
            ("ab[e-g]", "abc", Ok(false)),
            ("ab[^c]", "abc", Ok(false)),
            ("ab[^b-d]", "abc", Ok(false)),
            ("ab[^e-g]", "abc", Ok(true)),
            ("a\\*b", "a*b", Ok(true)),
            ("a\\*b", "ab", Ok(false)),
            ("a?b", "a☺b", Ok(true)),
            ("a[^a]b", "a☺b", Ok(true)),
            ("a???b", "a☺b", Ok(false)),
            ("a[^a][^a][^a]b", "a☺b", Ok(false)),
            ("[a-ζ]*", "α", Ok(true)),
            ("*[a-ζ]", "A", Ok(false)),
            ("a?b", "a/b", Ok(false)),
            ("a*b", "a/b", Ok(false)),
            ("[\\]a]", "]", Ok(true)),
            ("[\\-]", "-", Ok(true)),
            ("[x\\-]", "x", Ok(true)),
            ("[x\\-]", "-", Ok(true)),
            ("[x\\-]", "z", Ok(false)),
            ("[\\-x]", "x", Ok(true)),
            ("[\\-x]", "-", Ok(true)),
            ("[\\-x]", "a", Ok(false)),
            ("[]a]", "]", Err(BadPattern)),
            ("[-]", "-", Err(BadPattern)),
            ("[x-]", "x", Err(BadPattern)),
            ("[x-]", "-", Err(BadPattern)),
            ("[x-]", "z", Err(BadPattern)),
            ("[-x]", "x", Err(BadPattern)),
            ("[-x]", "-", Err(BadPattern)),
            ("[-x]", "a", Err(BadPattern)),
            ("\\", "a", Err(BadPattern)),
            ("[a-b-c]", "a", Err(BadPattern)),
            ("[", "a", Err(BadPattern)),
            ("[^", "a", Err(BadPattern)),
            ("[^bc", "a", Err(BadPattern)),
            ("a[", "a", Err(BadPattern)),
            ("a[", "ab", Err(BadPattern)),
            ("*x", "xxx", Ok(true)),
        ];
        for (pattern, name, want) in cases {
            assert_eq!(
                matches(pattern, name),
                *want,
                "{pattern:?} against {name:?}"
            );
        }
    }

    #[test]
    fn wildcards_are_detected() {
        assert!(has_wildcard("*.js"));
        assert!(has_wildcard("a?"));
        assert!(has_wildcard("[ab]"));
        assert!(!has_wildcard("src/app"));
    }
}
