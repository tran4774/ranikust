// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/shell/lex.go at tag v0.34.0.
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: Rust port; patterns use the Pike VM in
// `shell/pattern.rs` instead of Go regular expressions.

//! Shell-word processing: quotes, escapes, `$VAR`/`${VAR...}` expansion and
//! word splitting, a port of BuildKit's `shell.Lex`.

mod pattern;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::hash::BuildHasher;

use crate::goutil::{is_digit, is_letter, is_space};

/// Variables visible to the lexer, BuildKit's `EnvGetter`.
pub trait Env {
    /// The value of `name`, or `None` if it is not set. A variable set to the
    /// empty string returns `Some("")`.
    fn get(&self, name: &str) -> Option<&str>;
}

impl<E: Env + ?Sized> Env for &E {
    fn get(&self, name: &str) -> Option<&str> {
        (**self).get(name)
    }
}

impl Env for BTreeMap<String, String> {
    fn get(&self, name: &str) -> Option<&str> {
        BTreeMap::get(self, name).map(String::as_str)
    }
}

impl<S: BuildHasher> Env for HashMap<String, String, S> {
    fn get(&self, name: &str) -> Option<&str> {
        HashMap::get(self, name).map(String::as_str)
    }
}

/// An environment with no variables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoEnv;

impl Env for NoEnv {
    fn get(&self, _name: &str) -> Option<&str> {
        None
    }
}

/// An error from the shell lexer: an unterminated quote, a bad substitution,
/// a failed `${VAR?message}`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("failed to process {word:?}: {message}")]
pub struct LexError {
    word: String,
    message: String,
}

impl LexError {
    /// The message, without the word it refers to.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// The result of [`Lexer::process_word_with_matches`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WordMatches {
    /// The expanded word.
    pub result: String,
    /// Variables the word referenced that were set.
    pub matched: BTreeSet<String>,
    /// Variables the word referenced that were not set.
    pub unmatched: BTreeSet<String>,
}

/// Shell-word lexer: BuildKit's `shell.Lex`.
///
/// It strips quotes and escapes and expands `$VAR`, `${VAR}` and the
/// modifiers `${VAR:-x}`, `${VAR-x}`, `${VAR:+x}`, `${VAR+x}`,
/// `${VAR:?msg}`, `${VAR?msg}`, `${VAR#p}`, `${VAR##p}`, `${VAR%p}`,
/// `${VAR%%p}`, `${VAR/p/r}` and `${VAR//p/r}`. It does not run commands:
/// backquotes and `$(...)` are not special.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lexer {
    escape: char,
    raw_quotes: bool,
    raw_escapes: bool,
    skip_process_quotes: bool,
    skip_unset_env: bool,
}

impl Lexer {
    /// A lexer that uses `escape` (`\` or `` ` ``) as the escape character.
    #[must_use]
    pub fn new(escape: char) -> Self {
        Self {
            escape,
            raw_quotes: false,
            raw_escapes: false,
            skip_process_quotes: false,
            skip_unset_env: false,
        }
    }

    /// The escape character.
    #[must_use]
    pub fn escape(&self) -> char {
        self.escape
    }

    /// Keeps quote characters in the output (`RawQuotes`).
    #[must_use]
    pub fn raw_quotes(mut self, on: bool) -> Self {
        self.raw_quotes = on;
        self
    }

    /// Keeps escape characters in the output (`RawEscapes`).
    #[must_use]
    pub fn raw_escapes(mut self, on: bool) -> Self {
        self.raw_escapes = on;
        self
    }

    /// Treats quotes as ordinary characters (`SkipProcessQuotes`), as
    /// BuildKit does when it expands heredoc contents.
    #[must_use]
    pub fn skip_process_quotes(mut self, on: bool) -> Self {
        self.skip_process_quotes = on;
        self
    }

    /// Leaves references to unset variables as they are (`SkipUnsetEnv`).
    #[must_use]
    pub fn skip_unset_env(mut self, on: bool) -> Self {
        self.skip_unset_env = on;
        self
    }

    /// Expands `word` into a single string, keeping the spaces between
    /// words (`ProcessWord`).
    ///
    /// # Errors
    ///
    /// A [`LexError`] on an unterminated quote, a bad substitution or a
    /// failed `${VAR?message}`.
    pub fn process_word(&self, word: &str, env: &dyn Env) -> Result<String, LexError> {
        self.run(word, env, true).map(|(r, _)| r.result)
    }

    /// Expands `word` and splits the result into words on unquoted white
    /// space (`ProcessWords`). Splitting happens after expansion, so the
    /// value of an unquoted variable is split too.
    ///
    /// # Errors
    ///
    /// As [`Lexer::process_word`].
    pub fn process_words(&self, word: &str, env: &dyn Env) -> Result<Vec<String>, LexError> {
        self.run(word, env, false).map(|(_, words)| words)
    }

    /// As [`Lexer::process_word`], and also reports which variables the
    /// word referenced (`ProcessWordWithMatches`).
    ///
    /// # Errors
    ///
    /// As [`Lexer::process_word`].
    pub fn process_word_with_matches(
        &self,
        word: &str,
        env: &dyn Env,
    ) -> Result<WordMatches, LexError> {
        self.run(word, env, true).map(|(r, _)| r)
    }

    fn run(
        &self,
        word: &str,
        env: &dyn Env,
        capture: bool,
    ) -> Result<(WordMatches, Vec<String>), LexError> {
        let mut chars: Vec<char> = word.chars().collect();
        // Go's text/scanner skips a byte order mark at the very start.
        if chars.first() == Some(&'\u{FEFF}') {
            chars.remove(0);
        }
        let mut sw = ShellWord {
            depth: 0,
            lex: *self,
            chars,
            pos: 0,
            env,
            raw_escapes: self.raw_escapes,
            capture,
            matched: BTreeSet::new(),
            unmatched: BTreeSet::new(),
            words_buffer: String::new(),
        };
        match sw.process_stop_on(None, self.raw_escapes, false) {
            Ok((result, words)) => Ok((
                WordMatches {
                    result,
                    matched: sw.matched,
                    unmatched: sw.unmatched,
                },
                words,
            )),
            Err(message) => Err(LexError {
                word: word.to_owned(),
                message,
            }),
        }
    }
}

impl Default for Lexer {
    fn default() -> Self {
        Self::new('\\')
    }
}

impl fmt::Display for Lexer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lexer(escape {:?})", self.escape)
    }
}

/// Go's `string(rune)` of a scanner result, where EOF (-1) prints as U+FFFD.
fn rune_str(c: Option<char>) -> String {
    c.unwrap_or('\u{FFFD}').to_string()
}

fn is_special_param(c: char) -> bool {
    matches!(c, '@' | '*' | '#' | '?' | '-' | '$' | '!' | '0')
}

fn is_heredoc_space(c: char) -> bool {
    matches!(c, '\t' | '\r' | ' ')
}

/// Words being collected by `process_stop_on`. The buffer of the current
/// word is shared by every nested call, as in BuildKit, where a nested
/// `${...}` resets it.
struct Words {
    words: Vec<String>,
    in_word: bool,
}

impl Words {
    fn add_char(&mut self, buf: &mut String, ch: char) {
        if is_space(ch) && self.in_word {
            if !buf.is_empty() {
                self.words.push(std::mem::take(buf));
                self.in_word = false;
            }
        } else if !is_space(ch) {
            self.add_raw_char(buf, ch);
        }
    }

    fn add_raw_char(&mut self, buf: &mut String, ch: char) {
        buf.push(ch);
        self.in_word = true;
    }

    fn add_string(&mut self, buf: &mut String, s: &str) {
        for c in s.chars() {
            self.add_char(buf, c);
        }
    }

    fn add_raw_string(&mut self, buf: &mut String, s: &str) {
        buf.push_str(s);
        self.in_word = true;
    }

    fn get_words(mut self, buf: &mut String) -> Vec<String> {
        if !buf.is_empty() {
            self.words.push(std::mem::take(buf));
        }
        self.words
    }
}

/// How deeply `${…}` substitutions may nest.
const MAX_DEPTH: usize = 200;

struct ShellWord<'a> {
    depth: usize,
    lex: Lexer,
    chars: Vec<char>,
    pos: usize,
    env: &'a dyn Env,
    raw_escapes: bool,
    capture: bool,
    matched: BTreeSet<String>,
    unmatched: BTreeSet<String>,
    words_buffer: String,
}

type Step<T> = Result<T, String>;

impl ShellWord<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Processes input until `stop` (or the end), returning the processed
    /// text and its words. `raw_escapes` keeps escapes for this call;
    /// `preserve_pattern_escapes` doubles escapes inside quotes of a pattern.
    fn process_stop_on(
        &mut self,
        stop: Option<char>,
        raw_escapes: bool,
        preserve_pattern_escapes: bool,
    ) -> Step<(String, Vec<String>)> {
        // BuildKit recurses without a limit (Go stacks grow); a bound keeps
        // hostile input from overflowing the stack.
        if self.depth >= MAX_DEPTH {
            return Err("substitutions nested too deeply".to_owned());
        }
        self.depth += 1;
        let saved = self.raw_escapes;
        self.raw_escapes = raw_escapes;
        let r = self.process_stop_on_inner(stop, preserve_pattern_escapes);
        self.raw_escapes = saved;
        self.depth -= 1;
        r
    }

    fn process_stop_on_inner(
        &mut self,
        stop: Option<char>,
        preserve: bool,
    ) -> Step<(String, Vec<String>)> {
        let mut result = String::new();
        self.words_buffer.clear();
        let mut words = Words {
            words: Vec::new(),
            in_word: false,
        };
        while let Some(ch) = self.peek() {
            if stop == Some(ch) {
                self.next();
                let w = words.get_words(&mut self.words_buffer);
                return Ok((result, w));
            }
            match ch {
                '$' => {
                    let tmp = self.process_dollar()?;
                    result.push_str(&tmp);
                    words.add_string(&mut self.words_buffer, &tmp);
                }
                '<' => {
                    let tmp = self.process_possible_heredoc();
                    result.push_str(&tmp);
                    words.add_raw_string(&mut self.words_buffer, &tmp);
                }
                '\'' if !self.lex.skip_process_quotes => {
                    let tmp = self.process_single_quote(preserve)?;
                    result.push_str(&tmp);
                    words.add_raw_string(&mut self.words_buffer, &tmp);
                }
                '"' if !self.lex.skip_process_quotes => {
                    let tmp = self.process_double_quote(preserve)?;
                    result.push_str(&tmp);
                    words.add_raw_string(&mut self.words_buffer, &tmp);
                }
                _ => {
                    self.next();
                    if ch == self.lex.escape {
                        if self.raw_escapes {
                            words.add_raw_char(&mut self.words_buffer, ch);
                            result.push(ch);
                        }
                        // The escape character escapes the next character,
                        // except at the end of the word.
                        let Some(escaped) = self.next() else {
                            break;
                        };
                        words.add_raw_char(&mut self.words_buffer, escaped);
                        result.push(escaped);
                    } else {
                        words.add_char(&mut self.words_buffer, ch);
                        result.push(ch);
                    }
                }
            }
        }
        if let Some(stop) = stop {
            return Err(format!(
                "unexpected end of statement while looking for matching {stop}"
            ));
        }
        let w = words.get_words(&mut self.words_buffer);
        Ok((result, w))
    }

    fn process_single_quote(&mut self, preserve: bool) -> Step<String> {
        // Everything between single quotes is literal; a single quote cannot
        // be escaped inside them.
        let mut result = String::new();
        let quote = self.next();
        if self.lex.raw_quotes {
            result.extend(quote);
        }
        loop {
            match self.next() {
                None => {
                    return Err(
                        "unexpected end of statement while looking for matching single-quote"
                            .to_owned(),
                    );
                }
                Some('\'') => {
                    if self.lex.raw_quotes {
                        result.push('\'');
                    }
                    return Ok(result);
                }
                Some(c) => {
                    if preserve && c == self.lex.escape {
                        result.push(c);
                    }
                    result.push(c);
                }
            }
        }
    }

    fn process_double_quote(&mut self, preserve: bool) -> Step<String> {
        // Everything up to the next `"` is literal except `$`; the escape
        // character escapes only `"`, `$` and itself.
        let mut result = String::new();
        let quote = self.next();
        if self.lex.raw_quotes {
            result.extend(quote);
        }
        loop {
            match self.peek() {
                None => {
                    return Err(
                        "unexpected end of statement while looking for matching double-quote"
                            .to_owned(),
                    );
                }
                Some('"') => {
                    self.next();
                    if self.lex.raw_quotes {
                        result.push('"');
                    }
                    return Ok(result);
                }
                Some('$') => {
                    let value = self.process_dollar()?;
                    result.push_str(&value);
                }
                Some(mut ch) => {
                    self.next();
                    if ch == self.lex.escape {
                        match self.peek() {
                            // An escape at the end of the word is dropped.
                            None => continue,
                            Some(next) if next == '"' || next == '$' || next == self.lex.escape => {
                                if self.raw_escapes {
                                    result.push(ch);
                                }
                                ch = next;
                                self.next();
                            }
                            Some(_) => {
                                // Shell patterns need the escape kept even
                                // when quotes are kept.
                                if self.raw_escapes && (!self.lex.raw_quotes || preserve) {
                                    result.push(ch);
                                }
                            }
                        }
                    }
                    result.push(ch);
                }
            }
        }
    }

    fn process_dollar(&mut self) -> Step<String> {
        self.next();

        // $xxx
        if self.peek() != Some('{') {
            let name = self.process_name();
            if name.is_empty() {
                return Ok("$".to_owned());
            }
            return Ok(match self.get_env(&name) {
                Some(v) => v,
                None if self.lex.skip_unset_env => format!("${name}"),
                None => String::new(),
            });
        }

        self.next();
        match self.peek() {
            None => return Err("syntax error: missing '}'".to_owned()),
            Some('{' | '}' | ':') => return Err("syntax error: bad substitution".to_owned()),
            Some(_) => {}
        }
        let name = self.process_name();
        let mut ch = self.next();
        let mut chs = rune_str(ch);

        match ch {
            Some('}') => Ok(match self.get_env(&name) {
                Some(v) => v,
                None if self.lex.skip_unset_env => format!("${{{name}}}"),
                None => String::new(),
            }),
            Some(':' | '+' | '-' | '?' | '#' | '%') => {
                let mut null_is_unset = false;
                if ch == Some(':') {
                    null_is_unset = true;
                    ch = self.next();
                    chs.push_str(&rune_str(ch));
                }
                let raw = matches!(ch, Some('#' | '%'));
                if null_is_unset && raw {
                    return Err(format!("unsupported modifier ({chs}) in substitution"));
                }
                let word = match self.process_stop_on(Some('}'), raw, raw) {
                    Ok((w, _)) => w,
                    Err(e) => {
                        if self.peek().is_none() {
                            return Err("syntax error: missing '}'".to_owned());
                        }
                        return Err(e);
                    }
                };

                let value = self.get_env(&name);
                if self.lex.skip_unset_env && value.is_none() {
                    return Ok(format!("${{{name}{chs}{word}}}"));
                }
                let set = value.is_some();
                let value = value.unwrap_or_default();
                match ch {
                    Some('-') => Ok(if !set || (null_is_unset && value.is_empty()) {
                        word
                    } else {
                        value
                    }),
                    Some('+') => Ok(if !set || (null_is_unset && value.is_empty()) {
                        String::new()
                    } else {
                        word
                    }),
                    Some('?') => {
                        if !set {
                            let message = if word.is_empty() {
                                "is not allowed to be unset"
                            } else {
                                &word
                            };
                            return Err(format!("{name}: {message}"));
                        }
                        if null_is_unset && value.is_empty() {
                            let message = if word.is_empty() {
                                "is not allowed to be empty"
                            } else {
                                &word
                            };
                            return Err(format!("{name}: {message}"));
                        }
                        Ok(value)
                    }
                    Some(m @ ('%' | '#')) => {
                        // `%`/`#` remove the shortest match, `%%`/`##` the
                        // longest.
                        let (greedy, word) = match word.strip_prefix(m) {
                            Some(rest) => (true, rest),
                            None => (false, word.as_str()),
                        };
                        if m == '%' {
                            pattern::trim_suffix(word, &value, greedy)
                        } else {
                            pattern::trim_prefix(word, &value, greedy)
                        }
                    }
                    _ => Err(format!("unsupported modifier ({chs}) in substitution")),
                }
            }
            Some('/') => {
                let replace_all = self.peek() == Some('/');
                if replace_all {
                    self.next();
                }
                let pat = match self.process_stop_on(Some('/'), true, true) {
                    Ok((p, _)) => p,
                    Err(e) => {
                        if self.peek().is_none() {
                            return Err("syntax error: missing '/' in ${}".to_owned());
                        }
                        return Err(e);
                    }
                };
                let replacement = match self.process_stop_on(Some('}'), true, false) {
                    Ok((r, _)) => r,
                    Err(e) => {
                        if self.peek().is_none() {
                            return Err("syntax error: missing '}'".to_owned());
                        }
                        return Err(e);
                    }
                };
                let Some(value) = self.get_env(&name).or_else(|| {
                    // An unset variable expands to the empty string.
                    (!self.lex.skip_unset_env).then(String::new)
                }) else {
                    return Ok(format!("${{{name}/{pat}/{replacement}}}"));
                };
                let re = pattern::Pattern::compile(&pat, true, false)
                    .map_err(|e| format!("invalid pattern ({pat}) in substitution: {e}"))?;
                Ok(if replace_all {
                    pattern::replace_all(&re, &value, &replacement)
                } else {
                    pattern::replace_first(&re, &value, &replacement)
                })
            }
            _ => Err(format!("unsupported modifier ({chs}) in substitution")),
        }
    }

    /// A name: letters, digits and `_`; a run of digits; or one special
    /// parameter character.
    fn process_name(&mut self) -> String {
        let mut name = String::new();
        while let Some(ch) = self.peek() {
            if name.is_empty() && is_digit(ch) {
                while let Some(d) = self.peek().filter(|&c| is_digit(c)) {
                    self.next();
                    name.push(d);
                }
                return name;
            }
            if name.is_empty() && is_special_param(ch) {
                self.next();
                return ch.to_string();
            }
            if !is_letter(ch) && !is_digit(ch) && ch != '_' {
                break;
            }
            self.next();
            name.push(ch);
        }
        name
    }

    /// `<<` and the blanks after it form one token, so `<< EOF` stays one
    /// word for heredoc detection.
    fn process_possible_heredoc(&mut self) -> String {
        self.next();
        if self.peek() != Some('<') {
            return "<".to_owned();
        }
        self.next();
        let mut result = String::from("<<");
        while let Some(c) = self.peek().filter(|&c| is_heredoc_space(c)) {
            result.push(c);
            self.next();
        }
        result
    }

    fn get_env(&mut self, name: &str) -> Option<String> {
        let v = self.env.get(name).map(str::to_owned);
        if self.capture {
            if v.is_some() {
                self.matched.insert(name.to_owned());
            } else {
                self.unmatched.insert(name.to_owned());
            }
        }
        v
    }
}

#[cfg(test)]
mod tests;
