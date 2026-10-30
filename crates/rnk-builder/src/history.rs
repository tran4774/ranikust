//! The `created_by` text of each instruction's history entry, as BuildKit
//! writes it (Q-131, `ADR-0032`).
//!
//! BuildKit formats these with Go's `fmt` verbs
//! (`buildkit@cd8ee244 frontend/dockerfile/dockerfile2llb/convert.go:1333-1731`,
//! `convert_copy.go:102-329`, `convert_expose.go:44`): `%q` of a string list
//! for `CMD`/`ENTRYPOINT`, `%v` for `VOLUME`/`SHELL`, `%+v` of the health
//! config for `HEALTHCHECK`. The helpers here print what those verbs print.

use rnk_dockerfile::{ArgDecl, EnvList, Healthcheck};

mod isprint;

/// Go's `fmt.Sprintf("%v", list)` of a `[]string`: `[a b c]`.
pub(crate) fn go_list(items: &[String]) -> String {
    format!("[{}]", items.join(" "))
}

/// Go's `fmt.Sprintf("%q", list)` of a `[]string`: each item quoted as
/// `strconv.Quote` does, `["a" "b"]`.
pub(crate) fn go_quoted_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| go_quote(s)).collect();
    format!("[{}]", quoted.join(" "))
}

/// Go's `strconv.Quote` (`appendEscapedRune`): double quotes; `"` and `\`
/// escaped; a character Go's `strconv.IsPrint` accepts kept as it is; any
/// other written as `\a`…`\v`, `\xHH` below U+0080, `\uHHHH` or
/// `\UHHHHHHHH`.
pub(crate) fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        let escaped = match c {
            '"' => "\\\"".to_owned(),
            '\\' => "\\\\".to_owned(),
            c if go_is_print(c) => {
                out.push(c);
                continue;
            }
            '\x07' => "\\a".to_owned(),
            '\x08' => "\\b".to_owned(),
            '\x0c' => "\\f".to_owned(),
            '\n' => "\\n".to_owned(),
            '\r' => "\\r".to_owned(),
            '\t' => "\\t".to_owned(),
            '\x0b' => "\\v".to_owned(),
            c if (c as u32) < 0x20 || c == '\x7f' => format!("\\x{:02x}", c as u32),
            c if (c as u32) < 0x1_0000 => format!("\\u{:04x}", c as u32),
            c => format!("\\U{:08x}", c as u32),
        };
        out.push_str(&escaped);
    }
    out.push('"');
    out
}

/// Go's `strconv.IsPrint`: a letter, mark, number, punctuation, symbol or
/// the ASCII space, by Unicode 15.0 as Go 1.21's tables give them.
fn go_is_print(c: char) -> bool {
    let r = c as u32;
    if r <= 0xff {
        return (0x20..=0x7e).contains(&r) || ((0xa1..=0xff).contains(&r) && r != 0xad);
    }
    // The first index whose value is not below `x`, as Go's `bsearch`.
    fn first_ge<T: Ord + Copy>(a: &[T], x: T) -> usize {
        a.partition_point(|&v| v < x)
    }
    if let Ok(rr) = u16::try_from(r) {
        let i = first_ge(&isprint::IS_PRINT16, rr);
        if i >= isprint::IS_PRINT16.len()
            || rr < isprint::IS_PRINT16[i & !1]
            || isprint::IS_PRINT16[i | 1] < rr
        {
            return false;
        }
        let j = first_ge(&isprint::IS_NOT_PRINT16, rr);
        return j >= isprint::IS_NOT_PRINT16.len() || isprint::IS_NOT_PRINT16[j] != rr;
    }
    let i = first_ge(&isprint::IS_PRINT32, r);
    if i >= isprint::IS_PRINT32.len()
        || r < isprint::IS_PRINT32[i & !1]
        || isprint::IS_PRINT32[i | 1] < r
    {
        return false;
    }
    if r >= 0x2_0000 {
        return true;
    }
    // Below 0x20000, so it fits once 0x10000 is taken off.
    let Ok(rr) = u16::try_from(r - 0x1_0000) else {
        return false;
    };
    let j = first_ge(&isprint::IS_NOT_PRINT32, rr);
    j >= isprint::IS_NOT_PRINT32.len() || isprint::IS_NOT_PRINT32[j] != rr
}

/// Go's `time.Duration.String`: `0s`, `500ms`, `1.5s`, `1m30s`, `1h0m0s`.
pub(crate) fn go_duration(d: std::time::Duration) -> String {
    let u = d.as_nanos();
    if u == 0 {
        return "0s".to_owned();
    }
    if u < 1_000_000_000 {
        let (prec, unit) = match u {
            0..1_000 => (0, "ns"),
            1_000..1_000_000 => (3, "µs"),
            _ => (6, "ms"),
        };
        let (int, frac) = go_frac(u, prec);
        return format!("{int}{frac}{unit}");
    }
    let (secs, frac) = go_frac(u, 9);
    let mut out = format!("{}{frac}s", secs % 60);
    let mins = secs / 60;
    if mins > 0 {
        out = format!("{}m{out}", mins % 60);
        let hours = mins / 60;
        if hours > 0 {
            out = format!("{hours}h{out}");
        }
    }
    out
}

/// Go's `fmtFrac`: `v` split at `prec` decimal digits; the fraction as
/// `.ddd` without trailing zeros, empty when zero.
fn go_frac(v: u128, prec: u32) -> (u128, String) {
    let scale = 10u128.pow(prec);
    let frac = v % scale;
    if frac == 0 {
        return (v / scale, String::new());
    }
    let digits = format!("{frac:0width$}", width = prec as usize);
    (v / scale, format!(".{}", digits.trim_end_matches('0')))
}

/// `HEALTHCHECK %+v` of BuildKit's `HealthcheckConfig`: `{Test:[CMD-SHELL
/// x] Interval:30s Timeout:0s StartPeriod:0s StartInterval:0s Retries:3}`.
pub(crate) fn healthcheck(h: &Healthcheck) -> String {
    format!(
        "HEALTHCHECK {{Test:{} Interval:{} Timeout:{} StartPeriod:{} StartInterval:{} Retries:{}}}",
        go_list(&h.test),
        go_duration(h.interval),
        go_duration(h.timeout),
        go_duration(h.start_period),
        go_duration(h.start_interval),
        h.retries
    )
}

/// `ARG` as BuildKit's `dispatchArg` commits it: `ARG a=1 b`, with each
/// resolved value.
pub(crate) fn arg(decls: &[ArgDecl]) -> String {
    let parts: Vec<String> = decls
        .iter()
        .map(|d| match &d.value {
            Some(v) => format!("{}={v}", d.key),
            None => d.key.clone(),
        })
        .collect();
    format!("ARG {}", parts.join(" "))
}

/// BuildKit's `runCommandString`: the stage's `ARG`s, each with its value
/// in the environment, else its declared value, prefixed by `|N`; then the
/// argv, joined by spaces.
pub(crate) fn run(argv: &[String], args: &[ArgDecl], env: &EnvList) -> String {
    let mut build_env: Vec<(String, String)> = Vec::new();
    for a in args {
        let v = env
            .get(&a.key)
            .map(str::to_owned)
            .unwrap_or_else(|| a.value.clone().unwrap_or_default());
        match build_env.iter_mut().find(|(k, _)| *k == a.key) {
            Some(entry) => entry.1 = v,
            None => build_env.push((a.key.clone(), v)),
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if !build_env.is_empty() {
        parts.push(format!("|{}", build_env.len()));
        parts.extend(build_env.into_iter().map(|(k, v)| format!("{k}={v}")));
    }
    parts.extend(argv.iter().cloned());
    format!("RUN {}", parts.join(" "))
}

/// `COPY`/`ADD` as `dispatchCopy` commits them: the name, `--chown=` and
/// `--chmod=` when given, the sources, `<<name` per heredoc, the
/// destination as written.
pub(crate) fn copy(
    add: bool,
    chown: &str,
    chmod: &str,
    sources: &[String],
    heredocs: &[String],
    dest: &str,
) -> String {
    let mut out = String::from(if add { "ADD" } else { "COPY" });
    if !chown.is_empty() {
        out.push_str(&format!(" --chown={chown}"));
    }
    if !chmod.is_empty() {
        out.push_str(&format!(" --chmod={chmod}"));
    }
    for s in sources {
        out.push_str(&format!(" {s}"));
    }
    for h in heredocs {
        out.push_str(&format!(" <<{h}"));
    }
    out.push_str(&format!(" {dest}"));
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn lists_print_as_go() {
        assert_eq!(go_list(&s(&["/bin/sh", "-c"])), "[/bin/sh -c]");
        assert_eq!(go_list(&[]), "[]");
        assert_eq!(
            go_quoted_list(&s(&["/bin/sh", "-c", "echo \"hi\"\tx\\"])),
            r#"["/bin/sh" "-c" "echo \"hi\"\tx\\"]"#
        );
        assert_eq!(go_quote("a\x01é\u{85}"), r#""a\x01é\u0085""#);
    }

    #[test]
    fn quote_escapes_what_go_does_not_print() {
        // Checked against Go 1.21's strconv.Quote.
        let cases = [
            ("\u{a0}", r#""\u00a0""#),   // no-break space: Zs, not printed
            ("\u{ad}", r#""\u00ad""#),   // soft hyphen: Cf
            ("\u{200b}", r#""\u200b""#), // zero width space: Cf
            ("\u{2028}", r#""\u2028""#), // line separator: Zl
            ("\u{2029}", r#""\u2029""#), // paragraph separator: Zp
            ("\u{e000}", r#""\ue000""#), // private use
            ("\u{378}", r#""\u0378""#),  // unassigned
            ("\u{fffe}", r#""\ufffe""#), // noncharacter
            ("\u{10ffff}", r#""\U0010ffff""#),
            ("\u{1f600}", "\"\u{1f600}\""),     // emoji: printed
            ("日本\u{301}", "\"日本\u{301}\""), // marks: printed
            ("\x7f", r#""\x7f""#),
        ];
        for (input, want) in cases {
            assert_eq!(go_quote(input), want, "{input:?}");
        }
    }

    #[test]
    fn durations_print_as_go() {
        let cases = [
            (Duration::ZERO, "0s"),
            (Duration::from_nanos(1), "1ns"),
            (Duration::from_nanos(1_500), "1.5µs"),
            (Duration::from_millis(500), "500ms"),
            (Duration::from_millis(1_500), "1.5s"),
            (Duration::from_secs(30), "30s"),
            (Duration::from_secs(90), "1m30s"),
            (Duration::from_secs(3_600), "1h0m0s"),
            (Duration::from_secs(3_661), "1h1m1s"),
        ];
        for (d, want) in cases {
            assert_eq!(go_duration(d), want, "{d:?}");
        }
    }

    #[test]
    fn healthcheck_prints_as_buildkit() {
        let h = Healthcheck {
            test: s(&["CMD-SHELL", "curl -f localhost"]),
            interval: Duration::from_secs(30),
            retries: 3,
            ..Healthcheck::default()
        };
        assert_eq!(
            healthcheck(&h),
            "HEALTHCHECK {Test:[CMD-SHELL curl -f localhost] Interval:30s Timeout:0s \
             StartPeriod:0s StartInterval:0s Retries:3}"
        );
    }

    #[test]
    fn run_lists_the_stage_args_first() {
        let args = vec![
            ArgDecl {
                key: "A".to_owned(),
                value: Some("1".to_owned()),
            },
            ArgDecl {
                key: "B".to_owned(),
                value: None,
            },
            ArgDecl {
                key: "A".to_owned(),
                value: Some("2".to_owned()),
            },
        ];
        let env = EnvList::from_entries(&["A=3"]);
        assert_eq!(
            run(&s(&["/bin/sh", "-c", "make"]), &args, &env),
            "RUN |2 A=3 B= /bin/sh -c make"
        );
        assert_eq!(run(&s(&["make"]), &[], &EnvList::new()), "RUN make");
        assert_eq!(arg(&args[..2]), "ARG A=1 B");
    }

    #[test]
    fn copy_prints_flags_sources_and_destination() {
        assert_eq!(
            copy(false, "app", "755", &s(&["a", "b*"]), &s(&["EOF"]), "/dst/"),
            "COPY --chown=app --chmod=755 a b* <<EOF /dst/"
        );
        assert_eq!(copy(true, "", "", &s(&["x"]), &[], "."), "ADD x .");
    }
}
