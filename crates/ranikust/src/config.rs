//! The validated settings of a build.
//!
//! [`BuildConfig`] is what the libraries will receive: plain values, checked
//! once, with kaniko's flag rules applied. It lives in the binary for now;
//! no design document assigns it to a library yet.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde_json::{Value, json};

use crate::cli::BuildArgs;

/// The snapshot mode to use (ADR-0002, ADR-0023).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum Mode {
    /// Probe the environment and pick the fastest mode that works.
    #[default]
    Auto,
    /// Overlay snapshots; fails with the probe's diagnosis where it cannot
    /// work.
    Overlay,
    /// kaniko-style walk and hash.
    Kaniko,
}

/// What the kaniko-mode walk compares (kaniko's `--snapshot-mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum SnapshotMode {
    /// Content and metadata.
    #[default]
    Full,
    /// Mode, mtime, size, uid and gid.
    Redo,
    /// mtime only.
    Time,
}

/// Layer compression.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum Compression {
    /// gzip, the default.
    #[default]
    Gzip,
    /// zstd (OCI media types only).
    Zstd,
}

/// kaniko's `--image-format`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ImageFormatArg {
    /// Docker schema 2 media types.
    Docker,
    /// OCI media types, the default (Q-006, ADR-0032).
    Oci,
}

/// kaniko's `--verbosity` levels (logrus's).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum Verbosity {
    Trace,
    Debug,
    #[default]
    Info,
    #[value(alias = "warning")]
    Warn,
    Error,
    /// As `error`: ranikust has no level above it.
    Fatal,
    /// As `error`.
    Panic,
}

/// kaniko's `--log-format`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum LogFormatArg {
    /// Plain text.
    Text,
    /// logrus forced colors and seconds since start (kaniko's default).
    #[default]
    Color,
    /// One JSON object per line.
    Json,
}

/// The log settings of `args`, for `rnk_metrics::Config` (ADR-0036).
pub(crate) fn log_options(args: &BuildArgs) -> rnk_metrics::LogOptions {
    use tracing::level_filters::LevelFilter;
    rnk_metrics::LogOptions {
        level: match args.verbosity {
            Verbosity::Trace => LevelFilter::TRACE,
            Verbosity::Debug => LevelFilter::DEBUG,
            Verbosity::Info => LevelFilter::INFO,
            Verbosity::Warn => LevelFilter::WARN,
            Verbosity::Error | Verbosity::Fatal | Verbosity::Panic => LevelFilter::ERROR,
        },
        format: match args.log_format {
            LogFormatArg::Text => rnk_metrics::LogFormat::Text,
            LogFormatArg::Color => rnk_metrics::LogFormat::Color,
            LogFormatArg::Json => rnk_metrics::LogFormat::Json,
        },
        timestamp: args.log_timestamp,
    }
}

/// The network `RUN` gets (ADR-0010).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum Network {
    /// The host's network.
    #[default]
    Host,
    /// No network: only a loopback interface.
    None,
}

/// A `--build-arg`: `KEY=VALUE`, or `KEY` alone, whose value comes from the
/// environment when the build starts, as in kaniko.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BuildArg {
    /// The argument's name.
    pub(crate) name: String,
    /// The value; `None` takes it from the environment.
    pub(crate) value: Option<String>,
}

/// The only platform accepted for now (ADR-0009).
pub(crate) const PLATFORM: &str = "linux/amd64";

/// Workers of the kaniko-mode walk without `--snapshot-workers`: 16 and 32
/// were slower than 8 on every hash (ADR-0039).
pub(crate) const SNAPSHOT_WORKERS: NonZeroUsize = NonZeroUsize::new(8).unwrap();

/// The state directory without `--state-dir`, in every mode, as BuildKit's
/// `/var/lib/buildkit` (ADR-0050). It holds the store and every build root
/// ranikust makes; rootless mode's default comes in Phase 3.
pub(crate) const DEFAULT_STATE_DIR: &str = "/var/lib/ranikust";

/// The validated settings of `build`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BuildConfig {
    /// The build context: a local directory, absolute.
    pub(crate) context: PathBuf,
    /// The Dockerfile path as given; resolved against the context later.
    pub(crate) dockerfile: PathBuf,
    /// Build arguments in command-line order.
    pub(crate) build_args: Vec<BuildArg>,
    /// The stage to build and push, at most one (ADR-0049). Empty: the last
    /// stage.
    pub(crate) targets: Vec<String>,
    /// The image platform, `linux/amd64`.
    pub(crate) platform: &'static str,
    /// Images to push to.
    pub(crate) destinations: Vec<String>,
    /// Do not push.
    pub(crate) no_push: bool,
    /// Docker `save` tarball to write, absolute.
    pub(crate) tar_path: Option<PathBuf>,
    /// OCI image layout to write, absolute.
    pub(crate) oci_layout_path: Option<PathBuf>,
    /// Layer compression.
    pub(crate) compression: Compression,
    /// Fixed timestamps.
    pub(crate) reproducible: bool,
    /// Registries reached over plain HTTP when HTTPS fails
    /// (`--insecure-registry`) [open: Q-200].
    pub(crate) insecure_registries: Vec<String>,
    /// Extra CA certificates per registry, paths absolute.
    pub(crate) registry_certificates: Vec<(String, PathBuf)>,
    /// What the kaniko-mode walk compares.
    pub(crate) snapshot_mode: SnapshotMode,
    /// `--ignore-path`s, absolute and cleaned as kaniko's `filepath.Clean`.
    pub(crate) ignore_paths: Vec<PathBuf>,
    /// `--ignore-var-run`, kaniko's default `true`.
    pub(crate) ignore_var_run: bool,
    /// `--single-snapshot`.
    pub(crate) single_snapshot: bool,
    /// `--materialize`; no effect without a layer cache (Phase 4).
    pub(crate) materialize: bool,
    /// Workers of the kaniko-mode walk (ADR-0039).
    pub(crate) snapshot_workers: NonZeroUsize,
    /// The requested mode.
    pub(crate) mode: Mode,
    /// The state directory, absolute: `--state-dir`, else
    /// [`DEFAULT_STATE_DIR`] (ADR-0050).
    pub(crate) state_dir: PathBuf,
    /// Stages at once; `None` is the number of CPUs available (ADR-0013).
    pub(crate) jobs: Option<NonZeroUsize>,
    /// The network of `RUN`.
    pub(crate) network: Network,
    /// `--secret`s by id, file paths absolute.
    pub(crate) secrets: Vec<(String, rnk_builder::Secret)>,
    /// `--allow network.host`.
    pub(crate) allow_network_host: bool,
    /// `--allow network.none-userns` (ADR-0033).
    pub(crate) allow_network_none_userns: bool,
    /// The compression level, kaniko's range mapped to the encoder's;
    /// `None`: ranikust's default.
    pub(crate) compression_level: Option<i32>,
    /// `--image-format`; `None`: OCI.
    pub(crate) image_format: Option<ImageFormatArg>,
    /// `--label`s, for the pushed stage.
    pub(crate) labels: Vec<(String, String)>,
    /// `--annotation`s, for the manifest.
    pub(crate) annotations: BTreeMap<String, String>,
    /// The digest files, absolute.
    pub(crate) digest_file: Option<PathBuf>,
    pub(crate) image_name_digest_file: Option<PathBuf>,
    pub(crate) image_name_tag_digest_file: Option<PathBuf>,
    /// `--push-retry` and `--image-download-retry`.
    pub(crate) push_retry: Option<u32>,
    pub(crate) image_download_retry: Option<u32>,
    /// `--insecure` and `--insecure-pull`.
    pub(crate) insecure_push: bool,
    pub(crate) insecure_pull: bool,
    /// `--skip-tls-verify`, `--skip-tls-verify-pull` and
    /// `--skip-tls-verify-registry`.
    pub(crate) skip_tls_verify_push: bool,
    pub(crate) skip_tls_verify_pull: bool,
    pub(crate) skip_tls_verify_registries: Vec<String>,
}

/// kaniko's `--compression-level` for `compression`, as the encoder takes
/// it (ADR-0036): gzip -1 (Go's default) is 6, zstd 1 to 19. kaniko's
/// gzip -2 (Huffman only) and zstd 20 to 22 are refused
/// (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:499-508`).
fn compression_level(compression: Compression, level: i32) -> Result<i32, Invalid> {
    match (compression, level) {
        (Compression::Gzip, -1) => Ok(6),
        (Compression::Gzip, 0..=9) | (Compression::Zstd, 1..=19) => Ok(level),
        (Compression::Gzip, -2) => Err(Invalid(
            "--compression-level=-2 (gzip Huffman-only) is not supported: use -1 to 9".to_owned(),
        )),
        (Compression::Zstd, 20..=22) => Err(Invalid(format!(
            "--compression-level={level} is not supported for zstd: use 1 to 19"
        ))),
        (Compression::Gzip, _) => Err(Invalid(format!(
            "--compression-level={level} is out of range for gzip, use -1 to 9"
        ))),
        (Compression::Zstd, _) => Err(Invalid(format!(
            "--compression-level={level} is out of range for zstd, use 1 to 19"
        ))),
    }
}

/// A `KEY=VALUE` of `--label` or `--annotation`, split at the first `=`.
fn key_value(flag: &str, arg: &str) -> Result<(String, String), Invalid> {
    match arg.split_once('=') {
        Some((k, v)) => Ok((k.to_owned(), v.to_owned())),
        None => Err(Invalid(format!(
            "--{flag}={arg}: expected key=value, as kaniko"
        ))),
    }
}

/// Where a `--secret` comes from, as BuildKit's secrets store
/// (`session/secrets/secretsprovider/store.go`): `env=` first; without a
/// source, the variable named by the id when it is set, else the file named
/// by the id.
fn secret(s: &crate::cli::SecretArg) -> (String, rnk_builder::Secret) {
    use rnk_builder::Secret;
    let source = match (&s.env, &s.src) {
        (Some(env), _) => Secret::Env(env.into()),
        (None, Some(src)) => Secret::File(absolute(src)),
        (None, None) if std::env::var_os(&s.id).is_some() => Secret::Env(s.id.clone().into()),
        (None, None) => Secret::File(absolute(Path::new(&s.id))),
    };
    (s.id.clone(), source)
}

/// A rejected combination of flags.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Invalid(pub(crate) String);

impl BuildConfig {
    /// Validates `args`, applying kaniko's rules, and makes paths absolute
    /// against the current directory. Touches no file.
    ///
    /// # Errors
    ///
    /// [`Invalid`] with a message for the user.
    pub(crate) fn from_args(args: &BuildArgs) -> Result<Self, Invalid> {
        let compression = args.compression.unwrap_or_default();
        // kaniko `cmd/executor/cmd/root.go:200-202`.
        if !args.no_push && args.destinations.is_empty() {
            return Err(Invalid(
                "you must provide --destination, or use --no-push".to_owned(),
            ));
        }
        // kaniko `cmd/executor/cmd/root.go:185-187`.
        if args.tar_path.is_some() && compression == Compression::Zstd {
            return Err(Invalid(
                "--compression=zstd cannot be used with --tar-path: a Docker save \
                 tarball has no zstd layer media type; use --oci-layout-path"
                    .to_owned(),
            ));
        }
        // kaniko `cmd/executor/cmd/root.go:178-180`. The other check there,
        // `--tar-path` with `--image-format=oci`, is not kaniko's here:
        // ranikust's tarball holds OCI media types by default (ADR-0036).
        if args.image_format == Some(ImageFormatArg::Docker) && compression == Compression::Zstd {
            return Err(Invalid(
                "--compression=zstd cannot be used with --image-format=docker: the Docker \
                 schema2 format has no zstd layer media type; use --image-format=oci"
                    .to_owned(),
            ));
        }
        // kaniko `cmd/executor/cmd/root.go:212-217`.
        for (flag, set) in [
            (
                "--image-name-with-digest-file",
                args.image_name_with_digest_file.is_some(),
            ),
            (
                "--image-name-tag-with-digest-file",
                args.image_name_tag_with_digest_file.is_some(),
            ),
        ] {
            if set && args.destinations.is_empty() {
                return Err(Invalid(format!(
                    "you must provide --destination if setting {flag}"
                )));
            }
        }
        // kaniko PUTs a digest file whose path is an https:// URL
        // (`pkg/executor/push.go:150-166`); ranikust writes local files
        // only.
        for (flag, path) in [
            ("--digest-file", &args.digest_file),
            (
                "--image-name-with-digest-file",
                &args.image_name_with_digest_file,
            ),
            (
                "--image-name-tag-with-digest-file",
                &args.image_name_tag_with_digest_file,
            ),
        ] {
            if path
                .as_deref()
                .and_then(Path::to_str)
                .is_some_and(|p| p.starts_with("https://"))
            {
                return Err(Invalid(format!(
                    "{flag}: uploading the file to a URL, as kaniko can, is not supported; \
                     give a local path"
                )));
            }
        }
        let compression_level = args
            .compression_level
            .map(|l| compression_level(compression, l))
            .transpose()?;
        if let Some(platform) = &args.platform
            && platform != PLATFORM
        {
            return Err(Invalid(format!(
                "--platform={platform} is not supported yet: only {PLATFORM} (ADR-0009)"
            )));
        }
        let mut ids = std::collections::BTreeSet::new();
        if let Some(s) = args.secrets.iter().find(|s| !ids.insert(s.id.as_str())) {
            return Err(Invalid(format!("--secret id={} is given twice", s.id)));
        }
        if args.targets.iter().any(String::is_empty) {
            return Err(Invalid("--target names an empty stage".to_owned()));
        }
        // One stage per build (ADR-0049): kaniko's list builds the stages
        // after the first and throws them away.
        if args.targets.len() > 1 {
            return Err(Invalid(format!(
                "--target names {} stages ({}): give one; build the others in separate runs",
                args.targets.len(),
                args.targets.join(", ")
            )));
        }
        // kaniko keeps a relative `--ignore-path`, which then never matches
        // the absolute paths of its walk; ranikust says so instead.
        if let Some(path) = args.ignore_paths.iter().find(|p| !p.is_absolute()) {
            return Err(Invalid(format!(
                "--ignore-path={}: give an absolute path in the image; kaniko would never \
                 match a relative one",
                path.display()
            )));
        }
        Ok(Self {
            context: absolute(&context_with_sub_path(args)?),
            dockerfile: args.dockerfile.clone(),
            build_args: args
                .build_args
                .iter()
                .map(|arg| build_arg(arg))
                .collect::<Result<_, _>>()?,
            targets: args.targets.clone(),
            platform: PLATFORM,
            destinations: args.destinations.clone(),
            no_push: args.no_push,
            tar_path: args.tar_path.as_deref().map(absolute),
            oci_layout_path: args.oci_layout_path.as_deref().map(absolute),
            compression,
            reproducible: args.reproducible,
            insecure_registries: args.insecure_registries.clone(),
            registry_certificates: args
                .registry_certificates
                .iter()
                .map(|(registry, path)| (registry.clone(), absolute(path)))
                .collect(),
            snapshot_mode: args.snapshot_mode.unwrap_or_default(),
            ignore_paths: args.ignore_paths.iter().map(|p| clean(p)).collect(),
            ignore_var_run: args.ignore_var_run.unwrap_or(true),
            single_snapshot: args.single_snapshot,
            materialize: args.materialize,
            snapshot_workers: args.snapshot_workers.unwrap_or(SNAPSHOT_WORKERS),
            mode: args.mode,
            state_dir: args
                .state_dir
                .as_deref()
                .map_or_else(|| PathBuf::from(DEFAULT_STATE_DIR), absolute),
            jobs: args.jobs,
            network: args.network,
            secrets: args.secrets.iter().map(secret).collect(),
            allow_network_host: args.allow.contains(&crate::cli::Entitlement::NetworkHost),
            allow_network_none_userns: args
                .allow
                .contains(&crate::cli::Entitlement::NetworkNoneUserns),
            compression_level,
            image_format: args.image_format,
            labels: args
                .labels
                .iter()
                .map(|l| key_value("label", l))
                .collect::<Result<_, _>>()?,
            annotations: args
                .annotations
                .iter()
                .map(|a| key_value("annotation", a))
                .collect::<Result<_, _>>()?,
            digest_file: args.digest_file.as_deref().map(absolute),
            image_name_digest_file: args.image_name_with_digest_file.as_deref().map(absolute),
            image_name_tag_digest_file: args
                .image_name_tag_with_digest_file
                .as_deref()
                .map(absolute),
            push_retry: args.push_retry,
            image_download_retry: args.image_download_retry,
            insecure_push: args.insecure,
            insecure_pull: args.insecure_pull,
            skip_tls_verify_push: args.skip_tls_verify,
            skip_tls_verify_pull: args.skip_tls_verify_pull,
            skip_tls_verify_registries: args.skip_tls_verify_registries.clone(),
        })
    }

    /// The options that change what a build costs, for the metrics report's
    /// `meta.options`: no build argument, destination, path or credential.
    pub(crate) fn meta_options(&self) -> Value {
        json!({
            "compression": name(self.compression),
            "jobs": self.jobs.map(NonZeroUsize::get),
            "mode": name(self.mode),
            "network": name(self.network),
            "no_push": self.no_push,
            "outputs": {
                "destinations": self.destinations.len(),
                "oci_layout": self.oci_layout_path.is_some(),
                "tar": self.tar_path.is_some(),
            },
            "reproducible": self.reproducible,
            "single_snapshot": self.single_snapshot,
            "snapshot_mode": name(self.snapshot_mode),
            "snapshot_workers": self.snapshot_workers.get(),
            "targets": self.targets.len(),
        })
    }
}

/// What overlay mode makes of the kaniko-mode flags given in `args`
/// (ADR-0040): the warnings for those that change nothing there
/// (`--snapshot-mode`, `--snapshot-workers`, `--materialize`,
/// `--ignore-var-run=true`).
///
/// # Errors
///
/// [`Invalid`] for those that would change the image: `--ignore-path`,
/// `--single-snapshot` and `--ignore-var-run=false` take effect in kaniko
/// mode only (ADR-0049): overlay mode's layers never hold mounts or
/// `/var/run`, and squashing them costs a re-encode.
pub(crate) fn overlay_mode_flags(args: &BuildArgs) -> Result<Vec<String>, Invalid> {
    let mut changes = Vec::new();
    if !args.ignore_paths.is_empty() {
        changes.push("--ignore-path");
    }
    if args.single_snapshot {
        changes.push("--single-snapshot");
    }
    if args.ignore_var_run == Some(false) {
        changes.push("--ignore-var-run=false");
    }
    if !changes.is_empty() {
        return Err(Invalid(format!(
            "{} take effect in kaniko mode only (ADR-0049); overlay mode would build a \
             different image: drop them, or use --mode=kaniko",
            changes.join(", ")
        )));
    }
    let mut warnings = Vec::new();
    if args.snapshot_mode.is_some() {
        warnings.push("--snapshot-mode is ignored in overlay mode (ADR-0023)".to_owned());
    }
    for (given, flag) in [
        (args.snapshot_workers.is_some(), "--snapshot-workers"),
        (args.materialize, "--materialize"),
        (args.ignore_var_run == Some(true), "--ignore-var-run"),
    ] {
        if given {
            warnings.push(format!(
                "{flag} is ignored in overlay mode, where it changes nothing"
            ));
        }
    }
    Ok(warnings)
}

/// A value enum's command-line name.
pub(crate) fn name<T: ValueEnum>(value: T) -> String {
    value
        .to_possible_value()
        .map(|v| v.get_name().to_owned())
        .unwrap_or_default()
}

/// The local directory of `--context`: `dir://` stripped; other schemes are
/// refused with the phase that brings them.
/// The context directory with kaniko's `--context-sub-path` applied: joined
/// onto a `dir://` context (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:599-631`,
/// `pkg/buildcontext/dir.go:25`), staying inside it, so an absolute path
/// or `..` is refused. kaniko returns before the join for a plain path, so
/// there it is ignored ([`context_sub_path_ignored`]); other schemes are
/// refused with their phase by [`context_dir`].
fn context_with_sub_path(args: &BuildArgs) -> Result<PathBuf, Invalid> {
    let dir = context_dir(&args.context)?;
    let Some(sub) = args.context_sub_path.as_deref() else {
        return Ok(dir);
    };
    if !args.context.starts_with("dir://") {
        return Ok(dir);
    }
    let sub_path = Path::new(sub);
    let inside = sub_path.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    });
    if !inside {
        return Err(Invalid(format!(
            "--context-sub-path={sub}: must be a relative path inside the context, without .."
        )));
    }
    Ok(dir.join(sub_path))
}

/// Whether `--context-sub-path` is given for a plain local path, where
/// kaniko ignores it: the caller warns.
pub(crate) fn context_sub_path_ignored(args: &BuildArgs) -> bool {
    args.context_sub_path.is_some() && !args.context.contains("://")
}

fn context_dir(context: &str) -> Result<PathBuf, Invalid> {
    if context.is_empty() {
        return Err(Invalid("--context is empty".to_owned()));
    }
    if let Some(dir) = context.strip_prefix("dir://") {
        return Ok(PathBuf::from(dir));
    }
    for (scheme, why) in [
        ("git://", "git contexts come in Phase 5"),
        ("tar://", "tar contexts come in Phase 5"),
        ("https://", "HTTPS contexts come in Phase 5"),
        ("http://", "HTTP contexts come in Phase 5"),
        ("s3://", "S3 contexts are out of scope (ADR-0008)"),
        ("gs://", "GCS contexts are out of scope (ADR-0008)"),
        ("az://", "Azure contexts are out of scope (ADR-0008)"),
    ] {
        if context.starts_with(scheme) {
            return Err(Invalid(format!("--context={context}: {why}")));
        }
    }
    Ok(PathBuf::from(context))
}

/// A `--build-arg` value.
fn build_arg(arg: &str) -> Result<BuildArg, Invalid> {
    let (name, value) = match arg.split_once('=') {
        Some((name, value)) => (name, Some(value.to_owned())),
        None => (arg, None),
    };
    if name.is_empty() {
        return Err(Invalid(format!("--build-arg={arg}: the name is empty")));
    }
    Ok(BuildArg {
        name: name.to_owned(),
        value,
    })
}

/// `path` cleaned lexically, as Go's `filepath.Clean`: no `.`, no repeated
/// `/`, `..` resolved against the parent (never above `/`).
fn clean(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    out
}

/// `path` made absolute against the current directory, without touching the
/// filesystem: no symlink is resolved.
fn absolute(path: &Path) -> PathBuf {
    // `absolute` only fails without a current directory; the path as given is
    // then the best there is.
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests;
