# rnk-sandbox

> **Status:** in progress · **Phase:** 1-2 (`Direct` from 2; rootless: 3) ·
> **Depends on:** `rnk-rootfs` (`RootPath`, `USER` lookup) ·
> **Used by:** `rnk-builder`; `ranikust` (probes, process set-up) ·
> **Path:** `crates/rnk-sandbox/`

**TL;DR:** Runs each `RUN` in fresh namespaces inside the stage root, entered
with `pivot_root` (or `MS_MOVE`), with the capability bounding set cut to
Docker's defaults and Docker's default seccomp profile, and probes its own
capabilities. Never a bare `chroot` where mounting is possible; no `RUN`
process outlives the build. Rootless overlay mode
([#79](https://github.com/tran4774/ranikust/issues/79)) and root's
`--userns` ([#83](https://github.com/tran4774/ranikust/issues/83)) are
built; for them:
the probe of that path
([#74](https://github.com/tran4774/ranikust/issues/74)), the entry into
the build's user namespace
([#75](https://github.com/tran4774/ranikust/issues/75)), and a `RUN` in
it with the `sysfs` rbind the probe chooses (`/proc`'s only on request,
[ADR-0126](../../adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)),
`setgroups` except where the gid map gives one id and `setgroups` is
denied, and `--network=none`
([#78](https://github.com/tran4774/ranikust/issues/78)); the binary calls
them for both.

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
  steps, which `ranikust` runs with `rnk-snapshot`'s. The probe runs the
  `Full` per-`RUN` sequence in an empty root (`probe.rs`) and reports
  `root_entry` and how a `RUN` gets `proc` and `sysfs`: a new one, else,
  in the build's user namespace, an rbind, logged once (#78): `/sys`'s
  where the user-namespace probe found the same refusal, `/proc`'s only
  at the operator's request (`Fallbacks::allow_proc_rbind`,
  [ADR-0126](../../adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)),
  unasked a refusal at `Step::MountProc` with a warning that says how to
  get a new `proc`; `ranikust`'s `plan` refuses before that, with its own
  guidance.
- The user-namespace probe (`userns.rs`, #74): `ranikust` routes rootless
  overlay mode on it and reports it only elsewhere. Each step a `RUN` in a
  user namespace needs, in forked
  children that leave no namespace or mount behind: `unshare(CLONE_NEWUSER)`;
  the id map (root: its own ranges mirrored, else one id; another user:
  `newuidmap`/`newgidmap` with the subordinate ids the caller read, else one
  id; root with `--userns`, `BuildIds::Shifted`: that range alone, `0 S C`,
  root's own ids unmapped, its children taking the range's first ids before
  each step, which the probe's files are given to); `chown` to a mapped id
  other than 0; `unshare(CLONE_NEWNS)`, a second
  call after the map, so that the mount namespace is owned by the new user
  namespace and a refusal names its step (with a ranged map Ubuntu 24.04
  refuses it too, after `chown`); a new `proc` in a new PID namespace, else
  an rbind of `/proc`; read-only `sysfs`, else an rbind of `/sys`;
  `mknod` (reported, never needed). `UsernsReport::in_namespace` runs a
  closure as root of a namespace with the map that worked, for the overlay
  probe. `ranikust` probes as root in root's own modes (reported only,
  no subordinate ids) and as the user for rootless overlay mode, with the
  subordinate ids it reads (`subids.rs`), which the build's user namespace
  below takes too, and as root with `--userns`, with root's `ranikust`
  range.
- The build's user namespace (`userns/build.rs`, #75): moves the process
  into a user namespace of its own and a mount namespace that namespace
  owns, `/` a recursive slave. Each map of the probe's order is first tried
  in a forked child (`unshare`, the map, the mount namespace); the first
  that passes is made for the process itself: root mirrors every id it has
  through a helper forked before the `unshare`; another user maps its
  subordinate ids through `newuidmap` and `newgidmap` (from `/usr/bin` or
  `/bin`), from the same helper; else the process writes one id itself,
  `setgroups` denied (ADR-0098). Root with `--userns` (`BuildIds::Shifted`,
  ADR-0104) tries its `ranikust` range alone, never one id, written by the
  helper with its own ids unmapped; once mapped, `assume_root` drops every
  supplementary group and takes uid and gid 0 of the namespace, the
  range's first ids on the host, then makes the process not dumpable and
  arms its parent-death signal again, which the change of credentials
  resets. Another user is refused such a range before any change. The map and `setgroups` reported are
  read back once inside: any map that gives one id, root's mirrored one
  too, is one uid. Dumpable only while its map is written
  (ADR-0051). Nothing passing leaves the process as it was
  (`Error::NoUserNamespace`); a step failing once inside is
  `Error::UserNamespaceBroken`, after which the process must end.
- The process left outside that namespace (`supervise.rs`, ADR-0136): for
  root's `--mode=auto`, `ranikust` forks before it enters; the child sets
  up and builds, the parent waits with `SIGINT`, `SIGTERM` and `SIGCHLD`
  blocked, forwards each stop as `SIGUSR1` (`FORWARDED_STOP`), reaps any
  other child (PID 1), and ends as the child did, or, where the child
  exited asking for it before its build and no stop came, returns the
  reason so that kaniko mode follows in the parent. The child ignores
  `SIGINT` and `SIGTERM` and gets `SIGKILL` when the parent dies.
- The per-`RUN` sandbox: namespaces, mounts, `pivot_root`, capability drop,
  the `USER` ids `rnk-rootfs` resolved in the root, stubs, `exec`. In the
  build's user namespace (#78): the `proc` and `sysfs` the probe chose;
  `setgroups` skipped only where `/proc/self/setgroups` is `deny` and the
  gid map gives one id
  ([sandbox.md, step 2](../../design/sandbox.md#per-run-sequence)) and
  `devpts` without `gid=5` where gid 5 is unmapped, both read once per
  `Sandbox` (`OwnNamespace`).
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
pub fn enter_build_user_namespace(ids: BuildIds) -> Result<BuildUserNamespace>; // #75, instead of enter_build_namespace, before any thread and any read of credentials: a user namespace and a mount namespace it owns, the first map whose trial in a child passed; Error::NotSingleThreaded, InPlainChroot, NoUserNamespace { tried } with the process unchanged; UserNamespaceBroken(StepFailure) once inside: the process must end
pub struct BuildUserNamespace { pub map: IdMap, pub passed_over: Vec<(IdMap, StepResult)>, .. } // #[non_exhaustive]; read back from /proc/self once inside: map OneId whenever the uid or gid map gives one id (root's own one-id map mirrored too); setgroups_allowed(): as /proc/self/setgroups says (deny for OneId written by the process, or inherited); unreadable: UserNamespaceBroken; Display: "newuidmap map", "one uid map, setgroups denied (newuidmap: ...)"
pub fn trim_to_run_capabilities() -> Result<()>; // kaniko mode on /, before start_direct (ADR-0078): cuts ranikust's own sets to what a Direct RUN keeps, dumpable 0 again, read back; no-op when not root; refuses with other threads (Error::NotSingleThreaded), or (Error::RunReachesState) where the bounding set cannot be cut
pub fn probe(dir: &Path) -> Result<Caps>;       // the real sequence in an empty root: pivot_root, else MS_MOVE; as probe_with_fallbacks(dir, Fallbacks::default()), without its warning on a refused new proc
pub fn probe_with_fallbacks(dir: &Path, allowed: Fallbacks) -> Result<Caps>; // #78: a new proc and sysfs first; an EPERM of one allowed tried again as an rbind; warns once for a proc rbind, info for sysfs; a proc EPERM not allowed: Error::Setup at Step::MountProc and a warning with how to get a new proc
pub struct Caps { pub root_entry: RootEntry, .. } // #[non_exhaustive]; every RUN of the Sandbox gets them; proc(), sysfs(): KernelFs, read-only, so an rbind is only the probe's choice
pub enum KernelFs { Fresh, Rbind }              // #[non_exhaustive]; Display: "a new instance", "a read-only rbind" (the outcome line)
pub struct Fallbacks { .. }                    // Default: none; UsernsReport::fallbacks(): the /sys rbind the user-namespace probe needed, never /proc's
impl Fallbacks { pub fn allow_proc_rbind(self) -> Self; } // the operator's request (ADR-0126; `ranikust` calls it for --allow proc.rbind): Sandbox then runs RUNs one at a time (ADR-0127); a RUN reads every process's files of /proc/<pid> not under ptrace's check (cmdline, status, stat, mountinfo, cgroup, net/*…), not the environment, files or memory of one outside the build's user namespace and those below it (ADR-0130; threat model section 6), and writes nothing there: the bind is read-only (ADR-0132); this crate does not scan for a shared PID namespace: `ranikust` runs the peer scan (ADR-0129) and the check of ADR-0134 before it enters the build's user namespace
pub enum RootEntry { PivotRoot, MoveMount }
pub fn probe_user_namespace(dir: &RootPath, ids: BuildIds) -> Result<UsernsReport>; // #74, reported only; single-threaded caller; dir: an empty directory it made on the state directory's filesystem, held from there, left empty
pub struct UsernsReport { pub unshare: StepResult, pub maps: Vec<(IdMap, StepResult)>, pub chown: StepResult, pub mount_ns: StepResult, pub proc: MountResult, pub sysfs: MountResult, pub mknod: StepResult, pub cut_short: Option<String>, .. } // map(); stop(allowed: Fallbacks): the first step a RUN needs that failed, an rbind of /proc passing `proc` only where allowed has allow_proc_rbind (ADR-0126), one of /sys always; in_namespace(f: fn(&Path) -> Result<String, String>, path: &Path) -> Option<Result<Result<String, String>>>: f(path) as root of a new user and mount namespace with that map, in a child (None: no map; Err: f did not run or finish: Error::NotSingleThreaded, a host-side error, Error::ProbeChildLeft, else Error::ProbeClosure; else what f returned); Display: the log line; a probe's child not reaped after SIGKILL: Error::ProbeChildLeft, whose directory the caller leaves in place
pub enum IdMap { Ranges, Newuidmap, OneId, Shifted } // #[non_exhaustive]; "multi-range", "newuidmap", "one uid", "shifted range"
pub enum BuildIds { Own(Option<SubordinateIds>), Shifted(SubordinateIds) } // #[non_exhaustive]; Own: the caller's ids, with its subordinate ranges if any; Shifted: root's --userns range (ADR-0104), only as root, else Error::Setup before any change; shifted_root(): the range's first uid and gid
pub struct SubordinateIds { pub uids: IdRange, pub gids: IdRange } // the user's first /etc/subuid and /etc/subgid ranges, read by the caller
pub struct IdRange { pub start: u32, pub count: u32 }
pub enum StepResult { Ok, Failed(StepFailure), NotRun(&'static str) }   // #[non_exhaustive]
pub enum MountResult { Fresh, Rbind { fresh: StepFailure }, Failed { fresh: StepFailure, rbind: Option<StepFailure> }, NotRun(&'static str) } // #[non_exhaustive]
pub struct StepFailure { pub op: &'static str, pub errno: Option<i32>, pub detail: Option<String> }
pub struct Sandbox { /* spawner thread; journals in <state-dir>/sandbox/runs */ }
impl Sandbox {
    pub fn start(caps: Caps, dir: &Path) -> Result<Self>;   // replays journals; Full on any root, Direct on / only (ADR-0060; none where Caps::proc() is Rbind, ADR-0127); RUN cgroups where delegated (ADR-0110); Full RUNs under SeccompProfile::docker_default
    pub fn start_direct() -> Result<Self>;                   // kaniko mode on / (ADR-0051): Direct on / only; no directory or journal; the process becomes a child subreaper
    #[cfg(test)] pub(crate) fn start_with_root(..) -> Result<Self>; // the Direct chroot off /, test-only (ADR-0060)
    pub fn allow_userns_network(&mut self);                   // --allow network.none-userns (ADR-0033)
    pub fn set_seccomp(&mut self, profile: Option<SeccompProfile>); // --seccomp; None is unconfined; Full RUNs only; before the first RUN
    pub fn set_nofile_limit(&mut self, limit: NofileLimit);  // every later RUN starts with it (ranikust's limit before its raise)
    pub fn run(&self, spec: RunSpec) -> Result<ExitStatus>;  // blocking; cloned on the spawner thread; where Caps::proc() is Rbind, a RUN runs alone among those of every Sandbox of the process (the others share a turn), the turn held until its init is reaped, Isolation::Direct refused before any wait (ADR-0127); after a panic or a lost spawner thread during a turn, Error::TurnPoisoned for every later RUN
    pub fn cancel(&self);                                     // cgroup.kill and SIGKILL every running RUN; later runs: Error::Cancelled
}
pub struct SeccompProfile;  // compiled for this architecture, kernel and Docker's 14 capabilities: docker_default(), from_json(&[u8]); Error::SeccompProfile (ADR-0109)
pub fn fork_to_wait() -> io::Result<Forked>; // ADR-0136, single-threaded, before enter_build_user_namespace: Forked::Child(Supervised) enters; Forked::Parent(Box<Waiting>) waits outside; Err: the process as before
pub struct Supervised;   // proceed(self) -> io::Result<()>: before the build, the fallback no longer possible (Err: parent gone); undoing(&self) -> io::Result<()>: FORWARDED_STOP ignored from here, while the set-up is undone; fall_back(self, reason: &str) -> !: exits 75, the fallback's own status, with the reason (at most 4000 bytes) for the parent (logged here where it cannot be sent)
pub struct Waiting;      // wait(self: Box<Self>) -> String: returns the reason only where the child fell back and no SIGINT/SIGTERM came, mask and SIGCHLD as before the fork; else ends the process as the child ended (exit code, 1 for 75 without a reason; or the signal: the stop for FORWARDED_STOP or 75; 128 + n where the default action does not end it; a status lost: the stop, else exit 1)
pub const FORWARDED_STOP: i32; // SIGUSR1: what the child's build takes as SIGINT/SIGTERM
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
pub enum Error { Setup { step: Step, .. }, Cgroup { op, path, .. }, Exec { program, .. }, InvalidSpec, Privilege, Direct(String), SetupKilled { signal, seccomp }, InitExited { code }, .. }  // the init killed (seccomp: SIGSYS under a profile) or exited with no record before the command's status
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
  namespace the command makes (below). No user namespace of its own: in
  the build's, a `RUN` keeps its map (#78), and its `--network=none`
  clone brings `lo` up without the helper. Mounts exist
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
  A new `proc` or `sysfs` is used wherever it mounts: an rbind only where
  `Caps` says so, which only `probe_with_fallbacks` sets after an `EPERM`,
  for `/sys` where the user-namespace probe met it too, for `/proc` only
  on request (#78,
  [ADR-0126](../../adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md))
  and, in `ranikust`, after the peer scan. The rbind
  (`MS_BIND | MS_REC`, its source `/proc` or `/sys` opened `O_PATH`, its
  type checked before and after) gets read-only, `nosuid`, `nodev` and
  `noexec` on every mount of it with `mount_setattr(AT_RECURSIVE)`
  (`/proc`'s read-only: a `RUN` writes nothing there, not even its own
  `oom_score_adj` as a new `proc` lets it, [ADR-0132](../../adr/0132-the-proc-rbind-is-bound-read-only-so-a-run-writes-nothing-under-it.md));
  a refusal fails the `RUN`, never leaves a mount without them. The masks
  and read-only paths then go on top as for a new one. A `RUN` with a
  network namespace of its own tries a new `sysfs` first; where only the
  rbind mounts, it shows no more than a new one: each directory mount the
  rbind copied onto its root mount (its parent in the child's own
  mountinfo, read with no allocation by `child/submounts.rs`) gets an
  empty read-only tmpfs, its mount point found again below the rbind by
  descriptor (`hide_submounts`; a file mount there stays), and the network
  devices of the namespace it was mounted in are hidden
  (`hide_network_devices`): an empty read-only tmpfs over
  `devices/virtual/net`, over the directory of each device that a link of
  `class/net`, `class/ieee80211`, `class/macvtap`, `class/ipvtap` or
  `class/infiniband` names, and over those class directories, each found
  below the rbind by descriptor; its `/sys/class/net` is then empty, its
  own `lo` not shown.
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
  (`trusted_marks`: root in the initial user namespace, `trusted.delegate`
  alone; another uid, also `user.delegate` on a `D` it owns, ADR-0135:
  `enter_build_user_namespace` records first whether ranikust's effective
  uid is the owner `/proc/sys/kernel/ostype` shows, a sysctl uid 0 of the
  initial user namespace owns, seen through every level (`root_shown_as`,
  a regular file of a procfs, mode 0444, that reads `Linux` and is no
  mount's root), else
  whether `/proc/self/uid_map` maps it to 0, `maps_to_root`; since
  rootless overlay mode reads the marks as root of the build's user
  namespace, where `trusted.*` reads as absent) or is the root of its
  cgroup namespace, on a read-write cgroupfs with
  `cgroup.kill`; an unreadable mark, or an unreadable file of a marked or
  namespace-root `D`, is a `warn`. The probe runs every step once, and a
  failure leaves the build as without cgroups.
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
  checks for one thread first) but must not fork (a process it starts
  would outlive the child), and runs with an empty panic hook and, on its
  thread, no `tracing` subscriber: the inherited ones write ranikust's
  metrics report and log (a thread the function starts must carry the
  function's empty dispatcher over, as `rnk-snapshot`'s mount thread
  does). Each child is killed at its deadline (10 s, the closure 60 s) and
  reaped within 2 s (one that closed its report but is not reaped then is
  killed, and given 2 s more); its namespaces and mounts go with it. One
  still there after is `Error::ProbeChildLeft`: the probe's files stay,
  and `ranikust` leaves their directory, until the build's directory is
  removed (or, with an anonymous store, the state directory emptied): at
  the end of the run, at once when `--mode=auto` falls back to kaniko
  mode, else at the next start (a refused set-up, a run cut short).
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
- **Decisions:** [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) (default isolation), [ADR-0004](../../adr/0004-support-every-privilege-level.md) (probe or refuse), [ADR-0010](../../adr/0010-run-network-host-or-none.md) (network), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (bare `chroot` only on request), [ADR-0018](../../adr/0018-no-proot-in-1-0.md) (no proot), [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md) (`JAVA_TOOL_OPTIONS`), [ADR-0101](../../adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md) (the sealed copy of the binary), [ADR-0110](../../adr/0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md) (`RUN` cgroups where delegated), [ADR-0135](../../adr/0135-run-cgroups-trust-user-delegate-where-ranikusts-uid-in-the-initial-user-namespace-is-not-0.md) (`user.delegate` where ranikust's uid in the initial user namespace is not 0), [ADR-0098](../../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md) (planned, Phase 3: images with an owner the user namespace does not map are refused), [ADR-0103](../../adr/0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md) (the default seccomp profile), [ADR-0109](../../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md) (profiles compiled here, moby's default), [ADR-0104](../../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md) (`--userns` runs the whole build in a user namespace; the init's uid unmapped in rootless mode too), [ADR-0100](../../adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md) (planned, Phase 3: where the probe is tested)
- **Open:** [#3](https://github.com/tran4774/ranikust/issues/3) (the type that carries `Caps`), [#26](https://github.com/tran4774/ranikust/issues/26) (wiping secrets from memory); tasks [#34](https://github.com/tran4774/ranikust/issues/34), [#35](https://github.com/tran4774/ranikust/issues/35), [#39](https://github.com/tran4774/ranikust/issues/39), [#40](https://github.com/tran4774/ranikust/issues/40), [#88](https://github.com/tran4774/ranikust/issues/88) (bind targets against changes during set-up), [#89](https://github.com/tran4774/ranikust/issues/89) (host-safety attack suite across modes). Decided, not built yet: [ADR-0121](../../adr/0121-a-runs-bind-mounts-read-one-filtered-copy-of-the-context-copied-in-on-slash.md) (a placed secret file whose target exists is refused)
- **Evidence:** [exp-02](../../research/experiments/02-run-sandbox.md) (sequence, escape, PDEATHSIG, bounding set, stubs, 1-2.5 ms), [exp-06a](../../research/experiments/06a-root-rerun.md) (real root), [exp-13](../../research/experiments/13-docker-rootful-vm.md) (Docker probe matrix), [exp-12a](../../research/experiments/12a-fake-proc.md), [exp-14](../../research/experiments/14-real-builds-without-proc.md), [exp-15](../../research/experiments/15-java-next-concurrency.md), [exp-16](../../research/experiments/16-cgroup-v1-cpuset.md) (fake `/proc`, Java)
- **kaniko:** `kaniko@44aa7ddc0 pkg/commands/run.go:288` `runCommandInExec` (no namespaces; `:335` searches the process's `PATH`); `:356` and `:407` process-group kill (replaced by the PID namespace, kept for `Direct`); `:375` users from the process's `/etc/passwd`; `:83-135` cache mounts via `swapDir` (a bind here)
- **External:** BuildKit `executor/stubs.go` <https://github.com/moby/buildkit>, runc `--no-pivot`, `libcontainer` capabilities and `CloneBinary` (the sealed copy; `libcontainer/dmz` in v1.2.0, `libcontainer/exeseal` since v1.3.0) <https://github.com/opencontainers/runc>

## Tests

Tiers and commands: [`docs/testing.md`](../../testing.md#tiers).

- **T0** (`cargo test -p rnk-sandbox`): the cgroup decisions and parsers
  (`src/cgroup/tests.rs`: which trees count as delegated, membership,
  controllers, `cgroup.events`, the probe child's report, absent files and
  delegation marks and which a root or other ranikust trusts, whether
  ranikust is the initial user namespace's root by the owner a
  root-owned sysctl shows, else by the map, the sysctl taken only as a
  procfs file of mode 0444 that reads `Linux` and is no mount's root
  (`root_is_the_uid_a_root_owned_sysctl_shows_as_its_owner`), the single
  layout's reason, a taken `run-` name, a `RUN`'s cgroup that cannot be
  made (its step, its path, nothing left), the limits written, the removal
  of every cgroup below a `RUN`'s, a tree at the kernel's limits removed in
  full, its bounds, the cgroup a failed removal names, and a walk that
  enters no link or mount,
  detection on this host, which only reads); spec validation, mount plan
  order (a new `proc` and `sysfs` or their rbinds), the probe's next
  attempt (an rbind only after an allowed `EPERM`; `/proc`'s only on
  request, `/sys`'s without one), `setgroups` (skipped only where
  denied in a one-id map) and the tty gid read from `/proc/self`
  (`src/userns/tests.rs`),
  environment defaults, `PATH` candidates, `/etc/hosts`, capability lists,
  failure records, what the parent outside the build's user namespace
  does once its child ended (`supervise.rs`: a stop signal wins over the
  fallback, which needs its own exit status and a reason, the status alone
  an error's, a reason with an error's status none, else the child's end;
  a status lost: the stop, else an error's, the reason logged,
  `the_parent_ends_as_the_child_unless_it_falls_back_unstopped`),
  the sealed copy (the order of the ways and the stop
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
  journals only, a lock kept by a shared description, a lookup failing
  with `ELOOP` failing the clean-up and keeping the journal while
  `ENOTDIR` counts as gone, the older entries undone all the same), swaps
  and planted records, the journal's red-team tests (a file, FIFO, socket
  or whiteout where a directory is needed or as the target; stubs a `RUN`
  swaps for symlinks out of the root or for nodes; planted stub records
  aimed outside, naming `/` or ending in `..`), `Direct` children that fail
  before executing, mount sources and remount flags, the directories
  hidden from a `RUN` with a network namespace of its own under the rbind
  of `/sys`, in a tree laid out as sysfs
  (`the_network_directories_of_a_sysfs_are_each_hidden`), the mounts
  copied onto the rbind, chosen from mountinfo read in pieces of any size,
  relative to it, escapes undone, and each that cannot be placed failing
  (`child::submounts::tests`), `Direct` refusals
  whatever the root entry
  (`direct_is_refused_where_runs_get_an_rbind_of_proc`), the turns: under
  an rbind of `/proc` a `RUN` alone, whichever `Sandbox` runs it, the
  others side by side (`rbind_runs_run_alone_and_the_others_side_by_side`,
  on a lock of the test's own), and after a panic or a lost spawner
  thread during a turn no later one
  (`a_panic_during_a_turn_refuses_every_later_one`,
  `a_spawner_lost_during_a_turn_refuses_every_later_one`), the sealed
  heredoc memfd, the relay (a surviving write end, a full stream, a `RUN`
  reading its own stream back), the init's status records,
  `red_team_hostile_mount_point_names_are_matched_exactly` (the submount
  fallback where `mount_setattr` is missing, in `src/spawner.rs`), the spans
  (`tests/spans.rs`). Seccomp (`src/seccomp/`): the assembler's long jumps
  and its count past 4096 instructions, where it keeps nothing more;
  compiled profiles run on an interpreter (64-bit compares, an action per
  rule, long jumps, moby's profile against a table of calls for x86_64 and
  aarch64, `includes`/`excludes`, refused profiles, errno 0, every errno
  from 512 to 600, 4095 and above included, which ones refuse `keyctl`,
  `add_key` and `request_key`; random profiles against a reference, x32
  numbers, keys given twice, nesting and odd bytes, numbers in other forms,
  rules that disagree, the kernel's 4096 instructions, `names` read as a
  list of strings); a 1 MiB profile of 5.5 million instructions refused
  within 8 MiB (`tests/seccomp_memory.rs`, a counting allocator);
  `errno_0_reads_as_eproto`; a killed init's message, its signal and,
  for `SIGSYS` under a profile, the profile, and an init's own exit code
  named as the init's (`src/error.rs`); how a `RUN` ends
  (`a_runs_end_is_its_status_its_record_or_its_inits_end` in
  `src/spawner.rs`: its status, a failure record, a killed init winning
  over an init's record and losing to any other, an init's own code
  losing to every record and never passed off as the command's). Moby's
  profile's argument-rule form
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
  control; the parent check's read refused or faked failing with its errno
  or `EINVAL`, another parent with `ESRCH`; the command's go only on its
  init's go byte, never on an ended pipe, another byte or a read faked with
  no error, and the init's go byte
  sent, or the pipe ended without it, or the init ended where neither can
  be done; the init's closing: a refused `close` reported with its errno,
  `EBADF` and `EINTR` included (also run for the musl target), whether
  there are descriptors to close (at most the pass's listing left open) or
  only the listing; a pass that does not shrink ends it; nothing left where
  `close_range` alone is refused; a listing that cannot be read reported
  with its own errno, not `close_range`'s `ENOSYS`; the best-effort closing
  before the filters going on past a refused `close`, one descriptor
  numbered past the hard limit closed too; a refused `umask` after the
  profile; a faked `setsid`, `pipe2` or `fstatfs`, a faked or refused
  `prlimit64`; the last two on a stack filled beforehand, which shows an
  unwritten buffer in a debug build only; these
  test the helpers, not their call sites, whose `/proc/self/fd` opening
  overwrites that stack).
  `tests/direct_only.rs`: a `start_direct` sandbox
  refuses `Direct` off `/` (symlink to `/`, `//`, `/.`, `..` roots) and places
  nothing.
- **T0**, the user-namespace probe (`src/userns/tests.rs`): the maps
  planned for root and for another user, a shifted range tried alone and
  never one id (`a_shifted_build_tries_its_range_only_and_never_one_id`),
  the mirror of a map, the steps
  assembled from a child's records (a refused `unshare`, each map refused,
  a fresh kernel filesystem or its rbind, Ubuntu 24.04's ranged stop at
  `chown`), records across split reads and the deadline, the report's line
  and stop (`stop(allowed)`: an rbind of `/proc` passes only where allowed,
  one of `/sys` always), a map this process fails to write ending the
  waiting child at
  once, a failure part-way through making the probe's files removing what
  it made, `in_namespace` not running its closure beside other threads, a
  panic in the closure reported as not finishing, never as its refusal, nor
  through the parent's panic hook, and a log line of the closure not
  through its subscriber (in a copy of the test binary with one test
  thread); a child holding only stdio and its own descriptors; a child
  reaped within its grace, else killed, else reported left; a tool reaped
  elsewhere (`ECHILD`) not killed. Its red team (`red_team_*`, as any
  user; T22, T4, T7, T8, T9): names planted where it makes its files (a
  symlink out, a hard link of a file outside, a FIFO; the directory itself
  a symlink or a FIFO) never followed, opened or removed; nothing left
  after a probe and a closure that mounts (no process, orphan included, no
  descriptor, mount or entry); the prober killed while a child of the
  probe runs (`SIGKILL`, `SIGINT`, `SIGINT` to its group) leaves no
  process or mount.
- **T0**, the build's user namespace (`src/userns/build/tests.rs`): a
  trial passes only with every step and its end record; a refused `unshare`
  ends the trials; a trial stops at the first step refused, or says how it
  was cut short; the line names the map and what was passed over; the map
  helper reports a refused map and ends, and ends without writing when
  cancelled; refused beside other threads; the namespace is told as read
  back (a map giving one id is one uid, a parent's `deny` stands); a
  shifted range refused to a user other than root before any change. Its red team
  (`red_team_the_build_user_namespace_is_whole_or_not_entered`, as any user,
  with and without this user's ranges): the process either has the whole
  namespace (uid and gid maps, `setgroups`, euid 0, new user and mount
  namespaces, not dumpable) or kept its own, unchanged.
- **T1** (`just smoke`, rootless podman): the whole sequence in a user
  namespace with `pivot_root` and `RUN`s that check the result from inside
  (PID 2 under `ranikust-init`; the init under the command's filters; host
  name; capability sets `a80425fb` with empty inheritable and ambient;
  `nodev` root and `/dev`; read-only `/proc/sys` and `/dev/null`; masked
  kernel files; no terminal; only stdio descriptors; no stub in the
  layer); `sealed-exe`: a `RUN` reaches no thread of ranikust's or its
  init's (ADR-0101). `kaniko-flags` in the kaniko leg: the user-namespace
  probe stops at `proc` under rootless Podman's defaults, whose rbind needs
  `--allow proc.rbind` (#74, #79); `userns-probe`, its red team at the
  binary's level, and `rootless`, rootless overlay mode end to end
  ([their steps](../../testing.md#quick-smoke-cases)). `just test-rootless`
  (profile `default`):
  `the_build_user_namespace_of_root_maps_every_id_it_has`
  (`src/userns/build/tests.rs`): root of a rootless Podman container enters
  with its ranges mirrored and `setgroups` allowed;
  `the_build_user_namespace_below_one_id_is_told_one_uid`: root of a one-id
  namespace, `setgroups` denied (as `unshare -r`), enters with its map
  mirrored and is told one uid, `setgroups` denied;
  `root_of_rootless_podman_is_not_the_kernel_root_at_any_depth`: root of
  the container, and root of a namespace below it mapped 0 to 0 or
  through another uid, is not the initial namespace's root, where the map
  one level up calls the 0 to 0 one root. Profile `privileged`:
  `red_team_a_mount_in_the_build_s_namespace_never_shows_outside` (a tmpfs
  mounted in the build's namespaces below a shared mount of the parent
  never shows there, where a plain mount namespace's does);
  `red_team_namespace_limits_leave_the_build_whole_or_unchanged` (in a user
  namespace of the test's own, `user.max_user_namespaces` and
  `user.max_mnt_namespaces` at 0 and 1: entered whole, or unchanged by
  an error other than `UserNamespaceBroken`; prints how often each).
  `runs_in_the_build_user_namespace_get_the_fallbacks_the_probe_chose`
  (`src/userns/build/run_tests.rs`): root of rootless Podman, after both
  probes, gets an rbind of `/proc` (asked for) and of `/sys` with every mount
  `nosuid`, `nodev`, `noexec` and read-only, `/proc/kcore` masked: a write
  of its own `oom_score_adj`, or of that of a process outside the build of
  its kernel uid, fails with `EROFS`, one through `/dev/stderr` works
  (without the read-only flag, both writes succeed); `USER _apt` gets
  42:65534 through `setgroups`; `--network=none` has `lo` up in a network
  namespace of its own, and its `/sys` shows no network device or
  address, and no mount below it lists anything (Podman's cgroup2 at
  `/sys/fs/cgroup`, which the shared network's `RUN` sees, is an empty
  tmpfs there), its new `sysfs` refused below Podman's masks;
  `runs_below_one_id_skip_setgroups_and_the_tty_gid`: the same below a
  one-id namespace, `setgroups` skipped, `devpts` without a `gid=`, `USER
  _apt` refused at `setgid`; `an_rbind_of_proc_needs_a_request`: unasked,
  the sandbox's probe there refuses at `Step::MountProc` and no `RUN`
  starts; `runs_take_turns_under_an_rbind_of_proc`: two `RUN`s started
  together never overlap, and the second finds no process of the first
  (a search that finds the `RUN`'s own, the control); profile
  `privileged`, `runs_with_a_new_proc_run_side_by_side`: with a new
  `proc` they overlap;
  `a_run_under_an_rbind_of_proc_opens_nothing_of_a_process_outside_the_build`,
  `a_run_under_an_rbind_of_proc_opens_nothing_of_a_process_in_another_user_namespace`
  (`default`, each file refused with `EACCES`) and
  `a_run_with_a_new_proc_opens_nothing_of_a_process_outside_the_build`
  (`privileged`, none found): a `RUN` opens nothing of a process outside
  the build.
  Their red team (`src/userns/build/run_tests/red_team.rs`,
  profile `default`, Perl attacks that open for writing and never write):
  `red_team_a_run_writes_nothing_under_an_rbind_of_proc` (under
  `--seccomp=unconfined`, a root `RUN` opens for writing no file of its own
  or of a process outside the build below `/proc` (`EROFS`), no sysctl,
  `sysrq-trigger` or `/sys` file, and makes no mount below `/proc` or
  `/sys` writable by a remount of the mount or its superblock,
  `mount_setattr` or `open_tree` and `move_mount`, in its
  namespaces or in a user and mount namespace of its own (`EPERM`: the
  copies are locked), nor mounts a new `proc` in a PID namespace of its
  own; a tmpfs of its own takes each call, the control);
  `red_team_a_run_reads_no_gated_proc_file_of_a_process_outside_the_build`
  (as root and as `_apt`, the ptrace-gated files, magic links and
  `map_files` of a `sleep` outside the build of the same kernel uid, the
  test, PID 1 and the process holding the `Sandbox` are refused, a
  credential they hold or sit in is not reached, and `pidfd_send_signal`
  through their `/proc` directories fails with `EINVAL`; their command
  lines are seen, ADR-0130's residual);
  `red_team_runs_of_two_builds_reach_no_gated_proc_file_of_each_other` (the
  same between the `RUN`s of two builds at once, a secret mount included);
  `red_team_a_cancelled_run_leaves_no_process_for_the_next_turn` (a `RUN`
  that leaves processes behind four ways is cancelled mid-turn: two jobs of
  another `Sandbox` wait for its turn, run one at a time, find none of
  them, and nothing outlives any `RUN`). `just smoke` also runs `smoke-build --userns` in a default
  `podman run`: refused at the new `proc` without `--allow proc.rbind`,
  and with it the checks of `smoke_checks` that an rbind of `/proc` and
  Podman's seccomp (no `sethostname`) leave.
- **T2** (VM only, `#[ignore = "T2: ..."]`, `just test-vm unit`):
  `probe_reaches_exec_as_root` and `a_run_joins_a_held_network_namespace`
  (`src/probe.rs`);
  `the_kernel_root_is_told_through_nested_user_namespaces`
  (`src/userns/build/tests.rs`): real root is root on the host, one level
  down mapped 0 to 0, and two levels down through uid 5, where the map one
  level up misses it; the sudo user is not, on the host or as root two
  levels down (where AppArmor restricts user namespaces, Ubuntu 24.04,
  the user's namespace gets no map);
  `the_user_namespace_steps_as_a_regular_user_stop_where_apparmor_restricts`
  (`src/userns/tests.rs`: as the user who ran sudo, one id, then
  `newuidmap` with that user's ranges; where AppArmor restricts user
  namespaces, Ubuntu 24.04, one id stops at the `uid_map` write and ranges
  at `chown`, elsewhere both pass; both harnesses install `newuidmap`, and
  the user must have subordinate ids; prints `measured:` lines);
  `the_build_user_namespace_of_a_regular_user_maps_its_ranges_else_one_uid`
  (`src/userns/build/tests.rs`: as that user, with ranges, without them,
  and with `newuidmap` hidden; where AppArmor restricts, every leg is
  refused at the `uid_map` write and nothing changed, elsewhere the ranges
  through `newuidmap`, else one uid, named as such);
  `runs_in_the_build_user_namespace_of_a_regular_user`
  (`src/userns/build/run_tests.rs`: the T1 `RUN`s as that user; refused
  where AppArmor restricts, elsewhere, with no request, a new `proc`, PID 2
  under `ranikust-init`, where the `RUN` writes its own `oom_score_adj`,
  and an rbind of `/sys`, but a new `sysfs` showing `lo` alone for a
  `--network=none` `RUN`);
  `red_team_a_mount_in_a_root_build_s_namespace_never_shows_outside` (the
  T1 mount red team as real root, whose map is every id);
  `the_user_namespace_steps_of_a_shifted_range_pass_as_its_first_ids`
  (`src/userns/tests.rs`) and
  `the_build_user_namespace_of_a_shifted_range_makes_root_the_range_s_first_ids`
  (`src/userns/build/tests.rs`): real root with a range of uids 300000 and
  gids 400000 probes every step and enters the namespace, whose maps read
  `0 300000 65536` and `0 400000 65536` and whose root has no
  supplementary group and is those ids on the host;
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
  `a_set_up_its_profile_kills_is_reported_as_killed`: a default action of
  `SCMP_ACT_KILL_PROCESS` or `SCMP_ACT_TRAP` fails the `RUN` as
  `Error::SetupKilled` (the init killed) by `SIGSYS` under its profile;
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
  profile and under `--seccomp=unconfined`, one filter apart. The VM's
  regular user runs the example with `--userns` and the same `--run`s
  (#78): refused where AppArmor restricts user namespaces (Ubuntu 24.04),
  elsewhere (22.04) a new `proc`, an rbind of `/sys`, and an image Docker
  runs.
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
  (`red_team_a_block_device_in_a_mount_target_is_enotdir_or_the_target_itself`);
  `red_team_a_uid_0_run_writes_nothing_under_rbinds_of_proc_and_sys`
  (`src/userns/build/run_tests/red_team.rs`, profile `sysadmin`: in real
  root's build user namespace a root `RUN` is kernel uid 0; under
  `--seccomp=unconfined`, with an rbind of `/sys`, and of `/proc` where a
  new one is refused, every mount below them is read-only, and the `RUN`
  opens no sysctl, `sysrq-trigger` or `/sys` file for writing and makes
  none of them writable, in its namespaces or new ones; where Ubuntu
  24.04's AppArmor leaves a new user namespace no capability, as on the
  test machine and the noble VM, the attacks in new ones stop there, and
  the jammy VM shows their locks).

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
