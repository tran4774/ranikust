use std::path::{Path, PathBuf};

use super::*;

/// Parses `args` (without the program name) into `build`'s flags.
fn build(args: &[&str]) -> BuildArgs {
    let argv = std::iter::once("ranikust").chain(args.iter().copied());
    match parse(argv).expect("parses").cli.into_command() {
        Command::Build(args) => *args,
        Command::Version => panic!("expected build"),
    }
}

fn refused(args: &[&str]) -> Vec<String> {
    let argv = std::iter::once("ranikust").chain(args.iter().copied());
    match parse(argv) {
        Err(ParseError::Refused(lines)) => lines,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn clap_error(args: &[&str]) -> clap::Error {
    let argv = std::iter::once("ranikust").chain(args.iter().copied());
    match parse(argv) {
        Err(ParseError::Clap(err)) => err,
        other => panic!("expected a clap error, got {other:?}"),
    }
}

#[test]
fn defaults_follow_kaniko() {
    let args = build(&[]);
    assert_eq!(args.context, "/workspace/");
    assert_eq!(args.dockerfile, PathBuf::from("Dockerfile"));
    assert!(args.destinations.is_empty());
    assert!(!args.no_push);
    assert!(!args.reproducible);
    assert_eq!(args.compression, None);
    assert_eq!(args.snapshot_mode, None);
    assert_eq!(args.mode, Mode::Auto);
    assert_eq!(args.network, Network::Host);
    assert_eq!(args.jobs, None);
}

/// A row of a parsing table: the arguments, and a check of what they set.
type Case = (&'static [&'static str], fn(&BuildArgs) -> bool);

#[test]
fn kaniko_spellings_are_accepted() {
    let cases: &[Case] = &[
        (&["-c", "/ctx"], |a| a.context == "/ctx"),
        (&["--context=/ctx"], |a| a.context == "/ctx"),
        (&["--context", "/ctx"], |a| a.context == "/ctx"),
        (&["-f", "app/Dockerfile"], |a| {
            a.dockerfile == Path::new("app/Dockerfile")
        }),
        (&["--dockerfile=x"], |a| a.dockerfile == Path::new("x")),
        (&["-d", "r/a:1", "--destination=r/b:2"], |a| {
            a.destinations == ["r/a:1", "r/b:2"]
        }),
        (&["--no-push"], |a| a.no_push),
        (&["--no-push=true"], |a| a.no_push),
        (&["--no-push=True"], |a| a.no_push),
        (&["--no-push=1"], |a| a.no_push),
        (&["--no-push=false"], |a| !a.no_push),
        (&["--no-push=0"], |a| !a.no_push),
        (&["--reproducible"], |a| a.reproducible),
        (&["--reproducible=F"], |a| !a.reproducible),
        (&["--tar-path=/out/image.tar"], |a| {
            a.tar_path.as_deref() == Some(Path::new("/out/image.tar"))
        }),
        (&["--oci-layout-path", "/out/layout"], |a| {
            a.oci_layout_path.as_deref() == Some(Path::new("/out/layout"))
        }),
        (&["--build-arg", "A=1", "--build-arg=B"], |a| {
            a.build_args == ["A=1", "B"]
        }),
        (&["--target", "build"], |a| a.targets == ["build"]),
        (&["--compression=zstd"], |a| {
            a.compression == Some(Compression::Zstd)
        }),
        (&["--compression", "gzip"], |a| {
            a.compression == Some(Compression::Gzip)
        }),
        (&["--registry-certificate=r.example=/certs/r.pem"], |a| {
            a.registry_certificates == [("r.example".to_owned(), PathBuf::from("/certs/r.pem"))]
        }),
        (&["--snapshot-mode=redo"], |a| {
            a.snapshot_mode == Some(SnapshotMode::Redo)
        }),
        (&["--snapshot-mode", "time"], |a| {
            a.snapshot_mode == Some(SnapshotMode::Time)
        }),
    ];
    for (args, check) in cases {
        assert!(check(&build(args)), "{args:?}");
    }
}

#[test]
fn ranikust_flags_are_accepted() {
    let args = build(&[
        "--mode=overlay",
        "--workdir=/w",
        "--state-dir",
        "/s",
        "--jobs=4",
        "--network=none",
        "--platform=linux/amd64",
    ]);
    assert_eq!(args.mode, Mode::Overlay);
    assert_eq!(args.workdir.as_deref(), Some(Path::new("/w")));
    assert_eq!(args.state_dir.as_deref(), Some(Path::new("/s")));
    assert_eq!(args.jobs.map(NonZeroUsize::get), Some(4));
    assert_eq!(args.network, Network::None);
    assert_eq!(args.platform.as_deref(), Some("linux/amd64"));
    assert_eq!(build(&["--mode", "kaniko"]).mode, Mode::Kaniko);
}

#[test]
fn the_build_subcommand_takes_the_same_flags() {
    let argv = ["ranikust", "build", "-d", "r/a:1", "--mode=kaniko"];
    let Command::Build(args) = parse(argv).expect("parses").cli.into_command() else {
        panic!("expected build");
    };
    assert_eq!(args.destinations, ["r/a:1"]);
    assert_eq!(args.mode, Mode::Kaniko);
}

#[test]
fn version_is_a_subcommand() {
    let command = parse(["ranikust", "version"])
        .expect("parses")
        .cli
        .into_command();
    assert!(matches!(command, Command::Version));
}

/// `--metrics-file` is the only way to ask for the report (ADR-0037), with
/// any subcommand.
#[test]
fn metrics_file_is_a_flag_of_every_subcommand() {
    for argv in [
        &["ranikust", "version", "--metrics-file=/m.json"][..],
        &["ranikust", "--no-push", "--metrics-file=/m.json"],
        &[
            "ranikust",
            "build",
            "--no-push",
            "--metrics-file",
            "/m.json",
        ],
    ] {
        let parsed = parse(argv.iter().copied()).expect("parses");
        assert_eq!(
            parsed.cli.metrics_file(),
            Some(Path::new("/m.json")),
            "{argv:?}"
        );
    }
}

#[test]
fn bad_values_are_clap_errors() {
    for args in [
        &["--jobs=0"][..],
        &["--jobs=many"],
        &["--mode=fast"],
        &["--snapshot-mode=quick"],
        &["--compression=xz"],
        &["--network=bridge"],
        &["--no-push=yes"],
        &["--registry-certificate=no-equals-sign"],
        &["--registry-certificate==/path"],
        &["--unknown-flag"],
        // A boolean takes its value only after `=`, as in pflag.
        &["--no-push", "true"],
        // Flags and a subcommand do not mix.
        &["-d", "r/a:1", "version"],
    ] {
        let err = clap_error(args);
        assert_eq!(err.exit_code(), 2, "{args:?}: {err}");
    }
}

#[test]
fn help_and_version_flags_exit_zero() {
    assert_eq!(clap_error(&["--help"]).exit_code(), 0);
    assert_eq!(clap_error(&["--version"]).exit_code(), 0);
}

#[test]
fn refused_kaniko_flags_say_why() {
    let cases: &[(&[&str], &str)] = &[
        (&["--kaniko-dir=/kaniko"], "use --state-dir"),
        (&["--cache=true"], "Phase 4"),
        (&["--cache"], "Phase 4"),
        (&["--cache-repo", "r/cache"], "Phase 4"),
        (&["--cache-ttl=1h"], "Phase 4"),
        (&["--compressed-caching"], "Phase 4"),
        (&["--bucket=b"], "out of scope"),
        (&["-b", "b"], "out of scope"),
        (&["-bbucket"], "out of scope"),
        (&["--azure-container-registry-config=x"], "out of scope"),
        (&["--git=branch=main"], "Phase 5"),
        (&["--registry-mirror=m.example"], "Phase 5"),
        (&["--registry-client-cert=r=a,b"], "Phase 5"),
        (&["--push-ignore-immutable-tag-errors"], "Phase 5"),
        (&["--dryrun"], "not scheduled"),
    ];
    for (args, reason) in cases {
        let lines = refused(args);
        assert_eq!(lines.len(), 1, "{args:?}: {lines:?}");
        assert!(lines[0].contains(reason), "{args:?}: {}", lines[0]);
        assert!(lines[0].starts_with("kaniko's --"), "{}", lines[0]);
    }
    let bake = refused(&["bake", "--set", "x"]);
    assert!(bake[0].contains("bake is out of scope"), "{bake:?}");
}

#[test]
fn every_refused_flag_is_reported_at_once() {
    let lines = refused(&["--cache=true", "-d", "r/a:1", "--cache-repo=r/c"]);
    assert_eq!(lines.len(), 2, "{lines:?}");
}

/// Parses `args` and returns the warnings.
fn warnings(args: &[&str]) -> (BuildArgs, Vec<String>) {
    let argv = std::iter::once("ranikust").chain(args.iter().copied());
    let parsed = parse(argv).expect("parses");
    let Command::Build(build) = parsed.cli.into_command() else {
        panic!("expected build");
    };
    (*build, parsed.warnings)
}

/// ADR-0049: a kaniko flag that changes nothing in ranikust is refused with
/// its reason, never ignored; `--cache=false` asks for what ranikust does.
#[test]
fn flags_that_change_nothing_are_refused() {
    let cases: &[(&[&str], &str)] = &[
        (&["--cache-dir", "/cache"], "out of scope"),
        (&["--use-new-run"], "drop the flag"),
        (&["--cleanup=true"], "drop the flag"),
        (&["--pre-cleanup"], "drop the flag"),
        (&["--image-fs-extract-retry=3"], "--image-download-retry"),
        (&["--force-build-metadata"], "deprecated in kaniko"),
        (&["--skip-unused-stages"], "deprecated in kaniko"),
        (&["--compressed-caching=false"], "Phase 4"),
        (&["--no-push-cache"], "Phase 4"),
    ];
    for (args, reason) in cases {
        let lines = refused(args);
        assert_eq!(lines.len(), 1, "{args:?}: {lines:?}");
        assert!(lines[0].contains(reason), "{args:?}: {}", lines[0]);
    }
    let (_, w) = warnings(&["--cache=false", "--no-push"]);
    assert!(w.is_empty(), "{w:?}");
}

/// The kaniko-mode flags of Phase 2 (ADR-0040), with kaniko's defaults and
/// `--whitelist-var-run` as the deprecated name of `--ignore-var-run`.
#[test]
fn kaniko_mode_flags_parse_with_kanikos_defaults() {
    let (args, w) = warnings(&["--no-push"]);
    assert!(w.is_empty(), "{w:?}");
    assert!(args.ignore_paths.is_empty());
    assert_eq!(args.ignore_var_run, None);
    assert!(!args.single_snapshot && !args.materialize && !args.force);
    assert_eq!(args.snapshot_workers, None);
    let (args, w) = warnings(&[
        "--ignore-path=/a",
        "--ignore-path",
        "/b/../c",
        "--ignore-var-run=false",
        "--single-snapshot",
        "--materialize=true",
        "--snapshot-workers=3",
        "--force",
        "--no-push",
    ]);
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(
        args.ignore_paths,
        [PathBuf::from("/a"), PathBuf::from("/b/../c")]
    );
    assert_eq!(args.ignore_var_run, Some(false));
    assert!(args.single_snapshot && args.materialize && args.force);
    assert_eq!(args.snapshot_workers.map(NonZeroUsize::get), Some(3));
    let (args, w) = warnings(&["--whitelist-var-run=false", "--no-push"]);
    assert_eq!(args.ignore_var_run, Some(false));
    assert!(w[0].contains("use --ignore-var-run"), "{w:?}");
    let lines = refused(&["--whitelist-var-run", "--ignore-var-run", "--no-push"]);
    assert!(lines[0].contains("name the same setting"), "{lines:?}");
    assert_eq!(
        clap_error(&["--snapshot-workers=0", "--no-push"]).exit_code(),
        2
    );
}

#[test]
fn two_spellings_of_one_flag_are_refused_naming_both() {
    let lines = refused(&[
        "--platform=linux/amd64",
        "--custom-platform=linux/amd64",
        "--no-push",
    ]);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("--custom-platform and --platform"),
        "{lines:?}"
    );
}

/// kaniko's deprecated spellings and `--custom-platform` (Q-020).
#[test]
fn old_spellings_and_custom_platform_are_aliases() {
    let (args, w) = warnings(&[
        "--tarPath=/t.tar",
        "--snapshotMode",
        "redo",
        "--customPlatform=linux/amd64",
        "--no-push",
    ]);
    assert_eq!(args.tar_path.as_deref(), Some(Path::new("/t.tar")));
    assert_eq!(args.snapshot_mode, Some(SnapshotMode::Redo));
    assert_eq!(args.platform.as_deref(), Some("linux/amd64"));
    assert_eq!(w.len(), 3, "{w:?}");
    assert!(w[0].contains("use --tar-path"), "{w:?}");
    let (args, w) = warnings(&["--custom-platform", "linux/amd64", "--no-push"]);
    assert_eq!(args.platform.as_deref(), Some("linux/amd64"));
    assert!(w.is_empty(), "{w:?}");
}

#[test]
fn logging_flags_take_kanikos_values() {
    let a = build(&[
        "-v",
        "debug",
        "--log-format=json",
        "--log-timestamp",
        "--no-push",
    ]);
    assert_eq!(a.verbosity, Verbosity::Debug);
    assert_eq!(a.log_format, LogFormatArg::Json);
    assert!(a.log_timestamp);
    let a = build(&["--no-push"]);
    assert_eq!(a.verbosity, Verbosity::Info);
    assert_eq!(a.log_format, LogFormatArg::Color);
    assert!(!a.log_timestamp, "off by default, as kaniko");
    assert_eq!(clap_error(&["--verbosity=loud"]).exit_code(), 2);
}

#[test]
fn compression_level_takes_negative_numbers() {
    let a = build(&["--compression-level", "-1", "--no-push"]);
    assert_eq!(a.compression_level, Some(-1));
}

#[test]
fn scanning_stops_at_double_dash_and_ignores_values() {
    // A value that looks like a refused flag is still a value.
    let args = build(&["--build-arg=X=--cache"]);
    assert_eq!(args.build_args, ["X=--cache"]);
    // After `--`, nothing is a flag; clap rejects the stray argument itself.
    let err = clap_error(&["--", "--cache"]);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn pflag_booleans() {
    for value in ["1", "t", "T", "true", "TRUE", "True"] {
        assert_eq!(parse_bool(value), Ok(true), "{value}");
    }
    for value in ["0", "f", "F", "false", "FALSE", "False"] {
        assert_eq!(parse_bool(value), Ok(false), "{value}");
    }
    assert!(parse_bool("yes").is_err());
    assert!(parse_bool("").is_err());
}

#[test]
fn insecure_registries_are_hosts() {
    let a = build(&[
        "--no-push",
        "--insecure-registry=registry:5000",
        "--insecure-registry",
        "r.example",
    ]);
    assert_eq!(a.insecure_registries, ["registry:5000", "r.example"]);
    assert!(parse_registry_host("http://r.example").is_err());
    assert!(parse_registry_host("").is_err());
    // kaniko's other insecure and TLS flags (ADR-0036).
    let a = build(&[
        "--no-push",
        "--insecure",
        "--insecure-pull=true",
        "--skip-tls-verify",
        "--skip-tls-verify-pull",
        "--skip-tls-verify-registry=r.example:5443",
    ]);
    assert!(a.insecure && a.insecure_pull && a.skip_tls_verify && a.skip_tls_verify_pull);
    assert_eq!(a.skip_tls_verify_registries, ["r.example:5443"]);
}

fn secret(id: &str, src: Option<&str>, env: Option<&str>) -> SecretArg {
    SecretArg {
        id: id.to_owned(),
        src: src.map(PathBuf::from),
        env: env.map(str::to_owned),
    }
}

/// buildctl's `parseSecret`, which kaniko copies.
#[test]
fn secrets_parse_as_buildctl() {
    let p = |v| parse_secret(v).unwrap();
    assert_eq!(
        p("id=npm,src=/run/npmrc"),
        secret("npm", Some("/run/npmrc"), None)
    );
    assert_eq!(p("source=/a,id=b"), secret("b", Some("/a"), None));
    assert_eq!(p("ID=c,Type=file,SRC=/c"), secret("c", Some("/c"), None));
    // type=env takes the variable from env=, else from src=.
    assert_eq!(p("id=a,type=env,env=A"), secret("a", None, Some("A")));
    assert_eq!(p("id=a,type=env,src=A"), secret("a", None, Some("A")));
    assert_eq!(p("id=a,env=A"), secret("a", None, Some("A")));
    // No source: decided by the environment later.
    assert_eq!(p("id=a"), secret("a", None, None));
    for bad in ["src=/a", "id=a,type=ssh", "id=a,src", "id=a,mode=0400"] {
        assert!(parse_secret(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn allow_takes_buildkit_entitlements() {
    assert_eq!(
        parse_entitlement("network.host").unwrap(),
        Entitlement::NetworkHost
    );
    assert_eq!(
        parse_entitlement("network.none-userns").unwrap(),
        Entitlement::NetworkNoneUserns
    );
    assert!(parse_entitlement("security.insecure").is_err());
    assert!(parse_entitlement("network").is_err());
}
