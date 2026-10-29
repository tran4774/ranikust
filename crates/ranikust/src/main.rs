//! The `ranikust` command-line tool.
//!
//! Parses the `build`, `push`, `prune`, `version` and `login` subcommands,
//! accepting the common kaniko flags, plus `--mode=auto|overlay|kaniko` to
//! choose the snapshot mode. While the process is still single-threaded it
//! checks the workdir, enters a private mount namespace when permitted, runs
//! the privilege probes of `rnk-sandbox` and `rnk-snapshot` in order and picks
//! the mode, and in kaniko mode locks the workdir; then it hands off to the
//! library crates. `prune` only calls `rnk-store`.
//!
//! # Boundaries
//!
//! - No build logic: everything past flag parsing and process setup lives in
//!   the library crates.
//! - Probes no capability itself: each library probes its own, and this
//!   binary runs the probes in order and combines their results.

fn main() {
    println!("ranikust {}", env!("CARGO_PKG_VERSION"));
    eprintln!("ranikust: not implemented yet");
}
