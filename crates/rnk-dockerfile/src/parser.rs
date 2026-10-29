// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/parser/{parser,line_parsers,split_command,directives}.go
// at tag v0.34.0.
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: Rust port; the node tree is flattened into a list
// of instructions with their arguments, and every error carries its lines.

//! The line parser: Dockerfile text to a raw syntax tree, BuildKit's
//! `parser.Parse`.
//!
//! It handles parser directives, the escape character, line continuations,
//! comments, empty continuation lines, the JSON (exec) form, flags and
//! heredocs, and splits each instruction's arguments the way its BuildKit
//! line parser does. It does not check instruction names or arguments; the
//! typed layer ([`crate::parse`]) does.

mod json;

use crate::error::{Error, ErrorKind, Lines};
use crate::goutil::{is_space, is_space_latin1, quote, to_lower, trim_space};
use crate::shell::{Lexer, NoEnv};

/// The default escape character.
pub const DEFAULT_ESCAPE: char = '\\';

/// Longest line BuildKit reads (Go's `bufio.MaxScanTokenSize - 1`).
const MAX_LINE: usize = 64 * 1024 - 1;

/// A parser directive at the top of the file (`# syntax=…`, `# escape=…`,
/// `# check=…`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    /// Lower-case name: `syntax`, `escape` or `check`.
    pub name: String,
    /// The value, trimmed.
    pub value: String,
    /// Line of the directive.
    pub line: usize,
}

/// A heredoc attached to an instruction (`<<EOF`, `<<-EOF`, `<<"EOF"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heredoc {
    /// The delimiter word with quotes removed; `COPY` uses it as the file
    /// name.
    pub name: String,
    /// File descriptor before `<<` (`3<<EOF`), 0 when absent.
    pub file_descriptor: u64,
    /// `false` when any part of the delimiter was quoted: the content is
    /// then not expanded.
    pub expand: bool,
    /// `<<-`: leading tabs are stripped from the content and the delimiter.
    pub chomp: bool,
    /// The lines between the instruction and the delimiter, each with its
    /// line ending, as written (tabs not yet stripped).
    pub content: String,
}

/// A warning the parser reports without failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Line the warning refers to.
    pub line: usize,
    /// The warning text.
    pub message: String,
}

/// One instruction of the raw syntax tree: BuildKit's `parser.Node`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The instruction keyword as written (`RUN`, `run`…).
    pub keyword: String,
    /// Flags before the arguments, as written but unquoted
    /// (`--from=build`).
    pub flags: Vec<String>,
    /// Arguments, split as the instruction's line parser splits them. For
    /// `ENV` and `LABEL` they come in triples `key, value, separator`, where
    /// the separator is `=` or empty for the legacy `ENV key value` form.
    /// Empty for `ONBUILD`, whose trigger is in [`Node::child`].
    pub args: Vec<String>,
    /// The trigger of an `ONBUILD`.
    pub child: Option<Box<Node>>,
    /// The arguments were a JSON array (exec form).
    pub json: bool,
    /// Heredocs, in the order of their delimiters on the line.
    pub heredocs: Vec<Heredoc>,
    /// The instruction line after continuation lines were joined and
    /// comments removed.
    pub original: String,
    /// Lines the instruction spans, heredoc bodies included.
    pub lines: Lines,
    /// Comment lines directly above the instruction, without `#`.
    pub comments: Vec<String>,
}

impl Node {
    /// The arguments with an `ONBUILD` trigger flattened into its keyword
    /// and arguments: BuildKit's `nodeArgs`.
    #[must_use]
    pub fn flat_args(&self) -> Vec<String> {
        let mut out = self.args.clone();
        if let Some(child) = &self.child {
            out.push(child.keyword.clone());
            out.extend(child.flat_args());
        }
        out
    }

    fn can_contain_heredoc(&self) -> bool {
        let node = match &self.child {
            Some(child) if to_lower(&self.keyword) == "onbuild" => child,
            _ => self,
        };
        matches!(to_lower(&node.keyword).as_str(), "add" | "copy" | "run") && !node.json
    }

    /// BuildKit's `Node.Dump()`: an s-expression used by BuildKit's parser
    /// tests.
    #[must_use]
    pub fn dump(&self) -> String {
        let mut s = to_lower(&self.keyword);
        if !self.flags.is_empty() {
            let quoted: Vec<String> = self.flags.iter().map(|f| quote(f)).collect();
            s.push_str(&format!(" [{}]", quoted.join(" ")));
        }
        for a in &self.args {
            s.push(' ');
            s.push_str(&quote(a));
        }
        if let Some(child) = &self.child {
            s.push_str(&format!(" ({})", child.dump()));
        }
        trim_space(&s).to_owned()
    }
}

/// The raw syntax tree of a Dockerfile: BuildKit's `parser.Result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ast {
    /// The escape character, `\` or `` ` ``.
    pub escape: char,
    /// Parser directives, in file order.
    pub directives: Vec<Directive>,
    /// Instructions, in file order.
    pub nodes: Vec<Node>,
    /// Empty continuation lines (BuildKit warns about them).
    pub warnings: Vec<Warning>,
}

impl Ast {
    /// The value of the `syntax` directive, if any.
    #[must_use]
    pub fn syntax(&self) -> Option<&str> {
        self.directive("syntax")
    }

    /// The value of the `check` directive, if any.
    #[must_use]
    pub fn check(&self) -> Option<&str> {
        self.directive("check")
    }

    fn directive(&self, name: &str) -> Option<&str> {
        self.directives
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.value.as_str())
    }

    /// BuildKit's `Node.Dump()` of the root node: one line per instruction.
    #[must_use]
    pub fn dump(&self) -> String {
        let mut s = String::new();
        for n in &self.nodes {
            s.push('(');
            s.push_str(&n.dump());
            s.push_str(")\n");
        }
        trim_space(&s).to_owned()
    }
}

/// Parses Dockerfile text into its raw syntax tree, as BuildKit's
/// `parser.Parse` does.
///
/// # Errors
///
/// An [`ErrorKind::Syntax`] error, with the lines it refers to, for an
/// invalid or repeated parser directive, a line longer than 65535 bytes, an
/// unterminated heredoc, a JSON array with a non-string element, a legacy
/// `ENV`/`LABEL` without a value, or a file without instructions.
pub fn parse_ast(src: &str) -> Result<Ast, Error> {
    Parser::new(src).run()
}

/// Reads lines like Go's `bufio.Scanner` with BuildKit's `scanLines`: each
/// line keeps its `\n`; a line of 64 KiB or more stops the scan with an
/// error.
struct LineScanner<'a> {
    src: &'a str,
    pos: usize,
    too_long: bool,
}

impl<'a> LineScanner<'a> {
    fn next(&mut self) -> Option<&'a str> {
        if self.too_long || self.pos >= self.src.len() {
            return None;
        }
        let rest = &self.src[self.pos..];
        let (end, content) = match rest.find('\n') {
            Some(i) => (i + 1, i),
            None => (rest.len(), rest.len()),
        };
        if content > MAX_LINE {
            self.too_long = true;
            return None;
        }
        self.pos += end;
        Some(&rest[..end])
    }
}

/// The directive parser and the escape character: BuildKit's `directives`.
struct Directives {
    escape: char,
    line: usize,
    done: bool,
    seen: Vec<Directive>,
}

impl Directives {
    /// BuildKit's `DirectiveParser.ParseLine`: directives are recognised
    /// only in the leading block of comment lines.
    fn parse_line(&mut self, line: &str) -> Result<Option<Directive>, String> {
        self.line += 1;
        if self.done {
            return Ok(None);
        }
        let Some(rest) = line.strip_prefix('#') else {
            self.done = true;
            return Ok(None);
        };
        let rest = rest.trim_start_matches(is_space);
        let Some((key, value)) = match_directive(rest) else {
            self.done = true;
            return Ok(None);
        };
        let key = key.to_ascii_lowercase();
        if !matches!(key.as_str(), "syntax" | "escape" | "check") {
            self.done = true;
            return Ok(None);
        }
        if self.seen.iter().any(|d| d.name == key) {
            return Err(format!("only one {key} parser directive can be used"));
        }
        let d = Directive {
            name: key,
            value: value.to_owned(),
            line: self.line,
        };
        self.seen.push(d.clone());
        Ok(Some(d))
    }

    /// BuildKit's `possibleParserDirective`.
    fn possible_parser_directive(&mut self, line: &str) -> Result<bool, String> {
        let Some(d) = self.parse_line(line)? else {
            return Ok(false);
        };
        if d.name == "escape" {
            match d.value.as_str() {
                "`" => self.escape = '`',
                "\\" => self.escape = '\\',
                v => {
                    return Err(format!("invalid escape token '{v}' does not match ` or \\"));
                }
            }
        }
        Ok(true)
    }
}

/// Go's `\s` in RE2: ASCII space, `\t`, `\n`, `\f`, `\r` (not `\v`).
fn is_re_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{c}' | '\r')
}

/// The directive regular expression
/// `^([a-zA-Z][a-zA-Z0-9]*)\s*=\s*(.+?)\s*$`, with RE2's leftmost-first
/// backtracking: if only white space follows `=`, the value is its last
/// character.
fn match_directive(s: &str) -> Option<(&str, &str)> {
    let mut chars = s.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let key_end = s
        .char_indices()
        .skip(1)
        .find(|&(_, c)| !c.is_ascii_alphanumeric())
        .map_or(s.len(), |(i, _)| i);
    let key = &s[..key_end];
    let rest = s[key_end..].trim_start_matches(is_re_space);
    let rest = rest.strip_prefix('=')?;
    if rest.contains('\n') {
        return None;
    }
    let after_ws = rest.trim_start_matches(is_re_space);
    if after_ws.is_empty() {
        // Only white space after `=`: `.+?` takes the last character.
        let last = rest.chars().last()?;
        return Some((key, &rest[rest.len() - last.len_utf8()..]));
    }
    Some((key, after_ws.trim_end_matches(is_re_space)))
}

fn trim_newline(s: &str) -> &str {
    s.trim_end_matches(['\r', '\n'])
}

fn trim_leading_whitespace(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

fn is_comment(line: &str) -> bool {
    trim_leading_whitespace(line).starts_with('#')
}

fn is_empty_continuation_line(line: &str) -> bool {
    trim_leading_whitespace(trim_newline(line)).is_empty()
}

/// BuildKit's line-continuation regular expression
/// ``([^E])E[ \t]*$|^E[ \t]*$`` for the escape character `E`: a trailing
/// escape continues the line unless it is itself escaped.
fn trim_continuation(line: &str, escape: char) -> (&str, bool) {
    let t = line.trim_end_matches([' ', '\t']);
    match t.strip_suffix(escape) {
        Some("") => ("", false),
        Some(before) if !before.ends_with(escape) => (before, false),
        _ => (line, true),
    }
}

/// Go's `regexp.MustCompile("[\t\v\f\r ]+")`.
fn is_ascii_blank(c: char) -> bool {
    matches!(c, '\t' | '\u{b}' | '\u{c}' | '\r' | ' ')
}

/// `reWhitespace.Split(s, 2)` on a string without leading blanks.
fn split_once_blank(s: &str) -> (&str, Option<&str>) {
    match s.find(is_ascii_blank) {
        Some(i) => {
            let rest = s[i..].trim_start_matches(is_ascii_blank);
            (&s[..i], Some(rest))
        }
        None => (s, None),
    }
}

/// `reWhitespace.Split(s, -1)` on a string without leading or trailing
/// blanks.
fn split_blanks(s: &str) -> Vec<String> {
    s.split(is_ascii_blank)
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The text of `b` from byte `pos`, which may fall inside a UTF-8 sequence
/// when BuildKit walked the bytes; the broken sequence becomes U+FFFD.
fn lossy_from(b: &[u8], pos: usize) -> String {
    String::from_utf8_lossy(&b[pos..]).into_owned()
}

/// BuildKit's `extractBuilderFlags`: leading `--flag` words, with quotes and
/// escapes removed. BuildKit walks the line byte by byte and reads each byte
/// as a Latin-1 character; that is kept, so non-ASCII flag values are
/// re-encoded the same way.
fn extract_builder_flags(line: &str, escape: char) -> (String, Vec<String>) {
    #[derive(PartialEq, Eq)]
    enum Phase {
        Spaces,
        Word,
        Quote,
    }
    let b = line.as_bytes();
    let mut words = Vec::new();
    let mut phase = Phase::Spaces;
    let mut word = String::new();
    let mut quote = 0u8;
    let mut blank_ok = false;
    let is_escape = |c: u8| char::from(c) == escape;
    let mut pos = 0;
    while pos <= b.len() {
        let at_end = pos == b.len();
        let mut ch = if at_end { 0 } else { b[pos] };
        if phase == Phase::Spaces {
            if at_end {
                break;
            }
            if is_space_latin1(ch) {
                pos += 1;
                continue;
            }
            // Only words starting with "--" are flags.
            if ch != b'-' || pos + 1 == b.len() || b[pos + 1] != b'-' {
                return (lossy_from(b, pos), words);
            }
            phase = Phase::Word;
        }
        if at_end {
            if word != "--" && (blank_ok || !word.is_empty()) {
                words.push(word);
            }
            break;
        }
        if phase == Phase::Word {
            if is_space_latin1(ch) {
                phase = Phase::Spaces;
                if word == "--" {
                    return (lossy_from(b, pos), words);
                }
                if blank_ok || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                word.clear();
                blank_ok = false;
                pos += 1;
                continue;
            }
            if ch == b'\'' || ch == b'"' {
                quote = ch;
                blank_ok = true;
                phase = Phase::Quote;
                pos += 1;
                continue;
            }
            if is_escape(ch) {
                if pos + 1 == b.len() {
                    pos += 1;
                    continue;
                }
                pos += 1;
                ch = b[pos];
            }
            word.push(char::from(ch));
            pos += 1;
            continue;
        }
        // Phase::Quote
        if ch == quote {
            phase = Phase::Word;
            pos += 1;
            continue;
        }
        if is_escape(ch) {
            if pos + 1 == b.len() {
                phase = Phase::Word;
                pos += 1;
                continue;
            }
            pos += 1;
            ch = b[pos];
        }
        word.push(char::from(ch));
        pos += 1;
    }
    (String::new(), words)
}

/// BuildKit's `splitCommand`: the keyword, the flags and the rest.
fn split_command(line: &str, escape: char) -> (String, Vec<String>, String) {
    let (cmd, rest) = split_once_blank(trim_space(line));
    let (args, flags) = match rest {
        Some(rest) => extract_builder_flags(rest, escape),
        None => (String::new(), Vec::new()),
    };
    (cmd.to_owned(), flags, trim_space(&args).to_owned())
}

/// BuildKit's `parseWords`: words separated by white space, keeping quotes
/// and escapes.
fn parse_words(rest: &str, escape: char) -> Vec<String> {
    #[derive(PartialEq, Eq)]
    enum Phase {
        Spaces,
        Word,
        Quote,
    }
    let chars: Vec<char> = rest.chars().collect();
    let mut words = Vec::new();
    let mut phase = Phase::Spaces;
    let mut quote = '\0';
    let mut blank_ok = false;
    let mut word = String::new();
    let mut pos = 0;
    while pos <= chars.len() {
        let at_end = pos == chars.len();
        if phase == Phase::Spaces {
            if at_end {
                break;
            }
            if is_space(chars[pos]) {
                pos += 1;
                continue;
            }
            phase = Phase::Word;
        }
        if at_end {
            if blank_ok || !word.is_empty() {
                words.push(word);
            }
            break;
        }
        let mut ch = chars[pos];
        if phase == Phase::Word {
            if is_space(ch) {
                phase = Phase::Spaces;
                if blank_ok || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                blank_ok = false;
                pos += 1;
                continue;
            }
            if ch == '\'' || ch == '"' {
                quote = ch;
                blank_ok = true;
                phase = Phase::Quote;
            }
            if ch == escape {
                if pos + 1 == chars.len() {
                    // An escape at the end of the line is dropped.
                    pos += 1;
                    continue;
                }
                // Outside quotes an escape keeps itself and the next
                // character, even a quote.
                word.push(ch);
                pos += 1;
                ch = chars[pos];
            }
            word.push(ch);
            pos += 1;
            continue;
        }
        // Phase::Quote
        if ch == quote {
            phase = Phase::Word;
        }
        // The escape is special except inside single quotes.
        if ch == escape && quote != '\'' {
            if pos + 1 == chars.len() {
                phase = Phase::Word;
                pos += 1;
                continue;
            }
            pos += 1;
            word.push(ch);
            ch = chars[pos];
        }
        word.push(ch);
        pos += 1;
    }
    words
}

/// The arguments a line parser produces.
struct Parsed {
    args: Vec<String>,
    child: Option<Box<Node>>,
    json: bool,
}

impl Parsed {
    fn args(args: Vec<String>) -> Self {
        Self {
            args,
            child: None,
            json: false,
        }
    }

    fn none() -> Self {
        Self::args(Vec::new())
    }
}

const NOT_STRING_ARRAY: &str =
    "when using JSON array syntax, arrays must be comprised of strings only";

/// BuildKit's `parseJSON`: `Ok(None)` when the text is not a JSON array.
fn parse_json(rest: &str) -> Result<Option<Parsed>, String> {
    let rest = rest.trim_start_matches(is_space);
    if !rest.starts_with('[') {
        return Ok(None);
    }
    match json::parse_string_array(rest) {
        Ok(args) => Ok(Some(Parsed {
            args,
            child: None,
            json: true,
        })),
        Err(json::JsonError::NotStrings) => Err(NOT_STRING_ARRAY.to_owned()),
        Err(json::JsonError::Invalid) => Ok(None),
    }
}

/// BuildKit's `parseMaybeJSON`: a JSON array, or the whole text as one
/// argument.
fn parse_maybe_json(rest: &str) -> Result<Parsed, String> {
    if rest.is_empty() {
        return Ok(Parsed::none());
    }
    Ok(parse_json(rest)?.unwrap_or_else(|| Parsed::args(vec![rest.to_owned()])))
}

/// BuildKit's `parseMaybeJSONToList`: a JSON array, or blank-separated
/// words.
fn parse_maybe_json_to_list(rest: &str) -> Result<Parsed, String> {
    Ok(parse_json(rest)?.unwrap_or_else(|| Parsed::args(split_blanks(rest))))
}

/// BuildKit's `parseNameVal` for `ENV` and `LABEL`.
fn parse_name_val(rest: &str, key: &str, escape: char) -> Result<Parsed, String> {
    let words = parse_words(rest, escape);
    let Some(first) = words.first() else {
        return Ok(Parsed::none());
    };
    // Legacy form `KEY name value`: the first word has no `=`.
    if !first.contains('=') {
        let (name, value) = split_once_blank(rest);
        let Some(value) = value else {
            return Err(format!("{key} must have two arguments"));
        };
        return Ok(Parsed::args(vec![
            name.to_owned(),
            value.to_owned(),
            String::new(),
        ]));
    }
    let mut args = Vec::with_capacity(words.len() * 3);
    for word in &words {
        let Some((k, v)) = word.split_once('=') else {
            return Err(format!(
                "Syntax error - can't find = in {}. Must be of the form: name=value",
                quote(word)
            ));
        };
        args.extend([k.to_owned(), v.to_owned(), "=".to_owned()]);
    }
    Ok(Parsed::args(args))
}

/// BuildKit's `parseHealthConfig`: the type word, then `parseMaybeJSON`.
/// The type ends at the first byte that is white space when read as
/// Latin-1, as in BuildKit.
fn parse_health_config(rest: &str) -> Result<Parsed, String> {
    let b = rest.as_bytes();
    let sep = b
        .iter()
        .position(|&c| is_space_latin1(c))
        .unwrap_or(b.len());
    let next = b[sep..]
        .iter()
        .position(|&c| !is_space_latin1(c))
        .map_or(b.len(), |i| sep + i);
    if sep == 0 {
        return Ok(Parsed::none());
    }
    let typ = String::from_utf8_lossy(&b[..sep]).into_owned();
    let cmd = parse_maybe_json(&lossy_from(b, next))?;
    let mut args = vec![typ];
    args.extend(cmd.args);
    Ok(Parsed {
        args,
        child: None,
        json: cmd.json,
    })
}

/// How deep `ONBUILD ONBUILD …` may nest in the raw tree. BuildKit has no
/// limit here (its typed layer rejects any nesting); the limit keeps the
/// recursion bounded on hostile input.
const MAX_ONBUILD_DEPTH: usize = 8;

/// Splits one logical line into a node: BuildKit's `newNodeFromLine`.
fn node_from_line(
    line: &str,
    escape: char,
    comments: Vec<String>,
    depth: usize,
) -> Result<Node, String> {
    let (cmd, flags, rest) = split_command(line, escape);
    let parsed = match to_lower(&cmd).as_str() {
        "add" | "copy" | "volume" => parse_maybe_json_to_list(&rest)?,
        "arg" => Parsed::args(parse_words(&rest, escape)),
        "cmd" | "entrypoint" | "run" | "shell" => parse_maybe_json(&rest)?,
        "env" => parse_name_val(&rest, "ENV", escape)?,
        "label" => parse_name_val(&rest, "LABEL", escape)?,
        "expose" | "from" => {
            if rest.is_empty() {
                Parsed::none()
            } else {
                Parsed::args(split_blanks(&rest))
            }
        }
        "healthcheck" => parse_health_config(&rest)?,
        "maintainer" | "stopsignal" | "user" | "workdir" => {
            if rest.is_empty() {
                Parsed::none()
            } else {
                Parsed::args(vec![rest])
            }
        }
        "onbuild" => {
            if rest.is_empty() {
                Parsed::none()
            } else {
                if depth >= MAX_ONBUILD_DEPTH {
                    return Err("ONBUILD triggers nested too deeply".to_owned());
                }
                let child = node_from_line(&rest, escape, Vec::new(), depth + 1)?;
                Parsed {
                    args: Vec::new(),
                    child: Some(Box::new(child)),
                    json: false,
                }
            }
        }
        // Unknown instructions keep one empty argument, as BuildKit's
        // `parseIgnore` does; the typed layer rejects them.
        _ => Parsed::args(vec![String::new()]),
    };
    Ok(Node {
        keyword: cmd,
        flags,
        args: parsed.args,
        child: parsed.child,
        json: parsed.json,
        heredocs: Vec::new(),
        original: line.to_owned(),
        lines: Lines::single(0),
        comments,
    })
}

/// BuildKit's `ParseHeredoc`: a word of the form `[n]<<[-]WORD`. Errors
/// only when the delimiter cannot be lexed.
pub(crate) fn parse_heredoc(src: &str) -> Result<Option<Heredoc>, String> {
    let digits = src.bytes().take_while(u8::is_ascii_digit).count();
    let Some(after) = src[digits..].strip_prefix("<<") else {
        return Ok(None);
    };
    let (chomp, after) = match after.strip_prefix('-') {
        Some(a) => (true, a),
        None => (false, after),
    };
    let rest = after.trim_start_matches(is_re_space);
    if rest.contains('<') || rest.is_empty() {
        return Ok(None);
    }
    // ParseUint(.., 10, 0) with the error ignored: 0 when absent or too big.
    let fd = src[..digits].parse::<u64>().unwrap_or(0);

    // Lex the word with and without quotes: if they differ, part of the
    // word was quoted and the content must not be expanded.
    let lex = Lexer::new('\\').skip_unset_env(true);
    let words = lex.process_words(rest, &NoEnv).map_err(|e| e.to_string())?;
    if words.len() != 1 {
        return Ok(None);
    }
    let raw = lex
        .raw_quotes(true)
        .process_words(rest, &NoEnv)
        .map_err(|e| e.to_string())?;
    if raw.len() != words.len() {
        return Err(format!(
            "internal lexing of heredoc produced inconsistent results: {rest}"
        ));
    }
    let quotes = |w: &str| w.chars().filter(|&c| c == '\'' || c == '"').count();
    let name = words.into_iter().next().unwrap_or_default();
    Ok(Some(Heredoc {
        expand: quotes(&name) == quotes(&raw[0]),
        name,
        file_descriptor: fd,
        chomp,
        content: String::new(),
    }))
}

/// BuildKit's `heredocsFromLine`: the heredoc words of a whole line, found
/// with the shell lexer so that `<<` inside quotes does not count.
fn heredocs_from_line(line: &str) -> Result<Vec<Heredoc>, String> {
    let lex = Lexer::new('\\')
        .raw_quotes(true)
        .raw_escapes(true)
        .skip_unset_env(true);
    // BuildKit ignores lexing errors here: no heredocs.
    let words = lex.process_words(line, &NoEnv).unwrap_or_default();
    let mut docs = Vec::new();
    for word in &words {
        if let Some(h) = parse_heredoc(word)? {
            docs.push(h);
        }
    }
    Ok(docs)
}

/// BuildKit's `MustParseHeredoc`: the heredoc a single word opens
/// (`<<EOF`, `<<-"EOF"`, `3<<EOF`), without content. A `RUN` whose whole
/// command is one such word runs the heredoc body as its script.
#[must_use]
pub fn parse_heredoc_marker(word: &str) -> Option<Heredoc> {
    // A delimiter that cannot be lexed means "not a heredoc" here.
    parse_heredoc(word).ok().flatten()
}

/// BuildKit's `ChompHeredocContent`: removes leading tabs from every line.
#[must_use]
pub fn chomp_heredoc_content(src: &str) -> String {
    src.split_inclusive('\n')
        .map(|l| l.trim_start_matches('\t'))
        .collect()
}

struct Parser<'a> {
    scanner: LineScanner<'a>,
    d: Directives,
    current_line: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            scanner: LineScanner {
                src,
                pos: 0,
                too_long: false,
            },
            d: Directives {
                escape: DEFAULT_ESCAPE,
                line: 0,
                done: false,
                seen: Vec::new(),
            },
            current_line: 0,
        }
    }

    /// BuildKit's `processLine`.
    fn process_line<'l>(
        &mut self,
        token: &'l str,
        strip_left: bool,
    ) -> Result<(&'l str, bool), Error> {
        let mut token = trim_newline(token);
        if strip_left {
            token = trim_leading_whitespace(token);
        }
        let ok = self
            .d
            .possible_parser_directive(token)
            .map_err(|m| Error::at(ErrorKind::Syntax, Lines::single(self.current_line + 1), m))?;
        let token = if is_comment(token) { "" } else { token };
        Ok((token, ok))
    }

    fn run(mut self) -> Result<Ast, Error> {
        let mut nodes: Vec<Node> = Vec::new();
        let mut warnings = Vec::new();
        let mut comments: Vec<String> = Vec::new();

        while let Some(raw) = self.scanner.next() {
            let mut bytes_read = raw;
            if self.current_line == 0 {
                bytes_read = bytes_read.strip_prefix('\u{FEFF}').unwrap_or(bytes_read);
            }
            if is_comment(bytes_read) {
                // BuildKit drops the first byte, which is `#` unless the
                // comment is indented.
                let comment = String::from_utf8_lossy(&bytes_read.as_bytes()[1..]);
                let comment = trim_space(&comment);
                if comment.is_empty() {
                    comments.clear();
                } else {
                    comments.push(comment.to_owned());
                }
            }
            let (line, directive) = self.process_line(bytes_read, true)?;
            if directive {
                // A directive is not a comment of the next instruction.
                comments.pop();
            }
            self.current_line += 1;
            let start_line = self.current_line;

            let (line, mut is_end) = trim_continuation(line, self.d.escape);
            if is_end && line.is_empty() {
                continue;
            }
            let mut buf = line.to_owned();

            let mut has_empty_continuation = false;
            while !is_end {
                let Some(raw) = self.scanner.next() else {
                    break;
                };
                let (l, _) = self.process_line(raw, false)?;
                self.current_line += 1;
                if is_comment(raw) {
                    // Comment lines inside a continued instruction vanish.
                    continue;
                }
                if is_empty_continuation_line(l) {
                    has_empty_continuation = true;
                    continue;
                }
                let (l, end) = trim_continuation(l, self.d.escape);
                is_end = end;
                buf.push_str(l);
            }

            if has_empty_continuation {
                warnings.push(Warning {
                    line: self.current_line,
                    message: format!("Empty continuation line found in: {buf}"),
                });
            }

            let lines = |end| Lines::new(start_line, end);
            let mut node = node_from_line(&buf, self.d.escape, std::mem::take(&mut comments), 0)
                .map_err(|m| Error::at(ErrorKind::Syntax, lines(self.current_line), m))?;

            if node.can_contain_heredoc() && buf.contains("<<") {
                let heredocs = heredocs_from_line(&buf)
                    .map_err(|m| Error::at(ErrorKind::Syntax, lines(self.current_line), m))?;
                for mut heredoc in heredocs {
                    let mut terminated = false;
                    let mut content = String::new();
                    while let Some(raw) = self.scanner.next() {
                        self.current_line += 1;
                        let mut candidate = trim_newline(raw);
                        if heredoc.chomp {
                            candidate = candidate.trim_start_matches('\t');
                        }
                        if candidate == heredoc.name {
                            terminated = true;
                            break;
                        }
                        content.push_str(raw);
                    }
                    if !terminated {
                        return Err(Error::at(
                            ErrorKind::Syntax,
                            lines(self.current_line),
                            "unterminated heredoc",
                        ));
                    }
                    heredoc.content = content;
                    node.heredocs.push(heredoc);
                }
            }

            node.lines = lines(self.current_line);
            let mut child = node.child.as_deref_mut();
            while let Some(c) = child {
                c.lines = node.lines;
                child = c.child.as_deref_mut();
            }
            nodes.push(node);
        }

        if nodes.is_empty() {
            return Err(Error::at(
                ErrorKind::Syntax,
                Lines::single(self.current_line.max(1)),
                "file with no instructions",
            ));
        }
        if self.scanner.too_long {
            return Err(Error::at(
                ErrorKind::Syntax,
                Lines::single(self.current_line + 1),
                format!("dockerfile line greater than max allowed size of {MAX_LINE}"),
            ));
        }
        Ok(Ast {
            escape: self.d.escape,
            directives: self.d.seen,
            nodes,
            warnings,
        })
    }
}

#[cfg(test)]
mod tests;
