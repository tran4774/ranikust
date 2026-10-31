use super::*;

/// A default `docker run` container (Docker 27, cgroup v2), trimmed, with a
/// volume whose path holds a space and a tab.
const CONTAINER: &str = "\
1287 1043 0:115 / / rw,relatime master:509 - overlay overlay rw,lowerdir=/var/lib/docker/l1,upperdir=/u,workdir=/w
1288 1287 0:118 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
1289 1287 0:119 / /dev rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755,inode64
1290 1289 0:120 / /dev/pts rw,nosuid,noexec,relatime - devpts devpts rw,gid=5,mode=620,ptmxmode=666
1291 1287 0:121 / /sys ro,nosuid,nodev,noexec,relatime - sysfs sysfs ro
1292 1291 0:30 / /sys/fs/cgroup ro,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw,nsdelegate
1293 1287 259:2 /var/lib/docker/containers/abc/resolv.conf /etc/resolv.conf rw,relatime - ext4 /dev/nvme0n1p2 rw
1294 1287 259:2 /var/lib/docker/containers/abc/hostname /etc/hostname rw,relatime - ext4 /dev/nvme0n1p2 rw
1295 1287 259:2 /var/lib/docker/containers/abc/hosts /etc/hosts rw,relatime - ext4 /dev/nvme0n1p2 rw
1296 1287 259:2 /home/u/my\\040vol /data/my\\040vol\\011x rw,relatime shared:1 master:2 - ext4 /dev/nvme0n1p2 rw
1297 1287 0:122 / /var/run/docker.sock rw - tmpfs tmpfs rw
";

#[test]
fn mountinfo_is_parsed_with_escapes_and_optional_fields() {
    let mounts = parse_mountinfo(CONTAINER.as_bytes()).unwrap();
    assert_eq!(mounts.len(), 11);
    assert_eq!(mounts[0].mount_point, Path::new("/"));
    assert_eq!(mounts[0].fs_type, "overlay");
    assert_eq!((mounts[1].mount_id, mounts[1].parent_id), (1288, 1287));
    assert_eq!(mounts[9].mount_point, Path::new("/data/my vol\tx"));
    assert_eq!(mounts[9].fs_type, "ext4");
    assert_eq!(mounts[9].fs_root, Path::new("/home/u/my vol"));
    assert_eq!(mounts[9].device, "259:2");
    assert_eq!(mounts[0].fs_root, Path::new("/"));
}

#[test]
fn malformed_mountinfo_is_an_error_not_a_guess() {
    for bad in [
        "1 2 0:1 / /x rw",                    // no separator
        "a 2 0:1 / /x rw - ext4 src rw",      // id not a number
        "1 2 0:1 / /x\\04 rw - ext4 src rw",  // short escape
        "1 2 0:1 / /x\\089 rw - ext4 src rw", // not octal
        "1 2 0:1 / x rw - ext4 src rw",       // relative mount point
        "1 2",
    ] {
        let r = parse_mountinfo(bad.as_bytes());
        assert!(matches!(r, Err(Error::Ignore(_))), "{bad:?}: {r:?}");
    }
    assert!(parse_mountinfo(b"").unwrap().is_empty());
}

fn spec<'a>(root: &'a str, state: Option<&'a str>, paths: &'a [PathBuf]) -> IgnoreSpec<'a> {
    IgnoreSpec {
        root: Path::new(root),
        state_dir: state.map(Path::new),
        ignore_paths: paths,
        ignore_var_run: true,
    }
}

#[test]
fn in_a_container_mounts_and_kanikos_defaults_are_ignored() {
    let mounts = parse_mountinfo(CONTAINER.as_bytes()).unwrap();
    let paths = [PathBuf::from("/var/cache/*")];
    let list = ignore_list(&spec("/", Some("/ranikust/state"), &paths), &mounts).unwrap();
    for p in [
        "/proc/self",
        "/dev/pts/0",
        "/sys/fs/cgroup/cpu.max",
        "/etc/resolv.conf",
        "/etc/hostname",
        "/etc/hosts",
        "/etc/mtab",
        "/data/my vol\tx/f",
        "/var/run",
        "/var/run/docker.sock",
        "/ranikust/executor",
        "/ranikust/state/blobs",
        "/tmp/apt-key-gpghome/k",
        "/var/cache/apt",
    ] {
        assert!(list.covers(p), "{p}");
    }
    for p in [
        "/",
        "/etc",
        "/etc/passwd",
        "/data",
        "/run",
        "/tmp/apt-key-gpghome",
        "/var/cache",
        "/usr",
        "/.ranikust-root",
    ] {
        assert!(!list.covers(p), "{p}");
    }
}

#[test]
fn without_ignore_var_run_var_run_is_kept_but_its_mounts_are_not() {
    let mounts = parse_mountinfo(CONTAINER.as_bytes()).unwrap();
    let mut s = spec("/", None, &[]);
    s.ignore_var_run = false;
    let list = ignore_list(&s, &mounts).unwrap();
    assert!(!list.covers("/var/run"));
    assert!(!list.covers("/var/run/lock"));
    assert!(list.covers("/var/run/docker.sock"));
}

#[test]
fn on_a_host_only_mounts_below_the_workdir_count() {
    let text = "\
22 1 259:2 / / rw - ext4 /dev/root rw
30 22 0:40 / /home/u/w/sub rw - tmpfs tmpfs rw
31 22 0:41 / /home/u/w rw - tmpfs tmpfs rw
32 22 0:42 / /home/u/wx rw - tmpfs tmpfs rw
33 22 0:43 / /proc rw - proc proc rw
";
    let mounts = parse_mountinfo(text.as_bytes()).unwrap();
    let list = ignore_list(
        &spec("/home/u/w", Some("/home/u/w/.ranikust"), &[]),
        &mounts,
    )
    .unwrap();
    assert!(list.covers("/sub"));
    assert!(list.covers("/.ranikust/blobs"));
    // The host's /proc is not in the root; the root's own /proc is ignored
    // anyway, mounted or not.
    assert!(list.covers("/proc"));
    // `/home/u/wx` is a sibling, not below the root; the root's own mount
    // is not an entry.
    assert!(!list.covers("/x"));
    assert!(!list.covers("/etc"));
    // No marker: ranikust makes the root itself (ADR-0050), so an image's
    // own `/.ranikust-root` is walked like any file.
    assert!(!list.covers("/.ranikust-root"));

    let outside = ignore_list(&spec("/home/u/w", Some("/home/u/.ranikust"), &[]), &mounts).unwrap();
    assert!(!outside.covers("/.ranikust"));
}

#[test]
fn bad_inputs_are_refused() {
    let none: [MountInfo; 0] = [];
    // Not physical: compared byte for byte, a `.` or `..` would fail open.
    for root in ["w", "", "/w/", "//w", "/w/./x", "/w/../x", "/w/."] {
        let r = ignore_list(&spec(root, None, &[]), &none);
        assert!(matches!(r, Err(Error::Ignore(_))), "{root:?}: {r:?}");
    }
    for state in [
        "/w",
        "/",
        "/w/x/../.ranikust",
        "/w//.ranikust",
        "w/.ranikust",
    ] {
        let r = ignore_list(&spec("/w", Some(state), &[]), &none);
        assert!(matches!(r, Err(Error::Ignore(_))), "{state:?}: {r:?}");
    }
    for bad in ["/../etc", "/", "/x/[a"] {
        let paths = [PathBuf::from(bad)];
        let r = ignore_list(&spec("/", None, &paths), &none);
        assert!(matches!(r, Err(Error::Rootfs(_))), "{bad}: {r:?}");
    }
}

#[test]
fn the_live_mountinfo_parses() {
    // Only that the running kernel's format is understood; the content is
    // the test host's.
    let mounts = read_mountinfo().unwrap();
    assert!(mounts.iter().any(|m| m.mount_point == Path::new("/")));
}
