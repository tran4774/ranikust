//! Process and container counters: `getrusage`, `/proc/self/status`, cgroup
//! v2 files and `/proc/net/dev`.
//!
//! Same keys and units as kaniko's `resources` section
//! (`kaniko@88b6d2b75 pkg/metrics/report.go:206-398`). Every reader is best
//! effort: a file that is missing or unreadable becomes an `error` entry,
//! never a failure.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// cgroup v2 files sampled at start and at exit (kaniko's list).
const CGROUP_FILES: [&str; 9] = [
    "cpu.stat",
    "cpu.max",
    "io.stat",
    "memory.current",
    "memory.peak",
    "memory.max",
    "memory.stat",
    "memory.events",
    "pids.peak",
];

/// cgroup files that are cumulative counters, so `end - start` means
/// something.
const CGROUP_COUNTERS: [&str; 3] = ["cpu.stat", "io.stat", "memory.events"];

/// Where the readers look; tests point them at a temporary directory.
#[derive(Clone, Debug)]
pub(crate) struct HostPaths {
    pub(crate) proc_self_cgroup: PathBuf,
    pub(crate) cgroup_root: PathBuf,
    pub(crate) proc_self_status: PathBuf,
    pub(crate) proc_net_dev: PathBuf,
}

impl Default for HostPaths {
    fn default() -> Self {
        Self {
            proc_self_cgroup: PathBuf::from("/proc/self/cgroup"),
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            proc_self_status: PathBuf::from("/proc/self/status"),
            proc_net_dev: PathBuf::from("/proc/net/dev"),
        }
    }
}

/// The cumulative counters worth a before/after pair: the cgroup and the
/// network interfaces.
pub(crate) fn sample(paths: &HostPaths) -> Value {
    let mut out = Map::new();
    out.insert("cgroup".to_owned(), cgroup(paths));
    match netdev(&paths.proc_net_dev) {
        Ok(devices) => out.insert("netdev".to_owned(), devices),
        Err(err) => out.insert("netdev_error".to_owned(), Value::from(err.to_string())),
    };
    Value::Object(out)
}

/// The `resources` section: rusage of this process and of its waited-for
/// children, `/proc/self/status`, the start and end samples, and the deltas
/// of the cumulative counters.
pub(crate) fn report(start: &Value, end: &Value, paths: &HostPaths) -> Value {
    json!({
        "rusage_self": rusage(Who::Myself),
        "rusage_children": rusage(Who::Children),
        "proc_status": proc_status(&paths.proc_self_status),
        "start": start,
        "end": end,
        "delta": delta(start, end),
    })
}

#[derive(Clone, Copy)]
enum Who {
    Myself,
    Children,
}

/// `getrusage(2)`; `maxrss_kb` is in KiB, as Linux reports it.
fn rusage(who: Who) -> Value {
    let who = match who {
        Who::Myself => libc::RUSAGE_SELF,
        Who::Children => libc::RUSAGE_CHILDREN,
    };
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is valid, writable memory for one `libc::rusage`;
    // getrusage writes only into it and keeps no pointer.
    let rc = unsafe { libc::getrusage(who, usage.as_mut_ptr()) };
    if rc != 0 {
        return json!({ "error": io::Error::last_os_error().to_string() });
    }
    // SAFETY: the memory was zeroed, a valid `rusage` (integers only), and
    // getrusage succeeded, so it holds the kernel's values.
    let usage = unsafe { usage.assume_init() };
    let micros = |tv: libc::timeval| -> u64 {
        let secs = u64::try_from(tv.tv_sec).unwrap_or(0);
        let usecs = u64::try_from(tv.tv_usec).unwrap_or(0);
        secs.saturating_mul(1_000_000_000)
            .saturating_add(usecs.saturating_mul(1_000))
    };
    let count = |v: libc::c_long| u64::try_from(v).unwrap_or(0);
    json!({
        "user_ns": micros(usage.ru_utime),
        "sys_ns": micros(usage.ru_stime),
        "maxrss_kb": count(usage.ru_maxrss),
        "minflt": count(usage.ru_minflt),
        "majflt": count(usage.ru_majflt),
        "inblock": count(usage.ru_inblock),
        "oublock": count(usage.ru_oublock),
        "nvcsw": count(usage.ru_nvcsw),
        "nivcsw": count(usage.ru_nivcsw),
    })
}

/// Peak and current resident and virtual size, and the thread count.
fn proc_status(path: &Path) -> Value {
    match fs::read_to_string(path) {
        Ok(text) => parse_proc_status(&text),
        Err(err) => json!({ "error": err.to_string() }),
    }
}

fn parse_proc_status(text: &str) -> Value {
    let mut out = Map::new();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if !matches!(key, "VmHWM" | "VmRSS" | "VmPeak" | "VmSize" | "Threads") {
            continue;
        }
        let mut words = rest.split_whitespace();
        let Some(Ok(value)) = words.next().map(str::parse::<i64>) else {
            continue;
        };
        let key = if words.next() == Some("kB") {
            format!("{key}_kb")
        } else {
            key.to_owned()
        };
        out.insert(key, Value::from(value));
    }
    Value::Object(out)
}

/// The cgroup v2 directory of this process and the values of
/// [`CGROUP_FILES`] in it.
fn cgroup(paths: &HostPaths) -> Value {
    let (dir, tried) = cgroup_dir(paths);
    let mut out = Map::new();
    out.insert(
        "dir".to_owned(),
        Value::from(
            dir.as_deref()
                .map(|d| d.display().to_string())
                .unwrap_or_default(),
        ),
    );
    out.insert("tried".to_owned(), Value::from(tried));
    if let Some(dir) = dir {
        let mut values = Map::new();
        let mut errors = Map::new();
        for name in CGROUP_FILES {
            match fs::read_to_string(dir.join(name)) {
                Ok(text) => values.insert(name.to_owned(), parse_cgroup_file(&text)),
                Err(err) => errors.insert(name.to_owned(), Value::from(err.to_string())),
            };
        }
        out.insert("values".to_owned(), Value::Object(values));
        if !errors.is_empty() {
            out.insert("errors".to_owned(), Value::Object(errors));
        }
    }
    Value::Object(out)
}

/// The path from `/proc/self/cgroup` under the cgroup root when the whole
/// hierarchy is mounted, else the root itself (a container's own cgroup under
/// a cgroup namespace); the first that has `memory.current` (the root cgroup
/// has none).
fn cgroup_dir(paths: &HostPaths) -> (Option<PathBuf>, Vec<String>) {
    let mut candidates = Vec::new();
    // Unreadable (no procfs, cgroup v1 only): only the root is tried, and
    // `tried` in the report shows it.
    if let Ok(text) = fs::read_to_string(&paths.proc_self_cgroup) {
        for line in text.lines() {
            if let Some(rel) = line.strip_prefix("0::")
                && !rel.is_empty()
                && rel != "/"
            {
                candidates.push(paths.cgroup_root.join(rel.trim_start_matches('/')));
            }
        }
    }
    candidates.push(paths.cgroup_root.clone());
    let mut tried = Vec::new();
    for candidate in candidates {
        tried.push(candidate.display().to_string());
        if candidate.join("memory.current").exists() {
            return (Some(candidate), tried);
        }
    }
    (None, tried)
}

/// Parses a single value, a flat keyed file (`key value` per line) or a
/// nested keyed file (`dev key=value ...` per line).
fn parse_cgroup_file(text: &str) -> Value {
    let text = text.trim();
    if !text.contains('\n') && !text.contains('=') {
        return text
            .parse::<i64>()
            .map_or_else(|_| Value::from(text), Value::from);
    }
    let mut flat = Map::new();
    let mut nested = Map::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(key), Some(first)) = (words.next(), words.next()) else {
            continue;
        };
        if !first.contains('=') {
            if let Ok(n) = first.parse::<i64>() {
                flat.insert(key.to_owned(), Value::from(n));
            }
            continue;
        }
        let mut pairs = Map::new();
        for pair in std::iter::once(first).chain(words) {
            if let Some((k, v)) = pair.split_once('=')
                && let Ok(n) = v.parse::<i64>()
            {
                pairs.insert(k.to_owned(), Value::from(n));
            }
        }
        nested.insert(key.to_owned(), Value::Object(pairs));
    }
    if nested.is_empty() {
        Value::Object(flat)
    } else {
        Value::Object(nested)
    }
}

/// Byte and packet counters per interface of the network namespace; in a
/// container that is all traffic of the build.
fn netdev(path: &Path) -> io::Result<Value> {
    let text = fs::read_to_string(path)?;
    let mut out = Map::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let numbers: Vec<i64> = rest
            .split_whitespace()
            .map(|w| w.parse().unwrap_or(0))
            .collect();
        let [
            rx_bytes,
            rx_packets,
            _,
            _,
            _,
            _,
            _,
            _,
            tx_bytes,
            tx_packets,
            ..,
        ] = numbers[..]
        else {
            continue;
        };
        out.insert(
            name.trim().to_owned(),
            json!({
                "rx_bytes": rx_bytes,
                "rx_packets": rx_packets,
                "tx_bytes": tx_bytes,
                "tx_packets": tx_packets,
            }),
        );
    }
    Ok(Value::Object(out))
}

/// `end - start` of the cumulative counters: cgroup `cpu.stat`, `io.stat`
/// and `memory.events`, and every interface of `netdev`.
fn delta(start: &Value, end: &Value) -> Value {
    let mut cgroup = Map::new();
    for name in CGROUP_COUNTERS {
        let pointer = format!("/cgroup/values/{name}");
        if let (Some(a), Some(b)) = (start.pointer(&pointer), end.pointer(&pointer)) {
            cgroup.insert(name.to_owned(), subtract(a, b));
        }
    }
    let mut out = Map::new();
    out.insert("cgroup".to_owned(), Value::Object(cgroup));
    if let (Some(a), Some(b)) = (start.get("netdev"), end.get("netdev")) {
        out.insert("netdev".to_owned(), subtract(a, b));
    }
    Value::Object(out)
}

/// `b - a`, key by key, recursing into objects; keys missing on either side
/// are left out.
fn subtract(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => Value::Object(
            b.iter()
                .filter_map(|(k, bv)| a.get(k).map(|av| (k.clone(), subtract(av, bv))))
                .filter(|(_, v)| !v.is_null())
                .collect(),
        ),
        (Value::Number(a), Value::Number(b)) => match (a.as_i64(), b.as_i64()) {
            (Some(a), Some(b)) => Value::from(b.saturating_sub(a)),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_host(dir: &Path) -> HostPaths {
        HostPaths {
            proc_self_cgroup: dir.join("proc-self-cgroup"),
            cgroup_root: dir.join("cgroup"),
            proc_self_status: dir.join("proc-self-status"),
            proc_net_dev: dir.join("proc-net-dev"),
        }
    }

    #[test]
    fn missing_files_degrade_to_error_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = fake_host(tmp.path());
        let start = sample(&paths);
        assert_eq!(start["cgroup"]["dir"], "");
        assert!(start["cgroup"].get("values").is_none());
        assert!(start["netdev_error"].is_string());
        let resources = report(&start, &start, &paths);
        assert!(resources["proc_status"]["error"].is_string());
        assert_eq!(resources["delta"], json!({ "cgroup": {} }));
        // getrusage always works for this process.
        assert!(resources["rusage_self"]["user_ns"].is_u64());
        assert!(resources["rusage_children"]["maxrss_kb"].is_u64());
    }

    #[test]
    fn cgroup_files_are_parsed_and_unreadable_ones_listed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = fake_host(tmp.path());
        let dir = paths.cgroup_root.join("build.slice/job");
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(&paths.proc_self_cgroup, "0::/build.slice/job\n").expect("write");
        fs::write(dir.join("memory.current"), "4096\n").expect("write");
        fs::write(dir.join("memory.peak"), "8192\n").expect("write");
        fs::write(dir.join("memory.max"), "max\n").expect("write");
        fs::write(dir.join("cpu.stat"), "usage_usec 100\nuser_usec 60\n").expect("write");
        fs::write(
            dir.join("io.stat"),
            "8:0 rbytes=10 wbytes=20 rios=1 wios=2\n",
        )
        .expect("write");

        let start = sample(&paths);
        let cg = &start["cgroup"];
        assert_eq!(cg["dir"], dir.display().to_string());
        assert_eq!(cg["values"]["memory.current"], 4096);
        assert_eq!(cg["values"]["memory.max"], "max");
        assert_eq!(cg["values"]["cpu.stat"]["user_usec"], 60);
        assert_eq!(cg["values"]["io.stat"]["8:0"]["wbytes"], 20);
        assert!(cg["errors"]["pids.peak"].is_string());

        fs::write(dir.join("cpu.stat"), "usage_usec 350\nuser_usec 61\n").expect("write");
        fs::write(
            dir.join("io.stat"),
            "8:0 rbytes=15 wbytes=20 rios=2 wios=2\n",
        )
        .expect("write");
        let end = sample(&paths);
        let d = delta(&start, &end);
        assert_eq!(
            d["cgroup"]["cpu.stat"],
            json!({ "usage_usec": 250, "user_usec": 1 })
        );
        assert_eq!(d["cgroup"]["io.stat"]["8:0"]["rbytes"], 5);
        assert!(d["cgroup"].get("memory.events").is_none());
    }

    #[test]
    fn falls_back_to_the_cgroup_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = fake_host(tmp.path());
        fs::create_dir_all(&paths.cgroup_root).expect("mkdir");
        fs::write(&paths.proc_self_cgroup, "0::/elsewhere\n").expect("write");
        fs::write(paths.cgroup_root.join("memory.current"), "1\n").expect("write");
        let (dir, tried) = cgroup_dir(&paths);
        assert_eq!(dir, Some(paths.cgroup_root.clone()));
        assert_eq!(tried.len(), 2);
    }

    #[test]
    fn proc_status_and_netdev_parse() {
        let status = "Name:\tranikust\nVmPeak:\t  2000 kB\nVmHWM:\t 1500 kB\nThreads:\t4\nVmRSS:\tbogus kB\n";
        assert_eq!(
            parse_proc_status(status),
            json!({ "VmPeak_kb": 2000, "VmHWM_kb": 1500, "Threads": 4 })
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("dev");
        fs::write(
            &path,
            "Inter-|   Receive\n face |bytes packets\n  eth0: 100 2 0 0 0 0 0 0 300 4 0 0 0 0 0 0\n  short: 1 2\n",
        )
        .expect("write");
        assert_eq!(
            netdev(&path).expect("netdev"),
            json!({ "eth0": { "rx_bytes": 100, "rx_packets": 2, "tx_bytes": 300, "tx_packets": 4 } })
        );
    }
}
