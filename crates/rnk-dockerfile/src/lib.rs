//! Dockerfile front end: from Dockerfile text to typed instructions and a
//! stage graph.
//!
//! Lexer, parser directives, heredocs, variable expansion, the typed
//! instruction set, the dependency graph between stages, and kaniko-style
//! stage squashing (used only when stages share the `/` rootfs in sequential
//! kaniko mode). Behavior follows BuildKit's parser, instructions and shell
//! lexer.
//!
//! # Boundaries
//!
//! - Data in, data out: no filesystem, network or process access.
//! - Does not schedule or build stages (`rnk-sched`, `rnk-builder`).

mod error;
mod goutil;
mod instructions;
mod parser;
mod shell;

pub use error::{Error, ErrorKind, Lines};
pub use instructions::{
    Add, ArgDecl, CmdLine, Copy, Device, Dockerfile, Healthcheck, InlineFile, Instruction,
    KeyValue, Located, Mount, MountType, Network, Run, Security, Sharing, SourceContent, Stage,
    from_ast, parse, parse_instruction,
};
pub use parser::{
    Ast, DEFAULT_ESCAPE, Directive, Heredoc, Node, Warning, chomp_heredoc_content, parse_ast,
};
pub use shell::{Env, LexError, Lexer, NoEnv, WordMatches};
