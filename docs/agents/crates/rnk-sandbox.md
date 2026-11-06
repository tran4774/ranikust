# rnk-sandbox

> **Status:** in progress · **Phase:** 1-2 (`Direct` from 2; rootless: 3) ·
> **Depends on:** `rnk-rootfs` (`RootPath`, `USER` lookup) ·
> **Used by:** `rnk-builder`; `ranikust` (probes, process set-up) ·
> **Path:** `crates/rnk-sandbox/`

**TL;DR:** Runs each `RUN` in fresh namespaces inside the stage root, entered
with `pivot_root` (or `MS_MOVE`), with the capability bounding set cut to
Docker's defaults and Docker's default seccomp profile, and probes its own
capabilities. Never a bare `chroot` where mounting is possible; no `RUN`
process outlives the build. A `RUN` in a user namespace, with the
`proc`/`sysfs` fallbacks, and `--userns` are Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)); the probe of that
path is built, reported only
([#74](https://github.com/tran4774/ranikust/issues/74)).

## Owns

- The sealed copy of ranikust's binary a build runs from (`seal.rs`,
  ADR-0101): a sealed memfd, else an `O_TMPFILE` copy (never directly on
  FUSE), run with `execveat` on its close-on-exec descriptor; the copy's run,
  marked by `RANIKUST_SEALED_COPY`, checks it and never makes another.
  Measures which mount the binary as installed is on, and whether a
  writable mount reaches it (`BinaryMount`), for `ranikust`'s refusal in
  kaniko mode on `/`.
- Process set-up, called by `ranikust` while single-threaded: private mount
  namespace, spawner thread, stub-journal replay; and the probe of its own
  steps, which `ranikust` runs with `rnk-snapshot`'s. Today the probe runs
  the `Full` per-`RUN` sequence in an empty root (`probe.rs`) and reports
  only `root_entry`; its guidance (uid map, `sysfs`, bind mounts) is
  Phase 3.
- The user-namespace probe (`userns.rs`, #74), reported only: nothing
  routes on it yet. Each step a `RUN` in a user namespace needs, in forked
  children that leave no namespace or mount behind: `unshare(CLONE_NEWUSER)`;
  the id map (root: its own ranges mirrored, else one id; another user:
  `newuidmap`/`newgidmap` with the subordinate ids the caller read, else one
  id); `chown` to a mapped id other than 0; `unshare(CLONE_NEWNS)`, a second
  call after the map, so that the mount namespace is owned by the new user
  namespace and a refusal names its step (with a ranged map Ubuntu 24.04
  refuses it too, after `chown`); a new `proc` in a new PID namespace, else
  an rbind of `/proc`; read-only `sysfs`, else an rbind of `/sys`;
  `mknod` (reported, never needed). `UsernsReport::in_namespace` runs a
  closure as root of a namespace with the map that worked, for the overlay
  probe. The `newuidmap` path has no caller but the T2 test: `ranikust`
  probes as root only and passes no subordinate ids, and task 5
  ([#75](https://github.com/tran4774/ranikust/issues/75)) decides where
  the lookup lives.
- The per-`RUN` sandbox: namespaces, mounts, `pivot_root`, capability drop,
  the `USER` ids `rnk-rootfs` resolved in the root, stubs, `exec`.
- The cgroups of full-sandbox `RUN`s in a delegated cgroup v2 tree
  (`cgroup.rs`, ADR-0110): the start-up probe, the nesting, and each `RUN`'s
  cgroups, killed and removed when it ends.
- Seccomp profiles (`seccomp/profile.rs`): Docker's JSON format compiled to
  classic BPF by the crate's own assembler (`seccomp/asm.rs`), moby's
  `default.json` embedded verbatim (ADR-0109); and ADR-0033's filter.
- `Isolation::Direct`: the sequential kaniko-mode `RUN` on `/`, with no
  namespace. A `chroot` of a root other than `/` (ADR-0043, ADR-0050) is
  test code only since ADR-0060.
- Not built: the no-mount `--parallel-stages` path (bare `chroot`, `mknod`
  `/dev` nodes, fake `/proc` and `/sys`, `JAVA_TOOL_OPTIONS`); the flag is
  refused (ADR-0060).

## Must not

Boundaries: `crates/rnk-sandbox/src/lib.rs`. In short:

- Look up users on the host or parse `/etc/passwd` itself (`rnk-rootfs`);
  snapshot, diff or mount stage roots or probe overlays (`rnk-snapshot`;
  the seal only reads whether `/` is one, ADR-0101);
  pick the mode (`ranikust`).
- Use a bare `chroot` where mounting is possible; `unshare` once threads
  exist; clone a `RUN` anywhere but on the spawner thread.
- Allocate, lock, format or panic between `clone` and `execve`.
- Write the cgroupfs anywhere but in and below ranikust's own cgroup, or
  there unless it is delegated (ADR-0110); set a resource limit on a cgroup
  (a `RUN`'s gets `cgroup.max.descendants` and `cgroup.max.depth` only).
- Bind a source below a directory a `RUN` can write (a cache `source=`
  subpath, Phase 5) by path: resolve it from that directory through
  `RootPath` first. `open_source` refuses only a final symlink.

## API

```rust
pub fn run_from_sealed_copy(ways: &[SealedCopy], walk: bool) -> rustix::io::Result<Sealing>; // first, single-threaded (ADR-0101): execveat from the first copy made; returns in the copy's run (Sealed, checked) or when none ran (Unsealed); Err only when RLIMIT_CORE cannot go back to 0; walk: whether the build may be kaniko mode on a container's / (the caller decides), without which no mount point is looked up
pub enum SealedCopy { Memfd, Tmpfile }  // ALL: the order tried
pub enum Sealing { Sealed { way: SealedCopy, passed_over: String, binary: BinaryMount }, Unsealed { why: String, binary: BinaryMount } } // passed_over: why the ways before failed; binary(): the installed binary's mount, measured before the copy (from the marker), or on the file run when unsealed
pub enum BinaryMount { Root, ReadOnly, Writable, Exposed, Linked, NoMountId, Unplaced, NotWeighed, Unknown(Errno) } // against the mount of /: statx STATX_MNT_ID, statvfs ST_RDONLY; on another read-only mount, and on / when it is not an overlay (copies_up: overlayfs, or FUSE typed fuse.fuse-overlayfs in mountinfo; verdict), Linked: stx_nlink > 1, Exposed: another writable mount of its major:minor whose root contains its path (rnk_rootfs::parse_mountinfo) or is its inode (a file root, statx_physical at its mount point through RootPath, its mount's root), Unplaced: not placed (a writable directory whose source was removed, a writable mount covered or not reached at its mount point, a table changed during the walk, among them); NotWeighed: the walk not asked for; Unknown: a failed call, a lookup of its name or a mount point among them, unless a writable mount was found to reach it on a table that read the same twice; NoMountId: an id of 0 (qemu-user, a memfd; fdinfo fallback: #95)
pub fn enter_build_namespace() -> Result<()>;   // unshare(CLONE_NEWNS) + rslave; refuses with other threads, and in a plain chroot (Error::InPlainChroot)
pub fn trim_to_run_capabilities() -> Result<()>; // kaniko mode on /, before start_direct (ADR-0078): cuts ranikust's own sets to what a Direct RUN keeps, dumpable 0 again, read back; no-op when not root; refuses with other threads (Error::NotSingleThreaded), or (Error::RunReachesState) where the bounding set cannot be cut
pub fn probe(dir: &Path) -> Result<Caps>;       // the real sequence in an empty root: pivot_root, else MS_MOVE
pub struct Caps { pub root_entry: RootEntry }   // #[non_exhaustive]
pub enum RootEntry { PivotRoot, MoveMount }
pub fn probe_user_namespace(dir: &Path, subordinate: Option<SubordinateIds>) -> Result<UsernsReport>; // #74, reported only; single-threaded caller; dir: an empty directory it made on the state directory's filesystem, left empty
pub struct UsernsReport { pub unshare: StepResult, pub maps: Vec<(IdMap, StepResult)>, pub chown: StepResult, pub mount_ns: StepResult, pub proc: MountResult, pub sysfs: MountResult, pub mknod: StepResult, pub cut_short: Option<String>, .. } // map(); stop(): the first step a RUN needs that failed; in_namespace(f: fn(&Path) -> Result<String, String>, path: &Path) -> Option<Result<Result<String, String>>>: f(path) as root of a new user and mount namespace with that map, in a child (None: no map; Err: f did not run or finish: Error::NotSingleThreaded, a host-side error, Error::ProbeChildLeft, else Error::ProbeClosure; else what f returned); Display: the log line; a probe's child not reaped after SIGKILL: Error::ProbeChildLeft, whose directory the caller leaves in place
pub enum IdMap { Ranges, Newuidmap, OneId }    // #[non_exhaustive]; "multi-range", "newuidmap", "one uid"
pub struct SubordinateIds { pub uids: IdRange, pub gids: IdRange } // the user's first /etc/subuid and /etc/subgid ranges, read by the caller
pub struct IdRange { pub start: u32, pub count: u32 }
pub enum StepResult { Ok, Failed(StepFailure), NotRun(&'static str) }   // #[non_exhaustive]
pub enum MountResult { Fresh, Rbind { fresh: StepFailure }, Failed { fresh: StepFailure, rbind: Option<StepFailure> }, NotRun(&'static str) } // #[non_exhaustive]
pub struct StepFailure { pub op: &'static str, pub errno: Option<i32>, pub detail: Option<String> }
pub struct Sandbox { /* spawner thread; journals in <state-dir>/sandbox/runs */ }
impl Sandbox {
    pub fn start(caps: Caps, dir: &Path) -> Result<Self>;   // replays journals; Full on any root, Direct on / only (ADR-0060); RUN cgroups where delegated (ADR-0110); Full RUNs under SeccompProfile::docker_default
    pub fn start_direct() -> Result<Self>;                   // kaniko mode on / (ADR-0051): Direct on / only; no directory or journal; the process becomes a child subreaper
    #[cfg(test)] pub(crate) fn start_with_root(..) -> Result<Self>; // the Direct chroot off /, test-only (ADR-0060)
    pub fn allow_userns_network(&mut self);                   // --allow network.none-userns (ADR-0033)
    pub fn set_seccomp(&mut self, profile: Option<SeccompProfile>); // --seccomp; None is unconfined; Full RUNs only; before the first RUN
    pub fn set_nofile_limit(&mut self, limit: NofileLimit);  // every later RUN starts with it (ranikust's limit before its raise)
    pub fn run(&self, spec: RunSpec) -> Result<ExitStatus>;  // blocking; cloned on the spawner thread
    pub fn cancel(&self);                                     // cgroup.kill and SIGKILL every running RUN; later runs: Error::Cancelled
}
pub struct SeccompProfile;  // compiled for this architecture, kernel and Docker's 14 capabilities: docker_default(), from_json(&[u8]); Error::SeccompProfile (ADR-0109)
pub fn block_stop_signals() -> io::Result<StopSignals>; // kaniko mode on / (ADR-0077): SIGINT/SIGTERM blocked while single-threaded, read from a signalfd
pub struct StopSignals;  // wait() -> io::Result<StopSignal>, blocking
pub struct StopSignal { pub signo: i32, pub pid: u32, pub obeyed: bool }  // obeyed: from the kernel, or kill/tgkill from pid 0 or ranikust's parent; never rt_sigqueueinfo
pub fn forbid_core_dumps() -> rustix::io::Result<()>;  // first in main (ADR-0051): soft RLIMIT_CORE 0; every later RUN gets the recorded soft limit back (Step::CoreLimit)
pub struct RunSpec { /* root_dir, argv, env, user, workdir, network, hostname, mounts, secrets, stdin/out/err, isolation, script, root_id */ }  // #[non_exhaustive]; RunSpec::new
pub struct BindMount { pub source, pub target, pub kind: MountKind, pub read_only }  // RUN --mount=type=cache
pub struct SecretFile { pub data: Vec<u8>, pub target, pub uid, pub gid, pub mode }  // written by the child on its own tmpfs, bound read-only; Debug hides the bytes
pub enum Stdio { Inherit, Null, Fd(OwnedFd) }
pub enum Network { Host, None }                 // [decided] ADR-0010
pub enum Isolation { Full, Direct }             // #[non_exhaustive]; no bare-chroot variant (ADR-0060)
pub struct NofileLimit { pub soft: Option<u64>, pub hard: Option<u64> } // NofileLimit::current(); None = unlimited
pub enum Error { Setup { step: Step, .. }, Cgroup { op, path, .. }, Exec { program, .. }, InvalidSpec, Privilege, Direct(String), .. }
impl Error { pub fn ran_no_command(&self) -> bool; } // before execve (Cgroup: before the clone): no RUN process can outlive it (ADR-0056); false for Cancelled
```

- `run` blocks: `rnk-builder` calls it from a blocking thread holding a
  `--jobs` permit; the fork happens on the spawner thread whatever thread
  calls. The caller resolves `RUN --mount` into `BindMount`s (cache directory
  from `rnk-store`) and `SecretFile`s and passes `--network`.
- `RunSpec::script` (`Direct` on `/` only) is a `#!` heredoc body: a sealed
  memfd run with `execveat`, read by its interpreter as `/dev/fd/<n>`
  (ADR-0057). `RunSpec::expect_root(&RootPath)` fixes the root's identity:
  the reopened root must match (dev, ino, mnt) in the parent and (dev, ino)
  in the child after the bind (ADR-0059).
- `Direct` refuses mounts, secret files and `Network::None`
  (`Error::Direct`, before any clone); `rnk-builder` leaves cache mounts out
  first, with a warning.
- `start_direct`: after each `RUN` the process tree is killed through pidfds
  (parent and start time checked again; where seccomp refuses `pidfd_open`, a
  plain kill of ranikust's own children, deeper ones on later rounds) and
  reaped by pid with `WNOHANG`; the child cuts its bounding set to Docker's
  defaults (`CAP_SETPCAP`), clears ambient and inheritable and cuts
  permitted/effective, or the `RUN` is refused (`Error::RunReachesState`);
  a start where the cut is impossible is refused too (ADR-0057); warns when
  Yama `ptrace_scope` is 0.
- `start_with_root` (test code): the path must be `checked`'s directory;
  replays that root's journals by identity; refuses the host's `/`, `/etc`,
  `/dev`; a later `RUN` on another path, or whose reopened root differs, is
  refused.

## Invariants

How the sequence works step by step:
[`design/sandbox.md#per-run-sequence`](../../design/sandbox.md#per-run-sequence).
What a change here must keep, with where it lives:

- **Death with the build.** One spawner thread, alive as long as the
  `Sandbox`, clones every `RUN` (raw `clone`, no `CLONE_VM`); the child arms
  `PR_SET_PDEATHSIG(SIGKILL)` and checks its parent pid in its own
  outer-procfs `stat` (`getppid` is 0 in a new PID namespace). An id switch
  clears the signal, so `Direct` arms and checks it again. The full
  sandbox's child stays as the `RUN`'s init, PID 1 of its namespace
  (`child::run_init`, ADR-0080): the command is PID 2, and the namespace dies
  with ranikust even when the command clears its own signal or executes a
  set-id binary. Every child first resets every signal disposition with raw
  `rt_sigaction` (`child::default_signals`). Dropping the `Sandbox` ends the
  thread and kills every running `RUN`.
- **Cancel.** `Sandbox::cancel` kills every running `RUN` with `SIGKILL`
  (its init, after its `cgroup.kill` where it has cgroups; a `Direct` one's
  process group too), then every later `run` returns `Error::Cancelled`. A
  child is forgotten only after `waitid(WNOWAIT)` saw it exit and before
  `wait4` reaps it: the kill never reaches a reused pid.
- **Between `clone` and `execve`** the child only makes system calls (rustix's
  raw backend, a few `libc` calls) on arguments built before the clone. A
  failing step writes `(step, errno)` to a close-on-exec pipe and exits 127;
  end-of-file on the pipe means `execve` succeeded. Right after the clone the
  child closes every descriptor it copied from ranikust but the few it needs
  (`close_inherited`, `Full` and `Direct` alike), then puts the stdio sources
  on 0-2 (`early_stdio`). `/dev/null` is never opened by name in the child:
  `Full` opens its fresh `/dev` tmpfs's node by descriptor before any
  `RUN --mount` (read-only bind); `Direct` gets the host's from the parent
  (`host_null`: no symlink, char device 1:3).
- **Namespaces:** mount, PID, UTS, IPC; network for `--network=none` (the
  clone's own, or, where `CAP_NET_ADMIN` is missing, refused unless
  `allow_userns_network`: a helper's user namespace joined as an nsfs
  descriptor under the `seccomp` module's denylist; probed once by
  `probe_network`, `NetNs`, ADR-0033); with `RUN` cgroups, a cgroup
  namespace the command makes (below). User namespace: Phase 3. Mounts exist
  only in the child, so the parent diffs the root as the image left it.
- **Mounts**, in the order of the design doc. Every target is resolved with
  `openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)` from the bound root and
  mounted through `/proc/self/fd/<n>`; `/proc`, `/sys` and `/dev` must be
  real directories (refused before any stub, checked again in the child).
  The read-only and masked `/proc` and `/sys` paths are found under the new
  mounts' descriptors, never by name. A bind that is remounted (`RUN --mount`,
  secrets, kernel paths) is made with `open_tree(OPEN_TREE_CLONE)`, attached
  with `move_mount` and remounted through its own descriptor (`bind_remount`);
  where seccomp refuses those calls (`mount_api_supported`), a target through
  a symlink fails with `ELOOP` and the mount found again must be new (not one
  of `MadeMounts`). Sources are opened by the child in its own mount
  namespace (`O_PATH`, no final symlink, type checked) and keep their locked
  flags. The root bind is `nodev` recursively (`mount_setattr(AT_RECURSIVE)`,
  Linux 5.12; an older kernel refuses a root with a mount below it).
  Then `pivot_root(".", ".")` + `umount2(MNT_DETACH)`; where seccomp refuses
  it and `/` is a mount root, `MS_MOVE` + `chroot(".")`.
- **Seccomp**
  ([ADR-0109](../../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md)):
  the profile is compiled in the parent, before the set-up, for the native
  architecture only (another's calls and x32 get `EPERM`), the running kernel
  and Docker's 14 capabilities; arguments compare as 64-bit values, the
  strictest matching action wins, a number `libc` does not name gets `ENOSYS`
  under a refusing default, and what the crate does not support
  (`SCMP_ACT_NOTIFY`, `SCMP_ACT_TRACE`, a listener, an unknown flag, field or
  name under a refusing action, two conditions on one argument, a rule without
  names, over 4096 instructions) is `Error::SeccompProfile`, never weakened,
  as is an `SCMP_ACT_ERRNO` whose errno is 0 (a faked success;
  `SeccompProfile::faking` compiles one in the tests only) or from 512 up,
  where the kernel keeps its own codes (516 may have it run its restart block
  instead). Syscall numbers come from `libc` only (`seccomp/syscalls.rs`, a
  table of `libc`'s `SYS_*` names per target, ADR-0009); a call the profile
  allows that `libc` does not name gets `ENOSYS`, and the T0 test
  `calls_the_default_allows_but_libc_does_not_name_are_pinned` lists them per
  target. A profile can only let a call run, refuse it or kill, so after every
  mount the full sandbox's child makes every step that need not follow it
  first, in the order of the design's
  [step 4](../../design/sandbox.md#per-run-sequence), and the init's fork;
  then the init and the command each install ADR-0033's filter when the
  child joined a held network namespace and the profile on top, both
  without `no_new_privs` (`install_filters`; the command after its cgroup
  namespace), and only then set the effective and permitted sets.
  Against a filter around ranikust, which can fake a success, the command
  checks that its parent is the init (`Step::Init`), an `execve` (trying no
  next candidate) or `execveat` back without an error fails as `Step::Exec`
  (`EPROTO`), a blocking `waitpid` back with 0 or an error, `EINTR` included,
  ends the init (125), an `EINTR` on a read of the init's or the command's
  fails the `RUN` (only a filter gives it, possibly forever), `last_errno`
  reads errno 0 as `EPROTO`, and the pipe, capability, death-signal,
  `fstatfs` and `RLIMIT_NOFILE` reads go through `libc` into values set
  beforehand. A failed install is `Step::Seccomp`, whose message points to
  `--seccomp=unconfined`, or `Step::NetworkFilter`. `Direct` gets no filter
  and joins its keyring after its capability drop, before `setsid`. The
  default profile refuses `keyctl`, `add_key` and `request_key`; root's user
  and persistent keyrings stay reachable under `--seccomp=unconfined`, a
  profile file that allows them, and in `Direct`. After a refused join,
  whatever the errno, the child looks ranikust's session keyring up before
  the profile (`KeyringJoin::Required`): found, the `RUN` fails as
  `Step::KeyringRefused`; the lookup refused too, it goes on. A profile that
  refuses all three calls (`SeccompProfile::refuses_keyrings`) and the probe
  skip the lookup (`Optional`); `Direct` goes on after `EPERM`, `ENOSYS` or
  `EACCES` and fails as `Step::SessionKeyring` otherwise.
- **Capabilities, for every `USER`:** bounding set cut to Docker's 14
  defaults, ambient cleared, effective/permitted the defaults the process has,
  inheritable empty. The drops go up to the kernel's last capability; an
  `EINVAL` ends them only where `caps::last_cap()` could not read it and
  assumed 63. Then `setsid`, `setgroups`, `setresgid`, `setresuid`;
  `WORKDIR` is resolved in the root without magic links, entered with
  `fchdir`, and `getcwd` must show it below the root (runc, CVE-2024-21626).
- **Only stdio crosses `execve`** (T7): every other descriptor becomes
  close-on-exec (`close_range`, or the `/proc/self/fd` list before 5.11);
  umask 022, `SIGPIPE` default, empty signal mask. `PATH` is searched after
  entering the root and switching user; a candidate through a magic link is
  refused (`ELOOP`). Not covered: a `#!/proc/self/exe` line or a symlink
  swapped after the check run ranikust's binary in the sandbox, which refuses
  its arguments; that binary is the sealed copy (ADR-0101).
- **Sealed copy** (ADR-0101): every process ranikust forks, the init
  included, runs the copy, so no `RUN` reaches a binary it could write.
  The memfd is sealed `SEAL|SHRINK|GROW|WRITE`, made with `MFD_EXEC` where
  the kernel knows it; the `O_TMPFILE` copy is `O_EXCL` and run read-only,
  never directly on FUSE (`TMPDIR`'s own type is read), and only `ETXTBSY`
  guards it. The copy runs with `execveat(fd, "", AT_EMPTY_PATH)`, the
  descriptor close-on-exec, never `/proc/self/fd/<n>`: an interpreter
  (binfmt_misc, a `#!` line) is then refused before the exec, and the next
  way tried. The copy gets back the core-file limit ranikust started with.
  When no way works the build goes on, warned. The marker carries the
  installed binary's mount only to a copy that checks; otherwise the file run
  is measured. Whether a writable mount reaches the binary is told from
  `/proc/self/mountinfo`: a directory root by its path, compared by
  component; a file root by its inode. Mount points and the binary's name
  are reached from `/` through `RootPath::statx_physical` only, never
  opened, with no symlink on the way; a mount point counts only when it
  leads to its mount's root (`STATX_ATTR_MOUNT_ROOT`), a covered mount is
  not looked up, and the table must read the same once the walk is done;
  the binary's own name counts only while it still leads to the binary on
  its mount. What cannot be told is `Unplaced`, a call that fails
  otherwise `Unknown` (both refused: fail closed), unless a writable mount
  was found to reach the binary on a table that read the same twice
  (`Exposed`). The walk runs only when the
  caller passes `walk` (`ranikust`'s `may_be_kaniko_on_slash`: as root,
  without `--mode=overlay`, in a container): without it a binary the walk
  would weigh is `NotWeighed`, refused should kaniko mode on `/` follow.
- **`Direct`:** no namespace, mount or capability change on `/`; the command
  gets its own session, whose process group is killed with `SIGKILL` once it
  exits (waited with `WNOWAIT`). Details in the design doc's variants table.
- **Relay** (`relay.rs`): no `RUN` holds ranikust's stdout or stderr; each
  becomes a pipe, `fchown`ed to the `RUN`'s ids when not root, copied by one
  thread per `RUN`. Its read ends are non-blocking and it blocks only in
  `poll`, so a `RUN` reading its own pipe back (`/proc/self/fd/1`) cannot
  park it. After the `RUN` is reaped and swept an `eventfd` stops it: the
  pipes are read until empty, a stream that refuses writes gets `DRAIN_TIME`
  (2 s), and the thread is joined; it never waits for a write end that
  outlived the `RUN` (kaniko#2928, ADR-0057).
- **Journal** ([stub mount points](../../design/sandbox.md#stub-mount-points)):
  stubs are journaled in `<state-dir>/sandbox/runs` before they exist (the
  journal is locked before it is named, so a replay never takes a live one)
  and removed before the diff (journals no process holds are replayed by
  `probe` and `Sandbox::start`); the parent's mtime returns to its old value
  only if it still shows the one right after placing. Placing and undoing stay
  on the root's own mount (`RootPath::stay_on_mount`). The replay runs as
  root on a directory others may write: `O_NOFOLLOW` opens, `unlinkat`
  through descriptors, a journal pinned `O_PATH` and reopened
  (`O_NONBLOCK | O_NOCTTY`) only if regular (a planted FIFO or device node
  is never opened), at most `MAX_JOURNAL_BYTES` (16 MiB; a `RUN` needing more
  fails before placing, a replay leaves a larger file unread), a root strictly
  below the state directory (or the shared test root, matched by device,
  inode and mount), a `swap` record naming one of the nine `PLACEABLE` paths
  and a `.ranikust-aside-` name, and `dir` or `dir/runs` never a symlink. A
  journal failing a check is left with a warning. Replay and crash unlock the
  `flock` explicitly before closing (see Pitfalls).
- **`RUN` cgroups** (`cgroup.rs`,
  [design](../../design/sandbox.md#run-cgroups), ADR-0110): only where
  ranikust's own cgroup `D` carries a delegation mark it trusts
  (`trusted_marks`: root, `trusted.delegate` alone; not root, also
  `user.delegate` on a `D` it owns) or is the root of its cgroup namespace, on
  a read-write cgroupfs with `cgroup.kill`; an unreadable mark, or an
  unreadable file of a marked or namespace-root `D`, is a `warn`. The probe
  runs every step once, and a failure leaves the build as without cgroups.
  Nested (ranikust alone in `D`, `cpu` delegated): ranikust in
  `D/ranikust-<pid>`, `+cpu`, an `init` and a `command` leaf per `RUN`;
  otherwise a controller-less `run-<pid>-<n>` with only a `command` leaf.
  Either way the command runs in `command`, in a cgroup namespace of its own
  rooted there and owned by ranikust's user namespace, where it cannot mount
  a cgroupfs: a `RUN` reads `0::/` whatever ranikust's pid and its own
  number, and every cgroup namespace it can make is rooted below the limited
  cgroup. The parent makes and opens a `RUN`'s cgroups before the clone,
  writing `LIMITS` to its `run-<pid>-<n>` first
  (`limit`: `cgroup.max.descendants` `REMOVE_LIMIT - 1`, `cgroup.max.depth`
  `REMOVE_DEPTH`), and a failure there fails the `RUN` before anything ran
  (`Error::Cgroup`); the init (nested) and the command each write `0` to
  their `cgroup.procs` descriptor (moved above 0–2, `fd_above_stdio` in
  `fd.rs`) and close it (`Step::JoinCgroup`), the command before its go, so
  no `RUN` process holds a cgroupfs descriptor; the command then makes its
  cgroup namespace (`Step::CgroupNamespace`), before its filters, which
  refuse `unshare`, and before it gives up `CAP_SYS_ADMIN`
  (`set_capability_sets`).
  After the reap: `cgroup.kill`, `populated 0` within 10 s, then
  `remove_tree`: the `RUN`'s cgroups with every cgroup a `RUN` with a
  cgroupfs of its own made below them, deepest first, each opened relative
  to its parent's descriptor by `open_below` (`openat2`, no link, no other
  mount), within `REMOVE_LIMIT` (4096) and
  `REMOVE_DEPTH` (32), which the limits keep a `RUN` within; any failure fails
  the `RUN` (`Error::Io`, naming the cgroup below the `RUN`'s that failed),
  and nested, the cgroups left keep the drop's `-cpu` from working (a
  warning). A `run-` name a killed ranikust with the same pid left is skipped,
  64 at most (`TAKEN_NAMES`). Removal is explicit, never on drop; dropping the
  `Sandbox` (after its thread) undoes the nesting. One tree per process
  (`CLAIMED`).
- **Limits:** every `RUN` gets `Sandbox::set_nofile_limit`'s
  `RLIMIT_NOFILE` right before `execve` (`Step::FileLimit`; unset, ranikust's)
  and `forbid_core_dumps`'s `RLIMIT_CORE` (`Step::CoreLimit`).
- **The probe** performs the real sequence in the configuration that will be
  used, never by kernel version; a command that does not exist fails at
  `execve` with `ENOENT`, which proves every step before it.
- **The user-namespace probe** forks one child per map from a
  single-threaded caller, before any credential is read; right after the
  fork the child closes every descriptor but stdio and those it uses (the
  pipes, the probe's directory, the `chown` target), then makes system
  calls only, then `_exit`. `in_namespace`'s function may allocate (it
  checks for one thread first) and runs with an empty panic hook and, on
  its thread, no `tracing` subscriber: the inherited ones write ranikust's
  metrics report and log (a thread the function starts must not log). Each
  child is killed at its deadline (10 s, the closure 60 s) and reaped
  within 2 s (one that closed its report but is not reaped then is killed,
  and given 2 s more); its namespaces and mounts go with it. One still
  there after is `Error::ProbeChildLeft`: the probe's directory stays, and
  `ranikust` leaves its own too, until the end of the build removes them.
  `newuidmap` and `newgidmap` each get 10 s, then are killed and given 2 s
  to be reaped; one whose wait fails is too, except after `ECHILD`. The
  child waits for them, so an attempt can run past its 10 s: it is then
  killed as soon as they are done, reaped, and reported as killed after
  10 s. Its mount namespace makes `/` a recursive slave before any mount.
  The files the steps need are made by the caller in the probe's
  directory through `RootPath` and removed before it returns. `newuidmap`
  is looked for in `/usr/bin` and `/bin` only, run with an empty
  environment. A child of a user other than root is dumpable only around
  its map write, which the kernel's owner check of `/proc/<pid>/uid_map`
  needs: made dumpable after `unshare`, not dumpable again right after the
  map, before `chown`. `ranikust` probes as root, whose children never
  change it.

## Refs

- **Design:** [`design/sandbox.md`](../../design/sandbox.md) ([per-`RUN` sequence](../../design/sandbox.md#per-run-sequence), [process set-up](../../design/sandbox.md#process-set-up-once-per-build), [stub mount points](../../design/sandbox.md#stub-mount-points), [variants by environment](../../design/sandbox.md#variants-by-environment)); [`design/modes-and-detection.md#probe-order---modeauto`](../../design/modes-and-detection.md#probe-order---modeauto); [`design/kaniko-mode.md#fake-proc`](../../design/kaniko-mode.md#fake-proc) (not built); [threat model T5-T12, T17-T19, T24](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) (default isolation), [ADR-0004](../../adr/0004-support-every-privilege-level.md) (probe or refuse), [ADR-0010](../../adr/0010-run-network-host-or-none.md) (network), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (bare `chroot` only on request), [ADR-0018](../../adr/0018-no-proot-in-1-0.md) (no proot), [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md) (`JAVA_TOOL_OPTIONS`), [ADR-0101](../../adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md) (the sealed copy of the binary), [ADR-0110](../../adr/0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md) (`RUN` cgroups where delegated), [ADR-0098](../../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md) (planned, Phase 3: images with an owner the user namespace does not map are refused), [ADR-0103](../../adr/0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md) (the default seccomp profile), [ADR-0109](../../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md) (profiles compiled here, moby's default), [ADR-0104](../../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md) (planned, Phase 3: `--userns` runs the whole build in a user namespace; the init's uid unmapped in rootless mode too), [ADR-0100](../../adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md) (planned, Phase 3: where the probe is tested)
- **Open:** [#3](https://github.com/tran4774/ranikust/issues/3) (the type that carries `Caps`), [#26](https://github.com/tran4774/ranikust/issues/26) (wiping secrets from memory), [#31](https://github.com/tran4774/ranikust/issues/31) (placed secret file whose target exists); tasks [#34](https://github.com/tran4774/ranikust/issues/34), [#35](https://github.com/tran4774/ranikust/issues/35), [#39](https://github.com/tran4774/ranikust/issues/39), [#40](https://github.com/tran4774/ranikust/issues/40), [#83](https://github.com/tran4774/ranikust/issues/83) (`--userns`), [#88](https://github.com/tran4774/ranikust/issues/88) (bind targets against changes during set-up), [#89](https://github.com/tran4774/ranikust/issues/89) (host-safety attack suite across modes)
- **Evidence:** [exp-02](../../research/experiments/02-run-sandbox.md) (sequence, escape, PDEATHSIG, bounding set, stubs, 1-2.5 ms), [exp-06a](../../research/experiments/06a-root-rerun.md) (real root), [exp-13](../../research/experiments/13-docker-rootful-vm.md) (Docker probe matrix), [exp-12a](../../research/experiments/12a-fake-proc.md), [exp-14](../../research/experiments/14-real-builds-without-proc.md), [exp-15](../../research/experiments/15-java-next-concurrency.md), [exp-16](../../research/experiments/16-cgroup-v1-cpuset.md) (fake `/proc`, Java)
- **kaniko:** `kaniko@44aa7ddc0 pkg/commands/run.go:288` `runCommandInExec` (no namespaces; `:335` searches the process's `PATH`); `:356` and `:407` process-group kill (replaced by the PID namespace, kept for `Direct`); `:375` users from the process's `/etc/passwd`; `:83-135` cache mounts via `swapDir` (a bind here)
- **External:** BuildKit `executor/stubs.go` <https://github.com/moby/buildkit>, runc `--no-pivot`, `libcontainer` capabilities and `CloneBinary` (the sealed copy; `libcontainer/dmz` in v1.2.0, `libcontainer/exeseal` since v1.3.0) <https://github.com/opencontainers/runc>

## Tests

Tiers and commands: [`docs/testing.md`](../../testing.md#tiers).

- **T0** (`cargo test -p rnk-sandbox`): the cgroup decisions and parsers
  (`src/cgroup/tests.rs`: which trees count as delegated, membership,
  controllers, `cgroup.events`, the probe child's report, absent files and
  delegation marks and which a root or other ranikust trusts, the single
  layout's reason, a taken `run-` name, a `RUN`'s cgroup that cannot be
  made (its step, its path, nothing left), the limits written, the removal
  of every cgroup below a `RUN`'s, a tree at the kernel's limits removed in
  full, its bounds, the cgroup a failed removal names, and a walk that
  enters no link or mount,
  detection on this host, which only reads); spec validation, mount plan order,
  environment defaults, `PATH` candidates, `/etc/hosts`, capability lists,
  failure records, the sealed copy (the order of the ways and the stop
  when the core-file limit cannot go back, one marker in the copy's
  environment, a memfd sealed against writes, an `O_TMPFILE` copy with
  no name, open read-only, a `#!` copy refused before the exec in a child
  run of the test binary, a way after another copying the whole binary,
  the binary's mount against `/` (a zero id: `NoMountId`), measured on
  files and carried in the marker, a marker that does not check not
  believed, a writable mount reaching the binary by sample mountinfo,
  a binary in a root that is not an overlay weighed (by sample values and
  on `/`), a fuse-overlayfs root counted as an overlay, a FUSE root the
  table does not type unplaced, the root mount not counted, FUSE told
  apart by `statfs` magic, escaped names in the mount table,
  `red_team_escaped_names_in_the_mount_table_are_compared_unescaped`; a
  file bind told by its inode, renamed or replaced, a mount root reached at
  its mount point (a file inside the mount is not its root), a symlink on
  the way placing nothing, a covered mount not looked up (a chain of
  parents ending at its first repeat), a table that
  changed during the walk placing nothing, no walk when not asked
  (`NotWeighed`), what cannot be told placing nothing (the binary's own
  name removed while it runs among them), a failed lookup keeping its
  errno unless the binary is reached,
  `red_team_a_writable_bind_of_a_removed_link_reaches_the_binary`,
  `red_team_binds_stacked_on_the_binarys_path`), the stub journal
  (placement through symlinks, removal, parent times, replay of dead
  journals only, a lock kept by a shared description), swaps and planted
  records, the journal's red-team tests (a file, FIFO, socket or whiteout
  where a directory is needed or as the target; stubs a `RUN` swaps for
  symlinks out of the root or for nodes; planted stub records aimed
  outside, naming `/` or ending in `..`), `Direct` children that fail
  before executing, mount sources and remount flags, `Direct` refusals,
  the sealed heredoc memfd, the relay (a surviving write end, a full
  stream, a `RUN` reading its own stream back), the init's status records,
  `red_team_hostile_mount_point_names_are_matched_exactly` (the submount
  fallback where `mount_setattr` is missing, in `src/spawner.rs`), the spans
  (`tests/spans.rs`). Seccomp (`src/seccomp/`): the assembler's long jumps;
  compiled profiles run on an interpreter (64-bit compares, an action per
  rule, long jumps, moby's profile against a table of calls for x86_64 and
  aarch64, `includes`/`excludes`, refused profiles, errno 0, every errno
  from 512 to 600, 4095 and above included, which ones refuse `keyctl`,
  `add_key` and `request_key`; random profiles against a reference, x32
  numbers, keys given twice, nesting and odd bytes, numbers in other forms,
  rules that disagree, the kernel's 4096 instructions);
  `errno_0_reads_as_eproto`. Moby's profile's argument-rule form
  (`docker_default_restricts_arguments_by_allowing_values`) and its refusal
  of `clone3` and `io_uring_*`
  (`docker_default_gives_a_run_neither_clone3_nor_io_uring`). The
  `tests::kernel` modules, skipped under qemu-user, install filters in forked
  children: `seccomp::profile`'s (compiled profiles; random ones answering as
  their rules say; 4096 instructions; on x86_64, x32 and, where the kernel
  has it, i386 calls refused), `caps`'s (an `EINVAL` from a bounding drop)
  and `child`'s (a refused keyring join with the session keyring found or
  not, each `KeyringJoin`; an `execve` or `execveat` back without an error; a
  `waitpid` back with 0 or `EINTR` with a command still to reap; an `EINTR` on
  the init's and the command's reads, the parent check's against an unfiltered
  control; the command's go only on its init's go byte, never on an ended
  pipe, another byte or a read faked with no error, and the init's go byte
  sent, or the pipe ended without it, or the init ended where neither can
  be done; the init's closing: a refused `close` reported with its errno,
  `EBADF` and `EINTR` included (also run for the musl target), whether
  there are descriptors to close (at most the pass's listing left open) or
  only the listing; a pass that does not shrink ends it; nothing left where
  `close_range` alone is refused; a refused `umask` after the profile; a
  faked `setsid`, `pipe2` or `fstatfs`, a faked or refused `prlimit64`; the
  last two on a stack filled
  beforehand, which shows an unwritten buffer in a debug build only; these
  test the helpers, not their call sites, whose `/proc/self/fd` opening
  overwrites that stack).
  `tests/direct_only.rs`: a `start_direct` sandbox
  refuses `Direct` off `/` (symlink to `/`, `//`, `/.`, `..` roots) and places
  nothing.
- **T0**, the user-namespace probe (`src/userns/tests.rs`): the maps
  planned for root and for another user, the mirror of a map, the steps
  assembled from a child's records (a refused `unshare`, each map refused,
  a fresh kernel filesystem or its rbind, Ubuntu 24.04's ranged stop at
  `chown`), records across split reads and the deadline, the report's line
  and stop, a map this process fails to write ending the waiting child at
  once, a failure part-way through making the probe's files removing what
  it made, `in_namespace` not running its closure beside other threads, a
  panic in the closure reported as not finishing, never as its refusal, nor
  through the parent's panic hook, and a log line of the closure not
  through its subscriber; a child holding only stdio and its own
  descriptors; a child reaped within its grace, else killed, else reported
  left; a tool reaped elsewhere (`ECHILD`) not killed. Its red team
  (`red_team_*`, as any user; T22, T4, T7, T8, T9): names planted where it
  makes its files (a symlink out, a hard link of a file outside, a FIFO;
  the directory itself a symlink or a FIFO) never followed, opened or
  removed; nothing left after a probe and a closure that mounts (no
  process, orphan included, no descriptor, mount or entry); the prober
  killed while a child of the probe runs (`SIGKILL`, `SIGINT`, `SIGINT` to
  its group) leaves no process or mount.
- **T1** (`just smoke`, rootless podman): the whole sequence in a user
  namespace with `pivot_root` and `RUN`s that check the result from inside
  (PID 2 under `ranikust-init`; the init under the command's filters; host
  name; capability sets `a80425fb` with empty inheritable and ambient;
  `nodev` root and `/dev`; read-only `/proc/sys` and `/dev/null`; masked
  kernel files; no terminal; only stdio descriptors; no stub in the
  layer); `sealed-exe`: a `RUN` reaches no thread of ranikust's or its
  init's (ADR-0101). `kaniko-flags` in the kaniko leg: the user-namespace
  probe passes under rootless Podman's defaults, with an rbind of `/proc`
  (#74); `userns-probe`, its red team at the binary's level
  ([its steps](../../testing.md#quick-smoke-cases)). Not yet: the
  sandbox's fallbacks.
- **T2** (VM only, `#[ignore = "T2: ..."]`, `just test-vm unit`):
  `probe_reaches_exec_as_root` and `a_run_joins_a_held_network_namespace`
  (`src/probe.rs`);
  `the_user_namespace_steps_as_a_regular_user_stop_where_apparmor_restricts`
  (`src/userns/tests.rs`: as the user who ran sudo, one id, then
  `newuidmap` with that user's ranges; where AppArmor restricts user
  namespaces, Ubuntu 24.04, one id stops at the `uid_map` write and ranges
  at `chown`, elsewhere both pass; both harnesses install `newuidmap`, and
  the user must have subordinate ids; prints `measured:` lines);
  `a_device_node_planted_as_a_journal_is_never_opened` (`src/journal.rs`); in `src/spawner.rs` the `Full` tests (`nodev` below the
  root; descriptors, symlinked `/proc`, a cache over `/dev`, symlinked mount
  targets, every mount holding only stdio, `/dev` nodes unchangeable;
  `the_run_is_pid_2_under_an_init_that_reports_its_status`;
  `red_team_no_run_outlives_the_thread_that_cloned_it`;
  `red_team_a_run_can_slow_its_inits_death_but_not_stop_it`, which prints the
  delays, threat model section 6;
  `a_running_binary_cannot_be_opened_for_writing`;
  `a_run_gets_its_seccomp_profile`: the default, unconfined, a file, a
  profile that refuses the keyring join, and unconfined with a held network
  (ADR-0033's filter); each `RUN` in a keyring of its own, its init under
  the same filters as its command;
  `a_runs_command_starts_only_once_its_init_has_finished`: under a profile
  that fails one of the init's last steps, or its closes (all, or one
  with `close_range`, refused with `EPERM` or `EBADF`), the `RUN` fails as
  `Step::Init` with that errno and its command leaves no mark, which it
  leaves under a control profile; a profile refusing `close` alone still
  starts the command, whose status ends the `RUN`;
  `a_profile_cannot_fake_the_steps_before_it`: profiles answering `prctl`,
  `setsid`, `umask`, `seccomp` or `capset` with `EINVAL` or 0 (which
  `--seccomp` refuses; compiled for the test) leave the bounding set, the
  session, the init's dump flag, ADR-0033's filter and the capability sets
  as they are and the umask at 022 whatever ranikust's; one faking the
  init's fork and `setsid` runs the `RUN`, the fork being before the
  profile, while the same filter around ranikust (on the test thread)
  fails it, and a filter install refused around ranikust fails it as
  `Step::Seccomp` before the command runs;
  `a_refused_keyring_join_fails_a_run_whose_profile_allows_keyctl`: under
  filters on the test thread, while the keyring can be looked up, a join
  refused with `EPERM` fails `unconfined`, not `default`, and one refused with
  `EACCES` fails `unconfined` and a file allowing `keyctl`, not `default` or a
  profile refusing `keyctl`, `add_key` and `request_key`; with `keyctl`
  refused whole, `unconfined` and that file run, as under Docker),
  in `src/spawner/cgroup_tests.rs` the `RUN` cgroups, each starting this test
  binary again in a transient systemd scope or a Docker container
  (`run_cgroups_follow_the_delegation_of_a_systemd_scope`: nested, single,
  cgroups a `RUN` made below its own up to the limits (`EAGAIN` past them,
  whatever it wrote to the limit files it could see) removed, a failed
  probe, a planted `run-` name, a scope without `Delegate=` and the ssh
  session's, and what a `RUN` under Docker's default profile reads in
  `/proc/self/cgroup` and `/proc/1/cgroup`, with Docker's capabilities
  alone and its init with none, both under the profile's filter, and the
  same reads for a later unconfined `RUN`, both under ranikust's filters
  alone; the default profile refuses the first a user namespace, which an
  unconfined `RUN` gets (the control); the `RUN`s that make a user
  namespace, there and below, are unconfined, as `--seccomp=unconfined`;
  `run_cgroup_limits_on_a_cgroupfs_without_nsdelegate`, the same `RUN`s with
  the VM's cgroupfs remounted without `nsdelegate`, where the limits hold
  too;
  `red_team_a_run_cannot_forge_a_delegation_mark_root_trusts`, a `RUN` that
  sets `user.delegate` on ranikust's cgroup, or the test where AppArmor
  keeps the `RUN` from it;
  `run_cgroups_in_a_privileged_container_and_none_in_a_default_one`;
  `a_nested_tree_shortens_the_spinner_cases_init_death`, the spinner cases nested;
  `a_killed_ranikusts_cgroups_go_with_its_unit_or_container`;
  `a_runs_cgroups_go_with_it_whatever_it_left_in_them`, `RUN`s that leave a
  frozen cgroup, a threaded subtree or mounts over a cgroupfs of their own,
  and one that froze all of its cgroups, cancelled;
  `no_process_of_a_frozen_run_outlives_a_killed_ranikust`, nested and single
  in a privileged container whose PID 1 outlives ranikust (both lift
  AppArmor's user namespace restriction meanwhile); only where
  `inner_marks` finds a `Delegate=yes` scope marked do the scope cases expect
  cgroups, the failed probe and the killed scope run, and the spinners and
  the teardown `RUN`s run in a scope rather than a container), which print
  `measured:` lines,
  `a_direct_run_on_slash_keeps_only_dockers_capabilities` and
  `direct_runs_in_a_chroot_with_placed_files_as_root`.
- **T2, VM smoke suite** (`tests/vm/guest/smoke.sh`, `just test-vm smoke`):
  the `smoke-build` example (`crates/ranikust/examples/smoke-build.rs`: pull,
  extract, sandboxed `--run` commands, a one-layer OCI layout) as real root
  in three environments (the host, `--privileged` Docker with `pivot_root`,
  `SYS_ADMIN` under Docker's seccomp with `MS_MOVE`). Its `--run`s are the
  `smoke_checks` of `tests/smoke/lib.bash` as in T1 plus what only real root
  can check: chroot escapes (`..`, from a descriptor, nested) with a plain
  chroot as their control, a write to `/proc/sys`, `mknod` of `/dev/null`'s
  1:3 and open; the example is also refused inside a plain chroot. The
  `ranikust` binary then builds `tests/smoke/e2e` in each environment and
  is refused inside a plain chroot too; in the host variant it is sent
  SIGKILL and SIGINT during `RUN sleep 300` (and SIGKILL during a `RUN` that
  cleared its own death signal), which leave no `RUN` process, and builds in
  overlay mode, kaniko mode per stage and with the memfd skipped while
  `tests/vm/guest/sealed-exe.py` checks that ranikust and its init run one
  sealed copy root cannot write (ADR-0101). The quick suite's kaniko mode
  checks a build on a container's `/` runs from a sealed memfd. In the
  `SYS_ADMIN` variant `tests/smoke/netns-check.py` passes over the default
  profile and under `--seccomp=unconfined`, one filter apart.
- **T2c** (rootful Docker, `just test-container unit-c`): the `Direct` `RUN`
  on `/` in `tests/direct_attacks.rs` (heredoc memfd cannot be rewritten, only
  own descriptors, stolen or read-back output cannot hang or stall, pid
  hopping caught by the sweep, non-blocking flood) and `src/spawner.rs`
  (`a_cancel_kills_a_running_run`, `a_direct_run_leaves_no_process_behind`,
  `a_direct_heredoc_runs_from_an_anonymous_file`, loopback for
  `--network=none`);
  `the_user_namespace_steps_as_root_pass_on_a_host_and_stop_at_unshare_in_docker`
  (`src/userns/tests.rs`, profile `default`, also in the T2 unit suite on the
  VM: every step passes with root's own ranges on a host, `unshare` is
  refused under Docker's defaults, and nothing is left in the probe's
  directory); the probe's red team as root (`red_team_as_root_*`, profile
  `sysadmin`: the T0 attacks with the whole path, `proc` and `sysfs`
  mounts and `mknodat` reached, and the probe on a tmpfs, one that runs
  out of inodes part-way, a read-only one and a ramfs, each leaving
  nothing; prints `measured:` lines); in `src/journal.rs`, a block device where a directory
  is needed or as the target
  (`red_team_a_block_device_in_a_mount_target_is_enotdir_or_the_target_itself`).

## Metrics

- `rnk_sandbox.run` per `RUN`: `wait4` rusage (`user_ns`, `sys_ns`,
  `max_rss_kb`, faults, blocks, context switches) when the child was reaped
  after `execve`, and `failed`; `rnk_sandbox.setup` (`stubs`);
  `rnk_sandbox.probe` (`ok`) per attempt; `rnk_sandbox.probe_userns`
  (`ok`, `map`) per user-namespace probe; `rnk_sandbox.cgroups` (`layout`:
  `nested`, `single` or `none`) once per `Sandbox::start` that may use them
  ([rows](../../design/metrics.md#rnk-sandbox)).
- Spans are opened on the caller's thread, so they nest in its span with no
  `parent:`; the spawner thread opens none; nothing is traced between fork
  and exec. The probe's own runs record no `run` or `setup` span.

## Pitfalls

- A bare `chroot` lets root escape to the host's `/` (exp-02, exp-06a,
  exp-13) or, under `--parallel-stages`, to sibling stages (exp-12).
- `MS_MOVE` + `chroot` leaves the old root mounted under the stage root; `..`
  stops at the moved root only when `/` is a mount root. So it is used only
  there, with Docker's 14 capabilities, and ranikust inside a plain chroot
  refuses (decided, [Q-077](../../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape)).
- A bind refuses (`EINVAL`) a source opened in another mount namespace: a
  descriptor the parent opened cannot be bound by the child (`just smoke`
  found it; no T0 test runs a `Full` child).
- busybox `test -w` is true for root whatever the mount: check read-only
  mounts in `/proc/self/mountinfo`.
- A fresh `proc` is writable: a sysctl write needs only uid 0, and
  `kernel/core_pattern` makes the host kernel run a `RUN` binary as host
  root. Docker's read-only and masked paths are not optional (T17).
- Docker stops device nodes with its device cgroup; without one only `nodev`
  does. A bind of a host node is the host's inode: `chmod` in a `RUN` changes
  the host's `/dev/null` unless the bind is read-only (T18).
- `setresuid` and `setresgid` clear `PR_SET_PDEATHSIG`; a `RUN` with a
  non-root `USER` outlived the build until it was armed again (T8).
- A `RUN` that keeps ranikust's controlling terminal can inject input into
  the operator's shell with `TIOCSTI` (T19).
- Multithreaded `unshare(CLONE_NEWNS)` moves one thread; `CLONE_NEWUSER` is
  `EINVAL` (exp-02 finding 5).
- PDEATHSIG fires when the parent *thread* exits (exp-02): without it a
  `RUN` outlived a killed build.
- Without the bounding-set drop `RUN` unmounted the `resolv.conf` bind and
  wrote the root; unjournaled stubs leak and survive `SIGKILL` (exp-02).
- A filter installed without `no_new_privs` needs `CAP_SYS_ADMIN`: after the
  capability drop it fails with `EACCES`, and with `no_new_privs` a setuid
  binary of the `RUN` would lose its privilege.
- Docker's seccomp blocks `pivot_root` even with `CAP_SYS_ADMIN`; on Ubuntu
  24.04 and later `unshare` works, then mounts fail (exp-02).
- Rust ignores `SIGPIPE` and ignored signals survive `execve`: unless reset,
  `yes | head` in a `RUN` ends with `EPIPE` errors instead of a quiet
  `SIGPIPE`.
- `next build` fails without `/proc/self/stat` (exp-14); Java is OOM-killed
  under a memory limit without `JAVA_TOOL_OPTIONS` (exp-15). A hard-coded
  cgroup file set fixes neither v1 nor v2 (exp-13, exp-16); never fake
  `/proc/self/{exe,fd,maps,status}` (exp-12a). Both apply to the unbuilt
  `--parallel-stages` path.
- An unrestored parent mtime put `/etc` in every layer (exp-12).
- The capability drop comes before the init forks the command, which needs
  `CAP_SYS_ADMIN` for its cgroup namespace once it is in `command`: so the
  child keeps it (out of the bounding set) only where the `RUN` has cgroups;
  the init drops it with the rest, the command before its go. A namespace
  made earlier, by the init, would be rooted at the init's cgroup: the
  command would not read `0::/`, and in the single layout would read
  ranikust's pid and the `RUN`'s number.
- Nesting moves ranikust's whole process out of its cgroup: anything that
  reads `/proc/self/cgroup` after `Sandbox::start` sees the leaf, which has
  only `cpu` (the memory gate of `rnk-sched` and the metrics of
  `rnk-metrics` read its parent, the cgroup ranikust started in, and no
  cgroup above). No cgroup holding a process may enable `+cpu`, and none
  with `+cpu` may take one, so the order is: leaf, move, `+cpu`; undone in
  reverse.
- Closing a `flock`ed file does not release the lock while a child cloned by
  another thread still shares its description (until `execve` or exit): a
  replay then saw a dead journal as live (a flaky T0 test). Unlock
  explicitly.
- A `Direct` `RUN` in a `chroot` has no `/proc`; where mounting works, run
  that root with `Isolation::Full` instead.
