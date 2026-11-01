# rnk-sandbox

> **Status:** Phase 1 implemented, untested as root (T2 VM harness
> missing) · **Phase:** 1 ·
> **Depends on:** `rnk-rootfs` (`RootPath`, `USER` lookup) · **Used by:** `rnk-builder`, `ranikust` (probes, process set-up) · **Path:** `crates/rnk-sandbox/`

**TL;DR:** Runs each `RUN` in fresh namespaces inside the stage root, entered
with `pivot_root` (or `MS_MOVE`), with the capability bounding set cut to
Docker's defaults, and probes its own capabilities. Never a bare `chroot`
where mounting is possible; no `RUN` process outlives the build.

## Owns

- Process set-up called by `ranikust` while single-threaded (private mount
  namespace, spawner thread, stub-journal replay), and the probes of its own
  steps: namespaces, uid map, `proc`, `pivot_root`/`MS_MOVE`, `sysfs`,
  bind mounts, `mknod`. `ranikust` runs them, with `rnk-snapshot`'s.
- Per-`RUN` sandbox: namespaces, mounts, `pivot_root`, capability drop, the
  `USER` ids `rnk-rootfs` resolved in the root, stubs, `exec`; and
  `Isolation::Direct`: sequential kaniko-mode `RUN` on `/`, or in a
  `chroot` of a root other than `/` (a root in the state directory, ADR-0050) with journaled placed files (`ADR-0043`).
- The no-mount `--parallel-stages` path (bare `chroot`, `mknod` `/dev`
  nodes, fake `/proc` and `/sys`, `JAVA_TOOL_OPTIONS`): not built, the flag
  is refused (ADR-0060).

## Must not

- Look up users on the host or parse `/etc/passwd` itself (`rnk-rootfs`);
  snapshot, diff or mount stage roots, or probe overlays (`rnk-snapshot`).
- Use a bare `chroot` where mounting is possible; `unshare` once threads
  exist; clone a `RUN` anywhere but on the spawner thread; pick the mode
  (`ranikust`).
- Allocate, lock, format or panic between `clone` and `execve`.
- Bind a source below a directory a `RUN` can write (a cache `source=`
  subpath, Phase 5) by path: resolve it from that directory through
  `RootPath` first. `open_source` refuses only a final symlink.

## API

```rust
pub fn enter_build_namespace() -> Result<()>;   // unshare(CLONE_NEWNS) + rslave; refuses with other threads, and in a plain chroot (Error::InPlainChroot)
pub fn probe(dir: &Path) -> Result<Caps>;       // the real sequence in an empty root: pivot_root, else MS_MOVE
pub struct Caps { pub root_entry: RootEntry }   // #[non_exhaustive]; what the probe found
pub enum RootEntry { PivotRoot, MoveMount }
pub struct Sandbox { /* spawner thread; journals in <state-dir>/sandbox/runs */ }
impl Sandbox {
    pub fn start(caps: Option<Caps>, dir: &Path) -> Result<Self>; // replays journals; None: Direct only
    pub fn start_direct() -> Result<Self>;                         // kaniko mode on / (ADR-0051): Direct on / only, no directory or journal, process made a child subreaper; after each RUN its process tree is killed through pidfds (parent and start time checked again; where seccomp refuses pidfd_open, a plain kill for ranikust's own children only, deeper ones on later rounds) and reaped by pid with WNOHANG; the child drops caps outside Docker's defaults from its bounding set (CAP_SETPCAP), or the RUN is refused (Error::RunReachesState) if any such cap would stay; ambient and inheritable cleared, permitted/effective cut to Docker's defaults; warns when Yama ptrace_scope is 0; refuses at start (Error::RunReachesState) where that drop is impossible (ADR-0057)
    pub fn start_with_root(caps, dir, shared_root: &Path, checked: &RootPath) -> Result<Self>; // the path must be `checked`'s directory; replays journals of that root, by identity; refuses the host's /, /etc, /dev; a later RUN on another path, or whose reopened root differs (dev, ino, mnt_id), is refused
    pub fn set_nofile_limit(&mut self, limit: NofileLimit);       // every later RUN starts with it (ranikust's limit before its raise)
    pub fn run(&self, spec: RunSpec) -> Result<ExitStatus>;       // blocking; cloned on the spawner thread
    pub fn cancel(&self);                                         // SIGKILL every running RUN; later runs: Error::Cancelled
}
pub fn forbid_core_dumps() -> rustix::io::Result<()>;  // first in main (ADR-0051): soft RLIMIT_CORE 0, hard kept; every later RUN gets the recorded soft limit back (Step::CoreLimit)
pub struct RunSpec { /* root_dir, argv, env, user, workdir, network, hostname, mounts, stdin/out/err, isolation, script, root_id */ }  // #[non_exhaustive]; RunSpec::new; script: a #! heredoc body, Direct on / only: sealed memfd run with execveat (ADR-0057); root_id via RunSpec::expect_root(&RootPath): the reopened root must be that (dev, ino, mnt) in the parent, (dev, ino) in the child after the bind (ADR-0059)
pub struct BindMount { pub source, pub target, pub kind: MountKind, pub read_only }  // RUN --mount=type=cache
pub struct SecretFile { pub data: Vec<u8>, pub target, pub uid, pub gid, pub mode }  // RunSpec::secrets: written
    // by the child on its own tmpfs, bound read-only; Debug hides the bytes, as RunSpec's the env values
pub enum Stdio { Inherit, Null, Fd(OwnedFd) }
pub enum Network { Host, None }                 // [decided] ADR-0010
pub enum Isolation { Full, Direct }             // #[non_exhaustive]; BareChroot { fake_proc } [design], Phase 2
pub struct NofileLimit { pub soft: Option<u64>, pub hard: Option<u64> } // NofileLimit::current(); None = unlimited
pub enum Error { Setup { step: Step, .. }, Exec { program, .. }, InvalidSpec, Privilege, Direct(String), .. }  // Direct: a Direct RUN asked for a mount, a secret file, --network=none, or a script it cannot run (ADR-0057)
impl Error { pub fn ran_no_command(&self) -> bool; } // before execve: no RUN process can outlive it (ADR-0056); false for Cancelled
```

`run` blocks: `rnk-builder` calls it from a blocking thread, holding a
`--jobs` permit; the fork happens on the spawner thread whatever thread
calls. The caller resolves `RUN --mount` into `BindMount`s (cache directory
from `rnk-store`, secret file) and passes `--network`.

## Invariants

- `unshare` only while single-threaded. One spawner thread, alive as long as
  the `Sandbox`, clones every `RUN` with a raw `clone` (no `CLONE_VM`); the
  child arms `PR_SET_PDEATHSIG(SIGKILL)`, then reads its parent pid in its
  own `stat` file of the outer procfs, opened before entering the root
  (`getppid` is 0 in a new PID namespace): another pid than ranikust's
  means it was reparented, so ranikust died before the signal was armed.
  The id switch clears the death signal (`commit_creds`), so it is armed
  and checked again after it. The child is PID 1 of its PID namespace, so
  its tree dies with it. Dropping
  the `Sandbox` ends the thread and kills every running `RUN`.
- `Sandbox::cancel` kills every running `RUN` with `SIGKILL` (its PID 1;
  a `Direct` one's process group too), as BuildKit on a cancel, and every
  later `run` returns `Error::Cancelled`. A child is forgotten only after
  `waitid(WNOWAIT)` saw it exit and before `wait4` reaps it: the kill
  never reaches a reused pid.
- Between `clone` and `execve` the child only makes system calls (rustix's
  raw backend, a few `libc` calls) on arguments built before the clone. A
  failing step writes `(step, errno)` to a close-on-exec pipe and exits
  127; end-of-file on the pipe means `execve` succeeded.
- Namespaces: mount, PID, UTS, IPC (plus user without `CAP_SYS_ADMIN`
  [design, Phase 3], network for `--network=none`, with `lo` up: the
  clone's own; where `CAP_NET_ADMIN` is missing, refused, or with
  `Sandbox::allow_userns_network` (`--allow network.none-userns`) one a
  helper creates in a user namespace of its own and hands over as an nsfs
  descriptor, which the child joins and runs under the `seccomp` module's
  denylist filter; probed once, ADR-0033, `NetNs`, `probe_network`). Mounts
  exist only in the child, so the parent diffs the root as the image left
  it.
- Mount order: all mounts private, self-bind remounted `nodev`, `proc`,
  read-only `sysfs`, `nodev` tmpfs `/dev` (nodes bound read-only from the
  outer `/dev`, `devpts`, `shm`, symlinks), writable copies of
  `resolv.conf`, `hosts`, `hostname`, `RUN --mount` remounted `nodev`, then
  Docker's read-only `/proc` paths and masked `/proc` and `/sys` paths. Every
  target is resolved with `openat2(RESOLVE_IN_ROOT)` from the bound root and
  mounted through `/proc/self/fd/<n>`; sources are opened by the child in
  its own mount namespace (`O_PATH`, no symlink as the last component, type
  checked; a parent's descriptor would be refused); remounts
  keep the source's locked flags. The `nodev` remount covers the root's top
  mount only; a mount nested under a stage root would need its own. Then
  `pivot_root(".", ".")` and
  `umount2(MNT_DETACH)`; where seccomp refuses it with `EPERM` and `/` is a
  mount root, `MS_MOVE` + `chroot(".")`.
- Capabilities, for every `USER`: bounding set cut to Docker's 14 defaults,
  ambient cleared, effective and permitted the defaults the process has,
  inheritable empty (Docker since 20.10.14). Then `setsid` (no controlling
  terminal), a new session keyring (root's user and persistent keyrings
  stay reachable until `--seccomp`, Phase 3; Docker's profile denies
  `keyctl`), and `setgroups`, `setresgid`,
  `setresuid` to the ids `rnk-rootfs` resolved in the root; `WORKDIR` is
  entered before the switch and again after if only root could.
- Only stdio crosses `execve` (threat T7): every other descriptor becomes
  close-on-exec (`close_range`, or the `/proc/self/fd` list before 5.11);
  descriptors the child keeps are numbered 3 or higher; stdin is
  `/dev/null` by default; umask 022, `SIGPIPE` default, empty signal mask.
  `PATH` is searched after entering the root and switching user.
- `Direct`: no namespace, mount or capability change; the command gets its
  own session, whose process group is killed with `SIGKILL` once the
  command exits (waited with `WNOWAIT`, so the group id cannot be reused),
  as kaniko does. On `/` nothing is placed. Another root is refused where
  the probe found the full sandbox possible, and when it is the host's
  `/`, `/etc` or `/dev` by device and inode (bind mounts included).
  Otherwise the child `chroot`s into it (`Step::Chroot`), checks the new
  `/` is the directory the parent checked, and drops to Docker's
  capabilities (bounding, effective, permitted), after the parent swapped
  in ranikust's `resolv.conf`, `hosts` (real host name: no UTS namespace),
  `hostname` and `mknod` `/dev` nodes (a refused node left out), each
  journaled as a `swap` before the image's entry is renamed aside to
  `.ranikust-aside-<id>-<name>`; `Stdio::Null` is the host's `/dev/null`,
  opened by the parent. No `/proc` (`ADR-0043`).
- `Direct` refuses mounts, secret files and `Network::None` (`Error::Direct`,
  before any clone). A `RunSpec::script` (`Direct` on `/` only) is written
  to a memfd (`MFD_EXEC` where known), sealed against writes, resizes and
  more seals, passed numbered 3 or higher; the child clears close-on-exec
  on it alone, after the close-on-exec pass, and runs
  `execveat(fd, "", AT_EMPTY_PATH)`, so the interpreter reads
  `/dev/fd/<n>`. Refused where `/dev/fd` is not a directory, where the
  kernel refuses executable memfds, and on a `Direct` root other than `/`
  (`ADR-0057`).
- No `RUN` holds ranikust's stream descriptors (`relay.rs`): each of
  stdout and stderr it would inherit becomes a pipe, `fchown`ed to the
  `RUN`'s ids when not root (an id the process cannot give leaves it
  root's), and one thread per `RUN` copies the pipes to ranikust's streams,
  waiting with `poll` on a full one; its read ends are non-blocking and it
  blocks only in `poll`, so a `RUN` reading its own pipe back
  (`/proc/self/fd/1`) cannot park it in a `read`. Once the `RUN` is reaped and swept an
  `eventfd` stops it: the pipes are read without blocking until empty, a
  stream that refuses writes gets two seconds, the drain stops at that
  deadline even while a writer keeps writing, and the thread is joined;
  never waiting for a write end that outlived the `RUN` (kaniko#2928,
  `ADR-0057`).
- The journal places and undoes on the root's own mount only
  (`RootPath::stay_on_mount`): a parent below another mount does not
  resolve, and an entry that is a mount point or on another mount is left.
- Every `RUN` (`Full` or `Direct`) gets the `RLIMIT_NOFILE` of
  `Sandbox::set_nofile_limit`, set after the close-on-exec pass, right
  before `execve` (`Step::FileLimit`); unset, it inherits ranikust's. The
  `RLIMIT_CORE` recorded by `forbid_core_dumps` is given back the same way
  (`Step::CoreLimit`).
- Stubs are journaled in `<state-dir>/sandbox/runs` before they exist (the
  journal is locked before it gets its name, so a replay never takes a live
  one) and removed before the diff; the parent's mtime returns to m0 only if it still
  shows m1, the value right after placing. Journals no process holds are
  replayed by `probe` and `Sandbox::start`. The replay runs as root on a
  directory others may have written: journals and run directories are
  opened with `O_NOFOLLOW` and the scratch files removed with `unlinkat`
  through descriptors, a journal's root must resolve strictly below the
  state directory (the parent of `dir`) or be the shared root given to
  `Sandbox::start_with_root`, opened once and matched by device, inode and
  mount, never by path (on it only swaps and the `/etc`, `/dev` stubs are
  undone), a `swap` record must name one of
  the nine `PLACEABLE` paths and an aside of the `.ranikust-aside-` form,
  and `dir` or `dir/runs` being a symlink is an error. A journal failing a
  check is left with a warning. Replay and crash unlock the journal's
  `flock` explicitly before closing it: the lock belongs to the open file
  description, which a child cloned meanwhile shares until it executes or
  exits.
- The probe performs the real sequence, in the configuration that will be
  used, never by kernel version; a command that does not exist fails at
  `execve` with `ENOENT`, which proves every step before it.
- Fake `/proc` (Phase 2): files chosen at run time from `/proc/self/cgroup`
  and `mountinfo` (v1 or v2 by `statfs`), read to EOF before `exec`, written
  0444; after the whole tree exits, all of `rootfs/proc` and `rootfs/sys`
  is deleted and their times restored. `JAVA_TOOL_OPTIONS` is prepended in
  the `RUN` environment only, never in the config, never with GC flags.

## Refs

- **Design:** [`design/sandbox.md#per-run-sequence`](../../design/sandbox.md#per-run-sequence), [`#process-set-up-once-per-build`](../../design/sandbox.md#process-set-up-once-per-build), [`#stub-mount-points`](../../design/sandbox.md#stub-mount-points); [`design/modes-and-detection.md#probe-order---modeauto`](../../design/modes-and-detection.md#probe-order---modeauto); [`design/kaniko-mode.md#fake-proc`](../../design/kaniko-mode.md#fake-proc), [`#java`](../../design/kaniko-mode.md#java); [threat model T5–T12, T17–T19](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) (default isolation), [ADR-0004](../../adr/0004-support-every-privilege-level.md) (probe or refuse), [ADR-0010](../../adr/0010-run-network-host-or-none.md) (network), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (bare `chroot` only on request), [ADR-0018](../../adr/0018-no-proot-in-1-0.md) (no proot), [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md) (`JAVA_TOOL_OPTIONS`)
- **Open:** [Q-017](../../open-questions.md#q-017-which-type-carries-the-probe-outcome-to-the-libraries) (`Caps`), [Q-032](../../open-questions.md#q-032-what-remains-to-verify-in-the-run-sandbox), [Q-077](../../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape) (`MS_MOVE` escape)
- **Evidence:** [exp-02](../../research/experiments/02-run-sandbox.md) (sequence, escape, PDEATHSIG, bounding set, stubs, 1–2.5 ms), [exp-06a](../../research/experiments/06a-root-rerun.md) (real root), [exp-13](../../research/experiments/13-docker-rootful-vm.md) (Docker probe matrix), [exp-12a](../../research/experiments/12a-fake-proc.md), [exp-14](../../research/experiments/14-real-builds-without-proc.md), [exp-15](../../research/experiments/15-java-next-concurrency.md), [exp-16](../../research/experiments/16-cgroup-v1-cpuset.md) (fake `/proc`, Java)
- **kaniko:** `kaniko@44aa7ddc0 pkg/commands/run.go:288` `runCommandInExec`
  (no namespaces; `:335` searches the process's `PATH`); `:356` and `:407`
  process-group kill, replaced by the PID namespace (kept for `Direct`);
  `:375` users from the process's `/etc/passwd`; `:83-135` cache mounts via
  `swapDir`, a bind here.
- **External:** BuildKit `executor/stubs.go` <https://github.com/moby/buildkit>, runc `--no-pivot` and `libcontainer` capabilities <https://github.com/opencontainers/runc>

## Tests

- **T0** (any user, `cargo test`): spec validation; mount plan order,
  environment defaults, `PATH` candidates, `/etc/hosts`; capability lists;
  failure records; the stub journal (placement through symlinks, removal,
  parent times, replay of dead journals only, a lock kept by a shared
  description); swaps (the image's entry back with its inode, a symlink
  kept, parent times, a parent the `RUN` changed kept, a refused node
  undone at once, only placeable paths under a real directory, replay on
  the shared root only, planted records skipped); `Direct` children cloned
  through the spawner thread that fail before executing anything (missing
  `WORKDIR`, then `setgroups` or a missing program; on a temp workdir,
  `chroot` refused as a normal user with every placed file undone); mount sources
  (symlink and type refused); remount flags (`nodev`, read-only `/dev`
  nodes); `Direct` refusals (`Error::Direct`); the sealed heredoc memfd; the relay
  (a surviving write end, a full stream); red team of the relay (T21,
  ADR-0057): a `RUN` that reads its own stream back through
  `/proc/self/fd/<n>` leaves the relay blocked in `read` after `poll`
  said ready, so `finish_returns_when_the_run_steals_its_own_output`
  (a write end kept after the `RUN`) and
  `a_stolen_stderr_does_not_stall_stdout` fail: findings, open
  (2026-11-01); the probe's `EPERM`-only fallback and the mount-root check; the
  spans (`tests/spans.rs`).
- **T1** (rootless podman): `just smoke` (2026-10-30, kernel 7.0, podman
  5.7): probe → `pivot_root`; `apk add` through the sandbox's network and
  `/etc` copies; checked inside: PID 1, host name, CapBnd/Prm/Eff
  `a80425fb` with empty Inh and Amb, root and `/dev` `nodev`, `/proc/sys`
  and `/dev/null` read-only, `/proc/kcore` and the other paths masked, no
  terminal, only stdio fds; no stub in the layer [measured]. To add: the
  fallbacks; `SIGKILL` during `RUN sleep 1000` leaves nothing.
- **T2** (VM only): `probe_reaches_exec_as_root` (`#[ignore]`);
  `a_direct_heredoc_runs_from_an_anonymous_file` (`$0` is `/dev/fd/<n>`,
  as root and uid 65534, which also writes `/dev/stdout` and
  `/dev/stderr`) and `a_direct_run_leaves_no_process_behind` (no process
  of the `RUN`, alive or zombie, told by a `comm` of its own)
  (`#[ignore]`, ADR-0057);
  red team of the `Direct` `RUN` on `/` (`tests/direct_attacks.rs`, T7,
  T11, T21, ADR-0057) [measured: VM, Ubuntu 24.04, kernel 6.8,
  2026-11-01]: held: a `#!` script cannot append to, truncate or rewrite
  its memfd (also through `/proc/self/fd/<n>`, as uid 65534), reads back
  unchanged, re-executes itself, and `#!/dev/fd/3` or a missing
  interpreter fails at once; a `RUN` holds only its pipes, `/dev/null`
  and its script, never a descriptor the parent opened without
  close-on-exec, and as uid 65534 owns and reopens its streams;
  `O_NONBLOCK` on its streams, a 4 MiB flood relayed whole and streams
  closed early leave ranikust's stdout flags as they were; a Python
  fork-and-exit loop under `setsid` (6000 pid changes a second) was
  caught by the sweep. Broken (findings, open):
  `red_team_direct_run_stealing_its_output_cannot_hang_the_build` (the
  build waits as long as a process outside the `RUN` keeps the stderr it
  was handed over `SCM_RIGHTS`) and
  `red_team_direct_run_reading_its_stderr_back_does_not_stall_its_stdout`
  (stdout no longer copied once the relay waits on stderr);
  `direct_runs_in_a_chroot_with_placed_files_as_root` (`#[ignore]`: a
  shell copied into a temp root sees the placed files and nodes, no
  `/proc`, `ulimit -n` 777; the root is as before after) [measured: passed
  in the VM, Ubuntu 24.04, kernel 6.8, 2026-10-31]; real root;
  escapes (`chroot` + `chdir("..")` under `pivot_root` and `MS_MOVE`,
  `umount /etc/resolv.conf`, a setuid binary) fail; writes to `/proc/sys`
  and `/proc/sysrq-trigger` fail; `mknod` + open and `chmod /dev/null`
  fail; `USER nobody` + `SIGKILL` of ranikust leaves nothing; no
  controlling terminal; the probe matrix of `docs/testing.md`; no fake file
  in any layer on the bare-`chroot` path.

## Metrics

- `rnk_sandbox.run` per `RUN`: `wait4` rusage (`user_ns`, `sys_ns`,
  `max_rss_kb`, faults, blocks, context switches) when the child was
  reaped after `execve`, and `failed`; `rnk_sandbox.setup` (`stubs`);
  `rnk_sandbox.probe` (`ok`) per attempt
  ([rows](../../design/metrics.md#rnk-sandbox)).
- Spans are opened on the caller's thread, so they nest in its span with no
  `parent:`; the spawner thread opens none; nothing is traced between fork
  and exec. The probe's own runs record no `run` or `setup` span.

## Pitfalls

- Root escapes a bare `chroot`, reaching the host's `/` (exp-02, exp-06a,
  exp-13) or, under `--parallel-stages`, sibling stages (exp-12).
- `MS_MOVE` + `chroot` leaves the old root mounted under the stage root;
  `..` stops at the moved root only when `/` is a mount root, otherwise root
  with `CAP_SYS_CHROOT` climbs out to ranikust's own filesystem [inferred]
  (Q-077).
- A bind refuses (`EINVAL`) a source opened in another mount namespace: a
  descriptor the parent opened cannot be bound by the child (`just smoke`
  found it; no T0 test runs a `Full` child).
- busybox `test -w` is true for root whatever the mount: check read-only
  mounts in `/proc/self/mountinfo`.
- A fresh `proc` is writable: a sysctl write needs only uid 0, and
  `kernel/core_pattern` makes the host kernel run a `RUN` binary as host
  root. Docker's read-only and masked paths are not optional (T17).
- Docker stops device nodes with its device cgroup; without one, only
  `nodev` does. A bind of a host node is the host's inode: `chmod` in a
  `RUN` changes the host's `/dev/null` unless the bind is read-only (T18).
- `setresuid` and `setresgid` clear `PR_SET_PDEATHSIG`; a `RUN` with a
  non-root `USER` outlived the build until it was armed again (T8).
- A `RUN` that keeps ranikust's controlling terminal can inject input into
  the operator's shell with `TIOCSTI` (T19).
- Multithreaded `unshare(CLONE_NEWNS)` moves one thread; `CLONE_NEWUSER` is
  `EINVAL` (exp-02 finding 5).
- Without PDEATHSIG a `RUN` outlived a killed build and kept writing; it
  fires when the parent *thread* exits (exp-02).
- Without the bounding-set drop `RUN` unmounted the `resolv.conf` bind and
  wrote the root; unjournaled stubs leak and survive `SIGKILL` (exp-02).
- Docker's seccomp blocks `pivot_root` even with `CAP_SYS_ADMIN` (sandbox
  design); on Ubuntu ≥ 24.04 `unshare` works, then mounts fail (exp-02).
- Rust ignores `SIGPIPE` and ignored signals survive `execve`: unless it is
  reset, `yes | head` in a `RUN` ends with `EPIPE` errors instead of a quiet
  `SIGPIPE`.
- `next build` fails without `/proc/self/stat` (exp-14); Java is OOM-killed
  under a memory limit without `JAVA_TOOL_OPTIONS` (exp-15).
- A hard-coded cgroup file set fixes neither v1 nor v2 (exp-13, exp-16);
  never fake `/proc/self/{exe,fd,maps,status}` (exp-12a).
- An unrestored parent mtime put `/etc` in every layer (exp-12, bugs found).
- Closing a `flock`ed file does not release the lock while a child cloned
  by another thread still shares its description (until `execve` or exit):
  a replay then saw a dead journal as live (a flaky T0 test). Unlock
  explicitly.
- A `Direct` `RUN` in a `chroot` has no `/proc`; on a host that can mount,
  the caller should run that root with `Isolation::Full` instead.

## Phase 1

- Done (`feat/rnk-sandbox`): `enter_build_namespace`, spawner thread,
  PDEATHSIG with the parent-pid check, re-armed after the id switch, the
  `RUN` sequence with `pivot_root` and
  the `MS_MOVE` fallback, bounding-set drop, `USER`, stubs and journal with
  replay, bind mounts for `RUN --mount=type=cache|secret`,
  `--network=none`, `Isolation::Direct` (early, from Phase 2), the probe
  of the root entry, and the hardening of the host-safety review: Docker's
  read-only and masked kernel files, `nodev`, read-only `/dev` node binds,
  `setsid`, a session keyring, mount sources by descriptor. Tested by T0 and
  by `just smoke` (T1, rootless `podman run --privileged`), where the whole
  sequence runs in a user namespace with `pivot_root` and `RUN`s check the
  result from inside; nothing has run as real root yet (T2).
- Left for `ranikust` (task 8): calling `enter_build_namespace` and `probe`
  in the single-threaded window and `Sandbox::start` after.
- Deferred: bare `chroot`, fake `/proc`, `JAVA_TOOL_OPTIONS` (Phase 2); user
  namespaces, the `proc` and `sysfs` fallbacks, the full probe and guidance
  (uid map, `sysfs`, bind, `mknod`), `--seccomp`, `--userns` (Phase 3);
  `RUN --mount=type=bind` (Phase 5).
