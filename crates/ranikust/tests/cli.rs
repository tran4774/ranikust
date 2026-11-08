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
    command(cwd, args, env).output().expect("run ranikust")
}

/// The command [`ranikust`] runs.
fn command(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ranikust"));
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("RANIKUST_METRICS_FILE")
        .env_remove("RANIKUST_METRICS_SUMMARY")
        // No test here makes a user namespace (T0): rootless overlay mode is
        // refused before its probe, unless the test sets another fault.
        .env("RANIKUST_DEBUG_FAULT", "no-userns")
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
    cmd
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

/// The run stops here as a user: rootless overlay mode, refused by the test
/// hook before its probe (a user namespace is T1, `just test-rootless`).
/// As root the set-up would really run, so the tests that use this stop
/// are skipped as root (`just test-vm smoke` covers that).
const STOPS_AS_A_USER: &str = "rootless overlay mode cannot run here";

/// Where a test sets a fault of its own, which replaces the hook's
/// `no-userns`: kaniko mode refuses a user other than root before any
/// namespace.
const KANIKO_STOPS_AS_A_USER: &str = "kaniko mode needs root";

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

/// With rootless overlay mode refused (the test hook), a user's build
/// stops before the state directory is touched.
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

/// The same with the default state directory, `~/.local/share/ranikust`.
#[test]
fn without_root_the_set_up_never_makes_the_default_state_dir() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir(&home).expect("home");
    fs::create_dir(tmp.path().join("ctx")).expect("context dir");
    for (mode, says) in [
        ("--mode=auto", STOPS_AS_A_USER),
        ("--mode=overlay", STOPS_AS_A_USER),
        ("--mode=kaniko", "needs root"),
    ] {
        let out = ranikust(
            tmp.path(),
            &["--no-push", "--context", "ctx", mode],
            &[("HOME", home.to_str().expect("utf-8 temp dir"))],
        );
        assert_eq!(out.status.code(), Some(1), "{mode}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains(says), "{mode}: {err}");
        let made: Vec<_> = fs::read_dir(&home).expect("home").collect();
        assert!(made.is_empty(), "{mode}: {made:?}");
    }
}

/// `--userns` from a user other than root is a usage error, before
/// anything is written: neither the state directory nor the default one
/// is made.
#[test]
fn userns_as_a_user_other_than_root_is_a_usage_error() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir(&home).expect("home");
    fs::create_dir(tmp.path().join("ctx")).expect("context dir");
    let home_env = [("HOME", home.to_str().expect("utf-8 temp dir"))];
    for state in [&["--state-dir=state"][..], &[]] {
        let mut argv = vec!["--no-push", "--context", "ctx", "--userns"];
        argv.extend_from_slice(state);
        let out = ranikust(tmp.path(), &argv, &home_env);
        assert_eq!(out.status.code(), Some(2), "{argv:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("--userns needs root"),
            "{}",
            stderr(&out)
        );
        assert!(!tmp.path().join("state").exists(), "{argv:?}");
        let made: Vec<_> = fs::read_dir(&home).expect("home").collect();
        assert!(made.is_empty(), "{argv:?}: {made:?}");
    }
}

/// A user with no user namespace is told, in order: `sudo`; where AppArmor
/// restricts user namespaces and ranikust runs unconfined in the host's
/// user namespace and in no container, the profile, after installing the
/// binary under `/usr/local/bin`, as this one, built in the checkout, is
/// the user's; a container. Elsewhere no profile is named.
#[test]
fn without_a_user_namespace_the_advice_is_sudo_the_profile_then_a_container() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir(tmp.path().join("ctx")).expect("context dir");
    let state = tmp.path().join("state");
    let out = ranikust(
        tmp.path(),
        &["--no-push", "--context", "ctx", "--state-dir=state"],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    let advice = err
        .lines()
        .find_map(|l| l.strip_prefix("  ").filter(|l| l.contains("with sudo")))
        .unwrap_or_else(|| panic!("no advice: {err}"));
    let at = |part: &str| advice.find(part);
    let sudo = at("run ranikust as root, with sudo").expect(advice);
    let container = at("; or run ranikust in a container").expect(advice);
    assert!(sudo < container, "{advice}");
    if offers_a_profile_here() {
        // As /usr/local/bin/ranikust, or elsewhere where /usr/local/bin is
        // not root's alone.
        let profile = at("or, as root, install ranikust ").expect(advice);
        assert!(sudo < profile && profile < container, "{advice}");
        // Named as `/proc/self/exe` names it: no symlink on the way.
        let binary = fs::canonicalize(env!("CARGO_BIN_EXE_ranikust")).expect("binary");
        let named = format!("{} is not a path only root can change (", binary.display());
        assert!(advice.contains(&named), "{advice}");
    } else {
        assert!(!advice.contains("AppArmor"), "{advice}");
    }
    assert!(!state.exists());
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
        "--mode=kaniko",
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
        assert!(err.contains(KANIKO_STOPS_AS_A_USER), "{err}");
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
        "--mode=kaniko",
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
        assert!(err.contains(KANIKO_STOPS_AS_A_USER), "{tmpdir}: {err}");
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
        .env("RANIKUST_DEBUG_FAULT", "no-userns")
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

/// Every path below `dir`, never following a symlink, sorted.
fn names(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).expect("read_dir") {
            let p = e.expect("entry").path();
            if fs::symlink_metadata(&p).expect("lstat").is_dir() {
                todo.push(p.clone());
            }
            out.push(p.strip_prefix(dir).expect("below").to_owned());
        }
    }
    out.sort();
    out
}

/// `prune` takes `--all` only until Phase 5: without it, a usage error
/// naming Phase 5, before any state directory is looked at.
#[test]
fn prune_without_all_is_a_usage_error_naming_phase_5() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let arg = format!("--state-dir={}", state.display());
    let out = ranikust(tmp.path(), &["prune", &arg], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("Phase 5"), "{}", stderr(&out));
    assert!(!state.exists());
    let out = ranikust(tmp.path(), &["prune", "--help"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    for flag in ["--all", "--state-dir", "--userns"] {
        assert!(stdout(&out).contains(flag), "{flag}: {}", stdout(&out));
    }
}

/// `prune --all --userns` is root's (#207, ADR-0139): from a user other
/// than root it is a usage error before any state directory, the default
/// included, is looked at; without `--all` it is `prune`'s usage error
/// naming Phase 5.
#[test]
fn prune_userns_as_a_user_other_than_root_is_a_usage_error() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir(&home).expect("home");
    let env = [("HOME", home.to_str().expect("utf-8 temp dir"))];
    for args in [
        &["prune", "--all", "--userns", "--state-dir=state"][..],
        &["prune", "--all", "--userns"],
        &["prune", "--userns", "--state-dir=state"],
    ] {
        let out = ranikust(tmp.path(), args, &env);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        let want = if args.contains(&"--all") {
            "--userns needs root"
        } else {
            "Phase 5"
        };
        assert!(stderr(&out).contains(want), "{args:?}: {}", stderr(&out));
        assert!(stdout(&out).is_empty(), "{args:?}: {}", stdout(&out));
        assert!(!tmp.path().join("state").exists(), "{args:?}");
        assert_eq!(names(&home), Vec::<std::path::PathBuf>::new(), "{args:?}");
    }
}

/// As root, a container's root included, `prune --all` is refused with
/// nothing changed, naming Phase 5's `prune`; as a user,
/// a state directory that does not exist, the default one included, is
/// left so.
#[test]
fn prune_all_refuses_root_and_leaves_a_missing_state_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir(&home).expect("home");
    let state = tmp.path().join("state");
    let arg = format!("--state-dir={}", state.display());
    let env = [("HOME", home.to_str().expect("utf-8 temp dir"))];
    let default = home.join(".local/share/ranikust");
    for (args, dir) in [
        (&["prune", "--all"][..], &default),
        (&["prune", "--all", &arg], &state),
    ] {
        let out = ranikust(tmp.path(), args, &env);
        if running_as_root() {
            assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
            let err = stderr(&out);
            assert!(err.contains("refuses root"), "{args:?}: {err}");
            assert!(err.contains("Phase 5"), "{args:?}: {err}");
        } else {
            assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
            assert_eq!(
                stdout(&out),
                format!("nothing to prune: {} does not exist\n", dir.display())
            );
        }
    }
    assert!(!state.exists());
    assert_eq!(names(&home), Vec::<std::path::PathBuf>::new());
}

/// As a user, `prune --all` refuses a directory without the store's
/// marker, changing nothing; on a state directory a build left, it either
/// empties it in the build's user namespace, or, where the machine gives
/// the user none, says what to do and changes nothing.
#[test]
fn prune_all_as_a_user_empties_a_state_directory_or_changes_nothing() {
    use std::os::unix::fs::PermissionsExt as _;
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    // The state directory's checks refuse a parent others may write.
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
    let precious = tmp.path().join("precious");
    fs::create_dir(&precious).expect("precious");
    fs::set_permissions(&precious, fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::write(precious.join("keep"), b"keep").expect("keep");
    let out = ranikust(tmp.path(), &["prune", "--all", "--state-dir=precious"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("has no regular file .ranikust-state"), "{err}");
    assert_eq!(names(&precious), [Path::new("keep")]);
    assert_eq!(fs::read(precious.join("keep")).expect("keep"), b"keep");
    let mode = fs::metadata(&precious)
        .expect("precious")
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);

    let state = tmp.path().join("state");
    rnk_store::Store::open(&rnk_store::BuildPaths::new(&state, &state))
        .and_then(rnk_store::Store::close)
        .expect("a state directory");
    let before = names(&state);
    assert!(
        before.contains(&rnk_store::STATE_MARKER.into()),
        "{before:?}"
    );
    // A path ending in `..` is refused before anything is opened, even one
    // that leads to the state directory.
    let out = ranikust(
        tmp.path(),
        &["prune", "--all", "--state-dir=state/blobs/.."],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains(&format!(
            "the state directory {} ends in ..: give the state directory's own path",
            state.join("blobs/..").display()
        )),
        "{err}"
    );
    assert_eq!(names(&state), before);
    let out = ranikust(tmp.path(), &["prune", "--all", "--state-dir=state"], &[]);
    match out.status.code() {
        Some(0) => {
            assert_eq!(stdout(&out), format!("emptied {}\n", state.display()));
            assert_eq!(names(&state), Vec::<std::path::PathBuf>::new());
            fs::remove_dir(&state).expect("rmdir");
        }
        Some(1) => {
            let err = stderr(&out);
            assert!(err.contains("no user namespace for the build: "), "{err}");
            // The commands enter the state directory (quoted when a shell
            // needs it) once, check that it is the one ranikust checked,
            // then work under the builds' lock on it.
            let pin = {
                use std::os::unix::fs::MetadataExt as _;
                let m = fs::metadata(&state).expect("state");
                format!(
                    "{}:{}:{}:{:o}",
                    m.dev(),
                    m.ino(),
                    m.uid(),
                    m.mode() & 0o7777
                )
            };
            let check = format!(" && stat -c %d:%i:%u:%a . | grep -qxF {pin} && exec flock -xn . ");
            assert!(err.contains(&check), "{err}");
            assert!(err.contains(" && exec flock -xn . sh -c '"), "{err}");
            assert!(err.contains(&*state.to_string_lossy()), "{err}");
            let plain = state.to_str().is_some_and(|s| {
                s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-+,:@%=".contains(&b))
            });
            if plain {
                let lock = format!(
                    "( eval 'case \":$PATH:\" in *::*|*:[!/]*) echo \"PATH holds a relative or \
                     empty entry: use absolute directories only\" >&2; exit 1;; esac; unset \
                     LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT GCONV_PATH || {{ echo \"cannot unset \
                     LD_LIBRARY_PATH, LD_PRELOAD, LD_AUDIT or GCONV_PATH: nothing changed\" >&2; \
                     exit 1; }}' && CDPATH= cd -P -- {}{check}",
                    state.display()
                );
                assert!(err.contains(&lock), "{err}");
            }
            assert!(err.contains("find . -xdev"), "{err}");
            assert_eq!(names(&state), before);
        }
        other => panic!("{other:?}: {}", stderr(&out)),
    }
}

/// As a user, `prune --all` empties a state directory whose cache holds a
/// tree far deeper than the descriptors it may open (a hard
/// `RLIMIT_NOFILE` of 256, which it cannot raise), as a `RUN` with a cache
/// mount can leave; a store then opens the directory again, as the next
/// build would. Where the machine gives the user no user namespace, it
/// changes nothing.
#[test]
fn prune_all_empties_a_cache_deeper_than_its_descriptor_limit() {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::CommandExt as _;
    const DEPTH: usize = 3000;
    const NOFILE: u64 = 256;
    if skip_as_root() {
        return;
    }
    /// Empties a temp dir on drop, a failed test included: the temp dir's
    /// own removal recurses with a descriptor per level.
    struct Emptied(std::path::PathBuf);
    impl Drop for Emptied {
        fn drop(&mut self) {
            let r = fs::canonicalize(&self.0)
                .map_err(|e| e.to_string())
                .and_then(|p| rnk_rootfs::RootPath::open(&p).map_err(|e| e.to_string()))
                .and_then(|root| {
                    rnk_rootfs::delete_contents(&root, &rnk_rootfs::IgnoreList::new())
                        .map_err(|e| e.to_string())
                });
            // A drop cannot fail the test; a tree left behind is at least shown.
            if let Err(e) = r {
                eprintln!("left behind in {}: {e}", self.0.display());
            }
        }
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let _emptied = Emptied(tmp.path().to_owned());
    // The state directory's checks refuse a parent others may write.
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
    let state = tmp.path().join("state");
    let paths = rnk_store::BuildPaths::new(&state, &state);
    let store = rnk_store::Store::open(&paths).expect("a state directory");
    let cache = store.cache_dir("deep").expect("a cache directory");
    store.close().expect("close");
    let dir = OFlags::PATH | OFlags::DIRECTORY;
    let mut cur = rustix::fs::open(&cache, dir, Mode::empty()).expect("cache");
    for _ in 0..DEPTH {
        rustix::fs::mkdirat(&cur, "d", Mode::from_raw_mode(0o755)).expect("mkdir");
        cur = rustix::fs::openat(&cur, "d", dir, Mode::empty()).expect("open");
    }
    drop(rustix::fs::openat(
        &cur,
        "f",
        OFlags::WRONLY | OFlags::CREATE,
        Mode::from_raw_mode(0o644),
    ));
    drop(cur);

    let mut cmd = command(tmp.path(), &["prune", "--all", "--state-dir=state"], &[]);
    // SAFETY: `setrlimit` is async-signal-safe and touches no memory the
    // parent shares.
    unsafe {
        cmd.pre_exec(|| {
            let limit = rustix::process::Rlimit {
                current: Some(NOFILE),
                maximum: Some(NOFILE),
            };
            rustix::process::setrlimit(rustix::process::Resource::Nofile, limit)
                .map_err(std::io::Error::from)
        });
    }
    let out = cmd.output().expect("run ranikust");
    match out.status.code() {
        Some(0) => {
            assert_eq!(stdout(&out), format!("emptied {}\n", state.display()));
            assert_eq!(names(&state), Vec::<std::path::PathBuf>::new());
            rnk_store::Store::open(&paths)
                .and_then(rnk_store::Store::close)
                .expect("a store in the emptied directory");
        }
        Some(1) => {
            let err = stderr(&out);
            assert!(err.contains("no user namespace for the build: "), "{err}");
        }
        other => panic!("{other:?}: {}", stderr(&out)),
    }
}

/// The guidance line of a refusal for want of a user namespace.
fn userns_advice(err: &str) -> &str {
    err.lines()
        .find_map(|l| l.strip_prefix("  ").filter(|l| l.contains("with sudo")))
        .unwrap_or_else(|| panic!("no advice: {err}"))
}

/// Whether the advice may offer a profile here, decided as ranikust's
/// `no_userns::Here` and `container::detect` decide it: the switch at 1,
/// this process unconfined, in the host's user namespace and in no
/// container.
fn offers_a_profile_here() -> bool {
    use std::io::ErrorKind::NotFound;
    let read = |p: &str| {
        String::from_utf8_lossy(&fs::read(p).unwrap_or_default())
            .trim()
            .to_owned()
    };
    // As Go's `!os.IsNotExist(err)`, which the container check follows.
    let exists = |p: &str| fs::metadata(p).map_or_else(|e| e.kind() != NotFound, |_| true);
    let restricted = read("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").trim() == "1";
    let label = match fs::read("/proc/self/attr/apparmor/current") {
        Err(e) if e.kind() == NotFound => fs::read("/proc/self/attr/current"),
        label => label,
    };
    let unconfined = label.is_ok_and(|v| v.trim_ascii() == b"unconfined");
    let host_userns =
        read("/proc/self/uid_map")
            .split_ascii_whitespace()
            .eq(["0", "0", "4294967295"]);
    // `container::Runtime::ALL`'s names.
    let runtimes = [
        "docker",
        "rkt",
        "systemd-nspawn",
        "lxc",
        "lxc-libvirt",
        "openvz",
        "kube",
        "garden",
        "podman",
        "gvisor",
        "firejail",
        "wsl",
    ];
    let names = |text: &str| !text.trim().is_empty() && runtimes.iter().any(|r| text.contains(r));
    let mounts = read("/proc/mounts");
    let mut root = mounts.lines().next().unwrap_or("").split(' ');
    let overlay_root = matches!(
        (root.next(), root.next(), root.next()),
        (Some(device), Some("/"), Some(fs)) if !device.is_empty() && fs.starts_with("overlay")
    );
    let in_container = [
        "/run/.containerenv",
        "/.dockerenv",
        "/var/run/secrets/kubernetes.io/serviceaccount",
        "/__runsc_containers__",
    ]
    .iter()
    .any(|p| exists(p))
        || names(&read("/proc/self/cgroup"))
        || (exists("/proc/vz") && !exists("/proc/bc"))
        || names(&read("/proc/1/cmdline"))
        || read("/proc/version_signature").starts_with("Microsoft")
        || std::env::var("container").is_ok_and(|v| names(&v))
        || names(&read("/run/systemd/container"))
        || overlay_root;
    restricted && unconfined && host_userns && !in_container
}

/// Red team (#81): the profile is offered only for a path that holds
/// ranikust itself, not for a root-owned program a forged marker names. The
/// loader case is `no_userns`' unit test: a run through the loader needs
/// `seal-none`, which replaces the `no-userns` fault and so would make a
/// user namespace.
#[test]
fn red_team_no_profile_for_a_root_program_that_is_not_ranikust() {
    use std::os::fd::AsRawFd as _;
    if skip_as_root() || !offers_a_profile_here() {
        return;
    }
    let tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    fs::create_dir(tmp.path().join("ctx")).expect("context dir");
    let args = ["--no-push", "--context", "ctx", "--state-dir=state"];
    let interpreter = ["/usr/bin/env", "/usr/bin/perl", "/usr/bin/python3"]
        .iter()
        .find_map(|p| fs::canonicalize(p).ok())
        .expect("an interpreter");
    let copy = tmp.path().join("copy");
    fs::copy(env!("CARGO_BIN_EXE_ranikust"), &copy).expect("copy");
    let held = fs::File::open(&copy).expect("open the copy");
    fs::remove_file(&copy).expect("remove the copy");
    let hex: String = interpreter
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let out = output_of_a_copy(
        Command::new(format!("/proc/self/fd/{}", held.as_raw_fd()))
            .args(args)
            .current_dir(tmp.path())
            .env_clear()
            .envs(container_var())
            .env("RANIKUST_DEBUG_FAULT", "no-userns")
            .env("RANIKUST_SEALED_COPY", format!("tmpfile root {hex}")),
    );
    let advice = userns_advice(&stderr(&out)).to_owned();
    assert!(
        !advice.contains(&format!("changed to {}", interpreter.display())),
        "{advice}"
    );
    // Where the interpreter's path fails the root-only check first, the
    // bytes are never weighed.
    if advice.contains("is not a path only root can change") {
        eprintln!(
            "skipped: {} fails the root-only check",
            interpreter.display()
        );
        return;
    }
    assert!(advice.contains("does not hold ranikust itself"), "{advice}");
}

/// `cmd`'s output, tried again while another test's child, forked before
/// a copy of the binary was closed, still holds it open for writing.
fn output_of_a_copy(cmd: &mut Command) -> Output {
    for _ in 0..100 {
        match cmd.output() {
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::TXTBSY.raw_os_error()) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            out => return out.expect("run the copy"),
        }
    }
    panic!("the copy stayed busy");
}

/// The variable `container`, which ranikust's container check reads, kept
/// where a test clears the rest, so [`offers_a_profile_here`] holds there.
fn container_var() -> Option<(&'static str, std::ffi::OsString)> {
    std::env::var_os("container").map(|v| ("container", v))
}

/// Whether `advice` binds the profile to a path of its own: the install
/// where `/usr/local/bin` is not root's alone says "changed to the
/// binary's", never a path.
fn binds_a_path(advice: &str) -> bool {
    advice.contains("changed to /")
}

/// A link to (or else a copy of) the binary at `to`.
fn place_binary(to: &Path) {
    if fs::hard_link(env!("CARGO_BIN_EXE_ranikust"), to).is_err() {
        fs::copy(env!("CARGO_BIN_EXE_ranikust"), to).expect("copy the binary");
    }
}

/// Red team (#81): terminal controls in the binary's path, `argv[0]` or
/// `PATH` reach no terminal.
#[test]
fn red_team_the_advice_escapes_a_hostile_binary_path() {
    use std::os::unix::process::CommandExt as _;
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let hostile =
        "x\x1b[2J\x1b]8;;https:e.example\x07\r\nranikust: note: trusted\u{202e}\u{2028}\u{9b}31m";
    let dir = tmp.path().join(hostile);
    fs::create_dir(&dir).expect("a hostile directory");
    let binary = dir.join("ranikust");
    place_binary(&binary);
    fs::create_dir(tmp.path().join("ctx")).expect("context dir");
    let path_var = format!("{}:/usr/bin:/bin", dir.display());
    let out = Command::new(&binary)
        .arg0(format!("{hostile}/ranikust"))
        .args([
            "--no-push",
            "--context",
            "ctx",
            "--state-dir=state",
            "--log-format=text",
        ])
        .current_dir(tmp.path())
        .env_clear()
        .envs(container_var())
        .env("RANIKUST_DEBUG_FAULT", "no-userns")
        .env("PATH", &path_var)
        .env("HOME", tmp.path())
        .output()
        .expect("run the hostile binary");
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err:?}");
    assert!(err.contains(STOPS_AS_A_USER), "{err:?}");
    for raw in ["\x1b", "\x07", "\r", "\u{202e}", "\u{2028}", "\u{9b}"] {
        assert!(!err.contains(raw), "{raw:?} reached stderr: {err:?}");
    }
    assert!(
        !err.lines().any(|l| l.starts_with("ranikust: note")),
        "a forged line: {err:?}"
    );
    let advice = userns_advice(&err);
    assert!(!binds_a_path(advice), "{advice}");
    if offers_a_profile_here() {
        assert!(advice.contains("\\u{1b}[2J"), "{advice}");
        assert!(advice.contains(" first"), "{advice}");
    }
    assert!(!tmp.path().join("state").exists());
}

/// Red team (#81): a binary no path names, or a sealed-copy marker set from
/// outside, gets no profile.
#[test]
fn red_team_a_deleted_binary_or_a_forged_marker_gets_no_profile() {
    use std::os::fd::AsRawFd as _;
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    fs::create_dir(tmp.path().join("ctx")).expect("context dir");
    let args = ["--no-push", "--context", "ctx", "--state-dir=state"];
    let hex = |p: &str| -> String { p.bytes().map(|b| format!("{b:02x}")).collect() };
    let run = |program: &Path, marker: Option<&str>| {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(tmp.path())
            .env_clear()
            .envs(container_var())
            .env("RANIKUST_DEBUG_FAULT", "no-userns")
            .env("HOME", tmp.path());
        if let Some(marker) = marker {
            cmd.env("RANIKUST_SEALED_COPY", marker);
        }
        let out = cmd.output().expect("run ranikust");
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(1), "{err}");
        assert!(err.contains(STOPS_AS_A_USER), "{err}");
        userns_advice(&err).to_owned()
    };
    let restricted = offers_a_profile_here();

    let gone = tmp.path().join("gone");
    place_binary(&gone);
    let held = fs::File::open(&gone).expect("open the binary");
    fs::remove_file(&gone).expect("remove its name");
    let advice = run(
        Path::new(&format!("/proc/self/fd/{}", held.as_raw_fd())),
        None,
    );
    assert!(!binds_a_path(&advice), "{advice}");
    if restricted {
        let named = " (deleted) is not a path only root can change";
        assert!(advice.contains(named), "{advice}");
    }

    let binary = fs::canonicalize(env!("CARGO_BIN_EXE_ranikust")).expect("binary");
    for marker in [
        format!("memfd root {}", hex("/usr/local/bin/ranikust")),
        format!("tmpfile root {}", hex("/usr/bin/env")),
        format!("tmpfile root {}", hex("/proc/self/exe")),
    ] {
        let advice = run(&binary, Some(&marker));
        assert!(!binds_a_path(&advice), "{marker}: {advice}");
        assert!(!advice.contains("/usr/bin/env"), "{marker}: {advice}");
        if restricted {
            let named = format!("{} is not a path only root can change", binary.display());
            assert!(advice.contains(&named), "{marker}: {advice}");
        }
    }
    assert!(!tmp.path().join("state").exists());
}

/// Every node under `root`: kind, size, mode, owner, inode, change and
/// modification times, link target.
fn tree(root: &Path) -> Vec<String> {
    use std::os::unix::fs::MetadataExt as _;
    let mut seen = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(path) = todo.pop() {
        let m = fs::symlink_metadata(&path).expect("lstat");
        let link = fs::read_link(&path).ok();
        seen.push(format!(
            "{} {:?} {} {:o} {} {} {}.{} {}.{} {:?}",
            path.display(),
            m.file_type(),
            m.len(),
            m.mode(),
            m.uid(),
            m.ino(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
            link
        ));
        if m.is_dir() {
            for e in fs::read_dir(&path).expect("ls") {
                todo.push(e.expect("entry").path());
            }
        }
    }
    seen.sort();
    seen
}

/// Red team (#81, T22): the refusal writes nothing, whatever the state
/// directory is or points to.
#[test]
fn red_team_nothing_is_written_before_the_refusal_whatever_the_state_dir() {
    if skip_as_root() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let t = tmp.path();
    fs::create_dir(t.join("ctx")).expect("context dir");
    fs::write(t.join("ctx/Dockerfile"), "FROM scratch\n").expect("Dockerfile");
    fs::create_dir(t.join("full")).expect("a state dir with a file");
    fs::write(t.join("full/keep"), b"x").expect("a file");
    fs::create_dir(t.join("target")).expect("a link target");
    std::os::unix::fs::symlink(t.join("target"), t.join("to-dir")).expect("symlink");
    std::os::unix::fs::symlink(t.join("absent"), t.join("dangling")).expect("symlink");
    fs::write(t.join("file"), b"y").expect("a file as the state dir");
    fs::create_dir(t.join("home")).expect("home");
    let home = t.join("home");
    let home = home.to_str().expect("UTF-8 temp dir");
    let before = tree(t);
    let forms: [&[&str]; 7] = [
        &["--state-dir=full"],
        &["--state-dir=to-dir"],
        &["--state-dir=dangling"],
        &["--state-dir=a/b/c"],
        &["--state-dir=file"],
        &["--state-dir=ctx"],
        &[],
    ];
    for form in forms {
        for mode in ["--mode=auto", "--mode=overlay"] {
            let mut args = vec!["--no-push", "--context", "ctx", mode];
            args.extend(form);
            let out = ranikust(t, &args, &[("HOME", home)]);
            let err = stderr(&out);
            assert_eq!(out.status.code(), Some(1), "{args:?}: {err}");
            assert!(err.contains(STOPS_AS_A_USER), "{args:?}: {err}");
            assert_eq!(tree(t), before, "{args:?}: {err}");
        }
    }
}
