//! The metrics report of the real binary on each way out: success, error,
//! panic (the panic hook, then abort, as in a release build), an unwritable
//! report path, and metrics off. Every file goes under a fresh temp dir.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const SIGABRT: i32 = 6;

/// Runs the binary in `cwd` with a clean metrics environment plus `env`.
fn ranikust(cwd: &Path, env: &[(&str, &OsStr)], fault: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ranikust"));
    cmd.current_dir(cwd);
    cmd.env_remove("RANIKUST_METRICS_FILE")
        .env_remove("RANIKUST_METRICS_SUMMARY")
        .env_remove("RANIKUST_DEBUG_FAULT")
        .env("RUST_BACKTRACE", "0");
    for (key, value) in env {
        cmd.env(key, value);
    }
    if let Some(fault) = fault {
        cmd.env("RANIKUST_DEBUG_FAULT", fault);
    }
    cmd.output().expect("run ranikust")
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("report")).expect("json")
}

#[test]
fn success_writes_the_report() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("out/metrics.json");
    let out = ranikust(
        tmp.path(),
        &[("RANIKUST_METRICS_FILE", path.as_os_str())],
        None,
    );
    assert_eq!(out.status.code(), Some(0));

    let report = read(&path);
    assert_eq!(report["schema"], "ranikust.metrics/v1");
    assert_eq!(report["exit"]["status"], "success");
    assert_eq!(report["exit"]["code"], 0);
    assert_eq!(
        report["meta"]["ranikust_version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(report["phases"]["ranikust.run"]["count"], 1);
    assert!(report["resources"]["rusage_self"]["user_ns"].is_u64());
    for section in ["stages", "commands", "snapshots"] {
        assert!(report[section].is_array(), "{section}");
    }
    // Sorted keys, two-space indent, final newline.
    let text = fs::read_to_string(&path).expect("report");
    assert!(text.starts_with("{\n  \"commands\": "));
    assert!(text.ends_with("}\n"));
}

#[test]
fn error_keeps_its_exit_code_and_is_reported() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("metrics.json");
    let out = ranikust(
        tmp.path(),
        &[("RANIKUST_METRICS_FILE", path.as_os_str())],
        Some("error"),
    );
    assert_eq!(out.status.code(), Some(1));

    let report = read(&path);
    assert_eq!(report["exit"]["status"], "error");
    assert_eq!(report["exit"]["code"], 1);
    assert!(
        report["exit"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("deliberate error"))
    );
    assert_eq!(report["phases"]["ranikust.run"]["count"], 1);
}

#[test]
fn panic_hook_writes_the_report_before_the_abort() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("metrics.json");
    let out = ranikust(
        tmp.path(),
        &[("RANIKUST_METRICS_FILE", path.as_os_str())],
        Some("panic"),
    );
    // Killed by SIGABRT, as `panic = "abort"` does: nothing after the hook ran.
    assert_eq!(out.status.signal(), Some(SIGABRT), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("deliberate panic"), "{stderr}");

    let report = read(&path);
    assert_eq!(report["exit"]["status"], "panic");
    assert!(
        report["exit"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("deliberate panic"))
    );
    // The run span never closed: counted as open, its wall time up to the panic.
    assert_eq!(report["phases"]["ranikust.run"]["open"], 1);
    assert_eq!(report["phases"]["ranikust.run"]["count"], 0);
}

#[test]
fn unwritable_report_only_warns() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let blocker = tmp.path().join("file");
    fs::write(&blocker, b"").expect("write");
    let out = ranikust(
        tmp.path(),
        &[(
            "RANIKUST_METRICS_FILE",
            blocker.join("metrics.json").as_os_str(),
        )],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("warning: metrics: writing"), "{stderr}");
}

#[test]
fn summary_goes_to_stderr() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &[("RANIKUST_METRICS_SUMMARY", OsStr::new("1"))],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ranikust metrics: success (exit 0)"),
        "{stderr}"
    );
    assert!(stderr.contains("ranikust.run"), "{stderr}");
    assert_eq!(fs::read_dir(tmp.path()).expect("read_dir").count(), 0);
}

#[test]
fn metrics_off_writes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = ranikust(
        tmp.path(),
        &[("RANIKUST_METRICS_FILE", OsStr::new(""))],
        Some("error"),
    );
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("metrics"), "{stderr}");
    assert_eq!(fs::read_dir(tmp.path()).expect("read_dir").count(), 0);
}
