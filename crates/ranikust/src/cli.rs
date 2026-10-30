//! Command-line parsing: kaniko's executor flags plus ranikust's own.
//!
//! `ranikust` with flags and no subcommand builds, as kaniko's `executor`
//! does, so a pipeline can swap the binary and keep its flags. Every accepted
//! kaniko flag keeps kaniko's name, short form and meaning (ADR-0011); kaniko
//! flags that ranikust does not accept yet are recognised and refused with a
//! reason, never silently ignored.

use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::config::{
    Compression, ImageFormatArg, LogFormatArg, Mode, Network, SnapshotMode, Verbosity,
};

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

    /// Write the metrics report (JSON) to this path when ranikust exits,
    /// also after an error or a panic (ADR-0025, ADR-0037). Any subcommand.
    #[arg(long, global = true, value_name = "PATH", help_heading = "ranikust")]
    metrics_file: Option<PathBuf>,
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
    /// `--metrics-file`, the only way to ask for the report file.
    pub(crate) fn metrics_file(&self) -> Option<&Path> {
        self.metrics_file.as_deref()
    }

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

    /// A sub-directory of a `dir://` context to build from, as kaniko;
    /// ignored, with a warning, for a plain path, which kaniko ignores too.
    #[arg(long, value_name = "DIR", help_heading = "Input")]
    pub(crate) context_sub_path: Option<String>,

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

    /// Compression level: gzip -1 to 9 (-1 is 6), zstd 1 to 19.
    #[arg(
        long,
        value_name = "LEVEL",
        allow_negative_numbers = true,
        help_heading = "Output"
    )]
    pub(crate) compression_level: Option<i32>,

    /// Media types of the image [default: oci].
    #[arg(long, value_enum, help_heading = "Output")]
    pub(crate) image_format: Option<ImageFormatArg>,

    /// A label `KEY=VALUE` on the pushed image's config; a `LABEL` of the
    /// same key in its stage wins. Repeat for several.
    #[arg(long = "label", value_name = "KEY=VALUE", help_heading = "Output")]
    pub(crate) labels: Vec<String>,

    /// An annotation `KEY=VALUE` on the image manifest. Repeat for several.
    #[arg(long = "annotation", value_name = "KEY=VALUE", help_heading = "Output")]
    pub(crate) annotations: Vec<String>,

    /// Write the image digest to this file.
    #[arg(long, value_name = "PATH", help_heading = "Output")]
    pub(crate) digest_file: Option<PathBuf>,

    /// Write `repository@digest`, one line per destination, to this file.
    #[arg(long, value_name = "PATH", help_heading = "Output")]
    pub(crate) image_name_with_digest_file: Option<PathBuf>,

    /// Write `repository:tag@digest`, one line per destination, to this file.
    #[arg(long, value_name = "PATH", help_heading = "Output")]
    pub(crate) image_name_tag_with_digest_file: Option<PathBuf>,

    /// Retries of a failed push, waiting 1 s, then twice as long each time
    /// [default: ranikust's retry policy].
    #[arg(long, value_name = "N", help_heading = "Output")]
    pub(crate) push_retry: Option<u32>,

    /// Retries of a failed base image pull, as --push-retry.
    #[arg(long, value_name = "N", help_heading = "Input")]
    pub(crate) image_download_retry: Option<u32>,

    /// Destinations may be plain HTTP.
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
    pub(crate) insecure: bool,

    /// Images pulled may come over plain HTTP.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = parse_bool,
        help_heading = "Input"
    )]
    pub(crate) insecure_pull: bool,

    /// Do not verify the TLS certificates of destinations.
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
    pub(crate) skip_tls_verify: bool,

    /// Do not verify the TLS certificates of registries pulled from.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = parse_bool,
        help_heading = "Input"
    )]
    pub(crate) skip_tls_verify_pull: bool,

    /// Do not verify the TLS certificate of this registry, pushing or
    /// pulling. Repeat for several.
    #[arg(
        long = "skip-tls-verify-registry",
        value_name = "REGISTRY",
        value_parser = parse_registry_host,
        help_heading = "Output"
    )]
    pub(crate) skip_tls_verify_registries: Vec<String>,

    /// Accepted: ranikust makes no write check before the build.
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
    pub(crate) skip_push_permission_check: bool,

    /// Accepted: overlay mode never touches the context.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = parse_bool,
        help_heading = "Input"
    )]
    pub(crate) preserve_context: bool,

    /// Log level: trace, debug, info, warn, error, fatal, panic.
    #[arg(
        short = 'v',
        long,
        value_enum,
        ignore_case = true,
        default_value_t = Verbosity::Info,
        help_heading = "Logging"
    )]
    pub(crate) verbosity: Verbosity,

    /// Log format, as kaniko's logrus: text, color (forced), json.
    #[arg(long, value_enum, default_value_t = LogFormatArg::Color, help_heading = "Logging")]
    pub(crate) log_format: LogFormatArg,

    /// The full time in colored logs instead of seconds since start.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = parse_bool,
        help_heading = "Logging"
    )]
    pub(crate) log_timestamp: bool,

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
    /// `--insecure-registry`: `host[:port]`. Repeat for several. Its
    /// credentials may go over plain HTTP (threat model T11).
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

    /// A path kept out of every kaniko-mode snapshot, as in kaniko; absolute.
    /// Repeat for several. Refused in overlay mode (Q-260).
    #[arg(long = "ignore-path", value_name = "PATH", help_heading = "Snapshots")]
    pub(crate) ignore_paths: Vec<PathBuf>,

    /// Keep `/var/run` out of kaniko-mode snapshots, as in kaniko
    /// [default: true]; `false` is refused in overlay mode (Q-260).
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_parser = parse_bool,
        help_heading = "Snapshots"
    )]
    pub(crate) ignore_var_run: Option<bool>,

    /// Take one snapshot at the end of the build, as in kaniko; refused in
    /// overlay mode (Q-260).
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = parse_bool,
        help_heading = "Snapshots"
    )]
    pub(crate) single_snapshot: bool,

    /// Make the final filesystem match the target even on full cache hits,
    /// as in kaniko; without a layer cache (Phase 4) it changes nothing.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = parse_bool,
        help_heading = "Snapshots"
    )]
    pub(crate) materialize: bool,

    /// Workers of the kaniko-mode walk (ADR-0039); ignored in overlay mode
    /// [default: 8].
    #[arg(long, value_name = "N", help_heading = "Snapshots")]
    pub(crate) snapshot_workers: Option<NonZeroUsize>,

    /// Build on the root `/` even outside a container, as kaniko's
    /// `--force`: kaniko mode then empties this machine's `/` (ADR-0040).
    /// Changes nothing with `--workdir`, whose checks apply instead.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = parse_bool,
        help_heading = "ranikust"
    )]
    pub(crate) force: bool,

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
    /// `RUN --network=host` lift `--network=none`; `network.none-userns`
    /// lets a `RUN --network=none` get loopback through a user namespace
    /// where ranikust lacks `CAP_NET_ADMIN` (ADR-0033). Repeat for several.
    #[arg(
        long = "allow",
        value_name = "ENTITLEMENT",
        value_parser = parse_entitlement,
        help_heading = "ranikust"
    )]
    pub(crate) allow: Vec<Entitlement>,
}

/// Why a kaniko flag is refused (ADR-0036).
#[derive(Clone, Copy, Debug)]
enum Refusal {
    /// The flag has a new name in ranikust.
    Renamed(&'static str),
    /// The flag comes in a later phase.
    NotYet(&'static str),
    /// The feature is out of scope (ADR-0008).
    OutOfScope(&'static str),
}

const PHASE_4: &str = "the layer cache comes in Phase 4 (Q-021)";
const PHASE_5: &str = "it comes in Phase 5 with registry mirrors and credential helpers (Q-047)";

/// kaniko executor flags that ranikust recognises but does not accept
/// (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go`, ADR-0036). `--cache` is
/// here for `--cache` and `--cache=true`; `--cache=false` is accepted.
const REFUSED: &[(&str, Refusal)] = &[
    ("kaniko-dir", Refusal::Renamed("use --state-dir")),
    ("cache", Refusal::NotYet(PHASE_4)),
    ("cache-repo", Refusal::NotYet(PHASE_4)),
    ("cache-ttl", Refusal::NotYet(PHASE_4)),
    ("cache-copy-layers", Refusal::NotYet(PHASE_4)),
    ("cache-run-layers", Refusal::NotYet(PHASE_4)),
    ("compressed-caching", Refusal::NotYet(PHASE_4)),
    (
        "git",
        Refusal::NotYet("git contexts come in Phase 5 (ADR-0035)"),
    ),
    ("registry-mirror", Refusal::NotYet(PHASE_5)),
    ("registry-map", Refusal::NotYet(PHASE_5)),
    ("credential-helpers", Refusal::NotYet(PHASE_5)),
    ("skip-default-registry-fallback", Refusal::NotYet(PHASE_5)),
    ("registry-client-cert", Refusal::NotYet(PHASE_5)),
    ("push-ignore-immutable-tag-errors", Refusal::NotYet(PHASE_5)),
    (
        "dryrun",
        Refusal::NotYet("not scheduled yet; the stage plan output comes in a later phase"),
    ),
    (
        "bucket",
        Refusal::OutOfScope("GCS contexts are out of scope (ADR-0008)"),
    ),
    (
        "azure-container-registry-config",
        Refusal::OutOfScope("cloud-specific registry configuration is out of scope (ADR-0008)"),
    ),
];

/// kaniko's short flags among [`REFUSED`].
const REFUSED_SHORT: &[(char, &str)] = &[('b', "bucket")];

/// kaniko subcommands ranikust refuses, with the reason.
const REFUSED_COMMANDS: &[(&str, &str)] = &[(
    "bake",
    "kaniko's bake is out of scope (ADR-0008); run one build per image",
)];

/// kaniko flags accepted and ignored with a warning, because ignoring them
/// leaves the image's bytes unchanged in overlay mode (ADR-0036, which
/// extends ADR-0023's rule for `--snapshot-mode`): `(name, takes a value,
/// why)`. A flag taking a value consumes the next argument when it has no
/// `=`; a boolean never does, as in pflag.
const IGNORED: &[(&str, bool, &str)] = &[
    (
        "cache-dir",
        true,
        "the warmer is out of scope (ADR-0008); base images are pulled into the store",
    ),
    (
        "use-new-run",
        false,
        "overlay mode captures each RUN without a walk",
    ),
    (
        "cleanup",
        false,
        "overlay mode leaves no build filesystem on /; the store in --state-dir is kept on purpose",
    ),
    (
        "pre-cleanup",
        false,
        "overlay mode leaves no build filesystem on /",
    ),
    (
        "image-fs-extract-retry",
        true,
        "layers are extracted from the store; pulls are retried by --image-download-retry",
    ),
    (
        "force-build-metadata",
        false,
        "deprecated in kaniko, which ignores it too",
    ),
    (
        "skip-unused-stages",
        false,
        "deprecated in kaniko; unused stages are always skipped",
    ),
];

/// kaniko's deprecated spellings, accepted with kaniko's warning
/// (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:456-466`), and aliases:
/// `(old, new, warn)`.
const RENAMED: &[(&str, &str, bool)] = &[
    ("tarPath", "tar-path", true),
    ("snapshotMode", "snapshot-mode", true),
    ("customPlatform", "platform", true),
    // `kaniko@44aa7ddc0 cmd/executor/cmd/root.go:67-68`.
    ("whitelist-var-run", "ignore-var-run", true),
    // Q-020: kaniko's name for --platform.
    ("custom-platform", "platform", false),
];

/// A parse failure: kaniko flags refused with a reason, or clap's own error.
#[derive(Debug)]
pub(crate) enum ParseError {
    /// kaniko flags ranikust does not accept; one line each.
    Refused(Vec<String>),
    /// clap's error, including `--help` and `--version`, which exit 0.
    Clap(clap::Error),
}

/// The parsed command line, and the warnings parsing it gave: ignored and
/// deprecated kaniko flags, to log once logging is set up.
#[derive(Debug)]
pub(crate) struct Parsed {
    pub(crate) cli: Cli,
    pub(crate) warnings: Vec<String>,
}

/// Parses the command line. Refused kaniko flags are reported, all of them,
/// before clap sees the arguments; ignored ones and deprecated spellings are
/// taken out or renamed, each with a warning.
pub(crate) fn parse<I, T>(args: I) -> Result<Parsed, ParseError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let pass = prepass(&args);
    if !pass.refused.is_empty() {
        return Err(ParseError::Refused(pass.refused));
    }
    Cli::try_parse_from(pass.args)
        .map(|cli| Parsed {
            cli,
            warnings: pass.warnings,
        })
        .map_err(ParseError::Clap)
}

/// What [`prepass`] made of the arguments.
struct Prepass {
    /// The arguments for clap: ignored flags taken out, old spellings
    /// renamed.
    args: Vec<OsString>,
    refused: Vec<String>,
    warnings: Vec<String>,
}

/// Sorts kaniko's flags before clap sees them (the program name kept, the
/// rest after `--` untouched).
fn prepass(all: &[OsString]) -> Prepass {
    let mut out = Prepass {
        args: Vec::with_capacity(all.len()),
        refused: Vec::new(),
        warnings: Vec::new(),
    };
    let mut iter = all.iter();
    if let Some(program) = iter.next() {
        out.args.push(program.clone());
    }
    let mut first = true;
    // Each long flag as written and the name clap gets, to catch two
    // spellings of one flag.
    let mut names: Vec<(&str, &str)> = Vec::new();
    while let Some(arg) = iter.next() {
        let Some(text) = arg.to_str() else {
            out.args.push(arg.clone());
            first = false;
            continue;
        };
        if text == "--" {
            out.args.push(arg.clone());
            out.args.extend(iter.cloned());
            break;
        }
        if first && let Some((_, why)) = REFUSED_COMMANDS.iter().find(|(c, _)| *c == text) {
            out.refused
                .push(format!("kaniko's {text} is not supported: {why}"));
        }
        first = false;
        let (name, value) = if let Some(long) = text.strip_prefix("--") {
            match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            }
        } else if let Some(short) = text.strip_prefix('-') {
            match short
                .chars()
                .next()
                .and_then(|c| REFUSED_SHORT.iter().find(|(s, _)| *s == c))
            {
                Some((_, long)) => (*long, None),
                None => {
                    out.args.push(arg.clone());
                    continue;
                }
            }
        } else {
            out.args.push(arg.clone());
            continue;
        };
        // `--cache=false` asks for exactly what Phase 1 does (Q-021).
        if name == "cache" && value.is_some_and(|v| parse_bool(v) == Ok(false)) {
            continue;
        }
        // Without a layer cache, not compressing or not pushing it changes
        // nothing; `--compressed-caching` (true, kaniko's default) without a
        // value or `=true` still names the cache and is refused below.
        if (name == "compressed-caching" && value.is_some_and(|v| parse_bool(v) == Ok(false)))
            || name == "no-push-cache"
        {
            out.warnings.push(format!(
                "--{name} is ignored: the layer cache comes in Phase 4"
            ));
            continue;
        }
        if let Some((name, refusal)) = REFUSED.iter().find(|(n, _)| *n == name) {
            let reason = match refusal {
                Refusal::Renamed(why) => format!("renamed in ranikust: {why}"),
                Refusal::NotYet(why) | Refusal::OutOfScope(why) => (*why).to_owned(),
            };
            out.refused
                .push(format!("kaniko's --{name} is not supported: {reason}"));
            continue;
        }
        if let Some((name, takes_value, why)) = IGNORED.iter().find(|(n, _, _)| *n == name) {
            if *takes_value && value.is_none() {
                // The value is the next argument.
                iter.next();
            }
            out.warnings.push(format!("--{name} is ignored: {why}"));
            continue;
        }
        if let Some((old, new, warn)) = RENAMED.iter().find(|(o, _, _)| *o == name) {
            if *warn {
                out.warnings
                    .push(format!("--{old} is deprecated in kaniko, use --{new}"));
            }
            names.push((*old, *new));
            out.args.push(match value {
                Some(v) => OsString::from(format!("--{new}={v}")),
                None => OsString::from(format!("--{new}")),
            });
            continue;
        }
        names.push((name, name));
        out.args.push(arg.clone());
    }
    // Two spellings of one flag: clap would only say it was given twice.
    let mut targets: Vec<&str> = RENAMED.iter().map(|(_, n, _)| *n).collect();
    targets.sort_unstable();
    targets.dedup();
    for new in targets {
        let mut given: Vec<&str> = names
            .iter()
            .filter(|(_, n)| *n == new)
            .map(|(o, _)| *o)
            .collect();
        given.sort_unstable();
        given.dedup();
        if given.len() > 1 {
            let spellings: Vec<String> = given.iter().map(|g| format!("--{g}")).collect();
            out.refused.push(format!(
                "{} name the same setting: give it once",
                spellings.join(" and ")
            ));
        }
    }
    out
}

/// pflag's boolean spellings (`strconv.ParseBool`), so `--no-push=true` and
/// `--no-push=1` mean what they mean to kaniko.
pub(crate) fn parse_bool(value: &str) -> Result<bool, String> {
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
    // Docker Hub's legacy name is the registry `Reference` calls docker.io.
    Ok(if value == "index.docker.io" {
        rnk_oci::Reference::DOCKER_HUB.to_owned()
    } else {
        value.to_owned()
    })
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
    /// `network.none-userns`: without `CAP_NET_ADMIN`, a `RUN
    /// --network=none` gets loopback in a network namespace a user namespace
    /// owns, under a seccomp filter (ADR-0033). ranikust's own.
    NetworkNoneUserns,
}

/// An `--allow` value. BuildKit's other entitlements (`security.insecure`,
/// `device`) unlock `RUN` options ranikust refuses anyway.
fn parse_entitlement(value: &str) -> Result<Entitlement, String> {
    match value {
        "network.host" => Ok(Entitlement::NetworkHost),
        "network.none-userns" => Ok(Entitlement::NetworkNoneUserns),
        "security.insecure" | "device" => Err(format!(
            "--allow {value}: the RUN options it unlocks are not supported"
        )),
        _ => Err(format!(
            "unknown entitlement {value:?} (network.host, network.none-userns)"
        )),
    }
}

#[cfg(test)]
mod tests;
