//! The `ranikust` command-line tool.
//!
//! Parses the `build` and `version` subcommands; `build` is the default and
//! accepts kaniko's common executor flags, plus `--mode=auto|overlay|kaniko`
//! to choose the snapshot mode (ADR-0011, ADR-0023, ADR-0049). `push` and
//! `prune` come later (Q-048); there is no `login` (ADR-0049).
//!
//! Set-up order, each step before the next: parse the flags (no I/O;
//! refused kaniko flags are reported first) → the Linux 5.8 check of
//! ADR-0028 (`rnk_rootfs::probe_support`), for every subcommand but
//! `version` → for `build`, the same run again from a sealed copy of the
//! binary (ADR-0101, `rnk_sandbox::run_from_sealed_copy`), which starts
//! over from the top → logs and the opt-in metrics of `rnk-metrics`, which
//! start no thread, the report asked for with `--metrics-file` only
//! (ADR-0037) → flag validation into a `BuildConfig` → the process set-up
//! of `setup`: a private mount namespace, the probes of `rnk-sandbox` and
//! `rnk-snapshot`, the outcome, the spawner thread → hand-off.
//! `--mode=auto` picks kaniko mode where root may not mount (ADR-0040).
//! Kaniko mode: root required; the container check of `container` decides
//! where it builds: on the shared root `/` in a container
//! (`design/kaniko-mode.md#stage-roots-on-the-shared-root`), else in a
//! directory per stage made in the state directory, the stages scheduled as
//! overlay mode's (`#per-stage-directories`, ADR-0050, ADR-0059); the state
//! directory's checks of `statedir`; the store, the sandbox, the ignore
//! list; then the build.
//!
//! The metrics report is written on every way out.
//!
//! # Boundaries
//!
//! - Orchestrates a build across the library crates (`build.rs`: the stage
//!   reader counts and hand-over decisions, the release thread, the units,
//!   the push set; ADR-0067), but holds no rule of a single concern: Dockerfile
//!   semantics, scheduling, executing an instruction, snapshots, the sandbox,
//!   filesystem access, storage, layers, images and registries live in their
//!   library crates.
//! - Probes no capability itself: each library probes its own, and this
//!   binary runs the probes in order and combines their results.

mod build;
mod cli;
mod config;
mod container;
mod exposure;
mod limits;
mod setup;
mod startup;
mod statedir;

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use rnk_metrics::{Config, Outcome, Telemetry};
use rnk_sandbox::{BinaryMount, SealedCopy, Sealing};

use crate::cli::{BuildArgs, Command, ParseError};
use crate::config::BuildConfig;

/// The allocator of every Rust allocation: the release binary is static musl,
/// whose allocator is 250–300 times slower than mimalloc at 16 threads
/// (`exp-05` Q2). C libraries (libzstd) still call libc's `malloc`.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Hidden test hook. `panic` panics and aborts and `error` fails with exit
/// code 1, after metrics are set up; `kernel-missing`, `kernel-blocked` and
/// `kernel-hidden` make the kernel check fail; `seal-tmpfile` skips the
/// memfd way of sealing the binary and `seal-none` both ways (ADR-0101). It
/// lets the tests run the real binary through these paths, including a
/// release build, where `panic = "abort"`.
const DEBUG_FAULT_ENV: &str = "RANIKUST_DEBUG_FAULT";

/// Exit code of a usage error, as clap uses.
const EXIT_USAGE: u8 = 2;

/// How a run ends when it does not succeed.
#[derive(Debug)]
struct Failure {
    code: u8,
    /// What failed. It may carry names ranikust does not choose (a base
    /// image's tar entry, a context file), so [`report_to`] escapes it.
    message: String,
    /// What to do instead, ranikust's own text, printed on a line of its
    /// own after the message; empty when there is none.
    guidance: &'static str,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_USAGE,
            message: message.into(),
            guidance: "",
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
            guidance: "",
        }
    }

    /// The set-up refused: its reason, then its guidance.
    fn refused(refusal: setup::Refusal) -> Self {
        Self::error(refusal.reason).with_guidance(refusal.guidance)
    }

    fn with_guidance(self, guidance: &'static str) -> Self {
        Self { guidance, ..self }
    }

    /// The message and the guidance as one text, for the metrics report:
    /// what a refusal's `Display` gives.
    fn text(&self) -> String {
        if self.guidance.is_empty() {
            self.message.clone()
        } else {
            format!("{}\n  {}", self.message, self.guidance)
        }
    }
}

fn main() -> ExitCode {
    // ADR-0051: first of all, before any credential is read. A process that
    // is not dumpable keeps `/proc/<pid>/{fd,environ,mem,maps,root}` from
    // any process without `CAP_SYS_PTRACE`: a `RUN` sharing ranikust's PID
    // namespace (kaniko mode on `/`) reads none of them. Nothing here
    // changes ranikust's credentials later; a change would reset the flag.
    if let Err(e) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        report(&format!("prctl(PR_SET_DUMPABLE, 0): {e}"), "");
        return ExitCode::from(1);
    }
    // No core file of ranikust either, whatever `core_pattern` and
    // `suid_dumpable` say: its memory holds the credentials. Every `RUN`
    // gets the original soft limit back.
    if let Err(e) = rnk_sandbox::forbid_core_dumps() {
        report(&format!("setrlimit(RLIMIT_CORE, 0): {e}"), "");
        return ExitCode::from(1);
    }
    let (command, metrics_file) = match cli::parse(std::env::args_os()) {
        Ok(parsed) => {
            let file = parsed.cli.metrics_file().map(Path::to_path_buf);
            (parsed.cli.into_command(), file)
        }
        Err(ParseError::Clap(err)) => {
            // Prints usage errors to stderr and `--help`/`--version` to
            // stdout; a closed stream is no reason to panic.
            let _ = err.print();
            return ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(EXIT_USAGE));
        }
        Err(ParseError::Refused(lines)) => {
            for line in &lines {
                report(line, "");
            }
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let fault = std::env::var(DEBUG_FAULT_ENV).ok();

    // ADR-0028: before any other work. `version` needs no filesystem access
    // and stays usable on any kernel, to tell what is installed.
    if !matches!(command, Command::Version)
        && let Err(err) = startup::check_kernel(fault.as_deref())
    {
        report(&err.to_string(), "");
        return ExitCode::from(1);
    }
    // ADR-0101: a build runs from a copy of the binary no `RUN` can write.
    // Still single-threaded, no credential or state read: the copy starts
    // over from the top.
    let sealing = if let Command::Build(args) = &command {
        let root = rustix::process::geteuid().is_root();
        let walk = may_be_kaniko_on_slash(args.mode, root, &container::Real);
        match rnk_sandbox::run_from_sealed_copy(seal_ways(fault.as_deref()), walk) {
            Ok(sealing) => Some(sealing),
            Err(e) => {
                report(&format!("setrlimit(RLIMIT_CORE, 0): {e}"), "");
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };

    // ADR-0037: the report path comes only from the flag.
    let mut warnings = Vec::new();
    if std::env::var_os("RANIKUST_METRICS_FILE").is_some_and(|v| !v.is_empty()) {
        warnings.push(
            "RANIKUST_METRICS_FILE is ignored: ask for the metrics report with --metrics-file"
                .to_owned(),
        );
    }
    // Refused before the report is set up, so nothing is written there; the
    // report's writer keeps to the build's own context when it writes.
    // `version` only warns when its report cannot be written.
    let fence = match metrics_preflight(&command, metrics_file.as_deref()) {
        Ok(fence) => fence,
        Err(message) => {
            report(&message, "");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let telemetry = rnk_metrics::init(metrics_config(&command, metrics_file.as_deref(), fence));
    telemetry.set_meta("ranikust_version", env!("CARGO_PKG_VERSION"));
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    match &sealing {
        Some(Sealing::Sealed {
            way,
            passed_over,
            binary,
        }) if passed_over.is_empty() => {
            tracing::debug!(%way, ?binary, "running from a sealed copy of the binary (ADR-0101)");
        }
        Some(Sealing::Sealed {
            way,
            passed_over,
            binary,
        }) => tracing::debug!(
            %way,
            %passed_over,
            ?binary,
            "running from a sealed copy of the binary (ADR-0101)"
        ),
        Some(Sealing::Unsealed { why, binary }) => tracing::warn!(
            ?binary,
            "running from the binary as installed, not from a sealed copy (ADR-0101): {why}"
        ),
        None => {}
    }
    let binary = sealing.as_ref().map(Sealing::binary);
    let result = tracing::debug_span!("ranikust.run").in_scope(|| {
        run(
            &command,
            &telemetry,
            fault.as_deref(),
            metrics_file.as_deref(),
            binary,
        )
    });
    let (code, outcome) = match &result {
        Ok(()) => (0, Outcome::success()),
        Err(failure) => {
            report(&failure.message, failure.guidance);
            (
                failure.code,
                Outcome::error(i32::from(failure.code), failure.text()),
            )
        }
    };
    telemetry.finish(&outcome);
    ExitCode::from(code)
}

/// The ways to seal the binary, in order; the test hook's `seal-tmpfile`
/// skips the memfd, `seal-none` both.
fn seal_ways(fault: Option<&str>) -> &'static [SealedCopy] {
    match fault {
        Some("seal-tmpfile") => &[SealedCopy::Tmpfile],
        Some("seal-none") => &[],
        _ => &SealedCopy::ALL,
    }
}

/// Whether the build may be kaniko mode on a container's `/`, the only one
/// that reads whether a writable mount reaches the binary, so the seal walks
/// those mounts (ADR-0101): not with `--mode=overlay`, not without root
/// (kaniko mode refuses), not outside a container (`container::detect`).
fn may_be_kaniko_on_slash(mode: config::Mode, root: bool, host: &impl container::Host) -> bool {
    match mode {
        config::Mode::Overlay => false,
        config::Mode::Auto | config::Mode::Kaniko => root && container::detect(host).is_some(),
    }
}

/// Writes `ranikust: error: …` to stderr, then the guidance, if any.
/// `eprintln!` would panic, and abort, on a closed stderr.
fn report(message: &str, guidance: &str) {
    let _ = report_to(&mut std::io::stderr(), message, guidance);
}

/// The fatal error line, with `message` escaped
/// ([`rnk_metrics::terminal_text`]): nothing in it can drive the terminal
/// or start a line that looks like ranikust's own. The guidance, ranikust's
/// own text, follows indented on its own line. One write.
fn report_to(out: &mut impl std::io::Write, message: &str, guidance: &str) -> std::io::Result<()> {
    let mut text = format!("ranikust: error: {}\n", rnk_metrics::terminal_text(message));
    if !guidance.is_empty() {
        text.push_str(&format!("  {guidance}\n"));
    }
    out.write_all(text.as_bytes())
}

/// The metrics settings: the report path from `--metrics-file` only
/// (ADR-0037), the summary from `RANIKUST_METRICS_SUMMARY`, and the log
/// settings of `build`.
fn metrics_config(
    command: &Command,
    metrics_file: Option<&Path>,
    fence: Option<rnk_rootfs::Fence>,
) -> Config {
    let mut config = Config::from_env();
    if let Command::Build(args) = command {
        config.logs = config::log_options(args);
    }
    config.file = metrics_file.map(|f| std::path::absolute(f).unwrap_or_else(|_| f.to_path_buf()));
    let build = matches!(command, Command::Build(_));
    config.write_file = Some(metrics_writer(build, fence, &BUILD_FENCE));
    config
}

/// The metrics report's writer: fenced by the context the build opened
/// (`build_fence`), else by the one found at start-up; a build's report is
/// never written without a fence.
fn metrics_writer(
    build: bool,
    fence: Option<rnk_rootfs::Fence>,
    build_fence: &'static std::sync::OnceLock<rnk_rootfs::Fence>,
) -> rnk_metrics::WriteFile {
    rnk_metrics::WriteFile(std::sync::Arc::new(move |path: &Path, bytes: &[u8]| {
        let fence = build_fence.get().or(fence.as_ref());
        if build && fence.is_none() {
            return Err(std::io::Error::other(
                "the build context could not be fenced, so no report is written (ADR-0076)",
            ));
        }
        write_metrics_file(path, bytes, fence)
    }))
}

/// The build context's directory as the build itself opened it (ADR-0076):
/// set once when the build opens its context, and preferred by the metrics
/// report's writer, so the report is kept out of the very context the build
/// read, not out of a second open of `--context` that a swap of its parent
/// could have changed in between.
pub(crate) static BUILD_FENCE: std::sync::OnceLock<rnk_rootfs::Fence> = std::sync::OnceLock::new();

/// The checks of `--metrics-file` before the report is set up: for a build
/// asking for a report, the build context must open, so the report's path
/// can be kept out of it, and the path must pass as an output. The fence
/// found, for the writer until the build opens its own context.
fn metrics_preflight(
    command: &Command,
    metrics_file: Option<&Path>,
) -> Result<Option<rnk_rootfs::Fence>, String> {
    if !matches!(command, Command::Build(_)) || metrics_file.is_none() {
        // The build reports its own usage errors and context.
        return Ok(context_fence(command).ok().flatten());
    }
    let fence = context_fence(command)?;
    if fence.is_none() {
        return Err(
            "--metrics-file: the build context cannot be opened, so the report cannot be kept \
             out of it (ADR-0076)"
                .to_owned(),
        );
    }
    check_metrics_file(metrics_file, fence.as_ref())?;
    Ok(fence)
}

/// The build context's directory as a fence, opened for the metrics
/// report's check (ADR-0076): `Ok(None)` for `version`, or a context that
/// cannot be opened; the usage error when the build's flags are refused.
fn context_fence(command: &Command) -> Result<Option<rnk_rootfs::Fence>, String> {
    let Command::Build(args) = command else {
        return Ok(None);
    };
    let config = BuildConfig::from_args(args).map_err(|invalid| invalid.0)?;
    Ok(rnk_rootfs::RootPath::open(&config.context)
        .ok()
        .and_then(|root| rnk_rootfs::Fence::of(&root).ok()))
}

/// `--metrics-file` is an output like the others (ADR-0076): its directory
/// must be reached without a symlink and lie outside the build context.
/// Checked before the report is set up.
fn check_metrics_file(
    metrics_file: Option<&Path>,
    fence: Option<&rnk_rootfs::Fence>,
) -> Result<(), String> {
    let Some(file) = metrics_file else {
        return Ok(());
    };
    let file = std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf());
    build::check_output_path("--metrics-file", &file, false, fence)
}

/// Writes the metrics report at exit: its directory reached without
/// following a symlink and never through `fence`, made if missing, the file
/// replaced through a temporary file renamed into place (ADR-0076).
fn write_metrics_file(
    path: &Path,
    bytes: &[u8],
    fence: Option<&rnk_rootfs::Fence>,
) -> std::io::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the metrics path has no file name",
        ));
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    rnk_rootfs::OutputDir::open(dir, Some(0o777), fence)
        .and_then(|dir| {
            dir.write_atomically(name, 0o644, |f| {
                f.write_all(bytes).map_err(|source| rnk_rootfs::Error::Io {
                    op: "write",
                    path: path.to_owned(),
                    source,
                })
            })
        })
        .map_err(std::io::Error::other)
}

/// `binary` is where the binary ranikust started from lies, measured for
/// `build` only.
fn run(
    command: &Command,
    telemetry: &Telemetry,
    fault: Option<&str>,
    metrics_file: Option<&Path>,
    binary: Option<BinaryMount>,
) -> Result<(), Failure> {
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
        Command::Build(args) => tracing::debug_span!("ranikust.build")
            .in_scope(|| build(args, telemetry, metrics_file, binary)),
    }
}

fn build(
    args: &BuildArgs,
    telemetry: &Telemetry,
    metrics_file: Option<&Path>,
    binary: Option<BinaryMount>,
) -> Result<(), Failure> {
    let config = BuildConfig::from_args(args).map_err(|invalid| Failure::usage(invalid.0))?;
    telemetry.set_meta("mode", config::name(config.mode));
    telemetry.set_meta("options", config.meta_options());
    tracing::debug!(
        mode = %config::name(config.mode),
        state_dir = %config.state_dir.display(),
        "build settings validated"
    );
    // Before any extraction; the registry limits go to the client.
    config.limits.set_process_wide();
    tracing::debug!(limits = %config.limits.text(), "limits in force");
    match config.mode {
        config::Mode::Kaniko => {
            // Whether root may mount picks how `RUN` is isolated on a root
            // in the state directory (ADR-0043); kaniko mode never mounts.
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
            let runtime = container::detect(&container::Real);
            kaniko(
                &config,
                "--mode=kaniko",
                can_mount,
                runtime,
                metrics_file,
                binary,
            )
        }
        config::Mode::Overlay => {
            overlay_flags(args)?;
            // One span for the whole set-up: the namespace, then the rest.
            let span = tracing::debug_span!("ranikust.setup");
            span.in_scope(setup::enter_namespace)
                .map_err(Failure::refused)?;
            overlay(&config, &span)
        }
        // Whether root may mount decides, before anything is written
        // (ADR-0040).
        config::Mode::Auto => {
            let span = tracing::debug_span!("ranikust.setup");
            match span.in_scope(setup::enter_namespace) {
                Ok(()) => {
                    // Root may mount: overlay mode if its probes pass. Where
                    // overlayfs is unusable (a state directory on NFS or on
                    // overlayfs) outside a container, kaniko mode keeps a
                    // directory per stage and the full sandbox (ADR-0059,
                    // `design/modes-and-detection.md`, `setup::route`).
                    // Overlay mode's flags are checked before its set-up
                    // writes anything, and count only if it is picked.
                    let flags = config::overlay_mode_flags(args);
                    let refusal = match span.in_scope(|| {
                        setup::set_up(&config.state_dir, config.allow_network_none_userns)
                    }) {
                        Ok(setup) => {
                            let warnings = match flags {
                                Ok(warnings) => warnings,
                                Err(invalid) => {
                                    close_store(setup.store);
                                    return Err(Failure::usage(invalid.0));
                                }
                            };
                            for warning in warnings {
                                tracing::warn!("{warning}");
                            }
                            return overlay_built(&config, setup);
                        }
                        Err(refusal) => refusal,
                    };
                    let runtime = container::detect(&container::Real);
                    match setup::route(&refusal, runtime.is_some()) {
                        setup::Route::Refuse => Err(Failure::refused(refusal)),
                        setup::Route::RefuseInContainer => Err(Failure::error(refusal.reason)
                            .with_guidance(
                                "in a container where root may mount, ranikust builds in \
                                 overlay mode only (ADR-0054): mount a volume at the state \
                                 directory",
                            )),
                        setup::Route::KanikoStages => {
                            let why = refusal.reason;
                            tracing::info!(
                                "kaniko mode with a directory per stage, because overlay mode \
                                 cannot run here: {why}"
                            );
                            kaniko(&config, "--mode=auto", true, runtime, metrics_file, binary)
                        }
                    }
                }
                Err(refusal) if refusal.needs_kaniko_mode() => {
                    let why = refusal.reason;
                    tracing::info!("kaniko mode, because root may not mount here: {why}");
                    let runtime = container::detect(&container::Real);
                    kaniko(&config, "--mode=auto", false, runtime, metrics_file, binary)
                }
                Err(refusal) => Err(Failure::refused(refusal)),
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

/// Overlay mode, once the namespace is entered: the rest of the set-up,
/// then the build.
fn overlay(config: &BuildConfig, span: &tracing::Span) -> Result<(), Failure> {
    let setup = span
        .in_scope(|| setup::set_up(&config.state_dir, config.allow_network_none_userns))
        .map_err(Failure::refused)?;
    overlay_built(config, setup)
}

/// Closes a store this run will not build with: its build directory goes
/// now, not at the next start. A store still shared (never, before a
/// build) is left to its last holder.
fn close_store(store: std::sync::Arc<rnk_store::Store>) {
    if let Ok(store) = std::sync::Arc::try_unwrap(store)
        && let Err(e) = store.close()
    {
        tracing::warn!("closing the store: {e}");
    }
}

/// Overlay mode's build, once set up.
fn overlay_built(config: &BuildConfig, setup: setup::Setup) -> Result<(), Failure> {
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
    report_built(&built)
}

/// Logs the built image and prints its digest. The digest is the build's
/// result: failing to print it fails the run.
fn report_built(built: &build::Built) -> Result<(), Failure> {
    tracing::info!("built image {}", built.digest);
    writeln!(std::io::stdout(), "{}", built.digest)
        .map_err(|e| Failure::error(format!("printing the image digest: {e}")))
}

/// Kaniko mode, chosen by `how` (ADR-0050): in a container (kaniko's
/// heuristic, ADR-0048) the root is `/`, and the stages run one unit at a
/// time on it (`design/kaniko-mode.md#stage-roots-on-the-shared-root`);
/// anywhere else each stage gets a directory of its own in the state
/// directory, `<state>/builds/<id>/stages/<n>/rootfs`, scheduled as
/// overlay mode's stages (`#per-stage-directories`, ADR-0059). Then
/// `RLIMIT_NOFILE` is raised (the original kept for `RUN`), the state
/// directory and store set up, and the ignore list built. `can_mount` is
/// whether the build's mount namespace could be entered; `runtime` is
/// what [`container::detect`] found, read once by the caller; `binary`
/// where the binary ranikust started from lies.
fn kaniko(
    config: &BuildConfig,
    how: &str,
    can_mount: bool,
    runtime: Option<container::Runtime>,
    metrics_file: Option<&Path>,
    binary: Option<BinaryMount>,
) -> Result<(), Failure> {
    let nofile = rnk_sandbox::NofileLimit::current();
    if rustix::process::geteuid().as_raw() != 0 {
        return Err(Failure::error(format!(
            "{how}: kaniko mode needs root (chown, chroot, switching users), as kaniko does; \
             run ranikust as root in the container or VM that builds"
        )));
    }
    let on_slash = match runtime {
        Some(runtime) => {
            tracing::debug!(%runtime, "running in a container: building on /");
            true
        }
        None => {
            tracing::info!(
                "not in a container: kaniko mode builds in a directory of the state directory \
                 {}, never on this machine's /",
                config.state_dir.display()
            );
            false
        }
    };
    if on_slash {
        refuse_unsafe_slash(config, how, can_mount, binary)?;
    }
    match statedir::raise_nofile_limit() {
        Ok(limit) => tracing::debug!(?limit, "RLIMIT_NOFILE raised to its hard limit"),
        // The walk still runs, with fewer descriptors to hold directories.
        Err(e) => tracing::warn!("cannot raise RLIMIT_NOFILE to its hard limit: {e}"),
    }
    if on_slash && config.jobs.is_some() {
        tracing::warn!(
            "kaniko mode on /: stages run one at a time on the shared root; --jobs still \
             bounds the uploads"
        );
    }
    // On `/` a `RUN` shares ranikust's PID namespace: SIGINT and SIGTERM
    // are blocked before any thread starts, and read with their sender, so
    // that only the operator's stop the build (ADR-0077).
    let stop = if on_slash {
        let blocked = rnk_sandbox::block_stop_signals().map_err(|e| {
            Failure::error(format!(
                "{how}: kaniko mode on /: cannot block SIGINT and SIGTERM, so that a RUN cannot \
                 stop the build: {e}"
            ))
        })?;
        Some(blocked)
    } else {
        None
    };
    let setup = tracing::debug_span!("ranikust.setup")
        .in_scope(|| setup::set_up_kaniko(&config.state_dir, on_slash, can_mount, nofile))
        .map_err(Failure::refused)?;
    if on_slash {
        // After the set-up gave ranikust only what a `RUN` keeps: the
        // environments it reads now are exactly those a `RUN` reads
        // (ADR-0078).
        exposure::warn_credential_holders(&exposure::secret_variables(config));
    }
    let setup::KanikoSetup {
        store,
        sandbox,
        isolation,
        root,
        state_dir,
        root_lock,
    } = setup;
    let snapshots = match (root, stop) {
        (Some((root, root_path)), Some(stop)) => build::Snapshots::Kaniko {
            snapshotter: kaniko_snapshotter(
                config,
                root,
                &root_path,
                &state_dir,
                &store,
                metrics_file,
            )?,
            isolation,
            stop,
        },
        (Some(_), None) => {
            return Err(Failure::error(
                "kaniko mode on /: the stop signals were not blocked",
            ));
        }
        (None, _) => {
            build::Snapshots::KanikoStages(stages_snapshotter(config, &store, metrics_file)?)
        }
    };
    let built = build::run(config, store, sandbox, snapshots).map_err(Failure::error)?;
    // The lock of `/` is held to the end of the build.
    drop(root_lock);
    report_built(&built)
}

/// What kaniko mode on a container's `/` refuses before anything is
/// written, in this order, then the warnings for what ranikust cannot
/// close itself. A `RUN` there runs directly, as root unless `USER` says
/// otherwise, so the container itself must keep it from the host.
fn refuse_unsafe_slash(
    config: &BuildConfig,
    how: &str,
    can_mount: bool,
    binary: Option<BinaryMount>,
) -> Result<(), Failure> {
    if can_mount {
        // A container that lets root mount (`--privileged`, CAP_SYS_ADMIN)
        // also gives a `Direct` `RUN` on `/` the host's devices and a
        // writable /proc/sys: refused (ADR-0054). Overlay mode runs there
        // instead.
        return Err(Failure::error(format!(
            "{how}: kaniko mode on / in a container where root may mount would run RUN with \
             the host's devices and a writable /proc/sys; use overlay mode there \
             (--mode=overlay or --mode=auto), with a volume at the state directory"
        )));
    }
    if config.network == config::Network::None {
        // Every `RUN` would be refused (ADR-0057): said before the pull.
        return Err(Failure::error(format!(
            "{how}: --network=none: kaniko mode on / runs RUN directly, where no network \
             namespace can be made, and RUN is never given the container's network in its \
             place; use overlay mode (a container that may mount) for a build without network"
        )));
    }
    // A writable /proc/sys hands a uid 0 `RUN` the host's root through
    // kernel/core_pattern without any capability (ADR-0051, ADR-0054).
    match exposure::proc_sys_read_only(Path::new("/proc/sys")) {
        Ok(true) => {}
        Ok(false) => {
            return Err(Failure::error(format!(
                "{how}: kaniko mode on / with a writable /proc/sys would let RUN write \
                 kernel/core_pattern and run a program as the host's root; keep the \
                 container's default read-only /proc/sys, or use overlay mode"
            )));
        }
        Err(e) => {
            return Err(Failure::error(format!(
                "{how}: cannot tell whether /proc/sys is read-only ({e}); kaniko mode on / \
                 refuses to build without knowing"
            )));
        }
    }
    refuse_writable_binary(how, binary)?;
    // What ranikust cannot close itself (ADR-0051, B4 and B5; ADR-0078).
    // The processes holding credentials are looked for after the set-up,
    // with the capabilities a `RUN` keeps (`kaniko`).
    exposure::warn_build_args(&config.build_args);
    for warning in exposure::file_secret_warnings(&config.secrets) {
        tracing::warn!("{warning}");
    }
    Ok(())
}

/// Kaniko mode on `/` refuses ranikust's binary on a writable mount other
/// than `/`'s, a bind mount or a volume, or reachable through one (ADR-0101):
/// a `RUN`, root on `/`, could alter it, and the build no longer runs it. A
/// binary in the container's own root builds on an overlay root (overlayfs
/// or fuse-overlayfs), elsewhere with one link and no other writable mount
/// reaching it; so does one on a read-only mount no writable mount
/// reaches; one whose mount is not known is refused.
fn refuse_writable_binary(how: &str, binary: Option<BinaryMount>) -> Result<(), Failure> {
    match binary {
        Some(BinaryMount::Root | BinaryMount::ReadOnly) => Ok(()),
        Some(BinaryMount::Writable) => Err(Failure::error(format!(
            "{how}: kaniko mode on / with ranikust's binary on a writable mount other than /'s (a \
             bind mount or a volume) would let RUN alter that file"
        ))
        .with_guidance(
            "mount the binary read-only: Docker -v <binary>:/ranikust/bin/ranikust:ro, Kubernetes \
             readOnly: true on its volumeMount (ADR-0101)",
        )),
        Some(BinaryMount::Exposed) => Err(Failure::error(format!(
            "{how}: kaniko mode on / with ranikust's binary in a tree or a file that a writable \
             mount also reaches (a link of it bound read-write among them, even one whose name \
             was removed) would let RUN alter that file"
        ))
        .with_guidance(
            "put the binary outside the mounted tree, or mount that tree read-only (Docker :ro, \
             Kubernetes readOnly: true on its volumeMount), or mount a single-link copy of the \
             binary read-only from outside it, and bind no link of it read-write (ADR-0101)",
        )),
        Some(BinaryMount::Linked) => Err(Failure::error(format!(
            "{how}: kaniko mode on / with ranikust's binary on a file with other hard links, \
             which a writable mount may reach, would let RUN alter that file"
        ))
        .with_guidance(
            "give the binary a single link: mount a single-link copy of it read-only, or, in the \
             container's root, leave it no hard-link alias (cp makes a copy with one link; \
             cargo's release binaries, such as target/release/ranikust, are hard links of their \
             copies in deps/) (ADR-0101)",
        )),
        Some(BinaryMount::Unplaced) => Err(Failure::error(format!(
            "{how}: cannot tell whether a writable mount reaches ranikust's binary \
             (/proc/self/mountinfo cannot be parsed, changed while it was read, does not place it \
             or does not type a FUSE root; a writable directory of its file system had its name \
             removed, or a writable mount of that file system cannot be reached at its mount \
             point; or the binary's own name was removed); kaniko mode on / refuses to build \
             without knowing"
        ))
        .with_guidance(
            "run the binary from a file that stays in place, beside no writable directory mount \
             whose source was removed and no writable mount another mount hides, and start the \
             build again once no mount changes while it starts; or build it into an image run \
             on an overlayfs root, as Docker's and containerd's default storage gives, where the \
             binary's check reads no mount table (ADR-0101)",
        )),
        Some(BinaryMount::NotWeighed) => Err(Failure::error(format!(
            "{how}: ranikust did not find itself in a container when it started, so it did not \
             look for writable mounts that reach its binary; kaniko mode on / refuses to build \
             without knowing"
        ))
        .with_guidance(
            "start the build again once what tells a container (files such as /.dockerenv, \
             /run/.containerenv or /proc/self/cgroup, ADR-0048's list) no longer changes while \
             it starts (ADR-0101)",
        )),
        Some(BinaryMount::NoMountId) => Err(Failure::error(format!(
            "{how}: cannot tell which mount ranikust's binary is on (statx gave no mount id: an \
             emulator such as qemu-user, or a binary run from a memfd); kaniko mode on / refuses \
             to build without knowing"
        ))
        .with_guidance(
            "run ranikust natively, built for the machine's own architecture, from a regular \
             file, not a memfd (ADR-0101)",
        )),
        Some(BinaryMount::Unknown(e)) => Err(Failure::error(format!(
            "{how}: cannot measure the mount of ranikust's binary ({e}); kaniko mode on / \
             refuses to build without knowing"
        ))
        .with_guidance(
            "mount /proc in the container so that /proc/self/exe and /proc/self/mountinfo can be \
             read, and let its seccomp profile allow fstatfs, openat2 and statx: ranikust reads \
             the binary's mount with statx (STATX_MNT_ID) and fstatvfs, and looks up the \
             binary's name and each writable mount point of its file system with openat2 and \
             statx (ADR-0101)",
        )),
        None => Err(Failure::error(format!(
            "{how}: the mount of ranikust's binary was not measured; kaniko mode on / refuses \
             to build without knowing"
        ))),
    }
}

/// Kaniko mode's snapshotter on `/` in a container, opened once by the
/// set-up, with the ignore list from mountinfo, the flags and the state
/// directory (inside `/`, so on the list), and the walk's options. The
/// paths given to the ignore list are physical: `/` and the state
/// directory, canonical and opened without following a symlink. What later
/// stages read of earlier ones is kept as tars in the anonymous store's
/// files, unpacked only for a reader in the state directory (ADR-0056).
fn kaniko_snapshotter(
    config: &BuildConfig,
    root: rnk_rootfs::RootPath,
    root_path: &Path,
    state_dir: &Path,
    store: &std::sync::Arc<rnk_store::Store>,
    metrics_file: Option<&Path>,
) -> Result<rnk_snapshot::Kaniko, Failure> {
    let fail = |what: &str, e: &dyn std::fmt::Display| Failure::error(format!("{what}: {e}"));
    if !store.is_anonymous() {
        return Err(Failure::error(
            "kaniko mode on / keeps its state in anonymous files only (ADR-0051)",
        ));
    }
    let mounts =
        rnk_snapshot::kaniko::read_mountinfo().map_err(|e| fail("reading mountinfo", &e))?;
    let state_root = rnk_rootfs::RootPath::open(state_dir)
        .map_err(|e| fail(&format!("opening {}", state_dir.display()), &e))?;
    same_mount_only_inside(&root, root_path, &state_root, state_dir, &mounts)?;
    let ignore = rnk_snapshot::kaniko::ignore_list(
        &ignore_spec(config, root_path, Some(state_dir)),
        &mounts,
    )
    .map_err(|e| fail("building the ignore list", &e))?;
    paths_outside_the_root(config, metrics_file, root_path, &ignore)?;
    // Nothing is kept by name: the keep area is the state directory, on
    // the ignore list, for the directories a `COPY --from` reads from while
    // no RUN runs (ADR-0056).
    let kaniko = rnk_snapshot::Kaniko::new(
        root,
        root_path.to_owned(),
        ignore,
        walk_options(config),
        state_root,
    );
    kaniko
        .keep_in_anonymous_files(std::sync::Arc::clone(store))
        .map_err(|e| fail("keeping stages as anonymous files", &e))?;
    Ok(kaniko)
}

/// Kaniko mode's snapshotter off `/` (ADR-0059): a directory per stage in
/// `<state>/builds/<id>/stages`, private to the build, with the ignore list
/// of a stage root (the fixed paths and `--ignore-path`; nothing is
/// mounted in a directory ranikust just made) and the walk's options. The
/// operator's paths must stay out of the stages directory.
fn stages_snapshotter(
    config: &BuildConfig,
    store: &std::sync::Arc<rnk_store::Store>,
    metrics_file: Option<&Path>,
) -> Result<rnk_snapshot::KanikoStages, Failure> {
    let fail = |what: &str, e: &dyn std::fmt::Display| Failure::error(format!("{what}: {e}"));
    let stages_dir = store.build_dir().join("stages");
    let ignore =
        stage_ignore_list(config, &stages_dir).map_err(|e| fail("building the ignore list", &e))?;
    paths_outside_the_root(config, metrics_file, &stages_dir, &ignore)?;
    rnk_snapshot::KanikoStages::new(store, ignore, walk_options(config))
        .map_err(|e| fail("setting up the stage directories", &e))
}

/// What the ignore list is built from: the root (and, on `/`, the state
/// directory) and `--ignore-path`.
fn ignore_spec<'a>(
    config: &'a BuildConfig,
    root: &'a Path,
    state_dir: Option<&'a Path>,
) -> rnk_snapshot::kaniko::IgnoreSpec<'a> {
    rnk_snapshot::kaniko::IgnoreSpec {
        root,
        state_dir,
        ignore_paths: &config.ignore_paths,
    }
}

/// The ignore list of a stage root off `/` (ADR-0059): the fixed paths and
/// `--ignore-path`. Mounts are none of a stage root's: it is made after
/// this list.
fn stage_ignore_list(
    config: &BuildConfig,
    stages_dir: &Path,
) -> Result<rnk_rootfs::IgnoreList, rnk_snapshot::Error> {
    rnk_snapshot::kaniko::ignore_list(&ignore_spec(config, stages_dir, None), &[])
}

/// The walk's options from the flags.
fn walk_options(config: &BuildConfig) -> rnk_snapshot::WalkOptions {
    rnk_snapshot::WalkOptions {
        workers: config.snapshot_workers,
        mode: match config.snapshot_mode {
            config::SnapshotMode::Full => rnk_snapshot::SnapshotMode::Full,
            config::SnapshotMode::Redo => rnk_snapshot::SnapshotMode::Redo,
            config::SnapshotMode::Time => rnk_snapshot::SnapshotMode::Time,
        },
    }
}

/// A state directory inside the root's tree on its filesystem, reached
/// through another mount (a bind mount of a directory inside the root), is
/// refused: the ignore list knows it by path only, and emptying the root
/// would delete the live store. Each path is placed in its filesystem with
/// its mount's `fs_root` from mountinfo; two volumes of sibling directories
/// on one filesystem pass.
fn same_mount_only_inside(
    root: &rnk_rootfs::RootPath,
    root_path: &Path,
    state: &rnk_rootfs::RootPath,
    state_path: &Path,
    mounts: &[rnk_snapshot::kaniko::MountInfo],
) -> Result<(), Failure> {
    match state_in_root(
        mounts,
        (root.mnt_id(), root_path),
        (state.mnt_id(), state_path),
    ) {
        Some(false) => Ok(()),
        None => Err(Failure::error(format!(
            "cannot place the root {} and the state directory {} in /proc/self/mountinfo",
            root_path.display(),
            state_path.display()
        ))),
        Some(true) => Err(Failure::usage(format!(
            "the state directory {} is a directory of the root {} reached through another \
             mount (a bind mount?): emptying the root would delete it; pass a --state-dir \
             outside the root",
            state_path.display(),
            root_path.display()
        ))),
    }
}

/// Whether the state directory (its mount id and path) lies in the root's
/// tree on the root's filesystem, or holds the root; `None` when mountinfo
/// lacks either mount.
fn state_in_root(
    mounts: &[rnk_snapshot::kaniko::MountInfo],
    (root_mnt, root_path): (u64, &Path),
    (state_mnt, state_path): (u64, &Path),
) -> Option<bool> {
    if state_path.starts_with(root_path) || root_mnt == state_mnt {
        return Some(false);
    }
    let in_fs = |mnt_id: u64, path: &Path| {
        let m = mounts.iter().find(|m| m.mount_id == mnt_id)?;
        // A removed root's former name places nothing.
        if m.root_deleted {
            return None;
        }
        let rel = path.strip_prefix(&m.mount_point).ok()?;
        Some((m.device.clone(), m.fs_root.join(rel)))
    };
    let (rdev, rfs) = in_fs(root_mnt, root_path)?;
    let (sdev, sfs) = in_fs(state_mnt, state_path)?;
    // Either way round: a state directory inside the root is deleted with
    // it, one holding the root deletes the root's files as the store's.
    Some(rdev == sdev && (sfs.starts_with(&rfs) || rfs.starts_with(&sfs)))
}

/// The operator's paths that the build reads or writes while the root
/// changes (the context, the outputs, file secrets, the metrics report)
/// must not be in the
/// root, where extracting a base or emptying it would replace or delete
/// them, unless the ignore list keeps them (a volume, `/ranikust`,
/// `--ignore-path`). Checked before the root changes. A path that does
/// not exist yet is judged by its nearest existing ancestor. For a file
/// secret on `/`, ADR-0085 decides that ranikust puts the file's own path on
/// that list itself, not its directory: decided, not built yet.
fn paths_outside_the_root(
    config: &BuildConfig,
    metrics_file: Option<&Path>,
    root: &Path,
    ignore: &rnk_rootfs::IgnoreList,
) -> Result<(), Failure> {
    let mut paths: Vec<(&str, &Path)> = vec![("the build context", config.context.as_path())];
    let outputs = [
        ("--tar-path", &config.tar_path),
        ("--oci-layout-path", &config.oci_layout_path),
        ("--digest-file", &config.digest_file),
        (
            "--image-name-with-digest-file",
            &config.image_name_digest_file,
        ),
        (
            "--image-name-tag-with-digest-file",
            &config.image_name_tag_digest_file,
        ),
    ];
    for (flag, p) in &outputs {
        if let Some(p) = p {
            paths.push((flag, p.as_path()));
        }
    }
    if let Some(p) = metrics_file {
        paths.push(("--metrics-file", p));
    }
    for (_, secret) in &config.secrets {
        if let rnk_builder::Secret::File(p) = secret {
            paths.push(("--secret", p.as_path()));
        }
    }
    for (what, path) in paths {
        let physical = physical_path(path);
        let Ok(rel) = physical.strip_prefix(root) else {
            continue;
        };
        let rel = Path::new("/").join(rel);
        if !ignore.covers(&rel) {
            return Err(Failure::usage(format!(
                "{what} {} lies in kaniko mode's root {}, which the build replaces and \
                 empties: put it on a volume or outside the root",
                path.display(),
                root.display()
            )));
        }
    }
    Ok(())
}

/// `path` made absolute with its nearest existing ancestor resolved.
fn physical_path(path: &Path) -> std::path::PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut rest = Vec::new();
    let mut at = abs.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            let mut out = real;
            for c in rest.iter().rev() {
                out.push(c);
            }
            return out;
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                at = parent;
            }
            _ => return abs,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config_of(flags: &[&str]) -> BuildConfig {
        let argv = std::iter::once("ranikust").chain(flags.iter().copied());
        let Command::Build(args) = cli::parse(argv).unwrap().cli.into_command() else {
            panic!("expected build");
        };
        BuildConfig::from_args(&args).unwrap()
    }

    fn reported(failure: &Failure) -> String {
        let mut out = Vec::new();
        report_to(&mut out, &failure.message, failure.guidance).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// A base image's tar entry name, as rnk-rootfs refuses it and the
    /// scheduler wraps it, reaches the fatal error line escaped: one line,
    /// nothing a terminal acts on, nothing that starts a line of its own.
    #[test]
    fn the_error_line_escapes_untrusted_names() {
        let name = "../\x1b]0;RNK\x07\n\x1b[36mINFO\x1b[0m[0001] built image sha256:0\u{9b}2J\r";
        let entry = rnk_rootfs::Error::Entry {
            path: name.into(),
            reason: "the name leaves the root".to_owned(),
        };
        let failure = Failure::error(format!("stage base: preparing: {entry}"));
        let out = reported(&failure);
        assert_eq!(
            out,
            "ranikust: error: stage base: preparing: tar entry \
             ../\\u{1b}]0;RNK\\u{7}\\n\\u{1b}[36mINFO\\u{1b}[0m[0001] built image \
             sha256:0\\u{9b}2J\\r: the name leaves the root\n"
        );
        assert!(!out.trim_end().chars().any(char::is_control), "{out:?}");
        // The metrics report keeps the message itself; JSON escapes it.
        assert_eq!(failure.text(), failure.message);
    }

    /// A refusal's guidance is ranikust's own text: it keeps its line,
    /// while a line feed in the reason is escaped. The metrics report gets
    /// what the refusal's `Display` gives, as before.
    #[test]
    fn guidance_keeps_its_own_line() {
        let refusal = setup::Refusal {
            reason: "probe of /x\ny failed".to_owned(),
            guidance: "pass --state-dir on a local filesystem",
            kind: setup::Kind::Other,
        };
        let shown = refusal.to_string();
        let failure = Failure::refused(refusal);
        assert_eq!(
            reported(&failure),
            "ranikust: error: probe of /x\\ny failed\n  pass --state-dir on a local filesystem\n"
        );
        assert_eq!(failure.text(), shown);
    }

    /// `--metrics-file` in the build context, or through a symlink, is
    /// refused before the report is set up; the report's writer never
    /// follows a symlink either (ADR-0076).
    #[test]
    fn the_metrics_file_stays_out_of_the_context_and_off_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = dir.path().join("ctx");
        std::fs::create_dir(&ctx).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("out")).unwrap();
        let flag = format!("--context={}", ctx.display());
        let argv = ["ranikust", flag.as_str(), "--no-push"];
        let command = cli::parse(argv).unwrap().cli.into_command();
        let fence = context_fence(&command).unwrap();
        assert!(fence.is_some());
        let check = |file: &Path| check_metrics_file(Some(file), fence.as_ref());
        let e = check(&ctx.join("m.json")).unwrap_err();
        assert!(e.contains("build context"), "{e}");
        let e = check(&dir.path().join("out/m.json")).unwrap_err();
        assert!(e.contains("symlink"), "{e}");
        check(&dir.path().join("m.json")).unwrap();

        // The writer itself, with the same fence: through a symlink or into
        // the context, nothing is written.
        let write = |p: &Path| write_metrics_file(p, b"{}\n", fence.as_ref());
        let e = write(&dir.path().join("out/m.json")).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        let e = write(&ctx.join("m.json")).unwrap_err();
        assert!(e.to_string().contains("build context"), "{e}");
        assert!(!ctx.join("m.json").exists());
        write(&dir.path().join("new/m.json")).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("new/m.json")).unwrap(),
            b"{}\n"
        );
    }

    /// A report asked for a build whose context cannot be opened is refused
    /// before anything is set up: without the context's fence, the report
    /// could not be kept out of it (ADR-0076). Without a report, the build
    /// reports the context itself.
    #[test]
    fn a_metrics_file_needs_a_context_that_opens() {
        let dir = tempfile::tempdir().unwrap();
        let flag = format!("--context={}", dir.path().join("missing").display());
        let argv = ["ranikust", flag.as_str(), "--no-push"];
        let command = cli::parse(argv).unwrap().cli.into_command();
        let e = metrics_preflight(&command, Some(&dir.path().join("m.json"))).unwrap_err();
        assert!(e.contains("cannot be opened"), "{e}");
        assert!(metrics_preflight(&command, None).unwrap().is_none());
        // The writer of such a build writes nothing either. It is built on a
        // fence cell of its own: the process-wide one may already hold the
        // fence of a build another test prepared.
        static NO_BUILD: std::sync::OnceLock<rnk_rootfs::Fence> = std::sync::OnceLock::new();
        let write = metrics_writer(true, None, &NO_BUILD);
        let e = (write.0)(&dir.path().join("m.json"), b"{}\n").unwrap_err();
        assert!(e.to_string().contains("could not be fenced"), "{e}");
        assert!(!dir.path().join("m.json").exists());
        // A usage error is reported as itself, not as an unopened context.
        let argv = ["ranikust", flag.as_str()];
        let command = cli::parse(argv).unwrap().cli.into_command();
        let e = metrics_preflight(&command, Some(&dir.path().join("m.json"))).unwrap_err();
        assert!(!e.contains("cannot be opened"), "{e}");
        assert!(e.contains("--destination"), "{e}");
    }

    /// Kaniko mode on `/` refuses in a fixed order: root that may mount
    /// (ADR-0054) before `--network=none` (ADR-0057). Both return before
    /// /proc/sys is read, so this needs no file and no privilege.
    #[test]
    fn the_refusals_on_slash_come_in_their_order() {
        let config = config_of(&["--no-push", "--network=none"]);
        let may_mount = refuse_unsafe_slash(&config, "--mode=kaniko", true, None).unwrap_err();
        assert!(
            may_mount
                .message
                .contains("in a container where root may mount"),
            "{}",
            may_mount.message
        );
        let no_mount = refuse_unsafe_slash(&config, "--mode=kaniko", false, None).unwrap_err();
        assert!(
            no_mount
                .message
                .contains("--network=none: kaniko mode on / runs RUN directly"),
            "{}",
            no_mount.message
        );
    }

    /// A host where `container::detect` finds Docker, or nothing.
    struct FakeHost {
        docker: bool,
    }

    impl container::Host for FakeHost {
        fn exists(&self, path: &str) -> bool {
            self.docker && path == "/.dockerenv"
        }
        fn read(&self, _: &str) -> String {
            String::new()
        }
        fn container_env(&self) -> Option<String> {
            None
        }
    }

    /// ADR-0101: the seal walks the writable mounts that may reach the
    /// binary only where the build may be kaniko mode on a container's `/`:
    /// as root, in a container, unless overlay mode is asked for.
    #[test]
    fn only_a_build_that_may_be_kaniko_on_slash_walks_the_mounts() {
        let (inside, outside) = (FakeHost { docker: true }, FakeHost { docker: false });
        for mode in [config::Mode::Auto, config::Mode::Kaniko] {
            assert!(may_be_kaniko_on_slash(mode, true, &inside), "{mode:?}");
            assert!(!may_be_kaniko_on_slash(mode, true, &outside), "{mode:?}");
            assert!(!may_be_kaniko_on_slash(mode, false, &inside), "{mode:?}");
        }
        assert!(!may_be_kaniko_on_slash(
            config::Mode::Overlay,
            true,
            &inside
        ));
    }

    /// ADR-0101: kaniko mode on `/` refuses ranikust's binary on a writable
    /// mount other than `/`'s, with the guidance to mount it read-only, and
    /// one a writable mount reaches (its tree, another hard link), with
    /// guidance of its own; one in the container's own root or on a
    /// read-only mount builds, and one whose mount is not known is refused.
    #[test]
    fn kaniko_mode_on_slash_refuses_a_binary_on_a_writable_mount() {
        let how = "--mode=kaniko";
        refuse_writable_binary(how, Some(BinaryMount::Root)).unwrap();
        refuse_writable_binary(how, Some(BinaryMount::ReadOnly)).unwrap();
        let writable = refuse_writable_binary(how, Some(BinaryMount::Writable)).unwrap_err();
        assert_eq!(writable.code, 1);
        assert!(
            writable
                .message
                .starts_with("--mode=kaniko: kaniko mode on / ")
                && writable.message.contains("writable mount other than /'s"),
            "{}",
            writable.message
        );
        assert!(
            writable.guidance.contains(":ro") && writable.guidance.contains("readOnly: true"),
            "{}",
            writable.guidance
        );
        let exposed = refuse_writable_binary(how, Some(BinaryMount::Exposed)).unwrap_err();
        assert!(
            exposed.code == 1
                && exposed.message.contains("a writable mount also reaches")
                && exposed.message.contains("even one whose name was removed")
                && exposed.guidance.contains("bind no link of it read-write")
                && exposed.guidance.contains("outside the mounted tree")
                && exposed.guidance.contains("single-link copy")
                && exposed.guidance.contains(":ro")
                && exposed.guidance.contains("readOnly: true"),
            "{}: {}",
            exposed.message,
            exposed.guidance
        );
        let linked = refuse_writable_binary(how, Some(BinaryMount::Linked)).unwrap_err();
        assert!(
            linked.code == 1
                && linked.message.contains("other hard links")
                && linked.guidance.contains("single-link copy")
                && linked.guidance.contains("hard-link alias")
                && linked.guidance.contains("read-only"),
            "{}: {}",
            linked.message,
            linked.guidance
        );
        let errno = rustix::io::Errno::NOSYS;
        for unknown in [
            Some(BinaryMount::NoMountId),
            Some(BinaryMount::Unplaced),
            Some(BinaryMount::NotWeighed),
            Some(BinaryMount::Unknown(errno)),
            None,
        ] {
            let refused = refuse_writable_binary(how, unknown).unwrap_err();
            assert_eq!(refused.code, 1);
            assert!(
                refused.message.contains("without knowing"),
                "{}",
                refused.message
            );
        }
        let unknown = refuse_writable_binary(how, Some(BinaryMount::Unknown(errno))).unwrap_err();
        assert!(
            unknown.message.contains(&errno.to_string())
                && unknown.guidance.contains("/proc/self/exe")
                && unknown.guidance.contains("/proc/self/mountinfo")
                && unknown.guidance.contains("fstatfs")
                && unknown.guidance.contains("openat2"),
            "{}: {}",
            unknown.message,
            unknown.guidance
        );
        let unplaced = refuse_writable_binary(how, Some(BinaryMount::Unplaced)).unwrap_err();
        assert!(
            unplaced.message.contains("/proc/self/mountinfo")
                && unplaced.message.contains("changed while it was read")
                && unplaced.message.contains("does not type a FUSE root")
                && unplaced
                    .message
                    .contains("cannot be reached at its mount point")
                && unplaced.guidance.contains("stays in place")
                && unplaced
                    .guidance
                    .contains("no mount changes while it starts")
                && unplaced.guidance.contains("overlayfs root")
                && !unplaced.guidance.contains("fuse-overlayfs"),
            "{}: {}",
            unplaced.message,
            unplaced.guidance
        );
        let unweighed = refuse_writable_binary(how, Some(BinaryMount::NotWeighed)).unwrap_err();
        assert!(
            unweighed
                .message
                .contains("did not find itself in a container")
                && unweighed.guidance.contains("start the build again")
                && unweighed.guidance.contains("/.dockerenv")
                && unweighed.guidance.contains("ADR-0048's list"),
            "{}: {}",
            unweighed.message,
            unweighed.guidance
        );
        let no_id = refuse_writable_binary(how, Some(BinaryMount::NoMountId)).unwrap_err();
        assert!(
            no_id.message.contains("no mount id")
                && no_id.message.contains("memfd")
                && no_id.guidance.contains("natively")
                && no_id.guidance.contains("not a memfd"),
            "{}: {}",
            no_id.message,
            no_id.guidance
        );
    }

    /// `--snapshot-mode` and `--snapshot-workers` reach the walk with their
    /// meaning (criterion 8, review finding #3): a swapped arm or a lost
    /// worker count fails here.
    #[test]
    fn the_snapshot_flags_reach_the_walk_with_their_meaning() {
        let opts = walk_options(&config_of(&["--no-push"]));
        assert_eq!(opts.mode, rnk_snapshot::SnapshotMode::Full);
        assert_eq!(opts.workers.get(), 8);
        for (flag, mode) in [
            ("--snapshot-mode=full", rnk_snapshot::SnapshotMode::Full),
            ("--snapshot-mode=redo", rnk_snapshot::SnapshotMode::Redo),
            ("--snapshot-mode=time", rnk_snapshot::SnapshotMode::Time),
        ] {
            let opts = walk_options(&config_of(&["--no-push", flag, "--snapshot-workers=3"]));
            assert_eq!(opts.mode, mode, "{flag}");
            assert_eq!(opts.workers.get(), 3, "{flag}");
        }
    }

    /// `--ignore-path` reaches a stage root's ignore list, a glob per
    /// component as in kaniko, and the list of `/` (criterion 8, review
    /// finding #3).
    #[test]
    fn ignore_paths_reach_the_ignore_lists_of_both_kinds_of_root() {
        let config = config_of(&[
            "--no-push",
            "--ignore-path=/var/cache/*",
            "--ignore-path=/opt/keep",
        ]);
        let stages = Path::new("/state/builds/b1/stages");
        let list = stage_ignore_list(&config, stages).unwrap();
        assert!(list.covers("var/cache/apt/x"), "glob entry");
        assert!(!list.covers("var/cache"), "the directory itself stays");
        assert!(list.covers("opt/keep/f") && list.covers("opt/keep"));
        assert!(!list.covers("opt/other"));
        let plain = stage_ignore_list(&config_of(&["--no-push"]), stages).unwrap();
        assert!(!plain.covers("var/cache/apt/x"));
        let spec = ignore_spec(
            &config,
            Path::new("/"),
            Some(Path::new("/var/lib/ranikust")),
        );
        assert_eq!(spec.ignore_paths, config.ignore_paths.as_slice());
        let on_slash = rnk_snapshot::kaniko::ignore_list(&spec, &[]).unwrap();
        assert!(on_slash.covers("var/cache/apt/x") && on_slash.covers("var/lib/ranikust/x"));
    }

    #[test]
    fn operator_paths_in_the_root_are_refused_unless_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("w");
        std::fs::create_dir_all(root.join("vol")).unwrap();
        let outside = base.join("out");
        let mut ignore = rnk_rootfs::IgnoreList::new();
        ignore.insert("/vol").unwrap();
        let ctx = format!("--context={}", outside.display());
        let check = |extra: &str| {
            let config = config_of(&["--no-push", &ctx, extra]);
            paths_outside_the_root(&config, None, &root, &ignore)
        };
        let tar = |p: &Path| format!("--tar-path={}", p.display());
        check(&tar(&outside.join("i.tar"))).unwrap();
        check(&tar(&root.join("vol/i.tar"))).unwrap();
        let e = check(&tar(&root.join("not-yet/i.tar"))).unwrap_err();
        assert!(e.message.contains("--tar-path"), "{}", e.message);
        let secret = format!("--secret=id=s,src={}", root.join("s").display());
        let e = check(&secret).unwrap_err();
        assert!(e.message.contains("--secret"), "{}", e.message);
        let in_root = format!("--context={}", root.display());
        let config = config_of(&["--no-push", &in_root]);
        let e = paths_outside_the_root(&config, None, &root, &ignore).unwrap_err();
        assert!(e.message.contains("context"), "{}", e.message);
        let config = config_of(&["--no-push", &ctx]);
        let report = root.join("m.json");
        let e = paths_outside_the_root(&config, Some(&report), &root, &ignore).unwrap_err();
        assert!(e.message.contains("--metrics-file"), "{}", e.message);
    }

    #[test]
    fn a_state_directory_beside_the_root_on_its_mount_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        for d in ["w", "s"] {
            std::fs::create_dir(base.join(d)).unwrap();
        }
        let open = |d: &str| rnk_rootfs::RootPath::open(base.join(d)).unwrap();
        let mounts = rnk_snapshot::kaniko::read_mountinfo().unwrap();
        let check = |state: &rnk_rootfs::RootPath, path: &Path, mounts: &[_]| {
            same_mount_only_inside(&open("w"), &base.join("w"), state, path, mounts)
        };
        check(&open("s"), &base.join("s"), &mounts).unwrap();
    }

    #[test]
    fn a_state_directory_bound_from_inside_the_root_is_refused() {
        // `/srv/w` is the root; `/data` holds two sibling volumes of one
        // filesystem, and `/state` binds `/srv/w/x` from the same one.
        let mounts = rnk_snapshot::kaniko::parse_mountinfo(
            b"1 0 8:1 / / rw - ext4 /dev/sda1 rw\n\
              2 1 8:1 /vols/a /data/a rw - ext4 /dev/sda1 rw\n\
              3 1 8:1 /vols/b /data/b rw - ext4 /dev/sda1 rw\n\
              4 1 8:1 /srv/w/x /state rw - ext4 /dev/sda1 rw\n\
              5 1 8:2 /srv/w /other rw - ext4 /dev/sdb1 rw\n",
        )
        .unwrap();
        let check = |root: (u64, &str), state: (u64, &str)| {
            state_in_root(
                &mounts,
                (root.0, Path::new(root.1)),
                (state.0, Path::new(state.1)),
            )
        };
        assert_eq!(check((1, "/srv/w"), (4, "/state")), Some(true));
        assert_eq!(check((1, "/srv/w"), (4, "/state/sub")), Some(true));
        assert_eq!(check((2, "/data/a"), (3, "/data/b")), Some(false));
        assert_eq!(
            check((1, "/srv/w"), (5, "/other")),
            Some(false),
            "another device"
        );
        assert_eq!(check((1, "/srv/w"), (1, "/srv/w/.ranikust")), Some(false));
        assert_eq!(check((1, "/srv/w"), (9, "/nowhere")), None);
        // `/state2` binds `/srv` from the same filesystem: it holds the root.
        let mut more = mounts.clone();
        more.extend(
            rnk_snapshot::kaniko::parse_mountinfo(b"6 1 8:1 /srv /state2 rw - ext4 /dev/sda1 rw\n")
                .unwrap(),
        );
        let held = state_in_root(&more, (1, Path::new("/srv/w")), (6, Path::new("/state2")));
        assert_eq!(held, Some(true));
        // `/state3` binds a directory whose name was removed since.
        more.extend(
            rnk_snapshot::kaniko::parse_mountinfo(
                b"7 1 8:1 /srv/w/y//deleted /state3 rw - ext4 /dev/sda1 rw\n",
            )
            .unwrap(),
        );
        let gone = state_in_root(&more, (1, Path::new("/srv/w")), (7, Path::new("/state3")));
        assert_eq!(gone, None, "a removed root is not placed");
    }

    /// Red team (criterion 8, T4): an `--ignore-path` the flags accept never
    /// takes a protected path off either ignore list (the state directory
    /// on `/`, a mount point, the fixed paths: they stay covered, the list
    /// only grows), and never fails later, once the set-up has written:
    /// what `BuildConfig::from_args` accepts, `ignore_list` accepts on `/`
    /// and on a stage root.
    #[test]
    fn red_team_accepted_ignore_paths_never_uncover_protected_paths_nor_fail_later() {
        use std::os::unix::ffi::OsStringExt as _;
        let mounts = rnk_snapshot::kaniko::parse_mountinfo(
            b"1 0 8:1 / / rw - ext4 /dev/sda1 rw\n\
              2 1 0:5 / /mnt/vol rw - ext4 /dev/sdb1 rw\n",
        )
        .unwrap();
        let state = Path::new("/var/lib/ranikust");
        let stages = Path::new("/var/lib/ranikust/builds/b1/stages");
        let mut corpus: Vec<Vec<u8>> = [
            "/var/lib/ranikust/*",
            "/var/lib/[r]anikust",
            "/var/lib/ranikust/x/..",
            "/var/lib/rani*/builds",
            "/var/lib/*",
            "/mnt/vol/*",
            "/mnt/[v]ol",
            "/proc/*",
            "/sys/../dev",
            "/dev/[^n]*",
            "/tmp/apt-key-gpghome",
            "/[a-z]*",
            "/??*",
            "/*/*",
            "/\\*",
            "/[\\]]",
            "/[\\-]x",
            "/a*b?c[d-f]",
            "/ ",
            "/\t",
            "/x/*/y/**",
            "/../../etc",
            "/etc/../../..//usr/./lib",
        ]
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .collect();
        corpus.push(b"/\xff*".to_vec());
        corpus.push(b"/[\xff]*".to_vec());
        corpus.push(format!("/{}", "a".repeat(255)).into_bytes());
        corpus.push(format!("/{}", "d/".repeat(1000)).into_bytes());
        corpus.push(format!("/{}", "*".repeat(200)).into_bytes());
        let protected = [
            "var/lib/ranikust",
            "var/lib/ranikust/builds/b1/stages/0/rootfs/etc",
            "mnt/vol",
            "mnt/vol/data",
            "proc",
            "proc/1/environ",
            "sys/kernel",
            "dev",
            "dev/null",
            "ranikust/bin/ranikust",
            "etc/mtab",
            ".dockerenv",
            "run/.containerenv",
            "tmp/apt-key-gpghome/x",
        ];
        let stage_protected = [
            "proc",
            "sys",
            "dev/null",
            "ranikust",
            "etc/mtab",
            ".dockerenv",
        ];
        let mut checked = 0;
        for path in &corpus {
            let shown = String::from_utf8_lossy(path);
            let mut flag = b"--ignore-path=".to_vec();
            flag.extend_from_slice(path);
            let argv = [
                std::ffi::OsString::from("ranikust"),
                "--no-push".into(),
                std::ffi::OsString::from_vec(flag),
            ];
            let Command::Build(args) = cli::parse(argv).unwrap().cli.into_command() else {
                panic!("expected build");
            };
            let Ok(config) = BuildConfig::from_args(&args) else {
                continue; // Refused before any I/O: nothing to check.
            };
            let on_slash = rnk_snapshot::kaniko::ignore_list(
                &ignore_spec(&config, Path::new("/"), Some(state)),
                &mounts,
            )
            .unwrap_or_else(|e| panic!("{shown}: accepted early, refused late on /: {e}"));
            for p in protected {
                assert!(on_slash.covers(p), "{shown}: {p} no longer ignored on /");
            }
            let staged = stage_ignore_list(&config, stages)
                .unwrap_or_else(|e| panic!("{shown}: accepted early, refused late off /: {e}"));
            for p in stage_protected {
                assert!(
                    staged.covers(p),
                    "{shown}: {p} no longer ignored on a stage root"
                );
            }
            checked += 1;
        }
        // The corpus is mostly accepted: the checks above ran.
        assert!(checked >= corpus.len() - 2, "{checked} of {}", corpus.len());
    }
}
