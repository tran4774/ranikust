//! The `ranikust` command-line tool.
//!
//! Parses the `build` and `version` subcommands; `build` is the default and
//! accepts kaniko's common executor flags, plus `--mode=auto|overlay|kaniko`
//! to choose the snapshot mode (ADR-0011, ADR-0023). `push`, `prune` and
//! `login` come later (Q-048).
//!
//! Set-up order, each step before the next: parse the flags (no I/O;
//! refused kaniko flags are reported first) → the Linux 5.8 check of
//! ADR-0028 (`rnk_rootfs::probe_support`), for every subcommand but
//! `version` → logs and the opt-in metrics of `rnk-metrics`, which start no
//! thread, with `--metrics-file` over `RANIKUST_METRICS_FILE` → flag
//! validation into a `BuildConfig` → the process set-up of `setup`: a
//! private mount namespace, the probes of `rnk-sandbox` and `rnk-snapshot`
//! (the latter a seam until task 9), the outcome, the spawner thread →
//! hand-off. Still to come in later tasks: the workdir checks (Q-011), the
//! store (task 7), kaniko mode and its workdir lock (Phase 2), and the
//! hand-off to the library crates. Until the builder exists, `build` stops
//! after the set-up with exit code 2.
//!
//! The metrics report is written on every way out.
//!
//! # Boundaries
//!
//! - No build logic: everything past flag parsing and process setup lives in
//!   the library crates.
//! - Probes no capability itself: each library probes its own, and this
//!   binary runs the probes in order and combines their results.

mod build;
mod cli;
mod config;
mod setup;
mod startup;

use std::io::Write as _;
use std::process::ExitCode;

use rnk_metrics::{Config, Outcome, Telemetry};

use crate::cli::{BuildArgs, Command, ParseError};
use crate::config::BuildConfig;

/// The allocator of every Rust allocation: the release binary is static musl,
/// whose allocator is 250–300 times slower than mimalloc at 16 threads
/// (`exp-05` Q2). C libraries (libzstd) still call libc's `malloc`.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Hidden test hook. `panic` panics and aborts and `error` fails with exit
/// code 1, after metrics are set up; `kernel-missing` and `kernel-blocked`
/// make the kernel check fail. It lets the tests run the real binary through
/// these paths, including a release build, where `panic = "abort"`.
const DEBUG_FAULT_ENV: &str = "RANIKUST_DEBUG_FAULT";

/// Exit code of a usage error, as clap uses.
const EXIT_USAGE: u8 = 2;

/// How a run ends when it does not succeed.
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_USAGE,
            message: message.into(),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

fn main() -> ExitCode {
    let (mut command, mut warnings) = match cli::parse(std::env::args_os()) {
        Ok(parsed) => (parsed.cli.into_command(), parsed.warnings),
        Err(ParseError::Clap(err)) => {
            // Prints usage errors to stderr and `--help`/`--version` to
            // stdout; a closed stream is no reason to panic.
            let _ = err.print();
            return ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(EXIT_USAGE));
        }
        Err(ParseError::Refused(lines)) => {
            for line in &lines {
                report(line);
            }
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // kaniko's environment variables (ADR-0036), before anything reads the
    // flags they stand for.
    if let Command::Build(args) = &mut command {
        match config::apply_env(args, std::env::vars_os()) {
            Ok(more) => warnings.extend(more),
            Err(invalid) => {
                report(&invalid.0);
                return ExitCode::from(EXIT_USAGE);
            }
        }
    }
    let fault = std::env::var(DEBUG_FAULT_ENV).ok();

    // ADR-0028: before any other work. `version` needs no filesystem access
    // and stays usable on any kernel, to tell what is installed.
    if !matches!(command, Command::Version)
        && let Err(err) = startup::check_kernel(fault.as_deref())
    {
        report(&err.to_string());
        return ExitCode::from(1);
    }

    let telemetry = rnk_metrics::init(metrics_config(&command));
    telemetry.set_meta("ranikust_version", env!("CARGO_PKG_VERSION"));
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    let result = tracing::debug_span!("ranikust.run")
        .in_scope(|| run(&command, &telemetry, fault.as_deref()));
    let (code, outcome) = match &result {
        Ok(()) => (0, Outcome::success()),
        Err(failure) => {
            report(&failure.message);
            (
                failure.code,
                Outcome::error(i32::from(failure.code), &failure.message),
            )
        }
    };
    telemetry.finish(&outcome);
    ExitCode::from(code)
}

/// Writes `ranikust: error: …` to stderr. `eprintln!` would panic, and
/// abort, on a closed stderr.
fn report(message: &str) {
    let _ = writeln!(std::io::stderr(), "ranikust: error: {message}");
}

/// The metrics settings: the environment, with `--metrics-file` taking the
/// report path's place.
fn metrics_config(command: &Command) -> Config {
    let mut config = Config::from_env();
    if let Command::Build(args) = command {
        config.logs = config::log_options(args);
        if let Some(file) = &args.metrics_file {
            config.file = Some(std::path::absolute(file).unwrap_or_else(|_| file.clone()));
        }
    }
    config
}

fn run(command: &Command, telemetry: &Telemetry, fault: Option<&str>) -> Result<(), Failure> {
    match fault {
        Some("panic") => deliberate_panic(),
        Some("error") => {
            return Err(Failure::error(format!(
                "deliberate error ({DEBUG_FAULT_ENV}=error)"
            )));
        }
        _ => {}
    }
    match command {
        Command::Version => {
            let _ = writeln!(std::io::stdout(), "ranikust {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Build(args) => {
            tracing::debug_span!("ranikust.build").in_scope(|| build(args, telemetry))
        }
    }
}

fn build(args: &BuildArgs, telemetry: &Telemetry) -> Result<(), Failure> {
    let config = BuildConfig::from_args(args).map_err(|invalid| Failure::usage(invalid.0))?;
    telemetry.set_meta("mode", config::name(config.mode));
    telemetry.set_meta("options", config.meta_options());
    tracing::debug!(
        mode = %config::name(config.mode),
        state_dir = ?config.state_dir_for(config.mode),
        "build settings validated"
    );
    if config.mode == config::Mode::Kaniko {
        return Err(Failure::usage(
            "--mode=kaniko is not implemented yet (Phase 2); use --mode=overlay as root",
        ));
    }
    // Overlay mode is the only outcome of Phase 1, so its state directory
    // is the one the probes use (Q-018, Q-110).
    let state_dir = config.state_dir_for(config::Mode::Overlay).ok_or_else(|| {
        Failure::usage(
            "overlay mode needs --workdir or --state-dir: the state directory holds the \
             stage roots",
        )
    })?;
    let setup = tracing::debug_span!("ranikust.setup")
        .in_scope(|| {
            setup::set_up(
                &state_dir,
                config.workdir.as_deref(),
                config.allow_network_none_userns,
            )
        })
        .map_err(|refusal| Failure::error(refusal.to_string()))?;
    if args.snapshot_mode.is_some() {
        tracing::warn!("--snapshot-mode is ignored in overlay mode (ADR-0023)");
    }
    tracing::debug!(state_dir = %setup.state_dir.display(), "set-up done");
    tracing::info!("set up for {}", setup.outcome);
    let setup::Outcome::OverlayRoot { overlay, .. } = setup.outcome;
    let built = build::run(&config, setup.store, setup.sandbox, overlay).map_err(Failure::error)?;
    tracing::info!("built image {}", built.digest);
    // The digest is the build's result: failing to print it fails the run.
    writeln!(std::io::stdout(), "{}", built.digest)
        .map_err(|e| Failure::error(format!("printing the image digest: {e}")))
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
