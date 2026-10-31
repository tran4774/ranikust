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
//! `version` → logs and the opt-in metrics of `rnk-metrics`, which start no
//! thread, the report asked for with `--metrics-file` only (ADR-0037) → flag
//! validation into a `BuildConfig` → the process set-up of `setup`: a
//! private mount namespace, the probes of `rnk-sandbox` and `rnk-snapshot`
//! (the latter a seam until task 9), the outcome, the spawner thread →
//! hand-off. `--mode=auto` picks kaniko mode where root may not mount
//! (ADR-0040). Kaniko mode: root required; the container check of
//! `container` picks the root, `/` in a container, else a directory made in
//! the state directory (ADR-0050); the state directory's checks of
//! `statedir`; the store, the sandbox, the ignore list; then the build on
//! the shared root
//! (`design/kaniko-mode.md#stage-roots-on-the-shared-root`).
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
mod statedir;

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
#[derive(Debug)]
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
                report(line);
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
        report(&err.to_string());
        return ExitCode::from(1);
    }

    // ADR-0037: the report path comes only from the flag.
    let mut warnings = Vec::new();
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
    let result = tracing::debug_span!("ranikust.run").in_scope(|| {
        run(
            &command,
            &telemetry,
            fault.as_deref(),
            metrics_file.as_deref(),
        )
    });
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

fn run(
    command: &Command,
    telemetry: &Telemetry,
    fault: Option<&str>,
    metrics_file: Option<&Path>,
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
        Command::Build(args) => {
            tracing::debug_span!("ranikust.build").in_scope(|| build(args, telemetry, metrics_file))
        }
    }
}

fn build(
    args: &BuildArgs,
    telemetry: &Telemetry,
    metrics_file: Option<&Path>,
) -> Result<(), Failure> {
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
        state_dir = %config.state_dir.display(),
        "build settings validated"
    );
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
            kaniko(&config, "--mode=kaniko", can_mount, metrics_file)
        }
        config::Mode::Overlay => {
            overlay_flags(args)?;
            // One span for the whole set-up: the namespace, then the rest.
            let span = tracing::debug_span!("ranikust.setup");
            span.in_scope(setup::enter_namespace)
                .map_err(|refusal| Failure::error(refusal.to_string()))?;
            overlay(&config, &span)
        }
        // Whether root may mount decides, before anything is written
        // (ADR-0040).
        config::Mode::Auto => {
            let span = tracing::debug_span!("ranikust.setup");
            match span.in_scope(setup::enter_namespace) {
                Ok(()) => {
                    overlay_flags(args)?;
                    overlay(&config, &span)
                }
                Err(refusal) if refusal.needs_kaniko_mode() => {
                    let why = refusal.reason;
                    tracing::info!("kaniko mode, because root may not mount here: {why}");
                    kaniko(&config, "--mode=auto", false, metrics_file)
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

/// Overlay mode, once the namespace is entered: the rest of the set-up,
/// then the build.
fn overlay(config: &BuildConfig, span: &tracing::Span) -> Result<(), Failure> {
    let setup = span
        .in_scope(|| setup::set_up(&config.state_dir, config.allow_network_none_userns))
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

/// Kaniko mode, chosen by `how` (ADR-0050): in a container (kaniko's
/// heuristic, ADR-0048) the root is `/`; anywhere else ranikust makes one
/// in its state directory, `<state>/builds/<id>/rootfs`. Then
/// `RLIMIT_NOFILE` is raised (the original kept for `RUN`), the state
/// directory and store set up, the ignore list built, and the stages run
/// one unit at a time on the shared root
/// (`design/kaniko-mode.md#stage-roots-on-the-shared-root`). `can_mount` is
/// whether the build's mount namespace could be entered.
fn kaniko(
    config: &BuildConfig,
    how: &str,
    can_mount: bool,
    metrics_file: Option<&Path>,
) -> Result<(), Failure> {
    if config.single_snapshot {
        return Err(Failure::usage(
            "--single-snapshot is not implemented in kaniko mode yet (Phase 2)",
        ));
    }
    let nofile = rnk_sandbox::NofileLimit::current();
    if rustix::process::geteuid().as_raw() != 0 {
        return Err(Failure::error(format!(
            "{how}: kaniko mode needs root (chown, chroot, switching users), as kaniko does; \
             run ranikust as root in the container or VM that builds"
        )));
    }
    let on_slash = match container::detect(&container::Real) {
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
    match statedir::raise_nofile_limit() {
        Ok(limit) => tracing::debug!(?limit, "RLIMIT_NOFILE raised to its hard limit"),
        // The walk still runs, with fewer descriptors to hold directories.
        Err(e) => tracing::warn!("cannot raise RLIMIT_NOFILE to its hard limit: {e}"),
    }
    if config.jobs.is_some() {
        tracing::warn!(
            "--jobs is ignored in kaniko mode: stages run one at a time on the shared root"
        );
    }
    let setup = tracing::debug_span!("ranikust.setup")
        .in_scope(|| setup::set_up_kaniko(&config.state_dir, on_slash, can_mount, nofile))
        .map_err(|refusal| Failure::error(refusal.to_string()))?;
    let setup::KanikoSetup {
        store,
        sandbox,
        isolation,
        root,
        root_path,
        state_dir,
        root_lock,
    } = setup;
    let snapshotter = kaniko_snapshotter(
        config,
        root,
        &root_path,
        on_slash.then_some(state_dir.as_path()),
        store.build_dir(),
        metrics_file,
    )?;
    let built = build::run(
        config,
        store,
        sandbox,
        build::Snapshots::Kaniko {
            snapshotter,
            isolation,
        },
    )
    .map_err(Failure::error)?;
    // The lock of `/` is held to the end of the build.
    drop(root_lock);
    tracing::info!("built image {}", built.digest);
    writeln!(std::io::stdout(), "{}", built.digest)
        .map_err(|e| Failure::error(format!("printing the image digest: {e}")))
}

/// Kaniko mode's snapshotter on `root`, opened once by the set-up (`/`, or
/// the directory made in the state directory), with the ignore list from
/// mountinfo, the flags and the state directory, and the walk's options.
/// `state_dir` is given when it lies inside the root (`/` in a container):
/// it goes on the ignore list. The paths given to the ignore list are
/// physical: `/`, or the build root the set-up made, and the state
/// directory, canonical and opened without following a symlink. What later
/// stages read of earlier ones is kept in `<build_dir>/kept`, made here,
/// private to the build.
fn kaniko_snapshotter(
    config: &BuildConfig,
    root: rnk_rootfs::RootPath,
    root_path: &Path,
    state_dir: Option<&Path>,
    build_dir: &Path,
    metrics_file: Option<&Path>,
) -> Result<rnk_snapshot::Kaniko, Failure> {
    let fail = |what: &str, e: &dyn std::fmt::Display| Failure::error(format!("{what}: {e}"));
    let mounts =
        rnk_snapshot::kaniko::read_mountinfo().map_err(|e| fail("reading mountinfo", &e))?;
    if let Some(state) = state_dir {
        let state_root = rnk_rootfs::RootPath::open(state)
            .map_err(|e| fail(&format!("opening {}", state.display()), &e))?;
        same_mount_only_inside(&root, root_path, &state_root, state, &mounts)?;
    }
    let spec = rnk_snapshot::kaniko::IgnoreSpec {
        root: root_path,
        state_dir,
        ignore_paths: &config.ignore_paths,
        ignore_var_run: config.ignore_var_run,
    };
    let ignore = rnk_snapshot::kaniko::ignore_list(&spec, &mounts)
        .map_err(|e| fail("building the ignore list", &e))?;
    paths_outside_the_root(config, metrics_file, root_path, &ignore)?;
    let walk = rnk_snapshot::WalkOptions {
        workers: config.snapshot_workers,
        mode: match config.snapshot_mode {
            config::SnapshotMode::Full => rnk_snapshot::SnapshotMode::Full,
            config::SnapshotMode::Redo => rnk_snapshot::SnapshotMode::Redo,
            config::SnapshotMode::Time => rnk_snapshot::SnapshotMode::Time,
        },
    };
    let build = rnk_rootfs::RootPath::open(build_dir)
        .map_err(|e| fail(&format!("opening {}", build_dir.display()), &e))?;
    let kept = build
        .create_dir("kept", 0o700)
        .and_then(|()| build.sub_root("kept"))
        .map_err(|e| fail(&format!("making {}/kept", build_dir.display()), &e))?;
    Ok(rnk_snapshot::Kaniko::new(
        root,
        root_path.to_owned(),
        ignore,
        walk,
        kept,
    ))
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
/// not exist yet is judged by its nearest existing ancestor.
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
    }
}
