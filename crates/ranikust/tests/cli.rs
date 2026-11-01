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
    // kaniko's variables are not read (ADR-0049); scrubbed so a test
    // never depends on the developer's environment.
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

/// The run stops here as a user: the private mount namespace needs root.
/// As root the set-up would really run, so the tests that use this stop
/// are skipped as root (`just test-vm smoke` covers that).
const STOPS_AS_A_USER: &str = "a private mount namespace";

/// Skips the calling test as root, where the run would go past the stop.
fn skip_as_root() -> bool {
    if running_as_root() {
        eprintln!("skipped: the tests run as root, where the set-up is not refused");
    }
    running_as_root()
}

#[test]
fn kaniko_mode_needs_root() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let arg = format!("--state-dir={}", state.display());
    let out = ranikust(tmp.path(), &["--no-push", "--mode=kaniko", &arg], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("needs root"), "{}", stderr(&out));
    assert!(!state.exists(), "nothing is created before the root check");
}

#[test]
fn kaniko_mode_flags_are_checked_against_the_mode() {
    if running_as_root() {
        // As root kaniko mode would really build.
        eprintln!("skipped: kaniko mode as root runs only in the VM");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--mode=kaniko", "--ignore-path=var/cache"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("absolute path"), "{}", stderr(&out));
    let out = ranikust(
        tmp.path(),
        &[
            "--no-push",
            "--mode=kaniko",
            "--ignore-path=/var/cache",
            "--single-snapshot",
            "--ignore-var-run=false",
            "--snapshot-workers=4",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("Phase 2"), "{}", stderr(&out));
    // In overlay mode they would change the image: refused before the
    // set-up (ADR-0049).
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--mode=overlay", "--single-snapshot"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("kaniko mode only"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn without_root_the_set_up_refuses_before_touching_the_state_dir() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    fs::create_dir(&state).expect("state dir");
    let out = ranikust(
        tmp.path(),
        &[
            "--no-push",
            "--context",
            ".",
            "--state-dir=state",
            "--metrics-file=m.json",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains(STOPS_AS_A_USER), "{err}");
    assert!(err.contains("sudo"), "{err}");
    assert_eq!(fs::read_dir(&state).expect("state dir").count(), 0);
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

/// ADR-0053, ADR-0054: `dir://` and a local `--context-sub-path` exit 2,
/// naming what to pass instead.
#[test]
fn dir_scheme_and_local_sub_path_exit_two() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(tmp.path(), &["--no-push", "--context=dir:///ctx"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("--context=/ctx"), "{}", stderr(&out));
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--context=/ctx", "--context-sub-path=app"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--context=/ctx/app"),
        "{}",
        stderr(&out)
    );
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
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let from_flag = tmp.path().join("flag.json");
    let from_env = tmp.path().join("env.json");
    let flag = format!("--metrics-file={}", from_flag.display());
    let out = ranikust(
        tmp.path(),
        &[
            "-d",
            "registry.example/team/app:1",
            "--jobs=2",
            "--mode=overlay",
            &flag,
        ],
        &[(
            "RANIKUST_METRICS_FILE",
            from_env.to_str().expect("utf-8 path"),
        )],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(!from_env.exists());
    let report: Value =
        serde_json::from_slice(&fs::read(&from_flag).expect("report")).expect("json");
    assert_eq!(report["exit"]["code"], 1);
    assert_eq!(report["meta"]["mode"], "overlay");
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
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--mode=overlay", "--metrics-file=out/m.json"],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(tmp.path().join("out/m.json").is_file());
}

/// kaniko's logging flags (ADR-0036), on a run that stops at the set-up (a
/// user cannot enter the mount namespace): `--verbosity` shows or hides lines,
/// `--log-format=json` writes JSON, and no timestamp by default.
#[test]
fn logging_flags_change_what_is_written() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = ["--no-push", "--mode=overlay", "--snapshot-mode=full"];
    let run = |extra: &[&str]| {
        let mut args: Vec<&str> = base.iter().chain(extra).copied().collect();
        if !extra.iter().any(|a| a.starts_with("--log-format")) {
            args.push("--log-format=text");
        }
        let out = ranikust(tmp.path(), &args, &[]);
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains(STOPS_AS_A_USER), "{err}");
        err
    };
    // `text` is logrus's plain formatter: time, level, msg, never colored.
    let info = run(&[]);
    assert!(info.contains("--snapshot-mode is ignored"), "{info}");
    assert!(!info.contains("build settings validated"), "{info}");
    let first = info.lines().next().expect("a line");
    assert!(first.starts_with("time="), "{first}");
    assert!(first.contains(" level=warning msg="), "{first}");
    assert!(!info.contains('\u{1b}'), "text is never colored");
    let debug = run(&["-v", "DEBUG"]);
    assert!(debug.contains("build settings validated"), "{debug}");
    let quiet = run(&["--verbosity=error"]);
    assert!(!quiet.contains("--snapshot-mode is ignored"), "{quiet}");
    assert!(run(&["-v", "warning"]).contains("--snapshot-mode is ignored"));
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
    assert!(value["msg"].as_str().unwrap().contains("--snapshot-mode"));
    assert!(value["time"].is_string());
}

/// ADR-0049: kaniko's variables are not read, so a mirror variable changes
/// nothing: the run stops where it would without it.
#[test]
fn kaniko_variables_are_not_read() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    for env in [
        &[("KANIKO_REGISTRY_MIRROR", "mirror.example")][..],
        &[("KANIKO_NO_PUSH", "maybe")],
    ] {
        let out = ranikust(tmp.path(), &["--no-push", "--mode=overlay"], env);
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains(STOPS_AS_A_USER), "{err}");
        assert!(!err.contains("KANIKO"), "{err}");
    }
}
