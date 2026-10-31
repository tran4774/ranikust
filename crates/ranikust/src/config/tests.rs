use std::path::{Path, PathBuf};

use super::*;

/// `build`'s flags from `args`, after `--no-push` unless they push.
fn args(args: &[&str]) -> BuildArgs {
    let argv = std::iter::once("ranikust").chain(args.iter().copied());
    match crate::cli::parse(argv).expect("parses").cli.into_command() {
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

/// kaniko's ranges, mapped to the encoder's (ADR-0036).
#[test]
fn compression_levels_follow_kaniko_where_the_encoder_can() {
    let level = |flags: &[&str]| config(flags).map(|c| c.compression_level);
    assert_eq!(level(&["--no-push"]), Ok(None));
    assert_eq!(level(&["--no-push", "--compression-level=-1"]), Ok(Some(6)));
    assert_eq!(level(&["--no-push", "--compression-level=9"]), Ok(Some(9)));
    assert_eq!(
        level(&["--no-push", "--compression=zstd", "--compression-level=19"]),
        Ok(Some(19))
    );
    for (flags, why) in [
        (&["--no-push", "--compression-level=-2"][..], "Huffman"),
        (
            &["--no-push", "--compression-level=10"],
            "out of range for gzip",
        ),
        (
            &["--no-push", "--compression=zstd", "--compression-level=22"],
            "not supported for zstd",
        ),
        (
            &["--no-push", "--compression=zstd", "--compression-level=0"],
            "out of range for zstd",
        ),
    ] {
        let err = level(flags).expect_err("refused");
        assert!(err.0.contains(why), "{flags:?}: {}", err.0);
    }
}

#[test]
fn image_format_labels_annotations_and_digest_files() {
    let c = config(&[
        "-d",
        "r.example/a:1",
        "--image-format=docker",
        "--label=a=b=c",
        "--annotation",
        "org.example=1",
        "--digest-file=d",
        "--image-name-with-digest-file=n",
    ])
    .expect("valid");
    assert_eq!(c.image_format, Some(ImageFormatArg::Docker));
    assert_eq!(c.labels, [("a".to_owned(), "b=c".to_owned())]);
    assert_eq!(c.annotations["org.example"], "1");
    assert!(c.digest_file.as_deref().is_some_and(Path::is_absolute));
    // kaniko `root.go:178`, `:212`.
    let err = config(&["--no-push", "--image-format=docker", "--compression=zstd"])
        .expect_err("zstd docker");
    assert!(err.0.contains("--image-format=oci"), "{}", err.0);
    let err = config(&["--no-push", "--image-name-with-digest-file=n"]).expect_err("no dest");
    assert!(err.0.contains("--destination"), "{}", err.0);
    // kaniko `root.go:215-217`: the tag file needs a destination too.
    let err = config(&["--no-push", "--image-name-tag-with-digest-file=n"]).expect_err("no dest");
    assert!(
        err.0.contains("--image-name-tag-with-digest-file"),
        "{}",
        err.0
    );
    // kaniko PUTs an https:// path; ranikust writes local files only.
    let err = config(&["--no-push", "--digest-file=https://bucket.example/d"]).expect_err("a URL");
    assert!(err.0.contains("local path"), "{}", err.0);
    let err = config(&["--no-push", "--label=nokey"]).expect_err("no =");
    assert!(err.0.contains("key=value"), "{}", err.0);
    // ADR-0036: an OCI tarball is allowed, unlike kaniko.
    assert!(config(&["--no-push", "--tar-path=/t", "--image-format=oci"]).is_ok());
}

/// kaniko's `--context-sub-path`: joined onto a `dir://` context, inside
/// it; ignored for a plain path, as kaniko returns before the join
/// (`root.go:599-631`).
#[test]
fn context_sub_path_applies_to_dir_contexts() {
    let c = config(&[
        "--no-push",
        "--context=dir:///ctx",
        "--context-sub-path=app/x",
    ])
    .expect("valid");
    assert_eq!(c.context, Path::new("/ctx/app/x"));
    for bad in ["../up", "/abs", "a/../../b"] {
        let flag = format!("--context-sub-path={bad}");
        let err = config(&["--no-push", "--context=dir:///ctx", &flag]).expect_err(bad);
        assert!(err.0.contains("inside the context"), "{bad}: {}", err.0);
    }
    let a = args(&["--no-push", "--context=/ctx", "--context-sub-path=app"]);
    assert_eq!(
        BuildConfig::from_args(&a).unwrap().context,
        Path::new("/ctx")
    );
    assert!(context_sub_path_ignored(&a));
    let a = args(&[
        "--no-push",
        "--context=dir:///ctx",
        "--context-sub-path=app",
    ]);
    assert!(!context_sub_path_ignored(&a));
}

#[test]
fn log_options_map_kanikos_levels() {
    use tracing::level_filters::LevelFilter;
    let o = log_options(&args(&["--no-push", "-v", "panic", "--log-format=text"]));
    assert_eq!(o.level, LevelFilter::ERROR);
    assert_eq!(o.format, rnk_metrics::LogFormat::Text);
    assert!(!o.timestamp);
    let o = log_options(&args(&[
        "--no-push",
        "--verbosity=trace",
        "--log-timestamp",
    ]));
    assert_eq!(o.level, LevelFilter::TRACE);
    assert!(o.timestamp);
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
fn one_non_empty_target_only() {
    assert!(config(&["--no-push", "--target="]).is_err());
    let ok = config(&["--no-push", "--target=b"]).expect("one target");
    assert_eq!(ok.targets, ["b"]);
    // ADR-0049: a list, comma-separated or repeated, is refused.
    for args in [
        &["--no-push", "--target=a,b"][..],
        &["--no-push", "--target", "a", "--target", "b"],
    ] {
        let err = config(args).expect_err("two targets");
        assert!(err.0.contains("give one"), "{}", err.0);
    }
}

#[test]
fn paths_become_absolute_without_touching_the_filesystem() {
    let ok = config(&[
        "--no-push",
        "--context=ctx",
        "--state-dir=state",
        "--tar-path=out/i.tar",
        "--registry-certificate=r.example=certs/r.pem",
    ])
    .expect("relative paths");
    let cwd = std::env::current_dir().expect("cwd");
    assert_eq!(ok.context, cwd.join("ctx"));
    assert_eq!(ok.state_dir, cwd.join("state"));
    assert_eq!(ok.tar_path, Some(cwd.join("out/i.tar")));
    assert_eq!(ok.registry_certificates[0].1, cwd.join("certs/r.pem"));
    // The Dockerfile is resolved against the context later.
    assert_eq!(ok.dockerfile, PathBuf::from("Dockerfile"));
}

#[test]
fn the_state_directory_defaults_to_var_lib_ranikust() {
    // In every mode, as BuildKit's `/var/lib/buildkit` (ADR-0050).
    let bare = config(&["--no-push"]).expect("defaults");
    assert_eq!(bare.state_dir, PathBuf::from("/var/lib/ranikust"));
    let explicit = config(&["--no-push", "--state-dir=/s"]).expect("state dir");
    assert_eq!(explicit.state_dir, PathBuf::from("/s"));
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

#[test]
fn secrets_are_absolute_and_ids_unique() {
    use rnk_builder::Secret;
    let ok = config(&["--no-push", "--secret", "id=a,src=rel/a"]).expect("config");
    assert!(matches!(&ok.secrets[0].1, Secret::File(p) if p.is_absolute() && p.ends_with("rel/a")));
    let env = config(&["--no-push", "--secret", "id=a,env=TOKEN"]).expect("config");
    assert_eq!(env.secrets[0].1, Secret::Env("TOKEN".into()));
    // No source: the variable named by the id when set, else the file.
    let none = config(&["--no-push", "--secret", "id=RNK_TEST_NEVER_SET_X3"]).expect("config");
    assert!(matches!(&none.secrets[0].1, Secret::File(p) if p.ends_with("RNK_TEST_NEVER_SET_X3")));
    let path = config(&["--no-push", "--secret", "id=PATH"]).expect("config");
    assert_eq!(path.secrets[0].1, Secret::Env("PATH".into()));
    let allow = config(&["--no-push", "--allow", "network.host"]).expect("config");
    assert!(allow.allow_network_host);
    assert!(!ok.allow_network_host);
    let twice = config(&[
        "--no-push",
        "--secret",
        "id=a,src=/a",
        "--secret",
        "id=a,src=/b",
    ]);
    assert!(twice.unwrap_err().0.contains("twice"));
}

/// The kaniko-mode flags (ADR-0040): kaniko's defaults, cleaned paths, a
/// relative `--ignore-path` refused.
#[test]
fn kaniko_mode_flags_take_kanikos_defaults_and_cleaned_paths() {
    let plain = config(&["--no-push"]).expect("plain");
    assert!(plain.ignore_paths.is_empty());
    assert!(plain.ignore_var_run);
    assert!(!plain.single_snapshot && !plain.materialize);
    assert_eq!(plain.snapshot_workers.get(), 8);
    let set = config(&[
        "--no-push",
        "--ignore-path=/var//cache/./x/../y/",
        "--ignore-path=/..",
        "--ignore-var-run=false",
        "--snapshot-workers=2",
    ])
    .expect("set");
    assert_eq!(
        set.ignore_paths,
        [PathBuf::from("/var/cache/y"), PathBuf::from("/")]
    );
    assert!(!set.ignore_var_run);
    assert_eq!(set.snapshot_workers.get(), 2);
    let err = config(&["--no-push", "--ignore-path=var/cache"]).expect_err("relative");
    assert!(err.0.contains("absolute path"), "{}", err.0);
}

/// In overlay mode, the kaniko-mode flags that would change the image are
/// refused (ADR-0049); the others are warned about.
#[test]
fn overlay_mode_refuses_kaniko_flags_that_change_the_image() {
    for flag in [
        "--ignore-path=/x",
        "--single-snapshot",
        "--ignore-var-run=false",
    ] {
        let err = overlay_mode_flags(&args(&["--no-push", flag])).expect_err(flag);
        assert!(err.0.contains("kaniko mode only"), "{flag}: {}", err.0);
    }
    let warnings = overlay_mode_flags(&args(&[
        "--no-push",
        "--snapshot-mode=redo",
        "--snapshot-workers=4",
        "--materialize",
        "--ignore-var-run",
    ]))
    .expect("warnings only");
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    assert!(
        overlay_mode_flags(&args(&["--no-push"]))
            .unwrap()
            .is_empty()
    );
}
