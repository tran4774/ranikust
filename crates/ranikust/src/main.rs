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
//! thread, the report asked for with `--metrics-file` only (ADR-0037) → flag
//! validation into a `BuildConfig` → the process set-up of `setup`: a
//! private mount namespace, the probes of `rnk-sandbox` and `rnk-snapshot`
//! (the latter a seam until task 9), the outcome, the spawner thread →
//! hand-off. `--mode=auto` picks kaniko mode where root may not mount
//! (ADR-0040). In kaniko mode, the set-up so far is the workdir checks and
//! lock of `workdir` (ADR-0038) for a `--workdir`, or the container check
//! of `container` for the root `/`; kaniko-mode builds stop there with exit
//! code 2 until the rest of Phase 2 lands.
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
mod container;
mod setup;
mod startup;
mod workdir;

use std::io::Write as _;
use std::path::Path;
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
    let (mut command, metrics_file, mut warnings) = match cli::parse(std::env::args_os()) {
        Ok(parsed) => {
            let file = parsed.cli.metrics_file().map(Path::to_path_buf);
            (parsed.cli.into_command(), file, parsed.warnings)
        }
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

    // ADR-0037: the report path comes only from the flag.
    if std::env::var_os("RANIKUST_METRICS_FILE").is_some_and(|v| !v.is_empty()) {
        warnings.push(
            "RANIKUST_METRICS_FILE is ignored: ask for the metrics report with --metrics-file"
                .to_owned(),
        );
    }
    let telemetry = rnk_metrics::init(metrics_config(&command, metrics_file.as_deref()));
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

/// The metrics settings: the report path from `--metrics-file` only
/// (ADR-0037), the summary from `RANIKUST_METRICS_SUMMARY`, and the log
/// settings of `build`.
fn metrics_config(command: &Command, metrics_file: Option<&Path>) -> Config {
    let mut config = Config::from_env();
    if let Command::Build(args) = command {
        config.logs = config::log_options(args);
    }
    config.file = metrics_file.map(|f| std::path::absolute(f).unwrap_or_else(|_| f.to_path_buf()));
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
    if config::context_sub_path_ignored(args) {
        tracing::warn!(
            "--context-sub-path is ignored: kaniko applies it to dir:// and fetched contexts \
             only, not to a plain path"
        );
    }
    telemetry.set_meta("mode", config::name(config.mode));
    telemetry.set_meta("options", config.meta_options());
    tracing::debug!(
        mode = %config::name(config.mode),
        state_dir = ?config.state_dir_for(config.mode),
        "build settings validated"
    );
    match config.mode {
        config::Mode::Kaniko => {
            // Whether root may mount picks how `RUN` is isolated on a host
            // workdir (ADR-0043); kaniko mode itself never mounts.
            let span = tracing::debug_span!("ranikust.setup");
            let can_mount = match span.in_scope(setup::enter_namespace) {
                Ok(()) => true,
                // Kaniko mode does not need the namespace: it only tells
                // whether `RUN` may get the full sandbox.
                Err(refusal) => {
                    tracing::debug!("RUN without the full sandbox: {}", refusal.reason);
                    false
                }
            };
            kaniko(&config, "--mode=kaniko", can_mount)
        }
        config::Mode::Overlay => {
            overlay_flags(args)?;
            let state_dir = overlay_state_dir(&config)?;
            // One span for the whole set-up: the namespace, then the rest.
            let span = tracing::debug_span!("ranikust.setup");
            span.in_scope(setup::enter_namespace)
                .map_err(|refusal| Failure::error(refusal.to_string()))?;
            overlay(&config, &state_dir, &span)
        }
        // Whether root may mount decides, before anything is written: the
        // namespace first, then overlay mode's state directory (ADR-0040).
        config::Mode::Auto => {
            let span = tracing::debug_span!("ranikust.setup");
            match span.in_scope(setup::enter_namespace) {
                Ok(()) => {
                    overlay_flags(args)?;
                    let state_dir = overlay_state_dir(&config)?;
                    overlay(&config, &state_dir, &span)
                }
                Err(refusal) if refusal.needs_kaniko_mode() => {
                    let why = refusal.reason;
                    tracing::info!("kaniko mode, because root may not mount here: {why}");
                    kaniko(&config, "--mode=auto", false)
                }
                Err(refusal) => Err(Failure::error(refusal.to_string())),
            }
        }
    }
}

/// The kaniko-mode flags in overlay mode: refused where they would change
/// the image, else a warning each (ADR-0040).
fn overlay_flags(args: &BuildArgs) -> Result<(), Failure> {
    let warnings = config::overlay_mode_flags(args).map_err(|invalid| Failure::usage(invalid.0))?;
    for warning in warnings {
        tracing::warn!("{warning}");
    }
    Ok(())
}

/// Overlay mode's state directory, which the probes use (Q-018, Q-110).
fn overlay_state_dir(config: &BuildConfig) -> Result<std::path::PathBuf, Failure> {
    config.state_dir_for(config::Mode::Overlay).ok_or_else(|| {
        Failure::usage(
            "overlay mode needs --workdir or --state-dir: the state directory holds the \
             stage roots",
        )
    })
}

/// Overlay mode, once the namespace is entered: the rest of the set-up,
/// then the build.
fn overlay(config: &BuildConfig, state_dir: &Path, span: &tracing::Span) -> Result<(), Failure> {
    let setup = span
        .in_scope(|| {
            setup::set_up(
                state_dir,
                config.workdir.as_deref(),
                config.allow_network_none_userns,
            )
        })
        .map_err(|refusal| Failure::error(refusal.to_string()))?;
    tracing::debug!(state_dir = %setup.state_dir.display(), "set-up done");
    tracing::info!("set up for {}", setup.outcome);
    let setup::Outcome::OverlayRoot { overlay, .. } = setup.outcome;
    let built = build::run(
        config,
        setup.store,
        setup.sandbox,
        build::Snapshots::Overlay(overlay),
    )
    .map_err(Failure::error)?;
    tracing::info!("built image {}", built.digest);
    // The digest is the build's result: failing to print it fails the run.
    writeln!(std::io::stdout(), "{}", built.digest)
        .map_err(|e| Failure::error(format!("printing the image digest: {e}")))
}

/// Kaniko mode, chosen by `how`: the root is a host `--workdir`, checked,
/// locked and marked (ADR-0038), or `/`, only inside a container unless
/// `--force` (ADR-0040); then `RLIMIT_NOFILE` is raised (the original kept
/// for `RUN`), the state directory and store set up, the ignore list built,
/// and the stages run one unit at a time on the shared root
/// (`design/kaniko-mode.md#stage-roots-on-the-shared-root`). `can_mount` is
/// whether the build's mount namespace could be entered.
fn kaniko(config: &BuildConfig, how: &str, can_mount: bool) -> Result<(), Failure> {
    if config.single_snapshot {
        return Err(Failure::usage(
            "--single-snapshot is not implemented in kaniko mode yet (Phase 2)",
        ));
    }
    let nofile = rnk_sandbox::NofileLimit::current();
    let mut claimed = match config.workdir.as_deref() {
        Some(dir) => Some(
            tracing::debug_span!("ranikust.workdir")
                .in_scope(|| workdir::claim(dir))
                .map_err(|refused| Failure::error(refused.to_string()))?,
        ),
        None => {
            match container::detect(&container::Real) {
                Some(runtime) => tracing::debug!(%runtime, "running in a container"),
                // kaniko's rule and warning
                // (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:240-245`).
                None if config.force => tracing::warn!(
                    "ranikust is being run outside of a container with --force: kaniko mode \
                     builds on / and empties it between stages. This can have dangerous \
                     effects on your system"
                ),
                None => return Err(Failure::error(OUTSIDE_A_CONTAINER)),
            }
            None
        }
    };
    if rustix::process::geteuid().as_raw() != 0 {
        return Err(Failure::error(format!(
            "{how}: kaniko mode needs root (chown, chroot, switching users), as kaniko does; \
             run ranikust as root in the container or VM that builds"
        )));
    }
    match workdir::raise_nofile_limit() {
        Ok(limit) => tracing::debug!(?limit, "RLIMIT_NOFILE raised to its hard limit"),
        // The walk still runs, with fewer descriptors to hold directories.
        Err(e) => tracing::warn!("cannot raise RLIMIT_NOFILE to its hard limit: {e}"),
    }
    if config.jobs.is_some() {
        tracing::warn!(
            "--jobs is ignored in kaniko mode: stages run one at a time on the shared root"
        );
    }
    // The marker before anything else is written in the workdir, the
    // state directory included when it lies inside.
    if let Some(w) = claimed.as_mut() {
        w.mark()
            .map_err(|refused| Failure::error(refused.to_string()))?;
    }
    // Always `Some` in kaniko mode.
    let state_dir = config
        .state_dir_for(config::Mode::Kaniko)
        .ok_or_else(|| Failure::error("kaniko mode has no state directory"))?;
    let root_path = claimed
        .as_ref()
        .map_or_else(|| std::path::PathBuf::from("/"), |w| w.path().to_owned());
    let setup = tracing::debug_span!("ranikust.setup")
        .in_scope(|| {
            setup::set_up_kaniko(
                &state_dir,
                claimed.as_ref().map(workdir::Workdir::path),
                can_mount,
                nofile,
            )
        })
        .map_err(|refusal| Failure::error(refusal.to_string()))?;
    let snapshotter = kaniko_snapshotter(config, claimed.as_ref(), &root_path, &state_dir)?;
    let built = build::run(
        config,
        setup.store,
        setup.sandbox,
        build::Snapshots::Kaniko {
            snapshotter,
            isolation: setup.isolation,
        },
    )
    .map_err(Failure::error)?;
    // The lock is held to the end of the build.
    drop(claimed);
    tracing::info!("built image {}", built.digest);
    writeln!(std::io::stdout(), "{}", built.digest)
        .map_err(|e| Failure::error(format!("printing the image digest: {e}")))
}

/// Kaniko mode's snapshotter on the claimed root: the root opened from the
/// workdir's checked descriptor (or `/`), the ignore list from mountinfo,
/// the flags and the state directory, and the walk's options. The paths
/// given to the ignore list are physical: the workdir's canonical path,
/// opened without following a symlink by `claim`, and the state
/// directory's, opened the same way here.
fn kaniko_snapshotter(
    config: &BuildConfig,
    claimed: Option<&workdir::Workdir>,
    root_path: &Path,
    state_dir: &Path,
) -> Result<rnk_snapshot::Kaniko, Failure> {
    let fail = |what: &str, e: &dyn std::fmt::Display| Failure::error(format!("{what}: {e}"));
    let root = match claimed {
        Some(w) => w
            .root()
            .sub_root("/")
            .map_err(|e| fail("reopening the workdir", &e))?,
        None => rnk_rootfs::RootPath::open("/").map_err(|e| fail("opening /", &e))?,
    };
    let state = std::fs::canonicalize(state_dir)
        .map_err(|e| fail(&format!("resolving {}", state_dir.display()), &e))?;
    rnk_rootfs::RootPath::open(&state)
        .map_err(|e| fail(&format!("opening {}", state.display()), &e))?;
    let mounts =
        rnk_snapshot::kaniko::read_mountinfo().map_err(|e| fail("reading mountinfo", &e))?;
    let spec = rnk_snapshot::kaniko::IgnoreSpec {
        root: root_path,
        state_dir: Some(&state),
        ignore_paths: &config.ignore_paths,
        ignore_var_run: config.ignore_var_run,
    };
    let ignore = rnk_snapshot::kaniko::ignore_list(&spec, &mounts)
        .map_err(|e| fail("building the ignore list", &e))?;
    let walk = rnk_snapshot::WalkOptions {
        workers: config.snapshot_workers,
        mode: match config.snapshot_mode {
            config::SnapshotMode::Full => rnk_snapshot::SnapshotMode::Full,
            config::SnapshotMode::Redo => rnk_snapshot::SnapshotMode::Redo,
            config::SnapshotMode::Time => rnk_snapshot::SnapshotMode::Time,
        },
    };
    Ok(rnk_snapshot::Kaniko::new(
        root,
        root_path.to_owned(),
        ignore,
        walk,
    ))
}

/// Why kaniko mode refuses the root `/` outside a container (ADR-0040).
const OUTSIDE_A_CONTAINER: &str = "kaniko mode without --workdir builds on / and empties it, \
     so it runs only inside a container; pass --workdir=DIR to build in a directory of this \
     machine, or --force if you are sure";

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
