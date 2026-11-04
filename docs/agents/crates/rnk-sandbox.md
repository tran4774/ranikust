# rnk-sandbox

> **Status:** in progress · **Phase:** 1-2 (`Direct` from 2; rootless: 3) ·
> **Depends on:** `rnk-rootfs` (`RootPath`, `USER` lookup) ·
> **Used by:** `rnk-builder`; `ranikust` (probes, process set-up) ·
> **Path:** `crates/rnk-sandbox/`

**TL;DR:** Runs each `RUN` in fresh namespaces inside the stage root, entered
with `pivot_root` (or `MS_MOVE`), with the capability bounding set cut to
Docker's defaults, and probes its own capabilities. Never a bare `chroot`
where mounting is possible; no `RUN` process outlives the build. User
namespaces, the `proc`/`sysfs` fallbacks, `--seccomp` and `--userns` are
Phase 3 ([#41](https://github.com/tran4774/ranikust/issues/41)).

## Owns

- The sealed copy of ranikust's binary a build runs from (`seal.rs`,
  ADR-0101): a read-only overlay of the binary's directory mounted nowhere,
  where `enter_build_namespace`'s steps succeed in a child, else a sealed
  memfd, else an `O_TMPFILE` copy, run with `execve`; the copy's run,
  marked by `RANIKUST_SEALED_COPY`, checks it and never makes another.
- Process set-up, called by `ranikust` while single-threaded: private mount
  namespace, spawner thread, stub-journal replay; and the probe of its own
  steps, which `ranikust` runs with `rnk-snapshot`'s. Today the probe runs
  the `Full` per-`RUN` sequence in an empty root (`probe.rs`) and reports
  only `root_entry`; the full probe and its guidance (uid map, `sysfs`, bind
  mounts, `mknod`) are Phase 3.
- The per-`RUN` sandbox: namespaces, mounts, `pivot_root`, capability drop,
  the `USER` ids `rnk-rootfs` resolved in the root, stubs, `exec`.
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
  the seal's overlay of ranikust's own binary, never attached, is the one
  exception); pick the mode (`ranikust`).
- Use a bare `chroot` where mounting is possible; `unshare` once threads
  exist; clone a `RUN` anywhere but on the spawner thread.
- Allocate, lock, format or panic between `clone` and `execve`.
- Bind a source below a directory a `RUN` can write (a cache `source=`
  subpath, Phase 5) by path: resolve it from that directory through
  `RootPath` first. `open_source` refuses only a final symlink.

## API

```rust
pub fn run_from_sealed_copy(ways: &[SealedCopy]) -> rustix::io::Result<Sealing>; // first, single-threaded (ADR-0101): execve from the first copy made; returns in the copy's run (Sealed, checked) or when none ran (Unsealed(why)); Err only when RLIMIT_CORE cannot go back to 0
pub enum SealedCopy { Overlay, Memfd, Tmpfile }  // ALL: the order tried
pub enum Sealing { Sealed { way: SealedCopy, passed_over: String }, Unsealed(String) } // passed_over: why the ways before failed
pub fn enter_build_namespace() -> Result<()>;   // unshare(CLONE_NEWNS) + rslave; refuses with other threads, and in a plain chroot (Error::InPlainChroot)
pub fn trim_to_run_capabilities() -> Result<()>; // kaniko mode on /, before start_direct (ADR-0078): cuts ranikust's own sets to what a Direct RUN keeps, dumpable 0 again, read back; no-op when not root; refuses with other threads (Error::NotSingleThreaded), or (Error::RunReachesState) where the bounding set cannot be cut
pub fn probe(dir: &Path) -> Result<Caps>;       // the real sequence in an empty root: pivot_root, else MS_MOVE
pub struct Caps { pub root_entry: RootEntry }   // #[non_exhaustive]
pub enum RootEntry { PivotRoot, MoveMount }
pub struct Sandbox { /* spawner thread; journals in <state-dir>/sandbox/runs */ }
impl Sandbox {
    pub fn start(caps: Caps, dir: &Path) -> Result<Self>;   // replays journals; Full on any root, Direct on / only (ADR-0060)
    pub fn start_direct() -> Result<Self>;                   // kaniko mode on / (ADR-0051): Direct on / only; no directory or journal; the process becomes a child subreaper
    #[cfg(test)] pub(crate) fn start_with_root(..) -> Result<Self>; // the Direct chroot off /, test-only (ADR-0060)
    pub fn allow_userns_network(&mut self);                   // --allow network.none-userns (ADR-0033)
    pub fn set_nofile_limit(&mut self, limit: NofileLimit);  // every later RUN starts with it (ranikust's limit before its raise)
    pub fn run(&self, spec: RunSpec) -> Result<ExitStatus>;  // blocking; cloned on the spawner thread
    pub fn cancel(&self);                                     // SIGKILL every running RUN; later runs: Error::Cancelled
}
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
pub enum Error { Setup { step: Step, .. }, Exec { program, .. }, InvalidSpec, Privilege, Direct(String), .. }
impl Error { pub fn ran_no_command(&self) -> bool; } // before execve: no RUN process can outlive it (ADR-0056); false for Cancelled
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
  (its init; a `Direct` one's process group too), then every later `run`
  returns `Error::Cancelled`. A child is forgotten only after
  `waitid(WNOWAIT)` saw it exit and before `wait4` reaps it: the kill never
  reaches a reused pid.
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
  `probe_network`, `NetNs`, ADR-0033). User namespace: Phase 3. Mounts exist
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
- **Capabilities, for every `USER`:** bounding set cut to Docker's 14
  defaults, ambient cleared, effective/permitted the defaults the process has,
  inheritable empty. Then `setsid`, a new session keyring (root's user and
  persistent keyrings stay reachable until a seccomp profile applies, planned,
  ADR-0103), `setgroups`, `setresgid`, `setresuid`; `WORKDIR` is resolved in
  the root without magic links, entered with `fchdir`, and `getcwd` must show it
  below the root (runc, CVE-2024-21626).
- **Only stdio crosses `execve`** (T7): every other descriptor becomes
  close-on-exec (`close_range`, or the `/proc/self/fd` list before 5.11);
  umask 022, `SIGPIPE` default, empty signal mask. `PATH` is searched after
  entering the root and switching user; a candidate through a magic link is
  refused (`ELOOP`). Not covered: a `#!/proc/self/exe` line or a symlink
  swapped after the check run ranikust's binary in the sandbox, which refuses
  its arguments; that binary is the sealed copy (ADR-0101).
- **Sealed copy** (ADR-0101): every process ranikust forks, the init
  included, runs the copy, so no `RUN` reaches a binary it could write. The
  overlay copy runs the installed file's pages: it is made only where the
  build's own namespace steps succeed (in a child), never for kaniko mode
  on `/`; it must have the running binary's inode number, size and time,
  its mount must be attached nowhere, and its empty layer, in `TMPDIR`, is
  removed once mounted; the memfd is sealed
  `SEAL|SHRINK|GROW|WRITE`, made with `MFD_EXEC` where the kernel knows it;
  the `O_TMPFILE` copy is `O_EXCL` and run read-only, and only `ETXTBSY`
  guards it. The copy gets back the core-file limit ranikust started with.
  When no way works the build goes on, warned.
- **`Direct`:** `start_direct` refuses to start from an overlay copy of
  the binary (ADR-0101; the copy's gate keeps it unreachable). No
  namespace, mount or capability change on `/`; the command
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
- **Limits:** every `RUN` gets `Sandbox::set_nofile_limit`'s
  `RLIMIT_NOFILE` right before `execve` (`Step::FileLimit`; unset, ranikust's)
  and `forbid_core_dumps`'s `RLIMIT_CORE` (`Step::CoreLimit`).
- **The probe** performs the real sequence in the configuration that will be
  used, never by kernel version; a command that does not exist fails at
  `execve` with `ENOENT`, which proves every step before it.

## Refs

- **Design:** [`design/sandbox.md`](../../design/sandbox.md) ([per-`RUN` sequence](../../design/sandbox.md#per-run-sequence), [process set-up](../../design/sandbox.md#process-set-up-once-per-build), [stub mount points](../../design/sandbox.md#stub-mount-points), [variants by environment](../../design/sandbox.md#variants-by-environment)); [`design/modes-and-detection.md#probe-order---modeauto`](../../design/modes-and-detection.md#probe-order---modeauto); [`design/kaniko-mode.md#fake-proc`](../../design/kaniko-mode.md#fake-proc) (not built); [threat model T5-T12, T17-T19](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) (default isolation), [ADR-0004](../../adr/0004-support-every-privilege-level.md) (probe or refuse), [ADR-0010](../../adr/0010-run-network-host-or-none.md) (network), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (bare `chroot` only on request), [ADR-0018](../../adr/0018-no-proot-in-1-0.md) (no proot), [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md) (`JAVA_TOOL_OPTIONS`), [ADR-0101](../../adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md) (the sealed copy of the binary), [ADR-0098](../../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md) (planned, Phase 3: images with an owner the user namespace does not map are refused), [ADR-0103](../../adr/0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md) (planned, Phase 3: the default seccomp profile), [ADR-0104](../../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md) (planned, Phase 3: `--userns` runs the whole build in a user namespace; the init's uid unmapped in rootless mode too), [ADR-0100](../../adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md) (planned, Phase 3: where the probe is tested)
- **Open:** [#3](https://github.com/tran4774/ranikust/issues/3) (the type that carries `Caps`), [#26](https://github.com/tran4774/ranikust/issues/26) (wiping secrets from memory), [#31](https://github.com/tran4774/ranikust/issues/31) (placed secret file whose target exists); tasks [#33](https://github.com/tran4774/ranikust/issues/33), [#34](https://github.com/tran4774/ranikust/issues/34), [#35](https://github.com/tran4774/ranikust/issues/35), [#39](https://github.com/tran4774/ranikust/issues/39), [#40](https://github.com/tran4774/ranikust/issues/40), [#82](https://github.com/tran4774/ranikust/issues/82) (`--seccomp`), [#83](https://github.com/tran4774/ranikust/issues/83) (`--userns`), [#88](https://github.com/tran4774/ranikust/issues/88) (bind targets against changes during set-up), [#89](https://github.com/tran4774/ranikust/issues/89) (host-safety attack suite across modes)
- **Evidence:** [exp-02](../../research/experiments/02-run-sandbox.md) (sequence, escape, PDEATHSIG, bounding set, stubs, 1-2.5 ms), [exp-06a](../../research/experiments/06a-root-rerun.md) (real root), [exp-13](../../research/experiments/13-docker-rootful-vm.md) (Docker probe matrix), [exp-12a](../../research/experiments/12a-fake-proc.md), [exp-14](../../research/experiments/14-real-builds-without-proc.md), [exp-15](../../research/experiments/15-java-next-concurrency.md), [exp-16](../../research/experiments/16-cgroup-v1-cpuset.md) (fake `/proc`, Java)
- **kaniko:** `kaniko@44aa7ddc0 pkg/commands/run.go:288` `runCommandInExec` (no namespaces; `:335` searches the process's `PATH`); `:356` and `:407` process-group kill (replaced by the PID namespace, kept for `Direct`); `:375` users from the process's `/etc/passwd`; `:83-135` cache mounts via `swapDir` (a bind here)
- **External:** BuildKit `executor/stubs.go` <https://github.com/moby/buildkit>, runc `--no-pivot`, `libcontainer` capabilities and `libcontainer/exeseal` (the sealed copy) <https://github.com/opencontainers/runc>

## Tests

Tiers and commands: [`docs/testing.md`](../../testing.md#tiers).

- **T0** (`cargo test -p rnk-sandbox`): spec validation, mount plan order,
  environment defaults, `PATH` candidates, `/etc/hosts`, capability lists,
  failure records, the sealed copy (the order of the ways and the stop
  when the core-file limit cannot go back, one marker in the copy's
  environment, no overlay where a child's namespace steps fail, a memfd
  sealed against writes, an `O_TMPFILE` copy with no name, open
  read-only), the stub journal (placement through symlinks, removal,
  parent times, replay of dead journals only, a lock kept by a shared
  description), swaps and planted records, `Direct` children that fail before
  executing, mount sources and remount flags, `Direct` refusals, the sealed
  heredoc memfd, the relay (a surviving write end, a full stream, a `RUN`
  reading its own stream back), the init's status records,
  `red_team_hostile_mount_point_names_are_matched_exactly` (the submount
  fallback where `mount_setattr` is missing, in `src/spawner.rs`), the spans
  (`tests/spans.rs`). `tests/direct_only.rs`: a `start_direct` sandbox
  refuses `Direct` off `/` (symlink to `/`, `//`, `/.`, `..` roots) and places
  nothing.
- **T1** (`just smoke`, rootless podman): the whole sequence in a user
  namespace with `pivot_root` and `RUN`s that check the result from inside
  (PID 2 under `ranikust-init`, host name, capability sets `a80425fb` with
  empty inheritable and ambient, `nodev` root and `/dev`, read-only
  `/proc/sys` and `/dev/null`, masked kernel files, no terminal, only stdio
  descriptors, no stub in the layer). Not yet: the fallbacks.
- **T2** (VM only, `#[ignore = "T2: ..."]`, `just test-vm unit`):
  `probe_reaches_exec_as_root` and `a_run_joins_a_held_network_namespace`
  (`src/probe.rs`); `an_overlay_copy_is_the_binary_read_only_and_mounted_nowhere`
  (`src/seal/tests.rs`); `a_device_node_planted_as_a_journal_is_never_opened`
  (`src/journal.rs`); in `src/spawner.rs` the `Full` tests (`nodev` below the
  root; descriptors, symlinked `/proc`, a cache over `/dev`, symlinked mount
  targets, every mount holding only stdio, `/dev` nodes unchangeable;
  `the_run_is_pid_2_under_an_init_that_reports_its_status`;
  `red_team_no_run_outlives_the_thread_that_cloned_it`;
  `red_team_a_run_can_slow_its_inits_death_but_not_stop_it`, which prints the
  delays, threat model section 6; `a_running_binary_cannot_be_opened_for_writing`),
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
  overlay mode, kaniko mode per stage and with the overlay skipped while
  `tests/vm/guest/sealed-exe.py` checks that ranikust and its init run one
  sealed copy root cannot write (ADR-0101). The quick suite's kaniko mode
  checks a build on a container's `/` runs from a sealed memfd, in a
  default container and with `--cap-add SYS_ADMIN` (AppArmor denies
  `mount`).
- **T2c** (rootful Docker, `just test-container unit-c`): the `Direct` `RUN`
  on `/` in `tests/direct_attacks.rs` (heredoc memfd cannot be rewritten, only
  own descriptors, stolen or read-back output cannot hang or stall, pid
  hopping caught by the sweep, non-blocking flood) and `src/spawner.rs`
  (`a_cancel_kills_a_running_run`, `a_direct_run_leaves_no_process_behind`,
  `a_direct_heredoc_runs_from_an_anonymous_file`, loopback for
  `--network=none`).

## Metrics

- `rnk_sandbox.run` per `RUN`: `wait4` rusage (`user_ns`, `sys_ns`,
  `max_rss_kb`, faults, blocks, context switches) when the child was reaped
  after `execve`, and `failed`; `rnk_sandbox.setup` (`stubs`);
  `rnk_sandbox.probe` (`ok`) per attempt
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
- Closing a `flock`ed file does not release the lock while a child cloned by
  another thread still shares its description (until `execve` or exit): a
  replay then saw a dead journal as live (a flaky T0 test). Unlock
  explicitly.
- A `Direct` `RUN` in a `chroot` has no `/proc`; where mounting works, run
  that root with `Isolation::Full` instead.
