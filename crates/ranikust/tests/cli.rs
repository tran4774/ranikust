//! The real binary's exit codes and messages: parsing, refused kaniko flags,
//! the kernel check (simulated through the hidden test hook), validation and
//! the not-yet-implemented build. Every file goes under a fresh temp dir.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

/// Runs the binary in `cwd` with `args` and a clean environment for the
/// variables it reads.
fn ranikust(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ranikust"));
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("RANIKUST_METRICS_FILE")
        .env_remove("RANIKUST_METRICS_SUMMARY")
        .env_remove("RANIKUST_DEBUG_FAULT")
        .env("RUST_BACKTRACE", "0");
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.output().expect("run ranikust")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn version_subcommand_and_flag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for args in [&["version"][..], &["--version"]] {
        let out = ranikust(tmp.path(), args, &[]);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert_eq!(
            stdout(&out).trim(),
            format!("ranikust {}", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn help_lists_kaniko_and_ranikust_flags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(tmp.path(), &["--help"], &[]);
    assert_eq!(out.status.code(), Some(0));
    let help = stdout(&out);
    for flag in [
        "--context",
        "--dockerfile",
        "--destination",
        "--no-push",
        "--snapshot-mode",
        "--mode",
        "--state-dir",
        "--metrics-file",
    ] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
}

/// Whether the tests run as root, from the effective uid in
/// `/proc/self/status`.
fn running_as_root() -> bool {
    fs::read_to_string("/proc/self/status")
        .expect("status")
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().nth(1).map(|euid| euid == "0"))
        .expect("Uid: line")
}

#[test]
fn overlay_mode_needs_a_workdir_or_state_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(tmp.path(), &["--no-push", "--context", "."], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--workdir"), "{}", stderr(&out));
}

#[test]
fn kaniko_mode_is_not_built_yet() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(tmp.path(), &["--no-push", "--mode=kaniko"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("Phase 2"), "{}", stderr(&out));
}

#[test]
fn without_root_the_set_up_refuses_before_touching_the_workdir() {
    if running_as_root() {
        // As root the set-up really runs: T2, in the VM smoke suite.
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let workdir = tmp.path().join("work");
    fs::create_dir(&workdir).expect("workdir");
    let out = ranikust(
        tmp.path(),
        &[
            "--no-push",
            "--context",
            ".",
            "--workdir=work",
            "--metrics-file=m.json",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("a private mount namespace"), "{err}");
    assert!(err.contains("sudo"), "{err}");
    assert_eq!(fs::read_dir(&workdir).expect("workdir").count(), 0);
    let report: Value =
        serde_json::from_slice(&fs::read(tmp.path().join("m.json")).expect("report"))
            .expect("json");
    assert_eq!(report["phases"]["ranikust.setup"]["count"], 1, "{report}");
    // Refused before the probes.
    assert!(report["phases"]["ranikust.probe"].is_null(), "{report}");
}

#[test]
fn kaniko_validation_is_a_usage_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(tmp.path(), &[], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("you must provide --destination, or use --no-push"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn refused_kaniko_flags_exit_two_with_reasons() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--cache=true", "--kaniko-dir=/kaniko"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("kaniko's --cache is not supported"), "{err}");
    assert!(err.contains("use --state-dir"), "{err}");
}

#[test]
fn unknown_flags_are_clap_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(tmp.path(), &["--no-such-flag"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--no-such-flag"), "{}", stderr(&out));
}

#[test]
fn an_old_kernel_is_refused_before_any_work() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report = tmp.path().join("metrics.json");
    let report_flag = format!("--metrics-file={}", report.display());
    let out = ranikust(
        tmp.path(),
        &["--no-push", &report_flag],
        &[("RANIKUST_DEBUG_FAULT", "kernel-missing")],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("Linux 5.8 or later"), "{err}");
    assert!(err.contains("4.18.0"), "{err}");
    // Nothing else ran: not even the metrics set-up.
    assert!(!report.exists());
}

#[test]
fn a_seccomp_block_gets_its_own_message() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &["--no-push"],
        &[("RANIKUST_DEBUG_FAULT", "kernel-blocked")],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("seccomp"), "{}", stderr(&out));
}

#[test]
fn version_works_on_any_kernel() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &["version"],
        &[("RANIKUST_DEBUG_FAULT", "kernel-missing")],
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn metrics_file_flag_overrides_the_environment() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let from_flag = tmp.path().join("flag.json");
    let from_env = tmp.path().join("env.json");
    let flag = format!("--metrics-file={}", from_flag.display());
    let out = ranikust(
        tmp.path(),
        &["-d", "registry.example/team/app:1", "--jobs=2", &flag],
        &[(
            "RANIKUST_METRICS_FILE",
            from_env.to_str().expect("utf-8 path"),
        )],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(!from_env.exists());
    let report: Value =
        serde_json::from_slice(&fs::read(&from_flag).expect("report")).expect("json");
    assert_eq!(report["exit"]["code"], 2);
    assert_eq!(report["meta"]["mode"], "auto");
    assert_eq!(report["meta"]["options"]["jobs"], 2);
    assert_eq!(report["meta"]["options"]["outputs"]["destinations"], 1);
    assert_eq!(report["phases"]["ranikust.build"]["count"], 1);
    let text = report.to_string();
    assert!(
        !text.contains("registry.example"),
        "destination leaked: {text}"
    );
}

#[test]
fn a_relative_metrics_file_is_relative_to_the_working_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(tmp.path(), &["--no-push", "--metrics-file=out/m.json"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(tmp.path().join("out/m.json").is_file());
}
