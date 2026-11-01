//! Red-team attacks on a `Direct` `RUN` on `/` (kaniko mode, ADR-0051,
//! ADR-0057): the `#!` heredoc memfd, the descriptors a `RUN` holds, and
//! the stdout/stderr relay against a `RUN` that steals, passes on or
//! outlives its own streams (T7, T11, T21).
//!
//! Every test is T2: a `Direct` `RUN` on `/` switches ids and groups and
//! runs on the machine's own root, so it runs as root in the test VM only
//! (`just test-vm unit`). Each writes only under a fresh directory of
//! `/tmp`, and every process it starts ends by itself within a bounded
//! time, whatever ranikust does.

use std::ffi::OsString;
use std::fs;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use rnk_sandbox::{Error, Isolation, RunSpec, Sandbox};

/// A directory of `/tmp` every user may write (the test's `TMPDIR` may be
/// closed to uid 65534).
fn scratch() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
    dir
}

fn shell(script: &str) -> RunSpec {
    let mut spec = RunSpec::new(
        "/",
        vec!["/bin/sh".into(), "-c".into(), OsString::from(script)],
    );
    spec.isolation = Isolation::Direct;
    spec.workdir = "/".into();
    spec
}

fn heredoc(body: &str) -> RunSpec {
    let mut spec = RunSpec::new("/", vec!["/dev/pipes/EOF".into()]);
    spec.isolation = Isolation::Direct;
    spec.workdir = "/".into();
    spec.script = Some(body.as_bytes().into());
    spec
}

fn epoch() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// Pids of live or zombie processes whose `comm` is `name`.
fn processes_named(name: &str) -> Vec<(i32, String)> {
    let mut out = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if stat.contains(&format!("({name})")) {
            out.push((pid, stat));
        }
    }
    out
}

/// Reaps the zombies named `name` that are this process's children; the
/// sandbox reaps only the pids it listed.
fn reap_named(name: &str) {
    let me = std::process::id().to_string();
    for (pid, stat) in processes_named(name) {
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .map(|(_, r)| r.split_whitespace().collect())
            .unwrap_or_default();
        if fields.first() == Some(&"Z") && fields.get(1) == Some(&me.as_str()) {
            // SAFETY: a plain waitpid on a pid that is a zombie child.
            unsafe {
                libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
            }
        }
    }
}

/// The `#!` script a `Direct` `RUN` runs from its sealed memfd cannot be
/// changed by the script itself: appending, truncating, reopening
/// `/dev/fd/<n>` or `/proc/self/fd/<n>` read-write. What it reads back
/// is what was given, and the next heredoc runs its own body. Nothing of
/// it is named in the root (`/dev/pipes`). An interpreter that is the
/// script itself (`#!/dev/fd/3`) fails, it does not loop. T2c (root in the VM, or a default rootful Docker run).
#[test]
#[ignore = "T2c: default: a Direct RUN switches groups on the container's /"]
fn red_team_direct_heredoc_cannot_rewrite_its_script() {
    let sandbox = Sandbox::start_direct().unwrap();
    let dir = scratch();
    for user in ["", "65534:65534"] {
        let out = dir.path().join(format!("rewrite-{}", user.len()));
        let body = format!(
            "#!/bin/sh\n\
             o={out}\n\
             n=${{0##*/}}\n\
             try() {{ if ( eval \"$1\" ) 2>/dev/null; then echo \"wrote: $1\" >> $o; fi; }}\n\
             try 'printf x >> \"$0\"'\n\
             try ': > \"$0\"'\n\
             try 'exec 5<>\"$0\"; printf y >&5'\n\
             try 'exec 5<>/proc/self/fd/$n; printf y >&5'\n\
             try 'printf z > /proc/self/fd/$n'\n\
             try 'printf z 1<>/dev/fd/$n'\n\
             test ! -e /dev/pipes || echo 'named: /dev/pipes' >> $o\n\
             cat \"$0\" > $o.read\n\
             echo end >> $o\n\
             # MARK\n",
            out = out.display()
        );
        let mut spec = heredoc(&body);
        spec.user = user.into();
        let status = sandbox.run(spec).unwrap();
        assert!(status.success(), "{user:?}: {status:?}");
        let got = fs::read_to_string(&out).unwrap();
        assert_eq!(got, "end\n", "{user:?}: the script changed: {got}");
        let read = fs::read_to_string(format!("{}.read", out.display())).unwrap();
        assert_eq!(read, body, "{user:?}: the script read back differs");
    }
    // The next script runs its own body.
    let out = dir.path().join("next");
    let status = sandbox
        .run(heredoc(&format!(
            "#!/bin/sh\necho next > {}\n",
            out.display()
        )))
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read_to_string(&out).unwrap(), "next\n");
    // A script that executes itself again, through `$0`.
    let out = dir.path().join("again");
    let status = sandbox
        .run(heredoc(&format!(
            "#!/bin/sh\necho \"$0\" >> {o}\n[ -n \"$AGAIN\" ] || AGAIN=1 exec \"$0\"\n",
            o = out.display()
        )))
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read_to_string(&out).unwrap().lines().count(), 2);
    // Interpreters that are the script itself, or that do not exist.
    for interp in [
        "/dev/fd/3",
        "/dev/fd/4",
        "/proc/self/fd/3",
        "/nonexistent/sh",
    ] {
        let start = Instant::now();
        let r = sandbox.run(heredoc(&format!("#!{interp}\necho no\n")));
        assert!(
            !matches!(&r, Ok(s) if s.success()),
            "#!{interp}: ran: {r:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(10), "#!{interp}");
    }
}

/// A `Direct` `RUN` holds no descriptor of ranikust's besides its own
/// pipes, `/dev/null` and its script's memfd: not one ranikust opened
/// without close-on-exec, not ranikust's own stdout. As root and as
/// another user, in shell form and as a `#!` heredoc. As another user it
/// owns its streams: it reopens `/dev/stdout`, `/proc/self/fd/1` and
/// `/dev/stderr` (kaniko#2928). T2c.
#[test]
#[ignore = "T2c: default: a Direct RUN switches groups on the container's /"]
fn red_team_direct_run_holds_only_its_own_descriptors() {
    let sandbox = Sandbox::start_direct().unwrap();
    let dir = scratch();
    // A descriptor of "ranikust" without close-on-exec.
    let canary = dir.path().join("rnk-canary-state");
    fs::write(&canary, b"state").unwrap();
    let c = std::ffi::CString::new(canary.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: open(2) with a NUL-terminated path; the result is owned below.
    let raw = unsafe { libc::open(c.as_ptr(), libc::O_RDWR) };
    assert!(raw >= 0);
    // SAFETY: a new descriptor this test owns.
    let leaked = unsafe { OwnedFd::from_raw_fd(raw) };
    let own_stdout = fs::read_link("/proc/self/fd/1").unwrap_or_default();
    for user in ["", "65534:65534"] {
        for form in ["shell", "heredoc"] {
            let out = dir.path().join(format!("fds-{}-{form}", user.len()));
            let list = format!(
                "for f in /proc/$$/fd/*; do printf '%s %s\\n' \"${{f##*/}}\" \"$(readlink \"$f\")\"; \
                 done > {o}.tmp; mv {o}.tmp {o}; \
                 echo a > /dev/stdout && echo b > /proc/self/fd/1 && echo c > /dev/stderr && \
                 echo d > /proc/self/fd/2 && stat -L -c %u /proc/$$/fd/1 > {o}.owner",
                o = out.display()
            );
            let mut spec = if form == "shell" {
                shell(&list)
            } else {
                heredoc(&format!("#!/bin/sh\n{list}\n"))
            };
            spec.user = user.into();
            let status = sandbox.run(spec).unwrap();
            assert!(status.success(), "{user:?} {form}: {status:?}");
            let got = fs::read_to_string(&out).unwrap();
            for line in got.lines() {
                let (n, target) = line.split_once(' ').unwrap_or((line, ""));
                let ok = target.starts_with("pipe:[")
                    || target == "/dev/null"
                    || target.starts_with("/memfd:ranikust-heredoc")
                    || target == format!("{}.tmp", out.display())
                    || target.starts_with("/proc/");
                assert!(ok, "{user:?} {form}: fd {n} -> {target}\n{got}");
                assert!(
                    !target.contains("rnk-canary-state"),
                    "{user:?} {form}: ranikust's descriptor reached the RUN: {line}"
                );
                if !own_stdout.as_os_str().is_empty()
                    && own_stdout.to_string_lossy().starts_with("pipe:[")
                {
                    assert_ne!(
                        Path::new(target),
                        own_stdout,
                        "{user:?} {form}: ranikust's own stdout reached the RUN"
                    );
                }
            }
            let owner = fs::read_to_string(format!("{}.owner", out.display())).unwrap();
            let want = if user.is_empty() { "0" } else { "65534" };
            assert_eq!(owner.trim(), want, "{user:?} {form}");
        }
    }
    drop(leaked);
}

/// Receives one descriptor sent with `SCM_RIGHTS` on `sock`.
fn receive_fd(sock: &std::os::unix::net::UnixStream) -> Option<OwnedFd> {
    let mut data = [0u8; 16];
    let mut control = [0u64; 8];
    // SAFETY: the pointers refer to `data` and `control`, live across the
    // call; the CMSG helpers walk only `control`.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        if libc::recvmsg(sock.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC) < 0 {
            return None;
        }
        let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        if cmsg.is_null() || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return None;
        }
        let fd = libc::CMSG_DATA(cmsg).cast::<i32>().read_unaligned();
        Some(OwnedFd::from_raw_fd(fd))
    }
}

/// Holds, from outside the `RUN`'s processes, the descriptor the `RUN`
/// sends to `path`, until `release` or `hold` has passed. Stands for
/// another process of the container (a sidecar, a daemon) the `RUN` hands
/// its stdout to.
fn holder(
    path: PathBuf,
    release: mpsc::Receiver<()>,
    hold: Duration,
) -> std::thread::JoinHandle<bool> {
    let listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let start = Instant::now();
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break Some(s),
                Err(_) if start.elapsed() < Duration::from_secs(30) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break None,
            }
        };
        let Some(stream) = stream else { return false };
        stream.set_nonblocking(false).unwrap();
        let fd = receive_fd(&stream);
        let got = fd.is_some();
        let _ = release.recv_timeout(hold);
        drop(fd);
        got
    })
}

/// This process's stdout sent to `/dev/null` while it lives: the relay
/// copies a `RUN`'s output to fd 1, and these tests write megabytes. The
/// T2 harness runs one test at a time.
struct QuietStdout {
    saved: i32,
}

impl QuietStdout {
    fn new() -> Self {
        let null = fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        // SAFETY: plain dup/dup2 on descriptors this process owns; fd 1 is
        // restored by `drop`.
        let saved = unsafe { libc::dup(1) };
        assert!(saved >= 0);
        // SAFETY: as above.
        assert_eq!(unsafe { libc::dup2(null.as_raw_fd(), 1) }, 1);
        Self { saved }
    }
}

impl Drop for QuietStdout {
    fn drop(&mut self) {
        // SAFETY: as in `new`; `saved` is closed once, here.
        unsafe {
            libc::dup2(self.saved, 1);
            libc::close(self.saved);
        }
    }
}

/// Python run by a `RUN` (killed by `SIGALRM` after 60 s whatever
/// happens): optionally sends its stderr to the Unix socket `argv[1]`
/// (another process then holds it), reopens its own stderr pipe for
/// reading through `/proc/self/fd/2` and reads it in a child; then, round
/// after round, writes a 64 KiB burst to stdout and one byte to stderr.
/// The relay, woken with both pipes ready, copies stdout first; the child
/// takes the byte meanwhile, and the relay's read of stderr waits. After
/// each round the script checks that 1 MiB more of stdout is taken within
/// 2 s (its stdout is non-blocking: it never waits on it); when it is not,
/// the relay has stopped copying stdout, and the script stops: exit 3
/// with `stall_fails`, else 0.
fn steal_script(send: bool, stall_fails: bool) -> String {
    let send = if send {
        "s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); \
         socket.send_fds(s, [b'x'], [2]); s.close()"
    } else {
        "pass"
    };
    let stalled = if stall_fails { 3 } else { 0 };
    format!(
        "import fcntl,os,signal,socket,sys,time\n\
         signal.alarm(60)\n\
         {send}\n\
         r=os.open('/proc/self/fd/2', os.O_RDONLY)\n\
         if os.fork() == 0:\n\
         \x20   while os.read(r, 65536): pass\n\
         \x20   os._exit(0)\n\
         os.close(r)\n\
         fcntl.fcntl(1, fcntl.F_SETFL, fcntl.fcntl(1, fcntl.F_GETFL) | os.O_NONBLOCK)\n\
         burst=b'y'*65536\n\
         def out(n, limit):\n\
         \x20   end=time.time()+limit\n\
         \x20   while n > 0:\n\
         \x20       try:\n\
         \x20           n-=os.write(1, burst[:min(n, 65536)])\n\
         \x20       except BlockingIOError:\n\
         \x20           if time.time() > end: return False\n\
         \x20           time.sleep(0.001)\n\
         \x20   return True\n\
         for _ in range(200):\n\
         \x20   out(65536, 0.2)\n\
         \x20   os.write(2, b'z')\n\
         \x20   time.sleep(0.002)\n\
         \x20   if not out(1 << 20, 2.0): sys.exit({stalled})\n"
    )
}

/// The `RUN` hands its stderr to a process outside its tree, which keeps
/// it and never writes, and steals its own stderr back
/// ([`steal_script`]) so that the relay waits in `read` on it. Once the
/// `RUN` exits, the build must go on after the relay's drain time, not
/// wait for that process (T21, ADR-0057: a `RUN` cannot hang ranikust;
/// the relay "never waits for the end of a pipe"). Ten `RUN`s, as the
/// race may go either way. T2c.
#[test]
#[ignore = "T2c: default: a Direct RUN switches groups on the container's /"]
fn red_team_direct_run_stealing_its_output_cannot_hang_the_build() {
    let sandbox = Sandbox::start_direct().unwrap();
    let dir = scratch();
    let hold = Duration::from_secs(15);
    let quiet = QuietStdout::new();
    let mut runs = Vec::new();
    for attempt in 0..10 {
        let sock = dir.path().join(format!("hold-{attempt}.sock"));
        let (release, released) = mpsc::channel();
        let holder = holder(sock.clone(), released, hold);
        let mut spec = shell(&format!("exec python3 -I -c \"$S\" {}", sock.display()));
        spec.env
            .push(("S".into(), steal_script(true, false).into()));
        let start = Instant::now();
        let r = sandbox.run(spec);
        let took = start.elapsed();
        let _ = release.send(());
        let got = holder.join().unwrap();
        runs.push((attempt, got, format!("{r:?}"), took));
        if took >= Duration::from_secs(10) {
            break;
        }
    }
    drop(quiet);
    for (attempt, got, r, took) in &runs {
        assert!(
            got,
            "attempt {attempt}: the RUN did not hand its stderr over: {r}"
        );
        assert!(r.starts_with("Ok("), "attempt {attempt}: {r}");
        assert!(
            *took < Duration::from_secs(10),
            "attempt {attempt}: the build waited {took:?} for a process outside the RUN \
             holding its stderr (released after {hold:?}); runs: {runs:?}"
        );
    }
}

/// A `RUN` that reads its own stderr back ([`steal_script`]) never stops
/// the relay from copying its stdout: its stdout writes keep being taken.
/// (The `RUN` exits 3 when 1 MiB of stdout waits more than 2 s.) T2c.
#[test]
#[ignore = "T2c: default: a Direct RUN switches groups on the container's /"]
fn red_team_direct_run_reading_its_stderr_back_does_not_stall_its_stdout() {
    let sandbox = Sandbox::start_direct().unwrap();
    let quiet = QuietStdout::new();
    let mut spec = shell("exec python3 -I -c \"$S\"");
    spec.env
        .push(("S".into(), steal_script(false, true).into()));
    let start = Instant::now();
    let r = sandbox.run(spec);
    let took = start.elapsed();
    drop(quiet);
    assert!(
        matches!(&r, Ok(s) if s.success()),
        "the relay stopped copying the RUN's stdout while the RUN read its stderr back \
         (exit 3), or the RUN failed: {r:?} after {took:?}"
    );
}

/// A fork loop under `setsid` whose process changes pid faster than the
/// sweep lists and kills (each process forks and exits at once), holding
/// the `RUN`'s stdout and stderr and never writing, while the `RUN` steals
/// its own stderr back ([`steal_script`]). The build must end in a bounded
/// time: `Ok` with none of the loop left, or [`Error::Survivors`]; never
/// wait for the loop (ADR-0051: "a tree still there after the rounds
/// fails the build"). The loop ends by itself after 30 s whatever happens.
/// T2c.
#[test]
#[ignore = "T2c: default: a Direct RUN switches groups on the container's /"]
fn red_team_direct_pid_hopping_survivor_fails_the_build_in_bounded_time() {
    const NAME: &str = "rnkhopper";
    let sandbox = Sandbox::start_direct().unwrap();
    let dir = scratch();
    let hopper = dir.path().join(NAME);
    let python = ["/usr/bin/python3", "/bin/python3"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("python3 in the test VM");
    std::os::unix::fs::symlink(python, &hopper).unwrap();
    let end = epoch() + 30.0;
    // At most 60000 hops: a bound on the zombies left if nobody reaps.
    let hop = format!(
        "import os,time\n\
         os.setsid()\n\
         n=0\n\
         while time.time() < {end} and n < 60000:\n\
         \x20   n+=1\n\
         \x20   try:\n\
         \x20       if os.fork(): os._exit(0)\n\
         \x20   except OSError:\n\
         \x20       time.sleep(0.01)\n\
         os._exit(0)\n"
    );
    let mut spec = shell("\"$H\" -I -c \"$L\" </dev/null & sleep 0.5; exec python3 -I -c \"$S\"");
    spec.env.push(("H".into(), hopper.clone().into()));
    spec.env.push(("L".into(), hop.into()));
    spec.env
        .push(("S".into(), steal_script(false, false).into()));
    // Reaps the loop's zombies while the sandbox does not.
    let (stop, stopped) = mpsc::channel::<()>();
    let reaper = std::thread::spawn(move || {
        while stopped.recv_timeout(Duration::from_millis(20)).is_err() {
            reap_named(NAME);
        }
    });
    let quiet = QuietStdout::new();
    let start = Instant::now();
    let r = sandbox.run(spec);
    let took = start.elapsed();
    drop(quiet);
    // Until the loop has ended by itself, then nothing of it is left.
    while epoch() < end + 2.0 {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = stop.send(());
    reaper.join().unwrap();
    reap_named(NAME);
    let left: Vec<_> = processes_named(NAME)
        .into_iter()
        .filter(|(_, s)| !s.contains(") Z "))
        .collect();
    assert!(
        left.is_empty(),
        "the loop outlived its own deadline: {left:?}"
    );
    assert!(
        matches!(&r, Ok(s) if s.success()) || matches!(&r, Err(Error::Survivors { .. })),
        "{r:?}"
    );
    assert!(
        took < Duration::from_secs(25),
        "the build waited {took:?} for a fork loop holding the RUN's streams ({r:?})"
    );
}

/// A `RUN` sets `O_NONBLOCK` on its stdout and stderr, floods them, then
/// closes them early and keeps running: the build succeeds, ranikust's own
/// stdout keeps its flags, and every byte written before the close reaches
/// it. T2c.
#[test]
#[ignore = "T2c: default: a Direct RUN switches groups on the container's /"]
fn red_team_direct_run_nonblocking_flood_and_early_close() {
    const BYTES: usize = 4 << 20;
    let sandbox = Sandbox::start_direct().unwrap();
    // ranikust's stdout, for this test: a pipe a thread drains and counts.
    let (r, w) = rustix::pipe::pipe().unwrap();
    // SAFETY: plain dup/dup2 on descriptors this process owns; fd 1 is
    // restored before the test returns (the T2 harness runs one test at a
    // time).
    let saved = unsafe { libc::dup(1) };
    assert!(saved >= 0);
    // SAFETY: as above.
    let before = unsafe { libc::fcntl(saved, libc::F_GETFL) };
    // SAFETY: as above.
    assert!(unsafe { libc::dup2(w.as_raw_fd(), 1) } == 1);
    drop(w);
    let counter = std::thread::spawn(move || {
        let mut n = 0usize;
        let mut buf = vec![0u8; 1 << 16];
        while let Ok(k) = rustix::io::read(&r, &mut buf) {
            if k == 0 {
                break;
            }
            n += k;
        }
        n
    });
    let script = format!(
        "import fcntl,os,select,time\n\
         for fd in (1, 2):\n\
         \x20   fcntl.fcntl(fd, fcntl.F_SETFL, fcntl.fcntl(fd, fcntl.F_GETFL) | os.O_NONBLOCK)\n\
         data = b'y' * {BYTES}\n\
         while data:\n\
         \x20   try:\n\
         \x20       data = data[os.write(1, data):]\n\
         \x20   except BlockingIOError:\n\
         \x20       select.select([], [1], [])\n\
         os.close(1)\n\
         os.close(2)\n\
         time.sleep(1)\n"
    );
    let mut spec = shell("exec python3 -I -c \"$S\"");
    spec.env.push(("S".into(), script.into()));
    let start = Instant::now();
    let result = sandbox.run(spec);
    let took = start.elapsed();
    // Restore fd 1: the counter then sees the end of its pipe.
    // SAFETY: as above; `saved` is closed once, here.
    let restored = unsafe { libc::dup2(saved, 1) };
    // SAFETY: as above.
    let after = unsafe { libc::fcntl(saved, libc::F_GETFL) };
    // SAFETY: as above.
    unsafe { libc::close(saved) };
    let n = counter.join().unwrap();
    assert_eq!(restored, 1);
    assert!(matches!(&result, Ok(s) if s.success()), "{result:?}");
    assert_eq!(
        before, after,
        "the RUN changed the flags of ranikust's stdout"
    );
    assert_eq!(n, BYTES, "bytes relayed to ranikust's stdout");
    assert!(took < Duration::from_secs(30), "{took:?}");
}
