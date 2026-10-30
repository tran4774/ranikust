//! Command-line parsing: kaniko's executor flags plus ranikust's own.
//!
//! `ranikust` with flags and no subcommand builds, as kaniko's `executor`
//! does, so a pipeline can swap the binary and keep its flags. Every accepted
//! kaniko flag keeps kaniko's name, short form and meaning (ADR-0011); kaniko
//! flags that ranikust does not accept yet are recognised and refused with a
//! reason, never silently ignored.

use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{Compression, Mode, Network, SnapshotMode};

/// Container image builder that ships as one static binary and runs inside
/// or outside containers.
///
/// Without a subcommand, `ranikust` builds, accepting kaniko's executor
/// flags.
#[derive(Debug, Parser)]
#[command(
    name = "ranikust",
    version,
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    build: BuildArgs,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Build an image from a Dockerfile (the default without a subcommand).
    Build(Box<BuildArgs>),
    /// Print the version.
    Version,
}

impl Cli {
    /// The subcommand to run: `build` with the top-level flags when none is
    /// given.
    pub(crate) fn into_command(self) -> Command {
        self.command
            .unwrap_or_else(|| Command::Build(Box::new(self.build)))
    }
}

/// The flags of `build`. kaniko's names come first in each group; ranikust's
/// own flags are in their own group.
#[derive(Clone, Debug, Default, Args)]
pub(crate) struct BuildArgs {
    /// Build context: a local directory, with or without `dir://` (git, tar
    /// and HTTPS contexts come in Phase 5).
    #[arg(
        short = 'c',
        long,
        value_name = "DIR",
        default_value = "/workspace/",
        help_heading = "Input"
    )]
    pub(crate) context: String,

    /// Path of the Dockerfile.
    #[arg(
        short = 'f',
        long,
        value_name = "PATH",
        default_value = "Dockerfile",
        help_heading = "Input"
    )]
    pub(crate) dockerfile: PathBuf,

    /// A build argument; `KEY` alone takes its value from the environment.
    /// Repeat for several.
    #[arg(long = "build-arg", value_name = "KEY[=VALUE]", help_heading = "Input")]
    pub(crate) build_args: Vec<String>,

    /// Stages to build, comma-separated or repeated; the first is the image
    /// pushed. Default: the last stage.
    #[arg(
        long = "target",
        value_name = "STAGE",
        value_delimiter = ',',
        help_heading = "Input"
    )]
    pub(crate) targets: Vec<String>,

    /// Platform of the image; only `linux/amd64` for now.
    #[arg(long, value_name = "OS/ARCH", help_heading = "Input")]
    pub(crate) platform: Option<String>,

    /// Registry image to push the result to. Repeat for several.
    #[arg(
        short = 'd',
        long = "destination",
        value_name = "IMAGE",
        help_heading = "Output"
    )]
    pub(crate) destinations: Vec<String>,

    /// Do not push the image.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = parse_bool,
        help_heading = "Output"
    )]
    pub(crate) no_push: bool,

    /// Also write the image as a Docker `save` tarball at this path.
    #[arg(long, value_name = "PATH", help_heading = "Output")]
    pub(crate) tar_path: Option<PathBuf>,

    /// Also write the image as an OCI image layout at this path.
    #[arg(long, value_name = "PATH", help_heading = "Output")]
    pub(crate) oci_layout_path: Option<PathBuf>,

    /// Layer compression [default: gzip].
    #[arg(long, value_enum, help_heading = "Output")]
    pub(crate) compression: Option<Compression>,

    /// Fixed timestamps, for byte-identical images.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = parse_bool,
        help_heading = "Output"
    )]
    pub(crate) reproducible: bool,

    /// A registry reached over plain HTTP when HTTPS fails, as kaniko's
    /// `--insecure-registry`: `host[:port]`. Repeat for several.
    // [open: Q-200] interim (a), and one flag of Q-023's list: kaniko's name
    // and meaning, through rnk-registry's `ClientOptions::insecure`.
    #[arg(
        long = "insecure-registry",
        value_name = "REGISTRY",
        value_parser = parse_registry_host,
        help_heading = "Output"
    )]
    pub(crate) insecure_registries: Vec<String>,

    /// Extra CA certificate for one registry. Repeat for several.
    #[arg(
        long = "registry-certificate",
        value_name = "REGISTRY=PATH",
        value_parser = parse_registry_certificate,
        help_heading = "Output"
    )]
    pub(crate) registry_certificates: Vec<(String, PathBuf)>,

    /// What the kaniko-mode walk compares, as in kaniko; ignored in overlay
    /// mode [default: full].
    #[arg(long, value_enum, help_heading = "Snapshots")]
    pub(crate) snapshot_mode: Option<SnapshotMode>,

    /// Snapshot mode: probe and choose, or force one.
    #[arg(long, value_enum, default_value_t = Mode::Auto, help_heading = "ranikust")]
    pub(crate) mode: Mode,

    /// Work directory (the build root in kaniko mode).
    #[arg(long, value_name = "DIR", help_heading = "ranikust")]
    pub(crate) workdir: Option<PathBuf>,

    /// State directory holding the store; kaniko's `--kaniko-dir`. Default:
    /// `<workdir>/.ranikust` in overlay mode, `/ranikust/state` in kaniko
    /// mode.
    #[arg(long, value_name = "DIR", help_heading = "ranikust")]
    pub(crate) state_dir: Option<PathBuf>,

    /// Stages run at once [default: the number of CPUs available].
    #[arg(long, value_name = "N", help_heading = "ranikust")]
    pub(crate) jobs: Option<NonZeroUsize>,

    /// Network of `RUN`: the host's, or none.
    #[arg(long, value_enum, default_value_t = Network::Host, help_heading = "ranikust")]
    pub(crate) network: Network,

    /// A secret for `RUN --mount=type=secret`, as buildctl's and kaniko's
    /// `--secret`: `id=ID[,type=file|env][,src=PATH|,env=VAR]`. Without a
    /// source, the variable named ID if set, else the file ID. Repeat for
    /// several. A secret never reaches a layer, the config or a log.
    #[arg(
        long = "secret",
        value_name = "id=ID,src=PATH",
        value_parser = parse_secret,
        help_heading = "ranikust"
    )]
    pub(crate) secrets: Vec<SecretArg>,

    /// Grants an entitlement, as buildctl's `--allow`: `network.host` lets a
    /// `RUN --network=host` lift `--network=none`. Repeat for several.
    #[arg(
        long = "allow",
        value_name = "ENTITLEMENT",
        value_parser = parse_entitlement,
        help_heading = "ranikust"
    )]
    pub(crate) allow: Vec<Entitlement>,

    /// Write the build metrics report (JSON) to this path; overrides
    /// `RANIKUST_METRICS_FILE`.
    #[arg(long, value_name = "PATH", help_heading = "ranikust")]
    pub(crate) metrics_file: Option<PathBuf>,
}

/// Why a kaniko flag is refused.
#[derive(Clone, Copy, Debug)]
enum Refusal {
    /// The flag has a new name in ranikust.
    Renamed(&'static str),
    /// The flag comes in a later phase or waits for a decision.
    NotYet(&'static str),
    /// The feature is out of scope (ADR-0008).
    OutOfScope(&'static str),
    /// kaniko itself deprecated the flag.
    Deprecated(&'static str),
}

/// kaniko executor flags that ranikust recognises but does not accept
/// (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go`, `docs/agents/kaniko-map.md`).
const REFUSED: &[(&str, Refusal)] = &[
    ("kaniko-dir", Refusal::Renamed("use --state-dir")),
    (
        "custom-platform",
        Refusal::Renamed("use --platform (Q-020 in docs/open-questions.md)"),
    ),
    (
        "cache",
        Refusal::NotYet("the layer cache comes in Phase 4 (Q-021)"),
    ),
    (
        "cache-repo",
        Refusal::NotYet("the layer cache comes in Phase 4 (Q-021)"),
    ),
    (
        "ignore-path",
        Refusal::NotYet("ignore paths matter in kaniko mode, which comes in Phase 2"),
    ),
    (
        "bucket",
        Refusal::OutOfScope("GCS contexts are out of scope (ADR-0008)"),
    ),
    ("git", Refusal::NotYet("git contexts come in Phase 5")),
    (
        "registry-mirror",
        Refusal::NotYet("registry mirrors come in Phase 5 (Q-047)"),
    ),
    (
        "registry-map",
        Refusal::NotYet("registry maps come in Phase 5 (Q-047)"),
    ),
    (
        "credential-helpers",
        Refusal::NotYet("credential helpers come in Phase 5 (Q-047)"),
    ),
    (
        "force",
        Refusal::NotYet("ranikust checks the workdir instead (Q-023)"),
    ),
    (
        "cache-dir",
        Refusal::NotYet("the store in --state-dir replaces it (Q-023)"),
    ),
    // Waiting for the flag list (Q-023).
    ("verbosity", Refusal::NotYet(Q023)),
    ("log-format", Refusal::NotYet(Q023)),
    ("log-timestamp", Refusal::NotYet(Q023)),
    ("label", Refusal::NotYet(Q023)),
    ("annotation", Refusal::NotYet(Q023)),
    ("image-format", Refusal::NotYet(Q023)),
    ("compression-level", Refusal::NotYet(Q023)),
    ("digest-file", Refusal::NotYet(Q023)),
    ("image-name-with-digest-file", Refusal::NotYet(Q023)),
    ("image-name-tag-with-digest-file", Refusal::NotYet(Q023)),
    ("insecure", Refusal::NotYet(Q023)),
    ("insecure-pull", Refusal::NotYet(Q023)),
    ("skip-tls-verify", Refusal::NotYet(Q023)),
    ("skip-tls-verify-pull", Refusal::NotYet(Q023)),
    ("skip-tls-verify-registry", Refusal::NotYet(Q023)),
    ("registry-client-cert", Refusal::NotYet(Q023)),
    ("skip-default-registry-fallback", Refusal::NotYet(Q023)),
    ("push-retry", Refusal::NotYet(Q023)),
    ("push-ignore-immutable-tag-errors", Refusal::NotYet(Q023)),
    ("image-download-retry", Refusal::NotYet(Q023)),
    ("image-fs-extract-retry", Refusal::NotYet(Q023)),
    ("context-sub-path", Refusal::NotYet(Q023)),
    ("single-snapshot", Refusal::NotYet(Q023)),
    ("use-new-run", Refusal::NotYet(Q023)),
    ("cleanup", Refusal::NotYet(Q023)),
    ("pre-cleanup", Refusal::NotYet(Q023)),
    ("preserve-context", Refusal::NotYet(Q023)),
    ("materialize", Refusal::NotYet(Q023)),
    ("ignore-var-run", Refusal::NotYet(Q023)),
    ("cache-ttl", Refusal::NotYet(Q023)),
    ("cache-copy-layers", Refusal::NotYet(Q023)),
    ("cache-run-layers", Refusal::NotYet(Q023)),
    ("compressed-caching", Refusal::NotYet(Q023)),
    ("no-push-cache", Refusal::NotYet(Q023)),
    ("skip-push-permission-check", Refusal::NotYet(Q023)),
    ("dryrun", Refusal::NotYet(Q023)),
    // kaniko's deprecated spellings.
    ("snapshotMode", Refusal::Deprecated("use --snapshot-mode")),
    ("customPlatform", Refusal::Deprecated("use --platform")),
    ("tarPath", Refusal::Deprecated("use --tar-path")),
    ("force-build-metadata", Refusal::Deprecated("drop it")),
    (
        "skip-unused-stages",
        Refusal::Deprecated("drop it; ranikust always skips unused stages"),
    ),
];

const Q023: &str = "not accepted yet; the kaniko flag list is decided in Phase 1 (Q-023)";

/// kaniko's short flags among [`REFUSED`].
const REFUSED_SHORT: &[(char, &str)] = &[('b', "bucket")];

/// A parse failure: kaniko flags refused with a reason, or clap's own error.
#[derive(Debug)]
pub(crate) enum ParseError {
    /// kaniko flags ranikust does not accept; one line each.
    Refused(Vec<String>),
    /// clap's error, including `--help` and `--version`, which exit 0.
    Clap(clap::Error),
}

/// Parses the command line. Refused kaniko flags are reported, all of them,
/// before clap sees the arguments.
pub(crate) fn parse<I, T>(args: I) -> Result<Cli, ParseError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let refused = refused_flags(args.iter().skip(1));
    if !refused.is_empty() {
        return Err(ParseError::Refused(refused));
    }
    Cli::try_parse_from(args).map_err(ParseError::Clap)
}

/// One line per refused kaniko flag among `args` (the program name already
/// skipped). Scanning stops at `--`.
fn refused_flags<'a>(args: impl Iterator<Item = &'a OsString>) -> Vec<String> {
    let mut lines = Vec::new();
    for arg in args {
        let Some(arg) = arg.to_str() else { continue };
        if arg == "--" {
            break;
        }
        let name = if let Some(long) = arg.strip_prefix("--") {
            long.split_once('=').map_or(long, |(name, _)| name)
        } else if let Some(short) = arg.strip_prefix('-') {
            let Some(first) = short.chars().next() else {
                continue;
            };
            match REFUSED_SHORT.iter().find(|(c, _)| *c == first) {
                Some((_, long)) => long,
                None => continue,
            }
        } else {
            continue;
        };
        if let Some((name, refusal)) = REFUSED.iter().find(|(n, _)| *n == name) {
            let reason = match refusal {
                Refusal::Renamed(why) => format!("renamed in ranikust: {why}"),
                Refusal::NotYet(why) => (*why).to_owned(),
                Refusal::OutOfScope(why) => (*why).to_owned(),
                Refusal::Deprecated(why) => format!("deprecated in kaniko: {why}"),
            };
            lines.push(format!("kaniko's --{name} is not supported: {reason}"));
        }
    }
    lines
}

/// pflag's boolean spellings (`strconv.ParseBool`), so `--no-push=true` and
/// `--no-push=1` mean what they mean to kaniko.
fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
        _ => Err(format!("expected true or false, got {value:?}")),
    }
}

/// `REGISTRY=PATH`, kaniko's `--registry-certificate` format.
fn parse_registry_certificate(value: &str) -> Result<(String, PathBuf), String> {
    match value.split_once('=') {
        Some((registry, path)) if !registry.is_empty() && !path.is_empty() => {
            Ok((registry.to_owned(), PathBuf::from(path)))
        }
        _ => Err(format!("expected REGISTRY=PATH, got {value:?}")),
    }
}

/// An `--insecure-registry` value: a registry host, with a port or not; no
/// scheme or path.
fn parse_registry_host(value: &str) -> Result<String, String> {
    if value.is_empty() || value.contains('/') || value.contains(char::is_whitespace) {
        return Err(format!(
            "expected a registry host such as registry:5000, got {value:?}"
        ));
    }
    Ok(value.to_owned())
}

/// A `--secret` as given: buildctl's `parseSecret`
/// (`cmd/buildctl/build/secret.go`), which kaniko copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SecretArg {
    pub(crate) id: String,
    /// `src=` or `source=`; with `type=env` and no `env=`, the variable.
    pub(crate) src: Option<PathBuf>,
    pub(crate) env: Option<String>,
}

/// A `--secret` value: comma-separated `key=value` fields, keys in any
/// case: `type` (`file` or `env`), `id`, `src`/`source`, `env`. With
/// `type=env` and no `env=`, `src` names the variable.
fn parse_secret(value: &str) -> Result<SecretArg, String> {
    let mut id = String::new();
    let mut src = None;
    let mut env = None;
    let mut typ = None;
    for field in value.split(',') {
        let Some((key, v)) = field.split_once('=') else {
            return Err(format!("invalid field {field:?}: must be a key=value pair"));
        };
        match key.to_ascii_lowercase().as_str() {
            "type" => match v {
                "file" | "env" => typ = Some(v.to_owned()),
                _ => return Err(format!("unsupported secret type {v:?}")),
            },
            "id" => v.clone_into(&mut id),
            "source" | "src" => src = Some(v.to_owned()),
            "env" => env = Some(v.to_owned()),
            _ => return Err(format!("unexpected key {key:?} in {field:?}")),
        }
    }
    if typ.as_deref() == Some("env") && env.as_deref().is_none_or(str::is_empty) {
        env = src.take();
    }
    if id.is_empty() {
        return Err(format!("secret missing ID in {value:?}"));
    }
    Ok(SecretArg {
        id,
        src: src.filter(|s| !s.is_empty()).map(PathBuf::from),
        env: env.filter(|e| !e.is_empty()),
    })
}

/// An entitlement of `--allow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entitlement {
    /// `network.host`: a `RUN --network=host` may lift `--network=none`.
    NetworkHost,
}

/// An `--allow` value. BuildKit's other entitlements (`security.insecure`,
/// `device`) unlock `RUN` options ranikust refuses anyway.
fn parse_entitlement(value: &str) -> Result<Entitlement, String> {
    match value {
        "network.host" => Ok(Entitlement::NetworkHost),
        "security.insecure" | "device" => Err(format!(
            "--allow {value}: the RUN options it unlocks are not supported"
        )),
        _ => Err(format!("unknown entitlement {value:?} (network.host)")),
    }
}

#[cfg(test)]
mod tests;
