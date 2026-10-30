//! The validated settings of a build.
//!
//! [`BuildConfig`] is what the libraries will receive: plain values, checked
//! once, with kaniko's flag rules applied. It lives in the binary for now;
//! no design document assigns it to a library yet.

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

/// The validated settings of `build`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BuildConfig {
    /// The build context: a local directory, absolute.
    pub(crate) context: PathBuf,
    /// The Dockerfile path as given; resolved against the context later.
    pub(crate) dockerfile: PathBuf,
    /// Build arguments in command-line order.
    pub(crate) build_args: Vec<BuildArg>,
    /// Stages to build; the first is the image pushed. Empty: the last stage.
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
    /// The requested mode.
    pub(crate) mode: Mode,
    /// The work directory, absolute, if given.
    pub(crate) workdir: Option<PathBuf>,
    /// The state directory, absolute, if given.
    pub(crate) state_dir: Option<PathBuf>,
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
        Ok(Self {
            context: absolute(&context_dir(&args.context)?),
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
            mode: args.mode,
            workdir: args.workdir.as_deref().map(absolute),
            state_dir: args.state_dir.as_deref().map(absolute),
            jobs: args.jobs,
            network: args.network,
            secrets: args.secrets.iter().map(secret).collect(),
            allow_network_host: args.allow.contains(&crate::cli::Entitlement::NetworkHost),
            allow_network_none_userns: args
                .allow
                .contains(&crate::cli::Entitlement::NetworkNoneUserns),
        })
    }

    /// The state directory for `mode` (ADR-0022): `--state-dir`, or
    /// `<workdir>/.ranikust` in overlay mode and `/ranikust/state` in kaniko
    /// mode. `None` in overlay mode without a workdir, and for
    /// [`Mode::Auto`] without `--state-dir`: the default is known only once
    /// the probe has picked a mode.
    pub(crate) fn state_dir_for(&self, mode: Mode) -> Option<PathBuf> {
        if let Some(dir) = &self.state_dir {
            return Some(dir.clone());
        }
        match mode {
            Mode::Kaniko => Some(PathBuf::from("/ranikust/state")),
            Mode::Overlay => self.workdir.as_ref().map(|w| w.join(".ranikust")),
            Mode::Auto => None,
        }
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
            "snapshot_mode": name(self.snapshot_mode),
            "targets": self.targets.len(),
        })
    }
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

/// `path` made absolute against the current directory, without touching the
/// filesystem: no symlink is resolved.
fn absolute(path: &Path) -> PathBuf {
    // `absolute` only fails without a current directory; the path as given is
    // then the best there is.
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests;
