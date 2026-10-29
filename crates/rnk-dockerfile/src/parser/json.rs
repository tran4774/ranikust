//! The JSON reading BuildKit's `parseJSON` needs: `json.Unmarshal` of the
//! argument text into `[]any`, then a check that every element is a string.
//!
//! Go's decoder is strict (RFC 8259): the whole text must be one JSON value,
//! surrounded only by white space, nested at most 10000 levels deep. Lone
//! surrogate escapes decode to U+FFFD.

/// Why the text is not a JSON array of strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonError {
    /// Not valid JSON, or not an array: the instruction falls back to the
    /// shell form.
    Invalid,
    /// A valid JSON array with an element that is not a string: an error.
    NotStrings,
}

const MAX_DEPTH: usize = 10_000;

/// Parses `text` as a JSON array of strings.
pub(crate) fn parse_string_array(text: &str) -> Result<Vec<String>, JsonError> {
    let mut p = Reader {
        b: text.as_bytes(),
        s: text,
        pos: 0,
    };
    p.skip_ws();
    if p.peek() != Some(b'[') {
        return Err(JsonError::Invalid);
    }
    // Read the top-level array by hand so that its string elements are kept;
    // nested values are only validated.
    p.pos += 1;
    let mut out = Vec::new();
    let mut all_strings = true;
    p.skip_ws();
    if p.peek() == Some(b']') {
        p.pos += 1;
    } else {
        loop {
            p.skip_ws();
            if p.peek() == Some(b'"') {
                out.push(p.string()?);
            } else {
                all_strings = false;
                p.value(1)?;
            }
            p.skip_ws();
            match p.next() {
                Some(b',') => {}
                Some(b']') => break,
                _ => return Err(JsonError::Invalid),
            }
        }
    }
    p.skip_ws();
    if p.pos != p.b.len() {
        return Err(JsonError::Invalid);
    }
    if all_strings {
        Ok(out)
    } else {
        Err(JsonError::NotStrings)
    }
}

struct Reader<'a> {
    b: &'a [u8],
    s: &'a str,
    pos: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// Validates one value. Containers are tracked on an explicit stack, so
    /// deep nesting cannot overflow the call stack.
    fn value(&mut self, depth: usize) -> Result<(), JsonError> {
        // Each entry is the closing byte of an open container.
        let mut stack: Vec<u8> = Vec::new();
        loop {
            self.skip_ws();
            // Read one scalar or open a container.
            match self.peek() {
                Some(b'[') => {
                    self.pos += 1;
                    if depth + stack.len() + 1 > MAX_DEPTH {
                        return Err(JsonError::Invalid);
                    }
                    self.skip_ws();
                    if self.peek() == Some(b']') {
                        self.pos += 1;
                    } else {
                        stack.push(b']');
                        continue;
                    }
                }
                Some(b'{') => {
                    self.pos += 1;
                    if depth + stack.len() + 1 > MAX_DEPTH {
                        return Err(JsonError::Invalid);
                    }
                    self.skip_ws();
                    if self.peek() == Some(b'}') {
                        self.pos += 1;
                    } else {
                        self.key()?;
                        stack.push(b'}');
                        continue;
                    }
                }
                Some(b'"') => {
                    self.string()?;
                }
                Some(b't') => self.literal(b"true")?,
                Some(b'f') => self.literal(b"false")?,
                Some(b'n') => self.literal(b"null")?,
                Some(b'-' | b'0'..=b'9') => self.number()?,
                _ => return Err(JsonError::Invalid),
            }
            // A value is complete: close containers or move to the next
            // element.
            loop {
                let Some(&close) = stack.last() else {
                    return Ok(());
                };
                self.skip_ws();
                match self.next() {
                    Some(b',') => {
                        if close == b'}' {
                            self.skip_ws();
                            self.key()?;
                        }
                        break;
                    }
                    Some(c) if c == close => {
                        stack.pop();
                    }
                    _ => return Err(JsonError::Invalid),
                }
            }
        }
    }

    /// An object key and its colon.
    fn key(&mut self) -> Result<(), JsonError> {
        if self.peek() != Some(b'"') {
            return Err(JsonError::Invalid);
        }
        self.string()?;
        self.skip_ws();
        if self.next() != Some(b':') {
            return Err(JsonError::Invalid);
        }
        Ok(())
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), JsonError> {
        if self.b[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(JsonError::Invalid)
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<(), JsonError> {
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(JsonError::Invalid),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(JsonError::Invalid);
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(JsonError::Invalid);
            }
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let h = self
            .s
            .get(self.pos..self.pos + 4)
            .ok_or(JsonError::Invalid)?;
        if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(JsonError::Invalid);
        }
        self.pos += 4;
        u32::from_str_radix(h, 16).map_err(|_| JsonError::Invalid)
    }

    /// A string starting at the opening quote.
    fn string(&mut self) -> Result<String, JsonError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // The loop stops only at ASCII bytes, so this is a char boundary.
            out.push_str(&self.s[start..self.pos]);
            match self.next() {
                Some(b'"') => return Ok(out),
                Some(b'\\') => match self.next() {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'/') => out.push('/'),
                    Some(b'b') => out.push('\u{8}'),
                    Some(b'f') => out.push('\u{c}'),
                    Some(b'n') => out.push('\n'),
                    Some(b'r') => out.push('\r'),
                    Some(b't') => out.push('\t'),
                    Some(b'u') => {
                        let u = self.hex4()?;
                        let c = if (0xD800..0xDC00).contains(&u) {
                            // A high surrogate needs a following low one.
                            let save = self.pos;
                            if self.s[self.pos..].starts_with("\\u") {
                                self.pos += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    char::from_u32(0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00))
                                } else {
                                    self.pos = save;
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            char::from_u32(u)
                        };
                        out.push(c.unwrap_or('\u{FFFD}'));
                    }
                    _ => return Err(JsonError::Invalid),
                },
                _ => return Err(JsonError::Invalid),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Result<Vec<String>, JsonError> {
        Ok(v.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn arrays_of_strings() {
        assert_eq!(
            parse_string_array(r#"["a", "b c"]"#),
            strings(&["a", "b c"])
        );
        assert_eq!(parse_string_array(" [ ] \n"), strings(&[]));
        assert_eq!(
            parse_string_array(r#"["\u00e9\n\"\\\/", "\ud83d\ude00", "\ud800"]"#),
            strings(&["é\n\"\\/", "😀", "\u{FFFD}"])
        );
    }

    #[test]
    fn invalid_json_falls_back() {
        for s in [
            "[",
            "[\"a\",]",
            "['a']",
            "[\"a\"] x",
            "{}",
            "[\"a\tb\"]",
            "[01]",
            "[1.]",
            "[tru]",
            "[\"\\x\"]",
            "[\"\\u12\"]",
        ] {
            assert_eq!(parse_string_array(s), Err(JsonError::Invalid), "{s:?}");
        }
    }

    #[test]
    fn non_string_elements_are_an_error() {
        for s in [
            "[1]",
            "[\"a\", null]",
            "[[\"a\"]]",
            "[{\"a\": [1, {}]}]",
            "[-0.5e+3, true]",
        ] {
            assert_eq!(parse_string_array(s), Err(JsonError::NotStrings), "{s:?}");
        }
    }

    #[test]
    fn depth_is_limited() {
        let deep = format!("[{}{}]", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert_eq!(parse_string_array(&deep), Err(JsonError::Invalid));
        let ok = format!(
            "[{}{}]",
            "[".repeat(MAX_DEPTH - 1),
            "]".repeat(MAX_DEPTH - 1)
        );
        assert_eq!(parse_string_array(&ok), Err(JsonError::NotStrings));
    }
}
