# rnk-sandbox

> **Status:** skeleton · **Phase:** 1 ·
> **Depends on:** `rnk-rootfs` (`USER` lookup) · **Used by:** `rnk-builder`, `ranikust` (probes, process set-up) · **Path:** `crates/rnk-sandbox/`

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
  `Isolation::Direct` [design]: sequential kaniko-mode `RUN` on `/`.
- The no-mount `--parallel-stages` path: bare `chroot`, `mknod` `/dev`
  nodes, fake `/proc` and `/sys`, `JAVA_TOOL_OPTIONS`.

## Must not

- Look up users on the host or parse `/etc/passwd` itself (`rnk-rootfs`);
  snapshot, diff or mount stage roots, or probe overlays (`rnk-snapshot`).
- Use a bare `chroot` where mounting is possible; `unshare` once threads
  exist; fork `RUN` from tokio's `spawn_blocking`; pick the mode (`ranikust`).

## API (planned)

```rust
pub fn enter_build_namespace() -> Result<()>;   // [design] unshare(CLONE_NEWNS) + rslave; single-threaded only
pub mod probe { /* userns + map, proc, pivot_root, sysfs, bind, mknod */ }  // [design] one fn per step, forked child each; ranikust calls them
pub struct Caps { /* pivot_root or MS_MOVE, proc or rbind, sysfs or rbind, mknod */ }  // [design] what the probes found; the Outcome is ranikust's
pub struct Sandbox { /* spawner thread, stub journal */ }               // [design]
impl Sandbox {
    pub fn start(caps: &Caps, journal: &Path) -> Result<Self>;          // [design] replays the journal first
    pub async fn run(&self, spec: RunSpec) -> Result<ExitStatus>;       // [design] caller holds a --jobs permit
}
pub struct RunSpec { /* root: RootPath, argv, env, user, workdir, network, mounts, isolation */ }  // [design]
pub enum Network { Host, None }                                     // [decided] ADR-0010
pub enum Isolation { Full, Direct, BareChroot { fake_proc: bool } } // [design] Direct: sequential kaniko mode on `/`
```

## Invariants

- `unshare` only while single-threaded. One spawner thread, alive for the
  build, forks every `RUN`; the child sets `PR_SET_PDEATHSIG(SIGKILL)`,
  re-checks `getppid()` and is PID 1 of its PID namespace, so its tree dies.
- Namespaces: mount, PID, UTS, IPC (plus user without `CAP_SYS_ADMIN`, network
  for `--network=none`). Mounts exist only in the child, so the parent diffs
  the root as the image left it.
- Mount order: self-bind, `proc`, read-only `sysfs`, tmpfs `/dev`, writable
  copies of `resolv.conf`, `hosts`, `hostname` (targets via `RootPath`;
  remounts keep the locked flags), `RUN --mount`; then `pivot_root(".", ".")`
  and `umount2(MNT_DETACH)`; where seccomp blocks it, `MS_MOVE` + `chroot(".")`.
- Bounding and inheritable sets: Docker's 14 defaults for every `USER`. Only
  stdio crosses `execve` (threat T7). `USER` comes from the root's files;
  `PATH` is searched after `pivot_root` and after `setuid`.
- Stubs are journaled in the state directory before they exist and removed
  before the diff; the parent's mtime returns to m0 only if it still shows
  m1, the value right after placing. The journal is replayed at start-up.
- The probe performs each step for real, in the configuration that will be
  used, never by kernel version; failures carry the errno; it is final.
- Fake `/proc`: files chosen at run time from `/proc/self/cgroup` and
  `mountinfo` (v1 or v2 by `statfs`), read to EOF before `exec`, written 0444;
  after the whole tree exits, all of `rootfs/proc` and `rootfs/sys` is
  deleted and their times restored. `JAVA_TOOL_OPTIONS` is prepended in the
  `RUN` environment only, never in the config, never with GC flags.

## Refs

- **Design:** [`design/sandbox.md#per-run-sequence`](../../design/sandbox.md#per-run-sequence), [`#process-set-up-once-per-build`](../../design/sandbox.md#process-set-up-once-per-build), [`#stub-mount-points`](../../design/sandbox.md#stub-mount-points); [`design/modes-and-detection.md#probe-order---modeauto`](../../design/modes-and-detection.md#probe-order---modeauto); [`design/kaniko-mode.md#fake-proc`](../../design/kaniko-mode.md#fake-proc), [`#java`](../../design/kaniko-mode.md#java); [threat model T5–T10, T12](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) (default isolation), [ADR-0004](../../adr/0004-support-every-privilege-level.md) (probe or refuse), [ADR-0010](../../adr/0010-run-network-host-or-none.md) (network), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (bare `chroot` only on request), [ADR-0018](../../adr/0018-no-proot-in-1-0.md) (no proot), [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md) (`JAVA_TOOL_OPTIONS`)
- **Evidence:** [exp-02](../../research/experiments/02-run-sandbox.md) (sequence, escape, PDEATHSIG, bounding set, stubs, 1–2.5 ms), [exp-06a](../../research/experiments/06a-root-rerun.md) (real root), [exp-13](../../research/experiments/13-docker-rootful-vm.md) (Docker probe matrix), [exp-12a](../../research/experiments/12a-fake-proc.md), [exp-14](../../research/experiments/14-real-builds-without-proc.md), [exp-15](../../research/experiments/15-java-next-concurrency.md), [exp-16](../../research/experiments/16-cgroup-v1-cpuset.md) (fake `/proc`, Java)
- **kaniko:** `kaniko@44aa7ddc0 pkg/commands/run.go:288` `runCommandInExec`
  (no namespaces; `:335` searches the process's `PATH`); `:356` and `:407`
  process-group kill, replaced by the PID namespace; `:375` users from the
  process's `/etc/passwd`; `:83-135` cache mounts via `swapDir`, a bind here.
- **External:** BuildKit `executor/stubs.go` <https://github.com/moby/buildkit>, runc `--no-pivot` <https://github.com/opencontainers/runc>

## Tests

- **T0:** `USER` lookup in fixture roots (`_apt`; numeric uid → gid 0); fake
  files from captured `cgroup` and `mountinfo` (Docker v1, v2); the
  `JAVA_TOOL_OPTIONS` value; probe outcomes from synthetic steps.
- **T1** (rootless podman): the sequence with its fallbacks; `SIGKILL` during
  `RUN sleep 1000` leaves nothing; no stub in a layer; only stdio fds.
- **T2** (VM only): real root; escapes (`chroot` + `chdir("..")`,
  `umount /etc/resolv.conf`, a setuid binary) fail; the probe matrix of
  `docs/testing.md`; no fake file in any layer on the bare-`chroot` path.

## Pitfalls

- Root escapes a bare `chroot`, reaching the host's `/` (exp-02, exp-06a,
  exp-13) or, under `--parallel-stages`, sibling stages (exp-12).
- Multithreaded `unshare(CLONE_NEWNS)` moves one thread; `CLONE_NEWUSER` is
  `EINVAL` (exp-02 finding 5).
- Without PDEATHSIG a `RUN` outlived a killed build and kept writing; it
  fires when the parent *thread* exits (exp-02).
- Without the bounding-set drop `RUN` unmounted the `resolv.conf` bind and
  wrote the root; unjournaled stubs leak and survive `SIGKILL` (exp-02).
- Docker's seccomp blocks `pivot_root` even with `CAP_SYS_ADMIN` (sandbox
  design); on Ubuntu ≥ 24.04 `unshare` works, then mounts fail (exp-02).
- `next build` fails without `/proc/self/stat` (exp-14); Java is OOM-killed
  under a memory limit without `JAVA_TOOL_OPTIONS` (exp-15).
- A hard-coded cgroup file set fixes neither v1 nor v2 (exp-13, exp-16);
  never fake `/proc/self/{exe,fd,maps,status}` (exp-12a).
- An unrestored parent mtime put `/etc` in every layer (exp-12, bugs found).

## Phase 1

- Build, in order: `enter_build_namespace`, spawner, PDEATHSIG → the `RUN`
  sequence as real root, `MS_MOVE` fallback → bounding-set drop → `USER` →
  stubs → `RUN --mount=type=cache|secret` → `--network=none`. Probe only
  this crate's part of steps 1 and 3 of modes-and-detection, with errno.
- Deferred: `Direct`, bare `chroot`, fake `/proc`, `JAVA_TOOL_OPTIONS`
  (Phase 2); user namespaces, fallbacks, full probe and guidance,
  `--seccomp`, `--userns` (Phase 3); `RUN --mount=type=bind` (Phase 5).
