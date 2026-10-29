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
