use std::path::{Path, PathBuf};

use super::*;

/// `build`'s flags from `args`, after `--no-push` unless they push.
fn args(args: &[&str]) -> BuildArgs {
    let argv = std::iter::once("ranikust").chain(args.iter().copied());
    match crate::cli::parse(argv).expect("parses").into_command() {
        crate::cli::Command::Build(args) => *args,
        crate::cli::Command::Version => panic!("expected build"),
    }
}

fn config(flags: &[&str]) -> Result<BuildConfig, Invalid> {
    BuildConfig::from_args(&args(flags))
}

#[test]
fn a_destination_is_required_unless_no_push() {
    let err = config(&[]).expect_err("no destination");
    assert!(err.0.contains("--destination"), "{}", err.0);
    assert!(config(&["--no-push"]).is_ok());
    assert!(config(&["-d", "r/a:1"]).is_ok());
}

#[test]
fn zstd_cannot_go_into_a_docker_tarball() {
    let err = config(&["--no-push", "--tar-path=/t.tar", "--compression=zstd"])
        .expect_err("zstd with tar");
    assert!(err.0.contains("--oci-layout-path"), "{}", err.0);
    assert!(config(&["--no-push", "--oci-layout-path=/l", "--compression=zstd"]).is_ok());
    assert!(config(&["--no-push", "--tar-path=/t.tar", "--compression=gzip"]).is_ok());
}

#[test]
fn only_linux_amd64() {
    let ok = config(&["--no-push"]).expect("default platform");
    assert_eq!(ok.platform, "linux/amd64");
    assert!(config(&["--no-push", "--platform=linux/amd64"]).is_ok());
    let err = config(&["--no-push", "--platform=linux/arm64"]).expect_err("arm64");
    assert!(err.0.contains("ADR-0009"), "{}", err.0);
}

#[test]
fn contexts_are_local_directories() {
    let ok = config(&["--no-push", "--context=dir:///src"]).expect("dir://");
    assert_eq!(ok.context, Path::new("/src"));
    for (context, why) in [
        ("git://github.com/a/b", "Phase 5"),
        ("tar:///ctx.tar.gz", "Phase 5"),
        ("https://example.com/ctx.tar.gz", "Phase 5"),
        ("s3://bucket/ctx.tar.gz", "out of scope"),
        ("gs://bucket/ctx.tar.gz", "out of scope"),
    ] {
        let flag = format!("--context={context}");
        let err = config(&["--no-push", &flag]).expect_err(context);
        assert!(err.0.contains(why), "{context}: {}", err.0);
    }
    assert!(config(&["--no-push", "--context="]).is_err());
}

#[test]
fn build_args_with_and_without_values() {
    let ok = config(&[
        "--no-push",
        "--build-arg=A=1",
        "--build-arg=B",
        "--build-arg=C=",
    ])
    .expect("build args");
    assert_eq!(
        ok.build_args,
        [
            BuildArg {
                name: "A".to_owned(),
                value: Some("1".to_owned())
            },
            BuildArg {
                name: "B".to_owned(),
                value: None
            },
            BuildArg {
                name: "C".to_owned(),
                value: Some(String::new())
            },
        ]
    );
    assert!(config(&["--no-push", "--build-arg==1"]).is_err());
}

#[test]
fn empty_targets_are_refused() {
    assert!(config(&["--no-push", "--target=a,,b"]).is_err());
    let ok = config(&["--no-push", "--target=a,b"]).expect("targets");
    assert_eq!(ok.targets, ["a", "b"]);
}

#[test]
fn paths_become_absolute_without_touching_the_filesystem() {
    let ok = config(&[
        "--no-push",
        "--context=ctx",
        "--workdir=work",
        "--state-dir=state",
        "--tar-path=out/i.tar",
        "--registry-certificate=r.example=certs/r.pem",
    ])
    .expect("relative paths");
    let cwd = std::env::current_dir().expect("cwd");
    assert_eq!(ok.context, cwd.join("ctx"));
    assert_eq!(ok.workdir, Some(cwd.join("work")));
    assert_eq!(ok.state_dir, Some(cwd.join("state")));
    assert_eq!(ok.tar_path, Some(cwd.join("out/i.tar")));
    assert_eq!(ok.registry_certificates[0].1, cwd.join("certs/r.pem"));
    // The Dockerfile is resolved against the context later.
    assert_eq!(ok.dockerfile, PathBuf::from("Dockerfile"));
}

#[test]
fn state_dir_defaults_per_mode() {
    let with_workdir = config(&["--no-push", "--workdir=/w"]).expect("workdir");
    assert_eq!(
        with_workdir.state_dir_for(Mode::Overlay),
        Some(PathBuf::from("/w/.ranikust"))
    );
    assert_eq!(
        with_workdir.state_dir_for(Mode::Kaniko),
        Some(PathBuf::from("/ranikust/state"))
    );
    assert_eq!(with_workdir.state_dir_for(Mode::Auto), None);
    let bare = config(&["--no-push"]).expect("no workdir");
    assert_eq!(bare.state_dir_for(Mode::Overlay), None);
    let explicit = config(&["--no-push", "--workdir=/w", "--state-dir=/s"]).expect("state dir");
    for mode in [Mode::Auto, Mode::Overlay, Mode::Kaniko] {
        assert_eq!(explicit.state_dir_for(mode), Some(PathBuf::from("/s")));
    }
}

#[test]
fn snapshot_mode_and_compression_default() {
    let ok = config(&["--no-push"]).expect("defaults");
    assert_eq!(ok.snapshot_mode, SnapshotMode::Full);
    assert_eq!(ok.compression, Compression::Gzip);
    assert_eq!(ok.mode, Mode::Auto);
}

#[test]
fn meta_options_hold_no_names_paths_or_arguments() {
    let ok = config(&[
        "-d",
        "registry.example/secret-team/app:1",
        "--build-arg=TOKEN=hunter2",
        "--tar-path=/private/out.tar",
        "--registry-certificate=r.example=/private/r.pem",
        "--jobs=3",
        "--target=build",
    ])
    .expect("config");
    let options = ok.meta_options();
    let text = options.to_string();
    for leaked in [
        "secret-team",
        "hunter2",
        "TOKEN",
        "/private",
        "r.example",
        "build",
    ] {
        assert!(!text.contains(leaked), "{leaked} in {text}");
    }
    assert_eq!(options["jobs"], 3);
    assert_eq!(options["outputs"]["destinations"], 1);
    assert_eq!(options["outputs"]["tar"], true);
    assert_eq!(options["targets"], 1);
    assert_eq!(options["mode"], "auto");
    assert_eq!(options["compression"], "gzip");
}

#[test]
fn value_names_match_the_flags() {
    assert_eq!(name(Mode::Kaniko), "kaniko");
    assert_eq!(name(SnapshotMode::Redo), "redo");
    assert_eq!(name(Network::None), "none");
    assert_eq!(name(Compression::Zstd), "zstd");
}
