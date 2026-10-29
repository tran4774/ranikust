// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/instructions/commands.go (the Expand and ExpandRaw
// methods) and dockerfile2llb/convert.go and convert_expose.go (dispatch)
// at tag v0.34.0.
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: one method on the instruction enum.

//! Variable expansion of instruction fields, as BuildKit's dispatcher does
//! it just before it runs an instruction.

use std::cell::Cell;

use super::{Instruction, Located};
use crate::error::{Error, ErrorKind};
use crate::shell::{Env, Lexer};

impl Located<Instruction> {
    /// Expands variables in the fields BuildKit expands, with `lex` (the
    /// Dockerfile's escape character) and the stage's environment at this
    /// instruction (`env`):
    ///
    /// - `ENV`, `LABEL`: keys and values;
    /// - `COPY`: `--chown`, `--chmod`, sources and destination; `ADD` also
    ///   `--checksum`; heredoc contents whose delimiter was not quoted, with
    ///   quotes left alone and `\` as the escape;
    /// - `WORKDIR`, `USER`, `STOPSIGNAL`, `VOLUME`;
    /// - `EXPOSE`: each port, split into words;
    /// - `RUN`: every `--mount` value, after which the mounts are parsed
    ///   completely.
    ///
    /// `ARG` is left alone: its value depends on `--build-arg` and is
    /// expanded by the stage scope. `FROM` is expanded by the stage graph.
    /// The command of `RUN`, `CMD` and `ENTRYPOINT` is never expanded: the
    /// shell does that.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Expansion`] if a word cannot be expanded,
    /// [`ErrorKind::Instruction`] if a `--mount` is invalid once expanded.
    pub fn expand(&mut self, lex: &Lexer, env: &dyn Env) -> Result<(), Error> {
        let raw = Lexer::new('\\').skip_process_quotes(true);
        let lex_failed = Cell::new(false);
        let mut word = |w: &str| {
            lex.process_word(w, env).map_err(|e| {
                lex_failed.set(true);
                e.to_string()
            })
        };
        let mut raw_word = |w: &str| {
            raw.process_word(w, env).map_err(|e| {
                lex_failed.set(true);
                e.to_string()
            })
        };
        let mut words = |w: &str| {
            lex.process_words(w, env).map_err(|e| {
                lex_failed.set(true);
                e.to_string()
            })
        };
        let r = expand_instruction(&mut self.value, &mut word, &mut raw_word, &mut words);
        r.map_err(|m| {
            let kind = if lex_failed.get() {
                ErrorKind::Expansion
            } else {
                ErrorKind::Instruction
            };
            Error::at(kind, self.lines, m)
        })
    }
}

type Word<'a> = &'a mut dyn FnMut(&str) -> Result<String, String>;

fn expand_all(values: &mut [String], word: Word<'_>) -> Result<(), String> {
    for v in values {
        *v = word(v)?;
    }
    Ok(())
}

fn expand_instruction(
    i: &mut Instruction,
    word: Word<'_>,
    raw_word: Word<'_>,
    words: &mut dyn FnMut(&str) -> Result<Vec<String>, String>,
) -> Result<(), String> {
    match i {
        Instruction::Env(kvs) | Instruction::Label(kvs) => {
            for kv in kvs {
                kv.key = word(&kv.key)?;
                kv.value = word(&kv.value)?;
            }
        }
        Instruction::Add(a) => {
            a.chown = word(&a.chown)?;
            a.chmod = word(&a.chmod)?;
            a.checksum = word(&a.checksum)?;
            expand_all(&mut a.sources, word)?;
            a.dest = word(&a.dest)?;
            for c in a.contents.iter_mut().filter(|c| c.expand) {
                c.data = raw_word(&c.data)?;
            }
        }
        Instruction::Copy(c) => {
            c.chown = word(&c.chown)?;
            c.chmod = word(&c.chmod)?;
            expand_all(&mut c.sources, word)?;
            c.dest = word(&c.dest)?;
            for content in c.contents.iter_mut().filter(|c| c.expand) {
                content.data = raw_word(&content.data)?;
            }
        }
        Instruction::Workdir(v) | Instruction::User(v) | Instruction::StopSignal(v) => {
            *v = word(v)?;
        }
        Instruction::Volume(v) => expand_all(v, word)?,
        Instruction::Expose(ports) => {
            let mut out = Vec::with_capacity(ports.len());
            for p in ports.iter() {
                out.extend(words(p)?);
            }
            *ports = out;
        }
        Instruction::Run(r) => r.parse_mounts(word)?,
        Instruction::Arg(_)
        | Instruction::Entrypoint(_)
        | Instruction::Cmd(_)
        | Instruction::Shell(_)
        | Instruction::Healthcheck(_)
        | Instruction::Onbuild(_)
        | Instruction::Maintainer(_) => {}
    }
    Ok(())
}
