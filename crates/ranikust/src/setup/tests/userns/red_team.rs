//! Red team of root's set-up for `--userns` (ADR-0104, ADR-0139):
//! processes of the range's first uid on the host race root's state
//! directory, store check, leftovers' removal and user namespace probe in
//! the directory it is given (T4, T21).

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, chown, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{alone_in, in_child, measured, spawn_child};

/// The range's first uid and gid: the build's root on the host.
const RANGE_IDS: (u32, u32) = (330_000, 430_000);
/// A third user's ids.
const FAR: (u32, u32) = (530_000, 530_000);
/// Rounds of root's set-up.
const ROUNDS: usize = 150;
/// Processes of the range racing it.
const RACERS: usize = 3;

/// The range root's builds use, from [`RANGE_IDS`].
fn range() -> rnk_sandbox::SubordinateIds {
    rnk_sandbox::SubordinateIds {
        uids: rnk_sandbox::IdRange {
            start: RANGE_IDS.0,
            count: 65_536,
        },
        gids: rnk_sandbox::IdRange {
            start: RANGE_IDS.1,
            count: 65_536,
        },
    }
}

/// `path` given to `ids` with `mode`.
fn own(path: &Path, ids: (u32, u32), mode: u32) {
    chown(path, Some(ids.0), Some(ids.1)).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// The value of `/proc/sys/fs/<name>` while it lives, set back after.
struct Sysctl(&'static str, String);

impl Sysctl {
    fn set(name: &'static str, value: &str) -> Self {
        let path = format!("/proc/sys/fs/{name}");
        let was = fs::read_to_string(&path).unwrap();
        fs::write(&path, value).unwrap();
        Self(name, was)
    }
}

impl Drop for Sysctl {
    fn drop(&mut self) {
        let _ = fs::write(format!("/proc/sys/fs/{}", self.0), self.1.trim());
    }
}

/// A fresh directory below `/var/lib`, root's, mode `0755`, so that the
/// range's uid may list it (and see the state directory's temporary
/// name); removed after.
struct Base(PathBuf);

impl Drop for Base {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Root's files outside the state directories: owner, mode and content
/// of each, a directory's entries; not the link count, which a hard link
/// the range makes raises.
fn watched(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for name in ["victim", "vdir", "vdir/f", "far", "fardir", "fardir/keep"] {
        let p = base.join(name);
        let m = fs::symlink_metadata(&p).unwrap();
        let content = if m.is_file() {
            String::from_utf8_lossy(&fs::read(&p).unwrap()).into_owned()
        } else {
            let mut n: Vec<String> = fs::read_dir(&p)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            n.sort();
            n.join(",")
        };
        out.push(format!(
            "{name} {:o} {}:{} {content}",
            m.mode(),
            m.uid(),
            m.gid()
        ));
    }
    out
}

/// `mkfifo`; whether it was made.
fn fifo(path: &Path) -> bool {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .is_ok()
}

/// What the racer did, by kind.
#[derive(Default)]
struct Counts {
    rounds: u64,
    /// A name made in `base` itself, root's: never.
    precreated: u64,
    /// Plants in a state directory before its move into place.
    before_rename: u64,
    /// `builds` swapped in a state directory.
    builds: u64,
    /// A probe directory moved away with a link out left at its name.
    probe_moved: u64,
    /// A probe directory removed.
    probe_removed: u64,
    /// A name in a probe directory swapped for a hard link or a link out.
    probe_swapped: u64,
}

/// The range's plants in the state directory `dir`, round `i`: the
/// store's marker, `builds` swapped for a link out, a hard link to root's
/// file or a FIFO, and now and then probe leftovers of each kind; whether
/// anything was made.
fn plant_state(dir: &Path, i: u64, victim: &Path, vdir: &Path, far: &Path) -> bool {
    let mut made = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(rnk_store::STATE_MARKER))
        .is_ok();
    let builds = dir.join("builds");
    // Removed, not moved aside: a name per round would pile up.
    let _ = fs::remove_file(&builds);
    let _ = fs::remove_dir(&builds);
    made |= match i % 3 {
        0 => symlink(vdir, &builds).is_ok(),
        1 => fs::hard_link(victim, &builds).is_ok(),
        _ => fifo(&builds),
    };
    if i.is_multiple_of(64) {
        let probe = |s: &str| dir.join(format!("{}rt-{i}-{s}", rnk_store::USERNS_PROBE_PREFIX));
        made |= symlink(vdir, probe("a")).is_ok();
        made |= fs::hard_link(victim, probe("b")).is_ok();
        made |= fifo(&probe("c"));
        made |= fs::hard_link(far, probe("d")).is_ok();
    }
    made
}

/// The range's moves on root's probe directory `p`, round `i`: moved
/// away with a link out to `vdir` at its name, removed, or, given, each
/// name root made or makes in it swapped for a hard link to `victim` or
/// a link out.
fn attack_probe(p: &Path, i: u64, victim: &Path, vdir: &Path, n: &mut Counts) {
    match i % 4 {
        0 => {
            if fs::rename(p, p.with_extension(format!("moved{i}"))).is_ok() {
                n.probe_moved += 1;
                let _ = symlink(vdir, p);
            }
        }
        1 => {
            if fs::remove_dir(p).is_ok() {
                n.probe_removed += 1;
            }
        }
        _ => {
            let victim_ino = fs::symlink_metadata(victim).map_or(0, |m| m.ino());
            for name in ["overlay", "proc", "sys", "chown", "mknod"] {
                let t = p.join(name);
                match fs::symlink_metadata(&t) {
                    // Ours from an earlier round: removed, so nothing piles up.
                    Ok(m) if m.file_type().is_symlink() || m.ino() == victim_ino => {
                        let _ = fs::remove_file(&t);
                    }
                    Ok(_) => {
                        if fs::rename(&t, p.join(format!("{name}.{i}"))).is_err() {
                            continue;
                        }
                    }
                    Err(_) => {}
                }
                let swapped = if i.is_multiple_of(2) {
                    fs::hard_link(victim, &t).is_ok()
                } else {
                    symlink(vdir, &t).is_ok()
                };
                if swapped {
                    n.probe_swapped += 1;
                }
            }
        }
    }
}

/// The racer, as the range's uid: until `base/stop` shows (or four
/// minutes), every move it may make on root's set-up in `base`.
fn race(base: &Path, victim: &Path, vdir: &Path, far: &Path) -> String {
    let stop = base.join("stop");
    let start = Instant::now();
    let mut n = Counts::default();
    let prefix = rnk_store::USERNS_PROBE_PREFIX;
    while !stop.exists() && start.elapsed() < Duration::from_secs(240) {
        n.rounds += 1;
        let i = n.rounds;
        if fs::create_dir(base.join(format!(".ranikust-new-{i}"))).is_ok() {
            n.precreated += 1;
        }
        let Ok(entries) = fs::read_dir(base) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            if name.starts_with(".ranikust-new-") {
                if plant_state(&p, i, victim, vdir, far) {
                    n.before_rename += 1;
                }
                continue;
            }
            if !name.starts_with('s') || name == "stop" {
                continue;
            }
            if plant_state(&p, i, victim, vdir, far) {
                n.builds += 1;
            }
            let Ok(inner) = fs::read_dir(&p) else {
                continue;
            };
            for probe in inner.flatten() {
                let pn = probe.file_name().to_string_lossy().into_owned();
                // Root's, `-again` too; not those moved or planted.
                if pn.starts_with(prefix) && pn.contains("root-") && !pn.contains(".moved") {
                    attack_probe(&probe.path(), i, victim, vdir, &mut n);
                }
            }
        }
    }
    format!(
        "rounds {} precreated {} before_rename {} builds {} probe_moved {} probe_removed {} \
         probe_swapped {}",
        n.rounds,
        n.precreated,
        n.before_rename,
        n.builds,
        n.probe_moved,
        n.probe_removed,
        n.probe_swapped
    )
}

/// Root's side: [`ROUNDS`] state directories made for the range, each
/// checked as `--userns` does before the namespace, its leftovers
/// removed, then the user namespace probe run in it; a tally of the
/// outcomes. Ends with `SIGALRM` if anything blocks.
fn root_side(base: &Path) -> String {
    // SAFETY: `alarm` only arms this process's timer.
    unsafe { libc::alarm(600) };
    let owner = crate::statedir::Owner::Mapped {
        uid: RANGE_IDS.0,
        gid: RANGE_IDS.1,
    };
    let ids = rnk_sandbox::BuildIds::Shifted(range());
    let mut tally = std::collections::BTreeMap::<String, u32>::new();
    let mut note = |what: String| *tally.entry(what).or_default() += 1;
    for i in 0..ROUNDS {
        let state = base.join(format!("s{i}"));
        // As `--userns` does: the store's hold kept through the probe.
        let staged = crate::statedir::stage(&state, owner).and_then(|staged| {
            let held = rnk_store::Store::check_state_dir(&state)
                .map_err(|e| crate::statedir::Refused(format!("the store's check: {e}")))?;
            staged.finish()?;
            Ok(held)
        });
        let _held = match staged {
            Ok(held) => {
                note("stage taken".to_owned());
                held
            }
            Err(e) => {
                let e: String = e.0.chars().take(70).collect();
                note(format!("stage refused: {e}"));
                continue;
            }
        };
        crate::setup::remove_own_probe_leftovers(&state);
        let name = format!("{}root-{i}", rnk_store::USERNS_PROBE_PREFIX);
        note(match crate::setup::probe_in_dir(&state, &name, ids) {
            Ok(_) => "probe ran".to_owned(),
            Err(e) => format!("probe failed: {}", e.chars().take(70).collect::<String>()),
        });
    }
    // SAFETY: as above, cancelled.
    unsafe { libc::alarm(0) };
    tally
        .iter()
        .map(|(k, v)| format!("{v}x {k}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// T2, as real root under `--userns` (ADR-0139): processes of the range's
/// first uid race root's set-up in the directory given to it — plants
/// in the state directory before its move into place, `builds` and the
/// store's marker swapped for links out, hard links to root's file (with
/// `fs.protected_hardlinks` and `fs.protected_symlinks` off) and FIFOs,
/// probe directories moved, removed, or their names swapped — while root
/// makes, checks and probes 150 state directories: root's set-up never
/// blocks, no file of root's or of a third user outside the state
/// directories changes owner, mode or content or is deleted, and no name
/// is made in root's directory but by root.
#[test]
#[ignore = "T2: real root sets up --userns state directories while a process of the range races it"]
fn red_team_the_range_racing_root_s_set_up_changes_nothing_of_root_s() {
    let name = "red_team_the_range_racing_root_s_set_up_changes_nothing_of_root_s";
    if !alone_in(module_path!(), name) {
        return;
    }
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let _links = Sysctl::set("protected_hardlinks", "0");
    let _symlinks = Sysctl::set("protected_symlinks", "0");
    let base = Base(PathBuf::from(format!(
        "/var/lib/rnk-red-team-race-{}",
        std::process::id()
    )));
    let _ = fs::remove_dir_all(&base.0);
    fs::create_dir(&base.0).unwrap();
    let b = base.0.as_path();
    own(b, (0, 0), 0o755);
    let victim = b.join("victim");
    fs::write(&victim, "root").unwrap();
    own(&victim, (0, 0), 0o600);
    let vdir = b.join("vdir");
    fs::create_dir(&vdir).unwrap();
    fs::write(vdir.join("f"), "root").unwrap();
    own(&vdir.join("f"), (0, 0), 0o644);
    own(&vdir, (0, 0), 0o755);
    let far = b.join("far");
    fs::write(&far, "far").unwrap();
    own(&far, FAR, 0o644);
    let fardir = b.join("fardir");
    fs::create_dir(&fardir).unwrap();
    fs::write(fardir.join("keep"), "far").unwrap();
    own(&fardir.join("keep"), FAR, 0o644);
    own(&fardir, FAR, 0o755);
    let before = watched(b);

    let racers: Vec<_> = (0..RACERS)
        .map(|_| {
            let (v, d, f, bb) = (victim.clone(), vdir.clone(), far.clone(), b.to_path_buf());
            spawn_child(Some(RANGE_IDS), move |_| race(&bb, &v, &d, &f))
        })
        .collect();
    let started = Instant::now();
    let root = in_child(None, || root_side(b));
    let took = started.elapsed();
    fs::write(b.join("stop"), "").unwrap();
    let mut each = Vec::new();
    for (racer, mut from_racer) in racers {
        let mut raced = String::new();
        from_racer.read_to_string(&mut raced).unwrap();
        let waited = rustix::process::waitpid(Some(racer), rustix::process::WaitOptions::empty());
        assert!(matches!(waited, Ok(Some(_))), "{waited:?}");
        each.push(raced);
    }
    let raced = each.join(" | ");
    measured(&format!("root, in {took:?}: {root}"));
    measured(&format!("the range: {raced}"));
    assert!(!root.starts_with("error:"), "{root}");
    for r in &each {
        assert!(r.starts_with("rounds "), "{r}");
        assert!(r.contains(" precreated 0 "), "{r}");
    }
    assert_eq!(watched(b), before, "root: {root}; the range: {raced}");
    // Every name in root's directory is root's, or a state directory.
    for e in fs::read_dir(b).unwrap() {
        let e = e.unwrap();
        let n = e.file_name().to_string_lossy().into_owned();
        let uid = e.metadata().unwrap().uid();
        assert!(
            uid == 0 || uid == FAR.0 || n.starts_with('s'),
            "{n} of uid {uid} in root's directory"
        );
        assert!(!n.starts_with(".ranikust-new-"), "{n} left");
    }
}
