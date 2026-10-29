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
//! First thing, it installs the logs and the opt-in metrics of `rnk-metrics`
//! (`RANIKUST_METRICS_FILE`), which start no thread, and writes the metrics
//! report on every way out.
//!
//! # Boundaries
//!
//! - No build logic: everything past flag parsing and process setup lives in
//!   the library crates.
//! - Probes no capability itself: each library probes its own, and this
//!   binary runs the probes in order and combines their results.

use std::process::ExitCode;

use rnk_metrics::{Config, Outcome};

/// Hidden test hook: `panic` panics and aborts, `error` fails with exit code
/// 1, after metrics are set up. It lets the tests of the report's exit paths
/// run the real binary, including a release build, where `panic = "abort"`.
const DEBUG_FAULT_ENV: &str = "RANIKUST_DEBUG_FAULT";

fn main() -> ExitCode {
    let telemetry = rnk_metrics::init(Config::from_env());
    telemetry.set_meta("ranikust_version", env!("CARGO_PKG_VERSION"));
    let result = tracing::debug_span!("ranikust.run").in_scope(run);
    let (code, outcome) = match &result {
        Ok(()) => (0, Outcome::success()),
        Err(message) => {
            eprintln!("ranikust: error: {message}");
            (1, Outcome::error(1, message))
        }
    };
    telemetry.finish(&outcome);
    ExitCode::from(code)
}

fn run() -> Result<(), String> {
    match std::env::var(DEBUG_FAULT_ENV).as_deref() {
        Ok("panic") => deliberate_panic(),
        Ok("error") => return Err(format!("deliberate error ({DEBUG_FAULT_ENV}=error)")),
        _ => {}
    }
    println!("ranikust {}", env!("CARGO_PKG_VERSION"));
    eprintln!("ranikust: not implemented yet");
    Ok(())
}

/// Panics, then aborts. Release builds abort inside the panic, right after
/// the panic hook; unwinding builds (tests) abort here, so a test sees what a
/// release build leaves behind: only what the hook wrote.
fn deliberate_panic() -> ! {
    fn panic_now() {
        panic!("deliberate panic ({DEBUG_FAULT_ENV}=panic)");
    }
    let unwound = std::panic::catch_unwind(panic_now);
    debug_assert!(unwound.is_err());
    std::process::abort()
}
