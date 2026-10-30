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
        .env_remove("DOCKER_CONFIG")
        .env_remove("DOCKER_AUTH_CONFIG")
        .env_remove("SOURCE_DATE_EPOCH")
        .env_remove("NO_COLOR")
        .env("RUST_BACKTRACE", "0");
    // kaniko's variables change what the binary does (ADR-0036).
    for (key, _) in std::env::vars_os() {
        let k = key.to_string_lossy();
        if k.starts_with("KANIKO_") || k.starts_with("FF_KANIKO_") {
            cmd.env_remove(&key);
        }
    }
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
fn kaniko_mode_checks_its_workdir_before_stopping() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workdir = tmp.path().join("work");
    fs::create_dir(&workdir).expect("workdir");
    fs::write(workdir.join("precious"), b"x").expect("file");
    let args = ["--no-push", "--mode=kaniko", "--workdir=work"];
    let out = ranikust(tmp.path(), &args, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("not empty"), "{}", stderr(&out));
    assert!(!workdir.join(".ranikust-root").exists());

    fs::remove_file(workdir.join("precious")).expect("remove");
    let out = ranikust(tmp.path(), &args, &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("passed kaniko mode's checks"),
        "{}",
        stderr(&out)
    );
    // The marker comes right before the first write, which this stop precedes.
    assert!(!workdir.join(".ranikust-root").exists());

    let out = ranikust(
        tmp.path(),
        &["--no-push", "--mode=kaniko", "--workdir=/usr"],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("system directory"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn without_root_the_set_up_refuses_before_touching_the_workdir() {
    if running_as_root() {
        // As root the set-up really runs: T2, in the VM smoke suite.
        eprintln!(
            "skipped: the tests run as root, where the set-up is not refused; \
             `just test-vm smoke` covers it"
        );
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
fn the_report_path_comes_from_the_flag_only() {
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

/// kaniko's logging flags (ADR-0036), on a run that stops after validation
/// (`--mode=kaniko` is Phase 2): `--verbosity` shows or hides lines,
/// `--log-format=json` writes JSON, and no timestamp by default.
#[test]
fn logging_flags_change_what_is_written() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = ["--no-push", "--mode=kaniko", "--force"];
    let run = |extra: &[&str]| {
        let mut args: Vec<&str> = base.iter().chain(extra).copied().collect();
        if !extra.iter().any(|a| a.starts_with("--log-format")) {
            args.push("--log-format=text");
        }
        let out = ranikust(tmp.path(), &args, &[]);
        assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("Phase 2"), "stopped at --mode=kaniko: {err}");
        err
    };
    // `text` is logrus's plain formatter: time, level, msg, never colored.
    let info = run(&[]);
    assert!(info.contains("--force is ignored"), "{info}");
    assert!(!info.contains("build settings validated"), "{info}");
    let first = info.lines().next().expect("a line");
    assert!(first.starts_with("time="), "{first}");
    assert!(first.contains(" level=warning msg="), "{first}");
    assert!(!info.contains('\u{1b}'), "text is never colored");
    let debug = run(&["-v", "DEBUG"]);
    assert!(debug.contains("build settings validated"), "{debug}");
    let quiet = run(&["--verbosity=error"]);
    assert!(!quiet.contains("--force is ignored"), "{quiet}");
    assert!(run(&["-v", "warning"]).contains("--force is ignored"));
    // `color`, kaniko's default, forces colors and counts seconds since
    // start; `--log-timestamp` gives the full time instead.
    let color = run(&["--log-format=color"]);
    let first = color.lines().next().expect("a line");
    assert!(
        first.starts_with("\u{1b}[33mWARN\u{1b}[0m[0000] "),
        "{first:?}"
    );
    let stamped = run(&["--log-format=color", "--log-timestamp"]);
    let first = stamped.lines().next().expect("a line");
    assert!(first.starts_with("\u{1b}[33mWARN\u{1b}[0m[2"), "{first:?}");
    let json = run(&["--log-format=json"]);
    let line = json
        .lines()
        .find(|l| l.contains("is ignored"))
        .expect("the warning");
    let value: Value = serde_json::from_str(line).expect("a JSON line");
    assert_eq!(value["level"], "warning");
    assert!(value["msg"].as_str().unwrap().contains("--force"));
    assert!(value["time"].is_string());
}

/// kaniko's variables (ADR-0036): mirrors are refused before anything runs.
#[test]
fn a_registry_mirror_variable_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &["--no-push"],
        &[("KANIKO_REGISTRY_MIRROR", "mirror.example")],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("Phase 5"), "{}", stderr(&out));
}
