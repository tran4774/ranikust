// Derived from moby/patternmatcher, https://github.com/moby/patternmatcher,
// ignorefile/ignorefile.go and ignorefile/ignorefile_test.go at tag v0.6.1
// (commit 5a6d8429a19b).
// Copyright 2012-2017 Docker, Inc. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: reads a byte slice; Go's bufio.Scanner line
// splitting written out.

//! [`read_patterns`], a port of `ignorefile.ReadAll`.

use crate::Error;
use crate::golang;

/// The longest line `bufio.Scanner` accepts with its default 64 KiB
/// buffer, which must also hold the line's `\n`.
const MAX_LINE: usize = 64 * 1024 - 1;

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// Reads the patterns of a `.dockerignore` file, as BuildKit does with
/// `ignorefile.ReadAll`:
///
/// - lines end at `\n`, and a `\r` before it is dropped;
/// - a UTF-8 byte order mark is stripped from the first line;
/// - a line starting with `#` is a comment (an indented `#` is not);
/// - each pattern is trimmed, and empty ones are dropped;
/// - a leading `!` (exception) is kept, the rest is trimmed again and
///   cleaned like `filepath.Clean`, and one leading `/` is removed, so
///   `/foo` and `foo` are the same pattern.
///
/// The result goes to [`Matcher::new`](crate::Matcher::new).
///
/// # Errors
///
/// [`Error::LineTooLong`] when a line is longer than 65,535 bytes,
/// `\r` included.
pub fn read_patterns(contents: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut patterns = Vec::new();
    let mut rest = contents;
    let mut line_number = 0;
    while !rest.is_empty() {
        let (line, next) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &[][..]),
        };
        if line.len() > MAX_LINE {
            return Err(Error::LineTooLong {
                line: line_number + 1,
            });
        }
        rest = next;
        let mut line = line.strip_suffix(b"\r").unwrap_or(line);
        if line_number == 0 {
            line = line.strip_prefix(UTF8_BOM).unwrap_or(line);
        }
        line_number += 1;
        // Comments are skipped before trimming.
        if line.starts_with(b"#") {
            continue;
        }
        let mut pattern = golang::trim_space(line);
        if pattern.is_empty() {
            continue;
        }
        let invert = pattern[0] == b'!';
        if invert {
            pattern = golang::trim_space(&pattern[1..]);
        }
        let mut out = Vec::with_capacity(pattern.len() + 1);
        if invert {
            out.push(b'!');
        }
        if !pattern.is_empty() {
            let cleaned = golang::clean(pattern);
            // Make absolute patterns relative to the context.
            match cleaned.strip_prefix(b"/") {
                Some(relative) if !relative.is_empty() => out.extend_from_slice(relative),
                _ => out.extend_from_slice(&cleaned),
            }
        }
        patterns.push(out);
    }
    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(contents: &str) -> Vec<String> {
        read_patterns(contents.as_bytes())
            .unwrap()
            .into_iter()
            .map(|p| String::from_utf8(p).unwrap())
            .collect()
    }

    /// Upstream `TestReadAll`.
    #[test]
    fn read_all_upstream() {
        assert!(read("").is_empty());
        let content = "test1\n/test2\n/a/file/here\n\nlastfile\n# this is a comment\n! /inverted/abs/path\n!\n! ";
        assert_eq!(
            read(content),
            [
                "test1",
                "test2",
                "a/file/here",
                "lastfile",
                "!inverted/abs/path",
                "!",
                "!"
            ]
        );
    }

    #[test]
    fn comments_bom_and_line_endings() {
        assert_eq!(
            read("\u{feff}#c\r\n a \r\n  #x\n\u{feff}b"),
            ["a", "#x", "\u{feff}b"]
        );
        assert_eq!(read("/\n//a\n./b/../c/\n!/"), ["/", "a", "c", "!/"]);
        // The BOM is stripped from the first line even when it is a comment.
        assert_eq!(read("\u{feff}#c\n#d"), Vec::<String>::new());
        // Only one `\r` is dropped, and only before `\n` or at the end.
        assert_eq!(read("a\r\r\nb\rc"), ["a", "b\rc"]);
    }

    #[test]
    fn invalid_utf8_is_kept() {
        assert_eq!(read_patterns(b"\xff/./x\n").unwrap(), [b"\xff/x".to_vec()]);
    }

    #[test]
    fn line_length_limit() {
        let long = "a".repeat(MAX_LINE);
        assert_eq!(read(&long).len(), 1);
        assert_eq!(read(&format!("{long}\n")).len(), 1);
        assert_eq!(
            read_patterns(format!("{long}a").as_bytes()),
            Err(Error::LineTooLong { line: 1 })
        );
        assert_eq!(
            read_patterns(format!("{long}\r\n").as_bytes()),
            Err(Error::LineTooLong { line: 1 })
        );
        assert_eq!(
            read_patterns(format!("x\n{long}a\ny").as_bytes()),
            Err(Error::LineTooLong { line: 2 })
        );
    }
}
