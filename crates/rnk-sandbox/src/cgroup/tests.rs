use super::*;
use crate::error::Step;

#[test]
fn the_own_cgroup_is_the_v2_line_below_the_namespace_root() {
    let cases: [(&[u8], Option<&[u8]>); 8] = [
        (b"0::/system.slice/a.scope\n", Some(b"system.slice/a.scope")),
        (b"0::/\n", Some(b"")),
        (
            b"12:cpu,cpuacct:/x\n1:name=systemd:/y\n0::/user.slice/u.scope\n",
            Some(b"user.slice/u.scope"),
        ),
        // cgroup v1 only.
        (b"12:cpu,cpuacct:/x\n1:name=systemd:/y\n", None),
        // Outside the reader's cgroup namespace.
        (b"0::/../../other.scope\n", None),
        (b"0::/a/./b\n", None),
        (b"0::/a//b\n", None),
        (b"0::relative\n", None),
    ];
    for (input, want) in cases {
        assert_eq!(
            own_cgroup(input),
            want,
            "{}",
            String::from_utf8_lossy(input)
        );
    }
}

#[test]
fn membership_says_whether_ranikust_is_listed_and_alone() {
    assert_eq!(membership(b"42\n", 42), (true, true));
    assert_eq!(membership(b"7\n42\n", 42), (true, false));
    // A pid of another PID namespace reads as 0.
    assert_eq!(membership(b"0\n42\n", 42), (true, false));
    assert_eq!(membership(b"7\n", 42), (false, false));
    assert_eq!(membership(b"", 42), (false, false));
    assert_eq!(membership(b"x\n42\n", 42), (true, false));
}

#[test]
fn controllers_and_events_are_read_by_name() {
    assert!(has_cpu(b"cpuset cpu io memory pids\n"));
    assert!(!has_cpu(b"cpuset io memory pids\n"));
    assert!(!has_cpu(b""));
    assert_eq!(populated(b"populated 0\nfrozen 0\n"), Some(false));
    assert_eq!(populated(b"populated 1\nfrozen 0\n"), Some(true));
    assert_eq!(populated(b"frozen 0\n"), None);
    assert_eq!(populated(b"populated 2\n"), None);
}

#[test]
fn only_a_marked_or_namespaced_writable_tree_with_cgroup_kill_is_used() {
    let delegated = Facts {
        marked: true,
        ns_root: false,
        read_only: false,
        kill: true,
        domain: true,
        alone: true,
        cpu: true,
    };
    assert_eq!(decide(delegated), Ok(Layout::Nested));
    let namespaced = Facts {
        marked: false,
        ns_root: true,
        ..delegated
    };
    assert_eq!(decide(namespaced), Ok(Layout::Nested));
    assert_eq!(
        decide(Facts {
            alone: false,
            ..delegated
        }),
        Ok(Layout::Single)
    );
    assert_eq!(
        decide(Facts {
            cpu: false,
            ..namespaced
        }),
        Ok(Layout::Single)
    );
    // Only writable is not delegated (ADR-0110, 1).
    for f in [
        Facts {
            marked: false,
            ..delegated
        },
        Facts {
            read_only: true,
            ..delegated
        },
        Facts {
            read_only: true,
            ..namespaced
        },
        Facts {
            kill: false,
            ..delegated
        },
        Facts {
            kill: false,
            ..namespaced
        },
        Facts {
            domain: false,
            ..delegated
        },
    ] {
        assert!(decide(f).is_err(), "{f:?}");
    }
}

/// Detection on this host only reads, finds a cgroup below the mount or
/// says why not, and never panics.
#[test]
fn detection_on_this_host_finds_a_cgroup_or_says_why_not() {
    match detect() {
        Ok(found) => {
            assert!(found.path.starts_with(MOUNT), "{}", found.path);
            // Read no further unless it may be delegated.
            let f = found.facts;
            if !f.marked && !f.ns_root {
                assert_eq!(f, Facts::default());
            } else if f.read_only {
                assert!(!f.kill && !f.domain && !f.alone && !f.cpu, "{f:?}");
            }
            println!(
                "{}: {:?} -> {:?}",
                found.path,
                found.facts,
                decide(found.facts)
            );
        }
        Err(NotFound::NotDelegated(why) | NotFound::Unreadable(why)) => {
            assert!(!why.is_empty());
            println!("no cgroup found: {why}");
        }
    }
}

/// A plain directory has no `cgroup.kill`: `fill` fails after the
/// `mkdir`, past a name a killed ranikust left.
#[test]
fn a_run_cgroup_that_cannot_be_made_names_its_step_and_cgroup_and_leaves_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("run-7-0")).unwrap();
    let dir = rustix::fs::open(
        tmp.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    // Dropping a tree releases the process's claim, which this one never
    // took: it is forgotten instead, with its descriptor.
    let tree = std::mem::ManuallyDrop::new(Tree {
        dir,
        path: tmp.path().display().to_string(),
        layout: Layout::Single,
        facts: Facts::default(),
        pid: 7,
        next: AtomicU64::new(0),
        leaf: None,
        moved: false,
        cpu: false,
    });
    let e = tree.run_cgroup().unwrap_err();
    match &e {
        Error::Cgroup { op, path, source } => {
            // A plain directory has no `cgroup.max.descendants`.
            assert_eq!(*op, "write a RUN's cgroup.max.descendants");
            assert_eq!(path.as_os_str(), tmp.path().join("run-7-1").as_os_str());
            assert_eq!(source.raw_os_error(), Some(libc::ENOENT));
        }
        e => panic!("{e:?}"),
    }
    assert!(e.ran_no_command());
    let mut left: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|d| d.unwrap().file_name())
        .collect();
    left.sort();
    assert_eq!(left, ["run-7-0"]);
}

#[test]
fn a_failure_to_make_a_runs_cgroup_names_the_step_and_the_cgroup() {
    let e = Error::Cgroup {
        op: "enable the cpu controller in a RUN's cgroup",
        path: "/sys/fs/cgroup/a.scope/run-7-0".into(),
        source: io::Error::from_raw_os_error(libc::EBUSY),
    };
    let text = e.to_string();
    for want in ["enable the cpu controller", "a.scope/run-7-0", "busy"] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert!(e.ran_no_command());
}

#[test]
fn a_join_or_namespace_failure_names_its_step_and_comes_before_the_command() {
    for (step, want) in [
        (Step::JoinCgroup, "join the RUN's cgroup"),
        (Step::CgroupNamespace, "make the RUN's cgroup namespace"),
    ] {
        let e = Error::Setup {
            step,
            source: io::Error::from_raw_os_error(libc::EPERM),
        };
        let text = e.to_string();
        assert!(text.contains(want), "{text}");
        assert!(text.contains("not permitted"), "{text}");
        assert!(e.ran_no_command());
    }
}

/// The probe child's report as `probe_child_main` writes it.
fn report_of(record: &[u8]) -> Result<(), Failed> {
    let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
    rustix::io::write(&w, record).unwrap();
    drop(w);
    read_report(&r)
}

#[test]
fn the_probe_report_names_the_cgroup_and_the_errno() {
    assert!(report_of(b"R").is_ok());
    for (which, step) in [
        (0, "move a child into a RUN's init cgroup"),
        (1, "move a child into a RUN's command cgroup"),
        (2, "make a cgroup namespace in a RUN's command cgroup"),
        (3, "finish a step it did not name"),
    ] {
        let [a, b, c, d] = libc::EACCES.to_le_bytes();
        let f = report_of(&[b'F', which, a, b, c, d]).unwrap_err();
        assert_eq!(f.step, step);
        assert_eq!(f.source.raw_os_error(), Some(libc::EACCES));
    }
    // A child that died before it reported, or a record cut short.
    for record in [&b""[..], b"F\x01\x0d"] {
        let f = report_of(record).unwrap_err();
        assert_eq!(f.step, "get a report from its child", "{record:?}");
        assert_eq!(f.source.kind(), io::ErrorKind::UnexpectedEof, "{record:?}");
    }
}

#[test]
fn only_a_missing_file_or_mark_reads_as_absent() {
    assert_eq!(
        if_present(Ok(b"domain".to_vec())),
        Ok(Some(b"domain".to_vec()))
    );
    assert_eq!(if_present(Err(Errno::NOENT)), Ok(None));
    assert_eq!(if_present(Err(Errno::ACCESS)), Err(Errno::ACCESS));
    assert_eq!(is_mark(Ok(b"1")), Ok(true));
    for no in [
        Ok(&b"0"[..]),
        Ok(b""),
        Err(Errno::NODATA),
        Err(Errno::NOTSUP),
        Err(Errno::RANGE),
    ] {
        assert_eq!(is_mark(no), Ok(false), "{no:?}");
    }
    assert_eq!(is_mark(Err(Errno::IO)), Err(Errno::IO));
}

/// Root trusts `trusted.delegate` alone: a root `RUN` can set
/// `user.delegate` (ADR-0110, 1). A ranikust whose uid in the initial user
/// namespace is not 0 trusts `user.delegate` too on a cgroup it owns, as
/// it sees it, as root of the build's user namespace included (ADR-0135).
#[test]
fn root_trusts_only_trusted_delegate_and_an_owner_user_delegate_too() {
    let trusted: &[&str] = &["trusted.delegate"];
    let both: &[&str] = &["trusted.delegate", "user.delegate"];
    // Root in the initial user namespace, seen from it or from the
    // build's user namespace.
    assert_eq!(trusted_marks(true, 0, 0), trusted);
    assert_eq!(trusted_marks(true, 0, 1000), trusted);
    assert_eq!(trusted_marks(true, 1000, 1000), trusted);
    // Another uid, outside the build's user namespace.
    assert_eq!(trusted_marks(false, 1000, 0), trusted);
    assert_eq!(trusted_marks(false, 1000, 1001), trusted);
    assert_eq!(trusted_marks(false, 1000, 1000), both);
    // Another uid, as root of the build's user namespace: its own uid
    // shows as 0, an unmapped owner as the overflow uid.
    assert_eq!(trusted_marks(false, 0, 0), both);
    assert_eq!(trusted_marks(false, 0, 65534), trusted);
    assert_eq!(trusted_marks(false, 0, 1), trusted);
}

/// Root is the uid the parent user namespace sees as 0, or a map that
/// cannot tell.
#[test]
fn root_is_uid_0_in_the_parent_user_namespace() {
    let identity = "         0          0 4294967295\n";
    let podman = "         0       1000          1\n         1     100000      65536\n";
    assert!(maps_to_root(0, identity));
    assert!(!maps_to_root(1000, identity));
    // Root of a rootless container is its user, as is a uid in it.
    assert!(!maps_to_root(0, podman));
    assert!(!maps_to_root(1000, podman));
    // A uid that is root in the parent.
    assert!(maps_to_root(1000, "      1000          0          1\n"));
    // Unmapped, unreadable or unparsable.
    assert!(maps_to_root(1000, "         0       1000          1\n"));
    assert!(maps_to_root(1000, ""));
    assert!(maps_to_root(1000, "0 0\n"));
    assert!(maps_to_root(1000, "0 0 x\n"));
    assert!(maps_to_root(1000, "0 0 1 2\n"));
}

/// Root is the uid its namespace shows a root-owned sysctl's owner as,
/// through any number of levels; the map decides only where that file is
/// not the sysctl.
#[test]
fn root_is_the_uid_a_root_owned_sysctl_shows_as_its_owner() {
    let proc = rustix::fs::PROC_SUPER_MAGIC;
    let file = 0o100_444;
    let ostype = |uid| sysctl_owner(proc, file, b"Linux\n", uid);
    let never = || -> String { panic!("the map is read only without the sysctl") };
    // A host root, and root of a namespace that maps it, at any depth.
    assert!(is_kernel_root(0, ostype(0), never));
    // Uid 5 of a namespace that maps it to the initial root.
    assert!(is_kernel_root(5, ostype(5), never));
    // A user on a host; root of a namespace whose root is not the
    // initial one (root of rootless Podman, a map one level down from it
    // that the parent's map would call root).
    assert!(!is_kernel_root(1000, ostype(0), never));
    assert!(!is_kernel_root(0, ostype(65534), never));
    // Overflow uid on both sides: cannot tell, so root.
    assert!(is_kernel_root(65534, ostype(65534), never));
    // Not on a procfs, not a regular file, or another procfs file bound
    // over it (a process named `Linux` has a `comm` of mode 0644): the map
    // decides.
    let tmpfs = 0x0102_1994;
    assert_eq!(sysctl_owner(tmpfs, file, b"Linux\n", 0), None);
    assert_eq!(sysctl_owner(proc, 0o040_555, b"Linux\n", 0), None);
    assert_eq!(sysctl_owner(proc, 0o100_644, b"Linux\n", 1000), None);
    assert_eq!(sysctl_owner(proc, file, b"1000 (sh) S 1\n", 1000), None);
    assert_eq!(sysctl_owner(proc, file, b"Linux", 0), None);
    let podman = "         0       1000          1\n";
    assert!(!is_kernel_root(0, None, || podman.to_owned()));
    assert!(is_kernel_root(0, None, String::new));
}

/// On this machine, which only reads: where the uid map is the initial
/// namespace's, the sysctl shows root as 0 and the two readings agree.
#[test]
fn the_sysctl_and_an_initial_map_agree_on_this_host() {
    let map = std::fs::read_to_string("/proc/self/uid_map").unwrap();
    let initial = map.split_whitespace().eq(["0", "0", "4294967295"]);
    let shown = root_shown_as();
    eprintln!("uid map {map:?}, root shown as {shown:?}");
    if !initial {
        eprintln!("skipped: not in the initial user namespace");
        return;
    }
    let Ok(Some(root)) = shown else {
        eprintln!("skipped: no procfs at /proc/sys");
        return;
    };
    assert_eq!(root, 0);
    let euid = rustix::process::geteuid().as_raw();
    assert_eq!(kernel_root_as_it_stands(), maps_to_root(euid, &map));
}

/// A root ranikust reads no `user.delegate`, which anyone who may write the
/// directory can set; one that is not root reads it on a directory it owns.
#[test]
fn a_root_ranikust_never_reads_user_delegate() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = open_dir(tmp.path());
    match rustix::fs::fsetxattr(&dir, "user.delegate", b"1", rustix::fs::XattrFlags::empty()) {
        Err(Errno::NOTSUP) => {
            eprintln!("skipped: no user xattrs on {}", tmp.path().display());
            return;
        }
        r => r.unwrap(),
    }
    let me = rustix::process::geteuid().as_raw();
    assert_eq!(delegation_mark(dir.as_fd(), true, me), Ok(false));
    assert_eq!(delegation_mark(dir.as_fd(), false, me), Ok(true));
    assert_eq!(
        delegation_mark(dir.as_fd(), false, me.wrapping_add(1)),
        Ok(false)
    );
}

/// The walk below a `RUN`'s cgroup opens no link and enters no other
/// mount; it only opens here, never removes.
#[test]
fn the_walk_below_a_runs_cgroup_enters_no_link_or_mount() {
    let slash = open_dir(std::path::Path::new("/"));
    match open_below(slash.as_fd(), "proc") {
        Err(Errno::NOSYS) => {
            eprintln!("skipped: no openat2 here");
            return;
        }
        r => assert_eq!(r.err(), Some(Errno::XDEV), "/proc is a mount of its own"),
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("a")).unwrap();
    std::os::unix::fs::symlink("a", tmp.path().join("link")).unwrap();
    let dir = open_dir(tmp.path());
    assert!(open_below(dir.as_fd(), "a").is_ok());
    // `O_DIRECTORY` with `O_NOFOLLOW` on a link is `ENOTDIR`.
    let link = open_below(dir.as_fd(), "link").err();
    assert!(
        matches!(link, Some(Errno::NOTDIR | Errno::LOOP)),
        "{link:?}"
    );
}

/// A plain directory tree stands for a `RUN`'s cgroups with those it made
/// below them.
fn tree_of(top: &std::path::Path, dirs: &[&str]) {
    for d in dirs {
        std::fs::create_dir_all(top.join(d)).unwrap();
    }
}

fn open_dir(path: &std::path::Path) -> OwnedFd {
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap()
}

#[test]
fn a_runs_cgroup_goes_with_every_cgroup_below_it() {
    let tmp = tempfile::tempdir().unwrap();
    tree_of(
        tmp.path(),
        &["run-7-0/init", "run-7-0/command/a/b/c", "run-7-0/command/d"],
    );
    std::fs::create_dir(tmp.path().join("other")).unwrap();
    let dir = open_dir(tmp.path());
    let mut left = REMOVE_LIMIT;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut at = PathBuf::new();
    remove_tree(dir.as_fd(), "run-7-0", 0, &mut left, deadline, &mut at).unwrap();
    assert_eq!(REMOVE_LIMIT - left, 7);
    let names: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|d| d.unwrap().file_name())
        .collect();
    assert_eq!(names, ["other"]);
}

/// Past the bound, or through what is not a directory, nothing more is
/// removed and the step says why.
#[test]
fn a_removal_stops_at_its_bounds_and_never_follows_a_link() {
    let tmp = tempfile::tempdir().unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    tree_of(tmp.path(), &["wide/a", "wide/b", "wide/c"]);
    let dir = open_dir(tmp.path());
    let mut left = 3;
    let mut at = PathBuf::new();
    let e = remove_tree(dir.as_fd(), "wide", 0, &mut left, deadline, &mut at).unwrap_err();
    assert_eq!(e.step, LIST_BELOW, "{e:?}");
    assert_eq!(at, PathBuf::new(), "the RUN's own cgroup");
    assert!(tmp.path().join("wide/a").is_dir());

    let deep: Vec<String> = (1..=REMOVE_DEPTH + 1).map(|i| format!("d{i}")).collect();
    tree_of(tmp.path(), &[format!("deep/{}", deep.join("/")).as_str()]);
    let mut left = REMOVE_LIMIT;
    let mut at = PathBuf::new();
    let e = remove_tree(dir.as_fd(), "deep", 0, &mut left, deadline, &mut at).unwrap_err();
    assert!(e.source.to_string().contains("deep"), "{e:?}");
    assert_eq!(at, PathBuf::from(deep[..REMOVE_DEPTH].join("/")));
    assert!(tmp.path().join("deep/d1").is_dir());

    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(outside.path().join("kept")).unwrap();
    tree_of(tmp.path(), &["linked/a"]);
    std::os::unix::fs::symlink(outside.path(), tmp.path().join("linked/a/out")).unwrap();
    let mut left = REMOVE_LIMIT;
    let mut at = PathBuf::new();
    let e = remove_tree(dir.as_fd(), "linked", 0, &mut left, deadline, &mut at).unwrap_err();
    assert_eq!(e.source.raw_os_error(), Some(libc::ENOTEMPTY), "{e:?}");
    assert_eq!(at, PathBuf::from("a"), "the cgroup that failed");
    assert!(outside.path().join("kept").is_dir());
}

/// The kernel's limits on a `RUN`'s cgroup are what its removal goes
/// through: its own cgroup is the one more the removal counts.
#[test]
fn a_runs_cgroup_limits_are_what_its_removal_goes_through() {
    let tmp = tempfile::tempdir().unwrap();
    for (file, _, _) in LIMITS {
        std::fs::write(tmp.path().join(file), "").unwrap();
    }
    limit(open_dir(tmp.path()).as_fd()).unwrap();
    let read = |file: &str| std::fs::read_to_string(tmp.path().join(file)).unwrap();
    assert_eq!(read("cgroup.max.descendants"), "4095");
    assert_eq!(read("cgroup.max.depth"), "32");
    assert_eq!(REMOVE_LIMIT, 4096);
    assert_eq!(REMOVE_DEPTH, 32);

    // A failed write names its file.
    std::fs::remove_file(tmp.path().join("cgroup.max.depth")).unwrap();
    let e = limit(open_dir(tmp.path()).as_fd()).unwrap_err();
    assert_eq!(e.step, "write a RUN's cgroup.max.depth", "{e:?}");
    assert_eq!(e.source.raw_os_error(), Some(libc::ENOENT), "{e:?}");
}

/// A failed removal names the cgroup that failed: one below the `RUN`'s,
/// or the `RUN`'s own.
#[test]
fn a_failed_removal_names_the_cgroup_that_failed() {
    let tmp = tempfile::tempdir().unwrap();
    // Dropping a tree releases the process's claim, which this one never took.
    let tree = std::mem::ManuallyDrop::new(Tree {
        dir: open_dir(tmp.path()),
        path: tmp.path().display().to_string(),
        layout: Layout::Single,
        facts: Facts::default(),
        pid: 7,
        next: AtomicU64::new(0),
        leaf: None,
        moved: false,
        cpu: false,
    });
    // A plain directory with the two files the removal writes and reads,
    // whose regular files keep it from going.
    let run = |name: &str| {
        let at = tmp.path().join(name);
        std::fs::create_dir(&at).unwrap();
        std::fs::write(at.join("cgroup.kill"), "").unwrap();
        std::fs::write(at.join("cgroup.events"), "populated 0\n").unwrap();
        let file = |f: &str| {
            rustix::fs::open(at.join(f), OFlags::RDWR | OFlags::CLOEXEC, Mode::empty()).unwrap()
        };
        RunCgroup {
            tree: &tree,
            name: name.to_owned(),
            dir: open_dir(&at),
            kill: Killer(Arc::new(file("cgroup.kill"))),
            events: file("cgroup.events"),
            init: None,
            command: file("cgroup.events"),
        }
    };
    let below = run("run-7-1");
    tree_of(tmp.path(), &["run-7-1/x/y"]);
    std::fs::write(tmp.path().join("run-7-1/x/y/file"), "").unwrap();
    let own = run("run-7-2");
    for (r, want) in [(below, "run-7-1/x/y"), (own, "run-7-2")] {
        match r.remove().unwrap_err() {
            Error::Io { op, path, source } => {
                assert_eq!(op, "remove a RUN's cgroup");
                // Compared as strings: a trailing `/` would be equal as paths.
                assert_eq!(path.as_os_str(), tmp.path().join(want).as_os_str());
                assert_eq!(source.raw_os_error(), Some(libc::ENOTEMPTY));
            }
            e => panic!("{e:?}"),
        }
    }
}

/// As many cgroups, and as deep, as the kernel lets a `RUN` make go with
/// its own; one more, past what the kernel allows, does not.
#[test]
fn a_tree_at_the_kernels_limits_is_removed_in_full() {
    let tmp = tempfile::tempdir().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let fill = |top: &str, extra: usize| {
        let chain: Vec<&str> = vec!["x"; REMOVE_DEPTH];
        tree_of(tmp.path(), &[format!("{top}/{}", chain.join("/")).as_str()]);
        for i in 0..REMOVE_LIMIT - 1 - REMOVE_DEPTH + extra {
            std::fs::create_dir(tmp.path().join(format!("{top}/w{i}"))).unwrap();
        }
    };
    let dir = open_dir(tmp.path());
    fill("full", 0);
    let mut left = REMOVE_LIMIT;
    let mut at = PathBuf::new();
    remove_tree(dir.as_fd(), "full", 0, &mut left, deadline, &mut at).unwrap();
    assert_eq!(left, 0);
    assert!(!tmp.path().join("full").exists());

    fill("over", 1);
    let mut left = REMOVE_LIMIT;
    let e = remove_tree(dir.as_fd(), "over", 0, &mut left, deadline, &mut at).unwrap_err();
    assert_eq!(e.step, LIST_BELOW, "{e:?}");
    assert!(tmp.path().join("over").is_dir());
}

#[test]
fn the_log_says_why_a_run_is_not_nested() {
    let not_alone = Facts {
        alone: false,
        cpu: true,
        ..Facts::default()
    };
    let no_cpu = Facts {
        alone: true,
        cpu: false,
        ..Facts::default()
    };
    assert!(Layout::Single.describe(not_alone).contains("not alone"));
    assert!(
        Layout::Single
            .describe(no_cpu)
            .contains("cpu is not delegated")
    );
    assert!(Layout::Nested.describe(no_cpu).starts_with("nested"));
}

#[test]
fn only_the_first_mkdirs_eexist_means_a_name_is_taken() {
    let failed = |step, errno| Failed {
        step,
        source: io::Error::from_raw_os_error(errno),
    };
    assert!(name_taken(&failed(MAKE_RUN, libc::EEXIST)));
    assert!(!name_taken(&failed(
        "create a RUN's command cgroup",
        libc::EEXIST
    )));
    assert!(!name_taken(&failed(MAKE_RUN, libc::EACCES)));
}
