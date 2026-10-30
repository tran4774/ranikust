//! The memory gate: whether another stage may start (ADR-0013).
//!
//! Memory, not CPU, bounds parallel stages (exp-08, exp-12). Before a
//! stage starts, the gate compares free memory with a threshold: the
//! cgroup's `memory.max` minus `memory.current` when a limit exists
//! (ADR-0007: cgroups are only read), else `MemAvailable`.

use std::fs;
use std::path::{Path, PathBuf};

/// The answer of a [`MemoryGate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryCheck {
    /// Whether another stage may start now.
    pub admit: bool,
    /// The memory in use the gate saw (`memory.current`), for the
    /// `max_memory_bytes` field of the stage span.
    pub current_bytes: Option<u64>,
}

/// Decides whether another stage may start. The scheduler always admits a
/// stage when none is running, so a gate cannot stop a build.
pub trait MemoryGate: Send + Sync {
    /// Called with the number of stages running.
    fn check(&self, running: usize) -> MemoryCheck;
}

/// Admits every stage: `--jobs` alone bounds the build.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unlimited;

impl MemoryGate for Unlimited {
    fn check(&self, _running: usize) -> MemoryCheck {
        MemoryCheck {
            admit: true,
            current_bytes: None,
        }
    }
}

/// Free memory below which no further stage starts: an eighth of the
/// limit, at least 512 MiB.
// [open: Q-030] the algorithm and thresholds are to be measured; this is a
// reversible placeholder: one function, and the gate is a trait.
const MIN_HEADROOM: u64 = 512 << 20;

fn threshold(limit: u64) -> u64 {
    (limit / 8).max(MIN_HEADROOM)
}

/// The host's memory: the cgroup v2 limit of this process, else
/// `/proc/meminfo`.
#[derive(Debug, Clone, Default)]
pub struct SystemMemory {
    cgroup: Option<PathBuf>,
}

impl SystemMemory {
    /// Finds this process's cgroup v2 directory, if `memory.max` is
    /// readable there; cgroup v1 falls back to `MemAvailable`.
    #[must_use]
    pub fn detect() -> Self {
        let cgroup = fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|s| cgroup_dir(&s, Path::new("/sys/fs/cgroup")))
            .filter(|d| d.join("memory.max").is_file());
        Self { cgroup }
    }

    fn read(&self) -> Option<(u64, u64, Option<u64>)> {
        if let Some(dir) = &self.cgroup
            && let Some(current) = read_u64(&dir.join("memory.current"))
            && let Ok(max) = fs::read_to_string(dir.join("memory.max"))
            && let Some(max) = parse_max(&max)
        {
            return Some((max.saturating_sub(current), max, Some(current)));
        }
        let current = self
            .cgroup
            .as_ref()
            .and_then(|d| read_u64(&d.join("memory.current")));
        let (available, total) = parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)?;
        Some((available, total, current))
    }
}

impl MemoryGate for SystemMemory {
    fn check(&self, _running: usize) -> MemoryCheck {
        match self.read() {
            Some((free, limit, current)) => MemoryCheck {
                admit: free >= threshold(limit),
                current_bytes: current,
            },
            // Nothing readable (no /proc): the gate cannot judge, so it
            // admits, and `--jobs` alone bounds the build.
            None => MemoryCheck {
                admit: true,
                current_bytes: None,
            },
        }
    }
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The cgroup v2 directory from `/proc/self/cgroup` (`0::/path`).
fn cgroup_dir(proc_cgroup: &str, mount: &Path) -> Option<PathBuf> {
    let path = proc_cgroup.lines().find_map(|l| l.strip_prefix("0::"))?;
    let rel = path.trim().trim_start_matches('/');
    if rel.split('/').any(|c| c == "..") {
        return None;
    }
    Some(if rel.is_empty() {
        mount.to_owned()
    } else {
        mount.join(rel)
    })
}

/// `memory.max`: a number, or `max` for no limit.
fn parse_max(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

/// `(MemAvailable, MemTotal)` in bytes.
fn parse_meminfo(s: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        let line = s.lines().find_map(|l| l.strip_prefix(name))?;
        let kb: u64 = line.trim().trim_end_matches("kB").trim().parse().ok()?;
        kb.checked_mul(1024)
    };
    Some((field("MemAvailable:")?, field("MemTotal:")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_dir_from_proc() {
        let m = Path::new("/sys/fs/cgroup");
        assert_eq!(cgroup_dir("0::/\n", m), Some(m.to_owned()));
        assert_eq!(
            cgroup_dir("1:name=systemd:/x\n0::/user.slice/a.scope\n", m),
            Some(m.join("user.slice/a.scope"))
        );
        assert_eq!(cgroup_dir("0::/../etc\n", m), None);
        assert_eq!(cgroup_dir("12:memory:/docker/abc\n", m), None);
    }

    #[test]
    fn memory_max_and_meminfo() {
        assert_eq!(parse_max("max\n"), None);
        assert_eq!(parse_max("8589934592\n"), Some(8 << 30));
        let mi = "MemTotal:       16000000 kB\nMemFree: 1 kB\nMemAvailable:    4000000 kB\n";
        assert_eq!(
            parse_meminfo(mi),
            Some((4_000_000 * 1024, 16_000_000 * 1024))
        );
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn threshold_is_an_eighth_at_least_512_mib() {
        assert_eq!(threshold(8 << 30), 1 << 30);
        assert_eq!(threshold(1 << 30), MIN_HEADROOM);
    }

    #[test]
    fn system_memory_answers_on_this_host() {
        // Whatever the host, the gate gives an answer and never panics.
        let _check = SystemMemory::detect().check(1);
    }
}
