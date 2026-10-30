//! The memory gate: whether another stage may start (ADR-0013).
//!
//! Memory, not CPU, bounds parallel stages (exp-08, exp-12). Before a
//! stage starts, the gate compares free memory with a threshold: the
//! cgroup's `memory.max` minus its working set when a limit exists
//! (ADR-0007: cgroups are only read), else `MemAvailable`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

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
    /// Called with the number of stages running. A gate that answers
    /// `admit: true` may assume the scheduler starts one stage on it.
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

// [open: Q-030] the algorithm and thresholds are to be measured; these are
// reversible placeholders: two constants and one function, behind a trait.

/// Free memory below which no further stage starts: an eighth of the
/// limit, at least 512 MiB.
const MIN_HEADROOM: u64 = 512 << 20;

/// How long after admitting a stage the next reading counts as fresh: a
/// stage that has just started has not grown yet, so a reading taken at
/// once would admit every waiting stage on the same free memory.
const SETTLE: Duration = Duration::from_millis(500);

fn threshold(limit: u64) -> u64 {
    (limit / 8).max(MIN_HEADROOM)
}

/// The host's memory: the cgroup v2 limit of this process, else
/// `/proc/meminfo`. Admits at most one stage per fresh reading.
#[derive(Debug, Default)]
pub struct SystemMemory {
    cgroup: Option<PathBuf>,
    /// When the gate last admitted a stage while others ran.
    last_admit: Mutex<Option<Instant>>,
}

/// What one reading found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    free: u64,
    limit: u64,
    current: Option<u64>,
}

impl SystemMemory {
    /// Finds this process's cgroup v2 directory, if `memory.max` is
    /// readable there; cgroup v1 falls back to `MemAvailable`.
    #[must_use]
    pub fn detect() -> Self {
        // No /proc/self/cgroup (no procfs, or not Linux): no cgroup, and
        // the gate falls back to /proc/meminfo.
        let cgroup = fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|s| cgroup_dir(&s, Path::new("/sys/fs/cgroup")))
            .filter(|d| d.join("memory.max").is_file());
        match &cgroup {
            Some(d) => tracing::debug!(cgroup = %d.display(), "memory gate reads the cgroup"),
            None => tracing::debug!("memory gate reads /proc/meminfo"),
        }
        Self::with_cgroup(cgroup)
    }

    fn with_cgroup(cgroup: Option<PathBuf>) -> Self {
        Self {
            cgroup,
            last_admit: Mutex::new(None),
        }
    }

    fn read(&self) -> Option<Reading> {
        if let Some(dir) = &self.cgroup {
            let current = read_u64(&dir.join("memory.current"));
            // An unreadable memory.max or memory.stat counts as missing:
            // no limit, or no page cache to discount.
            let max = fs::read_to_string(dir.join("memory.max"))
                .ok()
                .and_then(|s| parse_max(&s));
            let inactive = fs::read_to_string(dir.join("memory.stat"))
                .ok()
                .and_then(|s| parse_inactive_file(&s));
            if let (Some(current), Some(max)) = (current, max) {
                return Some(cgroup_reading(current, max, inactive));
            }
            let (free, limit) = meminfo()?;
            return Some(Reading {
                free,
                limit,
                current,
            });
        }
        let (free, limit) = meminfo()?;
        Some(Reading {
            free,
            limit,
            current: None,
        })
    }

    fn decide(&self, running: usize, reading: Option<Reading>, now: Instant) -> MemoryCheck {
        let Some(r) = reading else {
            // Nothing readable: the gate cannot judge, so it admits, and
            // `--jobs` alone bounds the build.
            return MemoryCheck {
                admit: true,
                current_bytes: None,
            };
        };
        let mut last = self
            .last_admit
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let fresh = last.is_none_or(|t| now.saturating_duration_since(t) >= SETTLE);
        let admit = running == 0 || (fresh && r.free >= threshold(r.limit));
        if admit && running > 0 {
            *last = Some(now);
        }
        MemoryCheck {
            admit,
            current_bytes: r.current,
        }
    }
}

impl MemoryGate for SystemMemory {
    fn check(&self, running: usize) -> MemoryCheck {
        self.decide(running, self.read(), Instant::now())
    }
}

/// Free memory under a cgroup limit: the limit minus the working set,
/// `memory.current` less the inactive page cache the kernel reclaims
/// before it hits the limit (the kubelet's working set).
fn cgroup_reading(current: u64, max: u64, inactive_file: Option<u64>) -> Reading {
    let working_set = current.saturating_sub(inactive_file.unwrap_or(0));
    Reading {
        free: max.saturating_sub(working_set),
        limit: max,
        current: Some(current),
    }
}

/// `(MemAvailable, MemTotal)` from `/proc/meminfo`.
fn meminfo() -> Option<(u64, u64)> {
    // No /proc/meminfo: nothing to judge by.
    parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)
}

fn read_u64(path: &Path) -> Option<u64> {
    // A missing or malformed file is the same as no value.
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

/// `memory.max`: a number, or `max` for no limit (`None`).
fn parse_max(s: &str) -> Option<u64> {
    // `max` does not parse: no limit.
    s.trim().parse().ok()
}

/// `inactive_file` from `memory.stat`, in bytes.
fn parse_inactive_file(s: &str) -> Option<u64> {
    s.lines()
        .find_map(|l| l.strip_prefix("inactive_file "))?
        .trim()
        .parse()
        // A malformed value is the same as none.
        .ok()
}

/// `(MemAvailable, MemTotal)` in bytes.
fn parse_meminfo(s: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        let line = s.lines().find_map(|l| l.strip_prefix(name))?;
        // A malformed value is the same as a missing field.
        let kb: u64 = line.trim().trim_end_matches("kB").trim().parse().ok()?;
        kb.checked_mul(1024)
    };
    Some((field("MemAvailable:")?, field("MemTotal:")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

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
    fn memory_max_stat_and_meminfo() {
        assert_eq!(parse_max("max\n"), None);
        assert_eq!(parse_max("8589934592\n"), Some(8 * GIB));
        let stat = "anon 1000\nfile 9000\nactive_file 2000\ninactive_file 7000\n";
        assert_eq!(parse_inactive_file(stat), Some(7000));
        assert_eq!(parse_inactive_file("anon 1\n"), None);
        let mi = "MemTotal:       16000000 kB\nMemFree: 1 kB\nMemAvailable:    4000000 kB\n";
        assert_eq!(
            parse_meminfo(mi),
            Some((4_000_000 * 1024, 16_000_000 * 1024))
        );
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn page_cache_does_not_count_as_used() {
        // An 8 GiB pod after pulls: 7.5 GiB charged, 6 GiB of it inactive
        // page cache. Without the discount the gate would see 0.5 GiB free.
        let r = cgroup_reading(7 * GIB + GIB / 2, 8 * GIB, Some(6 * GIB));
        assert_eq!(r.free, 6 * GIB + GIB / 2);
        assert!(r.free >= threshold(r.limit));
        assert_eq!(cgroup_reading(7 * GIB, 8 * GIB, None).free, GIB);
        assert_eq!(cgroup_reading(9 * GIB, 8 * GIB, None).free, 0);
    }

    #[test]
    fn threshold_is_an_eighth_at_least_512_mib() {
        assert_eq!(threshold(8 * GIB), GIB);
        assert_eq!(threshold(GIB), MIN_HEADROOM);
    }

    #[test]
    fn one_stage_per_fresh_reading() {
        let gate = SystemMemory::with_cgroup(None);
        let plenty = Some(Reading {
            free: 8 * GIB,
            limit: 16 * GIB,
            current: Some(GIB),
        });
        let t0 = Instant::now();
        // Nothing runs: admitted, and it does not start the settle time.
        assert!(gate.decide(0, plenty, t0).admit);
        assert!(gate.decide(1, plenty, t0).admit);
        // Same moment, same reading: a second waiting stage waits.
        assert!(!gate.decide(2, plenty, t0).admit);
        assert!(!gate.decide(2, plenty, t0 + SETTLE / 2).admit);
        assert!(gate.decide(2, plenty, t0 + SETTLE).admit);
        // A fresh reading with too little free memory still refuses.
        let tight = Some(Reading {
            free: GIB,
            limit: 16 * GIB,
            current: Some(15 * GIB),
        });
        assert!(!gate.decide(3, tight, t0 + 3 * SETTLE).admit);
        // Unreadable memory admits.
        assert!(gate.decide(3, None, t0 + 3 * SETTLE).admit);
    }

    #[test]
    fn fixture_cgroup_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("memory.current"), format!("{}\n", 7 * GIB)).unwrap();
        fs::write(dir.path().join("memory.max"), format!("{}\n", 8 * GIB)).unwrap();
        fs::write(
            dir.path().join("memory.stat"),
            format!("anon {}\ninactive_file {}\n", GIB, 5 * GIB),
        )
        .unwrap();
        let gate = SystemMemory::with_cgroup(Some(dir.path().to_owned()));
        assert_eq!(
            gate.read(),
            Some(Reading {
                free: 6 * GIB,
                limit: 8 * GIB,
                current: Some(7 * GIB),
            })
        );
        fs::write(dir.path().join("memory.max"), "max\n").unwrap();
        // No limit: /proc/meminfo decides, memory.current is still shown.
        assert_eq!(gate.read().map(|r| r.current), Some(Some(7 * GIB)));
    }

    #[test]
    fn system_memory_answers_on_this_host() {
        // Whatever the host, the gate gives an answer and never panics.
        let _check = SystemMemory::detect().check(1);
    }
}
