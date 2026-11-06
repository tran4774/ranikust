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
        .env_remove("RANIKUST_SEALED_COPY")
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
            "--snapshot-workers=4",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    // Refused for good, in every mode (ADR-0065, Q-420: b).
    assert!(stderr(&out).contains("ADR-0065"), "{}", stderr(&out));
    // Nor in overlay mode, whose refusal no longer sends the user to
    // `--mode=kaniko` (criterion 8, review finding #1).
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--mode=overlay", "--single-snapshot"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("ADR-0065"), "{}", stderr(&out));
    assert!(!stderr(&out).contains("--mode=kaniko"), "{}", stderr(&out));
    // `--ignore-path` would change an overlay-mode image: refused before
    // the set-up (ADR-0049).
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--mode=overlay", "--ignore-path=/x"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("kaniko mode only"),
        "{}",
        stderr(&out)
    );
    // The whole root or a malformed pattern: refused while the flags are
    // read, exit 2, nothing created (review finding #7).
    let state = tmp.path().join("state");
    let arg = format!("--state-dir={}", state.display());
    for bad in ["--ignore-path=/..", "--ignore-path=/x/[a"] {
        let out = ranikust(tmp.path(), &["--no-push", "--mode=kaniko", &arg, bad], &[]);
        assert_eq!(out.status.code(), Some(2), "{bad}: {}", stderr(&out));
        assert!(!state.exists(), "{bad}: nothing is created");
    }
}

/// `--single-snapshot` is refused with `--mode=auto` too (no `--mode`), as a
/// non-root user, before anything is created (ADR-0065; host-safety round 1
/// of criterion 8, item 1).
#[test]
fn single_snapshot_is_refused_in_auto_mode_before_any_work() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let arg = format!("--state-dir={}", state.display());
    let out = ranikust(tmp.path(), &["--no-push", &arg, "--single-snapshot"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("ADR-0065"), "{}", stderr(&out));
    assert!(!state.exists(), "nothing is created");
}

/// `--materialize` changes nothing until Phase 4's cache: refused rather than
/// accepted to be ignored (ADR-0064), before any work.
#[test]
fn materialize_is_refused_before_any_work() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let arg = format!("--state-dir={}", state.display());
    for flag in ["--materialize", "--materialize=true", "--materialize=false"] {
        let out = ranikust(tmp.path(), &["--no-push", &arg, flag], &[]);
        assert_eq!(out.status.code(), Some(2), "{flag}: {}", stderr(&out));
        assert!(stderr(&out).contains("Phase 4"), "{flag}: {}", stderr(&out));
        assert!(!state.exists(), "{flag}: nothing is created");
    }
}

#[test]
fn without_root_the_set_up_refuses_before_touching_the_state_dir() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    fs::create_dir(&state).expect("state dir");
    // The report goes beside the context, never into it (ADR-0076).
    fs::create_dir(tmp.path().join("ctx")).expect("context dir");
    let out = ranikust(
        tmp.path(),
        &[
            "--no-push",
            "--context",
            "ctx",
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
    // ADR-0052, D4: `/var/run` is ignored only as a mount point; the flag
    // that switched it has nothing left to do.
    for flag in [
        "--ignore-var-run=false",
        "--ignore-var-run",
        "--whitelist-var-run",
    ] {
        let out = ranikust(tmp.path(), &["--no-push", flag], &[]);
        assert_eq!(out.status.code(), Some(2), "{flag}");
        assert!(stderr(&out).contains("mount point"), "{}", stderr(&out));
    }
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

/// `--seccomp` with a file that cannot be a profile, or no value, is a
/// usage error before any work (ADR-0109): nothing is created.
#[test]
fn a_seccomp_file_that_cannot_be_read_or_compiled_is_refused_before_any_work() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let state_flag = format!("--state-dir={}", state.display());
    let malformed = tmp.path().join("malformed.json");
    fs::write(&malformed, "{\"defaultAction\": ").expect("write");
    let notify = tmp.path().join("notify.json");
    fs::write(&notify, r#"{"defaultAction": "SCMP_ACT_NOTIFY"}"#).expect("write");
    let missing = tmp.path().join("missing.json");
    for (file, want) in [
        (&malformed, "seccomp profile"),
        (&notify, "SCMP_ACT_NOTIFY"),
        (&missing, "cannot open"),
    ] {
        let flag = format!("--seccomp={}", file.display());
        for mode in ["--mode=auto", "--mode=overlay", "--mode=kaniko"] {
            let out = ranikust(tmp.path(), &["--no-push", &state_flag, mode, &flag], &[]);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{flag} {mode}: {}",
                stderr(&out)
            );
            assert!(
                stderr(&out).contains(want),
                "{flag} {mode}: {}",
                stderr(&out)
            );
            assert!(!state.exists(), "{flag} {mode}: nothing is created");
        }
    }
    let out = ranikust(tmp.path(), &["--no-push", &state_flag, "--seccomp="], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(!state.exists(), "nothing is created");
}

#[test]
fn enosys_on_a_new_kernel_does_not_blame_the_kernel() {
    // An x86_64 binary under Rosetta (an amd64 container on Apple Silicon)
    // gets ENOSYS from openat2 on a 7.2 kernel.
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &["--no-push"],
        &[("RANIKUST_DEBUG_FAULT", "kernel-hidden")],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("7.2.12-simulated"), "{err}");
    assert!(err.contains("Rosetta"), "{err}");
    assert!(err.contains("architecture"), "{err}");
    assert!(!err.contains("needs Linux 5.8 or later"), "{err}");
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
    // A report needs a build context that opens, to be kept out of it
    // (ADR-0076).
    fs::create_dir(tmp.path().join("ctx")).expect("context");
    let out = ranikust(
        tmp.path(),
        &[
            "-d",
            "registry.example/team/app:1",
            "--jobs=2",
            "--mode=overlay",
            "--context=ctx",
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

/// `--metrics-file` in the build context is refused before the report is
/// set up, so nothing is written there (ADR-0076).
#[test]
fn a_metrics_file_in_the_context_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--context=.", "--metrics-file=out/m.json"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("build context"), "{}", stderr(&out));
    assert!(!tmp.path().join("out").exists());
}

#[test]
fn a_relative_metrics_file_is_relative_to_the_working_directory() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir(tmp.path().join("ctx")).expect("context");
    let out = ranikust(
        tmp.path(),
        &[
            "--no-push",
            "--mode=overlay",
            "--context=ctx",
            "--metrics-file=out/m.json",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(tmp.path().join("out/m.json").is_file());
}

/// A report asked for a build whose context cannot be opened is refused
/// before anything is set up: without the context's fence the report could
/// not be kept out of it (ADR-0076).
#[test]
fn a_metrics_file_needs_a_build_context_that_opens() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--context=missing", "--metrics-file=m.json"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("cannot be opened"),
        "{}",
        stderr(&out)
    );
    assert!(!tmp.path().join("m.json").exists());
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

/// ADR-0101: a build runs again from a sealed copy of the binary, once,
/// with the same arguments: a sealed memfd, first tried, so no way is
/// logged as passed over; the test hook makes the `O_TMPFILE` copy the
/// first, or leaves none, and the build then goes on with a warning. A
/// marker set from outside makes no copy and passes for none: the run
/// warns.
#[test]
fn a_build_runs_from_a_sealed_copy_made_once() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let tmpdir = tmp.path().to_str().expect("UTF-8 temp dir");
    let args = [
        "--no-push",
        "--mode=overlay",
        "--log-format=text",
        "-v",
        "debug",
    ];
    let run = |env: &[(&str, &str)]| {
        let mut env = env.to_vec();
        env.push(("TMPDIR", tmpdir));
        let out = ranikust(tmp.path(), &args, &env);
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(1), "{err}");
        // The copy got `-v debug`: the stop's debug lines are there.
        assert!(err.contains(STOPS_AS_A_USER), "{err}");
        assert!(err.contains("build settings validated"), "{err}");
        err
    };
    let sealed = |err: &str| -> Vec<String> {
        err.lines()
            .filter(|l| l.contains("running from a sealed copy"))
            .map(str::to_owned)
            .collect()
    };
    let unsealed = "running from the binary as installed";
    for (fault, way) in [(None, "memfd"), (Some("seal-tmpfile"), "tmpfile")] {
        let env: Vec<_> = fault
            .map(|f| ("RANIKUST_DEBUG_FAULT", f))
            .into_iter()
            .collect();
        let err = run(&env);
        let lines = sealed(&err);
        assert_eq!(lines.len(), 1, "{fault:?}: {err}");
        assert!(lines[0].contains(&format!("way={way}")), "{fault:?}: {err}");
        assert!(!lines[0].contains("passed_over"), "{fault:?}: {err}");
        // The binary's mount, measured before the copy, reached it.
        assert!(lines[0].contains(" binary="), "{fault:?}: {err}");
        assert!(!err.contains(unsealed), "{fault:?}: {err}");
    }
    // The O_TMPFILE copy left nothing behind.
    assert_eq!(fs::read_dir(tmp.path()).expect("ls").count(), 0);
    let err = run(&[("RANIKUST_DEBUG_FAULT", "seal-none")]);
    assert!(sealed(&err).is_empty(), "{err}");
    assert!(err.contains(unsealed), "{err}");
    assert!(err.contains("no way was tried"), "{err}");
    for marker in ["memfd root", "memfd"] {
        let err = run(&[("RANIKUST_SEALED_COPY", marker)]);
        assert!(sealed(&err).is_empty(), "{marker}: {err}");
        assert!(err.contains(unsealed), "{marker}: {err}");
        let why = if marker == "memfd" {
            "names no way"
        } else {
            "is no such copy"
        };
        assert!(err.contains(why), "{marker}: {err}");
    }
}

/// Red team (ADR-0101, T21): a hostile `TMPDIR` loses only the `O_TMPFILE` copy.
#[test]
fn red_team_a_hostile_tmpdir_only_loses_the_tmpfile_copy() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let file = tmp.path().join("file");
    fs::write(&file, b"x").expect("a file");
    let fifo = tmp.path().join("fifo");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .expect("a FIFO");
    let dir = tmp.path().join("dir");
    fs::create_dir(&dir).expect("a directory");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&dir, &link).expect("a symlink");
    let missing = tmp.path().join("missing");
    let args = [
        "--no-push",
        "--mode=overlay",
        "--log-format=text",
        "-v",
        "debug",
    ];
    let run = |tmpdir: &Path| {
        let tmpdir = tmpdir.to_str().expect("UTF-8");
        let env = [("RANIKUST_DEBUG_FAULT", "seal-tmpfile"), ("TMPDIR", tmpdir)];
        let out = ranikust(tmp.path(), &args, &env);
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(1), "{tmpdir}: {err}");
        assert!(err.contains(STOPS_AS_A_USER), "{tmpdir}: {err}");
        err
    };
    for lost in [&file, &fifo, &missing] {
        let err = run(lost);
        assert!(
            err.contains("running from the binary as installed") && err.contains("tmpfile: "),
            "{}: {err}",
            lost.display()
        );
    }
    let err = run(&link);
    assert!(
        err.contains("running from a sealed copy") && err.contains("way=tmpfile"),
        "{err}"
    );
    assert_eq!(fs::read_dir(&dir).expect("ls").count(), 0);
    let mut names: Vec<_> = fs::read_dir(tmp.path())
        .expect("ls")
        .map(|e| e.expect("entry").file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["dir", "fifo", "file", "link"]);
    assert_eq!(fs::read(&file).expect("read"), b"x");
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

/// Red team (ADR-0060, T12): `--parallel-stages` exits 2 before any work,
/// with any mode, in any spelling the pre-pass knows, also repeated: the
/// state directory is never created and the value is never echoed. A
/// `RANIKUST_PARALLEL_STAGES` variable is not read: the run stops where it
/// stops without it.
#[test]
fn parallel_stages_is_refused_before_any_work() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let state_arg = format!("--state-dir={}", state.display());
    let mut cases: Vec<Vec<&str>> = vec![
        vec!["--parallel-stages"],
        vec!["--parallel-stages=false"],
        vec!["--parallel-stages=0"],
        vec!["--parallel-stages=tok-RNK-4242"],
        vec!["--parallel-stages", "--parallel-stages"],
        vec!["build", "--parallel-stages"],
    ];
    // As root a mode that went past the refusal would really build.
    if !running_as_root() {
        cases.push(vec!["--mode=kaniko", "--parallel-stages"]);
        cases.push(vec!["--mode=overlay", "--parallel-stages"]);
    }
    for case in &cases {
        let mut args = case.clone();
        args.extend(["--no-push", "--context=/nonexistent-rnk-ctx", &state_arg]);
        let out = ranikust(tmp.path(), &args, &[]);
        assert_eq!(out.status.code(), Some(2), "{case:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("ADR-0060"), "{case:?}: {err}");
        assert!(
            !err.contains("tok-RNK-4242"),
            "{case:?}: the value is echoed: {err}"
        );
        assert!(!state.exists(), "{case:?}: the state directory was created");
    }
    if skip_as_root() {
        return;
    }
    for env in [
        &[("RANIKUST_PARALLEL_STAGES", "true")][..],
        &[("RANIKUST_PARALLEL_STAGES", "1"), ("PARALLEL_STAGES", "1")],
    ] {
        let out = ranikust(tmp.path(), &["--no-push", "--mode=overlay"], env);
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains(STOPS_AS_A_USER), "{err}");
        assert!(!err.to_lowercase().contains("parallel"), "{err}");
    }
}

/// Runs the binary in `cwd` with raw (possibly non-UTF-8) arguments and
/// the environment of [`ranikust`].
fn ranikust_os(cwd: &Path, args: &[std::ffi::OsString]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ranikust"));
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("RANIKUST_METRICS_FILE")
        .env_remove("RANIKUST_METRICS_SUMMARY")
        .env_remove("RANIKUST_DEBUG_FAULT")
        .env_remove("RANIKUST_SEALED_COPY")
        .env("RUST_BACKTRACE", "0");
    cmd.output().expect("run ranikust")
}

/// Red team (ADR-0064, criterion 8): `--single-snapshot` and `--materialize`
/// exit 2 before any work in every spelling: every boolean spelling,
/// repeated, a value as a separate argument, a non-UTF-8 value, after
/// `--`, in an `@argfile` (never expanded), as the value of another flag,
/// behind the `build` subcommand, in every mode. The state directory is
/// never created. A `RANIKUST_*` or `KANIKO_*` variable naming them is not
/// read: the run stops where it stops without it.
#[test]
fn red_team_single_snapshot_and_materialize_have_no_bypass_spelling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let state_arg = format!("--state-dir={}", state.display());
    fs::write(
        tmp.path().join("argf"),
        "--materialize\n--single-snapshot\n",
    )
    .expect("argfile");
    let spelled: Vec<String> = ["1", "t", "T", "true", "TRUE", "True"]
        .iter()
        .map(|v| format!("--single-snapshot={v}"))
        .chain(
            ["1", "0", "f", "yes", "", "tok-RNK-4242"]
                .iter()
                .map(|v| format!("--materialize={v}")),
        )
        .collect();
    let mut cases: Vec<Vec<&str>> = spelled.iter().map(|s| vec![s.as_str()]).collect();
    cases.extend([
        vec!["--single-snapshot=yes"],
        vec!["--single-snapshot="],
        vec!["--single-snapshot", "true"],
        vec!["--single-snapshot", "--single-snapshot=false"],
        vec!["--single-snapshot=false", "--single-snapshot"],
        vec!["--single-snap"],
        vec!["-single-snapshot"],
        vec!["--materialize", "true"],
        vec!["--materialize", "--materialize=false"],
        vec!["-materialize"],
        vec!["--MATERIALIZE"],
        vec!["--context", "--materialize"],
        vec!["--", "--materialize"],
        vec!["--", "--single-snapshot"],
        vec!["@argf"],
    ]);
    for mode in ["--mode=auto", "--mode=overlay", "--mode=kaniko"] {
        cases.push(vec![mode, "--single-snapshot"]);
        cases.push(vec![mode, "--materialize"]);
    }
    for case in &cases {
        let mut args = vec!["--no-push", &state_arg];
        args.extend(case.iter().copied());
        let out = ranikust(tmp.path(), &args, &[]);
        assert_eq!(out.status.code(), Some(2), "{case:?}: {}", stderr(&out));
        assert!(
            !stderr(&out).contains("tok-RNK-4242"),
            "{case:?}: the value is echoed: {}",
            stderr(&out)
        );
        assert!(!state.exists(), "{case:?}: the state directory was created");
    }
    // Behind the subcommand.
    for flag in ["--single-snapshot", "--materialize"] {
        let out = ranikust(tmp.path(), &["build", "--no-push", &state_arg, flag], &[]);
        assert_eq!(out.status.code(), Some(2), "build {flag}: {}", stderr(&out));
        assert!(
            !state.exists(),
            "build {flag}: the state directory was created"
        );
    }
    // Non-UTF-8 values and names.
    use std::os::unix::ffi::OsStringExt as _;
    for raw in [
        b"--single-snapshot=\xff".to_vec(),
        b"--materialize=\xff".to_vec(),
        b"--single-snapshot\xff".to_vec(),
    ] {
        let args = [
            "--no-push".into(),
            state_arg.clone().into(),
            std::ffi::OsString::from_vec(raw.clone()),
        ];
        let out = ranikust_os(tmp.path(), &args);
        assert_eq!(out.status.code(), Some(2), "{raw:?}: {}", stderr(&out));
        assert!(!state.exists(), "{raw:?}: the state directory was created");
    }
    if skip_as_root() {
        return;
    }
    for env in [
        &[
            ("RANIKUST_SINGLE_SNAPSHOT", "true"),
            ("RANIKUST_MATERIALIZE", "true"),
        ][..],
        &[("KANIKO_SINGLE_SNAPSHOT", "true"), ("SINGLE_SNAPSHOT", "1")],
    ] {
        let out = ranikust(tmp.path(), &["--no-push", "--mode=overlay"], env);
        assert_eq!(out.status.code(), Some(1), "{env:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains(STOPS_AS_A_USER), "{err}");
        assert!(
            !err.contains("ADR-0065") && !err.contains("Phase 4"),
            "{err}"
        );
    }
}

/// Red team (criterion 8, review finding #7): an `--ignore-path` that is the
/// whole root or takes every top-level entry in another spelling (`//`,
/// `/.`, `/*/..`, `/./*`, `//*/`, `/**`, `/*?`, a class over all of
/// ASCII, a negated class of a non-UTF-8 byte), or a malformed pattern,
/// exits 2 before anything is created, in every mode.
#[test]
fn red_team_whole_root_ignore_paths_are_refused_in_every_spelling_before_any_io() {
    use std::os::unix::ffi::OsStringExt as _;
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let state_arg = format!("--state-dir={}", state.display());
    let mut bad: Vec<Vec<u8>> = [
        "//",
        "/.",
        "/./.",
        "/a/b/../../..",
        "/*/..",
        "/[^.]*/..",
        "/./*",
        "//*/",
        "/*/.",
        "/**",
        "/*?",
        "/[!-~]*",
        "/x\\",
        "/[]",
        "/[^]",
        "/[a-]",
        "/[z-a]",
        "/[^-]*",
        "/[^/]*",
    ]
    .iter()
    .map(|p| p.as_bytes().to_vec())
    .collect();
    bad.push(b"/[^\xff]*".to_vec());
    for path in &bad {
        for mode in ["--mode=auto", "--mode=overlay", "--mode=kaniko"] {
            let mut flag = b"--ignore-path=".to_vec();
            flag.extend_from_slice(path);
            let args = [
                "--no-push".into(),
                mode.into(),
                state_arg.clone().into(),
                std::ffi::OsString::from_vec(flag),
            ];
            let out = ranikust_os(tmp.path(), &args);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{} {mode}: {}",
                String::from_utf8_lossy(path),
                stderr(&out)
            );
            assert!(
                !state.exists(),
                "{}: the state directory was created",
                String::from_utf8_lossy(path)
            );
        }
    }
}
