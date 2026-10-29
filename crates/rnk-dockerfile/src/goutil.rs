//! Small helpers that reproduce the behaviour of Go standard-library
//! functions BuildKit relies on (`unicode`, `strings`, `strconv`, `time`),
//! written from their documented behaviour.
//!
//! Go strings are byte strings and Rust strings are UTF-8, so every helper
//! here works on valid UTF-8 and documents where Go would differ on invalid
//! input.
#![allow(
    dead_code,
    reason = "the flag and duration helpers are used by the instruction layer, added next"
)]

/// Go's `unicode.IsSpace`. Go's table is the Unicode `White_Space`
/// property, which is exactly what `char::is_whitespace` implements.
pub(crate) fn is_space(c: char) -> bool {
    c.is_whitespace()
}

/// Go's `unicode.IsSpace(rune(b))` for a single byte, as BuildKit uses it
/// when it walks a string byte by byte: the byte is read as a Latin-1 code
/// point, so the UTF-8 continuation bytes `0x85` and `0xA0` count as spaces.
pub(crate) fn is_space_latin1(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ' | 0x85 | 0xA0)
}

/// Go's `unicode.IsDigit` (general category `Nd`). Exact for ASCII; for
/// other characters Rust only offers the wider `N` category, which also
/// accepts `Nl` and `No` (`Ⅻ`, `²`).
pub(crate) fn is_digit(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_digit()
    } else {
        c.is_numeric()
    }
}

/// Go's `unicode.IsLetter` (general category `L`). Exact for Latin-1; above
/// it, Rust's `Alphabetic` property minus numbers, which also accepts a few
/// combining marks.
pub(crate) fn is_letter(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphabetic()
    } else {
        c.is_alphabetic() && !c.is_numeric()
    }
}

/// Go's `strings.ToLower`: one simple mapping per character. Rust's
/// `to_lowercase` only differs on `İ` (U+0130), which it maps to two
/// characters and Go maps to `i`.
pub(crate) fn to_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let mut lower = c.to_lowercase();
        match (lower.next(), lower.next()) {
            (Some(l), None) => out.push(l),
            _ => out.push(if c == '\u{130}' { 'i' } else { c }),
        }
    }
    out
}

/// Go's `strings.ToUpper`, close enough for comparing against ASCII
/// keywords: characters whose uppercase form is several characters (`ß`,
/// ligatures) stay as they are, as they do in Go.
pub(crate) fn to_upper(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let mut upper = c.to_uppercase();
        match (upper.next(), upper.next()) {
            (Some(u), None) => out.push(u),
            _ => out.push(c),
        }
    }
    out
}

/// Go's `strings.EqualFold(s, target)` for an ASCII `target`: simple case
/// folding, under which `ſ` (U+017F) equals `s` and the Kelvin sign
/// (U+212A) equals `k`.
pub(crate) fn equal_fold_ascii(s: &str, target: &str) -> bool {
    let mut a = s.chars();
    let mut b = target.chars();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                let y = y.to_ascii_lowercase();
                let x = match x {
                    '\u{17F}' => 's',
                    '\u{212A}' => 'k',
                    other => other.to_ascii_lowercase(),
                };
                if x != y {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// Go's `strings.TrimSpace`.
pub(crate) fn trim_space(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Go's `strconv.Quote`, used by the AST dump. Printable characters stay as
/// they are; Go's `IsPrint` is approximated as "not a control character and
/// not white space other than the ASCII space".
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            ' ' => out.push(' '),
            c if c.is_control() || c.is_whitespace() || c == '\u{FEFF}' => {
                let v = u32::from(c);
                if v < 0x80 {
                    out.push_str(&format!("\\x{v:02x}"));
                } else if v <= 0xFFFF {
                    out.push_str(&format!("\\u{v:04x}"));
                } else {
                    out.push_str(&format!("\\U{v:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Go's `strconv.ParseBool`.
pub(crate) fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Go's `strconv.ParseUint(s, radix, bits)` for an explicit radix: no sign,
/// no prefix, no underscores.
pub(crate) fn parse_uint(s: &str, radix: u32, bits: u32) -> Option<u64> {
    if s.is_empty() || !s.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let v = u64::from_str_radix(s, radix).ok()?;
    if bits < 64 && v >> bits != 0 {
        return None;
    }
    Some(v)
}

/// Go's `strconv.ParseInt(s, 10, 32)`.
pub(crate) fn parse_int32(s: &str) -> Option<i32> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i32>().ok()
}

/// A duration in nanoseconds, as Go's `time.Duration`.
pub(crate) type Nanos = i64;

/// Go's `time.ParseDuration`: a possibly signed sequence of decimal numbers,
/// each with an optional fraction and a unit (`ns`, `us`, `µs`, `μs`, `ms`,
/// `s`, `m`, `h`), such as `300ms`, `-1.5h` or `2h45m`; `0` needs no unit.
pub(crate) fn parse_duration(s: &str) -> Result<Nanos, String> {
    let invalid = || format!("time: invalid duration \"{s}\"");
    let (neg, mut rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if rest == "0" {
        return Ok(0);
    }
    if rest.is_empty() {
        return Err(invalid());
    }
    // Total in nanoseconds; Go allows up to 1<<63 before the sign is applied.
    let limit: u128 = 1 << 63;
    let mut total: u128 = 0;
    while !rest.is_empty() {
        let int_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let int_part = &rest[..int_len];
        rest = &rest[int_len..];
        let mut frac_part = "";
        if let Some(after_dot) = rest.strip_prefix('.') {
            let frac_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
            frac_part = &after_dot[..frac_len];
            rest = &after_dot[frac_len..];
            if int_len == 0 && frac_len == 0 {
                return Err(invalid());
            }
        } else if int_len == 0 {
            return Err(invalid());
        }
        let unit_len = rest
            .char_indices()
            .find(|&(_, c)| c == '.' || c.is_ascii_digit())
            .map_or(rest.len(), |(i, _)| i);
        let unit = &rest[..unit_len];
        rest = &rest[unit_len..];
        let scale: u128 = match unit {
            "ns" => 1,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            "" => return Err(format!("time: missing unit in duration \"{s}\"")),
            _ => return Err(format!("time: unknown unit \"{unit}\" in duration \"{s}\"")),
        };
        let mut value: u128 = 0;
        for b in int_part.bytes() {
            value = value * 10 + u128::from(b - b'0');
            if value > limit {
                return Err(invalid());
            }
        }
        value *= scale;
        // The fraction contributes floor(frac * scale), as Go computes it.
        let mut frac_value: u128 = 0;
        let mut frac_scale: u128 = 1;
        for b in frac_part.bytes() {
            if frac_scale > limit {
                break;
            }
            frac_value = frac_value * 10 + u128::from(b - b'0');
            frac_scale *= 10;
        }
        value += frac_value * scale / frac_scale;
        total += value;
        if total > limit {
            return Err(invalid());
        }
    }
    if neg {
        i64::try_from(total).map(|v| -v).or_else(|_| {
            if total == limit {
                Ok(i64::MIN)
            } else {
                Err(invalid())
            }
        })
    } else {
        i64::try_from(total).map_err(|_| invalid())
    }
}

/// `go-units`' `RAMInBytes`: a size with an optional binary unit (`b`, `k`,
/// `kb`, `kib`, … up to `p`), such as `64m` or `1.5GiB`.
pub(crate) fn ram_in_bytes(s: &str) -> Result<i64, String> {
    let sep = s.rfind(|c: char| c.is_ascii_digit() || c == '.' || c == ' ');
    let Some(sep) = sep else {
        return Err(format!("invalid size: '{s}'"));
    };
    // The separator is ASCII, so slicing at it and after it is valid.
    let (num, sfx) = if s.as_bytes()[sep] == b' ' {
        (&s[..sep], &s[sep + 1..])
    } else {
        (&s[..=sep], &s[sep + 1..])
    };
    let valid_float = !num.is_empty()
        && num
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'));
    let size: f64 = if valid_float { num.parse().ok() } else { None }
        .ok_or_else(|| format!("strconv.ParseFloat: parsing \"{num}\": invalid syntax"))?;
    if size < 0.0 {
        return Err(format!("invalid size: '{s}'"));
    }
    let bad_suffix = || format!("invalid suffix: '{}'", to_lower(sfx));
    let mul: f64 = if sfx.is_empty() {
        1.0
    } else {
        let lower = sfx.to_ascii_lowercase();
        let b = lower.as_bytes();
        if b.len() > 3 {
            return Err(bad_suffix());
        }
        if b[0] == b'b' {
            if b.len() > 1 {
                return Err(bad_suffix());
            }
            1.0
        } else {
            let m: f64 = match b[0] {
                b'k' => 1024.0,
                b'm' => 1_048_576.0,
                b'g' => 1_073_741_824.0,
                b't' => 1_099_511_627_776.0,
                b'p' => 1_125_899_906_842_624.0,
                _ => return Err(bad_suffix()),
            };
            if (b.len() == 2 && b[1] != b'b') || (b.len() == 3 && &b[1..] != b"ib") {
                return Err(bad_suffix());
            }
            m
        }
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Go converts the float with int64(size); Rust's cast saturates instead of wrapping"
    )]
    Ok((size * mul) as i64)
}

/// A single CSV record, as `go-csvvalue`'s `Fields` (and `encoding/csv`
/// without lazy quotes) reads it: fields separated by `,`; a field that
/// starts with `"` is quoted and `""` inside it is one `"`; a bare `"` in an
/// unquoted field is an error. One trailing `\n` or `\r\n` is dropped; an
/// empty record is an error (`EOF`).
pub(crate) fn csv_fields(line: &str) -> Result<Vec<String>, String> {
    let line = line
        .strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line);
    if line.is_empty() {
        return Err("EOF".to_owned());
    }
    let mut fields = Vec::new();
    let mut rest = line;
    let mut col = 1usize;
    loop {
        if let Some(quoted) = rest.strip_prefix('"') {
            col += 1;
            let mut field = String::new();
            let mut r = quoted;
            loop {
                let Some(i) = r.find('"') else {
                    return Err(format!(
                        "parse error on line 1, column {}: extraneous or missing \" in quoted-field",
                        col + r.len()
                    ));
                };
                field.push_str(&r[..i]);
                col += i + 1;
                r = &r[i + 1..];
                if let Some(after) = r.strip_prefix('"') {
                    field.push('"');
                    col += 1;
                    r = after;
                } else if let Some(after) = r.strip_prefix(',') {
                    fields.push(field);
                    col += 1;
                    rest = after;
                    break;
                } else if r.is_empty() {
                    fields.push(field);
                    return Ok(fields);
                } else {
                    return Err(format!(
                        "parse error on line 1, column {}: extraneous or missing \" in quoted-field",
                        col - 1
                    ));
                }
            }
        } else {
            let (field, next) = match rest.find(',') {
                Some(i) => (&rest[..i], Some(&rest[i + 1..])),
                None => (rest, None),
            };
            if let Some(j) = field.find('"') {
                return Err(format!(
                    "parse error on line 1, column {}: bare \" in non-quoted-field",
                    col + j
                ));
            }
            fields.push(field.to_owned());
            match next {
                Some(n) => {
                    col += field.len() + 1;
                    rest = n;
                }
                None => return Ok(fields),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_mapping_follows_go() {
        assert_eq!(to_lower("FROM"), "from");
        assert_eq!(to_lower("\u{130}"), "i");
        assert_eq!(to_lower("WOR\u{212A}DIR"), "workdir");
        assert_eq!(to_upper("stra\u{df}e"), "STRA\u{df}E");
        assert!(equal_fold_ascii("AS", "as"));
        assert!(equal_fold_ascii("a\u{17f}", "as"));
        assert!(!equal_fold_ascii("ass", "as"));
        assert!(!equal_fold_ascii("a", "as"));
    }

    #[test]
    fn spaces_follow_go() {
        assert!(is_space('\u{85}') && is_space('\u{a0}') && is_space('\u{3000}'));
        assert!(!is_space('\u{200b}'));
        assert!(is_space_latin1(0xA0) && !is_space_latin1(0xC3));
        assert_eq!(trim_space("\u{a0} x \t"), "x");
    }

    #[test]
    fn quote_matches_strconv() {
        assert_eq!(quote("a\"b\\c\td"), r#""a\"b\\c\td""#);
        assert_eq!(quote("\u{1}é"), "\"\\x01é\"");
        assert_eq!(quote("\u{a0}"), "\"\\u00a0\"");
    }

    #[test]
    fn numbers_follow_strconv() {
        assert_eq!(parse_bool("True"), Some(true));
        assert_eq!(parse_bool("yes"), None);
        assert_eq!(parse_uint("0755", 8, 32), Some(0o755));
        assert_eq!(parse_uint("8", 8, 32), None);
        assert_eq!(parse_uint("+1", 10, 32), None);
        assert_eq!(parse_uint("4294967296", 10, 32), None);
        assert_eq!(parse_int32("-3"), Some(-3));
        assert_eq!(parse_int32("2147483648"), None);
        assert_eq!(parse_int32("1_0"), None);
    }

    #[test]
    fn durations_follow_time_parse_duration() {
        assert_eq!(parse_duration("0"), Ok(0));
        assert_eq!(parse_duration("1s"), Ok(1_000_000_000));
        assert_eq!(parse_duration("1.5h"), Ok(5_400_000_000_000));
        assert_eq!(parse_duration("2h45m"), Ok(9_900_000_000_000));
        assert_eq!(parse_duration("-300ms"), Ok(-300_000_000));
        assert_eq!(parse_duration("1\u{b5}s"), Ok(1_000));
        assert_eq!(parse_duration(".5s"), Ok(500_000_000));
        assert_eq!(parse_duration("1.s"), Ok(1_000_000_000));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("1").is_err());
        assert!(parse_duration("1x").is_err());
        assert!(parse_duration(".s").is_err());
        assert!(parse_duration("9999999999h").is_err());
        assert_eq!(parse_duration("-2562047h47m16.854775808s"), Ok(i64::MIN));
        assert!(parse_duration("2562047h47m16.854775808s").is_err());
    }

    #[test]
    fn sizes_follow_go_units() {
        assert_eq!(ram_in_bytes("32"), Ok(32));
        assert_eq!(ram_in_bytes("32b"), Ok(32));
        assert_eq!(ram_in_bytes("32k"), Ok(32 * 1024));
        assert_eq!(ram_in_bytes("32KiB"), Ok(32 * 1024));
        assert_eq!(ram_in_bytes("1.5m"), Ok(1_572_864));
        assert_eq!(ram_in_bytes("1 g"), Ok(1 << 30));
        assert!(ram_in_bytes("32kbb").is_err());
        assert!(ram_in_bytes("32x").is_err());
        assert!(ram_in_bytes("k").is_err());
        assert!(ram_in_bytes("-1").is_err());
    }

    #[test]
    fn csv_follows_go_csvvalue() {
        assert_eq!(csv_fields("a,b"), Ok(vec!["a".to_owned(), "b".to_owned()]));
        assert_eq!(
            csv_fields("type=cache,\"target=/a,b\""),
            Ok(vec!["type=cache".to_owned(), "target=/a,b".to_owned()])
        );
        assert_eq!(csv_fields("\"a\"\"b\""), Ok(vec!["a\"b".to_owned()]));
        assert_eq!(csv_fields("a,"), Ok(vec!["a".to_owned(), String::new()]));
        assert!(csv_fields("").is_err());
        assert!(csv_fields("a\"b").is_err());
        assert!(csv_fields("\"a\"b").is_err());
        assert!(csv_fields("\"ab").is_err());
    }
}
