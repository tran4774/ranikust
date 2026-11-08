//! The `ranikust` command-line tool.
//!
//! Parses the `build`, `prune` and `version` subcommands; `build` is the
//! default and accepts kaniko's common executor flags, plus
//! `--mode=auto|overlay|kaniko` to choose the snapshot mode (ADR-0011,
//! ADR-0023, ADR-0049). `prune` takes `--all`, required, and `--state-dir`:
//! it empties the state directory of a user other than root inside the
//! build's user namespace (`prune`, ADR-0105); with `--userns`, root
//! empties `--userns`'s state directory in its range's namespace
//! (ADR-0139). By size and age it comes in Phase 5, `push` later (Q-048);
//! there is no `login` (ADR-0049).
//!
//! Set-up order, each step before the next: parse the flags (no I/O;
//! refused kaniko flags are reported first) → the Linux 5.8 check of
//! ADR-0028 (`rnk_rootfs::probe_support`), for every subcommand but
//! `version` → for `build`, the same run again from a sealed copy of the
//! binary (ADR-0101, `rnk_sandbox::run_from_sealed_copy`), which starts
//! over from the top → logs and the opt-in metrics of `rnk-metrics`, which
//! start no thread, the report asked for with `--metrics-file` only
//! (ADR-0037) → flag validation into a `BuildConfig` → the `--seccomp`
//! profile read and compiled (ADR-0109), after the re-exec, so never by the
//! process that makes the copy → the process set-up of `setup`: a private
//! mount namespace, the probes of `rnk-sandbox` and `rnk-snapshot`, the
//! outcome, the spawner thread → hand-off.
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
mod owners;
mod prune;
mod setup;
mod startup;
mod statedir;
mod subids;
#[cfg(test)]
mod test_util;

use std::borrow::Cow;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use rnk_metrics::{Config, Outcome, Telemetry};
use rnk_sandbox::{BinaryMount, SealedCopy, Sealing};

use crate::cli::{BuildArgs, Command, ParseError, SeccompArg};
use crate::config::{BuildConfig, Seccomp};

/// The allocator of every Rust allocation: the release binary is static musl,
/// whose allocator is 250–300 times slower than mimalloc at 16 threads
/// (`exp-05` Q2). C libraries (libzstd) still call libc's `malloc`.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Hidden test hook. `panic` panics and aborts and `error` fails with exit
/// code 1, after metrics are set up; `kernel-missing`, `kernel-blocked` and
/// `kernel-hidden` make the kernel check fail; `seal-tmpfile` skips the
/// memfd way of sealing the binary and `seal-none` both ways (ADR-0101);
/// `overlay-refused` takes overlay mode's probe, once it passed, as a
/// failed probe check, which routes as NFS's refused mount does
/// ([`setup::set_up`]); `no-userns` refuses rootless overlay mode before
/// its probe as where no user namespace is usable, with that advice, so
/// that a test of the binary as a user other than root makes no user
/// namespace ([`rootless`]); `rootless-broken` fails rootless
/// overlay mode's set-up in the build's user namespace at its last step,
/// once its sandbox started, where `--mode=auto` falls back to kaniko mode
/// for root (ADR-0136). It lets the tests run the real binary through
/// these paths, including a release build, where `panic = "abort"`.
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
    /// own after the message; empty when there is none. It may name the
    /// state directory, so [`report_to`] escapes it too.
    guidance: Cow<'static, str>,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_USAGE,
            message: message.into(),
            guidance: Cow::Borrowed(""),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
            guidance: Cow::Borrowed(""),
        }
    }

    /// The set-up refused: its reason, then its guidance.
    fn refused(refusal: setup::Refusal) -> Self {
        Self::error(refusal.reason).with_guidance(refusal.guidance)
    }

    /// The build failed: with the ways out when an id its user namespace
    /// does not map is why (ADR-0098); `made` when ranikust made that
    /// namespace.
    fn built(failed: build::Failed, made: bool) -> Self {
        let guidance = failed
            .unmapped
            .as_ref()
            .map(|found| owners::guidance(found, made))
            .unwrap_or_default();
        Self::error(failed.message).with_guidance(guidance)
    }

    fn with_guidance(self, guidance: impl Into<Cow<'static, str>>) -> Self {
        Self {
            guidance: guidance.into(),
            ..self
        }
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
            ..
        }) if passed_over.is_empty() => {
            tracing::debug!(%way, ?binary, "running from a sealed copy of the binary (ADR-0101)");
        }
        Some(Sealing::Sealed {
            way,
            passed_over,
            binary,
            ..
        }) => tracing::debug!(
            %way,
            %passed_over,
            ?binary,
            "running from a sealed copy of the binary (ADR-0101)"
        ),
        Some(Sealing::Unsealed { why, binary, .. }) => tracing::warn!(
            ?binary,
            "running from the binary as installed, not from a sealed copy (ADR-0101): {why}"
        ),
        None => {}
    }
    let result = tracing::debug_span!("ranikust.run").in_scope(|| {
        run(
            &command,
            &telemetry,
            fault.as_deref(),
            metrics_file.as_deref(),
            sealing.as_ref(),
        )
    });
    let (code, outcome) = match &result {
        Ok(()) => (0, Outcome::success()),
        Err(failure) => {
            report(&failure.message, &failure.guidance);
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
/// own text, follows indented on its own line, escaped the same way: it
/// may name the state directory. One write.
fn report_to(out: &mut impl std::io::Write, message: &str, guidance: &str) -> std::io::Result<()> {
    let mut text = format!("ranikust: error: {}\n", rnk_metrics::terminal_text(message));
    if !guidance.is_empty() {
        let guidance = rnk_metrics::terminal_text(guidance);
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

/// `sealing` is how the binary ranikust started from was sealed, for
/// `build` only.
fn run(
    command: &Command,
    telemetry: &Telemetry,
    fault: Option<&str>,
    metrics_file: Option<&Path>,
    sealing: Option<&Sealing>,
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
            .in_scope(|| build(args, telemetry, metrics_file, sealing, fault)),
        Command::Prune(args) => {
            let (state_dir, pruned) = tracing::debug_span!("ranikust.prune")
                .in_scope(|| prune::run(args))
                .map_err(|failed| match failed {
                    prune::Failed::Usage(message) => Failure::usage(message),
                    prune::Failed::Refused(refusal) => Failure::refused(refusal),
                })?;
            let line = prune::summary(&state_dir, pruned);
            // As `version`: a closed stdout is no reason to fail what is done.
            let _ = writeln!(std::io::stdout(), "{}", rnk_metrics::terminal_text(&line));
            Ok(())
        }
    }
}

/// `sealing` says where the binary ranikust started from lies, and its
/// path, which a refusal of rootless overlay mode names; `fault` is the
/// test hook's value ([`DEBUG_FAULT_ENV`]).
fn build(
    args: &BuildArgs,
    telemetry: &Telemetry,
    metrics_file: Option<&Path>,
    sealing: Option<&Sealing>,
    fault: Option<&str>,
) -> Result<(), Failure> {
    let binary = sealing.map(Sealing::binary);
    let path = sealing.and_then(Sealing::binary_path);
    let refuse_overlay = fault == Some("overlay-refused");
    let config = BuildConfig::from_args(args).map_err(|invalid| Failure::usage(invalid.0))?;
    let seccomp = config::read_seccomp(config.seccomp.as_ref())
        .map_err(|invalid| Failure::usage(invalid.0))?;
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
    // Before a set-up may enter a mount namespace of its own: kaniko mode on
    // `/` counts this one as ranikust's too, held open until its scan
    // (`exposure::scan_peers`); a build that may bind `/proc` compares PID
    // 1's mounts with its own (`exposure::check_pid_namespace`).
    let started_in = exposure::mount_namespace().map(|ns| {
        if config.allow_proc_rbind {
            ns.with_mounts()
        } else {
            ns
        }
    });
    // Root's `--userns` builds in a user namespace of its `ranikust` range
    // whether or not it may mount, and is refused rather than built in a
    // mode without one (ADR-0104).
    if config.userns && rustix::process::geteuid().is_root() {
        let span = tracing::debug_span!("ranikust.setup");
        let started_in = started_in.as_ref().map_err(|&e| e);
        return rootless(
            &config,
            &seccomp,
            &span,
            started_in,
            Via::Userns(args),
            fault,
            path,
        )
        .refused();
    }
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
                &seccomp,
                KanikoBy::Flag,
                can_mount,
                runtime,
                metrics_file,
                binary,
                started_in,
            )
        }
        config::Mode::Overlay => {
            overlay_flags(args)?;
            // One span for the whole set-up: the namespace, then the rest.
            let span = tracing::debug_span!("ranikust.setup");
            // A user other than root builds in a user namespace of its own.
            if !rustix::process::geteuid().is_root() {
                let started_in = started_in.as_ref().map_err(|&e| e);
                return rootless(
                    &config,
                    &seccomp,
                    &span,
                    started_in,
                    Via::Overlay,
                    fault,
                    path,
                )
                .refused();
            }
            let refusal = match span.in_scope(setup::enter_namespace) {
                Ok(()) => {
                    // Only kaniko mode on `/` and a build that binds `/proc`
                    // look at it, never reached here.
                    drop(started_in);
                    return overlay(&config, &seccomp, &span, refuse_overlay);
                }
                Err(refusal) => refusal,
            };
            if !refusal.needs_kaniko_mode() {
                return Err(Failure::refused(refusal));
            }
            // Root may not mount: rootless overlay mode, where ADR-0107 lets
            // it run; else the refusal stands, with why it did not run.
            let why = match rootless_kept_off() {
                Some(off) => off,
                None => {
                    let started_in = started_in.as_ref().map_err(|&e| e);
                    match rootless(
                        &config,
                        &seccomp,
                        &span,
                        started_in,
                        Via::Overlay,
                        fault,
                        path,
                    ) {
                        Rootless::Built(built) => return built,
                        Rootless::NotHere(not_here) => not_here.refusal.to_string(),
                        // Never: only `--mode=auto` falls back.
                        Rootless::FellBack(reason) => reason,
                    }
                }
            };
            Err(Failure::refused(setup::Refusal {
                reason: format!("{}\n  {why}", refusal.reason),
                ..refusal
            }))
        }
        // Whether root may mount decides, before anything is written
        // (ADR-0040).
        config::Mode::Auto => {
            let span = tracing::debug_span!("ranikust.setup");
            // A user other than root builds in a user namespace of its own.
            if !rustix::process::geteuid().is_root() {
                let started_in = started_in.as_ref().map_err(|&e| e);
                return rootless(
                    &config,
                    &seccomp,
                    &span,
                    started_in,
                    Via::AutoUser(args),
                    fault,
                    path,
                )
                .refused();
            }
            match span.in_scope(setup::enter_namespace) {
                Ok(()) => {
                    // Only kaniko mode on `/` and a build that binds `/proc`
                    // look at it, never reached here.
                    drop(started_in);
                    // Root may mount: overlay mode if its probes pass. Where
                    // overlayfs is unusable (a state directory on NFS or on
                    // overlayfs) outside a container, kaniko mode keeps a
                    // directory per stage and the full sandbox (ADR-0059,
                    // `design/modes-and-detection.md`, `setup::route`).
                    // Overlay mode's flags are checked before its set-up
                    // writes anything, and count only if it is picked.
                    let flags = config::overlay_mode_flags(args);
                    let mut userns_probe = true;
                    let refusal = match span.in_scope(|| {
                        setup::set_up(
                            &config.state_dir,
                            config.allow_network_none_userns,
                            &seccomp,
                            &mut userns_probe,
                            config.allow_proc_rbind,
                            refuse_overlay,
                        )
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
                            return overlay_built(&config, setup, build::Stops::Own);
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
                            // Never read: off `/`, or with `can_mount`, no scan runs.
                            let started_in = Err(rustix::io::Errno::INVAL);
                            // One probe line per build.
                            let by = if userns_probe {
                                KanikoBy::Auto
                            } else {
                                KanikoBy::AutoProbed
                            };
                            kaniko(
                                &config,
                                &seccomp,
                                by,
                                true,
                                runtime,
                                metrics_file,
                                binary,
                                started_in,
                            )
                        }
                    }
                }
                Err(refusal) if refusal.needs_kaniko_mode() => {
                    let why = refusal.reason;
                    // Root may not mount: rootless overlay mode first, where
                    // ADR-0107 lets it run and no flag needs kaniko mode,
                    // then kaniko mode, as before it.
                    let by = match kaniko_only_kept_off(args).or_else(rootless_kept_off) {
                        Some(off) => {
                            tracing::info!("ranikust: {off}");
                            KanikoBy::Auto
                        }
                        None => {
                            let started_in = started_in.as_ref().map_err(|&e| e);
                            let via = Via::AutoRoot(args);
                            match rootless(&config, &seccomp, &span, started_in, via, fault, path) {
                                Rootless::Built(built) => return built,
                                Rootless::NotHere(not_here) => {
                                    tracing::info!("ranikust: {}", not_here.refusal);
                                    // One probe line per build.
                                    if not_here.probed {
                                        KanikoBy::AutoProbed
                                    } else {
                                        KanikoBy::Auto
                                    }
                                }
                                Rootless::FellBack(reason) => {
                                    // A step its probe passed failed.
                                    tracing::warn!(
                                        "ranikust: {reason}; kaniko mode instead, before \
                                         anything was built (ADR-0136)"
                                    );
                                    KanikoBy::AutoProbed
                                }
                            }
                        }
                    };
                    tracing::info!("kaniko mode, because root may not mount here: {why}");
                    let runtime = container::detect(&container::Real);
                    kaniko(
                        &config,
                        &seccomp,
                        by,
                        false,
                        runtime,
                        metrics_file,
                        binary,
                        started_in,
                    )
                }
                Err(refusal) => Err(Failure::refused(refusal)),
            }
        }
    }
}

/// What [`rootless`] did.
enum Rootless {
    /// The run's result: built, or failed once rootless overlay mode was
    /// chosen (a refused peer scan, the overlay flags, a failure in the
    /// build's user namespace).
    Built(Result<(), Failure>),
    /// Rootless overlay mode cannot run here; the process is unchanged.
    NotHere(setup::rootless::NotHere),
    /// [`Via::AutoRoot`]: the set-up failed in the build's user namespace,
    /// in the child that entered it, for this reason; the store it opened
    /// in the state directory is emptied where it holds no data, and this
    /// process is as it was before rootless overlay mode, its probe line
    /// logged (ADR-0136).
    FellBack(String),
}

impl Rootless {
    /// The run's result where nothing may follow rootless overlay mode: a
    /// user other than root, or root's `--userns`.
    fn refused(self) -> Result<(), Failure> {
        match self {
            Self::Built(built) => built,
            Self::NotHere(not_here) => Err(Failure::refused(not_here.refusal)),
            // Never: only root's `--mode=auto` falls back.
            Self::FellBack(reason) => Err(Failure::error(reason)),
        }
    }
}

/// How [`rootless`] was reached.
#[derive(Clone, Copy)]
enum Via<'a> {
    /// `--mode=overlay`, whose flags were checked first.
    Overlay,
    /// `--mode=auto` as a user other than root, with its flags.
    AutoUser(&'a BuildArgs),
    /// `--mode=auto` as root that may not mount, with its flags: kaniko
    /// mode follows where the set-up fails in the build's user namespace
    /// (ADR-0136).
    AutoRoot(&'a BuildArgs),
    /// `--userns` as root, with its flags, in either mode it allows: the
    /// `ranikust` range, never another mode (ADR-0104).
    Userns(&'a BuildArgs),
}

impl Via<'_> {
    /// The flag, for messages.
    fn flag(self) -> &'static str {
        match self {
            Self::Overlay => "--mode=overlay",
            Self::AutoUser(_) | Self::AutoRoot(_) => "--mode=auto",
            Self::Userns(_) => "--userns",
        }
    }

    /// Whether a set-up that fails in the build's user namespace falls
    /// back to kaniko mode: root's `--mode=auto` only, which had kaniko
    /// mode before rootless overlay mode; never a user other than root,
    /// whom kaniko mode refuses, nor `--mode=overlay` (ADR-0136), nor
    /// `--userns`, whose build kaniko mode would run as the host's root
    /// (ADR-0104).
    fn falls_back(self) -> bool {
        matches!(self, Self::AutoRoot(_))
    }
}

/// Why `--mode=auto`, as root that may not mount, does not try rootless
/// overlay mode: a flag only kaniko mode honours, which overlay mode refuses
/// and kaniko mode, the mode such a build got before rootless overlay mode,
/// builds with (ADR-0049).
fn kaniko_only_kept_off(args: &BuildArgs) -> Option<String> {
    config::kaniko_only_flag(args).map(|flag| {
        format!(
            "rootless overlay mode is not tried: {flag} takes effect in kaniko mode only \
             (ADR-0049)"
        )
    })
}

/// Why root that may not mount does not try rootless overlay mode: a
/// writable `/proc/sys`, or one that cannot be told, where a `RUN` as the
/// map's uid 0, the host's root unless ranikust runs in a user namespace,
/// could write it (ADR-0107, case (ii), and ADR-0054).
fn rootless_kept_off() -> Option<String> {
    match exposure::proc_sys_read_only(Path::new("/proc/sys")) {
        Ok(true) => None,
        Ok(false) => Some(
            "rootless overlay mode is not tried as root where /proc/sys is writable (ADR-0107)"
                .to_owned(),
        ),
        Err(e) => Some(format!(
            "rootless overlay mode is not tried as root where it cannot tell whether /proc/sys \
             is read-only ({e}) (ADR-0107)"
        )),
    }
}

/// Rootless overlay mode (ADR-0104, ADR-0107): the user namespace probe
/// ([`setup::rootless::plan`]); then, where the build binds `/proc`
/// (`--allow proc.rbind`), the peer scan, refused as kaniko mode on `/`
/// refuses it (ADR-0129, ADR-0130), the check of the PID namespace from
/// files with no ptrace check (ADR-0134), and the warnings for what the
/// rbind exposes, all before the build's user namespace, where ranikust
/// could no longer open the other processes; then overlay mode's flags, where
/// `via` gives them (`--mode=auto`; `--mode=overlay` checked them first,
/// and `--userns` before the probe, so that its usage error comes before
/// any write); then the set-up in the namespace and the build. Where `via`
/// falls back ([`Via::falls_back`]), a child enters the namespace, sets up
/// and builds, and this process waits outside it
/// ([`rnk_sandbox::fork_to_wait`]): it returns only where the child's
/// set-up failed ([`Rootless::FellBack`]), else ends as the child did.
/// `started_in` as for [`kaniko`]; `fault` is the test hook's value
/// ([`DEBUG_FAULT_ENV`]: `no-userns`, `rootless-broken`); `binary` the
/// path ranikust started from ([`Sealing::binary_path`]), which the
/// refusal of a user with no user namespace weighs.
fn rootless(
    config: &BuildConfig,
    seccomp: &Seccomp,
    span: &tracing::Span,
    started_in: rustix::io::Result<&exposure::MountNamespace>,
    via: Via<'_>,
    fault: Option<&str>,
    binary: Option<&Path>,
) -> Rootless {
    let how = via.flag();
    // `--userns` has no other mode to try: its usage error precedes any write.
    if let Via::Userns(args) = via
        && let Err(failure) = overlay_flags(args)
    {
        return Rootless::Built(Err(failure));
    }
    let userns = matches!(via, Via::Userns(_));
    let plan = match span.in_scope(|| {
        if fault == Some("no-userns") {
            let euid = rustix::process::geteuid().as_raw();
            return Err(setup::rootless::refused_by_hook(
                DEBUG_FAULT_ENV,
                euid,
                userns,
                binary,
            ));
        }
        setup::rootless::plan(&config.state_dir, config.allow_proc_rbind, userns, binary)
    }) {
        Ok(plan) => plan,
        Err(not_here) => return Rootless::NotHere(not_here),
    };
    let scanned = plan.proc_rbind();
    if scanned {
        let scan = exposure::scan_peers(started_in);
        if let Err(failure) = refuse_reachable_peers(how, exposure::Reach::ProcRbind, scan)
            .and_then(|()| {
                refuse_shared_pid_namespace(how, exposure::check_pid_namespace(started_in))
            })
        {
            return Rootless::Built(Err(failure));
        }
        exposure::warn_build_args(&config.build_args, exposure::Reach::ProcRbind);
    }
    if let Via::AutoUser(args) | Via::AutoRoot(args) = via
        && let Err(failure) = overlay_flags(args)
    {
        return Rootless::Built(Err(failure));
    }
    let child = if via.falls_back() {
        match span.in_scope(rnk_sandbox::fork_to_wait) {
            Ok(rnk_sandbox::Forked::Child(child)) => Some(child),
            Ok(rnk_sandbox::Forked::Parent(waiting)) => {
                // The child holds the state directory until its store is
                // open; a copy held here would keep it from undoing it.
                drop(plan);
                return Rootless::FellBack(waiting.wait());
            }
            Err(e) => {
                return Rootless::NotHere(setup::rootless::NotHere {
                    refusal: setup::Refusal {
                        kind: setup::Kind::Other,
                        reason: format!(
                            "rootless overlay mode is not tried: the process that would wait \
                             outside the build's user namespace cannot be forked: {e}"
                        ),
                        guidance: "".into(),
                    },
                    probed: true,
                });
            }
        }
    } else {
        None
    };
    let then = setup::rootless::Then {
        userns_network: config.allow_network_none_userns,
        child: child.as_ref(),
        broken_by: (fault == Some("rootless-broken")).then_some(DEBUG_FAULT_ENV),
        binary,
    };
    let setup =
        span.in_scope(|| setup::rootless::set_up(&config.state_dir, plan, scanned, seccomp, &then));
    match (setup, child) {
        (Ok(setup), Some(child)) => match child.proceed() {
            Ok(()) => Rootless::Built(overlay_built(config, setup, build::Stops::Forwarded)),
            Err(e) => {
                close_store(setup.store);
                Rootless::Built(Err(Failure::error(format!("rootless overlay mode: {e}"))))
            }
        },
        (Ok(setup), None) => Rootless::Built(overlay_built(config, setup, build::Stops::Own)),
        (Err(refusal), Some(child)) => child.fall_back(&refusal.reason),
        (Err(refusal), None) if refusal.kind == setup::Kind::Broken => {
            Rootless::Built(Err(Failure::refused(refusal)))
        }
        (Err(refusal), None) => Rootless::NotHere(setup::rootless::NotHere {
            refusal,
            probed: true,
        }),
    }
}

/// How [`kaniko`] was reached: `--mode=kaniko`, or picked by
/// `--mode=auto`, which also runs the user-namespace probe, reported only,
/// unless overlay mode's set-up ran it before it was refused.
#[derive(Clone, Copy)]
enum KanikoBy {
    Flag,
    Auto,
    AutoProbed,
}

impl KanikoBy {
    /// The flag, for messages.
    fn flag(self) -> &'static str {
        match self {
            Self::Flag => "--mode=kaniko",
            Self::Auto | Self::AutoProbed => "--mode=auto",
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
/// then the build. `refuse_overlay` is the test hook's `overlay-refused`.
fn overlay(
    config: &BuildConfig,
    seccomp: &Seccomp,
    span: &tracing::Span,
    refuse_overlay: bool,
) -> Result<(), Failure> {
    let setup = span
        .in_scope(|| {
            setup::set_up(
                &config.state_dir,
                config.allow_network_none_userns,
                seccomp,
                &mut false,
                config.allow_proc_rbind,
                refuse_overlay,
            )
        })
        .map_err(Failure::refused)?;
    overlay_built(config, setup, build::Stops::Own)
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

/// Overlay mode's build, once set up, stopped by `stops`.
fn overlay_built(
    config: &BuildConfig,
    setup: setup::Setup,
    stops: build::Stops,
) -> Result<(), Failure> {
    tracing::debug!(state_dir = %setup.state_dir.display(), "set-up done");
    tracing::info!("set up for {}", setup.outcome);
    let made = matches!(setup.outcome, setup::Outcome::OverlayRootless { .. });
    let (setup::Outcome::OverlayRoot { overlay, .. }
    | setup::Outcome::OverlayRootless { overlay, .. }) = setup.outcome;
    let built = build::run(
        config,
        setup.store,
        setup.sandbox,
        build::Snapshots::Overlay(overlay, stops),
    )
    .map_err(|f| Failure::built(f, made))?;
    report_built(&built)
}

/// Logs the built image and prints its digest. The digest is the build's
/// result: failing to print it fails the run.
fn report_built(built: &build::Built) -> Result<(), Failure> {
    tracing::info!("built image {}", built.digest);
    writeln!(std::io::stdout(), "{}", built.digest)
        .map_err(|e| Failure::error(format!("printing the image digest: {e}")))
}

/// Kaniko mode, reached as `by` says (ADR-0050): in a container (kaniko's
/// heuristic, ADR-0048) the root is `/`, and the stages run one unit at a
/// time on it (`design/kaniko-mode.md#stage-roots-on-the-shared-root`);
/// anywhere else each stage gets a directory of its own in the state
/// directory, `<state>/builds/<id>/stages/<n>/rootfs`, scheduled as
/// overlay mode's stages (`#per-stage-directories`, ADR-0059). Then
/// `RLIMIT_NOFILE` is raised (the original kept for `RUN`), the state
/// directory and store set up, and the ignore list built. `by` names the
/// flag in messages and says whether the user-namespace probe runs here
/// ([`KanikoBy`]); `seccomp` is the profile of a full-sandbox `RUN`;
/// `can_mount` is whether the build's
/// mount namespace could be entered; `runtime` is what
/// [`container::detect`] found, read once by the caller; `binary` where the
/// binary ranikust started from lies; `started_in` the mount namespace
/// ranikust started in ([`exposure::mount_namespace`]), read only on `/`
/// where root may not mount ([`refuse_unsafe_slash`]), closed before the
/// set-up.
#[expect(
    clippy::too_many_arguments,
    reason = "the build's settings, the route the caller took and what kaniko mode on `/` weighs"
)]
fn kaniko(
    config: &BuildConfig,
    seccomp: &Seccomp,
    by: KanikoBy,
    can_mount: bool,
    runtime: Option<container::Runtime>,
    metrics_file: Option<&Path>,
    binary: Option<BinaryMount>,
    started_in: rustix::io::Result<exposure::MountNamespace>,
) -> Result<(), Failure> {
    let how = by.flag();
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
        let started_in = started_in.as_ref().map_err(|&e| e);
        refuse_unsafe_slash(config, how, can_mount, binary, started_in)?;
    }
    // Only the scan needs it.
    drop(started_in);
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
    let userns_probe = match by {
        KanikoBy::Auto => true,
        KanikoBy::Flag | KanikoBy::AutoProbed => false,
    };
    let setup = tracing::debug_span!("ranikust.setup")
        .in_scope(|| {
            setup::set_up_kaniko(
                &config.state_dir,
                on_slash,
                can_mount,
                nofile,
                seccomp,
                userns_probe,
                config.allow_proc_rbind,
            )
        })
        .map_err(Failure::refused)?;
    if on_slash {
        // Once the set-up left ranikust only what a `RUN` keeps (ADR-0078),
        // so that it reads environments as a root `RUN` would.
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
    let built =
        build::run(config, store, sandbox, snapshots).map_err(|f| Failure::built(f, false))?;
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
    started_in: rustix::io::Result<&exposure::MountNamespace>,
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
    if let Some(arg) = &config.seccomp {
        // A `RUN` there runs `Direct`, under the container's own profile
        // (ADR-0103, ADR-0109).
        let value = match arg {
            SeccompArg::Default => "default".to_owned(),
            SeccompArg::Unconfined => "unconfined".to_owned(),
            SeccompArg::File(path) => path.display().to_string(),
        };
        return Err(Failure::usage(format!(
            "{how}: --seccomp={value}: kaniko mode on / runs RUN directly, under the \
             container's own seccomp profile (ADR-0103); drop the flag, or give the container \
             the profile (docker run --security-opt seccomp=FILE)"
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
    refuse_reachable_peers(
        how,
        exposure::Reach::Slash,
        exposure::scan_peers(started_in),
    )?;
    // What ranikust cannot close itself (ADR-0051, B4 and B5; ADR-0078).
    // The processes holding credentials are looked for after the set-up,
    // with the capabilities a `RUN` keeps (`kaniko`).
    exposure::warn_build_args(&config.build_args, exposure::Reach::Slash);
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

/// Kaniko mode on `/`, and a build that binds `/proc` (`reach`), refuse a
/// PID namespace shared with a process of another mount namespace that
/// ranikust can open through `/proc` when it starts (ADR-0101, ADR-0129):
/// a `RUN` shares that PID namespace and could reach that process's files,
/// which the container's own mount table does not show, or, through the
/// rbind, read its command line. A scan that cannot tell, or one of too
/// many threads, is refused too, each with its own guidance.
fn refuse_reachable_peers(
    how: &str,
    reach: exposure::Reach,
    scan: exposure::PeerScan,
) -> Result<(), Failure> {
    use exposure::PeerScan;
    let subject = reach.build();
    match scan {
        PeerScan::None => Ok(()),
        PeerScan::Reachable(process) => Err(Failure::error(format!(
            "{how}: {subject} sharing its PID namespace with process {process}, of another \
             mount namespace, whose files ranikust reaches through /proc/{}, would let {}",
            process.pid,
            match reach {
                exposure::Reach::Slash => "RUN reach that process's files".to_owned(),
                exposure::Reach::ProcRbind => format!(
                    "every RUN read that process's {}",
                    exposure::UNCHECKED_PROC_FILES.trim_start_matches("the ")
                ),
            }
        ))
        .with_guidance(
            "do not share the build container's PID namespace with another container: no \
             Kubernetes shareProcessNamespace: true or hostPID: true on its pod, no Docker or \
             podman --pid container:<name> or --pid=host (ADR-0101)",
        )),
        PeerScan::TooMany => Err(Failure::error(format!(
            "{how}: {subject} sharing its PID namespace with more than {} threads; \
             ranikust refuses to build without looking at each for one of another mount \
             namespace that a RUN could reach",
            exposure::PEER_SCAN_LIMIT
        ))
        .with_guidance(
            "build in a container with a PID namespace of its own (no Kubernetes hostPID: true \
             or shareProcessNamespace: true, no --pid=host or --pid container:<name>), where \
             fewer threads run (ADR-0101)",
        )),
        PeerScan::Unknown(process, step, e) => Err(Failure::error(format!(
            "{how}: cannot tell whether process {process} is of another mount namespace that \
             ranikust reaches through /proc: {step} failed ({e}); {subject} refuses to \
             build without knowing"
        ))
        .with_guidance(
            "let the container's seccomp and LSM profiles answer opening /proc/<pid>, \
             /proc/<pid>/root, /proc/<pid>/task, /proc/<pid>/task/<tid> and \
             /proc/<pid>/task/<tid>/root with success, EACCES or EPERM and nothing else, allow \
             reading the /proc/<pid>/task they open, opening ns/mnt with O_PATH and fstat on \
             it, and share no PID namespace with another container (ADR-0101)",
        )),
        PeerScan::Unlisted(e) => Err(Failure::error(format!(
            "{how}: cannot list the processes of its PID namespace in /proc ({e}); {subject} \
             refuses to build without knowing whether one of another mount namespace is in \
             reach"
        ))
        .with_guidance("mount /proc in the container, readable by root (ADR-0101)")),
        PeerScan::OwnNamespace(e) => Err(Failure::error(format!(
            "{how}: cannot read ranikust's own mount namespace, /proc/self/ns/mnt ({e}); {subject} \
             refuses to build without knowing which processes are of another mount \
             namespace"
        ))
        .with_guidance(
            "mount /proc in the container, and let its seccomp profile allow opening \
             /proc/self/ns/mnt with O_PATH and fstat (ADR-0101)",
        )),
    }
}

/// A build that binds `/proc` refuses a PID namespace that files with no
/// ptrace check show shared with another mount namespace, or cannot tell
/// (ADR-0134): through the rbind every `RUN` reads those files of each
/// process there, whether or not ranikust may open the process.
fn refuse_shared_pid_namespace(how: &str, found: exposure::PidNamespace) -> Result<(), Failure> {
    use exposure::PidNamespace;
    let subject = exposure::Reach::ProcRbind.build();
    let reads = exposure::UNCHECKED_PROC_FILES;
    let guidance = "give the build container a PID namespace of its own: no Kubernetes \
         hostPID: true or shareProcessNamespace: true on its pod, no Docker or podman \
         --pid=host or --pid container:<name>, and start ranikust in the container's mount \
         namespace (ADR-0134)";
    match found {
        PidNamespace::Own => Ok(()),
        PidNamespace::Host(process) => Err(Failure::error(format!(
            "{how}: {subject} in the host's PID namespace (process {process} is a kernel \
             thread) would let every RUN read {reads} of each process of the host"
        ))
        .with_guidance(guidance)),
        PidNamespace::OtherInit(process) => Err(Failure::error(format!(
            "{how}: {subject} sharing its PID namespace with PID 1, process {process}, of \
             another mount namespace (its /proc/1/mountinfo lists no mount of ranikust's) \
             would let every RUN read {reads} of each process there"
        ))
        .with_guidance(guidance)),
        PidNamespace::Unknown(step, e) => Err(Failure::error(format!(
            "{how}: cannot tell whether {subject} shares its PID namespace with another mount \
             namespace: {step} failed ({e}); it refuses to build without knowing"
        ))
        .with_guidance(
            "mount /proc in the container, let ranikust read /proc/self/mountinfo, \
             /proc/1/mountinfo and /proc/2/stat, and give the build container a PID namespace \
             of its own (ADR-0134)",
        )),
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
        report_to(&mut out, &failure.message, &failure.guidance).unwrap();
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
            guidance: "pass --state-dir on a local filesystem".into(),
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

    /// Guidance that names the state directory prints it on the guidance
    /// line, escaped as the message is: one line, nothing a terminal acts on.
    #[test]
    fn guidance_naming_a_path_is_escaped() {
        let failure = Failure::error("refused").with_guidance(format!(
            "remove {}",
            Path::new("/s/\x1b]0;x\x07\nINFO").display()
        ));
        assert_eq!(
            reported(&failure),
            "ranikust: error: refused\n  remove /s/\\u{1b}]0;x\\u{7}\\nINFO\n"
        );
    }

    /// Red team (ADR-0098, T16): a base layer of many owners the namespace
    /// does not map, under names of terminal escapes, line breaks and bidi
    /// controls, gives a refusal of two lines, escaped, that names sixteen.
    #[test]
    fn red_team_unmapped_owners_under_hostile_names_reach_the_terminal_escaped() {
        let hostile = "\x1b]0;RNK\x07\n\x1b[2J\r\u{202e}\u{9b}";
        let mut tar = tar::Builder::new(Vec::new());
        for i in 0..40_u64 {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o755);
            h.set_uid(3_000_000_000 + i);
            h.set_gid(3_000_000_000 + i);
            h.set_size(0);
            let name = format!("{i}{hostile}{}/", "x".repeat(200));
            tar.append_data(&mut h, name, std::io::empty()).unwrap();
        }
        let tar = tar.into_inner().unwrap();
        let ids = rnk_rootfs::MappedIds::parse("0 0 1\n", "0 0 1\n").unwrap();
        let mut found = rnk_rootfs::Unmapped::owners(ids);
        rnk_rootfs::scan_owners(tar.as_slice(), &mut found).unwrap();
        let e = rnk_rootfs::Error::Unmapped(found.clone());
        let failed = build::Failed {
            message: format!("stage base: preparing: {e}"),
            unmapped: Some(found),
        };
        let out = reported(&Failure::built(failed, true));
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(out.ends_with('\n'));
        assert!(
            out.lines()
                .flat_map(str::chars)
                .all(|c| !c.is_control() && c != '\u{202e}'),
            "{out:?}"
        );
        assert_eq!(
            out.matches("\\u{1b}]0;RNK\\u{7}\\n\\u{1b}[2J\\r\\u{202e}\\u{9b}x")
                .count(),
            16
        );
        assert!(
            out.contains(", and 32 more entries with other ids;"),
            "{out}"
        );
        assert!(out.len() < 16 * 6 * 260 + 1024, "{} bytes", out.len());
        let guidance = out.lines().nth(1).unwrap();
        assert!(
            guidance.starts_with("  ") && !guidance.contains("RNK"),
            "{guidance}"
        );
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
    /// (ADR-0054) before `--seccomp` (ADR-0103, a usage error) before
    /// `--network=none` (ADR-0057). All return before /proc/sys is read, so
    /// this needs no file and no privilege.
    #[test]
    fn the_refusals_on_slash_come_in_their_order() {
        // Never read: every refusal here comes before the scan.
        let started_in = Err(rustix::io::Errno::NOENT);
        let both = config_of(&["--no-push", "--network=none", "--seccomp=unconfined"]);
        let may_mount =
            refuse_unsafe_slash(&both, "--mode=kaniko", true, None, started_in).unwrap_err();
        assert!(
            may_mount
                .message
                .contains("in a container where root may mount"),
            "{}",
            may_mount.message
        );
        let seccomp =
            refuse_unsafe_slash(&both, "--mode=kaniko", false, None, started_in).unwrap_err();
        assert_eq!(seccomp.code, EXIT_USAGE, "{}", seccomp.message);
        assert!(
            seccomp
                .message
                .contains("--seccomp=unconfined: kaniko mode on / runs RUN directly"),
            "{}",
            seccomp.message
        );
        for value in ["default", "/p.json"] {
            let config = config_of(&["--no-push", &format!("--seccomp={value}")]);
            let refused =
                refuse_unsafe_slash(&config, "--mode=kaniko", false, None, started_in).unwrap_err();
            assert_eq!(refused.code, EXIT_USAGE, "{value}");
            assert!(
                refused.message.contains(&format!("--seccomp={value}:")),
                "{value}"
            );
        }
        let config = config_of(&["--no-push", "--network=none"]);
        let may_mount =
            refuse_unsafe_slash(&config, "--mode=kaniko", true, None, started_in).unwrap_err();
        assert!(
            may_mount
                .message
                .contains("in a container where root may mount"),
            "{}",
            may_mount.message
        );
        let no_mount =
            refuse_unsafe_slash(&config, "--mode=kaniko", false, None, started_in).unwrap_err();
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

    #[test]
    fn kaniko_mode_on_slash_refuses_a_reachable_process_of_another_mount_namespace() {
        use exposure::{PeerScan, Process, Reach};
        let how = "--mode=kaniko";
        refuse_reachable_peers(how, Reach::Slash, PeerScan::None).unwrap();
        let sleep = || Process {
            pid: "42".into(),
            comm: "sleep".into(),
        };
        let reached =
            refuse_reachable_peers(how, Reach::Slash, PeerScan::Reachable(sleep())).unwrap_err();
        assert!(
            reached.code == 1
                && reached.message.starts_with(
                    "--mode=kaniko: kaniko mode on / sharing its PID namespace with process 42 \
                     (sleep), of another mount namespace, whose files ranikust reaches through \
                     /proc/42,"
                )
                && reached
                    .message
                    .ends_with("would let RUN reach that process's files")
                && reached
                    .guidance
                    .starts_with("do not share the build container's PID namespace")
                && reached.guidance.contains("shareProcessNamespace: true")
                && reached.guidance.contains("hostPID: true")
                && reached.guidance.contains("--pid container:<name>")
                && reached.guidance.contains("--pid=host"),
            "{}: {}",
            reached.message,
            reached.guidance
        );
        let errno = rustix::io::Errno::LOOP;
        // Each cause has its own message and guidance, and exits as every
        // refusal does.
        let mut guidance = Vec::new();
        for (scan, message, says) in [
            (
                PeerScan::TooMany,
                format!("more than {} threads;", exposure::PEER_SCAN_LIMIT),
                "fewer threads run",
            ),
            (
                PeerScan::Unknown(sleep(), "opening /proc/<pid>/task/<tid>/root", errno),
                format!(
                    "cannot tell whether process 42 (sleep) is of another mount namespace that \
                     ranikust reaches through /proc: opening /proc/<pid>/task/<tid>/root failed \
                     ({errno})"
                ),
                "/proc/<pid>/root, /proc/<pid>/task, /proc/<pid>/task/<tid> and \
                 /proc/<pid>/task/<tid>/root with success, EACCES or EPERM and nothing else, \
                 allow reading",
            ),
            (
                PeerScan::Unlisted(errno),
                format!("cannot list the processes of its PID namespace in /proc ({errno})"),
                "mount /proc",
            ),
            (
                PeerScan::OwnNamespace(errno),
                format!("cannot read ranikust's own mount namespace, /proc/self/ns/mnt ({errno})"),
                "/proc/self/ns/mnt",
            ),
        ] {
            let refused = refuse_reachable_peers(how, Reach::Slash, scan).unwrap_err();
            assert!(
                refused.code == 1
                    && refused.message.starts_with("--mode=kaniko: ")
                    && refused.message.contains(&message)
                    && refused.guidance.contains(says)
                    && refused.guidance.ends_with("(ADR-0101)"),
                "{}: {}",
                refused.message,
                refused.guidance
            );
            guidance.push(refused.guidance);
        }
        // A process whose name could not be read is named by its pid.
        let unnamed = Process {
            pid: "42".into(),
            comm: String::new(),
        };
        let refused = refuse_reachable_peers(
            how,
            Reach::Slash,
            PeerScan::Unknown(unnamed, "opening /proc/<pid>", errno),
        )
        .unwrap_err();
        assert!(
            refused
                .message
                .contains("whether process 42 is of another mount namespace"),
            "{}",
            refused.message
        );
        guidance.push(reached.guidance);
        guidance.sort();
        guidance.dedup();
        assert_eq!(guidance.len(), 5);
    }

    /// ADR-0129, ADR-0130: a build with an rbind of `/proc` refuses a
    /// shared PID namespace as kaniko mode on `/` does, naming what the
    /// rbind exposes.
    #[test]
    fn a_build_with_an_rbind_of_proc_refuses_a_reachable_process_of_another_mount_namespace() {
        use exposure::{PeerScan, Process, Reach};
        let how = "--mode=auto";
        refuse_reachable_peers(how, Reach::ProcRbind, PeerScan::None).unwrap();
        let sleep = Process {
            pid: "42".into(),
            comm: "sleep".into(),
        };
        let reached =
            refuse_reachable_peers(how, Reach::ProcRbind, PeerScan::Reachable(sleep)).unwrap_err();
        assert!(
            reached.code == 1
                && reached.message.starts_with(
                    "--mode=auto: a build with an rbind of /proc (--allow proc.rbind) sharing \
                     its PID namespace with process 42 (sleep),"
                )
                && reached.message.ends_with(
                    "would let every RUN read that process's files of /proc/<pid> not under \
                     ptrace's check, such as cmdline, status, stat, mountinfo, cgroup and net/*"
                )
                && reached
                    .guidance
                    .starts_with("do not share the build container's PID namespace"),
            "{}: {}",
            reached.message,
            reached.guidance
        );
        let errno = rustix::io::Errno::LOOP;
        let unlisted =
            refuse_reachable_peers(how, Reach::ProcRbind, PeerScan::Unlisted(errno)).unwrap_err();
        assert!(
            unlisted
                .message
                .contains("; a build with an rbind of /proc (--allow proc.rbind) refuses"),
            "{}",
            unlisted.message
        );
    }

    /// ADR-0134: a build with an rbind of `/proc` refuses the host's PID
    /// namespace, PID 1 of another mount namespace, and a check that
    /// cannot tell, naming what every `RUN` would read.
    #[test]
    fn a_build_with_an_rbind_of_proc_refuses_a_pid_namespace_shared_as_unchecked_files_show() {
        use exposure::{PidNamespace, Process};
        let how = "--mode=overlay";
        refuse_shared_pid_namespace(how, PidNamespace::Own).unwrap();
        let process = |pid: &str, comm: &str| Process {
            pid: pid.into(),
            comm: comm.into(),
        };
        let host = refuse_shared_pid_namespace(how, PidNamespace::Host(process("2", "kthreadd")))
            .unwrap_err();
        let other =
            refuse_shared_pid_namespace(how, PidNamespace::OtherInit(process("1", "systemd")))
                .unwrap_err();
        for (failure, what) in [
            (
                &host,
                "in the host's PID namespace (process 2 (kthreadd) is a kernel thread)",
            ),
            (
                &other,
                "with PID 1, process 1 (systemd), of another mount namespace",
            ),
        ] {
            assert!(
                failure.code == 1
                    && failure.message.starts_with(
                        "--mode=overlay: a build with an rbind of /proc (--allow proc.rbind) "
                    )
                    && failure.message.contains(what)
                    && failure.message.contains(
                        "the files of /proc/<pid> not under ptrace's check, such as cmdline, \
                         status, stat, mountinfo, cgroup and net/*"
                    )
                    && failure.guidance.contains("PID namespace of its own")
                    && failure.guidance.contains("--pid=host"),
                "{}: {}",
                failure.message,
                failure.guidance
            );
        }
        let unknown = refuse_shared_pid_namespace(
            how,
            PidNamespace::Unknown("reading /proc/1/mountinfo", rustix::io::Errno::ACCESS),
        )
        .unwrap_err();
        assert!(
            unknown.code == 1
                && unknown
                    .message
                    .contains("reading /proc/1/mountinfo failed (Permission denied")
                && unknown
                    .message
                    .ends_with("it refuses to build without knowing"),
            "{}",
            unknown.message
        );
    }

    /// The test hook's refusal leaves the process unchanged and gives a
    /// user other than root the advice for no user namespace, root nothing
    /// more, root's `--userns` its own, named.
    #[test]
    fn the_no_userns_hook_refuses_rootless_overlay_mode_before_its_probe() {
        let as_root = setup::rootless::refused_by_hook(DEBUG_FAULT_ENV, 0, false, None);
        assert_eq!(as_root.refusal.guidance, "");
        let userns = setup::rootless::refused_by_hook(DEBUG_FAULT_ENV, 0, true, None).refusal;
        assert!(
            userns.reason.starts_with("--userns cannot run here"),
            "{}",
            userns.reason
        );
        assert!(userns.guidance.starts_with("--userns needs"));
        assert!(!userns.guidance.contains("AppArmor"));
        let not_here = setup::rootless::refused_by_hook(DEBUG_FAULT_ENV, 1000, false, None);
        assert!(!not_here.probed);
        assert_eq!(not_here.refusal.kind, setup::Kind::Other);
        assert!(
            not_here
                .refusal
                .reason
                .ends_with("refused by RANIKUST_DEBUG_FAULT=no-userns"),
            "{}",
            not_here.refusal.reason
        );
        assert!(not_here.refusal.guidance.contains("sudo"));
    }

    /// Under `--mode=auto`, root that may not mount keeps kaniko mode for a
    /// flag only kaniko mode honours, where rootless overlay mode would
    /// refuse it; a flag overlay mode only warns about tries it still.
    #[test]
    fn auto_mode_keeps_kaniko_mode_for_a_flag_only_it_honours() {
        let args_of = |flags: &[&str]| {
            let argv = std::iter::once("ranikust").chain(flags.iter().copied());
            let Command::Build(args) = cli::parse(argv).unwrap().cli.into_command() else {
                panic!("expected build");
            };
            args
        };
        let ignored = args_of(&["--no-push", "--ignore-path=/var/cache/*"]);
        let note = kaniko_only_kept_off(&ignored).expect("--ignore-path keeps kaniko mode");
        assert!(
            note.starts_with("rootless overlay mode is not tried: --ignore-path "),
            "{note}"
        );
        assert!(config::overlay_mode_flags(&ignored).is_err());
        for flags in [
            &["--no-push"][..],
            &["--no-push", "--snapshot-mode=redo", "--snapshot-workers=4"],
        ] {
            assert_eq!(kaniko_only_kept_off(&args_of(flags)), None, "{flags:?}");
            assert!(
                config::overlay_mode_flags(&args_of(flags)).is_ok(),
                "{flags:?}"
            );
        }
    }

    /// Only root's `--mode=auto`, which built in kaniko mode before
    /// rootless overlay mode, falls back to it from the build's user
    /// namespace; a user other than root and `--mode=overlay` get the error
    /// (ADR-0136).
    #[test]
    fn only_roots_auto_mode_falls_back_to_kaniko_mode() {
        let argv = ["ranikust", "--no-push"];
        let Command::Build(args) = cli::parse(argv).unwrap().cli.into_command() else {
            panic!("expected build");
        };
        assert!(Via::AutoRoot(&args).falls_back());
        assert!(!Via::AutoUser(&args).falls_back());
        assert!(!Via::Overlay.falls_back());
        // `--userns` is refused rather than built without its namespace.
        assert!(!Via::Userns(&args).falls_back());
        assert_eq!(Via::Userns(&args).flag(), "--userns");
        assert_eq!(Via::AutoRoot(&args).flag(), "--mode=auto");
        assert_eq!(Via::Overlay.flag(), "--mode=overlay");
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
