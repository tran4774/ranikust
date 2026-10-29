use std::path::{Path, PathBuf};

use super::*;

/// Parses `args` (without the program name) into `build`'s flags.
fn build(args: &[&str]) -> BuildArgs {
    let argv = std::iter::once("ranikust").chain(args.iter().copied());
    match parse(argv).expect("parses").into_command() {
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
        (&["--target=a,b", "--target", "c"], |a| {
            a.targets == ["a", "b", "c"]
        }),
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
        "--metrics-file=/m.json",
    ]);
    assert_eq!(args.mode, Mode::Overlay);
    assert_eq!(args.workdir.as_deref(), Some(Path::new("/w")));
    assert_eq!(args.state_dir.as_deref(), Some(Path::new("/s")));
    assert_eq!(args.jobs.map(NonZeroUsize::get), Some(4));
    assert_eq!(args.network, Network::None);
    assert_eq!(args.platform.as_deref(), Some("linux/amd64"));
    assert_eq!(args.metrics_file.as_deref(), Some(Path::new("/m.json")));
    assert_eq!(build(&["--mode", "kaniko"]).mode, Mode::Kaniko);
}

#[test]
fn the_build_subcommand_takes_the_same_flags() {
    let argv = ["ranikust", "build", "-d", "r/a:1", "--mode=kaniko"];
    let Command::Build(args) = parse(argv).expect("parses").into_command() else {
        panic!("expected build");
    };
    assert_eq!(args.destinations, ["r/a:1"]);
    assert_eq!(args.mode, Mode::Kaniko);
}

#[test]
fn version_is_a_subcommand() {
    let command = parse(["ranikust", "version"])
        .expect("parses")
        .into_command();
    assert!(matches!(command, Command::Version));
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
        (&["--custom-platform=linux/arm64"], "use --platform"),
        (&["--cache=true"], "Phase 4"),
        (&["--cache-repo", "r/cache"], "Phase 4"),
        (&["--ignore-path=/var"], "Phase 2"),
        (&["--secret", "id=a,src=/a"], "RUN --mount=type=secret"),
        (&["--bucket=b"], "out of scope"),
        (&["-b", "b"], "out of scope"),
        (&["-bbucket"], "out of scope"),
        (&["--git=branch=main"], "Phase 5"),
        (&["--registry-mirror=m.example"], "Phase 5"),
        (&["--force"], "workdir"),
        (&["--cache-dir=/cache"], "--state-dir"),
        (&["--verbosity=debug"], "Q-023"),
        (&["--label", "a=b"], "Q-023"),
        (&["--image-format=oci"], "Q-023"),
        (&["--skip-tls-verify"], "Q-023"),
        (&["--use-new-run"], "Q-023"),
        (&["--snapshotMode=redo"], "use --snapshot-mode"),
        (&["--tarPath=/x"], "use --tar-path"),
        (&["--skip-unused-stages"], "deprecated in kaniko: drop it"),
    ];
    for (args, reason) in cases {
        let lines = refused(args);
        assert_eq!(lines.len(), 1, "{args:?}: {lines:?}");
        assert!(lines[0].contains(reason), "{args:?}: {}", lines[0]);
        assert!(lines[0].starts_with("kaniko's --"), "{}", lines[0]);
    }
}

#[test]
fn every_refused_flag_is_reported_at_once() {
    let lines = refused(&["--cache=true", "-d", "r/a:1", "--verbosity=info"]);
    assert_eq!(lines.len(), 2, "{lines:?}");
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
